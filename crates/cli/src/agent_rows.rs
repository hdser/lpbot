//! The agent tables' rows (`lpbot_sim::agent_tables`) from an agent's
//! shared log and scores, for `lpbot run` to write beside the six result
//! tables and `lpbot load-results` to put in front of the viewer.

use anyhow::Result;
use lpbot_sim::agent_tables::{AgentLookRow, AgentScoreRow};
use serde_json::Value;

use crate::agent_log::{read_records, records_path, scores_path, Identity};

fn int(v: &Value) -> i64 {
    v.as_i64().unwrap_or(0)
}

fn text(v: &Value) -> String {
    v.as_str().unwrap_or_default().to_string()
}

fn scale(seen: &Value, label: &str, field: &str) -> Option<f64> {
    seen["scales"]
        .as_array()?
        .iter()
        .find(|s| s["label"] == label)?[field]
        .as_f64()
}

/// A look's row, from its record in the shared log.
pub fn look_row(r: &Value, who: &Identity) -> AgentLookRow {
    let seen = &r["seen"];
    let position = seen["position"].as_object().map(|p| {
        format!(
            "{:+.2}% to {:+.2}%, {}",
            p.get("lower_pct").and_then(Value::as_f64).unwrap_or(0.0),
            p.get("upper_pct").and_then(Value::as_f64).unwrap_or(0.0),
            p.get("state").and_then(Value::as_str).unwrap_or("?")
        )
    });
    AgentLookRow {
        agent: r["agent"].as_str().unwrap_or(&who.name).to_string(),
        model: r["model"].as_str().unwrap_or(&who.model).to_string(),
        number: int(&r["number"]),
        time: int(&r["as_of"]),
        look: int(&r["look"]),
        day: r["day"].as_f64().unwrap_or(0.0),
        block: int(&r["block"]),
        action: text(&r["decision"]["action"]),
        lower_pct: r["decision"]["lower_pct"].as_f64(),
        upper_pct: r["decision"]["upper_pct"].as_f64(),
        next: text(&r["decision"]["next"]),
        note: text(&r["note"]),
        reasoning: text(&r["reasoning"]),
        expect_low_pct: r["expect"]["low_pct"].as_f64(),
        expect_high_pct: r["expect"]["high_pct"].as_f64(),
        expect_confidence: r["expect"]["confidence"].as_f64(),
        expect_text: text(&r["expect"]["text"]),
        stance: r["stance"]["kind"].as_str().map(str::to_string),
        toward: r["stance"]["toward"].as_str().map(str::to_string),
        shown: r["shown"].as_array().map_or(0, |s| s.len() as i64),
        result_pct: seen["result_pct"].as_f64(),
        position,
        change_1h_pct: scale(seen, "1h", "change_pct"),
        change_1d_pct: scale(seen, "1d", "change_pct"),
        change_7d_pct: scale(seen, "7d", "change_pct"),
        vol_1d_pct: scale(seen, "1d", "vol_pct_per_day"),
        gas_gwei: seen["gas_gwei"].as_f64(),
    }
}

/// A score's row, from its record in the scores file.
pub fn score_row(r: &Value, who: &Identity) -> AgentScoreRow {
    let alternative = |name: &str| {
        r["alternatives"]
            .as_array()?
            .iter()
            .find(|a| a["alt"] == name)?["change_pct"]
            .as_f64()
    };
    AgentScoreRow {
        agent: r["agent"].as_str().unwrap_or(&who.name).to_string(),
        model: r["model"].as_str().unwrap_or(&who.model).to_string(),
        number: int(&r["number"]),
        look: int(&r["look"]),
        time: int(&r["decided_as_of"]),
        end_time: int(&r["as_of"]),
        day: r["day"].as_f64().unwrap_or(0.0),
        horizon: text(&r["horizon"]),
        horizon_seconds: int(&r["horizon_seconds"]),
        ended: text(&r["ended"]),
        action: text(&r["decision"]["action"]),
        lower_pct: r["decision"]["lower_pct"].as_f64(),
        upper_pct: r["decision"]["upper_pct"].as_f64(),
        position_open: r["position_open"].as_bool().unwrap_or(false),
        decision_pct: alternative("decision").unwrap_or(0.0),
        cash_pct: alternative("cash"),
        hold_pct: alternative("hold"),
        best: text(&r["best"]),
        regret_pct: r["regret_pct"].as_f64().unwrap_or(0.0),
        edge_vs_cash_pct: r["edge_vs_cash_pct"].as_f64(),
        edge_vs_hold_pct: r["edge_vs_hold_pct"].as_f64(),
        price_change_pct: r["price"]["change_pct"].as_f64(),
        price_high_pct: r["price"]["high_pct"].as_f64(),
        price_low_pct: r["price"]["low_pct"].as_f64(),
        band_held: r["expect"]["held"].as_bool(),
        brier: r["expect"]["brier"].as_f64(),
    }
}

/// The rows of an agent run: its decided looks from the shared log and its
/// scores, if `lpbot agent evaluate` has written them.
pub fn rows(who: &Identity) -> Result<(Vec<AgentLookRow>, Vec<AgentScoreRow>)> {
    let looks = read_records(&records_path(&who.decisions))?
        .iter()
        .filter(|r| r["kind"] == "look" && r.get("decision").is_some())
        .map(|r| look_row(r, who))
        .collect();
    let scores = read_records(&scores_path(&who.decisions))?
        .iter()
        .filter(|r| r["kind"] == "eval")
        .map(|r| score_row(r, who))
        .collect();
    Ok((looks, scores))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn who() -> Identity {
        Identity {
            name: "b".into(),
            model: "claude-opus-5-5".into(),
            decisions: "p/b.jsonl".into(),
            peers: Vec::new(),
        }
    }

    #[test]
    fn a_look_record_becomes_a_row() {
        let r = json!({
            "kind": "look", "agent": "b", "model": "claude-opus-5-5", "number": 3,
            "look": 1_000, "as_of": 1_005, "day": 0.5, "block": 77,
            "seen": {"result_pct": 0.2, "gas_gwei": 1.5,
                     "position": {"lower_pct": -2.0, "upper_pct": -1.0, "state": "resting"},
                     "scales": [{"label": "1h", "change_pct": -0.4},
                                {"label": "1d", "change_pct": -2.5, "vol_pct_per_day": 3.1}]},
            "decision": {"action": "enter", "lower_pct": -3.0, "upper_pct": -1.0, "next": "4h"},
            "note": "n", "reasoning": "r",
            "expect": {"low_pct": -2.0, "high_pct": 1.0, "confidence": 0.6, "text": "t"},
            "stance": {"kind": "adapt", "toward": "a"},
            "shown": [{"agent": "a"}, {"agent": "a"}],
        });
        let row = look_row(&r, &who());
        assert_eq!(
            (row.number, row.time, row.look, row.block),
            (3, 1_005, 1_000, 77)
        );
        assert_eq!(row.action, "enter");
        assert_eq!((row.lower_pct, row.upper_pct), (Some(-3.0), Some(-1.0)));
        assert_eq!(row.expect_confidence, Some(0.6));
        assert_eq!(row.stance.as_deref(), Some("adapt"));
        assert_eq!(row.shown, 2);
        assert_eq!(row.position.as_deref(), Some("-2.00% to -1.00%, resting"));
        assert_eq!((row.change_1h_pct, row.change_7d_pct), (Some(-0.4), None));
        assert_eq!(row.vol_1d_pct, Some(3.1));

        let bare = look_row(
            &json!({"kind": "look", "decision": {"action": "hold", "next": "1d"},
                    "expect": null, "stance": null, "seen": {"position": null}}),
            &who(),
        );
        assert_eq!(
            (bare.agent.as_str(), bare.model.as_str()),
            ("b", "claude-opus-5-5")
        );
        assert_eq!(
            (bare.stance, bare.position, bare.expect_low_pct),
            (None, None, None)
        );
    }

    #[test]
    fn a_score_record_becomes_a_row() {
        let r = json!({
            "kind": "eval", "agent": "b", "number": 2, "look": 1_000, "decided_as_of": 1_005,
            "as_of": 4_605, "day": 0.5, "horizon": "next", "horizon_seconds": 3_600,
            "ended": "look", "decision": {"action": "hold"}, "position_open": true,
            "alternatives": [{"alt": "decision", "change_pct": 0.1},
                             {"alt": "cash", "change_pct": -0.02},
                             {"alt": "hold", "change_pct": 0.1}],
            "best": "decision", "regret_pct": 0.0, "edge_vs_cash_pct": 0.12,
            "edge_vs_hold_pct": 0.0,
            "price": {"change_pct": 0.5, "high_pct": 0.9, "low_pct": -0.1},
            "expect": {"held": false, "brier": 0.36},
        });
        let row = score_row(&r, &who());
        assert_eq!(
            (row.time, row.end_time, row.horizon_seconds),
            (1_005, 4_605, 3_600)
        );
        assert_eq!(
            (row.decision_pct, row.cash_pct, row.hold_pct),
            (0.1, Some(-0.02), Some(0.1))
        );
        assert!(row.position_open);
        assert_eq!((row.band_held, row.brier), (Some(false), Some(0.36)));
        assert_eq!(row.price_high_pct, Some(0.9));
        assert_eq!(
            row.model, "claude-opus-5-5",
            "from the identity when not recorded"
        );
        let flat = score_row(
            &json!({"kind": "eval", "expect": null, "price": null}),
            &who(),
        );
        assert_eq!(
            (flat.hold_pct, flat.band_held, flat.price_change_pct),
            (None, None, None)
        );
    }
}
