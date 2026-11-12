//! The six result tables every run writes, as Parquet.
//!
//! ```text
//! <results>/<table>/<run id>.parquet
//! ```
//!
//! Every table starts with a `run_id` column, so the files of a table can
//! be read together and joined on it. Times are Unix seconds. Amounts are
//! in whole value tokens.

use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use arrow_array::{
    Array, ArrayRef, BooleanArray, Float64Array, Int64Array, RecordBatch, StringArray,
};
use arrow_schema::{DataType, Field, Schema};
use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
use parquet::arrow::ArrowWriter;
use parquet::basic::Compression;
use parquet::file::properties::WriterProperties;

use crate::engine::Output;
use crate::RESULT_TABLES;

#[derive(Debug, thiserror::Error)]
pub enum TableError {
    #[error("{path}: {source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("{path}: {reason}")]
    Format { path: PathBuf, reason: String },
}

/// One column of a table. Every column may hold empty cells.
#[derive(Debug, Clone, PartialEq)]
pub enum Column {
    Text(Vec<Option<String>>),
    Float(Vec<Option<f64>>),
    Int(Vec<Option<i64>>),
    Flag(Vec<Option<bool>>),
}

impl Column {
    pub fn len(&self) -> usize {
        match self {
            Column::Text(v) => v.len(),
            Column::Float(v) => v.len(),
            Column::Int(v) => v.len(),
            Column::Flag(v) => v.len(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn data_type(&self) -> DataType {
        match self {
            Column::Text(_) => DataType::Utf8,
            Column::Float(_) => DataType::Float64,
            Column::Int(_) => DataType::Int64,
            Column::Flag(_) => DataType::Boolean,
        }
    }

    fn array(&self) -> ArrayRef {
        match self {
            Column::Text(v) => Arc::new(StringArray::from(v.clone())),
            Column::Float(v) => Arc::new(Float64Array::from(v.clone())),
            Column::Int(v) => Arc::new(Int64Array::from(v.clone())),
            Column::Flag(v) => Arc::new(BooleanArray::from(v.clone())),
        }
    }
}

/// A result table: its name and its columns, all of the same length.
#[derive(Debug, Clone, PartialEq)]
pub struct Table {
    pub name: String,
    pub columns: Vec<(String, Column)>,
}

impl Table {
    pub fn rows(&self) -> usize {
        self.columns.first().map_or(0, |(_, c)| c.len())
    }

    pub fn column(&self, name: &str) -> Option<&Column> {
        self.columns.iter().find(|(n, _)| n == name).map(|(_, c)| c)
    }
}

fn text<'a>(values: impl IntoIterator<Item = &'a str>) -> Column {
    Column::Text(values.into_iter().map(|v| Some(v.to_string())).collect())
}

fn float(values: impl IntoIterator<Item = f64>) -> Column {
    Column::Float(values.into_iter().map(Some).collect())
}

fn float_or_empty(values: impl IntoIterator<Item = Option<f64>>) -> Column {
    Column::Float(values.into_iter().collect())
}

fn int(values: impl IntoIterator<Item = i64>) -> Column {
    Column::Int(values.into_iter().map(Some).collect())
}

fn int_or_empty(values: impl IntoIterator<Item = Option<i64>>) -> Column {
    Column::Int(values.into_iter().collect())
}

fn table(name: &str, run_id: &str, rows: usize, columns: Vec<(&str, Column)>) -> Table {
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

/// What identifies a run and where it ran.
#[derive(Debug, Clone, PartialEq)]
pub struct RunInfo {
    pub run_id: String,
    pub experiment: String,
    pub index: usize,
    pub config_hash: String,
    pub code_version: String,
    pub mode: String,
    pub window_from: String,
    pub window_to: String,
    pub capital_usd: f64,
    pub loss_limit_pct: f64,
    pub autonomy: String,
    pub strategy: String,
    pub patience: String,
    pub pool: String,
    pub value_token: String,
    pub fee_pips: u32,
    pub width_ticks: i32,
    /// The swept settings of this run, as `path=value` joined by `; `.
    pub sweep: String,
    pub config_yaml: String,
}

/// A pool's ranking figures when the run started.
#[derive(Debug, Clone, PartialEq)]
pub struct PoolRankRow {
    pub time: u64,
    pub pool: String,
    pub fee: f64,
    pub volume_24h_usd: f64,
    pub volume_per_day_usd: f64,
    pub depth_1pct_usd: f64,
    pub implied_vol_24h: Option<f64>,
    pub implied_vol: Option<f64>,
    pub excluded: Option<String>,
}

/// The six tables of one run.
pub fn result_tables(info: &RunInfo, out: &Output, pool_rank: &[PoolRankRow]) -> Vec<Table> {
    let id = info.run_id.as_str();
    let s = &out.summary;
    let c = &out.campaigns;
    let d = &out.decisions;
    let t = &out.series;
    let z = &out.zones;

    vec![
        table(
            "runs",
            id,
            1,
            vec![
                ("experiment", text([info.experiment.as_str()])),
                ("run_index", int([info.index as i64])),
                ("config_hash", text([info.config_hash.as_str()])),
                ("code_version", text([info.code_version.as_str()])),
                ("mode", text([info.mode.as_str()])),
                ("window_from", text([info.window_from.as_str()])),
                ("window_to", text([info.window_to.as_str()])),
                ("capital_usd", float([info.capital_usd])),
                ("loss_limit_pct", float([info.loss_limit_pct])),
                ("autonomy", text([info.autonomy.as_str()])),
                ("strategy", text([info.strategy.as_str()])),
                ("patience", text([info.patience.as_str()])),
                ("pool", text([info.pool.as_str()])),
                ("value_token", text([info.value_token.as_str()])),
                ("fee_pips", int([i64::from(info.fee_pips)])),
                ("width_ticks", int([i64::from(info.width_ticks)])),
                ("sweep", text([info.sweep.as_str()])),
                ("start_capital", float([s.start_capital])),
                ("end_capital", float([s.end_capital])),
                ("result_pct", float([s.result_pct])),
                ("fees", float([s.fees])),
                ("gas", float([s.gas])),
                ("campaigns", int([i64::from(s.campaigns)])),
                ("passes", int([i64::from(s.passes)])),
                ("max_drawdown_pct", float([s.max_drawdown_pct])),
                (
                    "halted_by_loss_limit",
                    Column::Flag(vec![Some(s.halted_by_loss_limit)]),
                ),
                ("events", int([s.events as i64])),
                ("config_yaml", text([info.config_yaml.as_str()])),
            ],
        ),
        table(
            "campaigns",
            id,
            c.len(),
            vec![
                ("campaign", int(c.iter().map(|r| i64::from(r.id)))),
                ("pool", text(c.iter().map(|_| info.pool.as_str()))),
                (
                    "value_token",
                    text(c.iter().map(|_| info.value_token.as_str())),
                ),
                ("fee_pips", int(c.iter().map(|_| i64::from(info.fee_pips)))),
                ("tick_lower", int(c.iter().map(|r| i64::from(r.tick_lower)))),
                ("tick_upper", int(c.iter().map(|r| i64::from(r.tick_upper)))),
                ("opened_time", int(c.iter().map(|r| r.opened_time as i64))),
                ("opened_block", int(c.iter().map(|r| r.opened_block as i64))),
                ("closed_time", int(c.iter().map(|r| r.closed_time as i64))),
                ("closed_block", int(c.iter().map(|r| r.closed_block as i64))),
                (
                    "exit_reason",
                    text(c.iter().map(|r| r.exit_reason.as_str())),
                ),
                ("amount_in", float(c.iter().map(|r| r.amount_in))),
                ("result", float(c.iter().map(|r| r.result))),
                ("fees", float(c.iter().map(|r| r.fees))),
                ("gas", float(c.iter().map(|r| r.gas))),
                ("passes", int(c.iter().map(|r| i64::from(r.passes)))),
                ("flips", int(c.iter().map(|r| i64::from(r.flips)))),
                (
                    "seconds_held",
                    int(c.iter().map(|r| (r.closed_time - r.opened_time) as i64)),
                ),
                (
                    "seconds_beyond",
                    int(c.iter().map(|r| r.seconds_beyond as i64)),
                ),
            ],
        ),
        table(
            "decisions",
            id,
            d.len(),
            vec![
                ("campaign", int(d.iter().map(|r| i64::from(r.campaign)))),
                ("time", int(d.iter().map(|r| r.time as i64))),
                ("block", int(d.iter().map(|r| r.block as i64))),
                ("tick", int(d.iter().map(|r| i64::from(r.tick)))),
                ("leg", text(d.iter().map(|r| r.leg.as_str()))),
                (
                    "seconds_stranded",
                    int(d.iter().map(|r| r.seconds_stranded as i64)),
                ),
                ("action", text(d.iter().map(|r| r.action.as_str()))),
                (
                    "target_lower",
                    int_or_empty(d.iter().map(|r| r.target_lower.map(i64::from))),
                ),
                (
                    "target_upper",
                    int_or_empty(d.iter().map(|r| r.target_upper.map(i64::from))),
                ),
                // Filled by the adaptive decision; a fixed rule expects nothing.
                ("expected_wait", float_or_empty(d.iter().map(|_| None))),
                ("expected_flip", float_or_empty(d.iter().map(|_| None))),
                ("expected_close", float_or_empty(d.iter().map(|_| None))),
            ],
        ),
        table(
            "series",
            id,
            t.len(),
            vec![
                ("time", int(t.iter().map(|r| r.time as i64))),
                ("block", int(t.iter().map(|r| r.block as i64))),
                ("tick", int(t.iter().map(|r| i64::from(r.tick)))),
                ("price", float(t.iter().map(|r| r.price))),
                ("equity", float(t.iter().map(|r| r.equity))),
                ("hold", float(t.iter().map(|r| r.hold))),
                ("fees", float(t.iter().map(|r| r.fees))),
                ("state", text(t.iter().map(|r| r.state.as_str()))),
                (
                    "tick_lower",
                    int_or_empty(t.iter().map(|r| r.tick_lower.map(i64::from))),
                ),
                (
                    "tick_upper",
                    int_or_empty(t.iter().map(|r| r.tick_upper.map(i64::from))),
                ),
                (
                    "price_lower",
                    float_or_empty(t.iter().map(|r| r.price_lower)),
                ),
                (
                    "price_upper",
                    float_or_empty(t.iter().map(|r| r.price_upper)),
                ),
            ],
        ),
        table(
            "pool_rank",
            id,
            pool_rank.len(),
            vec![
                ("time", int(pool_rank.iter().map(|r| r.time as i64))),
                ("pool", text(pool_rank.iter().map(|r| r.pool.as_str()))),
                ("fee", float(pool_rank.iter().map(|r| r.fee))),
                (
                    "volume_24h_usd",
                    float(pool_rank.iter().map(|r| r.volume_24h_usd)),
                ),
                (
                    "volume_per_day_usd",
                    float(pool_rank.iter().map(|r| r.volume_per_day_usd)),
                ),
                (
                    "depth_1pct_usd",
                    float(pool_rank.iter().map(|r| r.depth_1pct_usd)),
                ),
                (
                    "implied_vol_24h",
                    float_or_empty(pool_rank.iter().map(|r| r.implied_vol_24h)),
                ),
                (
                    "implied_vol",
                    float_or_empty(pool_rank.iter().map(|r| r.implied_vol)),
                ),
                (
                    "excluded",
                    Column::Text(pool_rank.iter().map(|r| r.excluded.clone()).collect()),
                ),
            ],
        ),
        table(
            "zone_map",
            id,
            z.len(),
            vec![
                ("pool", text(z.iter().map(|_| info.pool.as_str()))),
                ("time", int(z.iter().map(|r| r.time as i64))),
                (
                    "lookback_days",
                    int(z.iter().map(|r| i64::from(r.lookback_days))),
                ),
                ("tick_lower", int(z.iter().map(|r| i64::from(r.tick_lower)))),
                ("tick_upper", int(z.iter().map(|r| i64::from(r.tick_upper)))),
                ("observed_days", float(z.iter().map(|r| r.observed_days))),
                ("passes_per_day", float(z.iter().map(|r| r.passes_per_day))),
                ("time_in_range", float(z.iter().map(|r| r.time_in_range))),
                (
                    "return_odds_1d",
                    float_or_empty(z.iter().map(|r| r.return_odds_1d)),
                ),
            ],
        ),
    ]
}

/// Where a run's file of a table lives.
pub fn table_path(results: &Path, table: &str, run_id: &str) -> PathBuf {
    results.join(table).join(format!("{run_id}.parquet"))
}

/// Writes one table. The file appears only once it is whole.
pub fn write_table(results: &Path, run_id: &str, table: &Table) -> Result<PathBuf, TableError> {
    let path = table_path(results, &table.name, run_id);
    let io = |source| TableError::Io {
        path: path.clone(),
        source,
    };
    let format = |reason: String| TableError::Format {
        path: path.clone(),
        reason,
    };
    let rows = table.rows();
    if let Some((name, _)) = table.columns.iter().find(|(_, c)| c.len() != rows) {
        return Err(format(format!("column `{name}` is not {rows} rows long")));
    }
    let schema = Arc::new(Schema::new(
        table
            .columns
            .iter()
            .map(|(name, column)| Field::new(name, column.data_type(), true))
            .collect::<Vec<_>>(),
    ));
    let arrays: Vec<ArrayRef> = table.columns.iter().map(|(_, c)| c.array()).collect();
    let batch = RecordBatch::try_new(schema.clone(), arrays).map_err(|e| format(e.to_string()))?;

    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(io)?;
    }
    let temporary = path.with_extension("tmp");
    let file = File::create(&temporary).map_err(io)?;
    let properties = WriterProperties::builder()
        .set_compression(Compression::SNAPPY)
        .build();
    let mut writer =
        ArrowWriter::try_new(file, schema, Some(properties)).map_err(|e| format(e.to_string()))?;
    writer.write(&batch).map_err(|e| format(e.to_string()))?;
    writer.close().map_err(|e| format(e.to_string()))?;
    fs::rename(&temporary, &path).map_err(io)?;
    Ok(path)
}

/// Writes all six tables of a run and returns their paths.
pub fn write_run(
    results: &Path,
    info: &RunInfo,
    out: &Output,
    pool_rank: &[PoolRankRow],
) -> Result<Vec<PathBuf>, TableError> {
    let tables = result_tables(info, out, pool_rank);
    debug_assert!(tables.iter().map(|t| t.name.as_str()).eq(RESULT_TABLES));
    tables
        .iter()
        .map(|table| write_table(results, &info.run_id, table))
        .collect()
}

/// Reads a table file back.
pub fn read_table(path: &Path) -> Result<Table, TableError> {
    let format = |reason: String| TableError::Format {
        path: path.to_path_buf(),
        reason,
    };
    let file = File::open(path).map_err(|source| TableError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    let builder =
        ParquetRecordBatchReaderBuilder::try_new(file).map_err(|e| format(e.to_string()))?;
    let schema = builder.schema().clone();
    let mut columns: Vec<(String, Column)> = schema
        .fields()
        .iter()
        .map(|field| {
            let empty = match field.data_type() {
                DataType::Utf8 => Column::Text(Vec::new()),
                DataType::Float64 => Column::Float(Vec::new()),
                DataType::Int64 => Column::Int(Vec::new()),
                DataType::Boolean => Column::Flag(Vec::new()),
                other => return Err(format(format!("unexpected column type {other}"))),
            };
            Ok((field.name().clone(), empty))
        })
        .collect::<Result<_, _>>()?;

    for batch in builder.build().map_err(|e| format(e.to_string()))? {
        let batch = batch.map_err(|e| format(e.to_string()))?;
        for (i, (_, column)) in columns.iter_mut().enumerate() {
            let array = batch.column(i).as_any();
            let wrong = || format("a column changed type between batches".to_string());
            match column {
                Column::Text(values) => {
                    let a = array.downcast_ref::<StringArray>().ok_or_else(wrong)?;
                    values.extend(a.iter().map(|v| v.map(str::to_string)));
                }
                Column::Float(values) => {
                    values.extend(
                        array
                            .downcast_ref::<Float64Array>()
                            .ok_or_else(wrong)?
                            .iter(),
                    );
                }
                Column::Int(values) => {
                    values.extend(array.downcast_ref::<Int64Array>().ok_or_else(wrong)?.iter());
                }
                Column::Flag(values) => {
                    values.extend(
                        array
                            .downcast_ref::<BooleanArray>()
                            .ok_or_else(wrong)?
                            .iter(),
                    );
                }
            }
        }
    }
    let name = path
        .parent()
        .and_then(Path::file_name)
        .and_then(|n| n.to_str())
        .unwrap_or_default()
        .to_string();
    Ok(Table { name, columns })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::{CampaignRow, DecisionRow, SeriesRow, Summary, ZoneLook};

    fn info() -> RunInfo {
        RunInfo {
            run_id: "exp-000-abcd1234".into(),
            experiment: "exp".into(),
            index: 0,
            config_hash: "abcd1234".into(),
            code_version: "0.1.0".into(),
            mode: "backtest".into(),
            window_from: "2026-01-08".into(),
            window_to: "2026-01-30".into(),
            capital_usd: 10_000.0,
            loss_limit_pct: 5.0,
            autonomy: "auto".into(),
            strategy: "eth-zone-roundtrip".into(),
            patience: "wait".into(),
            pool: "USDC/WETH 0.05".into(),
            value_token: "WETH".into(),
            fee_pips: 500,
            width_ticks: 20,
            sweep: "entry.width_ticks=20; patience.mode=wait".into(),
            config_yaml: "mode: backtest\n".into(),
        }
    }

    fn output() -> Output {
        Output {
            campaigns: vec![CampaignRow {
                id: 1,
                tick_lower: -20,
                tick_upper: 0,
                opened_time: 100,
                opened_block: 10,
                closed_time: 400,
                closed_block: 40,
                exit_reason: "round_trip_complete".into(),
                amount_in: 5.0,
                result: 0.005,
                fees: 0.005,
                gas: 0.0,
                passes: 2,
                flips: 0,
                seconds_beyond: 120,
            }],
            decisions: vec![
                DecisionRow {
                    campaign: 1,
                    time: 200,
                    block: 20,
                    tick: -50,
                    leg: "entry".into(),
                    seconds_stranded: 0,
                    action: "wait".into(),
                    target_lower: None,
                    target_upper: None,
                },
                DecisionRow {
                    campaign: 1,
                    time: 300,
                    block: 30,
                    tick: -50,
                    leg: "entry".into(),
                    seconds_stranded: 100,
                    action: "flip".into(),
                    target_lower: Some(-40),
                    target_upper: Some(-20),
                },
            ],
            series: vec![SeriesRow {
                time: 100,
                block: 10,
                tick: 5,
                price: 2_000.5,
                equity: 5.0,
                hold: 5.0,
                fees: 0.0,
                state: "flat".into(),
                tick_lower: None,
                tick_upper: None,
                price_lower: None,
                price_upper: None,
            }],
            zones: vec![ZoneLook {
                time: 100,
                lookback_days: 7,
                tick_lower: -20,
                tick_upper: 0,
                observed_days: 6.5,
                passes_per_day: 3.25,
                time_in_range: 0.125,
                return_odds_1d: None,
            }],
            summary: Summary {
                start_capital: 5.0,
                end_capital: 5.005,
                result_pct: 0.1,
                fees: 0.005,
                campaigns: 1,
                passes: 2,
                events: 8,
                ..Default::default()
            },
        }
    }

    fn temp(label: &str) -> PathBuf {
        let path =
            std::env::temp_dir().join(format!("lpbot-tables-{label}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&path);
        path
    }

    #[test]
    fn a_run_writes_exactly_the_six_tables() {
        let rank = [PoolRankRow {
            time: 100,
            pool: "USDC/WETH 0.05".into(),
            fee: 0.0005,
            volume_24h_usd: 4e8,
            volume_per_day_usd: 3.5e8,
            depth_1pct_usd: 8e6,
            implied_vol_24h: Some(0.6),
            implied_vol: Some(0.55),
            excluded: None,
        }];
        let tables = result_tables(&info(), &output(), &rank);
        let names: Vec<&str> = tables.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, RESULT_TABLES);
        let rows: Vec<usize> = tables.iter().map(Table::rows).collect();
        assert_eq!(rows, vec![1, 1, 2, 1, 1, 1]);
        for table in &tables {
            assert_eq!(table.columns[0].0, "run_id");
            assert!(table.columns.iter().all(|(_, c)| c.len() == table.rows()));
        }
    }

    #[test]
    fn tables_survive_a_round_trip_with_their_empty_cells() {
        let dir = temp("round-trip");
        let paths = write_run(&dir, &info(), &output(), &[]).unwrap();
        assert_eq!(paths.len(), 6);
        assert_eq!(
            paths[1],
            dir.join("campaigns").join("exp-000-abcd1234.parquet")
        );
        let written = result_tables(&info(), &output(), &[]);
        for (path, table) in paths.iter().zip(&written) {
            assert_eq!(&read_table(path).unwrap(), table, "{}", table.name);
        }

        let decisions = read_table(&paths[2]).unwrap();
        assert_eq!(
            decisions.column("target_lower"),
            Some(&Column::Int(vec![None, Some(-40)]))
        );
        assert_eq!(
            decisions.column("expected_wait"),
            Some(&Column::Float(vec![None, None]))
        );
        // A table without rows is still a file with its columns.
        let rank = read_table(&paths[4]).unwrap();
        assert_eq!(rank.rows(), 0);
        assert!(rank.column("implied_vol").is_some());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_table_with_uneven_columns_is_refused() {
        let dir = temp("uneven");
        let bad = Table {
            name: "runs".into(),
            columns: vec![
                ("a".into(), Column::Int(vec![Some(1), Some(2)])),
                ("b".into(), Column::Int(vec![Some(1)])),
            ],
        };
        let error = write_table(&dir, "x", &bad).unwrap_err().to_string();
        assert!(error.contains("column `b`"), "{error}");
        assert!(!table_path(&dir, "runs", "x").exists());
        assert!(read_table(&dir.join("absent.parquet")).is_err());
        let _ = fs::remove_dir_all(&dir);
    }
}
