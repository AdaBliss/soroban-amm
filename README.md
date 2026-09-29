# Soroban AMM

[![CI](https://github.com/promisszn/soroban-amm/workflows/CI/badge.svg)](https://github.com/promisszn/soroban-amm/actions)

A full-stack AMM protocol built on Stellar's Soroban smart contract platform. It ships a battle-tested V2 constant-product pool today and is actively building a V3-style concentrated liquidity engine — the only open-source implementation of its kind on Stellar.

---

## Table of Contents

- [Why This Project](#why-this-project)
- [Overview](#overview)
- [Architecture](#architecture)
- [Contracts](#contracts)
  - [AMM Pool Contract](#amm-pool-contract)
  - [LP Token Contract](#lp-token-contract)
  - [Factory Contract](#factory-contract)
  - [Governance Contract](#governance-contract)
  - [TWAP Consumer Contract](#twap-consumer-contract)
  - [Concentrated Liquidity Contract](#concentrated-liquidity-contract)
  - [Staking Contract](#staking-contract)
- [Storage Layout and Upgrade Considerations](#storage-layout-and-upgrade-considerations)
- [Error Codes](#error-codes)
- [Math and Formulas](#math-and-formulas)
- [Getting Started](#getting-started)
  - [Prerequisites](#prerequisites)
  - [Build](#build)
  - [Test](#test)
- [Usage](#usage)
  - [Deploy via Factory](#deploy-via-factory)
  - [Deploy Manually](#deploy-manually)
  - [Add Liquidity](#add-liquidity)
  - [Swap Tokens](#swap-tokens)
  - [Remove Liquidity](#remove-liquidity)
  - [Query the Pool](#query-the-pool)
  - [Use the TWAP Oracle](#use-the-twap-oracle)
  - [TypeScript Client Example](#typescript-client-example)
  - [Python Client Example](#python-client-example)
- [Off-chain Simulator](#off-chain-simulator)
- [Deployment Runbook](docs/deployment-runbook.md)
- [Roadmap](#roadmap)
- [Contributing](#contributing)
- [Changelog](#changelog)
- [Security](#security)
- [License](#license)

---

## Why This Project

Stellar has fast finality (~5 seconds), sub-cent fees, and a large existing user base around stablecoins and remittances. Its native DEX is order-book based — a constant-product AMM is a fundamentally different and more composable liquidity model that fills a real gap in the ecosystem.

Several AMM protocols already exist on Stellar, but each has meaningful limitations:

| Protocol | Pool model | Governance | Open source |
|---|---|---|---|
| **Soroswap** | V2 constant-product only | None | Yes |
| **Phoenix** | V2 + stable pools | None | Yes |
| **Sushi** | Concentrated liquidity | None | No (Sushi mainline) |
| **Aquarius** | Governance layer only | AQUA token | Partial |
| **This project** | V2 now + V3 CL in progress | On-chain, LP-token-governed | Yes |

What makes this project different:

- **Concentrated liquidity (V3) in development** — the only open-source Soroban implementation targeting tick-based range positions, the same capital-efficiency model pioneered by Uniswap v3. LPs can earn more fees by concentrating capital in active price ranges instead of spreading it across an infinite curve.
- **On-chain governance** — LP token holders can propose and vote on fee changes directly through a governance contract, with configurable quorum, voting windows, minimum stake, vote locking, and proposal cancellation. No protocol is governed off-chain or by a single admin key.
- **TWAP oracle** — a manipulation-resistant time-weighted average price feed that other protocols (lending markets, derivatives) can build on top of, without needing a separate oracle network.
- **Flash loans** — single-transaction borrowing from pool reserves with a configurable fee, enabling arbitrage, collateral swaps, and liquidation bots.
- **Full composability** — every contract in the protocol is independently deployable and interoperable. The factory, governance, and TWAP contracts can be used with any pool, not just the ones deployed here.

---

## Overview

The protocol lets users:

- **Provide liquidity** — deposit two tokens into a pool and receive LP tokens representing their share of the reserves (V2), or mint a tick-range position for concentrated capital efficiency (V3).
- **Swap tokens** — exchange one pool token for the other at a price determined by the pool's invariant, with slippage protection.
- **Redeem liquidity** — burn LP tokens (V2) or close a position (V3) to withdraw a proportional share of reserves plus accrued fees.
- **Govern the protocol** — stake LP tokens to propose fee changes; vote on active proposals; execute passing proposals on-chain.
- **Access price data** — query a TWAP oracle for a manipulation-resistant average price over any window.

---

## Architecture

The project is a Cargo workspace with five contracts:

```
soroban-amm/
├── Cargo.toml                        # Workspace root
└── contracts/
    ├── amm/                          # V2 constant-product AMM pool
    │   └── src/lib.rs
    ├── token/                        # SEP-41 LP token contract
    │   └── src/lib.rs
    ├── factory/                      # Pool factory and registry
    │   └── src/lib.rs
    ├── governance/                   # On-chain LP governance
    │   └── src/lib.rs
    ├── twap_consumer/                # TWAP oracle consumer
    │   └── src/lib.rs
    └── concentrated_liquidity/       # V3-style tick-based AMM (in progress)
        └── src/lib.rs
```

The V2 AMM contract depends on the token contract — adding or removing liquidity mints or burns LP shares via the token contract. The factory deploys and initialises an AMM + LP token pair in a single transaction. The governance contract holds a reference to a pool and allows LP token holders to vote on parameter changes. The TWAP consumer reads cumulative price state from any AMM pool. The concentrated liquidity contract is standalone — it does not use the LP token and manages positions internally.

---

## Contracts

The protocol consists of modular, composable smart contracts. Detailed machine-readable specifications of all public contract functions, argument types, return values, and events are maintained in **[`docs/abi.json`](docs/abi.json)**.

### AMM Pool Contract

Located in [`contracts/amm/src/lib.rs`](contracts/amm/src/lib.rs).

The core V2 constant-product pool (`x * y = k`). It manages reserves for a token pair, executes swaps with configurable fees and slippage protection, mints and burns LP shares, accumulates TWAP price ratios, provides flash loans, and enforces emergency circuit breakers and pause controls.

- **Flash loans**: Single-transaction borrowing from pool reserves repayable within the receiver callback with configurable fees (`flash_loan`). Borrowers must implement the [`FlashLoanReceiver`](examples/flash_loan_receiver/README.md) callback interface.
- **Protocol fees**: Configurable protocol fee share routed to a dedicated recipient address.
- **Admin & Safety**: Two-step admin transfer (`propose_admin` / `accept_admin`), emergency pause/unpause, and single-block price deviation circuit breaker.
- **Interface & Schema**: See [`docs/abi.json`](docs/abi.json) under `"amm"` for complete function definitions (`initialize`, `swap`, `add_liquidity`, `remove_liquidity`, `flash_loan`, `get_amount_out`, `get_info`, etc.).

#### Flash Loan Receiver Interface

Borrowers must implement a callback contract with this interface:

```rust
pub trait FlashLoanReceiver {
    fn on_flash_loan(env: Env, token: Address, amount: i128, fee: i128, data: Bytes) -> bool;
}
```

During `flash_loan`, the AMM transfers `amount` of `token` to `receiver`, invokes `on_flash_loan`, and verifies that the pool's token balance increased by at least `fee`. If the receiver does not return `amount + fee` before the callback finishes, the transaction reverts. See [examples/flash_loan_receiver/README.md](examples/flash_loan_receiver/README.md) for a reference implementation covering arbitrage, collateral swaps, and failure modes.

### LP Token Contract

Located in [`contracts/token/src/lib.rs`](contracts/token/src/lib.rs).

A SEP-41 compliant token representing proportional liquidity shares in a pool. The corresponding AMM pool contract is the administrator with sole authority to `mint` and `burn` shares.

- **Capabilities**: Standard SEP-41 balance queries, transfers, and allowances, plus balance locking for governance voting.
- **Interface & Schema**: See [`docs/abi.json`](docs/abi.json) under `"token"`.

### Factory Contract

Located in [`contracts/factory/src/lib.rs`](contracts/factory/src/lib.rs).

A single-entry-point registry for deploying and discovering pools.

- Deploys an AMM pool and its paired LP token in a single atomic transaction.
- Normalizes token pair order (smaller address first) to enforce uniqueness per pair.
- Maintains a registry of all deployed pools with total count, paginated listings (`get_pools`), and reverse LP token lookups.
- **Interface & Schema**: See [`docs/abi.json`](docs/abi.json) under `"factory"`.

### Governance Contract

Located in [`contracts/governance/src/lib.rs`](contracts/governance/src/lib.rs).

Enables on-chain parameter management governed by LP token holders.

- **Proposals**: LP token holders meeting the minimum stake threshold can propose fee and parameter changes.
- **Voting**: Votes are weighted by the voter's LP token balance at the time of voting; tokens are locked until proposal resolution.
- **Execution & Safety**: Passed proposals require a timelock delay before execution, and support emergency cancellation and veto mechanics.
- **Interface & Schema**: See [`docs/abi.json`](docs/abi.json) under `"governance"`.

### TWAP Consumer Contract

Located in [`contracts/twap_consumer/src/lib.rs`](contracts/twap_consumer/src/lib.rs).

An integration contract that reads the AMM's cumulative price oracle and computes a fixed-window TWAP for external consumers.

- Stores periodic snapshots (`save_snapshot`).
- Computes windowed average prices (`get_twap_price`).
- Validates spot prices against TWAP with configurable maximum deviation (`validate_price_against_twap`, `assert_lending_price_safe`).
- **Interface & Schema**: See [`docs/abi.json`](docs/abi.json) under `"twap_consumer"`.

### Concentrated Liquidity Contract

Located in [`contracts/concentrated_liquidity/src/lib.rs`](contracts/concentrated_liquidity/src/lib.rs).

A V3-style tick-based AMM where liquidity providers specify a price range `[lower_tick, upper_tick]` for their capital. Only liquidity within the active price range earns fees, enabling significantly higher capital efficiency than full-range V2 pools.

- **Status: In active development.** The position model, fee accounting, and in-place position modification flows are implemented. The tick registry, tick bitmap, math library, and swap engine are tracked in issues [#177](https://github.com/promisszn/soroban-amm/issues/177)–[#180](https://github.com/promisszn/soroban-amm/issues/180).
- **Key differences from V2**: Range-bound capital concentration, non-fungible position records `(owner, lower_tick, upper_tick)`, and tick accumulators for geometric TWAP.
- **Source**: See [`contracts/concentrated_liquidity/src/lib.rs`](contracts/concentrated_liquidity/src/lib.rs).

### Staking Contract

Located in [`contracts/staking/src/lib.rs`](contracts/staking/src/lib.rs).

Lets liquidity providers stake LP tokens to earn secondary reward tokens.

- **Boost & Escrow**: Stakers can lock LP tokens for fixed durations to earn a boost multiplier on rewards (modelled on Curve's veToken design).
- **Accumulator**: Rewards are distributed using a rewards-per-share accumulator.
- **Full Guide**: See [`contracts/staking/README.md`](contracts/staking/README.md).

---

## Storage Layout and Upgrade Considerations

Soroban contracts partition state across storage tiers based on lifetime and access patterns:

- **Instance Storage**: Used for contract configuration, admin keys, and pool parameters whose lifetime matches the contract instance.
- **Persistent Storage**: Used for user balances, allowances, and position records with independent TTL management.

**Upgrade Considerations:**
- **Storage Immutability**: Critical parameters (e.g., token pair addresses and LP token contract identity) are established at initialization and remain immutable.
- **DataKey Stability**: State is keyed by the binary representation of `DataKey` enums. Modifying variant order, discriminants, or payload types is a breaking storage change.
- **Code Upgrades**: Logic upgrades are performed via `upgrade(new_wasm_hash)`. Changing storage layout or migrating tiers requires explicit migration procedures. See the **[Deployment Runbook](docs/deployment-runbook.md)** for upgrade guides and checklists.

---

## Error Codes

Protocol entry points return typed Soroban contract errors (`#[contracterror]`) with numeric discriminants and descriptive symbols rather than generic panics.

The authoritative reference for all error codes, numeric discriminants, failure causes, and recovery remedies across all protocol contracts is maintained in **[`docs/error-codes.md`](docs/error-codes.md)**:

- **AMM Pool Errors (`AmmError`)**: Defined in [`contracts/amm`](contracts/amm)
- **Factory Errors (`FactoryError`)**: Defined in [`contracts/factory`](contracts/factory)
- **Governance Errors (`GovernanceError`)**: Defined in [`contracts/governance`](contracts/governance)
- **LP Token Errors**: Trap and assertion conditions in [`contracts/token`](contracts/token)
- **Other Contracts**: Concentrated Liquidity, Staking, Router, Oracle Aggregator, etc.

For complete discriminant tables, causes, and remedies, consult **[`docs/error-codes.md`](docs/error-codes.md)**. CI automatically verifies that document against all contract enums on every pull request via `make check-docs`.

---

## Math and Formulas

### Constant-Product Invariant (V2)

Every swap must satisfy:

```
reserve_a * reserve_b = k   (constant)
```

### Swap Output

Fees are deducted from the input before applying the formula:

```
amount_in_with_fee = amount_in * (10_000 - fee_bps)

amount_out = (amount_in_with_fee * reserve_out)
           / (reserve_in * 10_000 + amount_in_with_fee)
```

### Initial LP Shares (First Deposit)

Uses the geometric mean of the deposited amounts:

```
shares = sqrt(amount_a * amount_b)
```

### Subsequent LP Shares

Uses the lesser of the two proportional contributions to prevent imbalanced deposits:

```
shares = min(
    amount_a * total_shares / reserve_a,
    amount_b * total_shares / reserve_b
)
```

### Liquidity Removal

Proportional to pool ownership at the time of withdrawal:

```
out_a = shares * reserve_a / total_shares
out_b = shares * reserve_b / total_shares
```

### Concentrated Liquidity Price Model

Price is represented as `sqrtPrice` — the square root of the token B / token A ratio. Ticks are integer indices where each tick step is a `0.01%` price change:

```
price(tick) = 1.0001^tick
```

Token amounts for a position `[lower_tick, upper_tick]` with liquidity `L` are derived from the sqrt price at each boundary, following the Uniswap v3 whitepaper formulas.

---

## Getting Started

### Prerequisites

- [Rust](https://www.rust-lang.org/tools/install) (stable toolchain)
- `wasm32v1-none` compilation target:
  ```sh
  rustup target add wasm32v1-none
  ```
- [Stellar CLI](https://developers.stellar.org/docs/tools/stellar-cli) (`stellar`) for deployment:
  ```sh
  cargo install --locked stellar-cli --features opt
  ```

### Setup

1. **Clone the repository:**

   ```sh
   git clone https://github.com/promisszn/soroban-amm.git
   cd soroban-amm
   ```

2. **Verify the toolchain and target are installed:**

   ```sh
   rustup show                          # confirm stable toolchain is active
   rustup target list --installed       # should include wasm32v1-none
   ```

   If the WASM target is missing:

   ```sh
   rustup target add wasm32v1-none
   ```

3. **Configure the Stellar CLI for your target network** (testnet shown):

   ```sh
   stellar network add testnet \
     --rpc-url https://soroban-testnet.stellar.org \
     --network-passphrase "Test SDF Network ; September 2015"
   ```

4. **Create or import an account identity:**

   ```sh
   # Generate a new keypair and fund it via Friendbot
   stellar keys generate --default-seed mykey
   stellar keys fund mykey --network testnet
   ```

   Or import an existing secret key:

   ```sh
   stellar keys add mykey --secret-key
   # paste your secret key when prompted
   ```

5. **Confirm everything is wired up:**

   ```sh
   stellar keys address mykey           # should print your public key
   ```

You are now ready to build, test, and deploy.

### Build

Build all contracts as optimised WASM binaries:

```sh
cargo build --release --target wasm32v1-none
```

Or via the Makefile alias:

```sh
make build
```

To optimize the compiled WASM binaries for size (typically reducing size by 20-40% using `wasm-opt` through Stellar CLI):

```sh
make optimize
```

Output files:

```
target/wasm32v1-none/release/amm.wasm
target/wasm32v1-none/release/token.wasm
target/wasm32v1-none/release/factory.wasm
target/wasm32v1-none/release/governance.wasm
target/wasm32v1-none/release/twap_consumer.wasm
target/wasm32v1-none/release/concentrated_liquidity.wasm
```

### Test

Run the full test suite across all packages:

```sh
cargo build --release --target wasm32v1-none
cargo test --workspace
```

The factory tests embed compiled WASM at compile time, so the build step is required before running tests. All other packages can be tested independently without a prior build.

The same command runs in CI on every pull request.

For a real-network smoke test on Stellar testnet, run the end-to-end script:

```sh
scripts/e2e.sh
```

The script deploys fresh contracts, funds a test account, adds liquidity, swaps, removes liquidity, and exits non-zero on any failed assertion. CI runs it against testnet nightly, on relevant pushes to `main` and on each release, and files an issue when it fails.

---

## Usage

### Automated Deployment

The fastest way to deploy the full protocol (all 18 contracts) to testnet or mainnet is using the provided deployment script. For prerequisites, deployment order, per-contract parameters, verification, upgrades, and emergency procedures, see the **[Deployment Runbook](docs/deployment-runbook.md)**.

```sh
./scripts/deploy.sh [network] [--only factory,pools] [--skip staking] [--force]
```

- **network**: Optional target network (defaults to `testnet`). Also reads `$NETWORK` / `$STELLAR_NETWORK`.
- **--only / --skip**: Deploy a subset of contracts (comma-separated names). Useful for incremental or single-contract redeploys.
- **--force**: Re-deploy even if an address is already persisted; without it, re-running is a no-op and a killed run resumes where it left off.
- The script builds `wasm32v1-none` artifacts, generates/funds a deployer account if needed, uploads WASM hashes, deploys and initializes every contract in dependency order, and verifies each initialization by reading state back.
- Deployed contract IDs and WASM hashes are printed to the console and persisted to `.soroban-amm.deploy.env` incrementally (every address as it is created).

### ABI Schema & Events

- **ABI Specification**: A complete machine-readable JSON schema of all public contract functions, arguments, return types, and event definitions is available at **[`docs/abi.json`](docs/abi.json)**.
- **Event Schema Versioning**: All contract events use versioned payloads (`schema_version: u32`) to ensure backward-compatible indexing. For complete topic formats and event payload specifications, see **[`docs/event-schema-versioning.md`](docs/event-schema-versioning.md)**.


### Development

The project includes a `Makefile` to simplify common development tasks:

- `make build`: Build contracts for production (`wasm32v1-none`)
- `make test`: Build WASM then run all contract unit tests
- `make fmt`: Format code using `cargo fmt`
- `make lint`: Run `clippy` with warnings treated as errors
- `make check`: Run formatting, linting, and tests in sequence
- `make deploy`: Deploy contracts to testnet via `scripts/deploy.sh`
- `make e2e`: Run full end-to-end integration tests
- `make clean`: Remove build artifacts

### Reproducible Builds with Docker

To ensure identical WASM binaries across different environments, you can use the provided Docker configuration:

```sh
# Build using Docker Compose
docker compose run --rm build

# Alternatively, using raw Docker
docker build -t soroban-amm-build .
docker run --rm -v $(pwd):/app soroban-amm-build
```

- **Base Image**: `rust:1.98.1-slim-bookworm` (matches `rust-toolchain.toml`)
- **Stellar CLI**: `27.1.0`

### Deploy via Factory

The factory is the recommended way to create pools. It deploys and initialises the AMM pool and its LP token in a single transaction, and registers the pool in its on-chain registry.

**1. Upload the contract WASM blobs:**

```sh
stellar contract upload \
  --wasm target/wasm32v1-none/release/amm.wasm \
  --network testnet --source <YOUR_KEY>
# → prints AMM_WASM_HASH

stellar contract upload \
  --wasm target/wasm32v1-none/release/token.wasm \
  --network testnet --source <YOUR_KEY>
# → prints TOKEN_WASM_HASH
```

**2. Deploy the factory:**

```sh
stellar contract deploy \
  --wasm target/wasm32v1-none/release/factory.wasm \
  --network testnet --source <YOUR_KEY>
# → prints FACTORY_CONTRACT_ID
```

**3. Initialise the factory:**

```sh
stellar contract invoke \
  --id <FACTORY_CONTRACT_ID> \
  --network testnet --source <YOUR_KEY> \
  -- initialize \
  --admin <YOUR_ADDRESS> \
  --amm_wasm_hash <AMM_WASM_HASH> \
  --token_wasm_hash <TOKEN_WASM_HASH>
```

**4. Create a pool (deploys AMM + LP token, registers the pair):**

```sh
stellar contract invoke \
  --id <FACTORY_CONTRACT_ID> \
  --network testnet --source <YOUR_KEY> \
  -- create_pool \
  --token_a <TOKEN_A_CONTRACT_ID> \
  --token_b <TOKEN_B_CONTRACT_ID> \
  --fee_bps 30
# → prints the new POOL_CONTRACT_ID
```

**5. Look up an existing pool:**

```sh
stellar contract invoke \
  --id <FACTORY_CONTRACT_ID> \
  -- get_pool \
  --token_a <TOKEN_A_CONTRACT_ID> \
  --token_b <TOKEN_B_CONTRACT_ID>

stellar contract invoke --id <FACTORY_CONTRACT_ID> -- all_pools
```

---

### Deploy Manually

Deploy the LP token contract first, then the AMM pool. The AMM contract address becomes the LP token's admin.

```sh
# Deploy the LP token
stellar contract deploy \
  --wasm target/wasm32v1-none/release/token.wasm \
  --network testnet \
  --source <YOUR_KEY>

# Deploy the AMM pool
stellar contract deploy \
  --wasm target/wasm32v1-none/release/amm.wasm \
  --network testnet \
  --source <YOUR_KEY>
```

Initialize the LP token (admin = AMM contract address):

```sh
stellar contract invoke \
  --id <LP_TOKEN_CONTRACT_ID> \
  --network testnet \
  --source <YOUR_KEY> \
  -- initialize \
  --admin <AMM_CONTRACT_ID> \
  --name "Pool LP Token" \
  --symbol "AMMLP" \
  --decimals 7
```

Initialize the AMM pool (fee of 30 bps = 0.30%):

```sh
stellar contract invoke \
  --id <AMM_CONTRACT_ID> \
  --network testnet \
  --source <YOUR_KEY> \
  -- initialize \
  --token_a <TOKEN_A_CONTRACT_ID> \
  --token_b <TOKEN_B_CONTRACT_ID> \
  --lp_token <LP_TOKEN_CONTRACT_ID> \
  --fee_bps 30 \
  --fee_recipient <FEE_RECIPIENT_ADDRESS> \
  --protocol_fee_bps 0
```

### Add Liquidity

```sh
stellar contract invoke \
  --id <AMM_CONTRACT_ID> \
  --network testnet \
  --source <YOUR_KEY> \
  -- add_liquidity \
  --provider <PROVIDER_ADDRESS> \
  --amount_a 1000000 \
  --amount_b 2000000 \
  --min_shares 0 \
  --deadline <UNIX_TIMESTAMP>
```

`min_shares` is the minimum LP tokens you are willing to accept. Set to `0` to skip slippage protection during initial seeding. `deadline` is the latest ledger timestamp at which the call is valid.

### Swap Tokens

```sh
stellar contract invoke \
  --id <AMM_CONTRACT_ID> \
  --network testnet \
  --source <YOUR_KEY> \
  -- swap \
  --trader <TRADER_ADDRESS> \
  --token_in <TOKEN_A_CONTRACT_ID> \
  --amount_in 100000 \
  --min_out 0 \
  --deadline <UNIX_TIMESTAMP>
```

Use `get_amount_out` first to compute an appropriate `min_out`.

### Remove Liquidity

```sh
stellar contract invoke \
  --id <AMM_CONTRACT_ID> \
  --network testnet \
  --source <YOUR_KEY> \
  -- remove_liquidity \
  --provider <PROVIDER_ADDRESS> \
  --shares <LP_SHARE_AMOUNT> \
  --min_a 0 \
  --min_b 0 \
  --deadline <UNIX_TIMESTAMP>
```

### Query the Pool

```sh
# Full pool info
stellar contract invoke --id <AMM_CONTRACT_ID> -- get_info

# Quote a swap
stellar contract invoke --id <AMM_CONTRACT_ID> \
  -- get_amount_out \
  --token_in <TOKEN_A_CONTRACT_ID> \
  --amount_in 100000

# LP share balance
stellar contract invoke --id <AMM_CONTRACT_ID> \
  -- shares_of --provider <PROVIDER_ADDRESS>
```

### Use the TWAP Oracle

The AMM exposes cumulative price state with `get_price_cumulative()`. The example consumer contract shows one way to turn that into a fixed-window TWAP.

1. Deploy `twap_consumer.wasm`.
2. Save a snapshot (for example every minute):

```sh
stellar contract invoke \
  --id <TWAP_CONSUMER_CONTRACT_ID> \
  --network testnet --source <YOUR_KEY> \
  -- save_snapshot \
  --pool <AMM_CONTRACT_ID>
```

3. After `window_seconds` has elapsed, read TWAP:

```sh
stellar contract invoke \
  --id <TWAP_CONSUMER_CONTRACT_ID> \
  --network testnet --source <YOUR_KEY> \
  -- get_twap_price \
  --pool <AMM_CONTRACT_ID> \
  --window_seconds 60
```

4. Validate the real-time AMM spot price against TWAP before accepting it in another protocol:

```sh
stellar contract invoke \
  --id <TWAP_CONSUMER_CONTRACT_ID> \
  --network testnet --source <YOUR_KEY> \
  -- validate_price_against_twap \
  --pool <AMM_CONTRACT_ID> \
  --window_seconds 60 \
  --spot_price <AMM_PRICE_RATIO_A> \
  --max_deviation_bps 500
```

Lending contracts can call `assert_lending_price_safe` before valuing collateral. The helper reverts when the real-time spot price differs from TWAP by more than `max_deviation_bps`, preventing a flash-loan-moved spot price from being used as the collateral oracle.

Notes:

- `window_seconds` must be greater than 0.
- `save_snapshot` must have been called at approximately `now_ts - window_seconds`.
- Returned TWAP is scaled the same way as AMM spot price (`1_000_000` scale factor).
- `max_deviation_bps` is configurable per integration; for example, `500` allows a 5% spot/TWAP difference.

### Client SDKs

| Language | Package | Covers |
|---|---|---|
| TypeScript | [`packages/sdk`](packages/sdk) (`@soroban-amm/sdk`) | AMM pool, factory, governance, concentrated liquidity, staking, incentive campaigns, router |
| Go | [`packages/go-sdk`](packages/go-sdk) | AMM pool (`contracts/amm`), with its own envelope, ScVal and RPC code and no third-party dependencies |
| Rust | [`contracts/amm-sdk`](contracts/amm-sdk) (`soroban_amm_sdk`) | typed client, shared types and event decoders for the AMM contracts |

There is no mobile SDK. Android and iOS apps can call the pools through the
Soroban RPC with any Stellar SDK for their platform, using the Go SDK as a
reference for envelope construction and ScVal encoding.

### TypeScript Client Example

A standalone TypeScript client is available in [examples/client](examples/client). It demonstrates connecting to Stellar testnet RPC, reading `get_info()`, quoting with `get_amount_out()`, executing `swap()`, and reading LP shares with `shares_of()`.

```sh
cd examples/client
npm install
npm run build
npm start
```

### Python Client Example

A standalone Python client is available in [examples/python](examples/python). It demonstrates the same flow using `py-stellar-base` (`stellar-sdk`): connect to Stellar testnet RPC, read `get_info()`, quote with `get_amount_out()`, execute `swap()`, and read LP shares with `shares_of()`.

```sh
cd examples/python
python3 -m venv .venv
. .venv/bin/activate
pip install -r requirements.txt
python client.py
```

---

## Off-chain Simulator

The repository now includes `packages/amm-simulator`, a Rust library and CLI for:

- swap simulation without gas costs
- multi-step strategy testing
- historical backtesting
- Monte Carlo stress testing

See [packages/amm-simulator/README.md](packages/amm-simulator/README.md) for the command-line usage and JSON formats.

---

## Roadmap

See **[ROADMAP.md](ROADMAP.md)** for the full phased plan. In brief: the V2
constant-product core (AMM, LP token, factory, governance, TWAP oracle) is
shipped and covered by 460+ tests and a fuzz suite; the V3-style concentrated
liquidity engine and the routing/ecosystem contracts are in active development;
and a formal third-party audit, testnet/mainnet deployment, and a web frontend
are planned.

---

## Contributing

Contributions are welcome. See **[CONTRIBUTING.md](CONTRIBUTING.md)** for the full contributor guide — setup, project layout, testing, and the pull-request process. New contributors should start with issues labeled [`good first issue`](https://github.com/promisszn/soroban-amm/labels/good%20first%20issue) and [`help wanted`](https://github.com/promisszn/soroban-amm/labels/help%20wanted). The guidelines below summarize the key points.

### Reporting Issues

- Search existing issues before opening a new one.
- Include the Rust / `soroban-sdk` version, the steps to reproduce, and the expected vs. actual behavior.
- For security vulnerabilities, **do not open a public issue** — see [SECURITY.md](SECURITY.md) for the responsible disclosure process.

### Development Workflow

1. **Fork** the repository and create a branch from `main`:

   ```sh
   git checkout -b feat/my-feature
   ```

   Branch naming conventions:
   | Prefix | Use for |
   |---|---|
   | `feat/` | New features |
   | `fix/` | Bug fixes |
   | `refactor/` | Code restructuring without behavior change |
   | `test/` | Adding or improving tests |
   | `docs/` | Documentation only |
   | `chore/` | Build scripts, tooling, dependencies |

2. **Make your changes**, then ensure the build and tests pass:

   ```sh
   cargo build --release --target wasm32v1-none
   cargo test --workspace
   ```

3. **Write tests** for any new behavior. All public functions should have at least one test. Tests live alongside the implementation in `src/lib.rs` under a `#[cfg(test)]` module.

4. **Keep commits focused.** One logical change per commit. Use the [Conventional Commits](https://www.conventionalcommits.org/) format:

   ```
   feat: add time-weighted average price accumulator
   fix: prevent zero-share mint on initial deposit
   test: cover swap with maximum fee setting
   ```

5. **Open a Pull Request** against `main`. In the PR description:
   - Explain _what_ changed and _why_.
   - Reference any related issues with `Closes #<issue>` or `Related to #<issue>`.
   - If the change affects contract behavior, include before/after output or test coverage evidence.

### Code Style

- An [`.editorconfig`](.editorconfig) at the workspace root defines shared formatting rules (UTF-8, LF line endings, 4-space indentation, trailing-whitespace trimming). Most editors apply it automatically; install the [EditorConfig plugin](https://editorconfig.org/#download) if yours does not.
- A [`rustfmt.toml`](rustfmt.toml) at the workspace root defines Rust formatting rules. It enforces:
  - **Edition**: 2021
  - **Max width**: 100 columns
  - **Indentation**: 4 spaces
  - **Line endings**: Unix (LF)
  - **Import grouping**: Standard library, external crates, then crate-local modules
- Run `cargo fmt` before committing to automatically apply these rules.
- Run `cargo clippy -- -D warnings` and resolve any warnings before opening a PR.
- Prefer explicit arithmetic with overflow checks over silent wrapping. The release profile already enables `overflow-checks = true`.
- Avoid unsafe code. There is no reason to use `unsafe` in a Soroban contract.
- Do not add dependencies without discussion. The contract binary size and attack surface matter.

### Pull Request Checklist

Before requesting review, confirm:

- [ ] `cargo fmt` has been run
- [ ] `cargo clippy -- -D warnings` passes
- [ ] `cargo test --workspace` passes
- [ ] New behavior is covered by tests
- [ ] Public interface and error documentation updated (`docs/abi.json`, `docs/error-codes.md`)
- [ ] `CHANGELOG.md` has been updated with any notable changes
- [ ] Commit messages follow the Conventional Commits format

### Versioning

This project follows [Semantic Versioning](https://semver.org/). Breaking changes to the on-chain interface (function signatures, storage layout, error codes) constitute a major version bump.

---

## Changelog

See [CHANGELOG.md](CHANGELOG.md) for a history of notable changes to this project.

---

## Security

Please do not open public issues for security vulnerabilities. See [SECURITY.md](SECURITY.md) for the full vulnerability disclosure policy, supported versions, and how to reach the maintainers privately.

---

## License

This project is licensed under the [MIT License](LICENSE).
