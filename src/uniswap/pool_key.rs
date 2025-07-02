use alloy::sol_types::SolValue;
use alloy_primitives::keccak256;
use serde::{Deserialize, Serialize};

use super::pool::PoolId;

alloy::sol!(
    type Currency is address;
    type IHooks is address;

    #[derive(Copy, Debug, Hash, PartialEq, Eq, Ord, PartialOrd, Serialize, Deserialize)]
    struct PoolKey {
        /// @notice The lower currency of the pool, sorted numerically
        Currency currency0;
        /// @notice The higher currency of the pool, sorted numerically
        Currency currency1;
        /// @notice The pool LP fee, capped at 1_000_000. If the highest bit is 1, the pool has a dynamic fee and must be exactly equal to 0x800000
        uint24 fee;
        /// @notice Ticks that involve positions must be a multiple of tick spacing
        int24 tickSpacing;
        /// @notice The hooks of the pool
        IHooks hooks;
    }
);

impl From<PoolKey> for PoolId {
    fn from(value: PoolKey) -> Self {
        keccak256(value.abi_encode())
    }
}
