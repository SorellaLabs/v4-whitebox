use std::sync::Arc;

use alloy::{primitives::address, providers::ProviderBuilder};
use tracing_subscriber;
use uni_v4::uniswap::pool_manager_service::PoolManagerService;

/// Example demonstrating how to use the PoolManagerService
///
/// This example shows:
/// 1. Setting up a provider connection
/// 2. Initializing the PoolManagerService with required addresses
/// 3. Fetching and initializing all existing pools
/// 4. Starting the block subscription service to track new pools
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
    println!("Initializing pool manager service...");

    let pool_service = PoolManagerService::new(
        provider.clone(),
        angstrom_address,
        controller_address,
        pool_manager_address,
        deploy_block,
        false // Use unlocked mode for example
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

    println!("🎉 Pool manager service is ready to use!");
    println!(
        "The service has been initialized with all existing pools and is ready for operations."
    );

    Ok(())
}
