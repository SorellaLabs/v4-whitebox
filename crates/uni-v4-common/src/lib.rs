pub mod pools;
mod pools_impl;
mod shared_pools;
pub mod traits;
pub mod updates;

// Re-export commonly used types
pub use pools::{PoolError, PoolId, SwapSimulationError, UniswapPools};
pub use traits::{PoolUpdateDelivery, PoolUpdateDeliveryExt};
pub use updates::{
    ModifyLiquidityEventData, PoolUpdate, PoolUpdateQueue, Slot0Data, Slot0Update, SwapEventData
};

/// Configuration for what types of pool updates should be streamed
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum StreamMode {
    /// Stream all updates (default behavior)
    #[default]
    Full,
    /// Only stream initialization updates: new pools, fee updates, and slot0
    /// updates
    InitializationOnly
}
