//! Writing the live tables into Postgres.
//!
//! Every write of the service is one transaction, so a dashboard never
//! sees half of one. Rows are upserted by each table's key: copied into a
//! staging table, then inserted, replacing the rows with the same key.

use std::io::Write;

use postgres::{Client, Transaction};

use crate::live_tables::{empty_live_tables, live_key};
use crate::loader::{copy_text, identifier, schema_sql, LoadError};
use crate::tables::Table;

fn database(what: &str) -> impl FnOnce(postgres::Error) -> LoadError {
    let what = what.to_string();
    move |source| LoadError::Database { what, source }
}

/// Statements that make sure a live table exists with all its columns,
/// its key and its indexes.
pub fn live_schema_sql(table: &Table) -> Result<Vec<String>, LoadError> {
    let name = identifier(&table.name)?;
    let mut statements = schema_sql(table)?;
    let key = live_key(name);
    if !key.is_empty() {
        statements.push(format!(
            "CREATE UNIQUE INDEX IF NOT EXISTS {name}_key ON {name} ({})",
            key.join(", ")
        ));
    }
    if table.column("time").is_some() {
        statements.push(format!(
            "CREATE INDEX IF NOT EXISTS {name}_time ON {name} (time)"
        ));
    }
    if name == "live_positions" {
        // Rows of open positions are rewritten often; room on each page
        // lets Postgres update them in place.
        statements.push(format!("ALTER TABLE {name} SET (fillfactor = 70)"));
    }
    Ok(statements)
}

/// The live tables as SQL, for the schema file.
pub fn live_schema() -> String {
    let mut sql = String::new();
    for table in empty_live_tables() {
        sql.push('\n');
        for statement in live_schema_sql(&table).expect("live tables have plain names") {
            sql.push_str(&statement);
            sql.push_str(";\n");
        }
    }
    sql
}

/// Creates the live tables where they are missing.
pub fn ensure_live_schema(client: &mut Client) -> Result<(), LoadError> {
    let mut transaction = client.transaction().map_err(database("begin"))?;
    for table in empty_live_tables() {
        for statement in live_schema_sql(&table)? {
            transaction
                .batch_execute(&statement)
                .map_err(database(&statement))?;
        }
    }
    transaction.commit().map_err(database("commit"))
}

/// The statement that moves staged rows into the table, replacing rows
/// with the same key.
pub fn upsert_sql(table: &Table, stage: &str) -> Result<String, LoadError> {
    let name = identifier(&table.name)?;
    let key = live_key(name);
    let columns = table
        .columns
        .iter()
        .map(|(c, _)| identifier(c))
        .collect::<Result<Vec<_>, _>>()?;
    let list = columns.join(", ");
    let updates: Vec<String> = columns
        .iter()
        .filter(|c| !key.contains(c))
        .map(|c| format!("{c} = EXCLUDED.{c}"))
        .collect();
    let action = if updates.is_empty() {
        "DO NOTHING".to_string()
    } else {
        format!("DO UPDATE SET {}", updates.join(", "))
    };
    Ok(format!(
        "INSERT INTO {name} ({list}) SELECT {list} FROM {stage} ON CONFLICT ({}) {action}",
        key.join(", ")
    ))
}

/// Writes rows, replacing those with the same key. The rows must not
/// repeat a key.
pub fn upsert(transaction: &mut Transaction, table: &Table) -> Result<usize, LoadError> {
    if table.rows() == 0 {
        return Ok(0);
    }
    let name = identifier(&table.name)?;
    let stage = format!("stage_{name}");
    let create = format!("CREATE TEMP TABLE {stage} (LIKE {name}) ON COMMIT DROP");
    transaction
        .batch_execute(&format!("DROP TABLE IF EXISTS {stage}; {create}"))
        .map_err(database(&create))?;
    let columns = table
        .columns
        .iter()
        .map(|(c, _)| identifier(c))
        .collect::<Result<Vec<_>, _>>()?
        .join(", ");
    let copy = format!("COPY {stage} ({columns}) FROM STDIN");
    let mut writer = transaction.copy_in(&copy).map_err(database(&copy))?;
    writer
        .write_all(copy_text(table).as_bytes())
        .map_err(|source| LoadError::Io {
            path: name.into(),
            source,
        })?;
    writer.finish().map_err(database(&copy))?;
    let insert = upsert_sql(table, &stage)?;
    transaction
        .batch_execute(&insert)
        .map_err(database(&insert))?;
    Ok(table.rows())
}

/// Removes a table's rows of some pools.
pub fn delete_pools(
    transaction: &mut Transaction,
    table: &str,
    pools: &[String],
) -> Result<u64, LoadError> {
    let name = identifier(table)?;
    let sql = format!("DELETE FROM {name} WHERE pool = ANY($1)");
    transaction.execute(&sql, &[&pools]).map_err(database(&sql))
}

/// Removes a table's rows where `column` is above `value`: what blocks
/// taken back by a reorg wrote.
pub fn delete_above(
    transaction: &mut Transaction,
    table: &str,
    column: &str,
    value: i64,
) -> Result<u64, LoadError> {
    let (name, column) = (identifier(table)?, identifier(column)?);
    let sql = format!("DELETE FROM {name} WHERE {column} > $1");
    transaction.execute(&sql, &[&value]).map_err(database(&sql))
}

/// Removes a table's rows where `column` is below `value`: rows older
/// than the table keeps.
pub fn delete_below(
    transaction: &mut Transaction,
    table: &str,
    column: &str,
    value: i64,
) -> Result<u64, LoadError> {
    let (name, column) = (identifier(table)?, identifier(column)?);
    let sql = format!("DELETE FROM {name} WHERE {column} < $1");
    transaction.execute(&sql, &[&value]).map_err(database(&sql))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::live_tables::{live_swaps_table, LIVE_TABLES};

    #[test]
    fn a_live_table_gets_its_key_and_time_index() {
        let tables = empty_live_tables();
        let positions = tables.iter().find(|t| t.name == "live_positions").unwrap();
        let statements = live_schema_sql(positions).unwrap();
        assert_eq!(
            statements[0],
            "CREATE TABLE IF NOT EXISTS live_positions (pool text NOT NULL)"
        );
        assert!(statements.contains(
            &"CREATE UNIQUE INDEX IF NOT EXISTS live_positions_key ON live_positions (pool, position_id)"
                .to_string()
        ));
        assert!(
            statements.contains(&"ALTER TABLE live_positions SET (fillfactor = 70)".to_string())
        );
        let schema = live_schema();
        for table in LIVE_TABLES {
            assert!(schema.contains(&format!(
                "CREATE TABLE IF NOT EXISTS {table} (pool text NOT NULL)"
            )));
            assert!(schema.contains(&format!("CREATE UNIQUE INDEX IF NOT EXISTS {table}_key")));
        }
        assert!(schema.contains("CREATE INDEX IF NOT EXISTS live_swaps_time ON live_swaps (time)"));
    }

    #[test]
    fn an_upsert_replaces_every_column_but_the_key() {
        let sql = upsert_sql(&live_swaps_table(&[]), "stage_live_swaps").unwrap();
        assert!(sql.starts_with(
            "INSERT INTO live_swaps (pool, pool_name, price_unit, block, time, log_index,"
        ));
        assert!(sql.contains("SELECT pool, pool_name, price_unit, block, time, log_index,"));
        assert!(sql
            .contains("FROM stage_live_swaps ON CONFLICT (pool, block, log_index) DO UPDATE SET"));
        assert!(sql.contains("pool_name = EXCLUDED.pool_name"));
        assert!(!sql.contains("block = EXCLUDED.block"));
        assert!(!sql.contains("log_index = EXCLUDED.log_index"));
    }
}
