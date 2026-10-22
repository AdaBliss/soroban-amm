use crate::error::{Result, SimulationError};
use serde::{Deserialize, Serialize};

const BPS_DENOMINATOR: i128 = 10_000;
const PRICE_SCALE: i128 = 1_000_000;

/// Minimum liquidity permanently locked on the first successful deposit.
///
/// This mirrors `contracts/amm::MINIMUM_LIQUIDITY` and prevents dust-pool
/// draining attacks by ensuring the first liquidity provider cannot withdraw
/// the entire pool's initial share supply.
pub const MINIMUM_LIQUIDITY: i128 = 1_000;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct PoolState {
    pub token_a: String,
    pub token_b: String,
    pub reserve_a: i128,
    pub reserve_b: i128,
    pub total_shares: i128,
    pub fee_bps: i128,
    #[serde(default)]
    pub protocol_fee_bps: i128,
    /// Percentage of the protocol fee returned to LP reserves.
    ///
    /// This mirrors the AMM contract's `LpRebateBps` storage value.
    #[serde(default)]
    pub lp_rebate_bps: i128,
    #[serde(default)]
    pub accrued_fee_a: i128,
    #[serde(default)]
    pub accrued_fee_b: i128,
    #[serde(default)]
    pub price_cumulative_a: i128,
    #[serde(default)]
    pub price_cumulative_b: i128,
    #[serde(default)]
    pub last_timestamp: u64,
    #[serde(default)]
    pub paused: bool,
    /// Indicates that the first-deposit minimum liquidity has been locked.
    ///
    /// This mirrors the contract's `DataKey::MinLiquidityLocked`.
    #[serde(default)]
    pub min_liquidity_locked: bool,
    /// Pool-level accounting for the permanently locked minimum liquidity.
    ///
    /// The simulator intentionally has no per-provider share ledger, so the
    /// locked shares are tracked as pool-level state for reporting purposes.
    #[serde(default)]
    pub locked_liquidity: i128,
}

impl PoolState {
    pub fn new(
        token_a: impl Into<String>,
        token_b: impl Into<String>,
        fee_bps: i128,
    ) -> Result<Self> {
        let pool = Self {
            token_a: token_a.into(),
            token_b: token_b.into(),
            reserve_a: 0,
            reserve_b: 0,
            total_shares: 0,
            fee_bps,
            protocol_fee_bps: 0,
            lp_rebate_bps: 0,
            accrued_fee_a: 0,
            accrued_fee_b: 0,
            price_cumulative_a: 0,
            price_cumulative_b: 0,
            last_timestamp: 0,
            paused: false,
            min_liquidity_locked: false,
            locked_liquidity: 0,
        };
        pool.validate()?;
        Ok(pool)
    }

    pub fn validate(&self) -> Result<()> {
        if self.token_a == self.token_b {
            return Err(SimulationError::InvalidToken {
                token: self.token_a.clone(),
            });
        }

        if !(0..=BPS_DENOMINATOR).contains(&self.fee_bps) {
            return Err(SimulationError::InvalidFeeBps {
                fee_bps: self.fee_bps,
            });
        }

        if !(0..=self.fee_bps).contains(&self.protocol_fee_bps) {
            return Err(SimulationError::InvalidFeeBps {
                fee_bps: self.protocol_fee_bps,
            });
        }

        if !(0..=BPS_DENOMINATOR).contains(&self.lp_rebate_bps) {
            return Err(SimulationError::InvalidFeeBps {
                fee_bps: self.lp_rebate_bps,
            });
        }

        if self.total_shares < 0 {
            return Err(SimulationError::InvalidInput(
                "total_shares cannot be negative".into(),
            ));
        }

        if self.locked_liquidity < 0 {
            return Err(SimulationError::InvalidInput(
                "locked_liquidity cannot be negative".into(),
            ));
        }

        if self.min_liquidity_locked {
            if self.locked_liquidity != MINIMUM_LIQUIDITY {
                return Err(SimulationError::InvalidInput(format!(
                    "locked_liquidity must equal MINIMUM_LIQUIDITY ({MINIMUM_LIQUIDITY}) \
                     when min_liquidity_locked is true"
                )));
            }

            if self.total_shares < MINIMUM_LIQUIDITY {
                return Err(SimulationError::InvalidInput(
                    "total_shares cannot be below MINIMUM_LIQUIDITY when the lock is active".into(),
                ));
            }
        } else {
            if self.locked_liquidity != 0 {
                return Err(SimulationError::InvalidInput(
                    "locked_liquidity must be zero when min_liquidity_locked is false".into(),
                ));
            }

            // Existing fixtures are deserializable because the new fields use
            // serde defaults. However, a populated pool with no lock marker is
            // inconsistent with the contract's first-deposit semantics.
            //
            // We deliberately reject this state rather than silently guessing
            // whether 1,000 shares were already locked.
            if self.total_shares > MINIMUM_LIQUIDITY {
                return Err(SimulationError::InvalidInput(
                    "pool with total_shares above MINIMUM_LIQUIDITY must have \
                     min_liquidity_locked set"
                        .into(),
                ));
            }
        }

        Ok(())
    }

    pub fn is_empty(&self) -> bool {
        self.reserve_a <= 0 || self.reserve_b <= 0
    }

    pub fn spot_price_a(&self) -> i128 {
        if self.reserve_a <= 0 || self.reserve_b <= 0 {
            0
        } else {
            self.reserve_b * PRICE_SCALE / self.reserve_a
        }
    }

    pub fn spot_price_b(&self) -> i128 {
        if self.reserve_a <= 0 || self.reserve_b <= 0 {
            0
        } else {
            self.reserve_a * PRICE_SCALE / self.reserve_b
        }
    }

    pub fn advance_to(&mut self, timestamp: u64) -> Result<()> {
        if timestamp < self.last_timestamp {
            return Err(SimulationError::InvalidInput(format!(
                "timestamps must be non-decreasing (got {timestamp}, last {})",
                self.last_timestamp
            )));
        }

        if timestamp == self.last_timestamp {
            return Ok(());
        }

        let delta = timestamp - self.last_timestamp;

        if !self.is_empty() {
            let spot_a = self.spot_price_a();
            let spot_b = self.spot_price_b();
            let delta_i128 = i128::from(delta);

            self.price_cumulative_a = self
                .price_cumulative_a
                .checked_add(
                    spot_a
                        .checked_mul(delta_i128)
                        .ok_or(SimulationError::Overflow)?,
                )
                .ok_or(SimulationError::Overflow)?;

            self.price_cumulative_b = self
                .price_cumulative_b
                .checked_add(
                    spot_b
                        .checked_mul(delta_i128)
                        .ok_or(SimulationError::Overflow)?,
                )
                .ok_or(SimulationError::Overflow)?;
        }

        self.last_timestamp = timestamp;
        Ok(())
    }

    pub fn quote_swap_exact_in(&self, token_in: &str, amount_in: i128) -> Result<SwapQuote> {
        if amount_in <= 0 {
            return Err(SimulationError::ZeroAmount);
        }

        let (reserve_in, reserve_out, token_out) = self.token_pair(token_in)?;

        if reserve_in <= 0 || reserve_out <= 0 {
            return Err(SimulationError::EmptyPool);
        }

        let amount_in_with_fee = amount_in
            .checked_mul(BPS_DENOMINATOR - self.fee_bps)
            .ok_or(SimulationError::Overflow)?;

        let numerator = amount_in_with_fee
            .checked_mul(reserve_out)
            .ok_or(SimulationError::Overflow)?;

        let denominator = reserve_in
            .checked_mul(BPS_DENOMINATOR)
            .ok_or(SimulationError::Overflow)?
            .checked_add(amount_in_with_fee)
            .ok_or(SimulationError::Overflow)?;

        let amount_out = numerator / denominator;

        let fee_amount = amount_in
            .checked_mul(self.fee_bps)
            .ok_or(SimulationError::Overflow)?
            / BPS_DENOMINATOR;

        let spot_price = reserve_out * PRICE_SCALE / reserve_in;
        let effective_price = amount_out * PRICE_SCALE / amount_in;

        let price_impact_bps = if spot_price > 0 {
            ((spot_price - effective_price) * BPS_DENOMINATOR / spot_price).max(0)
        } else {
            0
        };

        Ok(SwapQuote {
            token_in: token_in.to_string(),
            token_out,
            amount_in,
            amount_out,
            fee_amount,
            spot_price,
            effective_price,
            price_impact_bps,
            valid: amount_out > 0 && amount_out < reserve_out,
        })
    }

    pub fn quote_swap_exact_out(&self, token_out: &str, amount_out: i128) -> Result<SwapResult> {
        if amount_out <= 0 {
            return Err(SimulationError::ZeroAmount);
        }

        if self.fee_bps == BPS_DENOMINATOR {
            return Err(SimulationError::InvalidInput(
                "exact-out swaps are impossible at a 100% fee".into(),
            ));
        }

        let (reserve_in, reserve_out, token_in) = self.reverse_pair(token_out)?;

        if reserve_in <= 0 || reserve_out <= 0 {
            return Err(SimulationError::EmptyPool);
        }

        if amount_out >= reserve_out {
            return Err(SimulationError::SlippageExceeded);
        }

        let numerator = reserve_in
            .checked_mul(amount_out)
            .ok_or(SimulationError::Overflow)?
            .checked_mul(BPS_DENOMINATOR)
            .ok_or(SimulationError::Overflow)?;

        let denominator = reserve_out
            .checked_sub(amount_out)
            .ok_or(SimulationError::Overflow)?
            .checked_mul(BPS_DENOMINATOR - self.fee_bps)
            .ok_or(SimulationError::Overflow)?;

        let required_in = numerator / denominator + 1;

        let fee_amount = required_in
            .checked_mul(self.fee_bps)
            .ok_or(SimulationError::Overflow)?
            / BPS_DENOMINATOR;

        Ok(SwapResult {
            token_in,
            token_out: token_out.to_string(),
            amount_in: required_in,
            amount_out,
            fee_amount,
        })
    }

    pub fn quote_add_liquidity(&self, amount_a: i128, amount_b: i128) -> Result<LiquidityQuote> {
        if amount_a <= 0 || amount_b <= 0 {
            return Err(SimulationError::ZeroAmount);
        }

        if self.total_shares > 0 && (self.reserve_a <= 0 || self.reserve_b <= 0) {
            return Err(SimulationError::EmptyPool);
        }

        let raw_shares = if self.total_shares == 0 {
            isqrt(
                amount_a
                    .checked_mul(amount_b)
                    .ok_or(SimulationError::Overflow)?,
            )
        } else {
            let shares_a = amount_a
                .checked_mul(self.total_shares)
                .ok_or(SimulationError::Overflow)?
                / self.reserve_a;

            let shares_b = amount_b
                .checked_mul(self.total_shares)
                .ok_or(SimulationError::Overflow)?
                / self.reserve_b;

            shares_a.min(shares_b)
        };

        let shares = if self.total_shares == 0 && !self.min_liquidity_locked {
            if raw_shares <= MINIMUM_LIQUIDITY {
                return Err(SimulationError::InsufficientShares);
            }

            raw_shares
                .checked_sub(MINIMUM_LIQUIDITY)
                .ok_or(SimulationError::Overflow)?
        } else {
            raw_shares
        };

        Ok(LiquidityQuote {
            amount_a,
            amount_b,
            shares,
            pool_ratio: if self.reserve_a > 0 {
                self.reserve_b * PRICE_SCALE / self.reserve_a
            } else {
                0
            },
        })
    }

    pub fn quote_remove_liquidity(&self, shares: i128) -> Result<LiquidityQuote> {
        if shares <= 0 {
            return Err(SimulationError::ZeroAmount);
        }

        if self.total_shares <= 0 {
            return Err(SimulationError::EmptyPool);
        }

        // The permanently locked minimum liquidity is included in
        // total_shares but is not provider-owned and therefore cannot be
        // withdrawn through the simulator's provider-facing share amount.
        let withdrawable_shares = self
            .total_shares
            .checked_sub(self.locked_liquidity)
            .ok_or(SimulationError::Overflow)?;

        if shares > withdrawable_shares {
            return Err(SimulationError::SlippageExceeded);
        }

        Ok(LiquidityQuote {
            amount_a: shares
                .checked_mul(self.reserve_a)
                .ok_or(SimulationError::Overflow)?
                / self.total_shares,
            amount_b: shares
                .checked_mul(self.reserve_b)
                .ok_or(SimulationError::Overflow)?
                / self.total_shares,
            shares,
            pool_ratio: if self.reserve_a > 0 {
                self.reserve_b * PRICE_SCALE / self.reserve_a
            } else {
                0
            },
        })
    }

    pub fn execute_swap_exact_in(
        &mut self,
        token_in: &str,
        amount_in: i128,
        min_out: i128,
    ) -> Result<SwapQuote> {
        if self.paused {
            return Err(SimulationError::Paused);
        }

        let quote = self.quote_swap_exact_in(token_in, amount_in)?;

        if quote.amount_out < min_out {
            return Err(SimulationError::SlippageExceeded);
        }

        self.apply_checkpoint()?;

        let protocol_fee = self.protocol_fee(amount_in)?;
        let lp_rebate = self.lp_rebate(protocol_fee)?;
        let net_protocol_fee = protocol_fee
            .checked_sub(lp_rebate)
            .ok_or(SimulationError::Overflow)?;

        if token_in == self.token_a {
            // Only the net protocol fee leaves the LP reserves.
            // The rebate remains in the reserve for LPs.
            self.reserve_a = self
                .reserve_a
                .checked_add(amount_in)
                .ok_or(SimulationError::Overflow)?
                .checked_sub(net_protocol_fee)
                .ok_or(SimulationError::Overflow)?;

            self.reserve_b = self
                .reserve_b
                .checked_sub(quote.amount_out)
                .ok_or(SimulationError::Overflow)?;

            self.accrued_fee_a = self
                .accrued_fee_a
                .checked_add(net_protocol_fee)
                .ok_or(SimulationError::Overflow)?;
        } else {
            self.reserve_b = self
                .reserve_b
                .checked_add(amount_in)
                .ok_or(SimulationError::Overflow)?
                .checked_sub(net_protocol_fee)
                .ok_or(SimulationError::Overflow)?;

            self.reserve_a = self
                .reserve_a
                .checked_sub(quote.amount_out)
                .ok_or(SimulationError::Overflow)?;

            self.accrued_fee_b = self
                .accrued_fee_b
                .checked_add(net_protocol_fee)
                .ok_or(SimulationError::Overflow)?;
        }

        Ok(quote)
    }

    pub fn execute_swap_exact_out(
        &mut self,
        token_out: &str,
        amount_out: i128,
        max_in: i128,
    ) -> Result<SwapResult> {
        if self.paused {
            return Err(SimulationError::Paused);
        }

        let quote = self.quote_swap_exact_out(token_out, amount_out)?;

        if quote.amount_in > max_in {
            return Err(SimulationError::SlippageExceeded);
        }

        self.apply_checkpoint()?;

        let protocol_fee = self.protocol_fee(quote.amount_in)?;
        let lp_rebate = self.lp_rebate(protocol_fee)?;
        let net_protocol_fee = protocol_fee
            .checked_sub(lp_rebate)
            .ok_or(SimulationError::Overflow)?;

        if token_out == self.token_a {
            self.reserve_b = self
                .reserve_b
                .checked_add(quote.amount_in)
                .ok_or(SimulationError::Overflow)?
                .checked_sub(net_protocol_fee)
                .ok_or(SimulationError::Overflow)?;

            self.reserve_a = self
                .reserve_a
                .checked_sub(amount_out)
                .ok_or(SimulationError::Overflow)?;

            self.accrued_fee_b = self
                .accrued_fee_b
                .checked_add(net_protocol_fee)
                .ok_or(SimulationError::Overflow)?;
        } else {
            self.reserve_a = self
                .reserve_a
                .checked_add(quote.amount_in)
                .ok_or(SimulationError::Overflow)?
                .checked_sub(net_protocol_fee)
                .ok_or(SimulationError::Overflow)?;

            self.reserve_b = self
                .reserve_b
                .checked_sub(amount_out)
                .ok_or(SimulationError::Overflow)?;

            self.accrued_fee_a = self
                .accrued_fee_a
                .checked_add(net_protocol_fee)
                .ok_or(SimulationError::Overflow)?;
        }

        Ok(quote)
    }

    pub fn execute_add_liquidity(
        &mut self,
        amount_a: i128,
        amount_b: i128,
        min_shares: i128,
    ) -> Result<LiquidityQuote> {
        if self.paused {
            return Err(SimulationError::Paused);
        }

        let first_deposit = self.total_shares == 0 && !self.min_liquidity_locked;
        let quote = self.quote_add_liquidity(amount_a, amount_b)?;

        if quote.shares < min_shares {
            return Err(SimulationError::SlippageExceeded);
        }

        self.apply_checkpoint()?;

        self.reserve_a = self
            .reserve_a
            .checked_add(amount_a)
            .ok_or(SimulationError::Overflow)?;

        self.reserve_b = self
            .reserve_b
            .checked_add(amount_b)
            .ok_or(SimulationError::Overflow)?;

        if first_deposit {
            // The contract mints `raw_shares - MINIMUM_LIQUIDITY` to the
            // provider and permanently locks MINIMUM_LIQUIDITY.
            self.total_shares = quote
                .shares
                .checked_add(MINIMUM_LIQUIDITY)
                .ok_or(SimulationError::Overflow)?;

            self.locked_liquidity = MINIMUM_LIQUIDITY;
            self.min_liquidity_locked = true;
        } else {
            self.total_shares = self
                .total_shares
                .checked_add(quote.shares)
                .ok_or(SimulationError::Overflow)?;
        }

        Ok(quote)
    }

    pub fn execute_remove_liquidity(
        &mut self,
        shares: i128,
        min_a: i128,
        min_b: i128,
    ) -> Result<LiquidityQuote> {
        if self.paused {
            return Err(SimulationError::Paused);
        }

        let quote = self.quote_remove_liquidity(shares)?;

        if quote.amount_a < min_a || quote.amount_b < min_b {
            return Err(SimulationError::SlippageExceeded);
        }

        self.apply_checkpoint()?;

        self.reserve_a = self
            .reserve_a
            .checked_sub(quote.amount_a)
            .ok_or(SimulationError::Overflow)?;

        self.reserve_b = self
            .reserve_b
            .checked_sub(quote.amount_b)
            .ok_or(SimulationError::Overflow)?;

        self.total_shares = self
            .total_shares
            .checked_sub(shares)
            .ok_or(SimulationError::Overflow)?;

        Ok(quote)
    }

    /// Redeem LP shares into one token, mirroring the V2 contract's internal
    /// swap of the other proportional withdrawal amount.
    pub fn execute_remove_liquidity_one_sided(
        &mut self,
        shares: i128,
        token_out: &str,
        min_out: i128,
    ) -> Result<OneSidedLiquidityQuote> {
        if self.paused {
            return Err(SimulationError::Paused);
        }
        if shares <= 0 {
            return Err(SimulationError::ZeroAmount);
        }
        if token_out != self.token_a && token_out != self.token_b {
            return Err(SimulationError::InvalidToken {
                token: token_out.to_string(),
            });
        }
        let withdrawal = self.quote_remove_liquidity(shares)?;
        let (amount_keep, amount_swap) = if token_out == self.token_a {
            (withdrawal.amount_a, withdrawal.amount_b)
        } else {
            (withdrawal.amount_b, withdrawal.amount_a)
        };
        let reserve_a = self
            .reserve_a
            .checked_sub(withdrawal.amount_a)
            .ok_or(SimulationError::Overflow)?;
        let reserve_b = self
            .reserve_b
            .checked_sub(withdrawal.amount_b)
            .ok_or(SimulationError::Overflow)?;
        let amount_after_fee = amount_swap
            .checked_mul(BPS_DENOMINATOR - self.fee_bps)
            .ok_or(SimulationError::Overflow)?;
        let (reserve_in, reserve_out) = if token_out == self.token_a {
            (reserve_b, reserve_a)
        } else {
            (reserve_a, reserve_b)
        };
        if reserve_in <= 0 || reserve_out <= 0 {
            return Err(SimulationError::EmptyPool);
        }
        let swap_output = amount_after_fee
            .checked_mul(reserve_out)
            .ok_or(SimulationError::Overflow)?
            / reserve_in
                .checked_mul(BPS_DENOMINATOR)
                .ok_or(SimulationError::Overflow)?
                .checked_add(amount_after_fee)
                .ok_or(SimulationError::Overflow)?;
        let protocol_fee = self.protocol_fee(amount_swap)?;
        let rebate = self.lp_rebate(protocol_fee)?;
        let net_protocol_fee = protocol_fee
            .checked_sub(rebate)
            .ok_or(SimulationError::Overflow)?;
        let total_out = amount_keep
            .checked_add(swap_output)
            .ok_or(SimulationError::Overflow)?;
        if total_out < min_out {
            return Err(SimulationError::SlippageExceeded);
        }
        self.apply_checkpoint()?;
        if token_out == self.token_a {
            self.reserve_a = reserve_a
                .checked_sub(swap_output)
                .ok_or(SimulationError::Overflow)?;
            self.reserve_b = reserve_b
                .checked_add(amount_swap)
                .and_then(|v| v.checked_sub(net_protocol_fee))
                .ok_or(SimulationError::Overflow)?;
            self.accrued_fee_b = self
                .accrued_fee_b
                .checked_add(net_protocol_fee)
                .ok_or(SimulationError::Overflow)?;
        } else {
            self.reserve_a = reserve_a
                .checked_add(amount_swap)
                .and_then(|v| v.checked_sub(net_protocol_fee))
                .ok_or(SimulationError::Overflow)?;
            self.reserve_b = reserve_b
                .checked_sub(swap_output)
                .ok_or(SimulationError::Overflow)?;
            self.accrued_fee_a = self
                .accrued_fee_a
                .checked_add(net_protocol_fee)
                .ok_or(SimulationError::Overflow)?;
        }
        self.total_shares = self
            .total_shares
            .checked_sub(shares)
            .ok_or(SimulationError::Overflow)?;
        Ok(OneSidedLiquidityQuote {
            token_out: token_out.to_string(),
            shares,
            amount_keep,
            amount_swapped: amount_swap,
            swap_output,
            total_out,
            protocol_fee,
            net_protocol_fee,
        })
    }

    fn apply_checkpoint(&mut self) -> Result<()> {
        // The simulator keeps the TWAP accumulator consistent with the contract
        // by checkpointing at the current timestamp before every state change.
        self.advance_to(self.last_timestamp)
    }

    fn protocol_fee(&self, amount: i128) -> Result<i128> {
        if self.protocol_fee_bps <= 0 {
            return Ok(0);
        }

        amount
            .checked_mul(self.protocol_fee_bps)
            .ok_or(SimulationError::Overflow)
            .map(|value| value / BPS_DENOMINATOR)
    }

    fn lp_rebate(&self, protocol_fee: i128) -> Result<i128> {
        if protocol_fee <= 0 || self.lp_rebate_bps <= 0 {
            return Ok(0);
        }

        protocol_fee
            .checked_mul(self.lp_rebate_bps)
            .ok_or(SimulationError::Overflow)
            .map(|value| value / BPS_DENOMINATOR)
    }

    fn token_pair(&self, token_in: &str) -> Result<(i128, i128, String)> {
        if token_in == self.token_a {
            Ok((self.reserve_a, self.reserve_b, self.token_b.clone()))
        } else if token_in == self.token_b {
            Ok((self.reserve_b, self.reserve_a, self.token_a.clone()))
        } else {
            Err(SimulationError::InvalidToken {
                token: token_in.to_string(),
            })
        }
    }

    fn reverse_pair(&self, token_out: &str) -> Result<(i128, i128, String)> {
        if token_out == self.token_a {
            Ok((self.reserve_b, self.reserve_a, self.token_b.clone()))
        } else if token_out == self.token_b {
            Ok((self.reserve_a, self.reserve_b, self.token_a.clone()))
        } else {
            Err(SimulationError::InvalidToken {
                token: token_out.to_string(),
            })
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct SwapQuote {
    pub token_in: String,
    pub token_out: String,
    pub amount_in: i128,
    pub amount_out: i128,
    pub fee_amount: i128,
    pub spot_price: i128,
    pub effective_price: i128,
    pub price_impact_bps: i128,
    pub valid: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct SwapResult {
    pub token_in: String,
    pub token_out: String,
    pub amount_in: i128,
    pub amount_out: i128,
    pub fee_amount: i128,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct LiquidityQuote {
    pub amount_a: i128,
    pub amount_b: i128,
    pub shares: i128,
    pub pool_ratio: i128,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct OneSidedLiquidityQuote {
    pub token_out: String,
    pub shares: i128,
    pub amount_keep: i128,
    pub amount_swapped: i128,
    pub swap_output: i128,
    pub total_out: i128,
    pub protocol_fee: i128,
    pub net_protocol_fee: i128,
}

fn isqrt(n: i128) -> i128 {
    if n <= 0 {
        return 0;
    }

    let mut x = n;
    let mut y = (x + 1) / 2;

    while y < x {
        x = y;
        y = (x + n / x) / 2;
    }

    x
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pool_with_liquidity() -> PoolState {
        let mut pool = PoolState::new("TOKEN_A", "TOKEN_B", 30).expect("pool should be valid");

        pool.execute_add_liquidity(100_000, 100_000, 0)
            .expect("initial liquidity should succeed");

        pool
    }

    #[test]
    fn first_deposit_below_minimum_liquidity_returns_insufficient_shares() {
        let pool = PoolState::new("TOKEN_A", "TOKEN_B", 30).expect("pool should be valid");

        let result = pool.quote_add_liquidity(1_000, 1_000);

        assert!(matches!(result, Err(SimulationError::InsufficientShares)));
    }

    #[test]
    fn first_deposit_mints_sqrt_product_minus_minimum_liquidity() {
        let mut pool = PoolState::new("TOKEN_A", "TOKEN_B", 30).expect("pool should be valid");

        let quote = pool
            .execute_add_liquidity(2_000, 2_000, 1_000)
            .expect("first deposit should succeed");

        // sqrt(2,000 * 2,000) = 2,000.
        // Provider receives 2,000 - 1,000 = 1,000 shares.
        assert_eq!(quote.shares, 1_000);
        assert_eq!(pool.total_shares, 2_000);
        assert_eq!(pool.locked_liquidity, MINIMUM_LIQUIDITY);
        assert!(pool.min_liquidity_locked);

        let mut second_pool =
            PoolState::new("TOKEN_A", "TOKEN_B", 30).expect("pool should be valid");

        let second_quote = second_pool
            .execute_add_liquidity(5_000, 5_000, 4_000)
            .expect("first deposit should succeed");

        // sqrt(5,000 * 5,000) = 5,000.
        // Provider receives 5,000 - 1,000 = 4,000 shares.
        assert_eq!(second_quote.shares, 4_000);
        assert_eq!(second_pool.total_shares, 5_000);
        assert_eq!(second_pool.locked_liquidity, MINIMUM_LIQUIDITY);
    }

    #[test]
    fn second_deposit_is_unaffected_by_minimum_liquidity_lock() {
        let mut pool = PoolState::new("TOKEN_A", "TOKEN_B", 30).expect("pool should be valid");

        pool.execute_add_liquidity(2_000, 2_000, 1_000)
            .expect("first deposit should succeed");

        let quote = pool
            .quote_add_liquidity(1_000, 1_000)
            .expect("second deposit should succeed");

        // The pool has 2,000 total shares: 1,000 provider shares and
        // 1,000 permanently locked shares. The second provider receives:
        // 1,000 * 2,000 / 2,000 = 1,000 shares.
        assert_eq!(quote.shares, 1_000);

        pool.execute_add_liquidity(1_000, 1_000, 1_000)
            .expect("second deposit should execute");

        assert_eq!(pool.total_shares, 3_000);
        assert_eq!(pool.locked_liquidity, MINIMUM_LIQUIDITY);
        assert!(pool.min_liquidity_locked);
    }

    #[test]
    fn swap_exact_in_credits_net_protocol_fee_after_lp_rebate() {
        let mut pool = pool_with_liquidity();

        pool.protocol_fee_bps = 30;
        pool.lp_rebate_bps = 5_000;

        let initial_reserve_a = pool.reserve_a;

        let quote = pool
            .execute_swap_exact_in("TOKEN_A", 10_000, 0)
            .expect("swap should succeed");

        // protocol fee = 10,000 * 30 / 10,000 = 30
        // LP rebate = 30 * 5,000 / 10,000 = 15
        // net protocol fee = 30 - 15 = 15
        assert_eq!(pool.accrued_fee_a, 15);

        // The 15-token LP rebate remains in the reserve, so the reserve
        // increases by amount_in - net_protocol_fee.
        assert_eq!(pool.reserve_a, initial_reserve_a + 10_000 - 15);

        assert!(quote.amount_out > 0);
    }

    #[test]
    fn swap_with_zero_lp_rebate_preserves_previous_protocol_fee_accounting() {
        let mut pool = pool_with_liquidity();

        pool.protocol_fee_bps = 30;
        pool.lp_rebate_bps = 0;

        let initial_reserve_a = pool.reserve_a;

        pool.execute_swap_exact_in("TOKEN_A", 10_000, 0)
            .expect("swap should succeed");

        // With no LP rebate, the entire protocol fee is accrued.
        assert_eq!(pool.accrued_fee_a, 30);

        assert_eq!(pool.reserve_a, initial_reserve_a + 10_000 - 30);
    }

    #[test]
    fn swap_exact_out_credits_net_protocol_fee_after_lp_rebate() {
        let mut pool = pool_with_liquidity();

        pool.protocol_fee_bps = 30;
        pool.lp_rebate_bps = 5_000;

        let quote = pool
            .quote_swap_exact_out("TOKEN_B", 1_000)
            .expect("exact-out quote should succeed");

        let protocol_fee = quote.amount_in * pool.protocol_fee_bps / BPS_DENOMINATOR;

        let expected_rebate = protocol_fee * pool.lp_rebate_bps / BPS_DENOMINATOR;

        let expected_net_fee = protocol_fee - expected_rebate;

        pool.execute_swap_exact_out("TOKEN_B", 1_000, quote.amount_in)
            .expect("exact-out swap should succeed");

        assert_eq!(pool.accrued_fee_a, expected_net_fee);
    }

    #[test]
    fn validate_rejects_invalid_lp_rebate_bps() {
        let mut pool = PoolState::new("TOKEN_A", "TOKEN_B", 30).expect("pool should be valid");

        pool.lp_rebate_bps = BPS_DENOMINATOR + 1;

        assert!(matches!(
            pool.validate(),
            Err(SimulationError::InvalidFeeBps { fee_bps })
                if fee_bps == BPS_DENOMINATOR + 1
        ));
    }

    #[test]
    fn validate_accepts_valid_lp_rebate_bps_and_locked_state() {
        let mut pool = PoolState::new("TOKEN_A", "TOKEN_B", 30).expect("pool should be valid");

        pool.lp_rebate_bps = 5_000;
        pool.total_shares = 2_000;
        pool.locked_liquidity = MINIMUM_LIQUIDITY;
        pool.min_liquidity_locked = true;

        assert!(pool.validate().is_ok());
    }

    #[test]
    fn validate_rejects_populated_pool_without_minimum_liquidity_lock() {
        let mut pool = PoolState::new("TOKEN_A", "TOKEN_B", 30).expect("pool should be valid");

        pool.total_shares = MINIMUM_LIQUIDITY + 1;

        assert!(matches!(
            pool.validate(),
            Err(SimulationError::InvalidInput(_))
        ));
    }

    #[test]
    fn first_deposit_exactly_at_minimum_liquidity_is_rejected() {
        let pool = PoolState::new("TOKEN_A", "TOKEN_B", 30).expect("pool should be valid");

        // sqrt(1,000 * 1,000) = 1,000, so the provider would receive
        // zero shares after the mandatory 1,000-share lock.
        let result = pool.quote_add_liquidity(1_000, 1_000);

        assert!(matches!(result, Err(SimulationError::InsufficientShares)));
    }

    #[test]
    fn provider_cannot_withdraw_permanently_locked_liquidity() {
        let mut pool = PoolState::new("TOKEN_A", "TOKEN_B", 30).expect("pool should be valid");

        pool.execute_add_liquidity(2_000, 2_000, 1_000)
            .expect("first deposit should succeed");

        // total_shares = 2,000, of which 1,000 is permanently locked.
        // Only the provider's 1,000 shares can be withdrawn.
        let result = pool.quote_remove_liquidity(1_001);

        assert!(matches!(result, Err(SimulationError::SlippageExceeded)));

        let quote = pool
            .quote_remove_liquidity(1_000)
            .expect("provider shares should be withdrawable");

        assert_eq!(quote.amount_a, 1_000);
        assert_eq!(quote.amount_b, 1_000);
    }
}
