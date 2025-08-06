use std::cmp::Ordering;

use alloy_primitives::{Address, I256, U160};
use serde::{Deserialize, Serialize};

use crate::pools::PoolId;

/// Different types of pool updates
#[derive(Debug, Clone)]
pub enum PoolUpdate {
    /// New block notification
    NewBlock(u64),
    /// Pool created via controller
    NewPool {
        pool_id:      PoolId,
        token0:       Address,
        token1:       Address,
        bundle_fee:   u32,
        swap_fee:     u32,
        protocol_fee: u32,
        tick_spacing: i32,
        block:        u64
    },
    /// Swap event occurred
    SwapEvent {
        pool_id:   PoolId,
        block:     u64,
        tx_index:  u64,
        log_index: u64,
        event:     SwapEventData
    },
    /// Liquidity event occurred
    LiquidityEvent {
        pool_id:   PoolId,
        block:     u64,
        tx_index:  u64,
        log_index: u64,
        event:     ModifyLiquidityEventData
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

    // Helper constructors
    pub fn from_swap(
        pool_id: PoolId,
        block: u64,
        tx_index: u64,
        log_index: u64,
        event: SwapEventData
    ) -> Self {
        PoolUpdate::SwapEvent { pool_id, block, tx_index, log_index, event }
    }

    pub fn from_liquidity(
        pool_id: PoolId,
        block: u64,
        tx_index: u64,
        log_index: u64,
        event: ModifyLiquidityEventData
    ) -> Self {
        PoolUpdate::LiquidityEvent { pool_id, block, tx_index, log_index, event }
    }

    pub fn from_new_pool(
        pool_id: PoolId,
        token0: Address,
        token1: Address,
        bundle_fee: u32,
        swap_fee: u32,
        protocol_fee: u32,
        tick_spacing: i32,
        block: u64
    ) -> Self {
        PoolUpdate::NewPool {
            pool_id,
            token0,
            token1,
            bundle_fee,
            swap_fee,
            protocol_fee,
            tick_spacing,
            block
        }
    }

    pub fn from_fee_update(
        pool_id: PoolId,
        block: u64,
        bundle_fee: u32,
        swap_fee: u32,
        protocol_fee: u32
    ) -> Self {
        PoolUpdate::FeeUpdate { pool_id, block, bundle_fee, swap_fee, protocol_fee }
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

/// Slot0 update from real-time feed
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Hash)]
pub struct Slot0Update {
    /// there will be 120 updates per block or per 100ms
    pub seq_id:           u16,
    /// in case of block lag on node
    pub current_block:    u64,
    pub angstrom_pool_id: PoolId,
    pub uni_pool_id:      PoolId,

    pub sqrt_price_x96: U160,
    pub liquidity:      u128,
    pub tick:           i32
}

use std::collections::VecDeque;

use crate::traits::PoolUpdateDelivery;

/// A queue-based implementation of PoolUpdateDelivery that allows feeding
/// PoolUpdate instances
pub struct PoolUpdateQueue {
    updates: VecDeque<PoolUpdate>
}

impl PoolUpdateQueue {
    /// Create a new empty PoolUpdateQueue
    pub fn new() -> Self {
        Self { updates: VecDeque::new() }
    }

    /// Add a single update to the queue
    pub fn push(&mut self, update: PoolUpdate) {
        self.updates.push_back(update);
    }

    /// Add multiple updates to the queue
    pub fn extend(&mut self, updates: impl IntoIterator<Item = PoolUpdate>) {
        self.updates.extend(updates);
    }

    /// Get the number of pending updates
    pub fn len(&self) -> usize {
        self.updates.len()
    }

    /// Check if the queue is empty
    pub fn is_empty(&self) -> bool {
        self.updates.is_empty()
    }

    /// Clear all pending updates
    pub fn clear(&mut self) {
        self.updates.clear();
    }
}

impl Default for PoolUpdateQueue {
    fn default() -> Self {
        Self::new()
    }
}

impl PoolUpdateDelivery for PoolUpdateQueue {
    fn get_new_block(&mut self) -> Option<u64> {
        match self.updates.front() {
            Some(PoolUpdate::NewBlock(block)) => {
                let block = *block;
                self.updates.pop_front();
                Some(block)
            }
            _ => None
        }
    }

    fn get_reorg(&mut self) -> Option<(u64, u64)> {
        match self.updates.front() {
            Some(PoolUpdate::Reorg { from_block, to_block }) => {
                let result = (*from_block, *to_block);
                self.updates.pop_front();
                Some(result)
            }
            _ => None
        }
    }

    fn get_new_pool(&mut self) -> Option<(PoolId, Address, Address, u32, u32, u32, i32, u64)> {
        match self.updates.front() {
            Some(PoolUpdate::NewPool {
                pool_id,
                token0,
                token1,
                bundle_fee,
                swap_fee,
                protocol_fee,
                tick_spacing,
                block
            }) => {
                let result = (
                    *pool_id,
                    *token0,
                    *token1,
                    *bundle_fee,
                    *swap_fee,
                    *protocol_fee,
                    *tick_spacing,
                    *block
                );
                self.updates.pop_front();
                Some(result)
            }
            _ => None
        }
    }

    fn get_pool_removal(&mut self) -> Option<(PoolId, u64)> {
        match self.updates.front() {
            Some(PoolUpdate::PoolRemoved { pool_id, block }) => {
                let result = (*pool_id, *block);
                self.updates.pop_front();
                Some(result)
            }
            _ => None
        }
    }

    fn get_swap_event(&mut self) -> Option<(PoolId, u64, u64, u64, SwapEventData)> {
        match self.updates.front() {
            Some(PoolUpdate::SwapEvent { pool_id, block, tx_index, log_index, event }) => {
                let pool_id = *pool_id;
                let block = *block;
                let tx_index = *tx_index;
                let log_index = *log_index;
                let event = event.clone();
                self.updates.pop_front();
                Some((pool_id, block, tx_index, log_index, event))
            }
            _ => None
        }
    }

    fn get_liquidity_event(&mut self) -> Option<(PoolId, u64, u64, u64, ModifyLiquidityEventData)> {
        match self.updates.front() {
            Some(PoolUpdate::LiquidityEvent { pool_id, block, tx_index, log_index, event }) => {
                let pool_id = *pool_id;
                let block = *block;
                let tx_index = *tx_index;
                let log_index = *log_index;
                let event = event.clone();
                self.updates.pop_front();
                Some((pool_id, block, tx_index, log_index, event))
            }
            _ => None
        }
    }

    fn get_fee_update(&mut self) -> Option<(PoolId, u64, u32, u32, u32)> {
        match self.updates.front() {
            Some(PoolUpdate::FeeUpdate { pool_id, block, bundle_fee, swap_fee, protocol_fee }) => {
                let result = (*pool_id, *block, *bundle_fee, *swap_fee, *protocol_fee);
                self.updates.pop_front();
                Some(result)
            }
            _ => None
        }
    }

    fn get_slot0_update(&mut self) -> Option<(PoolId, Slot0Data)> {
        match self.updates.front() {
            Some(PoolUpdate::UpdatedSlot0 { pool_id, data }) => {
                let pool_id = *pool_id;
                let data = data.clone();
                self.updates.pop_front();
                Some((pool_id, data))
            }
            _ => None
        }
    }
}
