//! A market with a bid and an ask (FX): each order sees the candle of its own side, open positions
//! are marked at the side that closes them, and bars in closed hours are refused. Every number is
//! worked by hand in the comments.

mod common;

use common::*;
use td_core::{
    Candle, FeeSchedule, Instrument, InstrumentId, InstrumentKind, Millis, OrderRequest, OrderType,
    Side, Timeframe, Venue,
};
use td_engine::{Context, EngineError, OrderStatus, Session, SessionConfig, Strategy};

/// USD/JPY: tick 0.001, lot 1, no fees.
fn usdjpy() -> Instrument {
    Instrument {
        id: InstrumentId {
            venue: Venue::Binance,
            symbol: "USDJPY".into(),
            kind: InstrumentKind::Fx,
        },
        base: "USD".into(),
        quote: "JPY".into(),
        tick_size: 0.001,
        lot_size: 1.0,
        min_qty: 1.0,
        min_notional: 1.0,
        fees: FeeSchedule {
            maker: 0.0,
            taker: 0.0,
            per_unit: 0.0,
        },
    }
}

/// A million yen, no slippage, up to 1x.
fn fx_config() -> SessionConfig {
    SessionConfig {
        starting_cash: 1_000_000.0,
        slippage_bps: 0.0,
        fees: None,
        max_leverage: 1.0,
        account_currency: td_core::AccountCurrency::Quote,
    }
}

/// A one-minute candle `i` minutes after the epoch (a Thursday, when FX is open).
fn candle(i: i64, [open, high, low, close]: [f64; 4]) -> Candle {
    Candle {
        open_time: i * MIN,
        open,
        high,
        low,
        close,
        volume: 1.0,
        quote_volume: close,
    }
}

/// What to send at a bar's close.
#[derive(Clone)]
enum Send {
    Plain(OrderRequest),
    WithStop(OrderRequest, f64),
}

/// Sends the planned orders at the close of the given bar index; every one must be accepted.
struct Plan(Vec<(usize, Send)>);

impl Strategy for Plan {
    fn on_bar(&mut self, ctx: &mut Context, _bar: &Candle) {
        let index = ctx.bars().len() - 1;
        for (at, send) in &self.0 {
            if *at == index {
                match send.clone() {
                    Send::Plain(r) => {
                        ctx.submit(r).unwrap();
                    }
                    Send::WithStop(r, trigger) => {
                        ctx.submit_with_stop(r, trigger).unwrap();
                    }
                }
            }
        }
    }
}

fn market(side: Side, qty: f64) -> OrderRequest {
    req(&usdjpy(), side, qty, OrderType::Market)
}

fn limit(side: Side, qty: f64, price: f64) -> OrderRequest {
    req(&usdjpy(), side, qty, OrderType::Limit { price })
}

fn stop(side: Side, qty: f64, trigger: f64) -> OrderRequest {
    req(&usdjpy(), side, qty, OrderType::Stop { trigger })
}

fn stop_limit(side: Side, qty: f64, trigger: f64, price: f64) -> OrderRequest {
    req(
        &usdjpy(),
        side,
        qty,
        OrderType::StopLimit { trigger, price },
    )
}

/// Runs `plan` on FX over `(bid, ask)` one-minute bars.
fn run_fx(plan: Vec<(usize, Send)>, bars: &[([f64; 4], [f64; 4])]) -> Session {
    run_fx_with(plan, bars, fx_config())
}

fn run_fx_with(
    plan: Vec<(usize, Send)>,
    bars: &[([f64; 4], [f64; 4])],
    config: SessionConfig,
) -> Session {
    let mut s = Session::new(Box::new(Plan(plan)), usdjpy(), Timeframe::M1, config).unwrap();
    for (i, (bid, ask)) in bars.iter().enumerate() {
        let i = i as i64;
        s.on_bar_with_ask(candle(i, *bid), Some(candle(i, *ask)), &[])
            .unwrap();
    }
    s
}

/// `(side, price, qty)` of each fill.
fn fills(s: &Session) -> Vec<(Side, f64, f64)> {
    s.fills().iter().map(|f| (f.side, f.price, f.qty)).collect()
}

/// A quiet decision bar: bid 150.000 all through, ask 150.020.
const DECIDE: ([f64; 4], [f64; 4]) = (
    [150.000, 150.000, 150.000, 150.000],
    [150.020, 150.020, 150.020, 150.020],
);

#[test]
fn a_market_sell_fills_at_the_bid_open_and_a_market_buy_at_the_ask_open() {
    // Both sent at bar 0's close. Bar 1 opens at bid 150.000, ask 150.020: the buy pays the ask,
    // the sell gets the bid, so the round trip costs the 0.020 spread on 1000 units, 20 yen.
    let s = run_fx(
        vec![
            (0, Send::Plain(market(Side::Buy, 1000.0))),
            (0, Send::Plain(market(Side::Sell, 1000.0))),
        ],
        &[
            DECIDE,
            (
                [150.000, 150.100, 149.900, 150.050],
                [150.020, 150.120, 149.920, 150.070],
            ),
        ],
    );
    assert_eq!(
        fills(&s),
        [(Side::Buy, 150.020, 1000.0), (Side::Sell, 150.000, 1000.0)]
    );
    // Flat again, 1000 * (150.000 - 150.020) = -20 yen.
    approx(s.portfolio().position(), 0.0);
    approx(s.portfolio().cash(), 1_000_000.0 - 20.0);
}

#[test]
fn a_shorts_stop_triggers_on_the_ask_high_and_fills_at_the_ask_open_or_the_trigger() {
    // Short 1000 at bar 0's close with a stop at 150.500. Bar 1: the entry sells at the bid open,
    // 150.000. The stop is a buy, so it watches the ask: its rest of the bar starts at 150.000 plus
    // the 0.020 spread at the open, and the ask high 150.510 reaches 150.500 while the bid high,
    // 150.480, stays below it. It fills at max(150.020, 150.500) = 150.500.
    let s = run_fx(
        vec![(0, Send::WithStop(market(Side::Sell, 1000.0), 150.500))],
        &[
            DECIDE,
            (
                [150.000, 150.480, 149.900, 150.100],
                [150.020, 150.510, 149.920, 150.120],
            ),
        ],
    );
    assert_eq!(
        fills(&s),
        [(Side::Sell, 150.000, 1000.0), (Side::Buy, 150.500, 1000.0)]
    );

    // The stop rests through bar 1 (ask high 150.300), and bar 2 opens with the ask at 150.600,
    // past the trigger, and the bid at 150.580: it fills at max(150.600, 150.500) = 150.600, the
    // ask open, not the bid open.
    let s = run_fx(
        vec![(0, Send::WithStop(market(Side::Sell, 1000.0), 150.500))],
        &[
            DECIDE,
            (
                [150.000, 150.280, 149.900, 150.100],
                [150.020, 150.300, 149.920, 150.120],
            ),
            (
                [150.580, 150.700, 150.500, 150.600],
                [150.600, 150.720, 150.520, 150.620],
            ),
        ],
    );
    assert_eq!(
        fills(&s),
        [(Side::Sell, 150.000, 1000.0), (Side::Buy, 150.600, 1000.0)]
    );
}

#[test]
fn a_shorts_stop_ignores_the_bid_high() {
    // Short 1000 at bar 0's close with a stop at 150.500; bars 1 and 2 have a 0.050 spread.
    // Bar 1: the entry sells at the bid open, 150.000, and the stop rests, since the ask high,
    // 150.150, is short of it. Bar 2: the ask high, 150.530, reaches 150.500 while the bid high,
    // 150.480, stays below it. A resting buy stop reads the ask, so it fills on bar 2 at
    // max(150.150, 150.500) = 150.500; read on the bid it would still be open.
    let s = run_fx(
        vec![(0, Send::WithStop(market(Side::Sell, 1000.0), 150.500))],
        &[
            DECIDE,
            (
                [150.000, 150.100, 149.900, 150.050],
                [150.050, 150.150, 149.950, 150.100],
            ),
            (
                [150.100, 150.480, 150.000, 150.200],
                [150.150, 150.530, 150.050, 150.250],
            ),
        ],
    );
    assert_eq!(
        fills(&s),
        [(Side::Sell, 150.000, 1000.0), (Side::Buy, 150.500, 1000.0)]
    );
    assert_eq!(s.fills()[1].time, 2 * MIN);
}

#[test]
fn a_longs_stop_triggers_on_the_bid_low_and_ignores_the_ask_low() {
    // Long 1000 at bar 0's close with a stop at 149.500. Bar 1: the entry buys at the ask open,
    // 150.020. The stop is a sell, so it watches the bid: its rest of the bar starts at 150.020
    // less the 0.020 spread, and the bid low 149.490 reaches 149.500 while the ask low, 149.510,
    // stays above it. It fills at min(150.000, 149.500) = 149.500.
    let s = run_fx(
        vec![(0, Send::WithStop(market(Side::Buy, 1000.0), 149.500))],
        &[
            DECIDE,
            (
                [150.000, 150.100, 149.490, 149.800],
                [150.020, 150.120, 149.510, 149.820],
            ),
        ],
    );
    assert_eq!(
        fills(&s),
        [(Side::Buy, 150.020, 1000.0), (Side::Sell, 149.500, 1000.0)]
    );

    // The same stop resting to a later bar, with a 0.050 spread. Bar 1: the entry buys at the ask
    // open, 150.050, and the stop rests, since the bid low, 149.900, is above it. Bar 2: the bid
    // low, 149.480, reaches 149.500 while the ask low, 149.530, stays above it. A resting sell
    // stop reads the bid, so it fills on bar 2 at min(149.900, 149.500) = 149.500; read on the ask
    // it would still be open.
    let s = run_fx(
        vec![(0, Send::WithStop(market(Side::Buy, 1000.0), 149.500))],
        &[
            DECIDE,
            (
                [150.000, 150.100, 149.900, 149.950],
                [150.050, 150.150, 149.950, 150.000],
            ),
            (
                [149.900, 150.000, 149.480, 149.700],
                [149.950, 150.050, 149.530, 149.750],
            ),
        ],
    );
    assert_eq!(
        fills(&s),
        [(Side::Buy, 150.050, 1000.0), (Side::Sell, 149.500, 1000.0)]
    );
    assert_eq!(s.fills()[1].time, 2 * MIN);
}

#[test]
fn an_attached_stop_inside_the_spread_fills_at_its_sides_open_not_its_trigger() {
    // Short 1000 at bar 0's close with a stop at 150.010, inside the 0.020 spread. Bar 1 is quiet
    // again: the entry sells at the bid open, 150.000, and the stop, a buy, starts the rest of the
    // bar on the ask, 150.000 plus the 0.020 spread at the open: 150.020, already past its
    // trigger. It fills at max(150.020, 150.010) = 150.020; the ask never trades at 150.010.
    let s = run_fx(
        vec![(0, Send::WithStop(market(Side::Sell, 1000.0), 150.010))],
        &[DECIDE, DECIDE],
    );
    assert_eq!(
        fills(&s),
        [(Side::Sell, 150.000, 1000.0), (Side::Buy, 150.020, 1000.0)]
    );

    // The mirror: long 1000 with a stop at 149.990, below the decision bid close 150.000. Bar 1
    // opens at bid 149.980, ask 150.000: the entry buys at the ask open, 150.000, and the stop, a
    // sell, starts on the bid, 150.000 less the 0.020 spread: 149.980, already past its trigger.
    // It fills at min(149.980, 149.990) = 149.980.
    let s = run_fx(
        vec![(0, Send::WithStop(market(Side::Buy, 1000.0), 149.990))],
        &[
            DECIDE,
            (
                [149.980, 150.050, 149.950, 150.000],
                [150.000, 150.070, 149.970, 150.020],
            ),
        ],
    );
    assert_eq!(
        fills(&s),
        [(Side::Buy, 150.000, 1000.0), (Side::Sell, 149.980, 1000.0)]
    );
}

#[test]
fn an_attached_stop_after_a_limit_filled_inside_the_bar_starts_one_spread_from_the_limit() {
    // A sell limit at 150.100 with a stop at 150.110, sent at bar 0's close. Bar 1 opens at bid
    // 150.000, ask 150.020, and the bid high 150.150 reaches the limit: it fills inside the bar at
    // 150.100, not at the open. The stop, a buy, starts the rest of the bar on the ask, the limit
    // plus the 0.020 spread at the open: 150.120, past its trigger. It fills at
    // max(150.120, 150.110) = 150.120, right after the entry.
    let s = run_fx(
        vec![(
            0,
            Send::WithStop(limit(Side::Sell, 1000.0, 150.100), 150.110),
        )],
        &[
            DECIDE,
            (
                [150.000, 150.150, 149.950, 150.050],
                [150.020, 150.170, 149.970, 150.070],
            ),
        ],
    );
    assert_eq!(
        fills(&s),
        [(Side::Sell, 150.100, 1000.0), (Side::Buy, 150.120, 1000.0)]
    );

    // The mirror: a buy limit at 149.900 with a stop at 149.890. The ask low 149.870 reaches the
    // limit inside bar 1, which fills at 149.900. The stop, a sell, starts on the bid, the limit
    // less the 0.020 spread: 149.880, past its trigger. It fills at min(149.880, 149.890) =
    // 149.880.
    let s = run_fx(
        vec![(
            0,
            Send::WithStop(limit(Side::Buy, 1000.0, 149.900), 149.890),
        )],
        &[
            DECIDE,
            (
                [150.000, 150.050, 149.850, 149.950],
                [150.020, 150.070, 149.870, 149.970],
            ),
        ],
    );
    assert_eq!(
        fills(&s),
        [(Side::Buy, 149.900, 1000.0), (Side::Sell, 149.880, 1000.0)]
    );
}

#[test]
fn an_attached_stop_starts_inside_its_sides_candle_when_the_spread_narrows() {
    // A sell limit at 150.000 with a stop at 150.080, sent at bar 0's close (bid 149.800, ask
    // 149.900). Bar 1's bid opens at 149.800 and its high of 150.000 reaches the limit, which fills
    // there, inside the bar. One spread at the open above that is 150.000 + 0.100 = 150.100, but
    // the ask's high is 150.050: the spread narrowed to 0.050 at the top, so the ask was at most
    // 150.050 when the entry filled. The stop starts there, short of its trigger, and rests.
    // Bar 2's ask opens at 150.000 and its high of 150.120 reaches the trigger: the stop fills at
    // max(150.000, 150.080) = 150.080. Started at 150.100 it would have filled on bar 1 at
    // 150.100, a price the ask never reached.
    let s = run_fx(
        vec![(
            0,
            Send::WithStop(limit(Side::Sell, 1000.0, 150.000), 150.080),
        )],
        &[
            (
                [149.800, 149.800, 149.800, 149.800],
                [149.900, 149.900, 149.900, 149.900],
            ),
            (
                [149.800, 150.000, 149.800, 149.900],
                [149.900, 150.050, 149.850, 149.950],
            ),
            (
                [149.950, 150.100, 149.900, 150.000],
                [150.000, 150.120, 149.950, 150.050],
            ),
        ],
    );
    assert_eq!(
        fills(&s),
        [(Side::Sell, 150.000, 1000.0), (Side::Buy, 150.080, 1000.0)]
    );
    assert_eq!(s.fills()[1].time, 2 * MIN);

    // The mirror: a buy limit at 150.000 with a stop at 149.920, sent at bar 0's close (bid
    // 150.100, ask 150.200). Bar 1's ask opens at 150.200 and its low of 150.000 reaches the limit,
    // which fills there. One spread at the open below that is 150.000 - 0.100 = 149.900, but the
    // bid's low is 149.950: the stop starts there, short of its trigger, and rests. Bar 2's bid
    // opens at 150.000 and its low of 149.900 reaches the trigger: the stop fills at
    // min(150.000, 149.920) = 149.920. Started at 149.900 it would have filled on bar 1 at
    // 149.900, a price the bid never reached.
    let s = run_fx(
        vec![(
            0,
            Send::WithStop(limit(Side::Buy, 1000.0, 150.000), 149.920),
        )],
        &[
            (
                [150.100, 150.100, 150.100, 150.100],
                [150.200, 150.200, 150.200, 150.200],
            ),
            (
                [150.100, 150.150, 149.950, 150.050],
                [150.200, 150.200, 150.000, 150.100],
            ),
            (
                [150.000, 150.050, 149.900, 149.950],
                [150.050, 150.100, 149.950, 150.000],
            ),
        ],
    );
    assert_eq!(
        fills(&s),
        [(Side::Buy, 150.000, 1000.0), (Side::Sell, 149.920, 1000.0)]
    );
    assert_eq!(s.fills()[1].time, 2 * MIN);
}

#[test]
fn an_attached_stop_starts_no_lower_than_its_sides_low_when_the_spread_widens() {
    // A sell stop at 149.850 with a protective stop at 149.880, sent at bar 0's close. Bar 1 opens
    // at bid 150.000, ask 150.020, and the bid falls to 149.800: the entry triggers and sells at
    // min(150.000, 149.850) = 149.850. One spread at the open above that is 149.850 + 0.020 =
    // 149.870, but the ask's low is 149.900: the spread widened to 0.100 at the bottom, so the ask
    // was at least 149.900 when the entry filled, already past the trigger. The stop fills at
    // max(149.900, 149.880) = 149.900, 50 yen lost. Started at 149.870 it would have filled at its
    // trigger, 149.880, a price the ask never traded at.
    let s = run_fx(
        vec![(
            0,
            Send::WithStop(stop(Side::Sell, 1000.0, 149.850), 149.880),
        )],
        &[
            DECIDE,
            (
                [150.000, 150.050, 149.800, 149.900],
                [150.020, 150.070, 149.900, 149.950],
            ),
        ],
    );
    assert_eq!(
        fills(&s),
        [(Side::Sell, 149.850, 1000.0), (Side::Buy, 149.900, 1000.0)]
    );
    assert!(s.fills().iter().all(|f| f.time == MIN));
}

#[test]
fn an_attached_stop_after_a_limit_filled_between_the_bars_is_crossed_on_the_way_to_the_open() {
    // A sell limit at 150.100 with a stop at 150.150, sent at bar 0's close. Bar 1's bid opens at
    // 150.300, through the limit: the market rose through 150.100 between the bars, where the limit
    // filled. The ask, one spread at the open above, was then at 150.100 + 0.020 = 150.120 and rose
    // on to its open of 150.320, through the stop: it fills at its trigger, 150.150. That happened
    // before the bar's ask candle began, so the candle's low of 150.250 does not hold the start up;
    // held there, the stop would fill at 150.250.
    let s = run_fx(
        vec![(
            0,
            Send::WithStop(limit(Side::Sell, 1000.0, 150.100), 150.150),
        )],
        &[
            DECIDE,
            (
                [150.300, 150.400, 150.230, 150.350],
                [150.320, 150.420, 150.250, 150.370],
            ),
        ],
    );
    assert_eq!(
        fills(&s),
        [(Side::Sell, 150.100, 1000.0), (Side::Buy, 150.150, 1000.0)]
    );
}

#[test]
fn a_take_profit_that_waits_for_the_open_takes_its_side_of_the_open() {
    // A market buy of 1000 and a reduce-only take-profit selling it at 150.100, both sent on bar
    // 0's close. Bar 1's bid opens at 150.300, through the take-profit: the market passed it
    // between the bars, while the account was flat, so it waits for the open. There the buy fills
    // at the ask open, 150.320, and the take-profit then sells at the bid open, 150.300, the
    // better of its limit and the open on its side.
    let s = run_fx(
        vec![
            (0, Send::Plain(market(Side::Buy, 1000.0))),
            (
                0,
                Send::Plain(reduce_only(limit(Side::Sell, 1000.0, 150.100))),
            ),
        ],
        &[
            DECIDE,
            (
                [150.300, 150.400, 150.230, 150.350],
                [150.320, 150.420, 150.250, 150.370],
            ),
        ],
    );
    assert_eq!(
        fills(&s),
        [(Side::Buy, 150.320, 1000.0), (Side::Sell, 150.300, 1000.0)]
    );
    assert_eq!(s.portfolio().position(), 0.0);
    // The round trip pays the spread at the open: 1000 * 0.020 = 20 yen.
    approx(s.portfolio().cash(), 1_000_000.0 - 20.0);
}

#[test]
fn a_longs_protective_stop_is_checked_against_the_ask_it_buys_at() {
    // A market buy decided at the ask close of 150.020 with a stop at 150.010: below the price the
    // entry is valued at, so it is accepted, as a short's stop between the closes is. It fills at
    // the ask open, 150.020, and the stop, at its trigger above the bid of 150.000, sells at once
    // at the bid open.
    let s = run_fx(
        vec![(0, Send::WithStop(market(Side::Buy, 1000.0), 150.010))],
        &[DECIDE, DECIDE],
    );
    assert_eq!(
        fills(&s),
        [(Side::Buy, 150.020, 1000.0), (Side::Sell, 150.000, 1000.0)]
    );
}

#[test]
fn a_buy_limit_fills_on_the_ask_and_a_sell_limit_on_the_bid() {
    // Both rest from bar 0's close: buy 1000 at 149.900, sell 1000 at 150.200.
    // Bar 1: the bid low 149.890 is under the buy limit but the ask low 149.910 is not, and the
    // ask high 150.210 is over the sell limit but the bid high 150.190 is not. Neither fills.
    // Bar 2: the ask low 149.890 reaches the buy limit and the bid high 150.210 the sell limit.
    // Both fill at their limits, as makers. The bar opens between them, so both fill inside its
    // range, in order of id: the buy first.
    let s = run_fx(
        vec![
            (0, Send::Plain(limit(Side::Buy, 1000.0, 149.900))),
            (0, Send::Plain(limit(Side::Sell, 1000.0, 150.200))),
        ],
        &[
            DECIDE,
            (
                [150.000, 150.190, 149.890, 150.000],
                [150.020, 150.210, 149.910, 150.020],
            ),
            (
                [150.000, 150.210, 149.870, 150.000],
                [150.020, 150.230, 149.890, 150.020],
            ),
        ],
    );
    assert_eq!(
        fills(&s),
        [(Side::Buy, 149.900, 1000.0), (Side::Sell, 150.200, 1000.0)]
    );
    assert!(s.fills().iter().all(|f| f.time == 2 * MIN));
    assert!(
        s.fills()
            .iter()
            .all(|f| f.liquidity == td_core::Liquidity::Maker)
    );
}

#[test]
fn a_buy_limit_between_the_bid_and_the_ask_close_rests_on_the_ask() {
    // A buy limit at 150.010, sent at bar 0's close: above the bid close 150.000 but below the ask
    // close 150.020, so it is not marketable when it arrives and rests. Bar 1's ask opens at
    // 149.990, through it: it filled between the bars at its limit, 150.010, as a maker. Taken as
    // marketable against the bid close it would have filled as a taker at the better ask open.
    let s = run_fx(
        vec![(0, Send::Plain(limit(Side::Buy, 1000.0, 150.010)))],
        &[
            DECIDE,
            (
                [149.970, 150.000, 149.900, 149.980],
                [149.990, 150.020, 149.920, 150.000],
            ),
        ],
    );
    assert_eq!(fills(&s), [(Side::Buy, 150.010, 1000.0)]);
    assert_eq!(s.fills()[0].liquidity, td_core::Liquidity::Maker);
    assert_eq!(s.orders()[0].decision_close, 150.020);
}

#[test]
fn a_buy_stop_limit_triggers_and_fills_on_the_ask() {
    // A buy stop-limit, trigger 150.300 and limit 150.320, sent at bar 0's close. Bar 1 has a
    // 0.040 spread: the ask high 150.330 reaches the trigger while the bid high 150.290 does not.
    // The ask opened at 150.040, below the trigger, so it triggers at 150.300, inside its limit,
    // and fills there as a taker. Read on the bid it would not have triggered.
    let s = run_fx(
        vec![(
            0,
            Send::Plain(stop_limit(Side::Buy, 1000.0, 150.300, 150.320)),
        )],
        &[
            DECIDE,
            (
                [150.000, 150.290, 149.900, 150.100],
                [150.040, 150.330, 149.940, 150.140],
            ),
        ],
    );
    assert_eq!(fills(&s), [(Side::Buy, 150.300, 1000.0)]);
    assert_eq!(s.fills()[0].time, MIN);
    assert_eq!(s.fills()[0].liquidity, td_core::Liquidity::Taker);
}

#[test]
fn a_sell_stop_limit_triggered_past_its_limit_rests_on_the_bid() {
    // A sell stop-limit, trigger 149.800 and limit 149.850, sent at bar 0's close. Bars 1 to 3
    // have a 0.030 spread. Bar 1's bid opens at 149.700, through the trigger and below the limit,
    // so it triggers but cannot fill and rests as a sell limit at 149.850. Bar 2: the ask high
    // 149.870 is over the limit but the bid high 149.840 is not, and a sell reads the bid, so it
    // still rests. Bar 3: the bid high 149.860 reaches the limit, and it fills there as a maker.
    let s = run_fx(
        vec![(
            0,
            Send::Plain(stop_limit(Side::Sell, 1000.0, 149.800, 149.850)),
        )],
        &[
            DECIDE,
            (
                [149.700, 149.750, 149.650, 149.700],
                [149.730, 149.780, 149.680, 149.730],
            ),
            (
                [149.700, 149.840, 149.650, 149.800],
                [149.730, 149.870, 149.680, 149.830],
            ),
            (
                [149.800, 149.860, 149.750, 149.800],
                [149.830, 149.890, 149.780, 149.830],
            ),
        ],
    );
    assert_eq!(fills(&s), [(Side::Sell, 149.850, 1000.0)]);
    assert_eq!(s.fills()[0].time, 3 * MIN);
    assert_eq!(s.fills()[0].liquidity, td_core::Liquidity::Maker);
}

#[test]
fn an_ioc_order_takes_its_sides_open_or_is_cancelled() {
    // Three IOC orders sent at bar 0's close; bar 1 opens at bid 150.000, ask 150.020.
    // - A buy limit at 150.030 is marketable at the ask open and fills there, at 150.020, as a
    //   taker. Read on the bid it would have filled at 150.000.
    // - A sell limit at 150.010 is not marketable at the bid open, 150.000, so it is cancelled,
    //   although the ask open and the bid high are over it: an IOC sees only its side's open.
    // - A market sell fills at the bid open, 150.000.
    let s = run_fx(
        vec![
            (0, Send::Plain(ioc(limit(Side::Buy, 1000.0, 150.030)))),
            (0, Send::Plain(ioc(limit(Side::Sell, 1000.0, 150.010)))),
            (0, Send::Plain(ioc(market(Side::Sell, 1000.0)))),
        ],
        &[
            DECIDE,
            (
                [150.000, 150.100, 149.900, 150.050],
                [150.020, 150.120, 149.920, 150.070],
            ),
        ],
    );
    assert_eq!(
        fills(&s),
        [(Side::Buy, 150.020, 1000.0), (Side::Sell, 150.000, 1000.0)]
    );
    assert!(
        s.fills()
            .iter()
            .all(|f| f.time == MIN && f.liquidity == td_core::Liquidity::Taker)
    );
    assert!(matches!(s.orders()[1].status, OrderStatus::Cancelled(_)));
}

#[test]
fn an_open_short_is_marked_at_the_ask_close_and_a_long_at_the_bid_close() {
    let bar1 = (
        [150.000, 150.200, 149.900, 150.100],
        [150.020, 150.220, 149.920, 150.120],
    );
    // Short 1000 at the bid open 150.000. Bar 1 closes at bid 150.100, ask 150.120. Closing the
    // short buys at the ask: 1000 * (150.000 - 150.120) = -120 yen.
    let s = run_fx(
        vec![(0, Send::Plain(market(Side::Sell, 1000.0)))],
        &[DECIDE, bar1],
    );
    assert_eq!(s.portfolio().mark_price(), 150.120);
    approx(s.equity()[1].equity, 1_000_000.0 - 120.0);

    // Long 1000 at the ask open 150.020. Closing it sells at the bid close:
    // 1000 * (150.100 - 150.020) = +80 yen.
    let s = run_fx(
        vec![(0, Send::Plain(market(Side::Buy, 1000.0)))],
        &[DECIDE, bar1],
    );
    assert_eq!(s.portfolio().mark_price(), 150.100);
    approx(s.equity()[1].equity, 1_000_000.0 + 80.0);
}

const HOUR: Millis = 3_600_000;
const DAY: Millis = 24 * HOUR;

/// 2024-01-12, a Friday in New York's standard time: the FX week closes at 22:00 UTC and reopens
/// on Sunday 2024-01-14 at 22:00 UTC.
const FRIDAY: Millis = 19_734 * DAY;

fn at(open_time: Millis, [open, high, low, close]: [f64; 4]) -> Candle {
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

#[test]
fn a_bar_wholly_in_closed_hours_is_refused() {
    let flat = [150.0, 150.0, 150.0, 150.0];
    let ask = [150.02, 150.02, 150.02, 150.02];
    let feed = |tf: Timeframe, open_time: Millis| {
        let mut s = Session::new(Box::new(Plan(vec![])), usdjpy(), tf, fx_config()).unwrap();
        s.on_bar_with_ask(at(open_time, flat), Some(at(open_time, ask)), &[])
    };
    // Friday 21:00 UTC is the week's last open hour; 22:00 is the first closed one, and so is
    // Saturday noon. Sunday 21:00 is the last closed hour and 22:00 the first open one.
    assert_eq!(feed(Timeframe::H1, FRIDAY + 21 * HOUR), Ok(()));
    for closed in [
        FRIDAY + 22 * HOUR,
        FRIDAY + DAY + 12 * HOUR,
        FRIDAY + 2 * DAY + 21 * HOUR,
    ] {
        assert_eq!(
            feed(Timeframe::H1, closed),
            Err(EngineError::ClosedBar { time: closed })
        );
    }
    assert_eq!(feed(Timeframe::H1, FRIDAY + 2 * DAY + 22 * HOUR), Ok(()));
    // The UTC day bar that opens on Sunday at 00:00 holds the two open hours from 22:00, so it is
    // accepted; Saturday's day bar is closed throughout.
    assert_eq!(feed(Timeframe::D1, FRIDAY + 2 * DAY), Ok(()));
    assert_eq!(
        feed(Timeframe::D1, FRIDAY + DAY),
        Err(EngineError::ClosedBar { time: FRIDAY + DAY })
    );
}

#[test]
fn an_order_decided_on_fridays_last_bar_fills_at_sundays_first_open_on_its_side() {
    // A buy and a sell sent at the close of Friday's 21:00 bar, the last before the weekend. The
    // next bar fed is Sunday's 22:00 bar, which reopens at bid 149.500, ask 149.530: the buy pays
    // that ask open and the sell gets that bid open, the reopening prices and spread.
    let mut s = Session::new(
        Box::new(Plan(vec![
            (0, Send::Plain(market(Side::Buy, 1000.0))),
            (0, Send::Plain(market(Side::Sell, 1000.0))),
        ])),
        usdjpy(),
        Timeframe::H1,
        fx_config(),
    )
    .unwrap();
    let friday = FRIDAY + 21 * HOUR;
    let sunday = FRIDAY + 2 * DAY + 22 * HOUR;
    s.on_bar_with_ask(
        at(friday, [150.0, 150.0, 150.0, 150.0]),
        Some(at(friday, [150.02, 150.02, 150.02, 150.02])),
        &[],
    )
    .unwrap();
    s.on_bar_with_ask(
        at(sunday, [149.5, 149.6, 149.4, 149.55]),
        Some(at(sunday, [149.53, 149.63, 149.43, 149.58])),
        &[],
    )
    .unwrap();
    assert_eq!(
        fills(&s),
        [(Side::Buy, 149.530, 1000.0), (Side::Sell, 149.500, 1000.0)]
    );
    assert!(s.fills().iter().all(|f| f.time == sunday));
}

#[test]
fn asks_are_required_for_fx_must_match_the_bar_and_are_refused_elsewhere() {
    let bid = candle(0, [150.0, 150.0, 150.0, 150.0]);
    let ask = candle(0, [150.02, 150.02, 150.02, 150.02]);
    let fx = || Session::new(Box::new(Plan(vec![])), usdjpy(), Timeframe::M1, fx_config()).unwrap();

    // FX without an ask: an error, never the bid used as the ask.
    assert_eq!(
        fx().on_bar(bid, &[]),
        Err(EngineError::MissingAsk {
            time: 0,
            kind: InstrumentKind::Fx
        })
    );
    // An ask from another minute.
    let late = Candle {
        open_time: MIN,
        ..ask
    };
    assert_eq!(
        fx().on_bar_with_ask(bid, Some(late), &[]),
        Err(EngineError::UnmatchedAsk { time: MIN })
    );
    // An ask on spot.
    let mut s = Session::new(Box::new(Plan(vec![])), spot(), Timeframe::M1, config(0.0)).unwrap();
    assert_eq!(
        s.on_bar_with_ask(bid, Some(ask), &[]),
        Err(EngineError::AsksWithoutQuotes {
            kind: InstrumentKind::Spot
        })
    );
    // Nothing was fed by any of them.
    assert!(s.bars().is_empty());
}

#[test]
fn a_per_unit_commission_is_charged_on_every_fill() {
    // 0.002 yen per unit, no rate. Buy 1000 at bar 0's close, sell 1000 at bar 1's close.
    // Entry: bar 1's ask open 150.020, fee 1000 * 0.002 = 2 yen.
    // Exit: bar 2's bid open 150.100, fee 2 yen.
    // Net: 1000 * (150.100 - 150.020) - 2 - 2 = 80 - 4 = 76 yen.
    let fees = FeeSchedule {
        maker: 0.0,
        taker: 0.0,
        per_unit: 0.002,
    };
    let config = SessionConfig {
        fees: Some(fees),
        ..fx_config()
    };
    let quotes = [
        DECIDE,
        (
            [150.000, 150.100, 149.900, 150.050],
            [150.020, 150.120, 149.920, 150.070],
        ),
        (
            [150.100, 150.200, 150.000, 150.150],
            [150.120, 150.220, 150.020, 150.170],
        ),
    ];
    let s = run_fx_with(
        vec![
            (0, Send::Plain(market(Side::Buy, 1000.0))),
            (1, Send::Plain(market(Side::Sell, 1000.0))),
        ],
        &quotes,
        config,
    );
    assert_eq!(
        fills(&s),
        [(Side::Buy, 150.020, 1000.0), (Side::Sell, 150.100, 1000.0)]
    );
    for fill in s.fills() {
        approx(fill.fee, 2.0);
    }
    approx(s.portfolio().fees_paid(), 4.0);
    approx(s.portfolio().cash(), 1_000_000.0 + 76.0);
    let trades = td_engine::build_trades(
        s.fills(),
        s.bars(),
        &asks_of(&quotes),
        s.bar_funding(),
        td_core::AccountCurrency::Quote,
        None,
    );
    assert_eq!(trades.len(), 1);
    approx(trades[0].fees, 4.0);
    approx(trades[0].net_pnl, 76.0);

    // With a rate as well, the per-unit term adds to it: 0.0001 of 150 020 notional is 15.002,
    // plus 2 per-unit, 17.002 on the entry.
    let both = FeeSchedule {
        taker: 0.0001,
        ..fees
    };
    approx(both.fee(150.020, 1000.0, both.taker), 15.002 + 2.0);
}

/// An FX backtest of the SMA cross over `bars` one-minute bars, all traded.
fn fx_backtest(bars: usize) -> td_engine::BacktestConfig {
    td_engine::BacktestConfig {
        strategy: "sma_cross".into(),
        params: td_engine::Params::new()
            .with("fast", td_engine::ParamValue::Int(2))
            .with("slow", td_engine::ParamValue::Int(4)),
        instrument: usdjpy(),
        timeframe: Timeframe::M1,
        from: 0,
        to: bars as Millis * MIN,
        starting_cash: 1_000_000.0,
        slippage_bps: 0.0,
        fee_override: Some(FeeSchedule {
            maker: 0.0,
            taker: 0.0,
            per_unit: 0.002,
        }),
        max_leverage: 1.0,
        warmup_from: None,
        close_at_end: false,
        account_currency: td_core::AccountCurrency::Quote,
    }
}

/// Bid bars drifting up by 0.010 a minute from 150, and asks 0.020 above them.
fn quotes(n: usize) -> (Vec<Candle>, Vec<Candle>) {
    let bids: Vec<Candle> = (0..n)
        .map(|i| {
            let p = 150.0 + 0.01 * i as f64;
            candle(i as i64, [p, p + 0.005, p - 0.005, p + 0.002])
        })
        .collect();
    let asks = bids
        .iter()
        .map(|b| Candle {
            open: b.open + 0.02,
            high: b.high + 0.02,
            low: b.low + 0.02,
            close: b.close + 0.02,
            ..*b
        })
        .collect();
    (bids, asks)
}

#[test]
fn an_fx_run_needs_matching_asks_and_a_spot_run_refuses_them() {
    let (bids, asks) = quotes(8);
    let config = fx_backtest(8);
    assert!(td_engine::run(&config, &bids, &asks, &[]).is_ok());
    // Without asks.
    assert_eq!(
        td_engine::run(&config, &bids, &[], &[]).unwrap_err(),
        EngineError::MissingAsk {
            time: 0,
            kind: InstrumentKind::Fx
        }
    );
    // One ask missing, for the bar at minute 3.
    let mut gap = asks.clone();
    gap.remove(3);
    assert_eq!(
        td_engine::run(&config, &bids, &gap, &[]).unwrap_err(),
        EngineError::MissingAsk {
            time: 3 * MIN,
            kind: InstrumentKind::Fx
        }
    );
    // An ask at a time with no bar: minute 3's ask moved 30 seconds on.
    let mut moved = asks.clone();
    moved[3].open_time += 30_000;
    assert_eq!(
        td_engine::run(&config, &bids, &moved, &[]).unwrap_err(),
        EngineError::UnmatchedAsk {
            time: 3 * MIN + 30_000
        }
    );
    // Asks on a spot run.
    let spot_config = td_engine::BacktestConfig {
        instrument: spot(),
        fee_override: None,
        ..config
    };
    assert_eq!(
        td_engine::run(&spot_config, &bids, &asks, &[]).unwrap_err(),
        EngineError::AsksWithoutQuotes {
            kind: InstrumentKind::Spot
        }
    );
}

#[test]
fn an_ask_below_the_bid_is_refused() {
    // Every bar's bid, and an ask 0.020 above it at each price.
    let bid = [150.000, 150.100, 149.900, 150.050];
    let ask = [150.020, 150.120, 149.920, 150.070];
    // The ask with its open, high, low or close 0.010 below the bid's, still a valid candle.
    let crossed = [
        [149.990, 150.120, 149.920, 150.070],
        [150.020, 150.090, 149.920, 150.070],
        [150.020, 150.120, 149.890, 150.070],
        [150.020, 150.120, 149.920, 150.040],
    ];
    let config = fx_backtest(8);
    let bids: Vec<Candle> = (0..8).map(|i| candle(i, bid)).collect();
    let asks_with = |at_3: [f64; 4]| -> Vec<Candle> {
        (0..8)
            .map(|i| candle(i, if i == 3 { at_3 } else { ask }))
            .collect()
    };
    assert!(td_engine::run(&config, &bids, &asks_with(ask), &[]).is_ok());
    for prices in crossed {
        // In a run, at minute 3.
        assert_eq!(
            td_engine::run(&config, &bids, &asks_with(prices), &[]).unwrap_err(),
            EngineError::CrossedQuote { time: 3 * MIN },
            "{prices:?}"
        );
        // On a session, after one good bar: refused, and nothing fed.
        let mut s =
            Session::new(Box::new(Plan(vec![])), usdjpy(), Timeframe::M1, fx_config()).unwrap();
        s.on_bar_with_ask(candle(0, bid), Some(candle(0, ask)), &[])
            .unwrap();
        assert_eq!(
            s.on_bar_with_ask(candle(1, bid), Some(candle(1, prices)), &[]),
            Err(EngineError::CrossedQuote { time: MIN }),
            "{prices:?}"
        );
        assert_eq!(s.bars().len(), 1);
    }
    // The two series swapped, as a loader that mixed up the files would pass them: the "ask" is
    // then 0.020 below the "bid" at every price, and each round trip would earn the spread.
    assert_eq!(
        td_engine::run(&config, &asks_with(ask), &bids, &[]).unwrap_err(),
        EngineError::CrossedQuote { time: 0 }
    );
}

#[test]
fn a_zero_spread_is_accepted() {
    // The ask equal to the bid at every price. A buy and a sell sent at bar 0's close both fill at
    // bar 1's open, 150.000, so the round trip costs nothing.
    let flat = [150.000, 150.000, 150.000, 150.000];
    let ranged = [150.000, 150.100, 149.900, 150.050];
    let s = run_fx(
        vec![
            (0, Send::Plain(market(Side::Buy, 1000.0))),
            (0, Send::Plain(market(Side::Sell, 1000.0))),
        ],
        &[(flat, flat), (ranged, ranged)],
    );
    assert_eq!(
        fills(&s),
        [(Side::Buy, 150.000, 1000.0), (Side::Sell, 150.000, 1000.0)]
    );
    approx(s.portfolio().cash(), 1_000_000.0);
    // In a run, too.
    let bids: Vec<Candle> = (0..8).map(|i| candle(i, ranged)).collect();
    assert!(td_engine::run(&fx_backtest(8), &bids, &bids, &[]).is_ok());
}

#[test]
fn a_second_ask_at_one_time_is_refused() {
    // Two asks at minute 3, the second with its high a tick higher: an error, as two bars at one
    // time are, rather than the later one kept. The fingerprint, which the tester takes before it
    // runs anything, refuses them the same way.
    let (bids, mut asks) = quotes(8);
    let second = Candle {
        high: asks[3].high + 0.001,
        ..asks[3]
    };
    asks.insert(4, second);
    let config = fx_backtest(8);
    let duplicate = EngineError::DuplicateAsk { time: 3 * MIN };
    assert_eq!(
        td_engine::run(&config, &bids, &asks, &[]).unwrap_err(),
        duplicate
    );
    assert_eq!(
        td_engine::config_fingerprint(&config, &bids, &asks, &[]).unwrap_err(),
        duplicate
    );
}

#[test]
fn bad_asks_are_refused_on_warm_up_bars_and_by_the_fingerprint() {
    // Eight bars: the first three warm the strategy up and the other five trade. The bid at minute
    // 1, a warm-up bar, is 150.010 / 150.015 / 150.005 / 150.012 and its ask 0.020 above. Warm-up
    // bars reach the session without their asks, so the run checks those asks itself, as the
    // session checks a traded bar's. The fingerprint runs no session, so it checks every ask.
    let (bids, asks) = quotes(8);
    let config = td_engine::BacktestConfig {
        from: 3 * MIN,
        warmup_from: Some(0),
        ..fx_backtest(8)
    };
    assert!(td_engine::run(&config, &bids, &asks, &[]).is_ok());
    assert!(td_engine::config_fingerprint(&config, &bids, &asks, &[]).is_ok());
    // Each case replaces one ask, and the run and the fingerprint must both refuse it.
    let refuse = |at: usize, ask: Candle, refused: EngineError| {
        let mut asks = asks.clone();
        asks[at] = ask;
        assert_eq!(
            td_engine::run(&config, &bids, &asks, &[]).unwrap_err(),
            refused
        );
        assert_eq!(
            td_engine::config_fingerprint(&config, &bids, &asks, &[]).unwrap_err(),
            refused
        );
    };
    // An ask low of 149.995 at minute 1, under the bid low of 150.005: crossed.
    let crossed = Candle {
        low: 149.995,
        ..asks[1]
    };
    refuse(1, crossed, EngineError::CrossedQuote { time: MIN });
    // An ask high of 150.020 at minute 1, under the ask's own low of 150.025 and its open of
    // 150.030: not a valid candle.
    let invalid = Candle {
        high: 150.020,
        ..asks[1]
    };
    let not_a_candle = td_core::CoreError::InvalidCandle {
        open_time: MIN,
        reason: "high is below open or close",
    };
    refuse(1, invalid, EngineError::Core(not_a_candle));
    // A traded bar's ask, at minute 5, with its low of 150.035 under the bid low of 150.045: the
    // session refuses it during the run, and the fingerprint refuses it the same way.
    let crossed = Candle {
        low: 150.035,
        ..asks[5]
    };
    refuse(5, crossed, EngineError::CrossedQuote { time: 5 * MIN });
}

#[test]
fn the_fx_benchmark_buys_at_the_first_ask_open_and_pays_the_per_unit_commission() {
    // All of a million yen at the first ask open, 150.020, with 0.002 a unit:
    // floor(1 000 000 / (150.020 + 0.002)) = floor(6665.69...) = 6665 units, fee 6665 * 0.002
    // = 13.33 yen.
    let (bids, asks) = quotes(8);
    let result = td_engine::run(&fx_backtest(8), &bids, &asks, &[]).unwrap();
    let bench = &result.benchmark;
    assert_eq!(bench.entry_price, 150.020);
    assert_eq!(bench.qty, 6665.0);
    // Marked at the last bid close, 150.072: 1 000 000 - 13.33 + 6665 * (150.072 - 150.020).
    let last = bench.equity.last().unwrap().equity;
    approx(last, 1_000_000.0 - 13.33 + 6665.0 * (150.072 - 150.020));
}

#[test]
fn the_fingerprint_covers_the_asks_and_is_unchanged_without_them() {
    let (bids, asks) = quotes(8);
    let config = fx_backtest(8);
    let base = td_engine::config_fingerprint(&config, &bids, &asks, &[]).unwrap();
    assert_eq!(
        td_engine::run(&config, &bids, &asks, &[])
            .unwrap()
            .data_fingerprint,
        base
    );
    // One ask's high moved by one tick.
    let mut changed = asks.clone();
    changed[5].high += 0.001;
    assert_ne!(
        td_engine::config_fingerprint(&config, &bids, &changed, &[]).unwrap(),
        base
    );
    assert_ne!(
        td_engine::data_fingerprint(&bids, &asks, &[]),
        td_engine::data_fingerprint(&bids, &changed, &[])
    );

    // Crypto passes no asks, and its fingerprint is the one main computed before asks existed for
    // these two bars and one funding event.
    let bars = [
        bar(0, 100.0, 101.0, 99.0, 100.5),
        bar(1, 100.5, 102.0, 100.0, 101.0),
    ];
    assert_eq!(
        td_engine::data_fingerprint(&bars, &[], &[(30_000, 0.0001)]),
        "7e1973b2e1f7195998b82e84290884c5953883179b654faa6d8472066c3a60cf"
    );
}

/// Runs `script` on FX over `(bid, ask)` one-minute bars with `max_leverage`.
fn run_script(script: Box<Script>, bars: &[([f64; 4], [f64; 4])], max_leverage: f64) -> Session {
    let config = SessionConfig {
        max_leverage,
        ..fx_config()
    };
    let mut s = Session::new(script, usdjpy(), Timeframe::M1, config).unwrap();
    for (i, (bid, ask)) in bars.iter().enumerate() {
        let i = i as i64;
        s.on_bar_with_ask(candle(i, *bid), Some(candle(i, *ask)), &[])
            .unwrap();
    }
    s
}

#[test]
fn an_fx_short_is_allowed_from_flat_with_a_protective_stop() {
    // Flat at bar 0's close, a sell of 1000 with a buy stop at 150.500 above it: spot would refuse
    // both (no short, no stop on a sell). On FX the sell fills at bar 1's bid open, 150.000, the
    // position is -1000, and the stop rests as the one open order, a buy.
    let s = run_fx(
        vec![(0, Send::WithStop(market(Side::Sell, 1000.0), 150.500))],
        &[DECIDE, DECIDE],
    );
    assert_eq!(fills(&s), vec![(Side::Sell, 150.000, 1000.0)]);
    assert_eq!(s.portfolio().position(), -1000.0);
    let open = s.open_orders();
    assert_eq!(open.len(), 1);
    assert_eq!(open[0].request.side, Side::Buy);
    assert_eq!(
        open[0].request.order_type,
        OrderType::Stop { trigger: 150.500 }
    );
}

#[test]
fn an_fx_position_is_capped_by_the_leverage_setting() {
    // Flat with 1 000 000 yen of equity; orders are valued at the decision bar's close, 150.000.
    // At 1x the cap is 1 000 000: a sell of 6667 would be worth 6667 * 150 = 1 000 050 and is
    // refused, one of 6666 is worth 999 900 and is accepted, filling at the bid open, 150.000.
    let (script, log) = Script::new(vec![
        (0, market(Side::Sell, 6667.0)),
        (0, market(Side::Sell, 6666.0)),
    ]);
    let s = run_script(script, &[DECIDE, DECIDE], 1.0);
    let log = log.lock().unwrap();
    let err = log[0].as_ref().unwrap_err();
    assert!(
        err.reason.starts_with("over the leverage cap of 1x"),
        "{}",
        err.reason
    );
    assert!(log[1].is_ok());
    assert_eq!(fills(&s), vec![(Side::Sell, 150.000, 6666.0)]);

    // At 2x the cap is 2 000 000. A buy is valued at the ask close, 150.020, the price it is
    // decided on: one of 13 332 would be worth 2 000 066.64 and is refused (at the bid close it
    // would pass, at 1 999 800), one of 13 331 is worth 1 999 916.62 and is accepted. That is more
    // than the cash, which FX does not check or cut, as it would on spot: it fills whole at the
    // ask open, 150.020.
    let (script, log) = Script::new(vec![
        (0, market(Side::Buy, 13_332.0)),
        (0, market(Side::Buy, 13_331.0)),
    ]);
    let s = run_script(script, &[DECIDE, DECIDE], 2.0);
    let log = log.lock().unwrap();
    let err = log[0].as_ref().unwrap_err();
    assert!(
        err.reason.starts_with("over the leverage cap of 2x"),
        "{}",
        err.reason
    );
    assert!(log[1].is_ok());
    assert_eq!(fills(&s), vec![(Side::Buy, 150.020, 13_331.0)]);
}

#[test]
fn funding_on_an_fx_session_is_refused() {
    // FX has no funding: an event inside the bar's minute is an error, not a charge.
    let mut s = Session::new(Box::new(Plan(vec![])), usdjpy(), Timeframe::M1, fx_config()).unwrap();
    let (bid, ask) = DECIDE;
    assert_eq!(
        s.on_bar_with_ask(candle(0, bid), Some(candle(0, ask)), &[(30_000, 0.0001)]),
        Err(EngineError::FundingOnSpot)
    );
}

#[test]
fn the_built_in_strategies_short_on_fx() {
    // Bids falling 0.010 a minute from 150 with asks 0.020 above. With allow_short, the moving
    // average cross goes short once the fast average is below the slow one, and the Donchian
    // breakout goes short on a close below the previous lows; each first fill is a sell.
    let n = 12;
    let bids: Vec<Candle> = (0..n)
        .map(|i| {
            let p = 150.0 - 0.01 * i as f64;
            candle(i, [p, p + 0.005, p - 0.005, p + 0.002])
        })
        .collect();
    let asks: Vec<Candle> = bids
        .iter()
        .map(|b| Candle {
            open: b.open + 0.02,
            high: b.high + 0.02,
            low: b.low + 0.02,
            close: b.close + 0.02,
            ..*b
        })
        .collect();
    let short = td_engine::ParamValue::Bool(true);
    let sma = td_engine::BacktestConfig {
        params: td_engine::Params::new()
            .with("fast", td_engine::ParamValue::Int(2))
            .with("slow", td_engine::ParamValue::Int(4))
            .with("allow_short", short),
        ..fx_backtest(n as usize)
    };
    let int = td_engine::ParamValue::Int;
    let donchian = td_engine::BacktestConfig {
        strategy: "donchian_breakout".into(),
        params: td_engine::Params::new()
            .with("entry", int(2))
            .with("exit", int(2))
            .with("atr_period", int(2))
            .with("allow_short", short),
        ..fx_backtest(n as usize)
    };
    for config in [sma, donchian] {
        let result = td_engine::run(&config, &bids, &asks, &[]).unwrap();
        assert_eq!(
            result.fills.first().map(|f| f.side),
            Some(Side::Sell),
            "{}",
            config.strategy
        );
    }
}

/// The entry bar of the shorts below: the ask 0.020 above the bid at the open, low and close, and
/// 0.100 above it at the high, where it spikes to 150.300.
const SPIKE: ([f64; 4], [f64; 4]) = (
    [150.000, 150.200, 149.900, 150.100],
    [150.020, 150.300, 149.920, 150.120],
);

/// A bar that opens with the ask at 149.900, 0.050 above the bid, and rises through 150.400.
const THROUGH: ([f64; 4], [f64; 4]) = (
    [149.850, 150.450, 149.850, 150.400],
    [149.900, 150.500, 149.900, 150.420],
);

/// A bar whose ask rises from its open of 150.100 to 150.400, then falls to 149.920.
const DIP: ([f64; 4], [f64; 4]) = (
    [150.050, 150.250, 149.900, 149.950],
    [150.100, 150.400, 149.920, 149.970],
);

/// A bar that opens with the bid at its low of 149.800 and the ask 0.100 above it, an ask that
/// comes down no further than 149.850.
const WIDE: ([f64; 4], [f64; 4]) = (
    [149.800, 150.000, 149.800, 149.900],
    [149.900, 150.050, 149.850, 149.950],
);

/// The ask candle of each of `quotes`, by bar, as the measures take them.
fn asks_of(quotes: &[([f64; 4], [f64; 4])]) -> Vec<Option<Candle>> {
    quotes
        .iter()
        .enumerate()
        .map(|(i, (_, ask))| Some(candle(i as i64, *ask)))
        .collect()
}

/// A short of 1000 sold at bar 1's bid open, 150.000, with a buy stop at 150.400.
fn stopped_short() -> Vec<(usize, Send)> {
    vec![(0, Send::WithStop(market(Side::Sell, 1000.0), 150.400))]
}

/// A short of 1000 sold at bar 1's bid open, 150.000, and a buy limit at 149.950 sent at bar 1's
/// close to close it.
fn limited_short() -> Vec<(usize, Send)> {
    vec![
        (0, Send::Plain(market(Side::Sell, 1000.0))),
        (1, Send::Plain(limit(Side::Buy, 1000.0, 149.950))),
    ]
}

fn intrabar_of(s: &Session, quotes: &[([f64; 4], [f64; 4])]) -> td_engine::risk::Intrabar {
    td_engine::risk::intrabar(
        1_000_000.0,
        s.bars(),
        &asks_of(quotes),
        s.equity(),
        s.fills(),
        s.bar_funding(),
        td_core::AccountCurrency::Quote,
    )
}

/// The trades of a session, with their stops.
fn trades_of(s: &Session, quotes: &[([f64; 4], [f64; 4])]) -> Vec<td_engine::Trade> {
    let mut trades = td_engine::build_trades(
        s.fills(),
        s.bars(),
        &asks_of(quotes),
        s.bar_funding(),
        td_core::AccountCurrency::Quote,
        None,
    );
    td_engine::attach_stops(
        &mut trades,
        s.trade_stops(),
        td_core::AccountCurrency::Quote,
    );
    trades
}

#[test]
fn an_fx_shorts_equity_inside_each_bar_is_valued_on_the_ask() {
    // The stopped short. Bar 1 closes with it marked at the ask close: 1 000 000 + 1000 *
    // (150.000 - 150.120) = 999 880. Inside bar 1 the ask's high and low give 999 880 - 1000 *
    // (150.300 - 150.120) = 999 700 and 999 880 + 1000 * (150.120 - 149.920) = 1 000 080; the
    // bid's high would give 999 880 - 1000 * (150.200 - 150.100) = 999 780.
    // Bar 2 opens at ask 149.900, 999 880 + 1000 * (150.120 - 149.900) = 1 000 100, and the stop
    // fills at 150.400: 1 000 100 - 1000 * (150.400 - 149.900) = 999 600, flat after. Read from
    // the bid close, the bid open would give 999 880 + 1000 * (150.100 - 149.850) = 1 000 130 and
    // the stop 999 580, an equity the account never had.
    let quotes = [DECIDE, SPIKE, THROUGH];
    let s = run_fx(stopped_short(), &quotes);
    assert_eq!(
        fills(&s),
        [(Side::Sell, 150.000, 1000.0), (Side::Buy, 150.400, 1000.0)]
    );
    approx(s.equity()[1].equity, 999_880.0);
    approx(s.equity()[2].equity, 999_600.0);
    let x = intrabar_of(&s, &quotes);
    approx(x.ranges[1].low, 999_700.0);
    approx(x.ranges[1].high, 1_000_080.0);
    approx(x.ranges[2].low, 999_600.0);
    approx(x.ranges[2].high, 1_000_100.0);
    // The fall from the peak inside bars is 1 000 100 - 999 600 = 500, against 1 000 130 - 999 580
    // = 550 on the bid, and the leverage peaks at the stop, 1000 * 150.400 / 999 600.
    let drawdown = td_engine::risk::trailing_drawdown(1_000_000.0, &x.ranges).unwrap();
    approx(drawdown.amount, 500.0);
    approx(x.peak_leverage.unwrap(), 150_400.0 / 999_600.0);

    // The limited short. The ask opens bar 2 at 150.100, 999 880 + 1000 * (150.120 - 150.100) =
    // 999 900, and the limit fills at 149.950 as a maker, so after the ask's high of 150.400:
    // 999 900 - 1000 * (150.400 - 150.100) = 999 600, then 1 000 050 flat. The bid's high of
    // 150.250 would give 999 730.
    let quotes = [DECIDE, SPIKE, DIP];
    let s = run_fx(limited_short(), &quotes);
    assert_eq!(
        fills(&s),
        [(Side::Sell, 150.000, 1000.0), (Side::Buy, 149.950, 1000.0)]
    );
    let x = intrabar_of(&s, &quotes);
    approx(x.ranges[2].low, 999_600.0);
    approx(x.ranges[2].high, 1_000_050.0);
}

#[test]
fn an_fx_shorts_excursions_and_open_mark_are_on_the_ask() {
    // The stopped short, still open after bar 1. Against its entry of 150.000, the ask's high of
    // 150.300 puts it 1000 * 0.300 = 300 down and the ask's low of 149.920 80 up; the bid would
    // say 200 and 100. Open at the end, it is marked at the ask close of 150.120: -120 yen, an R of
    // -120 / (1000 * (150.400 - 150.000)) = -0.3, where the bid close would say -100 and -0.25.
    let quotes = [DECIDE, SPIKE];
    let trades = trades_of(&run_fx(stopped_short(), &quotes), &quotes);
    let t = &trades[0];
    assert!(t.open);
    approx(t.mae.unwrap(), 300.0);
    approx(t.mfe.unwrap(), 80.0);
    approx(t.exit_price, 150.120);
    approx(t.net_pnl, -120.0);
    approx(t.stop.unwrap(), 150.400);
    approx(t.r_multiple.unwrap(), -0.3);

    // Stopped on bar 2, which opens at ask 149.900, 100 up (150 on the bid); the stop fills at
    // 150.400, 400 down. A closed trade's R reads only its fills and its stop's trigger, the price
    // the stop fills at on its own side: -400 / 400 = -1.
    let quotes = [DECIDE, SPIKE, THROUGH];
    let trades = trades_of(&run_fx(stopped_short(), &quotes), &quotes);
    let t = &trades[0];
    assert!(!t.open);
    approx(t.mfe.unwrap(), 100.0);
    approx(t.mae.unwrap(), 400.0);
    approx(t.r_multiple.unwrap(), -1.0);

    // Closed on bar 2 by the buy limit at 149.950: before a maker fill the ask reached its high of
    // 150.400, 400 down, where the bid's high of 150.250 would say 250.
    let quotes = [DECIDE, SPIKE, DIP];
    let trades = trades_of(&run_fx(limited_short(), &quotes), &quotes);
    let t = &trades[0];
    assert!(!t.open);
    approx(t.mae.unwrap(), 400.0);
    approx(t.mfe.unwrap(), 80.0);

    // Added to at bar 2's open by a sell of 1000 more at the bid open, 149.800. The 1000 short
    // before the add counts on the ask, one spread at the open (0.100) above the add's price:
    // 149.900, 1000 * (150.000 - 149.900) = 100 up. The add's own price held to the ask's range,
    // whose low is 149.850, would count 150, and the bid would count 200. After the add, short 2000
    // from 149.900, the ask's low is only 2000 * (149.900 - 149.850) = 100 up.
    let quotes = [DECIDE, SPIKE, WIDE];
    let trades = trades_of(&run_fx(plan_add(), &quotes), &quotes);
    let t = &trades[0];
    assert_eq!(t.qty, 2000.0);
    approx(t.entry_price, 149.900);
    approx(t.mfe.unwrap(), 100.0);

    // The run's equity inside bar 2 counts the add the same way. Bar 1 closed at 999 880, marked
    // at the ask close of 150.120 (see above); the ask opens bar 2 at 149.900: 999 880 + 1000 *
    // 0.220 = 1 000 100. At the add the 1000 short is valued at 149.900 too, 1 000 100, not at the
    // add's price of 149.800, which would give 1 000 200. Short 2000 after it, marked at the ask
    // close of 149.950, equity is 999 900; the ask's low of 149.850 gives 1 000 100 and its high of
    // 150.050 999 700.
    let s = run_fx(plan_add(), &quotes);
    let x = intrabar_of(&s, &quotes);
    approx(s.equity()[2].equity, 999_900.0);
    approx(x.ranges[2].high, 1_000_100.0);
    approx(x.ranges[2].low, 999_700.0);
}

#[test]
fn an_adds_shifted_price_is_held_within_the_ask_range() {
    // Short 1000 from bar 1's bid open of 150.000. Bar 2 opens with the ask 0.100 above the bid,
    // and the spread narrows to 0.010 at the high, where a sell limit adds 1000 at 150.140. One
    // spread at the open above the add's price is 150.240, past the ask's high of 150.160, which
    // is as high as the ask went: the 1000 short before the add is valued there, at
    // 1 000 000 - 1000 * 0.160 = 999 840, not at 999 760.
    let narrowing = (
        [150.000, 150.150, 149.950, 150.050],
        [150.100, 150.160, 149.960, 150.060],
    );
    let quotes = [DECIDE, SPIKE, narrowing];
    let plan = vec![
        (0, Send::Plain(market(Side::Sell, 1000.0))),
        (1, Send::Plain(limit(Side::Sell, 1000.0, 150.140))),
    ];
    let s = run_fx(plan, &quotes);
    assert_eq!(s.fills().len(), 2);
    approx(s.fills()[1].price, 150.140);
    let x = intrabar_of(&s, &quotes);
    // After the add, short 2000 from 150.070, the ask's high of 150.160 is 2000 * 0.090 = 180
    // down: 999 820, the lowest the equity went.
    approx(x.ranges[2].low, 999_820.0);

    // A sell limit at 150.300 that bar 2 opens through fills between the bars, before the candle
    // began, so its shifted price of 150.320 stands below the ask's low of 150.370: on the way up
    // to the open the 1000 short was worth 1 000 000 - 1000 * 0.320 = 999 680, the highest the
    // equity was in bar 2 (the ask open of 150.420 gives 999 580).
    let gap = (
        [150.400, 150.410, 150.350, 150.380],
        [150.420, 150.430, 150.370, 150.400],
    );
    let quotes = [DECIDE, SPIKE, gap];
    let plan = vec![
        (0, Send::Plain(market(Side::Sell, 1000.0))),
        (1, Send::Plain(limit(Side::Sell, 1000.0, 150.300))),
    ];
    let s = run_fx(plan, &quotes);
    approx(s.fills()[1].price, 150.300);
    approx(intrabar_of(&s, &quotes).ranges[2].high, 999_680.0);
}

/// A short of 1000 sold at bar 1's bid open and 1000 more sold at bar 2's.
fn plan_add() -> Vec<(usize, Send)> {
    vec![
        (0, Send::Plain(market(Side::Sell, 1000.0))),
        (1, Send::Plain(market(Side::Sell, 1000.0))),
    ]
}

#[test]
fn a_run_values_its_fx_shorts_on_the_ask() {
    // Bids falling 0.010 a minute, with the ask 0.020 above the bid's open, low and close and
    // 0.050 above its high. The moving average cross goes short and is still short at the end:
    // the run's equity extremes are those its own record gives with the asks, not with the bid
    // alone, and its open trade is marked at the last ask close.
    let n = 12;
    let bids: Vec<Candle> = (0..n)
        .map(|i| {
            let p = 150.0 - 0.01 * i as f64;
            candle(i, [p, p + 0.005, p - 0.005, p + 0.002])
        })
        .collect();
    let asks: Vec<Candle> = bids
        .iter()
        .map(|b| Candle {
            open: b.open + 0.02,
            high: b.high + 0.05,
            low: b.low + 0.02,
            close: b.close + 0.02,
            ..*b
        })
        .collect();
    let config = td_engine::BacktestConfig {
        params: td_engine::Params::new()
            .with("fast", td_engine::ParamValue::Int(2))
            .with("slow", td_engine::ParamValue::Int(4))
            .with("allow_short", td_engine::ParamValue::Bool(true)),
        ..fx_backtest(n as usize)
    };
    let result = td_engine::run(&config, &bids, &asks, &[]).unwrap();
    let extremes = |asks: &[Option<Candle>]| {
        td_engine::risk::intrabar(
            1_000_000.0,
            &bids,
            asks,
            &result.equity,
            &result.fills,
            &[],
            td_core::AccountCurrency::Quote,
        )
        .ranges
    };
    let quoted: Vec<Option<Candle>> = asks.iter().copied().map(Some).collect();
    assert_eq!(result.equity_extremes, extremes(&quoted));
    assert_ne!(result.equity_extremes, extremes(&[]));
    let last = result.trades.last().unwrap();
    assert!(last.open);
    assert_eq!(last.side, td_engine::TradeSide::Short);
    approx(last.exit_price, asks[asks.len() - 1].close);
}
