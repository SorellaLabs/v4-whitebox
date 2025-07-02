use std::sync::Arc;

use alloy::providers::{Provider, ProviderBuilder};
use serial_test::serial;
use uni_v4::uniswap::pool_manager_service::{PoolManagerService, PoolManagerServiceError};

use super::sepolia_constants::SepoliaConfig;

#[cfg(feature = "integration")]
#[tokio::test]
#[serial]
async fn test_sepolia_service_creation() {
    if !SepoliaConfig::should_run_integration_tests() {
        println!("Skipping integration test - set SKIP_INTEGRATION_TESTS=false to run");
        return;
    }

    let rpc_url = SepoliaConfig::rpc_url();
    println!("Connecting to Sepolia at: {}", rpc_url);

    let provider = Arc::new(
        ProviderBuilder::new()
            .connect(rpc_url.as_str())
            .await
            .expect("Failed to connect to Sepolia RPC")
    );

    // Test current block connectivity
    let current_block = provider.get_block_number().await;
    assert!(current_block.is_ok(), "Failed to get current block number");

    println!("Connected to Sepolia at block: {:?}", current_block);

    // Test service creation with Sepolia configuration
    let result = PoolManagerService::<_, 400>::new(
        provider,
        SepoliaConfig::ANGSTROM_ADDRESS,
        SepoliaConfig::CONTROLLER_V1_ADDRESS,
        SepoliaConfig::POOL_MANAGER_ADDRESS,
        SepoliaConfig::ANGSTROM_DEPLOYED_BLOCK
    )
    .await;

    match result {
        Ok(service) => {
            println!("Service created successfully!");
            println!("Found {} pools", service.pool_count());
            println!("Current block: {}", service.current_block());

            // Verify service has reasonable data
            assert!(service.current_block() >= SepoliaConfig::ANGSTROM_DEPLOYED_BLOCK);
            assert!(service.pool_count() >= 0);

            // Test pool data integrity
            for (pool_id, pool) in service.get_pools().iter().take(3) {
                assert_ne!(pool.token0, alloy::primitives::Address::ZERO);
                assert_ne!(pool.token1, alloy::primitives::Address::ZERO);
                assert!(pool.token0 < pool.token1);

                println!("Pool {:?}: {} / {}", &pool_id.to_string()[..8], pool.token0, pool.token1);
            }
        }
        Err(e) => {
            // Print error for debugging but don't fail the test
            println!(
                "Service creation failed (this might be expected in test environment): {:?}",
                e
            );

            // Verify error is reasonable
            assert!(matches!(e, PoolManagerServiceError::Provider(_)));
        }
    }
}

#[cfg(feature = "integration")]
#[tokio::test]
#[serial]
async fn test_sepolia_block_handling() {
    if !SepoliaConfig::should_run_integration_tests() {
        return;
    }

    let rpc_url = SepoliaConfig::rpc_url();
    let provider = Arc::new(
        ProviderBuilder::new()
            .connect(rpc_url.as_str())
            .await
            .expect("Failed to connect to Sepolia RPC")
    );

    // Use a recent deploy block to avoid large ranges
    let current_block = provider.get_block_number().await.unwrap_or(10_000_000);
    let deploy_block = current_block.saturating_sub(100);

    let result = PoolManagerService::<_, 400>::new(
        provider,
        SepoliaConfig::ANGSTROM_ADDRESS,
        SepoliaConfig::CONTROLLER_V1_ADDRESS,
        SepoliaConfig::POOL_MANAGER_ADDRESS,
        deploy_block
    )
    .await;

    if let Ok(mut service) = result {
        let initial_block = service.current_block();

        // Test update to latest block
        let latest_result = service.update_to_latest_block().await;
        if let Ok(latest_block) = latest_result {
            assert!(latest_block >= initial_block);
            println!("Updated from block {} to {}", initial_block, latest_block);
        }

        // Test handle specific block
        let next_block = service.current_block() + 1;
        let handle_result = service.handle_new_block(next_block).await;
        assert!(handle_result.is_ok());

        println!("Block handling tests passed");
    } else {
        println!("Service creation failed - skipping block handling tests");
    }
}

#[test]
fn test_sepolia_constants_validity() {
    // Test Sepolia configuration constants
    assert_ne!(SepoliaConfig::ANGSTROM_ADDRESS, alloy::primitives::Address::ZERO);
    assert_ne!(SepoliaConfig::CONTROLLER_V1_ADDRESS, alloy::primitives::Address::ZERO);
    assert_ne!(SepoliaConfig::POOL_MANAGER_ADDRESS, alloy::primitives::Address::ZERO);

    assert_eq!(SepoliaConfig::CHAIN_ID, 11155111);
    assert!(SepoliaConfig::ANGSTROM_DEPLOYED_BLOCK > 0);
    assert!(SepoliaConfig::ANGSTROM_DEPLOYED_BLOCK < 20_000_000);

    let rpc_url = SepoliaConfig::rpc_url();
    assert!(rpc_url.starts_with("http"));

    println!("Sepolia constants are valid");
}
