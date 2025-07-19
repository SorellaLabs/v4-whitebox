use std::{collections::HashMap, sync::Arc};

use alloy::{
    primitives::{
        Address,
        aliases::{I24, U24}
    },
    providers::Provider
};
use dashmap::DashMap;
use futures::future::join_all;
use thiserror::Error;

use super::{
    pool_data_loader::{DataLoader, PoolDataLoader, TickData},
    pools::PoolId
};
use crate::{
    fetch_pool_keys::fetch_angstrom_pools,
    uni_structure::{BaselinePoolState, liquidity_base::BaselineLiquidity, tick_info::TickInfo},
    uniswap::{pool_key::PoolKey, pool_registry::UniswapPoolRegistry}
};

pub const INITIAL_TICKS_PER_SIDE: u16 = 400;
const TICKS_PER_BATCH: usize = 25;

#[derive(Error, Debug)]
pub enum BaselinePoolFactoryError {
    #[error("Pool data loading error: {0}")]
    PoolDataLoading(String),
    #[error("Provider error: {0}")]
    Provider(String),
    #[error("Pool initialization error: {0}")]
    Initialization(String)
}

/// Factory for creating BaselinePoolState instances with full tick loading
#[derive(Clone)]
pub struct BaselinePoolFactory<P> {
    provider:     Arc<P>,
    registry:     UniswapPoolRegistry,
    pool_manager: Address,
    tick_band:    u16
}

impl<P: Provider + 'static> BaselinePoolFactory<P>
where
    DataLoader: PoolDataLoader
{
    pub async fn new(
        deploy_block: u64,
        current_block: u64,
        angstrom_addr: Address,
        provider: Arc<P>,
        registry: UniswapPoolRegistry,
        pool_manager: Address,
        tick_band: Option<u16>
    ) -> (Self, Arc<DashMap<PoolId, BaselinePoolState>>) {
        // Fetch all existing pool keys
        let pool_keys = fetch_angstrom_pools(
            deploy_block as usize,
            current_block as usize,
            angstrom_addr,
            provider.as_ref()
        )
        .await;

        let mut this = Self {
            provider,
            registry,
            pool_manager,
            tick_band: tick_band.unwrap_or(INITIAL_TICKS_PER_SIDE)
        };

        let pools = DashMap::new();

        for pool_key in pool_keys {
            let pool_id = PoolId::from(pool_key);

            // Use the factory to create and initialize the pool
            let baseline_state = this
                .create_new_baseline_angstrom_pool(pool_key, current_block, false)
                .await
                .unwrap();

            pools.insert(pool_id, baseline_state);
        }

        (this, Arc::new(pools))
    }

    pub fn registry(&self) -> UniswapPoolRegistry {
        self.registry.clone()
    }

    pub fn get_uniswap_pool_ids(&self) -> impl Iterator<Item = PoolId> + '_ {
        self.registry.private_keys()
    }

    /// Creates a BaselinePoolState with full tick loading from existing pools
    /// in registry
    pub async fn init_baseline_pools(
        &self,
        block: u64,
        is_bundle_mode: bool
    ) -> Vec<BaselinePoolState> {
        let pool_ids: Vec<_> = self.registry.pools().keys().copied().collect();
        let mut futures = Vec::new();

        for pool_id in pool_ids {
            let internal = self
                .registry
                .conversion_map
                .get(&pool_id)
                .expect("factory conversion map failure");

            let future =
                self.create_baseline_pool_from_registry(*internal, pool_id, block, is_bundle_mode);
            futures.push(future);
        }

        let results = join_all(futures).await;
        results
            .into_iter()
            .map(|result| result.expect("failed to init baseline pool").0)
            .collect()
    }

    /// Creates a new BaselinePoolState with full tick loading for Angstrom
    /// pools
    pub async fn create_new_baseline_angstrom_pool(
        &mut self,
        mut pool_key: PoolKey,
        block: u64,
        is_bundle_mode: bool
    ) -> Result<BaselinePoolState, BaselinePoolFactoryError> {
        // Add to registry
        let pub_key = PoolId::from(pool_key);
        self.registry.pools.insert(pub_key, pool_key);

        // Create private key
        pool_key.fee = U24::from(0x800000);
        let priv_key = PoolId::from(pool_key);
        self.registry.conversion_map.insert(pub_key, priv_key);

        let internal = self
            .registry
            .conversion_map
            .get(&pub_key)
            .expect("new angstrom pool not in conversion map");

        let baseline_state = self
            .create_baseline_pool_from_registry(*internal, pub_key, block, is_bundle_mode)
            .await?;

        Ok(baseline_state)
    }

    /// Core method that creates BaselinePoolState with complete tick loading
    async fn create_baseline_pool_from_registry(
        &self,
        internal_pool_id: PoolId,
        pool_id: PoolId,
        block: u64,
        is_bundle_mode: bool
    ) -> Result<BaselinePoolState, BaselinePoolFactoryError> {
        // Create data loader
        let data_loader = DataLoader::new_with_registry(
            internal_pool_id,
            pool_id,
            self.registry.clone(),
            self.pool_manager
        );

        // Load basic pool data
        let pool_data = data_loader
            .load_pool_data(Some(block), self.provider.clone())
            .await
            .map_err(|e| {
                BaselinePoolFactoryError::PoolDataLoading(format!("Failed to load pool data: {e}"))
            })?;

        // Extract basic pool state
        let liquidity = pool_data.liquidity;
        let sqrt_price_x96 = pool_data.sqrtPrice.into();
        let tick = pool_data.tick.as_i32();
        let tick_spacing = pool_data.tickSpacing.as_i32();
        let book_fee = pool_data.fee.to::<u32>();

        // Load ticks in both directions
        let (ticks, tick_bitmap) = self
            .load_tick_data_in_band(&data_loader, tick, tick_spacing, Some(block))
            .await?;

        // Create BaselineLiquidity with loaded tick data
        let baseline_liquidity = BaselineLiquidity::new(
            tick_spacing,
            tick,
            sqrt_price_x96,
            liquidity,
            ticks,
            tick_bitmap
        );

        // Create fee configuration based on bundle mode parameter
        let fee_config = crate::uni_structure::FeeConfiguration {
            is_bundle_mode,
            bundle_fee: book_fee, // Store original fee as bundle fee
            swap_fee: book_fee,   // Use same fee for swap fee in unlocked mode
            protocol_fee: 500     // Mock protocol fee (0.05% in basis points of 1e6)
        };

        // Create and return BaselinePoolState
        Ok(BaselinePoolState::new(
            baseline_liquidity,
            block,
            fee_config,
            pool_data.tokenA,
            pool_data.tokenB,
            pool_data.tokenADecimals,
            pool_data.tokenBDecimals
        ))
    }

    /// Loads complete tick data in both directions around the current tick
    async fn load_tick_data_in_band(
        &self,
        data_loader: &DataLoader,
        current_tick: i32,
        tick_spacing: i32,
        block_number: Option<u64>
    ) -> Result<
        (HashMap<i32, TickInfo>, HashMap<i16, alloy::primitives::U256>),
        BaselinePoolFactoryError
    > {
        // Load ticks in both directions concurrently
        let (asks_result, bids_result) = futures::future::join(
            self.load_ticks_in_direction(
                data_loader,
                true,
                current_tick,
                tick_spacing,
                block_number
            ),
            self.load_ticks_in_direction(
                data_loader,
                false,
                current_tick,
                tick_spacing,
                block_number
            )
        )
        .await;

        let asks = asks_result?;
        let bids = bids_result?;

        // Combine tick data from both directions
        let mut all_ticks = asks;
        all_ticks.extend(bids);

        // Apply ticks to create final tick maps
        self.apply_ticks(all_ticks, tick_spacing)
    }

    /// Loads ticks in a specific direction
    async fn load_ticks_in_direction(
        &self,
        data_loader: &DataLoader,
        zero_for_one: bool,
        current_tick: i32,
        tick_spacing: i32,
        block_number: Option<u64>
    ) -> Result<Vec<TickData>, BaselinePoolFactoryError> {
        let mut fetched_ticks = Vec::new();
        let mut tick_start = current_tick;
        let mut ticks_loaded = 0u16;

        while ticks_loaded < self.tick_band {
            let ticks_to_load =
                std::cmp::min(TICKS_PER_BATCH as u16, self.tick_band - ticks_loaded);

            let (batch_ticks, next_tick) = self
                .get_tick_data_batch_request(
                    data_loader,
                    I24::unchecked_from(tick_start),
                    zero_for_one,
                    ticks_to_load,
                    tick_spacing,
                    block_number
                )
                .await?;

            fetched_ticks.extend(batch_ticks);
            ticks_loaded += ticks_to_load;

            // Update tick_start for next batch
            tick_start = if zero_for_one {
                next_tick.wrapping_sub(tick_spacing)
            } else {
                next_tick.wrapping_add(tick_spacing)
            };
        }

        Ok(fetched_ticks)
    }

    /// Makes batch request for tick data
    async fn get_tick_data_batch_request(
        &self,
        data_loader: &DataLoader,
        tick_start: I24,
        zero_for_one: bool,
        num_ticks: u16,
        tick_spacing: i32,
        block_number: Option<u64>
    ) -> Result<(Vec<TickData>, i32), BaselinePoolFactoryError> {
        let (ticks, _last_tick_bitmap) = data_loader
            .load_tick_data(
                tick_start,
                zero_for_one,
                num_ticks,
                I24::unchecked_from(tick_spacing),
                block_number,
                self.provider.clone()
            )
            .await
            .map_err(|e| {
                BaselinePoolFactoryError::PoolDataLoading(format!("Failed to load tick data: {e}"))
            })?;

        // Calculate next tick start position
        let next_tick = if zero_for_one {
            tick_start.as_i32() - (num_ticks as i32)
        } else {
            tick_start.as_i32() + (num_ticks as i32)
        };

        Ok((ticks, next_tick))
    }

    /// Applies loaded tick data to create tick maps and bitmaps
    fn apply_ticks(
        &self,
        mut fetched_ticks: Vec<TickData>,
        tick_spacing: i32
    ) -> Result<
        (HashMap<i32, TickInfo>, HashMap<i16, alloy::primitives::U256>),
        BaselinePoolFactoryError
    > {
        let mut ticks = HashMap::new();
        let mut tick_bitmap = HashMap::new();

        // Sort ticks by tick value
        fetched_ticks.sort_by_key(|t| t.tick.as_i32());

        // Process only initialized ticks
        for tick_data in fetched_ticks.into_iter().filter(|t| t.initialized) {
            let tick_info = TickInfo {
                initialized:   tick_data.initialized,
                liquidity_net: tick_data.liquidityNet
            };

            ticks.insert(tick_data.tick.as_i32(), tick_info);

            // Update tick bitmap
            self.flip_tick_if_not_init(&mut tick_bitmap, tick_data.tick.as_i32(), tick_spacing);
        }

        Ok((ticks, tick_bitmap))
    }

    /// Flips tick in bitmap if not already initialized
    fn flip_tick_if_not_init(
        &self,
        tick_bitmap: &mut HashMap<i16, alloy::primitives::U256>,
        tick: i32,
        tick_spacing: i32
    ) {
        use alloy::primitives::U256;

        let compressed = tick / tick_spacing;
        let word_pos = (compressed >> 8) as i16;
        let bit_pos = (compressed & 0xFF) as u8;

        let word = tick_bitmap.entry(word_pos).or_insert(U256::ZERO);
        let mask = U256::from(1) << bit_pos;

        if *word & mask == U256::ZERO {
            *word |= mask;
        }
    }

    /// Get current pool keys from registry
    pub fn current_pool_keys(&self) -> Vec<PoolKey> {
        self.registry.pools.values().cloned().collect()
    }

    /// Get conversion map from registry
    pub fn conversion_map(&self) -> &HashMap<PoolId, PoolId> {
        &self.registry.conversion_map
    }

    /// Remove pool from registry
    pub fn remove_pool(&mut self, pool_key: PoolKey) -> PoolId {
        let id = PoolId::from(pool_key);
        let _ = self.registry.pools.remove(&id);
        self.registry
            .conversion_map
            .remove(&id)
            .expect("failed to remove pool id in factory");
        id
    }
}
