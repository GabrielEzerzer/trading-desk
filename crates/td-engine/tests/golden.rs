//! Golden tests: small hand-built bar series where every fill price, fee, cash, position and
//! equity value is worked out by hand in the comments and asserted.

mod common;

use std::sync::{Arc, Mutex};

use common::*;
use td_core::{Candle, Liquidity, OrderType, Side, Timeframe};
use td_engine::{
    Context, EngineError, OrderStatus, Params, Session, Strategy, build_strategy, run_backtest,
};

fn run(inst: td_core::Instrument, slippage: f64, bars: &[Candle], script: Box<Script>) -> Session {
    run_backtest(script, inst, Timeframe::M1, bars, &[], config(slippage)).unwrap()
}

fn equity(s: &Session) -> Vec<f64> {
    s.equity().iter().map(|p| p.equity).collect()
}

#[test]
fn market_orders_fill_at_the_next_open_with_slippage_and_taker_fee() {
    let i = spot();
    let bars = [
        bar(0, 100.0, 101.0, 99.0, 100.0),
        bar(1, 100.0, 102.0, 99.5, 101.0),
        bar(2, 105.0, 106.0, 104.0, 105.5),
    ];
    let (script, log) = Script::new(vec![
        (0, req(&i, Side::Buy, 10.0, OrderType::Market)),
        (1, req(&i, Side::Sell, 10.0, OrderType::Market)),
    ]);
    let s = run(i, 10.0, &bars, script);
    assert!(log.lock().unwrap().iter().all(|r| r.is_ok()));

    let f = s.fills();
    assert_eq!(f.len(), 2);
    // Buy on bar 1: 100 * 1.001 = 100.10. Notional 1001, fee 0.2% = 2.002.
    assert_eq!(f[0].time, MIN);
    assert_eq!(f[0].side, Side::Buy);
    assert_eq!(f[0].liquidity, Liquidity::Taker);
    approx(f[0].price, 100.10);
    approx(f[0].qty, 10.0);
    approx(f[0].fee, 2.002);
    // Sell on bar 2: 105 * 0.999 = 104.895, rounded down to the tick: 104.89. Fee 0.2% of 1048.9.
    assert_eq!(f[1].time, 2 * MIN);
    approx(f[1].price, 104.89);
    approx(f[1].fee, 2.0978);

    // Bar 0: nothing filled. Bar 1: cash 10000 - 1001 - 2.002 = 8996.998, plus 10 * 101.
    // Bar 2: cash 8996.998 + 1048.9 - 2.0978 = 10043.8002, flat.
    let e = equity(&s);
    approx(e[0], 10_000.0);
    approx(e[1], 10_006.998);
    approx(e[2], 10043.8002);
    let p = s.portfolio();
    approx(p.cash(), 10043.8002);
    assert_eq!(p.position(), 0.0);
    approx(p.realised_pnl(), 10.0 * (104.89 - 100.10));
    approx(p.fees_paid(), 2.002 + 2.0978);
    assert!(s.orders().iter().all(|o| o.status == OrderStatus::Filled));
}

#[test]
fn limit_buys_fill_at_the_open_on_a_gap_and_at_the_limit_on_a_touch() {
    let i = spot();
    let bars = [
        bar(0, 100.0, 101.0, 99.0, 100.0),
        bar(1, 97.0, 99.0, 96.0, 98.0),
        bar(2, 96.0, 97.0, 94.5, 95.0),
    ];
    let (script, _) = Script::new(vec![
        (0, req(&i, Side::Buy, 5.0, OrderType::Limit { price: 98.0 })),
        (0, req(&i, Side::Buy, 1.0, OrderType::Limit { price: 95.0 })),
        (0, req(&i, Side::Buy, 1.0, OrderType::Limit { price: 90.0 })),
    ]);
    let s = run(i, 10.0, &bars, script);

    let f = s.fills();
    assert_eq!(f.len(), 2);
    // Bar 1 opens at 97, already below the 98 limit: fills at 97, maker 0.1% of 485.
    assert_eq!((f[0].order_id.0, f[0].time), (1, MIN));
    approx(f[0].price, 97.0);
    approx(f[0].fee, 0.485);
    assert_eq!(f[0].liquidity, Liquidity::Maker);
    // Bar 1's low 96 never reaches 95. Bar 2's low 94.5 does, after opening at 96: fills at 95.
    assert_eq!((f[1].order_id.0, f[1].time), (2, 2 * MIN));
    approx(f[1].price, 95.0);
    approx(f[1].fee, 0.095);

    // Bar 1: 10000 - 485.485 + 5 * 98. Bar 2: cash 10000 - 485.485 - 95.095 = 9419.42, plus 6 * 95.
    let e = equity(&s);
    approx(e[1], 10_004.515);
    approx(e[2], 9_989.42);
    approx(s.portfolio().cash(), 9_419.42);
    approx(s.portfolio().position(), 6.0);
    // The 90 limit was never reached and is still resting.
    assert_eq!(s.orders()[2].status, OrderStatus::Open);
    assert_eq!(s.open_orders().len(), 1);
}

#[test]
fn limit_sell_fills_at_the_open_on_a_gap_up() {
    let i = perp();
    let bars = [
        bar(0, 100.0, 101.0, 99.0, 100.0),
        bar(1, 103.0, 104.0, 102.5, 103.5),
    ];
    let (script, _) = Script::new(vec![(
        0,
        req(&i, Side::Sell, 1.0, OrderType::Limit { price: 102.0 }),
    )]);
    let s = run(i, 0.0, &bars, script);
    // Opens at 103, above the 102 limit: a short of 1 at 103, maker fee 0.103.
    approx(s.fills()[0].price, 103.0);
    approx(s.fills()[0].fee, 0.103);
    // Perpetual: cash 10000 - 0.103; unrealised -1 * (103.5 - 103) = -0.5.
    approx(s.portfolio().cash(), 9_999.897);
    approx(s.portfolio().position(), -1.0);
    approx(equity(&s)[1], 9_999.397);
}

#[test]
fn stops_trigger_on_the_range_and_fill_at_the_worse_of_open_and_trigger() {
    let i = perp();
    let bars = [
        bar(0, 100.0, 101.0, 99.0, 100.0),
        bar(1, 101.0, 104.0, 100.0, 103.0),
        bar(2, 103.0, 107.0, 102.0, 106.0),
        bar(3, 98.0, 99.0, 97.0, 98.5),
    ];
    let (script, _) = Script::new(vec![
        (
            0,
            req(&i, Side::Buy, 2.0, OrderType::Stop { trigger: 105.0 }),
        ),
        (
            2,
            reduce_only(req(&i, Side::Sell, 2.0, OrderType::Stop { trigger: 100.0 })),
        ),
    ]);
    let s = run(i, 10.0, &bars, script);

    let f = s.fills();
    assert_eq!(f.len(), 2);
    // Bar 1's high 104 stays under 105. Bar 2's high 107 triggers: max(103, 105) = 105, plus
    // 10 bps = 105.105, rounded up to 105.11. Taker fee 0.2% of 210.22 = 0.42044.
    assert_eq!(f[0].time, 2 * MIN);
    approx(f[0].price, 105.11);
    approx(f[0].fee, 0.42044);
    assert_eq!(f[0].liquidity, Liquidity::Taker);
    // Perpetual equity on bar 2: cash 9999.57956 plus 2 * (106 - 105.11).
    let e = equity(&s);
    approx(e[1], 10_000.0);
    approx(e[2], 10001.35956);
    // Bar 3 gaps to 98, below the 100 trigger: min(98, 100) = 98, less 10 bps = 97.902, rounded
    // down to 97.90. Fee 0.2% of 195.8 = 0.3916. Realised 2 * (97.90 - 105.11) = -14.42.
    approx(f[1].price, 97.90);
    approx(f[1].fee, 0.3916);
    let p = s.portfolio();
    approx(p.realised_pnl(), -14.42);
    assert_eq!(p.position(), 0.0);
    approx(p.cash(), 10_000.0 - 0.42044 - 14.42 - 0.3916);
    approx(e[3], 9984.76796);
}

#[test]
fn stop_limits_fill_on_trigger_when_marketable_and_rest_when_gapped_past() {
    let i = spot();
    let bars = [
        bar(0, 100.0, 101.0, 99.0, 100.0),
        bar(1, 104.0, 105.5, 103.0, 105.0),
        bar(2, 104.5, 105.0, 103.2, 104.0),
    ];
    let stop_limit = |trigger, price| OrderType::StopLimit { trigger, price };
    let (script, _) = Script::new(vec![
        (0, req(&i, Side::Buy, 1.0, stop_limit(105.0, 106.0))),
        (0, req(&i, Side::Buy, 1.0, stop_limit(105.0, 105.05))),
        (0, req(&i, Side::Buy, 1.0, stop_limit(103.0, 103.5))),
    ]);
    let s = run(i, 10.0, &bars, script);

    let f = s.fills();
    assert_eq!(f.len(), 3);
    // Order 1: bar 1's high 105.5 triggers at max(104, 105) = 105; 105.105 rounds up to 105.11,
    // inside the 106 limit. Taker fee 0.21022.
    assert_eq!((f[0].order_id.0, f[0].time), (1, MIN));
    approx(f[0].price, 105.11);
    approx(f[0].fee, 0.21022);
    assert_eq!(f[0].liquidity, Liquidity::Taker);
    // Order 2: same trigger, but slippage is capped at the 105.05 limit. Fee 0.2101.
    assert_eq!((f[1].order_id.0, f[1].time), (2, MIN));
    approx(f[1].price, 105.05);
    approx(f[1].fee, 0.2101);
    // Order 3: bar 1 opens at 104, past the 103 trigger and above the 103.5 limit, so it rests.
    // Bar 1's low of 103 is not taken as a fill: it may have come before the trigger. On bar 2 it
    // is a plain limit: the low 103.2 reaches 103.5, open 104.5 is above it, so it fills at 103.5
    // as a maker, fee 0.1035.
    assert_eq!((f[2].order_id.0, f[2].time), (3, 2 * MIN));
    approx(f[2].price, 103.5);
    approx(f[2].fee, 0.1035);
    assert_eq!(f[2].liquidity, Liquidity::Maker);

    // Bar 1: 10000 - 105.32022 - 105.2601 + 2 * 105. Bar 2: cash less 103.6035, plus 3 * 104.
    let e = equity(&s);
    approx(e[1], 9999.41968);
    approx(s.portfolio().cash(), 9685.81618);
    approx(e[2], 9997.81618);
}

#[test]
fn ioc_orders_only_see_the_open_and_cancel_if_unfilled() {
    let i = spot();
    let bars = [
        bar(0, 100.0, 101.0, 99.0, 100.0),
        bar(1, 100.0, 101.0, 98.0, 100.0),
    ];
    let (script, _) = Script::new(vec![
        (
            0,
            ioc(req(&i, Side::Buy, 1.0, OrderType::Limit { price: 99.0 })),
        ),
        (
            0,
            ioc(req(&i, Side::Buy, 1.0, OrderType::Limit { price: 101.0 })),
        ),
    ]);
    let s = run(i, 0.0, &bars, script);
    // Order 1: the low 98 passes 99 later in the bar, but an IOC order only trades on arrival, at
    // the open of 100, which is above the limit. It is cancelled.
    assert_eq!(
        s.orders()[0].status,
        OrderStatus::Cancelled("IOC order did not fill on its first bar".into())
    );
    // Order 2 is marketable at the open: fills at 100 as a taker, fee 0.2.
    assert_eq!(s.orders()[1].status, OrderStatus::Filled);
    let f = &s.fills()[0];
    approx(f.price, 100.0);
    approx(f.fee, 0.2);
    assert_eq!(f.liquidity, Liquidity::Taker);
    approx(s.portfolio().cash(), 9_899.8);
    assert!(s.open_orders().is_empty());
}

fn funding_session(side: Side) -> Session {
    let i = perp();
    const H: i64 = 3_600_000;
    let hour = |n: i64, o, h, l, c| Candle {
        open_time: n * H,
        ..bar(0, o, h, l, c)
    };
    let bars = [
        hour(0, 100.0, 100.0, 100.0, 100.0),
        hour(1, 100.0, 106.0, 99.0, 105.0),
        hour(2, 105.0, 111.0, 104.0, 110.0),
    ];
    // Unsorted on purpose; the event before the first bar falls in no bar and is not charged.
    let funding = [
        (2 * H, 0.0001),
        (H + 5_000, 0.0002),
        (1_000, 0.0005),
        (-5, 0.01),
    ];
    let (script, _) = Script::new(vec![(0, req(&i, side, 2.0, OrderType::Market))]);
    run_backtest(script, i, Timeframe::H1, &bars, &funding, config(0.0)).unwrap()
}

#[test]
fn funding_is_charged_to_longs_on_a_positive_rate() {
    let s = funding_session(Side::Buy);
    // Fill on bar 1 at 100, fee 0.4. The funding in bar 0 meets a flat position. Bar 1 charges
    // 2 * 105 * 0.0002 = 0.042, bar 2 charges 2 * 110 * 0.0001 = 0.022.
    let p = s.portfolio();
    approx(p.funding_paid(), 0.064);
    approx(p.cash(), 10_000.0 - 0.4 - 0.064);
    // Equity on bar 2: cash 9999.536 plus 2 * (110 - 100).
    approx(equity(&s)[1], 10_000.0 - 0.4 - 0.042 + 10.0);
    approx(equity(&s)[2], 10_019.536);
}

#[test]
fn funding_is_paid_to_shorts_on_a_positive_rate() {
    let s = funding_session(Side::Sell);
    let p = s.portfolio();
    approx(p.funding_paid(), -0.064);
    approx(p.cash(), 10_000.0 - 0.4 + 0.064);
    // Equity on bar 2: cash 9999.664 plus -2 * (110 - 100).
    approx(equity(&s)[2], 9_979.664);
}

#[test]
fn funding_outside_the_bar_or_on_spot_is_an_error() {
    let (script, _) = Script::new(vec![]);
    let mut s = Session::new(script, perp(), Timeframe::M1, config(0.0)).unwrap();
    let b = bar(1, 100.0, 100.0, 100.0, 100.0);
    assert_eq!(
        s.on_bar(b, &[(2 * MIN, 0.0001)]),
        Err(EngineError::FundingOutsideBar {
            time: 2 * MIN,
            bar_open: MIN
        })
    );
    assert!(matches!(
        s.on_bar(b, &[(MIN, f64::NAN)]),
        Err(EngineError::InvalidFundingRate { .. })
    ));
    assert!(s.equity().is_empty());

    let (script, _) = Script::new(vec![]);
    let mut s = Session::new(script, spot(), Timeframe::M1, config(0.0)).unwrap();
    assert_eq!(
        s.on_bar(b, &[(MIN, 0.0001)]),
        Err(EngineError::FundingOnSpot)
    );
}

#[test]
fn spot_cannot_short_or_sell_more_than_it_holds() {
    let i = spot();
    let bars = [
        bar(0, 100.0, 101.0, 99.0, 100.0),
        bar(1, 100.0, 101.0, 99.0, 100.0),
        bar(2, 100.0, 101.0, 99.0, 100.0),
    ];
    let (script, log) = Script::new(vec![
        (0, req(&i, Side::Buy, 1.0, OrderType::Market)),
        // The buy has not filled yet, so this is a short.
        (0, req(&i, Side::Sell, 1.0, OrderType::Market)),
        (1, req(&i, Side::Sell, 0.6, OrderType::Market)),
        // 1 held, 0.6 already being sold: 0.4 left.
        (
            1,
            req(&i, Side::Sell, 0.6, OrderType::Limit { price: 101.0 }),
        ),
    ]);
    let s = run(i, 0.0, &bars, script);
    let log = log.lock().unwrap();
    assert!(log[0].is_ok() && log[2].is_ok());
    for rejected in [&log[1], &log[3]] {
        let err = rejected.as_ref().unwrap_err();
        assert!(
            err.reason
                .starts_with("spot cannot sell more than it holds"),
            "{}",
            err.reason
        );
        assert!(matches!(
            s.orders()[err.id.0 as usize - 1].status,
            OrderStatus::Rejected(_)
        ));
    }
    assert_eq!(s.fills().len(), 2);
    approx(s.portfolio().position(), 0.4);
}

#[test]
fn orders_are_rounded_and_invalid_ones_rejected_with_a_reason() {
    let i = spot();
    let bars = [bar(0, 100.0, 101.0, 99.0, 100.0)];
    let (script, log) = Script::new(vec![
        (
            0,
            req(&i, Side::Buy, 1.23456, OrderType::Limit { price: 98.456 }),
        ),
        (0, req(&i, Side::Buy, 0.0004, OrderType::Market)),
        (
            0,
            req(&i, Side::Buy, 0.005, OrderType::Limit { price: 100.0 }),
        ),
    ]);
    let s = run(i, 0.0, &bars, script);
    let log = log.lock().unwrap();
    // Quantity rounds down to the lot, a buy limit rounds down to the tick.
    assert!(log[0].is_ok());
    assert_eq!(s.orders()[0].request.qty, 1.234);
    assert_eq!(
        s.orders()[0].request.order_type,
        OrderType::Limit { price: 98.45 }
    );
    // 0.0004 rounds to zero.
    assert!(log[1].as_ref().unwrap_err().reason.contains("qty"));
    // 0.005 * 100 = 0.5, under the minimum notional of 1.
    assert!(log[2].as_ref().unwrap_err().reason.contains("notional"));
    assert_eq!(s.open_orders().len(), 1);
}

#[test]
fn a_protective_stop_beats_a_take_profit_in_the_same_bar() {
    let i = perp();
    let bars = [
        bar(0, 100.0, 101.0, 99.0, 100.0),
        bar(1, 100.0, 101.0, 99.0, 100.0),
        bar(2, 100.0, 111.0, 94.0, 100.0),
    ];
    let (script, _) = Script::new(vec![
        (0, req(&i, Side::Buy, 1.0, OrderType::Market)),
        (
            1,
            reduce_only(req(&i, Side::Sell, 1.0, OrderType::Limit { price: 110.0 })),
        ),
        (
            1,
            reduce_only(req(&i, Side::Sell, 1.0, OrderType::Stop { trigger: 95.0 })),
        ),
    ]);
    let s = run(i, 0.0, &bars, script);
    // Bar 2 reaches both 110 and 95. The path is unknown, so the stop is taken first, at 95.
    let f = s.fills();
    assert_eq!(f.len(), 2);
    assert_eq!(f[1].order_id.0, 3);
    approx(f[1].price, 95.0);
    assert_eq!(
        s.orders()[1].status,
        OrderStatus::Cancelled("reduce-only order would not reduce the position".into())
    );
    assert_eq!(s.portfolio().position(), 0.0);
    // Fees: 0.2 on the entry at 100, 0.19 on the stop at 95. Realised -5.
    approx(s.portfolio().cash(), 10_000.0 - 0.2 - 0.19 - 5.0);
}

/// Records the history view's length and last bar on every callback.
struct Recorder(Arc<Mutex<Vec<(usize, i64, i64)>>>);

impl Strategy for Recorder {
    fn on_start(&mut self, ctx: &mut Context) {
        let last = ctx.bars().last().unwrap().open_time;
        self.0.lock().unwrap().push((ctx.bars().len(), last, -1));
    }

    fn on_bar(&mut self, ctx: &mut Context, bar: &Candle) {
        let last = ctx.bars().last().unwrap().open_time;
        self.0
            .lock()
            .unwrap()
            .push((ctx.bars().len(), last, bar.open_time));
    }
}

#[test]
fn a_strategy_never_sees_a_future_bar() {
    let bars: Vec<Candle> = (0..5)
        .map(|n| bar(n, 100.0, 110.0, 99.0, 100.0 + n as f64))
        .collect();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let strategy = Box::new(Recorder(seen.clone()));
    run_backtest(strategy, spot(), Timeframe::M1, &bars, &[], config(0.0)).unwrap();
    let seen = seen.lock().unwrap();
    // on_start on the first bar, then on_bar for every bar with history ending at that bar.
    assert_eq!(seen[0], (1, 0, -1));
    for (n, &(len, last, current)) in seen[1..].iter().enumerate() {
        assert_eq!(len, n + 1);
        assert_eq!(last, n as i64 * MIN);
        assert_eq!(current, last);
    }
    assert_eq!(seen.len(), 6);
}

#[test]
fn bad_bars_are_rejected_and_change_nothing() {
    let (script, _) = Script::new(vec![]);
    let mut s = Session::new(script, spot(), Timeframe::M1, config(0.0)).unwrap();
    s.on_bar(bar(5, 100.0, 101.0, 99.0, 100.0), &[]).unwrap();
    assert_eq!(
        s.on_bar(bar(5, 100.0, 101.0, 99.0, 100.0), &[]),
        Err(EngineError::DuplicateBar { time: 5 * MIN })
    );
    assert_eq!(
        s.on_bar(bar(4, 100.0, 101.0, 99.0, 100.0), &[]),
        Err(EngineError::OutOfOrderBar {
            prev: 5 * MIN,
            got: 4 * MIN
        })
    );
    let mut misaligned = bar(6, 100.0, 101.0, 99.0, 100.0);
    misaligned.open_time += 1;
    assert!(matches!(
        s.on_bar(misaligned, &[]),
        Err(EngineError::MisalignedBar { .. })
    ));
    assert!(matches!(
        s.on_bar(bar(6, 100.0, 99.0, 98.0, 100.0), &[]),
        Err(EngineError::Core(_))
    ));
    assert_eq!(s.bars().len(), 1);
    assert_eq!(s.equity().len(), 1);
    // A gap is fine.
    s.on_bar(bar(9, 100.0, 101.0, 99.0, 100.0), &[]).unwrap();
}

#[test]
fn bad_configs_are_rejected() {
    for (cash, slip) in [(-1.0, 0.0), (f64::NAN, 0.0), (1.0, -1.0), (1.0, 10_000.0)] {
        let (script, _) = Script::new(vec![]);
        let cfg = td_engine::SessionConfig {
            starting_cash: cash,
            slippage_bps: slip,
            fees: None,
        };
        assert!(matches!(
            Session::new(script, spot(), Timeframe::M1, cfg),
            Err(EngineError::InvalidConfig(_))
        ));
    }
}

/// Up 2 a bar for 40 bars, then down 2 a bar for 40. Each bar spans 1 either side of its close,
/// so every close breaks the previous bar's high (or low) by 1 and the true range is 3.
fn up_then_down() -> Vec<Candle> {
    (0..80)
        .map(|n| {
            let c = if n < 40 {
                100.0 + 2.0 * n as f64
            } else {
                178.0 - 2.0 * (n - 39) as f64
            };
            bar(n, c, c + 1.0, c - 1.0, c)
        })
        .collect()
}

#[test]
fn sma_cross_goes_long_on_the_rise_and_flat_on_the_fall() {
    let p = Params::new()
        .with("fast", td_engine::ParamValue::Int(3))
        .with("slow", td_engine::ParamValue::Int(8));
    let strategy = build_strategy("sma_cross", &p).unwrap();
    let s = run_backtest(
        strategy,
        spot(),
        Timeframe::M1,
        &up_then_down(),
        &[],
        config(5.0),
    )
    .unwrap();
    let f = s.fills();
    assert_eq!(f.len(), 2, "{f:?}");
    assert_eq!((f[0].side, f[1].side), (Side::Buy, Side::Sell));
    // Bar 7 is the first with 8 bars of history; the buy fills on bar 8.
    assert_eq!(f[0].time, 8 * MIN);
    assert!(f[1].time > 40 * MIN);
    assert_eq!(s.portfolio().position(), 0.0);
    assert!(s.portfolio().cash() > 10_000.0);
}

#[test]
fn donchian_enters_on_a_breakout_places_a_stop_and_exits() {
    let p = Params::new()
        .with("entry", td_engine::ParamValue::Int(5))
        .with("exit", td_engine::ParamValue::Int(3))
        .with("atr_period", td_engine::ParamValue::Int(3));
    let strategy = build_strategy("donchian_breakout", &p).unwrap();
    let s = run_backtest(
        strategy,
        perp(),
        Timeframe::M1,
        &up_then_down(),
        &[],
        config(0.0),
    )
    .unwrap();
    let orders = s.orders();
    // Entry, then its stop, then either the stop or a channel exit closes the trade.
    assert!(matches!(orders[0].request.order_type, OrderType::Market));
    assert!(matches!(
        orders[1].request.order_type,
        OrderType::Stop { .. }
    ));
    assert!(orders[1].request.reduce_only);
    // Bar 5 is the first with enough history; its close 110 breaks the 5-bar high of 109. ATR is
    // 3, so the stop sits 2 * 3 = 6 under the fill at bar 6's open of 112, and the size risks 1%
    // of 10000 on that distance: 100 / 6 = 16.666 after rounding down to the lot.
    let entry = &s.fills()[0];
    assert_eq!(entry.time, 6 * MIN);
    approx(entry.price, 112.0);
    approx(entry.qty, 16.666);
    let OrderType::Stop { trigger } = orders[1].request.order_type else {
        unreachable!()
    };
    approx(trigger, 106.0);
    // On the fall, bar 41's close 174 breaks the 3-bar low of 175: the stop is cancelled and the
    // position sold at bar 42's open.
    assert_eq!(
        orders[1].status,
        OrderStatus::Cancelled("cancelled by the strategy".into())
    );
    let exit = &s.fills()[1];
    assert_eq!(exit.time, 42 * MIN);
    approx(exit.qty, 16.666);
    assert_eq!(s.portfolio().position(), 0.0);
    assert!(s.open_orders().is_empty());
}
