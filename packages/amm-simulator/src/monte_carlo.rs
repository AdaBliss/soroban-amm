use crate::engine::AmmSimulator;
use crate::error::Result;
use crate::pool::PoolState;
use crate::replay::{TradeAction, TradeRecord};
// `Pcg64` is a portable, fixed-algorithm PRNG, so a seeded Monte Carlo run
// reproduces on any machine and across `rand` releases. `SmallRng` was
// neither: rand documents it as non-portable, and it selects a different
// algorithm on 32- and 64-bit targets. The golden-value tests at the bottom
// of this file pin the resulting output.
use rand::{seq::SliceRandom, RngExt, SeedableRng};
use rand_pcg::Pcg64;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct MonteCarloConfig {
    pub iterations: usize,
    #[serde(default)]
    pub amount_shock_bps: u32,
    #[serde(default)]
    pub shuffle_trades: bool,
    #[serde(default)]
    pub seed: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct MonteCarloReport {
    pub config: MonteCarloConfig,
    pub base_pool: PoolState,
    pub trades: usize,
    pub successful_paths: usize,
    pub failed_paths: usize,
    pub mean_final_reserve_a: f64,
    pub mean_final_reserve_b: f64,
    pub mean_final_price: f64,
    pub median_final_reserve_a: i128,
    pub median_final_reserve_b: i128,
    pub p95_final_reserve_a: i128,
    pub p95_final_reserve_b: i128,
    pub min_final_reserve_a: i128,
    pub max_final_reserve_a: i128,
    pub min_final_reserve_b: i128,
    pub max_final_reserve_b: i128,
}

impl MonteCarloReport {
    pub fn run(
        base_pool: &PoolState,
        trades: &[TradeRecord],
        config: MonteCarloConfig,
    ) -> Result<Self> {
        let mut rng = Pcg64::seed_from_u64(config.seed);
        let mut reserve_a_samples = Vec::with_capacity(config.iterations);
        let mut reserve_b_samples = Vec::with_capacity(config.iterations);
        let mut price_samples = Vec::with_capacity(config.iterations);
        let mut successful_paths = 0usize;
        let mut failed_paths = 0usize;

        for _ in 0..config.iterations {
            let mut simulator = AmmSimulator::new(base_pool.clone());
            let mut scenario = trades.to_vec();

            if config.shuffle_trades {
                scenario.shuffle(&mut rng);
                let start_ts = scenario.first().map(|trade| trade.timestamp).unwrap_or(0);

                for (idx, trade) in scenario.iter_mut().enumerate() {
                    trade.timestamp = start_ts + idx as u64;
                }
            }

            let scenario = perturb_trades(&scenario, config.amount_shock_bps, &mut rng);
            simulator.replay(&scenario)?;

            if simulator.steps.iter().any(|step| step.error.is_some()) {
                failed_paths += 1;
            } else {
                successful_paths += 1;
                reserve_a_samples.push(simulator.pool.reserve_a);
                reserve_b_samples.push(simulator.pool.reserve_b);
                price_samples.push(simulator.pool.spot_price_a() as f64);
            }
        }

        // A report with no successful paths has no meaningful distribution
        // statistics, so return the documented all-zero report.
        if reserve_a_samples.is_empty() {
            return Ok(Self {
                config,
                base_pool: base_pool.clone(),
                trades: trades.len(),
                successful_paths,
                failed_paths,
                mean_final_reserve_a: 0.0,
                mean_final_reserve_b: 0.0,
                mean_final_price: 0.0,
                median_final_reserve_a: 0,
                median_final_reserve_b: 0,
                p95_final_reserve_a: 0,
                p95_final_reserve_b: 0,
                min_final_reserve_a: 0,
                max_final_reserve_a: 0,
                min_final_reserve_b: 0,
                max_final_reserve_b: 0,
            });
        }

        reserve_a_samples.sort_unstable();
        reserve_b_samples.sort_unstable();

        let mean_final_reserve_a = reserve_a_samples.iter().map(|v| *v as f64).sum::<f64>()
            / reserve_a_samples.len() as f64;

        let mean_final_reserve_b = reserve_b_samples.iter().map(|v| *v as f64).sum::<f64>()
            / reserve_b_samples.len() as f64;

        let mean_final_price = price_samples.iter().sum::<f64>() / price_samples.len() as f64;

        let median_index = reserve_a_samples.len() / 2;

        let p95_index = ((reserve_a_samples.len() as f64) * 0.95).floor() as usize;

        let p95_index = p95_index.min(reserve_a_samples.len() - 1);

        Ok(Self {
            config,
            base_pool: base_pool.clone(),
            trades: trades.len(),
            successful_paths,
            failed_paths,
            mean_final_reserve_a,
            mean_final_reserve_b,
            mean_final_price,
            median_final_reserve_a: reserve_a_samples[median_index],
            median_final_reserve_b: reserve_b_samples[median_index],
            p95_final_reserve_a: reserve_a_samples[p95_index],
            p95_final_reserve_b: reserve_b_samples[p95_index],
            min_final_reserve_a: *reserve_a_samples.first().unwrap(),
            max_final_reserve_a: *reserve_a_samples.last().unwrap(),
            min_final_reserve_b: *reserve_b_samples.first().unwrap(),
            max_final_reserve_b: *reserve_b_samples.last().unwrap(),
        })
    }
}

fn perturb_trades(
    trades: &[TradeRecord],
    amount_shock_bps: u32,
    rng: &mut Pcg64,
) -> Vec<TradeRecord> {
    if amount_shock_bps == 0 {
        return trades.to_vec();
    }

    trades
        .iter()
        .cloned()
        .map(|mut trade| {
            trade.action = match trade.action {
                TradeAction::SwapExactIn {
                    token_in,
                    amount_in,
                    min_out,
                } => TradeAction::SwapExactIn {
                    token_in,
                    amount_in: shock_amount(amount_in, amount_shock_bps, rng),
                    min_out,
                },

                TradeAction::SwapExactOut {
                    token_out,
                    amount_out,
                    max_in,
                } => TradeAction::SwapExactOut {
                    token_out,
                    amount_out: shock_amount(amount_out, amount_shock_bps, rng),
                    max_in,
                },

                TradeAction::AddLiquidity {
                    amount_a,
                    amount_b,
                    min_shares,
                } => TradeAction::AddLiquidity {
                    amount_a: shock_amount(amount_a, amount_shock_bps, rng),
                    amount_b: shock_amount(amount_b, amount_shock_bps, rng),
                    min_shares,
                },

                TradeAction::RemoveLiquidity {
                    shares,
                    min_a,
                    min_b,
                } => TradeAction::RemoveLiquidity {
                    shares: shock_amount(shares, amount_shock_bps, rng),
                    min_a,
                    min_b,
                },
                action @ (TradeAction::RemoveLiquidityOneSided { .. }
                | TradeAction::SetFees { .. }
                | TradeAction::Pause
                | TradeAction::Unpause) => action,
            };

            trade
        })
        .collect()
}

fn shock_amount(amount: i128, shock_bps: u32, rng: &mut Pcg64) -> i128 {
    let shock = rng.random_range(-(shock_bps as i128)..=(shock_bps as i128));

    let perturbed = amount + amount * shock / 10_000;

    // Trade amounts must never fall below one after perturbation.
    perturbed.max(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn funded_pool() -> PoolState {
        let mut pool = PoolState::new("TOKEN_A", "TOKEN_B", 30).unwrap();

        pool.reserve_a = 1_000_000;
        pool.reserve_b = 2_000_000;
        pool.total_shares = 1_000_000;

        pool
    }

    fn successful_trades() -> Vec<TradeRecord> {
        vec![TradeRecord {
            timestamp: 1,
            label: Some("deterministic swap".into()),
            action: TradeAction::SwapExactIn {
                token_in: "TOKEN_A".into(),
                amount_in: 10_000,
                min_out: 1,
            },
        }]
    }

    fn failed_trades() -> Vec<TradeRecord> {
        vec![TradeRecord {
            timestamp: 1,
            label: Some("empty pool swap".into()),
            action: TradeAction::SwapExactIn {
                token_in: "TOKEN_A".into(),
                amount_in: 1_000,
                min_out: 1,
            },
        }]
    }

    #[test]
    fn monte_carlo_same_seed_produces_reproducible_statistics() {
        let pool = funded_pool();
        let trades = successful_trades();

        let config = MonteCarloConfig {
            iterations: 20,
            amount_shock_bps: 500,
            shuffle_trades: false,
            seed: 42,
        };

        let first = MonteCarloReport::run(&pool, &trades, config.clone()).unwrap();

        let second = MonteCarloReport::run(&pool, &trades, config).unwrap();

        assert_eq!(first.successful_paths, second.successful_paths);
        assert_eq!(first.failed_paths, second.failed_paths);

        assert_eq!(first.mean_final_reserve_a, second.mean_final_reserve_a);
        assert_eq!(first.mean_final_reserve_b, second.mean_final_reserve_b);
        assert_eq!(first.mean_final_price, second.mean_final_price);

        assert_eq!(first.median_final_reserve_a, second.median_final_reserve_a);
        assert_eq!(first.median_final_reserve_b, second.median_final_reserve_b);

        assert_eq!(first.p95_final_reserve_a, second.p95_final_reserve_a);
        assert_eq!(first.p95_final_reserve_b, second.p95_final_reserve_b);

        assert_eq!(first.min_final_reserve_a, second.min_final_reserve_a);
        assert_eq!(first.max_final_reserve_a, second.max_final_reserve_a);
    }

    #[test]
    fn monte_carlo_all_failed_paths_returns_zero_report() {
        let pool = PoolState::new("TOKEN_A", "TOKEN_B", 30).unwrap();
        let trades = failed_trades();

        let config = MonteCarloConfig {
            iterations: 10,
            amount_shock_bps: 0,
            shuffle_trades: false,
            seed: 7,
        };

        let report = MonteCarloReport::run(&pool, &trades, config).unwrap();

        assert_eq!(report.successful_paths, 0);
        assert_eq!(report.failed_paths, 10);

        assert_eq!(report.mean_final_reserve_a, 0.0);
        assert_eq!(report.mean_final_reserve_b, 0.0);
        assert_eq!(report.mean_final_price, 0.0);

        assert_eq!(report.median_final_reserve_a, 0);
        assert_eq!(report.median_final_reserve_b, 0);
        assert_eq!(report.p95_final_reserve_a, 0);
        assert_eq!(report.p95_final_reserve_b, 0);

        assert_eq!(report.min_final_reserve_a, 0);
        assert_eq!(report.max_final_reserve_a, 0);
        assert_eq!(report.min_final_reserve_b, 0);
        assert_eq!(report.max_final_reserve_b, 0);
    }

    #[test]
    fn monte_carlo_p95_never_exceeds_maximum() {
        let pool = funded_pool();
        let trades = successful_trades();

        for iterations in [1, 2, 5, 10, 25, 50] {
            let config = MonteCarloConfig {
                iterations,
                amount_shock_bps: 1_000,
                shuffle_trades: false,
                seed: 123,
            };

            let report = MonteCarloReport::run(&pool, &trades, config).unwrap();

            assert!(
                report.successful_paths > 0,
                "expected at least one successful path for {iterations} iterations"
            );

            assert!(
                report.p95_final_reserve_a <= report.max_final_reserve_a,
                "p95 reserve A ({}) exceeded max reserve A ({})",
                report.p95_final_reserve_a,
                report.max_final_reserve_a
            );

            assert!(
                report.p95_final_reserve_b <= report.max_final_reserve_b,
                "p95 reserve B ({}) exceeded max reserve B ({})",
                report.p95_final_reserve_b,
                report.max_final_reserve_b
            );
        }
    }

    #[test]
    fn shock_amount_never_returns_less_than_one() {
        let mut rng = Pcg64::seed_from_u64(99);

        for _ in 0..1_000 {
            let shocked = shock_amount(1, 100_000, &mut rng);

            assert!(
                shocked >= 1,
                "shock_amount returned invalid value: {shocked}"
            );
        }
    }

    #[test]
    fn perturb_trades_preserves_trade_count_and_actions() {
        let trades = vec![
            TradeRecord {
                timestamp: 1,
                label: Some("swap in".into()),
                action: TradeAction::SwapExactIn {
                    token_in: "TOKEN_A".into(),
                    amount_in: 1_000,
                    min_out: 1,
                },
            },
            TradeRecord {
                timestamp: 2,
                label: Some("swap out".into()),
                action: TradeAction::SwapExactOut {
                    token_out: "TOKEN_B".into(),
                    amount_out: 500,
                    max_in: Some(1_000),
                },
            },
            TradeRecord {
                timestamp: 3,
                label: Some("add".into()),
                action: TradeAction::AddLiquidity {
                    amount_a: 2_000,
                    amount_b: 4_000,
                    min_shares: 1,
                },
            },
            TradeRecord {
                timestamp: 4,
                label: Some("remove".into()),
                action: TradeAction::RemoveLiquidity {
                    shares: 100,
                    min_a: 1,
                    min_b: 1,
                },
            },
        ];

        let mut rng = Pcg64::seed_from_u64(11);
        let perturbed = perturb_trades(&trades, 500, &mut rng);

        assert_eq!(perturbed.len(), trades.len());

        for (original, updated) in trades.iter().zip(perturbed.iter()) {
            assert_eq!(original.timestamp, updated.timestamp);
            assert_eq!(original.label, updated.label);

            match (&original.action, &updated.action) {
                (
                    TradeAction::SwapExactIn {
                        token_in: original_token,
                        min_out: original_min_out,
                        ..
                    },
                    TradeAction::SwapExactIn {
                        token_in: updated_token,
                        min_out: updated_min_out,
                        amount_in,
                    },
                ) => {
                    assert_eq!(original_token, updated_token);
                    assert_eq!(original_min_out, updated_min_out);
                    assert!(*amount_in >= 1);
                }

                (
                    TradeAction::SwapExactOut {
                        token_out: original_token,
                        max_in: original_max_in,
                        ..
                    },
                    TradeAction::SwapExactOut {
                        token_out: updated_token,
                        max_in: updated_max_in,
                        amount_out,
                    },
                ) => {
                    assert_eq!(original_token, updated_token);
                    assert_eq!(original_max_in, updated_max_in);
                    assert!(*amount_out >= 1);
                }

                (
                    TradeAction::AddLiquidity {
                        min_shares: original_min_shares,
                        ..
                    },
                    TradeAction::AddLiquidity {
                        min_shares: updated_min_shares,
                        amount_a,
                        amount_b,
                    },
                ) => {
                    assert_eq!(original_min_shares, updated_min_shares);
                    assert!(*amount_a >= 1);
                    assert!(*amount_b >= 1);
                }

                (
                    TradeAction::RemoveLiquidity {
                        min_a: original_min_a,
                        min_b: original_min_b,
                        ..
                    },
                    TradeAction::RemoveLiquidity {
                        min_a: updated_min_a,
                        min_b: updated_min_b,
                        shares,
                    },
                ) => {
                    assert_eq!(original_min_a, updated_min_a);
                    assert_eq!(original_min_b, updated_min_b);
                    assert!(*shares >= 1);
                }

                _ => panic!("trade action variant changed during perturbation"),
            }
        }
    }

    /// Pins the exact output of a seeded run.
    ///
    /// `Pcg64` is a fixed algorithm and portable across platforms, so these
    /// numbers should hold on any machine and across `rand` upgrades. What
    /// this test really guards is the rest of the pipeline: `random_range`
    /// and `shuffle` live in `rand` and carry no cross-major value-stability
    /// guarantee, so if a future bump changes how randomness is consumed,
    /// this fails loudly instead of silently moving everyone's backtests.
    ///
    /// If it fails after a dependency bump, that is a real change in reported
    /// output: confirm it is only resampling noise, then re-baseline these
    /// constants in the same PR and call it out in the CHANGELOG.
    #[test]
    fn seeded_run_matches_golden_values() {
        let report = MonteCarloReport::run(
            &funded_pool(),
            &successful_trades(),
            MonteCarloConfig {
                iterations: 20,
                amount_shock_bps: 500,
                shuffle_trades: true,
                seed: 42,
            },
        )
        .expect("seeded run should succeed");

        assert_eq!(report.successful_paths, 20);
        assert_eq!(report.failed_paths, 0);
        assert_eq!(report.mean_final_reserve_a, 1009922.7);
        assert_eq!(report.mean_final_reserve_b, 1980408.55);
        assert_eq!(report.median_final_reserve_a, 1009940);
        assert_eq!(report.median_final_reserve_b, 1980392);
        assert_eq!(report.p95_final_reserve_a, 1010429);
        assert_eq!(report.p95_final_reserve_b, 1981227);
        assert_eq!(report.min_final_reserve_a, 1009504);
        assert_eq!(report.max_final_reserve_a, 1010429);
    }

    /// Pins the raw draw sequence, so a failure in the test above can be told
    /// apart at a glance: if this one still passes, the generator is fine and
    /// something downstream of it changed.
    #[test]
    fn seeded_rng_draw_sequence_is_stable() {
        let mut rng = Pcg64::seed_from_u64(7);
        let draws: Vec<i128> = (0..6)
            .map(|_| rng.random_range(-500i128..=500i128))
            .collect();

        assert_eq!(draws, vec![-269, 335, 92, -176, 68, 272]);
    }
}
