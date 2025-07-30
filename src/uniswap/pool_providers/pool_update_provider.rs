use std::{
    cmp::Ordering,
    collections::{HashSet, VecDeque},
    pin::Pin,
    sync::Arc,
    task::{Context, Poll}
};

use alloy::{
    consensus::Transaction,
    eips::BlockId,
    primitives::{Address, I256, U160, aliases::I24},
    providers::Provider,
    rpc::types::{Block, Filter},
    sol_types::{SolCall, SolEvent}
};
use futures::{FutureExt, StreamExt, stream::Stream};
use thiserror::Error;

use crate::{
    pool_providers::PoolEventStream,
    uniswap::{
        pool_data_loader::{DataLoader, IUniswapV4Pool, PoolDataLoader},
        pool_key::PoolKey,
        pool_registry::UniswapPoolRegistry,
        pools::PoolId
    }
};

/// Number of blocks to keep in history for reorg detection
const REORG_DETECTION_BLOCKS: u64 = 10;

alloy::sol! {
    #[derive(Debug, PartialEq, Eq)]
    contract ControllerV1 {

    event PoolConfigured(
        address indexed asset0,
        address indexed asset1,
        uint16 tickSpacing,
        uint24 bundleFee,
        uint24 unlockedFee,
        uint24 protocolUnlockedFee
    );

    event PoolRemoved(
        address indexed asset0, address indexed asset1, int24 tickSpacing, uint24 feeInE6
    );

        struct PoolUpdate {
            address assetA;
            address assetB;
            uint24 bundleFee;
            uint24 unlockedFee;
            uint24 protocolUnlockedFee;
        }

        function batchUpdatePools(PoolUpdate[] calldata updates) external;
    }
}

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
    /// Pool configured/added via controller
    PoolConfigured {
        pool_id:      PoolId,
        pool_key:     PoolKey,
        block:        u64,
        bundle_fee:   u32,
        swap_fee:     u32,
        protocol_fee: u32,
        tick_spacing: i32
    },
    /// Pool removed via controller
    PoolRemoved { pool_id: PoolId, block: u64 },
    /// Fee update event. the pool_id here is the uniswap pool_id
    FeeUpdate {
        pool_id:      PoolId,
        block:        u64,
        bundle_fee:   u32,
        swap_fee:     u32,
        protocol_fee: u32
    },
    /// Reorg detected
    Reorg { from_block: u64, to_block: u64 },
    /// Updated slot0 data after reorg
    UpdatedSlot0 { pool_id: PoolId, data: Slot0Data }
}

impl PoolUpdate {
    pub fn sort(&self, b: &Self) -> Ordering {
        let (this_tx_index, this_log_index) = match self {
            PoolUpdate::SwapEvent { tx_index, log_index, .. } => (*tx_index, *log_index),
            PoolUpdate::LiquidityEvent { tx_index, log_index, .. } => (*tx_index, *log_index),
            _ => (u64::MAX, u64::MAX)
        };

        let (other_tx_index, other_log_index) = match b {
            PoolUpdate::SwapEvent { tx_index, log_index, .. } => (*tx_index, *log_index),
            PoolUpdate::LiquidityEvent { tx_index, log_index, .. } => (*tx_index, *log_index),
            _ => (u64::MAX, u64::MAX)
        };

        this_tx_index
            .cmp(&other_tx_index)
            .then_with(|| this_log_index.cmp(&other_log_index))
    }
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

/// Stored event for reorg handling - only liquidity events need to be stored
#[derive(Debug, Clone)]
struct StoredEvent {
    block:           u64,
    tx_index:        u64,
    log_index:       u64,
    pool_id:         PoolId,
    liquidity_event: ModifyLiquidityEventData
}

/// Pool update provider that streams pool state changes
pub struct PoolUpdateProvider<P>
where
    P: Provider + 'static
{
    provider:           Arc<P>,
    pool_manager:       Address,
    controller_address: Address,
    angstrom_address:   Address,
    pool_registry:      UniswapPoolRegistry,
    tracked_pools:      HashSet<PoolId>,
    event_history:      VecDeque<StoredEvent>,
    current_block:      u64
}

impl<P> PoolUpdateProvider<P>
where
    P: Provider + 'static
{
    /// Create a new pool update provider
    pub async fn new(
        provider: Arc<P>,
        pool_manager: Address,
        controller_address: Address,
        angstrom_address: Address,
        pool_registry: UniswapPoolRegistry
    ) -> Self {
        let current_block = provider
            .get_block(BlockId::Number(alloy::eips::BlockNumberOrTag::Latest))
            .await
            .unwrap()
            .unwrap()
            .number();

        Self::new_at_block(
            provider,
            pool_manager,
            controller_address,
            angstrom_address,
            pool_registry,
            current_block
        )
    }

    /// Create a new pool update provider at a specific block
    pub fn new_at_block(
        provider: Arc<P>,
        pool_manager: Address,
        controller_address: Address,
        angstrom_address: Address,
        pool_registry: UniswapPoolRegistry,
        current_block: u64
    ) -> Self {
        Self {
            provider,
            pool_manager,
            controller_address,
            angstrom_address,
            pool_registry,
            tracked_pools: HashSet::new(),
            event_history: VecDeque::with_capacity(REORG_DETECTION_BLOCKS as usize),
            current_block
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

    /// Process a swap event log
    fn process_swap_event(
        &self,
        log: &alloy::rpc::types::Log,
        block_number: u64
    ) -> Option<PoolUpdate> {
        if let Ok(swap_event) = IUniswapV4Pool::Swap::decode_log(&log.inner) {
            // Check if we're tracking this Uniswap pool ID
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

                return Some(PoolUpdate::SwapEvent {
                    pool_id:   swap_event.id, // Use Uniswap pool ID
                    block:     block_number,
                    tx_index:  log.transaction_index.unwrap(),
                    log_index: log.log_index.unwrap(),
                    event:     event_data
                });
            }
        }
        None
    }

    /// Process a liquidity event log
    fn process_liquidity_event(
        &mut self,
        log: &alloy::rpc::types::Log,
        block_number: u64,
        store_in_history: bool
    ) -> Option<PoolUpdate> {
        if let Ok(modify_event) = IUniswapV4Pool::ModifyLiquidity::decode_log(&log.inner) {
            // Check if we're tracking this Uniswap pool ID
            if self.tracked_pools.contains(&modify_event.id) {
                let event_data = ModifyLiquidityEventData {
                    sender:          modify_event.sender,
                    tick_lower:      modify_event.tickLower.as_i32(),
                    tick_upper:      modify_event.tickUpper.as_i32(),
                    liquidity_delta: modify_event.liquidityDelta,
                    salt:            modify_event.salt.0
                };

                // Store in history only if requested
                if store_in_history {
                    self.add_to_history(StoredEvent {
                        block:           block_number,
                        tx_index:        log.transaction_index.unwrap(),
                        log_index:       log.log_index.unwrap(),
                        pool_id:         modify_event.id, // Use Uniswap pool ID
                        liquidity_event: event_data.clone()
                    });
                }

                return Some(PoolUpdate::LiquidityEvent {
                    pool_id:   modify_event.id, // Use Uniswap pool ID
                    block:     block_number,
                    tx_index:  log.transaction_index.unwrap(),
                    log_index: log.log_index.unwrap(),
                    event:     event_data
                });
            }
        }
        None
    }

    /// Process controller event logs
    fn process_controller_logs(&mut self, logs: Vec<alloy::rpc::types::Log>) -> Vec<PoolUpdate> {
        let mut updates = Vec::new();

        for log in logs {
            let block_number = log.block_number.unwrap();

            if let Ok(event) = ControllerV1::PoolConfigured::decode_log(&log.inner) {
                let pool_key = PoolKey {
                    currency0:   event.asset0,
                    currency1:   event.asset1,
                    fee:         event.bundleFee,
                    tickSpacing: I24::unchecked_from(event.tickSpacing),
                    hooks:       self.angstrom_address
                };

                self.pool_registry.add_new_pool(pool_key.clone());

                // Get the Uniswap pool ID from registry
                let angstrom_pool_id = PoolId::from(pool_key);
                let pool_id = self
                    .pool_registry
                    .private_key_from_public(&angstrom_pool_id)
                    .unwrap();

                updates.push(PoolUpdate::PoolConfigured {
                    pool_key,
                    pool_id,
                    block: block_number,
                    bundle_fee: event.bundleFee.to(),
                    swap_fee: event.unlockedFee.to(),
                    protocol_fee: event.protocolUnlockedFee.to(),
                    tick_spacing: event.tickSpacing as i32
                });
            }

            if let Ok(event) = ControllerV1::PoolRemoved::decode_log(&log.inner) {
                let pool_key = PoolKey {
                    currency0:   event.asset0,
                    currency1:   event.asset1,
                    fee:         event.feeInE6,
                    tickSpacing: event.tickSpacing,
                    hooks:       self.angstrom_address
                };

                // Get the Uniswap pool ID from registry
                let angstrom_pool_id = PoolId::from(pool_key);
                let pool_id = self
                    .pool_registry
                    .private_key_from_public(&angstrom_pool_id)
                    .unwrap();

                updates.push(PoolUpdate::PoolRemoved { pool_id, block: block_number });
            }
        }

        updates
    }

    /// Process batch update pools from transaction
    fn process_batch_update_pools(
        &self,
        tx: &alloy::rpc::types::Transaction,
        block_number: u64
    ) -> Vec<PoolUpdate> {
        let mut updates = Vec::new();

        // Check if transaction is to the controller
        if tx.to() == Some(self.controller_address) {
            // Try to decode as batchUpdatePools call
            if let Ok(call) = ControllerV1::batchUpdatePoolsCall::abi_decode(tx.input()) {
                for update in call.updates {
                    // Normalize asset order
                    let (_asset0, _asset1) = if update.assetB > update.assetA {
                        (update.assetA, update.assetB)
                    } else {
                        (update.assetB, update.assetA)
                    };
                    let pools = self
                        .pool_registry
                        .get_pools_by_token_pair(update.assetA, update.assetB);

                    // Find the pool with matching fee (or just use the first one if no match)
                    let pool_key = pools
                        .iter()
                        .find(|pk| pk.fee.to::<u32>() == update.bundleFee.to::<u32>())
                        .or_else(|| pools.first())
                        .cloned()
                        .cloned();

                    if let Some(pool_key) = pool_key {
                        // Get the Uniswap pool ID from registry
                        let angstrom_pool_id = PoolId::from(pool_key);
                        let pool_id = self
                            .pool_registry
                            .private_key_from_public(&angstrom_pool_id)
                            .unwrap();

                        updates.push(PoolUpdate::FeeUpdate {
                            pool_id,
                            block: block_number,
                            bundle_fee: update.bundleFee.to(),
                            swap_fee: update.unlockedFee.to(),
                            protocol_fee: update.protocolUnlockedFee.to()
                        });
                    }
                }
            }
        }

        updates
    }

    /// Process events for a block range
    async fn process_events_for_block_range(
        &mut self,
        from_block: u64,
        to_block: u64,
        store_in_history: bool
    ) -> Result<Vec<PoolUpdate>, PoolUpdateError> {
        let mut updates = Vec::new();

        // If no pools are tracked, return early
        if self.tracked_pools.is_empty() {
            return Ok(updates);
        }

        // Create pool topics for filtering - tracked_pools already contains Uniswap
        // pool IDs
        let pool_topics: Vec<_> = self
            .tracked_pools
            .iter()
            .map(|pool_id| pool_id.0.into())
            .collect();

        // Create filters for swap and liquidity events
        let swap_filter = Filter::new()
            .address(self.pool_manager)
            .event_signature(IUniswapV4Pool::Swap::SIGNATURE_HASH)
            .topic1(pool_topics.clone())
            .from_block(from_block)
            .to_block(to_block);

        let modify_filter = Filter::new()
            .address(self.pool_manager)
            .event_signature(IUniswapV4Pool::ModifyLiquidity::SIGNATURE_HASH)
            .topic1(pool_topics)
            .from_block(from_block)
            .to_block(to_block);

        // Get logs for both event types
        let (swap_logs, modify_logs) = futures::try_join!(
            self.provider.get_logs(&swap_filter),
            self.provider.get_logs(&modify_filter)
        )
        .map_err(|e| PoolUpdateError::Provider(format!("Failed to get logs: {e}")))?;

        // Process swap logs
        for log in swap_logs {
            let block_number = log.block_number.unwrap_or(from_block);
            if let Some(update) = self.process_swap_event(&log, block_number) {
                updates.push(update);
            }
        }

        // Process modify liquidity logs
        for log in modify_logs {
            let block_number = log.block_number.unwrap_or(from_block);
            if let Some(update) = self.process_liquidity_event(&log, block_number, store_in_history)
            {
                updates.push(update);
            }
        }

        // Query controller events
        let controller_filter = Filter::new()
            .address(self.controller_address)
            .from_block(from_block)
            .to_block(to_block);

        let controller_logs = self
            .provider
            .get_logs(&controller_filter)
            .await
            .map_err(|e| {
                PoolUpdateError::Provider(format!("Failed to get controller logs: {e}"))
            })?;

        // Process controller logs
        updates.extend(self.process_controller_logs(controller_logs));

        // Process transactions to find batchUpdatePools calls
        // For single blocks, get the block directly. For ranges, iterate.
        if from_block == to_block {
            let block = self
                .provider
                .get_block(BlockId::Number(from_block.into()))
                .full()
                .await
                .map_err(|e| PoolUpdateError::Provider(format!("Failed to get block: {e}")))?
                .ok_or_else(|| PoolUpdateError::Provider("Block not found".to_string()))?;

            if let Some(transactions) = block.transactions.as_transactions() {
                for tx in transactions {
                    updates.extend(self.process_batch_update_pools(tx, from_block));
                }
            }
        } else {
            // For block ranges, iterate through each block
            for block_num in from_block..=to_block {
                let block = self
                    .provider
                    .get_block(BlockId::Number(block_num.into()))
                    .full()
                    .await
                    .map_err(|e| PoolUpdateError::Provider(format!("Failed to get block: {e}")))?;

                if let Some(block) = block {
                    if let Some(transactions) = block.transactions.as_transactions() {
                        for tx in transactions {
                            updates.extend(self.process_batch_update_pools(tx, block_num));
                        }
                    }
                }
            }
        }

        Ok(updates)
    }

    /// Process events for a specific block
    async fn process_block_events(
        &mut self,
        block_number: u64
    ) -> Result<Vec<PoolUpdate>, PoolUpdateError> {
        // Use the shared helper with store_in_history = true for single blocks
        self.process_events_for_block_range(block_number, block_number, true)
            .await
    }

    /// Add event to history, maintaining the 10-block window
    fn add_to_history(&mut self, event: StoredEvent) {
        self.event_history.push_back(event);

        // Maintain exactly REORG_DETECTION_BLOCKS worth of history
        // Remove all events from blocks that are too old
        let cutoff_block = self
            .current_block
            .saturating_sub(REORG_DETECTION_BLOCKS - 1);

        // Remove all events from blocks older than cutoff
        self.event_history.retain(|e| e.block >= cutoff_block);
    }

    /// Fetch current slot0 data for a pool at the current block
    async fn fetch_slot0_data(&self, pool_id: PoolId) -> Result<Slot0Data, PoolUpdateError> {
        self.fetch_slot0_data_at_block(pool_id, self.current_block)
            .await
    }

    /// Fetch slot0 data for a pool at a specific block
    async fn fetch_slot0_data_at_block(
        &self,
        pool_id: PoolId,
        block: u64
    ) -> Result<Slot0Data, PoolUpdateError> {
        // Get the internal pool ID from the conversion map
        let internal_pool_id =
            self.pool_registry
                .conversion_map
                .get(&pool_id)
                .ok_or_else(|| {
                    PoolUpdateError::Provider(format!("Pool ID {pool_id:?} not found in registry"))
                })?;

        // Create a DataLoader for this pool
        let data_loader = DataLoader::new_with_registry(
            *internal_pool_id,
            pool_id,
            self.pool_registry.clone(),
            self.pool_manager
        );

        // Load pool data at specific block
        let pool_data = data_loader
            .load_pool_data(Some(block), self.provider.clone())
            .await
            .map_err(|e| PoolUpdateError::Provider(format!("Failed to load pool data: {e}")))?;

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

        // Process blocks in chunks to avoid overwhelming the provider
        const CHUNK_SIZE: u64 = 100;
        let mut current = from_block;

        while current <= to_block {
            let end = (current + CHUNK_SIZE - 1).min(to_block);

            // Use the shared helper with store_in_history = false for backfilling
            let chunk_updates = self
                .process_events_for_block_range(current, end, false)
                .await?;
            all_updates.extend(chunk_updates);

            current = end + 1;
        }

        Ok(all_updates)
    }

    /// Get inverse liquidity events for reorg handling
    fn get_inverse_liquidity_events(&self, from_block: u64, to_block: u64) -> Vec<PoolUpdate> {
        let mut inverse_events = Vec::new();

        // Iterate through history in reverse order to process most recent first
        for event in self.event_history.iter().rev() {
            if event.block < from_block || event.block > to_block {
                continue;
            }

            // Create inverse event by negating liquidity delta
            let inverse_event = ModifyLiquidityEventData {
                sender:          event.liquidity_event.sender,
                tick_lower:      event.liquidity_event.tick_lower,
                tick_upper:      event.liquidity_event.tick_upper,
                liquidity_delta: -event.liquidity_event.liquidity_delta,
                salt:            event.liquidity_event.salt
            };

            inverse_events.push(PoolUpdate::LiquidityEvent {
                pool_id:   event.pool_id,
                block:     event.block,
                tx_index:  event.tx_index,
                log_index: event.log_index,
                event:     inverse_event
            });
        }

        inverse_events
    }

    /// Get pools affected by events
    fn get_affected_pools(&self, updates: &[PoolUpdate]) -> HashSet<PoolId> {
        let mut affected_pools = HashSet::new();

        for update in updates {
            match update {
                PoolUpdate::SwapEvent { pool_id, .. }
                | PoolUpdate::LiquidityEvent { pool_id, .. }
                | PoolUpdate::UpdatedSlot0 { pool_id, .. }
                | PoolUpdate::PoolConfigured { pool_id, .. }
                | PoolUpdate::PoolRemoved { pool_id, .. }
                | PoolUpdate::FeeUpdate { pool_id, .. } => {
                    affected_pools.insert(*pool_id);
                }
                _ => {}
            }
        }

        affected_pools
    }

    /// Clear history for reorg
    fn clear_history_from_block(&mut self, from_block: u64) {
        self.event_history.retain(|event| event.block < from_block);
    }

    /// Handle a reorg event
    async fn handle_reorg(&mut self) -> Vec<PoolUpdate> {
        let mut updates = Vec::new();
        let reorg_start = self
            .current_block
            .saturating_sub(REORG_DETECTION_BLOCKS - 1);

        // 1. First, emit the reorg event so the pipeline knows a reorg is happening
        updates.push(PoolUpdate::Reorg { from_block: reorg_start, to_block: self.current_block });

        // 2. Get inverse liquidity events
        let inverse_events = self.get_inverse_liquidity_events(reorg_start, self.current_block);
        updates.extend(inverse_events.clone());

        // 3. Clear affected history
        self.clear_history_from_block(reorg_start);

        // 4. Re-query the blocks
        match self.backfill_blocks(reorg_start, self.current_block).await {
            Ok(fresh_events) => {
                // Get affected pools from both inverse and fresh events
                let mut affected_pools = self.get_affected_pools(&inverse_events);
                affected_pools.extend(self.get_affected_pools(&fresh_events));

                // Add fresh events to history
                for update in &fresh_events {
                    if let Some(stored_event) = Self::update_to_stored_event(update) {
                        self.add_to_history(stored_event);
                    }
                }

                updates.extend(fresh_events);

                // 5. Query slot0 for affected pools
                for pool_id in affected_pools {
                    if let Ok(slot0_data) = self.fetch_slot0_data(pool_id).await {
                        updates.push(PoolUpdate::UpdatedSlot0 { pool_id, data: slot0_data });
                    }
                }
            }
            Err(e) => {
                // Log error but continue
                panic!("Failed to backfill during reorg: {e}");
            }
        }
        updates.push(PoolUpdate::Reorg { from_block: reorg_start, to_block: self.current_block });

        updates
    }

    pub async fn on_new_block(&mut self, block: Block) -> Vec<PoolUpdate> {
        let mut updates = Vec::new();
        let block_number = block.number();

        // Check for reorg
        if block_number == self.current_block {
            // Reorg detected!
            updates = self.handle_reorg().await;
        } else if block_number > self.current_block {
            // Always emit NewBlock event first for normal block progression
            updates.push(PoolUpdate::NewBlock(block_number));

            // Then process block events
            match self.process_block_events(block_number).await {
                Ok(block_updates) => {
                    updates.extend(block_updates);
                }
                Err(e) => {
                    tracing::error!("Failed to process block {}: {}", block_number, e);
                }
            }

            // Update current block
            self.current_block = block_number;

            // Clean up old events from history to maintain exactly 10 blocks
            let cutoff_block = self
                .current_block
                .saturating_sub(REORG_DETECTION_BLOCKS - 1);
            self.event_history.retain(|e| e.block >= cutoff_block);
        } else if block_number < self.current_block {
            // Block is behind our current block, this shouldn't happen in normal operation
            tracing::warn!(
                "Received old block {} when current block is {}",
                block_number,
                self.current_block
            );
        }

        updates
    }

    /// Convert PoolUpdate to StoredEvent for history
    /// Only liquidity events are stored since we re-query slot0 after reorgs
    fn update_to_stored_event(update: &PoolUpdate) -> Option<StoredEvent> {
        match update {
            PoolUpdate::LiquidityEvent { pool_id, block, tx_index, log_index, event } => {
                Some(StoredEvent {
                    block:           *block,
                    tx_index:        *tx_index,
                    log_index:       *log_index,
                    pool_id:         *pool_id,
                    liquidity_event: event.clone()
                })
            }
            _ => None
        }
    }
}

pub struct StateStream<P: Provider + 'static, B>
where
    B: Stream<Item = Block> + Unpin + Send + 'static
{
    update_provider:      Option<PoolUpdateProvider<P>>,
    block_stream:         B,
    processing:
        Option<Pin<Box<dyn Future<Output = (PoolUpdateProvider<P>, Vec<PoolUpdate>)> + Send>>>,
    start_tracking_pools: Vec<PoolId>,
    stop_tracking_pools:  Vec<PoolId>,
    pool_reg:             Option<UniswapPoolRegistry>
}

impl<P: Provider + 'static, B> StateStream<P, B>
where
    B: Stream<Item = Block> + Unpin + Send + 'static
{
    pub fn new(update_provider: PoolUpdateProvider<P>, block_stream: B) -> Self {
        Self {
            update_provider: Some(update_provider),
            block_stream,
            processing: None,
            start_tracking_pools: vec![],
            stop_tracking_pools: vec![],
            pool_reg: None
        }
    }
}

impl<P: Provider + 'static, B> PoolEventStream for StateStream<P, B>
where
    B: Stream<Item = Block> + Unpin + Send + 'static
{
    fn stop_tracking_pool(&mut self, pool_id: PoolId) {
        if let Some(update_provider) = self.update_provider.as_mut() {
            update_provider.remove_pool(pool_id);
        } else {
            self.stop_tracking_pools.push(pool_id);
        }
    }

    fn start_tracking_pool(&mut self, pool_id: PoolId) {
        if let Some(update_provider) = self.update_provider.as_mut() {
            update_provider.add_pool(pool_id);
        } else {
            self.start_tracking_pools.push(pool_id);
        }
    }

    fn set_pool_registry(&mut self, pool_registry: UniswapPoolRegistry) {
        if let Some(update_provider) = self.update_provider.as_mut() {
            update_provider.pool_registry = pool_registry;
        } else {
            self.pool_reg = Some(pool_registry);
        }
    }
}

impl<P: Provider + 'static, B> Stream for StateStream<P, B>
where
    B: Stream<Item = Block> + Unpin + Send + 'static
{
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

        let updater = this.update_provider.as_mut().unwrap();
        for pool in this.start_tracking_pools.drain(..) {
            updater.add_pool(pool);
        }
        for pool in this.stop_tracking_pools.drain(..) {
            updater.remove_pool(pool);
        }
        if let Some(pool_reg) = this.pool_reg.take() {
            updater.pool_registry = pool_reg;
        }

        if let Poll::Ready(possible_new_block) = this.block_stream.poll_next_unpin(cx) {
            if let Some(new_block) = possible_new_block {
                cx.waker().wake_by_ref();
                let mut update_provider = this.update_provider.take().unwrap();

                let processing_future = async move {
                    let updates = update_provider.on_new_block(new_block).await;
                    (update_provider, updates)
                }
                .boxed();

                this.processing = Some(processing_future)
            } else {
                return Poll::Ready(None)
            }
        }

        Poll::Pending
    }
}
