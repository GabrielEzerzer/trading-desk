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
    run_backtest(strategy, inst, Timeframe::M1, bars, &[], &[], cfg).unwrap()
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
    // Taker fee on 109,250 of notional is 218.5. The equity left after it, 9,800 - 1,000 * 9.25 -
    // 218.5 = 331.5, goes as the liquidation fee: 550 in all.
    approx(f[1].fee, 550.0);
    assert_eq!(f[1].liquidity, td_core::Liquidity::Taker);
    let order = &s.orders()[f[1].order_id.0 as usize - 1];
    assert_eq!(order.request.client_tag.as_deref(), Some("liquidation"));
    assert!(order.request.reduce_only);
    assert_eq!(order.status, OrderStatus::Filled);

    // Flat at zero from here on.
    assert_eq!(s.portfolio().position(), 0.0);
    let e = equity(&s);
    approx(e[0], 10_000.0);
    approx(e[1], 0.0);
    approx(e[2], 0.0);
    approx(s.portfolio().shortfall_written_off(), 0.0);

    // The liquidation closes the trade in the trade list: -9,250 gross, 750 of fees.
    let trades = build_trades(
        s.fills(),
        s.bars(),
        &[],
        s.bar_funding(),
        td_core::AccountCurrency::Quote,
        None,
    );
    assert_eq!(trades.len(), 1);
    assert_eq!(trades[0].side, TradeSide::Short);
    assert!(!trades[0].open);
    approx(trades[0].exit_price, 109.25);
    approx(trades[0].net_pnl, -10_000.0);
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
    // Taker fee 90,500 * 0.2% = 181, and the 9,800 - 9,500 - 181 = 119 left is the liquidation
    // fee.
    approx(f[1].fee, 300.0);
    assert_eq!(s.portfolio().position(), 0.0);
    let e = equity(&s);
    approx(e[1], 9_800.0);
    approx(e[2], 0.0);
}

#[test]
fn a_liquidation_inside_the_bar_comes_before_a_stop_beyond_its_price() {
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

    // The short enters at bar 1's open and its stop at 115 is placed. The bar's high of 130 passes
    // both the liquidation price of 109.25 and the stop, and the liquidation is reached first: the
    // stop is cancelled and nothing is written off.
    let f = s.fills();
    assert_eq!(f.len(), 2);
    approx(f[1].price, 109.25);
    match &s.orders()[1].status {
        OrderStatus::Cancelled(reason) => assert!(reason.contains("liquidation"), "{reason}"),
        other => panic!("expected the stop cancelled, got {other:?}"),
    }
    assert_eq!(s.portfolio().position(), 0.0);
    approx(equity(&s)[1], 0.0);
    approx(s.portfolio().shortfall_written_off(), 0.0);
}

#[test]
fn a_gap_far_past_the_liquidation_price_cannot_take_equity_below_zero() {
    let i = perp();
    let bars = [
        bar(0, 100.0, 101.0, 99.0, 100.0),
        bar(1, 100.0, 101.0, 99.0, 100.0),
        bar(2, 120.0, 121.0, 119.0, 120.0),
    ];
    let (script, _) = Script::new(vec![(0, req(&i, Side::Sell, 1000.0, OrderType::Market))]);
    let s = run(i, lev(10.0), &bars, script);

    // Bar 2 opens at 120, past the short's liquidation price of 109.25, so it is liquidated at the
    // open: 9,800 - 20,000 - 240 of taker fee = -10,440, with nothing left for a liquidation fee.
    // The account is flat, so the balance is floored at zero and the 10,440 is written off, as a
    // venue's insurance fund would take it.
    let f = s.fills();
    assert_eq!(f.len(), 2);
    approx(f[1].price, 120.0);
    approx(f[1].fee, 240.0);
    assert_eq!(s.portfolio().position(), 0.0);
    approx(s.portfolio().cash(), 0.0);
    approx(equity(&s)[2], 0.0);
    approx(s.portfolio().shortfall_written_off(), 10_440.0);
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
        // Nor is flipping to a long: if the open buys close the short first, 100 + 400 + 2,000
        // less the 1,000 short opens a long of 1,500, 157,500 of notional against 48,000 allowed.
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

#[test]
fn a_short_breaching_in_a_bar_is_liquidated_before_a_limit_that_would_close_it() {
    let i = perp();
    let bars = [
        bar(0, 100.0, 101.0, 99.0, 100.0),
        bar(1, 100.0, 101.0, 99.0, 100.0),
        bar(2, 100.0, 130.0, 94.0, 96.0),
    ];
    let (script, log) = Script::new(vec![
        (0, req(&i, Side::Sell, 1000.0, OrderType::Market)),
        // A take-profit that bar 2 reaches inside its range, at 95.
        (
            1,
            reduce_only(req(&i, Side::Buy, 1000.0, OrderType::Limit { price: 95.0 })),
        ),
    ]);
    let s = run(i, lev(10.0), &bars, script);
    assert!(log.lock().unwrap().iter().all(|r| r.is_ok()));

    // The path inside bar 2 is unknown, so the worse order counts: the high of 130 first, which
    // passes the short's liquidation price of 109.25 (cash 9,800, as in the first test), then the
    // low of 94. The liquidation comes first and cancels the take-profit, which would otherwise
    // fill at 95 and leave 9,800 + 5,000 - 95 = 14,705.
    let f = s.fills();
    assert_eq!(f.len(), 2, "the entry and the liquidation");
    approx(f[1].price, 109.25);
    assert_eq!(f[1].time, 2 * MIN);
    // Taker fee 218.5, and the 331.5 left goes as the liquidation fee.
    approx(f[1].fee, 550.0);
    match &s.orders()[1].status {
        OrderStatus::Cancelled(reason) => assert!(reason.contains("liquidation"), "{reason}"),
        other => panic!("expected the take-profit cancelled, got {other:?}"),
    }
    assert_eq!(s.portfolio().position(), 0.0);
    approx(equity(&s)[2], 0.0);
}

#[test]
fn a_reduce_only_close_cannot_clear_the_way_for_a_reversal_past_the_cap() {
    let i = perp();
    let bars = [
        bar(0, 100.0, 101.0, 99.0, 100.0),
        bar(1, 100.0, 101.0, 99.0, 100.0),
        bar(2, 100.0, 101.0, 99.0, 100.0),
    ];
    let (script, log) = Script::new(vec![
        // Long 99 at 100, fee 19.8: equity 9,980.2 at bar 1's close, and 9,980.2 allowed at 1x.
        (0, req(&i, Side::Buy, 99.0, OrderType::Market)),
        (1, reduce_only(req(&i, Side::Sell, 99.0, OrderType::Market))),
        // The reduce-only sell closes the long first, so this opens a fresh short of 198:
        // 19,800, over the cap.
        (1, req(&i, Side::Sell, 198.0, OrderType::Market)),
        // A fresh short of 99 is 9,900: within.
        (1, req(&i, Side::Sell, 99.0, OrderType::Market)),
    ]);
    let s = run(i, lev(1.0), &bars, script);
    let log = log.lock().unwrap();
    assert!(log[0].is_ok() && log[1].is_ok());
    assert!(log[2].as_ref().unwrap_err().reason.contains("leverage"));
    assert!(log[3].is_ok());
    approx(s.portfolio().position(), -99.0);
}

#[test]
fn a_flip_is_capped_on_the_fresh_position_it_opens() {
    let i = perp();
    let bars = [
        bar(0, 100.0, 101.0, 99.0, 100.0),
        bar(1, 100.0, 106.0, 99.0, 105.0),
        bar(2, 105.0, 106.0, 104.0, 105.0),
    ];
    let (script, log) = Script::new(vec![
        (0, req(&i, Side::Sell, 1000.0, OrderType::Market)),
        // At bar 1's close the short of 1,000 has 4,800 of equity, 48,000 allowed at 10x. Buying
        // 2,000 closes it and opens a long of 1,000: 105,000, over.
        (1, req(&i, Side::Buy, 2000.0, OrderType::Market)),
        // Buying 1,400 opens a long of 400: 42,000, within.
        (1, req(&i, Side::Buy, 1400.0, OrderType::Market)),
    ]);
    let s = run(i, lev(10.0), &bars, script);
    let log = log.lock().unwrap();
    assert!(log[0].is_ok());
    assert!(log[1].as_ref().unwrap_err().reason.contains("leverage"));
    assert!(log[2].is_ok());
    // The buy fills at 105: -5,000 realised on the short and 147,000 * 0.2% = 294 of fee, so cash
    // is 9,800 - 5,000 - 294 = 4,506, long 400 at 105.
    approx(s.portfolio().position(), 400.0);
    approx(s.portfolio().cash(), 4_506.0);
}

#[test]
fn a_flip_values_its_fresh_position_at_the_dearest_of_the_orders() {
    let i = perp();
    let bars = [
        bar(0, 100.0, 101.0, 99.0, 100.0),
        bar(1, 100.0, 101.0, 99.0, 100.0),
    ];
    let (script, log) = Script::new(vec![
        // Short 50 at 100, fee 10: equity 9,990 at bar 1's close, and 9,990 allowed at 1x.
        (0, req(&i, Side::Sell, 50.0, OrderType::Market)),
        // Only closes the short: no fresh position.
        (
            1,
            req(&i, Side::Buy, 50.0, OrderType::Limit { price: 50.0 }),
        ),
        // If the limit closes the short first, this stop opens a long of 50 at 300: 15,000, over.
        // Valued at the average of the two orders, 175, it would be 8,750 and pass.
        (
            1,
            req(&i, Side::Buy, 50.0, OrderType::Stop { trigger: 300.0 }),
        ),
        // A long of 50 at 199 is 9,950: within.
        (
            1,
            req(&i, Side::Buy, 50.0, OrderType::Stop { trigger: 199.0 }),
        ),
    ]);
    run(i, lev(1.0), &bars, script);
    let log = log.lock().unwrap();
    assert!(log[0].is_ok() && log[1].is_ok());
    assert!(log[2].as_ref().unwrap_err().reason.contains("leverage"));
    assert!(log[3].is_ok());
}

#[test]
fn funding_owed_later_in_the_bar_brings_the_liquidation_price_closer() {
    let i = perp();
    let bars = [
        bar(0, 100.0, 101.0, 99.0, 100.0),
        bar(1, 100.0, 101.0, 99.0, 100.0),
        bar(2, 95.0, 96.0, 90.7, 95.0),
    ];
    // 0.1% funding half way through bar 2: the long of 1,000 owes 1,000 * 95 * 0.001 = 95.
    let funding = [(2 * MIN + 30_000, 0.001)];
    let (script, _) = Script::new(vec![(0, req(&i, Side::Buy, 1000.0, OrderType::Market))]);
    let s = run_backtest(script, i, Timeframe::M1, &bars, &[], &funding, lev(10.0)).unwrap();

    // Without the funding the long is liquidated at 90.66 (second test), above bar 2's low of 90.7.
    // With it, cash 9,800 - 95 = 9,705 meets maintenance at (100,000 - 9,705) / 995 = 90.7487...,
    // rounded up to 90.75, which the low reaches.
    let f = s.fills();
    assert_eq!(f.len(), 2, "the entry and the liquidation");
    approx(f[1].price, 90.75);
    // Taker fee 181.5; 9,800 - 9,250 - 181.5 = 368.5 left goes as the liquidation fee. The account
    // is flat when the funding falls due, so it pays none.
    approx(f[1].fee, 550.0);
    approx(s.bar_funding()[2].in_bar, 0.0);
    approx(equity(&s)[2], 0.0);
}

#[test]
fn a_long_is_liquidated_when_the_low_touches_its_trigger_and_not_a_tick_above() {
    let i = perp();
    let open_long = || {
        Script::new(vec![(
            0,
            req(&perp(), Side::Buy, 1000.0, OrderType::Market),
        )])
    };
    let first = [
        bar(0, 100.0, 101.0, 99.0, 100.0),
        bar(1, 100.0, 101.0, 99.0, 100.0),
    ];

    // The trigger is 90.66 (second test). A low of 90.67 leaves the long open: 9,800 - 5,000.
    let (script, _) = open_long();
    let bars = [first[0], first[1], bar(2, 95.0, 96.0, 90.67, 95.0)];
    let s = run(i.clone(), lev(10.0), &bars, script);
    assert_eq!(s.fills().len(), 1);
    approx(s.portfolio().position(), 1000.0);
    approx(equity(&s)[2], 4_800.0);

    // A low of exactly 90.66 liquidates there: taker fee 90,660 * 0.2% = 181.32, and the
    // 9,800 - 9,340 - 181.32 = 278.68 left goes as the liquidation fee.
    let (script, _) = open_long();
    let bars = [first[0], first[1], bar(2, 95.0, 96.0, 90.66, 95.0)];
    let s = run(i, lev(10.0), &bars, script);
    let f = s.fills();
    assert_eq!(f.len(), 2);
    approx(f[1].price, 90.66);
    approx(f[1].fee, 460.0);
    // The account ends at exactly zero, with no float residue written off.
    assert_eq!(s.portfolio().cash(), 0.0);
    assert_eq!(equity(&s)[2], 0.0);
    assert_eq!(s.portfolio().shortfall_written_off(), 0.0);
}

#[test]
fn a_resting_stop_above_the_liquidation_price_fills_before_the_liquidation() {
    let i = perp();
    let bars = [
        bar(0, 100.0, 101.0, 99.0, 100.0),
        bar(1, 100.0, 101.0, 99.0, 100.0),
        bar(2, 100.0, 101.0, 89.0, 90.0),
        bar(3, 90.0, 91.0, 89.0, 90.0),
    ];
    let (script, log) = Script::new(vec![
        (0, req(&i, Side::Buy, 1000.0, OrderType::Market)),
        (
            1,
            reduce_only(req(
                &i,
                Side::Sell,
                1000.0,
                OrderType::Stop { trigger: 95.0 },
            )),
        ),
    ]);
    let s = run(i, lev(10.0), &bars, script);
    assert!(log.lock().unwrap().iter().all(|r| r.is_ok()));

    // Long 1,000 at 100, cash 9,800, liquidation price 90.66 (second test). Bar 2 falls from 100
    // to 89: the price passes the stop at 95 before 90.66, so the stop fills there and the long
    // is closed before it can be liquidated. -5,000 realised and 95,000 * 0.2% = 190 of fee.
    let f = s.fills();
    assert_eq!(f.len(), 2, "the entry and the stop, no liquidation");
    approx(f[1].price, 95.0);
    approx(f[1].fee, 190.0);
    assert_eq!(s.orders()[1].status, OrderStatus::Filled);
    assert!(
        s.orders()
            .iter()
            .all(|o| o.request.client_tag.as_deref() != Some("liquidation"))
    );
    assert_eq!(s.portfolio().position(), 0.0);
    let e = equity(&s);
    approx(e[2], 4_610.0);
    approx(e[3], 4_610.0);
    approx(s.portfolio().shortfall_written_off(), 0.0);
}

#[test]
fn an_attached_stop_above_the_liquidation_price_fills_before_the_liquidation() {
    let i = perp();
    let bars = [
        bar(0, 100.0, 101.0, 99.0, 100.0),
        bar(1, 100.0, 101.0, 89.0, 90.0),
        bar(2, 90.0, 91.0, 89.0, 90.0),
    ];
    let steps = Steps(|ctx: &mut td_engine::Context, index: usize| {
        if index == 0 {
            let r = req(ctx.instrument(), Side::Buy, 1000.0, OrderType::Market);
            ctx.submit_with_stop(r, 95.0).unwrap();
        }
    });
    let s = run(i, lev(10.0), &bars, Box::new(steps));

    // The long enters at bar 1's open of 100 and its stop at 95 is placed. The bar falls to 89,
    // past the liquidation price of 90.66, but it passes the stop first: the stop fills at 95 and
    // equity is 9,800 - 5,000 - 190 = 4,610.
    let f = s.fills();
    assert_eq!(f.len(), 2, "the entry and the stop, no liquidation");
    approx(f[1].price, 95.0);
    approx(f[1].fee, 190.0);
    assert_eq!(s.orders()[1].status, OrderStatus::Filled);
    assert_eq!(s.portfolio().position(), 0.0);
    let e = equity(&s);
    approx(e[1], 4_610.0);
    approx(e[2], 4_610.0);
    approx(s.portfolio().shortfall_written_off(), 0.0);
}

#[test]
fn a_partly_closed_long_is_liquidated_at_the_price_of_what_is_left() {
    let i = perp();
    let bars = [
        bar(0, 100.0, 101.0, 99.0, 100.0),
        bar(1, 100.0, 101.0, 99.0, 100.0),
        bar(2, 98.0, 99.0, 97.0, 98.0),
        bar(3, 90.0, 91.0, 85.0, 86.0),
    ];
    let (script, log) = Script::new(vec![
        (0, req(&i, Side::Buy, 1000.0, OrderType::Market)),
        (
            1,
            reduce_only(req(&i, Side::Sell, 400.0, OrderType::Market)),
        ),
    ]);
    let s = run(i, lev(10.0), &bars, script);
    assert!(log.lock().unwrap().iter().all(|r| r.is_ok()));

    // Long 1,000 at 100, cash 9,800. Selling 400 at bar 2's open of 98 realises -800 with a fee of
    // 39,200 * 0.2% = 78.4: cash 8,921.6, long 600 at 100. Equity 8,921.6 + 600 * (p - 100) meets
    // maintenance 0.005 * 600 * p at p = (60,000 - 8,921.6) / (600 * 0.995) = 85.5584...,
    // rounded up to 85.56. Bar 2's low of 97 is above it; bar 3 opens at 90 and reaches 85.
    let f = s.fills();
    assert_eq!(f.len(), 3);
    approx(f[1].price, 98.0);
    assert_eq!(f[2].side, Side::Sell);
    approx(f[2].qty, 600.0);
    approx(f[2].price, 85.56);
    assert_eq!(f[2].time, 3 * MIN);
    // Equity at 85.56 is 8,921.6 - 8,664 = 257.6: the taker fee of 51,336 * 0.2% = 102.672 and a
    // liquidation fee of the 154.928 left.
    approx(f[2].fee, 257.6);
    approx(s.portfolio().fees_paid(), 200.0 + 78.4 + 257.6);
    assert_eq!(s.portfolio().position(), 0.0);
    approx(equity(&s)[3], 0.0);
}

#[test]
fn stops_above_a_longs_liquidation_price_fill_nearest_the_open_first() {
    let i = perp();
    let bars = [
        bar(0, 100.0, 101.0, 99.0, 100.0),
        bar(1, 100.0, 101.0, 99.0, 100.0),
        bar(2, 100.0, 101.0, 89.0, 90.0),
    ];
    let stop = |trigger: f64| reduce_only(req(&i, Side::Sell, 500.0, OrderType::Stop { trigger }));
    // The stop at 95 is sent first, so order ids alone would fill it first.
    let (script, log) = Script::new(vec![
        (0, req(&i, Side::Buy, 1000.0, OrderType::Market)),
        (1, stop(95.0)),
        (1, stop(97.0)),
    ]);
    let s = run(i.clone(), lev(10.0), &bars, script);
    assert!(log.lock().unwrap().iter().all(|r| r.is_ok()));

    // Long 1,000 at 100, cash 9,800, liquidation price 90.66 (second test), which bar 2's low of
    // 89 passes. Falling from 100 the market reaches 97 first: 500 sell there, -1,500 realised
    // and 48,500 * 0.2% = 97 of fee, cash 8,203. The long of 500 left meets maintenance at
    // (50,000 - 8,203) / (500 * 0.995) = 84.0141..., 84.02, below the low, so it is safe and the
    // stop at 95 fills in the range: -2,500 and 95 of fee, cash 5,608.
    let f = s.fills();
    assert_eq!(f.len(), 3, "the entry and both stops, no liquidation");
    approx(f[1].price, 97.0);
    approx(f[1].qty, 500.0);
    approx(f[1].fee, 97.0);
    approx(f[2].price, 95.0);
    approx(f[2].qty, 500.0);
    approx(f[2].fee, 95.0);
    assert!(
        s.orders()
            .iter()
            .all(|o| o.request.client_tag.as_deref() != Some("liquidation"))
    );
    assert_eq!(s.portfolio().position(), 0.0);
    approx(equity(&s)[2], 5_608.0);
    approx(s.portfolio().shortfall_written_off(), 0.0);
}

#[test]
fn a_shorts_stop_below_its_liquidation_price_fills_before_the_liquidation() {
    let i = perp();
    let bars = [
        bar(0, 100.0, 101.0, 99.0, 100.0),
        bar(1, 100.0, 101.0, 99.0, 100.0),
        bar(2, 100.0, 130.0, 99.0, 110.0),
    ];
    let (script, log) = Script::new(vec![
        (0, req(&i, Side::Sell, 1000.0, OrderType::Market)),
        (
            1,
            reduce_only(req(
                &i,
                Side::Buy,
                1000.0,
                OrderType::Stop { trigger: 105.0 },
            )),
        ),
    ]);
    let s = run(i, lev(10.0), &bars, script);
    assert!(log.lock().unwrap().iter().all(|r| r.is_ok()));

    // Short 1,000 at 100, cash 9,800, liquidation price 109.25 (first test), which bar 2's high
    // of 130 passes. Rising from 100 the market reaches the stop at 105 first: -5,000 realised
    // and 105,000 * 0.2% = 210 of fee, so 4,590 is left and the short is closed.
    let f = s.fills();
    assert_eq!(f.len(), 2, "the entry and the stop, no liquidation");
    assert_eq!(f[1].side, Side::Buy);
    approx(f[1].price, 105.0);
    approx(f[1].fee, 210.0);
    assert_eq!(s.orders()[1].status, OrderStatus::Filled);
    assert_eq!(s.portfolio().position(), 0.0);
    approx(equity(&s)[2], 4_590.0);
    approx(s.portfolio().shortfall_written_off(), 0.0);
}

#[test]
fn a_gap_past_the_liquidation_price_liquidates_before_a_limit_beyond_it() {
    let i = perp();
    let bars = [
        bar(0, 100.0, 101.0, 99.0, 100.0),
        bar(1, 100.0, 101.0, 99.0, 100.0),
        bar(2, 89.0, 89.5, 88.0, 89.0),
    ];
    // At 20x the buy limit fits beside the long: 100,000 + 45,000 of 200,000 allowed.
    let (script, log) = Script::new(vec![
        (0, req(&i, Side::Buy, 1000.0, OrderType::Market)),
        (
            0,
            req(&i, Side::Buy, 500.0, OrderType::Limit { price: 90.0 }),
        ),
    ]);
    let s = run(i, lev(20.0), &bars, script);
    assert!(log.lock().unwrap().iter().all(|r| r.is_ok()));

    // Long 1,000 at 100, cash 9,800, liquidation price 90.66 (second test). Bar 2 opens at 89, so
    // between the bars the market fell from 100 through 90.66 and then 90, where the limit rests.
    // It reached the liquidation price first, so the long is liquidated, at the open as for any
    // gap past that price, and the limit never fills: 1,000 sold at 89, a taker fee of 178 and
    // nothing left for a liquidation fee. 9,800 - 11,000 - 178 = -1,378 is written off.
    let f = s.fills();
    assert_eq!(f.len(), 2, "the entry and the liquidation");
    assert_eq!(f[1].side, Side::Sell);
    approx(f[1].qty, 1000.0);
    approx(f[1].price, 89.0);
    approx(f[1].fee, 178.0);
    let order = &s.orders()[f[1].order_id.0 as usize - 1];
    assert_eq!(order.request.client_tag.as_deref(), Some("liquidation"));
    match &s.orders()[1].status {
        OrderStatus::Cancelled(reason) => assert!(reason.contains("liquidation"), "{reason}"),
        other => panic!("expected the limit cancelled, got {other:?}"),
    }
    assert_eq!(s.portfolio().position(), 0.0);
    approx(equity(&s)[2], 0.0);
    approx(s.portfolio().shortfall_written_off(), 1_378.0);
}

#[test]
fn a_gap_past_the_liquidation_price_liquidates_before_the_fills_at_the_open() {
    let i = perp();
    let bars = [
        bar(0, 100.0, 101.0, 99.0, 100.0),
        bar(1, 100.0, 101.0, 99.0, 100.0),
        bar(2, 89.0, 89.5, 88.0, 89.0),
    ];
    let (script, log) = Script::new(vec![
        (0, req(&i, Side::Buy, 1000.0, OrderType::Market)),
        (
            1,
            reduce_only(req(&i, Side::Sell, 400.0, OrderType::Market)),
        ),
    ]);
    let s = run(i, lev(10.0), &bars, script);
    assert!(log.lock().unwrap().iter().all(|r| r.is_ok()));

    // The market sell waits for bar 2's open of 89, but the market passed the liquidation price
    // of 90.66 on its way there: the whole long is liquidated at the open and the sell is
    // cancelled with the other open orders. Same numbers as the test above.
    let f = s.fills();
    assert_eq!(f.len(), 2, "the entry and the liquidation");
    approx(f[1].qty, 1000.0);
    approx(f[1].price, 89.0);
    match &s.orders()[1].status {
        OrderStatus::Cancelled(reason) => assert!(reason.contains("liquidation"), "{reason}"),
        other => panic!("expected the sell cancelled, got {other:?}"),
    }
    approx(s.portfolio().shortfall_written_off(), 1_378.0);
}

#[test]
fn a_stop_above_the_liquidation_price_that_the_bar_gaps_through_fills_first() {
    let i = perp();
    let bars = [
        bar(0, 100.0, 101.0, 99.0, 100.0),
        bar(1, 100.0, 101.0, 99.0, 100.0),
        bar(2, 89.0, 89.5, 88.0, 89.0),
    ];
    let (script, log) = Script::new(vec![
        (0, req(&i, Side::Buy, 1000.0, OrderType::Market)),
        (
            1,
            reduce_only(req(
                &i,
                Side::Sell,
                1000.0,
                OrderType::Stop { trigger: 95.0 },
            )),
        ),
    ]);
    let s = run(i, lev(10.0), &bars, script);
    assert!(log.lock().unwrap().iter().all(|r| r.is_ok()));

    // Falling from 100 to the open of 89 the market triggered the stop at 95 before it reached the
    // liquidation price of 90.66, so the stop fills first, at the open as any stop the bar opens
    // through, and closes the long: no liquidation. The loss is the same, 9,800 - 11,000 - 178 of
    // taker fee, and the 1,378 is written off.
    let f = s.fills();
    assert_eq!(f.len(), 2, "the entry and the stop, no liquidation");
    assert_eq!(f[1].order_id, s.orders()[1].id);
    approx(f[1].price, 89.0);
    assert_eq!(s.orders()[1].status, OrderStatus::Filled);
    assert!(
        s.orders()
            .iter()
            .all(|o| o.request.client_tag.as_deref() != Some("liquidation"))
    );
    approx(s.portfolio().shortfall_written_off(), 1_378.0);
}

/// Calls the closure with the context and the bar's index at each bar's close.
struct Steps<F>(F);

impl<F: FnMut(&mut td_engine::Context, usize) + Send> Strategy for Steps<F> {
    fn on_bar(&mut self, ctx: &mut td_engine::Context, _bar: &td_core::Candle) {
        let index = ctx.bars().len() - 1;
        (self.0)(ctx, index);
    }
}
