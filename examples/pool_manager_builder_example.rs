use std::sync::Arc;

use alloy::{primitives::address, providers::ProviderBuilder};
use futures::stream;
use uni_v4::uniswap::{
    pool_key::PoolKey,
    pool_manager_service_builder::{PoolManagerServiceBuilder, Slot0Update},
    pool_providers::pool_update_provider::{PoolUpdateProvider, StateStream}
};

#[tokio::main]
async fn main() -> eyre::Result<()> {
    // Setup provider
    let provider = Arc::new(
        ProviderBuilder::new()
            .on_builtin("https://eth.llamarpc.com")
            .await?
    );

    // Define addresses
    let angstrom_address = address!("0x1111111111111111111111111111111111111111");
    let controller_address = address!("0x2222222222222222222222222222222222222222");
    let pool_manager_address = address!("0x3333333333333333333333333333333333333333");
    let deploy_block = 20000000;

    println!("Example 1: Basic builder usage");
    {
        let service = PoolManagerServiceBuilder::new(
            provider.clone(),
            angstrom_address,
            controller_address,
            pool_manager_address,
            deploy_block
        )
        .build_without_event_stream()
        .await?;

        println!("Created service with {} pools", service.pool_count());
    }

    println!("\nExample 2: Builder with custom tick range");
    {
        let service = PoolManagerServiceBuilder::new(
            provider.clone(),
            angstrom_address,
            controller_address,
            pool_manager_address,
            deploy_block
        )
        .with_initial_tick_range_size(200) // Custom tick range
        .build_without_event_stream()
        .await?;

        println!("Created service with custom tick range");
    }

    println!("\nExample 3: Builder with fixed pools and auto-creation disabled");
    {
        // Create some example pool keys
        let pool_keys = vec![
            // Add your specific pool keys here
        ];

        let service = PoolManagerServiceBuilder::new(
            provider.clone(),
            angstrom_address,
            controller_address,
            pool_manager_address,
            deploy_block
        )
        .with_fixed_pools(pool_keys)
        .with_auto_pool_creation(false)
        .build_without_event_stream()
        .await?;

        println!("Created service with fixed pools only");
    }

    println!("\nExample 4: Builder with event stream");
    {
        // Create pool update provider
        let pool_registry = uni_v4::uniswap::pool_registry::UniswapPoolRegistry::default();
        let update_provider = PoolUpdateProvider::new(
            provider.clone(),
            pool_manager_address,
            controller_address,
            angstrom_address,
            pool_registry
        )
        .await;

        // Create block stream
        let block_stream =
            uni_v4::pool_providers::completed_block_stream::CompletedBlockStream::new(
                provider.clone()
            );

        // Create state stream
        let event_stream = StateStream::new(update_provider, block_stream);

        let service = PoolManagerServiceBuilder::new(
            provider.clone(),
            angstrom_address,
            controller_address,
            pool_manager_address,
            deploy_block
        )
        .with_event_stream(event_stream)
        .with_bundle_mode(true)
        .build()
        .await?;

        println!("Created service with event stream in bundle mode");
    }

    println!("\nExample 5: Builder with slot0 stream");
    {
        // Create a mock slot0 stream
        let slot0_updates = vec![Slot0Update {
            seq_id:         0,
            current_block:  20000001,
            pool_id:        Default::default(),
            sqrt_price_x96: 1000000u128.into(),
            tick:           100
        }];
        let slot0_stream = stream::iter(slot0_updates);

        let service = PoolManagerServiceBuilder::new(
            provider.clone(),
            angstrom_address,
            controller_address,
            pool_manager_address,
            deploy_block
        )
        .with_slot0_stream(slot0_stream)
        .build_without_event_stream()
        .await?;

        println!("Created service with slot0 stream");
    }

    Ok(())
}
