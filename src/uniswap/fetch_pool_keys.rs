use std::{collections::HashSet, sync::OnceLock};

use alloy::{
    primitives::{Address, aliases::I24},
    providers::Provider,
    rpc::types::Filter,
    sol_types::SolEvent
};
use futures::StreamExt;

use super::pool_key::PoolKey;

/// Controller V1 address - this could be made configurable
static CONTROLLER_V1_ADDRESS: OnceLock<Address> = OnceLock::new();

alloy::sol! {
    #[derive(Debug, PartialEq, Eq)]
    contract ControllerV1 {
        event PoolConfigured(
            address indexed asset0,
            address indexed asset1,
            uint24 indexed bundleFee,
            uint16 tickSpacing,
            address hook,
            uint24 feeInE6
        );

        event PoolRemoved(
            address indexed asset0,
            address indexed asset1,
            uint24 indexed feeInE6,
            int24 tickSpacing
        );
    }
}

pub fn set_controller_address(address: Address) {
    CONTROLLER_V1_ADDRESS.set(address).unwrap_or_else(|_| {
        // In test environments, controller address might already be set
        // This is acceptable as long as it's the same address
        if let Some(existing) = CONTROLLER_V1_ADDRESS.get() {
            if *existing != address {
                panic!(
                    "Controller address already set to different value: existing={:?}, new={:?}",
                    existing, address
                );
            }
        }
    });
}

pub async fn fetch_angstrom_pools<P>(
    // the block angstrom was deployed at
    mut deploy_block: usize,
    end_block: usize,
    angstrom_address: Address,
    db: &P
) -> Vec<PoolKey>
where
    P: Provider
{
    let mut filters = vec![];
    let controller_address = *CONTROLLER_V1_ADDRESS
        .get()
        .expect("Controller address not set. Call set_controller_address() first.");

    loop {
        let this_end_block = std::cmp::min(deploy_block + 99_999, end_block);

        if this_end_block == deploy_block {
            break;
        }

        tracing::info!(?deploy_block, ?this_end_block);
        let filter = Filter::new()
            .from_block(deploy_block as u64)
            .to_block(this_end_block as u64)
            .address(controller_address);

        filters.push(filter);

        deploy_block = std::cmp::min(end_block, this_end_block);
    }

    let logs = futures::stream::iter(filters)
        .map(|filter| async move {
            db.get_logs(&filter)
                .await
                .unwrap()
                .into_iter()
                .collect::<Vec<_>>()
        })
        .buffered(10)
        .collect::<Vec<_>>()
        .await
        .into_iter()
        .flatten()
        .collect::<Vec<_>>();
    tracing::info!(?logs);

    logs.into_iter()
        .fold(HashSet::new(), |mut set, log| {
            if let Ok(pool) = ControllerV1::PoolConfigured::decode_log(&log.clone().into_inner()) {
                let pool_key = PoolKey {
                    currency0:   pool.asset0,
                    currency1:   pool.asset1,
                    fee:         pool.bundleFee,
                    tickSpacing: I24::try_from_be_slice(&{
                        let bytes = pool.tickSpacing.to_be_bytes();
                        let mut a = [0u8; 3];
                        a[1..3].copy_from_slice(&bytes);
                        a
                    })
                    .unwrap(),
                    hooks:       angstrom_address
                };

                set.insert(pool_key);
                return set;
            }

            if let Ok(pool) = ControllerV1::PoolRemoved::decode_log(&log.clone().into_inner()) {
                let pool_key = PoolKey {
                    currency0:   pool.asset0,
                    currency1:   pool.asset1,
                    fee:         pool.feeInE6,
                    tickSpacing: pool.tickSpacing,
                    hooks:       angstrom_address
                };

                set.remove(&pool_key);
                return set;
            }
            set
        })
        .into_iter()
        .collect::<Vec<_>>()
}
