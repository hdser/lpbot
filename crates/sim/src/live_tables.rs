//! The live tables: what `lpbot watch-pools` keeps current in Postgres.
//!
//! Unlike the result and pool tables they are not files. The service
//! writes them straight into the database, row by row as things change,
//! each table with a key its rows are replaced by:
//!
//! | Table | One row per | Key |
//! | --- | --- | --- |
//! | `live_pools` | Pool | `pool` |
//! | `live_pool_series` | Pool and minute with activity | `pool`, `time` |
//! | `live_liquidity` | Pool, scope and bucket of prices, now | `pool`, `scope`, `tick_lower` |
//! | `live_swaps` | Swap | `pool`, `block`, `log_index` |
//! | `live_positions` | Position | `pool`, `position_id` |
//!
//! Amounts are in whole tokens, prices in the pool's price unit (the
//! dollar price of its other token, or token1 per token0 without a
//! dollar), times in Unix seconds.

use crate::tables::{float, float_or_empty, int, int_or_empty, text, Column, Table};

/// Names of the live tables, in the order the schema creates them.
pub const LIVE_TABLES: [&str; 5] = [
    "live_pools",
    "live_pool_series",
    "live_liquidity",
    "live_swaps",
    "live_positions",
];

/// The columns a live table's rows are replaced by.
pub fn live_key(table: &str) -> &'static [&'static str] {
    match table {
        "live_pools" => &["pool"],
        "live_pool_series" => &["pool", "time"],
        "live_liquidity" => &["pool", "scope", "tick_lower"],
        "live_swaps" => &["pool", "block", "log_index"],
        "live_positions" => &["pool", "position_id"],
        _ => &[],
    }
}

/// What names a pool in every live row.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct PoolLabel {
    /// The address, lower-case hex.
    pub pool: String,
    /// Such as `USDC/WETH 0.05`.
    pub name: String,
    /// What the price is in, such as `USDC per WETH`.
    pub price_unit: String,
}

/// A pool now.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct LivePoolRow {
    pub label: PoolLabel,
    pub fee_pct: f64,
    pub tick_spacing: i32,
    /// The last block applied, and its time.
    pub block: u64,
    pub time: u64,
    /// When the row was written, by the clock of the machine.
    pub updated: u64,
    /// The node's head and finalized block at the time.
    pub head_block: u64,
    pub finalized_block: u64,
    pub tick: i32,
    pub price: f64,
    /// Active liquidity at the price.
    pub liquidity: f64,
    /// What all positions hold at the price.
    pub locked0: f64,
    pub locked1: f64,
    pub tvl_usd: Option<f64>,
    /// Dollars per whole token0 and token1.
    pub usd0: Option<f64>,
    pub usd1: Option<f64>,
    pub volume_usd_1h: Option<f64>,
    pub fees_usd_1h: Option<f64>,
    pub swaps_1h: u32,
    pub volume_usd_24h: Option<f64>,
    pub fees_usd_24h: Option<f64>,
    pub swaps_24h: u32,
    /// Positions the service knows that hold liquidity, and those in range.
    pub positions_open: u32,
    pub positions_in_range: u32,
    /// Share of the active liquidity in positions the service knows.
    pub known_share: Option<f64>,
    pub symbol0: String,
    pub symbol1: String,
    /// When the service started following positions.
    pub since: u64,
    pub reorgs: u64,
    pub anomalies: u64,
}

/// A pool in one minute with activity.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct LiveSeriesRow {
    pub label: PoolLabel,
    /// Start of the minute.
    pub time: u64,
    pub block: u64,
    pub open: f64,
    pub high: f64,
    pub low: f64,
    pub close: f64,
    pub tick: i32,
    pub liquidity: f64,
    pub volume0: f64,
    pub volume1: f64,
    pub volume_usd: Option<f64>,
    pub fees_usd: Option<f64>,
    pub swaps: u32,
    pub mints: u32,
    pub burns: u32,
}

/// The liquidity in one bucket of prices now.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct LiveLiquidityRow {
    pub label: PoolLabel,
    /// `near` (around the price, a tick spacing per bucket) or `wide`.
    pub scope: String,
    pub time: u64,
    pub tick_lower: i32,
    pub tick_upper: i32,
    pub price_low: f64,
    pub price_high: f64,
    pub price_mid: f64,
    pub liquidity: f64,
    pub amount0: f64,
    pub amount1: f64,
    pub value_usd: Option<f64>,
    /// The value split by the token held: the base token (the one priced)
    /// and the quote token (the one it is priced in).
    pub volatile_usd: Option<f64>,
    pub dollar_usd: Option<f64>,
    pub at_price: bool,
}

/// One swap.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct LiveSwapRow {
    pub label: PoolLabel,
    pub block: u64,
    pub time: u64,
    pub log_index: u32,
    pub tx_hash: String,
    pub sender: String,
    pub recipient: String,
    /// What the pool received (positive) or paid out (negative).
    pub amount0: f64,
    pub amount1: f64,
    /// `buy` when the swapper took the base token out, `sell` when in.
    pub side: String,
    /// The price after the swap.
    pub price: f64,
    pub tick: i32,
    pub liquidity: f64,
    pub volume_usd: Option<f64>,
    pub fee_usd: Option<f64>,
}

/// One position: a position NFT, or a pool position of another owner.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct LivePositionRow {
    pub label: PoolLabel,
    /// `nft-<token>` for an NFT, else `<owner>-<lower>-<upper>-<block>`
    /// with the block it was opened or first seen in.
    pub position_id: String,
    /// `nft` or `pool`.
    pub kind: String,
    pub token_id: Option<u64>,
    /// The NFT's holder, or the pool position's owner.
    pub owner: String,
    pub tick_lower: i32,
    pub tick_upper: i32,
    /// The range in the price unit, low first.
    pub price_low: f64,
    pub price_high: f64,
    /// `open` while it holds liquidity, else `closed`.
    pub status: String,
    pub in_range: bool,
    pub liquidity: f64,
    /// Its share of the active liquidity, while in range.
    pub share: Option<f64>,
    /// What it holds now at the price.
    pub amount0: f64,
    pub amount1: f64,
    pub value_usd: Option<f64>,
    /// What the owner could collect now: fees and taken-out principal.
    pub owed0: f64,
    pub owed1: f64,
    pub owed_usd: Option<f64>,
    /// Fees earned since followed, up to now.
    pub fees0: f64,
    pub fees1: f64,
    pub fees_usd: Option<f64>,
    pub collected0: f64,
    pub collected1: f64,
    pub deposited0: f64,
    pub deposited1: f64,
    pub withdrawn0: f64,
    pub withdrawn1: f64,
    pub opened_block: Option<u64>,
    pub opened_time: Option<u64>,
    pub closed_block: Option<u64>,
    pub closed_time: Option<u64>,
    /// When the service started following it.
    pub since_block: u64,
    pub since_time: u64,
    /// Its last event.
    pub changed_block: u64,
    pub changed_time: u64,
    /// When the row was written.
    pub updated: u64,
}

fn labels<'a, R: 'a>(
    rows: &'a [R],
    label: impl Fn(&R) -> &PoolLabel,
) -> Vec<(&'static str, Column)> {
    vec![
        ("pool", text(rows.iter().map(|r| label(r).pool.as_str()))),
        (
            "pool_name",
            text(rows.iter().map(|r| label(r).name.as_str())),
        ),
        (
            "price_unit",
            text(rows.iter().map(|r| label(r).price_unit.as_str())),
        ),
    ]
}

fn build(name: &str, columns: Vec<(&str, Column)>) -> Table {
    Table {
        name: name.to_string(),
        columns: columns
            .into_iter()
            .map(|(n, c)| (n.to_string(), c))
            .collect(),
    }
}

fn u(v: u64) -> i64 {
    i64::try_from(v).unwrap_or(i64::MAX)
}

fn flag(values: impl IntoIterator<Item = bool>) -> Column {
    Column::Flag(values.into_iter().map(Some).collect())
}

pub fn live_pools_table(r: &[LivePoolRow]) -> Table {
    let mut c = labels(r, |r| &r.label);
    c.extend([
        ("fee_pct", float(r.iter().map(|r| r.fee_pct))),
        (
            "tick_spacing",
            int(r.iter().map(|r| i64::from(r.tick_spacing))),
        ),
        ("block", int(r.iter().map(|r| u(r.block)))),
        ("time", int(r.iter().map(|r| u(r.time)))),
        ("updated", int(r.iter().map(|r| u(r.updated)))),
        ("head_block", int(r.iter().map(|r| u(r.head_block)))),
        (
            "finalized_block",
            int(r.iter().map(|r| u(r.finalized_block))),
        ),
        ("tick", int(r.iter().map(|r| i64::from(r.tick)))),
        ("price", float(r.iter().map(|r| r.price))),
        ("liquidity", float(r.iter().map(|r| r.liquidity))),
        ("locked0", float(r.iter().map(|r| r.locked0))),
        ("locked1", float(r.iter().map(|r| r.locked1))),
        ("tvl_usd", float_or_empty(r.iter().map(|r| r.tvl_usd))),
        ("usd0", float_or_empty(r.iter().map(|r| r.usd0))),
        ("usd1", float_or_empty(r.iter().map(|r| r.usd1))),
        (
            "volume_usd_1h",
            float_or_empty(r.iter().map(|r| r.volume_usd_1h)),
        ),
        (
            "fees_usd_1h",
            float_or_empty(r.iter().map(|r| r.fees_usd_1h)),
        ),
        ("swaps_1h", int(r.iter().map(|r| i64::from(r.swaps_1h)))),
        (
            "volume_usd_24h",
            float_or_empty(r.iter().map(|r| r.volume_usd_24h)),
        ),
        (
            "fees_usd_24h",
            float_or_empty(r.iter().map(|r| r.fees_usd_24h)),
        ),
        ("swaps_24h", int(r.iter().map(|r| i64::from(r.swaps_24h)))),
        (
            "positions_open",
            int(r.iter().map(|r| i64::from(r.positions_open))),
        ),
        (
            "positions_in_range",
            int(r.iter().map(|r| i64::from(r.positions_in_range))),
        ),
        (
            "known_share",
            float_or_empty(r.iter().map(|r| r.known_share)),
        ),
        ("symbol0", text(r.iter().map(|r| r.symbol0.as_str()))),
        ("symbol1", text(r.iter().map(|r| r.symbol1.as_str()))),
        ("since", int(r.iter().map(|r| u(r.since)))),
        ("reorgs", int(r.iter().map(|r| u(r.reorgs)))),
        ("anomalies", int(r.iter().map(|r| u(r.anomalies)))),
    ]);
    build("live_pools", c)
}

pub fn live_series_table(r: &[LiveSeriesRow]) -> Table {
    let mut c = labels(r, |r| &r.label);
    c.extend([
        ("time", int(r.iter().map(|r| u(r.time)))),
        ("block", int(r.iter().map(|r| u(r.block)))),
        ("open", float(r.iter().map(|r| r.open))),
        ("high", float(r.iter().map(|r| r.high))),
        ("low", float(r.iter().map(|r| r.low))),
        ("close", float(r.iter().map(|r| r.close))),
        ("tick", int(r.iter().map(|r| i64::from(r.tick)))),
        ("liquidity", float(r.iter().map(|r| r.liquidity))),
        ("volume0", float(r.iter().map(|r| r.volume0))),
        ("volume1", float(r.iter().map(|r| r.volume1))),
        ("volume_usd", float_or_empty(r.iter().map(|r| r.volume_usd))),
        ("fees_usd", float_or_empty(r.iter().map(|r| r.fees_usd))),
        ("swaps", int(r.iter().map(|r| i64::from(r.swaps)))),
        ("mints", int(r.iter().map(|r| i64::from(r.mints)))),
        ("burns", int(r.iter().map(|r| i64::from(r.burns)))),
    ]);
    build("live_pool_series", c)
}

pub fn live_liquidity_table(r: &[LiveLiquidityRow]) -> Table {
    let mut c = labels(r, |r| &r.label);
    c.extend([
        ("scope", text(r.iter().map(|r| r.scope.as_str()))),
        ("time", int(r.iter().map(|r| u(r.time)))),
        ("tick_lower", int(r.iter().map(|r| i64::from(r.tick_lower)))),
        ("tick_upper", int(r.iter().map(|r| i64::from(r.tick_upper)))),
        ("price_low", float(r.iter().map(|r| r.price_low))),
        ("price_high", float(r.iter().map(|r| r.price_high))),
        ("price_mid", float(r.iter().map(|r| r.price_mid))),
        ("liquidity", float(r.iter().map(|r| r.liquidity))),
        ("amount0", float(r.iter().map(|r| r.amount0))),
        ("amount1", float(r.iter().map(|r| r.amount1))),
        ("value_usd", float_or_empty(r.iter().map(|r| r.value_usd))),
        (
            "volatile_usd",
            float_or_empty(r.iter().map(|r| r.volatile_usd)),
        ),
        ("dollar_usd", float_or_empty(r.iter().map(|r| r.dollar_usd))),
        ("at_price", flag(r.iter().map(|r| r.at_price))),
    ]);
    build("live_liquidity", c)
}

pub fn live_swaps_table(r: &[LiveSwapRow]) -> Table {
    let mut c = labels(r, |r| &r.label);
    c.extend([
        ("block", int(r.iter().map(|r| u(r.block)))),
        ("time", int(r.iter().map(|r| u(r.time)))),
        ("log_index", int(r.iter().map(|r| i64::from(r.log_index)))),
        ("tx_hash", text(r.iter().map(|r| r.tx_hash.as_str()))),
        ("sender", text(r.iter().map(|r| r.sender.as_str()))),
        ("recipient", text(r.iter().map(|r| r.recipient.as_str()))),
        ("amount0", float(r.iter().map(|r| r.amount0))),
        ("amount1", float(r.iter().map(|r| r.amount1))),
        ("side", text(r.iter().map(|r| r.side.as_str()))),
        ("price", float(r.iter().map(|r| r.price))),
        ("tick", int(r.iter().map(|r| i64::from(r.tick)))),
        ("liquidity", float(r.iter().map(|r| r.liquidity))),
        ("volume_usd", float_or_empty(r.iter().map(|r| r.volume_usd))),
        ("fee_usd", float_or_empty(r.iter().map(|r| r.fee_usd))),
    ]);
    build("live_swaps", c)
}

pub fn live_positions_table(r: &[LivePositionRow]) -> Table {
    let mut c = labels(r, |r| &r.label);
    let opt = |f: fn(&LivePositionRow) -> Option<u64>| {
        int_or_empty(r.iter().map(move |row| f(row).map(u)))
    };
    c.extend([
        (
            "position_id",
            text(r.iter().map(|r| r.position_id.as_str())),
        ),
        ("kind", text(r.iter().map(|r| r.kind.as_str()))),
        ("token_id", opt(|r| r.token_id)),
        ("owner", text(r.iter().map(|r| r.owner.as_str()))),
        ("tick_lower", int(r.iter().map(|r| i64::from(r.tick_lower)))),
        ("tick_upper", int(r.iter().map(|r| i64::from(r.tick_upper)))),
        ("price_low", float(r.iter().map(|r| r.price_low))),
        ("price_high", float(r.iter().map(|r| r.price_high))),
        ("status", text(r.iter().map(|r| r.status.as_str()))),
        ("in_range", flag(r.iter().map(|r| r.in_range))),
        ("liquidity", float(r.iter().map(|r| r.liquidity))),
        ("share", float_or_empty(r.iter().map(|r| r.share))),
        ("amount0", float(r.iter().map(|r| r.amount0))),
        ("amount1", float(r.iter().map(|r| r.amount1))),
        ("value_usd", float_or_empty(r.iter().map(|r| r.value_usd))),
        ("owed0", float(r.iter().map(|r| r.owed0))),
        ("owed1", float(r.iter().map(|r| r.owed1))),
        ("owed_usd", float_or_empty(r.iter().map(|r| r.owed_usd))),
        ("fees0", float(r.iter().map(|r| r.fees0))),
        ("fees1", float(r.iter().map(|r| r.fees1))),
        ("fees_usd", float_or_empty(r.iter().map(|r| r.fees_usd))),
        ("collected0", float(r.iter().map(|r| r.collected0))),
        ("collected1", float(r.iter().map(|r| r.collected1))),
        ("deposited0", float(r.iter().map(|r| r.deposited0))),
        ("deposited1", float(r.iter().map(|r| r.deposited1))),
        ("withdrawn0", float(r.iter().map(|r| r.withdrawn0))),
        ("withdrawn1", float(r.iter().map(|r| r.withdrawn1))),
        ("opened_block", opt(|r| r.opened_block)),
        ("opened_time", opt(|r| r.opened_time)),
        ("closed_block", opt(|r| r.closed_block)),
        ("closed_time", opt(|r| r.closed_time)),
        ("since_block", int(r.iter().map(|r| u(r.since_block)))),
        ("since_time", int(r.iter().map(|r| u(r.since_time)))),
        ("changed_block", int(r.iter().map(|r| u(r.changed_block)))),
        ("changed_time", int(r.iter().map(|r| u(r.changed_time)))),
        ("updated", int(r.iter().map(|r| u(r.updated)))),
    ]);
    build("live_positions", c)
}

/// The five live tables, empty, in the order of `LIVE_TABLES`: for the
/// schema.
pub fn empty_live_tables() -> Vec<Table> {
    vec![
        live_pools_table(&[]),
        live_series_table(&[]),
        live_liquidity_table(&[]),
        live_swaps_table(&[]),
        live_positions_table(&[]),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_live_table_has_its_name_its_key_and_rows_of_one_length() {
        let tables = empty_live_tables();
        assert!(tables.iter().map(|t| t.name.as_str()).eq(LIVE_TABLES));
        for table in &tables {
            assert_eq!(table.columns[0].0, "pool");
            for column in live_key(&table.name) {
                assert!(table.column(column).is_some(), "{}: {column}", table.name);
            }
        }
        assert!(live_key("runs").is_empty());

        let label = PoolLabel {
            pool: "0xabc".into(),
            name: "USDC/WETH 0.05".into(),
            price_unit: "USDC per WETH".into(),
        };
        let rows = vec![
            LivePositionRow {
                label: label.clone(),
                position_id: "nft-7".into(),
                token_id: Some(7),
                opened_block: Some(10),
                ..Default::default()
            },
            LivePositionRow {
                label,
                position_id: "0x1-0-60-12".into(),
                ..Default::default()
            },
        ];
        let table = live_positions_table(&rows);
        assert_eq!(table.rows(), 2);
        assert!(table.columns.iter().all(|(_, c)| c.len() == 2));
        assert_eq!(
            table.column("token_id"),
            Some(&Column::Int(vec![Some(7), None]))
        );
        assert_eq!(
            table.column("pool_name"),
            Some(&Column::Text(vec![
                Some("USDC/WETH 0.05".into()),
                Some("USDC/WETH 0.05".into())
            ]))
        );
    }
}
