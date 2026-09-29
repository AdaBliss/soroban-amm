# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]
### Removed
- `packages/android-sdk` is removed (#962). It was one Kotlin file whose two methods, transaction building and submission, were empty TODOs, under the placeholder `com.example.amm` namespace. No workflow built or tested it, no manifest referenced it and no document mentioned it, so the directory only implied mobile support that did not exist. Removing it is deliberate: a real Android SDK would need transaction construction, signing and RPC transport, XDR fixtures shared with the Go SDK and a Gradle CI job, and should arrive as a complete package rather than a placeholder. The README now lists the client SDKs that do exist (TypeScript, Go, Rust).

### Changed
- `README.md`: Removed duplicate, hand-maintained copies of contract error codes, entrypoint signatures, and storage layout tables in favor of authoritative links to `docs/error-codes.md` and `docs/abi.json`. Streamlined the contracts overview and updated PR checklists.
- `services/graphql-api` moves to `graphql-subscriptions` 3.x, the major the graphql 17 upgrade (#984) will need, taken on its own as that issue asks. The one breaking change that applies is the rename of `PubSub.asyncIterator` to `asyncIterableIterator`, which now returns an `AsyncIterableIterator`, and the `iterall` dependency is gone. The resolvers moved into `src/resolvers.ts` so a new test can run them through a real `ApolloServer` and assert that `BAD_USER_INPUT` errors still reach clients with their `extensions.code`. Dependabot now groups `graphql-subscriptions` with `graphql` and `@apollo/server`. graphql itself stays on 16 until both libraries accept 17.
- `governance` and `factory` return a typed `NotInitialized` error instead of trapping when called before `initialize` (#931, #932). Instance configuration is read through shared accessors, so the admin setters, `propose_admin`, pool creation, `propose` and `get_params` all report `GovernanceError::NotInitialized = 35` / `FactoryError::NotInitialized = 12`. Code 2 is already taken in both enums (`InvalidVotingPeriod`, `InvalidFeeBps`), so the new variant is appended and every existing discriminant is unchanged. `governance::get_params` now returns `Result`; its success value is identical, so generated clients keep working.
- Pre-init calls and incentive-campaign failures now return typed errors instead of trapping. `dex_aggregator` (`AggregatorError::NotInitialized = 6`), `concentrated_liquidity` (`ClError::NotInitialized = 24`) and `amm` (`AmmError::NotInitialized = 21`, mirrored in `pool_interfaces` and `SdkAmmError`) report `NotInitialized` when called before `initialize`. `incentive_campaigns` gains `IncentiveError` (19 variants) in place of its `assert!`/`expect` preconditions. Existing discriminants are unchanged. A few views that used to trap now return `Result`: CL `current_tick` / `get_tokens` / `fee_bps`, AMM `get_info` / `get_fee_info` / `shares_of`, aggregator `register_cl_pool` / `remove_cl_pool`, and the incentive `get_*` lookups. Their success value is identical, so cross-contract callers and generated clients keep working. Callers using the `try_` client form see the extra error arm. See `docs/error-codes.md`.
- `soroban_amm_simulator` now builds against `rand` 0.10 and `rand_pcg` 0.10. The migration is two renames in `monte_carlo.rs`: `Rng::gen_range` became `random_range` in 0.9, and the `Rng` extension trait became `RngExt` in 0.10. The 0.10 bump does not move seeded output — the golden-value tests described in the next entry pass unchanged — so it adds nothing to the re-baseline note below.
- `soroban_amm_simulator` Monte Carlo runs are now reproducible. The generator was `SmallRng`, which `rand` documents as non-portable *and* platform-dependent — it selects Xoshiro256 on 64-bit targets and Xoshiro128 on 32-bit ones, so the same seed already produced different results on different machines, and any `rand` release was free to change the algorithm again. It is now `rand_pcg::Pcg64`, a fixed, portable algorithm, and two golden-value tests pin both the raw draw sequence and the full report for a known seed so any future drift fails the build instead of silently moving reported numbers.
- **Re-baseline stored Monte Carlo reports.** Changing the generator changes the draws, so a seeded run produces different sample statistics than it did on the previous release (the distributions are equivalent; the individual numbers move). This is a one-time shift — the point of the change is that it will not happen again unnoticed.

### Fixed
- The Docker image and the release workflow pinned Stellar CLI releases whose locked dependency tree no longer compiles on the pinned Rust 1.98.1. `cargo install stellar-cli --locked` builds the CLI's own `Cargo.lock`, and 25.1.0 (Dockerfile) and 23.0.0 (`release.yml`) lock `ethnum` 1.5.2 and 1.5.0, which fail with `E0512: cannot transmute between types of different sizes`. That failed the `docker-build` CI job on every commit since it was added (#1017), and would have failed the next tagged release at "Install Stellar CLI". Both now use 27.1.0, the version `smoke-test.yml` already uses: the Docker image builds it from source (it locks `ethnum` 1.5.3) and also installs `libdbus-1-dev` and `libudev-dev`, which 27.x needs on Linux, and `release.yml` installs it with the `stellar/stellar-cli` action as `smoke-test.yml` does instead of compiling it. `README.md` lists the same version. With the CLI building, `docker-build` then reached the Dockerfile's dependency-caching `cargo fetch`, which had never run: it copies only the member manifests, and Cargo will not load a manifest whose targets have no source file. That step now creates empty placeholders for each member's targets, fetches, and deletes them in the same layer.
- `soroban_amm_simulator`: the concentrated-liquidity module had no swap engine, and `ClPoolState::current_tick` was hardcoded to 0 by `initialize` whatever the price (#961). `ClPoolState::swap` now ports the contract's tick walk step for step (tick crossing with `liquidity_net`, fee growth and `fee_growth_outside` flips, protocol fee split, price limits, partial fills), `initialize` derives the tick from the price, and a new `initialize_at_tick` mirrors the contract's `initialize`. `tests/cl_swap_parity.rs` runs the real contract beside the simulator on shared fixtures and requires exact agreement after every swap. The sqrt price and current tick are now private (read them with `sqrt_price_x96()` / `current_tick()`) so they can no longer be set out of step with each other, `cl::math::sqrt_price_x96_to_tick` now returns the floor tick as the contract does instead of rounding up between ticks, `ClPoolState::new` rejects `fee_bps = 10_000` as the contract does, and the placeholder `initialize_tick`, which created tick entries the contract can never hold, is removed.
- `concentrated_liquidity`: the position path (mint, single-token mint and its
  quote, `modify_position`, burn, `quote_position`) converted between liquidity
  and token amounts through `mul_u128_u96`, which silently dropped every bit of
  its product above 128. `overflow-checks` did not catch it because the loss
  happened in a `wrapping_shl`. Sqrt-price differences grow exponentially with
  tick magnitude, so ordinary ranges overflowed: for L = 1e6 over
  [100_000, 200_000] the true product is 5.09x `u128::MAX` and the result came
  back 55.7x too small, and a 10^12 deposit there was credited ~1e6 liquidity
  instead of ~1.5e14. `get_amount0_delta`, `get_amount1_delta`,
  `get_liquidity_for_amount0` and `get_liquidity_for_amount1` are now evaluated
  over 256-bit intermediates through the existing `mul_div` and
  `amount{0,1}_delta_exact` helpers, keep rounding down (toward the pool), and
  return `None` instead of a wrapped value when a result cannot be represented.
  The contract reports that as the new `ClError::MathOverflow = 25` (mirrored
  in `pool_interfaces`). `mul_u128_u96` is removed. `amm-fuzz` now checks all
  four conversions against an arbitrary-precision reference (#963).
- `@types/node` matches the Node 22 runtime CI uses. `packages/sdk`,
  `services/graphql-api` and `services/webhook-streamer` type-checked against
  Node 26 typings, so `tsc` accepted APIs that do not exist on Node 22; all
  three are on `^22.20.4` now (webhook-streamer's exact `26.5.0` pin came from
  a Dependabot bump and is replaced by a caret range like the others). `.nvmrc`
  is the single source of the CI Node major, `setup-node` reads it, every
  package in the CI matrix declares `engines.node: ">=22"`, and a new
  `node-versions` CI job (`make check-node-versions`) fails when any package's
  `@types/node` or `engines.node` disagrees with it (#983).
- `@soroban-amm/sdk`: `AmmErrors` / `AmmErrorNames` stopped at code 18, so `decodeError` fell through to a generic error for `AlreadyExecuted` (19), `ProposalExpired` (20) and the new `NotInitialized` (21). All three are now mapped, and a test parses `pub enum AmmError` from `contracts/amm/src/lib.rs` so the TypeScript map can't fall behind the contract again unnoticed.
- `incentive_campaigns`: `recover_leftover_funds` could run more than once on the same campaign. Recovery marks the campaign inactive but leaves `funding_amount - total_distributed` unchanged, so every repeat call transferred the same leftover again, taken from the balance other campaigns hold in the same reward token. A campaign that is already inactive now returns `IncentiveError::CampaignInactive`.
- `concentrated_liquidity`: the swap engine priced every step in a
  3-significant-digit scale (`p_c = (sqrt_price_x96 * 1000) >> 96`) while
  minting, burning and quoting priced at the full `sqrt_price_x96`. One unit of
  that scale is a ~20-tick move, so any trade large enough to change it at all
  moved the pool ~20 ticks regardless of size — a 3,900-token swap against
  L = 5e8 moved the price 20 ticks where the correct answer is a fraction of
  one. The pool price therefore ran away from the price its trades had actually
  paid for and crossed out of live position ranges without the volume to
  justify it. Positions then redeemed against a price the pool had never been
  at: in the reproducer all three positions were valued entirely in token A,
  leaving the pool holding `(a = 0, b = 400225)` against `tokens_owed`
  `(a = 400508, b = 0)` — solvent in aggregate, insolvent per token, with token
  B stranded and unclaimable. `compute_step`, `compute_final_price_and_output`
  and `compute_final_price_and_input` now work entirely in Q64.96 using the
  canonical Uniswap V3 formulas, evaluated through a new 256-bit-intermediate
  `mul_div`/`mul_div_ceil` so products like `liquidity * sqrt_price` no longer
  have to fit in a `u128`. Input rounds up and output rounds down, so a step can
  never move the price further, or pay out more, than the input justifies.
- `concentrated_liquidity`: `amounts_for_liquidity_to_burn` and
  `liquidity_from_amounts` valued in-range positions at
  `tick_to_sqrt_price_x96(current_tick)` — the price at the tick's lower edge —
  rather than the pool's live `SqrtPriceX96`. A tick spans a whole price band
  and every swap moves the price inside it, so an in-range position was split
  between the two tokens at a price the pool was not at, redeeming for more of
  one token than the pool held. This was the residual per-token shortfall left
  after the swap-price fix above (171 units on ~400,000 in the reproducer).
  Both now take the live price, clamped into the position's own band.
- `concentrated_liquidity`: `initialize` never wrote `SqrtPriceX96` — only
  swaps did — so an untraded pool had no stored price and every reader had to
  reconstruct one from the tick. It is now set at initialization; readers keep
  a tick-derived fallback so pools deployed before this change still work.
- `amm-fuzz`: the CL stateful suite now asserts per-token solvency on every
  step (the pool's balances cover every position's burn proceeds plus its
  uncollected fees). The four regression tests that documented the bugs above
  — `cl_burn_all_returns_everything`, `cl_solvency_per_token_regression`,
  `cl_solvency_total_value_regression` and `cl_active_liquidity_regression` —
  were `#[ignore]`d as known failures and now run as ordinary tests.
- CI has been red on `main` since 2026-08-30. The WASM build pulled the off-chain `soroban_amm_simulator` CLI into `cargo build --workspace --target wasm32v1-none`, where its host-only dependencies (clap, csv, rand/getrandom) cannot compile — breaking `build-and-test`, `fuzz` and `release.yml` alike. Which crates are excluded now lives in a single `scripts/build_workspace.sh`, shared by `ci.yml`, `release.yml`, the `Makefile`, `deploy.sh` and `optimize_contracts.sh` (the last two carried the same latent bug), so the five cannot drift apart again.
- The `Makefile` was corrupted by 5529cc1: literal `\t` sequences in place of tabs made it unparseable (`Makefile:37: *** missing separator`), which is why the `fuzz` job died two seconds in. The same commit renamed the target to `fzzz-cl` and broke `MAKEFILE_LIST`, `RUSTDOCFLAGS`, the `*.wasm` glob, and the `optimize`/`audit` recipes. Restored, keeping the intended `release-build` alias.
- `release.yml` was corrupted by the same PR: a step truncated mid-command with a literal `\n` swallowed the `Generate checksums` and `Update changelog` steps, so the workflow failed to parse and every tag push failed instantly. Restored, and given `fetch-depth: 0` since the changelog walks tag history.
- `soroban_amm_simulator` could never load a trade file in JSON. `TradeRecord` deserialized a `#[serde(flatten)]` internally tagged `TradeAction`, and both `flatten` and `tag = "..."` route the value through serde's private `Content` buffer, which rejects 128-bit integers ("i128 is not supported", serde-rs/serde#1183). Deserialization now goes through an explicit flat struct that preserves the documented on-disk format, shared with the CSV loader so both formats keep one `kind` mapping. Covered by a new serialize/load round-trip test.
- The simulator's `cl::math::tick_to_sqrt_price_x96` was a stub that applied only tick bits `0x1` and `0x2`, returning price 1.0 for every other tick and making the CL simulator's output meaningless. Replaced with the full binary-decomposition algorithm ported from `contracts/concentrated_liquidity`, verified to agree bit-for-bit with the on-chain function.
- `soroban_amm_sdk::decode_amm_event` could panic on malformed event data instead of returning `None`: the SDK's tuple `TryFromVal` unpacks through the host's `vec_unpack_to_slice`, which traps on an arity mismatch rather than returning an error, so `.ok()?` never got a chance. All decode branches now check the payload length first, keeping the function total for untrusted RPC input.
- `scripts/size_report.sh` used GNU-only `realpath --relative-to`, aborting the whole script (and `make size`/`make size-check`) on macOS.
- propose_emergency_withdraw: expiry timestamp is now refreshed only when a new approval is recorded, preventing a single signer from keeping a stale proposal alive indefinitely.
- concentrated_liquidity::set_position_nft: changing the NFT contract address now requires the pool to have no tokenized positions, preventing index orphaning and unauthorized control transfer.
- `LpToken::unlock` previously authorised against the currently configured `DataKey::Locker`, so any `set_locker` rotation orphaned LP tokens whose locker had locked them via `LockedVote`. The unlock function now requires auth from the locker that originally locked the tokens, recorded per-locker in a new `LockEntry(Address, Address)` storage entry. Each locker retains authority over its own contribution; a freshly-set locker can only unlock tokens it itself locked. (closes #556)
- `contracts/router/Cargo.toml` and the workspace `Cargo.toml` both contained duplicate table entries (`[dependencies]`, `[dev-dependencies]`, and member list) that caused `cargo` to refuse to load the workspace entirely. Merged into single tables and removed duplicate members.
- `concentrated_liquidity`: two compounding bugs let `current_tick` desync from the discrete `active_liquidity`/`fee_growth_outside` bookkeeping after a swap. First, the tick-crossing target price was derived via a crude reimplementation (`sqrt(tick_to_price(tick))` rescaled) instead of the existing high-precision `tick_to_sqrt_price_x96`, losing enough precision to round-trip to the wrong tick; fixed by reusing `tick_to_sqrt_price_x96` at all 3 call sites. Second, a swap step that stays *within* the current range (not crossing) computes its landing price in the engine's deliberately low-precision (~3 significant digit), pool-favorable-rounded scale, which right at a boundary can be indistinguishable from (or past) the next tick once reconstructed at full precision — resolving `current_tick` past a boundary the discrete bookkeeping never actually crossed. Fixed by clamping that branch's `current_tick` to the known-correct side of the boundary it was told not to reach. Together these caused `active_liquidity()` to diverge from the true sum of in-range positions after a multi-tick swap, and caused positions to lose previously-accrued fees once price crossed out of their range. The originally-suspected nearest-vs-floor rounding difference between `price_to_tick` and `sqrt_price_x96_to_tick` was real but not itself sufficient to reproduce the bug; `price_to_tick` is still removed as a genuine (if secondary) inconsistency. (closes #786, closes #785)
- `concentrated_liquidity`: `burn_position`/`burn_position_by_token_id`/`collect_fees` recomputed principal and fee payouts independently of the swap engine's own (lower-precision) price-stepping math, so a payout could exceed the contract's actual token balance and hard-trap instead of failing gracefully. Burn and fee-collection payouts are now clamped to the contract's real on-hand balance at transfer time, with any shortfall preserved in the position's `tokens_owed` so it remains claimable once the contract's balance recovers, instead of being lost. (closes #787)
- CI installed JavaScript dependencies with `npm install`, which re-resolves the tree on every run and rewrites lockfiles as it sees fit, so a transitive dependency publishing between two runs could turn CI red on an unchanged tree — the exact failure `rust-toolchain.toml` was introduced to eliminate on the Rust side. Three of the seven npm workspaces (`packages/ts-advanced-client`, `services/graphql-api`, `services/health-dashboard`) had no `package-lock.json` at all, so there was no record of what had actually been installed: a supply-chain gap, and most acutely for the deployed `graphql-api` service. All three now commit a lockfile, and `ci.yml` installs with `npm ci`, which installs exactly what the lockfile pins and fails if it disagrees with `package.json`, making lockfile drift visible instead of silent. All seven workspaces were verified to build and pass their CI test command against `npm ci` on Node 22, the same runtime `ci.yml` uses. Dependabot already watches all seven manifests, so pinning does not mean going stale. (closes #956)

### Added
- `router` and `dex_aggregator` gain an admin pause switch: `pause(admin)`, `unpause(admin)` and `is_paused()` (#937, #938). Both check that `admin` is the stored admin (`Unauthorized` otherwise) and require its authorization, and emit versioned `pause` / `unpause` events carrying `(admin,)`. While paused, router `swap_exact_in` / `swap_exact_out` return `RouterError::Paused = 9`, and aggregator `set_max_hops`, `register_cl_pool`, `set_routing_tokens`, `remove_cl_pool`, `execute_route` and `swap_best` return `AggregatorError::Paused = 7`. Quotes and path views stay callable. New codes: `RouterError::Unauthorized = 10`, `AggregatorError::Unauthorized = 8`.
- `LpToken::migrate_legacy_lock(holder, locker, amount)` admin-only helper to migrate a holder's pre-fix `Locked(holder) > 0` balance into per-locker `LockEntry` entries after upgrading from a contract version that tracked only the total `Locked` counter.
- `LpTokenInterface::unlock` now takes an explicit `locker: Address` parameter; `governance::unlock_vote` calls it with `env.current_contract_address()` as the locker.
- `batch_auction`: settlement venues are now validated against a registry instead of trusting a pool's self-reported token pair, closing a fund-drain vector where an order could name an arbitrary attacker-controlled contract as its venue. A venue is accepted only if the admin has explicitly allow-listed it (`add_venue`/`remove_venue`/`is_venue_allowed`/`list_venues`) or the configured factory (`set_factory`) attests to it having deployed that pool for that token pair (using a new `concentrated_liquidity::fee_bps()` getter for CL venues). Venues are re-validated at settlement, not just submission, so one removed in between causes that order to be refunded instead of executed. Also adds a `MAX_ORDER_LIFETIME_SECS` ceiling on trader-supplied deadlines, a permissionless `expire_order`/`get_expired_orders` path so a trader isn't dependent on the full batch window elapsing to reclaim an expired order, and `claim_refund` so a refund/payout whose transfer fails at settlement becomes claimable later instead of being permanently stranded. (closes #700)

### Breaking
- `LpToken::unlock(holder, amount)` is replaced by `LpToken::unlock(holder, locker, amount)`. The previous locker parameter read from `DataKey::Locker` storage is now an explicit argument. External SDK clients bound to the old public ABI must switch to the new signature.

### Legacy
- `contracts/amm/src/lib.rs` references several `DataKey` enum variants that are not declared in the enum on `main` (`FeeBps`, `AccruedFeeA`, `AccruedFeeB`, `FeeRecipient`, `ProtocolFeeBps`, `FlashLoanFeeBps`, `Paused`, `Admin`, `PendingAdmin`). They are unrelated to this fix and are tracked as a separate AMM-compile-blocker issue.
- Governance contract with multi-type parameter voting (`ProposalKind` enum covering Fee, Protocol Fee, Flash Loan Fee, Transfer Admin, Pause, and Unpause), timelocks, quorum requirements, and voting power locks (#137)
- Factory contract for deploying and registering AMM pools, featuring pool count (`get_pool_count`) and paginated pool queries (`get_pools`) (#139)
- Flash loan support with a dedicated update interface (`update_flash_loan_fee`) and configurable fees
- TWAP price accumulators via `get_price_cumulative` and a sample `TwapConsumer` contract
- Protocol fee collection (`set_protocol_fee`, `get_protocol_fee`, `withdraw_protocol_fees`)
- Emergency pause/unpause circuit breakers (`pause`, `unpause`, `is_paused`)
- Post-deployment swap fee adjustment (`update_fee`)
- Two-step administrator transfer (`propose_admin`, `accept_admin`)
- Ledger timestamp `deadline` parameter on `swap`, `swap_exact_out`, `add_liquidity`, and `remove_liquidity` for execution safety
- Detailed swap quotes (`simulate_swap`) including price impact and fee breakdown
- Reverse query quote (`get_amount_in`)
- Python client example (`examples/python/`)
- TS client example (`examples/client/`)
- Reproducible contract build environment with Docker
- Makefile with shortcuts for building, testing, linting, formatting, and end-to-end testing
- Complete machine-readable ABI schema JSON (`docs/abi.json`) (#143)
### Changed
- `reserve_manager` docs clarify the contract is off-chain-only; on-chain AMM hookup is deferred (#518).
