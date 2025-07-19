use std::{pin::Pin, sync::Arc, task::Poll};

use alloy::{providers::Provider, rpc::types::Block};
use alloy_primitives::B256;
use futures::{FutureExt, Stream, StreamExt, future::BoxFuture, stream::FuturesOrdered};

pub type BlockQueryResponse = Block;

pub struct CompletedBlockStream<P: Provider> {
    prev_block_hash:   B256,
    prev_block_number: u64,
    provider:          Arc<P>,
    /// ParentHash
    solve_for_data:    Option<BlockQueryResponse>,
    block_stream:      Pin<Box<dyn Stream<Item = Block> + Send + 'static>>,
    processing_blocks: FuturesOrdered<BoxFuture<'static, BlockQueryResponse>>
}

impl<P: Provider> CompletedBlockStream<P> {
    pub fn new(
        prev_block_hash: B256,
        prev_block_number: u64,
        provider: Arc<P>,
        block_stream: Pin<Box<dyn Stream<Item = Block> + Send + 'static>>
    ) -> Self {
        Self {
            prev_block_hash,
            prev_block_number,
            provider,
            solve_for_data: None,
            block_stream,
            processing_blocks: FuturesOrdered::default()
        }
    }
}

impl<P: Provider + 'static> Stream for CompletedBlockStream<P> {
    type Item = BlockQueryResponse;

    fn poll_next(
        self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>
    ) -> std::task::Poll<Option<Self::Item>> {
        let this = self.get_mut();

        if let Some(wanted_block_data) = this.solve_for_data.take() {
            match this.processing_blocks.poll_next_unpin(cx) {
                Poll::Ready(Some(got_block)) => {
                    this.prev_block_hash = got_block.hash();
                    this.prev_block_number = got_block.number();

                    if wanted_block_data.header.parent_hash == this.prev_block_hash
                        || got_block.number() + 1 == wanted_block_data.number()
                    {
                        this.processing_blocks
                            .push_front(futures::future::ready(wanted_block_data).boxed());

                        Poll::Ready(Some(got_block))
                    } else {
                        this.solve_for_data = Some(wanted_block_data);

                        this.processing_blocks.push_front(
                            process_no_block(this.provider.clone(), this.prev_block_number + 1)
                                .boxed()
                        );

                        Poll::Ready(Some(got_block))
                    }
                }
                _ => {
                    // Restore if not ready, to stay in backfill mode
                    this.solve_for_data = Some(wanted_block_data);
                    Poll::Pending
                }
            }
        } else {
            // Normal mode (unchanged, but update gap detection to || for better hash proof)
            while let Poll::Ready(data) = this.block_stream.poll_next_unpin(cx) {
                match data {
                    Some(new_block) => {
                        if new_block.number() <= this.prev_block_number {
                            continue;
                        }
                        this.processing_blocks
                            .push_back(futures::future::ready(new_block).boxed());
                    }
                    None => return Poll::Ready(None)
                }
            }

            while let Poll::Ready(Some(block)) = this.processing_blocks.poll_next_unpin(cx) {
                if block.header.parent_hash != this.prev_block_hash
                    && block.number() != this.prev_block_number + 1
                    // for reorgs.
                    && this.prev_block_number != block.number()
                {
                    cx.waker().wake_by_ref(); // Optional; can remove if unnecessary
                    tracing::warn!("The block processing stream has gapped");
                    this.solve_for_data = Some(block);
                    this.processing_blocks.push_front(
                        process_no_block(this.provider.clone(), this.prev_block_number + 1).boxed()
                    );

                    return Poll::Pending;
                }

                this.prev_block_number = block.number();
                this.prev_block_hash = block.hash();

                return Poll::Ready(Some(block));
            }

            Poll::Pending
        }
    }
}

async fn process_no_block<P: Provider>(client: Arc<P>, block_number: u64) -> Block {
    client
        .get_block((block_number).into())
        .full()
        .await
        .unwrap()
        .unwrap()
}
