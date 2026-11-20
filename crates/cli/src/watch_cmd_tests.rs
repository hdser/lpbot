//! The live rows from a state: prices in dollars, positions, swaps, bars
//! and the liquidity around the price, and the plan of a replay.

use std::collections::VecDeque;

use alloy_primitives::{Address, B256, U256};
use lpbot_data::live::{Bar, LivePool, LiveState, SwapRecord};
use lpbot_data::positions::{Ledger, PoolKey, PositionView, Range, Stamp};
use lpbot_data::snapshot::SnapshotStore;
use lpbot_data::store::{EventStore, StoredLog};
use lpbot_venues::uniswap_v3::tick_math::get_sqrt_ratio_at_tick;

use super::*;

fn config() -> Config {
    crate::chain_cmd::load_base_config(Path::new(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../config/mainnet.yml"
    )))
    .unwrap()
}

fn token(symbol: &str) -> Address {
    let (_, tokens) = crate::chain_cmd::chain_addresses(&config(), "ethereum").unwrap();
    tokens[symbol].0
}

/// The tick where token1 per token0, in smallest units, is `raw`.
fn tick_of(raw: f64) -> i32 {
    (raw.ln() / 1.0001f64.ln()).round() as i32
}

/// A pool at a raw price, with `liquidity` minted around it.
fn pool_at(fee: u32, spacing: i32, raw: f64, liquidity: u128) -> Pool {
    let mut pool = Pool::new(fee, spacing);
    let tick = tick_of(raw);
    pool.initialize(get_sqrt_ratio_at_tick(tick).unwrap())
        .unwrap();
    if liquidity > 0 {
        let lower = (tick - 50 * spacing).div_euclid(spacing) * spacing;
        let upper = (tick + 50 * spacing).div_euclid(spacing) * spacing;
        pool.mint(lower, upper, liquidity).unwrap();
    }
    pool
}

fn address(byte: u8) -> Address {
    Address::repeat_byte(byte)
}

/// USDC/WETH 0.05 with ETH at $2,000, WBTC/WETH 0.3 at 30 ETH per BTC, and
/// USDC/USDT 0.01 at par.
fn state() -> LiveState {
    let mut pools = std::collections::BTreeMap::new();
    let mut add = |pool: Address, token0: &str, token1: &str, inner: Pool| {
        pools.insert(
            pool,
            LivePool {
                address: pool,
                token0: token(token0),
                token1: token(token1),
                pool: inner,
                bars: VecDeque::new(),
            },
        );
    };
    // WETH per USDC in smallest units: 1/2000 × 10^18 / 10^6.
    add(
        address(1),
        "USDC",
        "WETH",
        pool_at(500, 10, 1e12 / 2_000.0, 10u128.pow(18)),
    );
    // WETH per WBTC in smallest units: 30 × 10^18 / 10^8.
    add(
        address(2),
        "WBTC",
        "WETH",
        pool_at(3_000, 60, 30.0 * 1e10, 10u128.pow(15)),
    );
    add(
        address(3),
        "USDC",
        "USDT",
        pool_at(100, 1, 1.0, 10u128.pow(12)),
    );
    let mut state = LiveState::from_snapshots(&[], 3_600);
    state.pools = pools;
    state.at = Stamp {
        block: 1_000,
        time: 1_700_000_000,
    };
    state
}

fn close(a: f64, b: f64, tolerance: f64) -> bool {
    (a - b).abs() <= tolerance * b.abs().max(1e-12)
}

#[test]
fn pools_are_priced_in_dollars_directly_or_through_a_reference_pool() {
    let state = state();
    let pricing = Pricing::new(&config(), "ethereum", &state).unwrap();
    let (usdc, weth) = pricing.usd(&state, address(1)).unwrap();
    assert!(close(usdc, 1.0, 1e-9));
    assert!(close(weth, 2_000.0, 1e-3), "{weth}");
    // WBTC/WETH has no stable: WETH's dollars come from USDC/WETH.
    let (wbtc, weth_too) = pricing.usd(&state, address(2)).unwrap();
    assert!(close(weth_too, weth, 1e-9));
    assert!(close(wbtc, 60_000.0, 1e-3), "{wbtc}");
    let (a, b) = pricing.usd(&state, address(3)).unwrap();
    assert!(close(a, 1.0, 1e-3) && close(b, 1.0, 1e-9));
    assert_eq!(pricing.label(address(1)).name, "USDC/WETH 0.05");
    assert_eq!(pricing.label(address(2)).price_unit, "WETH per WBTC");
    assert!(pricing.stable_pair(address(3)) && !pricing.stable_pair(address(1)));
}

fn view(pool: &Pool, owner: Address, lower: i32, upper: i32, liquidity: u128) -> PositionView {
    PositionView {
        id: PositionId::Nft(42),
        range: Range {
            pool: address(1),
            tick_lower: lower,
            tick_upper: upper,
        },
        owner,
        ledger: Ledger {
            liquidity,
            last_inside: pool.fee_growth_inside(lower, upper),
            mark: pool.fee_growth_inside(lower, upper),
            owed: (U256::from(2_000_000u64), U256::ZERO),
            opened: Some(Stamp {
                block: 900,
                time: 1_699_990_000,
            }),
            since: Stamp {
                block: 900,
                time: 1_699_990_000,
            },
            ..Ledger::default()
        },
    }
}

#[test]
fn a_position_row_holds_amounts_at_the_price_and_its_range_in_dollars() {
    let state = state();
    let pricing = Pricing::new(&config(), "ethereum", &state).unwrap();
    let pool = &state.pools[&address(1)].pool;
    let (lower, upper) = (
        (pool.tick() - 100).div_euclid(10) * 10,
        (pool.tick() + 100).div_euclid(10) * 10,
    );
    let usd = pricing.usd(&state, address(1));
    let v = view(pool, address(0x77), lower, upper, 10u128.pow(17));
    let row = position_row(
        pricing.label(address(1)),
        pricing.quote(address(1)),
        usd,
        pool,
        &v,
        5,
    );
    assert_eq!(row.position_id, "nft-42");
    assert_eq!((row.kind.as_str(), row.token_id), ("nft", Some(42)));
    assert_eq!(row.status, "open");
    assert!(row.in_range);
    assert!(row.amount0 > 0.0 && row.amount1 > 0.0);
    let (u0, u1) = usd.unwrap();
    assert!(close(
        row.value_usd.unwrap(),
        row.amount0 * u0 + row.amount1 * u1,
        1e-12
    ));
    // The range in dollars per ETH, low first, around $2,000.
    assert!(row.price_low < 2_000.0 && 2_000.0 < row.price_high);
    // Owed: 2 USDC, nothing pending since its last touch.
    assert!(close(row.owed0, 2.0, 1e-12));
    assert_eq!(row.fees0, 0.0);
    assert!(close(row.share.unwrap(), 0.1, 1e-9), "{:?}", row.share);
    assert_eq!(row.opened_block, Some(900));
    assert_eq!(row.owner, format!("{:#x}", address(0x77)));

    // Out of range, and once emptied: no share, nothing held.
    let above = view(
        pool,
        address(0x77),
        upper + 1_000,
        upper + 2_000,
        10u128.pow(17),
    );
    let row = position_row(
        pricing.label(address(1)),
        pricing.quote(address(1)),
        usd,
        pool,
        &above,
        5,
    );
    assert!(!row.in_range && row.share.is_none());
    let empty = view(pool, address(0x77), lower, upper, 0);
    let row = position_row(
        pricing.label(address(1)),
        pricing.quote(address(1)),
        usd,
        pool,
        &empty,
        5,
    );
    assert_eq!(row.status, "closed");
    assert_eq!((row.amount0, row.amount1), (0.0, 0.0));
}

#[test]
fn a_pool_position_is_named_by_owner_range_and_opening_block() {
    let state = state();
    let pool = &state.pools[&address(1)].pool;
    let mut v = view(pool, address(0xAB), -60, 60, 1);
    v.id = PositionId::Pool(PoolKey {
        owner: address(0xAB),
        range: v.range,
    });
    assert_eq!(
        position_id(&v),
        format!("{:#x}-{}-{}-900", address(0xAB), -60, 60)
    );
    v.ledger.opened = None;
    v.ledger.since.block = 777;
    assert!(position_id(&v).ends_with("-777"));
}

#[test]
fn swaps_say_which_way_they_went_and_what_they_were_worth() {
    let state = state();
    let pricing = Pricing::new(&config(), "ethereum", &state).unwrap();
    let usd = pricing.usd(&state, address(1));
    // 2,000 USDC in, 1 WETH out: the swapper bought ETH.
    let swap = SwapRecord {
        pool: address(1),
        block: 1_000,
        time: 1_700_000_000,
        log_index: 3,
        tx_hash: B256::repeat_byte(9),
        sender: address(5),
        recipient: address(6),
        amount0: 2_000e6,
        amount1: -1e18,
        price: 1e12 / 2_000.0,
        tick: 0,
        liquidity: 1.0,
    };
    let row = swap_row(
        pricing.label(address(1)),
        pricing.quote(address(1)),
        usd,
        500,
        &swap,
    );
    assert_eq!(row.side, "buy");
    assert!(close(row.amount0, 2_000.0, 1e-12) && close(row.amount1, -1.0, 1e-12));
    assert!(close(row.volume_usd.unwrap(), 2_000.0, 1e-9));
    assert!(close(row.fee_usd.unwrap(), 1.0, 1e-9));
    assert!(close(row.price, 2_000.0, 1e-9));
    // The other way: WETH in, USDC out.
    let sell = SwapRecord {
        amount0: -2_000e6,
        amount1: 1e18,
        ..swap
    };
    let row = swap_row(
        pricing.label(address(1)),
        pricing.quote(address(1)),
        usd,
        500,
        &sell,
    );
    assert_eq!(row.side, "sell");
    assert!(close(row.fee_usd.unwrap(), 1.0, 1e-3));
}

#[test]
fn a_minute_row_reads_high_and_low_in_the_price_unit() {
    let state = state();
    let pricing = Pricing::new(&config(), "ethereum", &state).unwrap();
    // In raw terms the price is WETH per USDC, so a higher raw price is a
    // cheaper ETH: high and low swap places in dollars.
    let bar = Bar {
        minute: 1_700_000_000 - 20,
        block: 999,
        open: 1e12 / 2_000.0,
        high: 1e12 / 1_990.0,
        low: 1e12 / 2_010.0,
        close: 1e12 / 2_005.0,
        volume0: 4_000e6,
        volume1: 2e18,
        fees0: 1e6,
        fees1: 0.0,
        swaps: 2,
        ..Bar::default()
    };
    let row = series_row(
        pricing.label(address(1)),
        pricing.quote(address(1)),
        pricing.usd(&state, address(1)),
        &bar,
    );
    assert!(close(row.high, 2_010.0, 1e-9) && close(row.low, 1_990.0, 1e-9));
    assert!(close(row.close, 2_005.0, 1e-9));
    assert!(close(row.volume_usd.unwrap(), 4_000.0, 1e-9));
    assert!(close(row.fees_usd.unwrap(), 1.0, 1e-9));
    assert_eq!(row.time, 1_700_000_000 - 20);
}

#[test]
fn the_liquidity_around_the_price_comes_in_a_wide_and_a_near_scope() {
    let state = state();
    let pricing = Pricing::new(&config(), "ethereum", &state).unwrap();
    for pool in [address(1), address(2), address(3)] {
        let lp = &state.pools[&pool];
        let rows = liquidity_now(
            pricing.label(pool),
            pricing.quote(pool),
            pricing.usd(&state, pool),
            &lp.pool,
            pricing.stable_pair(pool),
            7,
        );
        for scope in ["wide", "near"] {
            let of: Vec<&LiveLiquidityRow> = rows.iter().filter(|r| r.scope == scope).collect();
            assert!(of.len() > 3, "{scope}: {} rows", of.len());
            assert_eq!(of.iter().filter(|r| r.at_price).count(), 1, "{scope}");
            assert!(
                of.iter().any(|r| r.value_usd.unwrap_or(0.0) > 0.0),
                "{scope}"
            );
        }
        let near_width = rows
            .iter()
            .find(|r| r.scope == "near")
            .map(|r| r.tick_upper - r.tick_lower)
            .unwrap();
        assert_eq!(near_width, lp.pool.tick_spacing());
    }
}

#[test]
fn the_last_row_of_each_key_wins() {
    let rows = vec![(1, "a"), (2, "b"), (1, "c")];
    assert_eq!(last_by(rows, |r| r.0.to_string()), vec![(2, "b"), (1, "c")]);
}

#[test]
fn a_replay_starts_at_each_pools_snapshot_and_ends_on_the_last_common_day() {
    let dir = std::env::temp_dir().join(format!("lpbot-watch-plan-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let state = state();
    let days: Vec<Day> = (0..4)
        .map(|i| Day::from_ymd(2026, 11, 10 + i).unwrap())
        .collect();
    let snapshots = SnapshotStore::new(&dir);
    let events = EventStore::new(&dir);
    let log = StoredLog {
        block_number: 1,
        block_time: days[0].start(),
        log_index: 0,
        tx_hash: B256::ZERO,
        topics: vec![],
        data: vec![],
    };
    for (pool, stored_days) in [(address(1), 0..4), (address(2), 1..3)] {
        let first = stored_days.start;
        snapshots
            .write(
                "ethereum",
                &StoredSnapshot {
                    pool,
                    token0: state.pools[&pool].token0,
                    token1: state.pools[&pool].token1,
                    block: 1,
                    day: days[first],
                    state: state.pools[&pool].pool.snapshot(),
                },
            )
            .unwrap();
        for i in stored_days {
            events
                .write_day("ethereum", pool, days[i], std::slice::from_ref(&log), true)
                .unwrap();
        }
    }
    let planned = plan(&dir, "ethereum", &[], None).unwrap();
    assert_eq!(planned.len(), 2);
    assert_eq!(planned[0].day, days[0]);
    assert_eq!(planned[1].day, days[1]);
    assert_eq!(
        last_common_day(&dir, "ethereum", &planned).unwrap(),
        days[2]
    );
    // A day missing in the middle ends the history there.
    std::fs::remove_dir_all(
        dir.join("events/chain=ethereum")
            .join(format!("pool={:#x}", address(1)))
            .join(format!("day={}", days[1])),
    )
    .unwrap();
    assert_eq!(
        last_common_day(&dir, "ethereum", &planned).unwrap(),
        days[0]
    );
    assert!(plan(&dir, "ethereum", &[address(9)], None).is_err());
    let _ = std::fs::remove_dir_all(&dir);
}
