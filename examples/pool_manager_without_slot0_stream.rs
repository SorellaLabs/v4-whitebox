use std::{collections::HashSet, sync::Arc, time::Duration};

use alloy::{
    primitives::address,
    providers::{Provider, ProviderBuilder}
};
use futures::StreamExt;
use uni_v4_common::PoolId;
use uni_v4_upkeeper::{
    pool_manager_service_builder::PoolManagerServiceBuilder,
    pool_providers::{
        completed_block_stream::CompletedBlockStream,
        pool_update_provider::{PoolUpdateProvider, StateStream}
    },
    pool_registry::UniswapPoolRegistry,
    slot0::NoOpSlot0Stream
};

/// Example demonstrating PoolManagerServiceBuilder without slot0 stream
///
/// This example shows how to:
/// 1. Use PoolManagerServiceBuilder for a simpler setup without real-time
///    updates
/// 2. Rely solely on block-based events for pool state changes
/// 3. Create services with different configurations (with/without event stream)
/// 4. Process pool updates through block events only
#[tokio::main]
async fn main() -> eyre::Result<()> {
    // Initialize logging
    tracing_subscriber::fmt::init();

    // Setup provider
    let rpc_url = std::env::var("RPC_URL").expect("no rpc url set, must be ws");

    let provider = Arc::new(ProviderBuilder::new().connect(&rpc_url).await.unwrap());

    // Configuration addresses (replace with actual deployment addresses)
    let angstrom_address = address!("0x0000000aa232009084Bd71A5797d089AA4Edfad4");
    let controller_address = address!("0x1746484EA5e11C75e009252c102C8C33e0315fD4");
    let pool_manager_address = address!("0x000000000004444c5dc75cB358380D2e3dE08A90");
    let deploy_block = 22971782;

    // Example 1: Basic setup without any streams (static pool state)

    // Example 2: Setup with event stream but no slot0 stream
    println!("\n📡 Example 1: Building pool manager with event stream only...");

    // Create event stream for block-based updates
    let pool_registry = UniswapPoolRegistry::default();
    let update_provider = PoolUpdateProvider::new(
        provider.clone(),
        pool_manager_address,
        controller_address,
        angstrom_address,
        pool_registry
    )
    .await;

    let block = provider.get_block_number().await.unwrap();
    let prev_block_hash = provider
        .get_block_by_number(block.into())
        .hashes()
        .await
        .unwrap()
        .unwrap()
        .hash();

    let block_stream = provider
        .subscribe_full_blocks()
        .full()
        .channel_size(10)
        .into_stream()
        .await
        .unwrap();

    let block_stream = CompletedBlockStream::new(
        prev_block_hash,
        block,
        provider.clone(),
        block_stream.map(|block| block.unwrap()).boxed()
    );
    let event_stream = StateStream::new(update_provider, block_stream);
    // Build service with event stream but without slot0 stream
    println!("🔧 Configuring pool manager with custom settings...");
    let service = PoolManagerServiceBuilder::<_, _, NoOpSlot0Stream>::new(
        provider.clone(),
        angstrom_address,
        controller_address,
        pool_manager_address,
        deploy_block,
        event_stream
    )
    .with_initial_tick_range_size(100) // Custom tick range (default: 300)
    .with_tick_edge_threshold(50) // When to load more ticks (default: 100)
    .with_ticks_per_batch(20) // Ticks loaded per batch (default: 10)
    .with_reorg_detection_blocks(15) // Blocks to keep for reorg detection (default: 10)
    .with_reorg_lookback_block_chunk(150) // Chunk size for reorg lookback (default: 100)
    .build()
    .await?;

    println!("✅ Pool service initialized!");
    println!("📊 Found {} pools", service.get_pools().len());
    println!("🔗 Current block: {}", service.current_block());

    // Get all pool IDs to subscribe to slot0 updates
    let pool_ids: HashSet<PoolId> = service
        .get_pools()
        .iter()
        .map(|entry| *entry.key())
        .collect();
    println!("started with pool_ids {pool_ids:#?}");

    let pools = service.get_pools();
    // spawn the upkeeper service.
    tokio::spawn(service);

    // Main event loop - process both block events and slot0 updates
    println!("🔄 Starting event processing loop...");
    println!("   Block events: Pool creations, swaps, mints, burns");
    println!("   Slot0 updates: Real-time price, liquidity, and tick changes");
    println!("Press Ctrl+C to stop");

    loop {
        tokio::time::sleep(Duration::from_secs(12)).await;
        let updated_to_block = pools.get_block();
        println!("pools are updated to block number: {updated_to_block}");
    }
}
