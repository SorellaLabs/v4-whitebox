use std::sync::Arc;

use alloy::{primitives::address, providers::ProviderBuilder};
use tracing_subscriber;
use uni_v4::uniswap::pool_manager_service::PoolManagerService;

/// Example demonstrating how to use the PoolManagerService with block handling
///
/// This example shows:
/// 1. Setting up and initializing the service
/// 2. Processing individual blocks for new pools
/// 3. Updating to the latest block
/// 4. Processing a range of blocks
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Initialize logging
    tracing_subscriber::fmt::init();

    // Setup provider (replace with your RPC endpoint)
    let rpc_url = std::env::var("RPC_URL")
        .unwrap_or_else(|_| "https://sepolia.infura.io/v3/YOUR_API_KEY".to_string());

    let provider = Arc::new(ProviderBuilder::new().connect(rpc_url.as_str()).await?);

    // Configuration addresses (replace with actual deployment addresses)
    let angstrom_address = address!("0x1234567890123456789012345678901234567890"); // Replace with actual Angstrom address
    let controller_address = address!("0x4De4326613020a00F5545074bC578C87761295c7"); // Replace with actual Controller address  
    let pool_manager_address = address!("0xabcdefabcdefabcdefabcdefabcdefabcdefabcd"); // Replace with actual PoolManager address
    let deploy_block = 7_838_402u64; // Replace with actual deployment block

    // Create and initialize the pool manager service
    println!("🔄 Initializing pool manager service...");

    let mut pool_service = PoolManagerService::new(
        provider.clone(),
        angstrom_address,
        controller_address,
        pool_manager_address,
        deploy_block
    )
    .await?;

    println!("✅ Pool service initialized successfully!");
    println!("📊 Found {} pools", pool_service.pool_count());
    println!("🔗 Current block: {}", pool_service.current_block());

    // Display information about discovered pools
    for (pool_id, pool_info) in pool_service.get_pools() {
        println!("Pool: {:?}", pool_id);
        println!("  Token0: {}", pool_info.token0);
        println!("  Token1: {}", pool_info.token1);
        println!("  Fee: {}", pool_info.baseline_state.fee());
        println!("  Tick Spacing: {}", pool_info.baseline_state.tick_spacing());
        println!("  Liquidity: {}", pool_info.baseline_state.current_liquidity());
        println!("  Current Tick: {}", pool_info.baseline_state.current_tick());
        println!("  ---");
    }

    // Example 1: Update to the latest block
    println!("🔍 Updating to latest block...");
    let latest_block = pool_service.update_to_latest_block().await?;
    println!("📈 Updated to block: {}", latest_block);
    println!("📊 Pool count after update: {}", pool_service.pool_count());

    // Example 2: Process a specific block manually
    println!("🎯 Processing next block manually...");
    let next_block = latest_block + 1;
    match pool_service.handle_new_block(next_block).await {
        Ok(_) => {
            println!("✅ Successfully processed block {}", next_block);
            println!("📊 Pool count: {}", pool_service.pool_count());
        }
        Err(e) => {
            println!("⚠️ Note: Block {} may not exist yet: {}", next_block, e);
        }
    }

    // Example 3: Process a range of historical blocks (if needed)
    if let (Ok(start), Ok(end)) = (
        std::env::var("START_BLOCK")
            .and_then(|s| s.parse::<u64>().map_err(|_| std::env::VarError::NotPresent)),
        std::env::var("END_BLOCK")
            .and_then(|s| s.parse::<u64>().map_err(|_| std::env::VarError::NotPresent))
    ) {
        println!("🔄 Processing block range {} to {}...", start, end);
        pool_service.process_block_range(start, end).await?;
        println!("✅ Finished processing block range");
        println!("📊 Final pool count: {}", pool_service.pool_count());
    }

    // Example 4: Simulate a block monitoring loop
    println!("🔄 Simulating block monitoring (press Ctrl+C to stop)...");
    let mut last_checked_block = pool_service.current_block();

    loop {
        tokio::time::sleep(tokio::time::Duration::from_secs(12)).await; // ~1 block time

        match pool_service.update_to_latest_block().await {
            Ok(current_block) => {
                if current_block > last_checked_block {
                    println!(
                        "🆕 New block: {} (found {} new pools)",
                        current_block,
                        pool_service.pool_count()
                    );
                    last_checked_block = current_block;
                }
            }
            Err(e) => {
                println!("❌ Error checking for new blocks: {}", e);
            }
        }
    }
}
