use std::collections::HashMap;

use alloy::primitives::{Address, aliases::U24};

use super::{pool_key::PoolKey, pools::PoolId};

#[derive(Debug, Default, Clone)]
pub struct UniswapPoolRegistry {
    pub pools:                  HashMap<PoolId, PoolKey>,
    pub conversion_map:         HashMap<PoolId, PoolId>,
    pub reverse_conversion_map: HashMap<PoolId, PoolId>
}

impl UniswapPoolRegistry {
    pub fn get(&self, pool_id: &PoolId) -> Option<&PoolKey> {
        self.pools.get(pool_id)
    }

    pub fn pools(&self) -> HashMap<PoolId, PoolKey> {
        self.pools.clone()
    }

    pub fn private_keys(&self) -> impl Iterator<Item = PoolId> + '_ {
        self.conversion_map.values().copied()
    }

    pub fn public_keys(&self) -> impl Iterator<Item = PoolId> + '_ {
        self.conversion_map.keys().copied()
    }

    pub fn public_key_from_private(&self, pk: &PoolId) -> Option<PoolId> {
        self.reverse_conversion_map.get(pk).copied()
    }

    pub fn private_key_from_public(&self, pk: &PoolId) -> Option<PoolId> {
        self.conversion_map.get(pk).copied()
    }

    pub fn add_new_pool(&mut self, mut pool_key: PoolKey) {
        self.pools.insert(pool_key.into(), pool_key);

        let copyed_pub: PoolId = pool_key.into();
        pool_key.fee = U24::from(0x800000);
        let priv_key = PoolId::from(pool_key);
        self.conversion_map.insert(copyed_pub, priv_key);
        self.reverse_conversion_map.insert(priv_key, copyed_pub);
    }

    /// Get pool key by token pair (searches all pools with these tokens)
    /// Returns all pools that match the token pair, regardless of fee tier
    pub fn get_pools_by_token_pair(&self, token0: Address, token1: Address) -> Vec<&PoolKey> {
        // Normalize token order
        let (normalized_token0, normalized_token1) =
            if token0 < token1 { (token0, token1) } else { (token1, token0) };

        self.pools
            .values()
            .filter(|pool_key| {
                pool_key.currency0 == normalized_token0 && pool_key.currency1 == normalized_token1
            })
            .collect()
    }

    /// Get pool ID by token pair and fee
    /// Returns the pool ID if a pool exists with the given tokens and fee
    pub fn get_pool_id_by_tokens_and_fee(
        &self,
        token0: Address,
        token1: Address,
        fee: u32
    ) -> Option<PoolId> {
        // Normalize token order
        let (normalized_token0, normalized_token1) =
            if token0 < token1 { (token0, token1) } else { (token1, token0) };

        self.pools
            .iter()
            .find(|(_, pool_key)| {
                pool_key.currency0 == normalized_token0
                    && pool_key.currency1 == normalized_token1
                    && pool_key.fee.to::<u32>() == fee
            })
            .map(|(pool_id, _)| *pool_id)
    }
}

impl From<Vec<PoolKey>> for UniswapPoolRegistry {
    fn from(pools: Vec<PoolKey>) -> Self {
        let pubmap = pools
            .iter()
            .map(|pool_key| {
                let pool_id = PoolId::from(*pool_key);
                (pool_id, *pool_key)
            })
            .collect();

        let mut conversion_map = HashMap::new();
        let mut reverse_conversion_map = HashMap::new();

        for mut pool_key in pools {
            let pool_id_pub = PoolId::from(pool_key);
            pool_key.fee = U24::from(0x800000);
            let pool_id_priv = PoolId::from(pool_key);
            conversion_map.insert(pool_id_pub, pool_id_priv);
            reverse_conversion_map.insert(pool_id_priv, pool_id_pub);
        }

        Self { pools: pubmap, conversion_map, reverse_conversion_map }
    }
}
