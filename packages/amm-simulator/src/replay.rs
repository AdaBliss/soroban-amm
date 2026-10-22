use crate::pool::{LiquidityQuote, OneSidedLiquidityQuote, PoolState, SwapQuote, SwapResult};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TradeAction {
    SwapExactIn {
        token_in: String,
        amount_in: i128,
        #[serde(default)]
        min_out: i128,
    },
    SwapExactOut {
        token_out: String,
        amount_out: i128,
        #[serde(default)]
        max_in: Option<i128>,
    },
    AddLiquidity {
        amount_a: i128,
        amount_b: i128,
        #[serde(default)]
        min_shares: i128,
    },
    RemoveLiquidity {
        shares: i128,
        #[serde(default)]
        min_a: i128,
        #[serde(default)]
        min_b: i128,
    },
    RemoveLiquidityOneSided {
        shares: i128,
        token_out: String,
        #[serde(default)]
        min_out: i128,
    },
    SetFees {
        fee_bps: i128,
        protocol_fee_bps: i128,
        #[serde(default)]
        lp_rebate_bps: i128,
    },
    Pause,
    Unpause,
}

#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct TradeRecord {
    pub timestamp: u64,
    #[serde(default)]
    pub label: Option<String>,
    #[serde(flatten)]
    pub action: TradeAction,
}

/// The flat on-disk shape of a [`TradeRecord`]: `timestamp`/`label` alongside a
/// `kind` discriminator and that kind's own fields, as documented in the
/// crate README.
///
/// `TradeRecord` cannot simply derive `Deserialize` over a `#[serde(flatten)]`
/// internally tagged `TradeAction`. Both `flatten` and `tag = "..."` route the
/// value through serde's private `Content` buffer, which rejects 128-bit
/// integers, so every `i128` amount fails with "i128 is not supported"
/// (serde-rs/serde#1183) and no trade file could be loaded at all.
/// Deserializing through this explicit struct keeps the documented format while
/// bypassing that buffer. Serialization is unaffected and still derived.
///
/// The CSV loader reads the same field names, one row per record.
#[derive(Deserialize)]
pub(crate) struct FlatTradeRecord {
    pub timestamp: u64,
    #[serde(default)]
    pub label: Option<String>,
    pub kind: String,
    #[serde(default)]
    pub token_in: Option<String>,
    #[serde(default)]
    pub token_out: Option<String>,
    #[serde(default)]
    pub amount_in: Option<i128>,
    #[serde(default)]
    pub amount_out: Option<i128>,
    #[serde(default)]
    pub amount_a: Option<i128>,
    #[serde(default)]
    pub amount_b: Option<i128>,
    #[serde(default)]
    pub shares: Option<i128>,
    #[serde(default)]
    pub min_out: Option<i128>,
    #[serde(default)]
    pub max_in: Option<i128>,
    #[serde(default)]
    pub min_shares: Option<i128>,
    #[serde(default)]
    pub min_a: Option<i128>,
    #[serde(default)]
    pub min_b: Option<i128>,
    #[serde(default)]
    pub fee_bps: Option<i128>,
    #[serde(default)]
    pub protocol_fee_bps: Option<i128>,
    #[serde(default)]
    pub lp_rebate_bps: Option<i128>,
}

impl FlatTradeRecord {
    /// Returns `Err` with the offending discriminator if `kind` is unsupported.
    pub(crate) fn into_record(self) -> std::result::Result<TradeRecord, String> {
        let action = match self.kind.as_str() {
            "swap_exact_in" => TradeAction::SwapExactIn {
                token_in: self.token_in.unwrap_or_default(),
                amount_in: self.amount_in.unwrap_or_default(),
                min_out: self.min_out.unwrap_or_default(),
            },
            "swap_exact_out" => TradeAction::SwapExactOut {
                token_out: self.token_out.unwrap_or_default(),
                amount_out: self.amount_out.unwrap_or_default(),
                max_in: self.max_in,
            },
            "add_liquidity" => TradeAction::AddLiquidity {
                amount_a: self.amount_a.unwrap_or_default(),
                amount_b: self.amount_b.unwrap_or_default(),
                min_shares: self.min_shares.unwrap_or_default(),
            },
            "remove_liquidity" => TradeAction::RemoveLiquidity {
                shares: self.shares.unwrap_or_default(),
                min_a: self.min_a.unwrap_or_default(),
                min_b: self.min_b.unwrap_or_default(),
            },
            "remove_liquidity_one_sided" => TradeAction::RemoveLiquidityOneSided {
                shares: self.shares.unwrap_or_default(),
                token_out: self.token_out.unwrap_or_default(),
                min_out: self.min_out.unwrap_or_default(),
            },
            "set_fees" => TradeAction::SetFees {
                fee_bps: self.fee_bps.unwrap_or_default(),
                protocol_fee_bps: self.protocol_fee_bps.unwrap_or_default(),
                lp_rebate_bps: self.lp_rebate_bps.unwrap_or_default(),
            },
            "pause" => TradeAction::Pause,
            "unpause" => TradeAction::Unpause,
            other => return Err(other.to_string()),
        };

        Ok(TradeRecord {
            timestamp: self.timestamp,
            label: self.label,
            action,
        })
    }
}

impl<'de> Deserialize<'de> for TradeRecord {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        FlatTradeRecord::deserialize(deserializer)?
            .into_record()
            .map_err(|kind| serde::de::Error::custom(format!("unknown trade kind `{kind}`")))
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct TradeOutcome {
    pub record: TradeRecord,
    pub before: PoolState,
    pub after: PoolState,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub swap: Option<SwapQuote>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exact_out: Option<SwapResult>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub liquidity: Option<LiquidityQuote>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub one_sided_liquidity: Option<OneSidedLiquidityQuote>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ReplaySummary {
    pub trades: usize,
    pub successful_trades: usize,
    pub failed_trades: usize,
    pub total_amount_in: i128,
    pub total_amount_out: i128,
    pub total_fees: i128,
    pub final_pool: PoolState,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ReplayReport {
    pub summary: ReplaySummary,
    pub steps: Vec<crate::engine::SimulationStep>,
}

impl ReplayReport {
    pub fn from_simulator(simulator: &crate::engine::AmmSimulator) -> Self {
        Self {
            summary: ReplaySummary {
                trades: simulator.steps.len(),
                successful_trades: simulator
                    .steps
                    .iter()
                    .filter(|step| step.error.is_none())
                    .count(),
                failed_trades: simulator
                    .steps
                    .iter()
                    .filter(|step| step.error.is_some())
                    .count(),
                total_amount_in: simulator.total_amount_in,
                total_amount_out: simulator.total_amount_out,
                total_fees: simulator.total_fees,
                final_pool: simulator.pool.clone(),
            },
            steps: simulator.steps.clone(),
        }
    }
}
