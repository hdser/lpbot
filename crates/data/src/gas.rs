//! Gas prices per block: each block's base fee and the median priority fee
//! its transactions paid, as the node's fee history gives them. A run
//! charges each action the price of the block it lands in.
//!
//! ```text
//! <root>/gas/chain=<chain>/blocks=<first>.parquet
//! <root>/gas/chain=<chain>/blocks=<first>.partial.parquet
//! ```
//!
//! A file holds the blocks `first` to `first + CHUNK - 1`, with `first` a
//! multiple of `CHUNK`. A chunk the chain has finished is written once; the
//! chunk at the head is `.partial` and rewritten by every fetch until it
//! is whole. A partial chunk may reach past the finalized block, as a
//! forward run's catch-up stores it: it then records the last of its
//! blocks that was final, the next fetch keeps those and reads the rest
//! again, and the chunk is written whole only once its last block is final.

use std::collections::BTreeSet;
use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use arrow_array::{Array, ArrayRef, RecordBatch, UInt64Array};
use arrow_schema::{DataType, Field, Schema};
use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
use parquet::arrow::ArrowWriter;
use parquet::basic::Compression;
use parquet::file::metadata::KeyValue;
use parquet::file::properties::WriterProperties;

use crate::store::{write_durably, StoreError, StoreResult};

/// Blocks per file.
pub const CHUNK: u64 = 100_000;

/// The gas prices of a run of consecutive blocks, in wei per gas.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct BlockFees {
    first: u64,
    base: Vec<u64>,
    tip: Vec<u64>,
}

impl BlockFees {
    /// The fees of the blocks from `first` on, one entry per block.
    /// `None` when the two lists differ in length.
    pub fn new(first: u64, base: Vec<u64>, tip: Vec<u64>) -> Option<Self> {
        (base.len() == tip.len()).then_some(Self { first, base, tip })
    }

    /// The first block covered.
    pub fn first(&self) -> u64 {
        self.first
    }

    /// One past the last block covered.
    pub fn end(&self) -> u64 {
        self.first + self.base.len() as u64
    }

    pub fn len(&self) -> usize {
        self.base.len()
    }

    pub fn is_empty(&self) -> bool {
        self.base.is_empty()
    }

    /// Base fee and median tip of a block, when covered.
    pub fn at(&self, block: u64) -> Option<(u64, u64)> {
        let i = usize::try_from(block.checked_sub(self.first)?).ok()?;
        Some((*self.base.get(i)?, *self.tip.get(i)?))
    }

    /// Whether every block from `from` to `to`, both included, is covered.
    pub fn covers(&self, from: u64, to: u64) -> bool {
        from <= to && from >= self.first && to < self.end()
    }

    /// Adds the blocks that follow on directly. `Err` with the gap or
    /// overlap otherwise; an empty run adds nothing.
    pub fn extend(&mut self, next: BlockFees) -> Result<(), String> {
        if next.is_empty() {
            return Ok(());
        }
        if self.is_empty() {
            *self = next;
            return Ok(());
        }
        if next.first != self.end() {
            return Err(format!(
                "blocks {} on do not follow block {}",
                next.first,
                self.end() - 1
            ));
        }
        self.base.extend(next.base);
        self.tip.extend(next.tip);
        Ok(())
    }

    /// The blocks from `from` to `to`, both included, as far as covered.
    pub fn range(&self, from: u64, to: u64) -> BlockFees {
        let start = from.max(self.first).min(self.end());
        let end = to.saturating_add(1).min(self.end()).max(start);
        let (a, b) = ((start - self.first) as usize, (end - self.first) as usize);
        BlockFees {
            first: start,
            base: self.base[a..b].to_vec(),
            tip: self.tip[a..b].to_vec(),
        }
    }

    /// The same blocks with each tip replaced by the median of the tips of
    /// that block and the `window - 1` before it, as a wallet sets its
    /// priority fee from recent blocks. A block with a handful of
    /// transactions can show a median tip of thousands of gwei; no sender
    /// would pay that for the next block.
    pub fn with_recent_median_tip(&self, window: usize) -> BlockFees {
        let window = window.max(1);
        let mut recent = Vec::with_capacity(window);
        let tip = (0..self.tip.len())
            .map(|i| {
                recent.clear();
                recent.extend_from_slice(&self.tip[(i + 1).saturating_sub(window)..=i]);
                let middle = recent.len() / 2;
                *recent.select_nth_unstable(middle).1
            })
            .collect();
        BlockFees {
            first: self.first,
            base: self.base.clone(),
            tip,
        }
    }

    /// How many blocks are covered and their average price (base fee plus
    /// median tip), in wei per gas. `None` when empty.
    pub fn summary(&self) -> Option<(f64, f64)> {
        if self.is_empty() {
            return None;
        }
        let total: f64 = self
            .base
            .iter()
            .zip(&self.tip)
            .map(|(b, t)| (*b + *t) as f64)
            .sum();
        let n = self.len() as f64;
        Some((n, total / n))
    }
}

/// The first block of the chunk that holds `block`.
pub fn chunk_of(block: u64) -> u64 {
    block - block % CHUNK
}

/// The gas prices stored under one root.
pub struct GasStore {
    root: PathBuf,
}

impl GasStore {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    fn dir(&self, chain: &str) -> PathBuf {
        self.root.join("gas").join(format!("chain={chain}"))
    }

    fn path(&self, chain: &str, first: u64, partial: bool) -> PathBuf {
        let name = if partial {
            format!("blocks={first}.partial.parquet")
        } else {
            format!("blocks={first}.parquet")
        };
        self.dir(chain).join(name)
    }

    /// The first blocks of the chunks stored whole.
    pub fn complete_chunks(&self, chain: &str) -> StoreResult<BTreeSet<u64>> {
        let dir = self.dir(chain);
        let entries = match fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(BTreeSet::new()),
            Err(source) => return Err(StoreError::Io { path: dir, source }),
        };
        let mut chunks = BTreeSet::new();
        for entry in entries {
            let entry = entry.map_err(|source| StoreError::Io {
                path: dir.clone(),
                source,
            })?;
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if let Some(first) = name
                .strip_prefix("blocks=")
                .and_then(|rest| rest.strip_suffix(".parquet"))
                .and_then(|n| n.parse::<u64>().ok())
            {
                chunks.insert(first);
            }
        }
        Ok(chunks)
    }

    /// Stores one chunk's fees, starting at its first block, every one of
    /// them final. A chunk with all its blocks is written whole and replaces
    /// the partial file; fewer is written as the partial file.
    pub fn write_chunk(&self, chain: &str, fees: &BlockFees) -> StoreResult<()> {
        self.write_head_chunk(chain, fees, u64::MAX)
    }

    /// Stores one chunk's fees, starting at its first block, when its newest
    /// blocks may be past `final_block`, the newest block no reorg can
    /// change. It is written whole only once it has all its blocks and the
    /// last of them is final; otherwise as the partial file, which records
    /// the last of its blocks that is final (`final_part`).
    pub fn write_head_chunk(
        &self,
        chain: &str,
        fees: &BlockFees,
        final_block: u64,
    ) -> StoreResult<()> {
        let first = fees.first();
        if !first.is_multiple_of(CHUNK) || fees.len() as u64 > CHUNK {
            return Err(StoreError::BadFile {
                path: self.dir(chain),
                reason: format!(
                    "a gas chunk starts at a multiple of {CHUNK} and holds at most that many \
                     blocks, not {} from block {first}",
                    fees.len()
                ),
            });
        }
        let last = fees.end().saturating_sub(1);
        let whole = fees.len() as u64 == CHUNK && last <= final_block;
        let path = self.path(chain, first, !whole);
        // The mark is left out when every block is final, as before it existed.
        let mark = (!whole && final_block < last).then_some(final_block);
        write_fees(&path, fees, mark)?;
        if whole {
            let partial = self.path(chain, first, true);
            if partial.is_file() {
                let _ = fs::remove_file(partial);
            }
        }
        Ok(())
    }

    /// The blocks of a chunk's partial file that were final when it was
    /// written: all of them, unless it records an earlier final block.
    /// `None` when the chunk has no partial file.
    pub fn final_part(&self, chain: &str, first: u64) -> StoreResult<Option<BlockFees>> {
        let path = self.path(chain, first, true);
        if !path.is_file() {
            return Ok(None);
        }
        let (fees, mark) = read_fees_marked(&path)?;
        Ok(Some(match mark {
            Some(final_block) if final_block < first => BlockFees::default(),
            Some(final_block) => fees.range(first, final_block),
            None => fees,
        }))
    }

    /// The fees of the blocks `from` to `to` with each tip the median of
    /// the last `window` blocks' (`BlockFees::with_recent_median_tip`),
    /// counted over the blocks before `from` too when they are stored, so
    /// the first blocks see a whole window. `None` when any block from
    /// `from` to `to` is not stored.
    pub fn read_with_recent_tips(
        &self,
        chain: &str,
        from: u64,
        to: u64,
        window: usize,
    ) -> StoreResult<Option<BlockFees>> {
        let before = from.saturating_sub(window.saturating_sub(1) as u64);
        let read = match self.read(chain, before, to)? {
            Some(fees) => Some(fees),
            None => self.read(chain, from, to)?,
        };
        Ok(read.map(|fees| fees.with_recent_median_tip(window).range(from, to)))
    }

    /// The fees of the blocks `from` to `to`, both included, or `None`
    /// when any of them is not stored.
    pub fn read(&self, chain: &str, from: u64, to: u64) -> StoreResult<Option<BlockFees>> {
        let mut fees = BlockFees::default();
        let mut chunk = chunk_of(from);
        while chunk <= to {
            let whole = self.path(chain, chunk, false);
            let partial = self.path(chain, chunk, true);
            let path = if whole.is_file() {
                whole
            } else if partial.is_file() {
                partial
            } else {
                return Ok(None);
            };
            let stored = read_fees(&path)?;
            if fees.extend(stored.range(from, to)).is_err() {
                return Ok(None);
            }
            chunk += CHUNK;
        }
        Ok(fees.covers(from, to).then_some(fees))
    }
}

fn schema() -> Arc<Schema> {
    Arc::new(Schema::new(vec![
        Field::new("block", DataType::UInt64, false),
        Field::new("base_fee", DataType::UInt64, false),
        Field::new("median_tip", DataType::UInt64, false),
    ]))
}

/// The key under which a partial chunk records the last of its blocks
/// that was final when it was written.
const FINAL_KEY: &str = "lpbot.final_block";

fn write_fees(path: &Path, fees: &BlockFees, final_block: Option<u64>) -> StoreResult<()> {
    let columns: Vec<ArrayRef> = vec![
        Arc::new(UInt64Array::from_iter_values(fees.first()..fees.end())),
        Arc::new(UInt64Array::from(fees.base.clone())),
        Arc::new(UInt64Array::from(fees.tip.clone())),
    ];
    let batch = RecordBatch::try_new(schema(), columns).map_err(|source| StoreError::Arrow {
        path: path.to_path_buf(),
        source,
    })?;
    let parquet = |source| StoreError::Parquet {
        path: path.to_path_buf(),
        source,
    };
    let properties = WriterProperties::builder()
        .set_compression(Compression::SNAPPY)
        .set_key_value_metadata(
            final_block.map(|block| vec![KeyValue::new(FINAL_KEY.to_string(), block.to_string())]),
        )
        .build();
    let mut bytes = Vec::new();
    let mut writer =
        ArrowWriter::try_new(&mut bytes, schema(), Some(properties)).map_err(parquet)?;
    writer.write(&batch).map_err(parquet)?;
    writer.close().map_err(parquet)?;
    write_durably(path, &bytes).map_err(|source| StoreError::Io {
        path: path.to_path_buf(),
        source,
    })
}

fn read_fees(path: &Path) -> StoreResult<BlockFees> {
    read_fees_marked(path).map(|(fees, _)| fees)
}

/// A file's fees and the final block it records, if any.
fn read_fees_marked(path: &Path) -> StoreResult<(BlockFees, Option<u64>)> {
    let bad = |reason: String| StoreError::Unreadable {
        path: path.to_path_buf(),
        reason,
    };
    let file = File::open(path).map_err(|source| StoreError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    let builder = ParquetRecordBatchReaderBuilder::try_new(file).map_err(|e| bad(e.to_string()))?;
    let mark = builder
        .metadata()
        .file_metadata()
        .key_value_metadata()
        .and_then(|pairs| pairs.iter().find(|kv| kv.key == FINAL_KEY))
        .and_then(|kv| kv.value.as_deref())
        .map(|value| {
            value
                .parse::<u64>()
                .map_err(|_| bad(format!("`{FINAL_KEY}` is {value}, not a block number")))
        })
        .transpose()?;
    let reader = builder.build().map_err(|e| bad(e.to_string()))?;
    let mut fees = BlockFees::default();
    for batch in reader {
        let batch = batch.map_err(|e| bad(e.to_string()))?;
        let column = |name: &str| -> StoreResult<Vec<u64>> {
            let array = batch
                .column_by_name(name)
                .and_then(|c| c.as_any().downcast_ref::<UInt64Array>())
                .ok_or_else(|| bad(format!("column `{name}` is missing or not a number")))?;
            Ok((0..array.len()).map(|i| array.value(i)).collect())
        };
        let blocks = column("block")?;
        let Some(&first) = blocks.first() else {
            continue;
        };
        if blocks.windows(2).any(|w| w[1] != w[0] + 1) {
            return Err(bad("the blocks are not consecutive".into()));
        }
        let next = BlockFees::new(first, column("base_fee")?, column("median_tip")?)
            .ok_or_else(|| bad("the columns differ in length".into()))?;
        fees.extend(next).map_err(bad)?;
    }
    Ok((fees, mark))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::testing::TempDir;

    fn fees(first: u64, n: u64) -> BlockFees {
        BlockFees::new(
            first,
            (first..first + n).map(|b| b * 10).collect(),
            (first..first + n).map(|b| b % 7).collect(),
        )
        .unwrap()
    }

    #[test]
    fn a_block_is_found_by_its_number() {
        let f = fees(100, 5);
        assert_eq!(f.first(), 100);
        assert_eq!(f.end(), 105);
        assert_eq!(f.at(102), Some((1_020, 102 % 7)));
        assert_eq!(f.at(99), None);
        assert_eq!(f.at(105), None);
        assert!(f.covers(100, 104));
        assert!(!f.covers(100, 105));
        assert!(!f.covers(104, 100));
        assert!(BlockFees::new(0, vec![1], vec![]).is_none());
    }

    #[test]
    fn runs_extend_only_when_they_follow_on() {
        let mut f = fees(100, 5);
        assert!(f.extend(fees(106, 2)).is_err(), "a gap");
        assert!(f.extend(fees(104, 2)).is_err(), "an overlap");
        f.extend(fees(105, 3)).unwrap();
        assert_eq!(f.end(), 108);
        assert_eq!(f.at(107), Some((1_070, 107 % 7)));
        let mut empty = BlockFees::default();
        empty.extend(fees(5, 1)).unwrap();
        assert_eq!(empty.first(), 5);
    }

    #[test]
    fn a_range_is_cut_to_what_is_covered() {
        let f = fees(100, 10);
        let r = f.range(103, 105);
        assert_eq!((r.first(), r.len()), (103, 3));
        assert_eq!(f.range(90, 101).len(), 2);
        assert_eq!(f.range(108, 200).len(), 2);
        assert!(f.range(200, 300).is_empty());
        assert_eq!(
            f.summary(),
            Some((
                10.0,
                f.base
                    .iter()
                    .zip(&f.tip)
                    .map(|(b, t)| (b + t) as f64)
                    .sum::<f64>()
                    / 10.0
            ))
        );
    }

    #[test]
    fn an_odd_tip_is_smoothed_by_the_blocks_before_it() {
        let f = BlockFees::new(10, vec![5; 6], vec![1, 2, 3, 5_000, 2, 1]).unwrap();
        let s = f.with_recent_median_tip(3);
        assert_eq!(s.tip, vec![1, 2, 2, 3, 3, 2]);
        assert_eq!(s.base, f.base);
        assert_eq!(s.first(), 10);
        assert_eq!(f.with_recent_median_tip(1), f);
    }

    #[test]
    fn the_first_blocks_read_count_their_tips_over_the_blocks_before() {
        let dir = TempDir::new("gas-recent");
        let store = GasStore::new(dir.path());
        // Tips 0, 1, 2, ...: the median of three ending at block b is b - 1.
        let tips: Vec<u64> = (0..10).collect();
        let fees = BlockFees::new(CHUNK, vec![7; 10], tips).unwrap();
        store.write_chunk("ethereum", &fees).unwrap();
        let read = store
            .read_with_recent_tips("ethereum", CHUNK + 5, CHUNK + 8, 3)
            .unwrap()
            .unwrap();
        assert_eq!((read.first(), read.len()), (CHUNK + 5, 4));
        assert_eq!(read.at(CHUNK + 5), Some((7, 4)), "blocks 3 to 5 counted");
        // At the first stored block there is nothing before it to count.
        let start = store
            .read_with_recent_tips("ethereum", CHUNK, CHUNK + 1, 3)
            .unwrap()
            .unwrap();
        assert_eq!(start.at(CHUNK), Some((7, 0)));
        assert_eq!(start.at(CHUNK + 1), Some((7, 1)));
        assert!(store
            .read_with_recent_tips("ethereum", CHUNK + 5, CHUNK + 20, 3)
            .unwrap()
            .is_none());
    }

    #[test]
    fn a_chunk_past_the_final_block_keeps_only_its_final_part_for_the_next_fetch() {
        let dir = TempDir::new("gas-head");
        let store = GasStore::new(dir.path());
        assert_eq!(store.final_part("ethereum", CHUNK).unwrap(), None);
        // Ten blocks stored, the first six of them final.
        store
            .write_head_chunk("ethereum", &fees(CHUNK, 10), CHUNK + 5)
            .unwrap();
        assert_eq!(
            store.read("ethereum", CHUNK, CHUNK + 9).unwrap(),
            Some(fees(CHUNK, 10)),
            "a run reads every stored block"
        );
        assert_eq!(
            store.final_part("ethereum", CHUNK).unwrap(),
            Some(fees(CHUNK, 6))
        );
        // Nothing final yet in the chunk.
        store
            .write_head_chunk("ethereum", &fees(CHUNK, 3), CHUNK - 1)
            .unwrap();
        assert_eq!(
            store.final_part("ethereum", CHUNK).unwrap(),
            Some(BlockFees::default())
        );
        // A chunk written as before, or with every block final, is all final.
        store.write_chunk("ethereum", &fees(CHUNK, 4)).unwrap();
        assert_eq!(
            store.final_part("ethereum", CHUNK).unwrap(),
            Some(fees(CHUNK, 4))
        );
        // All its blocks, the last not yet final: still partial.
        store
            .write_head_chunk("ethereum", &fees(CHUNK, CHUNK), 2 * CHUNK - 2)
            .unwrap();
        assert!(store.complete_chunks("ethereum").unwrap().is_empty());
        assert_eq!(
            store.final_part("ethereum", CHUNK).unwrap(),
            Some(fees(CHUNK, CHUNK - 1))
        );
        store
            .write_head_chunk("ethereum", &fees(CHUNK, CHUNK), 2 * CHUNK - 1)
            .unwrap();
        assert_eq!(
            store.complete_chunks("ethereum").unwrap(),
            BTreeSet::from([CHUNK])
        );
        assert_eq!(store.final_part("ethereum", CHUNK).unwrap(), None);
    }

    #[test]
    fn chunks_round_trip_and_a_whole_chunk_replaces_the_partial_one() {
        let dir = TempDir::new("gas-store");
        let store = GasStore::new(dir.path());
        store.write_chunk("ethereum", &fees(CHUNK, 10)).unwrap();
        assert!(store.complete_chunks("ethereum").unwrap().is_empty());
        assert_eq!(
            store.read("ethereum", CHUNK + 2, CHUNK + 5).unwrap(),
            Some(fees(CHUNK + 2, 4))
        );
        assert_eq!(store.read("ethereum", CHUNK, CHUNK + 10).unwrap(), None);

        store.write_chunk("ethereum", &fees(CHUNK, CHUNK)).unwrap();
        store.write_chunk("ethereum", &fees(2 * CHUNK, 3)).unwrap();
        assert_eq!(
            store.complete_chunks("ethereum").unwrap(),
            BTreeSet::from([CHUNK])
        );
        let across = store
            .read("ethereum", 2 * CHUNK - 2, 2 * CHUNK + 1)
            .unwrap()
            .unwrap();
        assert_eq!(across, fees(2 * CHUNK - 2, 4));
        assert_eq!(store.read("ethereum", 0, 5).unwrap(), None);
        assert!(store.write_chunk("ethereum", &fees(5, 3)).is_err());
    }
}
