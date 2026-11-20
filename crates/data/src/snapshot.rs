//! Stored pool snapshots: a pool's full state at one block.
//!
//! A replay needs the state the pool was in before its first event. The
//! backfill stores it next to the events:
//!
//! ```text
//! <root>/snapshots/chain=<chain>/pool=<address>/day=<YYYY-MM-DD>.json
//! ```
//!
//! The file for a day holds the state right before that day's first block.
//! Numbers wider than 53 bits are written as decimal strings, so any JSON
//! reader gets them exactly.

use std::fs;
use std::path::{Path, PathBuf};

use alloy_primitives::{Address, U256};
use lpbot_venues::uniswap_v3::pool::{PoolSnapshot, TickInfo};
use serde_json::{json, Value};

use crate::day::Day;
use crate::store::{write_durably, StoreError, StoreResult};

/// A pool's state before the first block of `day`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredSnapshot {
    pub pool: Address,
    pub token0: Address,
    pub token1: Address,
    /// The state is the one after this block.
    pub block: u64,
    pub day: Day,
    pub state: PoolSnapshot,
}

pub(crate) fn to_json(snapshot: &StoredSnapshot) -> Value {
    let s = &snapshot.state;
    let ticks: Vec<Value> = s
        .ticks
        .iter()
        .map(|(tick, info)| {
            json!([
                tick,
                info.liquidity_gross.to_string(),
                info.liquidity_net.to_string(),
                info.fee_growth_outside0.to_string(),
                info.fee_growth_outside1.to_string(),
            ])
        })
        .collect();
    json!({
        "pool": format!("{:#x}", snapshot.pool),
        "token0": format!("{:#x}", snapshot.token0),
        "token1": format!("{:#x}", snapshot.token1),
        "block": snapshot.block,
        "day": snapshot.day.to_string(),
        "fee_pips": s.fee_pips,
        "tick_spacing": s.tick_spacing,
        "sqrt_price_x96": s.sqrt_price_x96.to_string(),
        "tick": s.tick,
        "liquidity": s.liquidity.to_string(),
        "fee_growth_global0": s.fee_growth_global0.to_string(),
        "fee_growth_global1": s.fee_growth_global1.to_string(),
        "fee_protocol0": s.fee_protocol0,
        "fee_protocol1": s.fee_protocol1,
        "ticks": ticks,
    })
}

pub(crate) fn from_json(value: &Value) -> Result<StoredSnapshot, String> {
    fn text<'a>(value: &'a Value, key: &str) -> Result<&'a str, String> {
        value
            .get(key)
            .and_then(Value::as_str)
            .ok_or_else(|| format!("`{key}` is missing or not text"))
    }
    fn parsed<T: std::str::FromStr>(value: &Value, key: &str) -> Result<T, String> {
        text(value, key)?
            .parse()
            .map_err(|_| format!("`{key}` cannot be read"))
    }
    fn integer<T: TryFrom<i64>>(value: &Value, key: &str) -> Result<T, String> {
        value
            .get(key)
            .and_then(Value::as_i64)
            .and_then(|n| T::try_from(n).ok())
            .ok_or_else(|| format!("`{key}` is missing or out of range"))
    }
    fn big(text: Option<&str>, what: &str) -> Result<U256, String> {
        text.and_then(|t| U256::from_str_radix(t, 10).ok())
            .ok_or_else(|| format!("{what} cannot be read"))
    }

    let mut ticks = Vec::new();
    for entry in value
        .get("ticks")
        .and_then(Value::as_array)
        .ok_or("`ticks` is missing")?
    {
        let field = |i: usize| entry.get(i).and_then(Value::as_str);
        let tick = entry
            .get(0)
            .and_then(Value::as_i64)
            .and_then(|t| i32::try_from(t).ok())
            .ok_or("a tick index cannot be read")?;
        ticks.push((
            tick,
            TickInfo {
                liquidity_gross: field(1)
                    .and_then(|t| t.parse().ok())
                    .ok_or("a tick's gross liquidity cannot be read")?,
                liquidity_net: field(2)
                    .and_then(|t| t.parse().ok())
                    .ok_or("a tick's net liquidity cannot be read")?,
                fee_growth_outside0: big(field(3), "a tick's fee growth")?,
                fee_growth_outside1: big(field(4), "a tick's fee growth")?,
                watchers: 0,
            },
        ));
    }

    Ok(StoredSnapshot {
        pool: parsed(value, "pool")?,
        token0: parsed(value, "token0")?,
        token1: parsed(value, "token1")?,
        block: value
            .get("block")
            .and_then(Value::as_u64)
            .ok_or("`block` is missing")?,
        day: parsed(value, "day")?,
        state: PoolSnapshot {
            fee_pips: integer(value, "fee_pips")?,
            tick_spacing: integer(value, "tick_spacing")?,
            sqrt_price_x96: big(text(value, "sqrt_price_x96").ok(), "`sqrt_price_x96`")?,
            tick: integer(value, "tick")?,
            liquidity: parsed(value, "liquidity")?,
            fee_growth_global0: big(
                text(value, "fee_growth_global0").ok(),
                "`fee_growth_global0`",
            )?,
            fee_growth_global1: big(
                text(value, "fee_growth_global1").ok(),
                "`fee_growth_global1`",
            )?,
            fee_protocol0: integer(value, "fee_protocol0")?,
            fee_protocol1: integer(value, "fee_protocol1")?,
            ticks,
        },
    })
}

/// The snapshots of a data folder.
#[derive(Debug, Clone)]
pub struct SnapshotStore {
    root: PathBuf,
}

impl SnapshotStore {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    fn pool_dir(&self, chain: &str, pool: Address) -> PathBuf {
        self.root
            .join("snapshots")
            .join(format!("chain={chain}"))
            .join(format!("pool={pool:#x}"))
    }

    fn path(&self, chain: &str, pool: Address, day: Day) -> PathBuf {
        self.pool_dir(chain, pool).join(format!("day={day}.json"))
    }

    /// Whether a usable snapshot is stored for the day. A file that does
    /// not read back (empty, or cut short by a power cut) is set aside as
    /// `<name>.damaged` and does not count.
    pub fn has(&self, chain: &str, pool: Address, day: Day) -> bool {
        matches!(self.load(chain, pool, day), Ok(Some(_)))
    }

    /// The day's snapshot, `None` when none is stored or the stored one
    /// is damaged; a damaged one is set aside.
    pub fn load(
        &self,
        chain: &str,
        pool: Address,
        day: Day,
    ) -> StoreResult<Option<StoredSnapshot>> {
        let path = self.path(chain, pool, day);
        if !path.is_file() {
            return Ok(None);
        }
        match read_snapshot(&path) {
            Ok(snapshot) => Ok(Some(snapshot)),
            Err(StoreError::Unreadable { reason, .. }) => {
                crate::store::discard(&path, &reason);
                Ok(None)
            }
            Err(other) => Err(other),
        }
    }

    /// Stores a snapshot. The file appears under its name only once it is
    /// whole and on the disk.
    pub fn write(&self, chain: &str, snapshot: &StoredSnapshot) -> StoreResult<()> {
        let path = self.path(chain, snapshot.pool, snapshot.day);
        write_durably(&path, to_json(snapshot).to_string().as_bytes())
            .map_err(|source| StoreError::Io { path, source })
    }

    pub fn read(&self, chain: &str, pool: Address, day: Day) -> StoreResult<StoredSnapshot> {
        read_snapshot(&self.path(chain, pool, day))
    }

    /// Days a snapshot is stored for, oldest first.
    pub fn days(&self, chain: &str, pool: Address) -> StoreResult<Vec<Day>> {
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
            if let Some(day) = name
                .to_str()
                .and_then(|n| n.strip_prefix("day="))
                .and_then(|n| n.strip_suffix(".json"))
                .and_then(|d| d.parse::<Day>().ok())
            {
                days.push(day);
            }
        }
        days.sort_unstable();
        Ok(days)
    }

    /// The newest snapshot taken on or before `day`, the one a replay that
    /// starts on `day` rolls forward from.
    pub fn latest_on_or_before(
        &self,
        chain: &str,
        pool: Address,
        day: Day,
    ) -> StoreResult<Option<StoredSnapshot>> {
        // A damaged file is set aside and the next older one is used.
        for stored in self.days(chain, pool)?.into_iter().rev() {
            if stored > day {
                continue;
            }
            if let Some(snapshot) = self.load(chain, pool, stored)? {
                return Ok(Some(snapshot));
            }
        }
        Ok(None)
    }

    /// The oldest usable snapshot, the start of the longest replay.
    pub fn earliest(&self, chain: &str, pool: Address) -> StoreResult<Option<StoredSnapshot>> {
        for stored in self.days(chain, pool)? {
            if let Some(snapshot) = self.load(chain, pool, stored)? {
                return Ok(Some(snapshot));
            }
        }
        Ok(None)
    }
}

/// Reads one snapshot file. A file that is not a snapshot, such as an
/// empty one, is `Unreadable`, with what to do about it.
pub fn read_snapshot(path: &Path) -> StoreResult<StoredSnapshot> {
    let text = fs::read_to_string(path).map_err(|source| StoreError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    let bad = |reason: String| StoreError::Unreadable {
        path: path.to_path_buf(),
        reason,
    };
    if text.trim().is_empty() {
        return Err(bad("it is empty".into()));
    }
    let value: Value = serde_json::from_str(&text).map_err(|e| bad(e.to_string()))?;
    from_json(&value).map_err(bad)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::store::testing::TempDir;

    pub fn sample(day: &str, block: u64) -> StoredSnapshot {
        StoredSnapshot {
            pool: "0x88e6A0c2dDD26FEEb64F039a2c41296FcB3f5640"
                .parse()
                .unwrap(),
            token0: "0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48"
                .parse()
                .unwrap(),
            token1: "0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2"
                .parse()
                .unwrap(),
            block,
            day: day.parse().unwrap(),
            state: PoolSnapshot {
                fee_pips: 500,
                tick_spacing: 10,
                sqrt_price_x96: U256::MAX >> 96,
                tick: -200_311,
                liquidity: u128::MAX,
                fee_growth_global0: U256::MAX,
                fee_growth_global1: U256::from(7u8),
                fee_protocol0: 4,
                fee_protocol1: 6,
                ticks: vec![
                    (
                        -887_270,
                        TickInfo {
                            liquidity_gross: u128::MAX,
                            liquidity_net: i128::MIN,
                            fee_growth_outside0: U256::MAX,
                            fee_growth_outside1: U256::ZERO,
                            watchers: 0,
                        },
                    ),
                    (
                        200_310,
                        TickInfo {
                            liquidity_gross: 5,
                            liquidity_net: -5,
                            fee_growth_outside0: U256::from(1u8),
                            fee_growth_outside1: U256::from(2u8),
                            watchers: 0,
                        },
                    ),
                ],
            },
        }
    }

    #[test]
    fn a_snapshot_survives_a_round_trip_at_the_extremes_of_every_field() {
        let dir = TempDir::new("snapshot");
        let store = SnapshotStore::new(dir.path());
        let snapshot = sample("2026-11-01", 24_000_000);
        assert!(!store.has("ethereum", snapshot.pool, snapshot.day));
        store.write("ethereum", &snapshot).unwrap();
        assert!(store.has("ethereum", snapshot.pool, snapshot.day));
        assert_eq!(
            store.read("ethereum", snapshot.pool, snapshot.day).unwrap(),
            snapshot
        );
        assert!(dir
            .path()
            .join("snapshots/chain=ethereum/pool=0x88e6a0c2ddd26feeb64f039a2c41296fcb3f5640/day=2026-11-01.json")
            .is_file());
    }

    #[test]
    fn a_replay_gets_the_newest_snapshot_not_after_its_start() {
        let dir = TempDir::new("snapshot");
        let store = SnapshotStore::new(dir.path());
        let pool = sample("2026-11-01", 1).pool;
        let day = |text: &str| text.parse::<Day>().unwrap();
        assert_eq!(
            store
                .latest_on_or_before("ethereum", pool, day("2026-11-05"))
                .unwrap(),
            None
        );
        store.write("ethereum", &sample("2026-11-01", 100)).unwrap();
        store.write("ethereum", &sample("2026-11-10", 900)).unwrap();
        let found = |text: &str| {
            store
                .latest_on_or_before("ethereum", pool, day(text))
                .unwrap()
                .map(|s| s.block)
        };
        assert_eq!(found("2026-10-31"), None);
        assert_eq!(found("2026-11-01"), Some(100));
        assert_eq!(found("2026-11-09"), Some(100));
        assert_eq!(found("2026-11-10"), Some(900));
        assert_eq!(found("2027-01-01"), Some(900));
        assert_eq!(
            store.days("ethereum", pool).unwrap(),
            vec![day("2026-11-01"), day("2026-11-10")]
        );
    }

    #[test]
    fn a_damaged_snapshot_file_says_what_is_wrong() {
        let dir = TempDir::new("snapshot");
        let path = dir.path().join("day=2026-11-01.json");
        fs::write(&path, "{not json").unwrap();
        let error = read_snapshot(&path).unwrap_err();
        assert!(matches!(error, StoreError::Unreadable { .. }), "{error}");
        assert!(error.to_string().contains("lpbot backfill"), "{error}");

        let mut value = to_json(&sample("2026-11-01", 1));
        value["liquidity"] = json!("-1");
        fs::write(&path, value.to_string()).unwrap();
        let error = read_snapshot(&path).unwrap_err().to_string();
        assert!(error.contains("liquidity"), "{error}");

        let mut value = to_json(&sample("2026-11-01", 1));
        value["ticks"][0][3] = json!(12);
        fs::write(&path, value.to_string()).unwrap();
        let error = read_snapshot(&path).unwrap_err().to_string();
        assert!(error.contains("fee growth"), "{error}");
    }

    #[test]
    fn a_snapshot_left_empty_by_a_power_cut_counts_as_missing() {
        let dir = TempDir::new("snapshot");
        let store = SnapshotStore::new(dir.path());
        let (old, new) = (sample("2026-11-01", 100), sample("2026-11-10", 900));
        store.write("ethereum", &old).unwrap();
        store.write("ethereum", &new).unwrap();
        let path = store.path("ethereum", new.pool, new.day);
        // Only the file itself is left: no temporary file beside it.
        assert_eq!(fs::read_dir(path.parent().unwrap()).unwrap().count(), 2);

        // The newer file is empty, as an unsynced write leaves it.
        fs::write(&path, b"").unwrap();
        let read = |text: &str| {
            store
                .latest_on_or_before("ethereum", new.pool, text.parse().unwrap())
                .unwrap()
                .map(|s| s.block)
        };
        // A replay falls back on the older snapshot instead of failing.
        assert_eq!(read("2026-11-12"), Some(100));
        assert!(!path.exists());
        assert!(path.with_file_name("day=2026-11-10.json.damaged").is_file());
        assert!(!store.has("ethereum", new.pool, new.day));
        assert_eq!(store.days("ethereum", new.pool).unwrap(), vec![old.day]);

        // `has` sets a damaged file aside too, so the backfill takes it again.
        fs::write(store.path("ethereum", old.pool, old.day), b"{\"pool\":").unwrap();
        assert!(!store.has("ethereum", old.pool, old.day));
        assert_eq!(store.earliest("ethereum", old.pool).unwrap(), None);
        store.write("ethereum", &old).unwrap();
        assert_eq!(store.earliest("ethereum", old.pool).unwrap(), Some(old));
    }
}
