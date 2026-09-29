# Soroban AMM Simulator

Off-chain simulation engine for the Soroban AMM pool.

## What it does

- Simulates swaps with the same constant-product math as the on-chain pool
- Replays historical trade logs for backtesting
- Runs Monte Carlo stress tests over a trade set
- Simulates concentrated-liquidity swaps across ticks (`cl` module, library only)
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

```bash
cargo run -p soroban-amm-simulator --bin amm-sim -- quote \
  --pool pool.json \
  --token-in XLM \
  --amount-in 1000000 \
  --pretty

cargo run -p soroban-amm-simulator --bin amm-sim -- replay \
  --pool pool.json \
  --trades trades.json

cargo run -p soroban-amm-simulator --bin amm-sim -- monte-carlo \
  --pool pool.json \
  --trades trades.json \
  --iterations 1000 \
  --amount-shock-bps 50
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
  "protocol_fee_bps": 5
}
```

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
