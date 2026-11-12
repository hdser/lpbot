//! Pool ranking.
//!
//! Pools are ranked by implied volatility, which combines fee tier, volume
//! and in-range liquidity. Pools that fail a floor are listed but not ranked:
//! a thin pool with a handful of trades shows a huge implied volatility that
//! no real position could collect.

use std::collections::BTreeMap;

use crate::metrics::implied_vol_annual;

/// What is measured for one pool. Money figures are in US dollars.
#[derive(Debug, Clone, PartialEq)]
pub struct PoolStats {
    pub id: String,
    /// Fee tier as a fraction (0.0005 for 0.05%).
    pub fee: f64,
    /// Volume traded over the last 24 hours.
    pub volume_24h: f64,
    /// Average daily volume over the longer window (7 days by default).
    pub volume_per_day_long: f64,
    /// In-range liquidity times the square root of price, in dollars.
    pub l_sqrt_p: f64,
}

impl PoolStats {
    /// Value sitting within 1% of the price, in dollars.
    pub fn depth_1pct(&self) -> f64 {
        self.l_sqrt_p / 100.0
    }
}

/// Minimums a pool must clear before it is ranked.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Floors {
    /// Average daily volume over the longer window, in dollars.
    pub volume_per_day: f64,
    /// Value within 1% of the price, in dollars.
    pub depth_1pct: f64,
}

/// A pool with its ranking figures.
#[derive(Debug, Clone, PartialEq)]
pub struct RankedPool {
    pub stats: PoolStats,
    /// Implied volatility from the last 24 hours of volume, annualised.
    pub iv_24h: Option<f64>,
    /// Implied volatility from the longer window. This is what ranks the pool.
    pub iv_long: Option<f64>,
    /// Why the pool is not ranked, if it is not.
    pub excluded: Option<String>,
}

/// Ranks pools by long-window implied volatility, highest first. Pools that
/// fail a floor or have no usable figures come last, in the order given.
pub fn rank(pools: Vec<PoolStats>, floors: &Floors) -> Vec<RankedPool> {
    let mut ranked = Vec::new();
    let mut excluded = Vec::new();
    for stats in pools {
        let iv_24h = implied_vol_annual(stats.fee, stats.volume_24h, stats.l_sqrt_p);
        let iv_long = implied_vol_annual(stats.fee, stats.volume_per_day_long, stats.l_sqrt_p);
        let reason = if stats.depth_1pct() < floors.depth_1pct {
            Some(format!(
                "in-range liquidity {:.0} is below the floor {:.0}",
                stats.depth_1pct(),
                floors.depth_1pct
            ))
        } else if stats.volume_per_day_long < floors.volume_per_day {
            Some(format!(
                "daily volume {:.0} is below the floor {:.0}",
                stats.volume_per_day_long, floors.volume_per_day
            ))
        } else if iv_long.is_none() {
            Some("no usable figures".to_string())
        } else {
            None
        };
        let pool = RankedPool {
            stats,
            iv_24h,
            iv_long,
            excluded: reason,
        };
        if pool.excluded.is_some() {
            excluded.push(pool);
        } else {
            ranked.push(pool);
        }
    }
    ranked.sort_by(|a, b| {
        b.iv_long
            .partial_cmp(&a.iv_long)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    ranked.extend(excluded);
    ranked
}

/// One pool's view of a price: one unit of `base` is worth `price` units of
/// `quote`. `depth_quote` is the pool's in-range liquidity in units of
/// `quote`; deeper pools are trusted more.
#[derive(Debug, Clone, PartialEq)]
pub struct PriceEdge {
    pub base: String,
    pub quote: String,
    pub price: f64,
    pub depth_quote: f64,
}

/// Dollar prices of tokens from pool prices.
///
/// Stablecoins are worth one dollar by definition. Every other token takes
/// its price from the deepest pool that pairs it with an already priced
/// token, repeated until nothing changes. A token with no path to a
/// stablecoin gets no price.
pub fn usd_prices(stables: &[String], edges: &[PriceEdge]) -> BTreeMap<String, f64> {
    let mut prices: BTreeMap<String, f64> = stables.iter().map(|s| (s.clone(), 1.0)).collect();
    loop {
        // Best new price found in this round, per token: (weight, price).
        let mut found: BTreeMap<String, (f64, f64)> = BTreeMap::new();
        for edge in edges {
            if !(edge.price.is_finite() && edge.price > 0.0) {
                continue;
            }
            // (token to price, its dollar price, dollar price of the quote token)
            let candidate = match (prices.get(&edge.base), prices.get(&edge.quote)) {
                (None, Some(quote_usd)) => {
                    Some((edge.base.clone(), edge.price * quote_usd, *quote_usd))
                }
                (Some(base_usd), None) => {
                    let quote_usd = base_usd / edge.price;
                    Some((edge.quote.clone(), quote_usd, quote_usd))
                }
                _ => None,
            };
            if let Some((token, price, quote_usd)) = candidate {
                // Compare pools by their depth in dollars.
                let weight = edge.depth_quote * quote_usd;
                let best = found.entry(token).or_insert((f64::NEG_INFINITY, 0.0));
                if weight > best.0 {
                    *best = (weight, price);
                }
            }
        }
        if found.is_empty() {
            return prices;
        }
        for (token, (_, price)) in found {
            prices.insert(token, price);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pool(id: &str, fee: f64, volume_24h: f64, volume_long: f64, depth_1pct: f64) -> PoolStats {
        PoolStats {
            id: id.into(),
            fee,
            volume_24h,
            volume_per_day_long: volume_long,
            l_sqrt_p: depth_1pct * 100.0,
        }
    }

    const FLOORS: Floors = Floors {
        volume_per_day: 5_000_000.0,
        depth_1pct: 1_000_000.0,
    };

    #[test]
    fn pools_are_ranked_by_long_window_implied_volatility() {
        let ranked = rank(
            vec![
                pool("deep-busy", 0.0005, 100e6, 100e6, 5e6),
                pool("shallow-busy", 0.0005, 100e6, 100e6, 2e6),
                pool("high-fee", 0.003, 30e6, 30e6, 5e6),
            ],
            &FLOORS,
        );
        let order: Vec<&str> = ranked.iter().map(|p| p.stats.id.as_str()).collect();
        // What ranks is fee times volume per unit of in-range liquidity:
        // 2.5e-4 for the shallow pool, 1.8e-4 for the high-fee pool and
        // 1.0e-4 for the deep one.
        assert_eq!(order, ["shallow-busy", "high-fee", "deep-busy"]);
        assert!(ranked.iter().all(|p| p.excluded.is_none()));
        let iv = ranked[2].iv_long.unwrap();
        assert!(
            (iv - 0.382).abs() < 0.001,
            "the design doc example, got {iv}"
        );
    }

    #[test]
    fn one_unusual_day_does_not_reorder_the_list() {
        let ranked = rank(
            vec![
                pool("steady", 0.0005, 80e6, 80e6, 5e6),
                pool("spiked-today", 0.0005, 900e6, 40e6, 5e6),
            ],
            &FLOORS,
        );
        assert_eq!(ranked[0].stats.id, "steady");
        assert!(ranked[1].iv_24h > ranked[0].iv_24h, "the spike still shows");
    }

    #[test]
    fn thin_pools_are_listed_but_not_ranked() {
        let ranked = rank(
            vec![
                pool("dust", 0.01, 10e6, 10e6, 5_000.0),
                pool("quiet", 0.0005, 1e6, 1e6, 5e6),
                pool("good", 0.0005, 100e6, 100e6, 5e6),
                pool("empty", 0.0005, 0.0, 0.0, 0.0),
            ],
            &FLOORS,
        );
        assert_eq!(ranked[0].stats.id, "good");
        assert!(ranked[0].excluded.is_none());
        // The dust pool has by far the highest raw figure and must not lead.
        let dust = ranked.iter().find(|p| p.stats.id == "dust").unwrap();
        assert!(dust.iv_long.unwrap() > ranked[0].iv_long.unwrap());
        assert!(dust.excluded.as_ref().unwrap().contains("liquidity"));
        let quiet = ranked.iter().find(|p| p.stats.id == "quiet").unwrap();
        assert!(quiet.excluded.as_ref().unwrap().contains("volume"));
        assert!(ranked
            .iter()
            .find(|p| p.stats.id == "empty")
            .unwrap()
            .excluded
            .is_some());
    }

    fn edge(base: &str, quote: &str, price: f64, depth_quote: f64) -> PriceEdge {
        PriceEdge {
            base: base.into(),
            quote: quote.into(),
            price,
            depth_quote,
        }
    }

    #[test]
    fn dollar_prices_come_from_the_deepest_pool_with_a_priced_token() {
        let stables = vec!["USDC".to_string(), "USDT".to_string()];
        let prices = usd_prices(
            &stables,
            &[
                // One USDC is worth 0.0005 WETH: WETH is worth 2,000 dollars.
                // Depth of 900 WETH is 1.8 million dollars.
                edge("USDC", "WETH", 0.0005, 900.0),
                // A shallower pool (100,000 dollars) with a stale price must
                // not win, even though its depth is the larger raw number.
                edge("WETH", "USDT", 1_500.0, 100_000.0),
                // WBTC only trades against WETH here.
                edge("WBTC", "WETH", 30.0, 500.0),
            ],
        );
        assert_eq!(prices["USDC"], 1.0);
        assert_eq!(prices["USDT"], 1.0);
        assert!((prices["WETH"] - 2_000.0).abs() < 1e-9);
        assert!((prices["WBTC"] - 60_000.0).abs() < 1e-6);
    }

    #[test]
    fn a_token_with_no_path_to_a_stablecoin_gets_no_price() {
        let prices = usd_prices(
            &["USDC".to_string()],
            &[
                edge("AAA", "BBB", 2.0, 1.0),
                edge("WETH", "USDC", f64::NAN, 1.0),
            ],
        );
        assert_eq!(prices.len(), 1);
    }
}
