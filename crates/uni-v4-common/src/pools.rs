/// Shared (thread-safe) pool implementation with DashMap
pub mod shared {
    pub use crate::shared_pools::*;
}

/// Single-threaded pool implementation without locking
pub mod pools {
    pub use crate::pools_impl::*;
}

// Re-export the shared version as default for backward compatibility
pub use crate::shared_pools::{PoolError, PoolId, SwapSimulationError, UniswapPools};
