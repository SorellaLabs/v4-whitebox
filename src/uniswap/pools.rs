use std::sync::{Arc, atomic::AtomicU64};

use alloy_primitives::{B256, FixedBytes};
use dashmap::{DashMap, mapref::one::Ref};
use thiserror::Error;
use tokio::sync::{Notify, futures::Notified};
use uniswap_v3_math::error::UniswapV3MathError;

use super::ConversionError;
use crate::{BaselinePoolState, pool_providers::pool_update_provider::PoolUpdate};

#[derive(Clone)]
pub struct UniswapPools {
    pools:        Arc<DashMap<PoolId, BaselinePoolState>>,
    // what block these are up to date for.
    block_number: Arc<AtomicU64>,
    // When the manager for the pools pushes a new block. It will notify all people who are
    // waiting.
    notifier:     Arc<Notify>
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

    pub(crate) fn update_pools(&self, mut updates: Vec<PoolUpdate>) {
        let mut new_block_number = 0;

        // we sort ascending
        updates.sort_by(|a, b| a.sort(&b));

        for update in updates {
            match update {
                PoolUpdate::NewBlock(block_number) => {
                    new_block_number = block_number;
                }
                PoolUpdate::Reorg { to_block, .. } => {
                    new_block_number = to_block;
                }
                PoolUpdate::SwapEvent { pool_id, event, .. } => {
                    let mut pool = self.pools.get_mut(&pool_id).unwrap();
                    let state = pool.value_mut();
                    // update slot0 values
                    state.update_slot0(event.tick, event.sqrt_price_x96.into(), event.liquidity);
                }
                PoolUpdate::LiquidityEvent { pool_id, event, .. } => {
                    let mut pool = self.pools.get_mut(&pool_id).unwrap();
                    let state = pool.value_mut();

                    state.update_liquidity(
                        event.tick_lower,
                        event.tick_upper,
                        event.liquidity_delta
                    );
                }
                PoolUpdate::FeeUpdate { pool_id, bundle_fee, swap_fee, protocol_fee, .. } => {
                    let mut pool = self.pools.get_mut(&pool_id).unwrap();
                    let fees = pool.value_mut().fees_mut();

                    fees.bundle_fee = bundle_fee;
                    fees.swap_fee = swap_fee;
                    fees.protocol_fee = protocol_fee;
                }
                PoolUpdate::UpdatedSlot0 { pool_id, data } => {
                    let mut pool = self.pools.get_mut(&pool_id).unwrap();
                    let state = pool.value_mut();
                    state.update_slot0(data.tick, data.sqrt_price_x96.into(), data.liquidity);
                }
                _ => {}
            }
        }

        assert!(
            new_block_number != 0,
            "Got a update but no block info with update. Should never happen"
        );

        self.block_number
            .store(new_block_number, std::sync::atomic::Ordering::SeqCst);

        self.notifier.notify_waiters();
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
    ConversionError(#[from] ConversionError),
    #[error(transparent)]
    Eyre(#[from] eyre::Error)
}
