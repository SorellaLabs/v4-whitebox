use std::{
    collections::{HashMap, HashSet, VecDeque},
    ops::RangeInclusive,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
    time::Duration
};

use alloy::{
    consensus::BlockHeader,
    eips::BlockId,
    primitives::{Address, I256, Log, U160, U256},
    providers::{Provider, ProviderBuilder, WatchTxError},
    pubsub::PubSubFrontend,
    rpc::types::{Block, Filter},
    sol_types::SolEvent
};
use futures::{
    FutureExt, StreamExt,
    future::BoxFuture,
    stream::{BoxStream, Stream}
};
use thiserror::Error;

use crate::{
    pool_providers::completed_block_stream::CompletedBlockStream,
    uniswap::{
        pool::PoolId,
        pool_data_loader::{DataLoader, IUniswapV4Pool, PoolDataLoader},
        pool_registry::UniswapPoolRegistry
    }
};

#[derive(Debug, Error)]
pub enum PoolUpdateError {
    #[error("Provider error: {0}")]
    Provider(String),
    #[error("Event decode error: {0}")]
    EventDecode(String),
    #[error("Reorg handling error: {0}")]
    ReorgHandling(String)
}

/// Represents different types of pool updates
#[derive(Debug, Clone)]
pub enum PoolUpdate {
    /// New block has been processed
    NewBlock(u64),
    /// Swap event occurred
    SwapEvent {
        pool_id:   PoolId,
        block:     u64,
        tx_index:  u64,
        log_index: u64,
        event:     SwapEventData
    },
    /// Liquidity modification event occurred
    LiquidityEvent {
        pool_id:   PoolId,
        block:     u64,
        tx_index:  u64,
        log_index: u64,
        event:     ModifyLiquidityEventData
    },
    /// Reorg detected
    Reorg { from_block: u64, to_block: u64 },
    /// Updated slot0 data after reorg
    UpdatedSlot0 { pool_id: PoolId, data: Slot0Data }
}

/// Swap event data
#[derive(Debug, Clone)]
pub struct SwapEventData {
    pub sender:         Address,
    pub amount0:        i128,
    pub amount1:        i128,
    pub sqrt_price_x96: U160,
    pub liquidity:      u128,
    pub tick:           i32,
    pub fee:            u32
}

/// Modify liquidity event data
#[derive(Debug, Clone)]
pub struct ModifyLiquidityEventData {
    pub sender:          Address,
    pub tick_lower:      i32,
    pub tick_upper:      i32,
    pub liquidity_delta: I256,
    pub salt:            [u8; 32]
}

/// Current slot0 data for a pool
#[derive(Debug, Clone)]
pub struct Slot0Data {
    pub sqrt_price_x96: U160,
    pub tick:           i32,
    pub liquidity:      u128
}

/// Stored event for reorg handling
#[derive(Debug, Clone)]
struct StoredEvent {
    block:     u64,
    tx_index:  u64,
    log_index: u64,
    pool_id:   PoolId,
    event:     EventType
}

#[derive(Debug, Clone)]
enum EventType {
    Swap(SwapEventData),
    ModifyLiquidity(ModifyLiquidityEventData)
}

/// Pool update provider that streams pool state changes
pub struct PoolUpdateProvider<P>
where
    P: Provider + 'static
{
    provider:             Arc<P>,
    pool_manager:         Address,
    pool_registry:        UniswapPoolRegistry,
    tracked_pools:        HashSet<PoolId>,
    event_history:        VecDeque<StoredEvent>,
    event_history_size:   usize,
    current_block:        u64,
    last_processed_block: Option<u64>
}

impl<P> PoolUpdateProvider<P>
where
    P: Provider + 'static
{
    /// Create a new pool update provider
    pub async fn new(
        provider: Arc<P>,
        pool_manager: Address,
        pool_registry: UniswapPoolRegistry,
        event_history_size: usize
    ) -> Self {
        let current_block = provider
            .get_block(BlockId::Number(alloy::eips::BlockNumberOrTag::Latest))
            .await
            .unwrap()
            .unwrap();

        let block_stream = CompletedBlockStream::new(
            current_block.header.parent_hash,
            current_block.number(),
            provider.clone(),
            provider
                .subscribe_full_blocks()
                .channel_size(5)
                .into_stream()
                .await
                .unwrap()
                .map(|block| block.unwrap())
                .boxed()
        );

        Self {
            provider,
            pool_manager,
            pool_registry,
            tracked_pools: HashSet::new(),
            event_history: VecDeque::with_capacity(event_history_size),
            event_history_size,
            current_block: 0,
            last_processed_block: None
        }
    }

    /// Add a pool to track
    pub fn add_pool(&mut self, pool_id: PoolId) {
        self.tracked_pools.insert(pool_id);
    }

    /// Remove a pool from tracking
    pub fn remove_pool(&mut self, pool_id: PoolId) {
        self.tracked_pools.remove(&pool_id);
    }

    /// Get all tracked pool IDs
    pub fn tracked_pools(&self) -> Vec<PoolId> {
        self.tracked_pools.iter().copied().collect()
    }

    /// Process events for a specific block
    async fn process_block_events(
        &mut self,
        block_number: u64
    ) -> Result<Vec<PoolUpdate>, PoolUpdateError> {
        let mut updates = Vec::new();

        // If no pools are tracked, return early
        if self.tracked_pools.is_empty() {
            return Ok(updates);
        }

        // Create filters with pool ID topics for both event types
        let pool_topics: Vec<_> = self
            .tracked_pools
            .iter()
            .map(|pool_id| pool_id.0.into())
            .collect();

        // Create filter for Swap events
        let swap_filter = Filter::new()
            .address(self.pool_manager)
            .event_signature(IUniswapV4Pool::Swap::SIGNATURE_HASH)
            .topic1(pool_topics.clone())
            .from_block(block_number)
            .to_block(block_number);

        // Create filter for ModifyLiquidity events
        let modify_filter = Filter::new()
            .address(self.pool_manager)
            .event_signature(IUniswapV4Pool::ModifyLiquidity::SIGNATURE_HASH)
            .topic1(pool_topics)
            .from_block(block_number)
            .to_block(block_number);

        // Get swap logs
        let swap_logs =
            self.provider.get_logs(&swap_filter).await.map_err(|e| {
                PoolUpdateError::Provider(format!("Failed to get swap logs: {}", e))
            })?;

        // Get modify liquidity logs
        let modify_logs =
            self.provider.get_logs(&modify_filter).await.map_err(|e| {
                PoolUpdateError::Provider(format!("Failed to get modify logs: {}", e))
            })?;

        // Process swap logs
        for log in swap_logs {
            if let Ok(swap_event) = IUniswapV4Pool::Swap::decode_log(&log.inner) {
                // Double-check the pool ID (should already be filtered)
                if self.tracked_pools.contains(&swap_event.id) {
                    let event_data = SwapEventData {
                        sender:         swap_event.sender,
                        amount0:        swap_event.amount0,
                        amount1:        swap_event.amount1,
                        sqrt_price_x96: swap_event.sqrtPriceX96,
                        liquidity:      swap_event.liquidity,
                        tick:           swap_event.tick.as_i32(),
                        fee:            swap_event.fee.to()
                    };

                    // Store in history
                    self.add_to_history(StoredEvent {
                        block:     block_number,
                        tx_index:  log.transaction_index.unwrap_or(0),
                        log_index: log.log_index.unwrap_or(0),
                        pool_id:   swap_event.id,
                        event:     EventType::Swap(event_data.clone())
                    });

                    updates.push(PoolUpdate::SwapEvent {
                        pool_id:   swap_event.id,
                        block:     block_number,
                        tx_index:  log.transaction_index.unwrap_or(0),
                        log_index: log.log_index.unwrap_or(0),
                        event:     event_data
                    });
                }
            }
        }

        // Process modify liquidity logs
        for log in modify_logs {
            if let Ok(modify_event) = IUniswapV4Pool::ModifyLiquidity::decode_log(&log.inner) {
                // Double-check the pool ID (should already be filtered)
                if self.tracked_pools.contains(&modify_event.id) {
                    let event_data = ModifyLiquidityEventData {
                        sender:          modify_event.sender,
                        tick_lower:      modify_event.tickLower.as_i32(),
                        tick_upper:      modify_event.tickUpper.as_i32(),
                        liquidity_delta: modify_event.liquidityDelta,
                        salt:            modify_event.salt.0
                    };

                    // Store in history
                    self.add_to_history(StoredEvent {
                        block:     block_number,
                        tx_index:  log.transaction_index.unwrap_or(0),
                        log_index: log.log_index.unwrap_or(0),
                        pool_id:   modify_event.id,
                        event:     EventType::ModifyLiquidity(event_data.clone())
                    });

                    updates.push(PoolUpdate::LiquidityEvent {
                        pool_id:   modify_event.id,
                        block:     block_number,
                        tx_index:  log.transaction_index.unwrap_or(0),
                        log_index: log.log_index.unwrap_or(0),
                        event:     event_data
                    });
                }
            }
        }

        Ok(updates)
    }

    /// Handle a reorg by inversing liquidity events and re-querying
    async fn handle_reorg(
        &mut self,
        reorg_range: RangeInclusive<u64>
    ) -> Result<Vec<PoolUpdate>, PoolUpdateError> {
        let mut updates = Vec::new();
        let mut affected_pools = HashMap::new();

        // Find and inverse liquidity events in the reorg range
        let events_to_inverse: Vec<_> = self
            .event_history
            .iter()
            .filter(|e| reorg_range.contains(&e.block))
            .cloned()
            .collect();

        // Inverse liquidity events (in reverse order)
        for event in events_to_inverse.iter().rev() {
            if let EventType::ModifyLiquidity(liquidity_event) = &event.event {
                // Mark pool as affected
                affected_pools.insert(event.pool_id, true);

                // Create inverse event
                let inverse_event = ModifyLiquidityEventData {
                    sender:          liquidity_event.sender,
                    tick_lower:      liquidity_event.tick_lower,
                    tick_upper:      liquidity_event.tick_upper,
                    liquidity_delta: -liquidity_event.liquidity_delta,
                    salt:            liquidity_event.salt
                };

                updates.push(PoolUpdate::LiquidityEvent {
                    pool_id:   event.pool_id,
                    block:     event.block,
                    tx_index:  event.tx_index,
                    log_index: event.log_index,
                    event:     inverse_event
                });
            }
        }

        // Remove events from history that are in reorg range
        self.event_history
            .retain(|e| !reorg_range.contains(&e.block));

        // Re-query events for the reorg range
        for block in reorg_range {
            match self.process_block_events(block).await {
                Ok(block_updates) => updates.extend(block_updates),
                Err(e) => return Err(e)
            }
        }

        // Fetch updated slot0 for affected pools
        for pool_id in affected_pools.keys() {
            if let Ok(slot0) = self.fetch_slot0_data(*pool_id).await {
                updates.push(PoolUpdate::UpdatedSlot0 { pool_id: *pool_id, data: slot0 });
            }
        }

        Ok(updates)
    }

    /// Add event to history, maintaining size limit
    fn add_to_history(&mut self, event: StoredEvent) {
        if self.event_history.len() >= self.event_history_size {
            self.event_history.pop_front();
        }
        self.event_history.push_back(event);
    }

    /// Fetch current slot0 data for a pool
    async fn fetch_slot0_data(&self, pool_id: PoolId) -> Result<Slot0Data, PoolUpdateError> {
        // Get the internal pool ID from the conversion map
        let internal_pool_id =
            self.pool_registry
                .conversion_map
                .get(&pool_id)
                .ok_or_else(|| {
                    PoolUpdateError::Provider(format!(
                        "Pool ID {:?} not found in registry",
                        pool_id
                    ))
                })?;

        // Create a DataLoader for this pool
        let data_loader = DataLoader::new_with_registry(
            *internal_pool_id,
            pool_id,
            self.pool_registry.clone(),
            self.pool_manager
        );

        // Load pool data
        let pool_data = data_loader
            .load_pool_data(None, self.provider.clone())
            .await
            .map_err(|e| PoolUpdateError::Provider(format!("Failed to load pool data: {}", e)))?;

        Ok(Slot0Data {
            sqrt_price_x96: U160::from(pool_data.sqrtPrice),
            tick:           pool_data.tick.as_i32(),
            liquidity:      pool_data.liquidity
        })
    }

    /// Backfill events for missed blocks
    async fn backfill_blocks(
        &mut self,
        from_block: u64,
        to_block: u64
    ) -> Result<Vec<PoolUpdate>, PoolUpdateError> {
        let mut all_updates = Vec::new();

        // If no pools are tracked, return early
        if self.tracked_pools.is_empty() {
            return Ok(all_updates);
        }

        // Create pool topics for filtering
        let pool_topics: Vec<_> = self
            .tracked_pools
            .iter()
            .map(|pool_id| pool_id.0.into())
            .collect();

        // Process blocks in chunks to avoid overwhelming the provider
        const CHUNK_SIZE: u64 = 100;
        let mut current = from_block;

        while current <= to_block {
            let end = (current + CHUNK_SIZE - 1).min(to_block);

            // Create filters for both event types
            let swap_filter = Filter::new()
                .address(self.pool_manager)
                .event_signature(IUniswapV4Pool::Swap::SIGNATURE_HASH)
                .topic1(pool_topics.clone())
                .from_block(current)
                .to_block(end);

            let modify_filter = Filter::new()
                .address(self.pool_manager)
                .event_signature(IUniswapV4Pool::ModifyLiquidity::SIGNATURE_HASH)
                .topic1(pool_topics.clone())
                .from_block(current)
                .to_block(end);

            // Get logs for both event types
            let (swap_logs, modify_logs) = futures::try_join!(
                self.provider.get_logs(&swap_filter),
                self.provider.get_logs(&modify_filter)
            )
            .map_err(|e| {
                PoolUpdateError::Provider(format!("Failed to get logs during backfill: {}", e))
            })?;

            // Process swap logs
            for log in swap_logs {
                let block_number = log.block_number.unwrap_or(0);

                if let Ok(swap_event) = IUniswapV4Pool::Swap::decode_log(&log.inner) {
                    // Double-check pool ID
                    if self.tracked_pools.contains(&swap_event.id) {
                        let event_data = SwapEventData {
                            sender:         swap_event.sender,
                            amount0:        swap_event.amount0,
                            amount1:        swap_event.amount1,
                            sqrt_price_x96: swap_event.sqrtPriceX96,
                            liquidity:      swap_event.liquidity,
                            tick:           swap_event.tick.as_i32(),
                            fee:            swap_event.fee.to()
                        };

                        all_updates.push(PoolUpdate::SwapEvent {
                            pool_id:   swap_event.id,
                            block:     block_number,
                            tx_index:  log.transaction_index.unwrap_or(0),
                            log_index: log.log_index.unwrap_or(0),
                            event:     event_data
                        });
                    }
                }
            }

            // Process modify liquidity logs
            for log in modify_logs {
                let block_number = log.block_number.unwrap_or(0);

                if let Ok(modify_event) = IUniswapV4Pool::ModifyLiquidity::decode_log(&log.inner) {
                    // Double-check pool ID
                    if self.tracked_pools.contains(&modify_event.id) {
                        let event_data = ModifyLiquidityEventData {
                            sender:          modify_event.sender,
                            tick_lower:      modify_event.tickLower.as_i32(),
                            tick_upper:      modify_event.tickUpper.as_i32(),
                            liquidity_delta: modify_event.liquidityDelta,
                            salt:            modify_event.salt.0
                        };

                        all_updates.push(PoolUpdate::LiquidityEvent {
                            pool_id:   modify_event.id,
                            block:     block_number,
                            tx_index:  log.transaction_index.unwrap_or(0),
                            log_index: log.log_index.unwrap_or(0),
                            event:     event_data
                        });
                    }
                }
            }

            current = end + 1;
        }

        Ok(all_updates)
    }

    async fn on_new_block(&mut self, block: Block) -> Vec<PoolUpdate> {
        vec![]
    }
}

pub struct StateStream<P: Provider + 'static> {
    update_provider: Option<PoolUpdateProvider<P>>,
    block_stream:    CompletedBlockStream<P>,
    processing:
        Option<Pin<Box<dyn Future<Output = (PoolUpdateProvider<P>, Vec<PoolUpdate>)> + Send>>>
}

impl<P: Provider + 'static> Stream for StateStream<P> {
    type Item = Vec<PoolUpdate>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();

        // If we are processing something, we don't want to poll the block stream as
        // this could cause panics as the update provider has moved.
        if let Some(mut processing) = this.processing.take() {
            if let Poll::Ready((provider, new_updates)) = processing.poll_unpin(cx) {
                this.update_provider = Some(provider);

                return Poll::Ready(Some(new_updates));
            }
            this.processing = Some(processing);

            return Poll::Pending
        }

        if let Poll::Ready(Some(new_block)) = this.block_stream.poll_next_unpin(cx) {
            cx.waker().wake_by_ref();
            let mut update_provider = this.update_provider.take().unwrap();

            let processing_future = async move {
                let updates = update_provider.on_new_block(new_block).await;
                (update_provider, updates)
            }
            .boxed();

            this.processing = Some(processing_future)
        }

        Poll::Pending
    }
}
