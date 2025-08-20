use std::ops::Neg;

use alloy::primitives::{I256, U256};
// use itertools::Itertools;
use uniswap_v3_math::tick_math::{MAX_SQRT_RATIO, MIN_SQRT_RATIO};

use super::{FeeConfiguration, liquidity_base::LiquidityAtPoint};
use crate::{ray::Ray, sqrt_pricex96::SqrtPriceX96};

const U256_1: U256 = U256::from_limbs([1, 0, 0, 0]);

#[derive(Debug, Clone)]
pub struct PoolSwap<'a> {
    pub(super) liquidity:     LiquidityAtPoint<'a>,
    /// swap to sqrt price limit
    pub(super) target_price:  Option<SqrtPriceX96>,
    /// if its negative, it is an exact out.
    pub(super) target_amount: I256,
    /// zfo = true
    pub(super) direction:     bool,
    // the fee configuration of the pool.
    pub(super) fee_config:    FeeConfiguration,
    pub(super) is_bundle:     bool
}

impl<'a> PoolSwap<'a> {
    pub fn swap(mut self) -> eyre::Result<PoolSwapResult<'a>> {
        // We want to ensure that we set the right limits and are swapping the correct
        // way.

        if self.direction
            && self
                .target_price
                .as_ref()
                .map(|target_price| target_price > &self.liquidity.current_sqrt_price)
                .unwrap_or_default()
        {
            return Err(eyre::eyre!("direction and sqrt_price diverge"));
        }

        let range_start = self.liquidity.current_sqrt_price;
        let range_start_tick = self.liquidity.current_tick;

        let exact_input = self.target_amount.is_positive();
        let sqrt_price_limit_x96 = self.target_price.map(|p| p.into()).unwrap_or_else(|| {
            if self.direction { MIN_SQRT_RATIO + U256_1 } else { MAX_SQRT_RATIO - U256_1 }
        });

        let mut amount_remaining = self.target_amount;
        let mut sqrt_price_x96: U256 = self.liquidity.current_sqrt_price.into();

        let mut steps = Vec::new();

        while amount_remaining != I256::ZERO && sqrt_price_x96 != sqrt_price_limit_x96 {
            let sqrt_price_start_x_96 = sqrt_price_x96;

            let (next_tick, liquidity, init) = self
                .liquidity
                .get_to_next_initialized_tick_within_one_word(self.direction)?;

            let sqrt_price_next_x96 =
                uniswap_v3_math::tick_math::get_sqrt_ratio_at_tick(next_tick)?;

            let target_sqrt_ratio = if (self.direction
                && sqrt_price_next_x96 < sqrt_price_limit_x96)
                || (!self.direction && sqrt_price_next_x96 > sqrt_price_limit_x96)
            {
                sqrt_price_limit_x96
            } else {
                sqrt_price_next_x96
            };

            // Use 0 fee for bundle mode, swap_fee for unlocked mode
            let swap_fee = if self.is_bundle { 0 } else { self.fee_config.swap_fee };

            let (new_sqrt_price_x_96, amount_in, amount_out, fee_amount) =
                uniswap_v3_math::swap_math::compute_swap_step(
                    sqrt_price_x96,
                    target_sqrt_ratio,
                    liquidity,
                    amount_remaining,
                    swap_fee
                )?;

            sqrt_price_x96 = new_sqrt_price_x_96;
            if exact_input {
                // swap amount is positive so we sub
                amount_remaining = amount_remaining.saturating_sub(I256::from_raw(amount_in));
                amount_remaining = amount_remaining.saturating_sub(I256::from_raw(fee_amount));
            } else {
                // we add as is neg
                amount_remaining = amount_remaining.saturating_add(I256::from_raw(amount_out));
            }

            let (d_t0, d_t1) = if self.direction {
                // zero-for-one swap: token0 in, token1 out
                // fee is always on the input (token0) side
                ((amount_in + fee_amount).to(), amount_out.to())
            } else {
                // one-for-zero swap: token1 in, token0 out
                // fee is always on the input (token1) side
                (amount_out.to(), (amount_in + fee_amount).to())
            };

            self.liquidity.move_to_next_tick(
                sqrt_price_x96,
                self.direction,
                sqrt_price_x96 == sqrt_price_next_x96,
                sqrt_price_x96 != sqrt_price_start_x_96
            )?;

            steps.push(PoolSwapStep { end_tick: next_tick, init, liquidity, d_t0, d_t1 });
        }

        // the final sqrt price
        self.liquidity.set_sqrt_price(sqrt_price_x96);

        let (total_d_t0, total_d_t1) = steps.iter().fold((0u128, 0u128), |(mut t0, mut t1), x| {
            t0 += x.d_t0;
            t1 += x.d_t1;
            (t0, t1)
        });

        // Calculate protocol fee for unlocked mode
        let (protocol_fee_amount, protocol_fee_token) = if !self.is_bundle {
            // In unlocked mode, apply protocol fee to the target amount
            let target_amount = if exact_input != self.direction {
                I256::from_raw(U256::from(total_d_t0))
            } else {
                I256::from_raw(U256::from(total_d_t1))
            };

            // Take absolute value of target amount
            let p_target_amount =
                if target_amount < I256::ZERO { target_amount.neg() } else { target_amount };
            let p_target_amount_u256 = p_target_amount.into_raw();

            // false for token0, true for token1
            let protocol_fee_token = self.direction;

            let fee_rate_e6 = U256::from(self.fee_config.protocol_fee);
            let one_e6 = U256::from(1_000_000);

            let protocol_fee_amount_u256 = if exact_input {
                p_target_amount_u256 * fee_rate_e6 / one_e6
            } else {
                p_target_amount_u256 * one_e6 / (one_e6 - fee_rate_e6) - p_target_amount_u256
            };

            // Convert back to u128 for the protocol fee amount
            let protocol_fee_amount = protocol_fee_amount_u256.saturating_to::<u128>();

            (protocol_fee_amount, protocol_fee_token)
        } else {
            // In bundle mode, no protocol fee is applied during swap
            (0, false)
        };

        Ok(PoolSwapResult {
            fee_config: self.fee_config,
            start_price: range_start,
            start_tick: range_start_tick,
            end_price: self.liquidity.current_sqrt_price,
            end_tick: self.liquidity.current_tick,
            total_d_t0,
            total_d_t1,
            steps,
            end_liquidity: self.liquidity,
            protocol_fee_amount,
            protocol_fee_token,
            is_bundle: self.is_bundle
        })
    }
}

#[derive(Debug, Clone)]
pub struct PoolSwapResult<'a> {
    pub fee_config:          FeeConfiguration,
    pub start_price:         SqrtPriceX96,
    pub start_tick:          i32,
    pub end_price:           SqrtPriceX96,
    pub end_tick:            i32,
    pub total_d_t0:          u128,
    pub total_d_t1:          u128,
    pub steps:               Vec<PoolSwapStep>,
    pub end_liquidity:       LiquidityAtPoint<'a>,
    pub protocol_fee_amount: u128,
    pub protocol_fee_token:  bool, // false for token0, true for token1
    pub is_bundle:           bool
}

impl<'a> PoolSwapResult<'a> {
    /// initialize a swap from the end of this swap into a new swap.
    pub fn swap_to_amount(
        &'a self,
        amount: I256,
        direction: bool
    ) -> eyre::Result<PoolSwapResult<'a>> {
        PoolSwap {
            liquidity: self.end_liquidity.clone(),
            target_price: None,
            direction,
            target_amount: amount,
            fee_config: self.fee_config.clone(),
            is_bundle: self.is_bundle
        }
        .swap()
    }

    pub fn swap_to_price(&'a self, price_limit: SqrtPriceX96) -> eyre::Result<PoolSwapResult<'a>> {
        let direction = self.end_price >= price_limit;

        let price_swap = PoolSwap {
            liquidity: self.end_liquidity.clone(),
            target_price: Some(price_limit),
            direction,
            target_amount: I256::MAX,
            fee_config: self.fee_config.clone(),
            is_bundle: self.is_bundle
        }
        .swap()?;

        let amount_in = if direction { price_swap.total_d_t0 } else { price_swap.total_d_t1 };
        let amount = I256::unchecked_from(amount_in);

        self.swap_to_amount(amount, direction)
    }

    pub fn was_empty_swap(&self) -> bool {
        self.total_d_t0 == 0 || self.total_d_t1 == 0
    }

    /// Returns the amount of T0 exchanged over this swap with a sign attached,
    /// negative if performing this swap consumes T0 (T0 is the input quantity
    /// for the described swap) and positive if performing this swap provides T0
    /// (T0 is the output quantity for the described swap)
    pub fn t0_signed(&self) -> I256 {
        let val = I256::unchecked_from(self.total_d_t0);
        if self.zero_for_one() { val.neg() } else { val }
    }

    /// Returns the amount of T1 exchanged over this swap with a sign attached,
    /// negative if performing this swap consumes T1 (T1 is the input quantity
    /// for the described swap) and positive if performing this swap provides T1
    /// (T1 is the output quantity for the described swap)
    pub fn t1_signed(&self) -> I256 {
        let val = I256::unchecked_from(self.total_d_t1);
        if self.zero_for_one() { val } else { val.neg() }
    }

    /// Returns a boolean indicating whether this PoolPriceVec is
    /// `zero_for_one`.  This will be true if the AMM is buying T0 and the AMM
    /// price is decreasing, false if the AMM is selling T0 and the AMM price is
    /// increasing.
    pub fn zero_for_one(&self) -> bool {
        self.start_price > self.end_price
    }

    pub fn input(&self) -> u128 {
        if self.zero_for_one() { self.total_d_t0 } else { self.total_d_t1 }
    }

    pub fn output(&self) -> u128 {
        if self.zero_for_one() { self.total_d_t1 } else { self.total_d_t0 }
    }
}

/// the step of swapping across this pool
#[derive(Clone, Debug)]
pub struct PoolSwapStep {
    pub end_tick:  i32,
    pub init:      bool,
    pub liquidity: u128,
    pub d_t0:      u128,
    pub d_t1:      u128
}

impl PoolSwapStep {
    pub fn avg_price(&self) -> Option<Ray> {
        if self.empty() {
            None
        } else {
            Some(Ray::calc_price(U256::from(self.d_t0), U256::from(self.d_t1)))
        }
    }

    pub fn empty(&self) -> bool {
        self.d_t0 == 0 || self.d_t1 == 0
    }
}
