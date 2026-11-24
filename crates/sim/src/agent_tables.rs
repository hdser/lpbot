//! The two agent tables an agent run writes besides the six result tables
//! (`docs/DESIGN.md`, "Several agents in the agent mode"), for the viewer:
//!
//! - `agent_looks`: one row a look, with what the agent saw, decided,
//!   expected and why, and how it stood toward its peers;
//! - `agent_scores`: one row a decision and horizon, with what the decision
//!   and the actions not taken made over it.
//!
//! Both are keyed by `run_id` like the result tables, so they join with
//! `runs` and `series`, and `lpbot load-results` replaces a run's rows.
//! Times are Unix seconds, percentages are percent.

use std::path::{Path, PathBuf};

use crate::tables::{write_table, Column, Table, TableError};

/// One look of an agent run.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct AgentLookRow {
    pub agent: String,
    pub model: String,
    /// The look's number in the run, from 1.
    pub number: i64,
    /// The time of the block the look was shown at.
    pub time: i64,
    /// The look's time as the decisions file keys it.
    pub look: i64,
    pub day: f64,
    pub block: i64,
    pub action: String,
    /// An entry's range, in percent from the price at the look.
    pub lower_pct: Option<f64>,
    pub upper_pct: Option<f64>,
    pub next: String,
    pub note: String,
    pub reasoning: String,
    /// The band expected until the next look, and how sure.
    pub expect_low_pct: Option<f64>,
    pub expect_high_pct: Option<f64>,
    pub expect_confidence: Option<f64>,
    pub expect_text: String,
    /// `own`, `copy`, `adapt` or `depart`, and toward which peer; empty
    /// when no peers' records were shown.
    pub stance: Option<String>,
    pub toward: Option<String>,
    /// Peers' records the look was handed.
    pub shown: i64,
    /// What the look showed: the run so far, the position, the price moves.
    pub result_pct: Option<f64>,
    pub position: Option<String>,
    pub change_1h_pct: Option<f64>,
    pub change_1d_pct: Option<f64>,
    pub change_7d_pct: Option<f64>,
    pub vol_1d_pct: Option<f64>,
    pub gas_gwei: Option<f64>,
}

/// One decision scored over one horizon.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct AgentScoreRow {
    pub agent: String,
    pub model: String,
    pub number: i64,
    pub look: i64,
    /// When the decision was made and when its horizon ended.
    pub time: i64,
    pub end_time: i64,
    pub day: f64,
    /// `next` or a duration such as `1d`.
    pub horizon: String,
    pub horizon_seconds: i64,
    /// `look`, or `window_end` when the horizon ran past the window.
    pub ended: String,
    pub action: String,
    pub lower_pct: Option<f64>,
    pub upper_pct: Option<f64>,
    pub position_open: bool,
    /// The run's equity change over the horizon, in percent of its capital,
    /// for the decision and the actions not taken.
    pub decision_pct: f64,
    pub cash_pct: Option<f64>,
    pub hold_pct: Option<f64>,
    pub best: String,
    pub regret_pct: f64,
    pub edge_vs_cash_pct: Option<f64>,
    pub edge_vs_hold_pct: Option<f64>,
    /// What price did over the horizon, from the price at the look.
    pub price_change_pct: Option<f64>,
    pub price_high_pct: Option<f64>,
    pub price_low_pct: Option<f64>,
    /// Whether the expected band held, and the Brier score of its confidence.
    pub band_held: Option<bool>,
    pub brier: Option<f64>,
}

fn text(values: impl IntoIterator<Item = String>) -> Column {
    Column::Text(values.into_iter().map(Some).collect())
}

fn text_or_empty(values: impl IntoIterator<Item = Option<String>>) -> Column {
    Column::Text(values.into_iter().collect())
}

fn int(values: impl IntoIterator<Item = i64>) -> Column {
    Column::Int(values.into_iter().map(Some).collect())
}

fn float(values: impl IntoIterator<Item = f64>) -> Column {
    Column::Float(values.into_iter().map(Some).collect())
}

fn float_or_empty(values: impl IntoIterator<Item = Option<f64>>) -> Column {
    Column::Float(values.into_iter().collect())
}

fn keyed(name: &str, run_id: &str, rows: usize, columns: Vec<(&str, Column)>) -> Table {
    let mut all = vec![(
        "run_id".to_string(),
        Column::Text(vec![Some(run_id.to_string()); rows]),
    )];
    all.extend(columns.into_iter().map(|(n, c)| (n.to_string(), c)));
    Table {
        name: name.to_string(),
        columns: all,
    }
}

/// The two agent tables of a run, in the order of `AGENT_TABLES`.
pub fn agent_tables(run_id: &str, looks: &[AgentLookRow], scores: &[AgentScoreRow]) -> Vec<Table> {
    let l = looks;
    let s = scores;
    vec![
        keyed(
            "agent_looks",
            run_id,
            l.len(),
            vec![
                ("agent", text(l.iter().map(|r| r.agent.clone()))),
                ("model", text(l.iter().map(|r| r.model.clone()))),
                ("number", int(l.iter().map(|r| r.number))),
                ("time", int(l.iter().map(|r| r.time))),
                ("look", int(l.iter().map(|r| r.look))),
                ("day", float(l.iter().map(|r| r.day))),
                ("block", int(l.iter().map(|r| r.block))),
                ("action", text(l.iter().map(|r| r.action.clone()))),
                ("lower_pct", float_or_empty(l.iter().map(|r| r.lower_pct))),
                ("upper_pct", float_or_empty(l.iter().map(|r| r.upper_pct))),
                ("next", text(l.iter().map(|r| r.next.clone()))),
                ("note", text(l.iter().map(|r| r.note.clone()))),
                ("reasoning", text(l.iter().map(|r| r.reasoning.clone()))),
                (
                    "expect_low_pct",
                    float_or_empty(l.iter().map(|r| r.expect_low_pct)),
                ),
                (
                    "expect_high_pct",
                    float_or_empty(l.iter().map(|r| r.expect_high_pct)),
                ),
                (
                    "expect_confidence",
                    float_or_empty(l.iter().map(|r| r.expect_confidence)),
                ),
                ("expect_text", text(l.iter().map(|r| r.expect_text.clone()))),
                ("stance", text_or_empty(l.iter().map(|r| r.stance.clone()))),
                ("toward", text_or_empty(l.iter().map(|r| r.toward.clone()))),
                ("shown", int(l.iter().map(|r| r.shown))),
                ("result_pct", float_or_empty(l.iter().map(|r| r.result_pct))),
                (
                    "position",
                    text_or_empty(l.iter().map(|r| r.position.clone())),
                ),
                (
                    "change_1h_pct",
                    float_or_empty(l.iter().map(|r| r.change_1h_pct)),
                ),
                (
                    "change_1d_pct",
                    float_or_empty(l.iter().map(|r| r.change_1d_pct)),
                ),
                (
                    "change_7d_pct",
                    float_or_empty(l.iter().map(|r| r.change_7d_pct)),
                ),
                ("vol_1d_pct", float_or_empty(l.iter().map(|r| r.vol_1d_pct))),
                ("gas_gwei", float_or_empty(l.iter().map(|r| r.gas_gwei))),
            ],
        ),
        keyed(
            "agent_scores",
            run_id,
            s.len(),
            vec![
                ("agent", text(s.iter().map(|r| r.agent.clone()))),
                ("model", text(s.iter().map(|r| r.model.clone()))),
                ("number", int(s.iter().map(|r| r.number))),
                ("look", int(s.iter().map(|r| r.look))),
                ("time", int(s.iter().map(|r| r.time))),
                ("end_time", int(s.iter().map(|r| r.end_time))),
                ("day", float(s.iter().map(|r| r.day))),
                ("horizon", text(s.iter().map(|r| r.horizon.clone()))),
                ("horizon_seconds", int(s.iter().map(|r| r.horizon_seconds))),
                ("ended", text(s.iter().map(|r| r.ended.clone()))),
                ("action", text(s.iter().map(|r| r.action.clone()))),
                ("lower_pct", float_or_empty(s.iter().map(|r| r.lower_pct))),
                ("upper_pct", float_or_empty(s.iter().map(|r| r.upper_pct))),
                (
                    "position_open",
                    Column::Flag(s.iter().map(|r| Some(r.position_open)).collect()),
                ),
                ("decision_pct", float(s.iter().map(|r| r.decision_pct))),
                ("cash_pct", float_or_empty(s.iter().map(|r| r.cash_pct))),
                ("hold_pct", float_or_empty(s.iter().map(|r| r.hold_pct))),
                ("best", text(s.iter().map(|r| r.best.clone()))),
                ("regret_pct", float(s.iter().map(|r| r.regret_pct))),
                (
                    "edge_vs_cash_pct",
                    float_or_empty(s.iter().map(|r| r.edge_vs_cash_pct)),
                ),
                (
                    "edge_vs_hold_pct",
                    float_or_empty(s.iter().map(|r| r.edge_vs_hold_pct)),
                ),
                (
                    "price_change_pct",
                    float_or_empty(s.iter().map(|r| r.price_change_pct)),
                ),
                (
                    "price_high_pct",
                    float_or_empty(s.iter().map(|r| r.price_high_pct)),
                ),
                (
                    "price_low_pct",
                    float_or_empty(s.iter().map(|r| r.price_low_pct)),
                ),
                (
                    "band_held",
                    Column::Flag(s.iter().map(|r| r.band_held).collect()),
                ),
                ("brier", float_or_empty(s.iter().map(|r| r.brier))),
            ],
        ),
    ]
}

/// Writes a run's two agent tables and returns their paths.
pub fn write_agent(
    results: &Path,
    run_id: &str,
    looks: &[AgentLookRow],
    scores: &[AgentScoreRow],
) -> Result<Vec<PathBuf>, TableError> {
    agent_tables(run_id, looks, scores)
        .iter()
        .map(|table| write_table(results, run_id, table))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tables::{read_table, table_path};
    use crate::AGENT_TABLES;

    #[test]
    fn an_agent_runs_tables_are_written_keyed_by_run_and_read_back() {
        let looks = vec![AgentLookRow {
            agent: "b".into(),
            model: "claude-opus-5-5".into(),
            number: 1,
            time: 1_000,
            look: 990,
            action: "enter".into(),
            lower_pct: Some(-3.0),
            upper_pct: Some(-1.0),
            reasoning: "after a fall".into(),
            stance: Some("adapt".into()),
            toward: Some("a".into()),
            shown: 2,
            ..AgentLookRow::default()
        }];
        let scores = vec![
            AgentScoreRow {
                agent: "b".into(),
                horizon: "next".into(),
                decision_pct: -0.05,
                cash_pct: Some(0.0),
                band_held: Some(true),
                ..AgentScoreRow::default()
            },
            AgentScoreRow {
                agent: "b".into(),
                horizon: "1d".into(),
                band_held: None,
                ..AgentScoreRow::default()
            },
        ];
        let tables = agent_tables("panel-001", &looks, &scores);
        assert!(tables.iter().map(|t| t.name.as_str()).eq(AGENT_TABLES));
        assert_eq!((tables[0].rows(), tables[1].rows()), (1, 2));
        assert_eq!(tables[0].columns[0].0, "run_id");

        let dir = std::env::temp_dir().join(format!("lpbot-agent-tables-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let paths = write_agent(&dir, "panel-001", &looks, &scores).unwrap();
        assert_eq!(paths[1], table_path(&dir, "agent_scores", "panel-001"));
        let back = read_table(&paths[0]).unwrap();
        assert_eq!(back, tables[0]);
        let back = read_table(&paths[1]).unwrap();
        assert_eq!(
            back.column("band_held"),
            Some(&Column::Flag(vec![Some(true), None]))
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
