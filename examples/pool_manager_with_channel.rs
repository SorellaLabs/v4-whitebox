use std::{sync::Arc, time::Duration};

use alloy::{
    eips::BlockNumberOrTag,
    primitives::address,
    providers::{Provider, ProviderBuilder, WsConnect}
};
use eyre::Result;
use futures::StreamExt;
use tokio::sync::mpsc;
use uni_v4_common::{PoolUpdate, StreamMode};
use uni_v4_upkeeper::{
    pool_manager_service_builder::{NoOpSlot0Stream, PoolManagerServiceBuilder},
    pool_providers::{
        completed_block_stream::CompletedBlockStream,
        pool_update_provider::{PoolUpdateProvider, StateStream}
    }
};

#[tokio::main]
async fn main() -> Result<()> {
    // Initialize logging
    tracing_subscriber::fmt::init();

    // Configuration - update these values
    let ws_url = std::env::var("ETH_WS_URL")
        .unwrap_or_else(|_| "wss://ethereum-mainnet.g.alchemy.com/v2/YOUR_API_KEY".to_string());

    // Uniswap V4 addresses (example addresses - replace with actual)
    let pool_manager_address = address!("0000000000000000000000000000000000000000");
    let angstrom_address = address!("0000000000000000000000000000000000000000");
    let controller_address = address!("0000000000000000000000000000000000000000");
    let deploy_block = 20_000_000;

    println!("🔌 Connecting to Ethereum node via WebSocket...");
    let ws = WsConnect::new(ws_url);
    let provider = Arc::new(ProviderBuilder::default().connect_ws(ws).await?);

    println!("📊 Setting up pool update provider...");

    // Choose the stream mode (Full or InitializationOnly)
    let stream_mode = StreamMode::Full; // Change to StreamMode::InitializationOnly to filter updates

    let update_provider = PoolUpdateProvider::new(
        provider.clone(),
        pool_manager_address,
        controller_address,
        angstrom_address,
        Default::default()
    )
    .await
    .with_stream_mode(stream_mode);

    println!("   Using stream mode: {:?}", stream_mode);

    // Create block stream
    let latest_block = provider
        .get_block(BlockNumberOrTag::Latest.into())
        .await?
        .unwrap();

    let block = latest_block.header.number;
    let prev_block_hash = latest_block.header.parent_hash;

    let block_stream = provider
        .subscribe_full_blocks()
        .into_stream()
        .await?
        .filter_map(|result| async move { result.ok() })
        .take(1000);

    let block_stream =
        CompletedBlockStream::new(prev_block_hash, block, provider.clone(), Box::pin(block_stream));
    let event_stream = StateStream::new(update_provider, block_stream);

    // Create channel for receiving pool updates
    let (tx, mut rx) = mpsc::channel::<PoolUpdate>(1000);

    // Build service with channel mode
    println!("🔧 Building pool manager service with channel mode...");
    let service = PoolManagerServiceBuilder::<_, _, NoOpSlot0Stream>::new(
        provider.clone(),
        angstrom_address,
        controller_address,
        pool_manager_address,
        deploy_block,
        event_stream
    )
    .with_initial_tick_range_size(300)
    .with_tick_edge_threshold(100)
    .with_update_channel(tx) // Enable channel mode
    .build()
    .await?;

    println!("✅ Pool service initialized in channel mode!");
    println!("📊 Found {} pools", service.get_pools().len());
    println!("🔗 Current block: {}", service.current_block());

    // Create a local pool instance for the receiver
    let initial_pools = service.get_pools();

    // Spawn the upkeeper service
    tokio::spawn(service);

    // Spawn a task to receive and process updates
    let update_processor = tokio::spawn(async move {
        let mut local_pools = initial_pools;
        let mut message_count = 0;

        println!("📨 Starting message receiver...");

        while let Some(msg) = rx.recv().await {
            message_count += 1;

            // Log the message type
            match &msg {
                PoolUpdate::NewBlock(block) => {
                    println!("📦 Block #{}: Received NewBlock", block);
                }
                PoolUpdate::NewPool { pool_id, .. } => {
                    println!("🏊 Received NewPool config for pool {:?}", pool_id);
                }
                PoolUpdate::SwapEvent { pool_id, .. } => {
                    println!("💱 Received SwapEvent for pool {:?}", pool_id);
                }
                PoolUpdate::LiquidityEvent { pool_id, .. } => {
                    println!("💧 Received LiquidityEvent for pool {:?}", pool_id);
                }
                PoolUpdate::NewTicks { pool_id, ticks, .. } => {
                    println!("📊 Received NewTicks for pool {:?} ({} ticks)", pool_id, ticks.len());
                }
                PoolUpdate::NewPoolState { pool_id, .. } => {
                    println!("🆕 Received NewPoolState with state for pool {:?}", pool_id);
                }
                PoolUpdate::Slot0Update(update) => {
                    println!("🔄 Received Slot0Update for pool {:?}", update.angstrom_pool_id);
                }
                _ => {
                    println!("📬 Received other message type");
                }
            }

            // Apply the update to our local pool instance
            local_pools.update_pools(vec![msg]);

            // Print stats every 100 messages
            if message_count % 100 == 0 {
                println!(
                    "📊 Processed {} messages, tracking {} pools",
                    message_count,
                    local_pools.len()
                );
            }
        }

        println!("Channel closed after {} messages", message_count);
    });

    // Main loop - just wait and print status
    println!("🔄 Pool manager running in channel mode...");
    println!("   All updates are being sent via channel");
    println!("   Receiver task is processing updates independently");
    println!("Press Ctrl+C to stop");

    loop {
        tokio::time::sleep(Duration::from_secs(30)).await;
        println!("⏰ Still running... (30s heartbeat)");
    }
}
