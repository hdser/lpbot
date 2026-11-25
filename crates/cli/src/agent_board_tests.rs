//! The advice board end to end on a forward panel over the simulated pool
//! history: agent a asks, agent b answers, a reads the answers and decides,
//! and the decision records them. Asking is refused in a backtest, a late
//! answer is marked and shown at the asker's next look, and a decision acts
//! no earlier than the newest record it read.

use std::path::{Path, PathBuf};
use std::time::Duration;

use lpbot_core::experiment::{resolve_experiment, ResolvedRun, RunOverrides};
use serde_json::{json, Value};

use super::*;
use crate::agent_cmd::{self, Decision, Forward};
use crate::agent_log::{read_records, records_path, Expectation};
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

/// Two agents deciding the demo pool forward, from 8 January to the end
/// of the stored history.
fn forward_panel(label: &str) -> Panel {
    let root = std::env::temp_dir().join(format!("lpbot-board-{label}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    let base = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../config/mainnet.yml");
    let agents = root.join("agent");
    let file = root.join("panel.yml");
    std::fs::write(
        &file,
        format!(
            "experiment: board-test\nbase: {}\nmode: backtest\n\
             window: {{ from: 2026-01-08, to: open }}\n\
             run: {{ capital_usd: 10000, loss_limit_pct: 20, autonomy: auto }}\n\
             strategy: eth-zone-roundtrip\nsweep:\n  pool: [\"{pool}\"]\n  capital_share: [1.0]\n  \
             agent:\n    \
             - {{ name: a, model: test-model-a, decisions: {a} }}\n    \
             - {{ name: b, model: test-model-b, decisions: {b}, peers: [a] }}\n",
            base.display(),
            pool = crate::demo_cmd::DEMO_POOL.to_checksum(None),
            a = agents.join("a.jsonl").display(),
            b = agents.join("b.jsonl").display(),
        ),
    )
    .unwrap();
    let runs = resolve_experiment(&file, &RunOverrides::default()).unwrap();
    assert_eq!(runs.len(), 2);
    let lake = root.join("lake");
    let (_, tokens) = chain_addresses(&runs[0].config, "ethereum").unwrap();
    crate::demo_cmd::run(&lake, "ethereum", &tokens, 12, 1).unwrap();
    Panel { root, lake, runs }
}

/// Forward, on what is stored: no node in a unit test.
fn stored() -> Forward {
    Forward {
        sync: false,
        rpc: None,
        max_lag: u64::MAX,
        wait: 0,
    }
}

fn identities(runs: &[ResolvedRun]) -> Vec<Identity> {
    runs.iter()
        .map(|r| {
            crate::agent_log::identity(&r.config.strategies[0])
                .unwrap()
                .unwrap()
        })
        .collect()
}

fn stay_out<'a>() -> Decision<'a> {
    Decision {
        action: "stay_out",
        lower_pct: None,
        upper_pct: None,
        next: "1h",
        note: "test",
        reasoning: "a test decision",
        expect: Expectation::default(),
        stance: None,
        toward: None,
    }
}

#[test]
fn an_agent_asks_another_reads_its_answer_and_its_decision_records_both() {
    let p = forward_panel("ask");
    let (run_a, run_b) = (&p.runs[0], &p.runs[1]);
    let panel = identities(&p.runs);
    let (who_a, who_b) = (&panel[0], &panel[1]);
    let chain = "ethereum";
    assert!(agent_cmd::next(run_a, &p.lake, chain, &stored())
        .unwrap()
        .is_some());
    assert!(agent_cmd::next(run_b, &p.lake, chain, &stored())
        .unwrap()
        .is_some());
    let at = read_pending(&who_a.decisions).unwrap().at.unwrap();

    // A question is due a set time after the look was shown, by the clock.
    let now = at + 30;
    let ask = ask_at(run_a, &panel, "all", " Is the floor holding? ", 600, now).unwrap();
    assert_eq!(ask["id"], "a-1-1");
    assert_eq!(ask["to"], json!(["b"]));
    assert_eq!(ask["question"], "Is the floor holding?");
    assert_eq!(
        (ask["as_of"].as_u64(), ask["deadline"].as_u64()),
        (Some(at), Some(at + 600))
    );
    assert!(ask["seen_text"].as_str().unwrap().starts_with("Look 1"));
    let late = ask_at(run_a, &panel, "b", "too late?", 600, at + 700).unwrap_err();
    assert!(late.to_string().contains("already late"), "{late}");
    assert!(ask_at(run_a, &panel, "c", "who?", 600, now).is_err());
    let second = ask_at(run_a, &panel, "b", "And the volume?", 600, now + 5).unwrap();
    assert_eq!(second["id"], "a-1-2");

    // b finds both in its inbox, with a's look, and answers one in time.
    let board = read_board(&board_path(&who_a.decisions)).unwrap();
    let inbox = inbox_text(&board, "b", now + 10);
    assert!(
        inbox.contains("Question a-1-1 from a (test-model-a)"),
        "{inbox}"
    );
    assert!(inbox.contains("Its look, as a saw it:"), "{inbox}");
    assert!(inbox_text(&board, "a", now + 10).contains("No open question to a."));
    let would = Would {
        action: "enter".into(),
        lower_pct: Some(-3.0),
        upper_pct: Some(-1.0),
    };
    let answer = answer_at(
        run_b,
        "a-1-1",
        "Yes: bid below it.",
        Some(&would),
        Some(0.6),
        now + 60,
    )
    .unwrap();
    assert_eq!(answer["late"], false);
    assert_eq!(answer["would"]["lower_pct"], -3.0);
    let b_at = read_pending(&who_b.decisions).unwrap().at.unwrap();
    assert_eq!(answer["as_of"].as_u64(), Some(b_at.max(at)));
    let again = answer_at(run_b, "a-1-1", "Changed my mind", None, None, now + 70).unwrap_err();
    assert!(again.to_string().contains("already"), "{again}");
    assert!(
        answer_at(run_a, "a-1-1", "self", None, None, now).is_err(),
        "not to a"
    );
    assert!(answer_at(run_b, "a-9-9", "what?", None, None, now).is_err());
    assert!(answer_at(run_b, "a-1-1", "  ", None, None, now).is_err());

    // a reads the answers and decides: its record holds the questions, the
    // answer it read and who did not answer.
    answers(run_a, false, Duration::from_millis(10)).unwrap();
    let read = read_pending(&who_a.decisions).unwrap().record.unwrap()["board_read"].clone();
    assert_eq!(read[0]["key"], "answer:a-1-1:b");
    agent_cmd::decide(run_a, &p.lake, chain, &stay_out(), &stored()).unwrap();
    let records = read_records(&records_path(&who_a.decisions)).unwrap();
    let advice = &records[0]["advice"];
    assert_eq!(advice["asked"].as_array().unwrap().len(), 2);
    assert_eq!(advice["answers"][0]["agent"], "b");
    assert_eq!(advice["answers"][0]["read"], true);
    assert_eq!(
        advice["unanswered"],
        json!([{"ask": "a-1-2", "agent": "b"}])
    );
    let log = std::fs::read_to_string(agent_cmd::log_path(&who_a.decisions)).unwrap();
    assert!(
        log.contains("Asked b (a-1-1): Is the floor holding?"),
        "{log}"
    );
    assert!(
        log.contains("Answer from b (on time, would enter -3.00% to -1.00%)"),
        "{log}"
    );
    let decisions = std::fs::read_to_string(&who_a.decisions).unwrap();
    let line: Value = serde_json::from_str(decisions.lines().next().unwrap()).unwrap();
    assert_eq!(
        line["at"].as_u64(),
        Some(at.max(b_at)),
        "acts where the newest answer is"
    );

    // b answers the other one after its deadline: kept, marked late, and
    // shown at a's next look, once.
    let late = answer_at(run_b, "a-1-2", "The volume is thin.", None, None, at + 900).unwrap();
    assert_eq!(late["late"], true);
    let mut record = json!({});
    let text = late_section(who_a, &records, &mut record).unwrap();
    assert!(text.contains("after their deadline"), "{text}");
    assert!(text.contains("The volume is thin."), "{text}");
    assert_eq!(record["late_shown"], json!(["answer:a-1-2:b"]));
    let shown = vec![json!({"advice": {"late_shown": ["answer:a-1-2:b"]}})];
    assert_eq!(late_section(who_a, &shown, &mut json!({})).unwrap(), "");

    let rates = summary(&panel).unwrap();
    assert!(rates.contains("2 question(s), 2 answer(s)"), "{rates}");
    assert!(
        rates.contains(
            "b (test-model-b): asked 0; of 2 question(s) to it, answered 1 on time and 1 late"
        ),
        "{rates}"
    );
}

#[test]
fn a_decision_acts_no_earlier_than_the_newest_answer_it_read() {
    let p = forward_panel("newest");
    let run_a = &p.runs[0];
    let panel = identities(&p.runs);
    let who_a = &panel[0];
    assert!(agent_cmd::next(run_a, &p.lake, "ethereum", &stored())
        .unwrap()
        .is_some());
    let at = read_pending(&who_a.decisions).unwrap().at.unwrap();
    ask_at(run_a, &panel, "b", "What do you see?", 600, at + 10).unwrap();
    // b has seen a later market than a's look when it answers.
    append(&board_path(&who_a.decisions), |_| {
        Ok(
            json!({"schema": crate::agent_log::SCHEMA, "kind": "answer", "ask": "a-1-1",
                  "asker": "a", "agent": "b", "model": "test-model-b", "answer": "A drop.",
                  "as_of": at + 120, "late": false}),
        )
    })
    .unwrap();
    answers(run_a, false, Duration::from_millis(10)).unwrap();
    agent_cmd::decide(run_a, &p.lake, "ethereum", &stay_out(), &stored()).unwrap();
    let decisions = std::fs::read_to_string(&who_a.decisions).unwrap();
    let line: Value = serde_json::from_str(decisions.lines().next().unwrap()).unwrap();
    assert_eq!(line["at"].as_u64(), Some(at + 120));
    let record = &read_records(&records_path(&who_a.decisions)).unwrap()[0];
    assert_eq!(record["as_of"].as_u64(), Some(at + 120));
}

#[test]
fn answering_an_older_question_carries_the_newer_market_seen_in_its_inbox() {
    let p = forward_panel("answer-inbox-knowledge");
    let run_b = &p.runs[1];
    let panel = identities(&p.runs);
    let who_b = &panel[1];
    assert!(agent_cmd::next(run_b, &p.lake, "ethereum", &stored())
        .unwrap()
        .is_some());
    let at = read_pending(&who_b.decisions).unwrap().at.unwrap();
    let now = crate::agent_log::now_unix();
    // Newer information comes first, so the last question read cannot be
    // used as a shortcut for the newest market the inbox disclosed.
    for (id, as_of) in [("a-2-1", at + 120), ("a-1-1", at - 120)] {
        append(&board_path(&who_b.decisions), |_| {
            Ok(
                json!({"kind": "ask", "id": id, "agent": "a", "model": "test-model-a",
                      "to": ["b"], "question": "What do you see?", "look": as_of,
                      "as_of": as_of, "deadline": now + 600, "asked_unix": now}),
            )
        })
        .unwrap();
    }
    assert_eq!(inbox(run_b, 0, Duration::from_millis(10)).unwrap(), 2);
    let read = read_pending(&who_b.decisions).unwrap().record.unwrap()["board_read"].clone();
    assert_eq!(read[0]["as_of"].as_u64(), Some(at + 120));
    assert_eq!(read[1]["as_of"].as_u64(), Some(at - 120));
    let answer = answer_at(run_b, "a-1-1", "My view includes both.", None, None, now).unwrap();
    assert_eq!(answer["as_of"].as_u64(), Some(at + 120));
}

#[test]
fn asking_is_refused_in_a_backtest() {
    let p = forward_panel("backtest");
    let mut run_a = p.runs[0].clone();
    run_a.config.window.as_mut().unwrap().to = "2026-01-10".into();
    let panel = identities(&p.runs);
    assert!(agent_cmd::next(&run_a, &p.lake, "ethereum", &stored())
        .unwrap()
        .is_some());
    let err = ask_at(&run_a, &panel, "b", "Will it hold?", 600, 0)
        .unwrap_err()
        .to_string();
    assert!(err.contains("forward and live panels"), "{err}");
    let err = answer_at(&run_a, "b-1-1", "yes", None, None, 0)
        .unwrap_err()
        .to_string();
    assert!(err.contains("lockstep panels"), "{err}");
}

#[test]
fn a_waiting_inbox_returns_as_soon_as_a_question_arrives_and_answers_once_all_are_in() {
    let p = forward_panel("wait");
    let (run_a, run_b) = (p.runs[0].clone(), p.runs[1].clone());
    let panel = identities(&p.runs);
    let board = board_path(&panel[0].decisions);
    assert!(agent_cmd::next(&run_a, &p.lake, "ethereum", &stored())
        .unwrap()
        .is_some());
    let look = read_pending(&panel[0].decisions).unwrap().look;
    let poll = Duration::from_millis(20);

    // b waits up to a minute; the question comes after a fifth of a second.
    let started = std::time::Instant::now();
    let waiting = std::thread::spawn(move || inbox(&run_b, 60, poll).unwrap());
    std::thread::sleep(Duration::from_millis(200));
    let due = crate::agent_log::now_unix() + 600;
    append(&board, |_| {
        Ok(
            json!({"kind": "ask", "id": "a-1-1", "agent": "a", "model": "test-model-a",
                  "to": ["b"], "question": "Anyone there?", "look": look, "as_of": 0,
                  "deadline": due, "asked_unix": 0}),
        )
    })
    .unwrap();
    assert_eq!(waiting.join().unwrap(), 1);
    assert!(
        started.elapsed() < Duration::from_secs(30),
        "it did not wait out the minute"
    );

    // a waits for its answers; b's comes after a fifth of a second.
    let started = std::time::Instant::now();
    let reading = std::thread::spawn(move || answers(&run_a, true, poll).unwrap());
    std::thread::sleep(Duration::from_millis(200));
    append(&board, |_| {
        Ok(
            json!({"kind": "answer", "ask": "a-1-1", "asker": "a", "agent": "b",
                  "model": "test-model-b", "answer": "Here.", "as_of": 0, "late": false}),
        )
    })
    .unwrap();
    reading.join().unwrap();
    assert!(
        started.elapsed() < Duration::from_secs(30),
        "it returned once all had answered"
    );
    let read = read_pending(&panel[0].decisions).unwrap().record.unwrap()["board_read"].clone();
    assert_eq!(read[0]["key"], "answer:a-1-1:b");
}
