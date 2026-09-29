# Soroban AMM Python Client Examples

This folder contains Python scripts showing how to interact with the Soroban AMM contracts using the Stellar Python SDK (`stellar-sdk`).

It contains examples for the following contracts:
- **AMM Pool Contract** (`client.py`)
- **Factory Contract** (`factory_client.py`)
- **Governance Contract** (`governance_client.py`)
- **TWAP Consumer Contract** (`twap_client.py`)

## Install

Set up a virtual environment and install the required dependencies:

```sh
python3 -m venv .venv
. .venv/bin/activate
pip install -r requirements.txt
```

---

## 1. AMM Pool Client (`client.py`)

Demonstrates adding liquidity, quoting, and executing swaps on a specific pool.

### Configure

Set the environment variables before running the script:

```sh
export AMM_CONTRACT_ID=<deployed AMM contract id>
export SOURCE_SECRET=<secret key for the transaction source and LP/trader>
export TOKEN_IN_CONTRACT_ID=<token A or token B contract id>
export SWAP_AMOUNT_IN=100000
export SWAP_MIN_OUT=0
export SWAP_DEADLINE_SECONDS=300 # Optional, deadline window in seconds (defaults to 300)
```

### Run

```sh
python client.py
```

---

## 2. Factory Client (`factory_client.py`)

Demonstrates deploying and querying pools registry via the pool factory.

`create_pool` calls the factory's `create_pool(caller, token_a, token_b,
fee_tier, governance_wasm_hash)` with the deploying address as `caller` and,
optionally, a `governance_wasm_hash` (32-byte WASM hash) that deploys a
per-pool governance contract alongside the pool. It returns both the pool
address and the optional governance contract address; when no governance
WASM hash is supplied, the governance address is `None`.

### Configure

Set the environment variables before running the script:

```sh
export FACTORY_CONTRACT_ID=<deployed factory contract id>
export SOURCE_SECRET=<secret key for the transaction source and deployer>
export TOKEN_A_CONTRACT_ID=<token A contract id>
export TOKEN_B_CONTRACT_ID=<token B contract id>
export FEE_BPS=30 # Optional, defaults to 30
```

### Run

```sh
python factory_client.py
```

---

## 3. Governance Client (`governance_client.py`)

Demonstrates submitting fee proposals, querying proposal status/details, and casting LP-weighted votes (supporting `For`, `Against`, and `Abstain`).

### Configure

Set the environment variables before running the script:

```sh
export GOV_CONTRACT_ID=<deployed governance contract id>
export SOURCE_SECRET=<secret key for the proposer/voter LP holder>
export PROPOSAL_FEE_BPS=50 # Optional, target pool fee to propose (defaults to 50)
export PROPOSAL_ID=<proposal ID to query/vote on> # Optional, if not set, a new proposal is created
export VOTE_CHOICE=For # Optional: For, Against, or Abstain (defaults to For)
```

### Run

```sh
python governance_client.py
```

---

## 4. TWAP Consumer Client (`twap_client.py`)

Demonstrates reading a manipulation-resistant time-weighted average price from
the TWAP consumer contract: it saves a price snapshot for a pool, reads the
TWAP (single direction and both directions) over a window, optionally validates
a real-time spot price against the TWAP, and lists the pools the consumer is
tracking.

A TWAP over `window_seconds` needs a snapshot taken roughly `window_seconds`
ago, so in production `save_snapshot` is invoked on a schedule (e.g. a keeper
every minute). When run once, the read step will report that it needs an
earlier snapshot; run the script again after `window_seconds` has elapsed, or
set `SAVE_SNAPSHOT=false` to read against snapshots saved previously.

### Configure

Set the environment variables before running the script:

```sh
export TWAP_CONTRACT_ID=<deployed TWAP consumer contract id>
export POOL_CONTRACT_ID=<AMM pool contract id to read prices from>
export SOURCE_SECRET=<secret key for the transaction source / snapshot keeper>
export WINDOW_SECONDS=60 # Optional, TWAP window in seconds (defaults to 60)
export SAVE_SNAPSHOT=true # Optional, set to false to skip saving a snapshot
export SPOT_PRICE=<price, 1_000_000 scale> # Optional, enables spot-vs-TWAP validation
export MAX_DEVIATION_BPS=500 # Optional, allowed spot/TWAP deviation (defaults to 500)
```

### Run

```sh
python twap_client.py
```

## Shared helpers, errors, and development checks

All examples import configuration, ScVal conversion, JSON formatting, and RPC wrappers from `common.py`. Missing configuration raises `ConfigError`; failed simulation or submission raises `InvocationError` with a readable contract-method message and a link to `docs/error-codes.md`. The examples keep all process exit behavior inside their `__main__` guards, so helpers are safe to import in tests.

The examples target **Soroban Testnet** by default. Override `STELLAR_RPC_URL` and `STELLAR_NETWORK_PASSPHRASE` when using another network. Every script accepts the shared settings `STELLAR_RPC_URL` and `STELLAR_NETWORK_PASSPHRASE`; the contract-specific variables are listed in each section above.

## Exit-code contract

Every example exits with one of the following codes:

| Code | Meaning |
|------|---------|
| `0`  | All steps completed successfully. |
| `1`  | An unexpected error occurred (contract call failed, missing config, etc.). |
| `2`  | *(twap_client only)* No snapshot old enough exists yet — expected on the first run. Re-run after `WINDOW_SECONDS` have elapsed. |

Scripts that encounter an expected transient condition (e.g. `twap_client.py` finding no old-enough snapshot) exit with the documented code for that condition rather than `0`, so callers and CI can distinguish "ran fine" from "ran but couldn't complete yet".

Install exact runtime dependencies with:

```sh
python3 -m venv .venv
. .venv/bin/activate
python -m pip install -r requirements.txt
```

Install development dependencies and run the offline helper suite with:

```sh
python -m pip install -r requirements-dev.txt
python -m pytest examples/python
python -m mypy examples/python
python -m ruff check examples/python
```

Each script can be inspected without credentials using `python examples/python/<script>.py --help` once the command-line options are added by the corresponding example update; a complete configured run is, for example, `AMM_CONTRACT_ID=... SOURCE_SECRET=... TOKEN_IN_CONTRACT_ID=... python examples/python/client.py`.
