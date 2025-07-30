//! Handles the management of baseline liquidity fetching. This will
//! be the base, in which all other V4 related actions are built ontop of.

use std::collections::HashMap;

use alloy::primitives::{I256, U256};
use itertools::Itertools;
use malachite::num::conversion::traits::SaturatingInto;
use serde::{Deserialize, Serialize};
use uniswap_v3_math::{
    tick_bitmap::{flip_tick, next_initialized_tick_within_one_word},
    tick_math::{MAX_TICK, MIN_TICK, get_tick_at_sqrt_ratio}
};

use super::tick_info::TickInfo;
use crate::sqrt_pricex96::SqrtPriceX96;

/// baseline holder for
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BaselineLiquidity {
    pub(super) tick_spacing:     i32,
    pub(super) start_tick:       i32,
    pub(super) start_sqrt_price: SqrtPriceX96,
    pub(super) start_liquidity:  u128,
    /// should only have ticks that are initialized.
    initialized_ticks:           HashMap<i32, TickInfo>,
    /// should only have ticks that are initialized, i.e have liquidity
    tick_bitmap:                 HashMap<i16, U256>
}

impl BaselineLiquidity {
    pub fn new(
        tick_spacing: i32,
        start_tick: i32,
        start_sqrt_price: SqrtPriceX96,
        start_liquidity: u128,
        initialized_ticks: HashMap<i32, TickInfo>,
        tick_bitmap: HashMap<i16, U256>
    ) -> Self {
        Self {
            start_tick,
            start_sqrt_price,
            start_liquidity,
            initialized_ticks,
            tick_bitmap,
            tick_spacing
        }
    }

    pub fn update_liquidity_from_event(
        &mut self,
        tick_lower: i32,
        tick_upper: i32,
        liquidity_delta: I256
    ) {
        let min_tick_init = self
            .initialized_ticks
            .keys()
            .min()
            .copied()
            .expect("No min-initialized tick. This will break the update event algo.");

        let max_tick_init = self
            .initialized_ticks
            .keys()
            .max()
            .copied()
            .expect("No max-initialized tick. This will break the update event algo");

        // should never overflow
        let liquidity_delta = i128::try_from(liquidity_delta).unwrap();

        // Case where we surround the current position (greater than the position on
        // both sides);
        if tick_lower < min_tick_init && tick_upper > max_tick_init {
            // we don't flip any ticks here as there outside of our loaded band
            if liquidity_delta.is_negative() {
                self.start_liquidity -= liquidity_delta.unsigned_abs();
            } else {
                self.start_liquidity += liquidity_delta as u128;
            }

            return
        }

        // Case were we are fully inside the range
        if tick_lower >= min_tick_init && tick_upper <= max_tick_init {
            // handle lower
            let tick_info_lower = self.initialized_ticks.entry(tick_lower).or_default();
            let start_am_lower = tick_info_lower.liquidity_net;
            tick_info_lower.liquidity_net += liquidity_delta;
            let end_am_lower = tick_info_lower.liquidity_net;
            if end_am_lower == 0 {
                self.initialized_ticks.remove(&tick_lower);
            }
            if start_am_lower == 0 || end_am_lower == 0 {
                flip_tick(&mut self.tick_bitmap, tick_lower, self.tick_spacing).unwrap();
            }

            // handle upper
            let tick_info_upper = self.initialized_ticks.entry(tick_upper).or_default();
            let start_am_upper = tick_info_upper.liquidity_net;
            tick_info_upper.liquidity_net -= liquidity_delta;
            let end_am_upper = tick_info_upper.liquidity_net;
            if end_am_upper == 0 {
                self.initialized_ticks.remove(&tick_upper);
            }
            if start_am_upper == 0 || end_am_upper == 0 {
                flip_tick(&mut self.tick_bitmap, tick_upper, self.tick_spacing).unwrap();
            }

            if self.start_tick > tick_lower && self.start_tick < tick_upper {
                if liquidity_delta.is_negative() {
                    self.start_liquidity -= liquidity_delta.unsigned_abs();
                } else {
                    self.start_liquidity += liquidity_delta as u128;
                }
            }

            return
        }

        // lower tick is in range, upper out of range
        if tick_lower >= min_tick_init && tick_upper > max_tick_init {
            // we are fully out of range here.
            if tick_lower > max_tick_init {
                return;
            }

            // update the tick
            let tick_info = self.initialized_ticks.entry(tick_lower).or_default();

            let start_am = tick_info.liquidity_net;

            // Bc we are lower, we add the liq.
            tick_info.liquidity_net += liquidity_delta;

            // if we have no more liq here, we remove from the tick map
            let end_am = tick_info.liquidity_net;
            if end_am == 0 {
                self.initialized_ticks.remove(&tick_lower);
            }

            // if we started un-init or we became un-init, we need to flip the tick
            if start_am == 0 || end_am == 0 {
                flip_tick(&mut self.tick_bitmap, tick_lower, self.tick_spacing).unwrap();
            }

            // if we are less than start tick, means that we effect slot0 and need to add.
            if tick_lower <= self.start_tick {
                if liquidity_delta.is_negative() {
                    self.start_liquidity -= liquidity_delta.unsigned_abs();
                } else {
                    self.start_liquidity += liquidity_delta as u128;
                }
            }
            return
        }

        // upper tick in range,
        if tick_lower < min_tick_init && tick_upper <= max_tick_init {
            // we are fully out of range here.
            if tick_upper < min_tick_init {
                return;
            }

            // update the tick
            let tick_info = self.initialized_ticks.entry(tick_upper).or_default();

            let start_am = tick_info.liquidity_net;

            // Bc we are upper, we sub the liq.
            tick_info.liquidity_net -= liquidity_delta;

            // if we have no more liq here, we remove from the tick map
            let end_am = tick_info.liquidity_net;
            if end_am == 0 {
                self.initialized_ticks.remove(&tick_upper);
            }

            // if we started un-init or we became un-init, we need to flip the tick
            if start_am == 0 || end_am == 0 {
                flip_tick(&mut self.tick_bitmap, tick_upper, self.tick_spacing).unwrap();
            }

            // if our upper tick is ge the start tick, and lower is out of range, means that
            // the current liq needs to be updated.
            if tick_upper >= self.start_tick {
                if liquidity_delta.is_negative() {
                    self.start_liquidity -= liquidity_delta.unsigned_abs();
                } else {
                    self.start_liquidity += liquidity_delta as u128;
                }
            }
        }
    }

    /// returns a liquidity ref were the current liquidity is properly
    /// calculated based on were the sqrt_price is at
    pub fn at_sqrt_price(&self, price: SqrtPriceX96) -> eyre::Result<LiquidityAtPoint<'_>> {
        // we are zero for one if the price is going down.
        let zfo = self.start_sqrt_price >= price;
        let tick_at_price = get_tick_at_sqrt_ratio(price.into())?;

        // now that we have the direction, what we need to do is calculate what the
        // current liquidity will be.
        let current_liquidity: i128 = self.start_liquidity.saturating_into();
        // if we are going down
        // let current_tick = self.start_tick;
        let liquidity = if zfo {
            // we want to sort high to low, so that as iterator is consumed, we are going
            // down
            self.initialized_ticks
                .iter()
                .filter(|(t, _)| *t < &self.start_tick)
                // we want high to low.
                .sorted_by_key(|(k, _)| -**k)
                .fold(current_liquidity, |mut acc, (_, info)| {
                    if info.initialized {
                        acc += -info.liquidity_net;
                    }
                    acc
                })
        } else {
            self.initialized_ticks
                .iter()
                .filter(|(t, _)| *t > &self.start_tick)
                // we want low to high
                .sorted_by_key(|(k, _)| *k)
                .fold(current_liquidity, |mut acc, (_, info)| {
                    if info.initialized {
                        acc += info.liquidity_net;
                    }
                    acc
                })
        };

        // If there is no liquidity ranges. We don't have to worry
        // about undefined behaviour.
        let min_tick_init = self
            .initialized_ticks
            .keys()
            .min()
            .copied()
            .unwrap_or(MIN_TICK);
        let max_tick_init = self
            .initialized_ticks
            .keys()
            .max()
            .copied()
            .unwrap_or(MAX_TICK);

        Ok(LiquidityAtPoint {
            tick_spacing: self.tick_spacing,
            current_tick: tick_at_price,
            current_liquidity: liquidity.unsigned_abs(),
            current_sqrt_price: price,
            initialized_ticks: &self.initialized_ticks,
            min_tick_init,
            max_tick_init,
            tick_bitmap: &self.tick_bitmap
        })
    }

    pub fn current(&self) -> LiquidityAtPoint<'_> {
        // If there is no liquidity ranges. We don't have to worry
        // about undefined behaviour.
        let min_tick_init = self
            .initialized_ticks
            .keys()
            .min()
            .copied()
            .unwrap_or(MIN_TICK);
        let max_tick_init = self
            .initialized_ticks
            .keys()
            .max()
            .copied()
            .unwrap_or(MAX_TICK);

        LiquidityAtPoint {
            tick_spacing: self.tick_spacing,
            current_tick: self.start_tick,
            current_sqrt_price: self.start_sqrt_price,
            current_liquidity: self.start_liquidity,
            initialized_ticks: &self.initialized_ticks,
            min_tick_init,
            max_tick_init,
            tick_bitmap: &self.tick_bitmap
        }
    }

    /// Get mutable reference to initialized ticks
    pub fn initialized_ticks_mut(&mut self) -> &mut HashMap<i32, TickInfo> {
        &mut self.initialized_ticks
    }

    /// Get immutable reference to initialized ticks
    pub fn initialized_ticks(&self) -> &HashMap<i32, TickInfo> {
        &self.initialized_ticks
    }

    /// Get immutable reference to tick bitmap
    pub fn tick_bitmap(&self) -> &HashMap<i16, U256> {
        &self.tick_bitmap
    }

    /// Update tick bitmap
    pub fn update_tick_bitmap(&mut self, word_pos: i16, word: U256) {
        self.tick_bitmap.insert(word_pos, word);
    }

    /// Get the minimum initialized tick
    pub fn get_min_initialized_tick(&self) -> Option<i32> {
        self.initialized_ticks.keys().min().copied()
    }

    /// Get the maximum initialized tick
    pub fn get_max_initialized_tick(&self) -> Option<i32> {
        self.initialized_ticks.keys().max().copied()
    }

    /// Get the current tick
    pub fn get_current_tick(&self) -> i32 {
        self.start_tick
    }

    /// Get the tick spacing
    pub fn get_tick_spacing(&self) -> i32 {
        self.tick_spacing
    }
}

/// represents the liquidity at a specified point. All operations use this
/// object.

#[derive(Clone, Debug)]
pub struct LiquidityAtPoint<'a> {
    pub(super) tick_spacing:       i32,
    pub(super) current_tick:       i32,
    pub(super) current_sqrt_price: SqrtPriceX96,
    pub(super) current_liquidity:  u128,
    pub(super) max_tick_init:      i32,
    pub(super) min_tick_init:      i32,
    initialized_ticks:             &'a HashMap<i32, TickInfo>,
    tick_bitmap:                   &'a HashMap<i16, U256>
}

impl LiquidityAtPoint<'_> {
    pub fn min_sqrt_price(&self) -> SqrtPriceX96 {
        SqrtPriceX96::at_tick(self.min_tick_init + 1).unwrap()
    }

    pub fn max_sqrt_price(&self) -> SqrtPriceX96 {
        SqrtPriceX96::at_tick(self.max_tick_init - 1).unwrap()
    }

    /// moves to the next tick initialized within one word returning
    /// the tick and the liquidity to swap with
    pub fn get_to_next_initialized_tick_within_one_word(
        &self,
        direction: bool
    ) -> eyre::Result<(i32, u128, bool)> {
        let (tick_next, init) = next_initialized_tick_within_one_word(
            self.tick_bitmap,
            self.current_tick,
            self.tick_spacing,
            direction
        )?;

        if tick_next > self.max_tick_init || tick_next < self.min_tick_init {
            return Err(eyre::eyre!("out of initialized tick ranges loaded for uniswap"));
        }

        // adjust self view
        Ok((tick_next, self.current_liquidity, init))
    }

    pub fn move_to_next_tick(
        &mut self,
        sqrt_price: U256,
        direction: bool,
        full_range: bool,
        sqrt_moved: bool
    ) -> eyre::Result<()> {
        let (tick_next, init) = next_initialized_tick_within_one_word(
            self.tick_bitmap,
            self.current_tick,
            self.tick_spacing,
            direction
        )?;

        if full_range {
            if init {
                let liq_net = self
                    .initialized_ticks
                    .get(&tick_next)
                    .map(|info| if direction { -info.liquidity_net } else { info.liquidity_net })
                    .unwrap_or_default();

                self.current_liquidity = if liq_net < 0 {
                    self.current_liquidity
                        .checked_sub(liq_net.unsigned_abs())
                        .ok_or_else(|| {
                            eyre::eyre!("underflow on liquidity. Shouldn't be possible")
                        })?
                } else {
                    self.current_liquidity + (liq_net.unsigned_abs())
                };
            }
            if direction {
                self.current_tick = tick_next - 1
            } else {
                self.current_tick = tick_next
            };
        } else if sqrt_moved {
            self.current_tick = get_tick_at_sqrt_ratio(sqrt_price)?;
        }

        Ok(())
    }

    pub fn set_sqrt_price(&mut self, sqrt_price: U256) {
        self.current_sqrt_price = sqrt_price.into();
    }
}
