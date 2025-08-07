use std::{collections::HashSet, sync::OnceLock};

use alloy::{
    primitives::{Address, aliases::I24},
    providers::Provider,
    rpc::types::Filter,
    sol_types::SolEvent
};
use futures::StreamExt;
use uni_v4_common::{PoolKey, PoolKeyWithFees};

/// Controller V1 address - this could be made configurable
static CONTROLLER_V1_ADDRESS: OnceLock<Address> = OnceLock::new();

alloy::sol! {
    #[derive(Debug, PartialEq, Eq)]
    contract ControllerV1 {
        event PoolConfigured(
            address indexed asset0,
            address indexed asset1,
            uint16 tickSpacing,
            uint24 bundleFee,
            uint24 unlockedFee,
            uint24 protocolUnlockedFee
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
        if let Some(existing) = CONTROLLER_V1_ADDRESS.get()
            && *existing != address
        {
            panic!(
                "Controller address already set to different value: existing={existing:?}, \
                 new={address:?}"
            );
        }
    });
}

pub async fn fetch_angstrom_pools<P>(
    // the block angstrom was deployed at
    mut deploy_block: usize,
    end_block: usize,
    angstrom_address: Address,
    db: &P
) -> Vec<PoolKeyWithFees>
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

    logs.into_iter()
        .fold(HashSet::new(), |mut set, log| {
            if let Ok(pool) = ControllerV1::PoolConfigured::decode_log(&log.clone().into_inner()) {
                let pool_key_with_fees = PoolKeyWithFees {
                    pool_key:     PoolKey {
                        currency0:   pool.asset0,
                        currency1:   pool.asset1,
                        fee:         pool.bundleFee,
                        tickSpacing: I24::unchecked_from(pool.tickSpacing),
                        hooks:       angstrom_address
                    },
                    bundle_fee:   pool.bundleFee.to(),
                    swap_fee:     pool.unlockedFee.to(),
                    protocol_fee: pool.protocolUnlockedFee.to()
                };

                set.insert(pool_key_with_fees);
                return set;
            }

            if let Ok(pool) = ControllerV1::PoolRemoved::decode_log(&log.clone().into_inner()) {
                // For removal, we need to match by pool key, so we create a dummy with default
                // fees
                let pool_key_with_fees = PoolKeyWithFees {
                    pool_key:     PoolKey {
                        currency0:   pool.asset0,
                        currency1:   pool.asset1,
                        fee:         pool.feeInE6,
                        tickSpacing: pool.tickSpacing,
                        hooks:       angstrom_address
                    },
                    bundle_fee:   0,
                    swap_fee:     0,
                    protocol_fee: 0
                };

                set.retain(|p| p.pool_key != pool_key_with_fees.pool_key);
                return set;
            }
            set
        })
        .into_iter()
        .collect::<Vec<_>>()
}
