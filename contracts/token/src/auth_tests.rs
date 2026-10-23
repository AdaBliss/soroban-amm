extern crate std;

use super::*;
use soroban_sdk::{
    testutils::{Address as _, MockAuth, MockAuthInvoke},
    Address, Env, IntoVal, String, Symbol, Val,
};

#[cfg(feature = "testutils")]
use super::WASM as TOKEN_WASM;
#[cfg(not(feature = "testutils"))]
const TOKEN_WASM: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../target/wasm32v1-none/release/token.wasm"
));

/// Helper to assert that a privileged invocation strictly enforces authorization.
///
/// It performs three checks:
/// 1. Runs with no authorizations (`env.set_auths(&[])`) and asserts failure with `Error(Auth, InvalidAction)`.
/// 2. Runs with only a stranger's authorization and asserts failure with `Error(Auth, InvalidAction)`.
/// 3. Runs with only the expected signer's authorization and asserts success.
fn assert_requires_auth(
    env: &Env,
    _contract_id: &Address,
    signer_expected: &Address,
    invoke: &MockAuthInvoke,
) {
    let stranger = Address::generate(env);
    let stranger_auth = [MockAuth {
        address: &stranger,
        invoke,
    }];
    let expected_auth = [MockAuth {
        address: signer_expected,
        invoke,
    }];

    // 1. With no auths
    env.set_auths(&[]);
    assert_auth_err(env, invoke, "no authorizations");

    // 2. With stranger auth
    env.mock_auths(&stranger_auth);
    assert_auth_err(env, invoke, "stranger authorization");

    // 3. With expected signer auth
    env.mock_auths(&expected_auth);
    let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        env.invoke_contract::<Val>(
            invoke.contract,
            &Symbol::new(env, invoke.fn_name),
            invoke.args.clone(),
        );
    }));
    assert!(
        res.is_ok(),
        "expected {} to succeed with authorization from {:?}, failed: {:?}",
        invoke.fn_name,
        signer_expected,
        res
    );
}

fn assert_auth_err(env: &Env, invoke: &MockAuthInvoke, context: &str) {
    let prev_hook = std::panic::take_hook();
    std::panic::set_hook(std::boxed::Box::new(|_| {}));
    let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        env.invoke_contract::<Val>(
            invoke.contract,
            &Symbol::new(env, invoke.fn_name),
            invoke.args.clone(),
        );
    }));
    std::panic::set_hook(prev_hook);

    assert!(
        res.is_err(),
        "expected {} to fail under {}, but it succeeded",
        invoke.fn_name,
        context
    );
    let err = res.unwrap_err();
    let msg = if let Some(s) = err.downcast_ref::<&str>() {
        std::string::String::from(*s)
    } else if let Some(s) = err.downcast_ref::<std::string::String>() {
        s.clone()
    } else {
        std::string::String::new()
    };
    assert!(
        msg.contains("Error(Auth, InvalidAction)"),
        "expected Error(Auth, InvalidAction) for {} under {}, got: {}",
        invoke.fn_name,
        context,
        msg
    );
}

struct Fixture {
    env: Env,
    admin: Address,
    contract_addr: Address,
    client: LpTokenClient<'static>,
}

fn setup_fixture() -> Fixture {
    let env = Env::default();
    env.mock_all_auths();
    let admin = Address::generate(&env);
    let contract_addr = env.register_contract(None, LpToken);
    let client = LpTokenClient::new(&env, &contract_addr);
    client.initialize(
        &admin,
        &String::from_str(&env, "Test Token"),
        &String::from_str(&env, "TST"),
        &7u32,
    );
    Fixture {
        env,
        admin,
        contract_addr,
        client,
    }
}

#[test]
fn test_mint_requires_auth() {
    let f = setup_fixture();
    let user = Address::generate(&f.env);
    let invoke = MockAuthInvoke {
        contract: &f.contract_addr,
        fn_name: "mint",
        args: (user.clone(), 100_i128).into_val(&f.env),
        sub_invokes: &[],
    };
    assert_requires_auth(&f.env, &f.contract_addr, &f.admin, &invoke);
    assert_eq!(f.client.balance(&user), 100_i128);
}

#[test]
fn test_burn_requires_auth() {
    let f = setup_fixture();
    let user = Address::generate(&f.env);
    f.client.mint(&user, &100_i128);
    let invoke = MockAuthInvoke {
        contract: &f.contract_addr,
        fn_name: "burn",
        args: (user.clone(), 40_i128).into_val(&f.env),
        sub_invokes: &[],
    };
    assert_requires_auth(&f.env, &f.contract_addr, &f.admin, &invoke);
    assert_eq!(f.client.balance(&user), 60_i128);
}

#[test]
fn test_propose_admin_requires_auth() {
    let f = setup_fixture();
    let new_admin = Address::generate(&f.env);
    let invoke = MockAuthInvoke {
        contract: &f.contract_addr,
        fn_name: "propose_admin",
        args: (f.admin.clone(), new_admin.clone()).into_val(&f.env),
        sub_invokes: &[],
    };
    assert_requires_auth(&f.env, &f.contract_addr, &f.admin, &invoke);
}

#[test]
fn test_propose_admin_rejects_stranger_as_admin_arg() {
    let f = setup_fixture();
    let stranger = Address::generate(&f.env);
    let new_admin = Address::generate(&f.env);
    let res = f.client.try_propose_admin(&stranger, &new_admin);
    assert_eq!(res, Err(Ok(TokenError::Unauthorized)));
}

#[test]
fn test_accept_admin_requires_auth() {
    let f = setup_fixture();
    let new_admin = Address::generate(&f.env);
    f.client.propose_admin(&f.admin, &new_admin);
    let invoke = MockAuthInvoke {
        contract: &f.contract_addr,
        fn_name: "accept_admin",
        args: (new_admin.clone(),).into_val(&f.env),
        sub_invokes: &[],
    };
    assert_requires_auth(&f.env, &f.contract_addr, &new_admin, &invoke);
    assert_eq!(f.client.admin(), new_admin);
}

#[test]
fn test_accept_admin_rejects_stranger_nominee() {
    let f = setup_fixture();
    let stranger = Address::generate(&f.env);
    let res = f.client.try_accept_admin(&stranger);
    assert_eq!(res, Err(Ok(TokenError::Unauthorized)));
}

#[test]
fn test_upgrade_requires_auth() {
    let f = setup_fixture();
    let new_hash = f.env.deployer().upload_contract_wasm(TOKEN_WASM);
    let invoke = MockAuthInvoke {
        contract: &f.contract_addr,
        fn_name: "upgrade",
        args: (new_hash.clone(),).into_val(&f.env),
        sub_invokes: &[],
    };
    assert_requires_auth(&f.env, &f.contract_addr, &f.admin, &invoke);
}

#[test]
fn test_set_locker_requires_auth() {
    let f = setup_fixture();
    let locker = Address::generate(&f.env);
    let invoke = MockAuthInvoke {
        contract: &f.contract_addr,
        fn_name: "set_locker",
        args: (locker.clone(),).into_val(&f.env),
        sub_invokes: &[],
    };
    assert_requires_auth(&f.env, &f.contract_addr, &f.admin, &invoke);
    assert_eq!(f.client.locker(), locker);
}

#[test]
fn test_migrate_legacy_lock_requires_auth() {
    let f = setup_fixture();
    let holder = Address::generate(&f.env);
    let locker = Address::generate(&f.env);
    f.env.as_contract(&f.contract_addr, || {
        f.env
            .storage()
            .persistent()
            .set(&DataKey::Locked(holder.clone()), &100_i128);
    });
    let invoke = MockAuthInvoke {
        contract: &f.contract_addr,
        fn_name: "migrate_legacy_lock",
        args: (holder.clone(), locker.clone(), 50_i128).into_val(&f.env),
        sub_invokes: &[],
    };
    assert_requires_auth(&f.env, &f.contract_addr, &f.admin, &invoke);
}

#[test]
fn test_lock_requires_auth() {
    let f = setup_fixture();
    let locker = Address::generate(&f.env);
    let holder = Address::generate(&f.env);
    f.client.set_locker(&locker);
    f.client.mint(&holder, &100_i128);
    let invoke = MockAuthInvoke {
        contract: &f.contract_addr,
        fn_name: "lock",
        args: (holder.clone(), 40_i128).into_val(&f.env),
        sub_invokes: &[],
    };
    assert_requires_auth(&f.env, &f.contract_addr, &locker, &invoke);
    assert_eq!(f.client.locked_balance(&holder), 40_i128);
}

#[test]
fn test_unlock_requires_auth() {
    let f = setup_fixture();
    let locker = Address::generate(&f.env);
    let holder = Address::generate(&f.env);
    f.client.set_locker(&locker);
    f.client.mint(&holder, &100_i128);
    f.client.lock(&holder, &60_i128);
    let invoke = MockAuthInvoke {
        contract: &f.contract_addr,
        fn_name: "unlock",
        args: (holder.clone(), locker.clone(), 25_i128).into_val(&f.env),
        sub_invokes: &[],
    };
    assert_requires_auth(&f.env, &f.contract_addr, &locker, &invoke);
    assert_eq!(f.client.locked_balance(&holder), 35_i128);
}
