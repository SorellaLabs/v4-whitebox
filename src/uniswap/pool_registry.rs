use std::collections::HashMap;

use alloy::primitives::aliases::U24;

use super::{pool_key::PoolKey, pools::PoolId};

#[derive(Debug, Default, Clone)]
pub struct UniswapPoolRegistry {
    pub pools:          HashMap<PoolId, PoolKey>,
    pub conversion_map: HashMap<PoolId, PoolId>
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
        self.conversion_map
            .iter()
            .find(|(_, value)| *value == pk)
            .map(|(pk, _)| *pk)
    }

    pub fn private_key_from_public(&self, pk: &PoolId) -> Option<PoolId> {
        self.conversion_map.get(pk).copied()
    }

    pub fn add_new_pool(&mut self, mut pool_key: PoolKey) {
        self.pools.insert(pool_key.clone().into(), pool_key.clone());

        let copyed_pub: PoolId = pool_key.clone().into();
        pool_key.fee = U24::from(0x800000);
        let priv_key = PoolId::from(pool_key);
        self.conversion_map.insert(copyed_pub, priv_key);
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

        let priv_map = pools
            .into_iter()
            .map(|mut pool_key| {
                let pool_id_pub = PoolId::from(pool_key);
                pool_key.fee = U24::from(0x800000);
                let pool_id_priv = PoolId::from(pool_key);
                (pool_id_pub, pool_id_priv)
            })
            .collect();
        Self { pools: pubmap, conversion_map: priv_map }
    }
}
