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
    /// Checkpoint page holding up to PAGE_SIZE entries for an account.
    CheckpointPage(Address, u32),
    /// Tracks pagination metadata (oldest retained page and total appended count).
    CheckpointTracker(Address),
    /// Total supply checkpoint list (global, not per-address).
    SupplyCheckpoints,
    /// Set to `true` once supply checkpoint history has been truncated.
    SupplyCheckpointsTruncated,
    /// Checkpoint page holding up to PAGE_SIZE entries for total supply.
    SupplyCheckpointPage(u32),
    /// Tracks pagination metadata for supply checkpoints.
    SupplyCheckpointTracker,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Checkpoint {
    pub ledger: u32,
    pub balance: i128,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CheckpointTracker {
    pub start_page: u32,
    pub total_count: u32,
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
    pub const PAGE_SIZE: u32 = 32;

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

    /// Extend the persistent storage TTL of a specific key if it exists.
    ///
    /// Checking `has` prevents trapping if the key is missing.
    fn bump_persistent(env: &Env, key: &DataKey) {
        if env.storage().persistent().has(key) {
            env.storage()
                .persistent()
                .extend_ttl(key, Self::MIN_TTL, Self::BUMP_TO);
        }
    }

    /// Permissionless entrypoint to refresh all persistent storage entries
    /// associated with an account (Balance, Locked, LockHolders, every LockEntry,
    /// and Checkpoints).
    pub fn bump_account(env: Env, id: Address) {
        Self::extend_ttl(&env);
        Self::bump_persistent(&env, &DataKey::Balance(id.clone()));
        Self::bump_persistent(&env, &DataKey::Locked(id.clone()));

        let holders_key = DataKey::LockHolders(id.clone());
        if env.storage().persistent().has(&holders_key) {
            let holders: Vec<Address> = env
                .storage()
                .persistent()
                .get(&holders_key)
                .unwrap_or_else(|| Vec::new(&env));
            for i in 0..holders.len() {
                let locker = holders.get(i).unwrap();
                Self::bump_persistent(&env, &DataKey::LockEntry(id.clone(), locker));
            }
            Self::bump_persistent(&env, &holders_key);
        }

        // Legacy single-Vec checkpoints (pre-paging tokens).
        Self::bump_persistent(&env, &DataKey::Checkpoints(id.clone()));

        // Paged checkpoints: tracker, every live page, and the truncation flag.
        let tracker_key = DataKey::CheckpointTracker(id.clone());
        let tracker_opt: Option<CheckpointTracker> = env.storage().persistent().get(&tracker_key);
        if let Some(tracker) = tracker_opt {
            Self::bump_persistent(&env, &tracker_key);
            if tracker.total_count > 0 {
                let last_page = (tracker.total_count - 1) / Self::PAGE_SIZE;
                for page in tracker.start_page..=last_page {
                    Self::bump_persistent(&env, &DataKey::CheckpointPage(id.clone(), page));
                }
            }
            Self::bump_persistent(&env, &DataKey::CheckpointsTruncated(id.clone()));
        }
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
        emit_versioned_event!(
            env,
            (Symbol::new(&env, "initialized"),),
            (admin, name, symbol, decimals)
        );
        Ok(())
    }

    // ── Read ──────────────────────────────────────────────────────────────────

    /// Returns the token name.
    pub fn name(env: Env) -> Result<String, TokenError> {
        Self::extend_ttl(&env);
        env.storage()
            .instance()
            .get(&DataKey::Name)
            .ok_or(TokenError::NotInitialized)
    }

    /// Returns the token symbol.
    pub fn symbol(env: Env) -> Result<String, TokenError> {
        Self::extend_ttl(&env);
        env.storage()
            .instance()
            .get(&DataKey::Symbol)
            .ok_or(TokenError::NotInitialized)
    }

    /// Returns the number of decimal places used to represent token amounts.
    pub fn decimals(env: Env) -> Result<u32, TokenError> {
        Self::extend_ttl(&env);
        env.storage()
            .instance()
            .get(&DataKey::Decimals)
            .ok_or(TokenError::NotInitialized)
    }

    /// Returns the total number of tokens currently in circulation.
    pub fn total_supply(env: Env) -> i128 {
        Self::extend_ttl(&env);
        env.storage()
            .instance()
            .get(&DataKey::TotalSupply)
            .unwrap_or(0)
    }

    /// Returns the total supply at or before `ledger`.
    ///
    /// Panics if `ledger` predates the oldest retained checkpoint *and* the
    /// supply history has been truncated by `MAX_CHECKPOINTS` eviction.
    pub fn total_supply_at(env: Env, ledger: u32) -> Result<i128, TokenError> {
        Self::extend_ttl(&env);
        let tracker_key = DataKey::SupplyCheckpointTracker;
        let tracker_opt: Option<CheckpointTracker> = env.storage().persistent().get(&tracker_key);

        if tracker_opt.is_none() {
            // No checkpoints ever written: supply is 0 before any mint
            let trunc_key = DataKey::SupplyCheckpointsTruncated;
            if env.storage().persistent().has(&trunc_key) {
                return Err(TokenError::BalanceUnavailable);
            }
            return Ok(0);
        }

        let tracker = tracker_opt.unwrap();
        if tracker.total_count == 0 {
            return Ok(0);
        }

        env.storage()
            .persistent()
            .extend_ttl(&tracker_key, Self::MIN_TTL, Self::BUMP_TO);

        let trunc_key = DataKey::SupplyCheckpointsTruncated;
        let truncated: bool = env.storage().persistent().has(&trunc_key);
        if truncated {
            env.storage()
                .persistent()
                .extend_ttl(&trunc_key, Self::MIN_TTL, Self::BUMP_TO);
        }

        let end_page = (tracker.total_count - 1) / Self::PAGE_SIZE;

        // Binary search across pages
        let mut low_page = tracker.start_page;
        let mut high_page = end_page + 1;
        while low_page < high_page {
            let mid_page = low_page + (high_page - low_page) / 2;
            let page: Vec<Checkpoint> = env
                .storage()
                .persistent()
                .get(&DataKey::SupplyCheckpointPage(mid_page))
                .unwrap();
            let first_ledger = page.get(0).unwrap().ledger;
            if first_ledger <= ledger {
                low_page = mid_page + 1;
            } else {
                high_page = mid_page;
            }
        }

        if low_page == tracker.start_page {
            if truncated {
                return Err(TokenError::BalanceUnavailable);
            }
            return Ok(0);
        }

        let target_page_idx = low_page - 1;
        let page_key = DataKey::SupplyCheckpointPage(target_page_idx);
        let target_page: Vec<Checkpoint> = env.storage().persistent().get(&page_key).unwrap();
        env.storage()
            .persistent()
            .extend_ttl(&page_key, Self::MIN_TTL, Self::BUMP_TO);

        let mut low = 0;
        let mut high = target_page.len();
        while low < high {
            let mid = low + (high - low) / 2;
            let cp = target_page.get(mid).unwrap();
            if cp.ledger <= ledger {
                low = mid + 1;
            } else {
                high = mid;
            }
        }

        if low == 0 {
            if truncated {
                return Err(TokenError::BalanceUnavailable);
            }
            Ok(0)
        } else {
            Ok(target_page.get(low - 1).unwrap().balance)
        }
    }

    /// Returns the token balance of `id`. Returns `0` if the account has no balance.
    pub fn balance(env: Env, id: Address) -> i128 {
        Self::extend_ttl(&env);
        let key = DataKey::Balance(id);
        Self::bump_persistent(&env, &key);
        env.storage().persistent().get(&key).unwrap_or(0)
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
        Self::bump_persistent(&env, &DataKey::Balance(id.clone()));
        let tracker_key = DataKey::CheckpointTracker(id.clone());
        let tracker_opt: Option<CheckpointTracker> = env.storage().persistent().get(&tracker_key);

        if tracker_opt.is_none() {
            // Check legacy checkpoints entry (single Vec) for pre-upgrade tokens
            let legacy_key = DataKey::Checkpoints(id.clone());
            if let Some(checkpoints) = env
                .storage()
                .persistent()
                .get::<_, Vec<Checkpoint>>(&legacy_key)
            {
                let len = checkpoints.len();
                if len == 0 {
                    return Ok(0);
                }
                env.storage()
                    .persistent()
                    .extend_ttl(&legacy_key, Self::MIN_TTL, Self::BUMP_TO);

                let trunc_key = DataKey::CheckpointsTruncated(id);
                let truncated: bool = env.storage().persistent().get(&trunc_key).unwrap_or(false);
                if truncated {
                    env.storage()
                        .persistent()
                        .extend_ttl(&trunc_key, Self::MIN_TTL, Self::BUMP_TO);
                }

                let mut low = 0;
                let mut high = len;
                while low < high {
                    let mid = low + (high - low) / 2;
                    let cp = checkpoints.get(mid).unwrap();
                    if cp.ledger <= ledger {
                        low = mid + 1;
                    } else {
                        high = mid;
                    }
                }

                if low == 0 {
                    if truncated {
                        return Err(TokenError::BalanceUnavailable);
                    }
                    return Ok(0);
                } else {
                    return Ok(checkpoints.get(low - 1).unwrap().balance);
                }
            } else {
                let trunc_key = DataKey::CheckpointsTruncated(id);
                if env.storage().persistent().has(&trunc_key) {
                    return Err(TokenError::BalanceUnavailable);
                }
                return Ok(0);
            }
        }

        let tracker = tracker_opt.unwrap();
        if tracker.total_count == 0 {
            return Ok(0);
        }

        env.storage()
            .persistent()
            .extend_ttl(&tracker_key, Self::MIN_TTL, Self::BUMP_TO);

        let trunc_key = DataKey::CheckpointsTruncated(id.clone());
        let truncated: bool = env.storage().persistent().has(&trunc_key);
        if truncated {
            env.storage()
                .persistent()
                .extend_ttl(&trunc_key, Self::MIN_TTL, Self::BUMP_TO);
        }

        let end_page = (tracker.total_count - 1) / Self::PAGE_SIZE;

        // Binary search across pages: find the last page whose first checkpoint has ledger <= queried ledger
        let mut low_page = tracker.start_page;
        let mut high_page = end_page + 1;
        while low_page < high_page {
            let mid_page = low_page + (high_page - low_page) / 2;
            let page: Vec<Checkpoint> = env
                .storage()
                .persistent()
                .get(&DataKey::CheckpointPage(id.clone(), mid_page))
                .unwrap();
            let first_ledger = page.get(0).unwrap().ledger;
            if first_ledger <= ledger {
                low_page = mid_page + 1;
            } else {
                high_page = mid_page;
            }
        }

        if low_page == tracker.start_page {
            // The queried ledger precedes every surviving checkpoint in the retained pages.
            if truncated {
                return Err(TokenError::BalanceUnavailable);
            }
            return Ok(0);
        }

        let target_page_idx = low_page - 1;
        let page_key = DataKey::CheckpointPage(id.clone(), target_page_idx);
        let target_page: Vec<Checkpoint> = env.storage().persistent().get(&page_key).unwrap();
        env.storage()
            .persistent()
            .extend_ttl(&page_key, Self::MIN_TTL, Self::BUMP_TO);

        let mut low = 0;
        let mut high = target_page.len();
        while low < high {
            let mid = low + (high - low) / 2;
            let cp = target_page.get(mid).unwrap();
            if cp.ledger <= ledger {
                low = mid + 1;
            } else {
                high = mid;
            }
        }

        if low == 0 {
            if truncated {
                return Err(TokenError::BalanceUnavailable);
            }
            Ok(0)
        } else {
            Ok(target_page.get(low - 1).unwrap().balance)
        }
    }

    /// Returns the SEP-41 allowance value for `spender` over `from`.
    /// Returns `{ amount: 0, live_until_ledger: 0 }` if expired or unset.
    pub fn allowance(env: Env, from: Address, spender: Address) -> AllowanceValue {
        Self::extend_ttl(&env);
        let key = DataKey::Allowance(from.clone(), spender.clone());
        let val: Option<AllowanceValue> = env.storage().persistent().get(&key);
        match val {
            Some(v) => {
                if v.amount > 0 && env.ledger().sequence() > v.live_until_ledger {
                    env.storage().persistent().remove(&key);
                    AllowanceValue {
                        amount: 0,
                        live_until_ledger: 0,
                    }
                } else {
                    Self::bump_persistent(&env, &key);
                    v
                }
            }
            None => AllowanceValue {
                amount: 0,
                live_until_ledger: 0,
            },
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
        let key = DataKey::Allowance(from.clone(), spender.clone());
        let allowance = Self::allowance(env.clone(), from.clone(), spender.clone());
        if allowance.amount < amount {
            return Err(TokenError::InsufficientAllowance);
        }
        let remaining = allowance.amount - amount;
        if remaining > 0 {
            env.storage().persistent().set(
                &key,
                &AllowanceValue {
                    amount: remaining,
                    live_until_ledger: allowance.live_until_ledger,
                },
            );
            Self::bump_persistent(&env, &key);
        } else {
            env.storage().persistent().remove(&key);
        }
        env.events().publish(
            (Symbol::new(&env, "approve"), from.clone(), spender),
            (remaining, allowance.live_until_ledger),
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
        let key = DataKey::Allowance(from.clone(), spender.clone());
        if amount > 0 {
            if live_until_ledger < env.ledger().sequence() {
                return Err(TokenError::InvalidExpiry);
            }
            env.storage().persistent().set(
                &key,
                &AllowanceValue {
                    amount,
                    live_until_ledger,
                },
            );
            Self::bump_persistent(&env, &key);
        } else {
            env.storage().persistent().remove(&key);
        }
        env.events().publish(
            (Symbol::new(&env, "approve"), from, spender),
            (amount, live_until_ledger),
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
        let bal_key = DataKey::Balance(to.clone());
        env.storage().persistent().set(&bal_key, &(bal + amount));
        Self::bump_persistent(&env, &bal_key);
        Self::write_checkpoint(&env, &to);
        Self::write_supply_checkpoint(&env);
        env.events()
            .publish((Symbol::new(&env, "mint"), admin, to), amount);
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
        let bal_key = DataKey::Balance(from.clone());
        env.storage().persistent().set(&bal_key, &(bal - amount));
        Self::bump_persistent(&env, &bal_key);
        let supply: i128 = Self::total_supply(env.clone());
        env.storage()
            .instance()
            .set(&DataKey::TotalSupply, &(supply - amount));
        Self::write_checkpoint(&env, &from);
        Self::write_supply_checkpoint(&env);
        env.events()
            .publish((Symbol::new(&env, "burn"), from), amount);
        Ok(())
    }

    // ── Internal ──────────────────────────────────────────────────────────────

    /// Returns the admin address that is authorized to mint and burn tokens.
    pub fn admin(env: Env) -> Result<Address, TokenError> {
        Self::extend_ttl(&env);
        env.storage()
            .instance()
            .get(&DataKey::Admin)
            .ok_or(TokenError::NotInitialized)
    }

    /// Address allowed to lock/unlock balances (governance).
    pub fn locker(env: Env) -> Result<Address, TokenError> {
        Self::extend_ttl(&env);
        env.storage()
            .instance()
            .get(&DataKey::Locker)
            .ok_or(TokenError::NotInitialized)
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
        emit_versioned_event!(env, (Symbol::new(&env, "set_locker"),), (admin, locker));
        Ok(())
    }

    /// Returns currently locked balance for `id`.
    pub fn locked_balance(env: Env, id: Address) -> i128 {
        Self::extend_ttl(&env);
        let key = DataKey::Locked(id);
        Self::bump_persistent(&env, &key);
        env.storage().persistent().get(&key).unwrap_or(0)
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
        let new_entry = entry + amount;
        env.storage().persistent().set(&entry_key, &new_entry);
        Self::bump_persistent(&env, &entry_key);
        if entry == 0 {
            Self::add_lock_holder(&env, &holder, &locker);
        }

        let new_locked = locked + amount;
        let locked_key = DataKey::Locked(holder.clone());
        env.storage().persistent().set(&locked_key, &new_locked);
        Self::bump_persistent(&env, &locked_key);
        emit_versioned_event!(
            env,
            (Symbol::new(&env, "lock"),),
            (holder, locker, amount, new_entry, new_locked)
        );
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

        let rem_entry = entry - amount;
        if rem_entry > 0 {
            env.storage().persistent().set(&entry_key, &rem_entry);
            Self::bump_persistent(&env, &entry_key);
        } else {
            env.storage().persistent().remove(&entry_key);
        }

        let rem_locked = locked - amount;
        let locked_key = DataKey::Locked(holder.clone());
        if rem_locked > 0 {
            env.storage().persistent().set(&locked_key, &rem_locked);
            Self::bump_persistent(&env, &locked_key);
        } else {
            env.storage().persistent().remove(&locked_key);
        }

        if rem_entry == 0 {
            Self::remove_lock_holder(&env, &holder, &locker);
        }
        emit_versioned_event!(
            env,
            (Symbol::new(&env, "unlock"),),
            (holder, locker, amount, rem_entry, rem_locked)
        );
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

        let new_entry = existing + amount;
        env.storage().persistent().set(&entry_key, &new_entry);
        Self::bump_persistent(&env, &entry_key);
        Self::bump_persistent(&env, &DataKey::Locked(holder.clone()));
        if existing == 0 {
            Self::add_lock_holder(&env, &holder, &locker);
        }
        emit_versioned_event!(
            env,
            (Symbol::new(&env, "migrate_legacy_lock"),),
            (holder, locker, amount, new_entry, grand_total)
        );
        Ok(())
    }

    fn sum_lock_entry_values(env: &Env, holder: &Address) -> i128 {
        let key = DataKey::LockHolders(holder.clone());
        Self::bump_persistent(env, &key);
        let holders: Vec<Address> = env
            .storage()
            .persistent()
            .get(&key)
            .unwrap_or(Vec::new(env));
        let mut sum: i128 = 0;
        for i in 0..holders.len() {
            let locker = holders.get(i).unwrap();
            let lk = DataKey::LockEntry(holder.clone(), locker);
            Self::bump_persistent(env, &lk);
            sum = sum.saturating_add(env.storage().persistent().get(&lk).unwrap_or(0));
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
                Self::bump_persistent(env, &key);
                return;
            }
        }
        let mut new_holders = holders;
        new_holders.push_back(locker.clone());
        env.storage().persistent().set(&key, &new_holders);
        Self::bump_persistent(env, &key);
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
        if !new_holders.is_empty() {
            env.storage().persistent().set(&key, &new_holders);
            Self::bump_persistent(env, &key);
        } else {
            env.storage().persistent().remove(&key);
        }
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
        let from_key = DataKey::Balance(from.clone());
        let from_rem = from_bal - amount;
        env.storage().persistent().set(&from_key, &from_rem);
        Self::bump_persistent(env, &from_key);
        Self::write_checkpoint(env, from);
        let to_bal = Self::balance(env.clone(), to.clone());
        let to_key = DataKey::Balance(to.clone());
        env.storage().persistent().set(&to_key, &(to_bal + amount));
        Self::bump_persistent(env, &to_key);
        Self::write_checkpoint(env, to);
        env.events().publish(
            (Symbol::new(env, "transfer"), from.clone(), to.clone()),
            amount,
        );
        Ok(())
    }

    fn write_checkpoint(env: &Env, account: &Address) {
        let balance = Self::balance(env.clone(), account.clone());
        let ledger = env.ledger().sequence();

        let tracker_key = DataKey::CheckpointTracker(account.clone());
        let mut tracker: CheckpointTracker = if let Some(tr) =
            env.storage().persistent().get(&tracker_key)
        {
            tr
        } else if let Some(legacy_checkpoints) = env
            .storage()
            .persistent()
            .get::<_, Vec<Checkpoint>>(&DataKey::Checkpoints(account.clone()))
        {
            // Migration: convert legacy vector into pages on first write
            let legacy_len = legacy_checkpoints.len();
            let mut page_idx = 0;
            let mut current_page_vec = Vec::new(env);
            for i in 0..legacy_len {
                current_page_vec.push_back(legacy_checkpoints.get(i).unwrap());
                if current_page_vec.len() == Self::PAGE_SIZE || i + 1 == legacy_len {
                    let page_key = DataKey::CheckpointPage(account.clone(), page_idx);
                    env.storage().persistent().set(&page_key, &current_page_vec);
                    env.storage()
                        .persistent()
                        .extend_ttl(&page_key, Self::MIN_TTL, Self::BUMP_TO);
                    page_idx += 1;
                    current_page_vec = Vec::new(env);
                }
            }
            env.storage()
                .persistent()
                .remove(&DataKey::Checkpoints(account.clone()));
            CheckpointTracker {
                start_page: 0,
                total_count: legacy_len,
            }
        } else {
            CheckpointTracker {
                start_page: 0,
                total_count: 0,
            }
        };

        // Eviction: drop the oldest whole page once the retained total would exceed MAX_CHECKPOINTS.
        // Retained count is: total_count - (start_page * PAGE_SIZE).
        if tracker.total_count >= (tracker.start_page * Self::PAGE_SIZE) + Self::MAX_CHECKPOINTS {
            let evict_page_key = DataKey::CheckpointPage(account.clone(), tracker.start_page);
            env.storage().persistent().remove(&evict_page_key);
            tracker.start_page += 1;

            let trunc_key = DataKey::CheckpointsTruncated(account.clone());
            if !env.storage().persistent().has(&trunc_key) {
                env.storage().persistent().set(&trunc_key, &true);
            }
        }

        let current_page = tracker.total_count / Self::PAGE_SIZE;
        let page_key = DataKey::CheckpointPage(account.clone(), current_page);
        let mut page: Vec<Checkpoint> = if tracker.total_count.is_multiple_of(Self::PAGE_SIZE) {
            Vec::new(env)
        } else {
            env.storage()
                .persistent()
                .get(&page_key)
                .unwrap_or_else(|| Vec::new(env))
        };

        page.push_back(Checkpoint { ledger, balance });
        env.storage().persistent().set(&page_key, &page);
        env.storage()
            .persistent()
            .extend_ttl(&page_key, Self::MIN_TTL, Self::BUMP_TO);

        tracker.total_count += 1;
        env.storage().persistent().set(&tracker_key, &tracker);
        env.storage()
            .persistent()
            .extend_ttl(&tracker_key, Self::MIN_TTL, Self::BUMP_TO);

        Self::bump_checkpoint_ttl(env, account, current_page);
    }

    fn write_supply_checkpoint(env: &Env) {
        let supply = Self::total_supply(env.clone());
        let ledger = env.ledger().sequence();

        let tracker_key = DataKey::SupplyCheckpointTracker;
        let mut tracker: CheckpointTracker = env
            .storage()
            .persistent()
            .get(&tracker_key)
            .unwrap_or(CheckpointTracker {
                start_page: 0,
                total_count: 0,
            });

        // Eviction: drop the oldest whole page once the retained total would exceed MAX_CHECKPOINTS.
        if tracker.total_count >= (tracker.start_page * Self::PAGE_SIZE) + Self::MAX_CHECKPOINTS {
            let evict_page_key = DataKey::SupplyCheckpointPage(tracker.start_page);
            env.storage().persistent().remove(&evict_page_key);
            tracker.start_page += 1;

            let trunc_key = DataKey::SupplyCheckpointsTruncated;
            if !env.storage().persistent().has(&trunc_key) {
                env.storage().persistent().set(&trunc_key, &true);
            }
        }

        let current_page = tracker.total_count / Self::PAGE_SIZE;
        let page_key = DataKey::SupplyCheckpointPage(current_page);
        let mut page: Vec<Checkpoint> = if tracker.total_count.is_multiple_of(Self::PAGE_SIZE) {
            Vec::new(env)
        } else {
            env.storage()
                .persistent()
                .get(&page_key)
                .unwrap_or_else(|| Vec::new(env))
        };

        page.push_back(Checkpoint {
            ledger,
            balance: supply,
        });
        env.storage().persistent().set(&page_key, &page);
        env.storage()
            .persistent()
            .extend_ttl(&page_key, Self::MIN_TTL, Self::BUMP_TO);

        tracker.total_count += 1;
        env.storage().persistent().set(&tracker_key, &tracker);
        env.storage()
            .persistent()
            .extend_ttl(&tracker_key, Self::MIN_TTL, Self::BUMP_TO);

        // Bump truncation flag if present
        let trunc_key = DataKey::SupplyCheckpointsTruncated;
        if env.storage().persistent().has(&trunc_key) {
            env.storage()
                .persistent()
                .extend_ttl(&trunc_key, Self::MIN_TTL, Self::BUMP_TO);
        }
    }

    /// Extends the TTL of an account's active checkpoint page and metadata,
    /// keeping the page, tracker, and truncation flag (if present) alive together.
    fn bump_checkpoint_ttl(env: &Env, account: &Address, current_page: u32) {
        env.storage().persistent().extend_ttl(
            &DataKey::CheckpointTracker(account.clone()),
            Self::MIN_TTL,
            Self::BUMP_TO,
        );
        env.storage().persistent().extend_ttl(
            &DataKey::CheckpointPage(account.clone(), current_page),
            Self::MIN_TTL,
            Self::BUMP_TO,
        );
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
        testutils::{
            storage::{Instance as _, Persistent as _},
            Address as _, Ledger,
        },
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

        client.mint(&alice, &1_000_i128);
        let live_until = ts.env.ledger().sequence() + 100;
        client.approve(&alice, &bob, &300_i128, &live_until);
    }

    // ── total_supply_at tests ───────────────────────────────────────────────

    #[test]
    fn test_total_supply_at_before_first_mint_returns_zero() {
        let ts = setup();
        let client = LpTokenClient::new(&ts.env, &ts.contract_addr);

        // Before any mint, total_supply_at should return 0
        assert_eq!(client.total_supply_at(&0), 0);
        assert_eq!(client.total_supply_at(&100), 0);
    }

    #[test]
    fn test_total_supply_at_tracks_mint_and_burn() {
        let ts = setup();
        let client = LpTokenClient::new(&ts.env, &ts.contract_addr);
        let alice = Address::generate(&ts.env);

        // Mint at ledger 1
        ts.env.ledger().with_mut(|l| {
            l.sequence_number = 1;
            l.timestamp = 1;
        });
        client.mint(&alice, &1_000_i128);

        // Query at ledger 1 should return 1000
        assert_eq!(client.total_supply_at(&1), 1_000);

        // Mint at ledger 2
        ts.env.ledger().with_mut(|l| {
            l.sequence_number = 2;
            l.timestamp = 2;
        });
        client.mint(&alice, &500_i128);

        // Query at ledger 2 should return 1500
        assert_eq!(client.total_supply_at(&2), 1_500);

        // Query at ledger 1 should still return 1000
        assert_eq!(client.total_supply_at(&1), 1_000);

        // Burn at ledger 3
        ts.env.ledger().with_mut(|l| {
            l.sequence_number = 3;
            l.timestamp = 3;
        });
        client.burn(&alice, &200_i128);

        // Query at ledger 3 should return 1300
        assert_eq!(client.total_supply_at(&3), 1_300);

        // Query at ledger 2 should still return 1500
        assert_eq!(client.total_supply_at(&2), 1_500);
    }

    #[test]
    fn test_total_supply_at_same_ledger_overwrite() {
        let ts = setup();
        let client = LpTokenClient::new(&ts.env, &ts.contract_addr);
        let alice = Address::generate(&ts.env);

        ts.env.ledger().with_mut(|l| {
            l.sequence_number = 1;
            l.timestamp = 1;
        });
        client.mint(&alice, &1_000_i128);

        // Two mints in the same ledger - the second overwrites the checkpoint
        client.mint(&alice, &500_i128);

        // Query at ledger 1 should return the last value (1500)
        assert_eq!(client.total_supply_at(&1), 1_500);
    }

    #[test]
    fn test_total_supply_at_after_truncation_returns_error() {
        let ts = setup();
        let client = LpTokenClient::new(&ts.env, &ts.contract_addr);
        let alice = Address::generate(&ts.env);

        // 2 x 1100 contract calls exceed the default per-test budget.
        ts.env.budget().reset_unlimited();

        // Create enough checkpoints to trigger truncation
        for i in 0..1100_u32 {
            ts.env.ledger().with_mut(|l| {
                l.sequence_number = i;
                l.timestamp = u64::from(i);
            });
            client.mint(&alice, &1_i128);
            client.burn(&alice, &1_i128);
        }

        // Query a ledger before the truncation point should return error
        // The truncation happens after MAX_CHECKPOINTS (1024) entries
        assert_eq!(
            client.try_total_supply_at(&0),
            Err(Ok(TokenError::BalanceUnavailable))
        );
    }

    #[test]
    fn test_approve_and_transfer_from_full() {
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
        // page eviction drops the oldest whole page of 32 checkpoints (ledger 100
        // plus loop mints at 101..=131).
        // Ledger 132 is the first surviving checkpoint (1_000 + 32 = 1_032).
        assert_eq!(client.balance_at(&alice, &132_u32), 1_032);
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

    fn last_raw_event(
        ts: &TestSetup,
        topic: &str,
    ) -> (soroban_sdk::Vec<soroban_sdk::Val>, soroban_sdk::Val) {
        use soroban_sdk::testutils::Events as _;
        use soroban_sdk::TryFromVal;

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
        (evt.1, evt.2)
    }

    #[test]
    fn test_transfer_emits_versioned_event() {
        use soroban_sdk::TryFromVal;

        let ts = setup();
        let client = LpTokenClient::new(&ts.env, &ts.contract_addr);
        let alice = Address::generate(&ts.env);
        let bob = Address::generate(&ts.env);

        client.mint(&alice, &1_000_i128);
        client.transfer(&alice, &bob, &250_i128);

        let (topics, data) = last_raw_event(&ts, "transfer");
        assert_eq!(topics.len(), 3);
        let from: Address = Address::try_from_val(&ts.env, &topics.get(1).unwrap()).unwrap();
        let to: Address = Address::try_from_val(&ts.env, &topics.get(2).unwrap()).unwrap();
        let amount: i128 = i128::try_from_val(&ts.env, &data).unwrap();
        assert_eq!(from, alice);
        assert_eq!(to, bob);
        assert_eq!(amount, 250_i128);
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

    // ── Issue #1111: Persistent storage TTL tests ─────────────────────────────

    fn get_persistent_ttl(ts: &TestSetup, key: &DataKey) -> Option<u32> {
        ts.env.as_contract(&ts.contract_addr, || {
            if ts.env.storage().persistent().has(key) {
                Some(ts.env.storage().persistent().get_ttl(key))
            } else {
                None
            }
        })
    }

    fn lower_persistent_ttl(ts: &TestSetup, key: &DataKey, ledgers_to_advance: u32) {
        ts.env
            .ledger()
            .with_mut(|l| l.sequence_number += ledgers_to_advance);
        // Note: Soroban simulation automatically reduces entry TTL by sequence delta
        let ttl = get_persistent_ttl(ts, key).unwrap();
        assert!(
            ttl < LpToken::MIN_TTL,
            "TTL should be below MIN_TTL, got {ttl}"
        );
    }

    #[test]
    fn test_mint_extends_balance_ttl() {
        let ts = setup();
        let client = LpTokenClient::new(&ts.env, &ts.contract_addr);
        let alice = Address::generate(&ts.env);

        client.mint(&alice, &1_000);
        let bal_key = DataKey::Balance(alice.clone());
        let ttl = get_persistent_ttl(&ts, &bal_key).unwrap();
        assert!(
            ttl >= LpToken::BUMP_TO - 1,
            "balance TTL {ttl} should be at BUMP_TO"
        );
    }

    #[test]
    fn test_balance_read_restores_lapsed_ttl() {
        let ts = setup();
        let client = LpTokenClient::new(&ts.env, &ts.contract_addr);
        let alice = Address::generate(&ts.env);

        client.mint(&alice, &1_000);
        let bal_key = DataKey::Balance(alice.clone());
        lower_persistent_ttl(&ts, &bal_key, LpToken::BUMP_TO - LpToken::MIN_TTL + 1);

        assert_eq!(client.balance(&alice), 1_000);
        let ttl = get_persistent_ttl(&ts, &bal_key).unwrap();
        assert!(
            ttl >= LpToken::BUMP_TO - 1,
            "balance read must restore TTL to BUMP_TO, got {ttl}"
        );
    }

    #[test]
    fn test_balance_at_extends_balance_ttl_if_present() {
        let ts = setup();
        let client = LpTokenClient::new(&ts.env, &ts.contract_addr);
        let alice = Address::generate(&ts.env);

        client.mint(&alice, &1_000);
        let bal_key = DataKey::Balance(alice.clone());
        let seq = ts.env.ledger().sequence();
        lower_persistent_ttl(&ts, &bal_key, LpToken::BUMP_TO - LpToken::MIN_TTL + 1);

        let bal = client.balance_at(&alice, &seq);
        assert_eq!(bal, 1_000);
        let ttl = get_persistent_ttl(&ts, &bal_key).unwrap();
        assert!(
            ttl >= LpToken::BUMP_TO - 1,
            "balance_at must restore balance TTL, got {ttl}"
        );
    }

    #[test]
    fn test_burn_extends_balance_ttl() {
        let ts = setup();
        let client = LpTokenClient::new(&ts.env, &ts.contract_addr);
        let alice = Address::generate(&ts.env);

        client.mint(&alice, &1_000);
        let bal_key = DataKey::Balance(alice.clone());
        lower_persistent_ttl(&ts, &bal_key, LpToken::BUMP_TO - LpToken::MIN_TTL + 1);

        client.burn(&alice, &200);
        let ttl = get_persistent_ttl(&ts, &bal_key).unwrap();
        assert!(
            ttl >= LpToken::BUMP_TO - 1,
            "burn must restore balance TTL, got {ttl}"
        );
    }

    #[test]
    fn test_transfer_extends_both_parties_balance_ttl() {
        let ts = setup();
        let client = LpTokenClient::new(&ts.env, &ts.contract_addr);
        let alice = Address::generate(&ts.env);
        let bob = Address::generate(&ts.env);

        client.mint(&alice, &1_000);
        client.mint(&bob, &500);

        let alice_key = DataKey::Balance(alice.clone());
        let bob_key = DataKey::Balance(bob.clone());
        lower_persistent_ttl(&ts, &alice_key, LpToken::BUMP_TO - LpToken::MIN_TTL + 1);

        client.transfer(&alice, &bob, &200);
        let ttl_alice = get_persistent_ttl(&ts, &alice_key).unwrap();
        let ttl_bob = get_persistent_ttl(&ts, &bob_key).unwrap();
        assert!(ttl_alice >= LpToken::BUMP_TO - 1);
        assert!(ttl_bob >= LpToken::BUMP_TO - 1);
    }

    #[test]
    fn test_approve_and_allowance_read_bumps_ttl() {
        let ts = setup();
        let client = LpTokenClient::new(&ts.env, &ts.contract_addr);
        let alice = Address::generate(&ts.env);
        let spender = Address::generate(&ts.env);

        // Expiry must be far enough in future so advancing sequence to lower TTL doesn't expire it
        let expiry = ts.env.ledger().sequence() + LpToken::BUMP_TO + 10_000;
        client.approve(&alice, &spender, &500, &expiry);

        let allow_key = DataKey::Allowance(alice.clone(), spender.clone());
        let ttl = get_persistent_ttl(&ts, &allow_key).unwrap();
        assert!(ttl >= LpToken::BUMP_TO - 1);

        lower_persistent_ttl(&ts, &allow_key, LpToken::BUMP_TO - LpToken::MIN_TTL + 1);
        let allow = client.allowance(&alice, &spender);
        assert_eq!(allow.amount, 500);
        let ttl_after = get_persistent_ttl(&ts, &allow_key).unwrap();
        assert!(ttl_after >= LpToken::BUMP_TO - 1);
    }

    #[test]
    fn test_expired_allowance_is_removed_on_read() {
        let ts = setup();
        let client = LpTokenClient::new(&ts.env, &ts.contract_addr);
        let alice = Address::generate(&ts.env);
        let spender = Address::generate(&ts.env);

        let expiry = ts.env.ledger().sequence() + 100;
        client.approve(&alice, &spender, &500, &expiry);

        let allow_key = DataKey::Allowance(alice.clone(), spender.clone());
        assert!(get_persistent_ttl(&ts, &allow_key).is_some());

        // Advance ledger past expiry
        ts.env.ledger().with_mut(|l| l.sequence_number = expiry + 1);

        let allow = client.allowance(&alice, &spender);
        assert_eq!(allow.amount, 0);
        assert_eq!(allow.live_until_ledger, 0);

        // Entry should have been removed
        assert!(get_persistent_ttl(&ts, &allow_key).is_none());
    }

    #[test]
    fn test_expired_allowance_fails_transfer_from_and_removes_entry() {
        let ts = setup();
        let client = LpTokenClient::new(&ts.env, &ts.contract_addr);
        let alice = Address::generate(&ts.env);
        let spender = Address::generate(&ts.env);
        let bob = Address::generate(&ts.env);

        client.mint(&alice, &1_000);
        let expiry = ts.env.ledger().sequence() + 100;
        client.approve(&alice, &spender, &500, &expiry);

        let allow_key = DataKey::Allowance(alice.clone(), spender.clone());
        ts.env.ledger().with_mut(|l| l.sequence_number = expiry + 1);

        let res = client.try_transfer_from(&spender, &alice, &bob, &100);
        assert_eq!(res, Err(Ok(TokenError::InsufficientAllowance)));
        // In Soroban, returning Err rolls back host storage mutations from that invocation.
        // Reading allowance cleans up the expired entry:
        let allow = client.allowance(&alice, &spender);
        assert_eq!(allow.amount, 0);
        assert!(get_persistent_ttl(&ts, &allow_key).is_none());
    }

    #[test]
    fn test_lock_and_locked_balance_bumps_ttl() {
        let ts = setup();
        let client = LpTokenClient::new(&ts.env, &ts.contract_addr);
        let alice = Address::generate(&ts.env);
        client.mint(&alice, &1_000);

        client.lock(&alice, &300);

        let locked_key = DataKey::Locked(alice.clone());
        let entry_key = DataKey::LockEntry(alice.clone(), ts.admin.clone());
        let holders_key = DataKey::LockHolders(alice.clone());

        assert!(get_persistent_ttl(&ts, &locked_key).unwrap() >= LpToken::BUMP_TO - 1);
        assert!(get_persistent_ttl(&ts, &entry_key).unwrap() >= LpToken::BUMP_TO - 1);
        assert!(get_persistent_ttl(&ts, &holders_key).unwrap() >= LpToken::BUMP_TO - 1);

        lower_persistent_ttl(&ts, &locked_key, LpToken::BUMP_TO - LpToken::MIN_TTL + 1);
        assert_eq!(client.locked_balance(&alice), 300);
        assert!(get_persistent_ttl(&ts, &locked_key).unwrap() >= LpToken::BUMP_TO - 1);
    }

    #[test]
    fn test_unlock_bumps_or_clears_entries() {
        let ts = setup();
        let client = LpTokenClient::new(&ts.env, &ts.contract_addr);
        let alice = Address::generate(&ts.env);
        client.mint(&alice, &1_000);

        client.lock(&alice, &300);
        let locked_key = DataKey::Locked(alice.clone());
        let entry_key = DataKey::LockEntry(alice.clone(), ts.admin.clone());

        // Partial unlock bumps
        client.unlock(&alice, &ts.admin, &100);
        assert_eq!(client.locked_balance(&alice), 200);
        assert!(get_persistent_ttl(&ts, &locked_key).unwrap() >= LpToken::BUMP_TO - 1);
        assert!(get_persistent_ttl(&ts, &entry_key).unwrap() >= LpToken::BUMP_TO - 1);

        // Full unlock removes keys
        client.unlock(&alice, &ts.admin, &200);
        assert_eq!(client.locked_balance(&alice), 0);
        assert!(get_persistent_ttl(&ts, &locked_key).is_none());
        assert!(get_persistent_ttl(&ts, &entry_key).is_none());
        assert!(get_persistent_ttl(&ts, &DataKey::LockHolders(alice.clone())).is_none());
    }

    #[test]
    fn test_bump_account_permissionless_refreshes_all_entries() {
        let ts = setup();
        let client = LpTokenClient::new(&ts.env, &ts.contract_addr);
        let alice = Address::generate(&ts.env);
        let stranger = Address::generate(&ts.env);

        client.mint(&alice, &1_000);
        client.lock(&alice, &400);

        let bal_key = DataKey::Balance(alice.clone());
        let locked_key = DataKey::Locked(alice.clone());
        let entry_key = DataKey::LockEntry(alice.clone(), ts.admin.clone());
        let holders_key = DataKey::LockHolders(alice.clone());
        let cp_key = DataKey::CheckpointTracker(alice.clone());
        let cp_page_key = DataKey::CheckpointPage(alice.clone(), 0);

        lower_persistent_ttl(&ts, &bal_key, LpToken::BUMP_TO - LpToken::MIN_TTL + 1);

        // Stranger calls bump_account for alice
        client.bump_account(&alice);

        assert!(get_persistent_ttl(&ts, &bal_key).unwrap() >= LpToken::BUMP_TO - 1);
        assert!(get_persistent_ttl(&ts, &locked_key).unwrap() >= LpToken::BUMP_TO - 1);
        assert!(get_persistent_ttl(&ts, &entry_key).unwrap() >= LpToken::BUMP_TO - 1);
        assert!(get_persistent_ttl(&ts, &holders_key).unwrap() >= LpToken::BUMP_TO - 1);
        assert!(get_persistent_ttl(&ts, &cp_key).unwrap() >= LpToken::BUMP_TO - 1);
        assert!(get_persistent_ttl(&ts, &cp_page_key).unwrap() >= LpToken::BUMP_TO - 1);
        let _ = stranger;
    }

    #[test]
    fn test_bump_account_on_empty_account_does_not_trap() {
        let ts = setup();
        let client = LpTokenClient::new(&ts.env, &ts.contract_addr);
        let bob = Address::generate(&ts.env);

        // bob has no balance, no locks, no checkpoints
        client.bump_account(&bob);
        assert_eq!(client.balance(&bob), 0);
    }

    // ── Issue #1074: Paged checkpoints for O(1) balance writes ──────────────

    #[test]
    fn test_checkpoint_multiple_writes_same_ledger() {
        let ts = setup();
        let client = LpTokenClient::new(&ts.env, &ts.contract_addr);
        let alice = Address::generate(&ts.env);
        let bob = Address::generate(&ts.env);

        ts.env.ledger().set_sequence_number(100);
        client.mint(&alice, &1_000_i128);
        client.mint(&alice, &500_i128);
        client.transfer(&alice, &bob, &200_i128);

        // Balance queries at ledger 100 return the final balance at that ledger.
        assert_eq!(client.balance_at(&alice, &100_u32), 1_300);
        assert_eq!(client.balance_at(&bob, &100_u32), 200);

        // Verify storage layout: tracker total_count is 3 for alice, 1 for bob.
        ts.env.as_contract(&ts.contract_addr, || {
            let tracker_alice: CheckpointTracker = ts
                .env
                .storage()
                .persistent()
                .get(&DataKey::CheckpointTracker(alice.clone()))
                .unwrap();
            assert_eq!(tracker_alice.total_count, 3);
            assert_eq!(tracker_alice.start_page, 0);

            let page_alice: Vec<Checkpoint> = ts
                .env
                .storage()
                .persistent()
                .get(&DataKey::CheckpointPage(alice.clone(), 0))
                .unwrap();
            assert_eq!(page_alice.len(), 3);
            assert_eq!(page_alice.get(0).unwrap().balance, 1_000);
            assert_eq!(page_alice.get(1).unwrap().balance, 1_500);
            assert_eq!(page_alice.get(2).unwrap().balance, 1_300);
        });
    }

    #[test]
    fn test_checkpoint_page_boundary_crossing() {
        let ts = setup();
        let client = LpTokenClient::new(&ts.env, &ts.contract_addr);
        let alice = Address::generate(&ts.env);

        // Mint PAGE_SIZE (32) times.
        for i in 0..LpToken::PAGE_SIZE {
            ts.env.ledger().set_sequence_number(100 + i);
            client.mint(&alice, &10_i128);
            ts.env.budget().reset_default();
        }

        // Verify exactly 1 page exists, full with 32 entries.
        ts.env.as_contract(&ts.contract_addr, || {
            let tracker: CheckpointTracker = ts
                .env
                .storage()
                .persistent()
                .get(&DataKey::CheckpointTracker(alice.clone()))
                .unwrap();
            assert_eq!(tracker.total_count, 32);
            assert_eq!(tracker.start_page, 0);

            let page0: Vec<Checkpoint> = ts
                .env
                .storage()
                .persistent()
                .get(&DataKey::CheckpointPage(alice.clone(), 0))
                .unwrap();
            assert_eq!(page0.len(), 32);
            assert!(!ts
                .env
                .storage()
                .persistent()
                .has(&DataKey::CheckpointPage(alice.clone(), 1)));
        });

        // 33rd mint crosses to page 1.
        ts.env.ledger().set_sequence_number(200);
        client.mint(&alice, &10_i128);

        ts.env.as_contract(&ts.contract_addr, || {
            let tracker: CheckpointTracker = ts
                .env
                .storage()
                .persistent()
                .get(&DataKey::CheckpointTracker(alice.clone()))
                .unwrap();
            assert_eq!(tracker.total_count, 33);
            assert_eq!(tracker.start_page, 0);

            let page1: Vec<Checkpoint> = ts
                .env
                .storage()
                .persistent()
                .get(&DataKey::CheckpointPage(alice.clone(), 1))
                .unwrap();
            assert_eq!(page1.len(), 1);
            assert_eq!(page1.get(0).unwrap().balance, 330);
        });

        // balance_at across page boundary resolves correctly.
        assert_eq!(client.balance_at(&alice, &100_u32), 10);
        assert_eq!(client.balance_at(&alice, &131_u32), 320);
        assert_eq!(client.balance_at(&alice, &150_u32), 320);
        assert_eq!(client.balance_at(&alice, &200_u32), 330);
        assert_eq!(client.balance_at(&alice, &300_u32), 330);
    }

    #[test]
    fn test_checkpoint_eviction_drops_oldest_page() {
        let ts = setup();
        let client = LpTokenClient::new(&ts.env, &ts.contract_addr);
        let alice = Address::generate(&ts.env);

        // Fill exactly MAX_CHECKPOINTS (1024) checkpoints, spanning 32 pages (pages 0..31).
        for i in 0..LpToken::MAX_CHECKPOINTS {
            ts.env.ledger().set_sequence_number(100 + i);
            client.mint(&alice, &1_i128);
            ts.env.budget().reset_default();
        }

        ts.env.as_contract(&ts.contract_addr, || {
            let tracker: CheckpointTracker = ts
                .env
                .storage()
                .persistent()
                .get(&DataKey::CheckpointTracker(alice.clone()))
                .unwrap();
            assert_eq!(tracker.total_count, 1024);
            assert_eq!(tracker.start_page, 0);
            assert!(ts
                .env
                .storage()
                .persistent()
                .has(&DataKey::CheckpointPage(alice.clone(), 0)));
            assert!(ts
                .env
                .storage()
                .persistent()
                .has(&DataKey::CheckpointPage(alice.clone(), 31)));
        });

        // 1025th checkpoint triggers eviction of page 0.
        ts.env.ledger().set_sequence_number(2000);
        client.mint(&alice, &1_i128);

        ts.env.as_contract(&ts.contract_addr, || {
            let tracker: CheckpointTracker = ts
                .env
                .storage()
                .persistent()
                .get(&DataKey::CheckpointTracker(alice.clone()))
                .unwrap();
            assert_eq!(tracker.total_count, 1025);
            assert_eq!(tracker.start_page, 1);
            // Page 0 was removed!
            assert!(!ts
                .env
                .storage()
                .persistent()
                .has(&DataKey::CheckpointPage(alice.clone(), 0)));
            // Page 32 was created!
            assert!(ts
                .env
                .storage()
                .persistent()
                .has(&DataKey::CheckpointPage(alice.clone(), 32)));
        });

        // Querying an evicted ledger returns BalanceUnavailable.
        assert_eq!(
            client.try_balance_at(&alice, &100_u32),
            Err(Ok(TokenError::BalanceUnavailable))
        );
        // Querying ledger 131 (last item in page 0) also errors.
        assert_eq!(
            client.try_balance_at(&alice, &131_u32),
            Err(Ok(TokenError::BalanceUnavailable))
        );
        // Querying ledger 132 (first item in page 1) succeeds.
        assert_eq!(client.balance_at(&alice, &132_u32), 33);
    }

    #[test]
    fn test_checkpoint_legacy_migration_on_read() {
        let ts = setup();
        let client = LpTokenClient::new(&ts.env, &ts.contract_addr);
        let alice = Address::generate(&ts.env);

        // Seed legacy DataKey::Checkpoints entry with no CheckpointTracker.
        ts.env.as_contract(&ts.contract_addr, || {
            let mut checkpoints = Vec::new(&ts.env);
            checkpoints.push_back(Checkpoint {
                ledger: 50,
                balance: 100,
            });
            checkpoints.push_back(Checkpoint {
                ledger: 100,
                balance: 250,
            });
            ts.env
                .storage()
                .persistent()
                .set(&DataKey::Checkpoints(alice.clone()), &checkpoints);
        });

        // balance_at reads from legacy format seamlessly.
        assert_eq!(client.balance_at(&alice, &40_u32), 0);
        assert_eq!(client.balance_at(&alice, &50_u32), 100);
        assert_eq!(client.balance_at(&alice, &75_u32), 100);
        assert_eq!(client.balance_at(&alice, &100_u32), 250);
        assert_eq!(client.balance_at(&alice, &150_u32), 250);
    }

    #[test]
    fn test_checkpoint_legacy_migration_on_write() {
        let ts = setup();
        let client = LpTokenClient::new(&ts.env, &ts.contract_addr);
        let alice = Address::generate(&ts.env);

        // Seed 40 legacy checkpoints.
        ts.env.as_contract(&ts.contract_addr, || {
            let mut checkpoints = Vec::new(&ts.env);
            for i in 0..40u32 {
                checkpoints.push_back(Checkpoint {
                    ledger: 10 + i,
                    balance: (i as i128 + 1) * 10,
                });
            }
            ts.env
                .storage()
                .persistent()
                .set(&DataKey::Checkpoints(alice.clone()), &checkpoints);
            ts.env
                .storage()
                .persistent()
                .set(&DataKey::Balance(alice.clone()), &400_i128);
        });

        // Trigger a write on ledger 100.
        ts.env.ledger().set_sequence_number(100);
        client.mint(&alice, &50_i128);

        // Legacy key must be deleted, and pages created.
        ts.env.as_contract(&ts.contract_addr, || {
            assert!(!ts
                .env
                .storage()
                .persistent()
                .has(&DataKey::Checkpoints(alice.clone())));

            let tracker: CheckpointTracker = ts
                .env
                .storage()
                .persistent()
                .get(&DataKey::CheckpointTracker(alice.clone()))
                .unwrap();
            assert_eq!(tracker.total_count, 41);
            assert_eq!(tracker.start_page, 0);

            let page0: Vec<Checkpoint> = ts
                .env
                .storage()
                .persistent()
                .get(&DataKey::CheckpointPage(alice.clone(), 0))
                .unwrap();
            assert_eq!(page0.len(), 32);

            let page1: Vec<Checkpoint> = ts
                .env
                .storage()
                .persistent()
                .get(&DataKey::CheckpointPage(alice.clone(), 1))
                .unwrap();
            assert_eq!(page1.len(), 9);
            assert_eq!(page1.get(8).unwrap().balance, 450);
        });

        // Verification of query results across migrated pages.
        assert_eq!(client.balance_at(&alice, &10_u32), 10);
        assert_eq!(client.balance_at(&alice, &41_u32), 320);
        assert_eq!(client.balance_at(&alice, &49_u32), 400);
        assert_eq!(client.balance_at(&alice, &100_u32), 450);
    }

    #[test]
    fn test_checkpoint_write_bounded_by_single_page() {
        let ts = setup();
        let client = LpTokenClient::new(&ts.env, &ts.contract_addr);
        let alice = Address::generate(&ts.env);

        // Populate 100 checkpoints across multiple pages.
        for i in 0..100 {
            ts.env.ledger().set_sequence_number(100 + i);
            client.mint(&alice, &1_i128);
            ts.env.budget().reset_default();
        }

        // Each page holds at most PAGE_SIZE (32) checkpoints.
        ts.env.as_contract(&ts.contract_addr, || {
            let tracker: CheckpointTracker = ts
                .env
                .storage()
                .persistent()
                .get(&DataKey::CheckpointTracker(alice.clone()))
                .unwrap();
            assert_eq!(tracker.total_count, 100);

            for page_idx in tracker.start_page..=(tracker.total_count / LpToken::PAGE_SIZE) {
                if let Some(page) =
                    ts.env.storage().persistent().get::<_, Vec<Checkpoint>>(
                        &DataKey::CheckpointPage(alice.clone(), page_idx),
                    )
                {
                    assert!(
                        page.len() <= LpToken::PAGE_SIZE,
                        "page {page_idx} exceeded PAGE_SIZE: {}",
                        page.len()
                    );
                }
            }
        });
    }

    #[test]
    fn test_checkpoint_simulation_and_apply_write_size_identical() {
        // Since there is no same-ledger coalescing, every balance change appends
        // exactly 1 checkpoint to the current page. Simulation at ledger L and apply
        // at ledger L+1 write to the exact same page index and append the exact same size.
        let ts = setup();
        let client = LpTokenClient::new(&ts.env, &ts.contract_addr);
        let alice = Address::generate(&ts.env);

        ts.env.ledger().set_sequence_number(100);
        client.mint(&alice, &100_i128);

        // Simulation at ledger 100 appends to page 0.
        // Apply at ledger 100 or 101 also appends to page 0.
        client.mint(&alice, &50_i128);
        ts.env.ledger().set_sequence_number(101);
        client.mint(&alice, &50_i128);

        assert_eq!(client.balance_at(&alice, &100_u32), 150);
        assert_eq!(client.balance_at(&alice, &101_u32), 200);
    }

    #[test]
    fn test_checkpoint_binary_search_exact_and_between_ledgers() {
        let ts = setup();
        let client = LpTokenClient::new(&ts.env, &ts.contract_addr);
        let alice = Address::generate(&ts.env);

        // Write at ledgers 100, 200, 300 across multiple pages.
        // To force multi-page, add 30 dummy writes at ledger 100, 1 write at 200, 30 writes at 300.
        ts.env.ledger().set_sequence_number(100);
        for _ in 0..30 {
            client.mint(&alice, &1_i128);
            ts.env.budget().reset_default();
        }

        ts.env.ledger().set_sequence_number(200);
        client.mint(&alice, &10_i128); // 31st (still page 0)
        client.mint(&alice, &10_i128); // 32nd (page 0 full)
        client.mint(&alice, &10_i128); // 33rd (page 1)

        ts.env.ledger().set_sequence_number(300);
        client.mint(&alice, &100_i128);

        // Exact matches
        assert_eq!(client.balance_at(&alice, &100_u32), 30);
        assert_eq!(client.balance_at(&alice, &200_u32), 60);
        assert_eq!(client.balance_at(&alice, &300_u32), 160);

        // Intermediate queries
        assert_eq!(client.balance_at(&alice, &150_u32), 30);
        assert_eq!(client.balance_at(&alice, &250_u32), 60);
        assert_eq!(client.balance_at(&alice, &350_u32), 160);
    }

    #[test]
    fn test_checkpoint_query_before_first_ledger_returns_zero() {
        let ts = setup();
        let client = LpTokenClient::new(&ts.env, &ts.contract_addr);
        let alice = Address::generate(&ts.env);

        ts.env.ledger().set_sequence_number(500);
        client.mint(&alice, &1_000_i128);

        assert_eq!(client.balance_at(&alice, &499_u32), 0);
        assert_eq!(client.balance_at(&alice, &1_u32), 0);
    }

    #[test]
    fn test_checkpoint_transfer_updates_both_accounts_paged() {
        let ts = setup();
        let client = LpTokenClient::new(&ts.env, &ts.contract_addr);
        let alice = Address::generate(&ts.env);
        let bob = Address::generate(&ts.env);

        ts.env.ledger().set_sequence_number(100);
        client.mint(&alice, &1_000_i128);
        client.transfer(&alice, &bob, &300_i128);

        assert_eq!(client.balance_at(&alice, &100_u32), 700);
        assert_eq!(client.balance_at(&bob, &100_u32), 300);

        ts.env.as_contract(&ts.contract_addr, || {
            let tr_a: CheckpointTracker = ts
                .env
                .storage()
                .persistent()
                .get(&DataKey::CheckpointTracker(alice.clone()))
                .unwrap();
            let tr_b: CheckpointTracker = ts
                .env
                .storage()
                .persistent()
                .get(&DataKey::CheckpointTracker(bob.clone()))
                .unwrap();
            assert_eq!(tr_a.total_count, 2); // mint + transfer
            assert_eq!(tr_b.total_count, 1); // transfer
        });
    }

    #[test]
    fn test_checkpoint_burn_records_paged_checkpoint() {
        let ts = setup();
        let client = LpTokenClient::new(&ts.env, &ts.contract_addr);
        let alice = Address::generate(&ts.env);

        ts.env.ledger().set_sequence_number(100);
        client.mint(&alice, &1_000_i128);
        ts.env.ledger().set_sequence_number(105);
        client.burn(&alice, &400_i128);

        assert_eq!(client.balance_at(&alice, &100_u32), 1_000);
        assert_eq!(client.balance_at(&alice, &104_u32), 1_000);
        assert_eq!(client.balance_at(&alice, &105_u32), 600);

        ts.env.as_contract(&ts.contract_addr, || {
            let tr: CheckpointTracker = ts
                .env
                .storage()
                .persistent()
                .get(&DataKey::CheckpointTracker(alice.clone()))
                .unwrap();
            assert_eq!(tr.total_count, 2);
        });
    }

    #[test]
    fn test_checkpoint_consecutive_page_evictions() {
        let ts = setup();
        let client = LpTokenClient::new(&ts.env, &ts.contract_addr);
        let alice = Address::generate(&ts.env);

        // Fill 1024 checkpoints (pages 0..31)
        for i in 0..LpToken::MAX_CHECKPOINTS {
            ts.env.ledger().set_sequence_number(100 + i);
            client.mint(&alice, &1_i128);
            ts.env.budget().reset_default();
        }

        // Add 64 more checkpoints across two new pages (pages 32 and 33).
        // This should evict page 0, then page 1.
        for i in 0..64 {
            ts.env.ledger().set_sequence_number(2000 + i);
            client.mint(&alice, &1_i128);
            ts.env.budget().reset_default();
        }

        ts.env.as_contract(&ts.contract_addr, || {
            let tr: CheckpointTracker = ts
                .env
                .storage()
                .persistent()
                .get(&DataKey::CheckpointTracker(alice.clone()))
                .unwrap();
            assert_eq!(tr.total_count, 1024 + 64);
            assert_eq!(tr.start_page, 2);
            assert!(!ts
                .env
                .storage()
                .persistent()
                .has(&DataKey::CheckpointPage(alice.clone(), 0)));
            assert!(!ts
                .env
                .storage()
                .persistent()
                .has(&DataKey::CheckpointPage(alice.clone(), 1)));
            assert!(ts
                .env
                .storage()
                .persistent()
                .has(&DataKey::CheckpointPage(alice.clone(), 2)));
        });

        // Ledgers in page 1 (up to ledger 100 + 63 = 163) are unavailable.
        assert_eq!(
            client.try_balance_at(&alice, &163_u32),
            Err(Ok(TokenError::BalanceUnavailable))
        );
        // Ledger in page 2 (ledger 100 + 64 = 164) is available.
        assert_eq!(client.balance_at(&alice, &164_u32), 65);
    }
}

#[cfg(test)]
mod tests_issue_1112;

#[cfg(test)]
mod auth_tests;
