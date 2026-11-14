//! The account currency: a dollar account trading USD/JPY holds and reports every amount in
//! dollars, each yen amount converted at the price it arises at, and FX trades are also counted in
//! pips. Every number is worked by hand in the comments.

mod common;

use common::*;
use td_core::{
    AccountCurrency, Candle, FeeSchedule, Instrument, InstrumentId, InstrumentKind, OrderRequest,
    OrderType, Side, Timeframe, Venue,
};
use td_engine::{Context, EngineError, Session, SessionConfig, Strategy};

/// `base`/`quote` FX: tick 0.001, lot 1, no fees.
fn fx(base: &str, quote: &str) -> Instrument {
    Instrument {
        id: InstrumentId {
            venue: Venue::Binance,
            symbol: format!("{base}{quote}"),
            kind: InstrumentKind::Fx,
        },
        base: base.into(),
        quote: quote.into(),
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

fn usdjpy() -> Instrument {
    fx("USD", "JPY")
}

/// A commission of 0.003 yen per unit, no rate.
const PER_UNIT: FeeSchedule = FeeSchedule {
    maker: 0.0,
    taker: 0.0,
    per_unit: 0.003,
};

/// A dollar account of 100 000, no slippage, up to 1x.
fn dollars() -> SessionConfig {
    SessionConfig {
        starting_cash: 100_000.0,
        slippage_bps: 0.0,
        fees: None,
        max_leverage: 1.0,
        account_currency: AccountCurrency::Base,
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

/// A bid candle and its ask candle.
type Quote = ([f64; 4], [f64; 4]);

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

fn market(side: Side, qty: f64) -> Send {
    Send::Plain(req(&usdjpy(), side, qty, OrderType::Market))
}

/// Runs `strategy` on `inst` over `(bid, ask)` one-minute bars.
fn run_on(
    inst: Instrument,
    strategy: Box<dyn Strategy>,
    quotes: &[Quote],
    config: SessionConfig,
) -> Session {
    let mut s = Session::new(strategy, inst, Timeframe::M1, config).unwrap();
    for (i, (bid, ask)) in quotes.iter().enumerate() {
        let i = i as i64;
        s.on_bar_with_ask(candle(i, *bid), Some(candle(i, *ask)), &[])
            .unwrap();
    }
    s
}

fn run(plan: Vec<(usize, Send)>, quotes: &[Quote], config: SessionConfig) -> Session {
    run_on(usdjpy(), Box::new(Plan(plan)), quotes, config)
}

fn asks_of(quotes: &[Quote]) -> Vec<Option<Candle>> {
    quotes
        .iter()
        .enumerate()
        .map(|(i, (_, ask))| Some(candle(i as i64, *ask)))
        .collect()
}

/// The trades of a session with their stops, counted in pips on USD/JPY.
fn trades_of(s: &Session, quotes: &[Quote]) -> Vec<td_engine::Trade> {
    let account = s.portfolio().account();
    let mut trades = td_engine::build_trades(
        s.fills(),
        s.bars(),
        &asks_of(quotes),
        s.bar_funding(),
        account,
        usdjpy().pip_size(),
    );
    td_engine::attach_stops(&mut trades, s.trade_stops(), account);
    trades
}

/// A flat bar: bid `p` all through, ask 0.020 above it.
fn flat(p: f64) -> Quote {
    ([p, p, p, p], [p + 0.02, p + 0.02, p + 0.02, p + 0.02])
}

/// The ask 0.020 above each of `bid`'s prices.
fn spread(bid: [f64; 4]) -> Quote {
    (bid, bid.map(|p| p + 0.02))
}

/// The short round trip: sell 7400 at bar 0's close, buy it back at bar 1's close. It sells at bar
/// 1's bid open, 150.000, and buys at bar 2's ask open, 148.000.
fn short_round_trip() -> (Vec<(usize, Send)>, Vec<Quote>) {
    let plan = vec![
        (0, market(Side::Sell, 7400.0)),
        (1, market(Side::Buy, 7400.0)),
    ];
    let quotes = vec![
        flat(150.000),
        spread([150.000, 150.100, 149.000, 149.000]),
        spread([147.980, 148.100, 147.900, 148.000]),
    ];
    (plan, quotes)
}

#[test]
fn a_usdjpy_short_on_a_dollar_account_is_counted_in_dollars_at_each_fills_price() {
    // 7400 sold at 150.000 and bought back at 148.000, 0.003 yen a unit each way.
    // Entry fee: 7400 * 0.003 = 22.2 yen, at 150.000 = 0.148 dollars.
    // Exit fee: 22.2 yen, at 148.000 = 0.15 dollars. Fees 0.298 dollars.
    // Gross: 7400 * (150.000 - 148.000) = 14 800 yen, realised at the closing fill's 148.000 =
    // 100 dollars. Net: 100 - 0.298 = 99.702 dollars; cash and equity 100 099.702.
    let (plan, quotes) = short_round_trip();
    let config = SessionConfig {
        fees: Some(PER_UNIT),
        ..dollars()
    };
    let s = run(plan, &quotes, config);
    let fees: Vec<f64> = s.fills().iter().map(|f| f.fee).collect();
    assert_eq!(s.fills()[0].price, 150.000);
    assert_eq!(s.fills()[1].price, 148.000);
    approx(fees[0], 0.148);
    approx(fees[1], 0.15);
    approx(s.portfolio().fees_paid(), 0.298);
    approx(s.portfolio().realised_pnl(), 100.0);
    approx(s.portfolio().cash(), 100_099.702);
    approx(s.equity()[2].equity, 100_099.702);
    let t = &trades_of(&s, &quotes)[0];
    approx(t.gross_pnl, 100.0);
    approx(t.fees, 0.298);
    approx(t.net_pnl, 99.702);
}

#[test]
fn the_same_short_on_a_yen_account_differs_by_exactly_the_conversion() {
    // On 15 000 000 yen (100 000 dollars at 150) the yen figures stand unconverted: fees 22.2 +
    // 22.2 = 44.4 yen, gross 14 800, net 14 755.6. Each is its dollar figure times the price it was
    // converted at: 0.148 * 150 = 22.2, 0.15 * 148 = 22.2, 100 * 148 = 14 800.
    let (plan, quotes) = short_round_trip();
    let yen = SessionConfig {
        starting_cash: 15_000_000.0,
        fees: Some(PER_UNIT),
        account_currency: AccountCurrency::Quote,
        ..dollars()
    };
    let s = run(plan.clone(), &quotes, yen);
    let t = &trades_of(&s, &quotes)[0];
    approx(s.fills()[0].fee, 22.2);
    approx(s.fills()[1].fee, 22.2);
    approx(t.gross_pnl, 14_800.0);
    approx(t.net_pnl, 14_755.6);
    // Cash this large carries rounding in the ninth decimal; to the micro-yen it is exact.
    assert!((s.portfolio().cash() - 15_014_755.6).abs() < 1e-6);

    let d = run(
        plan,
        &quotes,
        SessionConfig {
            fees: Some(PER_UNIT),
            ..dollars()
        },
    );
    let dt = &trades_of(&d, &quotes)[0];
    approx(d.fills()[0].fee * 150.0, s.fills()[0].fee);
    approx(d.fills()[1].fee * 148.0, s.fills()[1].fee);
    approx(dt.gross_pnl * 148.0, t.gross_pnl);
}

#[test]
fn open_equity_is_converted_at_the_mark_on_the_closing_side() {
    // A long of 7500 bought at bar 1's ask open, 148.000, marked at the bid close, 150.000:
    // 7500 * (150.000 - 148.000) = 15 000 yen, at 150.000 = 100 dollars; equity 100 100. Marked at
    // the ask close instead, 7500 * 2.020 / 150.020 = 100.99 dollars.
    let quotes = [
        flat(147.980),
        (
            [147.980, 150.100, 147.900, 150.000],
            [148.000, 150.120, 147.920, 150.020],
        ),
    ];
    let s = run(vec![(0, market(Side::Buy, 7500.0))], &quotes, dollars());
    assert_eq!(s.fills()[0].price, 148.000);
    approx(s.equity()[1].equity, 100_100.0);
    approx(s.portfolio().unrealised_pnl(), 100.0);
    // The open trade is marked there too.
    let t = &trades_of(&s, &quotes)[0];
    assert!(t.open);
    approx(t.net_pnl, 100.0);

    // A short of 7500 sold at bar 1's bid open, 152.000, marked at the ask close, 150.000:
    // 7500 * (152.000 - 150.000) = 15 000 yen, at 150.000 = 100 dollars; equity 100 100. Marked at
    // the bid close instead, 7500 * 2.020 / 149.980 = 101.01 dollars.
    let quotes = [
        flat(152.000),
        (
            [152.000, 152.100, 149.900, 149.980],
            [152.020, 152.120, 149.920, 150.000],
        ),
    ];
    let s = run(vec![(0, market(Side::Sell, 7500.0))], &quotes, dollars());
    assert_eq!(s.fills()[0].price, 152.000);
    approx(s.equity()[1].equity, 100_100.0);
    let t = &trades_of(&s, &quotes)[0];
    assert!(t.open);
    approx(t.exit_price, 150.000);
    approx(t.net_pnl, 100.0);
}

#[test]
fn an_open_shorts_intrabar_range_on_a_dollar_account_takes_each_extreme_at_its_own_price() {
    // A short of 7600 sold at bar 1's bid open, 150.000. Its ask rises to 152.000 and falls to
    // 148.000 inside the bar and closes at 150.000.
    // At the ask high: 7600 * (150.000 - 152.000) = -15 200 yen, at 152.000 = -100 dollars, so
    // the low is 99 900.
    // At the ask low: 7600 * (150.000 - 148.000) = 15 200 yen, at 148.000 = 102.7027 dollars, so
    // the high is 100 102.7027.
    // At the close, 150.000, the short is flat on its entry: equity 100 000. Both extremes
    // converted at that close would be 15 200 / 150 = 101.33 either way.
    let quotes = [
        flat(150.000),
        (
            [150.000, 151.980, 147.980, 149.980],
            [150.020, 152.000, 148.000, 150.000],
        ),
    ];
    let s = run(vec![(0, market(Side::Sell, 7600.0))], &quotes, dollars());
    approx(s.equity()[1].equity, 100_000.0);
    let x = td_engine::risk::intrabar(
        100_000.0,
        s.bars(),
        &asks_of(&quotes),
        s.equity(),
        s.fills(),
        s.bar_funding(),
        AccountCurrency::Base,
    );
    approx(x.ranges[1].low, 99_900.0);
    approx(x.ranges[1].high, 100_000.0 + 15_200.0 / 148.0);
    // The leverage peaks where the equity is lowest: 7600 dollars of notional on 99 900.
    approx(x.peak_leverage.unwrap(), 7600.0 / 99_900.0);
}

#[test]
fn a_dollar_accounts_intrabar_range_follows_the_average_entry_through_an_add_a_cut_and_a_flip() {
    // On 100 000 dollars at 0.003 yen a unit, each order filling at the next bar's open:
    // - bar 1 buys 50 000 at the ask open, 150.500: long 50 000 at 150.500;
    // - bar 2 buys 30 000 at the ask open, 151.300: long 80 000 at (50 000 * 150.500 + 30 000 *
    //   151.300) / 80 000 = 150.800;
    // - bar 3 sells 40 000 at the bid open, 152.200, realising 40 000 * (152.200 - 150.800) =
    //   56 000 yen: long 40 000, still at 150.800;
    // - bar 4 sells 70 000 at the bid open, 151.400: 40 000 close the long, realising 40 000 *
    //   (151.400 - 150.800) = 24 000 yen, and 30 000 open a short at 151.400, still open after
    //   bar 5.
    let quotes = [
        flat(150.000),
        spread([150.480, 151.200, 149.600, 151.000]),
        spread([151.280, 152.400, 150.100, 152.000]),
        spread([152.200, 153.000, 151.000, 151.500]),
        spread([151.400, 152.900, 149.200, 150.000]),
        spread([149.800, 151.600, 147.500, 148.000]),
    ];
    let plan = vec![
        (0, market(Side::Buy, 50_000.0)),
        (1, market(Side::Buy, 30_000.0)),
        (2, market(Side::Sell, 40_000.0)),
        (3, market(Side::Sell, 70_000.0)),
    ];
    let config = SessionConfig {
        fees: Some(PER_UNIT),
        ..dollars()
    };
    let s = run(plan, &quotes, config);
    // Each fill as worked above: its bar, the signed quantity, the price, the yen it realises,
    // and the position and average entry after it.
    let steps = [
        (1, 50_000.0, 150.500, 0.0, 50_000.0, 150.500),
        (2, 30_000.0, 151.300, 0.0, 80_000.0, 150.800),
        (3, -40_000.0, 152.200, 56_000.0, 40_000.0, 150.800),
        (4, -70_000.0, 151.400, 24_000.0, -30_000.0, 151.400),
    ];
    let filled: Vec<(usize, f64, f64)> = s
        .fills()
        .iter()
        .map(|f| ((f.time / MIN) as usize, f.side.sign() * f.qty, f.price))
        .collect();
    let planned: Vec<(usize, f64, f64)> =
        steps.iter().map(|step| (step.0, step.1, step.2)).collect();
    assert_eq!(filled, planned);

    // The replay: the equity at a price `p` is the cash plus the position's yen P&L against its
    // average entry, converted at `p`. Each bar is valued at the points the range is taken over:
    // the open with the position held into the bar; each fill's price with the position before
    // it, after which the fill's fee and realised yen, converted at its price, go into the cash;
    // then the low, high and close with the position after the fills. A short is valued on the
    // ask, anything else on the bid.
    let equity_at = |cash: f64, pos: f64, avg: f64, p: f64| {
        if pos == 0.0 {
            cash
        } else {
            cash + pos * (p - avg) / p
        }
    };
    let (mut cash, mut pos, mut avg) = (100_000.0, 0.0, 0.0);
    let mut next = 0;
    let mut replayed = Vec::new();
    for (k, (bid, ask)) in quotes.iter().enumerate() {
        let closing = |pos: f64| if pos < 0.0 { *ask } else { *bid };
        let mut points = vec![equity_at(cash, pos, avg, closing(pos)[0])];
        while let Some(&(_, qty, price, realised, after, after_avg)) =
            steps.get(next).filter(|step| step.0 == k)
        {
            next += 1;
            points.push(equity_at(cash, pos, avg, price));
            cash += (realised - f64::abs(qty) * 0.003) / price;
            (pos, avg) = (after, after_avg);
        }
        let [_, high, low, close] = closing(pos);
        points.extend([low, high, close].map(|p| equity_at(cash, pos, avg, p)));
        // The replay's close is the session's equity at the close.
        approx(*points.last().unwrap(), s.equity()[k].equity);
        let low = points.iter().copied().fold(f64::INFINITY, f64::min);
        let high = points.iter().copied().fold(f64::NEG_INFINITY, f64::max);
        replayed.push((low, high));
    }
    let x = td_engine::risk::intrabar(
        100_000.0,
        s.bars(),
        &asks_of(&quotes),
        s.equity(),
        s.fills(),
        s.bar_funding(),
        AccountCurrency::Base,
    );
    assert_eq!(x.ranges.len(), replayed.len());
    for (k, (range, &(low, high))) in x.ranges.iter().zip(&replayed).enumerate() {
        assert!(
            (range.low - low).abs() < 1e-9 && (range.high - high).abs() < 1e-9,
            "bar {k}: the engine's range is {range:?}, the replay's [{low}, {high}]"
        );
    }

    // The flip's fee, 70 000 * 0.003 = 210 yen at 151.400, splits 40:30 between the long it
    // closes and the short it opens. The short is open, marked at the ask close, 148.020: its
    // gross P&L is the open position's unrealised P&L, 30 000 * (151.400 - 148.020) = 101 400
    // yen at 148.020.
    let trades = trades_of(&s, &quotes);
    assert_eq!(trades.len(), 2);
    let (long, short) = (&trades[0], &trades[1]);
    assert!(!long.open && short.open);
    approx(short.fees, 90.0 / 151.400);
    let unrealised = s.portfolio().unrealised_pnl();
    approx(unrealised, 101_400.0 / 148.020);
    approx(short.gross_pnl, unrealised);
    approx(
        s.portfolio().realised_pnl(),
        56_000.0 / 152.200 + 24_000.0 / 151.400,
    );
    // End equity less the starting cash is the closed long's net P&L plus the open position's
    // unrealised P&L, less the open short's share of the fee: the sum of both trades' net P&L.
    // To 1e-9 dollars, against float noise of about 1e-11 on six-figure sums.
    let gain = s.equity().last().unwrap().equity - 100_000.0;
    approx(gain, long.net_pnl + unrealised - short.fees);
    approx(gain, long.net_pnl + short.net_pnl);
}

#[test]
fn a_stopped_trades_r_on_a_dollar_account_converts_the_risk_at_the_stop() {
    // A short of 7600 sold at bar 1's bid open, 150.000, with a buy stop at 152.000. The risk is
    // 7600 * (152.000 - 150.000) = 15 200 yen, converted at the stop, 152.000: 100 dollars.
    // Bar 2's ask opens at 151.000 and reaches 152.500, so the stop fills at max(151.000,
    // 152.000) = 152.000: a loss of 15 200 yen at 152.000 = 100 dollars, R = -100 / 100 = -1.
    // Converted at the entry instead, the risk would be 15 200 / 150 = 101.33 and R -0.987.
    let quotes = [
        flat(150.000),
        flat(150.000),
        (
            [150.980, 152.480, 150.900, 152.000],
            [151.000, 152.500, 150.920, 152.020],
        ),
    ];
    let order = req(&usdjpy(), Side::Sell, 7600.0, OrderType::Market);
    let s = run(
        vec![(0, Send::WithStop(order, 152.000))],
        &quotes,
        dollars(),
    );
    assert_eq!(s.fills()[1].price, 152.000);
    let t = &trades_of(&s, &quotes)[0];
    assert!(!t.open);
    approx(t.net_pnl, -100.0);
    approx(t.r_multiple.unwrap(), -1.0);

    // The same short closed by a market buy at bar 2's ask open instead, 148.000 (the stop never
    // reached): a gain of 15 200 yen at 148.000 = 102.7027 dollars, R = 102.7027 / 100 = 1.027.
    let quotes = [
        flat(150.000),
        flat(150.000),
        spread([147.980, 148.000, 147.900, 147.950]),
    ];
    let order = req(&usdjpy(), Side::Sell, 7600.0, OrderType::Market);
    let plan = vec![
        (0, Send::WithStop(order, 152.000)),
        (1, market(Side::Buy, 7600.0)),
    ];
    let s = run(plan, &quotes, dollars());
    let t = &trades_of(&s, &quotes)[0];
    approx(t.exit_price, 148.000);
    approx(t.net_pnl, 15_200.0 / 148.0);
    approx(t.r_multiple.unwrap(), 15_200.0 / 148.0 / 100.0);
}

#[test]
fn a_scaled_exit_on_a_dollar_account_converts_each_reducing_fill_at_its_own_price() {
    // A short of 7400 from 150.000 bought back in two halves: 3700 at bar 2's ask open, 148.000,
    // and 3700 at bar 3's ask open, 146.000.
    // First half: 3700 * 2 = 7400 yen at 148.000 = 50 dollars.
    // Second half: 3700 * 4 = 14 800 yen at 146.000 = 101.3699 dollars.
    // Gross 151.3699 dollars, the portfolio's realised P&L. Converting the whole 22 200 yen at the
    // last exit, 146.000, would say 152.05; at the average exit, 147.000, 151.02.
    let quotes = [
        flat(150.000),
        flat(150.000),
        spread([147.980, 148.000, 147.900, 147.950]),
        spread([145.980, 146.000, 145.900, 145.950]),
    ];
    let plan = vec![
        (0, market(Side::Sell, 7400.0)),
        (1, market(Side::Buy, 3700.0)),
        (2, market(Side::Buy, 3700.0)),
    ];
    let s = run(plan, &quotes, dollars());
    let gross = 7400.0 / 148.0 + 14_800.0 / 146.0;
    approx(s.portfolio().realised_pnl(), gross);
    let t = &trades_of(&s, &quotes)[0];
    approx(t.exit_price, 147.000);
    approx(t.gross_pnl, gross);
    approx(t.net_pnl, gross);
}

#[test]
fn a_base_account_is_refused_on_spot_and_on_a_perpetual() {
    // Spot pays for what it buys in the quote currency; a perpetual's margin and funding are in the
    // quote asset, and a coin-margined account would be an inverse contract, a different formula.
    for inst in [spot(), perp()] {
        let kind = inst.id.kind;
        let refused = Session::new(Box::new(Plan(vec![])), inst, Timeframe::M1, dollars());
        assert!(
            matches!(refused, Err(EngineError::InvalidConfig(_))),
            "{kind:?} accepted a base account"
        );
        // The reason is one sentence with single spaces.
        if let Err(EngineError::InvalidConfig(reason)) = refused {
            assert!(!reason.contains("  "), "{kind:?}: {reason:?}");
        }
    }
    // FX takes it, and every kind still takes a quote account.
    assert!(Session::new(Box::new(Plan(vec![])), usdjpy(), Timeframe::M1, dollars()).is_ok());
    let quote = SessionConfig {
        account_currency: AccountCurrency::Quote,
        ..dollars()
    };
    for inst in [spot(), perp(), usdjpy()] {
        assert!(Session::new(Box::new(Plan(vec![])), inst, Timeframe::M1, quote).is_ok());
    }
}

/// Records `ctx.to_account(0.12)` at bar 0's close.
struct ToAccount(std::sync::Arc<std::sync::Mutex<Option<f64>>>);

impl Strategy for ToAccount {
    fn on_bar(&mut self, ctx: &mut Context, _bar: &Candle) {
        *self.0.lock().unwrap() = Some(ctx.to_account(0.12));
    }
}

#[test]
fn to_account_converts_at_the_last_close_and_sizes_625_000_units() {
    // USD/JPY closes at 150.000. 0.12 yen (12 pips) is 0.12 / 150 = 0.0008 dollars. With 100 000
    // dollars, 0.5% risk is 500 dollars, and 500 / 0.0008 = 625 000 units.
    let seen = std::sync::Arc::default();
    run_on(
        usdjpy(),
        Box::new(ToAccount(std::sync::Arc::clone(&seen))),
        &[flat(150.000)],
        dollars(),
    );
    let per_unit = seen.lock().unwrap().unwrap();
    approx(per_unit, 0.0008);
    let qty = 0.005 * 100_000.0 / per_unit;
    assert!((qty - 625_000.0).abs() < 1e-6, "{qty}");

    // On a yen account the amount is returned as it is.
    let seen = std::sync::Arc::default();
    let yen = SessionConfig {
        account_currency: AccountCurrency::Quote,
        ..dollars()
    };
    run_on(
        usdjpy(),
        Box::new(ToAccount(std::sync::Arc::clone(&seen))),
        &[flat(150.000)],
        yen,
    );
    assert_eq!(seen.lock().unwrap().unwrap(), 0.12);
}

#[test]
fn donchian_sizes_its_risk_in_the_account_currency() {
    // Entry, exit and ATR over 2 bars, the stop 1 ATR away, 0.05% risk. Each bid bar has a range
    // of 0.125 and bar 2 rises 0.125 from bar 1's close, so the ATR is 0.125. Bar 2 closes at
    // 150.003, above bar 0's and bar 1's highs of 149.9405: a long breakout with its stop 0.125
    // below the close.
    // The risk is 0.0005 * 100 000 = 50 dollars; 0.125 yen is 0.125 / 150.003 dollars a unit at the
    // close, so the size is 50 * 150.003 / 0.125 = 60 001.2, rounded down to the lot: 60 001.
    // The cash cap is about 100 000 * 0.995 / 1 = 99 500 units, so the risk decides. Read as yen,
    // the size would be 50 / 0.125 = 400.
    let low = [149.878, 149.9405, 149.8155, 149.878];
    let quotes = [
        spread(low),
        spread(low),
        spread([149.878, 150.003, 149.878, 150.003]),
        flat(150.003),
    ];
    let params = td_engine::Params::new()
        .with("entry", td_engine::ParamValue::Int(2))
        .with("exit", td_engine::ParamValue::Int(2))
        .with("atr_period", td_engine::ParamValue::Int(2))
        .with("atr_stop", td_engine::ParamValue::Float(1.0))
        .with("risk", td_engine::ParamValue::Float(0.0005));
    let strategy = td_engine::build_strategy("donchian_breakout", &params).unwrap();
    let s = run_on(usdjpy(), strategy, &quotes, dollars());
    assert_eq!(s.fills().len(), 1);
    assert_eq!(s.fills()[0].side, Side::Buy);
    assert_eq!(s.fills()[0].qty, 60_001.0);
}

/// Runs `script` on `inst` with a dollar-style base account of 100 000 at 1x over one flat bar at
/// bid `p`, then a second to fill.
fn capped(inst: Instrument, p: f64, script: Box<Script>) -> Session {
    run_on(inst, script, &[flat(p), flat(p)], dollars())
}

#[test]
fn the_leverage_cap_compares_the_notional_in_the_account_currency() {
    // 100 000 dollars at 1x allows 100 000 dollars of USD/JPY, which is 100 000 units at any
    // price. A sell of 100 001 is refused and one of 100 000 accepted. Compared in yen against the
    // dollar equity, 100 000 units would be 15 000 000 and refused.
    let (script, log) = Script::new(vec![
        (0, req(&usdjpy(), Side::Sell, 100_001.0, OrderType::Market)),
        (0, req(&usdjpy(), Side::Sell, 100_000.0, OrderType::Market)),
    ]);
    let s = capped(usdjpy(), 150.000, script);
    let log = log.lock().unwrap();
    let err = log[0].as_ref().unwrap_err();
    assert!(
        err.reason.starts_with("over the leverage cap of 1x"),
        "{}",
        err.reason
    );
    assert!(log[1].is_ok());
    assert_eq!(s.fills()[0].qty, 100_000.0);

    // On a pair priced below 1 the quote amount is the smaller one: a euro account of 100 000 on
    // EUR/GBP at 0.850. A sell of 100 001 is worth 100 001 euros, over the cap, though its pound
    // notional, 100 001 * 0.850 = 85 000.85, would pass.
    let eurgbp = fx("EUR", "GBP");
    let (script, log) = Script::new(vec![
        (0, req(&eurgbp, Side::Sell, 100_001.0, OrderType::Market)),
        (0, req(&eurgbp, Side::Sell, 100_000.0, OrderType::Market)),
    ]);
    capped(eurgbp, 0.850, script);
    let log = log.lock().unwrap();
    assert!(log[0].is_err());
    assert!(log[1].is_ok());
}

#[test]
fn trades_count_pips_and_net_pips_and_the_report_averages_them() {
    // On one dollar account at 0.003 yen a unit:
    // The short round trip, 7400 from 150.000 to 148.000: (150.000 - 148.000) / 0.01 = 200 pips.
    // A pip on the 7400 closed is 7400 * 0.01 = 74 yen, at the exit's 148.000 = 0.5 dollars, so
    // its net 99.702 dollars is 99.702 / 0.5 = 199.404 net pips.
    // Then a long of 7500 bought at bar 3's ask open, 151.000, and sold at bar 4's bid open,
    // 150.000: (150.000 - 151.000) / 0.01 = -100 pips. Gross 7500 * -1 = -7500 yen at 150.000 =
    // -50 dollars; fees 22.5 yen at 151.000 and 22.5 at 150.000 = 0.149007 + 0.15; net -50.299007.
    // A pip on 7500 is 75 yen, at 150.000 0.5 dollars: -100.598013 net pips.
    // The averages: (200 - 100) / 2 = 50 pips, (199.404 - 100.598013) / 2 = 49.402993 net pips.
    let (mut plan, mut quotes) = short_round_trip();
    plan.push((2, market(Side::Buy, 7500.0)));
    plan.push((3, market(Side::Sell, 7500.0)));
    quotes.push(spread([150.980, 151.100, 150.900, 151.000]));
    quotes.push(spread([150.000, 150.100, 149.900, 150.000]));
    let config = SessionConfig {
        fees: Some(PER_UNIT),
        ..dollars()
    };
    let s = run(plan, &quotes, config);
    let trades = trades_of(&s, &quotes);
    assert_eq!(trades.len(), 2);
    let (short, long) = (&trades[0], &trades[1]);
    approx(short.pips.unwrap(), 200.0);
    approx(short.net_pips.unwrap(), 199.404);
    let long_net = -50.0 - 22.5 / 151.0 - 22.5 / 150.0;
    approx(long.net_pnl, long_net);
    approx(long.pips.unwrap(), -100.0);
    approx(long.net_pips.unwrap(), long_net / 0.5);

    let series = td_engine::metrics::EquitySeries::new(100_000.0, s.equity(), Timeframe::M1);
    let positions = vec![0.0; s.equity().len()];
    let report = td_engine::metrics::report(
        &series,
        &positions,
        &trades,
        s.fills(),
        0.0,
        AccountCurrency::Base,
    );
    approx(report.avg_trade_pips.unwrap(), 50.0);
    approx(
        report.avg_trade_net_pips.unwrap(),
        (199.404 + long_net / 0.5) / 2.0,
    );
}

#[test]
fn an_open_trades_pips_are_on_the_mark() {
    // The short of 7500 from 152.000 still open, marked at the ask close of 150.000:
    // (152.000 - 150.000) / 0.01 = 200 pips.
    let quotes = [
        flat(152.000),
        (
            [152.000, 152.100, 149.900, 149.980],
            [152.020, 152.120, 149.920, 150.000],
        ),
    ];
    let s = run(vec![(0, market(Side::Sell, 7500.0))], &quotes, dollars());
    let t = &trades_of(&s, &quotes)[0];
    assert!(t.open);
    approx(t.pips.unwrap(), 200.0);
    approx(t.net_pips.unwrap(), 200.0);
}

/// A perpetual backtest of the SMA cross on `bars`.
fn crypto_backtest(n: usize) -> td_engine::BacktestConfig {
    td_engine::BacktestConfig {
        strategy: "sma_cross".into(),
        params: td_engine::Params::new()
            .with("fast", td_engine::ParamValue::Int(2))
            .with("slow", td_engine::ParamValue::Int(4)),
        instrument: perp(),
        timeframe: Timeframe::M1,
        from: 0,
        to: n as i64 * MIN,
        starting_cash: 10_000.0,
        slippage_bps: 0.0,
        fee_override: None,
        max_leverage: 1.0,
        warmup_from: None,
        close_at_end: false,
        account_currency: AccountCurrency::Quote,
        references: Vec::new(),
    }
}

/// Bars rising, then falling, then rising, so the cross trades.
fn zigzag(n: usize) -> Vec<Candle> {
    (0..n)
        .map(|i| {
            let p = 100.0 + [0.0, 1.0, 2.0, 3.0, 2.0, 1.0, 0.0, 1.0, 2.0, 3.0][i % 10];
            bar(i as i64, p, p + 0.5, p - 0.5, p)
        })
        .collect()
}

#[test]
fn crypto_has_no_pips_and_saves_no_pip_or_account_keys() {
    let bars = zigzag(30);
    let result = td_engine::run(&crypto_backtest(30), &bars, &[], &[]).unwrap();
    assert!(!result.trades.is_empty());
    assert!(
        result
            .trades
            .iter()
            .all(|t| t.pips.is_none() && t.net_pips.is_none())
    );
    assert_eq!(result.metrics.avg_trade_pips, None);
    assert_eq!(result.metrics.avg_trade_net_pips, None);
    let json = serde_json::to_string(&result).unwrap();
    for key in ["pips", "account_currency"] {
        assert!(!json.contains(key), "{key} in the saved crypto run");
    }
}

#[test]
fn the_account_currency_is_saved_only_when_base_and_leaves_the_data_fingerprint_alone() {
    // A quote account leaves the key out, so saved runs and their JSON are as before; loading one
    // without the key gives a quote account. A base account saves "base".
    let quote = crypto_backtest(30);
    let json = serde_json::to_value(&quote).unwrap();
    assert!(json.get("account_currency").is_none());
    let loaded: td_engine::BacktestConfig = serde_json::from_value(json).unwrap();
    assert_eq!(loaded.account_currency, AccountCurrency::Quote);

    let base = td_engine::BacktestConfig {
        account_currency: AccountCurrency::Base,
        ..quote.clone()
    };
    let json = serde_json::to_value(&base).unwrap();
    assert_eq!(json["account_currency"], "base");
    let loaded: td_engine::BacktestConfig = serde_json::from_value(json).unwrap();
    assert_eq!(loaded.account_currency, AccountCurrency::Base);

    // The run's fingerprint covers its data only (bars, asks, funding), which a rerun compares to
    // tell whether the data changed. It is pinned for these bars, and the account currency, a
    // setting, does not move it.
    let bars = zigzag(30);
    let pinned = td_engine::config_fingerprint(&quote, &bars, &[], &[]).unwrap();
    assert_eq!(
        pinned,
        "15348cf9992f6a6cb190b8e743f87b414e18f7604c69334b9ac773881a9599f1"
    );
    assert_eq!(
        td_engine::config_fingerprint(&base, &bars, &[], &[]).unwrap(),
        pinned
    );
}

#[test]
fn a_dollar_backtest_runs_end_to_end_and_reports_pips() {
    // The SMA cross on USD/JPY on a dollar account: the run starts with 100 000 dollars, its
    // trades carry pips, and the report averages them.
    let n = 30;
    let bids: Vec<Candle> = zigzag(n)
        .into_iter()
        .map(|b| Candle {
            open: b.open + 50.0,
            high: b.high + 50.0,
            low: b.low + 50.0,
            close: b.close + 50.0,
            ..b
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
    let config = td_engine::BacktestConfig {
        instrument: usdjpy(),
        starting_cash: 100_000.0,
        account_currency: AccountCurrency::Base,
        ..crypto_backtest(n)
    };
    let result = td_engine::run(&config, &bids, &asks, &[]).unwrap();
    assert!(!result.trades.is_empty());
    assert!(result.trades.iter().all(|t| t.pips.is_some()));
    assert!(result.metrics.avg_trade_pips.is_some());
    // Each trade's pips are its price move over 0.01.
    for t in &result.trades {
        let sign = if t.side == td_engine::TradeSide::Long {
            1.0
        } else {
            -1.0
        };
        approx(
            t.pips.unwrap(),
            sign * (t.exit_price - t.entry_price) / 0.01,
        );
    }
}

#[test]
fn a_dollar_backtest_converts_the_benchmark_excursions_return_and_turnover_at_their_prices() {
    // The SMA cross with fast 1 and slow 2 is long while a close is above the one before and flat
    // otherwise. On 100 000 dollars at 0.003 yen a unit, with half the equity: bar 1 closes up,
    // so it buys 100 000 * 0.5 / (150.500 / 150.500) = 50 000 (under its cap of 100 000 * 0.995
    // = 99 500) at bar 2's ask open, 150.500; bar 4 closes down, so it sells them at bar 5's bid
    // open, 151.200; bars 5 and 6 do not close up.
    let quotes: Vec<Quote> = [
        [150.000, 150.000, 150.000, 150.000],
        [150.000, 150.600, 149.900, 150.500],
        [150.480, 151.000, 149.800, 150.900],
        [150.900, 152.000, 150.500, 151.500],
        [151.500, 151.600, 150.600, 151.000],
        [151.200, 151.300, 150.900, 151.000],
        [150.900, 150.900, 150.900, 150.900],
    ]
    .into_iter()
    .map(spread)
    .collect();
    let bids: Vec<Candle> = quotes
        .iter()
        .enumerate()
        .map(|(i, (bid, _))| candle(i as i64, *bid))
        .collect();
    let asks: Vec<Candle> = asks_of(&quotes).into_iter().flatten().collect();
    let config = td_engine::BacktestConfig {
        params: td_engine::Params::new()
            .with("fast", td_engine::ParamValue::Int(1))
            .with("slow", td_engine::ParamValue::Int(2))
            .with("equity_fraction", td_engine::ParamValue::Float(0.5)),
        instrument: usdjpy(),
        starting_cash: 100_000.0,
        fee_override: Some(PER_UNIT),
        account_currency: AccountCurrency::Base,
        ..crypto_backtest(quotes.len())
    };
    let result = td_engine::run(&config, &bids, &asks, &[]).unwrap();

    // The trade: 50 000 from 150.500 to 151.200. Gross 50 000 * 0.700 = 35 000 yen at the exit's
    // 151.200; fees 50 000 * 0.003 = 150 yen at 150.500 and 150 yen at 151.200.
    assert_eq!(result.trades.len(), 1);
    let t = &result.trades[0];
    assert!(!t.open);
    assert_eq!(t.qty, 50_000.0);
    approx(t.entry_price, 150.500);
    approx(t.exit_price, 151.200);
    let entry_fee = 150.0 / 150.500;
    let net = 35_000.0 / 151.200 - entry_fee - 150.0 / 151.200;
    approx(t.net_pnl, net);
    // The notional is the 50 000 dollars bought (50 000 * 150.500 yen at 150.500); over 7 525 000
    // yen instead, the return would be 150.5 times smaller.
    approx(t.return_on_notional, net / 50_000.0);
    // The long is valued on the bid. Its worst price is bar 2's low after the entry, 149.800:
    // 50 000 * -0.700 = -35 000 yen at 149.800. Its best is bar 3's high, 152.000: 50 000 *
    // 1.500 = 75 000 yen at 152.000.
    approx(t.mae.unwrap(), 35_000.0 / 149.800);
    approx(t.mfe.unwrap(), 75_000.0 / 152.000);

    // The equity at each close: 100 000 until the entry; while long, less the entry fee, with the
    // long marked at the bid close; then 100 000 plus the trade's net.
    let long = |close: f64| 100_000.0 - entry_fee + 50_000.0 * (close - 150.500) / close;
    let closes = [
        100_000.0,
        100_000.0,
        long(150.900),
        long(151.500),
        long(151.000),
        100_000.0 + net,
        100_000.0 + net,
    ];
    assert_eq!(result.equity.len(), closes.len());
    for (point, close) in result.equity.iter().zip(closes) {
        approx(point.equity, close);
    }
    // Turnover: the 50 000 bought and the 50 000 sold are 100 000 dollars traded, each fill's yen
    // notional at its own price, over the mean equity at the closes. In yen the traded amount
    // would be about 151 times larger.
    let mean = closes.iter().sum::<f64>() / closes.len() as f64;
    approx(result.metrics.turnover.unwrap(), 100_000.0 / mean);

    // Buy and hold puts the 100 000 dollars into bar 0's ask open, 150.020. A unit costs
    // (150.020 + 0.003) / 150.020 dollars, so the cash buys 100 000 * 150.020 / 150.023 =
    // 99 998.0003, rounded down to the lot: 99 998 (at a unit cost in yen, 666). The fee is
    // 99 998 * 0.003 = 299.994 yen at 150.020, 2.00 dollars. At the last bid close, 150.900, the
    // long is up 99 998 * 0.880 = 87 998.24 yen at 150.900, 583.16 dollars.
    let bench = &result.benchmark;
    approx(bench.entry_price, 150.020);
    assert_eq!(bench.qty, 99_998.0);
    let end = 100_000.0 - 99_998.0 * 0.003 / 150.020 + 99_998.0 * (150.900 - 150.020) / 150.900;
    approx(bench.equity.last().unwrap().equity, end);
}
