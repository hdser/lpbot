//! The event store: a pool's raw logs as Parquet files, one per UTC day.
//!
//! ```text
//! <root>/events/chain=<chain>/pool=<address>/day=<YYYY-MM-DD>/events.parquet
//! ```
//!
//! Logs are stored raw (topics and data), so the replica replays exactly
//! what the chain emitted and a new event type needs no new backfill. A day
//! whose last block is final is written once as `events.parquet` and never
//! touched again. The day still in progress is `events.partial.parquet`,
//! rewritten by every backfill until the day closes.

use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use alloy_primitives::{Address, B256};
use arrow_array::builder::{BinaryBuilder, FixedSizeBinaryBuilder};
use arrow_array::{
    Array, ArrayRef, BinaryArray, FixedSizeBinaryArray, RecordBatch, UInt32Array, UInt64Array,
};
use arrow_schema::{ArrowError, DataType, Field, Schema};
use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
use parquet::arrow::ArrowWriter;
use parquet::basic::Compression;
use parquet::errors::ParquetError;
use parquet::file::properties::WriterProperties;

use crate::day::Day;

const COMPLETE_FILE: &str = "events.parquet";
const PARTIAL_FILE: &str = "events.partial.parquet";

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("{path}: {source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("{path}: {source}")]
    Parquet { path: PathBuf, source: ParquetError },
    #[error("{path}: {source}")]
    Arrow { path: PathBuf, source: ArrowError },
    #[error("{path} is not an event file: {reason}")]
    BadFile { path: PathBuf, reason: String },
    #[error(
        "no events stored for pool {pool:#x} on {day}; run `lpbot backfill` for that range first"
    )]
    MissingDay { pool: Address, day: Day },
    #[error(
        "the stored events of pool {pool:#x} do not replay at block {block}, log {log_index}: \
         {reason}. The stored history has a gap or is damaged; delete the pool's folders under \
         `events` and `snapshots` and run `lpbot backfill` again"
    )]
    Replay {
        pool: Address,
        block: u64,
        log_index: u32,
        reason: String,
    },
}

pub type StoreResult<T> = Result<T, StoreError>;

/// One log as the chain emitted it, with the time of its block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredLog {
    pub block_number: u64,
    /// Unix time of the block, in seconds.
    pub block_time: u64,
    pub log_index: u32,
    pub tx_hash: B256,
    /// Up to four topics; the first is the event signature.
    pub topics: Vec<B256>,
    pub data: Vec<u8>,
}

/// What the store holds for one pool and day.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DayState {
    Missing,
    /// The day was still in progress when it was written.
    Partial,
    Complete,
}

fn schema() -> Arc<Schema> {
    let topic = |name: &str, nullable| Field::new(name, DataType::FixedSizeBinary(32), nullable);
    Arc::new(Schema::new(vec![
        Field::new("block_number", DataType::UInt64, false),
        Field::new("block_time", DataType::UInt64, false),
        Field::new("log_index", DataType::UInt32, false),
        topic("tx_hash", false),
        topic("topic0", true),
        topic("topic1", true),
        topic("topic2", true),
        topic("topic3", true),
        Field::new("data", DataType::Binary, false),
    ]))
}

/// Writes logs to a Parquet file. The file appears under its name only
/// once it is whole: it is written beside it first, then renamed.
pub fn write_logs(path: &Path, logs: &[StoredLog]) -> StoreResult<()> {
    let io = |source| StoreError::Io {
        path: path.to_path_buf(),
        source,
    };
    let arrow = |source| StoreError::Arrow {
        path: path.to_path_buf(),
        source,
    };
    let parquet = |source| StoreError::Parquet {
        path: path.to_path_buf(),
        source,
    };
    if let Some(log) = logs.iter().find(|l| l.topics.len() > 4) {
        return Err(StoreError::BadFile {
            path: path.to_path_buf(),
            reason: format!(
                "a log in block {} has {} topics",
                log.block_number,
                log.topics.len()
            ),
        });
    }

    let mut tx_hashes = FixedSizeBinaryBuilder::with_capacity(logs.len(), 32);
    let mut topics: Vec<FixedSizeBinaryBuilder> = (0..4)
        .map(|_| FixedSizeBinaryBuilder::with_capacity(logs.len(), 32))
        .collect();
    let mut data = BinaryBuilder::new();
    for log in logs {
        tx_hashes.append_value(log.tx_hash).map_err(arrow)?;
        for (i, builder) in topics.iter_mut().enumerate() {
            match log.topics.get(i) {
                Some(topic) => builder.append_value(topic).map_err(arrow)?,
                None => builder.append_null(),
            }
        }
        data.append_value(&log.data);
    }
    let mut columns: Vec<ArrayRef> = vec![
        Arc::new(UInt64Array::from_iter_values(
            logs.iter().map(|l| l.block_number),
        )),
        Arc::new(UInt64Array::from_iter_values(
            logs.iter().map(|l| l.block_time),
        )),
        Arc::new(UInt32Array::from_iter_values(
            logs.iter().map(|l| l.log_index),
        )),
        Arc::new(tx_hashes.finish()),
    ];
    for mut builder in topics {
        columns.push(Arc::new(builder.finish()));
    }
    columns.push(Arc::new(data.finish()));
    let batch = RecordBatch::try_new(schema(), columns).map_err(arrow)?;

    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(io)?;
    }
    let temporary = path.with_extension("tmp");
    let file = File::create(&temporary).map_err(io)?;
    let properties = WriterProperties::builder()
        .set_compression(Compression::SNAPPY)
        .build();
    let mut writer = ArrowWriter::try_new(file, schema(), Some(properties)).map_err(parquet)?;
    writer.write(&batch).map_err(parquet)?;
    writer.close().map_err(parquet)?;
    fs::rename(&temporary, path).map_err(io)
}

/// Reads a file written by `write_logs`, in the order it was written.
pub fn read_logs(path: &Path) -> StoreResult<Vec<StoredLog>> {
    let bad = |reason: String| StoreError::BadFile {
        path: path.to_path_buf(),
        reason,
    };
    let file = File::open(path).map_err(|source| StoreError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    let reader = ParquetRecordBatchReaderBuilder::try_new(file)
        .and_then(|builder| builder.build())
        .map_err(|source| StoreError::Parquet {
            path: path.to_path_buf(),
            source,
        })?;

    let mut logs = Vec::new();
    for batch in reader {
        let batch = batch.map_err(|source| StoreError::Arrow {
            path: path.to_path_buf(),
            source,
        })?;
        fn column<'a, T: Array + 'static>(
            batch: &'a RecordBatch,
            name: &str,
        ) -> Result<&'a T, String> {
            batch
                .column_by_name(name)
                .ok_or_else(|| format!("column `{name}` is missing"))?
                .as_any()
                .downcast_ref::<T>()
                .ok_or_else(|| format!("column `{name}` has the wrong type"))
        }
        let block_numbers = column::<UInt64Array>(&batch, "block_number").map_err(bad)?;
        let block_times = column::<UInt64Array>(&batch, "block_time").map_err(bad)?;
        let log_indexes = column::<UInt32Array>(&batch, "log_index").map_err(bad)?;
        let tx_hashes = column::<FixedSizeBinaryArray>(&batch, "tx_hash").map_err(bad)?;
        let data = column::<BinaryArray>(&batch, "data").map_err(bad)?;
        let mut topic_columns = Vec::with_capacity(4);
        for name in ["topic0", "topic1", "topic2", "topic3"] {
            topic_columns.push(column::<FixedSizeBinaryArray>(&batch, name).map_err(bad)?);
        }
        let word = |array: &FixedSizeBinaryArray, row: usize| {
            B256::try_from(array.value(row)).map_err(|_| bad("a hash is not 32 bytes".into()))
        };

        for row in 0..batch.num_rows() {
            let mut topics = Vec::new();
            for column in &topic_columns {
                if column.is_null(row) {
                    break;
                }
                topics.push(word(column, row)?);
            }
            logs.push(StoredLog {
                block_number: block_numbers.value(row),
                block_time: block_times.value(row),
                log_index: log_indexes.value(row),
                tx_hash: word(tx_hashes, row)?,
                topics,
                data: data.value(row).to_vec(),
            });
        }
    }
    Ok(logs)
}

/// The event store rooted at a data folder.
#[derive(Debug, Clone)]
pub struct EventStore {
    root: PathBuf,
}

impl EventStore {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn pool_dir(&self, chain: &str, pool: Address) -> PathBuf {
        self.root
            .join("events")
            .join(format!("chain={chain}"))
            .join(format!("pool={pool:#x}"))
    }

    fn day_dir(&self, chain: &str, pool: Address, day: Day) -> PathBuf {
        self.pool_dir(chain, pool).join(format!("day={day}"))
    }

    pub fn day_state(&self, chain: &str, pool: Address, day: Day) -> DayState {
        let dir = self.day_dir(chain, pool, day);
        if dir.join(COMPLETE_FILE).is_file() {
            DayState::Complete
        } else if dir.join(PARTIAL_FILE).is_file() {
            DayState::Partial
        } else {
            DayState::Missing
        }
    }

    /// Stores a day's logs. A complete day replaces any partial file.
    pub fn write_day(
        &self,
        chain: &str,
        pool: Address,
        day: Day,
        logs: &[StoredLog],
        complete: bool,
    ) -> StoreResult<()> {
        let dir = self.day_dir(chain, pool, day);
        if complete {
            write_logs(&dir.join(COMPLETE_FILE), logs)?;
            let partial = dir.join(PARTIAL_FILE);
            if partial.is_file() {
                fs::remove_file(&partial).map_err(|source| StoreError::Io {
                    path: partial,
                    source,
                })?;
            }
            Ok(())
        } else {
            write_logs(&dir.join(PARTIAL_FILE), logs)
        }
    }

    /// A day's logs in chain order, or `None` when the day is not stored.
    pub fn read_day(
        &self,
        chain: &str,
        pool: Address,
        day: Day,
    ) -> StoreResult<Option<Vec<StoredLog>>> {
        let dir = self.day_dir(chain, pool, day);
        match self.day_state(chain, pool, day) {
            DayState::Complete => read_logs(&dir.join(COMPLETE_FILE)).map(Some),
            DayState::Partial => read_logs(&dir.join(PARTIAL_FILE)).map(Some),
            DayState::Missing => Ok(None),
        }
    }

    /// The logs of every day from `from` to `to`, both included, in chain
    /// order. A day that is not stored is an error, so a gap can never be
    /// mistaken for a quiet day.
    pub fn read_days(
        &self,
        chain: &str,
        pool: Address,
        from: Day,
        to: Day,
    ) -> StoreResult<Vec<StoredLog>> {
        let mut logs = Vec::new();
        let mut day = from;
        while day <= to {
            logs.extend(
                self.read_day(chain, pool, day)?
                    .ok_or(StoreError::MissingDay { pool, day })?,
            );
            day = day.next();
        }
        Ok(logs)
    }

    /// The days stored for a pool, oldest first.
    pub fn days(&self, chain: &str, pool: Address) -> StoreResult<Vec<(Day, DayState)>> {
        let dir = self.pool_dir(chain, pool);
        if !dir.is_dir() {
            return Ok(Vec::new());
        }
        let io = |source| StoreError::Io {
            path: dir.clone(),
            source,
        };
        let mut days = Vec::new();
        for entry in fs::read_dir(&dir).map_err(io)? {
            let name = entry.map_err(io)?.file_name();
            let Some(day) = name
                .to_str()
                .and_then(|n| n.strip_prefix("day="))
                .and_then(|d| d.parse::<Day>().ok())
            else {
                continue;
            };
            match self.day_state(chain, pool, day) {
                DayState::Missing => {}
                state => days.push((day, state)),
            }
        }
        days.sort_unstable_by_key(|(day, _)| *day);
        Ok(days)
    }
}

#[cfg(test)]
pub(crate) mod testing {
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU32, Ordering};

    /// A folder under the system's temporary folder, removed when dropped.
    pub struct TempDir(PathBuf);

    impl TempDir {
        pub fn new(label: &str) -> Self {
            static NEXT: AtomicU32 = AtomicU32::new(0);
            let path = std::env::temp_dir().join(format!(
                "lpbot-{label}-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&path).expect("create a temporary folder");
            Self(path)
        }

        pub fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::testing::TempDir;
    use super::*;

    fn pool() -> Address {
        "0x88e6A0c2dDD26FEEb64F039a2c41296FcB3f5640"
            .parse()
            .unwrap()
    }

    fn log(block: u64, index: u32, topics: usize, data: &[u8]) -> StoredLog {
        StoredLog {
            block_number: block,
            block_time: 1_700_000_000 + block * 12,
            log_index: index,
            tx_hash: B256::repeat_byte(block as u8),
            topics: (0..topics)
                .map(|i| B256::repeat_byte(0xA0 + i as u8))
                .collect(),
            data: data.to_vec(),
        }
    }

    fn sample() -> Vec<StoredLog> {
        vec![
            log(100, 0, 3, &[1, 2, 3]),
            log(100, 7, 4, &[0xFF; 160]),
            log(101, 2, 1, &[]),
            log(250, 0, 0, &[9]),
        ]
    }

    #[test]
    fn logs_survive_a_round_trip_through_parquet() {
        let dir = TempDir::new("store");
        let path = dir.path().join("a/b/events.parquet");
        write_logs(&path, &sample()).unwrap();
        assert_eq!(read_logs(&path).unwrap(), sample());
        assert!(!path.with_extension("tmp").exists());
    }

    #[test]
    fn an_empty_day_is_a_file_with_no_rows() {
        let dir = TempDir::new("store");
        let path = dir.path().join("events.parquet");
        write_logs(&path, &[]).unwrap();
        assert_eq!(read_logs(&path).unwrap(), vec![]);
    }

    #[test]
    fn writing_the_same_logs_twice_gives_the_same_bytes() {
        let dir = TempDir::new("store");
        let (a, b) = (dir.path().join("a.parquet"), dir.path().join("b.parquet"));
        write_logs(&a, &sample()).unwrap();
        write_logs(&b, &sample()).unwrap();
        assert_eq!(fs::read(&a).unwrap(), fs::read(&b).unwrap());
    }

    #[test]
    fn a_log_with_too_many_topics_or_a_foreign_file_is_refused() {
        let dir = TempDir::new("store");
        let path = dir.path().join("events.parquet");
        let error = write_logs(&path, &[log(1, 0, 5, &[])]).unwrap_err();
        assert!(error.to_string().contains("5 topics"), "{error}");
        assert!(!path.exists());

        fs::write(&path, b"not parquet").unwrap();
        assert!(matches!(read_logs(&path), Err(StoreError::Parquet { .. })));
        assert!(matches!(
            read_logs(&dir.path().join("absent.parquet")),
            Err(StoreError::Io { .. })
        ));
    }

    #[test]
    fn days_are_laid_out_by_chain_pool_and_day() {
        let dir = TempDir::new("store");
        let store = EventStore::new(dir.path());
        let day: Day = "2026-11-01".parse().unwrap();
        store
            .write_day("ethereum", pool(), day, &sample(), true)
            .unwrap();
        let expected = dir.path().join(
            "events/chain=ethereum/pool=0x88e6a0c2ddd26feeb64f039a2c41296fcb3f5640/day=2026-11-01/events.parquet",
        );
        assert!(expected.is_file());
        assert_eq!(store.root(), dir.path());
    }

    #[test]
    fn a_partial_day_gives_way_to_the_complete_one() {
        let dir = TempDir::new("store");
        let store = EventStore::new(dir.path());
        let day: Day = "2026-11-01".parse().unwrap();
        assert_eq!(store.day_state("ethereum", pool(), day), DayState::Missing);
        assert_eq!(store.read_day("ethereum", pool(), day).unwrap(), None);

        store
            .write_day("ethereum", pool(), day, &sample()[..2], false)
            .unwrap();
        assert_eq!(store.day_state("ethereum", pool(), day), DayState::Partial);
        assert_eq!(
            store.read_day("ethereum", pool(), day).unwrap().unwrap(),
            sample()[..2]
        );

        store
            .write_day("ethereum", pool(), day, &sample(), true)
            .unwrap();
        assert_eq!(store.day_state("ethereum", pool(), day), DayState::Complete);
        assert_eq!(
            store.read_day("ethereum", pool(), day).unwrap().unwrap(),
            sample()
        );
        let files: Vec<_> = fs::read_dir(store.day_dir("ethereum", pool(), day))
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(files, vec![std::ffi::OsString::from("events.parquet")]);
    }

    #[test]
    fn a_range_of_days_reads_in_order_and_names_a_gap() {
        let dir = TempDir::new("store");
        let store = EventStore::new(dir.path());
        let first: Day = "2026-10-31".parse().unwrap();
        let (second, third) = (first.next(), first.next().next());
        store
            .write_day("ethereum", pool(), first, &sample()[..1], true)
            .unwrap();
        store
            .write_day("ethereum", pool(), third, &sample()[2..], false)
            .unwrap();

        let error = store
            .read_days("ethereum", pool(), first, third)
            .unwrap_err();
        assert!(
            error.to_string().contains("2026-11-01")
                && error.to_string().contains("lpbot backfill"),
            "{error}"
        );

        store
            .write_day("ethereum", pool(), second, &sample()[1..2], true)
            .unwrap();
        assert_eq!(
            store.read_days("ethereum", pool(), first, third).unwrap(),
            sample()
        );
        assert_eq!(
            store.days("ethereum", pool()).unwrap(),
            vec![
                (first, DayState::Complete),
                (second, DayState::Complete),
                (third, DayState::Partial)
            ]
        );
        assert_eq!(store.days("ethereum", Address::ZERO).unwrap(), vec![]);
        assert_eq!(store.days("base", pool()).unwrap(), vec![]);
    }
}
