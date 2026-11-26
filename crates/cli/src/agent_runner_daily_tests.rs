use super::*;
use lpbot_sim::agent::{AgentAction, AgentDecision};

struct TestDir(PathBuf);

impl TestDir {
    fn new() -> Self {
        let path = std::env::temp_dir().join(fresh_name("lpbot-runner-daily"));
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
        experiment: root.join("missing-experiment.yml"),
        experiment_sha256: String::new(),
        agents: vec![Agent {
            name: "solo".into(),
            decisions: root.join("solo.jsonl"),
        }],
        state_dir: root.join("state"),
        codex: "/usr/bin/false".into(),
        codex_home: root.join("auth"),
        results_dir: root.join("results"),
        poll_seconds: 30,
        timeout_seconds: 300,
        min_disk_gb: 150,
        max_load: 4.0,
        max_cpu_c: 80.0,
    }
}

fn state() -> State {
    State {
        schema: "lpbot.codex-runner/1".into(),
        experiment_sha256: String::new(),
        activated_unix: 0,
        context: "test".into(),
        resolved_config_hashes: BTreeMap::new(),
        evaluation_day: String::new(),
        evaluation: None,
        agents: BTreeMap::new(),
    }
}

fn decisions(count: usize) -> Vec<AgentDecision> {
    (0..count)
        .map(|index| AgentDecision {
            look: if index == 0 {
                100
            } else {
                200 + index as u64 * 3_600
            },
            at: Some(200 + index as u64 * 3_600),
            action: if index == 0 {
                AgentAction::Enter {
                    lower_pct: -1.0,
                    upper_pct: 1.0,
                }
            } else {
                AgentAction::Hold
            },
            next_look: 3_600,
            note: String::new(),
        })
        .collect()
}

fn write_decisions(config: &Config, decisions: &[AgentDecision]) {
    let text = decisions
        .iter()
        .map(|decision| {
            let mut row = json!({
                "look":decision.look, "at":decision.at,
                "action":decision.action.name(), "next":"1h",
                // Write time is irrelevant to information-boundary age.
                "decided":u64::MAX,
            });
            if let AgentAction::Enter {
                lower_pct,
                upper_pct,
            } = decision.action
            {
                row["lower_pct"] = json!(lower_pct);
                row["upper_pct"] = json!(upper_pct);
            }
            format!("{row}\n")
        })
        .collect::<String>();
    std::fs::write(&config.agents[0].decisions, text).unwrap();
}

fn score(decision: &AgentDecision, seconds: u64) -> Value {
    let start = decision.at.unwrap();
    json!({
        "kind":"eval", "agent":"solo", "look":decision.look,
        "decided_as_of":start, "as_of":start+seconds,
        "horizon":"1d", "horizon_seconds":86_400,
        "scored_seconds":seconds, "complete":seconds>=86_400,
        "decision":{"action":decision.action.name()},
    })
}

#[test]
fn aged_canonical_decisions_bootstrap_scoring_without_prior_eval_files() {
    let dir = TestDir::new();
    let config = config(&dir.0);
    let canonical = decisions(5);
    write_decisions(&config, &canonical);
    assert!(!agent_log::scores_path(&config.agents[0].decisions).exists());
    let read = daily_solo_decisions(&config).unwrap();
    let at = canonical.last().unwrap().at.unwrap() + 86_400;
    assert_eq!(daily_candidates(&read, at), (5, true));
    let fresh: Vec<_> = read
        .iter()
        .map(|decision| score(decision, 86_400))
        .collect();
    assert_eq!(daily_maturity(&read, &fresh), (5, true));
}

#[test]
fn candidate_age_uses_the_seen_market_and_requires_an_aged_entry() {
    let mut canonical = decisions(5);
    let at = canonical.last().unwrap().at.unwrap() + 86_400;
    assert_eq!(daily_candidates(&canonical, at - 1), (4, true));
    canonical[0].action = AgentAction::StayOut;
    let mut recent_entry = decisions(1).remove(0);
    recent_entry.look = at - 100;
    recent_entry.at = Some(at);
    canonical.push(recent_entry);
    assert_eq!(daily_candidates(&canonical, at), (5, false));
    canonical.last_mut().unwrap().at = Some(at + 1);
    assert_eq!(daily_candidates(&canonical, at), (5, false));
}

#[test]
fn a_stale_store_cannot_turn_wall_clock_age_or_advertised_duration_into_maturity() {
    let canonical = decisions(5);
    let at = canonical.last().unwrap().at.unwrap() + 86_400;
    assert_eq!(daily_candidates(&canonical, at), (5, true));
    let mut scores: Vec<_> = canonical
        .iter()
        .map(|decision| score(decision, 86_400))
        .collect();
    scores[0] = score(&canonical[0], 86_399);
    scores[0]["complete"] = json!(true);
    assert_eq!(daily_maturity(&canonical, &scores), (4, false));
    scores.push(scores[1].clone());
    assert_eq!(
        daily_maturity(&canonical, &scores),
        (4, false),
        "duplicate rows are one decision"
    );
    for field in [
        "decided_as_of",
        "as_of",
        "scored_seconds",
        "horizon_seconds",
        "complete",
    ] {
        let mut malformed = score(&canonical[0], 86_400);
        malformed.as_object_mut().unwrap().remove(field);
        assert_eq!(
            daily_maturity(&canonical, &[malformed]),
            (0, false),
            "missing {field}"
        );
    }
    let mut orphan = score(&canonical[0], 86_400);
    orphan["look"] = json!(99);
    assert_eq!(daily_maturity(&canonical, &[orphan]), (0, false));
    let mut changed_action = score(&canonical[0], 86_400);
    changed_action["decision"]["action"] = json!("hold");
    assert_eq!(daily_maturity(&canonical, &[changed_action]), (0, false));
}

#[test]
fn daily_state_counts_complete_and_unfinished_rows_by_horizon() {
    let canonical = decisions(2);
    let mut next = score(&canonical[0], 3_600);
    next["horizon"] = json!("next");
    next["horizon_seconds"] = json!(3_600);
    next["complete"] = json!(true);
    next["ended"] = json!("look");
    next["next_look"] = json!(canonical[1].look);
    next["next_as_of"] = next["as_of"].clone();
    let mut pending = next.clone();
    pending["next_as_of"] = json!(null);
    let counts = daily_horizon_counts(&[
        next,
        pending,
        score(&canonical[0], 86_400),
        score(&canonical[1], 86_399),
    ]);
    assert_eq!(
        counts,
        json!({
            "next":{"complete":1,"unfinished":1},
            "1d":{"complete":1,"unfinished":1},
        })
    );
}

#[test]
fn reserving_the_day_is_durable_and_prevents_a_second_scoring_job() {
    let dir = TestDir::new();
    let config = config(&dir.0);
    let mut state = state();
    let today = day();
    reserve_daily(&config, &mut state, &today).unwrap();
    let persisted: State =
        serde_json::from_slice(&std::fs::read(config.state_dir.join("state.json")).unwrap())
            .unwrap();
    assert_eq!(persisted.evaluation_day, today);
    assert_eq!(persisted.evaluation.unwrap()["status"], "reserved");
    // The missing experiment and unavailable server resources would fail if
    // another heavy evaluation were attempted after the durable reservation.
    daily(&config, &mut state).unwrap();
    assert!(!config.state_dir.join("jobs").exists());
}

#[test]
fn malformed_canonical_inputs_do_not_bootstrap_scoring() {
    let dir = TestDir::new();
    let config = config(&dir.0);
    let canonical = decisions(5);
    write_decisions(&config, &canonical);
    let path = &config.agents[0].decisions;
    let valid = std::fs::read_to_string(path).unwrap();
    std::fs::write(path, valid.trim_end()).unwrap();
    assert!(daily_solo_decisions(&config).is_err());
    std::fs::write(path, format!("{valid}{}\n", valid.lines().last().unwrap())).unwrap();
    assert!(daily_solo_decisions(&config).is_err());
}
