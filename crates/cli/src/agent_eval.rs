//! `lpbot agent evaluate`: every decision of an agent run scored against
//! what happened and against the actions not taken (`docs/DESIGN.md`,
//! "Several agents in the agent mode").
//!
//! For each decision the run is replayed to its look, and from that state
//! each action the agent could have taken is run alone for the horizon:
//! the decision as made, cash (stay out, or exit what is open) and, with a
//! position open, holding it. The engine carries them out as it carries out
//! any decision, so gas, latency, the share cap, the campaign exits and the
//! loss limit are the same. The history is read once; the replays run on
//! several threads.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

use anyhow::{anyhow, bail, Result};
use lpbot_core::experiment::ResolvedRun;
use lpbot_data::lake::{swaps, TimedEvent};
use lpbot_sim::agent::{ticks_to_pct, AgentAction, AgentDecision, AgentPlan, Observation};
use lpbot_sim::engine::{Dollar, Output, Settings, Summary, Token};
use serde_json::{json, Value};

use crate::agent_log::{self, Expectation, Identity, SCHEMA};
use crate::run_cmd::{parse_duration, prepare_one, Prepared};

/// How long a decision is left alone before it is scored: until the
/// agent's own next look, or a duration.
#[derive(Debug, Clone, PartialEq)]
pub struct Horizon {
    pub label: String,
    pub seconds: Option<u64>,
}

/// Reads `next,1d,7d`.
pub fn parse_horizons(text: &str) -> Result<Vec<Horizon>> {
    let mut horizons = Vec::new();
    for part in text.split(',').map(str::trim).filter(|p| !p.is_empty()) {
        let seconds = if part == "next" {
            None
        } else {
            Some(parse_duration(part).filter(|s| *s > 0).ok_or_else(|| {
                anyhow!("--horizons {part}: use next or a duration such as 4h or 1d")
            })?)
        };
        if horizons.iter().any(|h: &Horizon| h.label == part) {
            continue;
        }
        horizons.push(Horizon {
            label: part.to_string(),
            seconds,
        });
    }
    if horizons.is_empty() {
        bail!("--horizons needs at least one, such as next,1d");
    }
    Ok(horizons)
}

/// The actions a decision is compared with.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Alt {
    Decision,
    Cash,
    Hold,
}

impl Alt {
    fn name(self) -> &'static str {
        match self {
            Alt::Decision => "decision",
            Alt::Cash => "cash",
            Alt::Hold => "hold",
        }
    }
}

/// Which alternative an action amounts to, with a position open or not:
/// flat, staying out, holding and exiting are all cash; open, staying out
/// is holding.
fn amounts_to(action: &AgentAction, open: bool) -> Alt {
    match (action, open) {
        (AgentAction::Enter { .. }, _) => Alt::Decision,
        (_, false) => Alt::Cash,
        (AgentAction::Exit, true) => Alt::Cash,
        (AgentAction::Hold | AgentAction::StayOut, true) => Alt::Hold,
    }
}

/// How a replay ended: the run's result then, in percent of its capital,
/// the time of the block it ended at, and whether it ended at a look or
/// with the window.
#[derive(Debug, Clone, Copy, PartialEq)]
struct End {
    result_pct: f64,
    as_of: u64,
    at_look: bool,
}

fn end_of(out: &Output, prepared: &Prepared) -> End {
    match &out.pending {
        Some(o) => End {
            result_pct: o.result_pct,
            as_of: prepared.block_time(o.block).unwrap_or(o.look),
            at_look: true,
        },
        None => End {
            result_pct: out.summary.result_pct,
            // A window open at the end ends at the newest stored block, not
            // at the end of its last day, which is still to come.
            as_of: match (prepared.open, prepared.newest) {
                (true, Some((_, time))) => time.min(prepared.settings.trade_to),
                _ => prepared.settings.trade_to,
            },
            at_look: false,
        },
    }
}

/// Where a replay stops at the look after its last decision. In a forward
/// run a look that is due without a decision is shown at the newest block,
/// which is not where the run's own looks were seen.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Stop {
    /// As the run is set up.
    Head,
    /// A forward run's look that was seen at the block of this time: the
    /// history ends there, so the look is shown where it was.
    Seen(u64),
    /// At the first block at or after the look is due, as in a backtest.
    Due,
}

impl Stop {
    /// The run's settings with this stop.
    fn apply(self, settings: &mut Settings) {
        match self {
            Stop::Head => {}
            Stop::Seen(time) => {
                settings.trade_to = settings.trade_to.min(time.saturating_add(1));
            }
            Stop::Due => settings.agent_forward = false,
        }
    }

    /// Where the look answered by `decision` was seen, in a forward run.
    fn seen(prepared: &Prepared, decision: Option<&AgentDecision>) -> Stop {
        match decision.and_then(|d| d.at) {
            Some(at) if prepared.open => Stop::Seen(at),
            _ => Stop::Head,
        }
    }
}

/// Runs each plan on the prepared history, on up to `jobs` threads, and
/// keeps what `keep` takes from each output.
fn replay_all<T: Send>(
    prepared: &Prepared,
    plans: &[(Vec<AgentDecision>, Stop)],
    jobs: usize,
    keep: impl Fn(&Output) -> T + Sync,
) -> Result<Vec<T>> {
    let next = AtomicUsize::new(0);
    let results: Mutex<Vec<Option<Result<T>>>> =
        Mutex::new((0..plans.len()).map(|_| None).collect());
    std::thread::scope(|scope| {
        for _ in 0..jobs.clamp(1, plans.len().max(1)) {
            scope.spawn(|| loop {
                let i = next.fetch_add(1, Ordering::Relaxed);
                let Some((plan, stop)) = plans.get(i) else {
                    break;
                };
                let mut settings: Settings = prepared.settings.clone();
                settings.agent = Some(AgentPlan {
                    decisions: plan.clone(),
                });
                stop.apply(&mut settings);
                let result = prepared.replay(&settings).map(|out| keep(&out));
                results.lock().expect("no thread panicked")[i] = Some(result);
            });
        }
    });
    results
        .into_inner()
        .expect("no thread panicked")
        .into_iter()
        .map(|r| r.unwrap_or_else(|| Err(anyhow!("a replay did not run"))))
        .collect()
}

/// The direction in ticks in which the volatile token's dollar price rises.
fn rise_sign(settings: &Settings) -> i32 {
    let dollar = match settings.dollar {
        Dollar::Value => settings.value,
        Dollar::Other => settings.value.other(),
    };
    if dollar == Token::One {
        1
    } else {
        -1
    }
}

/// What price did after `from` up to `to`: its change, highest and lowest,
/// in percent from its price at `from`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Moves {
    pub change_pct: f64,
    pub high_pct: f64,
    pub low_pct: f64,
}

pub fn moves(events: &[TimedEvent], sign: i32, from: u64, to: u64) -> Option<Moves> {
    let start = swaps(events)
        .take_while(|(time, _)| *time <= from)
        .last()
        .map(|(_, swap)| swap.tick)?;
    let first = events.partition_point(|e| e.time <= from);
    let (mut high, mut low, mut last) = (0, 0, 0);
    for (time, swap) in swaps(&events[first..]) {
        if time > to {
            break;
        }
        let rise = sign * (swap.tick - start);
        high = high.max(rise);
        low = low.min(rise);
        last = rise;
    }
    Some(Moves {
        change_pct: ticks_to_pct(f64::from(last)),
        high_pct: ticks_to_pct(f64::from(high)),
        low_pct: ticks_to_pct(f64::from(low)),
    })
}

/// A band held when price stayed within it; the Brier score is the square
/// of the confidence's miss.
fn band_score(expect: &Expectation, moves: &Moves) -> Option<(bool, Option<f64>)> {
    let (low, high) = (expect.low_pct?, expect.high_pct?);
    let held = moves.low_pct >= low && moves.high_pct <= high;
    let brier = expect
        .confidence
        .map(|p| (p - if held { 1.0 } else { 0.0 }).powi(2));
    Some((held, brier))
}

/// One decision scored over one horizon.
#[derive(Debug, Clone, PartialEq)]
pub struct Score {
    pub horizon: String,
    pub open: bool,
    pub decision: f64,
    pub cash: f64,
    pub hold: Option<f64>,
    pub band: Option<(bool, Option<f64>)>,
}

impl Score {
    fn best(&self) -> (Alt, f64) {
        let mut best = (Alt::Decision, self.decision);
        for (alt, value) in [(Alt::Cash, Some(self.cash)), (Alt::Hold, self.hold)] {
            if let Some(v) = value {
                if v > best.1 + 1e-9 {
                    best = (alt, v);
                }
            }
        }
        best
    }

    pub fn regret(&self) -> f64 {
        self.best().1 - self.decision
    }
}

/// The scores of one horizon added up.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Tally {
    pub decisions: usize,
    pub edge_cash: f64,
    pub edge_hold: f64,
    pub open: usize,
    pub regret: f64,
    pub best: usize,
    pub bands: usize,
    pub held: usize,
    pub brier: f64,
    pub briers: usize,
}

impl Tally {
    pub fn add(&mut self, score: &Score) {
        self.decisions += 1;
        self.edge_cash += score.decision - score.cash;
        if let Some(hold) = score.hold {
            self.open += 1;
            self.edge_hold += score.decision - hold;
        }
        self.regret += score.regret();
        if score.best().0 == Alt::Decision {
            self.best += 1;
        }
        if let Some((held, brier)) = score.band {
            self.bands += 1;
            self.held += usize::from(held);
            if let Some(b) = brier {
                self.brier += b;
                self.briers += 1;
            }
        }
    }

    fn mean(sum: f64, n: usize) -> String {
        if n == 0 {
            "-".into()
        } else {
            format!("{:+.3}%", sum / n as f64)
        }
    }

    fn line(&self) -> String {
        let band = if self.bands == 0 {
            "no bands given".to_string()
        } else {
            let brier = if self.briers == 0 {
                String::new()
            } else {
                format!(", Brier {:.3}", self.brier / self.briers as f64)
            };
            format!("band held {} of {}{brier}", self.held, self.bands)
        };
        format!(
            "edge over cash {} a decision ({}), over holding {} ({} with a position open); \
             regret {:.3}% in all; best {} of {}; {band}",
            Self::mean(self.edge_cash, self.decisions),
            self.decisions,
            Self::mean(self.edge_hold, self.open),
            self.open,
            self.regret,
            self.best,
            self.decisions
        )
    }
}

/// One agent's run, scored.
#[derive(Debug, Clone)]
pub struct Report {
    pub name: String,
    pub model: String,
    pub value_token: String,
    pub summary: Summary,
    pub scored: usize,
    pub tallies: Vec<(String, Tally)>,
    pub written: std::path::PathBuf,
}

impl Report {
    pub fn describe(&self) -> String {
        let s = &self.summary;
        let mut text = format!(
            "Agent {} ({}): {} decision(s) scored. The run: {:+.2}% in {} ({:+.2}% in dollars; \
             holding {} made {:+.2}% in dollars), {} campaign(s), fees {:.4}, gas {:.4}.\n",
            self.name,
            self.model,
            self.scored,
            s.result_pct,
            self.value_token,
            s.result_pct_usd,
            self.value_token,
            s.hold_value_pct_usd,
            s.campaigns,
            s.fees,
            s.gas
        );
        for (label, tally) in &self.tallies {
            let _ = writeln!(text, "  over {label}: {}", tally.line());
        }
        let _ = writeln!(text, "  Scores: {}", self.written.display());
        text
    }
}

/// The agents' scores side by side, one row per horizon and agent.
pub fn compare(reports: &[Report]) -> String {
    let mut text = String::new();
    let _ = writeln!(
        text,
        "{:<8} {:<10} {:<20} {:>9} {:>10} {:>10} {:>9} {:>8} {:>9} {:>7}",
        "horizon",
        "agent",
        "model",
        "decisions",
        "edge/cash",
        "edge/hold",
        "regret",
        "best",
        "band",
        "Brier"
    );
    let labels: Vec<String> = reports
        .first()
        .map(|r| r.tallies.iter().map(|(l, _)| l.clone()).collect())
        .unwrap_or_default();
    for label in labels {
        for r in reports {
            let Some((_, t)) = r.tallies.iter().find(|(l, _)| *l == label) else {
                continue;
            };
            let _ = writeln!(
                text,
                "{:<8} {:<10} {:<20} {:>9} {:>10} {:>10} {:>8.3}% {:>8} {:>9} {:>7}",
                label,
                r.name,
                r.model,
                t.decisions,
                Tally::mean(t.edge_cash, t.decisions),
                Tally::mean(t.edge_hold, t.open),
                t.regret,
                format!("{}/{}", t.best, t.decisions),
                if t.bands == 0 {
                    "-".to_string()
                } else {
                    format!("{}/{}", t.held, t.bands)
                },
                if t.briers == 0 {
                    "-".to_string()
                } else {
                    format!("{:.3}", t.brier / t.briers as f64)
                }
            );
        }
    }
    let _ = writeln!(
        text,
        "\n{:<10} {:<20} {:>12} {:>10} {:>10} {:>10} {:>10} {:>10}",
        "agent", "model", "result", "in $", "hold in $", "campaigns", "fees", "gas"
    );
    for r in reports {
        let s = &r.summary;
        let _ = writeln!(
            text,
            "{:<10} {:<20} {:>11.2}% {:>9.2}% {:>9.2}% {:>10} {:>10.4} {:>10.4}",
            r.name,
            r.model,
            s.result_pct,
            s.result_pct_usd,
            s.hold_value_pct_usd,
            s.campaigns,
            s.fees,
            s.gas
        );
    }
    text
}

fn action_json(action: &AgentAction, next: &str) -> Value {
    match action {
        AgentAction::Enter {
            lower_pct,
            upper_pct,
        } => {
            json!({"action": "enter", "lower_pct": lower_pct, "upper_pct": upper_pct, "next": next})
        }
        other => json!({"action": other.name(), "next": next}),
    }
}

fn duration_label(seconds: u64) -> String {
    if seconds.is_multiple_of(86_400) {
        format!("{}d", seconds / 86_400)
    } else if seconds.is_multiple_of(3_600) {
        format!("{}h", seconds / 3_600)
    } else if seconds.is_multiple_of(60) {
        format!("{}m", seconds / 60)
    } else {
        format!("{seconds}s")
    }
}

/// Truncated scores remain useful to the evaluator and dashboard, but must
/// not be described as completed outcomes to another decider.
fn horizon_complete(horizon: &Horizon, start: u64, end: End, next_as_of: Option<u64>) -> bool {
    let Some(seconds) = end.as_of.checked_sub(start).filter(|seconds| *seconds > 0) else {
        return false;
    };
    match horizon.seconds {
        Some(requested) => seconds >= requested,
        None => end.at_look && next_as_of == Some(end.as_of),
    }
}

/// Scores every decision of one agent run and writes `<name>.eval.jsonl`.
pub fn evaluate(
    run: &ResolvedRun,
    who: &Identity,
    root: &std::path::Path,
    chain: &str,
    horizons: &[Horizon],
    jobs: usize,
) -> Result<Report> {
    let strategy = run
        .config
        .strategies
        .first()
        .ok_or_else(|| anyhow!("the run has no strategy"))?;
    let prepared = prepare_one(run, root, chain)?;
    let decisions = prepared
        .settings
        .agent
        .as_ref()
        .map(|plan| plan.decisions.clone())
        .unwrap_or_default();
    let actual = prepared.replay(&prepared.settings)?;
    let used = decisions.len().saturating_sub(actual.unused_decisions);
    if used == 0 {
        bail!(
            "{} has no decisions to score; decide its looks with `lpbot agent decide` first",
            who.decisions.display()
        );
    }
    let expectations: HashMap<u64, Expectation> =
        agent_log::read_records(&agent_log::records_path(&who.decisions))?
            .iter()
            .filter(|r| r["kind"] == "look")
            .filter_map(|r| Some((r["look"].as_u64()?, Expectation::from_json(&r["expect"])?)))
            .collect();

    // The run at every look: each plan stops at the look after its last
    // decision, which is also where the decision before it, as made, ends.
    // In a forward run each look is shown where it was seen.
    let prefixes: Vec<(Vec<AgentDecision>, Stop)> = (0..=used)
        .map(|k| {
            let answered = (k < used).then(|| &decisions[k]);
            (decisions[..k].to_vec(), Stop::seen(&prepared, answered))
        })
        .collect();
    let looks: Vec<(Option<Observation>, End)> = replay_all(&prepared, &prefixes, jobs, |out| {
        (out.pending.clone(), end_of(out, &prepared))
    })?;
    let mut at: Vec<(Observation, u64)> = Vec::with_capacity(used);
    for (k, decision) in decisions[..used].iter().enumerate() {
        match &looks[k].0 {
            Some(o) if o.look == decision.look => at.push((o.clone(), looks[k].1.as_of)),
            _ => bail!(
                "decision {} no longer answers the look the run asks; run `lpbot agent next` to see it",
                k + 1
            ),
        }
    }

    // Every alternative that needs a replay of its own.
    let mut jobs_list: Vec<(usize, usize, Alt)> = Vec::new();
    let mut plans: Vec<(Vec<AgentDecision>, Stop)> = Vec::new();
    for (k, (o, _)) in at.iter().enumerate() {
        let open = o.position.is_some();
        let chosen = amounts_to(&decisions[k].action, open);
        for (h, horizon) in horizons.iter().enumerate() {
            let mut alts = vec![Alt::Decision];
            if chosen != Alt::Cash {
                alts.push(Alt::Cash);
            }
            if open && chosen != Alt::Hold {
                alts.push(Alt::Hold);
            }
            for alt in alts {
                if alt == Alt::Decision && horizon.seconds.is_none() {
                    continue; // the run itself, at the next look
                }
                let action = match alt {
                    Alt::Decision => decisions[k].action,
                    Alt::Cash if open => AgentAction::Exit,
                    Alt::Cash => AgentAction::StayOut,
                    Alt::Hold => AgentAction::Hold,
                };
                let mut plan = decisions[..k].to_vec();
                plan.push(AgentDecision {
                    look: decisions[k].look,
                    // Acted on where the decision was, never earlier.
                    at: decisions[k].at,
                    action,
                    next_look: horizon.seconds.unwrap_or(decisions[k].next_look),
                    note: String::new(),
                });
                // A fixed horizon ends when it is due; until the next look,
                // where that look was seen.
                let stop = match horizon.seconds {
                    Some(_) => Stop::Due,
                    None => Stop::seen(&prepared, decisions.get(k + 1).filter(|_| k + 1 < used)),
                };
                jobs_list.push((k, h, alt));
                plans.push((plan, stop));
            }
        }
    }
    let ends = replay_all(&prepared, &plans, jobs, |out| end_of(out, &prepared))?;
    let mut found: HashMap<(usize, usize, Alt), End> = HashMap::new();
    for (job, end) in jobs_list.into_iter().zip(ends) {
        found.insert(job, end);
    }

    let sign = rise_sign(&prepared.settings);
    let mut records = Vec::new();
    let mut tallies: Vec<(String, Tally)> = horizons
        .iter()
        .map(|h| (h.label.clone(), Tally::default()))
        .collect();
    for (k, (o, start_as_of)) in at.iter().enumerate() {
        let open = o.position.is_some();
        let chosen = amounts_to(&decisions[k].action, open);
        let next_label = duration_label(decisions[k].next_look);
        for (h, horizon) in horizons.iter().enumerate() {
            let decision_end = if horizon.seconds.is_none() {
                looks[k + 1].1
            } else {
                found[&(k, h, Alt::Decision)]
            };
            let end_for = |alt: Alt| -> End {
                if alt == chosen {
                    decision_end
                } else {
                    found[&(k, h, alt)]
                }
            };
            let change = |end: End| end.result_pct - o.result_pct;
            let price = moves(prepared.events(), sign, *start_as_of, decision_end.as_of);
            let expect = expectations.get(&decisions[k].look);
            let band = match (horizon.seconds, expect, price) {
                (None, Some(expect), Some(price)) => band_score(expect, &price),
                _ => None,
            };
            let score = Score {
                horizon: horizon.label.clone(),
                open,
                decision: change(decision_end),
                cash: change(end_for(Alt::Cash)),
                hold: open.then(|| change(end_for(Alt::Hold))),
                band,
            };
            tallies[h].1.add(&score);
            let mut alternatives = vec![
                json!({"alt": "decision", "action": decisions[k].action.name(), "change_pct": score.decision}),
                json!({"alt": "cash", "action": if open { "exit" } else { "stay_out" }, "change_pct": score.cash}),
            ];
            if let Some(hold) = score.hold {
                alternatives.push(json!({"alt": "hold", "action": "hold", "change_pct": hold}));
            }
            let (best, _) = score.best();
            let seconds = decision_end.as_of.saturating_sub(*start_as_of);
            let next = if horizon.seconds.is_none() {
                at.get(k + 1)
            } else {
                None
            };
            records.push(json!({
                "schema": SCHEMA,
                "kind": "eval",
                "agent": who.name,
                "model": who.model,
                "experiment": run.experiment,
                "run_id": run.run_id,
                "look": decisions[k].look,
                "number": k + 1,
                "day": o.day,
                "decided_as_of": start_as_of,
                "as_of": decision_end.as_of,
                "horizon": horizon.label,
                "horizon_seconds": horizon.seconds.unwrap_or(seconds),
                "scored_seconds": seconds,
                "complete": horizon_complete(horizon, *start_as_of, decision_end, next.map(|(_, as_of)| *as_of)),
                "next_look": next.map(|(observation, _)| observation.look),
                "next_as_of": next.map(|(_, as_of)| as_of),
                "ended": if decision_end.at_look { "look" } else { "window_end" },
                "decision": action_json(&decisions[k].action, &next_label),
                "position_open": open,
                "alternatives": alternatives,
                "best": best.name(),
                "regret_pct": score.regret(),
                "edge_vs_cash_pct": score.decision - score.cash,
                "edge_vs_hold_pct": score.hold.map(|hold| score.decision - hold),
                "price": price.map(|p| json!({
                    "change_pct": p.change_pct, "high_pct": p.high_pct, "low_pct": p.low_pct,
                })),
                "expect": match (expect, band) {
                    (Some(e), Some((held, brier))) => json!({
                        "low_pct": e.low_pct, "high_pct": e.high_pct,
                        "confidence": e.confidence, "held": held, "brier": brier,
                    }),
                    _ => Value::Null,
                },
                "scored_unix": agent_log::now_unix(),
            }));
        }
    }
    // Replaced whole: a peer reading the scores now sees the old ones or
    // the new ones, never a file that is missing for a moment.
    let written = agent_log::scores_path(&who.decisions);
    agent_log::write_records(&written, &records)?;
    Ok(Report {
        name: who.name.clone(),
        model: who.model.clone(),
        value_token: strategy.value_token.clone(),
        summary: actual.summary,
        scored: used,
        tallies,
        written,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::{I256, U256};
    use lpbot_venues::uniswap_v3::events::PoolEvent;
    use lpbot_venues::uniswap_v3::pool::SwapEvent;

    #[test]
    fn horizons_read_as_the_next_look_or_durations() {
        let h = parse_horizons("next, 1d,4h,1d").unwrap();
        assert_eq!(
            h,
            vec![
                Horizon {
                    label: "next".into(),
                    seconds: None
                },
                Horizon {
                    label: "1d".into(),
                    seconds: Some(86_400)
                },
                Horizon {
                    label: "4h".into(),
                    seconds: Some(14_400)
                },
            ]
        );
        assert!(parse_horizons("soon").is_err());
        assert!(parse_horizons("0h").is_err());
        assert!(parse_horizons(" , ").is_err());
        assert_eq!(duration_label(4 * 3_600), "4h");
        assert_eq!(duration_label(2 * 86_400), "2d");
        assert_eq!(duration_label(90), "90s");
    }

    #[test]
    fn completion_uses_the_actual_interval_and_a_committed_next_decision() {
        let horizons = parse_horizons("next,1h").unwrap();
        let end = End {
            result_pct: 0.0,
            as_of: 3_700,
            at_look: true,
        };
        assert!(horizon_complete(&horizons[1], 100, end, None));
        assert!(!horizon_complete(&horizons[1], 101, end, None));
        assert!(!horizon_complete(&horizons[1], 3_701, end, None));
        assert!(horizon_complete(&horizons[0], 100, end, Some(3_700)));
        assert!(!horizon_complete(&horizons[0], 100, end, None));
        assert!(!horizon_complete(&horizons[0], 100, end, Some(3_800)));
        assert!(!horizon_complete(
            &horizons[0],
            100,
            End {
                at_look: false,
                ..end
            },
            Some(3_700)
        ));
    }

    #[test]
    fn an_action_amounts_to_cash_hold_or_the_entry() {
        let enter = AgentAction::Enter {
            lower_pct: -2.0,
            upper_pct: -1.0,
        };
        assert_eq!(amounts_to(&enter, false), Alt::Decision);
        assert_eq!(amounts_to(&enter, true), Alt::Decision);
        for flat in [AgentAction::StayOut, AgentAction::Hold, AgentAction::Exit] {
            assert_eq!(amounts_to(&flat, false), Alt::Cash);
        }
        assert_eq!(amounts_to(&AgentAction::Exit, true), Alt::Cash);
        assert_eq!(amounts_to(&AgentAction::Hold, true), Alt::Hold);
        assert_eq!(amounts_to(&AgentAction::StayOut, true), Alt::Hold);
    }

    fn swap(block: u64, time: u64, tick: i32) -> TimedEvent {
        TimedEvent {
            block,
            time,
            log_index: 0,
            event: PoolEvent::Swap(SwapEvent {
                amount0: I256::ZERO,
                amount1: I256::ZERO,
                sqrt_price_x96: U256::ZERO,
                liquidity: 0,
                tick,
            }),
        }
    }

    #[test]
    fn price_moves_are_measured_from_the_look_in_the_rising_direction() {
        let events = vec![
            swap(1, 100, 1_000),
            swap(2, 200, 1_100),
            swap(3, 300, 900),
            swap(4, 400, 1_050),
            swap(5, 500, 2_000),
        ];
        // Rising with the tick: from 1,000 at time 100 to 1,050 at 400.
        let up = moves(&events, 1, 100, 400).unwrap();
        assert!((up.change_pct - ticks_to_pct(50.0)).abs() < 1e-9);
        assert!((up.high_pct - ticks_to_pct(100.0)).abs() < 1e-9);
        assert!((up.low_pct - ticks_to_pct(-100.0)).abs() < 1e-9);
        // Rising as the tick falls, as for ETH in USDC/WETH.
        let down = moves(&events, -1, 100, 400).unwrap();
        assert!((down.change_pct - ticks_to_pct(-50.0)).abs() < 1e-9);
        assert!((down.high_pct - ticks_to_pct(100.0)).abs() < 1e-9);
        // Nothing traded since: no move; nothing traded before: unknown.
        let still = moves(&events, 1, 500, 900).unwrap();
        assert_eq!(
            (still.change_pct, still.high_pct, still.low_pct),
            (0.0, 0.0, 0.0)
        );
        assert!(moves(&events, 1, 50, 900).is_none());
    }

    #[test]
    fn a_score_finds_the_best_alternative_and_the_band() {
        let score = Score {
            horizon: "next".into(),
            open: true,
            decision: 0.10,
            cash: 0.00,
            hold: Some(0.25),
            band: None,
        };
        assert_eq!(score.best(), (Alt::Hold, 0.25));
        assert!((score.regret() - 0.15).abs() < 1e-12);
        let tie = Score {
            hold: Some(0.10),
            ..score.clone()
        };
        assert_eq!(tie.best().0, Alt::Decision, "a tie goes to the decision");

        let expect = Expectation {
            low_pct: Some(-1.0),
            high_pct: Some(1.0),
            confidence: Some(0.8),
            text: String::new(),
        };
        let inside = Moves {
            change_pct: 0.2,
            high_pct: 0.9,
            low_pct: -0.5,
        };
        let (held, brier) = band_score(&expect, &inside).unwrap();
        assert!(held);
        assert!((brier.unwrap() - 0.04).abs() < 1e-12);
        let outside = Moves {
            high_pct: 1.2,
            ..inside
        };
        let (held, brier) = band_score(&expect, &outside).unwrap();
        assert!(!held);
        assert!((brier.unwrap() - 0.64).abs() < 1e-12);
        assert!(band_score(&Expectation::default(), &inside).is_none());

        let mut tally = Tally::default();
        tally.add(&Score {
            band: Some((true, Some(0.04))),
            ..score.clone()
        });
        tally.add(&Score {
            open: false,
            decision: 0.0,
            cash: 0.0,
            hold: None,
            band: Some((false, None)),
            ..score
        });
        assert_eq!((tally.decisions, tally.open, tally.best), (2, 1, 1));
        assert!((tally.edge_cash - 0.10).abs() < 1e-12);
        assert!((tally.edge_hold + 0.15).abs() < 1e-12);
        assert_eq!((tally.bands, tally.held, tally.briers), (2, 1, 1));
        let line = tally.line();
        assert!(
            line.contains("edge over cash +0.050% a decision (2)"),
            "{line}"
        );
        assert!(
            line.contains("over holding -0.150% (1 with a position open)"),
            "{line}"
        );
        assert!(line.contains("band held 1 of 2, Brier 0.040"), "{line}");
    }
}

#[cfg(test)]
#[path = "agent_panel_tests.rs"]
mod panel_tests;
