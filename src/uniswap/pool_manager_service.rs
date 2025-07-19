use std::{collections::HashMap, sync::Arc};

use alloy::{primitives::Address, providers::Provider};
use thiserror::Error;

use super::{
    baseline_pool_factory::{BaselinePoolFactory, BaselinePoolFactoryError},
    fetch_pool_keys::{fetch_angstrom_pools, set_controller_address},
    pool_data_loader::DataLoader,
    pool_key::PoolKey,
    pools::PoolId
};
use crate::{
    pool_providers::PoolEventStream, pools::UniswapPools, uni_structure::BaselinePoolState,
    uniswap::pool_providers::pool_update_provider::PoolUpdate
};

/// Pool information combining BaselinePoolState with token metadata
#[derive(Debug, Clone)]
pub struct PoolInfo {
    pub baseline_state:  BaselinePoolState,
    pub token0:          Address,
    pub token1:          Address,
    pub token0_decimals: u8,
    pub token1_decimals: u8
}

#[derive(Error, Debug)]
pub enum PoolManagerServiceError {
    #[error("Provider error: {0}")]
    Provider(String),
    #[error("Pool initialization error: {0}")]
    PoolInit(String),
    #[error("Pool factory error: {0}")]
    PoolFactory(String),
    #[error("Baseline pool factory error: {0}")]
    BaselineFactory(#[from] BaselinePoolFactoryError)
}

/// Service for managing Uniswap V4 pools with real-time block subscription
/// updates
pub struct PoolManagerService<P, Event, S = ()>
where
    P: Provider + Clone + 'static,
    Event: PoolEventStream
{
    pub(crate) factory:                 BaselinePoolFactory<P>,
    pub(crate) event_stream:            Event,
    pub(crate) provider:                Arc<P>,
    pub(crate) angstrom_address:        Address,
    pub(crate) controller_address:      Address,
    pub(crate) deploy_block:            u64,
    pub(crate) pools:                   UniswapPools,
    pub(crate) current_block:           u64,
    pub(crate) is_bundle_mode:          bool,
    pub(crate) auto_pool_creation:      bool,
    pub(crate) slot0_stream:            Option<S>,
    pub(crate) initial_tick_range_size: u16
}

impl<P, Event, S> PoolManagerService<P, Event, S>
where
    P: Provider + Clone + 'static,
    DataLoader: super::pool_data_loader::PoolDataLoader,
    Event: PoolEventStream
{
    /// Create a new PoolManagerService and initialize it with existing pools
    pub async fn new(
        provider: Arc<P>,
        event_stream: Event,
        angstrom_address: Address,
        controller_address: Address,
        pool_manager_address: Address,
        deploy_block: u64,
        is_bundle_mode: bool,
        tick_band: Option<u16>
    ) -> Result<Self, PoolManagerServiceError> {
        // Set the controller address for the fetch_pool_keys module
        set_controller_address(controller_address);
        let current_block = provider.get_block_number().await.unwrap();

        // Create an empty registry for the factory - we'll populate it during
        // initialization
        let registry = super::pool_registry::UniswapPoolRegistry::default();
        let (factory, pools) = BaselinePoolFactory::new(
            deploy_block,
            current_block,
            angstrom_address,
            provider.clone(),
            registry,
            pool_manager_address,
            tick_band
        )
        .await;

        let mut service = Self {
            event_stream,
            factory,
            provider,
            angstrom_address,
            controller_address,
            deploy_block,
            pools: UniswapPools::new(pools, current_block),
            current_block: deploy_block,
            is_bundle_mode,
            auto_pool_creation: true,
            slot0_stream: None,
            initial_tick_range_size: 400
        };

        service
            .event_stream
            .set_pool_registry(service.factory.registry());

        // Ensure to register the pool_ids with the state stream.
        for pool_id in service.factory.get_uniswap_pool_ids() {
            service.event_stream.start_tracking_pool(pool_id);
        }

        Ok(service)
    }

    /// Get all currently tracked pools
    pub fn get_pools(&self) -> UniswapPools {
        self.pools.clone()
    }

    /// Get the current block number being processed
    pub fn current_block(&self) -> u64 {
        self.current_block
    }

    /// Get all current pool keys
    pub fn current_pool_keys(&self) -> Vec<PoolKey> {
        self.factory.current_pool_keys()
    }

    /// Handle a new block by updating pool keys and creating new pools as
    /// needed
    pub async fn handle_new_block(
        &mut self,
        block_number: u64
    ) -> Result<(), PoolManagerServiceError> {
        if block_number <= self.current_block {
            // Skip old blocks
            return Ok(());
        }

        tracing::debug!("Processing block {}", block_number);

        // Fetch updated pool keys from the last processed block to current block
        let updated_pool_keys = fetch_angstrom_pools(
            self.current_block as usize + 1,
            block_number as usize,
            self.angstrom_address,
            self.provider.as_ref()
        )
        .await;

        if !updated_pool_keys.is_empty() {
            tracing::info!(
                "Found {} pool updates in block {}",
                updated_pool_keys.len(),
                block_number
            );

            // Process pool changes
            for pool_key in updated_pool_keys {
                let pool_id = PoolId::from(pool_key);

                // Check if this is a new pool and auto_pool_creation is enabled
                if !self.pools.contains_key(&pool_id) && self.auto_pool_creation {
                    if let Err(e) = self.handle_new_pool(pool_key, block_number).await {
                        tracing::error!("Failed to create new pool {:?}: {}", pool_id, e);
                    }
                } else if !self.auto_pool_creation && !self.pools.contains_key(&pool_id) {
                    tracing::debug!(
                        "Skipping new pool {:?} - auto pool creation disabled",
                        pool_id
                    );
                }
            }
        }

        self.current_block = block_number;
        Ok(())
    }

    /// Process a pool update event from the PoolUpdateProvider
    pub async fn process_pool_update(
        &mut self,
        update: PoolUpdate
    ) -> Result<(), PoolManagerServiceError> {
        match update {
            PoolUpdate::NewBlock(block_number) => {
                self.current_block = block_number;
            }
            PoolUpdate::SwapEvent { pool_id, event, .. } => {
                // TODO: Apply swap event to pool state
                // This should update the pool's current price, tick, and liquidity
                tracing::debug!("Swap event for pool {:?}: {:?}", pool_id, event);
            }
            PoolUpdate::LiquidityEvent { pool_id, event, .. } => {
                // TODO: Apply liquidity event to pool state
                // This should modify the pool's liquidity at the specified tick range
                tracing::debug!("Liquidity event for pool {:?}: {:?}", pool_id, event);
            }
            PoolUpdate::PoolConfigured {
                pool_id,
                bundle_fee,
                swap_fee,
                protocol_fee,
                tick_spacing,
                ..
            } => {
                // TODO: Create new pool with proper fee configuration
                // fee_config = FeeConfiguration {
                //     is_bundle_mode: determine based on context,
                //     bundle_fee,
                //     swap_fee,
                //     protocol_fee
                // }
                tracing::info!(
                    "Pool configured: {:?}, bundle_fee: {}, swap_fee: {}, protocol_fee: {}, \
                     tick_spacing: {}",
                    pool_id,
                    bundle_fee,
                    swap_fee,
                    protocol_fee,
                    tick_spacing
                );
            }
            PoolUpdate::PoolRemoved { pool_id, .. } => {
                // TODO: Remove pool from tracking
                tracing::info!("Pool removed: {:?}", pool_id);
                self.pools.remove(&pool_id);
            }
            PoolUpdate::FeeUpdate { pool_id, bundle_fee, swap_fee, protocol_fee, .. } => {
                // TODO: Update existing pool's fee configuration
                tracing::info!(
                    "Fee update for pool {:?}: bundle_fee: {}, swap_fee: {}, protocol_fee: {}",
                    pool_id,
                    bundle_fee,
                    swap_fee,
                    protocol_fee
                );
            }
            PoolUpdate::UpdatedSlot0 { pool_id, data } => {
                // TODO: Update pool state from slot0 data after reorg
                tracing::debug!("Updated slot0 for pool {:?}: {:?}", pool_id, data);
            }
            PoolUpdate::Reorg { from_block, to_block } => {
                // TODO: Handle reorg - may need to reload pool states
                tracing::warn!("Reorg detected from block {} to {}", from_block, to_block);
            }
        }
        Ok(())
    }
}
