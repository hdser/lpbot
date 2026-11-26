use super::*;
use clap::Parser as _;

struct TestDir(PathBuf);

impl TestDir {
    fn new() -> Self {
        let path = std::env::temp_dir().join(fresh_name("lpbot-runner-regression"));
        std::fs::create_dir(&path).unwrap();
        Self(path)
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn config(root: &Path) -> Config {
    Config {
        experiment: root.join("experiment.yml"),
        experiment_sha256: String::new(),
        agents: vec![Agent {
            name: "solo".into(),
            decisions: root.join("solo.jsonl"),
        }],
        state_dir: root.join("state"),
        codex: PathBuf::from("/usr/bin/false"),
        codex_home: root.join("auth"),
        results_dir: root.join("results"),
        poll_seconds: 30,
        timeout_seconds: 300,
        min_disk_gb: 150,
        max_load: 4.0,
        max_cpu_c: 80.0,
    }
}

fn response() -> codex_worker::Response {
    codex_worker::Response {
        action: "enter".into(),
        next: "15m".into(),
        note: "-- exact note\n  second line  ".into(),
        reasoning: "- First observation\n- Second observation\n  ".into(),
        expect_low: -2.0,
        expect_high: 3.0,
        expect_confidence: 0.75,
        expect_text: "- Expected path\n  keep this spacing  ".into(),
        lower_pct: Some(-1.0),
        upper_pct: Some(1.0),
        stance: Some("own".into()),
        toward: None,
    }
}

fn attempt() -> Attempt {
    Attempt {
        count_before: 0,
        look: 100,
        as_of: 200,
        response: response_json(&response()),
        review: None,
    }
}

fn canonical(attempt: &Attempt) -> Value {
    json!({
        "look": attempt.look,
        "at": attempt.as_of,
        "action": attempt.response["action"],
        "next": attempt.response["next"],
        "note": attempt.response["note"],
        "lower_pct": attempt.response["lower_pct"],
        "upper_pct": attempt.response["upper_pct"],
    })
}

fn audit_record(attempt: &Attempt) -> Value {
    json!({
        "kind": "look",
        "look": attempt.look,
        "as_of": attempt.as_of,
        "decision": {
            "action": attempt.response["action"],
            "next": attempt.response["next"],
            "lower_pct": attempt.response["lower_pct"],
            "upper_pct": attempt.response["upper_pct"],
        },
        "note": attempt.response["note"],
        "reasoning": attempt.response["reasoning"],
        "expect": {
            "low_pct": attempt.response["expect_low"],
            "high_pct": attempt.response["expect_high"],
            "confidence": attempt.response["expect_confidence"],
            "text": attempt.response["expect_text"],
        },
        "stance": { "kind": "own", "toward": null },
    })
}

#[test]
fn release_refuses_a_delayed_model_or_command_without_durable_exit() {
    let dir = TestDir::new();
    assert!(verify_jobs_drained(&dir.0).is_ok());
    let active = dir.0.join("active");
    let job_dir = dir.0.join("jobs").join("delayed-own-job");
    std::fs::create_dir_all(&active).unwrap();
    std::fs::create_dir_all(&job_dir).unwrap();
    let spec = Job {
        args: vec!["runner-model-job".into(), "private-input.json".into()],
        token: "old-owner".into(),
        directory: job_dir.clone(),
        session: "delayed-own-job".into(),
        data_root: dir.0.join("lake"),
    };
    std::fs::write(
        active.join("model.json"),
        serde_json::to_vec(&spec).unwrap(),
    )
    .unwrap();
    assert!(verify_jobs_drained(&dir.0).is_err());
    std::fs::write(job_dir.join("exit.json"), "{}").unwrap();
    assert!(verify_jobs_drained(&dir.0).is_err());
    std::fs::write(job_dir.join("exit.json"), r#"{"success":false,"code":1}"#).unwrap();
    assert!(verify_jobs_drained(&dir.0).is_ok());
    std::fs::write(job_dir.join("exit.json"), r#"{"success":true,"code":0}"#).unwrap();
    assert!(verify_jobs_drained(&dir.0).is_ok());
}

#[test]
fn model_fields_survive_cli_parsing_with_leading_hyphens_and_newlines() {
    let dir = TestDir::new();
    let config = config(&dir.0);
    let waiting = agent_cmd::Pending {
        look: 100,
        at: Some(200),
        number: Some(1),
        day: Some(0.0),
        seen: None,
        record: None,
    };
    let response = response();
    let args = recording_args(&config, &config.agents[0], &waiting, &response).unwrap();
    let parsed = crate::Cli::try_parse_from(std::iter::once("lpbot".to_owned()).chain(args))
        .expect("model prose is an exact value, including a leading hyphen");
    let crate::Command::Agent(crate::AgentCommand::Decide(decision)) = parsed.command else {
        panic!("expected an agent decide command");
    };
    assert_eq!(decision.note, response.note);
    assert_eq!(decision.reasoning, response.reasoning);
    assert_eq!(decision.expect, response.expect_text);
    assert_eq!(decision.action, response.action);
    assert_eq!(decision.next, response.next);
    assert_eq!(decision.lower, response.lower_pct);
    assert_eq!(decision.upper, response.upper_pct);
    assert_eq!(decision.expect_low, Some(response.expect_low));
    assert_eq!(decision.expect_high, Some(response.expect_high));
    assert_eq!(decision.expect_confidence, Some(response.expect_confidence));
    assert_eq!(decision.expected_look, Some(waiting.look));
    assert_eq!(decision.expected_as_of, waiting.at);
}

#[test]
fn decision_acknowledgement_must_match_the_canonical_due_time() {
    assert!(verify_due_ack("Recorded.\nNext look due: 1970-01-01T00:15:00Z\n", 900).is_ok());
    assert!(verify_due_ack("Recorded.\nNext look due: 1970-01-01T00:15:01Z\n", 900).is_err());
    assert!(verify_due_ack("Recorded.\n", 900).is_err());
}

#[test]
fn entry_reviews_do_not_replace_the_daily_other_decision_review() {
    let mut state = AgentState::default();
    let mut task = ReviewTask {
        directory: PathBuf::new(),
        input: String::new(),
        author: json!({"action":"enter"}),
        observation_day: "2026-11-26".into(),
    };
    mark_reviewed_other(&mut state, &task);
    assert!(state.reviewed_day.is_none());
    task.author["action"] = json!("exit");
    mark_reviewed_other(&mut state, &task);
    assert!(state.reviewed_day.is_none());
    task.author["action"] = json!("hold");
    mark_reviewed_other(&mut state, &task);
    assert_eq!(state.reviewed_day.as_deref(), Some("2026-11-26"));
    task.observation_day = "2026-11-27".into();
    task.author["action"] = json!("enter");
    mark_reviewed_other(&mut state, &task);
    assert_eq!(state.reviewed_day.as_deref(), Some("2026-11-26"));
}

#[test]
fn recovered_commit_requires_its_matching_successful_job_and_due_acknowledgement() {
    let dir = TestDir::new();
    let config = config(&dir.0);
    let agent = &config.agents[0];
    let attempt = attempt();
    let due = attempt.as_of + 900;
    assert!(recovered_recording(&config, agent, &attempt, due).is_err());
    let waiting = agent_cmd::Pending {
        look: attempt.look,
        at: Some(attempt.as_of),
        number: None,
        day: None,
        seen: None,
        record: None,
    };
    let job = Job {
        args: recording_args(&config, agent, &waiting, &response()).unwrap(),
        token: "old-token".into(),
        directory: dir.0.join("completed-decide"),
        session: "own-decide".into(),
        data_root: dir.0.join("lake"),
    };
    std::fs::create_dir_all(&job.directory).unwrap();
    agent_lock::atomic(
        &config.state_dir.join("last/solo.json"),
        &serde_json::to_vec(&job).unwrap(),
    )
    .unwrap();
    let stdout = job.directory.join("stdout.txt");
    let exit = job.directory.join("exit.json");
    std::fs::write(
        &stdout,
        format!("Next look due: {}Z\n", agent_cmd::iso(due)),
    )
    .unwrap();
    std::fs::write(&exit, r#"{"success":false,"code":1}"#).unwrap();
    assert!(recovered_recording(&config, agent, &attempt, due).is_err());
    std::fs::write(&exit, r#"{"success":true,"code":0}"#).unwrap();
    assert!(!recovered_recording(&config, agent, &attempt, due).unwrap());
    std::fs::write(&stdout, "Recorded without a due acknowledgement.\n").unwrap();
    assert!(recovered_recording(&config, agent, &attempt, due).is_err());
    std::fs::write(&stdout, "The loss limit stopped the run\n").unwrap();
    assert!(recovered_recording(&config, agent, &attempt, due).unwrap());
    let mut mismatched = job;
    mismatched.args.push("different".into());
    agent_lock::atomic(
        &config.state_dir.join("last/solo.json"),
        &serde_json::to_vec(&mismatched).unwrap(),
    )
    .unwrap();
    assert!(recovered_recording(&config, agent, &attempt, due).is_err());
}

#[test]
fn canonical_recovery_refuses_unterminated_or_duplicate_decisions() {
    let dir = TestDir::new();
    let path = dir.0.join("solo.jsonl");
    assert_eq!(
        snapshot(&path, 55).unwrap(),
        Snapshot {
            count: 0,
            last_look: None,
            next_due: 55,
        }
    );
    let line = canonical(&attempt()).to_string();
    std::fs::write(&path, &line).unwrap();
    assert!(
        snapshot(&path, 55).is_err(),
        "a final newline commits the line"
    );
    std::fs::write(&path, format!("{line}\n")).unwrap();
    assert_eq!(
        snapshot(&path, 55).unwrap(),
        Snapshot {
            count: 1,
            last_look: Some(100),
            next_due: 1_100,
        }
    );
    std::fs::write(&path, format!("{line}\n{line}\n")).unwrap();
    assert!(
        snapshot(&path, 55).is_err(),
        "a duplicate look needs inspection"
    );
}

#[test]
fn committed_decision_needs_a_complete_matching_shared_audit_record() {
    let dir = TestDir::new();
    let path = dir.0.join("solo.jsonl");
    let attempt = attempt();
    assert!(!committed_audit(&path, &attempt).unwrap());
    std::fs::write(&path, format!("{}\n", canonical(&attempt))).unwrap();
    assert!(snapshot(&path, 0).is_ok(), "the action itself is committed");
    assert!(
        !committed_audit(&path, &attempt).unwrap(),
        "a crash before reasoning persistence must not silently reconcile"
    );
    let record = audit_record(&attempt);
    let log = agent_log::records_path(&path);
    std::fs::write(&log, record.to_string()).unwrap();
    assert!(
        !committed_audit(&path, &attempt).unwrap(),
        "a parseable but unterminated shared log is uncommitted"
    );
    std::fs::write(&log, format!("{record}\n")).unwrap();
    assert!(committed_audit(&path, &attempt).unwrap());
    for (field, replacement) in [
        ("reasoning", json!("another response")),
        ("as_of", json!(201)),
        ("note", json!("another note")),
    ] {
        let mut changed = record.clone();
        changed[field] = replacement;
        std::fs::write(&log, format!("{changed}\n")).unwrap();
        assert!(
            !committed_audit(&path, &attempt).unwrap(),
            "a mismatched {field} must not acknowledge the saved attempt"
        );
    }
    for (pointer, replacement) in [
        ("/decision/lower_pct", json!(-2.0)),
        ("/decision/upper_pct", json!(2.0)),
        ("/expect/low_pct", json!(-3.0)),
        ("/expect/high_pct", json!(4.0)),
        ("/expect/confidence", json!(0.5)),
        ("/expect/text", json!("different expectation")),
        ("/stance/kind", json!("adapt")),
        ("/stance/toward", json!("another-agent")),
    ] {
        let mut changed = record.clone();
        *changed.pointer_mut(pointer).unwrap() = replacement;
        std::fs::write(&log, format!("{changed}\n")).unwrap();
        assert!(
            !committed_audit(&path, &attempt).unwrap(),
            "a mismatched {pointer} must not acknowledge the saved attempt"
        );
    }
    std::fs::write(&log, format!("{record}\n")).unwrap();
    for (field, replacement) in [
        ("action", json!("stay_out")),
        ("lower_pct", json!(-2.0)),
        ("upper_pct", json!(2.0)),
        ("at", json!(201)),
    ] {
        let mut changed = canonical(&attempt);
        changed[field] = replacement;
        std::fs::write(&path, format!("{changed}\n")).unwrap();
        assert!(
            !committed_audit(&path, &attempt).unwrap(),
            "the canonical {field} must match the recorded response"
        );
    }
}

#[test]
fn changing_only_the_shared_base_cannot_reuse_an_active_run() {
    let dir = TestDir::new();
    let mut config = config(&dir.0);
    let base_path = dir.0.join("base.yml");
    let mut base = json!({
        "mode": "backtest",
        "run": { "capital_usd": 10_000, "loss_limit_pct": 5 },
        "universe": { "tokens": ["WETH", "USDC"] },
        "strategies": [{
            "id": "zone",
            "kind": "zone_round_trip",
            "value_token": "WETH",
            "agent": { "name": "solo", "model": "test", "decisions": config.agents[0].decisions },
        }],
        "risk": { "max_actions_per_day": 12 },
    });
    std::fs::write(&base_path, serde_yaml::to_string(&base).unwrap()).unwrap();
    let experiment = json!({
        "experiment": "runner-base-regression",
        "base": base_path,
        "mode": "backtest",
        "window": { "from": "2026-11-20", "to": "open" },
        "strategy": "zone",
    });
    let experiment_bytes = serde_yaml::to_string(&experiment).unwrap();
    std::fs::write(&config.experiment, &experiment_bytes).unwrap();
    config.experiment_sha256 = digest(experiment_bytes.as_bytes());
    let runs =
        lpbot_core::resolve_experiment(&config.experiment, &lpbot_core::RunOverrides::default())
            .unwrap();
    let state = State {
        schema: "lpbot.codex-runner/1".into(),
        experiment_sha256: config.experiment_sha256.clone(),
        activated_unix: 0,
        context: "test".into(),
        resolved_config_hashes: resolved_hashes(&config, &runs).unwrap(),
        evaluation_day: "2026-11-20".into(),
        evaluation: None,
        agents: BTreeMap::new(),
    };
    verify_resolved_runs(&config, &state, &runs).unwrap();
    base["risk"]["max_actions_per_day"] = json!(13);
    std::fs::write(&base_path, serde_yaml::to_string(&base).unwrap()).unwrap();
    assert_eq!(
        digest(&std::fs::read(&config.experiment).unwrap()),
        config.experiment_sha256,
        "the experiment checksum alone cannot detect a changed shared base"
    );
    let changed_runs =
        lpbot_core::resolve_experiment(&config.experiment, &lpbot_core::RunOverrides::default())
            .unwrap();
    assert_ne!(runs[0].config_hash, changed_runs[0].config_hash);
    let error = verify_resolved_runs(&config, &state, &changed_runs).unwrap_err();
    assert!(format!("{error:#}").contains("base config"));
}
