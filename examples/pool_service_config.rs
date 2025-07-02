use std::sync::Arc;

use alloy::{
    primitives::{Address, address},
    providers::{Provider, ProviderBuilder}
};
use serde::{Deserialize, Serialize};
use tracing_subscriber;
use uni_v4::uniswap::pool_manager_service::PoolManagerService;

/// Configuration for the PoolManagerService
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PoolServiceConfig {
    /// RPC endpoint URL
    pub rpc_url:              String,
    /// Angstrom contract address
    pub angstrom_address:     Address,
    /// Controller contract address  
    pub controller_address:   Address,
    /// Pool manager contract address
    pub pool_manager_address: Address,
    /// Block number when Angstrom was deployed
    pub deploy_block:         u64,
    /// Number of ticks per side for pool initialization
    pub ticks_per_side:       u16
}

impl Default for PoolServiceConfig {
    fn default() -> Self {
        Self {
            rpc_url:              "https://sepolia.infura.io/v3/YOUR_API_KEY".to_string(),
            angstrom_address:     address!("0x1234567890123456789012345678901234567890"),
            controller_address:   address!("0x4De4326613020a00F5545074bC578C87761295c7"),
            pool_manager_address: address!("0xabcdefabcdefabcdefabcdefabcdefabcdefabcd"),
            deploy_block:         7_838_402,
            ticks_per_side:       400
        }
    }
}

impl PoolServiceConfig {
    /// Load configuration from environment variables
    pub fn from_env() -> Result<Self, Box<dyn std::error::Error>> {
        let rpc_url = std::env::var("RPC_URL")?;
        let angstrom_address = std::env::var("ANGSTROM_ADDRESS")?.parse()?;
        let controller_address = std::env::var("CONTROLLER_ADDRESS")?.parse()?;
        let pool_manager_address = std::env::var("POOL_MANAGER_ADDRESS")?.parse()?;
        let deploy_block = std::env::var("DEPLOY_BLOCK")?.parse()?;
        let ticks_per_side = std::env::var("TICKS_PER_SIDE")
            .unwrap_or_else(|_| "400".to_string())
            .parse()?;

        Ok(Self {
            rpc_url,
            angstrom_address,
            controller_address,
            pool_manager_address,
            deploy_block,
            ticks_per_side
        })
    }

    /// Save configuration to a JSON file
    pub fn save_to_file(&self, path: &str) -> Result<(), Box<dyn std::error::Error>> {
        let json = serde_json::to_string_pretty(self)?;
        std::fs::write(path, json)?;
        Ok(())
    }

    /// Load configuration from a JSON file
    pub fn load_from_file(path: &str) -> Result<Self, Box<dyn std::error::Error>> {
        let content = std::fs::read_to_string(path)?;
        let config: Self = serde_json::from_str(&content)?;
        Ok(config)
    }
}

/// Enhanced pool service runner with configuration and error handling
pub struct PoolServiceRunner {
    config: PoolServiceConfig
}

impl PoolServiceRunner {
    pub fn new(config: PoolServiceConfig) -> Self {
        Self { config }
    }

    /// Run the pool service with the given configuration
    pub async fn run(&self) -> Result<(), Box<dyn std::error::Error>> {
        tracing::info!("Starting pool service with config: {:#?}", self.config);

        // Setup provider
        let provider = Arc::new(
            ProviderBuilder::new()
                .connect(self.config.rpc_url.as_str())
                .await?
        );

        // Verify connection
        let block_number = provider.get_block_number().await?;
        tracing::info!("✅ Connected to RPC at block {}", block_number);

        // Create and initialize pool manager service
        tracing::info!("🔄 Initializing pool service...");
        let pool_service = PoolManagerService::new(
            provider.clone(),
            self.config.angstrom_address,
            self.config.controller_address,
            self.config.pool_manager_address,
            self.config.deploy_block,
            false // Use unlocked mode for example
        )
        .await?;

        tracing::info!("✅ Pool service initialized successfully!");
        tracing::info!("📊 Discovered {} pools", pool_service.pool_count());
        tracing::info!("🔗 Starting from block: {}", pool_service.current_block());

        // Log pool information
        self.log_pool_summary(&pool_service);

        tracing::info!("🎉 Pool service is ready for use!");

        // The service is now ready to be used for pool operations
        // You can access pools via pool_service.get_pools() or
        // pool_service.get_pool(pool_id)

        Ok(())
    }

    /// Log a summary of discovered pools
    fn log_pool_summary<P: Provider + Clone + 'static>(&self, service: &PoolManagerService<P>) {
        for (i, (pool_id, pool_info)) in service.get_pools().iter().enumerate() {
            tracing::info!(
                "Pool {}: {} | {}-{} | Fee: {} | Liquidity: {} | Tick: {}",
                i + 1,
                hex::encode(&pool_id[..8]), // Show first 8 bytes of pool ID
                pool_info.token0,
                pool_info.token1,
                pool_info.baseline_state.fee(),
                pool_info.baseline_state.current_liquidity(),
                pool_info.baseline_state.current_tick()
            );
        }
    }
}

/// Example with configuration-based setup
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Initialize logging
    tracing_subscriber::fmt::init();

    // Try to load config from environment first, then fall back to file or default
    let config = if let Ok(config) = PoolServiceConfig::from_env() {
        tracing::info!("📁 Loaded configuration from environment variables");
        config
    } else if let Ok(config) = PoolServiceConfig::load_from_file("pool_service_config.json") {
        tracing::info!("📁 Loaded configuration from file");
        config
    } else {
        tracing::warn!(
            "⚠️  Using default configuration. Consider setting environment variables or creating \
             pool_service_config.json"
        );
        let default_config = PoolServiceConfig::default();

        // Save default config for reference
        if let Err(e) = default_config.save_to_file("pool_service_config.example.json") {
            tracing::warn!("Failed to save example config: {}", e);
        } else {
            tracing::info!("💾 Saved example configuration to pool_service_config.example.json");
        }

        default_config
    };

    // Create and run the service
    let runner = PoolServiceRunner::new(config);

    // Handle graceful shutdown on Ctrl+C
    let runner_task = tokio::spawn(async move {
        if let Err(e) = runner.run().await {
            tracing::error!("❌ Pool service error: {}", e);
        }
    });

    // Wait for Ctrl+C
    tokio::signal::ctrl_c().await?;
    tracing::info!("🛑 Received shutdown signal, stopping service...");

    runner_task.abort();
    tracing::info!("✅ Service stopped");

    Ok(())
}
