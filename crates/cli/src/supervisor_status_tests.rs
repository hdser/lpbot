//! The status's rules, and a panel on disk with fake processes.

use std::path::{Path, PathBuf};

use lpbot_data::day::Day;
use lpbot_sim::loader::copy_text;

use super::*;
use crate::supervisor_procs::Proc;

const NOW: u64 = 1_795_700_000;
const HOUR: u64 = 3_600;

fn facts() -> Facts {
    Facts {
        now: NOW,
        grace: 600,
        listed: Listed::Active,
        read_error: None,
        stopped: None,
        runner_error: None,
        owned: false,
        scheduler: Scheduler::Session { here: true },
        due: NOW + HOUR,
        look: 12,
        first_shown: None,
        next_until: Some(NOW + 2 * HOUR),
        answers_until: None,
        busy: false,
    }
}

fn state(f: &Facts) -> State {
    classify(f).0
}

#[test]
fn a_look_ahead_is_waiting_only_with_something_to_wake_it() {
    let f = facts();
    assert_eq!(state(&f), State::Waiting);
    assert!(classify(&f).1.contains("look 12"), "{}", classify(&f).1);
    // The waiter ends before the look is due.
    let short = Facts {
        next_until: Some(NOW + HOUR - 1),
        ..facts()
    };
    let (s, detail) = classify(&short);
    assert_eq!(s, State::NoWaiter);
    assert!(detail.contains("ends at"), "{detail}");
    // No waiter at all, on this machine.
    assert_eq!(
        state(&Facts {
            next_until: None,
            ..facts()
        }),
        State::NoWaiter
    );
    // Run from elsewhere: nothing here can tell.
    let away = Facts {
        next_until: None,
        scheduler: Scheduler::Session { here: false },
        ..facts()
    };
    assert_eq!(state(&away), State::Unverified);
    // A waiter reaching the due time counts wherever the list says it runs.
    let found = Facts {
        scheduler: Scheduler::Session { here: false },
        ..facts()
    };
    assert_eq!(state(&found), State::Waiting);
    // Exactly at the due time it is waiting still.
    assert_eq!(
        state(&Facts {
            next_until: Some(NOW + HOUR),
            ..facts()
        }),
        State::Waiting
    );
}

#[test]
fn lateness_counts_from_the_due_time_however_often_the_look_is_shown() {
    let due = NOW - 600;
    let at_grace = Facts {
        due,
        next_until: None,
        ..facts()
    };
    assert_eq!(state(&at_grace), State::Due);
    let shown = Facts {
        first_shown: Some(due + 30),
        ..at_grace.clone()
    };
    assert_eq!(state(&shown), State::Deciding);
    let busy = Facts {
        busy: true,
        ..at_grace.clone()
    };
    assert_eq!(state(&busy), State::Deciding);
    // One second past the grace: late, shown or not.
    let late = Facts {
        due: due - 1,
        first_shown: Some(NOW - 5),
        ..at_grace.clone()
    };
    let (s, detail) = classify(&late);
    assert_eq!(s, State::Late);
    assert!(detail.contains("late by 10 min"), "{detail}");
    assert!(detail.contains("first shown"), "{detail}");
    assert!(detail.contains("no agent next --wait"), "{detail}");
    let nobody = Facts {
        due: NOW - 3 * HOUR,
        ..at_grace
    };
    let (s, detail) = classify(&nobody);
    assert_eq!(s, State::Late);
    assert!(detail.contains("nothing has shown it"), "{detail}");
}

#[test]
fn answers_keep_a_look_deciding_until_their_deadline() {
    let waiting = Facts {
        due: NOW - 2 * HOUR,
        first_shown: Some(NOW - 2 * HOUR),
        answers_until: Some(NOW + 300),
        next_until: None,
        ..facts()
    };
    let (s, detail) = classify(&waiting);
    assert_eq!(s, State::Deciding);
    assert!(detail.contains("waiting for answers until"), "{detail}");
    let past = Facts {
        answers_until: Some(NOW - 1),
        ..waiting
    };
    assert_eq!(state(&past), State::Late);
}

#[test]
fn stops_fences_and_the_list_come_before_the_clock() {
    let late = Facts {
        due: NOW - 5 * HOUR,
        next_until: None,
        ..facts()
    };
    let paused = Facts {
        listed: Listed::Paused("held on purpose".into()),
        stopped: Some("x".into()),
        ..late.clone()
    };
    assert_eq!(classify(&paused), (State::Paused, "held on purpose".into()));
    let ended = Facts {
        listed: Listed::Ended("window over".into()),
        ..late.clone()
    };
    assert_eq!(state(&ended), State::Ended);
    let unreadable = Facts {
        read_error: Some("the last line is unterminated".into()),
        ..late.clone()
    };
    assert_eq!(state(&unreadable), State::Failing);
    let stopped = Facts {
        stopped: Some("committed_decide_inspection_required".into()),
        owned: true,
        scheduler: Scheduler::Runner { process: false },
        ..late.clone()
    };
    let (s, detail) = classify(&stopped);
    assert_eq!(s, State::Stopped);
    assert!(
        detail.contains("committed_decide_inspection_required"),
        "{detail}"
    );
    let down = Facts {
        owned: true,
        scheduler: Scheduler::Runner { process: false },
        due: NOW + HOUR,
        ..late.clone()
    };
    assert_eq!(state(&down), State::SchedulerDown);
    let up = Facts {
        scheduler: Scheduler::Runner { process: true },
        ..down.clone()
    };
    assert_eq!(state(&up), State::Waiting);
    let failing = Facts {
        runner_error: Some("node behind".into()),
        ..up
    };
    assert_eq!(state(&failing), State::Failing);
    assert!(State::SchedulerDown.needs_attention());
    assert!(!State::Paused.needs_attention());
}

#[test]
fn the_memory_keeps_the_first_sighting_and_forgets_decided_looks() {
    let mut memory = Memory::default();
    assert_eq!(memory.shown("r", 100, 500, Some(450)), 450);
    // Shown again later, its file rewritten: the first sighting stays.
    assert_eq!(memory.shown("r", 100, 900, Some(880)), 450);
    assert_eq!(memory.shown("r", 200, 900, None), 900);
    assert_eq!(memory.shown("other", 100, 1_000, Some(2_000)), 1_000);
    memory.states.insert("r".into(), "late".into());
    let back = Memory::parse(&memory.to_json());
    assert_eq!(back, memory);
    memory.forget_before("r", 200);
    assert!(!memory.first_shown.contains_key("r 100"));
    assert!(memory.first_shown.contains_key("r 200"));
    assert!(memory.first_shown.contains_key("other 100"));
    assert_eq!(Memory::parse("not json"), Memory::default());
}

#[test]
fn open_windows_are_found_as_refresh_sh_finds_them() {
    assert!(mentions_open_window(
        "window: { from: 2026-11-25, to: open }"
    ));
    assert!(mentions_open_window(
        "window:\n  from: 2026-11-25\n  to:   open\n"
    ));
    assert!(!mentions_open_window(
        "window: { from: 2026-01-01, to: 2026-02-01 }"
    ));
    assert!(!mentions_open_window("go_to: open"));
    assert!(!mentions_open_window("to: openly"));
}

#[test]
fn times_and_spans_read_plainly() {
    assert_eq!(clock(1_795_604_280), "2026-11-25 10:58 UTC");
    assert_eq!(span(45), "45 s");
    assert_eq!(span(600), "10 min");
    assert_eq!(span(3 * HOUR + 1_800), "3.5 h");
    assert_eq!(span(3 * 86_400), "3.0 d");
}

/// A forward panel of three agents on disk: a decided once, b never, and c
/// listed but not in the experiment.
struct Disk {
    root: PathBuf,
}

impl Drop for Disk {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

const AT: u64 = 1_795_600_000;

fn disk(label: &str) -> Disk {
    let root = std::env::temp_dir().join(format!("lpbot-status-{label}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("experiments")).unwrap();
    std::fs::create_dir_all(root.join("data/agent/p")).unwrap();
    let base = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../config/mainnet.yml");
    let experiment = |name: &str, window: &str, decisions: &[(&str, &str)]| {
        let agents: String = decisions
            .iter()
            .map(|(agent, model)| {
                format!(
                    "    - {{ name: {agent}, model: {model}, decisions: {} }}\n",
                    root.join(format!("data/agent/p/{agent}.jsonl")).display()
                )
            })
            .collect();
        std::fs::write(
            root.join(format!("experiments/{name}.yml")),
            format!(
                "experiment: {name}\nbase: {}\nmode: backtest\nwindow: {window}\n\
                 run: {{ capital_usd: 10000, loss_limit_pct: 20, autonomy: auto }}\n\
                 strategy: eth-zone-roundtrip\nsweep:\n  capital_share: [1.0]\n  agent:\n{agents}",
                base.display()
            ),
        )
        .unwrap();
    };
    experiment(
        "panel",
        "{ from: 2026-11-20, to: open }",
        &[("a", "model-a"), ("b", "model-b")],
    );
    // An open panel nobody listed and nobody decided, and a closed one.
    experiment(
        "idle",
        "{ from: 2026-11-20, to: open }",
        &[("x", "model-x")],
    );
    experiment(
        "closed",
        "{ from: 2026-01-01, to: 2026-01-05 }",
        &[("a", "model-a")],
    );
    std::fs::write(
        root.join("data/agent/p/a.jsonl"),
        format!("{{\"look\":{},\"at\":{AT},\"action\":\"enter\",\"lower\":-2,\"upper\":-0.5,\"lower_pct\":-2.0,\"upper_pct\":-0.5,\"next\":\"4h\",\"note\":\"n\"}}\n", AT - 60),
    )
    .unwrap();
    Disk { root }
}

fn listed(root: &Path) -> Checked {
    AgentList::parse(
        "grace: 10m\npanels:\n  - experiment: experiments/panel.yml\n    board: true\n    agents:\n      \
         - { name: a, place: here, waits_here: true }\n      \
         - { name: b, waits_here: true }\n      - { name: c }\n",
    )
    .unwrap()
    .check(root)
    .unwrap()
}

struct Fake(Vec<Proc>);

impl Processes for Fake {
    fn list(&self) -> Option<Vec<Proc>> {
        Some(self.0.clone())
    }
}

fn process(root: &Path, pid: u32, args: &str, started: u64) -> Proc {
    Proc {
        pid,
        argv: args.split(' ').map(str::to_string).collect(),
        cwd: Some(root.to_path_buf()),
        started: Some(started),
    }
}

fn by_agent<'a>(rows: &'a [AgentStatusRow], agent: &str) -> &'a AgentStatusRow {
    rows.iter().find(|r| r.agent == agent).unwrap()
}

#[test]
fn a_panel_on_disk_shows_every_listed_agent_with_its_state() {
    let disk = disk("panel");
    let root = &disk.root;
    let now = AT + HOUR;
    let procs = Fake(vec![
        process(
            root,
            41,
            "target/release/lpbot agent next experiments/panel.yml --agent a --wait 4h",
            AT + 120,
        ),
        // Someone else's experiment, and not an lpbot process.
        process(
            root,
            42,
            "target/release/lpbot agent next experiments/idle.yml --agent x --wait 1d",
            AT,
        ),
        process(root, 43, "sleep 100", AT),
    ]);
    let mut memory = Memory::default();
    let (rows, seen) = rows(
        root,
        Path::new("experiments"),
        &listed(root),
        &procs,
        &mut memory,
        now,
        600,
    )
    .unwrap();
    assert!(seen);
    // a, b and c; the idle and closed panels are not shown.
    assert_eq!(rows.len(), 3, "{rows:?}");
    assert!(rows.iter().all(|r| r.expected == 3));

    let a = by_agent(&rows, "a");
    assert_eq!(a.state, "waiting", "{}", a.detail);
    assert_eq!(a.due, Some(i64::try_from(AT + 4 * HOUR).unwrap()));
    assert_eq!(a.looks, 1);
    assert_eq!(a.last_action.as_deref(), Some("enter"));
    assert_eq!(a.last_range.as_deref(), Some("-2.00% to -0.50%"));
    assert_eq!(a.waiter_kind.as_deref(), Some("next"));
    assert_eq!(a.waiter_pid, Some(41));
    assert_eq!(a.listening, Some(false));
    assert!(
        a.detail.contains("not listening for questions"),
        "{}",
        a.detail
    );
    assert_eq!(a.scheduler, "session: process found");
    assert_eq!(a.place.as_deref(), Some("here"));

    // b never decided: its first look was due at the window's start.
    let b = by_agent(&rows, "b");
    let start = Day::from_ymd(2026, 11, 20).unwrap().start();
    assert_eq!(b.due, Some(i64::try_from(start).unwrap()));
    assert_eq!(b.state, "late", "{}", b.detail);
    assert!(b.detail.contains("nothing has shown it"), "{}", b.detail);

    let c = by_agent(&rows, "c");
    assert_eq!(c.state, "failing");
    assert!(
        c.detail.contains("has no agent run of that name"),
        "{}",
        c.detail
    );

    // The ones that need someone come first.
    assert_eq!(rows.last().unwrap().agent, "a");

    // Changes: everything is new the first time, nothing the second.
    assert_eq!(changes(&rows, &memory).len(), 3);
    memory.states = rows
        .iter()
        .map(|r| (r.run_id.clone(), r.state.clone()))
        .collect();
    assert!(changes(&rows, &memory).is_empty());

    // The table names every agent and the brief line its state.
    let text = table(&rows, now);
    assert!(
        text.contains("AGENT") && text.contains("model-a") && text.contains("in 3.0 h"),
        "{text}"
    );
    assert!(
        brief_line(b).starts_with("b (model-b, panel): late: "),
        "{}",
        brief_line(b)
    );
}

#[test]
fn a_shown_look_stays_late_from_its_due_time_and_answers_hold_it() {
    let disk = disk("shown");
    let root = &disk.root;
    let start = Day::from_ymd(2026, 11, 20).unwrap().start();
    let pending = root.join("data/agent/p/b.jsonl.pending");
    std::fs::write(
        &pending,
        format!("{{\"look\":{start},\"at\":{}}}\n", start + 60),
    )
    .unwrap();
    let mut memory = Memory::default();
    let list = listed(root);
    let run = |memory: &mut Memory, now: u64, procs: Vec<Proc>| {
        rows(
            root,
            Path::new("experiments"),
            &list,
            &Fake(procs),
            memory,
            now,
            600,
        )
        .unwrap()
        .0
    };
    let first = run(&mut memory, AT, vec![]);
    let b = by_agent(&first, "b").clone();
    assert_eq!(b.state, "late");
    let seen = b.first_shown.unwrap();
    // Shown again much later: the first sighting and the lateness stay.
    std::fs::write(
        &pending,
        format!("{{\"look\":{start},\"at\":{}}}\n", AT + 500),
    )
    .unwrap();
    let second = run(&mut memory, AT + 600, vec![]);
    let b = by_agent(&second, "b");
    assert_eq!(b.first_shown, Some(seen));
    assert_eq!(b.state, "late");
    assert!(b.detail.contains("not decided"), "{}", b.detail);

    // Waiting for answers to its questions at that look, due later.
    std::fs::write(
        root.join("data/agent/p/board.jsonl"),
        format!(
            "{{\"kind\":\"ask\",\"agent\":\"b\",\"look\":{start},\"deadline\":{}}}\n\
             {{\"kind\":\"ask\",\"agent\":\"a\",\"look\":{start},\"deadline\":{}}}\n",
            AT + 1_200,
            AT + 9_999
        ),
    )
    .unwrap();
    let answering = run(
        &mut memory,
        AT + 700,
        vec![process(
            root,
            9,
            "lpbot agent answers experiments/panel.yml --agent b --wait",
            AT + 650,
        )],
    );
    let b = by_agent(&answering, "b");
    assert_eq!(b.state, "deciding", "{}", b.detail);
    assert_eq!(b.waiter_kind.as_deref(), Some("answers"));
    assert_eq!(b.waiter_until, Some(i64::try_from(AT + 1_200).unwrap()));
}

#[test]
fn a_fence_never_shows_its_token_and_a_runner_must_be_running() {
    let disk = disk("fence");
    let root = &disk.root;
    std::fs::write(
        root.join("data/agent/p/a.jsonl.runner-owner.json"),
        r#"{"schema":"lpbot.runner-owner/1","token":"secret-token-123","agent":"a","activated_unix":1795500000}"#,
    )
    .unwrap();
    let mut memory = Memory::default();
    let list = listed(root);
    let (rows, _) = rows(
        root,
        Path::new("experiments"),
        &list,
        &Fake(vec![]),
        &mut memory,
        AT + HOUR,
        600,
    )
    .unwrap();
    let a = by_agent(&rows, "a");
    assert_eq!(a.state, "scheduler down", "{}", a.detail);
    assert_eq!(a.runner, "runner");
    assert_eq!(a.owner.as_deref(), Some("lpbot agent-runner"));
    assert_eq!(a.owner_since, Some(1_795_500_000));
    let everything = format!(
        "{}{}{}{:?}{}",
        table(&rows, AT),
        rows.iter().map(brief_line).collect::<String>(),
        rows.iter()
            .map(|r| row_json(r).to_string())
            .collect::<String>(),
        changes(&rows, &memory),
        copy_text(&agent_status_table(&rows)),
    );
    assert!(!everything.contains("secret-token-123"));
}

#[test]
fn without_a_list_only_open_panels_with_decisions_show() {
    let disk = disk("unlisted");
    let root = &disk.root;
    let empty = AgentList::default().check(root).unwrap();
    let mut memory = Memory::default();
    let (rows, _) = rows(
        root,
        Path::new("experiments"),
        &empty,
        &Fake(vec![]),
        &mut memory,
        AT + HOUR,
        600,
    )
    .unwrap();
    // Only a has a decisions file; nothing lists b, x or c.
    assert_eq!(rows.len(), 1, "{rows:?}");
    let a = &rows[0];
    assert_eq!(a.agent, "a");
    assert!(!a.listed);
    // Nothing says it runs here, and no process was seen.
    assert_eq!(a.state, "unverified", "{}", a.detail);
    assert_eq!(a.listening, None);
}
