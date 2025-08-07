use std::sync::Arc;

use alloy::{primitives::address, providers::ProviderBuilder};
use eyre::Result;
use tokio::sync::mpsc;
use uni_v4_common::PoolUpdate;
use uni_v4_upkeeper::pool_manager_service_builder::{
    NoOpEventStream, NoOpSlot0Stream, PoolManagerServiceBuilder
};

#[tokio::main]
async fn main() -> Result<()> {
    // Initialize logging
    tracing_subscriber::fmt::init();

    println!("🧪 Testing channel mode functionality...");

    // Create a mock provider
    let provider = Arc::new(ProviderBuilder::default().connect_anvil());

    // Test addresses
    let pool_manager_address = address!("0000000000000000000000000000000000000001");
    let angstrom_address = address!("0000000000000000000000000000000000000002");
    let controller_address = address!("0000000000000000000000000000000000000003");
    let deploy_block = 1;

    // Create channel for receiving pool updates
    let (tx, mut rx) = mpsc::channel::<PoolUpdate>(100);

    // Build service with channel mode
    println!("🔧 Building pool manager service with channel mode...");
    let service = PoolManagerServiceBuilder::<_, NoOpEventStream, NoOpSlot0Stream>::new(
        provider.clone(),
        angstrom_address,
        controller_address,
        pool_manager_address,
        deploy_block,
        NoOpEventStream
    )
    .with_update_channel(tx)
    .build()
    .await?;

    println!("✅ Service created successfully in channel mode!");

    // Spawn the service
    let service_handle = tokio::spawn(service);

    // Spawn a task to receive and process updates
    let receiver_handle = tokio::spawn(async move {
        println!("📨 Receiver task started, waiting for messages...");

        let mut count = 0;
        while let Some(msg) = rx.recv().await {
            count += 1;
            match msg {
                PoolUpdate::NewBlock(block) => {
                    println!("  ✅ Received NewBlock #{}", block);
                }
                PoolUpdate::NewPool { pool_id, .. } => {
                    println!("  ✅ Received NewPool for {:?}", pool_id);
                }
                PoolUpdate::NewTicks { pool_id, ticks, .. } => {
                    println!("  ✅ Received NewTicks for {:?} ({} ticks)", pool_id, ticks.len());
                }
                PoolUpdate::NewPoolState { pool_id, .. } => {
                    println!("  ✅ Received NewPoolState for {:?}", pool_id);
                }
                _ => {
                    println!("  ✅ Received other update type");
                }
            }

            // Exit after receiving a few messages for this test
            if count >= 3 {
                println!("\n🎉 Test passed! Received {} messages via channel", count);
                break;
            }
        }

        if count == 0 {
            println!("⚠️  No messages received");
        }
    });

    // Wait a bit for the test
    tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;

    // Clean shutdown
    service_handle.abort();
    let _ = receiver_handle.await;

    println!("\n✅ Channel mode test completed!");
    Ok(())
}
