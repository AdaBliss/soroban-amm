# Soroban AMM Simulator

Off-chain simulation engine for the Soroban AMM pool.

## What it does

- Simulates swaps with the same constant-product math as the on-chain pool
- Replays historical trade logs for backtesting
- Runs Monte Carlo stress tests over a trade set
- Simulates concentrated-liquidity swaps across ticks (`cl` module and CLI)
- Exposes both a Rust library and a CLI

## Concentrated liquidity

`soroban_amm_simulator::cl::ClPoolState` models a `concentrated_liquidity`
pool: price it with `initialize` (sqrt price) or `initialize_at_tick`, add
liquidity to tick ranges with `add_liquidity`, and trade with `swap`, which
steps through initialized ticks, applies each crossed tick's `liquidity_net`,
accrues fee growth and flips `fee_growth_outside`, and stops at a price limit
or where liquidity runs out.

```rust
use soroban_amm_simulator::cl::{swap_math, ClPoolState};

let mut pool = ClPoolState::new("XLM", "USDC", 30, 10)?;
pool.initialize_at_tick(0)?;
pool.add_liquidity("lp", -600, 600, 10_000_000_000_000)?;
let limit = swap_math::tick_to_sqrt_price_x96(-120) as i128; // 0 = no limit
let result = pool.swap(true, 1_000_000_000, limit, 0)?;
println!("out {} crossed {:?}", result.amount_out, result.ticks_crossed);
```

The swap is a port of the contract's own tick walk, integer rounding
included, and uses the same tick/price mapping as the contract's swap path
(`cl::swap_math`, not `cl::math`, which mirrors the contract's position math).
`tests/cl_swap_parity.rs` runs the real contract beside it on shared fixtures
and requires identical amounts, prices, ticks, liquidity and fee growth after
every swap, with zero tolerance. The oracle-deviation guard, deadlines, auth
and token transfers are not modelled.

## CLI

The Cargo package is `soroban_amm_simulator` and its executable target is
`amm_sim` (the executable's displayed command name is `amm-sim`). `--compact`
is the default; add `--pretty` for indented JSON.

```bash
cargo run -p soroban_amm_simulator --bin amm_sim -- quote \
  --pool pool.json \
  --token-in XLM \
  --amount-in 1000000 \
  --pretty

cargo run -p soroban_amm_simulator --bin amm_sim -- replay \
  --pool pool.json \
  --trades trades.json --compact

cargo run -p soroban_amm_simulator --bin amm_sim -- monte-carlo \
  --pool pool.json \
  --trades trades.json \
  --iterations 1000 \
  --amount-shock-bps 50

# CL state files are serialized ClPoolState JSON (including ticks and positions).
# Produce the file with serde_json::to_writer from a ClPoolState value.
cargo run -p soroban_amm_simulator --bin amm_sim -- cl-quote \
  --pool cl-pool.json --zero-for-one --amount-in 1000000
```

## JSON formats

`pool.json`

```json
{
  "token_a": "XLM",
  "token_b": "USDC",
  "reserve_a": 1000000,
  "reserve_b": 1000000,
  "total_shares": 1000000,
  "fee_bps": 30,
  "protocol_fee_bps": 5,
  "min_liquidity_locked": true,
  "locked_liquidity": 1000
}
```

Populated pools must carry the permanent first-deposit lock marker. Here,
1,000 of the 1,000,000 total shares are locked; both fields are required.

`trades.json`

```json
[
  {
    "timestamp": 1,
    "kind": "swap_exact_in",
    "token_in": "XLM",
    "amount_in": 100000
  },
  {
    "timestamp": 2,
    "kind": "swap_exact_out",
    "token_out": "USDC",
    "amount_out": 50000,
    "max_in": 60000
  }
]
```

For `cl-quote` and `cl-replay`, `--pool` is the JSON serialization of the
simulator's `ClPoolState` (`token_a`, `token_b`, price, tick, liquidity, fee
growth, initialized `ticks`, and `positions`). `cl-replay --swaps` takes a JSON
array with `zero_for_one`, `amount_in`, and optional
`sqrt_price_limit_x96`/`min_out` fields per swap.

Replay also accepts `remove_liquidity_one_sided` records with `shares`,
`token_out`, and optional `min_out`; `set_fees` records with `fee_bps`,
`protocol_fee_bps`, and optional `lp_rebate_bps`; and `pause`/`unpause` records.
CSV uses the same fields: `shares`, `token_out`, `min_out`, `fee_bps`,
`protocol_fee_bps`, and `lp_rebate_bps`. Control records use only
`timestamp,kind`. `set_fees` is a simulator replay configuration action; the
current V2 contract has no runtime swap-fee setter. One-sided contract parity
is kept as an ignored test for AMM issue #1093 until the reserve behavior is
resolved.
