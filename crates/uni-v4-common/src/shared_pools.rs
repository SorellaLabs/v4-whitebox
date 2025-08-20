use std::{
    ops::Deref,
    sync::{Arc, atomic::AtomicU64}
};

use alloy_primitives::{B256, FixedBytes};
use dashmap::{DashMap, mapref::one::Ref};
use thiserror::Error;
use tokio::sync::{
    Notify,
    futures::{Notified, OwnedNotified}
};
use uni_v4_structure::BaselinePoolState;
use uniswap_v3_math::error::UniswapV3MathError;

use crate::{
    traits::{PoolUpdateDelivery, PoolUpdateDeliveryExt},
    updates::PoolUpdate
};

#[derive(Clone)]
pub struct UniswapPools {
    pools:        Arc<DashMap<PoolId, BaselinePoolState>>,
    // what block these are up to date for.
    block_number: Arc<AtomicU64>,
    // When the manager for the pools pushes a new block. It will notify all people who are
    // waiting.
    notifier:     Arc<Notify>
}

impl Deref for UniswapPools {
    type Target = Arc<DashMap<PoolId, BaselinePoolState>>;

    fn deref(&self) -> &Self::Target {
        &self.pools
    }
}

impl UniswapPools {
    pub fn new(pools: Arc<DashMap<PoolId, BaselinePoolState>>, block_number: u64) -> Self {
        Self {
            pools,
            block_number: Arc::new(AtomicU64::from(block_number)),
            notifier: Arc::new(Notify::new())
        }
    }

    pub fn get_block(&self) -> u64 {
        self.block_number.load(std::sync::atomic::Ordering::SeqCst)
    }

    pub async fn wait_for_next_update(&self) {
        self.notifier.notified().await;
    }

    pub fn get_pool(&self, pool_id: &PoolId) -> Option<Ref<'_, PoolId, BaselinePoolState>> {
        self.pools.get(pool_id)
    }

    pub fn get_pools(&self) -> &DashMap<PoolId, BaselinePoolState> {
        &self.pools
    }

    pub fn next_block_future(&self) -> Notified<'_> {
        self.notifier.notified()
    }

    pub fn next_block_future_owned(&self) -> OwnedNotified {
        self.notifier.clone().notified_owned()
    }

    pub fn update_pools(&self, mut updates: Vec<PoolUpdate>) {
        if updates.is_empty() {
            return
        }

        let mut new_block_number = None;
        // we sort ascending
        updates.sort_by(|a, b| a.sort(b));

        for update in updates {
            match update {
                PoolUpdate::NewBlock(block_number) => {
                    new_block_number = Some(block_number);
                }
                PoolUpdate::Reorg { to_block, .. } => {
                    new_block_number = Some(to_block);
                }
                PoolUpdate::SwapEvent { pool_id, event, .. } => {
                    let Some(mut pool) = self.pools.get_mut(&pool_id) else {
                        continue;
                    };

                    let state = pool.value_mut();
                    // update slot0 values
                    state.update_slot0(event.tick, event.sqrt_price_x96.into(), event.liquidity);
                }
                PoolUpdate::LiquidityEvent { pool_id, event, .. } => {
                    let Some(mut pool) = self.pools.get_mut(&pool_id) else {
                        continue;
                    };
                    let state = pool.value_mut();

                    state.update_liquidity(
                        event.tick_lower,
                        event.tick_upper,
                        event.liquidity_delta
                    );
                }
                PoolUpdate::FeeUpdate { pool_id, bundle_fee, swap_fee, protocol_fee, .. } => {
                    let Some(mut pool) = self.pools.get_mut(&pool_id) else {
                        continue;
                    };
                    let fees = pool.value_mut().fees_mut();

                    fees.bundle_fee = bundle_fee;
                    fees.swap_fee = swap_fee;
                    fees.protocol_fee = protocol_fee;
                }
                PoolUpdate::UpdatedSlot0 { pool_id, data } => {
                    let Some(mut pool) = self.pools.get_mut(&pool_id) else {
                        continue;
                    };

                    let state = pool.value_mut();
                    state.update_slot0(data.tick, data.sqrt_price_x96.into(), data.liquidity);
                }
                PoolUpdate::NewTicks { pool_id, ticks, tick_bitmap } => {
                    let Some(mut pool) = self.pools.get_mut(&pool_id) else {
                        continue;
                    };

                    let baseline = pool.value_mut().get_baseline_liquidity_mut();

                    // Merge new ticks with existing ones
                    baseline.initialized_ticks_mut().extend(ticks);

                    // Update tick bitmap
                    for (word_pos, word) in tick_bitmap {
                        baseline.update_tick_bitmap(word_pos, word);
                    }
                }
                PoolUpdate::NewPoolState { pool_id, state } => {
                    self.pools.insert(pool_id, state);
                }
                PoolUpdate::Slot0Update(update) => {
                    let Some(mut pool) = self.pools.get_mut(&update.angstrom_pool_id) else {
                        continue;
                    };

                    pool.value_mut().update_slot0(
                        update.tick,
                        update.sqrt_price_x96.into(),
                        update.liquidity
                    );
                }
                _ => {}
            }
        }

        if let Some(bn) = new_block_number {
            self.block_number
                .store(bn, std::sync::atomic::Ordering::SeqCst);
            self.notifier.notify_waiters();
        }
    }

    /// Update pools using a PoolUpdateDelivery source
    /// Processes all available updates from the source
    pub fn update_from_source<T: PoolUpdateDelivery>(&self, source: &mut T) {
        let mut updates = Vec::new();

        // Collect all available updates using the extension trait
        while let Some(update) = source.next_update() {
            updates.push(update);
        }

        // Process them using the existing method
        self.update_pools(updates);
    }

    /// Update pools by processing a single update from a PoolUpdateDelivery
    /// source Returns true if an update was processed, false if no updates
    /// were available
    pub fn update_single_from_source<T: PoolUpdateDelivery>(&self, source: &mut T) -> bool {
        if let Some(update) = source.next_update() {
            self.update_pools(vec![update]);
            true
        } else {
            false
        }
    }
}

/// Pool identifier type
pub type PoolId = FixedBytes<32>;

#[derive(Error, Debug)]
pub enum SwapSimulationError {
    #[error("Could not get next tick")]
    InvalidTick,
    #[error(transparent)]
    UniswapV3MathError(#[from] UniswapV3MathError),
    #[error("Liquidity underflow")]
    LiquidityUnderflow,
    #[error("Invalid sqrt price limit")]
    InvalidSqrtPriceLimit,
    #[error("Amount specified must be non-zero")]
    ZeroAmountSpecified
}

#[derive(Error, Debug)]
pub enum PoolError {
    #[error("Invalid signature: [{}]", .0.iter().map(|b| format!("0x{}", alloy::hex::encode(b))).collect::<Vec<_>>().join(", "))]
    InvalidEventSignature(Vec<B256>),
    #[error("Swap simulation failed")]
    SwapSimulationFailed,
    #[error("Pool already initialized")]
    PoolAlreadyInitialized,
    #[error("Pool is not initialized")]
    PoolNotInitialized,
    #[error(transparent)]
    SwapSimulationError(#[from] SwapSimulationError),
    #[error(transparent)]
    AlloyContractError(#[from] alloy::contract::Error),
    #[error(transparent)]
    AlloySolTypeError(#[from] alloy::sol_types::Error),
    #[error(transparent)]
    Eyre(#[from] eyre::Error)
}
