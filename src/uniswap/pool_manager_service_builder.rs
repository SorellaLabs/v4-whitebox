use std::{collections::HashSet, sync::Arc};

use alloy::{primitives::Address, providers::Provider};
use futures::Stream;

use super::{
    pool_data_loader::DataLoader,
    pool_key::PoolKey,
    pool_manager_service::{PoolManagerService, PoolManagerServiceError},
    pool_registry::UniswapPoolRegistry,
    pools::PoolId,
    slot0::Slot0Stream
};
use crate::pool_providers::PoolEventStream;

/// Builder for creating a configured PoolManagerService
pub struct PoolManagerServiceBuilder<P, Event, S = NoOpSlot0Stream>
where
    P: Provider + Clone + 'static
{
    // Required fields
    provider:             Arc<P>,
    angstrom_address:     Address,
    controller_address:   Address,
    pool_manager_address: Address,
    deploy_block:         u64,
    event_stream:         Event,

    // Optional fields with defaults
    initial_tick_range_size: Option<u16>,
    tick_edge_threshold:     Option<u16>,
    fixed_pools:             Option<HashSet<PoolKey>>,
    auto_pool_creation:      bool,
    slot0_stream:            Option<S>,
    current_block:           Option<u64>
}

impl<P, Event, Slot0> PoolManagerServiceBuilder<P, Event, Slot0>
where
    P: Provider + Clone + 'static,
    Event: PoolEventStream
{
    /// Create a new builder with required parameters including event stream
    pub fn new(
        provider: Arc<P>,
        angstrom_address: Address,
        controller_address: Address,
        pool_manager_address: Address,
        deploy_block: u64,
        event_stream: Event
    ) -> Self {
        Self {
            provider,
            angstrom_address,
            controller_address,
            pool_manager_address,
            deploy_block,
            event_stream,
            initial_tick_range_size: None,
            tick_edge_threshold: None,
            fixed_pools: None,
            auto_pool_creation: true,
            slot0_stream: None,
            current_block: None
        }
    }
}

impl<P, Event, S> PoolManagerServiceBuilder<P, Event, S>
where
    P: Provider + Clone + 'static + Unpin
{
    /// Set the initial tick range size for loading pool data
    pub fn with_initial_tick_range_size(mut self, size: u16) -> Self {
        self.initial_tick_range_size = Some(size);
        self
    }

    /// Set the tick edge threshold for when to load more ticks
    pub fn with_tick_edge_threshold(mut self, threshold: u16) -> Self {
        self.tick_edge_threshold = Some(threshold);
        self
    }

    /// Set a fixed set of pools to load (disables auto-discovery)
    pub fn with_fixed_pools(mut self, pools: Vec<PoolKey>) -> Self {
        self.fixed_pools = Some(pools.into_iter().collect());
        self
    }

    /// Enable or disable automatic pool creation
    pub fn with_auto_pool_creation(mut self, enabled: bool) -> Self {
        self.auto_pool_creation = enabled;
        self
    }

    /// Set the current block to load pools at
    pub fn with_current_block(mut self, block: u64) -> Self {
        self.current_block = Some(block);
        self
    }

    /// Set the slot0 update stream
    pub fn with_slot0_stream<NewS: Slot0Stream + 'static>(
        self,
        stream: NewS
    ) -> PoolManagerServiceBuilder<P, Event, NewS> {
        PoolManagerServiceBuilder {
            provider:                self.provider,
            angstrom_address:        self.angstrom_address,
            controller_address:      self.controller_address,
            pool_manager_address:    self.pool_manager_address,
            deploy_block:            self.deploy_block,
            event_stream:            self.event_stream,
            initial_tick_range_size: self.initial_tick_range_size,
            tick_edge_threshold:     self.tick_edge_threshold,
            fixed_pools:             self.fixed_pools,
            auto_pool_creation:      self.auto_pool_creation,
            slot0_stream:            Some(stream),
            current_block:           self.current_block
        }
    }

    /// Build the PoolManagerService with the configured options
    pub async fn build(self) -> Result<PoolManagerService<P, Event, S>, PoolManagerServiceError>
    where
        Event: PoolEventStream,
        S: Slot0Stream + 'static,
        DataLoader: super::pool_data_loader::PoolDataLoader
    {
        // Use the required event stream
        let event_stream = self.event_stream;

        // Create service using the consolidated new method
        let service = PoolManagerService::new(
            self.provider.clone(),
            event_stream,
            self.angstrom_address,
            self.controller_address,
            self.pool_manager_address,
            self.deploy_block,
            self.initial_tick_range_size,
            self.tick_edge_threshold,
            self.fixed_pools,
            self.auto_pool_creation,
            self.slot0_stream,
            self.current_block
        )
        .await?;

        Ok(service)
    }
}

// Helper method for creating a builder with no-op streams
impl<P> PoolManagerServiceBuilder<P, NoOpEventStream, NoOpSlot0Stream>
where
    P: Provider + Clone + 'static + Unpin
{
    /// Create a new builder with no-op event stream and slot0 stream
    pub fn new_with_noop_stream(
        provider: Arc<P>,
        angstrom_address: Address,
        controller_address: Address,
        pool_manager_address: Address,
        deploy_block: u64
    ) -> Self {
        let builder = PoolManagerServiceBuilder::<P, NoOpEventStream, NoOpSlot0Stream>::new(
            provider,
            angstrom_address,
            controller_address,
            pool_manager_address,
            deploy_block,
            NoOpEventStream
        );
        builder.with_slot0_stream(NoOpSlot0Stream::default())
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

/// Re-export NoOpSlot0Stream from slot0 module
pub use super::slot0::NoOpSlot0Stream;
