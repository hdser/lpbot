//! `lpbot watch-pools`: the followed pools, kept current in Postgres from
//! the local node, block by block.
//!
//! It starts from the checkpoint of its last run, or, the first time and
//! with `--rebuild`, from the stored history: every pool's snapshot and
//! stored days, the position manager's days beside them (fetched and
//! stored first where missing). Then it follows the node's head, applies
//! each block, takes back blocks a reorg drops, and writes the live
//! tables at a short cadence. A checkpoint of the finalized state is
//! saved every few minutes, so a restart carries on from there.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use alloy_primitives::{Address, U256};
use anyhow::{anyhow, bail, Context, Result};
use lpbot_core::Config;
use lpbot_data::backfill::{AtHead, NodeSource};
use lpbot_data::day::Day;
use lpbot_data::lake::stored_pools;
use lpbot_data::live::{
    load_checkpoint, raw_price, replay_history, save_checkpoint, Bar, Follower, LiveError,
    LiveState, NodeHead, Step, SwapRecord,
};
use lpbot_data::manager::{first_missing_day, store_manager_days};
use lpbot_data::pool_stats::{amounts, bucket_points, segments, BucketPoint};
use lpbot_data::positions::{PositionId, PositionView};
use lpbot_data::snapshot::{SnapshotStore, StoredSnapshot};
use lpbot_data::store::{DayState, EventStore};
use lpbot_sim::live_db::{delete_above, delete_below, delete_pools, ensure_live_schema, upsert};
use lpbot_sim::live_tables::{
    live_liquidity_table, live_pools_table, live_positions_table, live_series_table,
    live_swaps_table, LiveLiquidityRow, LivePoolRow, LivePositionRow, LiveSeriesRow, LiveSwapRow,
    PoolLabel,
};
use lpbot_sim::LIVE_TABLES;
use lpbot_venues::uniswap_v3::pool::Pool;

use crate::chain_cmd::{safe_head, Chain, HEAD_MARGIN};
use crate::pool_stats_cmd::{
    liquidity_rows, quote_for, reference_pool, ticks_for, DollarSide, Quote,
};

/// What `watch-pools` was asked to do.
pub struct WatchArgs<'a> {
    pub lake: &'a Path,
    pub db_url: &'a str,
    /// Pools to follow; empty means every stored pool.
    pub pools: &'a [Address],
    /// Follow positions from this day; the earliest snapshot when `None`.
    pub from: Option<Day>,
    pub state_file: PathBuf,
    pub rebuild: bool,
    pub poll: Duration,
    pub every: Duration,
    pub refresh: Duration,
    pub checkpoint_every: Duration,
    /// Seconds of swaps and minute rows the tables keep.
    pub keep: u64,
    /// Seconds closed positions stay in the table.
    pub keep_closed: u64,
    pub threads: usize,
    /// Stop once at the head, after one write.
    pub once: bool,
}

/// Seconds of minute bars kept in memory: a day for the 24-hour figures,
/// and some.
const BARS_IN_MEMORY: u64 = 26 * 3_600;

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

fn f(x: U256) -> f64 {
    x.to_string().parse().unwrap_or(f64::INFINITY)
}

// --- prices and labels ----------------------------------------------------

/// How each followed pool's figures become whole tokens and dollars.
pub struct Pricing {
    quotes: BTreeMap<Address, Quote>,
    labels: BTreeMap<Address, PoolLabel>,
    fees: BTreeMap<Address, u32>,
    /// For a pool without a dollar stable: the pool its dollars come from.
    references: BTreeMap<Address, Address>,
    stables: Vec<String>,
}

impl Pricing {
    pub fn new(config: &Config, chain: &str, state: &LiveState) -> Result<Self> {
        let mut quotes = BTreeMap::new();
        let mut labels = BTreeMap::new();
        let mut fees = BTreeMap::new();
        for (address, lp) in &state.pools {
            let quote = quote_for(config, chain, lp.token0, lp.token1)?;
            let fee = lp.pool.fee_pips();
            labels.insert(
                *address,
                PoolLabel {
                    pool: format!("{address:#x}"),
                    name: format!(
                        "{}/{} {}",
                        quote.symbol0,
                        quote.symbol1,
                        f64::from(fee) / 10_000.0
                    ),
                    price_unit: quote.price_unit(),
                },
            );
            fees.insert(*address, fee);
            quotes.insert(*address, quote);
        }
        let stables = config.universe.usd_stables.clone();
        let candidates: Vec<(Address, Quote, u32)> = quotes
            .iter()
            .map(|(a, q)| (*a, q.clone(), fees[a]))
            .collect();
        let references = quotes
            .iter()
            .filter(|(_, q)| q.dollar.is_none())
            .filter_map(|(a, q)| reference_pool(q, &candidates, &stables).map(|r| (*a, r.0)))
            .collect();
        Ok(Self {
            quotes,
            labels,
            fees,
            references,
            stables,
        })
    }

    fn quote(&self, pool: Address) -> &Quote {
        &self.quotes[&pool]
    }

    fn label(&self, pool: Address) -> &PoolLabel {
        &self.labels[&pool]
    }

    /// Whether both of a pool's tokens are dollar stables.
    fn stable_pair(&self, pool: Address) -> bool {
        let q = self.quote(pool);
        self.stables.contains(&q.symbol0) && self.stables.contains(&q.symbol1)
    }

    /// Dollars per whole token0 and token1 of a pool now: from its own
    /// dollar stable, or from its reference pool's price.
    pub fn usd(&self, state: &LiveState, pool: Address) -> Option<(f64, f64)> {
        let quote = self.quotes.get(&pool)?;
        let raw = raw_price(&state.pools.get(&pool)?.pool);
        if quote.dollar.is_some() {
            return quote.dollars_per(raw, 0);
        }
        let reference = *self.references.get(&pool)?;
        let reference_quote = self.quotes.get(&reference)?;
        let usd = reference_quote.price(raw_price(&state.pools.get(&reference)?.pool));
        let symbol = reference_quote.volatile_symbol()?;
        let p = quote.one_per_zero(raw);
        if symbol == quote.symbol1 {
            Some((usd * p, usd))
        } else if symbol == quote.symbol0 {
            Some((usd, usd / p))
        } else {
            None
        }
    }
}

// --- rows -------------------------------------------------------------------

fn price_range(quote: &Quote, lower: i32, upper: i32) -> (f64, f64) {
    let at = |tick: i32| quote.price(1.0001f64.powi(tick));
    let (a, b) = (at(lower), at(upper));
    (a.min(b), a.max(b))
}

/// What names a position in the table: its NFT, or its owner, range and
/// the block its liquidity came in (or the book first saw it).
pub fn position_id(view: &PositionView) -> String {
    match view.id {
        PositionId::Nft(token) => format!("nft-{token}"),
        PositionId::Pool(key) => format!(
            "{:#x}-{}-{}-{}",
            key.owner,
            key.range.tick_lower,
            key.range.tick_upper,
            view.ledger
                .opened
                .map_or(view.ledger.since.block, |o| o.block)
        ),
    }
}

/// A position as its row shows it, at the pool's state now.
pub fn position_row(
    label: &PoolLabel,
    quote: &Quote,
    usd: Option<(f64, f64)>,
    pool: &Pool,
    view: &PositionView,
    updated: u64,
) -> LivePositionRow {
    let ledger = &view.ledger;
    let (lower, upper) = (view.range.tick_lower, view.range.tick_upper);
    let (pending_owed, pending_fees) = if ledger.liquidity > 0 {
        ledger.pending(pool.fee_growth_inside(lower, upper))
    } else {
        Default::default()
    };
    let w0 = |x: U256| quote.whole0(f(x));
    let w1 = |x: U256| quote.whole1(f(x));
    let owed = (
        w0(ledger.owed.0.saturating_add(pending_owed.0)),
        w1(ledger.owed.1.saturating_add(pending_owed.1)),
    );
    let fees = (
        w0(ledger.fees.0.saturating_add(pending_fees.0)),
        w1(ledger.fees.1.saturating_add(pending_fees.1)),
    );
    let sqrt = raw_price(pool).sqrt();
    let (held0, held1) = amounts(ledger.liquidity as f64, lower, upper, sqrt);
    let (held0, held1) = (quote.whole0(held0), quote.whole1(held1));
    let value = |a: (f64, f64)| usd.map(|(u0, u1)| a.0 * u0 + a.1 * u1);
    let in_range = ledger.liquidity > 0 && lower <= pool.tick() && pool.tick() < upper;
    let (price_low, price_high) = price_range(quote, lower, upper);
    LivePositionRow {
        label: label.clone(),
        position_id: position_id(view),
        kind: match view.id {
            PositionId::Nft(_) => "nft".into(),
            PositionId::Pool(_) => "pool".into(),
        },
        token_id: match view.id {
            PositionId::Nft(token) => Some(token),
            PositionId::Pool(_) => None,
        },
        owner: format!("{:#x}", view.owner),
        tick_lower: lower,
        tick_upper: upper,
        price_low,
        price_high,
        status: if ledger.liquidity > 0 {
            "open"
        } else {
            "closed"
        }
        .into(),
        in_range,
        liquidity: ledger.liquidity as f64,
        share: (in_range && pool.liquidity() > 0)
            .then(|| ledger.liquidity as f64 / pool.liquidity() as f64),
        amount0: held0,
        amount1: held1,
        value_usd: value((held0, held1)),
        owed0: owed.0,
        owed1: owed.1,
        owed_usd: value(owed),
        fees0: fees.0,
        fees1: fees.1,
        fees_usd: value(fees),
        collected0: w0(ledger.collected.0),
        collected1: w1(ledger.collected.1),
        deposited0: w0(ledger.deposited.0),
        deposited1: w1(ledger.deposited.1),
        withdrawn0: w0(ledger.withdrawn.0),
        withdrawn1: w1(ledger.withdrawn.1),
        opened_block: ledger.opened.map(|o| o.block),
        opened_time: ledger.opened.map(|o| o.time),
        closed_block: ledger.closed.map(|c| c.block),
        closed_time: ledger.closed.map(|c| c.time),
        since_block: ledger.since.block,
        since_time: ledger.since.time,
        changed_block: ledger.changed.block,
        changed_time: ledger.changed.time,
        updated,
    }
}

/// A minute bar in the pool's price unit and dollars.
pub fn series_row(
    label: &PoolLabel,
    quote: &Quote,
    usd: Option<(f64, f64)>,
    bar: &Bar,
) -> LiveSeriesRow {
    let (a, b) = (quote.price(bar.high), quote.price(bar.low));
    let (volume0, volume1) = (quote.whole0(bar.volume0), quote.whole1(bar.volume1));
    LiveSeriesRow {
        label: label.clone(),
        time: bar.minute,
        block: bar.block,
        open: quote.price(bar.open),
        high: a.max(b),
        low: a.min(b),
        close: quote.price(bar.close),
        tick: bar.tick,
        liquidity: bar.liquidity,
        volume0,
        volume1,
        volume_usd: usd.map(|(u0, u1)| match quote.quote_side() {
            DollarSide::Zero => volume0 * u0,
            DollarSide::One => volume1 * u1,
        }),
        fees_usd: usd.map(|(u0, u1)| quote.whole0(bar.fees0) * u0 + quote.whole1(bar.fees1) * u1),
        swaps: bar.swaps,
        mints: bar.mints,
        burns: bar.burns,
    }
}

/// A swap in whole tokens, the pool's price unit and dollars.
pub fn swap_row(
    label: &PoolLabel,
    quote: &Quote,
    usd: Option<(f64, f64)>,
    fee_pips: u32,
    swap: &SwapRecord,
) -> LiveSwapRow {
    let (a0, a1) = (quote.whole0(swap.amount0), quote.whole1(swap.amount1));
    // The base token is the one priced: token0 when token1 is the quote.
    let base_is_zero = quote.quote_side() == DollarSide::One;
    let bought = if base_is_zero { a0 < 0.0 } else { a1 < 0.0 };
    let fee = f64::from(fee_pips) / 1_000_000.0;
    LiveSwapRow {
        label: label.clone(),
        block: swap.block,
        time: swap.time,
        log_index: swap.log_index,
        tx_hash: format!("{:#x}", swap.tx_hash),
        sender: format!("{:#x}", swap.sender),
        recipient: format!("{:#x}", swap.recipient),
        amount0: a0,
        amount1: a1,
        side: if bought { "buy" } else { "sell" }.into(),
        price: quote.price(swap.price),
        tick: swap.tick,
        liquidity: swap.liquidity,
        volume_usd: usd.map(|(u0, u1)| {
            if base_is_zero {
                a1.abs() * u1
            } else {
                a0.abs() * u0
            }
        }),
        fee_usd: usd.map(|(u0, u1)| {
            if a0 > 0.0 {
                a0 * fee * u0
            } else {
                a1.max(0.0) * fee * u1
            }
        }),
    }
}

/// The liquidity around the price now, in two scopes: `wide` (20% either
/// side in 0.5% buckets; closer for stablecoin pairs) and `near` (a tenth
/// of that span, one tick spacing per bucket).
pub fn liquidity_now(
    label: &PoolLabel,
    quote: &Quote,
    usd_reference: Option<(f64, f64)>,
    pool: &Pool,
    stable_pair: bool,
    time: u64,
) -> Vec<LiveLiquidityRow> {
    let (span_pct, bucket_pct) = if stable_pair {
        (0.5, 0.01)
    } else {
        (20.0, 0.5)
    };
    let spacing = pool.tick_spacing().max(1);
    let round = |ticks: i32| (ticks + spacing - 1).div_euclid(spacing) * spacing;
    let wide = (round(ticks_for(bucket_pct)), ticks_for(span_pct));
    let near = (spacing, (ticks_for(span_pct) / 10).max(spacing * 3));
    // Without a dollar stable, the rows are priced through the reference
    // given; `liquidity_rows` wants it as a quote with a reference price.
    let mut quote = quote.clone();
    if quote.dollar.is_none() {
        quote.reference = usd_reference.map(|(u0, u1)| {
            let side = if quote.quote_side() == DollarSide::One {
                DollarSide::One
            } else {
                DollarSide::Zero
            };
            crate::pool_stats_cmd::Reference {
                side,
                usd: vec![(0, if side == DollarSide::One { u1 } else { u0 })],
                from: String::new(),
            }
        });
    }
    let mut out = Vec::new();
    for (scope, (width, span)) in [("wide", wide), ("near", near)] {
        let buckets: Vec<BucketPoint> = bucket_points(pool, time, width, span);
        for row in liquidity_rows(&quote, &buckets) {
            out.push(LiveLiquidityRow {
                label: label.clone(),
                scope: scope.into(),
                time,
                tick_lower: row.tick_lower,
                tick_upper: row.tick_upper,
                price_low: row.price_low,
                price_high: row.price_high,
                price_mid: row.price_mid,
                liquidity: row.liquidity,
                amount0: row.amount0,
                amount1: row.amount1,
                value_usd: row.value_usd,
                volatile_usd: row.volatile_usd,
                dollar_usd: row.dollar_usd,
                at_price: row.at_price,
            });
        }
    }
    out
}

/// Counts of a pool's shown positions.
#[derive(Debug, Clone, Copy, Default)]
struct Counts {
    open: u32,
    in_range: u32,
}

// --- writing ----------------------------------------------------------------

/// What one write puts in the database.
#[derive(Default)]
struct Batch {
    pools: Vec<LivePoolRow>,
    series: Vec<LiveSeriesRow>,
    liquidity: Vec<LiveLiquidityRow>,
    /// Pools whose liquidity rows are replaced.
    liquidity_pools: Vec<String>,
    swaps: Vec<LiveSwapRow>,
    positions: Vec<LivePositionRow>,
    /// After a reorg: the block both chains share.
    fork: Option<u64>,
}

/// Keeps the last row of each key.
fn last_by<T>(rows: Vec<T>, key: impl Fn(&T) -> String) -> Vec<T> {
    let mut seen = BTreeMap::new();
    for (i, row) in rows.iter().enumerate() {
        seen.insert(key(row), i);
    }
    let keep: BTreeSet<usize> = seen.into_values().collect();
    rows.into_iter()
        .enumerate()
        .filter(|(i, _)| keep.contains(i))
        .map(|(_, r)| r)
        .collect()
}

struct Writer {
    url: String,
    client: Option<postgres::Client>,
    /// Swaps not yet written, kept across a failed write.
    pending_swaps: Vec<LiveSwapRow>,
    /// Rows of positions that finished, not yet written: they have left
    /// the book, so a full write would not find them again.
    pending_finished: Vec<LivePositionRow>,
    /// The tick of each pool at the last refresh of positions in range.
    last_ticks: HashMap<Address, i32>,
    last_refresh: Option<Instant>,
    /// Write everything at the next write: after a reorg, a failed write,
    /// or at the start.
    full: bool,
    last_retention: Option<Instant>,
}

impl Writer {
    fn new(url: &str) -> Self {
        Self {
            url: url.to_string(),
            client: None,
            pending_swaps: Vec::new(),
            pending_finished: Vec::new(),
            last_ticks: HashMap::new(),
            last_refresh: None,
            full: true,
            last_retention: None,
        }
    }

    fn client(&mut self) -> Result<&mut postgres::Client> {
        if self.client.is_none() {
            let mut client = crate::load_cmd::connect(&self.url)?;
            ensure_live_schema(&mut client)?;
            self.client = Some(client);
        }
        Ok(self.client.as_mut().expect("just connected"))
    }

    fn commit(
        &mut self,
        batch: Batch,
        pools: &[String],
        retention: Option<(u64, u64)>,
    ) -> Result<usize> {
        let client = self.client()?;
        let mut tx = client.transaction()?;
        if let Some(fork) = batch.fork {
            let fork = i64::try_from(fork).unwrap_or(i64::MAX);
            delete_above(&mut tx, "live_swaps", "block", fork)?;
            delete_above(&mut tx, "live_positions", "opened_block", fork)?;
            delete_above(&mut tx, "live_pool_series", "block", fork)?;
        }
        let mut rows = 0;
        rows += upsert(&mut tx, &live_pools_table(&batch.pools))?;
        rows += upsert(
            &mut tx,
            &live_series_table(&last_by(batch.series, |r| {
                format!("{}{}", r.label.pool, r.time)
            })),
        )?;
        if !batch.liquidity_pools.is_empty() {
            delete_pools(&mut tx, "live_liquidity", &batch.liquidity_pools)?;
            rows += upsert(&mut tx, &live_liquidity_table(&batch.liquidity))?;
        }
        rows += upsert(
            &mut tx,
            &live_swaps_table(&last_by(batch.swaps, |r| {
                format!("{}{}-{}", r.label.pool, r.block, r.log_index)
            })),
        )?;
        rows += upsert(
            &mut tx,
            &live_positions_table(&last_by(batch.positions, |r| {
                format!("{}{}", r.label.pool, r.position_id)
            })),
        )?;
        if let Some((swaps_before, closed_before)) = retention {
            let (swaps_before, closed_before) = (
                i64::try_from(swaps_before).unwrap_or(0),
                i64::try_from(closed_before).unwrap_or(0),
            );
            delete_below(&mut tx, "live_swaps", "time", swaps_before)?;
            delete_below(&mut tx, "live_pool_series", "time", swaps_before)?;
            tx.execute(
                "DELETE FROM live_positions WHERE status = 'closed' AND closed_time < $1 \
                 AND pool = ANY($2)",
                &[&closed_before, &pools],
            )?;
        }
        tx.commit()?;
        Ok(rows)
    }

    /// Writes what changed in the live state. On failure nothing is lost
    /// for good: the next write writes everything again.
    fn write(
        &mut self,
        follower: &mut Follower,
        pricing: &Pricing,
        args: &WatchArgs,
        fork: Option<u64>,
    ) -> Result<usize> {
        let updated = now();
        let state = &mut follower.live;
        let out = state.take_outbox();
        let (changed, finished) = state.book.take_changes();
        let (anomalies, anomaly_count) = state.book.take_anomalies();
        for anomaly in anomalies.iter().take(5) {
            println!("  note: block {}: {}", anomaly.at.block, anomaly.what);
        }
        if fork.is_some() {
            self.full = true;
        }
        let full = std::mem::take(&mut self.full);
        let refresh = full
            || self
                .last_refresh
                .is_none_or(|t| t.elapsed() >= args.refresh);
        let state = &follower.live;
        let usd: BTreeMap<Address, Option<(f64, f64)>> = state
            .pools
            .keys()
            .map(|a| (*a, pricing.usd(state, *a)))
            .collect();

        let mut batch = Batch {
            fork,
            ..Batch::default()
        };
        // Positions: those that changed or finished, and on a refresh every
        // one whose range the price was in or passed through since the last.
        let row_of = |view: &PositionView| {
            let pool = view.range.pool;
            position_row(
                pricing.label(pool),
                pricing.quote(pool),
                usd[&pool],
                &state.pools[&pool].pool,
                view,
                updated,
            )
        };
        let mut counts: BTreeMap<Address, Counts> = BTreeMap::new();
        let mut written: BTreeSet<PositionId> = BTreeSet::new();
        for view in &finished {
            self.pending_finished.push(row_of(view));
            written.insert(view.id);
        }
        for view in &changed {
            batch.positions.push(row_of(view));
            written.insert(view.id);
        }
        for view in state.book.shown() {
            let pool = view.range.pool;
            let Some(lp) = state.pools.get(&pool) else {
                continue;
            };
            let tick = lp.pool.tick();
            let count = counts.entry(pool).or_default();
            if view.ledger.liquidity > 0 {
                count.open += 1;
                if view.range.tick_lower <= tick && tick < view.range.tick_upper {
                    count.in_range += 1;
                }
            }
            if written.contains(&view.id) {
                continue;
            }
            let last = self.last_ticks.get(&pool).copied().unwrap_or(tick);
            let (low, high) = (last.min(tick), last.max(tick));
            let passed = view.range.tick_lower <= high && low < view.range.tick_upper;
            if full || (refresh && passed && view.ledger.liquidity > 0) {
                batch.positions.push(row_of(&view));
            }
        }
        if refresh {
            self.last_refresh = Some(Instant::now());
            for (address, lp) in &state.pools {
                self.last_ticks.insert(*address, lp.pool.tick());
            }
        }

        // Minute rows, swaps, the liquidity around the price.
        let bars: Vec<(Address, Bar)> = if full {
            state
                .pools
                .iter()
                .flat_map(|(a, lp)| lp.bars.iter().map(move |b| (*a, b.clone())))
                .collect()
        } else {
            out.bars
                .iter()
                .filter_map(|(pool, minute)| state.bar(*pool, *minute).map(|b| (*pool, b.clone())))
                .collect()
        };
        for (pool, bar) in &bars {
            batch.series.push(series_row(
                pricing.label(*pool),
                pricing.quote(*pool),
                usd[pool],
                bar,
            ));
        }
        for swap in &out.swaps {
            self.pending_swaps.push(swap_row(
                pricing.label(swap.pool),
                pricing.quote(swap.pool),
                usd[&swap.pool],
                pricing.fees[&swap.pool],
                swap,
            ));
        }
        let changed_pools: Vec<Address> = if full || refresh {
            state.pools.keys().copied().collect()
        } else {
            out.pools.iter().copied().collect()
        };
        for pool in &changed_pools {
            let lp = &state.pools[pool];
            batch.liquidity.extend(liquidity_now(
                pricing.label(*pool),
                pricing.quote(*pool),
                usd[pool],
                &lp.pool,
                pricing.stable_pair(*pool),
                state.at.time,
            ));
            batch
                .liquidity_pools
                .push(pricing.label(*pool).pool.clone());
        }

        // Every pool's row.
        for (address, lp) in &state.pools {
            let quote = pricing.quote(*address);
            let usd = usd[address];
            let sqrt = raw_price(&lp.pool).sqrt();
            let (mut locked0, mut locked1) = (0.0, 0.0);
            for (lower, upper, liquidity) in segments(&lp.pool) {
                let (a0, a1) = amounts(liquidity, lower, upper, sqrt);
                locked0 += a0;
                locked1 += a1;
            }
            let (locked0, locked1) = (quote.whole0(locked0), quote.whole1(locked1));
            let dollars = |flow: lpbot_data::live::Flow| {
                usd.map(|(u0, u1)| {
                    let (v0, v1) = (quote.whole0(flow.volume0), quote.whole1(flow.volume1));
                    let volume = match quote.quote_side() {
                        DollarSide::Zero => v0 * u0,
                        DollarSide::One => v1 * u1,
                    };
                    let fees = quote.whole0(flow.fees0) * u0 + quote.whole1(flow.fees1) * u1;
                    (volume, fees)
                })
            };
            let hour = state.flow(*address, state.at.time.saturating_sub(3_600));
            let day = state.flow(*address, state.at.time.saturating_sub(86_400));
            let active = lp.pool.liquidity();
            let known = state.book.active_liquidity(*address, lp.pool.tick());
            let count = counts.get(address).copied().unwrap_or_default();
            batch.pools.push(LivePoolRow {
                label: pricing.label(*address).clone(),
                fee_pct: f64::from(lp.pool.fee_pips()) / 10_000.0,
                tick_spacing: lp.pool.tick_spacing(),
                block: state.at.block,
                time: state.at.time,
                updated,
                head_block: follower.head,
                finalized_block: follower.finalized,
                tick: lp.pool.tick(),
                price: quote.price(raw_price(&lp.pool)),
                liquidity: active as f64,
                locked0,
                locked1,
                tvl_usd: usd.map(|(u0, u1)| locked0 * u0 + locked1 * u1),
                usd0: usd.map(|u| u.0),
                usd1: usd.map(|u| u.1),
                volume_usd_1h: dollars(hour).map(|d| d.0),
                fees_usd_1h: dollars(hour).map(|d| d.1),
                swaps_1h: hour.swaps,
                volume_usd_24h: dollars(day).map(|d| d.0),
                fees_usd_24h: dollars(day).map(|d| d.1),
                swaps_24h: day.swaps,
                positions_open: count.open,
                positions_in_range: count.in_range,
                known_share: (active > 0).then(|| (known as f64 / active as f64).min(1.0)),
                symbol0: quote.symbol0.clone(),
                symbol1: quote.symbol1.clone(),
                since: state.since.time,
                reorgs: follower.reorgs,
                anomalies: anomaly_count,
            });
        }

        let retention = self
            .last_retention
            .is_none_or(|t| t.elapsed() >= Duration::from_secs(3_600))
            .then(|| {
                (
                    updated.saturating_sub(args.keep),
                    updated.saturating_sub(args.keep_closed),
                )
            });
        batch.swaps = std::mem::take(&mut self.pending_swaps);
        let pools: Vec<String> = state
            .pools
            .keys()
            .map(|a| pricing.label(*a).pool.clone())
            .collect();
        let swaps_kept = batch.swaps.clone();
        // Finished positions go first, so a position that finished and
        // opened again since keeps its newer row.
        let finished_kept = std::mem::take(&mut self.pending_finished);
        let mut positions = finished_kept.clone();
        positions.append(&mut batch.positions);
        batch.positions = positions;
        match self.commit(batch, &pools, retention) {
            Ok(rows) => {
                if retention.is_some() {
                    self.last_retention = Some(Instant::now());
                }
                Ok(rows)
            }
            Err(error) => {
                // Write it all again next time, on a fresh connection.
                self.client = None;
                self.full = true;
                self.pending_swaps = swaps_kept;
                self.pending_swaps.truncate(200_000);
                self.pending_finished = finished_kept;
                self.pending_finished.truncate(200_000);
                Err(error)
            }
        }
    }

    /// Writes rows of the replay of the history: a day's minute rows,
    /// swaps and the positions that finished.
    fn write_rows(
        &mut self,
        series: Vec<LiveSeriesRow>,
        swaps: Vec<LiveSwapRow>,
        positions: Vec<LivePositionRow>,
    ) -> Result<usize> {
        let client = self.client()?;
        let mut tx = client.transaction()?;
        let mut n = upsert(
            &mut tx,
            &live_series_table(&last_by(series, |r| format!("{}{}", r.label.pool, r.time))),
        )?;
        n += upsert(
            &mut tx,
            &live_swaps_table(&last_by(swaps, |r| {
                format!("{}{}-{}", r.label.pool, r.block, r.log_index)
            })),
        )?;
        n += upsert(
            &mut tx,
            &live_positions_table(&last_by(positions, |r| {
                format!("{}{}", r.label.pool, r.position_id)
            })),
        )?;
        tx.commit()?;
        Ok(n)
    }

    /// Removes the rows of these pools from every live table.
    fn clear(&mut self, pools: &[String], above: Option<u64>) -> Result<()> {
        let client = self.client()?;
        let mut tx = client.transaction()?;
        match above {
            None => {
                for table in LIVE_TABLES {
                    delete_pools(&mut tx, table, pools)?;
                }
            }
            Some(block) => {
                let block = i64::try_from(block).unwrap_or(i64::MAX);
                delete_above(&mut tx, "live_swaps", "block", block)?;
                delete_above(&mut tx, "live_positions", "opened_block", block)?;
                delete_above(&mut tx, "live_pool_series", "block", block)?;
            }
        }
        tx.commit()?;
        Ok(())
    }
}

// --- starting -----------------------------------------------------------------

/// The pools to follow, each with the snapshot its replay starts from.
fn plan(
    lake: &Path,
    chain: &str,
    asked: &[Address],
    from: Option<Day>,
) -> Result<Vec<StoredSnapshot>> {
    let pools = if asked.is_empty() {
        stored_pools(lake, chain)?
    } else {
        asked.to_vec()
    };
    if pools.is_empty() {
        bail!(
            "no pool history under {}; store some with `lpbot backfill` first",
            lake.display()
        );
    }
    let snapshots = SnapshotStore::new(lake);
    pools
        .iter()
        .map(|pool| {
            let found = match from {
                Some(day) => snapshots
                    .latest_on_or_before(chain, *pool, day)?
                    .or(snapshots.earliest(chain, *pool)?),
                None => snapshots.earliest(chain, *pool)?,
            };
            found.ok_or_else(|| {
                anyhow!("pool {pool:#x} has no stored snapshot; run `lpbot backfill` first")
            })
        })
        .collect()
}

/// The last day every pool has stored complete, with every day from its
/// snapshot on.
fn last_common_day(lake: &Path, chain: &str, snapshots: &[StoredSnapshot]) -> Result<Day> {
    let events = EventStore::new(lake);
    let mut last: Option<Day> = None;
    for snapshot in snapshots {
        let days = events.days(chain, snapshot.pool)?;
        let mut day = snapshot.day;
        let complete: BTreeSet<Day> = days
            .iter()
            .filter(|(_, s)| *s == DayState::Complete)
            .map(|(d, _)| *d)
            .collect();
        while complete.contains(&day) {
            day = day.next();
        }
        if day == snapshot.day {
            bail!(
                "pool {:#x} has no complete day stored from {}; run `lpbot backfill` first",
                snapshot.pool,
                snapshot.day
            );
        }
        let pool_last = Day(day.0 - 1);
        last = Some(last.map_or(pool_last, |l| l.min(pool_last)));
    }
    last.ok_or_else(|| anyhow!("no pools to follow"))
}

/// Replays the stored history into a state at its last stored day.
fn rebuild(
    config: &Config,
    chain: &Chain,
    args: &WatchArgs,
    writer: &mut Writer,
) -> Result<LiveState> {
    let snapshots = plan(args.lake, &chain.name, args.pools, args.from)?;
    let to = last_common_day(args.lake, &chain.name, &snapshots)?;
    let first = snapshots.iter().map(|s| s.day).min().expect("pools");
    println!(
        "rebuilding from the stored history: {} pool(s), positions from {first} to {to}",
        snapshots.len()
    );

    // The position manager's days, where missing.
    if let Some(missing) = first_missing_day(args.lake, &chain.name, first, to) {
        let head = safe_head(&chain.client)?;
        let node = NodeSource {
            client: &chain.client,
            head_margin: HEAD_MARGIN,
            log_chunk: 10_000,
        };
        let source = AtHead {
            source: &node,
            block: head.block,
            time: head.time,
        };
        println!("storing the position manager's logs from {missing} to {to}");
        let started = Instant::now();
        let report = store_manager_days(
            &source,
            args.lake,
            &chain.name,
            missing,
            to,
            args.threads,
            |day, n| {
                if day.0 % 10 == 0 {
                    println!("  {day}: {n} logs ({:.0?} so far)", started.elapsed());
                }
            },
        )?;
        println!(
            "  {} day(s) stored, {} already there",
            report.written.len(),
            report.skipped
        );
    }

    let labels: Vec<String> = snapshots.iter().map(|s| format!("{:#x}", s.pool)).collect();
    writer.clear(&labels, None)?;

    let mut state = LiveState::from_snapshots(&snapshots, BARS_IN_MEMORY);
    state.record_from = now().saturating_sub(args.keep);
    state.set_record(true);
    let pricing = Pricing::new(config, &chain.name, &state)?;
    let source = NodeHead {
        client: &chain.client,
        threads: args.threads,
    };
    let closed_from = now().saturating_sub(args.keep_closed);
    let started = Instant::now();
    let mut finished_rows = 0;
    let state = replay_history(
        args.lake,
        &chain.name,
        &snapshots,
        to,
        state,
        &source,
        |day, state| {
            // What finished, and the minute rows and swaps of the days the
            // tables keep, priced at the day's end.
            let (_, finished) = state.book.take_changes();
            let out = state.take_outbox();
            let updated = now();
            let usd = |pool: Address| pricing.usd(state, pool);
            let positions: Vec<LivePositionRow> = finished
                .iter()
                .filter(|v| v.ledger.closed.is_none_or(|c| c.time >= closed_from))
                .map(|v| {
                    let pool = v.range.pool;
                    position_row(
                        pricing.label(pool),
                        pricing.quote(pool),
                        usd(pool),
                        &state.pools[&pool].pool,
                        v,
                        updated,
                    )
                })
                .collect();
            let series: Vec<LiveSeriesRow> = out
                .bars
                .iter()
                .filter_map(|(pool, minute)| {
                    let bar = state.bar(*pool, *minute)?;
                    Some(series_row(
                        pricing.label(*pool),
                        pricing.quote(*pool),
                        usd(*pool),
                        bar,
                    ))
                })
                .collect();
            let swaps: Vec<LiveSwapRow> = out
                .swaps
                .iter()
                .map(|swap| {
                    swap_row(
                        pricing.label(swap.pool),
                        pricing.quote(swap.pool),
                        usd(swap.pool),
                        pricing.fees[&swap.pool],
                        swap,
                    )
                })
                .collect();
            finished_rows += positions.len();
            writer
                .write_rows(series, swaps, positions)
                .map_err(|e| LiveError::Other(format!("{e:#}")))?;
            let (direct, nfts) = state.book.len();
            println!(
                "  {day}: block {}, {direct} pool positions and {nfts} NFTs held, \
                 {finished_rows} closed rows written ({:.0?})",
                state.at.block,
                started.elapsed()
            );
            Ok(())
        },
    )?;
    Ok(state)
}

// --- running ------------------------------------------------------------------

pub fn run(config: &Config, chain: &Chain, args: &WatchArgs) -> Result<()> {
    let mut writer = Writer::new(args.db_url);
    writer
        .client()
        .context("the live tables need the database")?;

    let loaded = if args.rebuild {
        None
    } else {
        load_checkpoint(&args.state_file, &chain.name)?
    };
    let (state, hash) = match loaded {
        Some((state, hash)) => {
            let wanted: BTreeSet<Address> = plan(args.lake, &chain.name, args.pools, args.from)?
                .iter()
                .map(|s| s.pool)
                .collect();
            let have: BTreeSet<Address> = state.pools.keys().copied().collect();
            if wanted != have {
                bail!(
                    "the checkpoint {} follows other pools ({} against {} asked); start with \
                     --rebuild to follow these",
                    args.state_file.display(),
                    have.len(),
                    wanted.len()
                );
            }
            println!(
                "carrying on from the checkpoint at block {} ({})",
                state.at.block,
                args.state_file.display()
            );
            // Rows of blocks after it may be of blocks a reorg took back
            // while the service was down: they are written again.
            let labels: Vec<String> = have.iter().map(|a| format!("{a:#x}")).collect();
            writer.clear(&labels, Some(state.at.block))?;
            (state, hash)
        }
        None => {
            let state = rebuild(config, chain, args, &mut writer)?;
            save_checkpoint(&args.state_file, &chain.name, &state, None)?;
            (state, None)
        }
    };

    let pricing = Pricing::new(config, &chain.name, &state)?;
    let mut follower = Follower::new(state, hash);
    follower.live.record_from = now().saturating_sub(args.keep);
    let source = NodeHead {
        client: &chain.client,
        threads: args.threads,
    };
    println!(
        "following {} pool(s) from block {}; writing every {:?}",
        follower.live.pools.len(),
        follower.live.at.block,
        args.every
    );

    let mut last_write: Option<Instant> = None;
    let mut last_checkpoint = Instant::now();
    let mut checkpoint_block = follower.base.at.block;
    let mut last_status = Instant::now();
    let mut fork: Option<u64> = None;
    let mut failures = 0u32;
    let mut wrote_at_head = false;
    loop {
        let step = match follower.step(&source) {
            Ok(step) => {
                failures = 0;
                step
            }
            Err(LiveError::Chain(error)) => {
                failures += 1;
                println!("  the node did not answer ({error}); trying again");
                std::thread::sleep(args.poll * failures.min(10));
                continue;
            }
            Err(error) => return Err(error.into()),
        };
        let behind = follower.head.saturating_sub(follower.live.at.block);
        match &step {
            Step::Reorg { fork: at, dropped } => {
                println!("  reorg: {dropped} block(s) taken back, the chains share block {at}");
                fork = Some(fork.map_or(*at, |f| f.min(*at)));
            }
            Step::Retry(why) => println!("  {why}; reading again"),
            Step::Advanced { .. } | Step::Idle => {}
        }
        let due = last_write.is_none_or(|t| t.elapsed() >= args.every);
        let caught_up = behind == 0;
        if (due || fork.is_some()) && !matches!(step, Step::Retry(_)) {
            match writer.write(&mut follower, &pricing, args, fork) {
                Ok(_) => {
                    fork = None;
                    last_write = Some(Instant::now());
                    wrote_at_head |= caught_up;
                }
                Err(error) => println!("  could not write to the database ({error:#}); will again"),
            }
        }
        if follower.base.at.block > checkpoint_block
            && last_checkpoint.elapsed() >= args.checkpoint_every
        {
            save_checkpoint(
                &args.state_file,
                &chain.name,
                &follower.base,
                follower.base_hash,
            )?;
            checkpoint_block = follower.base.at.block;
            last_checkpoint = Instant::now();
        }
        if last_status.elapsed() >= Duration::from_secs(60) || (args.once && caught_up) {
            let (direct, nfts) = follower.live.book.len();
            println!(
                "  block {} ({} behind the head, {} past the finalized), {direct} pool \
                 positions and {nfts} NFTs held, {} reorg(s)",
                follower.live.at.block,
                behind,
                follower.tail_len(),
                follower.reorgs
            );
            last_status = Instant::now();
        }
        if args.once && wrote_at_head {
            save_checkpoint(
                &args.state_file,
                &chain.name,
                &follower.base,
                follower.base_hash,
            )?;
            return Ok(());
        }
        if caught_up || matches!(step, Step::Retry(_)) {
            std::thread::sleep(args.poll);
        }
    }
}

#[cfg(test)]
#[path = "watch_cmd_tests.rs"]
mod tests;
