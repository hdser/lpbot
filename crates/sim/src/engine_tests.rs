use alloy_primitives::{I256, U256};
use lpbot_data::lake::TimedEvent;
use lpbot_data::synth::{generate, SynthConfig};
use lpbot_risk::LossLimit;
use lpbot_strategy::FixedRule;
use lpbot_venues::uniswap_v3::events::PoolEvent;
use lpbot_venues::uniswap_v3::liquidity_amounts::get_liquidity_for_amount1;
use lpbot_venues::uniswap_v3::pool::{fees_earned, Pool, PoolSnapshot, Replay, SwapEvent};
use lpbot_venues::uniswap_v3::tick_math::get_sqrt_ratio_at_tick;

use crate::engine::{
    after_protocol, diluted_fee_growth, run, share_of, thinnest_liquidity, to_f64, Dollar,
    GasModel, Output, Policy, Settings, Token,
};

const T0: u64 = 1_800_000_000;
const HOUR: u64 = 3_600;
/// Liquidity of the test pool between ticks -10,000 and 10,000.
const DEEP: u128 = 1_000_000_000_000_000_000_000_000;

/// One thing that happens to the test pool.
#[derive(Debug, Clone, Copy)]
enum Step {
    /// A swap that leaves the price exactly on this tick's price.
    To(i32),
    /// A flash loan that pays these fees in token0 and token1.
    Flash(u128, u128),
    /// The protocol's fee share is switched on, as denominators.
    FeeProtocol(u8, u8),
}

/// A pool at tick 0 with `base` liquidity spread evenly between ticks
/// -10,000 and 10,000 plus the `extra` positions, and the events of each
/// block's steps, blocks one hour and 300 block numbers apart.
fn history_of(
    base: u128,
    extra: &[(i32, i32, u128)],
    blocks: &[&[Step]],
) -> (PoolSnapshot, Vec<TimedEvent>) {
    let mut pool = Pool::new(500, 10);
    pool.initialize(get_sqrt_ratio_at_tick(0).unwrap()).unwrap();
    pool.mint(-10_000, 10_000, base).unwrap();
    for (lower, upper, liquidity) in extra {
        pool.mint(*lower, *upper, *liquidity).unwrap();
    }
    let snapshot = pool.snapshot();

    let huge = I256::try_from(10u128.pow(32)).unwrap();
    let mut events = Vec::new();
    for (i, steps) in blocks.iter().enumerate() {
        let hours = i as u64 + 1;
        for (log_index, step) in steps.iter().enumerate() {
            let event = match *step {
                Step::To(target) => {
                    let limit = get_sqrt_ratio_at_tick(target).unwrap();
                    let zero_for_one = limit < pool.sqrt_price_x96();
                    let amounts = pool.swap(zero_for_one, huge, limit, &mut ()).unwrap();
                    PoolEvent::Swap(SwapEvent {
                        amount0: amounts.amount0,
                        amount1: amounts.amount1,
                        sqrt_price_x96: pool.sqrt_price_x96(),
                        liquidity: pool.liquidity(),
                        tick: pool.tick(),
                    })
                }
                Step::Flash(paid0, paid1) => {
                    let (paid0, paid1) = (U256::from(paid0), U256::from(paid1));
                    pool.flash(paid0, paid1).unwrap();
                    PoolEvent::Flash { paid0, paid1 }
                }
                Step::FeeProtocol(fee_protocol0, fee_protocol1) => {
                    pool.set_fee_protocol(fee_protocol0, fee_protocol1);
                    PoolEvent::SetFeeProtocol {
                        fee_protocol0,
                        fee_protocol1,
                    }
                }
            };
            events.push(TimedEvent {
                block: 1_000 + 300 * hours,
                time: T0 + HOUR * hours,
                log_index: log_index as u32,
                event,
            });
        }
    }
    (snapshot, events)
}

/// The deep pool, and the events of moving its price to each target tick
/// in turn, one swap per block.
fn history(targets: &[i32]) -> (PoolSnapshot, Vec<TimedEvent>) {
    let steps: Vec<[Step; 1]> = targets.iter().map(|t| [Step::To(*t)]).collect();
    let blocks: Vec<&[Step]> = steps.iter().map(|s| s.as_slice()).collect();
    history_of(DEEP, &[], &blocks)
}

fn settings(rule: FixedRule) -> Settings {
    Settings {
        value: Token::One,
        dollar: Dollar::Other,
        decimals0: 18,
        decimals1: 18,
        width: 20,
        max_gap: 20,
        capital: U256::from(10u128.pow(18)),
        capital_share: 1.0,
        max_share_of_in_range_liquidity: None,
        loss_limit: LossLimit::new(50.0).unwrap(),
        policy: Policy::Fixed(rule),
        decide_every: HOUR,
        check_every: HOUR,
        latency_blocks: 1,
        lookbacks_days: vec![7, 30],
        move_margin: 1.5,
        min_passes_per_day: 0.0,
        max_vol_to_width: None,
        max_daily_vol_ticks: None,
        min_gas_multiple: None,
        max_seconds_beyond: None,
        exit_vol_to_width: None,
        gas: GasModel::free(),
        series_every: HOUR,
        trade_from: T0,
        trade_to: T0 + 1_000 * HOUR,
    }
}

/// Down through the first range, a little further, then back up above it.
const THERE_AND_BACK: [i32; 8] = [5, 6, -50, -52, 30, 31, 32, 33];

fn there_and_back(rule: FixedRule) -> Output {
    let (snapshot, events) = history(&THERE_AND_BACK);
    run(snapshot, &events, &settings(rule)).unwrap()
}

fn assert_conserved(out: &Output) {
    let from_campaigns: f64 = out.campaigns.iter().map(|c| c.result).sum();
    let total = out.summary.end_capital - out.summary.start_capital;
    assert!(
        (from_campaigns - total).abs() < 1e-12,
        "campaigns {from_campaigns} vs run {total}"
    );
}

#[test]
fn a_round_trip_returns_the_value_token_plus_twice_the_fee() {
    let out = there_and_back(FixedRule::Wait);
    let first = &out.campaigns[0];
    assert_eq!((first.tick_lower, first.tick_upper), (-20, 0));
    assert_eq!(first.exit_reason, "round_trip_complete");
    assert_eq!((first.passes, first.flips), (2, 0));
    // All the capital went in, and it came back with about 0.1% on top:
    // two passes at the 0.05% fee tier.
    assert!(first.amount_in > 0.999_999 && first.amount_in <= 1.0);
    let gain = first.result / first.amount_in;
    assert!((0.000_95..0.001_05).contains(&gain), "{gain}");
    assert!((first.fees / first.amount_in - gain).abs() < 0.000_01);
    // One hour beyond the range until price came back, counted from the
    // first block that saw it there.
    assert_eq!(first.seconds_beyond, 2 * HOUR);
    assert!(out.summary.result_pct > 0.09 && out.summary.result_pct < 0.11);
    assert_conserved(&out);
}

#[test]
fn nothing_is_filled_on_the_block_it_was_decided_on() {
    let (snapshot, events) = history(&THERE_AND_BACK);
    let out = run(snapshot.clone(), &events, &settings(FixedRule::Wait)).unwrap();
    // Decided at the end of the first event's block, filled in the next
    // block that has an event.
    assert_eq!(out.campaigns[0].opened_block, events[1].block);
    assert!(out.campaigns[0].opened_block > events[0].block);

    // With two hours of latency the same decision fills two events later,
    // on the price then.
    let slow = Settings {
        latency_blocks: 600,
        ..settings(FixedRule::Wait)
    };
    let out = run(snapshot, &events, &slow).unwrap();
    assert_eq!(out.campaigns[0].opened_block, events[2].block);
}

#[test]
fn the_three_fixed_rules_rank_as_expected_when_price_comes_back() {
    let wait = there_and_back(FixedRule::Wait);
    let flip = there_and_back(FixedRule::Flip);
    let close = there_and_back(FixedRule::Close);

    // Close sold the value token through the range around tick -10 and
    // bought it back at -50: about 0.4% lost, the pass's fee and the swap
    // fee cancelling out.
    let closed = &close.campaigns[0];
    assert_eq!(closed.exit_reason, "patience_close");
    assert_eq!(closed.passes, 1);
    let loss = closed.result / closed.amount_in;
    assert!((-0.004_5..-0.003_5).contains(&loss), "{loss}");

    // Flip bought it back through the range around -30 instead: about
    // 0.2% lost on price, 0.1% earned in fees on the two passes.
    let flipped = &flip.campaigns[0];
    assert_eq!(flipped.exit_reason, "round_trip_complete");
    assert_eq!((flipped.passes, flipped.flips), (2, 1));
    let loss = flipped.result / flipped.amount_in;
    assert!((-0.001_5..-0.000_5).contains(&loss), "{loss}");

    assert!(wait.summary.result_pct > flip.summary.result_pct);
    assert!(flip.summary.result_pct > close.summary.result_pct);
    for out in [&wait, &flip, &close] {
        assert_conserved(out);
    }
}

#[test]
fn every_patience_decision_is_recorded() {
    let wait = there_and_back(FixedRule::Wait);
    // Stranded at hours 3 and 4; back in range at hour 5.
    assert_eq!(wait.decisions.len(), 2);
    assert!(wait
        .decisions
        .iter()
        .all(|d| d.action == "wait" && d.leg == "entry"));
    assert_eq!(wait.decisions[0].tick, -50);
    assert_eq!(wait.decisions[0].seconds_stranded, 0);
    assert_eq!(wait.decisions[1].seconds_stranded, HOUR);

    let flip = there_and_back(FixedRule::Flip);
    assert_eq!(flip.decisions[0].action, "flip");
    assert_eq!(
        (
            flip.decisions[0].target_lower,
            flip.decisions[0].target_upper
        ),
        (Some(-40), Some(-20))
    );
    let close = there_and_back(FixedRule::Close);
    assert_eq!(close.decisions[0].action, "close");
    assert_eq!(close.decisions[0].campaign, 1);
}

#[test]
fn the_series_follows_the_position_through_its_states() {
    let out = there_and_back(FixedRule::Wait);
    let states: Vec<&str> = out.series.iter().map(|r| r.state.as_str()).collect();
    assert_eq!(
        states,
        vec!["flat", "resting", "beyond", "beyond", "resting", "flat", "resting", "resting"]
    );
    let beyond = &out.series[2];
    assert_eq!((beyond.tick_lower, beyond.tick_upper), (Some(-20), Some(0)));
    // Marked at the pool price the stranded position is under water.
    assert!(beyond.equity < beyond.hold);
    assert!(beyond.fees > 0.0);
    // Token decimals are equal here, so the price is 1.0001^-tick.
    assert!((beyond.price - 1.0001f64.powi(50)).abs() < 1e-6);
    // The range as prices: ticks -20 and 0, low price first.
    assert_eq!(beyond.price_lower, Some(1.0));
    assert!((beyond.price_upper.unwrap() - 1.0001f64.powi(20)).abs() < 1e-12);
    assert_eq!(out.series[0].price_lower, None);
    assert!(out.series.windows(2).all(|w| w[0].time < w[1].time));
    assert!(out.summary.max_drawdown_pct > 0.3 && out.summary.max_drawdown_pct < 0.5);
}

#[test]
fn the_loss_limit_closes_everything_and_stops_the_run() {
    let (snapshot, events) = history(&[5, 6, -50, -52, -53, -54, 30, 31]);
    let tight = Settings {
        loss_limit: LossLimit::new(0.2).unwrap(),
        ..settings(FixedRule::Wait)
    };
    let out = run(snapshot, &events, &tight).unwrap();
    assert!(out.summary.halted_by_loss_limit);
    assert_eq!(out.campaigns.len(), 1);
    assert_eq!(out.campaigns[0].exit_reason, "loss_limit");
    // Closed in the block after the breach, and nothing opened afterwards.
    assert_eq!(out.campaigns[0].closed_block, events[3].block);
    assert!(out.summary.result_pct < -0.2 && out.summary.result_pct > -0.5);
    assert_eq!(out.series.last().unwrap().state, "flat");
    assert_conserved(&out);
}

#[test]
fn gas_is_charged_per_action_in_the_value_token() {
    let (snapshot, events) = history(&THERE_AND_BACK);
    let free = run(snapshot.clone(), &events, &settings(FixedRule::Wait)).unwrap();
    let priced = Settings {
        gas: GasModel {
            open_units: 500_000,
            close_units: 300_000,
            swap_units: 200_000,
            gas_price_gwei: 10.0,
            // The value token is worth one ETH here.
            value_raw_per_eth: 1e18,
        },
        ..settings(FixedRule::Wait)
    };
    let out = run(snapshot, &events, &priced).unwrap();
    // First campaign: open, close and the swap of the fee in the other token.
    let gas = out.campaigns[0].gas;
    assert!((gas - 0.010).abs() < 1e-9, "{gas}");
    // The gas set aside at entry is not deployed, so the fees are a touch lower too.
    assert!((free.campaigns[0].result - out.campaigns[0].result - gas).abs() < 1e-5);
    assert!(out.summary.gas >= gas);
    assert_conserved(&out);
}

#[test]
fn the_value_token_can_be_token0() {
    // The mirror image: value in token0, ranges above the price, the price
    // path turned upside down.
    let (snapshot, events) = history(&[-5, -6, 50, 52, -30, -31, -32, -33]);
    let mirrored = Settings {
        value: Token::Zero,
        ..settings(FixedRule::Wait)
    };
    let out = run(snapshot, &events, &mirrored).unwrap();
    let first = &out.campaigns[0];
    assert_eq!((first.tick_lower, first.tick_upper), (0, 20));
    assert_eq!(
        (first.passes, first.exit_reason.as_str()),
        (2, "round_trip_complete")
    );
    let gain = first.result / first.amount_in;
    assert!((0.000_95..0.001_05).contains(&gain), "{gain}");
    assert_conserved(&out);
}

#[test]
fn events_before_the_trading_window_only_build_history() {
    let (snapshot, events) = history(&THERE_AND_BACK);
    let late = Settings {
        trade_from: T0 + 5 * HOUR,
        ..settings(FixedRule::Wait)
    };
    let out = run(snapshot.clone(), &events, &late).unwrap();
    assert!(out.campaigns.iter().all(|c| c.opened_time > T0 + 5 * HOUR));
    assert_eq!(out.series[0].time, T0 + 5 * HOUR);
    assert_eq!(out.summary.events, 8);

    // And nothing happens after the window closes.
    let early = Settings {
        trade_to: T0 + 3 * HOUR,
        ..settings(FixedRule::Wait)
    };
    let out = run(snapshot, &events, &early).unwrap();
    assert_eq!(out.summary.events, 2);
    assert_eq!(out.campaigns.len(), 1);
    assert_eq!(out.campaigns[0].exit_reason, "window_end");
}

#[test]
fn bad_settings_are_refused_with_the_reason() {
    let (snapshot, events) = history(&[5]);
    let odd = Settings {
        width: 15,
        ..settings(FixedRule::Wait)
    };
    let error = run(snapshot.clone(), &events, &odd)
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("15 ticks") && error.contains("tick spacing of 10"),
        "{error}"
    );
    let broke = Settings {
        capital: U256::ZERO,
        ..settings(FixedRule::Wait)
    };
    assert!(run(snapshot, &events, &broke).is_err());
}

#[test]
fn a_simulated_month_runs_under_every_rule_and_adds_up() {
    let config = SynthConfig {
        days: 6,
        swaps_per_day: 240,
        ..Default::default()
    };
    let pool = lpbot_data::backfill::PoolRef {
        address: alloy_primitives::Address::repeat_byte(1),
        token0: alloy_primitives::Address::repeat_byte(2),
        token1: alloy_primitives::Address::repeat_byte(3),
    };
    let history = generate(&config, pool).unwrap();
    let base = Settings {
        decimals0: 6,
        capital: U256::from(5u128 * 10u128.pow(18)),
        // The first two days only build the zone history.
        trade_from: config.start.start() + 2 * 86_400,
        trade_to: config.start.start() + 6 * 86_400,
        lookbacks_days: vec![1, 2],
        ..settings(FixedRule::Wait)
    };
    for rule in [FixedRule::Wait, FixedRule::Flip, FixedRule::Close] {
        let s = Settings {
            policy: Policy::Fixed(rule),
            ..base.clone()
        };
        let out = run(history.snapshot.state.clone(), &history.events, &s).unwrap();
        let again = run(history.snapshot.state.clone(), &history.events, &s).unwrap();
        assert_eq!(out, again, "{rule:?} is not deterministic");
        assert!(
            !out.campaigns.is_empty() && out.series.len() > 90,
            "{rule:?}"
        );
        assert!(out.summary.passes > 0, "{rule:?}");
        assert!(!out.zones.is_empty());
        // Around 2,000 dollars per ETH, as the simulated pool is set up.
        assert!(out
            .series
            .iter()
            .all(|r| r.price > 1_800.0 && r.price < 2_200.0));
        // A made-up price says nothing about the rules; the run only has
        // to stay within reason and add up.
        assert!(
            out.summary.result_pct > -50.0 && out.summary.result_pct < 50.0,
            "{rule:?}: {}",
            out.summary.result_pct
        );
        assert_conserved(&out);
    }
}

// --- the adaptive decision ----------------------------------------------

use lpbot_strategy::adaptive::AdaptiveParams;

/// Hours of history in which price swings between two ticks, followed by
/// the usual entry and a drop below the first range.
fn swinging_then_stranded(low: i32, high: i32, tail: &[i32]) -> (PoolSnapshot, Vec<TimedEvent>) {
    let mut targets: Vec<i32> = (0..48)
        .map(|i| if i % 2 == 0 { high } else { low })
        .collect();
    targets.extend_from_slice(tail);
    history(&targets)
}

fn adaptive(params: AdaptiveParams) -> Settings {
    Settings {
        policy: Policy::Adaptive(params),
        // The swings are history only; trading starts after them.
        trade_from: T0 + 48 * HOUR + 1,
        lookbacks_days: vec![1, 2],
        ..settings(FixedRule::Wait)
    }
}

fn params() -> AdaptiveParams {
    AdaptiveParams {
        horizon: 4 * HOUR,
        switch_margin: 0.000_5,
        reinvest_credit: 0.0,
        lookback_days: 2,
    }
}

#[test]
fn with_a_history_of_coming_back_the_adaptive_decision_waits() {
    // Price has swung between -60 and 10 every hour for two days. After
    // the entry at [-20, 0) it drops to -50: a move back up of 50 ticks
    // has happened within four hours from every low in the history.
    let (snapshot, events) = swinging_then_stranded(-60, 10, &[5, 6, -50, -52, 30, 31, 32]);
    let out = run(snapshot, &events, &adaptive(params())).unwrap();
    let stranded: Vec<_> = out.decisions.iter().filter(|d| d.tick <= -50).collect();
    assert!(!stranded.is_empty());
    for decision in &stranded {
        assert_eq!(decision.action, "wait", "{decision:?}");
        let (wait, close) = (
            decision.expected_wait.unwrap(),
            decision.expected_close.unwrap(),
        );
        assert!(wait > close, "{decision:?}");
        assert!(decision.expected_flip.is_some());
    }
    // And price did come back: the campaign made its round trip.
    let first = &out.campaigns[0];
    assert_eq!((first.passes, first.flips), (2, 0));
    assert!(first.result > 0.0);
    assert_conserved(&out);
}

#[test]
fn where_price_has_never_been_nothing_is_expected_to_beat_waiting() {
    // Price has only ever swung between 0 and 12. A drop to -300 leaves
    // the range 300 ticks away, in a region with no history at all: no
    // return of that size, no pass through a range down there, no zone
    // to redeploy into. Every option comes down to the swap at about
    // this price, so nothing clears the margin and the bot stays put.
    let (snapshot, events) = swinging_then_stranded(0, 12, &[25, 26, -300, -302, -304, -306]);
    let out = run(snapshot, &events, &adaptive(params())).unwrap();
    let first = out.decisions.iter().find(|d| d.tick <= -300).unwrap();
    assert_eq!(first.action, "wait", "{first:?}");
    let (wait, close) = (first.expected_wait.unwrap(), first.expected_close.unwrap());
    assert!((wait - close).abs() / close < 0.000_2, "{first:?}");
    assert!((first.expected_flip.unwrap() - close).abs() / close < 0.000_2);
    assert_conserved(&out);
}

/// Two days of swings between -60 and -20, a jump up to 45 where the bot
/// enters, and a drop back to `stranded_at` in the swinging region.
fn stranded_in_a_busy_region(stranded_at: i32) -> (PoolSnapshot, Vec<TimedEvent>) {
    swinging_then_stranded(
        -60,
        -20,
        &[45, 46, stranded_at, stranded_at - 2, -20, -18, -16],
    )
}

#[test]
fn with_a_busy_range_next_to_the_price_the_adaptive_decision_flips() {
    // Stranded at the bottom of the swings: the range above the price
    // has been passed from every low in the history, while a return of
    // 100 ticks to the entry range happened once.
    let lean = AdaptiveParams {
        switch_margin: 0.000_2,
        ..params()
    };
    let (snapshot, events) = stranded_in_a_busy_region(-60);
    let out = run(snapshot.clone(), &events, &adaptive(lean)).unwrap();
    let first = out.decisions.iter().find(|d| d.tick <= -60).unwrap();
    assert_eq!(first.action, "flip", "{first:?}");
    assert_eq!(
        (first.target_lower, first.target_upper),
        (Some(-50), Some(-30))
    );
    let (wait, flip, close) = (
        first.expected_wait.unwrap(),
        first.expected_flip.unwrap(),
        first.expected_close.unwrap(),
    );
    assert!(flip > wait && wait > close, "{first:?}");

    // The flip was made, and price passed through it on the way to -20.
    let campaign = &out.campaigns[0];
    assert_eq!((campaign.flips, campaign.passes), (1, 2));
    assert_eq!(campaign.exit_reason, "round_trip_complete");
    // It did better than closing at -60 did.
    let closing = Settings {
        policy: Policy::Fixed(FixedRule::Close),
        ..adaptive(lean)
    };
    let closed = run(snapshot, &events, &closing).unwrap();
    assert_eq!(closed.campaigns[0].exit_reason, "patience_close");
    assert!(campaign.result > closed.campaigns[0].result);
    assert_conserved(&out);
}

#[test]
fn the_margin_and_the_reinvest_credit_tip_the_decision() {
    // Stranded inside the swings, with a range right below the price
    // that every swing passes through for freed capital to go into.
    let (snapshot, events) = stranded_in_a_busy_region(-30);
    // A margin no option can clear: the bot stays put whatever the odds.
    let patient = AdaptiveParams {
        switch_margin: 0.5,
        ..params()
    };
    let out = run(snapshot.clone(), &events, &adaptive(patient)).unwrap();
    assert!(out.decisions.iter().all(|d| d.action == "wait"));

    // Credit for what freed capital could earn there raises what closing
    // is expected to leave, and nothing else.
    let plain = run(snapshot.clone(), &events, &adaptive(patient)).unwrap();
    let credited = AdaptiveParams {
        reinvest_credit: 1.0,
        ..patient
    };
    let with_credit = run(snapshot, &events, &adaptive(credited)).unwrap();
    let (a, b) = (&with_credit.decisions[0], &plain.decisions[0]);
    assert!(
        a.expected_close.unwrap() > b.expected_close.unwrap(),
        "{a:?} {b:?}"
    );
    assert_eq!(a.expected_wait, b.expected_wait);
    assert_eq!(a.expected_flip, b.expected_flip);
}

#[test]
fn without_enough_history_the_adaptive_decision_waits_and_expects_nothing() {
    let (snapshot, events) = history(&THERE_AND_BACK);
    let fresh = Settings {
        policy: Policy::Adaptive(params()),
        ..settings(FixedRule::Wait)
    };
    let out = run(snapshot, &events, &fresh).unwrap();
    assert_eq!(out.decisions[0].action, "wait");
    assert_eq!(out.decisions[0].expected_wait, None);
    assert_eq!(out.decisions[0].expected_close, None);
}

// --- fees, the share cap and gas ------------------------------------------

/// Gas at 10 gwei with the value token worth one ETH: 0.005 to open,
/// 0.003 to close and 0.002 to swap.
fn priced() -> GasModel {
    GasModel {
        open_units: 500_000,
        close_units: 300_000,
        swap_units: 200_000,
        gas_price_gwei: 10.0,
        value_raw_per_eth: 1e18,
    }
}

/// Everything the pool's LPs earned over `events`, in each token, with
/// `liquidity` active throughout.
fn lp_fees_paid(snapshot: &PoolSnapshot, events: &[TimedEvent], liquidity: u128) -> (U256, U256) {
    let mut pool = Pool::from_snapshot(snapshot.clone());
    for event in events {
        pool.apply_event(&event.event, Replay::Strict, &mut ())
            .unwrap();
    }
    let (growth0, growth1) = pool.fee_growth_global();
    (
        fees_earned(growth0, U256::ZERO, liquidity),
        fees_earned(growth1, U256::ZERO, liquidity),
    )
}

#[test]
fn a_position_larger_than_the_pool_never_earns_more_than_the_pool_paid() {
    // The pool holds a thousandth of the liquidity the capital buys in the
    // range: the passive model's assumption is badly broken, and the fee
    // share must still stay below the whole fee.
    let thin = 10u128.pow(18);
    let blocks: Vec<[Step; 1]> = THERE_AND_BACK.iter().map(|t| [Step::To(*t)]).collect();
    let blocks: Vec<&[Step]> = blocks.iter().map(|b| b.as_slice()).collect();
    let (snapshot, events) = history_of(thin, &[], &blocks);
    let out = run(snapshot.clone(), &events, &settings(FixedRule::Wait)).unwrap();
    let first = &out.campaigns[0];
    assert_eq!(first.passes, 2);
    assert!(first.amount_in > 0.999_999, "{first:?}");

    let (fees0, fees1) = lp_fees_paid(&snapshot, &events, thin);
    // Both tokens are worth about the same here: the price stays within
    // 60 ticks of 1.
    let paid = (to_f64(fees0) + to_f64(fees1)) / 1e18;
    assert!(first.fees > 0.0);
    assert!(first.fees < paid * 1.01, "earned {} of {paid}", first.fees);
    // Taking each step's fee times the position's liquidity over the
    // pool's would have paid it about 0.1%, two passes at 0.05%: hundreds
    // of times what the pool's LPs were paid.
    assert!(first.fees < 0.000_01, "{}", first.fees);
    assert_conserved(&out);
}

#[test]
fn the_fee_share_counts_the_position_in() {
    let fee = U256::from(1_000_000u64);
    let share = |pool: u128, position: u128| {
        fees_earned(
            diluted_fee_growth(fee, pool, position),
            U256::ZERO,
            position,
        )
    };
    // Equal liquidity: half the fee. A third, rounded down.
    assert_eq!(share(1_000, 1_000), U256::from(500_000u64));
    assert_eq!(share(2, 1), U256::from(333_333u64));
    // A small position: its pro-rata share.
    assert_eq!(share(999_000, 1_000), U256::from(1_000u64));
    // A huge one: nearly all of it, never more.
    assert!(share(1, 10u128.pow(30)) < fee);
    // Where the pool had nothing, nothing was earned.
    assert_eq!(share(0, 1_000), U256::ZERO);
    assert_eq!(diluted_fee_growth(U256::ZERO, 5, 5), U256::ZERO);
}

#[test]
fn the_thinnest_liquidity_of_a_range_is_read_from_the_tick_map() {
    let mut pool = Pool::new(500, 10);
    pool.initialize(get_sqrt_ratio_at_tick(0).unwrap()).unwrap();
    pool.mint(-100, 100, 1_000).unwrap();
    pool.mint(20, 40, 500).unwrap();
    pool.mint(-60, -40, 300).unwrap();
    pool.mint(-5_000, 5_000, 1).unwrap();
    assert_eq!(pool.liquidity(), 1_001);

    // Above the price.
    assert_eq!(thinnest_liquidity(&pool, 20, 40), 1_501);
    assert_eq!(thinnest_liquidity(&pool, 20, 60), 1_001);
    assert_eq!(thinnest_liquidity(&pool, 40, 60), 1_001);
    assert_eq!(thinnest_liquidity(&pool, 90, 120), 1);
    // Below it.
    assert_eq!(thinnest_liquidity(&pool, -60, -40), 1_301);
    assert_eq!(thinnest_liquidity(&pool, -60, -20), 1_001);
    assert_eq!(thinnest_liquidity(&pool, -120, -60), 1);
    // A gap in the pool's liquidity reads zero.
    assert_eq!(thinnest_liquidity(&pool, 4_990, 5_010), 0);
    // Around the price.
    assert_eq!(thinnest_liquidity(&pool, -10, 30), 1_001);

    // With the price on an initialized tick, reached from above: the
    // pool reports the tick below it and the liquidity below it.
    let limit = get_sqrt_ratio_at_tick(-40).unwrap();
    pool.swap(true, I256::try_from(10u64.pow(9)).unwrap(), limit, &mut ())
        .unwrap();
    assert_eq!(pool.tick(), -41);
    assert_eq!(pool.liquidity(), 1_301);
    assert_eq!(thinnest_liquidity(&pool, 20, 40), 1_501);
    assert_eq!(thinnest_liquidity(&pool, -40, -20), 1_001);
    assert_eq!(thinnest_liquidity(&pool, -60, -40), 1_301);
}

#[test]
fn an_entry_is_capped_at_its_share_of_the_liquidity_in_its_range() {
    let (snapshot, events) = history(&THERE_AND_BACK);
    let uncapped = run(snapshot.clone(), &events, &settings(FixedRule::Wait)).unwrap();
    assert_eq!(uncapped.summary.capped_entries, 0);

    // The capital buys about a tenth of a percent of the range's
    // liquidity. At a cap of a hundredth of a percent a tenth of it goes
    // in, and the rest stays as cash.
    let capped = Settings {
        max_share_of_in_range_liquidity: Some(0.000_1),
        ..settings(FixedRule::Wait)
    };
    let out = run(snapshot.clone(), &events, &capped).unwrap();
    assert!(out.summary.capped_entries >= 1);
    let first = &out.campaigns[0];
    assert!((0.099_9..0.100_1).contains(&first.amount_in), "{first:?}");
    // The smaller position earns the same on what went in.
    let gain = first.result / first.amount_in;
    assert!((0.000_95..0.001_05).contains(&gain), "{gain}");
    let full = &uncapped.campaigns[0];
    assert!((first.result / full.result - first.amount_in / full.amount_in).abs() < 1e-4);
    assert_conserved(&out);

    // A cap the capital stays under changes nothing.
    let loose = Settings {
        max_share_of_in_range_liquidity: Some(0.05),
        ..settings(FixedRule::Wait)
    };
    let out = run(snapshot, &events, &loose).unwrap();
    assert_eq!(out.summary.capped_entries, 0);
    assert_eq!(out.campaigns, uncapped.campaigns);
}

#[test]
fn a_share_cap_that_is_not_a_share_is_refused() {
    let (snapshot, events) = history(&[5]);
    for share in [0.0, -0.1, 1.5, f64::NAN] {
        let odd = Settings {
            max_share_of_in_range_liquidity: Some(share),
            ..settings(FixedRule::Wait)
        };
        let error = run(snapshot.clone(), &events, &odd)
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("universe.max_share_of_in_range_liquidity"),
            "{error}"
        );
    }
}

/// The usual entry at [-20, 0), price inside it at -10 when the `during`
/// block happens, then down through it and back up above it.
fn with_a_flash(during: &[Step]) -> Output {
    let blocks: [&[Step]; 8] = [
        &[Step::To(5)],
        &[Step::To(6)],
        &[Step::To(-10)],
        during,
        &[Step::To(-50)],
        &[Step::To(-52)],
        &[Step::To(30)],
        &[Step::To(31)],
    ];
    let (snapshot, events) = history_of(DEEP, &[], &blocks);
    run(snapshot, &events, &settings(FixedRule::Wait)).unwrap()
}

#[test]
fn a_flash_loan_pays_the_position_its_share_while_price_is_in_its_range() {
    // The position: the whole capital in token1 over [-20, 0).
    let liquidity = get_liquidity_for_amount1(
        get_sqrt_ratio_at_tick(-20).unwrap(),
        get_sqrt_ratio_at_tick(0).unwrap(),
        U256::from(10u128.pow(18)),
    )
    .unwrap();
    let share = |lp_fee: u128| {
        let growth = diluted_fee_growth(U256::from(lp_fee), DEEP, liquidity);
        to_f64(fees_earned(growth, U256::ZERO, liquidity)) / 1e18
    };
    let paid = 10u128.pow(15);

    // Without a protocol fee the LPs get all of it.
    let none = with_a_flash(&[Step::Flash(0, 0)]);
    let flash = with_a_flash(&[Step::Flash(0, paid)]);
    assert_eq!(none.campaigns[0].exit_reason, "round_trip_complete");
    let earned = flash.campaigns[0].fees - none.campaigns[0].fees;
    assert!(earned > 0.0);
    assert!(
        (earned - share(paid)).abs() < 1e-15,
        "{earned} {}",
        share(paid)
    );
    let gained = flash.campaigns[0].result - none.campaigns[0].result;
    assert!((gained - earned).abs() < 1e-15);
    assert_conserved(&flash);

    // With one: the protocol takes a quarter first, as the pool does.
    let none = with_a_flash(&[Step::FeeProtocol(4, 4), Step::Flash(0, 0)]);
    let flash = with_a_flash(&[Step::FeeProtocol(4, 4), Step::Flash(0, paid)]);
    let earned = flash.campaigns[0].fees - none.campaigns[0].fees;
    assert_eq!(
        after_protocol(U256::from(paid), 4),
        U256::from(paid - paid / 4)
    );
    assert!((earned - share(paid - paid / 4)).abs() < 1e-15, "{earned}");

    // A flash while price is outside the range pays the position nothing.
    let outside = |during: &[Step]| {
        let blocks: [&[Step]; 7] = [
            &[Step::To(5)],
            &[Step::To(6)],
            during,
            &[Step::To(-50)],
            &[Step::To(-52)],
            &[Step::To(30)],
            &[Step::To(31)],
        ];
        let (snapshot, events) = history_of(DEEP, &[], &blocks);
        run(snapshot, &events, &settings(FixedRule::Wait)).unwrap()
    };
    let a = outside(&[Step::Flash(0, 0)]);
    let b = outside(&[Step::Flash(paid, paid)]);
    assert_eq!(a.campaigns[0].fees, b.campaigns[0].fees);
}

#[test]
fn gas_owed_after_a_flip_counts_against_the_equity() {
    // The flip at -50 puts everything back into the pool, so the free
    // capital cannot pay its gas and it is owed until the round trip ends.
    let (snapshot, events) = history(&THERE_AND_BACK);
    let free = run(snapshot.clone(), &events, &settings(FixedRule::Flip)).unwrap();
    let gas = Settings {
        gas: priced(),
        ..settings(FixedRule::Flip)
    };
    let out = run(snapshot, &events, &gas).unwrap();
    let (a, b) = (&free.series[3], &out.series[3]);
    assert_eq!((a.state.as_str(), b.state.as_str()), ("flipped", "flipped"));
    // Open 0.005, paid; close and open again 0.008, owed. The gas set
    // aside at entry also left a slightly smaller position, marked a
    // little differently.
    let spent = a.equity - b.equity;
    assert!((spent - 0.013).abs() < 1e-4, "{spent}");
    // Settled at the end of the round trip, with the last close: the
    // campaign paid all of it.
    let paid = out.campaigns[0].gas;
    assert!((paid - 0.016).abs() < 1e-12, "{paid}");
    assert_conserved(&out);
}

#[test]
fn gas_is_charged_in_exact_integers() {
    // 100,000 gas at 2.3 gwei is 230,000 gwei. Multiplied out in floating
    // point it comes to just under, which was cut down to the unit below.
    let (snapshot, events) = history(&THERE_AND_BACK);
    let gas = Settings {
        gas: GasModel {
            open_units: 100_000,
            close_units: 0,
            swap_units: 0,
            gas_price_gwei: 2.3,
            value_raw_per_eth: 1e18,
        },
        ..settings(FixedRule::Wait)
    };
    let out = run(snapshot, &events, &gas).unwrap();
    assert_eq!(out.campaigns[0].gas, 230_000_000_000_000.0 / 1e18);
    assert_conserved(&out);
}

#[test]
fn a_gas_price_that_is_not_a_price_is_refused() {
    let (snapshot, events) = history(&[5]);
    let odd = Settings {
        gas: GasModel {
            gas_price_gwei: -1.0,
            ..priced()
        },
        ..settings(FixedRule::Wait)
    };
    let error = run(snapshot, &events, &odd).unwrap_err().to_string();
    assert!(error.contains("costs.gas_price_gwei"), "{error}");
}

#[test]
fn a_share_of_an_amount_is_exact() {
    // Beyond what a 64-bit float holds to the unit.
    let raw = 2u128.pow(100) + 12_345;
    let amount = U256::from(raw);
    assert_eq!(share_of(amount, 1_000_000), amount);
    assert_eq!(share_of(amount, 500_000), U256::from(raw / 2));
    assert_eq!(share_of(amount, 250_000), U256::from(raw / 4));
    assert_eq!(share_of(amount, 0), U256::ZERO);
}

// --- a price exactly on a range's edge --------------------------------------

#[test]
fn a_price_exactly_on_the_far_bound_is_decided_like_any_beyond_it() {
    // Value in token1: the range [-20, 0) sits below the price. Price drops
    // through it to -50 and comes back up to exactly the price of -20 in
    // the same block. The position is wholly token0 there, while the
    // pool's tick reads -20, inside the range.
    let blocks: [&[Step]; 4] = [
        &[Step::To(5)],
        &[Step::To(6)],
        &[Step::To(-50), Step::To(-20)],
        &[Step::To(-25)],
    ];
    let (snapshot, events) = history_of(DEEP, &[], &blocks);
    let out = run(snapshot, &events, &settings(FixedRule::Close)).unwrap();
    assert_eq!(out.series[2].tick, -20);
    assert_eq!(out.series[2].state, "beyond");
    assert_eq!(out.decisions[0].action, "close", "{:?}", out.decisions);
    assert_eq!(out.campaigns[0].exit_reason, "patience_close");
    assert_eq!(out.campaigns[0].closed_block, events[4].block);

    // The mirror image, with value in token0 and the range [0, 20) above
    // the price, which runs up through it and comes back down to exactly
    // the price of 20. Liquidity starting at 20 makes that an initialized
    // tick, so the pool reports 19 there.
    let blocks: [&[Step]; 4] = [
        &[Step::To(-5)],
        &[Step::To(-6)],
        &[Step::To(50), Step::To(20)],
        &[Step::To(25)],
    ];
    let (snapshot, events) = history_of(DEEP, &[(20, 1_000, DEEP)], &blocks);
    let mirrored = Settings {
        value: Token::Zero,
        ..settings(FixedRule::Close)
    };
    let out = run(snapshot, &events, &mirrored).unwrap();
    let first = &out.campaigns[0];
    assert_eq!((first.tick_lower, first.tick_upper), (0, 20));
    assert_eq!(out.series[2].tick, 19);
    assert_eq!(out.series[2].state, "beyond");
    assert_eq!(out.decisions[0].action, "close", "{:?}", out.decisions);
    assert_eq!(first.exit_reason, "patience_close");
}

// --- what a flip records ----------------------------------------------------

#[test]
fn a_flip_records_the_range_it_went_to() {
    // Two hours of latency: decided at -50, carried out once price is at
    // -52, where the range next to the price starts one step lower.
    let (snapshot, events) = history(&THERE_AND_BACK);
    let slow = Settings {
        latency_blocks: 600,
        ..settings(FixedRule::Flip)
    };
    let out = run(snapshot, &events, &slow).unwrap();
    let flip = &out.decisions[0];
    assert_eq!((flip.action.as_str(), flip.tick), ("flip", -50));
    assert_eq!(
        (flip.target_lower, flip.target_upper),
        (Some(-50), Some(-30))
    );
    assert_eq!(out.campaigns[0].flips, 1);
    assert_eq!(out.campaigns[0].exit_reason, "round_trip_complete");
    assert_conserved(&out);
}

#[test]
fn a_flip_that_cannot_open_ends_the_campaign_as_flip_failed() {
    // One unit of the capital in a range 20,000 ticks wide: the unit of
    // token0 it turns into far below buys no liquidity in the flip's
    // range, so the flip cannot open and everything goes back to the
    // value token.
    let (snapshot, events) = history(&[5, 6, -20_050, -20_060]);
    let tiny = Settings {
        width: 20_000,
        max_gap: 20_000,
        capital: U256::from(1_000u16),
        capital_share: 0.001,
        ..settings(FixedRule::Flip)
    };
    let out = run(snapshot, &events, &tiny).unwrap();
    let flip = &out.decisions[0];
    assert_eq!(flip.action, "flip");
    assert_eq!(
        (flip.target_lower, flip.target_upper),
        (Some(-20_040), Some(-40))
    );
    let first = &out.campaigns[0];
    assert_eq!(first.exit_reason, "flip_failed");
    assert_eq!(first.flips, 0);
    assert_eq!(first.closed_block, events[3].block);
    assert_conserved(&out);
}

#[test]
fn what_a_flips_range_cannot_take_goes_back_with_a_charged_swap() {
    // A few units in a range 20,000 ticks wide, so that the flip's range
    // cannot take all of the token0 it gets: a unit is left over. It stays
    // in token0 through the flip and goes back with a swap when the round
    // trip is done, and that swap is charged like any other.
    let (snapshot, events) = history(&[5, 6, -20_050, -20_060, 10, 11]);
    let few = Settings {
        width: 20_000,
        max_gap: 20_000,
        capital: U256::from(10_000u16),
        capital_share: 0.000_5,
        // One unit of gas costs one unit of the value token.
        gas: GasModel {
            open_units: 100,
            close_units: 100,
            swap_units: 7,
            gas_price_gwei: 1.0,
            value_raw_per_eth: 1e9,
        },
        ..settings(FixedRule::Flip)
    };
    let out = run(snapshot, &events, &few).unwrap();
    let first = &out.campaigns[0];
    assert_eq!(
        (first.flips, first.exit_reason.as_str()),
        (1, "round_trip_complete")
    );
    // Open; close and open for the flip; close and swap at the end.
    assert_eq!(
        (first.gas * 1e18).round(),
        100.0 + 200.0 + 107.0,
        "{first:?}"
    );
    assert_conserved(&out);
}

#[test]
fn results_are_given_in_dollars_as_well_as_in_the_value_token() {
    // The other token is the dollar: dollars are value tokens times price.
    let out = there_and_back(FixedRule::Wait);
    let s = &out.summary;
    let first = &out.series[0];
    let last = out.series.last().unwrap();
    assert!((s.start_price - first.price).abs() < 1e-12);
    assert!(s.end_price > 0.0 && (s.end_price - last.price).abs() / last.price < 0.01);
    assert!((s.start_usd - s.start_capital * s.start_price).abs() < 1e-9);
    assert!((s.end_usd - s.end_capital * s.end_price).abs() < 1e-9);
    let usd = (s.end_usd / s.start_usd - 1.0) * 100.0;
    assert!((s.result_pct_usd - usd).abs() < 1e-9);
    let hold = (s.end_price / s.start_price - 1.0) * 100.0;
    assert!((s.hold_value_pct_usd - hold).abs() < 1e-9);
    for row in &out.series {
        assert!((row.equity_usd - row.equity * row.price).abs() < 1e-9);
        assert!((row.hold_usd - row.hold * row.price).abs() < 1e-9);
    }

    // The value token is the dollar: both measures are the same, and
    // holding it does nothing in dollars.
    let (snapshot, events) = history(&THERE_AND_BACK);
    let dollars = Settings {
        dollar: Dollar::Value,
        ..settings(FixedRule::Wait)
    };
    let out = run(snapshot, &events, &dollars).unwrap();
    let s = &out.summary;
    assert!((s.result_pct_usd - s.result_pct).abs() < 1e-9);
    assert_eq!(s.hold_value_pct_usd, 0.0);
    assert!(out
        .series
        .iter()
        .all(|row| (row.equity_usd - row.equity).abs() < 1e-12));
}

/// Thirty wild hours swinging 800 ticks, then thirty calmer hours
/// crossing the range next to the price back and forth.
fn wild_then_calm() -> (PoolSnapshot, Vec<TimedEvent>) {
    let mut targets = Vec::new();
    for i in 0..30 {
        targets.push(if i % 2 == 0 { 400 } else { -400 });
    }
    for i in 0..30 {
        targets.push(if i % 2 == 0 { 25 } else { -25 });
    }
    history(&targets)
}

#[test]
fn the_volatility_gate_keeps_the_bot_out_of_a_wild_day_and_lets_it_in_on_a_calm_one() {
    let (snapshot, events) = wild_then_calm();
    let open = run(snapshot.clone(), &events, &settings(FixedRule::Close)).unwrap();
    assert!(open.campaigns[0].opened_time < T0 + 5 * HOUR);

    let gated = Settings {
        // A day's volatility of at most 20 widths: the calm part is about
        // 12, the wild part about 200.
        max_vol_to_width: Some(20.0),
        ..settings(FixedRule::Close)
    };
    let out = run(snapshot.clone(), &events, &gated).unwrap();
    assert!(!out.campaigns.is_empty(), "the calm part lets it in");
    // Nothing starts until the wild hours have left the day's window.
    assert!(
        out.campaigns.iter().all(|c| c.opened_time > T0 + 30 * HOUR),
        "{:?}",
        out.campaigns
            .iter()
            .map(|c| (c.opened_time - T0) / HOUR)
            .collect::<Vec<_>>()
    );

    // An absolute limit in ticks works the same way; one nothing meets
    // keeps the bot out throughout.
    let strict = Settings {
        max_daily_vol_ticks: Some(1.0),
        ..settings(FixedRule::Close)
    };
    assert!(run(snapshot, &events, &strict)
        .unwrap()
        .campaigns
        .is_empty());
}

#[test]
fn an_entry_too_small_for_its_gas_is_not_made() {
    let (snapshot, events) = history(&THERE_AND_BACK);
    // A round trip costs 0.01 of the value token here; the capital is 1.
    let costly = Settings {
        gas: priced(),
        min_gas_multiple: Some(200.0),
        ..settings(FixedRule::Wait)
    };
    let out = run(snapshot.clone(), &events, &costly).unwrap();
    assert!(out.campaigns.is_empty());
    assert_eq!(out.summary.end_capital, out.summary.start_capital);
    let affordable = Settings {
        min_gas_multiple: Some(50.0),
        ..costly
    };
    assert!(!run(snapshot, &events, &affordable)
        .unwrap()
        .campaigns
        .is_empty());
}

#[test]
fn a_position_stranded_longer_than_the_longest_wait_is_closed() {
    let (snapshot, events) = history(&THERE_AND_BACK);
    let patient = run(snapshot.clone(), &events, &settings(FixedRule::Wait)).unwrap();
    assert_eq!(patient.campaigns[0].exit_reason, "round_trip_complete");

    let limited = Settings {
        max_seconds_beyond: Some(HOUR),
        ..settings(FixedRule::Wait)
    };
    let out = run(snapshot, &events, &limited).unwrap();
    assert_eq!(out.campaigns[0].exit_reason, "patience_timeout");
    let closing = out
        .decisions
        .iter()
        .find(|d| d.action == "close")
        .expect("the close is recorded");
    assert!(closing.seconds_stranded >= HOUR);
    assert_eq!(closing.expected_wait, None);
    assert_conserved(&out);
}

#[test]
fn a_resting_position_steps_aside_when_the_day_turns_wild() {
    // A calm day and more crossing the range below the price every hour,
    // then wild swings far above it, where the range holds only the
    // value token. Trading starts after the first day, so every range has
    // a day of crossings behind it.
    let mut targets = Vec::new();
    for i in 0..30 {
        targets.push(if i % 2 == 0 { 5 } else { -25 });
    }
    for i in 0..30 {
        targets.push(if i % 2 == 0 { 300 } else { 1_100 });
    }
    let (snapshot, events) = history(&targets);
    let calm = Settings {
        // Moving away is not what this is about.
        max_gap: 100_000,
        lookbacks_days: vec![1, 2],
        trade_from: T0 + 25 * HOUR,
        ..settings(FixedRule::Wait)
    };
    let stays = run(snapshot.clone(), &events, &calm).unwrap();
    assert!(stays
        .campaigns
        .iter()
        .all(|c| c.exit_reason != "stepped_aside"));

    let careful = Settings {
        exit_vol_to_width: Some(10.0),
        ..calm.clone()
    };
    let out = run(snapshot.clone(), &events, &careful).unwrap();
    let aside = out
        .campaigns
        .iter()
        .position(|c| c.exit_reason == "stepped_aside")
        .expect("a resting range steps aside once the day turns wild");
    assert!(out.campaigns[aside].closed_time > T0 + 30 * HOUR);
    assert_conserved(&out);
    // Alone, the exit rule lets the bot straight back in, to step aside
    // again an hour later.
    assert!(out.campaigns.len() > aside + 5);

    // With the entry gate at the same level it stays out while the day
    // is wild.
    let paired = Settings {
        max_vol_to_width: Some(10.0),
        ..careful
    };
    let out = run(snapshot, &events, &paired).unwrap();
    let aside = out
        .campaigns
        .iter()
        .position(|c| c.exit_reason == "stepped_aside")
        .expect("it steps aside");
    assert!(out.campaigns[aside].passes >= 2, "after round trips");
    assert_eq!(out.campaigns.len(), aside + 1, "and does not come back");
    assert_conserved(&out);
}
