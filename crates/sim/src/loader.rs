//! Loads the result tables into Postgres, where the viewer reads them.
//!
//! Tables are created from the files themselves, so a column added to a
//! result table needs no change here. Loading a run again replaces its
//! rows, so the command is safe to repeat.

use std::fs;
use std::io::Write;
use std::path::Path;

use postgres::Client;

use crate::engine::Output;
use crate::tables::{
    pool_tables, read_table, result_tables, Column, PoolInfo, RunInfo, Table, TableError,
};
use crate::{LIVE_TABLES, POOL_TABLES, RESULT_TABLES};

#[derive(Debug, thiserror::Error)]
pub enum LoadError {
    #[error(transparent)]
    Table(#[from] TableError),
    #[error("the database refused `{what}`: {source}")]
    Database {
        what: String,
        source: postgres::Error,
    },
    #[error("{path}: {source}")]
    Io {
        path: std::path::PathBuf,
        source: std::io::Error,
    },
    #[error("`{0}` cannot be used as a table or column name")]
    BadName(String),
}

/// Names come from our own files, but they still go into SQL text: only
/// lower-case letters, digits and underscores pass.
pub(crate) fn identifier(name: &str) -> Result<&str, LoadError> {
    let ok = !name.is_empty()
        && name.len() <= 63
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
        && !name.starts_with(|c: char| c.is_ascii_digit());
    if ok {
        Ok(name)
    } else {
        Err(LoadError::BadName(name.to_string()))
    }
}

fn sql_type(column: &Column) -> &'static str {
    match column {
        Column::Text(_) => "text",
        Column::Float(_) => "double precision",
        Column::Int(_) => "bigint",
        Column::Flag(_) => "boolean",
    }
}

/// The column a table's rows are replaced by: the run for a result table,
/// the pool for a pool table or a live table.
pub fn key_column(table: &str) -> &'static str {
    if POOL_TABLES.contains(&table) || LIVE_TABLES.contains(&table) {
        "pool"
    } else {
        "run_id"
    }
}

/// Columns that were whole numbers before and are fractions now: their
/// type is changed in place in an older database.
const WIDENED: &[(&str, &str)] = &[("zone_map", "lookback_days")];

/// Statements that make sure the table exists with every column of `table`.
pub fn schema_sql(table: &Table) -> Result<Vec<String>, LoadError> {
    let name = identifier(&table.name)?;
    let key = key_column(name);
    let mut statements = vec![format!(
        "CREATE TABLE IF NOT EXISTS {name} ({key} text NOT NULL)"
    )];
    for (column, values) in &table.columns {
        let column = identifier(column)?;
        statements.push(format!(
            "ALTER TABLE {name} ADD COLUMN IF NOT EXISTS {column} {}",
            sql_type(values)
        ));
        // A column that held whole numbers in older databases and holds
        // fractions now.
        if WIDENED.contains(&(name, column)) {
            statements.push(format!(
                "ALTER TABLE {name} ALTER COLUMN {column} TYPE {}",
                sql_type(values)
            ));
        }
    }
    statements.push(format!(
        "CREATE INDEX IF NOT EXISTS {name}_{key} ON {name} ({key})"
    ));
    Ok(statements)
}

/// The six result tables, the two pool tables and the five live tables
/// as SQL, for a database that has no runs yet.
///
/// The viewer's dashboards query these tables. Creating them empty lets a
/// fresh database show "no data" instead of an error. The same text is
/// checked in as `deploy/postgres/schema.sql`, which the start-up script
/// applies; a test keeps the two in step.
pub fn full_schema_sql() -> String {
    let mut tables = result_tables(&RunInfo::default(), &Output::default(), &[]);
    tables.extend(pool_tables(&PoolInfo::default(), &[], &[]));
    let mut sql = String::from(
        "-- The six result tables, the two pool tables and the five live tables, empty.\n\
         -- Generated from the table definitions in crates/sim/src/tables.rs and\n\
         -- crates/sim/src/live_tables.rs; do not edit by hand. To regenerate:\n\
         --   LPBOT_WRITE_SCHEMA=1 cargo test -p lpbot-sim schema_file\n",
    );
    for table in &tables {
        sql.push('\n');
        for statement in schema_sql(table).expect("result tables have plain names") {
            sql.push_str(&statement);
            sql.push_str(";\n");
        }
    }
    sql.push_str(&crate::live_db::live_schema());
    sql
}

fn escape(text: &str, out: &mut String) {
    for c in text.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\t' => out.push_str("\\t"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            other => out.push(other),
        }
    }
}

/// The table's rows in Postgres' text COPY format.
pub fn copy_text(table: &Table) -> String {
    let mut out = String::new();
    for row in 0..table.rows() {
        for (i, (_, column)) in table.columns.iter().enumerate() {
            if i > 0 {
                out.push('\t');
            }
            match column {
                Column::Text(v) => match &v[row] {
                    Some(text) => escape(text, &mut out),
                    None => out.push_str("\\N"),
                },
                Column::Float(v) => match v[row] {
                    Some(x) if x.is_nan() => out.push_str("NaN"),
                    Some(x) if x.is_infinite() => {
                        out.push_str(if x > 0.0 { "Infinity" } else { "-Infinity" })
                    }
                    Some(x) => out.push_str(&x.to_string()),
                    None => out.push_str("\\N"),
                },
                Column::Int(v) => match v[row] {
                    Some(x) => out.push_str(&x.to_string()),
                    None => out.push_str("\\N"),
                },
                Column::Flag(v) => match v[row] {
                    Some(x) => out.push(if x { 't' } else { 'f' }),
                    None => out.push_str("\\N"),
                },
            }
        }
        out.push('\n');
    }
    out
}

/// Replaces the rows of one run, or of one pool for a pool table. Returns
/// the rows written.
pub fn load_table(client: &mut Client, key: &str, table: &Table) -> Result<usize, LoadError> {
    let name = identifier(&table.name)?;
    let key_name = key_column(name);
    let database = |what: &str| {
        let what = what.to_string();
        move |source| LoadError::Database { what, source }
    };
    let mut transaction = client.transaction().map_err(database("begin"))?;
    for statement in schema_sql(table)? {
        transaction
            .batch_execute(&statement)
            .map_err(database(&statement))?;
    }
    let delete = format!("DELETE FROM {name} WHERE {key_name} = $1");
    transaction
        .execute(&delete, &[&key])
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

/// What a load did.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct LoadReport {
    pub runs: usize,
    /// Pools whose tables were loaded.
    pub pools: usize,
    pub rows: usize,
}

/// Loads every run, and every pool's tables, found under `results` into
/// the database.
pub fn load_results(client: &mut Client, results: &Path) -> Result<LoadReport, LoadError> {
    let mut report = LoadReport::default();
    for table in RESULT_TABLES.into_iter().chain(POOL_TABLES) {
        let dir = results.join(table);
        if !dir.is_dir() {
            continue;
        }
        let io = |source| LoadError::Io {
            path: dir.clone(),
            source,
        };
        let mut files: Vec<_> = fs::read_dir(&dir)
            .map_err(io)?
            .filter_map(|entry| entry.ok().map(|e| e.path()))
            .filter(|path| path.extension().is_some_and(|e| e == "parquet"))
            .collect();
        files.sort();
        for path in files {
            let Some(key) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            report.rows += load_table(client, key, &read_table(&path)?)?;
            match table {
                "runs" => report.runs += 1,
                "pool_series" => report.pools += 1,
                _ => {}
            }
        }
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Table {
        Table {
            name: "campaigns".into(),
            columns: vec![
                (
                    "run_id".into(),
                    Column::Text(vec![Some("r1".into()), Some("r1".into())]),
                ),
                (
                    "exit_reason".into(),
                    Column::Text(vec![Some("tab\there\\and\nline".into()), None]),
                ),
                ("result".into(), Column::Float(vec![Some(-0.5), None])),
                ("passes".into(), Column::Int(vec![Some(2), None])),
                ("halted".into(), Column::Flag(vec![Some(true), Some(false)])),
            ],
        }
    }

    #[test]
    fn the_schema_is_built_from_the_table_itself() {
        assert_eq!(
            schema_sql(&sample()).unwrap(),
            vec![
                "CREATE TABLE IF NOT EXISTS campaigns (run_id text NOT NULL)",
                "ALTER TABLE campaigns ADD COLUMN IF NOT EXISTS run_id text",
                "ALTER TABLE campaigns ADD COLUMN IF NOT EXISTS exit_reason text",
                "ALTER TABLE campaigns ADD COLUMN IF NOT EXISTS result double precision",
                "ALTER TABLE campaigns ADD COLUMN IF NOT EXISTS passes bigint",
                "ALTER TABLE campaigns ADD COLUMN IF NOT EXISTS halted boolean",
                "CREATE INDEX IF NOT EXISTS campaigns_run_id ON campaigns (run_id)",
            ]
        );
    }

    #[test]
    fn rows_are_written_in_copy_format_with_escapes_and_empty_cells() {
        assert_eq!(
            copy_text(&sample()),
            "r1\ttab\\there\\\\and\\nline\t-0.5\t2\tt\nr1\t\\N\t\\N\t\\N\tf\n"
        );
        let odd = Table {
            name: "t".into(),
            columns: vec![(
                "x".into(),
                Column::Float(vec![Some(f64::NAN), Some(f64::INFINITY), Some(1e-7)]),
            )],
        };
        assert_eq!(copy_text(&odd), "NaN\nInfinity\n0.0000001\n");
    }

    #[test]
    fn the_schema_file_matches_the_table_definitions() {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../deploy/postgres/schema.sql"
        );
        let current = full_schema_sql();
        if std::env::var_os("LPBOT_WRITE_SCHEMA").is_some() {
            std::fs::create_dir_all(std::path::Path::new(path).parent().unwrap()).unwrap();
            std::fs::write(path, &current).unwrap();
        }
        let on_disk = std::fs::read_to_string(path).unwrap_or_default();
        assert!(
            on_disk == current,
            "deploy/postgres/schema.sql is out of date. Regenerate it with:\n  \
             LPBOT_WRITE_SCHEMA=1 cargo test -p lpbot-sim schema_file"
        );
        // All six tables, each with its run_id column and index.
        for table in crate::RESULT_TABLES {
            assert!(current.contains(&format!("CREATE TABLE IF NOT EXISTS {table} ")));
            assert!(current.contains(&format!("CREATE INDEX IF NOT EXISTS {table}_run_id")));
        }
        // And the pool tables, keyed by pool.
        for table in crate::POOL_TABLES {
            assert!(current.contains(&format!(
                "CREATE TABLE IF NOT EXISTS {table} (pool text NOT NULL)"
            )));
            assert!(current.contains(&format!("CREATE INDEX IF NOT EXISTS {table}_pool")));
        }
        assert!(current
            .contains("ALTER TABLE series ADD COLUMN IF NOT EXISTS price_lower double precision;"));
        // And the live tables, keyed by pool and more.
        for table in crate::LIVE_TABLES {
            assert!(current.contains(&format!(
                "CREATE TABLE IF NOT EXISTS {table} (pool text NOT NULL)"
            )));
            assert!(current.contains(&format!("CREATE UNIQUE INDEX IF NOT EXISTS {table}_key")));
        }
    }

    #[test]
    fn names_that_are_not_plain_identifiers_are_refused() {
        for bad in [
            "",
            "runs; drop table runs",
            "Runs",
            "1runs",
            "a-b",
            "a b",
            "a\"b",
        ] {
            assert!(identifier(bad).is_err(), "{bad}");
        }
        assert!(identifier("zone_map").is_ok());
        let mut table = sample();
        table.columns[1].0 = "exit reason".into();
        assert!(matches!(schema_sql(&table), Err(LoadError::BadName(_))));
        table.name = "x;y".into();
        assert!(schema_sql(&table).is_err());
    }
}
