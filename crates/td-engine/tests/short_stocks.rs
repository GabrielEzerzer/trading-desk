//! Short sales of stocks when a run turns them on: whole shares below zero, the borrowable list,
//! the borrow fee, the leverage cap, attached buy stops and reduce-only covers.

mod common;

use common::*;
use td_core::{
    Candle, FeeSchedule, InstrumentKind, OrderId, OrderRequest, OrderType, Side, Timeframe,
};
use td_engine::{
    AlsoTradedBars, BacktestConfig, EngineError, OrderStatus, Session, SessionConfig, Shortable,
    StockShorting, Strategy, run_strategy, run_strategy_with_traded,
};

const DAY: i64 = 1_440 * MIN;
const MONDAY_2024_01_08: i64 = 1_704_672_000_000;
const NO_FEES: FeeSchedule = FeeSchedule {
    maker: 0.0,
    taker: 0.0,
    per_unit: 0.0,
};

fn day(i: i64, low: f64, high: f64) -> Candle {
    let mut b = bar(0, 100.0, high, low, 100.0);
    b.open_time = MONDAY_2024_01_08 + i * DAY;
    b
}

fn quiet(i: i64) -> Candle {
    day(i, 99.0, 101.0)
}

fn stock() -> td_core::Instrument {
    instrument(InstrumentKind::Stock)
}

fn session_config(max_leverage: f64) -> SessionConfig {
    SessionConfig {
        fees: Some(NO_FEES),
        max_leverage,
        ..config(0.0)
    }
}

fn session(
    strategy: Box<dyn Strategy>,
    shorting: Option<StockShorting>,
    bars: &[Candle],
) -> Session {
    let mut s = Session::new(strategy, stock(), Timeframe::D1, session_config(1.0)).unwrap();
    if let Some(shorting) = shorting {
        s.allow_short_sales(shorting).unwrap();
    }
    for b in bars {
        s.on_bar(*b, &[]).unwrap();
    }
    s
}

fn all() -> Option<StockShorting> {
    Some(StockShorting::new(Shortable::All))
}

fn order(side: Side, qty: f64) -> OrderRequest {
    req(&stock(), side, qty, OrderType::Market)
}

fn quiet_days(n: i64) -> Vec<Candle> {
    (0..n).map(quiet).collect()
}

#[test]
fn a_sell_beyond_the_holding_opens_and_adds_to_a_short_only_when_short_sales_are_on() {
    let plan = || vec![(0, order(Side::Sell, 5.0)), (1, order(Side::Sell, 3.0))];
    let (script, log) = Script::new(plan());
    let s = session(script, all(), &quiet_days(3));
    assert!(log.lock().unwrap().iter().all(|r| r.is_ok()));
    let f = s.fills();
    assert_eq!(f.len(), 2);
    approx(f[0].qty, 5.0);
    approx(f[1].qty, 3.0);
    let p = s.portfolio();
    approx(p.position(), -8.0);
    approx(p.cash(), 10_800.0);
    approx(*s.equity().last().map(|e| &e.equity).unwrap(), 10_000.0);

    let (script, log) = Script::new(plan());
    let s = session(script, None, &quiet_days(3));
    let log = log.lock().unwrap();
    let err = log[0].as_ref().unwrap_err();
    assert!(
        err.reason
            .starts_with("spot cannot sell more than it holds"),
        "{}",
        err.reason
    );
    assert!(s.fills().is_empty());
}

#[test]
fn a_fractional_holding_sold_through_zero_sells_the_holding_and_whole_shares_below_it() {
    let run = |sell: f64, cover: Option<f64>| {
        let mut plan = vec![(0, order(Side::Buy, 0.5)), (1, order(Side::Sell, sell))];
        plan.extend(cover.map(|qty| (2, order(Side::Buy, qty))));
        let (script, log) = Script::new(plan);
        let s = session(script, all(), &quiet_days(4));
        assert!(log.lock().unwrap().iter().all(|r| r.is_ok()));
        s
    };
    let s = run(2.7, Some(0.7));
    let qty: Vec<f64> = s.fills().iter().map(|f| f.qty).collect();
    assert_eq!(qty.len(), 3, "{qty:?}");
    approx(qty[0], 0.5);
    approx(qty[1], 2.5);
    approx(qty[2], 0.7);
    approx(s.portfolio().position(), -1.3);

    let s = run(1.3, None);
    approx(s.fills()[1].qty, 0.5);
    approx(s.portfolio().position(), 0.0);
}

#[test]
fn a_short_sale_of_less_than_one_share_while_flat_is_cancelled_at_fill_time() {
    let (script, log) = Script::new(vec![(0, order(Side::Sell, 0.6))]);
    let s = session(script, all(), &quiet_days(2));
    assert!(log.lock().unwrap()[0].is_ok());
    assert!(s.fills().is_empty());
    assert_eq!(
        s.orders()[0].status,
        OrderStatus::Cancelled(
            "short sale cancelled at fill time: nothing is held, and a short sale is in whole \
             shares"
                .into()
        )
    );
}

#[test]
fn a_stock_off_the_shortable_list_cannot_be_sold_short_but_its_holding_can_be_sold() {
    let only = |symbols: &[&str]| {
        Some(StockShorting::new(Shortable::Only(
            symbols.iter().map(|s| s.to_string()).collect(),
        )))
    };
    let plan = || {
        vec![
            (0, order(Side::Buy, 1.0)),
            (1, order(Side::Sell, 3.0)),
            (1, order(Side::Sell, 1.0)),
        ]
    };
    let (script, log) = Script::new(plan());
    let s = session(script, only(&["SPY"]), &quiet_days(3));
    let log = log.lock().unwrap();
    let err = log[1].as_ref().unwrap_err();
    assert!(
        err.reason.starts_with(
            "TESTUSDT is not on the run's list of stocks that can be sold short, so it cannot \
             sell more than it holds"
        ),
        "{}",
        err.reason
    );
    assert!(log[2].is_ok());
    approx(s.portfolio().position(), 0.0);

    let (script, log) = Script::new(plan());
    let s = session(script, only(&["TESTUSDT"]), &quiet_days(3));
    assert!(log.lock().unwrap().iter().all(|r| r.is_ok()));
    approx(s.portfolio().position(), -3.0);
}

#[test]
fn a_short_counts_toward_the_leverage_cap() {
    let (script, log) = Script::new(vec![
        (0, order(Side::Sell, 101.0)),
        (0, order(Side::Sell, 100.0)),
    ]);
    let s = session(script, all(), &quiet_days(2));
    let log = log.lock().unwrap();
    let err = log[0].as_ref().unwrap_err();
    assert!(
        err.reason.starts_with("over the leverage cap of 1x"),
        "{}",
        err.reason
    );
    assert!(log[1].is_ok());
    approx(s.portfolio().position(), -100.0);
}

#[test]
fn an_attached_buy_stop_on_a_short_entry_covers_the_bar_the_entry_fills_on() {
    struct Entry;
    impl Strategy for Entry {
        fn on_bar(&mut self, ctx: &mut td_engine::Context, _bar: &Candle) {
            if ctx.bars().len() == 1 {
                assert_eq!(
                    ctx.submit_with_stop(order(Side::Sell, 10.0), 105.0),
                    Ok((OrderId(1), OrderId(2)))
                );
            }
        }
    }
    let s = session(Box::new(Entry), all(), &[quiet(0), day(1, 99.0, 110.0)]);
    let f = s.fills();
    assert_eq!(f.len(), 2);
    assert_eq!((f[0].order_id.0, f[0].side), (1, Side::Sell));
    approx(f[0].price, 100.0);
    assert_eq!(
        (f[1].order_id.0, f[1].time, f[1].side),
        (2, f[0].time, Side::Buy)
    );
    approx(f[1].price, 105.0);
    approx(f[1].qty, 10.0);
    let p = s.portfolio();
    approx(p.position(), 0.0);
    approx(p.cash(), 9_950.0);
}

#[test]
fn a_reduce_only_buy_covers_a_short_and_never_turns_it_long() {
    let (script, log) = Script::new(vec![
        (0, order(Side::Sell, 5.0)),
        (1, reduce_only(order(Side::Buy, 8.0))),
    ]);
    let s = session(script, all(), &quiet_days(3));
    assert!(log.lock().unwrap().iter().all(|r| r.is_ok()));
    let f = s.fills();
    assert_eq!(f.len(), 2);
    assert_eq!(f[1].side, Side::Buy);
    approx(f[1].qty, 5.0);
    approx(s.portfolio().position(), 0.0);
    approx(s.portfolio().cash(), 10_000.0);
}

#[test]
fn short_sales_are_set_once_before_the_first_bar_on_a_stock_with_a_rate_of_zero_or_more() {
    let new = |inst| Session::new(Script::new(vec![]).0, inst, Timeframe::D1, config(0.0));
    let refused = |s: &mut Session, shorting| {
        matches!(
            s.allow_short_sales(shorting),
            Err(EngineError::InvalidConfig(_))
        )
    };
    let all = StockShorting::new(Shortable::All);
    assert!(refused(&mut new(perp()).unwrap(), all.clone()));
    let negative = StockShorting {
        borrow_rate: -0.01,
        ..all.clone()
    };
    assert!(refused(&mut new(stock()).unwrap(), negative));
    let mut twice = new(stock()).unwrap();
    twice.allow_short_sales(all.clone()).unwrap();
    assert!(refused(&mut twice, all.clone()));
    let mut late = new(stock()).unwrap();
    late.on_bar(quiet(0), &[]).unwrap();
    assert!(refused(&mut late, all));
}

fn borrow_days() -> Vec<Candle> {
    [10, 11, 14, 15].into_iter().map(quiet).collect()
}

fn run_config(borrow_rate: f64, shortable: Shortable) -> BacktestConfig {
    let bars = borrow_days();
    BacktestConfig {
        strategy: "script".into(),
        params: td_engine::Params::new(),
        instrument: stock(),
        timeframe: Timeframe::D1,
        from: bars[0].open_time,
        to: bars[3].open_time + DAY,
        starting_cash: 10_000.0,
        slippage_bps: 0.0,
        fee_override: Some(NO_FEES),
        max_leverage: 1.0,
        warmup_from: None,
        close_at_end: false,
        account_currency: td_core::AccountCurrency::Quote,
        references: Vec::new(),
        series: Vec::new(),
        also_traded: Vec::new(),
        start_when_all_listed: false,
        short_stocks: Some(StockShorting {
            borrow_rate,
            ..StockShorting::new(shortable)
        }),
    }
}

fn borrow_run(borrow_rate: f64) -> td_engine::BacktestResult {
    let (script, log) = Script::new(vec![(0, order(Side::Sell, 10.0))]);
    let config = run_config(borrow_rate, Shortable::All);
    let result = run_strategy(&config, script, &borrow_days(), &[], &[]).unwrap();
    assert!(log.lock().unwrap().iter().all(|r| r.is_ok()));
    result
}

#[test]
fn the_borrow_fee_is_charged_on_each_calendar_day_held_short_and_reported_as_funding() {
    let r = borrow_run(0.36);
    assert_eq!(r.fills.len(), 1);
    approx(r.fills[0].qty, 10.0);
    approx(r.metrics.funding_paid, 4.0);
    approx(r.metrics.fees_paid, 0.0);
    let equity: Vec<f64> = r.equity.iter().map(|e| e.equity).collect();
    assert_eq!(equity.len(), 4);
    for (actual, expected) in equity.iter().zip([10_000.0, 10_000.0, 9_997.0, 9_996.0]) {
        approx(*actual, expected);
    }
    approx(borrow_run(0.0).metrics.funding_paid, 0.0);
}

fn other() -> td_core::Instrument {
    let mut i = stock();
    i.id.symbol = "OTHER".into();
    i.base = "OTHER".into();
    i
}

#[test]
fn a_stock_traded_beside_the_first_is_sold_short_and_pays_its_borrow_fee_when_on_the_list() {
    let bars = borrow_days();
    let mut config = run_config(0.36, Shortable::Only(vec!["OTHER".into()]));
    config.also_traded = vec![other()];
    let (script, log) = Script::new(vec![
        (0, req(&other(), Side::Sell, 10.0, OrderType::Market)),
        (0, order(Side::Sell, 1.0)),
    ]);
    let others = [AlsoTradedBars {
        instrument: other().id,
        bars: bars.clone(),
        funding: Vec::new(),
        left_at: None,
    }];
    let r = run_strategy_with_traded(&config, script, &bars, &[], &[], &[], &[], &others).unwrap();
    let log = log.lock().unwrap();
    assert!(log[0].is_ok());
    let err = log[1].as_ref().unwrap_err();
    assert!(
        err.reason
            .starts_with("TESTUSDT is not on the run's list of stocks that can be sold short"),
        "{}",
        err.reason
    );
    assert_eq!(r.fills.len(), 1);
    assert_eq!(r.fills[0].instrument, other().id);
    approx(r.fills[0].qty, 10.0);
    approx(r.metrics.funding_paid, 4.0);
}
