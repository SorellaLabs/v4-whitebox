use std::{
    collections::HashMap,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll}
};

use alloy::{
    eips::BlockId,
    primitives::U256,
    providers::{Provider, ProviderBuilder},
    rpc::types::Block
};
use futures::{Stream, StreamExt};
use uni_v4::{
    pool_providers::pool_update_provider::{PoolUpdateProvider, StateStream},
    slot0::NoOpSlot0Stream,
    sqrt_pricex96::SqrtPriceX96,
    tick_info::TickInfo,
    uniswap::{
        pool_manager_service_builder::PoolManagerServiceBuilder,
        pool_registry::UniswapPoolRegistry, pools::PoolId
    }
};

// Test configuration - Uses ETH_URL environment variable
fn get_eth_url() -> String {
    std::env::var("ETH_URL").unwrap_or_else(|_| "ws://localhost:8545".to_string())
}

use futures::future::BoxFuture;

/// Block stream that fetches a specific range of historical blocks
pub struct HistoricalBlockStream<P: Provider> {
    provider:       Arc<P>,
    start_block:    u64,
    end_block:      u64,
    current_block:  u64,
    pending_future: Option<BoxFuture<'static, Option<Block>>>
}

impl<P: Provider> HistoricalBlockStream<P> {
    pub fn new(provider: Arc<P>, start_block: u64, end_block: u64) -> Self {
        Self { provider, start_block, end_block, current_block: start_block, pending_future: None }
    }
}

impl<P: Provider + 'static> Stream for HistoricalBlockStream<P> {
    type Item = Block;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        loop {
            if self.current_block > self.end_block {
                return Poll::Ready(None);
            }

            // If we have a pending future, poll it
            if let Some(mut future) = self.pending_future.take() {
                match future.as_mut().poll(cx) {
                    Poll::Ready(Some(block)) => {
                        self.current_block += 1;
                        return Poll::Ready(Some(block));
                    }
                    Poll::Ready(None) => {
                        // Block not found, continue to next
                        self.current_block += 1;
                        continue;
                    }
                    Poll::Pending => {
                        self.pending_future = Some(future);
                        return Poll::Pending;
                    }
                }
            } else {
                // Create new future for current block
                let provider = self.provider.clone();
                let block_num = self.current_block;

                let future = Box::pin(async move {
                    match provider.get_block(BlockId::Number(block_num.into())).await {
                        Ok(Some(block)) => Some(block),
                        _ => None
                    }
                });

                self.pending_future = Some(future);
                // Continue loop to poll the future we just created
            }
        }
    }
}

#[tokio::test]
async fn test_pool_state_consistency() {
    // Get ETH URL from environment
    let eth_url = get_eth_url();

    // block range were 50k liq was added
    let deploy_block = 22971782; // Deployment block
    let initial_block = 23033108 - 4200;
    let num_blocks_to_stream = 101;
    let final_block = initial_block + num_blocks_to_stream;

    // Real addresses from Sepolia deployment
    let angstrom_address =
        alloy::primitives::address!("0x0000000aa232009084Bd71A5797d089AA4Edfad4");
    let controller_address =
        alloy::primitives::address!("0x1746484EA5e11C75e009252c102C8C33e0315fD4");
    let pool_manager_address =
        alloy::primitives::address!("0x000000000004444c5dc75cB358380D2e3dE08A90");

    // Set the controller address for the fetch function
    uni_v4::uniswap::fetch_pool_keys::set_controller_address(controller_address);

    // Create real provider
    let provider = Arc::new(
        ProviderBuilder::default()
            .with_recommended_fillers()
            .connect(&eth_url)
            .await
            .unwrap()
    );

    // Step 2: Create historical block stream
    println!("Creating block stream from {} to {}", initial_block, final_block);
    let block_stream = HistoricalBlockStream::new(provider.clone(), initial_block + 1, final_block);

    // Step 3: Create update provider and state stream at initial block
    // Get the registry from service1's factory
    let update_provider = PoolUpdateProvider::new_at_block(
        provider.clone(),
        pool_manager_address,
        controller_address,
        angstrom_address,
        UniswapPoolRegistry::default(),
        initial_block
    );

    let state_stream = StateStream::new(update_provider, block_stream);
    // Use the builder to create service with all discovered pools
    let mut service1 = PoolManagerServiceBuilder::<_, _, NoOpSlot0Stream>::new(
        provider.clone(),
        angstrom_address,
        controller_address,
        pool_manager_address,
        deploy_block,
        state_stream
    )
    .with_initial_tick_range_size(400)
    .with_auto_pool_creation(true)
    .with_current_block(initial_block)
    .build()
    .await
    .expect("Failed to create first service");

    // Step 4: Note about processing
    println!("Processing {} blocks...", num_blocks_to_stream);
    (&mut service1).await;
    println!("service has ran through speicifed block_range");

    // Step 5: Now capture the state AFTER updates have been processed
    // Get the updated state from service1 after processing all updates
    println!("\nCapturing state after updates...");

    // Apply updates to service1
    // Since update_pools is private, we need to manually update the pools
    // For now, we'll just rely on the fresh service2 to get the final state

    // Define pool state snapshot structure
    #[derive(Debug)]
    struct PoolStateSnapshot {
        current_tick:      i32,
        current_liquidity: u128,
        sqrt_price:        SqrtPriceX96,
        tick_spacing:      i32,
        initialized_ticks: HashMap<i32, TickInfo>,
        tick_bitmap:       HashMap<i16, U256>
    }

    let updated_pools = service1.get_pools();

    // Capture complete pool state after updates
    let mut service1_pool_states: HashMap<PoolId, PoolStateSnapshot> = HashMap::new();
    let tracked_pool_ids: Vec<PoolId> = updated_pools
        .get_pools()
        .iter()
        .map(|entry| *entry.key())
        .collect();
    let initial_pool_count = tracked_pool_ids.len();

    for pool_id in &tracked_pool_ids {
        if let Some(pool_ref) = updated_pools.get_pools().get(pool_id) {
            let pool_state = pool_ref.value();
            let baseline = pool_state.get_baseline_liquidity();

            // Capture complete state
            let snapshot = PoolStateSnapshot {
                current_tick:      baseline.get_current_tick(),
                current_liquidity: pool_state.current_liquidity(),
                sqrt_price:        pool_state.current_price(),
                tick_spacing:      baseline.get_tick_spacing(),
                initialized_ticks: baseline.initialized_ticks().clone(),
                tick_bitmap:       baseline.tick_bitmap().clone()
            };

            println!(
                "Pool {:?} - tick: {}, liquidity: {}, initialized ticks: {}",
                pool_id,
                snapshot.current_tick,
                snapshot.current_liquidity,
                snapshot.initialized_ticks.len()
            );

            service1_pool_states.insert(*pool_id, snapshot);
        }
    }

    // Step 6: Initialize second service at final_block
    println!("\nInitializing fresh service at block {}...", final_block);

    let service2 = PoolManagerServiceBuilder::new_with_noop_stream(
        provider.clone(),
        angstrom_address,
        controller_address,
        pool_manager_address,
        deploy_block
    )
    .with_initial_tick_range_size(400)
    .with_current_block(final_block)
    .build()
    .await
    .expect("Failed to create second service");

    // Step 7: Compare tick data between updated service1 and fresh service2
    let fresh_pools = service2.get_pools();
    let fresh_pool_count = fresh_pools.get_pools().len();
    println!("\nFresh service found {} pools", fresh_pool_count);

    // Compare pools that existed initially
    let mut comparison_results = Vec::new();

    for pool_id in &tracked_pool_ids {
        let service1_state = service1_pool_states.get(pool_id);
        let fresh_pool_ref = fresh_pools.get_pools().get(pool_id);

        match (service1_state, fresh_pool_ref) {
            (Some(service1_snapshot), Some(fresh_ref)) => {
                let fresh = fresh_ref.value();
                let fresh_baseline = fresh.get_baseline_liquidity();

                // Compare basic state
                let mut mismatches = Vec::new();
                let mut subset_valid = true;

                // Check basic metrics
                if service1_snapshot.current_tick != fresh_baseline.get_current_tick() {
                    mismatches.push(format!(
                        "current tick: {} vs {}",
                        service1_snapshot.current_tick,
                        fresh_baseline.get_current_tick()
                    ));
                }
                if service1_snapshot.current_liquidity != fresh.current_liquidity() {
                    mismatches.push(format!(
                        "current liquidity: {} vs {}",
                        service1_snapshot.current_liquidity,
                        fresh.current_liquidity()
                    ));
                }
                if service1_snapshot.sqrt_price != fresh.current_price() {
                    mismatches.push(format!(
                        "sqrt price: {:?} vs {:?}",
                        service1_snapshot.sqrt_price,
                        fresh.current_price()
                    ));
                }
                if service1_snapshot.tick_spacing != fresh_baseline.get_tick_spacing() {
                    mismatches.push(format!(
                        "tick spacing: {} vs {}",
                        service1_snapshot.tick_spacing,
                        fresh_baseline.get_tick_spacing()
                    ));
                }

                // Check that all ticks in service2 exist in service1 with matching values
                let fresh_ticks = fresh_baseline.initialized_ticks();
                for (tick, fresh_tick_info) in fresh_ticks {
                    if let Some(service1_tick_info) = service1_snapshot.initialized_ticks.get(tick)
                    {
                        if service1_tick_info.liquidity_net != fresh_tick_info.liquidity_net {
                            mismatches.push(format!(
                                "tick {} liquidity_net mismatch: {} vs {}",
                                tick,
                                service1_tick_info.liquidity_net,
                                fresh_tick_info.liquidity_net
                            ));
                            subset_valid = false;
                        }
                    } else {
                        mismatches.push(format!("tick {} missing in service1 state", tick));
                        subset_valid = false;
                    }
                }

                // Log tick count and bitmap bounds comparison
                println!(
                    "  Pool {:?}: service1 has {} ticks, service2 has {} ticks",
                    pool_id,
                    service1_snapshot.initialized_ticks.len(),
                    fresh_ticks.len()
                );

                if mismatches.is_empty() && subset_valid {
                    comparison_results.push(format!(
                        "✅ Pool {:?}: Service2 state is subset of service1",
                        pool_id
                    ));
                } else {
                    comparison_results.push(format!(
                        "❌ Pool {:?}: {} issues - {}",
                        pool_id,
                        mismatches.len(),
                        mismatches.join(", ")
                    ));
                }
            }
            (None, Some(_)) => {
                comparison_results
                    .push(format!("⚠️  Pool {:?} missing in service1 state", pool_id));
            }
            (Some(_), None) => {
                comparison_results
                    .push(format!("⚠️  Pool {:?} missing in service2 state", pool_id));
            }
            (None, None) => {
                comparison_results.push(format!("⚠️  Pool {:?} missing in both states", pool_id));
            }
        }
    }

    // Print comparison summary
    println!("\n=== Comparison Results ===");
    for result in &comparison_results {
        println!("{}", result);
    }

    let failures = comparison_results
        .iter()
        .filter(|r| r.contains("❌"))
        .count();
    let successes = comparison_results
        .iter()
        .filter(|r| r.contains("✅"))
        .count();

    println!("\n=== Test Summary ===");
    println!("   Initial pools: {}", initial_pool_count);
    println!("   Fresh pools: {}", fresh_pool_count);
    println!("   Successful comparisons: {}", successes);
    println!("   Failed comparisons: {}", failures);

    // Fail the test if there were any mismatches
    assert_eq!(failures, 0, "Pool state comparison failed for {} pools", failures);

    println!("\n✅ Integration test completed successfully!");
}
