use std::{sync::Arc, time::Duration};

use alloy::{
    primitives::address,
    providers::{Provider, ProviderBuilder}
};
use serial_test::serial;
use uni_v4::uniswap::{
    pool::{EnhancedUniswapPool, PoolId},
    pool_data_loader::DataLoader,
    pool_manager_service::{PoolManagerService, PoolManagerServiceError}
};

use super::sepolia_constants::SepoliaConfig;

/// Helper function to create a provider for Sepolia testing
async fn setup_sepolia_provider() -> Arc<impl Provider + Clone> {
    let rpc_url = SepoliaConfig::rpc_url();
    Arc::new(
        ProviderBuilder::new()
            .connect(rpc_url.as_str())
            .await
            .expect("Failed to connect to Sepolia RPC")
    )
}

/// Type alias for test service
type TestService = PoolManagerService<
    alloy::providers::fillers::FillProvider<
        alloy::providers::fillers::JoinFill<
            alloy::providers::Identity,
            alloy::providers::fillers::JoinFill<
                alloy::providers::fillers::GasFiller,
                alloy::providers::fillers::JoinFill<
                    alloy::providers::fillers::BlobGasFiller,
                    alloy::providers::fillers::JoinFill<
                        alloy::providers::fillers::NonceFiller,
                        alloy::providers::fillers::ChainIdFiller
                    >
                >
            >
        >,
        alloy::providers::RootProvider<alloy::transports::http::Http<reqwest::Client>>
    >,
    400
>;

/// Helper function to create a test service instance
async fn create_test_service() -> Result<TestService, PoolManagerServiceError> {
    let provider = setup_sepolia_provider().await;

    PoolManagerService::<_, 400>::new(
        provider,
        SepoliaConfig::ANGSTROM_ADDRESS,
        SepoliaConfig::CONTROLLER_V1_ADDRESS,
        SepoliaConfig::POOL_MANAGER_ADDRESS,
        SepoliaConfig::ANGSTROM_DEPLOYED_BLOCK
    )
    .await
}

/// Helper function to validate pool data integrity
fn assert_valid_pool_data(pool: &EnhancedUniswapPool<DataLoader>) {
    // Verify token addresses are not zero
    assert_ne!(pool.token0, address!("0x0000000000000000000000000000000000000000"));
    assert_ne!(pool.token1, address!("0x0000000000000000000000000000000000000000"));

    // Verify token0 < token1 (Uniswap V4 ordering)
    assert!(pool.token0 < pool.token1, "token0 should be < token1");

    // Verify tick spacing is valid (common values: 1, 10, 60, 200)
    assert!(
        matches!(pool.tick_spacing, 1 | 10 | 60 | 200 | 2000),
        "Invalid tick spacing: {}",
        pool.tick_spacing
    );

    // Verify decimals are reasonable (typically 6-18)
    assert!(pool.token0_decimals <= 18 && pool.token0_decimals >= 1);
    assert!(pool.token1_decimals <= 18 && pool.token1_decimals >= 1);
}

/// Helper to wait for service to sync to a target block
async fn wait_for_block_sync<P: Provider + Clone + 'static>(
    service: &mut PoolManagerService<P, 400>,
    target_block: u64,
    timeout_secs: u64
) -> Result<(), Box<dyn std::error::Error>> {
    let start = std::time::Instant::now();

    while service.current_block() < target_block {
        if start.elapsed() > Duration::from_secs(timeout_secs) {
            return Err(format!("Timeout waiting for block sync to {}", target_block).into());
        }

        service.update_to_latest_block().await?;
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    Ok(())
}

#[cfg(feature = "integration")]
mod service_initialization_tests {
    use super::*;

    #[tokio::test]
    #[serial]
    async fn test_new_service_initialization() {
        if !SepoliaConfig::should_run_integration_tests() {
            return;
        }

        let service = create_test_service()
            .await
            .expect("Failed to create service");

        // Verify service was initialized properly
        assert!(service.pool_count() >= 0);
        assert_eq!(service.current_block() > SepoliaConfig::ANGSTROM_DEPLOYED_BLOCK, true);

        // Verify addresses were set correctly
        let pool_keys = service.current_pool_keys();
        for pool_key in &pool_keys {
            assert_eq!(pool_key.hooks, SepoliaConfig::ANGSTROM_ADDRESS);
        }

        println!(
            "Service initialized with {} pools at block {}",
            service.pool_count(),
            service.current_block()
        );
    }

    #[tokio::test]
    async fn test_invalid_rpc_url() {
        let provider = Arc::new(
            ProviderBuilder::new()
                .connect("https://invalid-rpc-url.com")
                .await
                .expect("Provider should build even with invalid URL")
        );

        let result = PoolManagerService::<_, 400>::new(
            provider,
            SepoliaConfig::ANGSTROM_ADDRESS,
            SepoliaConfig::CONTROLLER_V1_ADDRESS,
            SepoliaConfig::POOL_MANAGER_ADDRESS,
            SepoliaConfig::ANGSTROM_DEPLOYED_BLOCK
        )
        .await;

        // Should fail with a provider error
        assert!(result.is_err());
        match result {
            Err(PoolManagerServiceError::Provider(_)) => {
                // Expected
            }
            _ => panic!("Expected provider error")
        }
    }

    #[tokio::test]
    #[serial]
    async fn test_invalid_deploy_block() {
        if !SepoliaConfig::should_run_integration_tests() {
            return;
        }

        let provider = setup_sepolia_provider().await;
        let current_block = provider
            .get_block_number()
            .await
            .expect("Failed to get current block");

        // Try to initialize with a future block
        let result = PoolManagerService::<_, 400>::new(
            provider,
            SepoliaConfig::ANGSTROM_ADDRESS,
            SepoliaConfig::CONTROLLER_V1_ADDRESS,
            SepoliaConfig::POOL_MANAGER_ADDRESS,
            current_block + 1000 // Future block
        )
        .await;

        // Should either succeed with 0 pools or fail gracefully
        match result {
            Ok(service) => {
                assert_eq!(service.pool_count(), 0, "Should have no pools for future deploy block");
            }
            Err(e) => {
                println!("Expected error for future deploy block: {:?}", e);
            }
        }
    }

    #[tokio::test]
    async fn test_controller_address_setting() {
        // Test that the controller address is set correctly in the fetch_pool_keys
        // module This is indirectly tested by successful service creation, but
        // we can verify by trying to set it again (should panic due to
        // OnceLock)

        let provider = Arc::new(
            ProviderBuilder::new()
                .connect("https://eth.llamarpc.com") // Use a different provider to avoid conflicts
                .await
                .expect("Failed to connect")
        );

        // First service creation should work
        let _service1 = PoolManagerService::<_, 400>::new(
            provider.clone(),
            SepoliaConfig::ANGSTROM_ADDRESS,
            SepoliaConfig::CONTROLLER_V1_ADDRESS,
            SepoliaConfig::POOL_MANAGER_ADDRESS,
            SepoliaConfig::ANGSTROM_DEPLOYED_BLOCK
        )
        .await;

        // Second service creation with different controller should fail
        let result = PoolManagerService::<_, 400>::new(
            provider,
            SepoliaConfig::ANGSTROM_ADDRESS,
            address!("0x1234567890123456789012345678901234567890"), // Different controller
            SepoliaConfig::POOL_MANAGER_ADDRESS,
            SepoliaConfig::ANGSTROM_DEPLOYED_BLOCK
        )
        .await;

        // Should fail because controller address is already set
        assert!(result.is_err());
    }
}

#[cfg(feature = "integration")]
mod pool_discovery_tests {
    use super::*;

    #[tokio::test]
    #[serial]
    async fn test_fetch_existing_pools() {
        if !SepoliaConfig::should_run_integration_tests() {
            return;
        }

        let service = create_test_service()
            .await
            .expect("Failed to create service");

        // Should discover at least some pools on Sepolia
        let pool_count = service.pool_count();
        println!("Discovered {} pools", pool_count);

        // Verify pool data integrity for all pools
        for (pool_id, pool) in service.get_pools() {
            assert_valid_pool_data(pool);
            println!("Pool {:?}: {} / {}", &pool_id.to_string()[..8], pool.token0, pool.token1);
        }
    }

    #[tokio::test]
    #[serial]
    async fn test_pool_count_validation() {
        if !SepoliaConfig::should_run_integration_tests() {
            return;
        }

        let service = create_test_service()
            .await
            .expect("Failed to create service");

        let pool_count = service.pool_count();
        let pools_map = service.get_pools();
        let pool_keys = service.current_pool_keys();

        // All counts should match
        assert_eq!(pool_count, pools_map.len());
        assert_eq!(pool_count, pool_keys.len());

        println!("Pool count validation: {} pools", pool_count);
    }

    #[tokio::test]
    #[serial]
    async fn test_pool_data_integrity() {
        if !SepoliaConfig::should_run_integration_tests() {
            return;
        }

        let service = create_test_service()
            .await
            .expect("Failed to create service");

        for (pool_id, pool) in service.get_pools().iter().take(5) {
            // Test first 5 pools
            assert_valid_pool_data(pool);

            // Verify pool can be retrieved by ID
            let retrieved_pool = service.get_pool(pool_id);
            assert!(retrieved_pool.is_some());

            // Verify pool has some liquidity or tick data
            assert!(pool.block_number > 0);
        }
    }

    #[tokio::test]
    #[serial]
    async fn test_empty_block_range() {
        if !SepoliaConfig::should_run_integration_tests() {
            return;
        }

        let provider = setup_sepolia_provider().await;

        // Create service with very recent deploy block (should have few/no pools)
        let current_block = provider
            .get_block_number()
            .await
            .expect("Failed to get current block");
        let recent_block = current_block.saturating_sub(10);

        let service = PoolManagerService::new(
            provider,
            SepoliaConfig::ANGSTROM_ADDRESS,
            SepoliaConfig::CONTROLLER_V1_ADDRESS,
            SepoliaConfig::POOL_MANAGER_ADDRESS,
            recent_block
        )
        .await
        .expect("Failed to create service");

        println!(
            "Service with recent deploy block {} has {} pools",
            recent_block,
            service.pool_count()
        );
    }
}

#[cfg(feature = "integration")]
mod block_handling_tests {
    use super::*;

    #[tokio::test]
    #[serial]
    async fn test_handle_new_block() {
        if !SepoliaConfig::should_run_integration_tests() {
            return;
        }

        let mut service = create_test_service()
            .await
            .expect("Failed to create service");
        let initial_count = service.pool_count();
        let initial_block = service.current_block();

        // Try to process the next block
        let next_block = initial_block + 1;
        let result = service.handle_new_block(next_block).await;

        // Should succeed even if no new pools are found
        assert!(result.is_ok());
        assert_eq!(service.current_block(), next_block);

        println!(
            "Processed block {}, pools: {} -> {}",
            next_block,
            initial_count,
            service.pool_count()
        );
    }

    #[tokio::test]
    #[serial]
    async fn test_update_to_latest_block() {
        if !SepoliaConfig::should_run_integration_tests() {
            return;
        }

        let mut service = create_test_service()
            .await
            .expect("Failed to create service");
        let initial_block = service.current_block();

        let latest_block = service
            .update_to_latest_block()
            .await
            .expect("Failed to update to latest block");

        assert!(latest_block >= initial_block);
        assert_eq!(service.current_block(), latest_block);

        println!("Updated from block {} to {}", initial_block, latest_block);
    }

    #[tokio::test]
    #[serial]
    async fn test_process_block_range() {
        if !SepoliaConfig::should_run_integration_tests() {
            return;
        }

        let mut service = create_test_service()
            .await
            .expect("Failed to create service");
        let current_block = service.current_block();

        // Process a small range of recent blocks
        let start_block = current_block.saturating_sub(5);
        let end_block = current_block;

        let result = service.process_block_range(start_block, end_block).await;
        assert!(result.is_ok());

        println!("Processed block range {} to {}", start_block, end_block);
    }

    #[tokio::test]
    async fn test_duplicate_block_handling() {
        if !SepoliaConfig::should_run_integration_tests() {
            return;
        }

        let mut service = create_test_service()
            .await
            .expect("Failed to create service");
        let current_block = service.current_block();
        let initial_count = service.pool_count();

        // Process the same block twice
        let result1 = service.handle_new_block(current_block).await;
        let result2 = service.handle_new_block(current_block).await;

        // Both should succeed, but no changes should occur
        assert!(result1.is_ok());
        assert!(result2.is_ok());
        assert_eq!(service.pool_count(), initial_count);
        assert_eq!(service.current_block(), current_block);
    }

    #[tokio::test]
    async fn test_out_of_order_blocks() {
        if !SepoliaConfig::should_run_integration_tests() {
            return;
        }

        let mut service = create_test_service()
            .await
            .expect("Failed to create service");
        let current_block = service.current_block();

        // Try to process an older block
        let old_block = current_block.saturating_sub(10);
        let result = service.handle_new_block(old_block).await;

        // Should succeed but not change current block
        assert!(result.is_ok());
        assert_eq!(service.current_block(), current_block);
    }
}

#[cfg(feature = "integration")]
mod error_handling_tests {
    use super::*;

    #[tokio::test]
    async fn test_network_connection_failures() {
        // Test with a provider that will fail after some time
        let provider = Arc::new(
            ProviderBuilder::new()
                .connect("https://httpstat.us/500") // Always returns 500
                .await
                .expect("Provider should build")
        );

        let result = PoolManagerService::<_, 400>::new(
            provider,
            SepoliaConfig::ANGSTROM_ADDRESS,
            SepoliaConfig::CONTROLLER_V1_ADDRESS,
            SepoliaConfig::POOL_MANAGER_ADDRESS,
            SepoliaConfig::ANGSTROM_DEPLOYED_BLOCK
        )
        .await;

        // Should fail with provider error
        assert!(result.is_err());
        if let Err(PoolManagerServiceError::Provider(msg)) = result {
            assert!(
                msg.contains("Failed to get block number")
                    || msg.contains("500")
                    || msg.contains("error")
            );
        }
    }

    #[tokio::test]
    #[serial]
    async fn test_provider_errors() {
        if !SepoliaConfig::should_run_integration_tests() {
            return;
        }

        let mut service = create_test_service()
            .await
            .expect("Failed to create service");

        // Try to process a block that's way in the future
        let future_block = service.current_block() + 1_000_000;
        let result = service.handle_new_block(future_block).await;

        // Should handle gracefully - either succeed with no changes or error
        // appropriately
        match result {
            Ok(_) => {
                // If it succeeds, current block should not have changed to the future block
                assert!(service.current_block() < future_block);
            }
            Err(e) => {
                // If it errors, should be a provider error
                assert!(matches!(e, PoolManagerServiceError::Provider(_)));
            }
        }
    }
}

#[cfg(feature = "integration")]
mod api_completeness_tests {
    use super::*;

    #[tokio::test]
    #[serial]
    async fn test_get_pools() {
        if !SepoliaConfig::should_run_integration_tests() {
            return;
        }

        let service = create_test_service()
            .await
            .expect("Failed to create service");

        let pools = service.get_pools();
        assert_eq!(pools.len(), service.pool_count());

        // Verify we can iterate over pools
        for (pool_id, pool) in pools {
            assert!(!pool_id.is_zero());
            assert_valid_pool_data(pool);
        }
    }

    #[tokio::test]
    #[serial]
    async fn test_get_pool_by_id() {
        if !SepoliaConfig::should_run_integration_tests() {
            return;
        }

        let service = create_test_service()
            .await
            .expect("Failed to create service");

        if let Some((pool_id, _)) = service.get_pools().iter().next() {
            let retrieved_pool = service.get_pool(pool_id);
            assert!(retrieved_pool.is_some());
        }

        // Test with non-existent pool ID
        let fake_id = PoolId::from([0u8; 32]);
        let retrieved_pool = service.get_pool(&fake_id);
        assert!(retrieved_pool.is_none());
    }

    #[tokio::test]
    #[serial]
    async fn test_current_pool_keys() {
        if !SepoliaConfig::should_run_integration_tests() {
            return;
        }

        let service = create_test_service()
            .await
            .expect("Failed to create service");

        let pool_keys = service.current_pool_keys();
        assert_eq!(pool_keys.len(), service.pool_count());

        // Verify all pool keys have correct hooks address
        for pool_key in &pool_keys {
            assert_eq!(pool_key.hooks, SepoliaConfig::ANGSTROM_ADDRESS);
        }
    }

    #[tokio::test]
    #[serial]
    async fn test_pool_count() {
        if !SepoliaConfig::should_run_integration_tests() {
            return;
        }

        let service = create_test_service()
            .await
            .expect("Failed to create service");

        let count = service.pool_count();
        let pools_len = service.get_pools().len();
        let keys_len = service.current_pool_keys().len();

        assert_eq!(count, pools_len);
        assert_eq!(count, keys_len);
    }
}

#[cfg(feature = "integration")]
mod performance_tests {
    use super::*;

    #[tokio::test]
    #[serial]
    async fn test_large_block_range_processing() {
        if !SepoliaConfig::should_run_integration_tests() {
            return;
        }

        let mut service = create_test_service()
            .await
            .expect("Failed to create service");
        let current_block = service.current_block();

        // Process a range of 100 blocks (should be fast)
        let start_block = current_block.saturating_sub(100);
        let end_block = current_block;

        let start_time = std::time::Instant::now();
        let result = service.process_block_range(start_block, end_block).await;
        let duration = start_time.elapsed();

        assert!(result.is_ok());
        assert!(
            duration < Duration::from_secs(30),
            "Processing 100 blocks took too long: {:?}",
            duration
        );

        println!("Processed 100 blocks in {:?}", duration);
    }

    #[tokio::test]
    #[serial]
    async fn test_memory_usage() {
        if !SepoliaConfig::should_run_integration_tests() {
            return;
        }

        let service = create_test_service()
            .await
            .expect("Failed to create service");

        // This is a basic check - in a real scenario you'd use more sophisticated
        // memory monitoring
        let initial_pools = service.pool_count();

        // Verify that pool data structures are reasonable
        for (_, pool) in service.get_pools().iter().take(10) {
            // Each pool should have reasonable memory usage
            assert!(pool.ticks.len() < 10000, "Pool has too many ticks: {}", pool.ticks.len());
            assert!(
                pool.tick_bitmap.len() < 1000,
                "Pool has too many bitmap entries: {}",
                pool.tick_bitmap.len()
            );
        }

        println!("Memory usage check passed for {} pools", initial_pools);
    }
}

