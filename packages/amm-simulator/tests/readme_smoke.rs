use soroban_amm_simulator::{cl::ClPoolState, PoolState};
use std::{
    fs,
    process::Command,
    sync::atomic::{AtomicUsize, Ordering},
};

static N: AtomicUsize = AtomicUsize::new(0);
fn file(name: &str, text: &str) -> std::path::PathBuf {
    let p = std::env::temp_dir().join(format!(
        "amm-readme-{}-{}-{name}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    fs::write(&p, text).unwrap();
    p
}
fn run(args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_amm_sim"))
        .args(args)
        .output()
        .unwrap()
}
fn pool() -> std::path::PathBuf {
    file(
        "pool.json",
        r#"{"token_a":"XLM","token_b":"USDC","reserve_a":1000000,"reserve_b":1000000,"total_shares":1000000,"fee_bps":30,"protocol_fee_bps":5,"min_liquidity_locked":true,"locked_liquidity":1000}"#,
    )
}

#[test]
fn readme_package_and_binary_names_are_real() {
    let readme = include_str!("../README.md");
    assert!(readme.contains("-p soroban_amm_simulator --bin amm_sim"));
}
#[test]
fn readme_pool_fixture_validates() {
    let p = pool();
    assert!(soroban_amm_simulator::load_pool_state(p).is_ok());
}
#[test]
fn readme_quote_command_runs() {
    let p = pool();
    let o = run(&[
        "quote",
        "--pool",
        p.to_str().unwrap(),
        "--token-in",
        "XLM",
        "--amount-in",
        "1000",
        "--pretty",
    ]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
}
#[test]
fn readme_replay_command_runs() {
    let p = pool();
    let t = file("trades.json", "[]");
    let o = run(&[
        "replay",
        "--pool",
        p.to_str().unwrap(),
        "--trades",
        t.to_str().unwrap(),
    ]);
    assert!(o.status.success());
}
#[test]
fn readme_monte_carlo_command_runs() {
    let p = pool();
    let t = file("trades.json", "[]");
    let o = run(&[
        "monte-carlo",
        "--pool",
        p.to_str().unwrap(),
        "--trades",
        t.to_str().unwrap(),
        "--iterations",
        "2",
        "--amount-shock-bps",
        "50",
    ]);
    assert!(o.status.success());
}
#[test]
fn readme_cargo_examples_execute_from_the_document() {
    let p = pool();
    let t = file("trades.json", "[]");
    let mut blocks: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut in_block = false;
    for line in include_str!("../README.md").lines() {
        if !in_block && line.trim() == "```bash" {
            in_block = true;
            continue;
        }
        if !in_block {
            continue;
        }
        if line.trim() == "```" {
            if !current.is_empty() {
                blocks.push(std::mem::take(&mut current));
            }
            in_block = false;
            continue;
        }
        if line.starts_with("cargo run -p soroban_amm_simulator --bin amm_sim -- ") {
            if !current.is_empty() {
                blocks.push(std::mem::take(&mut current));
            }
            current.push_str(line.trim().trim_end_matches('\\'));
            current.push(' ');
            continue;
        }
        if !current.is_empty() && !line.trim().is_empty() && !line.trim_start().starts_with('#') {
            current.push_str(line.trim().trim_end_matches('\\'));
            current.push(' ');
        }
    }
    if !current.is_empty() {
        blocks.push(current);
    }
    assert!(blocks.len() >= 3);
    for block in blocks.iter().take(3) {
        let words: Vec<&str> = block.split_whitespace().collect();
        let sep = words.iter().position(|w| *w == "--").unwrap();
        let mut args: Vec<String> = words[sep + 1..].iter().map(|w| w.to_string()).collect();
        for arg in &mut args {
            if arg == "pool.json" {
                *arg = p.to_string_lossy().into_owned();
            }
            if arg == "trades.json" {
                *arg = t.to_string_lossy().into_owned();
            }
        }
        let refs: Vec<&str> = args.iter().map(String::as_str).collect();
        let out = run(&refs);
        assert!(
            out.status.success(),
            "documented command failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
}
#[test]
fn compact_replay_is_one_line() {
    let p = pool();
    let t = file("trades.json", "[]");
    let o = run(&[
        "replay",
        "--pool",
        p.to_str().unwrap(),
        "--trades",
        t.to_str().unwrap(),
        "--compact",
    ]);
    assert!(o.status.success());
    assert_eq!(String::from_utf8_lossy(&o.stdout).lines().count(), 1);
}
#[test]
fn pretty_replay_is_indented() {
    let p = pool();
    let t = file("trades.json", "[]");
    let o = run(&[
        "replay",
        "--pool",
        p.to_str().unwrap(),
        "--trades",
        t.to_str().unwrap(),
        "--pretty",
    ]);
    assert!(o.status.success());
    assert!(String::from_utf8_lossy(&o.stdout).lines().count() > 1);
}
#[test]
fn replay_exits_quietly_when_consumer_closes_pipe() {
    use std::io::BufRead;
    use std::process::Stdio;
    let p = pool();
    let mut trades = String::from("[");
    for i in 0..100 {
        if i > 0 {
            trades.push(',');
        }
        trades.push_str(&format!("{{\"timestamp\":{},\"kind\":\"pause\"}}", i));
    }
    trades.push(']');
    let t = file("pipe-trades.json", &trades);
    let mut child = Command::new(env!("CARGO_BIN_EXE_amm_sim"))
        .args([
            "replay",
            "--pool",
            p.to_str().unwrap(),
            "--trades",
            t.to_str().unwrap(),
            "--pretty",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let stdout = child.stdout.take().unwrap();
    let _first_line = std::io::BufReader::new(stdout).lines().next();
    let status = child.wait().unwrap();
    assert!(status.success());
    let stderr = child.stderr.take().unwrap();
    assert!(std::io::Read::bytes(stderr)
        .take(1024)
        .all(|b| b.unwrap() != b'\n'));
}
#[test]
fn invalid_arguments_use_input_exit_code() {
    let p = pool();
    let o = run(&["quote", "--pool", p.to_str().unwrap()]);
    assert_eq!(o.status.code(), Some(2));
}
#[test]
fn replay_trade_failures_use_distinct_exit_code() {
    let p = pool();
    let t = file(
        "failed-trade.json",
        r#"[{"timestamp":1,"kind":"swap_exact_in","token_in":"NOPE","amount_in":10}]"#,
    );
    let o = run(&[
        "replay",
        "--pool",
        p.to_str().unwrap(),
        "--trades",
        t.to_str().unwrap(),
    ]);
    assert_eq!(o.status.code(), Some(3));
    assert!(String::from_utf8_lossy(&o.stderr).contains("failed trade"));
}
#[test]
fn replay_updates_simulator_fee_configuration() {
    let p = pool();
    let t = file(
        "fees.json",
        r#"[{"timestamp":1,"kind":"set_fees","fee_bps":1,"protocol_fee_bps":1,"lp_rebate_bps":5000}]"#,
    );
    let o = run(&[
        "replay",
        "--pool",
        p.to_str().unwrap(),
        "--trades",
        t.to_str().unwrap(),
    ]);
    assert!(o.status.success());
    let report: serde_json::Value = serde_json::from_slice(&o.stdout).unwrap();
    assert_eq!(report["summary"]["final_pool"]["fee_bps"], 1);
    assert_eq!(report["summary"]["final_pool"]["lp_rebate_bps"], 5000);
}
#[test]
fn replay_pause_and_unpause_actions_apply() {
    let p = pool();
    let t = file(
        "pause.json",
        r#"[{"timestamp":1,"kind":"pause"},{"timestamp":2,"kind":"unpause"},{"timestamp":3,"kind":"swap_exact_in","token_in":"XLM","amount_in":100}]"#,
    );
    let o = run(&[
        "replay",
        "--pool",
        p.to_str().unwrap(),
        "--trades",
        t.to_str().unwrap(),
    ]);
    assert!(o.status.success());
    let report: serde_json::Value = serde_json::from_slice(&o.stdout).unwrap();
    assert_eq!(report["summary"]["successful_trades"], 3);
}
#[test]
fn one_sided_removal_is_available_in_replay() {
    let p = pool();
    let t = file(
        "one-sided.json",
        r#"[{"timestamp":1,"kind":"remove_liquidity_one_sided","shares":10000,"token_out":"XLM"}]"#,
    );
    let o = run(&[
        "replay",
        "--pool",
        p.to_str().unwrap(),
        "--trades",
        t.to_str().unwrap(),
    ]);
    assert!(o.status.success());
    let report: serde_json::Value = serde_json::from_slice(&o.stdout).unwrap();
    assert!(
        report["steps"][0]["outcome"]["one_sided_liquidity"]["total_out"]
            .as_i64()
            .unwrap()
            > 0
    );
}
#[test]
fn fee_configuration_csv_columns_parse() {
    let t = file(
        "fees.csv",
        "timestamp,kind,fee_bps,protocol_fee_bps,lp_rebate_bps\n1,set_fees,30,5,2500\n",
    );
    let records = soroban_amm_simulator::load_trade_records(t).unwrap();
    assert_eq!(records.len(), 1);
    assert_eq!(
        serde_json::to_value(&records[0]).unwrap()["kind"],
        "set_fees"
    );
}
#[test]
fn cl_quote_matches_the_library_swap_result() {
    #[derive(serde::Deserialize)]
    struct Output {
        result: soroban_amm_simulator::cl::ClSwapResult,
    }
    let mut cl = ClPoolState::new("XLM", "USDC", 30, 10).unwrap();
    cl.initialize_at_tick(0).unwrap();
    cl.add_liquidity("lp", -600, 600, 10_000_000_000_000)
        .unwrap();
    let mut expected = cl.clone();
    let expected = expected.swap(true, 1_000_000, 0, 0).unwrap();
    let p = file("cl.json", &serde_json::to_string(&cl).unwrap());
    let o = run(&[
        "cl-quote",
        "--pool",
        p.to_str().unwrap(),
        "--zero-for-one",
        "--amount-in",
        "1000000",
    ]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    let actual: Output = serde_json::from_slice(&o.stdout).unwrap();
    assert_eq!(actual.result, expected);
}
#[test]
fn cl_replay_runs_from_state_and_swaps_file() {
    let mut cl = ClPoolState::new("XLM", "USDC", 30, 10).unwrap();
    cl.initialize_at_tick(0).unwrap();
    cl.add_liquidity("lp", -600, 600, 10_000_000_000_000)
        .unwrap();
    let p = file("cl.json", &serde_json::to_string(&cl).unwrap());
    let s = file(
        "swaps.json",
        r#"[{"zero_for_one":true,"amount_in":1000000000}]"#,
    );
    let o = run(&[
        "cl-replay",
        "--pool",
        p.to_str().unwrap(),
        "--swaps",
        s.to_str().unwrap(),
    ]);
    assert!(o.status.success());
}

// Keep the documented JSON fixture's shape tied to PoolState deserialization.
#[test]
fn readme_example_has_documented_values() {
    let p = pool();
    let state: PoolState = soroban_amm_simulator::load_pool_state(p).unwrap();
    assert_eq!(state.total_shares, 1_000_000);
}
