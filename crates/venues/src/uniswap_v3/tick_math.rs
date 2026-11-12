//! Conversion between ticks and square-root prices (TickMath).
//!
//! A tick `i` stands for the price `1.0001^i`. Square-root prices are Q64.96
//! fixed-point numbers.

use alloy_primitives::{uint, I256, U256};

use super::{MathError, MathResult};

/// Lowest tick the protocol supports.
pub const MIN_TICK: i32 = -887_272;
/// Highest tick the protocol supports.
pub const MAX_TICK: i32 = 887_272;

/// Square-root price at `MIN_TICK`.
pub const MIN_SQRT_RATIO: U256 = uint!(4295128739_U256);
/// Square-root price at `MAX_TICK`.
pub const MAX_SQRT_RATIO: U256 = uint!(1461446703485210103287273052203988822378723970342_U256);

/// One multiplier per bit of the absolute tick, from bit 1 upward.
const MULTIPLIERS: [U256; 19] = [
    uint!(0xfff97272373d413259a46990580e213a_U256),
    uint!(0xfff2e50f5f656932ef12357cf3c7fdcc_U256),
    uint!(0xffe5caca7e10e4e61c3624eaa0941cd0_U256),
    uint!(0xffcb9843d60f6159c9db58835c926644_U256),
    uint!(0xff973b41fa98c081472e6896dfb254c0_U256),
    uint!(0xff2ea16466c96a3843ec78b326b52861_U256),
    uint!(0xfe5dee046a99a2a811c461f1969c3053_U256),
    uint!(0xfcbe86c7900a88aedcffc83b479aa3a4_U256),
    uint!(0xf987a7253ac413176f2b074cf7815e54_U256),
    uint!(0xf3392b0822b70005940c7a398e4b70f3_U256),
    uint!(0xe7159475a2c29b7443b29c7fa6e889d9_U256),
    uint!(0xd097f3bdfd2022b8845ad8f792aa5825_U256),
    uint!(0xa9f746462d870fdf8a65dc1f90e061e5_U256),
    uint!(0x70d869a156d2a1b890bb3df62baf32f7_U256),
    uint!(0x31be135f97d08fd981231505542fcfa6_U256),
    uint!(0x9aa508b5b7a84e1c677de54f3e99bc9_U256),
    uint!(0x5d6af8dedb81196699c329225ee604_U256),
    uint!(0x2216e584f5fa1ea926041bedfe98_U256),
    uint!(0x48a170391f7dc42444e8fa2_U256),
];

const BIT0_MULTIPLIER: U256 = uint!(0xfffcb933bd6fad37aa2d162d1a594001_U256);
const ONE_Q128: U256 = uint!(0x100000000000000000000000000000000_U256);

const LOG_SQRT_10001_FACTOR: I256 = I256::from_raw(uint!(255738958999603826347141_U256));
const TICK_LOW_OFFSET: I256 = I256::from_raw(uint!(3402992956809132418596140100660247210_U256));
const TICK_HIGH_OFFSET: I256 = I256::from_raw(uint!(291339464771989622907027621153398088495_U256));

/// The square-root price at a tick: `sqrt(1.0001^tick) * 2^96`.
pub fn get_sqrt_ratio_at_tick(tick: i32) -> MathResult<U256> {
    let abs_tick = tick.unsigned_abs();
    if abs_tick > MAX_TICK as u32 {
        return Err(MathError::TickOutOfRange);
    }

    let mut ratio = if abs_tick & 1 != 0 {
        BIT0_MULTIPLIER
    } else {
        ONE_Q128
    };
    for (i, multiplier) in MULTIPLIERS.iter().enumerate() {
        if abs_tick & (1 << (i + 1)) != 0 {
            // Both factors are below 2^128, so the product fits 256 bits.
            ratio = (ratio * *multiplier) >> 128;
        }
    }

    if tick > 0 {
        ratio = U256::MAX / ratio;
    }

    // Go from Q128.128 to Q128.96, rounding up so that
    // get_tick_at_sqrt_ratio of the result is always consistent.
    let shifted: U256 = ratio >> 32;
    let has_remainder = !(ratio & U256::from(u32::MAX)).is_zero();
    Ok(if has_remainder {
        shifted + U256::from(1u8)
    } else {
        shifted
    })
}

/// The greatest tick whose square-root price is at most `sqrt_price_x96`.
pub fn get_tick_at_sqrt_ratio(sqrt_price_x96: U256) -> MathResult<i32> {
    // The upper bound is exclusive: the price can never reach the max tick's price.
    if sqrt_price_x96 < MIN_SQRT_RATIO || sqrt_price_x96 >= MAX_SQRT_RATIO {
        return Err(MathError::SqrtPriceOutOfRange);
    }
    let ratio: U256 = sqrt_price_x96 << 32;

    // Index of the most significant bit.
    let msb = ratio.bit_len() - 1;

    let mut r: U256 = if msb >= 128 {
        ratio >> (msb - 127)
    } else {
        ratio << (127 - msb)
    };

    // Integer part of log2, as a signed Q64 number kept in two's complement.
    let mut log_2: U256 = if msb >= 128 {
        U256::from(msb - 128) << 64
    } else {
        let magnitude: U256 = U256::from(128 - msb) << 64;
        magnitude.wrapping_neg()
    };

    // Fourteen fractional bits, most significant first.
    for bit in (50..=63usize).rev() {
        r = (r * r) >> 127;
        let f: U256 = r >> 128;
        let bit_value: U256 = f << bit;
        log_2 |= bit_value;
        r >>= f.to::<usize>();
    }

    let log_sqrt10001 = I256::from_raw(log_2).wrapping_mul(LOG_SQRT_10001_FACTOR);

    let tick_low = (log_sqrt10001.wrapping_sub(TICK_LOW_OFFSET))
        .asr(128)
        .as_i32();
    let tick_high = (log_sqrt10001.wrapping_add(TICK_HIGH_OFFSET))
        .asr(128)
        .as_i32();

    Ok(if tick_low == tick_high {
        tick_low
    } else if get_sqrt_ratio_at_tick(tick_high)? <= sqrt_price_x96 {
        tick_high
    } else {
        tick_low
    })
}

#[cfg(test)]
mod tests {
    use super::super::test_util::{encode_price_sqrt, u};
    use super::super::Q96;
    use super::*;

    #[test]
    fn rejects_ticks_outside_the_range() {
        assert_eq!(
            get_sqrt_ratio_at_tick(MIN_TICK - 1),
            Err(MathError::TickOutOfRange)
        );
        assert_eq!(
            get_sqrt_ratio_at_tick(MAX_TICK + 1),
            Err(MathError::TickOutOfRange)
        );
    }

    #[test]
    fn matches_the_reference_values_at_the_extremes() {
        assert_eq!(get_sqrt_ratio_at_tick(MIN_TICK), Ok(u("4295128739")));
        assert_eq!(get_sqrt_ratio_at_tick(MIN_TICK + 1), Ok(u("4295343490")));
        assert_eq!(
            get_sqrt_ratio_at_tick(MAX_TICK - 1),
            Ok(u("1461373636630004318706518188784493106690254656249"))
        );
        assert_eq!(
            get_sqrt_ratio_at_tick(MAX_TICK),
            Ok(u("1461446703485210103287273052203988822378723970342"))
        );
        assert_eq!(get_sqrt_ratio_at_tick(MIN_TICK), Ok(MIN_SQRT_RATIO));
        assert_eq!(get_sqrt_ratio_at_tick(MAX_TICK), Ok(MAX_SQRT_RATIO));
    }

    #[test]
    fn tick_zero_is_a_price_of_one() {
        assert_eq!(get_sqrt_ratio_at_tick(0), Ok(Q96));
        assert_eq!(get_tick_at_sqrt_ratio(Q96), Ok(0));
    }

    #[test]
    fn stays_close_to_the_floating_point_value() {
        // The reference tests allow one millionth of relative error against
        // sqrt(1.0001^tick) * 2^96.
        for tick in [
            50, 100, 250, 500, 1_000, 2_500, 3_000, 4_000, 5_000, 50_000, 150_000, 250_000,
            500_000, 738_203,
        ] {
            for t in [-tick, tick] {
                let exact = 1.0001f64.powi(t).sqrt() * 2f64.powi(96);
                let got: f64 = get_sqrt_ratio_at_tick(t)
                    .unwrap()
                    .to_string()
                    .parse()
                    .unwrap();
                assert!(((got - exact) / exact).abs() < 1e-6, "tick {t}");
            }
        }
    }

    #[test]
    fn rejects_prices_outside_the_range() {
        assert_eq!(
            get_tick_at_sqrt_ratio(MIN_SQRT_RATIO - U256::from(1u8)),
            Err(MathError::SqrtPriceOutOfRange)
        );
        assert_eq!(
            get_tick_at_sqrt_ratio(MAX_SQRT_RATIO),
            Err(MathError::SqrtPriceOutOfRange)
        );
    }

    #[test]
    fn inverts_the_reference_values_at_the_extremes() {
        assert_eq!(get_tick_at_sqrt_ratio(MIN_SQRT_RATIO), Ok(MIN_TICK));
        assert_eq!(get_tick_at_sqrt_ratio(u("4295343490")), Ok(MIN_TICK + 1));
        assert_eq!(
            get_tick_at_sqrt_ratio(u("1461373636630004318706518188784493106690254656249")),
            Ok(MAX_TICK - 1)
        );
        assert_eq!(
            get_tick_at_sqrt_ratio(MAX_SQRT_RATIO - U256::from(1u8)),
            Ok(MAX_TICK - 1)
        );
    }

    #[test]
    fn the_tick_of_a_price_brackets_that_price() {
        // The reference property: ratio(tick) <= price < ratio(tick + 1).
        let prices = [
            MIN_SQRT_RATIO,
            encode_price_sqrt(1, 1_000_000_000_000),
            encode_price_sqrt(1, 1_000_000),
            encode_price_sqrt(1, 64),
            encode_price_sqrt(1, 8),
            encode_price_sqrt(1, 2),
            encode_price_sqrt(1, 1),
            encode_price_sqrt(2, 1),
            encode_price_sqrt(8, 1),
            encode_price_sqrt(64, 1),
            encode_price_sqrt(1_000_000, 1),
            encode_price_sqrt(1_000_000_000_000, 1),
            MAX_SQRT_RATIO - U256::from(1u8),
        ];
        for price in prices {
            let tick = get_tick_at_sqrt_ratio(price).unwrap();
            assert!(get_sqrt_ratio_at_tick(tick).unwrap() <= price, "{price}");
            assert!(get_sqrt_ratio_at_tick(tick + 1).unwrap() > price, "{price}");
        }
    }

    #[test]
    fn round_trips_every_tick_in_a_wide_sample() {
        let mut tick = MIN_TICK;
        while tick < MAX_TICK {
            let price = get_sqrt_ratio_at_tick(tick).unwrap();
            assert_eq!(get_tick_at_sqrt_ratio(price), Ok(tick), "tick {tick}");
            // One unit below the tick's price belongs to the tick below.
            if tick > MIN_TICK {
                assert_eq!(
                    get_tick_at_sqrt_ratio(price - U256::from(1u8)),
                    Ok(tick - 1),
                    "below tick {tick}"
                );
            }
            tick += 997;
        }
    }
}
