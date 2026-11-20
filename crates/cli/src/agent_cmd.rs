//! `lpbot agent next` and `lpbot agent decide`: a run decided one look at
//! a time by a person or a session (the agent mode of `lpbot_sim::agent`).
//!
//! `next` replays the run from its decisions up to the first look without
//! one and shows how the pool looks there; `decide` writes the decision for
//! that look and shows the next one. The decisions file is the run: `lpbot
//! run` on the same experiment writes its six tables.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};
use lpbot_core::experiment::ResolvedRun;
use lpbot_sim::agent::Observation;

use crate::run_cmd::{agent_file, simulate_one};

/// What a look asks of the agent, as it is written next to the decisions.
fn pending_path(decisions: &Path) -> PathBuf {
    let mut name = decisions.as_os_str().to_owned();
    name.push(".pending");
    PathBuf::from(name)
}

fn decisions_path(run: &ResolvedRun) -> Result<PathBuf> {
    let strategy = run
        .config
        .strategies
        .first()
        .ok_or_else(|| anyhow!("the run has no strategy"))?;
    agent_file(strategy).map(PathBuf::from).ok_or_else(|| {
        anyhow!(
            "the experiment's strategy has no agent.decisions file; set it, such as \
             `agent.decisions: [data/agent/my-run.jsonl]` under `sweep`"
        )
    })
}

/// Replays the run to its first look without a decision and shows it.
/// `None` once the window is over.
pub fn next(run: &ResolvedRun, root: &Path, chain: &str) -> Result<Option<Observation>> {
    let decisions = decisions_path(run)?;
    let (out, pool_name) = simulate_one(run, root, chain)?;
    if out.unused_decisions > 0 {
        println!(
            "note: {} decision(s) at the end of {} answer looks the run did not ask for (the \
             settings or the history changed since); they are left out",
            out.unused_decisions,
            decisions.display()
        );
    }
    let strategy = &run.config.strategies[0];
    let pending = pending_path(&decisions);
    match out.pending {
        Some(observation) => {
            if let Some(parent) = pending.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(&pending, format!("{{\"look\": {}}}\n", observation.look))
                .with_context(|| format!("writing {}", pending.display()))?;
            print!(
                "{}",
                describe(&observation, &pool_name, &strategy.value_token)
            );
            println!();
            println!(
                "Decide with: lpbot agent decide <experiment> --action stay_out|hold|exit|enter \
                 [--lower <pct> --upper <pct>] --next <duration> --note \"why\""
            );
            Ok(Some(observation))
        }
        None => {
            let _ = std::fs::remove_file(&pending);
            let s = &out.summary;
            println!(
                "The window is over: {:+.2}% in {} ({:+.2}% in dollars; holding it made \
                 {:+.2}% in dollars), {} campaign(s), fees {:.4}, gas {:.4}. `lpbot run` on the \
                 experiment writes the tables.",
                s.result_pct,
                strategy.value_token,
                s.result_pct_usd,
                s.hold_value_pct_usd,
                s.campaigns,
                s.fees,
                s.gas
            );
            Ok(None)
        }
    }
}

/// A decision for the look `lpbot agent next` last showed.
pub struct Decision<'a> {
    pub action: &'a str,
    pub lower_pct: Option<f64>,
    pub upper_pct: Option<f64>,
    pub next: &'a str,
    pub note: &'a str,
}

/// The decision's line in the decisions file.
pub fn decision_line(look: u64, decision: &Decision) -> Result<String> {
    if crate::run_cmd::parse_duration(decision.next).is_none() {
        bail!(
            "--next {} is not a duration such as 30m, 4h or 2d",
            decision.next
        );
    }
    let mut row = serde_json::json!({
        "look": look,
        "action": decision.action,
        "next": decision.next,
        "note": decision.note,
    });
    match decision.action {
        "enter" => {
            let (Some(lower), Some(upper)) = (decision.lower_pct, decision.upper_pct) else {
                bail!("an entry needs --lower and --upper, in percent from the price now");
            };
            row["lower_pct"] = serde_json::json!(lower);
            row["upper_pct"] = serde_json::json!(upper);
        }
        "stay_out" | "hold" | "exit" => {}
        other => bail!("--action {other}: use stay_out, hold, exit or enter"),
    }
    // The line must read back as it was meant.
    crate::run_cmd::parse_decisions(&row.to_string())?;
    Ok(row.to_string())
}

/// Writes the decision for the pending look and shows the next one.
pub fn decide(run: &ResolvedRun, root: &Path, chain: &str, decision: &Decision) -> Result<()> {
    let decisions = decisions_path(run)?;
    let pending = pending_path(&decisions);
    let look = match std::fs::read_to_string(&pending) {
        Ok(text) => serde_json::from_str::<serde_json::Value>(&text)
            .ok()
            .and_then(|v| v.get("look").and_then(serde_json::Value::as_u64))
            .ok_or_else(|| {
                anyhow!(
                    "{} is damaged; run `lpbot agent next` again",
                    pending.display()
                )
            })?,
        Err(_) => bail!(
            "no look is waiting for a decision; run `lpbot agent next <experiment>` first to see it"
        ),
    };
    let line = decision_line(look, decision)?;
    if let Some(parent) = decisions.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut content = std::fs::read_to_string(&decisions).unwrap_or_default();
    if !content.is_empty() && !content.ends_with('\n') {
        content.push('\n');
    }
    content.push_str(&line);
    content.push('\n');
    std::fs::write(&decisions, content)
        .with_context(|| format!("writing {}", decisions.display()))?;
    std::fs::remove_file(&pending).ok();
    println!("look {look}: {}", decision.action);
    println!();
    next(run, root, chain).map(|_| ())
}

fn hours(seconds: u64) -> String {
    let h = seconds as f64 / 3_600.0;
    if h < 48.0 {
        format!("{h:.1} h")
    } else {
        format!("{:.1} d", h / 24.0)
    }
}

/// The observation as text for the agent: what it sees and nothing else.
pub fn describe(o: &Observation, pool: &str, value_token: &str) -> String {
    let mut text = String::new();
    let _ = writeln!(
        text,
        "Look {}, day {:.2} of the run, on {pool}. Prices are the volatile token's, in percent from now.",
        o.looks_before + 1,
        o.day
    );
    let _ = writeln!(
        text,
        "Fee {:.2}%, one tick spacing {:.3}%. Gas {:.3} gwei: entering and leaving cost {:.3}% of the run.",
        o.fee_pct, o.spacing_pct, o.gas_gwei, o.round_trip_gas_pct
    );
    let _ = writeln!(
        text,
        "Run so far: {:+.2}% in {value_token}, {} campaign(s){}.",
        o.result_pct,
        o.campaigns,
        o.since_last_close
            .map(|s| format!(", the last ended {} ago", hours(s)))
            .unwrap_or_default()
    );
    match &o.position {
        Some(p) => {
            let _ = writeln!(
                text,
                "Position: {:+.2}% to {:+.2}%, {}, held {}, result {:+.3}%, fees {:+.3}%, {:.0}% in {value_token}.",
                p.lower_pct,
                p.upper_pct,
                p.state,
                hours(p.held_seconds),
                p.result_pct,
                p.fees_pct,
                p.value_share_pct
            );
        }
        None => {
            let _ = writeln!(text, "Position: none.");
        }
    }
    let _ = writeln!(
        text,
        "{:>5} {:>8} {:>8} {:>8} {:>10} {:>7} {:>9}",
        "scale", "change", "high", "low", "vol/day", "swaps", "volume"
    );
    for s in &o.scales {
        let _ = writeln!(
            text,
            "{:>5} {:>+8.2} {:>+8.2} {:>+8.2} {:>10} {:>7} {:>9}",
            s.label,
            s.change_pct,
            s.high_pct,
            s.low_pct,
            s.vol_pct_per_day
                .map(|v| format!("{v:.2}"))
                .unwrap_or_else(|| "-".into()),
            s.swaps,
            s.volume_vs_30d
                .map(|v| format!("{v:.2}x"))
                .unwrap_or_else(|| "-".into())
        );
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use lpbot_sim::agent::{PositionView, ScaleView};

    #[test]
    fn a_decision_line_reads_back_and_bad_ones_are_refused() {
        let enter = Decision {
            action: "enter",
            lower_pct: Some(-3.0),
            upper_pct: Some(-1.0),
            next: "4h",
            note: "after a 4% fall",
        };
        let line = decision_line(1_800_000_000, &enter).unwrap();
        let back = crate::run_cmd::parse_decisions(&line).unwrap();
        assert_eq!(back.len(), 1);
        assert_eq!(back[0].look, 1_800_000_000);
        assert_eq!(back[0].next_look, 4 * 3_600);
        assert_eq!(back[0].note, "after a 4% fall");
        let missing = Decision {
            lower_pct: None,
            ..enter
        };
        assert!(decision_line(1, &missing).is_err());
        let wrong = Decision {
            action: "buy",
            ..enter
        };
        assert!(decision_line(1, &wrong).is_err());
        let late = Decision {
            next: "soon",
            ..enter
        };
        assert!(decision_line(1, &late).is_err());
    }

    #[test]
    fn an_observation_reads_as_text_with_every_scale() {
        let o = Observation {
            look: 42,
            looks_before: 2,
            day: 1.5,
            block: 7,
            scales: vec![ScaleView {
                label: "1h",
                seconds: 3_600,
                change_pct: -1.25,
                high_pct: 0.5,
                low_pct: -0.25,
                vol_pct_per_day: Some(3.0),
                swaps: 120,
                volume_vs_30d: Some(1.8),
            }],
            fee_pct: 0.05,
            spacing_pct: 0.1,
            gas_gwei: 0.2,
            round_trip_gas_pct: 0.004,
            result_pct: 1.0,
            campaigns: 3,
            since_last_close: Some(7_200),
            position: Some(PositionView {
                lower_pct: -2.0,
                upper_pct: -0.5,
                state: "in_range".into(),
                held_seconds: 3_600,
                result_pct: 0.1,
                fees_pct: 0.2,
                value_share_pct: 60.0,
            }),
        };
        let text = describe(&o, "USDC/WETH 0.05", "USDC");
        assert!(text.contains("Look 3, day 1.50"), "{text}");
        assert!(!text.contains("42"), "the look's time is not shown: {text}");
        assert!(text.contains("-2.00% to -0.50%, in_range"), "{text}");
        assert!(text.contains("   1h    -1.25"), "{text}");
        assert!(text.contains("1.80x"), "{text}");
        assert!(text.contains("the last ended 2.0 h ago"), "{text}");
    }
}
