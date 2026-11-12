//! Pool ranking metrics.
//!
//! Implied volatility combines the three things a pool is ranked on: fee tier,
//! volume and in-range liquidity. See `docs/DESIGN.md`, "Pool universe and
//! ranking".

/// Days used to annualise a daily volatility.
pub const DAYS_PER_YEAR: f64 = 365.0;

/// Annualised implied volatility of a pool or of one zone inside it.
///
/// * `fee` is the fee tier as a fraction (0.0005 for the 0.05% tier).
/// * `volume_per_day` is traded volume per day in the quote token.
/// * `l_sqrt_p` is in-range liquidity times the square root of price, in the
///   quote token. It is the liquidity that is actually in range, not the
///   pool's total.
///
/// Returns `None` when an input is not a positive finite number, so a pool
/// with no in-range liquidity or no volume can never rank.
pub fn implied_vol_annual(fee: f64, volume_per_day: f64, l_sqrt_p: f64) -> Option<f64> {
    let ok = |x: f64| x.is_finite() && x > 0.0;
    if !(ok(fee) && ok(volume_per_day) && ok(l_sqrt_p)) {
        return None;
    }
    Some(2.0 * (fee * volume_per_day / l_sqrt_p).sqrt() * DAYS_PER_YEAR.sqrt())
}

/// Converts "value sitting within 1% of the price" into `L * sqrt(P)`.
///
/// Close to the price, the value within a fraction `d` on both sides is about
/// `L * sqrt(P) * d`, so the depth within 1% is one hundredth of `L * sqrt(P)`.
pub fn l_sqrt_p_from_depth_1pct(depth_within_1pct: f64) -> f64 {
    depth_within_1pct * 100.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_the_design_doc_example() {
        // 0.05% pool, 100 million a day, 5 million within 1% of the price.
        let l_sqrt_p = l_sqrt_p_from_depth_1pct(5_000_000.0);
        let iv = implied_vol_annual(0.0005, 100_000_000.0, l_sqrt_p).unwrap();
        assert!((iv - 0.382).abs() < 0.001, "got {iv}");
    }

    #[test]
    fn squared_iv_is_proportional_to_fees_per_unit_of_in_range_liquidity() {
        let a = implied_vol_annual(0.0005, 1.0e8, 5.0e8).unwrap();
        let b = implied_vol_annual(0.0005, 4.0e8, 5.0e8).unwrap();
        assert!((b / a - 2.0).abs() < 1e-12);
    }

    #[test]
    fn rejects_empty_or_invalid_inputs() {
        assert!(implied_vol_annual(0.0005, 1.0e8, 0.0).is_none());
        assert!(implied_vol_annual(0.0005, 0.0, 5.0e8).is_none());
        assert!(implied_vol_annual(0.0, 1.0e8, 5.0e8).is_none());
        assert!(implied_vol_annual(0.0005, f64::NAN, 5.0e8).is_none());
    }
}
