//! Swap parity between the simulator and the real concentrated_liquidity
//! contract.
//!
//! Each fixture builds the same pool twice — once by running the contract in
//! a Soroban test environment, once as a [`ClPoolState`] — then replays the
//! same swaps against both and compares the outcome after every step:
//! amounts in and out, sqrt price, current tick, active liquidity, global fee
//! growth, the full set of initialized ticks with every `TickInfo` field,
//! fee growth inside every position's range, and accrued protocol fees.
//!
//! Rounding tolerance: none. The simulator ports the contract's integer math
//! step for step, so any difference, however small, fails the test.
//!
//! Positions are opened on-chain with `mint_position` (token amounts); the
//! liquidity the contract derives is read back and fed to
//! [`ClPoolState::add_liquidity`], so both sides start from identical ticks.

use concentrated_liquidity::{ConcentratedLiquidity, ConcentratedLiquidityClient};
use soroban_amm_simulator::cl::{swap_math, ClPoolState};
use soroban_sdk::testutils::Address as _;
use soroban_sdk::token::{StellarAssetClient, TokenClient};
use soroban_sdk::{Address, Env};

const MIN_TICK: i32 = swap_math::MIN_TICK;
const MAX_TICK: i32 = swap_math::MAX_TICK;
const DEADLINE: u64 = u64::MAX;
const FUNDING: i128 = 1_000_000_000_000_000;

struct Fixture {
    name: &'static str,
    fee_bps: i128,
    protocol_fee_bps: i128,
    tick_spacing: i32,
    initial_tick: i32,
    /// `(lower_tick, upper_tick, amount_a_desired, amount_b_desired)`
    positions: &'static [(i32, i32, i128, i128)],
    /// `(zero_for_one, amount_in, price limit as a tick, or None for no limit)`
    swaps: &'static [(bool, i128, Option<i32>)],
}

const FIXTURES: &[Fixture] = &[
    Fixture {
        name: "single range, both directions, crossing out of range",
        fee_bps: 30,
        protocol_fee_bps: 0,
        tick_spacing: 10,
        initial_tick: 0,
        positions: &[(-600, 600, 1_000_000_000_000, 1_000_000_000_000)],
        swaps: &[
            (true, 1_000_000, None),
            (true, 250_000_000_000, None),
            (false, 600_000_000_000, None),
            (false, 5_000_000_000_000, None),
            (true, 777_777_777, None),
        ],
    },
    Fixture {
        name: "overlapping ranges, multi-tick crossings, protocol fee",
        fee_bps: 30,
        protocol_fee_bps: 2_000,
        tick_spacing: 10,
        initial_tick: -250,
        positions: &[
            (-1_000, 1_000, 500_000_000_000, 500_000_000_000),
            (-500, -100, 200_000_000_000, 200_000_000_000),
            (-300, 300, 300_000_000_000, 300_000_000_000),
            (200, 800, 100_000_000_000, 100_000_000_000),
        ],
        swaps: &[
            (false, 400_000_000_000, None),
            (true, 900_000_000_000, None),
            (false, 123_456_789, None),
            (false, 1_500_000_000_000, None),
            (true, 50_000_000_000, None),
        ],
    },
    Fixture {
        name: "price limits in both directions",
        fee_bps: 5,
        protocol_fee_bps: 1_000,
        tick_spacing: 1,
        initial_tick: 100,
        positions: &[
            (-200, 400, 800_000_000_000, 800_000_000_000),
            (50, 150, 100_000_000_000, 100_000_000_000),
        ],
        swaps: &[
            (true, 10_000_000_000_000, Some(75)),
            (true, 10_000_000_000_000, Some(-120)),
            (false, 10_000_000_000_000, Some(149)),
            (false, 1_000_000, Some(390)),
            (false, 10_000_000_000_000, Some(390)),
        ],
    },
    Fixture {
        name: "negative ticks with an unfunded gap between ranges",
        fee_bps: 100,
        protocol_fee_bps: 0,
        tick_spacing: 60,
        initial_tick: -23_040,
        positions: &[
            (-24_000, -22_020, 300_000_000_000, 300_000_000_000),
            (-21_000, -19_980, 300_000_000_000, 300_000_000_000),
        ],
        swaps: &[
            (false, 20_000_000_000, None),
            (false, 200_000_000_000, None),
            (true, 150_000_000_000, None),
            (true, 900_000_000_000_000, None),
        ],
    },
    Fixture {
        name: "zero fee pool",
        fee_bps: 0,
        protocol_fee_bps: 0,
        tick_spacing: 10,
        initial_tick: 5,
        positions: &[(-100, 100, 50_000_000_000, 50_000_000_000)],
        swaps: &[
            (true, 3_000_000_000, None),
            (false, 6_000_000_000, None),
            (true, 1, None),
        ],
    },
];

struct Chain<'a> {
    env: Env,
    admin: Address,
    fee_recipient: Address,
    client: ConcentratedLiquidityClient<'a>,
    token_a: TokenClient<'a>,
    token_b: TokenClient<'a>,
    trader: Address,
}

fn deploy(env: &Env, fx: &Fixture) -> Chain<'static> {
    let env = env.clone();
    env.budget().reset_unlimited();
    env.mock_all_auths();

    let admin = Address::generate(&env);
    let fee_recipient = Address::generate(&env);
    let trader = Address::generate(&env);
    let token_a = env
        .register_stellar_asset_contract_v2(admin.clone())
        .address();
    let token_b = env
        .register_stellar_asset_contract_v2(admin.clone())
        .address();
    for token in [&token_a, &token_b] {
        StellarAssetClient::new(&env, token).mint(&trader, &FUNDING);
    }

    let cl = env.register_contract(None, ConcentratedLiquidity);
    let client = ConcentratedLiquidityClient::new(&env, &cl);
    client.initialize(
        &admin,
        &token_a,
        &token_b,
        &fx.fee_bps,
        &fx.initial_tick,
        &fx.tick_spacing,
    );
    if fx.protocol_fee_bps > 0 {
        client.set_protocol_fee(&admin, &fee_recipient, &fx.protocol_fee_bps);
    }

    Chain {
        token_a: TokenClient::new(&env, &token_a),
        token_b: TokenClient::new(&env, &token_b),
        env,
        admin,
        fee_recipient,
        client,
        trader,
    }
}

/// Every initialized tick on-chain, found by walking the tick bitmap.
fn chain_ticks(chain: &Chain) -> Vec<i32> {
    let mut ticks = Vec::new();
    let mut cursor = MIN_TICK - 1;
    while let Some(t) = chain.client.next_initialized_tick_pub(&cursor) {
        ticks.push(t);
        cursor = t;
    }
    ticks
}

fn assert_same_state(chain: &Chain, sim: &ClPoolState, ranges: &[(i32, i32)], ctx: &str) {
    let state = chain.client.get_pool_state();
    assert_eq!(
        state.sqrt_price as i128,
        sim.sqrt_price_x96(),
        "{ctx}: sqrt price"
    );
    assert_eq!(
        state.current_tick,
        sim.current_tick(),
        "{ctx}: current tick"
    );
    assert_eq!(
        state.active_liquidity, sim.liquidity,
        "{ctx}: active liquidity"
    );

    // Uninitialized end ticks make fee_growth_inside return the globals.
    assert_eq!(
        chain.client.fee_growth_inside(&MIN_TICK, &MAX_TICK),
        (sim.fee_growth_global_a, sim.fee_growth_global_b),
        "{ctx}: global fee growth"
    );

    let sim_ticks: Vec<i32> = sim.ticks.keys().copied().collect();
    assert_eq!(chain_ticks(chain), sim_ticks, "{ctx}: initialized ticks");
    for (tick, info) in &sim.ticks {
        let on_chain = chain.client.get_tick_info(tick);
        assert_eq!(
            (
                on_chain.liquidity_gross,
                on_chain.liquidity_net,
                on_chain.fee_growth_outside_a,
                on_chain.fee_growth_outside_b,
            ),
            (
                info.liquidity_gross,
                info.liquidity_net,
                info.fee_growth_outside_a,
                info.fee_growth_outside_b,
            ),
            "{ctx}: tick {tick}"
        );
    }

    for &(lower, upper) in ranges {
        assert_eq!(
            chain.client.fee_growth_inside(&lower, &upper),
            sim.fee_growth_inside(lower, upper),
            "{ctx}: fee growth inside [{lower}, {upper})"
        );
    }
}

/// What a fixture's swaps exercised, as reported by the simulator after the
/// contract has been shown to agree with it.
#[derive(Default)]
struct Coverage {
    ticks_crossed: usize,
    stopped_at_limit: usize,
    partial_fills: usize,
}

fn run(fx: &Fixture) -> Coverage {
    let mut coverage = Coverage::default();
    let env = Env::default();
    let chain = deploy(&env, fx);

    let mut sim = ClPoolState::new("A", "B", fx.fee_bps, fx.tick_spacing).unwrap();
    sim.initialize_at_tick(fx.initial_tick).unwrap();
    sim.set_protocol_fee_bps(fx.protocol_fee_bps).unwrap();

    let mut ranges = Vec::new();
    for (i, &(lower, upper, amount_a, amount_b)) in fx.positions.iter().enumerate() {
        let provider = Address::generate(&chain.env);
        for token in [&chain.token_a, &chain.token_b] {
            StellarAssetClient::new(&chain.env, &token.address).mint(&provider, &FUNDING);
        }
        chain.client.mint_position(
            &provider, &lower, &upper, &amount_a, &amount_b, &0, &0, &DEADLINE,
        );
        let liquidity = chain
            .client
            .get_position(&provider, &lower, &upper)
            .liquidity;
        sim.add_liquidity(&format!("lp{i}"), lower, upper, liquidity)
            .unwrap();
        ranges.push((lower, upper));
    }
    assert_same_state(
        &chain,
        &sim,
        &ranges,
        &format!("{}: after minting", fx.name),
    );

    for (step, &(zero_for_one, amount_in, limit_tick)) in fx.swaps.iter().enumerate() {
        let ctx = format!("{}: swap #{step}", fx.name);
        let limit = limit_tick.map_or(0, swap_math::tick_to_sqrt_price_x96);
        let (token_in, token_out) = if zero_for_one {
            (&chain.token_a, &chain.token_b)
        } else {
            (&chain.token_b, &chain.token_a)
        };

        let in_before = token_in.balance(&chain.trader);
        let out_before = token_out.balance(&chain.trader);
        let amount_out = chain.client.swap(
            &chain.trader,
            &zero_for_one,
            &amount_in,
            &limit,
            &0,
            &DEADLINE,
        );
        let taken = in_before - token_in.balance(&chain.trader);
        assert_eq!(token_out.balance(&chain.trader) - out_before, amount_out);

        let result = sim
            .swap(zero_for_one, amount_in, limit as i128, 0)
            .unwrap_or_else(|e| panic!("{ctx}: simulator failed: {e}"));
        assert_eq!(result.amount_out, amount_out, "{ctx}: amount out");
        assert_eq!(result.amount_in, taken, "{ctx}: amount in");
        assert_same_state(&chain, &sim, &ranges, &ctx);

        coverage.ticks_crossed += result.ticks_crossed.len();
        if limit != 0 && result.sqrt_price_x96 == limit as i128 {
            coverage.stopped_at_limit += 1;
        }
        if result.amount_in < amount_in {
            coverage.partial_fills += 1;
        }
    }

    // Protocol fees: withdraw on-chain and compare what the recipient got.
    chain.client.withdraw_protocol_fees(&chain.admin);
    let recipient = if fx.protocol_fee_bps > 0 {
        &chain.fee_recipient
    } else {
        &chain.admin
    };
    assert_eq!(
        (
            chain.token_a.balance(recipient),
            chain.token_b.balance(recipient)
        ),
        (sim.protocol_fee_a, sim.protocol_fee_b),
        "{}: protocol fees",
        fx.name
    );
    coverage
}

#[test]
fn simulated_swaps_match_the_contract() {
    let mut total = Coverage::default();
    for fx in FIXTURES {
        let c = run(fx);
        total.ticks_crossed += c.ticks_crossed;
        total.stopped_at_limit += c.stopped_at_limit;
        total.partial_fills += c.partial_fills;
    }
    // Agreement only means something if the fixtures reach the interesting
    // paths: tick crossings, price limits, and swaps cut short by a limit or
    // by running out of liquidity.
    assert!(
        total.ticks_crossed >= 5,
        "only {} tick crossings",
        total.ticks_crossed
    );
    assert!(
        total.stopped_at_limit >= 2,
        "only {} swaps stopped at their limit",
        total.stopped_at_limit
    );
    assert!(
        total.partial_fills >= 3,
        "only {} partial fills",
        total.partial_fills
    );
}
