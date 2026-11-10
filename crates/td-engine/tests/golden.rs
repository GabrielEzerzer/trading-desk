//! Golden tests: small hand-built bar series where every fill price, fee, cash, position and
//! equity value is worked out by hand in the comments and asserted.

mod common;

use std::sync::{Arc, Mutex};

use common::*;
use td_core::{Candle, InstrumentKind, Liquidity, OrderId, OrderType, Side, Timeframe};
use td_engine::{
    AttachedStop, Context, EngineError, OrderStatus, Params, Portfolio, Session, Strategy,
    build_strategy, run_backtest,
};

fn run(
    inst: td_core::Instrument,
    slippage: f64,
    bars: &[Candle],
    strategy: Box<dyn Strategy>,
) -> Session {
    run_backtest(strategy, inst, Timeframe::M1, bars, &[], config(slippage)).unwrap()
}

/// Calls the closure with the context and the bar's index at each bar's close.
struct Steps<F>(F);

impl<F: FnMut(&mut Context, usize) + Send> Strategy for Steps<F> {
    fn on_bar(&mut self, ctx: &mut Context, _bar: &Candle) {
        let index = ctx.bars().len() - 1;
        (self.0)(ctx, index);
    }
}

fn steps(f: impl FnMut(&mut Context, usize) + Send + 'static) -> Box<dyn Strategy> {
    Box::new(Steps(f))
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
fn limit_buys_fill_at_their_limit_on_a_gap_and_on_a_touch() {
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
    // Bar 1 opens at 97, already below the 98 limit, which rested from the close of 100: the
    // market passed 98 between the bars, so it fills at 98, maker 0.1% of 490.
    assert_eq!((f[0].order_id.0, f[0].time), (1, MIN));
    approx(f[0].price, 98.0);
    approx(f[0].fee, 0.49);
    assert_eq!(f[0].liquidity, Liquidity::Maker);
    // Bar 1's low 96 never reaches 95. Bar 2's low 94.5 does, after opening at 96: fills at 95.
    assert_eq!((f[1].order_id.0, f[1].time), (2, 2 * MIN));
    approx(f[1].price, 95.0);
    approx(f[1].fee, 0.095);

    // Bar 1: 10000 - 490.49 + 5 * 98. Bar 2: cash 10000 - 490.49 - 95.095 = 9414.415, plus 6 * 95.
    let e = equity(&s);
    approx(e[1], 9_999.51);
    approx(e[2], 9_984.415);
    approx(s.portfolio().cash(), 9_414.415);
    approx(s.portfolio().position(), 6.0);
    // The 90 limit was never reached and is still resting.
    assert_eq!(s.orders()[2].status, OrderStatus::Open);
    assert_eq!(s.open_orders().len(), 1);
}

#[test]
fn a_limit_sell_fills_at_its_limit_on_a_gap_up() {
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
    // Opens at 103, above the 102 limit resting from the close of 100: a short of 1 at 102, maker
    // fee 0.102.
    approx(s.fills()[0].price, 102.0);
    approx(s.fills()[0].fee, 0.102);
    assert_eq!(s.fills()[0].liquidity, Liquidity::Maker);
    // Perpetual: cash 10000 - 0.102; unrealised -1 * (103.5 - 102) = -1.5.
    approx(s.portfolio().cash(), 9_999.898);
    approx(s.portfolio().position(), -1.0);
    approx(equity(&s)[1], 9_998.398);
}

#[test]
fn a_limit_priced_through_the_market_fills_as_a_taker_with_slippage() {
    let i = perp();
    let bars = [
        bar(0, 100.0, 101.0, 99.0, 100.0),
        bar(1, 100.0, 101.0, 99.0, 100.0),
        bar(2, 101.0, 102.0, 100.8, 101.5),
        bar(3, 100.3, 100.6, 100.0, 100.2),
    ];
    let limit = |price| OrderType::Limit { price };
    let (script, log) = Script::new(vec![
        (0, req(&i, Side::Buy, 1.0, OrderType::Market)),
        (0, req(&i, Side::Buy, 1.0, limit(150.0))),
        (0, req(&i, Side::Sell, 1.0, limit(50.0))),
        (0, req(&i, Side::Buy, 1.0, limit(100.05))),
        (1, req(&i, Side::Buy, 1.0, limit(100.5))),
    ]);
    let s = run(i, 10.0, &bars, script);
    assert!(log.lock().unwrap().iter().all(|r| r.is_ok()));

    let f = s.fills();
    assert_eq!(f.len(), 5);
    let fill = |n: usize| (f[n].order_id.0, f[n].time, f[n].liquidity);
    // Order 1, the market buy: 100 * 1.001 = 100.10, taker fee 0.2% = 0.2002.
    assert_eq!(fill(0), (1, MIN, Liquidity::Taker));
    approx(f[0].price, 100.10);
    approx(f[0].fee, 0.2002);
    // Order 2 buys at 150 when the close was 100: it is a market buy with a cap, so it pays the
    // same 100.10 and taker fee as order 1, not the open of 100 and a maker fee.
    assert_eq!(fill(1), (2, MIN, Liquidity::Taker));
    approx(f[1].price, 100.10);
    approx(f[1].fee, 0.2002);
    // Order 3 sells at 50 when the close was 100: 100 * 0.999 = 99.90, fee 0.1998.
    assert_eq!(fill(2), (3, MIN, Liquidity::Taker));
    approx(f[2].price, 99.90);
    approx(f[2].fee, 0.1998);
    // Order 4 buys at 100.05: the slipped 100.10 is past its limit, so it pays 100.05, fee 0.2001.
    assert_eq!(fill(3), (4, MIN, Liquidity::Taker));
    approx(f[3].price, 100.05);
    approx(f[3].fee, 0.2001);
    // Order 5 buys at 100.5 after a close of 100, but bar 2 opens at 101, past the limit, and its
    // low of 100.8 stays above it: it rests as a plain limit. Bar 3 opens through the limit at
    // 100.3, but that is no longer the order's first bar, so it fills as a resting limit: at its
    // limit of 100.5, as a maker, fee 0.1005.
    assert_eq!(fill(4), (5, 3 * MIN, Liquidity::Maker));
    approx(f[4].price, 100.5);
    approx(f[4].fee, 0.1005);

    // Perpetual, bar 1: the fees 0.2002 + 0.2002 + 0.1998 + 0.2001 = 0.8003, and order 3 closes 1
    // of the 2 bought at 100.10 for 99.90, realising -0.20: cash 9998.9997. Long 2 at an average
    // of (100.10 + 100.05) / 2 = 100.075, marked at 100: equity 9998.9997 - 0.15.
    let e = equity(&s);
    approx(e[1], 9998.8497);
    // Bar 2: marked at 101.5, unrealised 2 * 1.425 = 2.85.
    approx(e[2], 10001.8497);
    // Bar 3: cash less 0.1005 = 9998.8992. Long 3 at an average of (200.15 + 100.5) / 3, marked at
    // 100.2: unrealised 300.6 - 300.65 = -0.05.
    let p = s.portfolio();
    approx(p.cash(), 9998.8992);
    approx(p.position(), 3.0);
    approx(p.realised_pnl(), -0.20);
    approx(e[3], 9998.8492);
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

#[test]
fn resting_limits_a_bar_opens_through_fill_at_their_own_price_as_makers() {
    let i = perp();
    let bars = [
        bar(0, 105.0, 106.0, 104.0, 105.0),
        bar(1, 105.0, 106.0, 104.0, 105.0),
        bar(2, 90.0, 92.0, 89.0, 91.0),
        bar(3, 115.0, 116.0, 114.0, 115.0),
    ];
    let limit = |price| OrderType::Limit { price };
    let (script, _) = Script::new(vec![
        (0, req(&i, Side::Buy, 1.0, limit(100.0))),
        (0, req(&i, Side::Sell, 1.0, limit(110.0))),
    ]);
    let s = run(i, 10.0, &bars, script);
    // Both rest from the close of 105, the buy below it and the sell above it, and bar 1 reaches
    // neither. Bar 2 opens at 90, through the buy: the market passed 100 on the way down, where the
    // resting order filled, so it buys at 100, not 90, as a maker with no slippage: fee 0.1.
    let f = s.fills();
    assert_eq!(f.len(), 2);
    assert_eq!((f[0].order_id.0, f[0].time), (1, 2 * MIN));
    assert_eq!(f[0].liquidity, Liquidity::Maker);
    approx(f[0].price, 100.0);
    approx(f[0].fee, 0.1);
    // Bar 3 opens at 115, through the sell: it sells at 110, not 115, fee 0.11.
    assert_eq!((f[1].order_id.0, f[1].time), (2, 3 * MIN));
    assert_eq!(f[1].liquidity, Liquidity::Maker);
    approx(f[1].price, 110.0);
    approx(f[1].fee, 0.11);
    // Bar 2: cash 10000 - 0.1, long 1 from 100 marked at 91. Bar 3: realised 10, flat.
    let e = equity(&s);
    approx(e[2], 9990.9);
    let p = s.portfolio();
    assert_eq!(p.position(), 0.0);
    approx(p.realised_pnl(), 10.0);
    approx(p.cash(), 10009.79);
    approx(e[3], 10009.79);
}

#[test]
fn an_ioc_limit_marketable_at_the_open_takes_with_slippage_up_to_its_limit() {
    let i = spot();
    let bars = [
        bar(0, 100.0, 101.0, 99.0, 100.0),
        bar(1, 99.0, 100.0, 98.0, 99.5),
    ];
    let limit = |price| ioc(req(&i, Side::Buy, 1.0, OrderType::Limit { price }));
    let (script, _) = Script::new(vec![(0, limit(99.5)), (0, limit(99.05))]);
    let s = run(i, 10.0, &bars, script);
    // Neither was marketable at the close of 100, but an IOC order trades on arrival, at bar 1's
    // open of 99, where both are marketable: they take liquidity like a market order. 99 * 1.001
    // = 99.099, rounded up to 99.10, inside the 99.5 limit: taker fee 0.1982. The second is capped
    // at its 99.05 limit: fee 0.1981.
    let f = s.fills();
    assert_eq!(f.len(), 2);
    assert!(f.iter().all(|f| f.liquidity == Liquidity::Taker));
    approx(f[0].price, 99.10);
    approx(f[0].fee, 0.1982);
    approx(f[1].price, 99.05);
    approx(f[1].fee, 0.1981);
    approx(s.portfolio().cash(), 10_000.0 - 99.2982 - 99.2481);
}

#[test]
fn a_triggered_stop_limit_the_next_bar_opens_through_fills_at_its_limit() {
    let i = spot();
    let bars = [
        bar(0, 100.0, 101.0, 99.0, 100.0),
        bar(1, 104.0, 105.0, 103.8, 104.5),
        bar(2, 102.0, 103.0, 101.5, 102.5),
    ];
    let (script, _) = Script::new(vec![(
        0,
        req(
            &i,
            Side::Buy,
            1.0,
            OrderType::StopLimit {
                trigger: 103.0,
                price: 103.5,
            },
        ),
    )]);
    let s = run(i, 10.0, &bars, script);
    // Bar 1 opens at 104, past the 103 trigger and above the 103.5 limit: triggered, and the
    // limit rests. Bar 2 opens at 102, through the resting limit, so it buys at 103.5, not 102,
    // as a maker: fee 0.1035.
    let f = s.fills();
    assert_eq!(f.len(), 1);
    assert_eq!((f[0].order_id.0, f[0].time), (1, 2 * MIN));
    assert_eq!(f[0].liquidity, Liquidity::Maker);
    approx(f[0].price, 103.5);
    approx(f[0].fee, 0.1035);
    // Cash 10000 - 103.6035, plus 1 marked at 102.5.
    approx(s.portfolio().cash(), 9896.3965);
    approx(equity(&s)[2], 9998.8965);
}

/// A long entry resting at 100 with a stop, on a bar that opens at 90 after a close of 105.
fn resting_entry_with_stop(stop: f64, slippage: f64, low: f64) -> Session {
    let i = perp();
    let bars = [
        bar(0, 105.0, 106.0, 104.0, 105.0),
        bar(1, 90.0, 92.0, low, 91.0),
    ];
    let entry = req(&i, Side::Buy, 1.0, OrderType::Limit { price: 100.0 });
    let strategy = steps(move |ctx, n| {
        if n == 0 {
            ctx.submit_with_stop(entry.clone(), stop).unwrap();
        }
    });
    run(i, slippage, &bars, strategy)
}

#[test]
fn an_attached_stop_on_a_resting_entry_a_bar_opens_through_fills_at_its_trigger_on_the_way() {
    let s = resting_entry_with_stop(95.0, 10.0, 89.0);
    // Bar 1 opens at 90, through the resting entry at 100 and its stop at 95. The market traded
    // down through 100 between the bars, where the entry bought at its limit as a maker, fee 0.1
    // (no slippage on a limit). It went on down to the open of 90, crossing the stop at 95 on the
    // way, so the stop sells at 95 less slippage: 95 * 0.999 = 94.905, rounded down to 94.90, as
    // a taker, fee 0.2% of 94.90 = 0.1898. Realised 94.90 - 100 = -5.10.
    let f = s.fills();
    assert_eq!(f.len(), 2);
    assert_eq!(
        (f[0].order_id.0, f[0].time, f[0].liquidity),
        (1, MIN, Liquidity::Maker)
    );
    approx(f[0].price, 100.0);
    approx(f[0].fee, 0.1);
    assert_eq!(
        (f[1].order_id.0, f[1].time, f[1].liquidity),
        (2, MIN, Liquidity::Taker)
    );
    approx(f[1].price, 94.90);
    approx(f[1].fee, 0.1898);
    let p = s.portfolio();
    assert_eq!(p.position(), 0.0);
    approx(p.realised_pnl(), -5.10);
    // 10000 - 0.1 - 0.1898 - 5.10.
    approx(p.cash(), 9994.6102);
}

#[test]
fn a_stop_crossed_on_the_way_to_the_open_fills_before_the_fills_at_the_open() {
    let i = perp();
    let bars = [
        bar(0, 105.0, 106.0, 104.0, 105.0),
        bar(1, 90.0, 92.0, 89.0, 91.0),
    ];
    let entry = req(&i, Side::Buy, 1.0, OrderType::Limit { price: 100.0 });
    let exit = reduce_only(req(&i, Side::Sell, 1.0, OrderType::Market));
    let strategy = steps(move |ctx, n| {
        if n == 0 {
            ctx.submit_with_stop(entry.clone(), 95.0).unwrap();
            ctx.submit(exit.clone()).unwrap();
        }
    });
    let s = run(i, 0.0, &bars, strategy);
    // Between the bars the entry buys at 100 and its stop sells at 95, both before the open of 90,
    // where the reduce-only market exit (order 3) finds nothing left to close. Realised -5.
    let f = s.fills();
    assert_eq!(f.len(), 2);
    assert_eq!((f[1].order_id.0, f[1].time), (2, MIN));
    approx(f[1].price, 95.0);
    assert_eq!(
        s.orders()[2].status,
        OrderStatus::Cancelled("reduce-only order would not reduce the position".into())
    );
    approx(s.portfolio().realised_pnl(), -5.0);
}

#[test]
fn an_attached_stop_below_the_open_on_a_resting_entry_fills_inside_the_range() {
    let s = resting_entry_with_stop(85.0, 0.0, 80.0);
    // The entry buys at 100 between the bars, fee 0.1. The stop at 85 is below the open of 90, so
    // the market did not reach it on the way; the low of 80 does inside the range: it sells at 85,
    // fee 0.17. Realised -15: cash 10000 - 0.1 - 0.17 - 15.
    let f = s.fills();
    assert_eq!(f.len(), 2);
    approx(f[0].price, 100.0);
    assert_eq!((f[1].order_id.0, f[1].time), (2, MIN));
    approx(f[1].price, 85.0);
    approx(f[1].fee, 0.17);
    approx(s.portfolio().realised_pnl(), -15.0);
    approx(s.portfolio().cash(), 9984.73);
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
    // Unsorted on purpose; the event before the first bar falls in no bar and is not charged, and
    // the one at 2 * H is due at bar 2's open.
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
    // Fill on bar 1 at 100, fee 0.4. The funding in bar 0 meets a flat position. Bar 1's event is
    // inside the bar: 2 * 105 (the close) * 0.0002 = 0.042. Bar 2's is due at its open, on the
    // long held going into it: 2 * 105 (the open) * 0.0001 = 0.021.
    let p = s.portfolio();
    approx(p.funding_paid(), 0.063);
    approx(p.cash(), 10_000.0 - 0.4 - 0.063);
    // Equity on bar 2: cash 9999.537 plus 2 * (110 - 100).
    approx(equity(&s)[1], 10_000.0 - 0.4 - 0.042 + 10.0);
    approx(equity(&s)[2], 10_019.537);
}

#[test]
fn funding_is_paid_to_shorts_on_a_positive_rate() {
    let s = funding_session(Side::Sell);
    let p = s.portfolio();
    approx(p.funding_paid(), -0.063);
    approx(p.cash(), 10_000.0 - 0.4 + 0.063);
    // Equity on bar 2: cash 9999.663 plus -2 * (110 - 100).
    approx(equity(&s)[2], 9_979.663);
}

#[test]
fn funding_due_at_the_open_is_charged_on_a_long_stopped_out_inside_the_bar() {
    let i = perp();
    let bars = [
        bar(0, 100.0, 101.0, 99.0, 100.0),
        bar(1, 100.0, 101.0, 99.0, 100.0),
        bar(2, 100.0, 101.0, 94.0, 96.0),
    ];
    let (script, _) = Script::new(vec![
        (0, req(&i, Side::Buy, 2.0, OrderType::Market)),
        (
            1,
            reduce_only(req(&i, Side::Sell, 2.0, OrderType::Stop { trigger: 95.0 })),
        ),
    ]);
    let funding = [(2 * MIN, 0.001)];
    let s = run_backtest(script, i, Timeframe::M1, &bars, &funding, config(0.0)).unwrap();
    // The long of 2 bought at 100 on bar 1 (fee 0.4) is still held at bar 2's open, when the
    // funding is due: 2 * 100 * 0.001 = 0.2. The stop sells it later in the bar, at 95: fee 0.38,
    // realised -10.
    let p = s.portfolio();
    assert_eq!(p.position(), 0.0);
    approx(p.funding_paid(), 0.2);
    // 10000 - 0.4 - 0.2 - 10 - 0.38.
    approx(p.cash(), 9989.02);
    approx(equity(&s)[2], 9989.02);
}

#[test]
fn an_entry_inside_the_bar_does_not_pay_the_funding_due_at_its_open() {
    let i = perp();
    let bars = [
        bar(0, 100.0, 101.0, 99.0, 100.0),
        bar(1, 100.0, 101.0, 98.0, 100.0),
    ];
    let (script, _) = Script::new(vec![(
        0,
        req(&i, Side::Buy, 2.0, OrderType::Limit { price: 99.0 }),
    )]);
    let funding = [(MIN, 0.001)];
    let s = run_backtest(script, i, Timeframe::M1, &bars, &funding, config(0.0)).unwrap();
    // Flat at bar 1's open, when the funding is due, so nothing is paid. The limit then buys 2 at
    // 99 inside the bar, as a maker: fee 0.198.
    let p = s.portfolio();
    approx(p.position(), 2.0);
    assert_eq!(p.funding_paid(), 0.0);
    approx(p.cash(), 9999.802);
    // Marked at 100: 9999.802 + 2 * (100 - 99).
    approx(equity(&s)[1], 10_001.802);
}

#[test]
fn a_daily_bar_charges_funding_at_its_open_and_inside_it_on_different_positions() {
    let i = perp();
    const D: i64 = 86_400_000;
    const H8: i64 = 8 * 3_600_000;
    let day = |n: i64, o, h, l, c| Candle {
        open_time: n * D,
        ..bar(0, o, h, l, c)
    };
    let bars = [
        day(0, 100.0, 101.0, 99.0, 100.0),
        day(1, 100.0, 101.0, 99.0, 100.0),
        day(2, 100.0, 104.0, 97.0, 102.0),
    ];
    let (script, _) = Script::new(vec![
        (0, req(&i, Side::Buy, 1.0, OrderType::Market)),
        (1, req(&i, Side::Buy, 1.0, OrderType::Limit { price: 98.0 })),
    ]);
    // 00:00, 08:00 and 16:00 UTC on days 1 and 2.
    let funding = [
        (D, 0.0001),
        (D + H8, 0.0001),
        (D + 2 * H8, 0.0001),
        (2 * D, 0.0001),
        (2 * D + H8, 0.0002),
        (2 * D + 2 * H8, 0.0003),
    ];
    let s = run_backtest(script, i, Timeframe::D1, &bars, &funding, config(0.0)).unwrap();
    // Day 1: flat at 00:00, so nothing is due then. The market buy fills at the open at 100, fee
    // 0.2, and 08:00 and 16:00 are charged on that long of 1 at the close: 1 * 100 * 0.0001 = 0.01
    // each. Equity: 10000 - 0.2 - 0.02 = 9999.78.
    let e = equity(&s);
    approx(e[1], 9999.78);
    // Day 2: 00:00 is charged on the long of 1 held going into the day, at the open of 100: 0.01.
    // The limit buys 1 more at 98 inside the bar, maker fee 0.098, for a long of 2 at an average
    // of 99. 08:00 and 16:00 are charged on that long at the close of 102: 2 * 102 * 0.0002 =
    // 0.0408 and 2 * 102 * 0.0003 = 0.0612.
    let p = s.portfolio();
    approx(p.funding_paid(), 0.02 + 0.01 + 0.0408 + 0.0612);
    // Cash 10000 - 0.2 - 0.098 - 0.132 = 9999.57, plus 2 * (102 - 99).
    approx(p.cash(), 9999.57);
    approx(e[2], 10_005.57);
    // The session records that split per bar, for the trade list.
    let split: Vec<(f64, f64)> = s
        .bar_funding()
        .iter()
        .map(|f| (f.at_open, f.in_bar))
        .collect();
    assert_eq!(split.len(), 3);
    assert_eq!(split[0], (0.0, 0.0));
    assert_eq!(split[1].0, 0.0);
    approx(split[1].1, 0.02);
    approx(split[2].0, 0.01);
    approx(split[2].1, 0.0408 + 0.0612);
}

#[test]
fn funding_due_in_a_gap_is_charged_at_the_next_bar_open() {
    let i = perp();
    const H: i64 = 3_600_000;
    let hour = |n: i64, o, h, l, c| Candle {
        open_time: n * H,
        ..bar(0, o, h, l, c)
    };
    // The 08:00 bar is missing.
    let bars = [
        hour(6, 100.0, 100.0, 100.0, 100.0),
        hour(7, 100.0, 101.0, 99.0, 100.0),
        hour(9, 104.0, 105.0, 103.0, 104.0),
        hour(10, 104.0, 104.0, 104.0, 104.0),
    ];
    let (script, _) = Script::new(vec![(0, req(&i, Side::Buy, 2.0, OrderType::Market))]);
    // 08:00 falls in the gap; 05:00 is before the first bar and is not charged.
    let funding = [(8 * H, 0.0001), (5 * H, 0.01)];
    let s = run_backtest(script, i, Timeframe::H1, &bars, &funding, config(0.0)).unwrap();
    // The long of 2 bought at 07:00's open at 100 (fee 0.4) is held across the gap. The 08:00
    // funding is charged at the 09:00 bar's open on that long, at the open price: 2 * 104 * 0.0001
    // = 0.0208.
    let p = s.portfolio();
    approx(p.funding_paid(), 0.0208);
    approx(p.cash(), 10_000.0 - 0.4 - 0.0208);
    // Equity at 09:00: cash 9999.5792 plus 2 * (104 - 100).
    approx(equity(&s)[2], 10_007.579_2);
    let f = s.bar_funding()[2];
    approx(f.at_open, 0.0208);
    assert_eq!(f.in_bar, 0.0);

    // A session fed directly takes the gap's funding with the next bar, but not funding before the
    // first bar or in the bar before the gap.
    let (script, _) = Script::new(vec![]);
    let mut s = Session::new(script, perp(), Timeframe::H1, config(0.0)).unwrap();
    assert!(matches!(
        s.on_bar(bars[0], &[(5 * H, 0.0001)]),
        Err(EngineError::FundingOutsideBar { .. })
    ));
    s.on_bar(bars[0], &[]).unwrap();
    s.on_bar(bars[1], &[]).unwrap();
    assert!(matches!(
        s.on_bar(bars[2], &[(7 * H, 0.0001)]),
        Err(EngineError::FundingOutsideBar { .. })
    ));
    s.on_bar(bars[2], &[(8 * H, 0.0001)]).unwrap();
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
fn spot_buys_must_fit_in_the_cash_with_the_taker_fee_and_open_buys() {
    let i = spot();
    let bars = [
        bar(0, 100.0, 101.0, 99.0, 100.0),
        bar(1, 100.0, 101.0, 99.0, 100.0),
    ];
    let (script, log) = Script::new(vec![
        // 1000 * 100 = 100000, plus the 0.2% taker fee 100200, against 10000 of cash.
        (0, req(&i, Side::Buy, 1000.0, OrderType::Market)),
        // 49 * 100 * 1.002 = 4909.8.
        (0, req(&i, Side::Buy, 49.0, OrderType::Market)),
        // Valued at its limit: 50 * 98 * 1.002 = 4909.8, so 9819.6 with the open buy.
        (
            0,
            req(&i, Side::Buy, 50.0, OrderType::Limit { price: 98.0 }),
        ),
        // 2 * 100 * 1.002 = 200.4 would make 10020.
        (0, req(&i, Side::Buy, 2.0, OrderType::Market)),
        // 1.5 * 100 * 1.002 = 150.3 makes 9969.9, which fits.
        (0, req(&i, Side::Buy, 1.5, OrderType::Market)),
    ]);
    let s = run(i, 0.0, &bars, script);
    let log = log.lock().unwrap();
    assert!(log[1].is_ok() && log[2].is_ok() && log[4].is_ok());
    for rejected in [&log[0], &log[3]] {
        let err = rejected.as_ref().unwrap_err();
        assert!(
            err.reason
                .starts_with("spot cannot buy for more than its cash"),
            "{}",
            err.reason
        );
        assert!(matches!(
            s.orders()[err.id.0 as usize - 1].status,
            OrderStatus::Rejected(_)
        ));
    }
    // Bar 1 fills the two market buys at 100: 50.5 * 100 = 5050, fees 10.1. The 98 limit rests.
    approx(s.portfolio().position(), 50.5);
    approx(s.portfolio().cash(), 4_939.9);
    assert_eq!(s.open_orders().len(), 1);

    // A perpetual has no cash check, but the same oversized buy is 10x, over the default 1x
    // leverage cap.
    let (script, log) = Script::new(vec![(
        0,
        req(&perp(), Side::Buy, 1000.0, OrderType::Market),
    )]);
    run(perp(), 0.0, &bars, script);
    let log = log.lock().unwrap();
    assert!(log[0].as_ref().unwrap_err().reason.contains("leverage cap"));
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
    // Bar 2 opens at 100, between the two, and reaches both 110 and 95 inside its range. The path
    // is unknown, so the stop is taken first, at 95.
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

#[test]
fn a_take_profit_the_bar_opens_through_fills_before_a_stop_inside_the_range() {
    let i = perp();
    let bars = [
        bar(0, 100.0, 101.0, 99.0, 100.0),
        bar(1, 100.0, 101.0, 99.0, 100.0),
        bar(2, 112.0, 113.0, 94.0, 95.0),
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
    // Bar 2 opens at 112, already through the 110 take-profit, so the take-profit fills at the
    // open, before anything inside the range: at its limit of 110 as a maker, fee 0.11. The fall
    // to 94 that reaches the stop comes after it, when the position is already closed.
    let f = s.fills();
    assert_eq!(f.len(), 2);
    assert_eq!((f[1].order_id.0, f[1].time), (2, 2 * MIN));
    approx(f[1].price, 110.0);
    approx(f[1].fee, 0.11);
    assert_eq!(f[1].liquidity, Liquidity::Maker);
    assert_eq!(
        s.orders()[2].status,
        OrderStatus::Cancelled("reduce-only order would not reduce the position".into())
    );
    // Entry at 100, fee 0.2. Realised 110 - 100 = 10: cash 10000 - 0.2 + 10 - 0.11 = 10009.69.
    let p = s.portfolio();
    assert_eq!(p.position(), 0.0);
    approx(p.realised_pnl(), 10.0);
    approx(p.cash(), 10009.69);
    approx(equity(&s)[2], 10009.69);
}

/// Long 1 from bar 1 at 100. On bar 1's close the strategy adds 1 at market with a stop at 95
/// (orders 2 and 3) and sends `exit` for the whole position (order 4). Bar 2 opens at 112 and
/// falls to 94.
fn add_with_stop_and_exit(exit: OrderType) -> Session {
    let i = perp();
    let bars = [
        bar(0, 100.0, 101.0, 99.0, 100.0),
        bar(1, 100.0, 101.0, 99.0, 100.0),
        bar(2, 112.0, 113.0, 94.0, 95.0),
    ];
    let add = req(&i, Side::Buy, 1.0, OrderType::Market);
    let exit = reduce_only(req(&i, Side::Sell, 2.0, exit));
    let strategy = steps(move |ctx, n| match n {
        0 => {
            ctx.submit(add.clone()).unwrap();
        }
        1 => {
            ctx.submit_with_stop(add.clone(), 95.0).unwrap();
            ctx.submit(exit.clone()).unwrap();
        }
        _ => {}
    });
    run(i, 0.0, &bars, strategy)
}

#[test]
fn a_take_profit_the_bar_opens_through_fills_before_a_market_order_at_the_open() {
    let s = add_with_stop_and_exit(OrderType::Limit { price: 110.0 });
    // Bar 1: order 1 buys 1 at 100, fee 0.2. Bar 2 opens at 112, through the take-profit at 110:
    // the market passed 110 between the bars, before the open, when the long was still 1. So the
    // take-profit fills first, selling that 1 at its limit of 110 as a maker, fee 0.11, realised
    // +10. Then order 2 buys 1 at the open of 112, fee 0.224. Its stop at 95 is below the open, and
    // the fall to 94 inside the range reaches it: it sells 1 at 95, fee 0.19, realised 95 - 112 =
    // -17. No unit is sold at 110 in the same instant it was bought at 112. The take-profit was
    // for 2 but had only 1 to reduce: like any reduce-only order it fills once, cut to the
    // position it can close then, so its other 1 does not wait for the open.
    let f = s.fills();
    assert_eq!(f.len(), 4);
    let fill = |n: usize| (f[n].order_id.0, f[n].time, f[n].side, f[n].liquidity);
    assert_eq!(fill(1), (4, 2 * MIN, Side::Sell, Liquidity::Maker));
    approx(f[1].price, 110.0);
    approx(f[1].qty, 1.0);
    approx(f[1].fee, 0.11);
    assert_eq!(fill(2), (2, 2 * MIN, Side::Buy, Liquidity::Taker));
    approx(f[2].price, 112.0);
    approx(f[2].fee, 0.224);
    assert_eq!(fill(3), (3, 2 * MIN, Side::Sell, Liquidity::Taker));
    approx(f[3].price, 95.0);
    approx(f[3].qty, 1.0);
    approx(f[3].fee, 0.19);
    assert!(s.orders().iter().all(|o| o.status == OrderStatus::Filled));
    // Realised 10 - 17 = -7: cash 10000 - 0.2 - 0.11 - 0.224 - 0.19 - 7 = 9992.276, flat.
    let p = s.portfolio();
    assert_eq!(p.position(), 0.0);
    approx(p.realised_pnl(), -7.0);
    approx(p.cash(), 9992.276);
    approx(equity(&s)[2], 9992.276);
}

#[test]
fn an_attached_stop_inside_the_range_waits_for_the_fills_at_the_open() {
    let s = add_with_stop_and_exit(OrderType::Market);
    // Bar 2 opens at 112: order 2 buys 1 there, fee 0.224, for a long of 2 at an average of 106,
    // and the exit (order 4) sells both at the open, fee 0.448. The stop at 95 is placed with
    // order 2's fill, but the bar only reaches it inside the range, after the fills at the open,
    // and by then there is nothing left to close.
    let f = s.fills();
    assert_eq!(f.len(), 3);
    let fill = |n: usize| (f[n].order_id.0, f[n].time, f[n].side);
    assert_eq!(fill(1), (2, 2 * MIN, Side::Buy));
    approx(f[1].price, 112.0);
    assert_eq!(fill(2), (4, 2 * MIN, Side::Sell));
    approx(f[2].price, 112.0);
    approx(f[2].qty, 2.0);
    approx(f[2].fee, 0.448);
    assert_eq!(
        s.orders()[2].status,
        OrderStatus::Cancelled("reduce-only order would not reduce the position".into())
    );
    // Realised 2 * (112 - 106) = 12: cash 10000 - 0.2 - 0.224 - 0.448 + 12 = 10011.128, flat.
    let p = s.portfolio();
    assert_eq!(p.position(), 0.0);
    approx(p.realised_pnl(), 12.0);
    approx(p.cash(), 10011.128);
}

/// The common bracket: a market buy of 1 (order 1, left out when `entry` is false) and a
/// reduce-only take-profit selling 1 at `take_profit` (order 2), both sent on the close of 100.
/// Bar 1 opens at 105, through the take-profit. Slippage 10 bps.
fn bracket(entry: bool, take_profit: f64) -> Session {
    let i = perp();
    let bars = [
        bar(0, 100.0, 101.0, 99.0, 100.0),
        bar(1, 105.0, 106.0, 104.0, 105.0),
    ];
    let buy = req(&i, Side::Buy, 1.0, OrderType::Market);
    let sell = reduce_only(req(
        &i,
        Side::Sell,
        1.0,
        OrderType::Limit { price: take_profit },
    ));
    let strategy = steps(move |ctx, n| {
        if n == 0 {
            if entry {
                ctx.submit(buy.clone()).unwrap();
            }
            ctx.submit(sell.clone()).unwrap();
        }
    });
    run(i, 10.0, &bars, strategy)
}

#[test]
fn a_take_profit_sent_with_its_entry_fills_at_the_open_when_the_next_bar_opens_through_it() {
    let s = bracket(true, 103.0);
    // The market passed 103 between the bars, before the entry filled, so the take-profit had
    // nothing to reduce then. It waits for the open. The entry buys there: 105 * 1.001 = 105.105,
    // rounded up to 105.11, taker fee 0.2% = 0.21022. Then the take-profit has the long to reduce
    // and takes liquidity at the open: 105 * 0.999 = 104.895, rounded down to 104.89, above its
    // limit of 103, taker fee 0.20978. Realised 104.89 - 105.11 = -0.22.
    let f = s.fills();
    assert_eq!(f.len(), 2);
    let fill = |n: usize| (f[n].order_id.0, f[n].time, f[n].side, f[n].liquidity);
    assert_eq!(fill(0), (1, MIN, Side::Buy, Liquidity::Taker));
    approx(f[0].price, 105.11);
    approx(f[0].fee, 0.21022);
    assert_eq!(fill(1), (2, MIN, Side::Sell, Liquidity::Taker));
    approx(f[1].price, 104.89);
    approx(f[1].qty, 1.0);
    approx(f[1].fee, 0.20978);
    assert!(s.orders().iter().all(|o| o.status == OrderStatus::Filled));
    let p = s.portfolio();
    assert_eq!(p.position(), 0.0);
    approx(p.realised_pnl(), -0.22);
    // 10000 - 0.21022 - 0.20978 - 0.22.
    approx(p.cash(), 9999.36);
    approx(equity(&s)[1], 9999.36);
}

#[test]
fn a_take_profit_that_waited_for_the_open_fills_no_worse_than_its_limit() {
    let s = bracket(true, 104.95);
    // As above, but the open less slippage, 104.89, is below the limit of 104.95: the take-profit
    // sells at 104.95, taker fee 0.2099. Realised 104.95 - 105.11 = -0.16: cash 10000 - 0.21022
    // - 0.2099 - 0.16 = 9999.41988.
    let f = s.fills();
    assert_eq!(f.len(), 2);
    assert_eq!((f[1].order_id.0, f[1].liquidity), (2, Liquidity::Taker));
    approx(f[1].price, 104.95);
    approx(f[1].fee, 0.2099);
    assert_eq!(s.portfolio().position(), 0.0);
    approx(s.portfolio().realised_pnl(), -0.16);
    approx(s.portfolio().cash(), 9999.41988);
}

#[test]
fn a_take_profit_with_nothing_to_reduce_at_the_open_either_is_cancelled() {
    let s = bracket(false, 103.0);
    // No entry: the take-profit has nothing to reduce between the bars or at the open.
    assert!(s.fills().is_empty());
    assert_eq!(
        s.orders()[0].status,
        OrderStatus::Cancelled("reduce-only order would not reduce the position".into())
    );
    assert!(s.open_orders().is_empty());
    approx(s.portfolio().cash(), 10_000.0);
}

#[test]
fn limits_filled_between_the_bars_fill_in_the_order_the_market_passed_them() {
    let i = perp();
    let bars = [
        bar(0, 100.0, 101.0, 99.0, 100.0),
        bar(1, 100.0, 101.0, 99.0, 100.0),
        bar(2, 90.0, 91.0, 89.0, 90.0),
    ];
    let cover = |price| reduce_only(req(&i, Side::Buy, 2.0, OrderType::Limit { price }));
    let (script, _) = Script::new(vec![
        (0, req(&i, Side::Sell, 2.0, OrderType::Market)),
        (1, cover(96.0)),
        (1, cover(98.0)),
    ]);
    let s = run(i, 0.0, &bars, script);
    // Short 2 from bar 1 at 100, fee 0.4. Bar 2 opens at 90, through both covers. Falling from
    // 100 to 90 between the bars, the market passed 98 (order 3) before 96 (order 2), so order 3
    // closes the short at 98 as a maker, fee 0.196, and order 2 finds nothing to close. Realised
    // 2 * (100 - 98) = 4: cash 10000 - 0.4 - 0.196 + 4.
    let f = s.fills();
    assert_eq!(f.len(), 2);
    assert_eq!((f[1].order_id.0, f[1].time), (3, 2 * MIN));
    approx(f[1].price, 98.0);
    approx(f[1].qty, 2.0);
    approx(f[1].fee, 0.196);
    assert_eq!(
        s.orders()[1].status,
        OrderStatus::Cancelled("reduce-only order would not reduce the position".into())
    );
    let p = s.portfolio();
    assert_eq!(p.position(), 0.0);
    approx(p.realised_pnl(), 4.0);
    approx(p.cash(), 10003.404);
}

#[test]
fn a_reduce_only_order_closes_at_most_the_position_and_is_cancelled_when_flat() {
    let i = perp();
    let bars: Vec<Candle> = (0..4).map(|n| bar(n, 100.0, 101.0, 99.0, 100.0)).collect();
    let (script, log) = Script::new(vec![
        (0, req(&i, Side::Buy, 1.0, OrderType::Market)),
        // Long 1: a reduce-only sell of 2 can close only the 1.
        (1, reduce_only(req(&i, Side::Sell, 2.0, OrderType::Market))),
        // Flat after bar 2: nothing is left to reduce.
        (2, reduce_only(req(&i, Side::Sell, 1.0, OrderType::Market))),
    ]);
    let s = run(i, 0.0, &bars, script);
    assert!(log.lock().unwrap().iter().all(|r| r.is_ok()));
    let f = s.fills();
    assert_eq!(f.len(), 2);
    // Bar 2: the sell fills 1 at 100, taker fee 0.2, and the order is done.
    assert_eq!((f[1].order_id.0, f[1].time), (2, 2 * MIN));
    assert_eq!(f[1].side, Side::Sell);
    approx(f[1].qty, 1.0);
    approx(f[1].fee, 0.2);
    assert_eq!(s.orders()[1].status, OrderStatus::Filled);
    // Bar 3: the second sell finds no position and is cancelled.
    assert_eq!(
        s.orders()[2].status,
        OrderStatus::Cancelled("reduce-only order would not reduce the position".into())
    );
    let p = s.portfolio();
    assert_eq!(p.position(), 0.0);
    approx(p.cash(), 10_000.0 - 0.2 - 0.2);
    assert!(s.open_orders().is_empty());
}

#[test]
fn an_attached_stop_covers_the_bar_its_entry_fills_on() {
    let i = perp();
    let bars = [
        bar(0, 110.0, 111.0, 109.0, 110.0),
        bar(1, 112.0, 113.0, 100.0, 108.0),
    ];
    let entry = req(&i, Side::Buy, 1.0, OrderType::Market);
    let strategy = steps(move |ctx, n| {
        if n == 0 {
            // The entry's id and its stop's.
            assert_eq!(
                ctx.submit_with_stop(entry.clone(), 106.0),
                Ok((OrderId(1), OrderId(2)))
            );
        }
    });
    let s = run(i, 0.0, &bars, strategy);
    // The entry fills at bar 1's open of 112, fee 0.224. Its stop is placed then, and bar 1's low
    // of 100 counts as coming after the fill, so the stop fills on the same bar at 106, fee
    // 0.212. A stop submitted from on_fill could first fill on bar 2, after the drop.
    let f = s.fills();
    assert_eq!(f.len(), 2);
    assert_eq!((f[0].order_id.0, f[0].time, f[0].side), (1, MIN, Side::Buy));
    approx(f[0].price, 112.0);
    approx(f[0].fee, 0.224);
    assert_eq!(
        (f[1].order_id.0, f[1].time, f[1].side),
        (2, MIN, Side::Sell)
    );
    assert_eq!(f[1].liquidity, Liquidity::Taker);
    approx(f[1].price, 106.0);
    approx(f[1].qty, 1.0);
    approx(f[1].fee, 0.212);
    let o = s.orders();
    assert_eq!(
        o[0].attached_stop,
        Some(AttachedStop {
            id: OrderId(2),
            trigger: 106.0
        })
    );
    assert_eq!(o[1].request.order_type, OrderType::Stop { trigger: 106.0 });
    assert!(o[1].request.reduce_only);
    // Decided together with its entry, on bar 0.
    assert_eq!(o[1].submitted_at, 0);
    assert!(o.iter().all(|o| o.status == OrderStatus::Filled));
    // Realised 106 - 112 = -6: cash 10000 - 0.224 - 6 - 0.212 = 9993.564, flat.
    let p = s.portfolio();
    assert_eq!(p.position(), 0.0);
    approx(p.cash(), 9993.564);
    approx(equity(&s)[1], 9993.564);
    assert!(s.open_orders().is_empty());
}

#[test]
fn an_attached_stop_fills_at_the_open_when_its_entry_fills_through_it() {
    let i = perp();
    let bars = [
        bar(0, 100.0, 101.0, 99.0, 100.0),
        bar(1, 98.0, 99.5, 97.0, 99.0),
    ];
    let entry = req(&i, Side::Buy, 1.0, OrderType::Market);
    let strategy = steps(move |ctx, n| {
        if n == 0 {
            ctx.submit_with_stop(entry.clone(), 99.0).unwrap();
        }
    });
    let s = run(i, 10.0, &bars, strategy);
    // Bar 1 opens at 98, already under the 99 stop. The entry pays 98 * 1.001 = 98.098, rounded
    // up to 98.10, fee 0.1962. The market is at 98, through the stop, so the stop sells there at
    // once: 98 * 0.999 = 97.902, rounded down to 97.90, fee 0.1958.
    let f = s.fills();
    assert_eq!(f.len(), 2);
    approx(f[0].price, 98.10);
    approx(f[0].fee, 0.1962);
    assert_eq!((f[1].order_id.0, f[1].time), (2, MIN));
    approx(f[1].price, 97.90);
    approx(f[1].fee, 0.1958);
    // Realised 97.90 - 98.10 = -0.20: cash 10000 - 0.1962 - 0.20 - 0.1958 = 9999.408.
    let p = s.portfolio();
    assert_eq!(p.position(), 0.0);
    approx(p.realised_pnl(), -0.20);
    approx(p.cash(), 9999.408);
}

#[test]
fn an_attached_stop_is_an_ordinary_open_order_once_its_entry_fills() {
    let i = perp();
    let entry = req(&i, Side::Sell, 1.0, OrderType::Market);
    let strategy = steps(move |ctx, n| {
        if n == 0 {
            ctx.submit_with_stop(entry.clone(), 102.0).unwrap();
        }
    });
    let mut s = Session::new(strategy, i, Timeframe::M1, config(10.0)).unwrap();
    s.on_bar(bar(0, 100.0, 101.0, 99.0, 100.0), &[]).unwrap();
    // Until the entry fills, the stop is recorded but not open.
    assert_eq!(s.orders()[1].status, OrderStatus::Attached);
    assert_eq!(s.open_orders().len(), 1);
    // Bar 1: the short fills at 100 * 0.999 = 99.90, fee 0.1998. The high of 101 stays under the
    // stop at 102, which now rests as an open reduce-only buy stop.
    s.on_bar(bar(1, 100.0, 101.0, 99.0, 100.5), &[]).unwrap();
    assert_eq!(s.open_orders().len(), 1);
    let stop = &s.open_orders()[0];
    assert_eq!((stop.id.0, stop.request.side), (2, Side::Buy));
    assert_eq!(stop.request.order_type, OrderType::Stop { trigger: 102.0 });
    assert!(stop.request.reduce_only);
    approx(stop.request.qty, 1.0);
    assert_eq!(s.orders()[1].status, OrderStatus::Open);
    // Bar 2: the high of 103 reaches it after an open of 101: max(101, 102) = 102, plus 10 bps
    // = 102.102, rounded up to 102.11, fee 0.20422.
    s.on_bar(bar(2, 101.0, 103.0, 100.5, 102.5), &[]).unwrap();
    let f = s.fills();
    assert_eq!(f.len(), 2);
    approx(f[0].price, 99.90);
    approx(f[0].fee, 0.1998);
    assert_eq!((f[1].order_id.0, f[1].time), (2, 2 * MIN));
    approx(f[1].price, 102.11);
    approx(f[1].fee, 0.20422);
    // Realised 99.90 - 102.11 = -2.21 on the short: cash 10000 - 0.1998 - 2.21 - 0.20422.
    let p = s.portfolio();
    assert_eq!(p.position(), 0.0);
    approx(p.cash(), 9997.38598);
    assert_eq!(s.orders()[1].status, OrderStatus::Filled);
    assert!(s.open_orders().is_empty());
}

#[test]
fn an_attached_stop_is_cancelled_with_its_entry() {
    let i = perp();
    let limit = req(&i, Side::Buy, 1.0, OrderType::Limit { price: 90.0 });
    let ioc_limit = ioc(req(&i, Side::Buy, 1.0, OrderType::Limit { price: 95.0 }));
    let strategy = steps(move |ctx, n| match n {
        0 => {
            ctx.submit_with_stop(limit.clone(), 85.0).unwrap();
            ctx.submit_with_stop(ioc_limit.clone(), 90.0).unwrap();
        }
        1 => {
            // The stop is not open before its entry fills, so it cannot be cancelled on its own.
            assert!(!ctx.cancel(OrderId(2)));
            assert!(ctx.cancel(OrderId(1)));
        }
        _ => {}
    });
    let bars = [
        bar(0, 100.0, 101.0, 99.0, 100.0),
        bar(1, 100.0, 101.0, 99.0, 100.0),
    ];
    let s = run(i, 0.0, &bars, strategy);
    // The IOC entry sees only bar 1's open of 100, above its limit, and is cancelled by the
    // broker. The strategy cancels the resting entry. Both stops go with their entries.
    let cancelled = |reason: &str| OrderStatus::Cancelled(reason.into());
    let statuses: Vec<_> = s.orders().iter().map(|o| o.status.clone()).collect();
    assert_eq!(
        statuses,
        [
            cancelled("cancelled by the strategy"),
            cancelled("its entry was cancelled before it filled"),
            cancelled("IOC order did not fill on its first bar"),
            cancelled("its entry was cancelled before it filled"),
        ]
    );
    assert!(s.fills().is_empty());
    assert!(s.open_orders().is_empty());
}

#[test]
fn an_attached_stop_must_sit_on_the_losing_side_of_its_entry() {
    let i = perp();
    let market = |side| req(&i, side, 1.0, OrderType::Market);
    let cases = vec![
        // Bar 0 closes at 100, which values the market entries.
        (market(Side::Buy), 100.0, Some("must be below its entry")),
        (
            req(&i, Side::Buy, 1.0, OrderType::Limit { price: 95.0 }),
            96.0,
            Some("must be below its entry"),
        ),
        (market(Side::Sell), 99.5, Some("must be above its entry")),
        (reduce_only(market(Side::Buy)), 99.0, Some("reduce-only")),
        (
            market(Side::Buy),
            0.0,
            Some("trigger must be a finite number above zero"),
        ),
        // Rounds to the nearest tick, 99.00.
        (market(Side::Buy), 98.996, None),
    ];
    let strategy = steps(move |ctx, n| {
        if n != 0 {
            return;
        }
        for (r, stop, error) in &cases {
            let result = ctx.submit_with_stop(r.clone(), *stop);
            match error {
                Some(text) => {
                    let reason = result.unwrap_err().reason;
                    assert!(reason.contains(text), "{reason}");
                }
                None => assert!(result.is_ok(), "{result:?}"),
            }
        }
    });
    let s = run(i, 0.0, &[bar(0, 100.0, 101.0, 99.0, 100.0)], strategy);
    // Rejected entries record no stop; the accepted one, order 6, has its stop as order 7.
    let o = s.orders();
    assert_eq!(o.len(), 7);
    assert!(o[..5].iter().all(|o| o.attached_stop.is_none()));
    assert_eq!(
        o[5].attached_stop,
        Some(AttachedStop {
            id: OrderId(7),
            trigger: 99.0
        })
    );
    assert_eq!(o[6].request.order_type, OrderType::Stop { trigger: 99.0 });
    assert_eq!(o[6].status, OrderStatus::Attached);

    // Spot has no shorts to protect, so a sell cannot carry a stop even with a holding.
    let i = spot();
    let buy = req(&i, Side::Buy, 1.0, OrderType::Market);
    let sell = req(&i, Side::Sell, 1.0, OrderType::Market);
    let strategy = steps(move |ctx, n| match n {
        0 => assert!(ctx.submit(buy.clone()).is_ok()),
        1 => {
            let reason = ctx
                .submit_with_stop(sell.clone(), 101.0)
                .unwrap_err()
                .reason;
            assert!(reason.contains("spot cannot attach"), "{reason}");
        }
        _ => {}
    });
    let bars = [
        bar(0, 100.0, 101.0, 99.0, 100.0),
        bar(1, 100.0, 101.0, 99.0, 100.0),
    ];
    let s = run(i, 0.0, &bars, strategy);
    assert_eq!(s.orders().len(), 2);
    assert_eq!(s.portfolio().position(), 1.0);
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
    let cases = [
        (-1.0, 0.0, 1.0),
        (f64::NAN, 0.0, 1.0),
        (1.0, -1.0, 1.0),
        (1.0, 10_000.0, 1.0),
        (1.0, 0.0, 0.0),
        (1.0, 0.0, -2.0),
        (1.0, 0.0, f64::INFINITY),
    ];
    for (cash, slip, max_leverage) in cases {
        let (script, _) = Script::new(vec![]);
        let cfg = td_engine::SessionConfig {
            starting_cash: cash,
            slippage_bps: slip,
            fees: None,
            max_leverage,
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
    // 3, so the stop sits 2 * 3 = 6 away and the size risks 1% of 10000 on that distance:
    // 100 / 6 = 16.666 after rounding down to the lot. The stop is attached to the entry, 6 under
    // the close of 110 it was decided on: 104. The entry fills at bar 6's open of 112.
    let entry = &s.fills()[0];
    assert_eq!(entry.time, 6 * MIN);
    approx(entry.price, 112.0);
    approx(entry.qty, 16.666);
    assert_eq!(
        orders[0].attached_stop,
        Some(AttachedStop {
            id: OrderId(2),
            trigger: 104.0
        })
    );
    let OrderType::Stop { trigger } = orders[1].request.order_type else {
        unreachable!()
    };
    approx(trigger, 104.0);
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

#[test]
fn donchian_skips_an_entry_whose_stop_would_be_at_or_below_zero() {
    let p = Params::new()
        .with("entry", td_engine::ParamValue::Int(5))
        .with("exit", td_engine::ParamValue::Int(3))
        .with("atr_period", td_engine::ParamValue::Int(3))
        .with("atr_stop", td_engine::ParamValue::Float(40.0));
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
    // ATR is 3, so the stop sits 40 * 3 = 120 under the close. On the breakouts from bar 5 (close
    // 110) to bar 10 (close 120) that is at or below zero, and the strategy sends no order. Bar 11
    // closes at 122, so the stop is at 2: the entry goes out, sized 100 / 120 = 0.833 after
    // rounding down to the lot, and fills at bar 12's open of 124. Bar 41's exit closes it.
    let o = s.orders();
    assert_eq!(o.len(), 3, "entry, its stop and the exit: {o:?}");
    assert!(
        o.iter()
            .all(|o| !matches!(o.status, OrderStatus::Rejected(_)))
    );
    assert_eq!(o[0].submitted_at, 11 * MIN);
    assert_eq!(
        o[0].attached_stop,
        Some(AttachedStop {
            id: OrderId(2),
            trigger: 2.0
        })
    );
    let entry = &s.fills()[0];
    assert_eq!(entry.time, 12 * MIN);
    approx(entry.price, 124.0);
    approx(entry.qty, 0.833);
    assert_eq!(s.fills()[1].time, 42 * MIN);
    assert_eq!(s.portfolio().position(), 0.0);
}

#[test]
fn reference_strategies_size_full_spot_entries_to_pass_the_cash_check() {
    let sma = Params::new()
        .with("fast", td_engine::ParamValue::Int(3))
        .with("slow", td_engine::ParamValue::Int(8))
        .with("equity_fraction", td_engine::ParamValue::Float(1.0));
    let donchian = Params::new()
        .with("entry", td_engine::ParamValue::Int(5))
        .with("exit", td_engine::ParamValue::Int(3))
        .with("atr_period", td_engine::ParamValue::Int(3))
        .with("risk", td_engine::ParamValue::Float(1.0));
    // Uncapped, both would buy 10000 / close, which costs 10020 with the taker fee. Capped at
    // 10000 * 0.995 / (close * (1 + 0.002 + 0.0005)) with 5 bps of slippage: SMA decides on bar
    // 7's close of 114 and orders 87.063; Donchian decides on bar 5's close of 110 and orders
    // 90.228. Both pass the cash check. The next bar opens 2 higher, a gap bigger than the
    // headroom, so each buy is cut at fill time to the cash: SMA fills at 116 plus slippage,
    // 116.06, and 10000 / (116.06 * 1.002) = 85.990; Donchian at 112.06, and
    // 10000 / (112.06 * 1.002) = 89.059.
    let cases = [
        ("sma_cross", sma, 85.99),
        ("donchian_breakout", donchian, 89.059),
    ];
    for (name, params, qty) in cases {
        let strategy = build_strategy(name, &params).unwrap();
        let bars = up_then_down();
        let s = run_backtest(strategy, spot(), Timeframe::M1, &bars, &[], config(5.0)).unwrap();
        assert!(
            s.orders()
                .iter()
                .all(|o| !matches!(o.status, OrderStatus::Rejected(_))),
            "{name}: {:?}",
            s.orders()
        );
        let entry = &s.fills()[0];
        assert_eq!(entry.side, Side::Buy, "{name}");
        approx(entry.qty, qty);
        assert!(s.portfolio().cash() >= 0.0, "{name}");
    }
}

#[test]
fn warm_up_bars_count_as_history_and_the_strategy_trades_on_the_first_live_bar() {
    let p = Params::new()
        .with("fast", td_engine::ParamValue::Int(3))
        .with("slow", td_engine::ParamValue::Int(8));
    let strategy = build_strategy("sma_cross", &p).unwrap();
    let mut s = Session::new(strategy, spot(), Timeframe::M1, config(0.0)).unwrap();
    let bars = up_then_down();
    // The slow average needs 8 bars: warm up on bars 0 to 7. Bar 7 has them, but the strategy
    // does not trade while warming up. Warm-up records no equity and leaves the portfolio as it
    // started.
    for b in &bars[..8] {
        s.warm_up(*b, &[]).unwrap();
    }
    assert_eq!(s.bars().len(), 8);
    assert!(s.orders().is_empty(), "{:?}", s.orders());
    assert!(s.equity().is_empty());
    assert_eq!(
        s.portfolio(),
        &Portfolio::new(InstrumentKind::Spot, 10_000.0)
    );
    // Bar 8 is the first live bar. The fast average of the closes 112, 114 and 116 is 114, above
    // the slow average of the closes 102 to 116, 109, so the strategy buys 95% of the 10000
    // equity at the close of 116: 9500 / 116 = 81.896 after rounding down to the lot (the cash
    // cap, 9950 / (116 * 1.002) = 85.604, does not bind). The buy fills at bar 9's open of 118
    // with the 0.2% taker fee: 81.896 * 118 = 9663.728, fee 19.327456. Cash is then
    // 10000 - 9663.728 - 19.327456 = 316.944544, and equity at bar 9's close of 118 is
    // 316.944544 + 9663.728 = 9980.672544.
    s.on_bar(bars[8], &[]).unwrap();
    s.on_bar(bars[9], &[]).unwrap();
    assert_eq!(s.orders()[0].submitted_at, 8 * MIN);
    let f = s.fills();
    assert_eq!(f.len(), 1);
    assert_eq!((f[0].time, f[0].side), (9 * MIN, Side::Buy));
    approx(f[0].price, 118.0);
    approx(f[0].qty, 81.896);
    approx(f[0].fee, 19.327456);
    approx(s.portfolio().cash(), 316.944544);
    // The equity series starts at the first live bar.
    let times: Vec<i64> = s.equity().iter().map(|p| p.time).collect();
    assert_eq!(times, vec![8 * MIN, 9 * MIN]);
    let e = equity(&s);
    approx(e[0], 10_000.0);
    approx(e[1], 9980.672544);
}

#[test]
fn orders_submitted_while_warming_up_are_rejected_and_nothing_is_charged() {
    const WARM_UP: &str = "warm-up: trading is disabled";
    let log = Arc::new(Mutex::new(Vec::new()));
    let seen = log.clone();
    let strategy = steps(move |ctx, n| {
        let entry = req(ctx.instrument(), Side::Buy, 1.0, OrderType::Market);
        let plain = ctx.submit(entry.clone()).map_err(|e| e.reason);
        let stop = ctx.bar().close - 5.0;
        let with_stop = ctx.submit_with_stop(entry, stop).map_err(|e| e.reason);
        seen.lock()
            .unwrap()
            .push((n, ctx.is_warming_up(), plain, with_stop));
    });
    let mut s = Session::new(strategy, perp(), Timeframe::M1, config(0.0)).unwrap();
    // The funding passed with a warm-up bar is checked but not charged, and the portfolio is not
    // even marked: it is exactly as it started.
    s.warm_up(bar(0, 100.0, 101.0, 99.0, 100.0), &[(0, 0.01)])
        .unwrap();
    s.warm_up(bar(1, 100.0, 101.0, 99.0, 100.0), &[]).unwrap();
    assert_eq!(
        s.portfolio(),
        &Portfolio::new(InstrumentKind::Perpetual, 10_000.0)
    );
    assert!(s.fills().is_empty() && s.open_orders().is_empty() && s.equity().is_empty());
    // Each rejected order is recorded with its reason, and an entry's stop is not.
    let o = s.orders();
    assert_eq!(o.len(), 4);
    for (order, at) in o.iter().zip([0, 0, MIN, MIN]) {
        assert_eq!(order.status, OrderStatus::Rejected(WARM_UP.into()));
        assert_eq!((order.submitted_at, order.attached_stop), (at, None));
    }
    // Live, both go through: order 5, then order 6 with its stop as order 7.
    s.on_bar(bar(2, 100.0, 101.0, 99.0, 100.0), &[]).unwrap();
    let log = log.lock().unwrap();
    let reason = || WARM_UP.to_string();
    assert_eq!(log[0], (0, true, Err(reason()), Err(reason())));
    assert_eq!(log[1], (1, true, Err(reason()), Err(reason())));
    assert_eq!(
        log[2],
        (2, false, Ok(OrderId(5)), Ok((OrderId(6), OrderId(7))))
    );
}

#[test]
fn warm_up_bars_join_the_history_and_must_all_come_before_the_first_live_bar() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let strategy = Box::new(Recorder(seen.clone()));
    let mut s = Session::new(strategy, spot(), Timeframe::M1, config(0.0)).unwrap();
    let flat = |n| bar(n, 100.0, 101.0, 99.0, 100.0);
    // Warm-up bars are checked like live ones, and later bars are checked against them.
    s.warm_up(flat(0), &[]).unwrap();
    assert_eq!(
        s.warm_up(flat(0), &[]),
        Err(EngineError::DuplicateBar { time: 0 })
    );
    assert_eq!(
        s.warm_up(flat(1), &[(MIN, 0.01)]),
        Err(EngineError::FundingOnSpot)
    );
    s.warm_up(flat(1), &[]).unwrap();
    assert_eq!(
        s.on_bar(flat(1), &[]),
        Err(EngineError::DuplicateBar { time: MIN })
    );
    s.on_bar(flat(2), &[]).unwrap();
    s.on_bar(flat(3), &[]).unwrap();
    // Once a bar has been fed live, warming up is an error and changes nothing.
    assert_eq!(s.warm_up(flat(4), &[]), Err(EngineError::WarmUpAfterLive));
    assert_eq!(s.bars().len(), 4);
    // on_start runs once, on the first warm-up bar, and on_bar on every bar, warm-up or live,
    // with the history ending at that bar.
    assert_eq!(
        *seen.lock().unwrap(),
        vec![
            (1, 0, -1),
            (1, 0, 0),
            (2, MIN, MIN),
            (3, 2 * MIN, 2 * MIN),
            (4, 3 * MIN, 3 * MIN)
        ]
    );
}

#[test]
fn donchian_sends_nothing_while_warming_up_and_enters_on_the_first_live_bar() {
    let p = Params::new()
        .with("entry", td_engine::ParamValue::Int(5))
        .with("exit", td_engine::ParamValue::Int(3))
        .with("atr_period", td_engine::ParamValue::Int(3));
    let strategy = build_strategy("donchian_breakout", &p).unwrap();
    let mut s = Session::new(strategy, perp(), Timeframe::M1, config(0.0)).unwrap();
    let bars = up_then_down();
    // Bar 5's close of 110 breaks the 5-bar high of 109, but bar 5 is a warm-up bar, so the
    // strategy sends nothing, not even an order to be rejected.
    for b in &bars[..6] {
        s.warm_up(*b, &[]).unwrap();
    }
    assert!(s.orders().is_empty(), "{:?}", s.orders());
    // Live bar 6's close of 112 breaks the 5-bar high of 111, from bars 1 to 5. ATR is 3, so the
    // entry goes out on bar 6 with its stop 2 * 3 = 6 under the close, at 106.
    s.on_bar(bars[6], &[]).unwrap();
    let entry = &s.orders()[0];
    assert_eq!(entry.submitted_at, 6 * MIN);
    assert_eq!(
        entry.attached_stop,
        Some(AttachedStop {
            id: OrderId(2),
            trigger: 106.0
        })
    );
}
