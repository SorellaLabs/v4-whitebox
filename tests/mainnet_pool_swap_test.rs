use std::{collections::HashMap, sync::Arc};

use alloy::{
    primitives::{I256, address},
    providers::{Provider, ProviderBuilder}
};
use alloy_primitives::U256;
use uni_v4_structure::sqrt_pricex96::SqrtPriceX96;
use uni_v4_upkeeper::{
    pool_manager_service_builder::PoolManagerServiceBuilder, slot0::NoOpSlot0Stream
};

// Test configuration - Uses ETH_URL environment variable
fn get_eth_url() -> Option<String> {
    std::env::var("ETH_URL").ok()
}

#[tokio::test]
async fn test_specific_pool_at_block() {
    // Get ETH URL from environment
    let eth_url = get_eth_url();
    let Some(eth_url) = eth_url else {
        println!("No ETH_URL SET, returning");
        return;
    };

    let deploy_block = 22971782u64;
    let target_block = 23020805;

    // Real addresses from Sepolia deployment
    let angstrom_address = address!("0x0000000aa232009084Bd71A5797d089AA4Edfad4");
    let controller_address = address!("0x1746484EA5e11C75e009252c102C8C33e0315fD4");
    let pool_manager_address = address!("0x000000000004444c5dc75cB358380D2e3dE08A90");

    // Set the controller address for the fetch function
    uni_v4_upkeeper::fetch_pool_keys::set_controller_address(controller_address);

    // Create real provider
    let provider = Arc::new(
        ProviderBuilder::default()
            .with_recommended_fillers()
            .connect(&eth_url)
            .await
            .unwrap()
    );

    println!("Loading pools at block {} to find available pools", deploy_block);

    // Load pools to see what's available
    let service = PoolManagerServiceBuilder::new_with_noop_stream(
        provider.clone(),
        angstrom_address,
        controller_address,
        pool_manager_address,
        deploy_block
    )
    .with_initial_tick_range_size(600) // More ticks for complex swaps
    .with_auto_pool_creation(true)
    .with_current_block(target_block)
    .build()
    .await
    .expect("Failed to create service");

    let pools = service.get_pools();
    println!("\nFound {} pools at block {}", pools.get_pools().len(), deploy_block);

    // List all pools with their details
    for (idx, entry) in pools.get_pools().iter().enumerate() {
        let (pool_id, pool_state) = entry.pair();
        println!("\n[Pool {}] ID: {:?}", idx, pool_id);
        println!("block {}", pool_state.block_number());
        println!("  Token0: {:?} (decimals: {})", pool_state.token0, pool_state.token0_decimals);
        println!("  Token1: {:?} (decimals: {})", pool_state.token1, pool_state.token1_decimals);
        println!("  Current liquidity: {}", pool_state.current_liquidity());
        println!("  Current tick: {}", pool_state.current_tick());
        println!("  Tick spacing: {}", pool_state.tick_spacing());

        // Do a test swap on each pool
        if pool_state.current_liquidity() > 0 {
            println!("  Testing swap...");
            let test_amount = I256::from(U256::from(623754804)); // Small test amount
            match pool_state.swap_current_with_amount(test_amount, false, true) {
                Ok(result) => {
                    println!("    ✓ Swap successful - t1 out: {}", result.total_d_t1);
                    println!("    ✓ Swap successful - t0 out: {}", result.total_d_t0);
                }
                Err(e) => {
                    println!("    ✗ Swap failed: {}", e);
                }
            }
        }
    }
}
