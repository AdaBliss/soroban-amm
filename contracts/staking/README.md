# Staking Contract

Located in [src/lib.rs](src/lib.rs).

An LP staking and rewards contract. Liquidity providers stake their LP tokens to
earn a separate reward token, distributed through a rewards-per-share accumulator
(the MasterChef pattern) so each claim is computed in O(1).

Stakers may optionally lock their stake for a fixed duration to receive a boost
multiplier on their reward share, modelled on Curve's veToken design. The boost
is applied to an *effective* staked amount used only for reward math; the actual
LP token balance is never altered. Boost scales linearly with the remaining lock
time between a configurable minimum (1x) and maximum, and locks can only be
extended, never shortened.

## How it fits into the protocol

The contract is standalone and composable. It holds two token addresses: an LP
token (staked in) and a reward token (paid out). The LP token can be the fungible
LP token minted by any V2 AMM pool, and the reward token can be any SEP-41 token,
so a protocol can incentivise liquidity in a specific pool without changes to the
AMM itself. The admin funds and distributes rewards; stakers stake, claim, and
unstake independently.

## Public functions

### Setup

| Function | Description |
|---|---|
| `initialize(lp_token, reward_token, admin)` | One-time setup with default boost and lock-duration bounds. Takes no `require_auth`; verify the result with `get_config` / `get_admin`. Rejects `lp_token == reward_token` with `IdenticalTokens` |
| `initialize_with_boost_config(lp_token, reward_token, admin, min_boost_scaled, max_boost_scaled, min_lock_duration_secs, max_lock_duration_secs)` | One-time setup with custom boost multipliers and lock-duration bounds; same checks as `initialize` |

### Staking and locking

| Function | Description |
|---|---|
| `stake(staker, amount)` | Stake LP tokens with no lock (1x boost) |
| `stake_locked(staker, amount, lock_duration_secs)` | Stake LP tokens with an optional lock duration for a boost multiplier |
| `lock(staker, amount, lock_duration_seconds)` | Escrow LP tokens for a fixed lock duration at a boosted reward rate |
| `extend_lock(staker, new_duration_seconds)` | Extend an existing lock forward in time only |
| `unlock(staker) → (amount, rewards)` | Withdraw the full staked amount and accrued rewards after the lock expires |
| `unstake(staker, amount) → (amount, rewards)` | Unstake LP tokens and claim pending rewards; panics if the lock has not expired |

### Rewards

| Function | Description |
|---|---|
| `add_rewards(admin, amount)` | Transfer reward tokens into the pool; admin only |
| `update_rewards(admin, new_rewards)` | Distribute new rewards across all stakers via the accumulator; admin only |
| `reconcile_reward_accounting(admin, offset, limit, done)` | Repair legacy reward accounting on a deployed pool (see issue #1045); admin only, paginated |
| `claim(staker) → rewards` | Claim accrued rewards without unstaking |
| `pending_rewards(staker) → i128` | Read a staker's unclaimed rewards |

### Views

| Function | Description |
|---|---|
| `get_pool_info() → PoolInfo` | Read pool state: token addresses, admin, total effective staked, undistributed reward balance, and accumulated rewards per share |
| `get_owed_rewards() → i128` | Read rewards already assigned to stakers but not yet paid out (#1045) |
| `get_config() → StakingConfig` | Read back the stored tokens, admin and boost / lock bounds. Deploy tooling compares this with what it meant to set (#1114) |
| `get_admin() → Address` | Read the current admin |
| `get_staker_info(staker) → StakerInfo` | Read a staker's raw and effective amounts, rewards debt, lock expiry, and boost multiplier |
| `get_locked_position(staker) → LockedPosition` | Read a staker's locked amount, lock expiry, and boost multiplier |

### Keepers

| Function | Description |
|---|---|
| `list_expired_boosts(offset, limit) → Vec<Address>` | Scan one page of the staker registry for expired-but-unsettled boosts; `limit` is clamped to 100 |
| `settle_boost(staker)` / `settle_boost_batch(stakers)` | Permissionlessly drop an expired boost (batch of at most 50). While the pool is paused no reward is paid: the pending reward is carried in the rewards debt and stays claimable after `unpause` (#1114) |
| `register_existing_stakers(stakers)` | Backfill pre-upgrade stakers into the registry (at most 50, idempotent) |
| `migrate_staker_index(offset, limit) → u32` | Move the pre-#1113 staker vector into the registry, one page at a time (see below) |

### Admin

| Function | Description |
|---|---|
| `upgrade(admin, new_wasm_hash)` | Replace the contract WASM in place; `admin` must be the stored admin and sign. All staking state is preserved. See the [deployment runbook](../../docs/deployment-runbook.md#55-governance-concentrated-liquidity-oracle-aggregator-and-staking) |

## Staker registry and scaling (#1113)

Keeper functions (`list_expired_boosts`, `settle_boost_batch`,
`reconcile_reward_accounting`) need to enumerate stakers. The registry that backs
them lives in persistent storage, one entry per fact, so no operation depends on
how many stakers there are:

| Key | Value |
|---|---|
| `StakerCount` | number of registered stakers `n` |
| `StakerAt(i)`, `i < n` | the i-th staker |
| `StakerSlot(address)` | position of that staker in `StakerAt` |

- `stake` appends: it writes `StakerAt(n)`, `StakerSlot` and `StakerCount`.
- A full `unstake` or `emergency_withdraw` swap-removes: the last entry moves
  into the freed slot, then the tail is deleted. Order is not preserved.
- Every touched entry gets its TTL bumped, including the entries a page read
  visits, so a registry that keepers scan does not expire.
- The instance entry holds no per-staker data, so loading it costs the same with
  5 stakers as with 5,000. The older design kept every staker in one `Vec` in
  instance storage, which made each `stake` rewrite the whole vector and would
  eventually have hit the ledger entry size limit.

Cost of one `stake`, `unstake` or `emergency_withdraw` is constant (tests compare
the 5th and the 500th staker and require the cost within 10%). Cost of a keeper
call is proportional to its page size, which is capped (100 for listing, 50 for
batches).

`ReconcileState` and `ReconcileTotal`, used by `reconcile_reward_accounting`, are
persistent too. The total is removed when a run finalizes, as are the final page's
checkpoints; entries written by earlier pages carry a run id and are ignored by
later runs.

### Migrating a pool deployed before #1113

Until migrated, the old `StakerIndex` vector is still read: the keeper functions
scan the registry first and then the legacy entries that are not registered yet,
so no pre-upgrade staker is missed. New stakers go to the registry directly.

1. Upgrade the contract.
2. Call `migrate_staker_index(0, 50)`. It returns the number of legacy entries
   still to process.
3. Call it again with `offset` equal to the previous `offset + limit` until it
   returns 0. The last page deletes the legacy vector and emits
   `staker_index_migrated`.

Anyone can run the migration. Pages must be consecutive: a call whose `offset`
is not the next unprocessed position fails with `MigrationOutOfOrder`, so nobody
can delete the vector by calling its last page first. Entries whose balance is
already zero are skipped. While the migration is running, the offsets keeper
functions accept index the combined view (registry, then unmigrated legacy
entries), so a keeper that pages through during a migration should restart from
offset 0 when it finishes.

## Initialization is verifiable (#1114)

`initialize` takes no `require_auth` (the admin may be a contract that cannot sign
for itself), so whoever calls it first on a fresh deployment chooses the tokens and
the admin. To detect that, `scripts/deploy/staking.sh` reads `get_admin` and
`get_config` back after initialization and aborts on any difference. Both
initializers also emit an `initialized` event carrying the stored configuration.
