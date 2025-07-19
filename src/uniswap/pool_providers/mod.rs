use std::{ops::RangeInclusive, sync::Arc};

use alloy::{providers::Provider, rpc::types::eth::Filter};
use alloy_primitives::Log;
use futures::Stream;

use crate::{pool_providers::pool_update_provider::PoolUpdate, pools::PoolId};

pub mod completed_block_stream;
pub mod pool_update_provider;

pub trait PoolEventStream: Stream<Item = Vec<PoolUpdate>> + Send + Unpin + 'static {
    fn start_tracking_pool(&mut self, pool_id: PoolId);
    fn stop_tracking_pool(&mut self, pool_id: PoolId);
}
