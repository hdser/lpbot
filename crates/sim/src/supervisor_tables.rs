//! The supervisor tables: what every forward agent is doing now
//! (`agent_status`, one row an agent run, written by
//! `lpbot supervisor status --load`), and later every job the supervisor
//! ran (`agent_jobs`).
//!
//! `agent_status` holds the present only: each pass replaces every row, so
//! an agent that is gone disappears with it. Its key is the run, like the
//! agent tables, with a unique index so a row cannot repeat.

use crate::loader::{identifier, schema_sql, LoadError};
use crate::tables::{Column, Table};

/// The table of every forward agent's state now.
pub const AGENT_STATUS: &str = "agent_status";

/// One agent run as the status saw it. Times are Unix seconds.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct AgentStatusRow {
    pub run_id: String,
    /// The experiment's name, as `runs.experiment` holds it.
    pub experiment: String,
    pub agent: String,
    pub model: String,
    /// What runs the agent: `session`, `runner`, `supervisor`, or what the
    /// agent list names.
    pub runner: String,
    /// Where it runs, in the agent list's words.
    pub place: Option<String>,
    /// `waiting`, `unverified`, `deciding`, `due`, `late`, `no waiter`,
    /// `stopped`, `failing`, `scheduler down`, `paused` or `ended`.
    pub state: String,
    /// One plain sentence about the state.
    pub detail: String,
    /// Whether the agent list names the agent.
    pub listed: bool,
    /// Looks decided.
    pub looks: i64,
    pub last_number: Option<i64>,
    /// When the last decided look was seen (`at`, or its due time).
    pub last_time: Option<i64>,
    pub last_action: Option<String>,
    /// The entered range, as `-2.00% to -0.50%`.
    pub last_range: Option<String>,
    /// The canonical due time of the next look.
    pub due: Option<i64>,
    /// When the look waiting for a decision was first seen shown.
    pub first_shown: Option<i64>,
    /// `next`, `inbox` or `answers`: the wait the agent's process is in.
    pub waiter_kind: Option<String>,
    pub waiter_pid: Option<i64>,
    /// When that wait ends.
    pub waiter_until: Option<i64>,
    /// On a panel with the board on: whether it waits for questions.
    pub listening: Option<bool>,
    /// What holds the agent's ownership fence, never its token.
    pub owner: Option<String>,
    pub owner_since: Option<i64>,
    /// The scheduler and how alive it looks, kept apart from the agent.
    pub scheduler: String,
    /// When the scheduler last showed it was working.
    pub scheduler_seen: Option<i64>,
    pub runner_phase: Option<String>,
    pub stopped: Option<bool>,
    pub last_error: Option<String>,
    /// Agent runs the status found in this pass, on every row, so a
    /// missing row can be told.
    pub expected: i64,
    /// When the status was checked.
    pub checked: i64,
}

fn texts(rows: &[AgentStatusRow], f: impl Fn(&AgentStatusRow) -> Option<String>) -> Column {
    Column::Text(rows.iter().map(f).collect())
}

fn ints(rows: &[AgentStatusRow], f: impl Fn(&AgentStatusRow) -> Option<i64>) -> Column {
    Column::Int(rows.iter().map(f).collect())
}

fn flags(rows: &[AgentStatusRow], f: impl Fn(&AgentStatusRow) -> Option<bool>) -> Column {
    Column::Flag(rows.iter().map(f).collect())
}

/// The status rows as the `agent_status` table.
pub fn agent_status_table(rows: &[AgentStatusRow]) -> Table {
    let columns = vec![
        ("run_id", texts(rows, |r| Some(r.run_id.clone()))),
        ("experiment", texts(rows, |r| Some(r.experiment.clone()))),
        ("agent", texts(rows, |r| Some(r.agent.clone()))),
        ("model", texts(rows, |r| Some(r.model.clone()))),
        ("runner", texts(rows, |r| Some(r.runner.clone()))),
        ("place", texts(rows, |r| r.place.clone())),
        ("state", texts(rows, |r| Some(r.state.clone()))),
        ("detail", texts(rows, |r| Some(r.detail.clone()))),
        ("listed", flags(rows, |r| Some(r.listed))),
        ("looks", ints(rows, |r| Some(r.looks))),
        ("last_number", ints(rows, |r| r.last_number)),
        ("last_time", ints(rows, |r| r.last_time)),
        ("last_action", texts(rows, |r| r.last_action.clone())),
        ("last_range", texts(rows, |r| r.last_range.clone())),
        ("due", ints(rows, |r| r.due)),
        ("first_shown", ints(rows, |r| r.first_shown)),
        ("waiter_kind", texts(rows, |r| r.waiter_kind.clone())),
        ("waiter_pid", ints(rows, |r| r.waiter_pid)),
        ("waiter_until", ints(rows, |r| r.waiter_until)),
        ("listening", flags(rows, |r| r.listening)),
        ("owner", texts(rows, |r| r.owner.clone())),
        ("owner_since", ints(rows, |r| r.owner_since)),
        ("scheduler", texts(rows, |r| Some(r.scheduler.clone()))),
        ("scheduler_seen", ints(rows, |r| r.scheduler_seen)),
        ("runner_phase", texts(rows, |r| r.runner_phase.clone())),
        ("stopped", flags(rows, |r| r.stopped)),
        ("last_error", texts(rows, |r| r.last_error.clone())),
        ("expected", ints(rows, |r| Some(r.expected))),
        ("checked", ints(rows, |r| Some(r.checked))),
    ];
    Table {
        name: AGENT_STATUS.to_string(),
        columns: columns
            .into_iter()
            .map(|(name, column)| (name.to_string(), column))
            .collect(),
    }
}

/// The supervisor tables, empty, for the schema file.
pub fn empty_supervisor_tables() -> Vec<Table> {
    vec![agent_status_table(&[])]
}

/// Statements that make sure a supervisor table exists with every column
/// and its unique key.
pub fn supervisor_schema_sql(table: &Table) -> Result<Vec<String>, LoadError> {
    let name = identifier(&table.name)?;
    let mut statements = schema_sql(table)?;
    statements.push(format!(
        "CREATE UNIQUE INDEX IF NOT EXISTS {name}_key ON {name} (run_id)"
    ));
    Ok(statements)
}

/// The supervisor tables as SQL, for the schema file.
pub fn supervisor_schema() -> String {
    let mut sql = String::new();
    for table in empty_supervisor_tables() {
        sql.push('\n');
        for statement in supervisor_schema_sql(&table).expect("supervisor tables have plain names")
        {
            sql.push_str(&statement);
            sql.push_str(";\n");
        }
    }
    sql
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_field_becomes_a_column_of_one_row_each() {
        let row = AgentStatusRow {
            run_id: "agent-panel-forward-000-ee379e87".into(),
            experiment: "agent-panel-forward".into(),
            agent: "a".into(),
            model: "claude-opus-5-5".into(),
            runner: "session".into(),
            place: Some("tmux agent:a on the node".into()),
            state: "waiting".into(),
            detail: "waiting for look 12".into(),
            listed: true,
            looks: 11,
            last_number: Some(11),
            last_time: Some(1_795_604_000),
            last_action: Some("hold".into()),
            due: Some(1_795_618_400),
            waiter_kind: Some("next".into()),
            waiter_pid: Some(2_177_634),
            waiter_until: Some(1_795_619_000),
            listening: Some(true),
            scheduler: "session".into(),
            expected: 4,
            checked: 1_795_605_000,
            ..AgentStatusRow::default()
        };
        let table = agent_status_table(std::slice::from_ref(&row));
        assert_eq!(table.name, "agent_status");
        assert_eq!(table.rows(), 1);
        assert_eq!(table.columns.len(), 29);
        assert_eq!(
            table.column("state"),
            Some(&Column::Text(vec![Some("waiting".into())]))
        );
        assert_eq!(table.column("first_shown"), Some(&Column::Int(vec![None])));
        assert_eq!(
            table.column("listed"),
            Some(&Column::Flag(vec![Some(true)]))
        );
        assert_eq!(table.columns[0].0, "run_id");
    }

    #[test]
    fn the_schema_keys_the_table_by_run() {
        let sql = supervisor_schema();
        assert!(sql.contains("CREATE TABLE IF NOT EXISTS agent_status (run_id text NOT NULL);"));
        assert!(
            sql.contains("ALTER TABLE agent_status ADD COLUMN IF NOT EXISTS listening boolean;")
        );
        assert!(sql.contains(
            "CREATE UNIQUE INDEX IF NOT EXISTS agent_status_key ON agent_status (run_id);"
        ));
    }
}
