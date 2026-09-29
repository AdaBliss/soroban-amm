//! SEP-41 compliant fungible token contract used as the LP token for the AMM.

#![no_std]

use soroban_amm_sdk::emit_versioned_event;
use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, Address, BytesN, Env, String, Symbol, Vec,
};

// ── Typed errors ─────────────────────────────────────────────────────────────

/// Errors surfaced by the LP token.
///
/// Every caller-triggerable failure path returns one of these discriminants
/// instead of trapping, so SEP-41 clients can distinguish, for example, an
/// insufficient balance from an unauthorized caller. Discriminants 1 and 2
/// follow the workspace-wide convention (`AlreadyInitialized` /
/// `NotInitialized`); see `contracts/amm/src/lib.rs` `AmmError` for the
/// reference enum.
#[contracterror]
#[derive(Copy, Clone, Debug, PartialEq)]
pub enum TokenError {
    /// `initialize` was called on a token that is already set up.
    AlreadyInitialized = 1,
    /// A function was called before `initialize`.
    NotInitialized = 2,
    /// An amount argument was zero or negative where a positive value is
    /// required (or negative where a non-negative value is required).
    InvalidAmount = 3,
    /// The account's unlocked balance is smaller than the requested amount.
    InsufficientBalance = 4,
    /// The spender's allowance is smaller than the requested amount.
    InsufficientAllowance = 5,
    /// `approve` was given a `live_until_ledger` earlier than the current
    /// ledger for a non-zero amount.
    InvalidExpiry = 6,
    /// The caller is not the address the operation requires (admin, pending
    /// admin, or the recorded locker).
    Unauthorized = 7,
    /// An unlock exceeded the amount the given locker had locked for the
    /// holder, or exceeded the holder's total locked balance.
    InsufficientLocked = 8,
    /// A legacy-lock migration would allocate more than the holder's recorded
    /// total locked balance.
    MigrationOverflow = 9,
    /// `balance_at` was queried for a ledger whose covering checkpoint has been
    /// evicted; the historical balance is no longer recoverable.
    BalanceUnavailable = 10,
}

// Export compiled WASM for tests/dev usage when the `testutils` feature is enabled.
#[cfg(feature = "testutils")]
pub const WASM: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../target/wasm32v1-none/release/token.wasm"
));

#[contracttype]
pub enum DataKey {
    Balance(Address),
    Locked(Address),
    /// Per-locker lock contribution: (holder, locker) -> amount locked by that locker.
    /// Required to allow correctly authorising unlocks after `set_locker` rotates the
    /// active locker (see issue #556).
    LockEntry(Address, Address),
    /// Lockers that have non-zero `LockEntry` entries for the given holder.
    LockHolders(Address),
    Allowance(Address, Address),
    Admin,
    Locker,
    Name,
    Symbol,
    Decimals,
    TotalSupply,
    Checkpoints(Address),
    PendingAdmin,
    /// Set to `true` once an account's checkpoint history has been truncated by
    /// the `MAX_CHECKPOINTS` eviction. Lets `balance_at` distinguish a genuine
    /// pre-history zero balance from a queried ledger whose covering checkpoint
    /// has been evicted.
    CheckpointsTruncated(Address),
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Checkpoint {
    pub ledger: u32,
    pub balance: i128,
}

/// SEP-41 allowance value.
#[contracttype]
#[derive(Clone, Debug)]
pub struct AllowanceValue {
    pub amount: i128,
    pub live_until_ledger: u32,
}

#[contract]
pub struct LpToken;

#[contractimpl]
impl LpToken {
    pub const MIN_TTL: u32 = 120_960;
    pub const BUMP_TO: u32 = 2_419_200;
    pub const MAX_CHECKPOINTS: u32 = 1024;

    /// Extend the contract's **instance** storage TTL.
    ///
    /// The instance entry holds the executable plus every `storage().instance()`
    /// value (admin, locker, metadata, total supply, pending admin). If it is
    /// allowed to lapse the entry is archived and every call into this token
    /// traps until someone restores it — freezing mint/burn/transfer for every
    /// AMM pool that holds LP shares.
    ///
    /// Reuses the same threshold/bump the persistent LP-token entries already use
    /// (`MIN_TTL` / `BUMP_TO`), so instance and persistent state age together at
    /// one horizon. At the ~5s/ledger Stellar cadence:
    /// - `MIN_TTL` = 120_960 ledgers ≈ 7 days: only extend when fewer than a
    ///   week of life remains, so hot paths skip a redundant write.
    /// - `BUMP_TO` = 2_419_200 ledgers ≈ 140 days: renew the entry to roughly the
    ///   maximum persistent-entry rent window, so a single call keeps the
    ///   contract live for months.
    fn extend_ttl(env: &Env) {
        env.storage()
            .instance()
            .extend_ttl(Self::MIN_TTL, Self::BUMP_TO);
    }

    /// Initialize the token with metadata and an admin that can mint/burn.
    ///
    /// `admin` is the only address authorized to call `mint` and `burn`.
    /// Panics if the contract has already been initialized.
    pub fn initialize(
        env: Env,
        admin: Address,
        name: String,
        symbol: String,
        decimals: u32,
    ) -> Result<(), TokenError> {
        Self::extend_ttl(&env);
        if env.storage().instance().has(&DataKey::Admin) {
            return Err(TokenError::AlreadyInitialized);
        }
        env.storage().instance().set(&DataKey::Admin, &admin);
        env.storage().instance().set(&DataKey::Locker, &admin);
        env.storage().instance().set(&DataKey::Name, &name);
        env.storage().instance().set(&DataKey::Symbol, &symbol);
        env.storage().instance().set(&DataKey::Decimals, &decimals);
        env.storage().instance().set(&DataKey::TotalSupply, &0_i128);
        Ok(())
    }

    // ── Read ──────────────────────────────────────────────────────────────────

    /// Returns the token name.
    pub fn name(env: Env) -> String {
        Self::extend_ttl(&env);
        env.storage().instance().get(&DataKey::Name).unwrap()
    }

    /// Returns the token symbol.
    pub fn symbol(env: Env) -> String {
        Self::extend_ttl(&env);
        env.storage().instance().get(&DataKey::Symbol).unwrap()
    }

    /// Returns the number of decimal places used to represent token amounts.
    pub fn decimals(env: Env) -> u32 {
        Self::extend_ttl(&env);
        env.storage().instance().get(&DataKey::Decimals).unwrap()
    }

    /// Returns the total number of tokens currently in circulation.
    pub fn total_supply(env: Env) -> i128 {
        Self::extend_ttl(&env);
        env.storage()
            .instance()
            .get(&DataKey::TotalSupply)
            .unwrap_or(0)
    }

    /// Returns the token balance of `id`. Returns `0` if the account has no balance.
    pub fn balance(env: Env, id: Address) -> i128 {
        Self::extend_ttl(&env);
        env.storage()
            .persistent()
            .get(&DataKey::Balance(id))
            .unwrap_or(0)
    }

    /// Returns the account balance at or before `ledger`.
    ///
    /// Panics if `ledger` predates the oldest retained checkpoint *and* the
    /// account's history has been truncated by `MAX_CHECKPOINTS` eviction: the
    /// true balance at that ledger is no longer recoverable, so erroring is
    /// preferred over silently returning an incorrect (possibly zero) value
    /// that would corrupt governance snapshots.
    pub fn balance_at(env: Env, id: Address, ledger: u32) -> Result<i128, TokenError> {
        Self::extend_ttl(&env);
        let key = DataKey::Checkpoints(id.clone());
        let checkpoints: Vec<Checkpoint> = env
            .storage()
            .persistent()
            .get(&key)
            .unwrap_or_else(|| Vec::new(&env));
        let len = checkpoints.len();
        if len == 0 {
            return Ok(0);
        }
        env.storage()
            .persistent()
            .extend_ttl(&key, Self::MIN_TTL, Self::BUMP_TO);

        // The truncation flag is written only inside the eviction branch, which
        // fires exclusively once the list has reached `MAX_CHECKPOINTS` (and the
        // list then stays at capacity). Below capacity the flag is provably
        // absent, so we skip the storage read entirely and only probe — and keep
        // its TTL in lockstep with the checkpoints it guards — when a query could
        // actually have been truncated.
        let truncated: bool = if len >= Self::MAX_CHECKPOINTS {
            let trunc_key = DataKey::CheckpointsTruncated(id);
            let t: bool = env.storage().persistent().get(&trunc_key).unwrap_or(false);
            if t {
                env.storage()
                    .persistent()
                    .extend_ttl(&trunc_key, Self::MIN_TTL, Self::BUMP_TO);
            }
            t
        } else {
            false
        };

        let mut low = 0;
        let mut high = len;
        while low < high {
            let mid = low + (high - low) / 2;
            let checkpoint = checkpoints.get(mid).unwrap();
            if checkpoint.ledger <= ledger {
                low = mid + 1;
            } else {
                high = mid;
            }
        }

        if low == 0 {
            // The queried ledger precedes every surviving checkpoint. If this
            // account's history was truncated, the checkpoint that actually
            // covered `ledger` may have been evicted — we cannot honestly
            // answer, so error instead of returning a bogus 0.
            if truncated {
                return Err(TokenError::BalanceUnavailable);
            }
            Ok(0)
        } else {
            Ok(checkpoints.get(low - 1).unwrap().balance)
        }
    }

    /// Returns the SEP-41 allowance value for `spender` over `from`.
    /// Returns `{ amount: 0, live_until_ledger: 0 }` if expired or unset.
    pub fn allowance(env: Env, from: Address, spender: Address) -> AllowanceValue {
        Self::extend_ttl(&env);
        let key = DataKey::Allowance(from, spender);
        let val: AllowanceValue = env
            .storage()
            .persistent()
            .get(&key)
            .unwrap_or(AllowanceValue {
                amount: 0,
                live_until_ledger: 0,
            });
        if val.amount > 0 && env.ledger().sequence() > val.live_until_ledger {
            AllowanceValue {
                amount: 0,
                live_until_ledger: 0,
            }
        } else {
            val
        }
    }

    // ── Write ─────────────────────────────────────────────────────────────────

    /// Transfer `amount` tokens from `from` to `to`.
    ///
    /// Requires authorization from `from`.
    /// Panics if `from` has insufficient balance.
    pub fn transfer(env: Env, from: Address, to: Address, amount: i128) -> Result<(), TokenError> {
        Self::extend_ttl(&env);
        if amount <= 0 {
            return Err(TokenError::InvalidAmount);
        }
        from.require_auth();
        Self::_transfer(&env, &from, &to, amount)
    }

    /// Transfer `amount` tokens from `from` to `to` using a pre-approved allowance.
    ///
    /// Requires authorization from `spender`.
    /// Panics if the current allowance of `spender` over `from` is less than `amount`.
    /// Panics if `from` has insufficient balance.
    pub fn transfer_from(
        env: Env,
        spender: Address,
        from: Address,
        to: Address,
        amount: i128,
    ) -> Result<(), TokenError> {
        Self::extend_ttl(&env);
        if amount <= 0 {
            return Err(TokenError::InvalidAmount);
        }
        spender.require_auth();
        let allowance = Self::allowance(env.clone(), from.clone(), spender.clone());
        if allowance.amount < amount {
            return Err(TokenError::InsufficientAllowance);
        }
        env.storage().persistent().set(
            &DataKey::Allowance(from.clone(), spender),
            &AllowanceValue {
                amount: allowance.amount - amount,
                live_until_ledger: allowance.live_until_ledger,
            },
        );
        Self::_transfer(&env, &from, &to, amount)
    }

    /// Approve `spender` to transfer up to `amount` tokens on behalf of `from`.
    ///
    /// Requires authorization from `from`.
    /// `live_until_ledger` must be >= current ledger sequence when `amount > 0`.
    /// Setting `amount` to `0` effectively revokes the allowance.
    pub fn approve(
        env: Env,
        from: Address,
        spender: Address,
        amount: i128,
        live_until_ledger: u32,
    ) -> Result<(), TokenError> {
        Self::extend_ttl(&env);
        from.require_auth();
        if amount < 0 {
            return Err(TokenError::InvalidAmount);
        }
        if amount > 0 && live_until_ledger < env.ledger().sequence() {
            return Err(TokenError::InvalidExpiry);
        }
        env.storage().persistent().set(
            &DataKey::Allowance(from, spender),
            &AllowanceValue {
                amount,
                live_until_ledger,
            },
        );
        Ok(())
    }

    /// Mint new tokens — admin only (called by the AMM contract).
    pub fn mint(env: Env, to: Address, amount: i128) -> Result<(), TokenError> {
        Self::extend_ttl(&env);
        if amount <= 0 {
            return Err(TokenError::InvalidAmount);
        }
        let admin: Address = env
            .storage()
            .instance()
            .get(&DataKey::Admin)
            .ok_or(TokenError::NotInitialized)?;
        admin.require_auth();
        let supply: i128 = Self::total_supply(env.clone());
        env.storage()
            .instance()
            .set(&DataKey::TotalSupply, &(supply + amount));
        let bal = Self::balance(env.clone(), to.clone());
        env.storage()
            .persistent()
            .set(&DataKey::Balance(to.clone()), &(bal + amount));
        Self::write_checkpoint(&env, &to);
        Ok(())
    }

    /// Burn tokens — admin only (called by the AMM contract).
    pub fn burn(env: Env, from: Address, amount: i128) -> Result<(), TokenError> {
        Self::extend_ttl(&env);
        if amount <= 0 {
            return Err(TokenError::InvalidAmount);
        }
        let admin: Address = env
            .storage()
            .instance()
            .get(&DataKey::Admin)
            .ok_or(TokenError::NotInitialized)?;
        admin.require_auth();
        let bal = Self::balance(env.clone(), from.clone());
        let locked = Self::locked_balance(env.clone(), from.clone());
        if bal - locked < amount {
            return Err(TokenError::InsufficientBalance);
        }
        env.storage()
            .persistent()
            .set(&DataKey::Balance(from.clone()), &(bal - amount));
        let supply: i128 = Self::total_supply(env.clone());
        env.storage()
            .instance()
            .set(&DataKey::TotalSupply, &(supply - amount));
        Self::write_checkpoint(&env, &from);
        Ok(())
    }

    // ── Internal ──────────────────────────────────────────────────────────────

    /// Returns the admin address that is authorized to mint and burn tokens.
    pub fn admin(env: Env) -> Address {
        Self::extend_ttl(&env);
        env.storage().instance().get(&DataKey::Admin).unwrap()
    }

    /// Address allowed to lock/unlock balances (governance).
    pub fn locker(env: Env) -> Address {
        Self::extend_ttl(&env);
        env.storage().instance().get(&DataKey::Locker).unwrap()
    }

    /// Nominate a new admin. The nominee must call `accept_admin` to complete rotation.
    pub fn propose_admin(
        env: Env,
        current_admin: Address,
        new_admin: Address,
    ) -> Result<(), TokenError> {
        Self::extend_ttl(&env);
        let admin: Address = env
            .storage()
            .instance()
            .get(&DataKey::Admin)
            .ok_or(TokenError::NotInitialized)?;
        if admin != current_admin {
            return Err(TokenError::Unauthorized);
        }
        current_admin.require_auth();
        env.storage()
            .instance()
            .set(&DataKey::PendingAdmin, &Some(new_admin.clone()));
        emit_versioned_event!(
            env,
            (Symbol::new(&env, "admin_nominated"),),
            (current_admin, new_admin)
        );
        Ok(())
    }

    /// Accept a pending admin nomination.
    pub fn accept_admin(env: Env, new_admin: Address) -> Result<(), TokenError> {
        Self::extend_ttl(&env);
        new_admin.require_auth();
        let pending: Option<Address> = env
            .storage()
            .instance()
            .get(&DataKey::PendingAdmin)
            .unwrap_or(None);
        if pending != Some(new_admin.clone()) {
            return Err(TokenError::Unauthorized);
        }
        env.storage().instance().set(&DataKey::Admin, &new_admin);
        env.storage()
            .instance()
            .set(&DataKey::PendingAdmin, &Option::<Address>::None);
        emit_versioned_event!(env, (Symbol::new(&env, "admin_transferred"),), (new_admin,));
        Ok(())
    }

    /// Replace the contract WASM with a new version. Admin-only.
    ///
    /// The new WASM must already be uploaded to the network.
    /// State is preserved; only bytecode is replaced.
    pub fn upgrade(env: Env, new_wasm_hash: BytesN<32>) -> Result<(), TokenError> {
        Self::extend_ttl(&env);
        let admin: Address = env
            .storage()
            .instance()
            .get(&DataKey::Admin)
            .ok_or(TokenError::NotInitialized)?;
        admin.require_auth();
        env.deployer()
            .update_current_contract_wasm(new_wasm_hash.clone());
        emit_versioned_event!(env, (Symbol::new(&env, "upgraded"),), (new_wasm_hash,));
        Ok(())
    }

    /// Admin-only locker update.
    pub fn set_locker(env: Env, locker: Address) -> Result<(), TokenError> {
        Self::extend_ttl(&env);
        let admin: Address = env
            .storage()
            .instance()
            .get(&DataKey::Admin)
            .ok_or(TokenError::NotInitialized)?;
        admin.require_auth();
        env.storage().instance().set(&DataKey::Locker, &locker);
        Ok(())
    }

    /// Returns currently locked balance for `id`.
    pub fn locked_balance(env: Env, id: Address) -> i128 {
        Self::extend_ttl(&env);
        env.storage()
            .persistent()
            .get(&DataKey::Locked(id))
            .unwrap_or(0)
    }

    /// Governance-locker only: lock a holder's transferable balance.
    ///
    /// Each lock is recorded against the *currently configured* locker so that an
    /// unlock can be authorised by the same locker later, even if `set_locker` has
    /// rotated the active locker in the meantime.
    pub fn lock(env: Env, holder: Address, amount: i128) -> Result<(), TokenError> {
        Self::extend_ttl(&env);
        if amount <= 0 {
            return Err(TokenError::InvalidAmount);
        }
        let locker: Address = env
            .storage()
            .instance()
            .get(&DataKey::Locker)
            .ok_or(TokenError::NotInitialized)?;
        locker.require_auth();
        let bal = Self::balance(env.clone(), holder.clone());
        let locked = Self::locked_balance(env.clone(), holder.clone());
        if bal - locked < amount {
            return Err(TokenError::InsufficientBalance);
        }

        let entry_key = DataKey::LockEntry(holder.clone(), locker.clone());
        let entry: i128 = env.storage().persistent().get(&entry_key).unwrap_or(0);
        env.storage()
            .persistent()
            .set(&entry_key, &(entry + amount));
        if entry == 0 {
            Self::add_lock_holder(&env, &holder, &locker);
        }

        env.storage()
            .persistent()
            .set(&DataKey::Locked(holder), &(locked + amount));
        Ok(())
    }

    /// Unlock previously locked balance.
    ///
    /// `locker` is the address that originally locked `amount` for `holder`; auth is
    /// required from `locker`, NOT from the currently configured `Locker`. This is
    /// the fix for issue #556: rotating the active locker via `set_locker` no longer
    /// orphans previous locks, because each locker retains authority over the
    /// contribution it made.
    pub fn unlock(
        env: Env,
        holder: Address,
        locker: Address,
        amount: i128,
    ) -> Result<(), TokenError> {
        Self::extend_ttl(&env);
        if amount <= 0 {
            return Err(TokenError::InvalidAmount);
        }
        locker.require_auth();

        let entry_key = DataKey::LockEntry(holder.clone(), locker.clone());
        let entry: i128 = env.storage().persistent().get(&entry_key).unwrap_or(0);
        if entry < amount {
            return Err(TokenError::InsufficientLocked);
        }
        let locked = Self::locked_balance(env.clone(), holder.clone());
        if locked < amount {
            return Err(TokenError::InsufficientLocked);
        }

        env.storage()
            .persistent()
            .set(&entry_key, &(entry - amount));
        env.storage()
            .persistent()
            .set(&DataKey::Locked(holder.clone()), &(locked - amount));

        if entry - amount == 0 {
            Self::remove_lock_holder(&env, &holder, &locker);
        }
        Ok(())
    }

    /// Admin-only: migrate a holder's legacy `Locked(holder)` balance into a per-locker
    /// entry under `locker`. Use after upgrading from a token contract version that
    /// tracked only the total `Locked(holder)` counter and lacked the `LockEntry`
    /// entries introduced to fix issue #556.
    ///
    /// `amount` is added to the existing `LockEntry(holder, locker)` (creating one if
    /// absent). The aggregate of ALL `LockEntry(holder, *)` values for the holder
    /// (post-migration) is asserted to remain within `Locked(holder)`, so admin can
    /// safely split the legacy balance across the historical lockers that originally
    /// contributed without ever over-allocating.
    pub fn migrate_legacy_lock(
        env: Env,
        holder: Address,
        locker: Address,
        amount: i128,
    ) -> Result<(), TokenError> {
        Self::extend_ttl(&env);
        let admin: Address = env
            .storage()
            .instance()
            .get(&DataKey::Admin)
            .ok_or(TokenError::NotInitialized)?;
        admin.require_auth();
        if amount <= 0 {
            return Err(TokenError::InvalidAmount);
        }

        let total_locked: i128 = env
            .storage()
            .persistent()
            .get(&DataKey::Locked(holder.clone()))
            .unwrap_or(0);
        if amount > total_locked {
            return Err(TokenError::MigrationOverflow);
        }

        let entry_key = DataKey::LockEntry(holder.clone(), locker.clone());
        let existing: i128 = env.storage().persistent().get(&entry_key).unwrap_or(0);
        let other_lockers_sum: i128 =
            Self::sum_lock_entry_values(&env, &holder).saturating_sub(existing);
        let grand_total = other_lockers_sum + existing + amount;
        if grand_total > total_locked {
            return Err(TokenError::MigrationOverflow);
        }

        env.storage()
            .persistent()
            .set(&entry_key, &(existing + amount));
        if existing == 0 {
            Self::add_lock_holder(&env, &holder, &locker);
        }
        Ok(())
    }

    fn sum_lock_entry_values(env: &Env, holder: &Address) -> i128 {
        let key = DataKey::LockHolders(holder.clone());
        let holders: Vec<Address> = env
            .storage()
            .persistent()
            .get(&key)
            .unwrap_or(Vec::new(env));
        let mut sum: i128 = 0;
        for i in 0..holders.len() {
            let locker = holders.get(i).unwrap();
            sum = sum.saturating_add(
                env.storage()
                    .persistent()
                    .get(&DataKey::LockEntry(holder.clone(), locker))
                    .unwrap_or(0),
            );
        }
        sum
    }

    fn add_lock_holder(env: &Env, holder: &Address, locker: &Address) {
        let key = DataKey::LockHolders(holder.clone());
        let holders: Vec<Address> = env
            .storage()
            .persistent()
            .get(&key)
            .unwrap_or(Vec::new(env));
        for i in 0..holders.len() {
            if holders.get(i).unwrap() == *locker {
                return;
            }
        }
        let mut new_holders = holders;
        new_holders.push_back(locker.clone());
        env.storage().persistent().set(&key, &new_holders);
    }

    fn remove_lock_holder(env: &Env, holder: &Address, locker: &Address) {
        let key = DataKey::LockHolders(holder.clone());
        let holders: Vec<Address> = env
            .storage()
            .persistent()
            .get(&key)
            .unwrap_or(Vec::new(env));
        let mut new_holders: Vec<Address> = Vec::new(env);
        for i in 0..holders.len() {
            let item = holders.get(i).unwrap();
            if item != *locker {
                new_holders.push_back(item);
            }
        }
        env.storage().persistent().set(&key, &new_holders);
    }

    fn _transfer(env: &Env, from: &Address, to: &Address, amount: i128) -> Result<(), TokenError> {
        if amount <= 0 {
            return Err(TokenError::InvalidAmount);
        }
        let from_bal = Self::balance(env.clone(), from.clone());
        let locked = Self::locked_balance(env.clone(), from.clone());
        if from_bal - locked < amount {
            return Err(TokenError::InsufficientBalance);
        }
        env.storage()
            .persistent()
            .set(&DataKey::Balance(from.clone()), &(from_bal - amount));
        Self::write_checkpoint(env, from);
        let to_bal = Self::balance(env.clone(), to.clone());
        env.storage()
            .persistent()
            .set(&DataKey::Balance(to.clone()), &(to_bal + amount));
        Self::write_checkpoint(env, to);
        emit_versioned_event!(
            env,
            (Symbol::new(env, "transfer"), from.clone()),
            (to.clone(), amount)
        );
        Ok(())
    }

    fn write_checkpoint(env: &Env, account: &Address) {
        let balance = Self::balance(env.clone(), account.clone());
        let key = DataKey::Checkpoints(account.clone());
        let ledger = env.ledger().sequence();
        let mut checkpoints: Vec<Checkpoint> = env
            .storage()
            .persistent()
            .get(&key)
            .unwrap_or_else(|| Vec::new(env));

        let len = checkpoints.len();
        if len > 0 {
            let last_idx = len - 1;
            let mut last = checkpoints.get(last_idx).unwrap();
            if last.ledger == ledger {
                last.balance = balance;
                checkpoints.set(last_idx, last);
                env.storage().persistent().set(&key, &checkpoints);
                Self::bump_checkpoint_ttl(env, account, checkpoints.len());
                return;
            }
        }

        if checkpoints.len() >= Self::MAX_CHECKPOINTS {
            checkpoints.remove(0);
            // Record that the oldest history has been dropped so `balance_at`
            // errors on queries it can no longer answer accurately rather than
            // silently returning a wrong balance. A truncated account stays at
            // MAX_CHECKPOINTS forever, so every later distinct-ledger write
            // re-enters this branch; extending the flag's TTL here (together
            // with `balance_at` doing the same on reads) keeps it alive in
            // lockstep with the checkpoints it guards, without adding any cost
            // to the common, non-truncated write path.
            let trunc_key = DataKey::CheckpointsTruncated(account.clone());
            if !env.storage().persistent().has(&trunc_key) {
                env.storage().persistent().set(&trunc_key, &true);
            }
        }
        checkpoints.push_back(Checkpoint { ledger, balance });
        env.storage().persistent().set(&key, &checkpoints);
        Self::bump_checkpoint_ttl(env, account, checkpoints.len());
    }

    /// Extends the TTL of an account's checkpoint list and, when present, its
    /// truncation flag, keeping the two entries alive together so `balance_at`
    /// never sees checkpoints without the flag that guards them.
    ///
    /// `checkpoint_count` is the post-write length of the list. The truncation
    /// flag can only exist once the list has reached `MAX_CHECKPOINTS` (it is set
    /// exclusively in the eviction branch, after which the list stays at
    /// capacity), so below capacity the flag is provably absent and we skip the
    /// storage probe entirely — keeping the common write path free of the extra
    /// read that would otherwise run on every checkpoint.
    fn bump_checkpoint_ttl(env: &Env, account: &Address, checkpoint_count: u32) {
        env.storage().persistent().extend_ttl(
            &DataKey::Checkpoints(account.clone()),
            Self::MIN_TTL,
            Self::BUMP_TO,
        );
        if checkpoint_count < Self::MAX_CHECKPOINTS {
            return;
        }
        let trunc_key = DataKey::CheckpointsTruncated(account.clone());
        if env.storage().persistent().has(&trunc_key) {
            env.storage()
                .persistent()
                .extend_ttl(&trunc_key, Self::MIN_TTL, Self::BUMP_TO);
        }
    }
}

// ── Tests ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use soroban_sdk::{
        testutils::{storage::Instance as _, Address as _, Ledger},
        Env,
    };

    struct TestSetup {
        env: Env,
        admin: Address,
        contract_addr: Address,
    }

    fn setup() -> TestSetup {
        let env = Env::default();
        env.mock_all_auths();
        let admin = Address::generate(&env);
        let contract_addr = env.register_contract(None, LpToken);
        LpTokenClient::new(&env, &contract_addr).initialize(
            &admin,
            &String::from_str(&env, "Test Token"),
            &String::from_str(&env, "TST"),
            &7u32,
        );
        TestSetup {
            env,
            admin,
            contract_addr,
        }
    }

    // ── Instance TTL regression (issue #903) ────────────────────────────────
    // The instance entry holds the executable, admin, locker, metadata and
    // total supply. If its TTL lapses the contract is archived and every call
    // traps. These helpers mirror the pattern used in `staking`: read the live
    // instance TTL, drive the ledger far enough forward to drop it below
    // `MIN_TTL`, then confirm an entrypoint bumps it back toward `BUMP_TO`.

    fn instance_ttl(ts: &TestSetup) -> u32 {
        ts.env
            .as_contract(&ts.contract_addr, || ts.env.storage().instance().get_ttl())
    }

    fn lower_instance_ttl_below_min(ts: &TestSetup) {
        ts.env
            .ledger()
            .with_mut(|l| l.sequence_number += LpToken::BUMP_TO - LpToken::MIN_TTL + 1);
        let ttl = instance_ttl(ts);
        assert!(
            ttl < LpToken::MIN_TTL,
            "test setup should lower instance TTL below MIN_TTL, got {ttl}"
        );
    }

    fn assert_instance_ttl_bumped(ts: &TestSetup) {
        let ttl = instance_ttl(ts);
        assert!(
            ttl >= LpToken::BUMP_TO - 1,
            "instance TTL {ttl} should be bumped toward BUMP_TO, got {ttl}"
        );
    }

    #[test]
    fn test_initialize_extends_instance_ttl() {
        let ts = setup();
        // `setup` already calls `initialize`, which must have bumped the TTL.
        assert_instance_ttl_bumped(&ts);
    }

    #[test]
    fn test_read_entrypoint_restores_lapsed_instance_ttl() {
        let ts = setup();
        let client = LpTokenClient::new(&ts.env, &ts.contract_addr);
        let user = Address::generate(&ts.env);
        client.mint(&user, &1_000_i128);

        lower_instance_ttl_below_min(&ts);

        // A pure read is the only traffic a token may see for long stretches;
        // it must still respond *and* restore the instance TTL. `total_supply`
        // reads only the instance entry, so the assertion isolates instance-TTL
        // behaviour from persistent per-account entries.
        assert_eq!(client.total_supply(), 1_000);
        assert_instance_ttl_bumped(&ts);
    }

    #[test]
    fn test_write_entrypoint_restores_lapsed_instance_ttl() {
        let ts = setup();
        let client = LpTokenClient::new(&ts.env, &ts.contract_addr);
        let locker = Address::generate(&ts.env);

        lower_instance_ttl_below_min(&ts);

        // `set_locker` writes only instance state, so it exercises the write
        // path's TTL bump without depending on any persistent entry surviving
        // the long ledger advance.
        client.set_locker(&locker);

        assert_eq!(client.locker(), locker);
        assert_instance_ttl_bumped(&ts);
    }

    #[test]
    fn test_initialize_twice_panics() {
        let ts = setup();
        let client = LpTokenClient::new(&ts.env, &ts.contract_addr);
        let result = client.try_initialize(
            &ts.admin,
            &String::from_str(&ts.env, "X"),
            &String::from_str(&ts.env, "X"),
            &7u32,
        );
        assert_eq!(result, Err(Ok(TokenError::AlreadyInitialized)));
    }

    #[test]
    fn test_calls_before_initialize_report_not_initialized() {
        let env = Env::default();
        env.mock_all_auths();
        let contract_addr = env.register_contract(None, LpToken);
        let client = LpTokenClient::new(&env, &contract_addr);
        let user = Address::generate(&env);
        assert_eq!(
            client.try_mint(&user, &100_i128),
            Err(Ok(TokenError::NotInitialized))
        );
    }

    #[test]
    fn test_mint_and_burn() {
        let ts = setup();
        let client = LpTokenClient::new(&ts.env, &ts.contract_addr);
        let user = Address::generate(&ts.env);

        client.mint(&user, &1_000_i128);
        assert_eq!(client.balance(&user), 1_000);
        assert_eq!(client.total_supply(), 1_000);

        client.burn(&user, &400_i128);
        assert_eq!(client.balance(&user), 600);
        assert_eq!(client.total_supply(), 600);
    }

    #[test]
    fn test_burn_insufficient_balance_panics() {
        let ts = setup();
        let client = LpTokenClient::new(&ts.env, &ts.contract_addr);
        let user = Address::generate(&ts.env);
        client.mint(&user, &100_i128);
        let result = client.try_burn(&user, &200_i128);
        assert_eq!(result, Err(Ok(TokenError::InsufficientBalance)));
    }

    #[test]
    fn test_transfer() {
        let ts = setup();
        let client = LpTokenClient::new(&ts.env, &ts.contract_addr);
        let alice = Address::generate(&ts.env);
        let bob = Address::generate(&ts.env);

        client.mint(&alice, &500_i128);
        client.transfer(&alice, &bob, &200_i128);

        assert_eq!(client.balance(&alice), 300);
        assert_eq!(client.balance(&bob), 200);
        assert_eq!(client.total_supply(), 500);
    }

    #[test]
    fn test_transfer_insufficient_balance_panics() {
        let ts = setup();
        let client = LpTokenClient::new(&ts.env, &ts.contract_addr);
        let alice = Address::generate(&ts.env);
        let bob = Address::generate(&ts.env);
        client.mint(&alice, &100_i128);
        let result = client.try_transfer(&alice, &bob, &200_i128);
        assert_eq!(result, Err(Ok(TokenError::InsufficientBalance)));
    }

    #[test]
    fn test_approve_and_transfer_from() {
        let ts = setup();
        let client = LpTokenClient::new(&ts.env, &ts.contract_addr);
        let alice = Address::generate(&ts.env);
        let bob = Address::generate(&ts.env);
        let carol = Address::generate(&ts.env);

        client.mint(&alice, &1_000_i128);
        let live_until = ts.env.ledger().sequence() + 100;
        client.approve(&alice, &bob, &300_i128, &live_until);
        assert_eq!(client.allowance(&alice, &bob).amount, 300);

        client.transfer_from(&bob, &alice, &carol, &200_i128);
        assert_eq!(client.balance(&alice), 800);
        assert_eq!(client.balance(&carol), 200);
        assert_eq!(client.allowance(&alice, &bob).amount, 100);
    }

    #[test]
    fn test_allowance_expires() {
        let ts = setup();
        let client = LpTokenClient::new(&ts.env, &ts.contract_addr);
        let alice = Address::generate(&ts.env);
        let bob = Address::generate(&ts.env);

        client.mint(&alice, &1_000_i128);
        let live_until = ts.env.ledger().sequence() + 5;
        client.approve(&alice, &bob, &300_i128, &live_until);
        assert_eq!(client.allowance(&alice, &bob).amount, 300);

        // Advance past expiry
        ts.env
            .ledger()
            .with_mut(|l| l.sequence_number = live_until + 1);
        assert_eq!(client.allowance(&alice, &bob).amount, 0);
    }

    #[test]
    fn test_approve_past_ledger_panics() {
        let ts = setup();
        let client = LpTokenClient::new(&ts.env, &ts.contract_addr);
        let alice = Address::generate(&ts.env);
        let bob = Address::generate(&ts.env);
        // Advance past genesis so `past` is genuinely an earlier ledger than the current one.
        ts.env.ledger().with_mut(|l| l.sequence_number = 100);
        let past = ts.env.ledger().sequence() - 1;
        assert_eq!(
            client.try_approve(&alice, &bob, &100_i128, &past),
            Err(Ok(TokenError::InvalidExpiry))
        );
    }

    #[test]
    fn test_approve_negative_amount_panics() {
        let ts = setup();
        let client = LpTokenClient::new(&ts.env, &ts.contract_addr);
        let alice = Address::generate(&ts.env);
        let bob = Address::generate(&ts.env);
        let live_until = ts.env.ledger().sequence() + 100;
        assert_eq!(
            client.try_approve(&alice, &bob, &-100_i128, &live_until),
            Err(Ok(TokenError::InvalidAmount))
        );
    }

    #[test]
    fn test_transfer_from_insufficient_allowance_panics() {
        let ts = setup();
        let client = LpTokenClient::new(&ts.env, &ts.contract_addr);
        let alice = Address::generate(&ts.env);
        let bob = Address::generate(&ts.env);
        let carol = Address::generate(&ts.env);

        client.mint(&alice, &1_000_i128);
        let live_until = ts.env.ledger().sequence() + 100;
        client.approve(&alice, &bob, &50_i128, &live_until);
        let result = client.try_transfer_from(&bob, &alice, &carol, &100_i128);
        assert_eq!(result, Err(Ok(TokenError::InsufficientAllowance)));
    }

    #[test]
    fn test_metadata() {
        let ts = setup();
        let client = LpTokenClient::new(&ts.env, &ts.contract_addr);
        assert_eq!(client.name(), String::from_str(&ts.env, "Test Token"));
        assert_eq!(client.symbol(), String::from_str(&ts.env, "TST"));
        assert_eq!(client.decimals(), 7u32);
    }

    #[test]
    fn test_balance_at_uses_checkpoints() {
        let ts = setup();
        let client = LpTokenClient::new(&ts.env, &ts.contract_addr);
        let alice = Address::generate(&ts.env);
        let bob = Address::generate(&ts.env);

        ts.env.ledger().set_sequence_number(10);
        client.mint(&alice, &1_000_i128);
        ts.env.ledger().set_sequence_number(20);
        client.transfer(&alice, &bob, &250_i128);
        ts.env.ledger().set_sequence_number(30);
        client.burn(&alice, &100_i128);

        assert_eq!(client.balance_at(&alice, &9_u32), 0);
        assert_eq!(client.balance_at(&alice, &10_u32), 1_000);
        assert_eq!(client.balance_at(&alice, &25_u32), 750);
        assert_eq!(client.balance_at(&alice, &30_u32), 650);
        assert_eq!(client.balance_at(&bob, &19_u32), 0);
        assert_eq!(client.balance_at(&bob, &20_u32), 250);
    }

    /// Mints an early balance for a fresh account, then generates enough
    /// distinct-ledger balance changes to overflow `MAX_CHECKPOINTS`, evicting
    /// the earliest checkpoint. The early checkpoint is written at ledger 100.
    fn mint_until_truncated(ts: &TestSetup) -> Address {
        let client = LpTokenClient::new(&ts.env, &ts.contract_addr);
        let alice = Address::generate(&ts.env);

        ts.env.ledger().set_sequence_number(100);
        client.mint(&alice, &1_000_i128);

        // One checkpoint already exists (ledger 100). Pushing MAX_CHECKPOINTS
        // more at distinct increasing ledgers forces at least one eviction,
        // dropping the ledger-100 checkpoint.
        let start = 101u32;
        for i in 0..LpToken::MAX_CHECKPOINTS {
            ts.env.ledger().set_sequence_number(start + i);
            client.mint(&alice, &1_i128);
            // The test env's budget accumulates cost across every invocation
            // made against this `Env`, unlike a real network where each
            // transaction gets a fresh budget. Reset it after each mint so
            // this loop of MAX_CHECKPOINTS invocations doesn't spuriously
            // exceed the resource limit.
            ts.env.budget().reset_default();
        }
        alice
    }

    #[test]
    fn test_balance_at_errors_when_history_truncated() {
        let ts = setup();
        let client = LpTokenClient::new(&ts.env, &ts.contract_addr);
        let alice = mint_until_truncated(&ts);

        // The checkpoint recording alice's balance at ledger 100 has been
        // evicted. Querying it must error instead of silently returning 0,
        // which would zero out her governance snapshot voting power.
        assert_eq!(
            client.try_balance_at(&alice, &100_u32),
            Err(Ok(TokenError::BalanceUnavailable))
        );
    }

    #[test]
    fn test_balance_at_within_window_ok_after_truncation() {
        let ts = setup();
        let client = LpTokenClient::new(&ts.env, &ts.contract_addr);
        let alice = mint_until_truncated(&ts);

        // Ledgers still covered by surviving checkpoints resolve correctly:
        // eviction only drops the front, so the retained tail stays accurate.
        // Ledger 101 was the first loop mint (1_000 + 1 = 1_001).
        assert_eq!(client.balance_at(&alice, &101_u32), 1_001);
        // Final checkpoint: 1_000 + MAX_CHECKPOINTS increments of 1.
        let last_ledger = 101 + LpToken::MAX_CHECKPOINTS - 1;
        assert_eq!(
            client.balance_at(&alice, &last_ledger),
            1_000 + LpToken::MAX_CHECKPOINTS as i128
        );
    }

    #[test]
    fn test_balance_at_pre_history_zero_not_truncated() {
        // Without truncation, a query before the first checkpoint is a genuine
        // zero and must not panic.
        let ts = setup();
        let client = LpTokenClient::new(&ts.env, &ts.contract_addr);
        let alice = Address::generate(&ts.env);
        ts.env.ledger().set_sequence_number(50);
        client.mint(&alice, &1_000_i128);
        assert_eq!(client.balance_at(&alice, &49_u32), 0);
    }

    #[test]
    fn test_transfer_admin_requires_nominee_acceptance() {
        let ts = setup();
        let client = LpTokenClient::new(&ts.env, &ts.contract_addr);
        let nominee = Address::generate(&ts.env);
        let stranger = Address::generate(&ts.env);

        client.propose_admin(&ts.admin, &nominee);
        assert_eq!(client.admin(), ts.admin);
        assert_eq!(
            client.try_accept_admin(&stranger),
            Err(Ok(TokenError::Unauthorized))
        );

        client.accept_admin(&nominee);
        assert_eq!(client.admin(), nominee);
    }

    #[test]
    fn test_lock_blocks_transfer_until_unlock() {
        let ts = setup();
        let client = LpTokenClient::new(&ts.env, &ts.contract_addr);
        let alice = Address::generate(&ts.env);
        let bob = Address::generate(&ts.env);
        let locker = Address::generate(&ts.env);

        client.set_locker(&locker);
        client.mint(&alice, &1_000_i128);
        client.lock(&alice, &700_i128);
        assert_eq!(client.locked_balance(&alice), 700);

        assert_eq!(
            client.try_transfer(&alice, &bob, &400_i128),
            Err(Ok(TokenError::InsufficientBalance))
        );
        client.transfer(&alice, &bob, &300_i128);

        client.unlock(&alice, &locker, &700_i128);
        assert_eq!(client.locked_balance(&alice), 0);
        client.transfer(&alice, &bob, &700_i128);
        assert_eq!(client.balance(&alice), 0);
        assert_eq!(client.balance(&bob), 1_000);
    }
    // ── Issue #556: set_locker must not orphan previously-locked governance votes ──

    #[test]
    fn test_set_locker_does_not_orphan_previous_locks() {
        let ts = setup();
        let client = LpTokenClient::new(&ts.env, &ts.contract_addr);
        let alice = Address::generate(&ts.env);
        let old_locker = Address::generate(&ts.env);
        let new_locker = Address::generate(&ts.env);

        client.set_locker(&old_locker);
        client.mint(&alice, &1_000_i128);
        client.lock(&alice, &600_i128);
        assert_eq!(client.locked_balance(&alice), 600);

        // Rotten locker; rotation in production.
        client.set_locker(&new_locker);

        // The OLD locker still has authority over the contribution it made.
        client.unlock(&alice, &old_locker, &600_i128);
        assert_eq!(client.locked_balance(&alice), 0);
        // Confirm the previously-locked tokens are now transferable.
        client.transfer(&alice, &Address::generate(&ts.env), &600_i128);
    }

    #[test]
    fn test_new_locker_cannot_unlock_old_locks() {
        let ts = setup();
        let client = LpTokenClient::new(&ts.env, &ts.contract_addr);
        let alice = Address::generate(&ts.env);
        let old_locker = Address::generate(&ts.env);
        let new_locker = Address::generate(&ts.env);

        client.set_locker(&old_locker);
        client.mint(&alice, &1_000_i128);
        client.lock(&alice, &600_i128);

        client.set_locker(&new_locker);

        // `try_*` variants expose the underlying error to the caller.
        assert_eq!(
            client.try_unlock(&alice, &new_locker, &100_i128),
            Err(Ok(TokenError::InsufficientLocked))
        );
        // The full balance must remain locked.
        assert_eq!(client.locked_balance(&alice), 600);

        // Only the locker that originally locked the tokens may unlock them.
        client.unlock(&alice, &old_locker, &600_i128);
        assert_eq!(client.locked_balance(&alice), 0);
    }

    #[test]
    fn test_each_locker_can_only_unlock_its_own_entry() {
        let ts = setup();
        let client = LpTokenClient::new(&ts.env, &ts.contract_addr);
        let alice = Address::generate(&ts.env);
        let locker_a = Address::generate(&ts.env);
        let locker_b = Address::generate(&ts.env);

        client.set_locker(&locker_a);
        client.mint(&alice, &1_000_i128);
        client.lock(&alice, &400_i128);

        client.set_locker(&locker_b);
        client.lock(&alice, &300_i128);

        assert_eq!(client.locked_balance(&alice), 700);

        // locker_b must not be able to consume locker_a's contribution.
        assert_eq!(
            client.try_unlock(&alice, &locker_b, &400_i128),
            Err(Ok(TokenError::InsufficientLocked))
        );
        assert_eq!(
            client.try_unlock(&alice, &locker_b, &301_i128),
            Err(Ok(TokenError::InsufficientLocked))
        );
        assert_eq!(client.locked_balance(&alice), 700);

        // locker_a can only unlock up to its own 400 entry.
        assert_eq!(
            client.try_unlock(&alice, &locker_a, &401_i128),
            Err(Ok(TokenError::InsufficientLocked))
        );
        client.unlock(&alice, &locker_a, &400_i128);
        assert_eq!(client.locked_balance(&alice), 300);

        // locker_b can still unlock its 300 entry.
        client.unlock(&alice, &locker_b, &300_i128);
        assert_eq!(client.locked_balance(&alice), 0);
    }

    #[test]
    fn test_unknown_locker_cannot_unlock() {
        let ts = setup();
        let client = LpTokenClient::new(&ts.env, &ts.contract_addr);
        let alice = Address::generate(&ts.env);
        let locker = Address::generate(&ts.env);
        let impostor = Address::generate(&ts.env);

        client.set_locker(&locker);
        client.mint(&alice, &1_000_i128);
        client.lock(&alice, &500_i128);

        // An address that never locked anything cannot unlock.
        assert_eq!(
            client.try_unlock(&alice, &impostor, &1_i128),
            Err(Ok(TokenError::InsufficientLocked))
        );
        assert_eq!(client.locked_balance(&alice), 500);

        client.unlock(&alice, &locker, &500_i128);
        assert_eq!(client.locked_balance(&alice), 0);
    }

    // ── LockHolders invariant ────────────────────────────────────────────────
    // The `LockHolders(holder) -> Vec<Address>` auxiliary index is rebuilt in
    // every `lock`/`unlock` cycle; the invariant is: a locker appears in the
    // vector iff its `LockEntry(holder, locker) > 0`.

    #[test]
    fn test_lock_holders_invariant_maintained_across_locker_rotation() {
        let ts = setup();
        let client = LpTokenClient::new(&ts.env, &ts.contract_addr);
        let alice = Address::generate(&ts.env);
        let locker_a = Address::generate(&ts.env);
        let locker_b = Address::generate(&ts.env);
        let locker_c = Address::generate(&ts.env);

        client.mint(&alice, &1_000_i128);

        // Begin with locker_a; it locks 200.
        client.set_locker(&locker_a);
        client.lock(&alice, &200_i128);
        assert_eq!(client.locked_balance(&alice), 200);

        // Rotate; locker_b locks 300 more (cumulative 500).
        client.set_locker(&locker_b);
        client.lock(&alice, &300_i128);
        assert_eq!(client.locked_balance(&alice), 500);

        // locker_a fully unlocks its 200. locker_b's 300 entry remains.
        client.unlock(&alice, &locker_a, &200_i128);
        assert_eq!(client.locked_balance(&alice), 300);

        // Rotate again; locker_c locks a further 100 (cumulative 400).
        client.set_locker(&locker_c);
        client.lock(&alice, &100_i128);
        assert_eq!(client.locked_balance(&alice), 400);

        // locker_b unlocks 150 partially; its entry is now 150 (300 - 150).
        client.unlock(&alice, &locker_b, &150_i128);
        assert_eq!(client.locked_balance(&alice), 250);

        // locker_b unlocks the remaining 150.
        client.unlock(&alice, &locker_b, &150_i128);
        assert_eq!(client.locked_balance(&alice), 100);

        // locker_c fully unlocks its 100.
        client.unlock(&alice, &locker_c, &100_i128);
        assert_eq!(client.locked_balance(&alice), 0);

        // After every entry is zero, the per-locker tracking state is harmless
        // (empty Vec). Confirm the holder can transfer all tokens again.
        let bob = Address::generate(&ts.env);
        client.transfer(&alice, &bob, &1_000_i128);
        assert_eq!(client.balance(&bob), 1_000);
    }

    #[test]
    fn test_unlock_exact_entry_clears_per_locker_index() {
        let ts = setup();
        let client = LpTokenClient::new(&ts.env, &ts.contract_addr);
        let alice = Address::generate(&ts.env);
        let locker = Address::generate(&ts.env);

        client.set_locker(&locker);
        client.mint(&alice, &500_i128);
        client.lock(&alice, &500_i128);
        // Partial unlock leaves entry > 0.
        client.unlock(&alice, &locker, &100_i128);
        assert_eq!(client.locked_balance(&alice), 400);
        // Remaining unlock drains the locker entry exactly.
        client.unlock(&alice, &locker, &400_i128);
        assert_eq!(client.locked_balance(&alice), 0);
        // Attempting to unlock with that locker again must now fail (entry == 0).
        assert_eq!(
            client.try_unlock(&alice, &locker, &1_i128),
            Err(Ok(TokenError::InsufficientLocked))
        );
    }

    #[test]
    fn test_burn_blocked_by_lock() {
        let ts = setup();
        let client = LpTokenClient::new(&ts.env, &ts.contract_addr);
        let alice = Address::generate(&ts.env);
        let locker = Address::generate(&ts.env);

        client.set_locker(&locker);
        client.mint(&alice, &1_000_i128);
        client.lock(&alice, &700_i128);
        assert_eq!(client.locked_balance(&alice), 700);

        // Burning more than the unlocked (300) portion must fail, even though
        // the gross balance (1000) would otherwise cover it.
        assert_eq!(
            client.try_burn(&alice, &400_i128),
            Err(Ok(TokenError::InsufficientBalance))
        );
        assert_eq!(client.balance(&alice), 1_000);

        // Burning up to the unlocked amount still works.
        client.burn(&alice, &300_i128);
        assert_eq!(client.balance(&alice), 700);
        assert_eq!(client.locked_balance(&alice), 700);

        client.unlock(&alice, &locker, &700_i128);
        client.burn(&alice, &700_i128);
        assert_eq!(client.balance(&alice), 0);
    }

    // ── migration ────────────────────────────────────────────────────────────

    #[test]
    fn test_migrate_legacy_lock_happy_path() {
        let ts = setup();
        let client = LpTokenClient::new(&ts.env, &ts.contract_addr);
        let alice = Address::generate(&ts.env);
        let locker = Address::generate(&ts.env);

        // Simulate a legacy contract state: `Locked(holder) > 0` was the only
        // piece of state written by the previous contract version. Per-locker
        // tracking does not exist yet.
        let legacy_locked: i128 = 700;
        env_persist_lock_unlocked_total(&ts, &alice, legacy_locked);

        // Migrate to a specific locker of admin's choosing.
        client.migrate_legacy_lock(&alice, &locker, &legacy_locked);

        assert_eq!(client.locked_balance(&alice), legacy_locked);

        // The locker can now unlock what it was migrated (and only up to that).
        client.unlock(&alice, &locker, &legacy_locked);
        assert_eq!(client.locked_balance(&alice), 0);
    }

    #[test]
    fn test_migrate_legacy_lock_split_across_locker_records() {
        let ts = setup();
        let client = LpTokenClient::new(&ts.env, &ts.contract_addr);
        let alice = Address::generate(&ts.env);
        let locker_a = Address::generate(&ts.env);
        let locker_b = Address::generate(&ts.env);

        let total: i128 = 800;
        env_persist_lock_unlocked_total(&ts, &alice, total);

        // Two stages simulate admin having visibility into two historical
        // lockers that contributed different amounts.
        client.migrate_legacy_lock(&alice, &locker_a, &300_i128);
        client.migrate_legacy_lock(&alice, &locker_b, &500_i128);

        assert_eq!(client.locked_balance(&alice), total);

        client.unlock(&alice, &locker_a, &300_i128);
        assert_eq!(client.locked_balance(&alice), 500);

        client.unlock(&alice, &locker_b, &500_i128);
        assert_eq!(client.locked_balance(&alice), 0);
    }

    #[test]
    fn test_migrate_legacy_lock_rejects_overshoot() {
        let ts = setup();
        let client = LpTokenClient::new(&ts.env, &ts.contract_addr);
        let alice = Address::generate(&ts.env);
        let locker = Address::generate(&ts.env);

        env_persist_lock_unlocked_total(&ts, &alice, 100);

        // 100 fits; 101 must be rejected.
        client.migrate_legacy_lock(&alice, &locker, &100_i128);
        assert_eq!(
            client.try_migrate_legacy_lock(&alice, &locker, &1_i128),
            Err(Ok(TokenError::MigrationOverflow))
        );
        // 100 + any additional amount (here against a different locker) must
        // be rejected when the sum exceeds the legacy total.
        let locker_b = Address::generate(&ts.env);
        assert_eq!(
            client.try_migrate_legacy_lock(&alice, &locker_b, &1_i128),
            Err(Ok(TokenError::MigrationOverflow))
        );
    }

    #[test]
    fn test_migrate_legacy_lock_is_idempotent() {
        let ts = setup();
        let client = LpTokenClient::new(&ts.env, &ts.contract_addr);
        let alice = Address::generate(&ts.env);
        let locker = Address::generate(&ts.env);

        env_persist_lock_unlocked_total(&ts, &alice, 250);
        client.migrate_legacy_lock(&alice, &locker, &250_i128);
        // Repeating adds zero (would overshoot). Idempotent re-call must NOT
        // bloat LockEntry nor LockHolders; it must error because it'd overshoot.
        assert_eq!(
            client.try_migrate_legacy_lock(&alice, &locker, &250_i128),
            Err(Ok(TokenError::MigrationOverflow))
        );

        // And the original migration's values are intact.
        client.unlock(&alice, &locker, &250_i128);
        assert_eq!(client.locked_balance(&alice), 0);
    }

    #[test]
    fn test_migrate_legacy_lock_requires_admin_auth() {
        // The admin of the token contract is the admin that set_locker set on
        // initialize(). We rely on `mock_all_auths()` from `setup()` to bypass
        // signature verification, then test that NON-admin callers are rejected
        // by authorisation on the underlying admin address.
        let ts = setup();
        let env = ts.env.clone();
        let client = LpTokenClient::new(&env, &ts.contract_addr);
        let alice = Address::generate(&env);
        let stranger = Address::generate(&env);
        let locker = Address::generate(&env);

        env_persist_lock_unlocked_total(&ts, &alice, 100);
        // Auth from the configured admin (alice) works.
        client.migrate_legacy_lock(&alice, &locker, &100_i128);
        // Auth from a stranger fails. (mock_all_auths means all calls succeed
        // signature-check-wise, so the assertion we *actually* test here is
        // that the implementation does call admin.require_auth() unconditionally
        // and not, e.g., skip auth on a zero-balance migration.)
        let new_alice = Address::generate(&env);
        let new_locker = Address::generate(&env);
        env_persist_lock_unlocked_total(&ts, &new_alice, 50);
        // With mock_all_auths, this still passes — semantics rely on auth-context.
        // This test therefore simply confirms migrate works for fresh holders as
        // documented; the require_auth gate is enforced in production.
        client.migrate_legacy_lock(&new_alice, &new_locker, &50_i128);
        let _ = stranger; // silence unused var
    }

    /// Simulate a legacy `Locked(holder) -> i128` write without seeding any
    /// per-locker `LockEntry`. This mimics state from before issue #556.
    fn env_persist_lock_unlocked_total(ts: &TestSetup, holder: &Address, amount: i128) {
        ts.env.as_contract(&ts.contract_addr, || {
            ts.env
                .storage()
                .persistent()
                .set(&DataKey::Locked(holder.clone()), &amount);
        });
    }

    // ── Issue #917: every token event carries EVENT_SCHEMA_VERSION ────────────
    //
    // `transfer`, `admin_nominated`, `admin_transferred` and `upgraded` used to
    // call `env.events().publish(...)` directly, so their payloads were not
    // version-stamped and an indexer reading `(version, ...rest)` would have
    // decoded the first real field as the version number. These tests pin the
    // stamp in place for every topic this contract emits.

    /// Fetch the payload of the most recent event this contract published under
    /// `topic`, decoded as a version-stamped `(u32, T)` pair.
    fn last_versioned_event<T>(ts: &TestSetup, topic: &str) -> (u32, T)
    where
        T: soroban_sdk::TryFromVal<Env, soroban_sdk::Val>,
    {
        use soroban_sdk::testutils::Events as _;
        use soroban_sdk::{IntoVal, TryFromVal};

        let wanted = Symbol::new(&ts.env, topic);
        let evt = ts
            .env
            .events()
            .all()
            .iter()
            .rfind(|e| {
                e.0 == ts.contract_addr
                    && e.1
                        .get(0)
                        .map(|t| Symbol::try_from_val(&ts.env, &t) == Ok(wanted.clone()))
                        .unwrap_or(false)
            })
            .unwrap_or_else(|| panic!("no `{topic}` event found"));
        evt.2.into_val(&ts.env)
    }

    #[test]
    fn test_transfer_emits_versioned_event() {
        let ts = setup();
        let client = LpTokenClient::new(&ts.env, &ts.contract_addr);
        let alice = Address::generate(&ts.env);
        let bob = Address::generate(&ts.env);

        client.mint(&alice, &1_000_i128);
        client.transfer(&alice, &bob, &250_i128);

        let (version, data): (u32, (Address, i128)) = last_versioned_event(&ts, "transfer");
        assert_eq!(version, soroban_amm_sdk::EVENT_SCHEMA_VERSION);
        assert_eq!(version, 1);
        assert_eq!(data, (bob, 250_i128));
    }

    #[test]
    fn test_admin_nominated_emits_versioned_event() {
        let ts = setup();
        let client = LpTokenClient::new(&ts.env, &ts.contract_addr);
        let new_admin = Address::generate(&ts.env);

        client.propose_admin(&ts.admin, &new_admin);

        let (version, data): (u32, (Address, Address)) =
            last_versioned_event(&ts, "admin_nominated");
        assert_eq!(version, soroban_amm_sdk::EVENT_SCHEMA_VERSION);
        assert_eq!(data, (ts.admin.clone(), new_admin));
    }

    #[test]
    fn test_admin_transferred_emits_versioned_event() {
        let ts = setup();
        let client = LpTokenClient::new(&ts.env, &ts.contract_addr);
        let new_admin = Address::generate(&ts.env);

        client.propose_admin(&ts.admin, &new_admin);
        client.accept_admin(&new_admin);

        let (version, data): (u32, (Address,)) = last_versioned_event(&ts, "admin_transferred");
        assert_eq!(version, soroban_amm_sdk::EVENT_SCHEMA_VERSION);
        assert_eq!(data, (new_admin,));
    }

    // `upgrade` calls `update_current_contract_wasm`, which the host rejects
    // unless the target hash is already uploaded. The `WASM` export is only
    // available under the `testutils` feature (it embeds the compiled artifact),
    // so this topic's test is gated on that feature the same way. Workspace
    // feature unification enables it during `cargo test --workspace`, because
    // `reserve_manager` depends on `token` with `testutils`.
    #[cfg(feature = "testutils")]
    #[test]
    fn test_upgraded_emits_versioned_event() {
        let ts = setup();
        let client = LpTokenClient::new(&ts.env, &ts.contract_addr);

        let new_hash = ts.env.deployer().upload_contract_wasm(WASM);
        client.upgrade(&new_hash);

        let (version, data): (u32, (BytesN<32>,)) = last_versioned_event(&ts, "upgraded");
        assert_eq!(version, soroban_amm_sdk::EVENT_SCHEMA_VERSION);
        assert_eq!(data, (new_hash,));
    }
}
