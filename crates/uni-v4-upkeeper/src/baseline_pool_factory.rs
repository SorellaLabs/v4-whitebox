use std::{
    collections::{HashMap, HashSet},
    sync::Arc
};

use alloy::{
    primitives::{
        Address, U256,
        aliases::{I24, U24}
    },
    providers::Provider
};
use dashmap::DashMap;
use futures::{Stream, StreamExt, future::BoxFuture, stream::FuturesUnordered};
use thiserror::Error;
use uni_v4_structure::{BaselinePoolState, liquidity_base::BaselineLiquidity, tick_info::TickInfo};

use super::{
    pool_data_loader::{DataLoader, PoolDataLoader, TickData},
    pools::PoolId
};
use crate::{
    fetch_pool_keys::fetch_angstrom_pools, pool_key::PoolKey, pool_registry::UniswapPoolRegistry
};

pub const INITIAL_TICKS_PER_SIDE: u16 = 300;
const TICKS_PER_BATCH: usize = 10;

#[derive(Error, Debug)]
pub enum BaselinePoolFactoryError {
    #[error("Pool data loading error: {0}")]
    PoolDataLoading(String),
    #[error("Provider error: {0}")]
    Provider(String),
    #[error("Pool initialization error: {0}")]
    Initialization(String)
}

pub enum UpdateMessage {
    NewTicks(PoolId, HashMap<i32, TickInfo>, HashMap<i16, alloy::primitives::U256>),
    NewPool(PoolId, BaselinePoolState)
}

/// Factory for creating BaselinePoolState instances with full tick loading
pub struct BaselinePoolFactory<P> {
    provider:            Arc<P>,
    registry:            UniswapPoolRegistry,
    pool_manager:        Address,
    tick_band:           u16,
    tick_edge_threshold: u16,
    tick_loading: FuturesUnordered<
        BoxFuture<'static, (PoolId, HashMap<i32, TickInfo>, HashMap<i16, alloy::primitives::U256>)>
    >,
    pool_generator: FuturesUnordered<
        BoxFuture<'static, Result<(PoolId, BaselinePoolState), BaselinePoolFactoryError>>
    >
}

impl<P: Provider + 'static> BaselinePoolFactory<P>
where
    DataLoader: PoolDataLoader
{
    #[allow(clippy::too_many_arguments)]
    pub async fn new(
        deploy_block: u64,
        current_block: u64,
        angstrom_addr: Address,
        provider: Arc<P>,
        pool_manager: Address,
        tick_band: Option<u16>,
        tick_edge_threshold: Option<u16>,
        filter_pool_keys: Option<HashSet<PoolKey>>
    ) -> (Self, Arc<DashMap<PoolId, BaselinePoolState>>) {
        // Fetch all existing pool keys to get their fees
        let all_pool_keys_with_fees = fetch_angstrom_pools(
            deploy_block as usize,
            current_block as usize,
            angstrom_addr,
            provider.as_ref()
        )
        .await;

        // Create filter set if provided
        let filter_pool_ids: Option<HashSet<PoolId>> =
            filter_pool_keys.map(|keys| keys.into_iter().map(PoolId::from).collect());

        let registry = UniswapPoolRegistry::default();

        let mut this = Self {
            provider,
            registry,
            pool_manager,
            tick_band: tick_band.unwrap_or(INITIAL_TICKS_PER_SIDE),
            tick_edge_threshold: tick_edge_threshold.unwrap_or(100),
            tick_loading: FuturesUnordered::default(),
            pool_generator: FuturesUnordered::default()
        };

        let pools = DashMap::new();

        // Create pools - either all or filtered based on filter_pool_ids
        for pool_key_with_fees in all_pool_keys_with_fees {
            let angstrom_pool_id = PoolId::from(pool_key_with_fees.pool_key);

            // If no filter provided or pool is in filter, create it
            if filter_pool_ids
                .as_ref()
                .is_none_or(|filter| filter.contains(&angstrom_pool_id))
            {
                // Use the factory to create and initialize the pool
                let baseline_state = this
                    .create_new_baseline_angstrom_pool(
                        pool_key_with_fees.pool_key,
                        current_block,
                        pool_key_with_fees.bundle_fee,
                        pool_key_with_fees.swap_fee,
                        pool_key_with_fees.protocol_fee
                    )
                    .await
                    .unwrap();

                // Get the Uniswap pool ID from registry
                let uniswap_pool_id = this
                    .registry
                    .private_key_from_public(&angstrom_pool_id)
                    .expect("Pool should have been registered");
                pools.insert(uniswap_pool_id, baseline_state);
            }
        }

        (this, Arc::new(pools))
    }

    pub fn is_processing(&self) -> bool {
        !(self.tick_loading.is_empty() || self.pool_generator.is_empty())
    }

    pub fn registry(&self) -> UniswapPoolRegistry {
        self.registry.clone()
    }

    pub fn get_uniswap_pool_ids(&self) -> impl Iterator<Item = PoolId> + '_ {
        self.registry.private_keys()
    }

    /// Creates a new BaselinePoolState with full tick loading for Angstrom
    /// pools
    pub async fn create_new_baseline_angstrom_pool(
        &mut self,
        mut pool_key: PoolKey,
        block: u64,
        bundle_fee: u32,
        swap_fee: u32,
        protocol_fee: u32
    ) -> Result<BaselinePoolState, BaselinePoolFactoryError> {
        // Add to registry
        let pub_key = PoolId::from(pool_key);
        self.registry.pools.insert(pub_key, pool_key);

        // Create private key
        pool_key.fee = U24::from(0x800000);
        let priv_key = PoolId::from(pool_key);
        self.registry.conversion_map.insert(pub_key, priv_key);
        self.registry
            .reverse_conversion_map
            .insert(priv_key, pub_key);

        let internal = self
            .registry
            .conversion_map
            .get(&pub_key)
            .expect("new angstrom pool not in conversion map");

        let baseline_state = self
            .create_baseline_pool_from_registry(
                *internal,
                pub_key,
                block,
                bundle_fee,
                swap_fee,
                protocol_fee
            )
            .await?;

        Ok(baseline_state)
    }

    /// Core method that creates BaselinePoolState with complete tick loading
    async fn create_baseline_pool_from_registry(
        &self,
        internal_pool_id: PoolId,
        pool_id: PoolId,
        block: u64,
        bundle_fee: u32,
        swap_fee: u32,
        protocol_fee: u32
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

        // Create fee configuration with provided values
        let fee_config = uni_v4_structure::FeeConfiguration { bundle_fee, swap_fee, protocol_fee };

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

    /// Static version of load_tick_data_in_band for use in async contexts
    async fn load_tick_data_in_band_static(
        data_loader: &DataLoader,
        current_tick: i32,
        tick_spacing: i32,
        block_number: Option<u64>,
        provider: Arc<P>,
        tick_band: u16
    ) -> Result<
        (HashMap<i32, TickInfo>, HashMap<i16, alloy::primitives::U256>),
        BaselinePoolFactoryError
    > {
        // Load ticks in both directions concurrently
        let (asks_result, bids_result) = futures::future::join(
            Self::load_ticks_in_direction_static(
                data_loader,
                true,
                current_tick,
                tick_spacing,
                block_number,
                provider.clone(),
                tick_band
            ),
            Self::load_ticks_in_direction_static(
                data_loader,
                false,
                current_tick,
                tick_spacing,
                block_number,
                provider,
                tick_band
            )
        )
        .await;

        let asks = asks_result?;
        let bids = bids_result?;

        // Combine tick data from both directions
        let mut all_ticks = asks;
        all_ticks.extend(bids);

        // Apply ticks to create final tick maps
        Self::apply_ticks_static(all_ticks, tick_spacing)
    }

    /// Static version of load_ticks_in_direction
    async fn load_ticks_in_direction_static(
        data_loader: &DataLoader,
        zero_for_one: bool,
        current_tick: i32,
        tick_spacing: i32,
        block_number: Option<u64>,
        provider: Arc<P>,
        tick_band: u16
    ) -> Result<Vec<TickData>, BaselinePoolFactoryError> {
        let mut fetched_ticks = Vec::new();
        let mut tick_start = current_tick;
        let mut ticks_loaded = 0u16;

        while ticks_loaded < tick_band {
            let ticks_to_load = std::cmp::min(TICKS_PER_BATCH as u16, tick_band - ticks_loaded);

            let (batch_ticks, next_tick) = Self::get_tick_data_batch_request_static(
                data_loader,
                I24::unchecked_from(tick_start),
                zero_for_one,
                ticks_to_load,
                tick_spacing,
                block_number,
                provider.clone()
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

    /// Static version of get_tick_data_batch_request
    async fn get_tick_data_batch_request_static(
        data_loader: &DataLoader,
        tick_start: I24,
        zero_for_one: bool,
        num_ticks: u16,
        tick_spacing: i32,
        block_number: Option<u64>,
        provider: Arc<P>
    ) -> Result<(Vec<TickData>, i32), BaselinePoolFactoryError> {
        let (ticks, _last_tick_bitmap) = data_loader
            .load_tick_data(
                tick_start,
                zero_for_one,
                num_ticks,
                I24::unchecked_from(tick_spacing),
                block_number,
                provider
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

    /// Static version of apply_ticks
    fn apply_ticks_static(
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
            let tick = tick_data.tick.as_i32();
            let compressed = tick / tick_spacing;
            let word_pos = (compressed >> 8) as i16;
            let bit_pos = (compressed & 0xFF) as u8;

            let word = tick_bitmap.entry(word_pos).or_insert(U256::ZERO);
            let mask = U256::from(1) << bit_pos;

            if *word & mask == U256::ZERO {
                *word |= mask;
            }
        }

        Ok((ticks, tick_bitmap))
    }

    /// Get current pool keys from registry
    pub fn current_pool_keys(&self) -> Vec<PoolKey> {
        self.registry.pools.values().cloned().collect()
    }

    /// Get conversion map from registry
    pub fn conversion_map(&self) -> &HashMap<PoolId, PoolId> {
        &self.registry.conversion_map
    }

    pub fn remove_pool_by_id(&mut self, pool_id: PoolId) {
        let _ = self.registry.pools.remove(&pool_id);
        self.registry
            .conversion_map
            .remove(&pool_id)
            .expect("failed to remove pool id in factory");
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

    /// Request more ticks to be loaded for a pool
    pub fn request_more_ticks(
        &mut self,
        pool_id: PoolId,
        zero_for_one: bool,
        current_tick: i32,
        tick_spacing: i32,
        num_ticks: u16,
        block_number: Option<u64>
    ) {
        let provider = self.provider.clone();
        let pool_manager = self.pool_manager;
        let registry = self.registry.clone();

        // Get the internal pool ID for loading
        let internal_pool_id = *self
            .registry
            .conversion_map
            .get(&pool_id)
            .unwrap_or(&pool_id);

        // Create the future for loading ticks
        let future = async move {
            let data_loader =
                DataLoader::new_with_registry(internal_pool_id, pool_id, registry, pool_manager);

            let initial_tick_start = if zero_for_one {
                current_tick - tick_spacing
            } else {
                current_tick + tick_spacing
            };

            // Batch load ticks
            let mut fetched_ticks = Vec::new();
            let mut tick_start = initial_tick_start;
            let mut ticks_loaded = 0u16;

            while ticks_loaded < num_ticks {
                let ticks_to_load = std::cmp::min(TICKS_PER_BATCH as u16, num_ticks - ticks_loaded);

                let (batch_ticks, _) = data_loader
                    .load_tick_data(
                        I24::unchecked_from(tick_start),
                        zero_for_one,
                        ticks_to_load,
                        I24::unchecked_from(tick_spacing),
                        block_number,
                        provider.clone()
                    )
                    .await
                    .expect("Failed to load tick data");

                fetched_ticks.extend(batch_ticks);
                ticks_loaded += ticks_to_load;

                // Update tick_start for next batch
                tick_start = if zero_for_one {
                    tick_start - (ticks_to_load as i32 * tick_spacing)
                } else {
                    tick_start + (ticks_to_load as i32 * tick_spacing)
                };
            }

            // Process ticks into HashMap
            let mut tick_map = HashMap::new();
            let mut tick_bitmap = HashMap::new();

            for tick_data in fetched_ticks.into_iter().filter(|t| t.initialized) {
                let tick_info = TickInfo {
                    initialized:   tick_data.initialized,
                    liquidity_net: tick_data.liquidityNet
                };

                tick_map.insert(tick_data.tick.as_i32(), tick_info);

                // Update tick bitmap
                let tick = tick_data.tick.as_i32();
                let compressed = tick / tick_spacing;
                let word_pos = (compressed >> 8) as i16;
                let bit_pos = (compressed & 0xFF) as u8;

                let word = tick_bitmap.entry(word_pos).or_insert(U256::ZERO);
                let mask = U256::from(1) << bit_pos;

                if *word & mask == U256::ZERO {
                    *word |= mask;
                }
            }

            (pool_id, tick_map, tick_bitmap)
        };

        self.tick_loading.push(Box::pin(future));
    }

    /// Queue a new pool for creation
    pub fn queue_pool_creation(
        &mut self,
        pool_key: PoolKey,
        block: u64,
        bundle_fee: u32,
        swap_fee: u32,
        protocol_fee: u32
    ) {
        // First add to registry
        let pub_key = PoolId::from(pool_key);
        self.registry.pools.insert(pub_key, pool_key);

        // Create private key
        let mut priv_pool_key = pool_key;
        priv_pool_key.fee = U24::from(0x800000);
        let priv_key = PoolId::from(priv_pool_key);
        self.registry.conversion_map.insert(pub_key, priv_key);
        self.registry
            .reverse_conversion_map
            .insert(priv_key, pub_key);

        let internal_pool_id = priv_key;
        let pool_id = pub_key;
        let registry = self.registry.clone();
        let pool_manager = self.pool_manager;
        let provider = self.provider.clone();

        let future = async move {
            let data_loader = DataLoader::new_with_registry(
                internal_pool_id,
                pool_id,
                registry.clone(),
                pool_manager
            );

            // Load basic pool data
            let pool_data = data_loader
                .load_pool_data(Some(block), provider.clone())
                .await
                .map_err(|e| {
                    BaselinePoolFactoryError::PoolDataLoading(format!(
                        "Failed to load pool data: {e}"
                    ))
                })?;

            // Extract basic pool state
            let liquidity = pool_data.liquidity;
            let sqrt_price_x96 = pool_data.sqrtPrice.into();
            let tick = pool_data.tick.as_i32();
            let tick_spacing = pool_data.tickSpacing.as_i32();

            // Load ticks in both directions
            let (ticks, tick_bitmap) = Self::load_tick_data_in_band_static(
                &data_loader,
                tick,
                tick_spacing,
                Some(block),
                provider,
                INITIAL_TICKS_PER_SIDE
            )
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

            // Create fee configuration with provided values
            let fee_config =
                uni_v4_structure::FeeConfiguration { bundle_fee, swap_fee, protocol_fee };

            // Create and return BaselinePoolState with Uniswap pool_id
            Ok((
                internal_pool_id, // Use Uniswap pool ID
                BaselinePoolState::new(
                    baseline_liquidity,
                    block,
                    fee_config,
                    pool_data.tokenA,
                    pool_data.tokenB,
                    pool_data.tokenADecimals,
                    pool_data.tokenBDecimals
                )
            ))
        };

        self.pool_generator.push(Box::pin(future));
    }

    /// Check if a pool needs more ticks loaded and request them if needed
    pub fn check_and_request_ticks_if_needed(
        &mut self,
        pool_id: PoolId,
        pool_state: &BaselinePoolState,
        block_number: Option<u64>
    ) -> bool {
        let baseline = pool_state.get_baseline_liquidity();

        let current_tick = baseline.get_current_tick();
        let tick_spacing = baseline.get_tick_spacing();

        // Get min and max initialized ticks
        let Some(min_tick) = baseline.get_min_initialized_tick() else {
            // No ticks loaded, shouldn't happen
            return false;
        };
        let Some(max_tick) = baseline.get_max_initialized_tick() else {
            // No ticks loaded, shouldn't happen
            return false;
        };

        // Calculate the distance threshold
        let threshold = (tick_spacing * self.tick_edge_threshold as i32).abs();

        let mut requested = false;

        // Check if we need more ticks below current price
        if current_tick - min_tick < threshold {
            // Load more ticks below
            let start_tick = min_tick - tick_spacing;
            self.request_more_ticks(
                pool_id,
                true, // zero_for_one = true means loading ticks below
                start_tick,
                tick_spacing,
                self.tick_edge_threshold,
                block_number
            );
            requested = true;
            tracing::info!(
                "Requesting more ticks below for pool {:?}, current_tick: {}, min_tick: {}, \
                 threshold: {}, loading {} ticks",
                pool_id,
                current_tick,
                min_tick,
                threshold,
                self.tick_edge_threshold
            );
        }

        // Check if we need more ticks above current price
        if max_tick - current_tick < threshold {
            // Load more ticks above
            let start_tick = max_tick + tick_spacing;
            self.request_more_ticks(
                pool_id,
                false, // zero_for_one = false means loading ticks above
                start_tick,
                tick_spacing,
                self.tick_edge_threshold,
                block_number
            );
            requested = true;
            tracing::info!(
                "Requesting more ticks above for pool {:?}, current_tick: {}, max_tick: {}, \
                 threshold: {}, loading {} ticks",
                pool_id,
                current_tick,
                max_tick,
                threshold,
                self.tick_edge_threshold
            );
        }

        requested
    }
}

impl<P: Provider + Clone + Unpin + 'static> Stream for BaselinePoolFactory<P> {
    type Item = UpdateMessage;

    fn poll_next(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>
    ) -> std::task::Poll<Option<Self::Item>> {
        let this = self.get_mut();

        // First, try to poll tick loading futures
        if let std::task::Poll::Ready(Some((pool_id, ticks, tick_bitmap))) =
            this.tick_loading.poll_next_unpin(cx)
        {
            return std::task::Poll::Ready(Some(UpdateMessage::NewTicks(
                pool_id,
                ticks,
                tick_bitmap
            )));
        }

        // Then, try to poll pool generator futures
        while let std::task::Poll::Ready(Some(result)) = this.pool_generator.poll_next_unpin(cx) {
            match result {
                Ok((pool_id, pool_state)) => {
                    return std::task::Poll::Ready(Some(UpdateMessage::NewPool(pool_id, pool_state)))
                }
                Err(e) => {
                    // Log error but continue processing other futures
                    tracing::error!("Failed to generate pool: {}", e);
                    continue;
                }
            }
        }

        // Always return Pending, never None
        std::task::Poll::Pending
    }
}
