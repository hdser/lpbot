//! Tests of `lpbot gates`: the step runner and its summary, the pure
//! decisions, and the steps themselves against a mock node and the
//! simulated history.

use super::*;
use lpbot_chain::testing::MockNode;
use lpbot_core::experiment::{resolve_experiment, RunOverrides};
use serde_json::{json, Value};
use std::path::Path;

fn day(text: &str) -> Day {
    text.parse().unwrap()
}

fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// A step that records that it ran and ends as told.
fn step<'a>(
    name: &'static str,
    outcome: Result<Outcome, &'static str>,
) -> Step<'a, Vec<&'static str>> {
    Step {
        name,
        title: "a step",
        run: Box::new(move |ran: &mut Vec<&'static str>| {
            ran.push(name);
            outcome.clone().map_err(|message| anyhow!(message))
        }),
    }
}

fn pass() -> Result<Outcome, &'static str> {
    Ok(Outcome::Pass("fine".into()))
}

fn statuses(results: &[StepResult]) -> Vec<&'static str> {
    results.iter().map(|r| status(r.outcome.as_ref())).collect()
}

#[test]
fn steps_run_in_order_and_an_open_check_does_not_stop_them() {
    let mut ran = Vec::new();
    let steps = vec![
        step("a", pass()),
        step("b", Ok(Outcome::Open("could not tell".into()))),
        step("c", pass()),
    ];
    let results = run_steps(steps, &mut ran, &Plan::default()).unwrap();
    assert_eq!(ran, ["a", "b", "c"]);
    assert_eq!(statuses(&results), ["pass", "open", "pass"]);
    assert_eq!(failed_step(&results), None);
}

#[test]
fn the_first_failure_stops_the_rest() {
    let mut ran = Vec::new();
    let steps = vec![
        step("a", pass()),
        step("b", Ok(Outcome::Fail("does not match".into()))),
        step("c", pass()),
        step("d", pass()),
    ];
    let results = run_steps(steps, &mut ran, &Plan::default()).unwrap();
    assert_eq!(ran, ["a", "b"]);
    assert_eq!(statuses(&results), ["pass", "FAIL", "not run", "not run"]);
    assert_eq!(failed_step(&results), Some("b"));
}

#[test]
fn a_step_that_errors_counts_as_failed_with_the_error_as_its_note() {
    let mut ran = Vec::new();
    let steps = vec![step("a", Err("the node is not there")), step("b", pass())];
    let results = run_steps(steps, &mut ran, &Plan::default()).unwrap();
    assert_eq!(ran, ["a"]);
    assert_eq!(
        results[0].outcome,
        Some(Outcome::Fail("the node is not there".into()))
    );
    assert_eq!(results[1].outcome, None);
}

#[test]
fn a_plan_starts_later_and_leaves_steps_out() {
    let mut ran = Vec::new();
    let steps = vec![
        step("a", pass()),
        step("b", pass()),
        step("c", pass()),
        step("d", pass()),
    ];
    let plan = Plan {
        start_at: Some("b".into()),
        skip: vec!["c".into()],
    };
    let results = run_steps(steps, &mut ran, &plan).unwrap();
    assert_eq!(ran, ["b", "d"]);
    assert_eq!(statuses(&results), ["left out", "pass", "left out", "pass"]);
    assert_eq!(
        results[0].outcome,
        Some(Outcome::Skipped("started at `b`".into()))
    );
}

#[test]
fn a_step_name_that_does_not_exist_is_refused_before_anything_runs() {
    let mut ran = Vec::new();
    let plan = Plan {
        start_at: None,
        skip: vec!["histroy".into()],
    };
    let error = run_steps(vec![step("history", pass())], &mut ran, &plan)
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("histroy") && error.contains("history"),
        "{error}"
    );
    assert!(ran.is_empty());

    let names: Vec<&str> = STEPS.iter().map(|(name, _)| *name).collect();
    let late = Plan {
        start_at: Some("fees".into()),
        skip: vec!["calibrate".into(), "load".into()],
    };
    assert!(late.check(&names).is_ok());
    assert_eq!(Gates::steps().len(), STEPS.len());
}

#[test]
fn the_summary_lists_every_step_and_says_how_to_carry_on() {
    let results = vec![
        StepResult {
            name: "node",
            outcome: Some(Outcome::Pass("synced".into())),
            seconds: 2.0,
        },
        StepResult {
            name: "replica",
            outcome: Some(Outcome::Open("nothing to replay yet".into())),
            seconds: 75.0,
        },
        StepResult {
            name: "history",
            outcome: Some(Outcome::Fail("2 pools did not replay".into())),
            seconds: 4_000.0,
        },
        StepResult {
            name: "fees",
            outcome: None,
            seconds: 0.0,
        },
        StepResult {
            name: "load",
            outcome: Some(Outcome::Skipped("left out on request".into())),
            seconds: 0.0,
        },
    ];
    let lines = summary(&results);
    let text = lines.join("\n");
    assert!(lines.iter().any(|l| l.starts_with("  node")
        && l.contains("pass")
        && l.contains("2 s")
        && l.ends_with("synced")));
    assert!(lines
        .iter()
        .any(|l| l.contains("history") && l.contains("FAIL") && l.contains("1 h 6 min")));
    assert!(lines.iter().any(|l| l.trim() == "fees       not run"));
    assert!(
        text.contains("1 passed, 1 open, 1 failed, 1 left out, 1 not run, in 1 h 7 min."),
        "{text}"
    );
    assert!(text.contains("could not be decided"), "{text}");
    assert!(text.contains("make gates STEP=history"), "{text}");

    let clean = summary(&results[..1]);
    assert_eq!(clean.len(), 5);
    assert!(clean[4].starts_with("1 passed, 0 open, 0 failed"));
}

#[test]
fn durations_read_naturally() {
    assert_eq!(took(0.2), "0 s");
    assert_eq!(took(59.4), "59 s");
    assert_eq!(took(200.0), "3 min 20 s");
    assert_eq!(took(4_320.0), "1 h 12 min");
}

#[test]
fn the_node_passes_only_when_it_follows_the_chain_past_the_window() {
    let until = day("2026-10-31");
    let now = day("2026-11-16").start() + 500;
    let fresh = now - 12;

    assert_eq!(
        node_verdict(None, 26_450_000, fresh, now, until),
        Outcome::Pass("synced, head block 26450000, 12 s old".into())
    );

    let syncing = Some(SyncProgress {
        current: 100,
        highest: 200,
    });
    let Outcome::Fail(note) = node_verdict(syncing, 100, fresh, now, until) else {
        panic!("a syncing node must not pass");
    };
    assert!(
        note.contains("block 100 of 200") && note.contains("make status"),
        "{note}"
    );
    let unknown = Some(SyncProgress {
        current: 0,
        highest: 0,
    });
    let Outcome::Fail(note) = node_verdict(unknown, 100, fresh, now, until) else {
        panic!("a syncing node must not pass");
    };
    assert!(!note.contains("block 0"), "{note}");

    // Not syncing by its own account, but the head is two days old: the
    // state right after `make up`, before the consensus client catches up.
    let Outcome::Fail(note) = node_verdict(None, 26_414_278, now - 172_800, now, until) else {
        panic!("a stale head must not pass");
    };
    assert!(
        note.contains("48 h 0 min old") && note.contains("consensus client"),
        "{note}"
    );

    // A window that ends today or later is not history yet.
    let Outcome::Fail(note) = node_verdict(None, 1, fresh, now, day("2026-11-16")) else {
        panic!("an unfinished window must not pass");
    };
    assert!(note.contains("2026-11-16"), "{note}");
    // A clock that runs behind the chain does not underflow.
    assert!(matches!(
        node_verdict(None, 1, now + 30, now, until),
        Outcome::Pass(_)
    ));
}

#[test]
fn the_shipped_experiment_needs_history_from_ninety_days_before_its_window() {
    let runs = resolve_experiment(
        &repo().join("experiments/width-and-patience-eth.yml"),
        &RunOverrides::default(),
    )
    .unwrap();
    assert_eq!(span(&runs).unwrap(), (day("2026-01-31"), day("2026-10-31")));
    assert!(span(&[]).is_err());

    // The earliest start and the latest end over all runs.
    let mut mixed = runs[..2].to_vec();
    let window = mixed[1].config.window.as_mut().unwrap();
    window.from = "2026-03-01".into();
    window.to = "2026-11-10".into();
    assert_eq!(
        span(&mixed).unwrap(),
        (day("2025-12-01"), day("2026-11-10"))
    );
}

#[test]
fn the_fee_check_fails_on_a_contradiction_and_stays_open_without_enough_positions() {
    let summary = |exact, close, mismatched| FeeCheckSummary {
        exact,
        close,
        mismatched,
        ..Default::default()
    };
    assert!(matches!(
        fee_outcome("USDC/WETH 0.05", &summary(40, 2, 0), 20),
        Outcome::Pass(note) if note.contains("40 emptied") && note.contains("0.1%")
    ));
    assert!(matches!(
        fee_outcome("USDC/WETH 0.05", &summary(40, 2, 1), 20),
        Outcome::Fail(note) if note.contains("1 position")
    ));
    assert!(matches!(
        fee_outcome("USDC/WETH 0.05", &summary(3, 0, 0), 20),
        Outcome::Open(note) if note.contains("3 emptied") && note.contains("asks for 20")
    ));
}

#[test]
fn several_outcomes_become_the_worst_of_them() {
    let pass = Outcome::Pass("a fine".into());
    let open = Outcome::Open("b unknown".into());
    let fail = Outcome::Fail("c wrong".into());
    assert_eq!(
        combine(vec![pass.clone(), fail.clone(), open.clone()]),
        Outcome::Fail("c wrong; b unknown; a fine".into())
    );
    assert_eq!(
        combine(vec![pass.clone(), open]),
        Outcome::Open("b unknown; a fine".into())
    );
    assert_eq!(combine(vec![pass.clone()]), pass);
    assert!(matches!(combine(vec![]), Outcome::Skipped(_)));
}

#[test]
fn a_calibration_is_summed_up_by_what_ships() {
    assert_eq!(
        what_ships(&[("wait", 3), ("adaptive", 1)]),
        "always-wait ships in 3, the adaptive decision ships in 1"
    );
}

#[test]
fn only_the_host_of_an_endpoint_is_shown() {
    assert_eq!(
        host_of("https://ethereum-rpc.publicnode.com"),
        "ethereum-rpc.publicnode.com"
    );
    assert_eq!(
        host_of("https://rpc.example.org/v2/SECRETKEY?x=1"),
        "rpc.example.org"
    );
    assert_eq!(
        host_of("https://user:pass@rpc.example.org:8443/key"),
        "rpc.example.org:8443"
    );
    assert_eq!(host_of("http://127.0.0.1:8545"), "127.0.0.1:8545");
    assert_eq!(host_of("localhost:8545"), "localhost:8545");
}

// --- against a mock node ------------------------------------------------

fn quantity(n: u64) -> String {
    format!("0x{n:x}")
}

#[test]
fn every_pool_the_runs_name_is_checked() {
    let (mut gates, root) = demo_gates("twopools", None);
    let (_, tokens) = chain_addresses(&gates.runs[0].config, "ethereum").unwrap();
    crate::run_cmd::tests::store_other_pool(&root.join("lake"), &tokens, 11);
    let other = crate::run_cmd::tests::OTHER_POOL;
    let demo = crate::demo_cmd::DEMO_POOL;
    for (run, pool) in gates.runs.iter_mut().zip([demo, other]) {
        run.config.strategies[0].extra.insert(
            "pool".into(),
            serde_yaml::Value::String(pool.to_checksum(None)),
        );
    }
    let name = "USDC/WETH 0.05".to_string();
    assert_eq!(
        gates.traded_pools().unwrap(),
        [(demo, name.clone()), (other, name)]
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// A node whose newest block is `age` seconds old.
fn live_node(chain_id: u64, syncing: Value, age: u64, pool_word: &'static str) -> MockNode {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    MockNode::start(move |method, _| match method {
        "eth_chainId" => Ok(json!(quantity(chain_id))),
        "eth_syncing" => Ok(syncing.clone()),
        "eth_blockNumber" => Ok(json!(quantity(26_450_000))),
        "eth_getBlockByNumber" => Ok(json!({
            "timestamp": quantity(now - age),
            "hash": format!("0x{:064x}", 7),
        })),
        // Every question to the factory gets the same answer.
        "eth_call" => Ok(json!(pool_word)),
        other => Err((-32601, format!("{other} is not served"))),
    })
}

const SOME_POOL: &str = "0x00000000000000000000000088e6a0c2ddd26feeb64f039a2c41296fcb3f5640";
const NO_POOL: &str = "0x0000000000000000000000000000000000000000000000000000000000000000";

#[test]
fn the_node_step_passes_a_synced_node_and_stops_at_one_that_is_not() {
    let (mut gates, root) = demo_gates("node", None);
    let mut ask = |node: &MockNode| {
        gates.chain = None;
        gates.options.rpc = Some(node.url().to_string());
        gates.node()
    };

    let synced = live_node(1, json!(false), 6, SOME_POOL);
    assert!(matches!(
        ask(&synced).unwrap(),
        Outcome::Pass(note) if note.starts_with("synced, head block 26450000")
    ));

    let behind = live_node(
        1,
        json!({"currentBlock": "0x64", "highestBlock": "0xc8"}),
        6,
        SOME_POOL,
    );
    assert!(matches!(
        ask(&behind).unwrap(),
        Outcome::Fail(note) if note.contains("block 100 of 200")
    ));

    let stale = live_node(1, json!(false), 7_200, SOME_POOL);
    assert!(matches!(
        ask(&stale).unwrap(),
        Outcome::Fail(note) if note.contains("2 h 0 min old")
    ));

    let other_chain = live_node(5, json!(false), 6, SOME_POOL);
    let error = format!("{:#}", ask(&other_chain).unwrap_err());
    assert!(error.contains("serves chain id 5"), "{error}");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_database_that_does_not_answer_stops_the_gates_at_the_node_step() {
    let url = "postgres://lpbot:topsecret@127.0.0.1:9/lpbot";
    let (mut gates, root) = demo_gates("nodedb", Some(url.into()));
    let synced = live_node(1, json!(false), 6, SOME_POOL);
    gates.options.rpc = Some(synced.url().to_string());
    let Outcome::Fail(note) = gates.node().unwrap() else {
        panic!("the database is needed at the end");
    };
    assert!(
        note.contains("make viewer") && !note.contains("topsecret"),
        "{note}"
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn the_pools_step_lists_what_the_factory_has() {
    let (mut gates, root) = demo_gates("pools", None);
    let with_pools = live_node(1, json!(false), 6, SOME_POOL);
    gates.options.rpc = Some(with_pools.url().to_string());
    // Six pairs of the four universe tokens at four fee tiers.
    assert_eq!(
        gates.pools().unwrap(),
        Outcome::Pass("24 pools in the universe".into())
    );
    // Discovered once, then remembered.
    assert_eq!(gates.discovered().unwrap().len(), 24);

    let (mut gates, other_root) = demo_gates("nopools", None);
    let empty = live_node(1, json!(false), 6, NO_POOL);
    gates.options.rpc = Some(empty.url().to_string());
    assert!(matches!(
        gates.pools().unwrap(),
        Outcome::Fail(note) if note.contains("factory")
    ));
    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_dir_all(&other_root);
}

fn options(root: &Path, db: Option<String>) -> Options {
    Options {
        chain_name: "ethereum".into(),
        // No node listens here: a step that needs one fails at once.
        rpc: Some("http://127.0.0.1:9".into()),
        root: root.join("lake"),
        results: root.join("runs"),
        db,
        history_from: None,
        blocks: 2_000,
        log_chunk: 2_000,
        positions: 20,
        fit_days: None,
        score_days: None,
    }
}

/// The demo history and the first runs of the demo experiment, on a
/// short window so the test stays quick.
fn demo_gates(label: &str, db: Option<String>) -> (Gates, PathBuf) {
    let root = std::env::temp_dir().join(format!("lpbot-gates-{label}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let mut runs = resolve_experiment(
        &repo().join("experiments/demo-patience.yml"),
        &RunOverrides::default(),
    )
    .unwrap();
    runs.truncate(2);
    for run in &mut runs {
        run.config.window.as_mut().unwrap().to = "2026-01-10".into();
    }
    let (_, tokens) = chain_addresses(&runs[0].config, "ethereum").unwrap();
    crate::demo_cmd::run(&root.join("lake"), "ethereum", &tokens, 11, 1).unwrap();
    let gates = Gates::new(runs, options(&root, db)).unwrap();
    (gates, root)
}

#[test]
fn on_stored_history_the_steps_that_need_no_node_run_through() {
    let (mut gates, root) = demo_gates("local", None);
    assert_eq!(
        gates.traded_pools().unwrap(),
        [(crate::demo_cmd::DEMO_POOL, "USDC/WETH 0.05".to_string())]
    );
    // The demo window starts on 8 January and looks back 90 days.
    assert_eq!(gates.history_start().unwrap(), day("2025-10-10"));

    let plan = Plan {
        start_at: Some("fees".into()),
        skip: vec!["calibrate".into()],
    };
    let results = run_steps(Gates::steps(), &mut gates, &plan).unwrap();
    assert_eq!(
        STEPS.map(|(name, _)| name),
        [
            "node",
            "pools",
            "replica",
            "history",
            "fees",
            "backtest",
            "load",
            "calibrate"
        ]
    );
    assert_eq!(
        statuses(&results),
        ["left out", "left out", "left out", "left out", "open", "pass", "left out", "left out"]
    );
    // The demo pool has no Collect events, so nothing confirms the fees.
    assert!(
        matches!(&results[4].outcome, Some(Outcome::Open(note)) if note.contains("asks for 20"))
    );
    assert_eq!(
        results[5].outcome,
        Some(Outcome::Pass(format!(
            "2 runs written to {}",
            root.join("runs").display()
        )))
    );
    // Without a database the load says so and does not fail.
    assert!(
        matches!(&results[6].outcome, Some(Outcome::Skipped(note)) if note.contains("no database"))
    );
    assert_eq!(
        results[7].outcome,
        Some(Outcome::Skipped("left out on request".into()))
    );
    for run in &gates.runs {
        let table = lpbot_sim::tables::table_path(&root.join("runs"), "runs", &run.run_id);
        assert!(table.is_file(), "{}", table.display());
    }
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn without_a_node_the_first_step_fails_and_says_where_to_look() {
    let (mut gates, root) = demo_gates("nonode", None);
    let results = run_steps(Gates::steps(), &mut gates, &Plan::default()).unwrap();
    assert_eq!(failed_step(&results), Some("node"));
    let Some(Outcome::Fail(note)) = &results[0].outcome else {
        panic!("the node step must fail");
    };
    assert!(note.contains("make status"), "{note}");
    assert!(results[1..].iter().all(|r| r.outcome.is_none()));
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_database_that_is_not_there_fails_the_load_without_showing_its_password() {
    let url = "postgres://lpbot:topsecret@127.0.0.1:9/lpbot";
    let (mut gates, root) = demo_gates("nodb", Some(url.into()));
    let plan = Plan {
        start_at: Some("load".into()),
        skip: vec![],
    };
    let results = run_steps(Gates::steps(), &mut gates, &plan).unwrap();
    let Some(Outcome::Fail(note)) = &results[6].outcome else {
        panic!("the load must fail");
    };
    assert_eq!(results[7].outcome, None);
    assert!(
        note.contains("cannot connect to the database") && !note.contains("topsecret"),
        "{note}"
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn an_experiment_that_is_not_a_backtest_is_refused() {
    let mut runs = resolve_experiment(
        &repo().join("experiments/demo-patience.yml"),
        &RunOverrides::default(),
    )
    .unwrap();
    runs.truncate(1);
    runs[0].config.mode = Mode::Paper;
    let error = Gates::new(runs, options(Path::new("data"), None))
        .err()
        .expect("a paper run is not a gate")
        .to_string();
    assert!(error.contains("mode: backtest"), "{error}");
}
