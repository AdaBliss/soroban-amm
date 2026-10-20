# Token Storage TTL Policy

This document describes the Time-To-Live (TTL) management policy for the Soroban LP Token contract (`contracts/token`).

## Overview

Soroban persistent storage entries are archived when their TTL expires. When an entry is archived, transactions that require access to it fail until the entry is restored via a restore footprint transaction.

To ensure that LP token holders do not encounter unexpected archiving during typical usage, the token contract automatically extends the TTL of all persistent storage entries upon both reads and writes. Furthermore, a permissionless entrypoint `bump_account` allows dormant holders or external keepers to refresh an account's state before TTL expiration.

## TTL Thresholds and Constants

The token contract defines two constants for persistent entry TTL management:

- `MIN_TTL = 120_960` ledgers (~7 days assuming 5 seconds per ledger). If an entry's remaining TTL is less than `MIN_TTL`, it is bumped.
- `BUMP_TO = 2_419_200` ledgers (~140 days). The TTL is extended to this target duration.

Instance storage is bumped to identical thresholds (`MIN_TTL = 120_960`, `BUMP_TO = 2_419_200`) across all public functions to keep the contract instance and administrative state alive.

## Covered Persistent Keys

The token contract manages the following persistent storage entries under `DataKey`:

| Key Variant | Description | Read Bump | Write Bump |
|---|---|---|---|
| `Balance(Address)` | Current liquid LP token balance of a holder | Yes (`balance`, `balance_at`) | Yes (`mint`, `burn`, `_transfer`) |
| `Allowance(Address, Address)` | Allowance granted from owner to spender | Yes (`allowance` if active) | Yes (`approve`, `transfer_from`) |
| `Locked(Address)` | Total amount of LP tokens locked across all lockers | Yes (`locked_balance`) | Yes (`lock`, `unlock`, `migrate_legacy_lock`) |
| `LockEntry(Address, Address)` | LP tokens locked by a specific locker for a holder | No (internal lookup) | Yes (`lock`, `unlock`, `migrate_legacy_lock`) |
| `LockHolders(Address)` | List of lockers currently locking tokens for a holder | No (internal lookup) | Yes (`add_lock_holder`, `remove_lock_holder`) |
| `Checkpoints(Address)` | Historical balance checkpoints for snapshotting | Yes (`balance_at`) | Yes (`mint`, `burn`, `_transfer`) |
| `CheckpointsTruncated(Address)` | Truncation flag for checkpoint history | Yes (`balance_at`) | Yes (`truncate_checkpoints`) |

## Read, Write, and Refresh Behavior

1. **Write Paths**:
   Whenever a persistent key is written or modified, `env.storage().persistent().extend_ttl(&key, MIN_TTL, BUMP_TO)` is invoked.
   When an allowance or lock balance drops to zero (or an allowance expires), the corresponding key is deleted (`remove(&key)`) to conserve ledger storage.

2. **Read Paths**:
   Read entrypoints (`balance`, `allowance`, `locked_balance`, `balance_at`) perform a safe check with `env.storage().persistent().has(&key)`.
   If the entry exists, `extend_ttl` is called to refresh its lifetime to `BUMP_TO`. If the key does not exist, the TTL extension is safely skipped to prevent Soroban host traps on non-existent keys.

3. **Allowance Expiry and Cleanup**:
   When an allowance's `live_until_ledger` is strictly less than current ledger sequence (`env.ledger().sequence()`):
   - `allowance(from, spender)` returns `{ amount: 0, live_until_ledger: 0 }` and deletes the expired `Allowance` entry.
   - `transfer_from(spender, from, to, amount)` returns `TokenError::InsufficientAllowance` and removes the stale entry.

4. **Permissionless Account Renewal (`bump_account`)**:
   `bump_account(id: Address)` is a permissionless entrypoint that any holder or keeper bot can call. It extends the TTL for:
   - `Balance(id)`
   - `Locked(id)`
   - `LockHolders(id)`
   - Each `LockEntry(id, locker)` for all lockers listed in `LockHolders(id)`
   - `Checkpoints(id)` and `CheckpointsTruncated(id)`

   If any of these keys are not present (e.g. account has no locks or no checkpoints), the bump for that key is safely skipped without trapping.

## Checkpoints vs. Balances

Prior to this policy, `Checkpoints` had TTL extensions while `Balance` entries did not, which caused `balance_at` to resolve while `balance` could fail due to archived balance entries. Under this policy, both `Balance` and `Checkpoints` are kept in sync with identical `MIN_TTL` and `BUMP_TO` parameters, preventing desynchronization between historical and live token balances.
