use std::{collections::HashSet, sync::Arc, time::Duration};

use alloy::{
    primitives::address,
    providers::{Provider, ProviderBuilder}
};
use futures::StreamExt;
use jsonrpsee::ws_client::WsClientBuilder;
use uni_v4::{
    pool_providers::{
        completed_block_stream::CompletedBlockStream,
        pool_update_provider::{PoolUpdateProvider, StateStream}
    },
    slot0::NoOpSlot0Stream,
    uniswap::{
        pool_manager_service_builder::PoolManagerServiceBuilder,
        pool_registry::UniswapPoolRegistry, pools::PoolId, slot0::Slot0Client
    }
};

/// Example demonstrating PoolManagerServiceBuilder with slot0 stream for
/// real-time updates
///
/// This example shows how to:
/// 1. Connect to an Angstrom WebSocket RPC for real-time slot0 updates
/// 2. Set up event stream for block-based pool state changes
/// 3. Use PoolManagerServiceBuilder to create a service with both streams
/// 4. Handle real-time price/liquidity/tick updates via slot0
#[tokio::main]
async fn main() -> eyre::Result<()> {
    // Initialize logging
    tracing_subscriber::fmt::init();

    // Setup HTTP provider for general blockchain interaction
    let rpc_url = std::env::var("RPC_URL").expect("no rpc url set, must be ws");

    let provider = Arc::new(ProviderBuilder::new().connect(&rpc_url).await?);

    // Configuration addresses (replace with actual deployment addresses)
    let angstrom_address = address!("0x0000000aa232009084Bd71A5797d089AA4Edfad4");
    let controller_address = address!("0x1746484EA5e11C75e009252c102C8C33e0315fD4");
    let pool_manager_address = address!("0x000000000004444c5dc75cB358380D2e3dE08A90");
    let deploy_block = 22971782;

    // Connect to Angstrom WebSocket RPC for slot0 updates
    println!("🔌 Connecting to Angstrom WebSocket RPC...");
    let ws_url = std::env::var("ANGSTROM_WS_URL").expect("no angstrom ws set");

    let ws_client = Arc::new(WsClientBuilder::default().build(&ws_url).await?);
    let slot0_client = Slot0Client::new(ws_client);

    // Set up event stream for block-based updates
    println!("📡 Setting up event stream...");
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

    // Create pool manager service with both event stream and slot0 stream
    println!("🔨 Building pool manager service with slot0 stream...");
    let service = PoolManagerServiceBuilder::<_, _, NoOpSlot0Stream>::new(
        provider.clone(),
        angstrom_address,
        controller_address,
        pool_manager_address,
        deploy_block,
        event_stream
    )
    .with_slot0_stream(slot0_client)
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
        tracing::info!("pools are updated to block number: {updated_to_block}");
    }
}
