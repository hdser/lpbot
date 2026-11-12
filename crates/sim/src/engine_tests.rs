use alloy_primitives::{I256, U256};
use lpbot_data::lake::TimedEvent;
use lpbot_data::synth::{generate, SynthConfig};
use lpbot_risk::LossLimit;
use lpbot_strategy::FixedRule;
use lpbot_venues::uniswap_v3::events::PoolEvent;
use lpbot_venues::uniswap_v3::pool::{Pool, PoolSnapshot, SwapEvent};
use lpbot_venues::uniswap_v3::tick_math::get_sqrt_ratio_at_tick;

use crate::engine::{run, GasModel, Output, Settings, Token};

const T0: u64 = 1_800_000_000;
const HOUR: u64 = 3_600;

/// A pool with deep, even liquidity at tick 0, and the events of moving
/// its price to each target tick in turn, one hour and 300 blocks apart.
fn history(targets: &[i32]) -> (PoolSnapshot, Vec<TimedEvent>) {
    let mut pool = Pool::new(500, 10);
    pool.initialize(get_sqrt_ratio_at_tick(0).unwrap()).unwrap();
    pool.mint(-10_000, 10_000, 10u128.pow(24)).unwrap();
    let snapshot = pool.snapshot();

    let huge = I256::try_from(10u128.pow(32)).unwrap();
    let mut events = Vec::new();
    for (i, target) in targets.iter().enumerate() {
        let limit = get_sqrt_ratio_at_tick(*target).unwrap();
        let zero_for_one = limit < pool.sqrt_price_x96();
        let amounts = pool.swap(zero_for_one, huge, limit, &mut ()).unwrap();
        let hours = i as u64 + 1;
        events.push(TimedEvent {
            block: 1_000 + 300 * hours,
            time: T0 + HOUR * hours,
            log_index: 0,
            event: PoolEvent::Swap(SwapEvent {
                amount0: amounts.amount0,
                amount1: amounts.amount1,
                sqrt_price_x96: pool.sqrt_price_x96(),
                liquidity: pool.liquidity(),
                tick: pool.tick(),
            }),
        });
    }
    (snapshot, events)
}

fn settings(rule: FixedRule) -> Settings {
    Settings {
        value: Token::One,
        decimals0: 18,
        decimals1: 18,
        width: 20,
        max_gap: 20,
        capital: U256::from(10u128.pow(18)),
        capital_share: 1.0,
        loss_limit: LossLimit::new(50.0).unwrap(),
        rule,
        decide_every: HOUR,
        check_every: HOUR,
        latency_blocks: 1,
        lookbacks_days: vec![7, 30],
        move_margin: 1.5,
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
            rule,
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
