//! The gotobi strategy on fixture USD/JPY bid and ask bars, 5-minute, on a dollar account. Every
//! price and size is worked by hand in the comments.

use td_core::{
    AccountCurrency, Candle, FeeSchedule, Instrument, InstrumentId, InstrumentKind, Millis,
    OrderType, Side, Timeframe, Venue,
};
use td_engine::{
    EngineError, OrderStatus, ParamValue, Params, Session, SessionConfig, build_strategy, date,
    find_strategy,
};

const MIN: Millis = 60_000;
/// Every ask is the bid plus 0.4 pips.
const SPREAD: f64 = 0.004;

/// USD/JPY: tick 0.001, lots of 1,000, no fees.
fn usdjpy() -> Instrument {
    Instrument {
        id: InstrumentId {
            venue: Venue::Oanda,
            symbol: "USDJPY".into(),
            kind: InstrumentKind::Fx,
        },
        base: "USD".into(),
        quote: "JPY".into(),
        tick_size: 0.001,
        lot_size: 1_000.0,
        min_qty: 1_000.0,
        min_notional: 0.0,
        fees: FeeSchedule {
            maker: 0.0,
            taker: 0.0,
            per_unit: 0.0,
        },
    }
}

/// A dollar account of 100,000, no slippage, up to 30x.
fn dollars() -> SessionConfig {
    SessionConfig {
        starting_cash: 100_000.0,
        slippage_bps: 0.0,
        fees: None,
        max_leverage: 30.0,
        account_currency: AccountCurrency::Base,
    }
}

/// One bid bar: its open time and open, high, low, close.
type Bar = (Millis, [f64; 4]);

/// The 5-minute bars that open from 00:30 to 01:45 UTC on `day` (midnight UTC), flat at 150.000.
fn morning(day: Millis) -> Vec<Bar> {
    (6..=21).map(|i| (day + i * 5 * MIN, [150.0; 4])).collect()
}

/// Replaces the bar that opens at `hh:mm` UTC on `day`.
fn set(bars: &mut [Bar], day: Millis, hh: i64, mm: i64, ohlc: [f64; 4]) {
    let t = day + (hh * 60 + mm) * MIN;
    bars.iter_mut()
        .find(|b| b.0 == t)
        .expect("a bar opens then")
        .1 = ohlc;
}

fn candle((open_time, [open, high, low, close]): Bar) -> Candle {
    Candle {
        open_time,
        open,
        high,
        low,
        close,
        volume: 1.0,
        quote_volume: close,
    }
}

fn ask_of((t, [o, h, l, c]): Bar) -> Bar {
    (t, [o + SPREAD, h + SPREAD, l + SPREAD, c + SPREAD])
}

/// Runs gotobi with `params` on `bars` and their asks; the error of the first bar that fails.
fn run_tf(params: Params, bars: &[Bar], tf: Timeframe) -> Result<Session, EngineError> {
    let strategy = build_strategy("gotobi", &params)?;
    let mut s = Session::new(strategy, usdjpy(), tf, dollars())?;
    for &b in bars {
        s.on_bar_with_ask(candle(b), Some(candle(ask_of(b))), &[])?;
    }
    Ok(s)
}

fn run(params: Params, bars: &[Bar]) -> Session {
    run_tf(params, bars, Timeframe::M5).unwrap()
}

fn control() -> Params {
    Params::new().with("control", ParamValue::Bool(true))
}

fn approx(actual: f64, expected: f64) {
    assert!(
        (actual - expected).abs() < 1e-6 * expected.abs().max(1.0),
        "{actual} != {expected}"
    );
}

/// Tuesday 2026-11-10, a gotobi day.
fn gotobi_tuesday() -> Millis {
    date(2026, 11, 10)
}

#[test]
fn a_gotobi_day_sells_at_the_fixing_and_buys_back_at_half_past_ten() {
    let day = gotobi_tuesday();
    let mut bars = morning(day);
    // The decision bar, 00:50 to 00:55, closes at 150.000; the next bar opens a pip higher.
    set(&mut bars, day, 0, 55, [150.010, 150.015, 150.000, 150.005]);
    // The bar after the exit decision opens at 149.950 bid, 149.954 ask.
    set(&mut bars, day, 1, 30, [149.950, 149.960, 149.940, 149.950]);
    let s = run(Params::new(), &bars);

    let fills = s.fills();
    assert_eq!(fills.len(), 2);
    // 0.5% of 100,000 dollars on a 0.12 yen stop: 500 / (0.12 / 150.000) = 625,000 units.
    let (sell, buy) = (&fills[0], &fills[1]);
    assert_eq!((sell.side, sell.time), (Side::Sell, day + 55 * MIN));
    approx(sell.price, 150.010); // the bid open
    approx(sell.qty, 625_000.0);
    assert_eq!((buy.side, buy.time), (Side::Buy, day + 90 * MIN));
    approx(buy.price, 149.954); // the ask open
    approx(buy.qty, 625_000.0);

    let orders = s.orders();
    assert_eq!(orders.len(), 3, "{orders:?}");
    // The decision was the bid close of the bar that opened at 00:50.
    assert_eq!(orders[0].submitted_at, day + 50 * MIN);
    assert_eq!(orders[0].request.order_type, OrderType::Market);
    // The stop sits 12 pips above the decision close, and is cancelled at the exit.
    let stop = orders[0].attached_stop.unwrap();
    approx(stop.trigger, 150.120);
    assert_eq!(orders[1].id, stop.id);
    assert!(matches!(orders[1].status, OrderStatus::Cancelled(_)));
    // The exit was decided on the bar that closes at 01:30.
    assert_eq!(orders[2].submitted_at, day + 85 * MIN);
    assert!(orders[2].request.reduce_only);
    assert!(s.open_orders().is_empty());
    assert_eq!(s.portfolio().position(), 0.0);
}

#[test]
fn a_stop_out_before_half_past_ten_loses_about_the_risk_and_ends_the_trade() {
    let day = gotobi_tuesday();
    let mut bars = morning(day);
    set(&mut bars, day, 0, 55, [150.002, 150.002, 150.000, 150.000]);
    // The ask high, 150.134, reaches the stop at 150.120; the ask open, 150.004, is below it.
    set(&mut bars, day, 1, 5, [150.000, 150.130, 149.990, 150.050]);
    let s = run(Params::new(), &bars);

    let fills = s.fills();
    assert_eq!(fills.len(), 2, "nothing happens at 01:30: {fills:?}");
    assert_eq!((fills[1].side, fills[1].time), (Side::Buy, day + 65 * MIN));
    approx(fills[1].price, 150.120);
    assert_eq!(s.orders().len(), 2);
    assert!(s.open_orders().is_empty());
    assert_eq!(s.portfolio().position(), 0.0);

    // Sold 625,000 at 150.002 and bought back at 150.120: 625,000 * 0.118 = 73,750 yen, which is
    // 73,750 / 150.120 = 491.27 dollars at the price the loss arises at.
    let loss = 100_000.0 - s.portfolio().equity();
    approx(loss, 73_750.0 / 150.120);
    // It is close to the 500 dollars (0.5%) risked, not equal: the stop is measured from the
    // decision close (150.000), and the fill was 0.2 pips better (150.002), so 0.2 of the 12 pips
    // are not lost; the yen loss is converted at the stop price, not the decision close; and the
    // size is rounded down to the lot. The spread itself does not move it, since the buy stop
    // triggers on the ask at its own price. Each of these is a small fraction of the risk, so a
    // tolerance of a tenth of it, 0.05 percentage points, holds them all for a gap of a pip or so.
    assert!((loss / 100_000.0 - 0.005).abs() < 0.0005, "{loss}");
}

#[test]
fn a_day_that_is_not_a_gotobi_day_has_no_trade() {
    // Monday 2026-11-09.
    let s = run(Params::new(), &morning(date(2026, 11, 9)));
    assert!(s.orders().is_empty());
}

#[test]
fn a_gotobi_day_on_a_weekend_trades_on_the_friday_before() {
    // Sunday 2025-10-05 moves back to Friday 2025-10-03; Monday 2025-10-06 is not a gotobi day.
    let friday = date(2025, 10, 3);
    let mut bars = morning(friday);
    bars.extend(morning(date(2025, 10, 6)));
    let s = run(Params::new(), &bars);
    assert_eq!(s.fills().len(), 2);
    assert_eq!(s.fills()[0].time, friday + 55 * MIN);
    assert_eq!(s.fills()[1].time, friday + 90 * MIN);
}

#[test]
fn the_control_trades_business_days_that_are_not_gotobi_days() {
    // Monday 2026-11-09 is a business day and not a gotobi day.
    let monday = date(2026, 11, 9);
    let s = run(control(), &morning(monday));
    assert_eq!(s.fills().len(), 2);
    assert_eq!(s.fills()[0].time, monday + 55 * MIN);
    // Tuesday 2026-11-10 is a gotobi day, and Tuesday 2026-11-03 Culture Day, a holiday.
    assert!(
        run(control(), &morning(gotobi_tuesday()))
            .orders()
            .is_empty()
    );
    assert!(
        run(control(), &morning(date(2026, 11, 3)))
            .orders()
            .is_empty()
    );
    // Saturdays and Sundays have no FX bars at 00:55 UTC to feed; the strategy's own unit tests
    // check that the control does not count them as days to trade.
}

#[test]
fn the_size_risks_the_percentage_and_rounds_down_to_the_lot() {
    let day = gotobi_tuesday();
    let mut bars = morning(day);
    // The decision closes at 150.070. 1% risk is 1,000 dollars; the 0.12 yen stop is
    // 0.12 / 150.070 dollars a unit, so 1,000 * 150.070 / 0.12 = 1,250,583.33 units, rounded down
    // to the 1,000-unit lot: 1,250,000.
    set(&mut bars, day, 0, 50, [150.0, 150.070, 150.0, 150.070]);
    let params = Params::new().with("risk_pct", ParamValue::Float(1.0));
    let s = run(params, &bars);
    approx(s.orders()[0].request.qty, 1_250_000.0);
    approx(s.fills()[0].qty, 1_250_000.0);
    // A 6-pip stop doubles it before rounding: 2,501,166.67, so 2,501,000.
    let params = Params::new()
        .with("risk_pct", ParamValue::Float(1.0))
        .with("stop_pips", ParamValue::Float(6.0));
    let s = run(params, &bars);
    approx(s.orders()[0].request.qty, 2_501_000.0);
    approx(s.orders()[0].attached_stop.unwrap().trigger, 150.130);
}

#[test]
fn a_risk_below_a_hundredth_of_a_percent_is_refused() {
    let build = |pct: f64| {
        build_strategy(
            "gotobi",
            &Params::new().with("risk_pct", ParamValue::Float(pct)),
        )
        .err()
    };
    for pct in [0.0, 0.005, 100.5] {
        assert!(
            matches!(&build(pct), Some(EngineError::InvalidParam { name, .. }) if name == "risk_pct"),
            "{pct}"
        );
    }
    for pct in [0.01, 100.0] {
        assert_eq!(build(pct), None, "{pct}");
    }
}

#[test]
fn a_missing_decision_bar_means_no_trade_that_day() {
    // The bar that closes at 00:55 is missing from the data.
    let day = gotobi_tuesday();
    let bars: Vec<Bar> = morning(day)
        .into_iter()
        .filter(|b| b.0 != day + 50 * MIN)
        .collect();
    let s = run(Params::new(), &bars);
    assert!(s.orders().is_empty());
}

#[test]
fn a_bar_outside_the_holiday_table_fails_the_run() {
    // Wednesday 2028-01-05.
    let err = run_tf(Params::new(), &morning(date(2028, 1, 5)), Timeframe::M5)
        .err()
        .unwrap();
    let EngineError::StrategyFailed(reason) = &err else {
        panic!("{err}");
    };
    assert!(
        reason.contains("2028-01-05 is outside the Japanese holiday table"),
        "{reason}"
    );
    // The control fails the same way.
    assert!(matches!(
        run_tf(control(), &morning(date(2015, 12, 30)), Timeframe::M5),
        Err(EngineError::StrategyFailed(_))
    ));
}

#[test]
fn a_timeframe_without_bars_closing_at_the_fixing_fails_the_run() {
    let day = gotobi_tuesday();
    let bars: Vec<Bar> = (2..=6).map(|i| (day + i * 15 * MIN, [150.0; 4])).collect();
    let err = run_tf(Params::new(), &bars, Timeframe::M15).err().unwrap();
    let EngineError::StrategyFailed(reason) = &err else {
        panic!("{err}");
    };
    assert!(reason.contains("no 15m bar closes at both"), "{reason}");
    // 1m bars close at both, so the run goes through and trades.
    let bars: Vec<Bar> = (30..=100).map(|i| (day + i * MIN, [150.0; 4])).collect();
    let s = run_tf(Params::new(), &bars, Timeframe::M1).unwrap();
    assert_eq!(s.fills()[0].time, day + 55 * MIN);
    assert_eq!(s.fills()[1].time, day + 90 * MIN);
}

#[test]
fn an_exit_below_the_minimum_notional_still_closes_the_short() {
    let day = gotobi_tuesday();
    let mut bars = morning(day);
    // The bid falls to 148.000 by the exit decision, the close of the bar that opens at 01:25,
    // far from the stop. The 625,000 units are worth 625,000 * 150.000 = 93,750,000 yen at the
    // entry decision and 625,000 * 150.120 = 93,825,000 at the stop, but 92,500,000 at the exit
    // decision, below a minimum notional of 93,000,000 yen. FX is margined, so its reduce-only
    // exit is exempt from the minimum, as on a perpetual, and the short is closed.
    set(&mut bars, day, 1, 25, [150.0, 150.0, 148.0, 148.0]);
    let mut inst = usdjpy();
    inst.min_notional = 93_000_000.0;
    let strategy = build_strategy("gotobi", &Params::new()).unwrap();
    let mut s = Session::new(strategy, inst, Timeframe::M5, dollars()).unwrap();
    for &b in &bars {
        s.on_bar_with_ask(candle(b), Some(candle(ask_of(b))), &[])
            .unwrap();
    }
    let orders = s.orders();
    assert!(
        matches!(orders[1].status, OrderStatus::Cancelled(_)),
        "{orders:?}"
    );
    assert!(orders[2].request.reduce_only);
    assert!(
        matches!(orders[2].status, OrderStatus::Filled),
        "{orders:?}"
    );
    assert_eq!(s.fills()[1].time, day + 90 * MIN);
    approx(s.portfolio().position(), 0.0);
}

#[test]
fn the_registry_lists_gotobi_with_its_plan_on_a_dollar_account() {
    let entry = find_strategy("gotobi").unwrap();
    let params: Vec<(&str, ParamValue)> =
        entry.params.iter().map(|p| (p.name, p.default)).collect();
    assert_eq!(
        params,
        [
            ("stop_pips", ParamValue::Float(12.0)),
            ("risk_pct", ParamValue::Float(0.5)),
            ("control", ParamValue::Bool(false)),
        ]
    );
    let plan = &entry.plan;
    assert_eq!(plan.validate("gotobi"), Ok(()));
    assert_eq!(plan.instruments, [usdjpy().id]);
    assert_eq!(plan.timeframe, Timeframe::M5);
    assert_eq!(
        (plan.period.from, plan.period.to),
        (date(2016, 1, 1), date(2026, 10, 31))
    );
    assert_eq!(plan.backtest.account_currency, AccountCurrency::Base);
    let config = plan.backtest_config("gotobi", usdjpy());
    assert_eq!(config.account_currency, AccountCurrency::Base);
    assert_eq!(config.slippage_bps, 0.0);
    assert_eq!(config.max_leverage, 30.0);
    assert_eq!(config.starting_cash, 100_000.0);
    assert_eq!(entry.rule.validate(), Ok(()));
    // The plan survives JSON with its account currency.
    let json = serde_json::to_value(plan).unwrap();
    assert_eq!(json["backtest"]["account_currency"], "base");
    assert_eq!(
        &serde_json::from_value::<td_engine::TestPlan>(json).unwrap(),
        plan
    );

    // The other plans still run on a quote account, and their JSON leaves the field out.
    for name in ["sma_cross", "donchian_breakout"] {
        let plan = find_strategy(name).unwrap().plan;
        assert_eq!(
            plan.backtest.account_currency,
            AccountCurrency::Quote,
            "{name}"
        );
        let mut inst = usdjpy();
        inst.id = plan.instruments[0].clone();
        let config = plan.backtest_config(name, inst);
        assert_eq!(config.account_currency, AccountCurrency::Quote, "{name}");
        let json = serde_json::to_value(&plan).unwrap();
        assert!(json["backtest"].get("account_currency").is_none(), "{name}");
    }
}
