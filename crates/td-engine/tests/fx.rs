//! A market with a bid and an ask (FX): each order sees the candle of its own side, open positions
//! are marked at the side that closes them, and bars in closed hours are refused. Every number is
//! worked by hand in the comments.

mod common;

use common::*;
use td_core::{
    Candle, FeeSchedule, Instrument, InstrumentId, InstrumentKind, Millis, OrderRequest, OrderType,
    Side, Timeframe, Venue,
};
use td_engine::{Context, EngineError, Session, SessionConfig, Strategy};

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
    // The bid high, 150.550, crosses the 150.500 stop, but the ask high, 150.490, does not. The
    // quote is crossed at the high, which real data never is; it isolates which side the stop
    // reads. A buy stop reads the ask, so it stays open.
    let s = run_fx(
        vec![(0, Send::WithStop(market(Side::Sell, 1000.0), 150.500))],
        &[
            DECIDE,
            (
                [150.000, 150.550, 149.900, 150.100],
                [150.020, 150.490, 149.920, 150.120],
            ),
        ],
    );
    assert_eq!(fills(&s), [(Side::Sell, 150.000, 1000.0)]);
    assert_eq!(s.open_orders().len(), 1);
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

    // The ask low, 149.490, crosses the stop but the bid low, 149.510, does not: crossed at the
    // low, as above, to isolate the side. The sell stop stays open.
    let s = run_fx(
        vec![(0, Send::WithStop(market(Side::Buy, 1000.0), 149.500))],
        &[
            DECIDE,
            (
                [150.000, 150.100, 149.510, 149.800],
                [150.020, 150.120, 149.490, 149.820],
            ),
        ],
    );
    assert_eq!(fills(&s), [(Side::Buy, 150.020, 1000.0)]);
    assert_eq!(s.open_orders().len(), 1);
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
    let s = run_fx_with(
        vec![
            (0, Send::Plain(market(Side::Buy, 1000.0))),
            (1, Send::Plain(market(Side::Sell, 1000.0))),
        ],
        &[
            DECIDE,
            (
                [150.000, 150.100, 149.900, 150.050],
                [150.020, 150.120, 149.920, 150.070],
            ),
            (
                [150.100, 150.200, 150.000, 150.150],
                [150.120, 150.220, 150.020, 150.170],
            ),
        ],
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
    let trades = td_engine::build_trades(s.fills(), s.bars(), s.bar_funding());
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
