//! Writing the supervisor tables into Postgres.
//!
//! `agent_status` is replaced whole in one transaction, under an advisory
//! lock, so two writers (cron and a hand-run `--load`) never mix their
//! rows and a dashboard never sees half a pass.

use std::io::Write;

use postgres::Client;

use crate::loader::{copy_text, identifier, LoadError};
use crate::supervisor_tables::{supervisor_schema_sql, AGENT_STATUS};
use crate::tables::Table;

/// The advisory lock every writer of `agent_status` takes: any constant
/// that no other lock of the database uses.
pub const STATUS_LOCK: i64 = 0x6c70_626f_7473_7401;

fn database(what: &str) -> impl FnOnce(postgres::Error) -> LoadError {
    let what = what.to_string();
    move |source| LoadError::Database { what, source }
}

/// Replaces every row of `agent_status` with `table`'s. Returns the rows
/// written.
pub fn replace_status(client: &mut Client, table: &Table) -> Result<usize, LoadError> {
    let name = identifier(&table.name)?;
    if name != AGENT_STATUS {
        return Err(LoadError::BadName(name.to_string()));
    }
    let mut transaction = client.transaction().map_err(database("begin"))?;
    transaction
        .execute("SELECT pg_advisory_xact_lock($1)", &[&STATUS_LOCK])
        .map_err(database("taking the agent_status lock"))?;
    for statement in supervisor_schema_sql(table)? {
        transaction
            .batch_execute(&statement)
            .map_err(database(&statement))?;
    }
    let delete = format!("DELETE FROM {name}");
    transaction
        .batch_execute(&delete)
        .map_err(database(&delete))?;
    let columns = table
        .columns
        .iter()
        .map(|(column, _)| identifier(column))
        .collect::<Result<Vec<_>, _>>()?
        .join(", ");
    let copy = format!("COPY {name} ({columns}) FROM STDIN");
    let mut writer = transaction.copy_in(&copy).map_err(database(&copy))?;
    writer
        .write_all(copy_text(table).as_bytes())
        .map_err(|source| LoadError::Io {
            path: name.into(),
            source,
        })?;
    writer.finish().map_err(database(&copy))?;
    transaction.commit().map_err(database("commit"))?;
    Ok(table.rows())
}
