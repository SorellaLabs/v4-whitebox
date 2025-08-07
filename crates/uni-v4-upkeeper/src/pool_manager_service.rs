use std::{
    collections::HashSet,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll}
};

use alloy::{primitives::Address, providers::Provider};
use futures::{Future, Stream, StreamExt};
use thiserror::Error;
use tokio::sync::mpsc;
use uni_v4_common::{PoolId, PoolKey, PoolUpdate, Slot0Update, UniswapPools};
use uni_v4_structure::BaselinePoolState;

use super::{
    baseline_pool_factory::{BaselinePoolFactory, BaselinePoolFactoryError, UpdateMessage},
    fetch_pool_keys::set_controller_address
};
use crate::{pool_providers::PoolEventStream, slot0::Slot0Stream};

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
    P: Provider + Unpin + Clone + 'static,
    Event: PoolEventStream
{
    pub(crate) factory:            BaselinePoolFactory<P>,
    pub(crate) event_stream:       Event,
    pub(crate) pools:              UniswapPools,
    pub(crate) current_block:      u64,
    pub(crate) auto_pool_creation: bool,
    pub(crate) slot0_stream:       Option<S>,
    // If we are loading more ticks at a block, we will queue up updates messages here
    // so that we don't hit any race conditions.
    pending_updates:               Vec<PoolUpdate>,
    // Channel for sending updates instead of applying them directly
    update_sender:                 Option<mpsc::Sender<PoolUpdate>>
}

impl<P, Event, S> PoolManagerService<P, Event, S>
where
    P: Provider + Clone + Unpin + 'static,
    Event: PoolEventStream,
    BaselinePoolFactory<P>: Stream<Item = UpdateMessage> + Unpin,
    S: Slot0Stream
{
    /// Create a new PoolManagerService
    #[allow(clippy::too_many_arguments)]
    pub async fn new(
        provider: Arc<P>,
        event_stream: Event,
        angstrom_address: Address,
        controller_address: Address,
        pool_manager_address: Address,
        deploy_block: u64,
        tick_band: Option<u16>,
        tick_edge_threshold: Option<u16>,
        filter_pool_keys: Option<HashSet<PoolKey>>,
        auto_pool_creation: bool,
        slot0_stream: Option<S>,
        current_block: Option<u64>,
        ticks_per_batch: Option<usize>,
        update_channel: Option<mpsc::Sender<PoolUpdate>>
    ) -> Result<Self, PoolManagerServiceError> {
        // Set the controller address for the fetch_pool_keys module
        set_controller_address(controller_address);

        // Use provided current_block or get current block
        let current_block = if let Some(block) = current_block {
            block
        } else {
            provider.get_block_number().await.unwrap()
        };

        // Create factory with optional filtering
        let (factory, pools) = BaselinePoolFactory::new(
            deploy_block,
            current_block,
            angstrom_address,
            provider.clone(),
            pool_manager_address,
            tick_band,
            tick_edge_threshold,
            filter_pool_keys,
            ticks_per_batch
        )
        .await;

        let mut service = Self {
            event_stream,
            factory,
            pools: UniswapPools::new(pools, current_block),
            current_block: deploy_block,
            auto_pool_creation,
            slot0_stream,
            pending_updates: Vec::new(),
            update_sender: update_channel
        };

        service
            .event_stream
            .set_pool_registry(service.factory.registry());

        // Ensure to register the pool_ids with the state stream.
        for pool_id in service.factory.get_uniswap_pool_ids() {
            service.event_stream.start_tracking_pool(pool_id);
        }

        // Subscribe all initial pools to slot0 stream if present (using angstrom IDs)
        if let Some(slot0_stream) = &mut service.slot0_stream {
            let angstrom_pool_ids: HashSet<PoolId> =
                service.factory.registry().public_keys().collect();
            slot0_stream.subscribe_pools(angstrom_pool_ids);
        }

        // Send all initialized pools through the channel on startup
        if service.update_sender.is_some() {
            let initial_pool_updates: Vec<PoolUpdate> = service
                .pools
                .get_pools()
                .iter()
                .map(|entry| PoolUpdate::NewPoolState {
                    pool_id: *entry.key(),
                    state:   entry.value().clone()
                })
                .collect();

            for update in initial_pool_updates {
                service.dispatch_update(update);
            }
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

    /// Handle a new pool creation
    fn handle_new_pool(
        &mut self,
        pool_key: PoolKey,
        block_number: u64,
        bundle_fee: u32,
        swap_fee: u32,
        protocol_fee: u32
    ) {
        self.factory.queue_pool_creation(
            pool_key,
            block_number,
            bundle_fee,
            swap_fee,
            protocol_fee
        );
    }

    /// Dispatch an update either via channel or apply directly
    fn dispatch_update(&mut self, update: PoolUpdate) {
        if let Some(sender) = &self.update_sender {
            // Channel mode: send the update
            if let Err(e) = sender.try_send(update.clone()) {
                tracing::error!("Failed to send update via channel: {}", e);
            }

            // Always process certain critical updates internally even in channel mode
            match &update {
                PoolUpdate::NewBlock(block) => {
                    self.current_block = *block;
                }
                PoolUpdate::NewPool { .. } => {
                    // CRITICAL: Process new pool to ensure it gets created in the factory
                    // This will trigger pool data loading and initialization
                    self.process_pool_update(update);
                }
                PoolUpdate::PoolRemoved { .. } => {
                    // Process pool removal to clean up internal state
                    self.pools.update_pools(vec![update.clone()]);
                    self.process_pool_update(update);
                }
                _ => {
                    // Other updates are just forwarded via channel without
                    // internal processing
                }
            }
        } else {
            // Direct mode: apply the update
            match &update {
                PoolUpdate::NewBlock(block) => {
                    self.current_block = *block;
                }
                _ => {
                    // Let update_pools handle all other updates
                    self.pools.update_pools(vec![update.clone()]);
                    self.process_pool_update(update);
                }
            }
        }
    }

    pub fn slot0_update(&self, update: Slot0Update) {
        if let Some(mut pool) = self.get_pools().get_mut(&update.angstrom_pool_id) {
            pool.value_mut().update_slot0(
                update.tick,
                update.sqrt_price_x96.into(),
                update.liquidity
            );
        }
    }

    /// Process a pool update event from the PoolUpdateProvider
    pub fn process_pool_update(&mut self, update: PoolUpdate) {
        match &update {
            PoolUpdate::NewBlock(block_number) => {
                self.current_block = *block_number;
            }
            PoolUpdate::SwapEvent { pool_id, event, .. } => {
                tracing::debug!("Swap event for pool {:?}: {:?}", pool_id, event);
            }
            PoolUpdate::LiquidityEvent { pool_id, event, .. } => {
                tracing::debug!("Liquidity event for pool {:?}: {:?}", pool_id, event);
            }
            PoolUpdate::NewPool {
                pool_id,
                token0: _,
                token1: _,
                bundle_fee,
                swap_fee,
                protocol_fee,
                tick_spacing,
                block
            } => {
                if self.auto_pool_creation {
                    // Reconstruct pool_key from the NewPool data
                    // We need to get the pool_key from the registry
                    if let Some(pool_key) = self.factory.registry().pools.get(pool_id) {
                        self.handle_new_pool(
                            *pool_key,
                            *block,
                            *bundle_fee,
                            *swap_fee,
                            *protocol_fee
                        );

                        tracing::info!(
                            "Pool configured: {:?}, bundle_fee: {}, swap_fee: {}, protocol_fee: \
                             {}, tick_spacing: {}",
                            pool_id,
                            bundle_fee,
                            swap_fee,
                            protocol_fee,
                            tick_spacing
                        );
                    } else {
                        tracing::warn!("Pool {:?} not found in registry", pool_id);
                    }
                } else {
                    tracing::info!(
                        "Ignoring pool configured event (auto creation disabled): {:?}",
                        pool_id
                    );
                }
            }
            PoolUpdate::PoolRemoved { pool_id, .. } => {
                tracing::info!("Pool removed: {:?}", pool_id);
                self.pools.remove(pool_id);
                self.factory.remove_pool_by_id(*pool_id);

                // Unsubscribe pool from slot0 stream (pool_id here is already angstrom ID)
                if let Some(slot0_stream) = &mut self.slot0_stream {
                    slot0_stream.unsubscribe_pools(HashSet::from([*pool_id]));
                }
            }
            PoolUpdate::FeeUpdate { pool_id, bundle_fee, swap_fee, protocol_fee, .. } => {
                if let Some(mut pool) = self.pools.get_pools().get_mut(pool_id) {
                    let fees = pool.fees_mut();
                    fees.bundle_fee = *bundle_fee;
                    fees.swap_fee = *swap_fee;
                    fees.protocol_fee = *protocol_fee;

                    tracing::info!(
                        "Updated fees for pool {:?}: bundle_fee: {}, swap_fee: {}, protocol_fee: \
                         {}",
                        pool_id,
                        bundle_fee,
                        swap_fee,
                        protocol_fee
                    );
                } else {
                    tracing::warn!("Received fee update for unknown pool: {:?}", pool_id);
                }
            }
            PoolUpdate::UpdatedSlot0 { pool_id, data } => {
                tracing::debug!("Updated slot0 for pool {:?}: {:?}", pool_id, data);
            }
            PoolUpdate::Reorg { from_block, to_block } => {
                tracing::warn!("Reorg detected from block {} to {}", from_block, to_block);
            }
            PoolUpdate::NewPoolState { pool_id, state: _ } => {
                // This comes from the factory - just track the pool
                self.event_stream.start_tracking_pool(*pool_id);

                // Subscribe new pool to slot0 stream (using angstrom ID)
                if let Some(slot0_stream) = &mut self.slot0_stream
                    && let Some(angstrom_pool_id) =
                        self.factory.registry().public_key_from_private(pool_id)
                {
                    slot0_stream.subscribe_pools(HashSet::from([angstrom_pool_id]));
                }

                tracing::info!("Tracking new pool from factory: {:?}", pool_id);
            }
            PoolUpdate::NewTicks { .. } => {
                // These are handled by update_pools
                tracing::debug!("NewTicks update will be handled by update_pools");
            }
            PoolUpdate::Slot0Update(update) => {
                self.slot0_update(update.clone());
            }
        }
    }
}

impl<P, Event, S> Future for PoolManagerService<P, Event, S>
where
    P: Provider + Clone + Unpin + 'static,
    Event: PoolEventStream,
    BaselinePoolFactory<P>: Stream<Item = UpdateMessage> + Unpin,
    S: Slot0Stream
{
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        // Continuously poll the factory stream
        let this = self.get_mut();
        match this.factory.poll_next_unpin(cx) {
            Poll::Ready(Some(update)) => {
                // Convert factory update to PoolUpdate and dispatch
                let pool_update = match update {
                    UpdateMessage::NewTicks(pool_id, ticks, tick_bitmap) => {
                        PoolUpdate::NewTicks { pool_id, ticks, tick_bitmap }
                    }
                    UpdateMessage::NewPool(pool_id, state) => {
                        PoolUpdate::NewPoolState { pool_id, state }
                    }
                };
                this.dispatch_update(pool_update);
            }
            Poll::Ready(None) => {
                // Stream ended, which shouldn't happen in our case.
                return Poll::Ready(());
            }
            _ => {}
        }

        if !this.factory.is_processing() {
            let updates = this.pending_updates.drain(..).collect::<Vec<_>>();

            if this.update_sender.is_some() {
                // Channel mode: dispatch each update
                for event in updates {
                    this.dispatch_update(event);
                }
            } else {
                // Direct mode: apply updates and check tick ranges
                this.pools.update_pools(updates.clone());
                for event in updates {
                    this.process_pool_update(event);
                }

                // Check tick ranges for all pools after updates
                for entry in this.pools.get_pools().iter() {
                    this.factory.check_and_request_ticks_if_needed(
                        *entry.key(),
                        entry.value(),
                        Some(this.current_block)
                    );
                }
            }
        }

        while let Poll::Ready(events) = this.event_stream.poll_next_unpin(cx) {
            if let Some(events) = events {
                if this.factory.is_processing() {
                    this.pending_updates.extend(events);
                    continue;
                }

                if this.update_sender.is_some() {
                    // Channel mode: dispatch each update
                    for event in events {
                        this.dispatch_update(event);
                    }
                } else {
                    // Direct mode: apply updates and check tick ranges
                    this.pools.update_pools(events.clone());
                    for event in events {
                        this.process_pool_update(event);
                    }
                    // Check tick ranges for all pools after updates
                    for entry in this.pools.get_pools().iter() {
                        this.factory.check_and_request_ticks_if_needed(
                            *entry.key(),
                            entry.value(),
                            Some(this.current_block)
                        );
                    }
                }
            } else {
                return Poll::Ready(());
            }
        }

        if let Some(slot0_stream) = this.slot0_stream.as_mut() {
            let mut slot0_updates = Vec::new();
            while let Poll::Ready(Some(update)) = slot0_stream.poll_next_unpin(cx) {
                slot0_updates.push(update);
            }
            for update in slot0_updates {
                let pool_update = PoolUpdate::Slot0Update(update);
                this.dispatch_update(pool_update);
            }
        }

        Poll::Pending
    }
}
