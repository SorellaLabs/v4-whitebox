use alloy::primitives::{U160, aliases::I24};

#[derive(Debug, Clone)]
pub enum RewardsUpdate {
    CurrentOnly {
        amount:             u128,
        expected_liquidity: u128
    },
    MultiTick {
        start_tick:      I24,
        start_liquidity: u128,
        quantities:      Vec<u128>,
        reward_checksum: U160
    }
}
