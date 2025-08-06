use futures::Stream;
use uni_v4_common::{PoolId, PoolUpdate};

use crate::pool_registry::UniswapPoolRegistry;

pub mod completed_block_stream;
pub mod pool_update_provider;

pub trait PoolEventStream: Stream<Item = Vec<PoolUpdate>> + Send + Unpin + 'static {
    fn start_tracking_pool(&mut self, pool_id: PoolId);
    fn stop_tracking_pool(&mut self, pool_id: PoolId);
    fn set_pool_registry(&mut self, pool_registry: UniswapPoolRegistry);
}
