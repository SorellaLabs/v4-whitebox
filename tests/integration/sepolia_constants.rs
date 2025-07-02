use alloy::primitives::{Address, address};

/// Sepolia testnet configuration constants for testing
pub struct SepoliaConfig;

impl SepoliaConfig {
    /// Angstrom contract address on Sepolia
    pub const ANGSTROM_ADDRESS: Address = address!("0x3B9172ef12bd245A07DA0d43dE29e09036626AFC");
    /// Block number when Angstrom was deployed on Sepolia
    pub const ANGSTROM_DEPLOYED_BLOCK: u64 = 8578780;
    /// Sepolia chain ID
    pub const CHAIN_ID: u64 = 11155111;
    /// Controller V1 address on Sepolia
    pub const CONTROLLER_V1_ADDRESS: Address =
        address!("0x977c67e6CEe5b5De090006E87ADaFc99Ebed2a7A");
    /// Default RPC URL for Sepolia testing
    pub const DEFAULT_RPC_URL: &'static str = "https://sepolia.gateway.tenderly.co";
    /// Alternative RPC URLs for testing failover
    pub const FALLBACK_RPC_URLS: &'static [&'static str] = &[
        "https://sepolia.infura.io/v3/9aa3d95b3bc440fa88ea12eaa4456161",
        "https://rpc.sepolia.org",
        "https://ethereum-sepolia-rpc.publicnode.com"
    ];
    /// Gas token address on Sepolia
    pub const GAS_TOKEN_ADDRESS: Address = address!("0xfff9976782d46cc05630d1f6ebab18b2324d6b14");
    /// Pool Manager address on Sepolia
    pub const POOL_MANAGER_ADDRESS: Address =
        address!("0xE03A1074c86CFeDd5C142C4F04F1a1536e203543");
    /// Position Manager address on Sepolia  
    pub const POSITION_MANAGER_ADDRESS: Address =
        address!("0x429ba70129df741B2Ca2a85BC3A2a3328e5c09b4");

    /// Get RPC URL from environment or default
    pub fn rpc_url() -> String {
        std::env::var("SEPOLIA_RPC_URL").unwrap_or_else(|_| Self::DEFAULT_RPC_URL.to_string())
    }

    /// Check if we should run integration tests (requires network access)
    pub fn should_run_integration_tests() -> bool {
        std::env::var("SKIP_INTEGRATION_TESTS").is_err()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sepolia_constants() {
        // Verify all addresses are valid
        assert_ne!(SepoliaConfig::ANGSTROM_ADDRESS, Address::ZERO);
        assert_ne!(SepoliaConfig::CONTROLLER_V1_ADDRESS, Address::ZERO);
        assert_ne!(SepoliaConfig::POOL_MANAGER_ADDRESS, Address::ZERO);

        // Verify chain ID
        assert_eq!(SepoliaConfig::CHAIN_ID, 11155111);

        // Verify deploy block is reasonable
        assert!(SepoliaConfig::ANGSTROM_DEPLOYED_BLOCK > 0);
        assert!(SepoliaConfig::ANGSTROM_DEPLOYED_BLOCK < 20_000_000); // Reasonable upper bound
    }

    #[test]
    fn test_rpc_url_configuration() {
        let url = SepoliaConfig::rpc_url();
        assert!(url.starts_with("http"));
        assert!(url.contains("sepolia") || url.contains("11155111"));
    }
}
