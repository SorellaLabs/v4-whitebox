use std::{
    collections::HashSet,
    future::Future,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll, Waker}
};

use futures::{FutureExt, Stream, StreamExt};
use jsonrpsee::{
    core::{ClientError, client::Subscription},
    proc_macros::rpc,
    ws_client::WsClient
};
use uni_v4_common::{PoolId, Slot0Update};

#[rpc(client, namespace = "angstrom")]
#[async_trait::async_trait]
pub trait SubApi {
    #[subscription(
        name = "subscribeAmm",
        unsubscribe = "unsubscribeAmm",
        item = Slot0Update
    )]
    async fn subscribe_amm(&self, pools: HashSet<PoolId>) -> jsonrpsee::core::SubscriptionResult;
}

/// Trait for streams that provide slot0 updates with dynamic pool subscription
/// management
pub trait Slot0Stream: Stream<Item = Slot0Update> + Unpin + Send {
    /// Subscribe to updates for a set of pools
    fn subscribe_pools(&mut self, pools: HashSet<PoolId>);

    /// Unsubscribe from updates for a set of pools
    fn unsubscribe_pools(&mut self, pools: HashSet<PoolId>);

    /// Get the current set of subscribed pools
    fn subscribed_pools(&self) -> &HashSet<PoolId>;
}

/// Client for subscribing to slot0 updates via jsonrpsee
pub struct Slot0Client {
    client:               Arc<WsClient>,
    subscription:         Option<Subscription<Slot0Update>>,
    subscribed_pools:     HashSet<PoolId>,
    pending_subscription: Option<
        Pin<Box<dyn Future<Output = Result<Subscription<Slot0Update>, ClientError>> + Send>>
    >,
    waker:                Option<Waker>
}

impl Slot0Client {
    /// Create a new Slot0Client from a jsonrpsee WebSocket client
    pub fn new(client: Arc<WsClient>) -> Self {
        Self {
            client,
            subscription: None,
            subscribed_pools: HashSet::new(),
            pending_subscription: None,
            waker: None
        }
    }

    fn reconnect(&mut self) {
        let client = self.client.clone();
        let pools = self.subscribed_pools.clone();

        let connection_future = Box::pin(async move { client.subscribe_amm(pools).await });
        self.pending_subscription = Some(connection_future);
    }
}

impl Stream for Slot0Client {
    type Item = Slot0Update;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if self.waker.is_none() {
            self.waker = Some(cx.waker().clone());
        }

        if let Some(mut new_stream_future) = self.pending_subscription.take() {
            match new_stream_future.poll_unpin(cx) {
                Poll::Ready(stream) => match stream {
                    Ok(stream) => {
                        self.subscription = Some(stream);
                    }
                    Err(_) => {
                        cx.waker().wake_by_ref();
                        self.reconnect();
                    }
                },
                Poll::Pending => {
                    self.pending_subscription = Some(new_stream_future);
                }
            }
        }

        if let Some(cur_sub) = self.subscription.as_mut()
            && let Poll::Ready(update) = cur_sub.poll_next_unpin(cx)
        {
            match update {
                Some(Ok(update)) => return Poll::Ready(Some(update)),
                Some(Err(_)) | None => {
                    cx.waker().wake_by_ref();
                    self.reconnect();
                }
            }
        }

        Poll::Pending
    }
}

impl Slot0Stream for Slot0Client {
    fn subscribe_pools(&mut self, pools: HashSet<PoolId>) {
        self.subscribed_pools.extend(pools);

        let client = self.client.clone();
        let pools = self.subscribed_pools.clone();

        let connection_future = Box::pin(async move { client.subscribe_amm(pools).await });
        self.pending_subscription = Some(connection_future);

        if let Some(waker) = self.waker.as_ref() {
            waker.wake_by_ref();
        }
    }

    fn unsubscribe_pools(&mut self, pools: HashSet<PoolId>) {
        for pool in pools {
            self.subscribed_pools.remove(&pool);
        }

        let client = self.client.clone();
        let pools = self.subscribed_pools.clone();

        let connection_future = Box::pin(async move { client.subscribe_amm(pools).await });
        self.pending_subscription = Some(connection_future);

        if let Some(waker) = self.waker.as_ref() {
            waker.wake_by_ref();
        }
    }

    fn subscribed_pools(&self) -> &HashSet<PoolId> {
        &self.subscribed_pools
    }
}

/// A no-op implementation of Slot0Stream for testing or when no stream is
/// needed
#[derive(Default)]
pub struct NoOpSlot0Stream {
    placeholder: HashSet<PoolId>
}

impl Stream for NoOpSlot0Stream {
    type Item = Slot0Update;

    fn poll_next(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        Poll::Pending
    }
}

impl Slot0Stream for NoOpSlot0Stream {
    fn subscribe_pools(&mut self, _pools: HashSet<PoolId>) {
        // No-op
    }

    fn unsubscribe_pools(&mut self, _pools: HashSet<PoolId>) {
        // No-op
    }

    fn subscribed_pools(&self) -> &HashSet<PoolId> {
        &self.placeholder
    }
}
