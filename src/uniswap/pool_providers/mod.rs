use std::{ops::RangeInclusive, sync::Arc};

use alloy::{providers::Provider, rpc::types::eth::Filter};
use alloy_primitives::Log;

pub mod completed_block_stream;
pub mod mock_block_stream;
pub mod pool_update_provider;
pub mod provider_adapter;

#[derive(Debug, thiserror::Error)]
pub enum PoolManagerError {
    #[error("Generic pool manager error")]
    Generic,
    #[error("Transport error: {0}")]
    Transport(#[from] alloy::transports::RpcError<alloy::transports::TransportErrorKind>)
}

pub trait PoolManagerProvider: Send + Sync + Clone + Unpin {
    fn subscribe_blocks(self) -> futures::stream::BoxStream<'static, Option<PoolMangerBlocks>>;

    fn get_logs(&self, filter: &Filter) -> Result<Vec<Log>, PoolManagerError>;
    fn provider(&self) -> Arc<impl Provider>;
}

#[derive(Debug, Clone)]
pub enum PoolMangerBlocks {
    NewBlock(u64),
    Reorg(u64, RangeInclusive<u64>)
}
