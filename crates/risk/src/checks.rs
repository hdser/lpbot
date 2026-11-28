//! The pre-trade and infrastructure checks a live action passes before it
//! is sent (`docs/DESIGN.md`, "Risk engine and limits"; `docs/PLAN.md`,
//! piece 4). Each is a pure function on what the trader read from the
//! chain, so each can be tested on its own. A check can only let an action
//! through, put it off, shrink it or refuse it; none makes an action
//! bigger. Exits pass every check that is not about whether the chain can
//! be trusted: a position is never kept open because gas is dear.

use std::collections::{BTreeMap, VecDeque};

/// What a check says about an action.
#[derive(Debug, Clone, PartialEq)]
pub enum Verdict {
    Allow,
    /// Not now; try again later (at the next block or decision).
    Defer(String),
    /// Not at all, for the reason given.
    Veto(String),
}

impl Verdict {
    pub fn allowed(&self) -> bool {
        matches!(self, Verdict::Allow)
    }

    /// The first verdict that is not `Allow`, else `Allow`.
    pub fn all(verdicts: impl IntoIterator<Item = Verdict>) -> Verdict {
        verdicts
            .into_iter()
            .find(|v| !v.allowed())
            .unwrap_or(Verdict::Allow)
    }
}

/// Whether an action adds exposure or takes it away.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// Opens or moves a position.
    Entry,
    /// Takes a position out and back to the value token.
    Exit,
}

/// The narrowest range a position may have, in ticks.
pub fn check_width(tick_lower: i32, tick_upper: i32, min_ticks: i32) -> Verdict {
    let width = tick_upper.saturating_sub(tick_lower);
    if width < min_ticks {
        Verdict::Veto(format!(
            "the range [{tick_lower}, {tick_upper}) is {width} ticks wide, under the {min_ticks} \
             the limits allow (risk.min_width_ticks)"
        ))
    } else {
        Verdict::Allow
    }
}

/// The pool's price against its own time-weighted price and, when there is
/// one, against another pool of the same pair: a pool pushed away from both
/// is not traded until it comes back. Ticks of pools of the same pair are
/// comparable whatever their fee tier.
pub fn check_price(
    tick: i32,
    twap_tick: i32,
    other_pool_tick: Option<i32>,
    max_ticks: i32,
) -> Verdict {
    let off_twap = (i64::from(tick) - i64::from(twap_tick)).abs();
    if off_twap > i64::from(max_ticks) {
        return Verdict::Defer(format!(
            "the pool's tick {tick} is {off_twap} ticks from its time-weighted tick {twap_tick}, \
             more than the {max_ticks} allowed (risk.max_price_gap_ticks); waiting for it to \
             come back"
        ));
    }
    if let Some(other) = other_pool_tick {
        let off_other = (i64::from(tick) - i64::from(other)).abs();
        if off_other > i64::from(max_ticks) {
            return Verdict::Defer(format!(
                "the pool's tick {tick} is {off_other} ticks from the other pool of the pair \
                 ({other}), more than the {max_ticks} allowed; waiting for them to agree"
            ));
        }
    }
    Verdict::Allow
}

/// How far a swap's output falls short of the amount in at the pool's
/// price before the swap, in parts per million. `price` is token1 per
/// token0 in raw units. A swap that gets more than the mid price counts as
/// no impact.
pub fn swap_impact_ppm(zero_for_one: bool, amount_in: f64, amount_out: f64, price: f64) -> f64 {
    let fair = if zero_for_one {
        amount_in * price
    } else {
        amount_in / price
    };
    if !(fair.is_finite() && fair > 0.0 && amount_out.is_finite()) {
        return f64::INFINITY;
    }
    ((fair - amount_out) / fair * 1e6).max(0.0)
}

/// A swap whose impact, fee included, is over `max_ppm` is refused: it
/// would trade at a price the limits do not accept. Exits pass up to
/// `exit_max_ppm`, which is looser, since staying in costs more.
pub fn check_swap(impact_ppm: f64, kind: Kind, max_ppm: u32, exit_max_ppm: u32) -> Verdict {
    let limit = match kind {
        Kind::Entry => max_ppm,
        Kind::Exit => exit_max_ppm,
    };
    if impact_ppm.is_finite() && impact_ppm <= f64::from(limit) {
        Verdict::Allow
    } else {
        Verdict::Veto(format!(
            "the swap gives up {:.0} ppm against the pool's price, over the {limit} allowed \
             (risk.max_swap_impact_ppm{})",
            impact_ppm,
            if kind == Kind::Exit {
                ", for exits"
            } else {
                ""
            }
        ))
    }
}

/// The slippage bound the planner puts on a swap or a mint: it must not be
/// looser than the limits allow.
pub fn check_slippage(tolerance_ppm: u32, max_ppm: u32) -> Verdict {
    if tolerance_ppm == 0 || tolerance_ppm > max_ppm {
        Verdict::Veto(format!(
            "a slippage tolerance of {tolerance_ppm} ppm is outside 1 to {max_ppm} \
             (risk.max_slippage_ppm)"
        ))
    } else {
        Verdict::Allow
    }
}

/// Gas: an entry waits for a block cheaper than the cap; an exit is never
/// held back by it.
pub fn check_gas(max_fee_per_gas_wei: u128, cap_wei: u128, kind: Kind) -> Verdict {
    if kind == Kind::Exit || max_fee_per_gas_wei <= cap_wei {
        Verdict::Allow
    } else {
        Verdict::Defer(format!(
            "gas at {:.2} gwei is over the {:.2} gwei cap for entries (risk.max_gas_gwei)",
            max_fee_per_gas_wei as f64 / 1e9,
            cap_wei as f64 / 1e9
        ))
    }
}

/// Changes made to each pool's positions over the last day, to cap how
/// often a pool is touched. Exits are counted but never refused.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ActionLog {
    times: BTreeMap<String, VecDeque<u64>>,
}

impl ActionLog {
    pub fn new() -> Self {
        Self::default()
    }

    /// Records an action on `pool` at `now`.
    pub fn record(&mut self, pool: &str, now: u64) {
        self.times
            .entry(pool.to_string())
            .or_default()
            .push_back(now);
    }

    /// Actions on `pool` in the day before `now`.
    pub fn in_last_day(&mut self, pool: &str, now: u64) -> usize {
        let Some(times) = self.times.get_mut(pool) else {
            return 0;
        };
        while times.front().is_some_and(|t| *t + 86_400 <= now) {
            times.pop_front();
        }
        times.len()
    }

    pub fn check(&mut self, pool: &str, now: u64, max_per_day: usize, kind: Kind) -> Verdict {
        let done = self.in_last_day(pool, now);
        if kind == Kind::Exit || done < max_per_day {
            Verdict::Allow
        } else {
            Verdict::Defer(format!(
                "{done} change(s) to pool {pool} in the last day, the most the limits allow \
                 (risk.max_actions_per_pool_day)"
            ))
        }
    }
}

/// What the trader may do, given how far the local node is from the
/// public head.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodeMode {
    Normal,
    /// The node is behind: no new entries, exits only, through a public
    /// endpoint if need be.
    ExitOnly,
}

pub fn node_mode(local_head: u64, public_head: Option<u64>, max_lag_blocks: u64) -> NodeMode {
    match public_head {
        Some(public) if public > local_head + max_lag_blocks => NodeMode::ExitOnly,
        _ => NodeMode::Normal,
    }
}

/// The pool's state as the replica holds it and as the chain answers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PoolState {
    pub sqrt_price_x96: [u8; 32],
    pub tick: i32,
    pub liquidity: u128,
}

/// A replica that differs from the chain halts the pool: every decision on
/// it would be made on a state that is not there.
pub fn check_replica(replica: &PoolState, chain: &PoolState) -> Verdict {
    if replica == chain {
        return Verdict::Allow;
    }
    let mut differ = Vec::new();
    if replica.sqrt_price_x96 != chain.sqrt_price_x96 {
        differ.push("price");
    }
    if replica.tick != chain.tick {
        differ.push("tick");
    }
    if replica.liquidity != chain.liquidity {
        differ.push("liquidity");
    }
    Verdict::Veto(format!(
        "the pool replica differs from the chain ({}): the pool is halted until `lpbot \
         verify-pool` passes again",
        differ.join(", ")
    ))
}

/// How much each sleeve may hold, in dollars, under caps per pool and in
/// total: each request is shrunk in proportion until both hold. Sizing
/// only: the amounts themselves are worked out in integers from the
/// shares this returns.
pub fn fit_sleeves(
    requests: &[(String, f64)],
    max_per_pool_usd: f64,
    max_total_usd: f64,
) -> Vec<f64> {
    let mut sizes: Vec<f64> = requests.iter().map(|(_, usd)| usd.max(0.0)).collect();
    let mut per_pool: BTreeMap<&str, f64> = BTreeMap::new();
    for ((pool, _), size) in requests.iter().zip(&sizes) {
        *per_pool.entry(pool.as_str()).or_default() += size;
    }
    for ((pool, _), size) in requests.iter().zip(sizes.iter_mut()) {
        let total = per_pool[pool.as_str()];
        if total > max_per_pool_usd && total > 0.0 {
            *size *= max_per_pool_usd / total;
        }
    }
    let total: f64 = sizes.iter().sum();
    if total > max_total_usd && total > 0.0 {
        let scale = max_total_usd / total;
        sizes.iter_mut().for_each(|s| *s *= scale);
    }
    sizes
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_first_check_that_fails_decides() {
        assert_eq!(
            Verdict::all([Verdict::Allow, Verdict::Allow]),
            Verdict::Allow
        );
        let first = Verdict::Defer("gas".into());
        assert_eq!(
            Verdict::all([Verdict::Allow, first.clone(), Verdict::Veto("x".into())]),
            first
        );
        assert!(!first.allowed());
    }

    #[test]
    fn a_range_narrower_than_the_limit_is_refused() {
        assert!(check_width(-600, 0, 600).allowed());
        let refused = check_width(-60, 0, 120);
        assert!(matches!(&refused, Verdict::Veto(r) if r.contains("60 ticks wide")));
        assert!(
            !check_width(10, 0, 1).allowed(),
            "an inverted range is refused"
        );
    }

    #[test]
    fn a_pool_pushed_away_from_its_own_average_or_its_twin_waits() {
        assert!(check_price(100, 90, Some(110), 20).allowed());
        assert!(
            matches!(check_price(100, 50, None, 20), Verdict::Defer(r) if r.contains("50 ticks"))
        );
        assert!(
            matches!(check_price(100, 95, Some(160), 20), Verdict::Defer(r) if r.contains("other pool"))
        );
        assert!(
            check_price(i32::MAX, i32::MIN, None, 20) != Verdict::Allow,
            "no overflow"
        );
    }

    #[test]
    fn swap_impact_is_measured_against_the_price_before_the_swap() {
        // 1 token0 at 2,000 token1 each, 1,990 out: 0.5% = 5,000 ppm.
        let impact = swap_impact_ppm(true, 1.0, 1_990.0, 2_000.0);
        assert!((impact - 5_000.0).abs() < 1e-6);
        // The other way: 2,000 token1 for 0.99 token0 is 1%.
        assert!((swap_impact_ppm(false, 2_000.0, 0.99, 2_000.0) - 10_000.0).abs() < 1e-6);
        assert_eq!(swap_impact_ppm(true, 1.0, 2_100.0, 2_000.0), 0.0);
        assert_eq!(swap_impact_ppm(true, 0.0, 0.0, 2_000.0), f64::INFINITY);
        // Entries are held to the tighter limit, exits to the looser one.
        assert!(check_swap(4_000.0, Kind::Entry, 5_000, 20_000).allowed());
        assert!(!check_swap(6_000.0, Kind::Entry, 5_000, 20_000).allowed());
        assert!(check_swap(6_000.0, Kind::Exit, 5_000, 20_000).allowed());
        assert!(!check_swap(f64::INFINITY, Kind::Exit, 5_000, 20_000).allowed());
    }

    #[test]
    fn the_slippage_bound_must_be_set_and_within_the_limit() {
        assert!(check_slippage(3_000, 5_000).allowed());
        assert!(!check_slippage(0, 5_000).allowed());
        assert!(!check_slippage(5_001, 5_000).allowed());
    }

    #[test]
    fn dear_gas_holds_back_entries_and_never_exits() {
        let cap = 20_000_000_000;
        assert!(check_gas(10_000_000_000, cap, Kind::Entry).allowed());
        assert!(
            matches!(check_gas(30_000_000_000, cap, Kind::Entry), Verdict::Defer(r) if r.contains("30.00 gwei"))
        );
        assert!(check_gas(300_000_000_000, cap, Kind::Exit).allowed());
    }

    #[test]
    fn a_pool_touched_too_often_waits_a_day_except_to_exit() {
        let mut log = ActionLog::new();
        for t in [0, 100, 200] {
            log.record("p", t);
        }
        assert_eq!(log.in_last_day("p", 300), 3);
        assert!(!log.check("p", 300, 3, Kind::Entry).allowed());
        assert!(log.check("p", 300, 3, Kind::Exit).allowed());
        assert!(log.check("q", 300, 3, Kind::Entry).allowed(), "pools apart");
        // A day after the first, it has dropped out of the window.
        assert_eq!(log.in_last_day("p", 86_400), 2);
        assert!(log.check("p", 86_400, 3, Kind::Entry).allowed());
    }

    #[test]
    fn a_node_behind_the_public_head_allows_exits_only() {
        assert_eq!(node_mode(100, Some(102), 3), NodeMode::Normal);
        assert_eq!(node_mode(100, Some(104), 3), NodeMode::ExitOnly);
        assert_eq!(node_mode(100, None, 3), NodeMode::Normal);
        assert_eq!(node_mode(105, Some(100), 3), NodeMode::Normal);
    }

    #[test]
    fn a_replica_that_differs_from_the_chain_halts_the_pool() {
        let state = PoolState {
            sqrt_price_x96: [7; 32],
            tick: 10,
            liquidity: 5,
        };
        assert!(check_replica(&state, &state).allowed());
        let chain = PoolState {
            liquidity: 6,
            ..state
        };
        assert!(
            matches!(check_replica(&state, &chain), Verdict::Veto(r) if r.contains("(liquidity)"))
        );
    }

    #[test]
    fn sleeves_shrink_to_the_pool_and_total_caps_and_never_grow() {
        let requests = vec![
            ("a".to_string(), 6_000.0),
            ("a".to_string(), 6_000.0),
            ("b".to_string(), 3_000.0),
        ];
        let sizes = fit_sleeves(&requests, 10_000.0, 12_000.0);
        // Pool a holds 12,000 against a cap of 10,000: each to 5,000.
        // Then 13,000 in all against 12,000: everything by 12/13.
        let scale = 12.0 / 13.0;
        for (size, want) in sizes
            .iter()
            .zip([5_000.0 * scale, 5_000.0 * scale, 3_000.0 * scale])
        {
            assert!((size - want).abs() < 1e-9, "{sizes:?}");
        }
        assert!((sizes.iter().sum::<f64>() - 12_000.0).abs() < 1e-9);
        let small = fit_sleeves(&[("a".into(), 1_000.0)], 10_000.0, 12_000.0);
        assert_eq!(small, vec![1_000.0]);
        assert_eq!(fit_sleeves(&[("a".into(), -5.0)], 1.0, 1.0), vec![0.0]);
    }
}
