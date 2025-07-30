use std::{
    collections::HashSet,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll}
};

use alloy::{primitives::Address, providers::Provider};
use futures::{Future, Stream, StreamExt};
use thiserror::Error;

use super::{
    baseline_pool_factory::{BaselinePoolFactory, BaselinePoolFactoryError, UpdateMessage},
    fetch_pool_keys::set_controller_address,
    pool_key::PoolKey,
    pools::PoolId
};
use crate::{
    pool_providers::PoolEventStream,
    pools::UniswapPools,
    uni_structure::BaselinePoolState,
    uniswap::{
        pool_providers::pool_update_provider::PoolUpdate,
        slot0::{Slot0Stream, Slot0Update}
    }
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
    pending_updates:               Vec<PoolUpdate>
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
        current_block: Option<u64>
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
            filter_pool_keys
        )
        .await;

        let mut service = Self {
            event_stream,
            factory,
            pools: UniswapPools::new(pools, current_block),
            current_block: deploy_block,
            auto_pool_creation,
            slot0_stream,
            pending_updates: Vec::new()
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
            PoolUpdate::PoolConfigured {
                pool_key,
                pool_id,
                bundle_fee,
                swap_fee,
                protocol_fee,
                tick_spacing,
                block,
                ..
            } => {
                if self.auto_pool_creation {
                    self.handle_new_pool(*pool_key, *block, *bundle_fee, *swap_fee, *protocol_fee);

                    tracing::info!(
                        "Pool configured: {:?}, bundle_fee: {}, swap_fee: {}, protocol_fee: {}, \
                         tick_spacing: {}",
                        pool_id,
                        bundle_fee,
                        swap_fee,
                        protocol_fee,
                        tick_spacing
                    );
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
                // Process the update
                match update {
                    UpdateMessage::NewTicks(pool_id, ticks, tick_bitmap) => {
                        // Update the pool's tick data
                        if let Some(mut pool) = this.pools.get_pools().get_mut(&pool_id) {
                            let baseline = pool.get_baseline_liquidity_mut();

                            // Merge new ticks with existing ones
                            baseline.initialized_ticks_mut().extend(ticks);

                            // Update tick bitmap
                            for (word_pos, word) in tick_bitmap {
                                baseline.update_tick_bitmap(word_pos, word);
                            }

                            tracing::info!(
                                "Updated ticks for pool {:?}, total ticks: {}",
                                pool_id,
                                baseline.initialized_ticks_mut().len()
                            );
                        } else {
                            tracing::warn!("Received tick update for unknown pool: {:?}", pool_id);
                        }
                    }
                    UpdateMessage::NewPool(pool_id, pool_state) => {
                        // Add new pool to the pools map
                        this.pools.insert(pool_id, pool_state.clone());
                        this.event_stream.start_tracking_pool(pool_id);

                        // Subscribe new pool to slot0 stream (using angstrom ID)
                        if let Some(slot0_stream) = &mut this.slot0_stream
                            && let Some(angstrom_pool_id) =
                                this.factory.registry().public_key_from_private(&pool_id)
                        {
                            slot0_stream.subscribe_pools(HashSet::from([angstrom_pool_id]));
                        }

                        tracing::info!("Added new pool: {:?}", pool_id);
                    }
                }
            }
            Poll::Ready(None) => {
                // Stream ended, which shouldn't happen in our case.
                return Poll::Ready(());
            }
            _ => {}
        }

        if !this.factory.is_processing() {
            let updates = this.pending_updates.drain(..).collect::<Vec<_>>();
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

        while let Poll::Ready(events) = this.event_stream.poll_next_unpin(cx) {
            if let Some(events) = events {
                if this.factory.is_processing() {
                    this.pending_updates.extend(events);

                    continue;
                }

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
            } else {
                return Poll::Ready(());
            }
        }

        let updates = if let Some(slot0_stream) = this.slot0_stream.as_mut() {
            let mut updates = vec![];
            while let Poll::Ready(Some(update)) = slot0_stream.poll_next_unpin(cx) {
                updates.push(update);
            }
            updates
        } else {
            vec![]
        };

        for update in updates {
            this.slot0_update(update);
        }

        Poll::Pending
    }
}
