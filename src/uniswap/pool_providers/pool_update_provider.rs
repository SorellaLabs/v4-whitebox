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
    primitives::{Address, I256, Log, U160, U256},
    providers::{Provider, ProviderBuilder, WatchTxError},
    pubsub::PubSubFrontend,
    rpc::types::Filter,
    sol_types::SolEvent
};
use futures::{
    FutureExt, StreamExt,
    future::BoxFuture,
    stream::{BoxStream, Stream}
};
use thiserror::Error;

use crate::uniswap::{
    pool::PoolId,
    pool_data_loader::{DataLoader, IUniswapV4Pool, PoolDataLoader},
    pool_registry::UniswapPoolRegistry
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
#[derive(Clone)]
pub struct PoolUpdateProvider<P>
where
    P: Provider + Clone + 'static
{
    provider:             Arc<P>,
    pool_manager:         Address,
    pool_registry:        UniswapPoolRegistry,
    tracked_pools:        HashSet<PoolId>,
    event_history:        VecDeque<StoredEvent>,
    event_history_size:   usize,
    current_block:        u64,
    last_processed_block: Option<u64>,
    max_retry_attempts:   u32,
    retry_delay:          Duration
}

impl<P> PoolUpdateProvider<P>
where
    P: Provider + Clone + 'static
{
    /// Create a new pool update provider
    pub fn new(
        provider: Arc<P>,
        pool_manager: Address,
        pool_registry: UniswapPoolRegistry,
        event_history_size: usize
    ) -> Self {
        Self {
            provider,
            pool_manager,
            pool_registry,
            tracked_pools: HashSet::new(),
            event_history: VecDeque::with_capacity(event_history_size),
            event_history_size,
            current_block: 0,
            last_processed_block: None,
            max_retry_attempts: 5,
            retry_delay: Duration::from_secs(1)
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

    /// Subscribe to pool updates stream
    pub fn subscribe_updates(self) -> PoolUpdateStream<P> {
        PoolUpdateStream::new(self)
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

                if let Ok(swap_event) = IUniswapV4Pool::Swap::decode_log(&log) {
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

                if let Ok(modify_event) = IUniswapV4Pool::ModifyLiquidity::decode_log(&log) {
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
}

/// Stream state for the pool update stream
enum StreamState<P: Provider> {
    /// Waiting to subscribe to blocks
    Initializing { retry_count: u32 },
    /// Reconnecting after failure
    Reconnecting { retry_count: u32, backoff_future: BoxFuture<'static, ()> },
    /// Backfilling missed blocks
    Backfilling {
        future:     BoxFuture<'static, Result<Vec<PoolUpdate>, PoolUpdateError>>,
        next_state: Box<StreamState<P>>
    },
    /// Subscribed and waiting for blocks
    Subscribed {
        block_stream:    BoxStream<'static, alloy::rpc::types::Block>,
        pending_updates: VecDeque<Result<PoolUpdate, PoolUpdateError>>
    },
    /// Processing a block or reorg
    Processing {
        block_stream:    BoxStream<'static, alloy::rpc::types::Block>,
        future: BoxFuture<'static, (Vec<Result<PoolUpdate, PoolUpdateError>>, Option<u64>)>,
        pending_updates: VecDeque<Result<PoolUpdate, PoolUpdateError>>
    },
    /// Stream has ended
    Done
}

/// Custom stream implementation for pool updates
pub struct PoolUpdateStream<P>
where
    P: Provider + Clone + 'static
{
    provider: PoolUpdateProvider<P>,
    state:    StreamState<P>
}

impl<P> PoolUpdateStream<P>
where
    P: Provider + Clone + 'static
{
    fn new(provider: PoolUpdateProvider<P>) -> Self {
        Self { provider, state: StreamState::Initializing { retry_count: 0 } }
    }
}

impl<P> Stream for PoolUpdateStream<P>
where
    P: Provider + Clone + 'static
{
    type Item = Result<PoolUpdate, PoolUpdateError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        loop {
            match &mut self.state {
                StreamState::Initializing { retry_count } => {
                    let retry_count = *retry_count;

                    // Check if we've exceeded max retries
                    if retry_count >= self.provider.max_retry_attempts {
                        self.state = StreamState::Done;
                        return Poll::Ready(Some(Err(PoolUpdateError::Provider(format!(
                            "Failed to connect after {} attempts",
                            retry_count
                        )))));
                    }

                    // Start the subscription
                    let provider = self.provider.provider.clone();
                    let mut future = Box::pin(async move { provider.subscribe_blocks().await });

                    match future.poll_unpin(cx) {
                        Poll::Ready(Ok(subscription)) => {
                            // Check if we need to backfill
                            if let Some(last_block) = self.provider.last_processed_block {
                                // Get current block to see if we missed any
                                let provider_clone = self.provider.provider.clone();
                                let mut current_block_future =
                                    Box::pin(
                                        async move { provider_clone.get_block_number().await }
                                    );

                                match current_block_future.poll_unpin(cx) {
                                    Poll::Ready(Ok(current_block)) => {
                                        if current_block > last_block + 1 {
                                            // Need to backfill
                                            let mut provider_clone = self.provider.clone();
                                            let from = last_block + 1;
                                            let to = current_block - 1;

                                            let backfill_future = Box::pin(async move {
                                                provider_clone.backfill_blocks(from, to).await
                                            });

                                            self.state = StreamState::Backfilling {
                                                future:     backfill_future,
                                                next_state: Box::new(StreamState::Subscribed {
                                                    block_stream:    subscription
                                                        .into_stream()
                                                        .boxed(),
                                                    pending_updates: VecDeque::new()
                                                })
                                            };
                                        } else {
                                            self.state = StreamState::Subscribed {
                                                block_stream:    subscription.into_stream().boxed(),
                                                pending_updates: VecDeque::new()
                                            };
                                        }
                                    }
                                    Poll::Ready(Err(e)) => {
                                        // Failed to get current block, proceed without backfill
                                        self.state = StreamState::Subscribed {
                                            block_stream:    subscription.into_stream().boxed(),
                                            pending_updates: VecDeque::new()
                                        };
                                    }
                                    Poll::Pending => return Poll::Pending
                                }
                            } else {
                                self.state = StreamState::Subscribed {
                                    block_stream:    subscription.into_stream().boxed(),
                                    pending_updates: VecDeque::new()
                                };
                            }
                        }
                        Poll::Ready(Err(e)) => {
                            // Failed to subscribe, enter reconnection state
                            let delay = self.provider.retry_delay * (2u32.pow(retry_count.min(5)));
                            let backoff_future = Box::pin(tokio::time::sleep(delay));

                            self.state = StreamState::Reconnecting {
                                retry_count: retry_count + 1,
                                backoff_future
                            };
                        }
                        Poll::Pending => return Poll::Pending
                    }
                }

                StreamState::Reconnecting { retry_count, backoff_future } => {
                    match backoff_future.poll_unpin(cx) {
                        Poll::Ready(()) => {
                            let retry_count = *retry_count;
                            self.state = StreamState::Initializing { retry_count };
                        }
                        Poll::Pending => return Poll::Pending
                    }
                }

                StreamState::Backfilling { future, next_state } => {
                    match future.poll_unpin(cx) {
                        Poll::Ready(Ok(updates)) => {
                            let mut pending_updates = VecDeque::new();
                            pending_updates.extend(updates.into_iter().map(Ok));

                            // Move to next state with pending updates
                            match std::mem::replace(next_state, Box::new(StreamState::Done))
                                .as_mut()
                            {
                                StreamState::Subscribed {
                                    block_stream: _,
                                    pending_updates: next_pending
                                } => {
                                    next_pending.extend(pending_updates);
                                }
                                _ => {}
                            }

                            self.state =
                                *std::mem::replace(next_state, Box::new(StreamState::Done));
                        }
                        Poll::Ready(Err(e)) => {
                            // Backfill failed, but continue with subscription
                            let mut pending_updates = VecDeque::new();
                            pending_updates.push_back(Err(e));

                            // Move to next state with error
                            match std::mem::replace(next_state, Box::new(StreamState::Done))
                                .as_mut()
                            {
                                StreamState::Subscribed {
                                    block_stream: _,
                                    pending_updates: next_pending
                                } => {
                                    next_pending.extend(pending_updates);
                                }
                                _ => {}
                            }

                            self.state =
                                *std::mem::replace(next_state, Box::new(StreamState::Done));
                        }
                        Poll::Pending => return Poll::Pending
                    }
                }

                StreamState::Subscribed { block_stream, pending_updates } => {
                    // Check if we have pending updates to yield
                    if let Some(update) = pending_updates.pop_front() {
                        return Poll::Ready(Some(update));
                    }

                    // Poll for new blocks
                    match block_stream.poll_next_unpin(cx) {
                        Poll::Ready(Some(block)) => {
                            let block_number = block.header.number;
                            let mut provider = self.provider.clone();
                            let current_block = provider.current_block;

                            // Create future to process the block
                            let future = Box::pin(async move {
                                let mut results = Vec::new();
                                let mut new_block_number = None;

                                // Check for reorg
                                if block_number < current_block {
                                    results.push(Ok(PoolUpdate::Reorg {
                                        from_block: block_number,
                                        to_block:   current_block
                                    }));

                                    // Handle reorg
                                    let reorg_range = block_number..=current_block;
                                    match provider.handle_reorg(reorg_range).await {
                                        Ok(updates) => {
                                            results.extend(updates.into_iter().map(Ok));
                                        }
                                        Err(e) => {
                                            results.push(Err(PoolUpdateError::ReorgHandling(
                                                format!("Failed to handle reorg: {}", e)
                                            )));
                                        }
                                    }
                                }

                                // Process block events
                                match provider.process_block_events(block_number).await {
                                    Ok(updates) => {
                                        results.extend(updates.into_iter().map(Ok));
                                    }
                                    Err(e) => {
                                        results.push(Err(e));
                                    }
                                }

                                provider.current_block = block_number;
                                provider.last_processed_block = Some(block_number);
                                new_block_number = Some(block_number);
                                results.push(Ok(PoolUpdate::NewBlock(block_number)));

                                (results, new_block_number)
                            });

                            let stream =
                                std::mem::replace(block_stream, futures::stream::empty().boxed());
                            self.state = StreamState::Processing {
                                block_stream: stream,
                                future,
                                pending_updates: std::mem::take(pending_updates)
                            };
                        }
                        Poll::Ready(None) => {
                            // Stream ended, attempt to reconnect
                            self.state = StreamState::Initializing { retry_count: 0 };
                        }
                        Poll::Pending => return Poll::Pending
                    }
                }

                StreamState::Processing { block_stream, future, pending_updates } => {
                    match future.poll_unpin(cx) {
                        Poll::Ready((results, new_block_number)) => {
                            // Update provider's current block if needed
                            if let Some(block_num) = new_block_number {
                                self.provider.current_block = block_num;
                                self.provider.last_processed_block = Some(block_num);
                            }

                            // Add results to pending updates
                            pending_updates.extend(results);

                            // Transition back to subscribed state
                            let stream =
                                std::mem::replace(block_stream, futures::stream::empty().boxed());
                            let updates = std::mem::take(pending_updates);
                            self.state = StreamState::Subscribed {
                                block_stream:    stream,
                                pending_updates: updates
                            };
                        }
                        Poll::Pending => return Poll::Pending
                    }
                }

                StreamState::Done => return Poll::Ready(None)
            }
        }
    }
}
