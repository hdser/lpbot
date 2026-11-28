//! `lpbot select list` and `lpbot select approve`, and the check the trader
//! makes before it takes a plan (`docs/PLAN.md`, piece 3).
//!
//! `list` answers "what do we have that could trade": every experiment file,
//! rule or agent panel, whether it is a backtest (practice) or forward, its
//! agents, and, given a ladder's plan, where each of its runs stands.
//!
//! A plan moves no money by itself. `approve` records the owner's yes beside
//! the plan, with the plan's SHA-256, and `approved_plan` is what the trader
//! reads: it refuses a plan with no approval, one whose bytes changed after
//! it was approved, and one older than the trader accepts.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};
use lpbot_core::{resolve_experiment, RunOverrides};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::agent_brief::mandate;
use crate::agent_log::identity;
use crate::run_cmd::run_window;
use crate::select_cmd::{Plan, Standing};

/// One experiment file, as `list` shows it.
#[derive(Debug, Clone, PartialEq)]
pub struct Entry {
    pub file: PathBuf,
    pub experiment: String,
    /// `rule` or `agents`.
    pub kind: &'static str,
    /// The window: `2025-05-01 to 2026-10-31`, or `forward from 2026-11-01`.
    pub window: String,
    pub forward: bool,
    pub runs: usize,
    /// Each agent as `name (model, mandate)`.
    pub agents: Vec<String>,
    /// What the plan says of its runs, when a plan was given.
    pub standing: Option<String>,
    /// Why the file could not be read, when it could not.
    pub error: Option<String>,
}

/// Every experiment file under `folder`, by name.
pub fn entries(folder: &Path, plan: Option<&Plan>) -> Result<Vec<Entry>> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(folder)
        .with_context(|| format!("reading {}", folder.display()))?
        .filter_map(|entry| entry.ok().map(|e| e.path()))
        .filter(|path| path.extension().is_some_and(|e| e == "yml" || e == "yaml"))
        .collect();
    files.sort();
    Ok(files.iter().map(|file| entry(file, plan)).collect())
}

fn entry(file: &Path, plan: Option<&Plan>) -> Entry {
    let stem = file
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let mut entry = Entry {
        file: file.to_path_buf(),
        experiment: stem,
        kind: "rule",
        window: String::new(),
        forward: false,
        runs: 0,
        agents: Vec::new(),
        standing: None,
        error: None,
    };
    let runs = match resolve_experiment(file, &RunOverrides::default()) {
        Ok(runs) => runs,
        Err(error) => {
            entry.error = Some(format!("{error:#}"));
            return entry;
        }
    };
    entry.runs = runs.len();
    if let Some(first) = runs.first() {
        entry.experiment = first.experiment.clone();
        match run_window(&first.config) {
            Ok((from, None)) => {
                entry.forward = true;
                entry.window = format!("forward from {from}");
            }
            Ok((from, Some(to))) => entry.window = format!("{from} to {to}"),
            Err(error) => entry.window = format!("no window ({error})"),
        }
    }
    for run in &runs {
        let Some(strategy) = run.config.strategies.first() else {
            continue;
        };
        match identity(strategy) {
            Ok(Some(who)) => {
                entry.kind = "agents";
                let mandate = mandate(strategy).ok().flatten();
                entry.agents.push(match mandate {
                    Some(m) => format!("{} ({}, {m})", who.name, who.model),
                    None => format!("{} ({})", who.name, who.model),
                });
            }
            Ok(None) => {}
            Err(error) => entry.error = Some(format!("{error:#}")),
        }
    }
    if let Some(plan) = plan {
        let lines: Vec<_> = plan
            .tracks
            .iter()
            .flat_map(|t| &t.lines)
            .filter(|l| l.experiment == entry.experiment)
            .collect();
        if !lines.is_empty() {
            let passed = lines
                .iter()
                .filter(|l| l.standing == Standing::Passed)
                .count();
            let forward = lines
                .iter()
                .filter(|l| matches!(l.standing, Standing::Forward { .. }))
                .count();
            entry.standing = Some(format!(
                "{passed} passed, {forward} forward, {} practice",
                lines.len() - passed - forward
            ));
        }
    }
    entry
}

/// The entries as text: forward first, then practice.
pub fn list_text(entries: &[Entry], plan: Option<&Plan>) -> String {
    let mut text = String::new();
    for (title, forward) in [
        (
            "Forward: open windows, the candidates for real positions",
            true,
        ),
        (
            "Practice: backtests, which never earn real positions",
            false,
        ),
    ] {
        let rows: Vec<&Entry> = entries
            .iter()
            .filter(|e| e.error.is_none() && e.forward == forward)
            .collect();
        let _ = writeln!(text, "{title} ({})", rows.len());
        for e in rows {
            let _ = writeln!(
                text,
                "  {:<34} {:<6} {:>4} run(s)  {}{}",
                e.experiment,
                e.kind,
                e.runs,
                e.window,
                e.standing
                    .as_ref()
                    .map(|s| format!("  [{s}]"))
                    .unwrap_or_default()
            );
            if !e.agents.is_empty() {
                let _ = writeln!(text, "      agents: {}", e.agents.join(", "));
            }
        }
        let _ = writeln!(text);
    }
    let broken: Vec<&Entry> = entries.iter().filter(|e| e.error.is_some()).collect();
    if !broken.is_empty() {
        let _ = writeln!(text, "Could not be read ({})", broken.len());
        for e in broken {
            let _ = writeln!(
                text,
                "  {}: {}",
                e.file.display(),
                e.error.as_deref().unwrap_or_default()
            );
        }
        let _ = writeln!(text);
    }
    match plan {
        Some(plan) => {
            let _ = writeln!(
                text,
                "Standings are from ladder `{}` as of {}. `lpbot select ladder <file>` works them \
                 out again.",
                plan.ladder, plan.as_of
            );
        }
        None => {
            let _ = writeln!(
                text,
                "No plan given: `--plan data/selection/<ladder>.json` adds where each run stands."
            );
        }
    }
    text
}

/// The owner's approval of a plan, written beside it as
/// `<plan>.approved.json`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Approval {
    pub ladder: String,
    /// The plan's own `as_of`.
    pub as_of: u64,
    /// SHA-256 of the plan file's bytes, in hex.
    pub sha256: String,
    pub approved_by: String,
    pub approved_at: u64,
}

/// Where the approval of `plan` is kept.
pub fn approval_path(plan: &Path) -> PathBuf {
    let mut name = plan.as_os_str().to_owned();
    name.push(".approved.json");
    PathBuf::from(name)
}

fn digest(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

fn read_plan(bytes: &[u8], path: &Path) -> Result<Plan> {
    serde_json::from_slice(bytes).with_context(|| {
        format!(
            "{} is not a plan `lpbot select ladder` wrote; work it out again with `lpbot select \
             ladder <file>`",
            path.display()
        )
    })
}

/// Records the owner's yes to the plan at `plan`. The owner's step: a
/// session never runs it.
pub fn approve(plan: &Path, by: &str, now: u64) -> Result<Approval> {
    let by = by.trim();
    if by.is_empty() {
        bail!("--by names who approves, such as --by owner");
    }
    let bytes = std::fs::read(plan).with_context(|| format!("reading {}", plan.display()))?;
    let parsed = read_plan(&bytes, plan)?;
    let approval = Approval {
        ladder: parsed.ladder,
        as_of: parsed.as_of,
        sha256: digest(&bytes),
        approved_by: by.to_string(),
        approved_at: now,
    };
    let path = approval_path(plan);
    let temporary = path.with_extension("tmp");
    std::fs::write(&temporary, serde_json::to_string_pretty(&approval)?)
        .and_then(|()| std::fs::rename(&temporary, &path))
        .with_context(|| format!("writing {}", path.display()))?;
    Ok(approval)
}

/// The plan at `plan`, only if the owner approved exactly these bytes and
/// it was worked out no more than `max_age` seconds before `now`.
pub fn approved_plan(plan: &Path, max_age: u64, now: u64) -> Result<(Plan, Approval)> {
    let bytes = std::fs::read(plan).with_context(|| format!("reading {}", plan.display()))?;
    let parsed = read_plan(&bytes, plan)?;
    let path = approval_path(plan);
    let approval: Approval = match std::fs::read(&path) {
        Ok(text) => serde_json::from_slice(&text)
            .with_context(|| format!("{} is not an approval", path.display()))?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => bail!(
            "{} has no approval: nothing trades on it until the owner runs `lpbot select \
             approve {} --by <name>`",
            plan.display(),
            plan.display()
        ),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    if approval.sha256 != digest(&bytes) || approval.as_of != parsed.as_of {
        bail!(
            "{} changed after it was approved (approved {} as of {}): the owner approves it \
             again, or the trader keeps the plan it has",
            plan.display(),
            &approval.sha256[..12.min(approval.sha256.len())],
            approval.as_of
        );
    }
    let age = now.saturating_sub(parsed.as_of);
    if age > max_age {
        return Err(anyhow!(
            "{} was worked out {} hour(s) ago, more than the {} the trader accepts: work it out \
             again (`lpbot select ladder`) and have it approved",
            plan.display(),
            age / 3_600,
            max_age / 3_600
        ));
    }
    Ok((parsed, approval))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::select_cmd::{Allocation, Gates, Line, TrackPlan};

    fn temp(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "lpbot-select-ops-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn line(run: &str, standing: Standing, share: f64) -> Line {
        Line {
            run_id: run.into(),
            experiment: "forward-x".into(),
            what: "regime=a".into(),
            pool: "USDC/WETH 0.05".into(),
            value_token: "USDC".into(),
            days: 40.0,
            campaigns: 6,
            result_pct: 1.2,
            result_pct_usd: 1.2,
            hold_usd_pct: 0.0,
            max_drawdown_pct: 2.0,
            agent: None,
            standing,
            share,
            capital_usd: Some(10_000.0 * share),
        }
    }

    fn plan(as_of: u64) -> Plan {
        Plan {
            ladder: "dollars".into(),
            as_of,
            gates: Gates {
                min_days: 30.0,
                min_campaigns: 5,
                min_result_pct: 0.0,
                max_drawdown_pct: 5.0,
                min_decisions: 0,
                min_edge_vs_cash_pct: None,
                horizon: "1d".into(),
            },
            allocation: Allocation::default(),
            tracks: vec![TrackPlan {
                track: "dollars".into(),
                capital_usd: Some(10_000.0),
                lines: vec![
                    line("forward-x-000", Standing::Passed, 0.5),
                    line(
                        "forward-x-001",
                        Standing::Forward {
                            missing: vec!["3 more campaign(s)".into()],
                        },
                        0.0,
                    ),
                ],
                cash_share: 0.5,
                empty: Vec::new(),
            }],
        }
    }

    #[test]
    fn a_plan_trades_only_with_an_approval_of_its_exact_bytes() {
        let dir = temp("approve");
        let path = dir.join("dollars.json");
        let as_of = 1_800_000_000;
        std::fs::write(&path, serde_json::to_string_pretty(&plan(as_of)).unwrap()).unwrap();

        // No approval yet: refused, naming the command that approves.
        let refused = approved_plan(&path, 86_400, as_of + 60)
            .unwrap_err()
            .to_string();
        assert!(refused.contains("lpbot select approve"), "{refused}");

        assert!(
            approve(&path, "  ", as_of).is_err(),
            "who approves is named"
        );
        let approval = approve(&path, "owner", as_of + 30).unwrap();
        assert_eq!(approval.approved_by, "owner");
        assert_eq!(approval.as_of, as_of);
        let (read, kept) = approved_plan(&path, 86_400, as_of + 60).unwrap();
        assert_eq!(read, plan(as_of), "the plan reads back as it was written");
        assert_eq!(kept, approval);

        // Too old for the trader.
        let stale = approved_plan(&path, 3_600, as_of + 7_200)
            .unwrap_err()
            .to_string();
        assert!(stale.contains("more than the 1"), "{stale}");

        // One byte changed after the approval: refused.
        let mut changed = plan(as_of);
        changed.tracks[0].lines[0].share = 0.9;
        std::fs::write(&path, serde_json::to_string_pretty(&changed).unwrap()).unwrap();
        let altered = approved_plan(&path, 86_400, as_of + 60)
            .unwrap_err()
            .to_string();
        assert!(
            altered.contains("changed after it was approved"),
            "{altered}"
        );

        // A file that is not a plan is named as such.
        std::fs::write(&path, "{}").unwrap();
        assert!(approve(&path, "owner", as_of)
            .unwrap_err()
            .to_string()
            .contains("not a plan"));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn the_list_separates_forward_from_practice_and_names_agents_and_standings() {
        let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let dir = temp("list");
        for name in [
            "forward-regime-usdc.yml",
            "agent-wording-a.yml",
            "regime-usd-usdc-weth-005.yml",
        ] {
            let text = std::fs::read_to_string(repo.join("experiments").join(name)).unwrap();
            // The copies point at the repository's base config.
            let text = text.replace(
                "base: config/mainnet.yml",
                &format!("base: {}", repo.join("config/mainnet.yml").display()),
            );
            std::fs::write(dir.join(name), text).unwrap();
        }
        std::fs::write(dir.join("broken.yml"), "experiment: [").unwrap();
        let mut forward_plan = plan(1);
        forward_plan.tracks[0]
            .lines
            .iter_mut()
            .for_each(|l| l.experiment = "forward-regime-usdc".into());
        let entries = entries(&dir, Some(&forward_plan)).unwrap();
        assert_eq!(entries.len(), 4);
        let by = |name: &str| entries.iter().find(|e| e.experiment == name).unwrap();

        let rule = by("forward-regime-usdc");
        assert!(rule.forward && rule.kind == "rule", "{rule:?}");
        assert!(
            rule.window.starts_with("forward from 2026-11-01"),
            "{rule:?}"
        );
        assert_eq!(
            rule.standing.as_deref(),
            Some("1 passed, 1 forward, 0 practice")
        );

        let panel = by("agent-wording-a");
        assert!(!panel.forward && panel.kind == "agents");
        assert_eq!(panel.agents.len(), 6);
        assert!(panel
            .agents
            .contains(&"w-why (claude-haiku-5-5, trend-why)".to_string()));
        assert!(panel
            .agents
            .contains(&"w-open (claude-haiku-5-5)".to_string()));

        let backtest = by("regime-usd-usdc-weth-005");
        assert_eq!((backtest.forward, backtest.runs), (false, 78));
        assert!(by("broken").error.is_some());

        let text = list_text(&entries, Some(&forward_plan));
        let forward_at = text.find("Forward:").unwrap();
        let practice_at = text.find("Practice:").unwrap();
        let rule_at = text.find("forward-regime-usdc").unwrap();
        assert!(forward_at < rule_at && rule_at < practice_at, "{text}");
        assert!(text.contains("Could not be read (1)"), "{text}");
        assert!(text.contains("ladder `dollars`"), "{text}");
        std::fs::remove_dir_all(dir).unwrap();
    }
}
