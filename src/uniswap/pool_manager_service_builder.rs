use std::{collections::HashMap, sync::Arc};

use alloy::{
    primitives::{Address, U160},
    providers::Provider
};
use futures::Stream;
use serde::{Deserialize, Serialize};

use super::{
    baseline_pool_factory::BaselinePoolFactory,
    fetch_pool_keys::set_controller_address,
    pool_data_loader::DataLoader,
    pool_key::PoolKey,
    pool_manager_service::{PoolManagerService, PoolManagerServiceError},
    pool_registry::UniswapPoolRegistry,
    pools::PoolId
};
use crate::{pool_providers::PoolEventStream, pools::UniswapPools};

/// Update for slot0 data of a pool
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Hash)]
pub struct Slot0Update {
    /// there will be 120 updates per block or per 100ms
    pub seq_id:         u16,
    /// in case of block lag on node
    pub current_block:  u64,
    /// basic identifier
    pub pool_id:        PoolId,
    pub sqrt_price_x96: U160,
    pub tick:           i32
}

/// Builder for creating a configured PoolManagerService
pub struct PoolManagerServiceBuilder<P, Event, S>
where
    P: Provider + Clone + 'static
{
    // Required fields
    provider:             Arc<P>,
    angstrom_address:     Address,
    controller_address:   Address,
    pool_manager_address: Address,
    deploy_block:         u64,

    // Optional fields with defaults
    is_bundle_mode:          bool,
    event_stream:            Option<Event>,
    initial_tick_range_size: Option<u16>,
    fixed_pools:             Option<Vec<PoolKey>>,
    auto_pool_creation:      bool,
    slot0_stream:            Option<S>
}

impl<P> PoolManagerServiceBuilder<P, (), ()>
where
    P: Provider + Clone + 'static
{
    /// Create a new builder with required parameters
    pub fn new(
        provider: Arc<P>,
        angstrom_address: Address,
        controller_address: Address,
        pool_manager_address: Address,
        deploy_block: u64
    ) -> Self {
        Self {
            provider,
            angstrom_address,
            controller_address,
            pool_manager_address,
            deploy_block,
            is_bundle_mode: false,
            event_stream: None,
            initial_tick_range_size: None,
            fixed_pools: None,
            auto_pool_creation: true,
            slot0_stream: None
        }
    }
}

impl<P, Event, S> PoolManagerServiceBuilder<P, Event, S>
where
    P: Provider + Clone + 'static
{
    /// Set the event stream for pool updates
    pub fn with_event_stream<NewEvent: PoolEventStream>(
        self,
        event_stream: NewEvent
    ) -> PoolManagerServiceBuilder<P, NewEvent, S> {
        PoolManagerServiceBuilder {
            provider:                self.provider,
            angstrom_address:        self.angstrom_address,
            controller_address:      self.controller_address,
            pool_manager_address:    self.pool_manager_address,
            deploy_block:            self.deploy_block,
            is_bundle_mode:          self.is_bundle_mode,
            event_stream:            Some(event_stream),
            initial_tick_range_size: self.initial_tick_range_size,
            fixed_pools:             self.fixed_pools,
            auto_pool_creation:      self.auto_pool_creation,
            slot0_stream:            self.slot0_stream
        }
    }

    /// Set the initial tick range size for loading pool data
    pub fn with_initial_tick_range_size(mut self, size: u16) -> Self {
        self.initial_tick_range_size = Some(size);
        self
    }

    /// Set a fixed set of pools to load (disables auto-discovery)
    pub fn with_fixed_pools(mut self, pools: Vec<PoolKey>) -> Self {
        self.fixed_pools = Some(pools);
        self
    }

    /// Enable or disable automatic pool creation
    pub fn with_auto_pool_creation(mut self, enabled: bool) -> Self {
        self.auto_pool_creation = enabled;
        self
    }

    /// Set the slot0 update stream
    pub fn with_slot0_stream<NewS: Stream<Item = Slot0Update> + Send + 'static>(
        self,
        stream: NewS
    ) -> PoolManagerServiceBuilder<P, Event, NewS> {
        PoolManagerServiceBuilder {
            provider:                self.provider,
            angstrom_address:        self.angstrom_address,
            controller_address:      self.controller_address,
            pool_manager_address:    self.pool_manager_address,
            deploy_block:            self.deploy_block,
            is_bundle_mode:          self.is_bundle_mode,
            event_stream:            self.event_stream,
            initial_tick_range_size: self.initial_tick_range_size,
            fixed_pools:             self.fixed_pools,
            auto_pool_creation:      self.auto_pool_creation,
            slot0_stream:            Some(stream)
        }
    }

    /// Set bundle mode
    pub fn with_bundle_mode(mut self, enabled: bool) -> Self {
        self.is_bundle_mode = enabled;
        self
    }

    /// Build the PoolManagerService with the configured options
    pub async fn build(self) -> Result<PoolManagerService<P, Event, S>, PoolManagerServiceError>
    where
        Event: PoolEventStream + Default,
        S: Stream<Item = Slot0Update> + Send + 'static,
        DataLoader: super::pool_data_loader::PoolDataLoader
    {
        // Set the controller address for the fetch_pool_keys module
        set_controller_address(self.controller_address);

        // Get current block number
        let current_block = self.provider.get_block_number().await.unwrap();

        // Create registry and factory
        let registry = UniswapPoolRegistry::default();
        let tick_range_size = self.initial_tick_range_size.unwrap_or(400);
        let (factory, pools) = BaselinePoolFactory::new(
            self.deploy_block,
            current_block,
            self.angstrom_address,
            self.provider.clone(),
            registry,
            self.pool_manager_address,
            Some(tick_range_size)
        )
        .await;

        // Get event stream or create default
        let event_stream = self.event_stream.unwrap_or_default();

        // Create service
        let mut service = PoolManagerService {
            factory,
            event_stream,
            provider: self.provider.clone(),
            angstrom_address: self.angstrom_address,
            controller_address: self.controller_address,
            deploy_block: self.deploy_block,
            pools: UniswapPools::new(pools, current_block),
            current_block: self.deploy_block,
            is_bundle_mode: self.is_bundle_mode,
            auto_pool_creation: self.auto_pool_creation,
            slot0_stream: self.slot0_stream,
            initial_tick_range_size: tick_range_size
        };

        // Initialize with either fixed pools or discovered pools
        if let Some(fixed_pools) = self.fixed_pools {
            // TODO: Implement initialize_with_fixed_pools
            // service.initialize_with_fixed_pools(fixed_pools).await?;
        } else {
            // TODO: Implement initialize
            // service.initialize().await?;
        }

        // Register pools with event stream
        for pool_id in service.factory.get_uniswap_pool_ids() {
            service.event_stream.start_tracking_pool(pool_id);
        }

        Ok(service)
    }
}

// Helper impl for builders without event stream
impl<P> PoolManagerServiceBuilder<P, (), ()>
where
    P: Provider + Clone + 'static
{
    /// Build without an event stream (creates a no-op stream)
    pub async fn build_without_event_stream(
        self
    ) -> Result<PoolManagerService<P, NoOpEventStream, NoOpSlot0Stream>, PoolManagerServiceError>
    where
        DataLoader: super::pool_data_loader::PoolDataLoader
    {
        let builder = self
            .with_event_stream(NoOpEventStream)
            .with_slot0_stream(NoOpSlot0Stream);
        builder.build().await
    }
}

/// A no-op event stream implementation for when no event stream is needed
pub struct NoOpEventStream;

impl PoolEventStream for NoOpEventStream {
    fn start_tracking_pool(&mut self, _pool_id: PoolId) {}

    fn stop_tracking_pool(&mut self, _pool_id: PoolId) {}

    fn set_pool_registry(&mut self, _pool_registry: UniswapPoolRegistry) {}
}

impl Default for NoOpEventStream {
    fn default() -> Self {
        Self
    }
}

impl Stream for NoOpEventStream {
    type Item = Vec<crate::uniswap::pool_providers::pool_update_provider::PoolUpdate>;

    fn poll_next(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>
    ) -> std::task::Poll<Option<Self::Item>> {
        std::task::Poll::Pending
    }
}

/// A no-op slot0 stream implementation for when no slot0 stream is needed
pub struct NoOpSlot0Stream;

impl Stream for NoOpSlot0Stream {
    type Item = Slot0Update;

    fn poll_next(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>
    ) -> std::task::Poll<Option<Self::Item>> {
        std::task::Poll::Pending
    }
}
