//! Leverage cap, liquidation and the spot cash check at fill time. Every number is worked out by
//! hand in the comments. The common instrument has tick 0.01, lot 0.001, maker 0.1% and taker
//! 0.2%; slippage is 0 throughout, and the maintenance margin is 0.5% of notional.

mod common;

use common::*;
use td_core::{OrderType, Side, Timeframe};
use td_engine::{
    OrderStatus, Session, SessionConfig, Strategy, TradeSide, build_trades, run_backtest,
};

/// 10,000 starting cash, no slippage, the given leverage cap.
fn lev(max_leverage: f64) -> SessionConfig {
    SessionConfig {
        max_leverage,
        ..config(0.0)
    }
}

fn run(
    inst: td_core::Instrument,
    cfg: SessionConfig,
    bars: &[td_core::Candle],
    strategy: Box<dyn Strategy>,
) -> Session {
    run_backtest(strategy, inst, Timeframe::M1, bars, &[], cfg).unwrap()
}

fn equity(s: &Session) -> Vec<f64> {
    s.equity().iter().map(|p| p.equity).collect()
}

#[test]
fn a_10x_short_through_a_30_percent_wick_is_liquidated_at_the_maintenance_price() {
    let i = perp();
    let bars = [
        bar(0, 100.0, 101.0, 99.0, 100.0),
        bar(1, 100.0, 130.0, 98.0, 99.0),
        bar(2, 99.0, 100.0, 98.0, 99.0),
    ];
    // 1,000 short at 100 is 100,000 of notional on 10,000 of equity: exactly 10x.
    let (script, log) = Script::new(vec![(0, req(&i, Side::Sell, 1000.0, OrderType::Market))]);
    let s = run(i, lev(10.0), &bars, script);
    assert!(log.lock().unwrap()[0].is_ok());

    let f = s.fills();
    assert_eq!(f.len(), 2, "the entry and the liquidation");
    // Entry at bar 1's open: fee 100,000 * 0.2% = 200, so cash is 9,800.
    approx(f[0].price, 100.0);
    approx(f[0].fee, 200.0);
    // Equity at price p is 9,800 - 1,000 * (p - 100); maintenance is 0.005 * 1,000 * p. They meet
    // at p = (9,800 + 100,000) / (1,000 * 1.005) = 109.2537..., rounded down to the tick (so it
    // triggers no later): 109.25. The high of 130 reaches it; the open of 100 is below it.
    assert_eq!(f[1].side, Side::Buy);
    approx(f[1].qty, 1000.0);
    approx(f[1].price, 109.25);
    assert_eq!(f[1].time, MIN);
    // Taker fee on 109,250 of notional.
    approx(f[1].fee, 218.5);
    assert_eq!(f[1].liquidity, td_core::Liquidity::Taker);
    let order = &s.orders()[f[1].order_id.0 as usize - 1];
    assert_eq!(order.request.client_tag.as_deref(), Some("liquidation"));
    assert!(order.request.reduce_only);
    assert_eq!(order.status, OrderStatus::Filled);

    // 9,800 - 1,000 * 9.25 - 218.5 = 331.5, flat from here on.
    assert_eq!(s.portfolio().position(), 0.0);
    let e = equity(&s);
    approx(e[0], 10_000.0);
    approx(e[1], 331.5);
    approx(e[2], 331.5);

    // The liquidation closes the trade in the trade list: -9,250 gross, 418.5 of fees.
    let trades = build_trades(s.fills(), s.bars(), s.bar_funding());
    assert_eq!(trades.len(), 1);
    assert_eq!(trades[0].side, TradeSide::Short);
    assert!(!trades[0].open);
    approx(trades[0].exit_price, 109.25);
    approx(trades[0].net_pnl, -9_668.5);
}

#[test]
fn a_long_gapping_through_its_liquidation_price_is_liquidated_at_the_open() {
    let i = perp();
    let bars = [
        bar(0, 100.0, 101.0, 99.0, 100.0),
        bar(1, 100.0, 101.0, 99.0, 100.0),
        bar(2, 90.5, 91.0, 90.0, 90.8),
    ];
    let (script, _) = Script::new(vec![(0, req(&i, Side::Buy, 1000.0, OrderType::Market))]);
    let s = run(i, lev(10.0), &bars, script);

    // Long 1,000 at 100, fee 200, cash 9,800. Equity 9,800 + 1,000 * (p - 100) meets maintenance
    // 0.005 * 1,000 * p at p = (100,000 - 9,800) / (1,000 * 0.995) = 90.6532..., rounded up to
    // 90.66. Bar 1's low of 99 is above it; bar 2 opens at 90.5, already below it.
    let f = s.fills();
    assert_eq!(f.len(), 2);
    assert_eq!(f[1].side, Side::Sell);
    approx(f[1].price, 90.5);
    assert_eq!(f[1].time, 2 * MIN);
    // Fee 90,500 * 0.2% = 181. Cash 9,800 - 9,500 - 181 = 119.
    approx(f[1].fee, 181.0);
    assert_eq!(s.portfolio().position(), 0.0);
    let e = equity(&s);
    approx(e[1], 9_800.0);
    approx(e[2], 119.0);
}

#[test]
fn a_stop_beyond_the_liquidation_price_cannot_take_equity_below_zero() {
    let i = perp();
    let bars = [
        bar(0, 100.0, 101.0, 99.0, 100.0),
        bar(1, 100.0, 130.0, 98.0, 99.0),
    ];
    let steps = Steps(|ctx: &mut td_engine::Context, index: usize| {
        if index == 0 {
            let r = req(ctx.instrument(), Side::Sell, 1000.0, OrderType::Market);
            ctx.submit_with_stop(r, 115.0).unwrap();
        }
    });
    let s = run(i, lev(10.0), &bars, Box::new(steps));

    // The stop fills inside the bar at 115: 9,800 - 15,000 - 230 = -5,430. The account is flat,
    // so the balance is floored at zero and the 5,430 is written off, as a venue's insurance fund
    // would take it. (A venue would have liquidated at 109.25 first and left 331.5, so this is
    // the pessimistic side.)
    let f = s.fills();
    assert_eq!(f.len(), 2);
    approx(f[1].price, 115.0);
    assert_eq!(s.portfolio().position(), 0.0);
    approx(s.portfolio().cash(), 0.0);
    approx(equity(&s)[1], 0.0);
    approx(s.portfolio().shortfall_written_off(), 5_430.0);
}

#[test]
fn an_order_that_would_take_the_position_over_the_cap_is_rejected() {
    let i = perp();
    let bars = [
        bar(0, 100.0, 101.0, 99.0, 100.0),
        bar(1, 100.0, 101.0, 99.0, 100.0),
    ];
    // Default cap 1x on 10,000 of equity. Open orders on the same side count at their own price.
    let (script, log) = Script::new(vec![
        // 60 * 101 = 6,060: within the cap.
        (
            0,
            req(&i, Side::Sell, 60.0, OrderType::Limit { price: 101.0 }),
        ),
        // 6,060 + 50 * 100 = 11,060: over.
        (0, req(&i, Side::Sell, 50.0, OrderType::Market)),
        // 6,060 + 39 * 100 = 9,960: within.
        (0, req(&i, Side::Sell, 39.0, OrderType::Market)),
        // No buys are open, so 101 * 100 = 10,100 alone is over.
        (0, req(&i, Side::Buy, 101.0, OrderType::Market)),
    ]);
    let s = run(i, lev(1.0), &bars, script);
    let log = log.lock().unwrap();
    assert!(log[0].is_ok());
    assert!(log[1].as_ref().unwrap_err().reason.contains("leverage"));
    assert!(log[2].is_ok());
    assert!(log[3].as_ref().unwrap_err().reason.contains("leverage"));
    assert!(matches!(s.orders()[1].status, OrderStatus::Rejected(_)));
}

#[test]
fn orders_that_reduce_the_position_are_allowed_over_the_cap() {
    let i = perp();
    let bars = [
        bar(0, 100.0, 101.0, 99.0, 100.0),
        bar(1, 100.0, 106.0, 99.0, 105.0),
        bar(2, 105.0, 106.0, 104.0, 105.0),
    ];
    let (script, log) = Script::new(vec![
        (0, req(&i, Side::Sell, 1000.0, OrderType::Market)),
        // At bar 1's close: cash 9,800, equity 9,800 - 1,000 * 5 = 4,800 against 105,000 of
        // notional, 21.9x, over the 10x cap (the high of 106 is below the liquidation price of
        // 109.25). A buy that only shrinks the short is allowed, reduce-only or not.
        (1, req(&i, Side::Buy, 100.0, OrderType::Market)),
        (1, reduce_only(req(&i, Side::Buy, 400.0, OrderType::Market))),
        // Growing the short is not.
        (1, req(&i, Side::Sell, 1.0, OrderType::Market)),
        // Nor is flipping to a long larger than the short: with the open buy of 100 it ends at
        // +1,100, 115,500 of notional against 48,000 allowed.
        (1, req(&i, Side::Buy, 2000.0, OrderType::Market)),
    ]);
    let s = run(i, lev(10.0), &bars, script);
    let log = log.lock().unwrap();
    assert!(log[0].is_ok() && log[1].is_ok() && log[2].is_ok());
    assert!(log[3].as_ref().unwrap_err().reason.contains("leverage"));
    assert!(log[4].as_ref().unwrap_err().reason.contains("leverage"));
    approx(s.portfolio().position(), -500.0);
    // Realised 500 * (100 - 105) = -2,500, fees 52,500 * 0.2% = 105: cash 7,195, and the 500
    // still short marked at 105 lose another 2,500.
    approx(s.portfolio().cash(), 7_195.0);
    approx(equity(&s)[2], 4_695.0);
}

#[test]
fn a_spot_buy_after_a_gap_up_is_cut_to_the_cash_at_the_fill_price() {
    let i = spot();
    let bars = [
        bar(0, 100.0, 101.0, 99.0, 100.0),
        bar(1, 104.0, 105.0, 103.0, 104.0),
    ];
    let (script, log) = Script::new(vec![
        // 99.7 * 100 * 1.002 = 9,989.94 and 0.01 * 100 * 1.002 = 1.002: both fit in 10,000 at
        // the close they are sized on.
        (0, req(&i, Side::Buy, 99.7, OrderType::Market)),
        (0, req(&i, Side::Buy, 0.01, OrderType::Market)),
    ]);
    let s = run(i, config(0.0), &bars, script);
    assert!(log.lock().unwrap().iter().all(|r| r.is_ok()));

    // Bar 1 opens at 104. With the fee one unit costs 104.208, and 10,000 / 104.208 = 95.9619...,
    // rounded down to the lot: 95.961. Notional 9,979.944, fee 19.959888, cash left 0.096112.
    let f = s.fills();
    assert_eq!(f.len(), 1);
    approx(f[0].qty, 95.961);
    approx(f[0].price, 104.0);
    approx(f[0].fee, 19.959888);
    approx(s.portfolio().cash(), 0.096112);
    assert!(s.portfolio().cash() >= 0.0);
    approx(s.portfolio().position(), 95.961);
    // The cut order is filled for what the cash covered.
    assert_eq!(s.orders()[0].status, OrderStatus::Filled);
    // The second buy finds 0.096 of cash, less than one lot at 104: it is cancelled.
    match &s.orders()[1].status {
        OrderStatus::Cancelled(reason) => assert!(reason.contains("cash"), "{reason}"),
        other => panic!("expected the second buy cancelled, got {other:?}"),
    }
}

/// Calls the closure with the context and the bar's index at each bar's close.
struct Steps<F>(F);

impl<F: FnMut(&mut td_engine::Context, usize) + Send> Strategy for Steps<F> {
    fn on_bar(&mut self, ctx: &mut td_engine::Context, _bar: &td_core::Candle) {
        let index = ctx.bars().len() - 1;
        (self.0)(ctx, index);
    }
}
