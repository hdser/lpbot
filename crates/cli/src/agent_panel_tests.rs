//! A panel of two agents end to end on the simulated pool history: agent a
//! decides first, agent b reads a's records, and both are scored. Checks
//! the rule about time (b is handed nothing of a later than its own look),
//! the stance a look with peers' records needs, and that scores replay the
//! run exactly.

use std::path::{Path, PathBuf};

use lpbot_core::experiment::{resolve_experiment, ResolvedRun, RunOverrides};
use serde_json::Value;

use crate::agent_cmd::{self, Decision};
use crate::agent_eval::{evaluate, parse_horizons};
use crate::agent_log::{self, Expectation};
use crate::agent_peers;
use crate::chain_cmd::chain_addresses;

struct Panel {
    root: PathBuf,
    lake: PathBuf,
    runs: Vec<ResolvedRun>,
}

impl Drop for Panel {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn panel(label: &str) -> Panel {
    let root = std::env::temp_dir().join(format!("lpbot-panel-{label}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    let base = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../config/mainnet.yml");
    let agents = root.join("agent");
    let file = root.join("panel.yml");
    std::fs::write(
        &file,
        format!(
            "experiment: panel-test\nbase: {}\nmode: backtest\n\
             window: {{ from: 2026-01-08, to: 2026-01-10 }}\n\
             run: {{ capital_usd: 10000, loss_limit_pct: 20, autonomy: auto }}\n\
             strategy: eth-zone-roundtrip\nsweep:\n  capital_share: [1.0]\n  agent:\n    \
             - {{ name: a, model: test-model-a, decisions: {a} }}\n    \
             - {{ name: b, model: test-model-b, decisions: {b}, peers: [a] }}\n",
            base.display(),
            a = agents.join("a.jsonl").display(),
            b = agents.join("b.jsonl").display(),
        ),
    )
    .unwrap();
    let runs = resolve_experiment(&file, &RunOverrides::default()).unwrap();
    assert_eq!(runs.len(), 2);
    let lake = root.join("lake");
    let (_, tokens) = chain_addresses(&runs[0].config, "ethereum").unwrap();
    crate::demo_cmd::run(&lake, "ethereum", &tokens, 10, 1).unwrap();
    Panel { root, lake, runs }
}

fn decision<'a>(action: &'a str, next: &'a str) -> Decision<'a> {
    Decision {
        action,
        lower_pct: None,
        upper_pct: None,
        next,
        note: "test",
        reasoning: "a test decision",
        expect: Expectation::default(),
        stance: None,
        toward: None,
    }
}

/// The look waiting for agent `name`, as its pending file holds it.
fn pending_record(run: &ResolvedRun) -> Value {
    let who = agent_cmd::identity_of(run).unwrap();
    let mut path = who.decisions.as_os_str().to_owned();
    path.push(".pending");
    let pending: Value =
        serde_json::from_str(&std::fs::read_to_string(PathBuf::from(path)).unwrap()).unwrap();
    pending["record"].clone()
}

#[test]
fn two_agents_one_reading_the_other_up_to_its_own_look_and_both_scored() {
    let p = panel("two");
    let (run_a, run_b) = (&p.runs[0], &p.runs[1]);
    let chain = "ethereum";

    // Agent a decides four looks, six hours apart, then twelve.
    assert!(agent_cmd::next(run_a, &p.lake, chain).unwrap().is_some());
    let enter = Decision {
        lower_pct: Some(-1.0),
        upper_pct: Some(1.0),
        expect: Expectation {
            low_pct: Some(-1.5),
            high_pct: Some(1.5),
            confidence: Some(0.6),
            text: "chop".into(),
        },
        ..decision("enter", "6h")
    };
    agent_cmd::decide(run_a, &p.lake, chain, &enter).unwrap();
    agent_cmd::decide(run_a, &p.lake, chain, &decision("hold", "6h")).unwrap();
    agent_cmd::decide(run_a, &p.lake, chain, &decision("exit", "6h")).unwrap();
    agent_cmd::decide(run_a, &p.lake, chain, &decision("stay_out", "12h")).unwrap();
    let who_a = agent_cmd::identity_of(run_a).unwrap();
    let a_looks = agent_log::read_records(&agent_log::records_path(&who_a.decisions)).unwrap();
    assert_eq!(a_looks.len(), 4);
    assert_eq!(a_looks[0]["agent"], "a");
    assert_eq!(a_looks[0]["model"], "test-model-a");
    assert_eq!(a_looks[0]["decision"]["lower_pct"], -1.0);
    assert_eq!(a_looks[0]["expect"]["confidence"], 0.6);
    assert!(
        a_looks[1]["seen"]["position"].is_object(),
        "a holds a position at look 2"
    );
    let a_as_of: Vec<u64> = a_looks
        .iter()
        .map(|r| r["as_of"].as_u64().unwrap())
        .collect();
    assert!(a_as_of.windows(2).all(|w| w[0] < w[1]));

    // Agent b's first look is a's first: it is handed that one only.
    assert!(agent_cmd::next(run_b, &p.lake, chain).unwrap().is_some());
    let b_first = pending_record(run_b);
    let b_as_of = b_first["as_of"].as_u64().unwrap();
    assert_eq!(b_as_of, a_as_of[0]);
    let shown = b_first["shown"].as_array().unwrap();
    assert_eq!(shown.len(), 1, "{shown:?}");
    assert_eq!(shown[0]["look"], a_looks[0]["look"]);

    // It cannot decide without saying how it relates to a, and nothing is
    // written when it is refused.
    let who_b = agent_cmd::identity_of(run_b).unwrap();
    let err = agent_cmd::decide(run_b, &p.lake, chain, &decision("stay_out", "3h"))
        .unwrap_err()
        .to_string();
    assert!(err.contains("--stance"), "{err}");
    assert!(!who_b.decisions.exists());
    let depart = Decision {
        stance: Some("depart"),
        ..decision("stay_out", "3h")
    };
    agent_cmd::decide(run_b, &p.lake, chain, &depart).unwrap();
    let b_looks = agent_log::read_records(&agent_log::records_path(&who_b.decisions)).unwrap();
    assert_eq!(b_looks[0]["stance"]["kind"], "depart");
    assert_eq!(b_looks[0]["stance"]["toward"], "a", "the one peer shown");

    // Three hours on, a's second look (six hours on) is still ahead.
    assert!(pending_record(run_b)["shown"]
        .as_array()
        .unwrap()
        .is_empty());
    agent_cmd::decide(run_b, &p.lake, chain, &decision("stay_out", "4h")).unwrap();
    // Seven hours on it is handed a's second look, and not its third.
    let third = pending_record(run_b);
    let b_as_of = third["as_of"].as_u64().unwrap();
    let shown = third["shown"].as_array().unwrap();
    assert_eq!(shown.len(), 1, "{shown:?}");
    assert_eq!(shown[0]["look"], a_looks[1]["look"]);
    assert!(a_as_of[2] > b_as_of);

    // Scores of a, over the next look and six hours.
    let horizons = parse_horizons("next,6h").unwrap();
    let report = evaluate(run_a, &who_a, &p.lake, chain, &horizons, 2).unwrap();
    assert_eq!(report.scored, 4);
    let scores = agent_log::read_records(&agent_log::scores_path(&who_a.decisions)).unwrap();
    assert_eq!(scores.len(), 8);
    let change = |r: &Value, alt: &str| {
        r["alternatives"]
            .as_array()
            .unwrap()
            .iter()
            .find(|a| a["alt"] == alt)
            .and_then(|a| a["change_pct"].as_f64())
    };
    for r in &scores {
        assert_eq!(r["kind"], "eval");
        assert!(r["as_of"].as_u64() > r["decided_as_of"].as_u64(), "{r}");
        assert!(r["regret_pct"].as_f64().unwrap() >= 0.0, "{r}");
    }
    // Flat, cash is no change at all.
    assert_eq!(change(&scores[0], "cash"), Some(0.0));
    assert_eq!(scores[0]["position_open"], false);
    // The decisions left for six hours were left for their own next look:
    // the replay alone and the run agree exactly.
    for k in 0..3 {
        let (next, six) = (&scores[2 * k], &scores[2 * k + 1]);
        assert_eq!(
            (next["horizon"].as_str(), six["horizon"].as_str()),
            (Some("next"), Some("6h"))
        );
        assert_eq!(
            change(next, "decision"),
            change(six, "decision"),
            "decision {}",
            k + 1
        );
    }
    // Holding, the decision is holding.
    assert_eq!(scores[2]["position_open"], true);
    assert_eq!(scores[2]["edge_vs_hold_pct"].as_f64(), Some(0.0));
    assert!(change(&scores[2], "cash").is_some());
    // Only the decision with a band has its band scored.
    assert!(scores[0]["expect"]["held"].is_boolean(), "{}", scores[0]);
    assert!(scores[2]["expect"].is_null());

    // b is handed a's scores only once their horizon has passed.
    let handed = agent_peers::handover(&who_b, b_as_of, &Default::default()).unwrap();
    let evals: Vec<&Value> = handed
        .shown
        .iter()
        .filter(|s| s["kind"] == "eval")
        .collect();
    assert!(!evals.is_empty(), "{}", handed.text);
    for s in &handed.shown {
        assert!(s["as_of"].as_u64().unwrap() <= b_as_of, "{s}");
    }
    assert!(evals.len() < scores.len());

    // b's own scores: staying out while flat is cash, the best there is.
    let report_b = evaluate(run_b, &who_b, &p.lake, chain, &horizons, 2).unwrap();
    assert_eq!(report_b.scored, 2);
    let (_, next) = &report_b.tallies[0];
    assert_eq!((next.decisions, next.best, next.open), (2, 2, 0));
    assert!(next.regret.abs() < 1e-12 && next.edge_cash.abs() < 1e-12);
    let table = crate::agent_eval::compare(&[report, report_b]);
    assert!(
        table.contains("test-model-a") && table.contains("test-model-b"),
        "{table}"
    );

    // `lpbot run` writes each agent run's looks and scores beside its six
    // tables, for the viewer.
    use lpbot_sim::tables::{read_table, table_path, Column};
    let results = p.root.join("runs");
    crate::run_cmd::run(&p.runs, &p.lake, &results, chain).unwrap();
    let looks = read_table(&table_path(&results, "agent_looks", &run_a.run_id)).unwrap();
    assert_eq!(looks.rows(), 4);
    assert_eq!(
        looks.column("model"),
        Some(&Column::Text(vec![Some("test-model-a".into()); 4]))
    );
    let scored = read_table(&table_path(&results, "agent_scores", &run_a.run_id)).unwrap();
    assert_eq!(scored.rows(), 8);
    let b_looks = read_table(&table_path(&results, "agent_looks", &run_b.run_id)).unwrap();
    assert_eq!(b_looks.column("stance").map(Column::len), Some(2));
    for table in lpbot_sim::RESULT_TABLES {
        assert!(
            table_path(&results, table, &run_b.run_id).is_file(),
            "{table}"
        );
    }
}
