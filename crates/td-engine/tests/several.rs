//! A backtest that trades several instruments in one account ([`BacktestConfig::also_traded`]).
//! Every number is worked by hand in the comments: each instrument trades with 5 bps of slippage
//! (the first by the config, the others by their default) and no fees, so a market buy at an
//! open of 100 fills at 100.05 and a sell at 50 at 49.975, rounded down to 49.97.

mod common;

use std::sync::{Arc, Mutex};

use common::{MIN, bar};
use td_core::{
    AccountCurrency, Candle, FeeSchedule, Fill, Instrument, InstrumentId, InstrumentKind, Millis,
    OrderRequest, OrderType, Side, TimeInForce, Timeframe, Venue,
};
use td_engine::{
    AlsoTradedBars, BacktestConfig, BacktestResult, Context, EngineError, OrderStatus, Params,
    Series, SeriesRow, Strategy, config_fingerprint_with_traded, run_strategy_with_references,
    run_strategy_with_traded,
};

/// A USDT perpetual on Binance: tick 0.01, lot 0.001, no fees.
fn perp(base: &str) -> Instrument {
    Instrument {
        id: InstrumentId {
            venue: Venue::Binance,
            symbol: format!("{base}USDT"),
            kind: InstrumentKind::Perpetual,
        },
        base: base.into(),
        quote: "USDT".into(),
        tick_size: 0.01,
        lot_size: 0.001,
        min_qty: 0.001,
        min_notional: 1.0,
        fees: FeeSchedule {
            maker: 0.0,
            taker: 0.0,
            per_unit: 0.0,
        },
    }
}

fn a() -> Instrument {
    perp("AAA")
}

fn b() -> Instrument {
    perp("BBB")
}

/// `a()` traded with `b()` beside it over `n` one-minute bars from the epoch: 10 000 USDT, up to
/// 2x, 5 bps of slippage.
fn config(n: i64) -> BacktestConfig {
    BacktestConfig {
        strategy: "scripted".into(),
        params: Params::new(),
        instrument: a(),
        timeframe: Timeframe::M1,
        from: 0,
        to: n * MIN,
        starting_cash: 10_000.0,
        slippage_bps: 5.0,
        fee_override: None,
        max_leverage: 2.0,
        warmup_from: None,
        close_at_end: false,
        account_currency: AccountCurrency::Quote,
        references: Vec::new(),
        series: Vec::new(),
        also_traded: vec![b()],
        start_when_all_listed: false,
        short_stocks: None,
    }
}

/// A bar that opens and closes at `price` and trades only there.
fn flat(i: i64, price: f64) -> Candle {
    bar(i, price, price, price, price)
}

fn order(inst: &Instrument, side: Side, qty: f64, order_type: OrderType) -> OrderRequest {
    OrderRequest {
        instrument: inst.id.clone(),
        side,
        qty,
        order_type,
        tif: TimeInForce::Gtc,
        reduce_only: false,
        client_tag: None,
    }
}

fn market(inst: &Instrument, side: Side, qty: f64) -> OrderRequest {
    order(inst, side, qty, OrderType::Market)
}

/// What a scripted strategy saw: the open time of each bar it was called on, each fill with the
/// open time of the bar `on_fill` ran on, and what its script noted.
#[derive(Default)]
struct Seen {
    bars: Vec<Millis>,
    fills: Vec<(Millis, Fill)>,
    notes: Vec<String>,
}

type Log = Arc<Mutex<Seen>>;
type Script = Box<dyn FnMut(&mut Context, &mut Vec<String>) + Send>;

/// Runs `script` at every bar's close and records what it saw.
struct Scripted {
    script: Script,
    log: Log,
}

impl Strategy for Scripted {
    fn on_bar(&mut self, ctx: &mut Context, bar: &Candle) {
        let mut log = self.log.lock().unwrap();
        log.bars.push(bar.open_time);
        (self.script)(ctx, &mut log.notes);
    }

    fn on_fill(&mut self, ctx: &mut Context, fill: &Fill) {
        self.log
            .lock()
            .unwrap()
            .fills
            .push((ctx.bar().open_time, fill.clone()));
    }
}

fn scripted(
    script: impl FnMut(&mut Context, &mut Vec<String>) + Send + 'static,
) -> (Box<Scripted>, Log) {
    let log = Log::default();
    let strategy = Scripted {
        script: Box::new(script),
        log: log.clone(),
    };
    (Box::new(strategy), log)
}

/// The index of the bar the strategy is called on.
fn index(ctx: &Context) -> usize {
    ctx.bars().len() - 1
}

fn traded(inst: &Instrument, bars: Vec<Candle>, funding: Vec<(Millis, f64)>) -> AlsoTradedBars {
    AlsoTradedBars {
        instrument: inst.id.clone(),
        bars,
        funding,
        left_at: None,
    }
}

/// Runs `strategy` on `config` with `a_bars` and `a_funding` for the first instrument and
/// `others` beside it.
fn run(
    config: &BacktestConfig,
    strategy: Box<dyn Strategy>,
    a_bars: &[Candle],
    a_funding: &[(Millis, f64)],
    others: &[AlsoTradedBars],
) -> Result<BacktestResult, EngineError> {
    run_strategy_with_traded(config, strategy, a_bars, &[], a_funding, &[], &[], others)
}

#[track_caller]
fn close(actual: f64, expected: f64) {
    assert!(
        (actual - expected).abs() < 1e-9,
        "expected {expected}, got {actual}"
    );
}

fn final_equity(result: &BacktestResult) -> f64 {
    result.equity.last().unwrap().equity
}

#[test]
fn a_long_and_a_short_share_one_account_under_a_gross_leverage_cap() {
    let a_bars = vec![
        flat(0, 100.0),
        flat(1, 100.0),
        bar(2, 100.0, 106.0, 99.0, 105.0),
        flat(3, 110.0),
    ];
    let b_bars = vec![
        flat(0, 50.0),
        flat(1, 50.0),
        bar(2, 50.0, 51.0, 46.0, 47.0),
        flat(3, 45.0),
    ];
    let (strategy, log) = scripted(|ctx, notes| {
        if index(ctx) != 0 {
            return;
        }
        // 10 A at the close of 100 is 1 000 and 20 B at 50 another 1 000, on 10 000 at 2x.
        ctx.submit(market(&a(), Side::Buy, 10.0)).unwrap();
        ctx.submit(market(&b(), Side::Sell, 20.0)).unwrap();
        // 185 more A is 18 500, so A alone could reach 19 500, under the 20 000 allowed, but
        // with B's 1 000 the account could reach 20 500: rejected.
        let over = ctx.submit(market(&a(), Side::Buy, 185.0)).unwrap_err();
        notes.push(over.reason);
        // A buy limit at 50 for 350 is 17 500, so the account could reach 19 500: accepted. A
        // never trades down to 50, so it rests to the end.
        let limit = OrderType::Limit { price: 50.0 };
        let fits = ctx.submit(order(&a(), Side::Buy, 350.0, limit));
        notes.push(format!("{:?}", fits.map(|_| "accepted")));
    });
    let result = run(
        &config(4),
        strategy,
        &a_bars,
        &[],
        &[traded(&b(), b_bars, Vec::new())],
    )
    .unwrap();
    let notes = &log.lock().unwrap().notes;
    assert!(
        notes[0].contains("gross notional could reach 20500"),
        "{}",
        notes[0]
    );
    assert_eq!(notes[1], r#"Ok("accepted")"#);
    // A bought at 100.05 and B sold at 49.97 at bar 1's open; at the last close A is 110 and B 45:
    // 10 000 + 10 * (110 - 100.05) + 20 * (49.97 - 45) = 10 000 + 99.5 + 99.4.
    let fills: Vec<(String, Side, f64)> = result
        .fills
        .iter()
        .map(|f| (f.instrument.symbol.clone(), f.side, f.price))
        .collect();
    assert_eq!(
        fills,
        [
            ("AAAUSDT".to_string(), Side::Buy, 100.05),
            ("BBBUSDT".to_string(), Side::Sell, 49.97),
        ]
    );
    close(final_equity(&result), 10_198.9);
    assert_eq!(result.equity.len(), 4);
}

#[test]
fn an_instrument_listed_later_has_no_slots_or_orders_before_its_first_bar() {
    let a_bars: Vec<Candle> = (0..6).map(|i| flat(i, 100.0)).collect();
    let b_bars: Vec<Candle> = (3..6).map(|i| flat(i, 50.0)).collect();
    let script = |ctx: &mut Context, notes: &mut Vec<String>| {
        let slots: Vec<bool> = ctx
            .bars_of(&b().id)
            .unwrap()
            .iter()
            .map(Option::is_some)
            .collect();
        notes.push(format!(
            "{} {} {:?}",
            index(ctx),
            ctx.has_had_bar(&b().id),
            slots
        ));
        if index(ctx) == 1 && !ctx.is_warming_up() {
            notes.push(ctx.submit(market(&b(), Side::Buy, 1.0)).unwrap_err().reason);
        }
    };
    let (strategy, log) = scripted(script);
    let others = [traded(&b(), b_bars.clone(), Vec::new())];
    let result = run(&config(6), strategy, &a_bars, &[], &others).unwrap();
    let notes = log.lock().unwrap().notes.clone();
    assert_eq!(notes[0], "0 false [false]");
    assert_eq!(notes[1], "1 false [false, false]");
    assert!(
        notes[2].ends_with(
            "BBBUSDT:perpetual has no bar at 60000, so it has no price to check an order at"
        ),
        "{}",
        notes[2]
    );
    assert_eq!(notes[3], "2 false [false, false, false]");
    assert_eq!(notes[4], "3 true [false, false, false, true]");
    assert_eq!(result.equity.len(), 6);
    assert_eq!(result.orders.len(), 1);

    // Starting when every instrument is listed makes bars 0 to 2 warm-up bars: the strategy sees
    // them, and the run trades and records equity from bar 3.
    let mut config = config(6);
    config.start_when_all_listed = true;
    let (strategy, log) = scripted(|ctx, notes| notes.push(ctx.is_warming_up().to_string()));
    let result = run(&config, strategy, &a_bars, &[], &others).unwrap();
    assert_eq!(
        log.lock().unwrap().notes,
        ["true", "true", "true", "false", "false", "false"]
    );
    assert_eq!(result.equity[0].time, 3 * MIN);
    assert_eq!(result.equity.len(), 3);
    // The saved configuration is the one given; the start is worked out again from the data.
    assert_eq!(result.config, config);
}

#[test]
fn a_missing_bar_of_another_instrument_keeps_its_mark_and_charges_its_funding_at_the_next_open() {
    let a_bars: Vec<Candle> = (0..6).map(|i| flat(i, 100.0)).collect();
    let b_bars = vec![
        flat(0, 50.0),
        flat(1, 50.0),
        flat(2, 52.0),
        // No bar 3.
        bar(4, 51.0, 52.0, 47.0, 50.0),
        flat(5, 50.0),
    ];
    // Due while B has no bar.
    let b_funding = vec![(3 * MIN, 0.001)];
    let (strategy, log) = scripted(|ctx, notes| match index(ctx) {
        0 => {
            ctx.submit(market(&b(), Side::Buy, 10.0)).unwrap();
        }
        2 => {
            let limit = OrderType::Limit { price: 48.0 };
            ctx.submit(order(&b(), Side::Buy, 5.0, limit)).unwrap();
        }
        3 => {
            notes.push(format!("{:?}", ctx.bars_of(&b().id).unwrap().last()));
            notes.push(format!("{:?}", ctx.bar_of(&b().id)));
        }
        _ => {}
    });
    let others = [traded(&b(), b_bars, b_funding)];
    let result = run(&config(6), strategy, &a_bars, &[], &others).unwrap();
    assert_eq!(log.lock().unwrap().notes, ["Some(None)", "None"]);
    // The buy filled at 50.03 at bar 1. At bar 3 B is still marked at bar 2's close of 52:
    // 10 000 + 10 * (52 - 50.03).
    close(result.equity[3].equity, 10_019.7);
    // The limit at 48 does not fill while B has no bar, but on bar 4, whose low is 47.
    let limit_fill = &result.fills[1];
    assert_eq!((limit_fill.time, limit_fill.price), (4 * MIN, 48.0));
    // The funding due at 3 is charged at bar 4's open of 51 on the 10 held then: 0.51. Bar 4 then
    // closes at 50: 10 000 - 0.51 + 10 * (50 - 50.03) + 5 * (50 - 48).
    close(result.metrics.funding_paid, 0.51);
    close(result.equity[4].equity, 10_009.19);
}

#[test]
fn a_stop_of_another_instrument_fills_while_the_first_has_no_bar() {
    let a_bars = vec![
        flat(0, 100.0),
        flat(1, 100.0),
        flat(2, 100.0),
        // No bar 3.
        flat(4, 100.0),
        flat(5, 100.0),
    ];
    let b_bars = vec![
        flat(0, 50.0),
        flat(1, 50.0),
        flat(2, 50.0),
        bar(3, 50.0, 50.0, 44.0, 46.0),
        flat(4, 46.0),
        flat(5, 46.0),
    ];
    let b_funding = vec![(3 * MIN, 0.001)];
    let (strategy, log) = scripted(|ctx, _| {
        if index(ctx) == 0 {
            ctx.submit_with_stop(market(&b(), Side::Buy, 10.0), 45.0)
                .unwrap();
        }
    });
    let others = [traded(&b(), b_bars, b_funding)];
    let result = run(&config(6), strategy, &a_bars, &[], &others).unwrap();
    let log = log.lock().unwrap();
    // The strategy is never called at 3, where A has no bar.
    assert_eq!(log.bars, [0, MIN, 2 * MIN, 4 * MIN, 5 * MIN]);
    // The stop at 45 fills on B's bar 3 at 45 less 5 bps, 44.9775, rounded down to 44.97, and
    // the strategy hears of it at A's next bar.
    let (heard_at, stop) = &log.fills[1];
    assert_eq!(
        (stop.time, stop.price, *heard_at),
        (3 * MIN, 44.97, 4 * MIN)
    );
    // B's funding due at 3 is charged at its open of 50 on the 10 held: 0.5.
    close(result.metrics.funding_paid, 0.5);
    // Bought at 50.03 and sold at 44.97: 10 000 - 0.5 - 10 * 5.06.
    close(final_equity(&result), 9_948.9);
    // Bar 4's range takes in time 3: its high is the account at B's open of 50 after the funding,
    // 10 000 - 0.5 + 10 * (50 - 50.03), and its low no higher than after the stop.
    let range = result.equity_extremes[3];
    close(range.high, 9_999.2);
    assert!(range.low <= 9_948.9 + 1e-9, "{range:?}");
}

#[test]
fn each_instrument_pays_funding_on_its_own_position_and_price() {
    let a_bars = vec![
        flat(0, 100.0),
        flat(1, 100.0),
        bar(2, 101.0, 102.0, 101.0, 102.0),
        flat(3, 102.0),
    ];
    let b_bars = vec![
        flat(0, 50.0),
        flat(1, 50.0),
        bar(2, 48.0, 49.0, 48.0, 49.0),
        flat(3, 49.0),
    ];
    let a_funding = [(2 * MIN, 0.0001)];
    let b_funding = vec![(2 * MIN, 0.0002), (2 * MIN + 30_000, 0.0005)];
    let (strategy, _) = scripted(|ctx, _| {
        if index(ctx) == 0 {
            ctx.submit(market(&a(), Side::Buy, 10.0)).unwrap();
            ctx.submit(market(&b(), Side::Sell, 20.0)).unwrap();
        }
    });
    let others = [traded(&b(), b_bars, b_funding)];
    let result = run(&config(4), strategy, &a_bars, &a_funding, &others).unwrap();
    // At bar 2's open the long 10 A pays 10 * 101 * 0.0001 = 0.101, and the short 20 B gets
    // 20 * 48 * 0.0002 = 0.192; inside bar 2, B's second rate is charged at its close of 49:
    // the short gets 20 * 49 * 0.0005 = 0.49.
    let funding: Vec<(String, f64)> = result
        .trades
        .iter()
        .map(|t| (t.instrument.clone().unwrap().symbol, t.funding))
        .collect();
    assert_eq!(funding.len(), 2);
    assert_eq!(funding[0].0, "AAAUSDT");
    close(funding[0].1, 0.101);
    assert_eq!(funding[1].0, "BBBUSDT");
    close(funding[1].1, -0.682);
    close(result.metrics.funding_paid, 0.101 - 0.682);
    // 10 000 + 10 * (102 - 100.05) + 20 * (49.97 - 49) - 0.101 + 0.682.
    close(
        final_equity(&result),
        10_000.0 + 19.5 + 19.4 - 0.101 + 0.682,
    );
}

#[test]
fn two_positions_below_their_combined_maintenance_are_liquidated_together() {
    let a_bars = vec![
        flat(0, 100.0),
        flat(1, 100.0),
        flat(2, 89.0),
        flat(3, 100.0),
    ];
    let b_bars = vec![flat(0, 50.0), flat(1, 50.0), flat(2, 44.5), flat(3, 50.0)];
    let (strategy, _) = scripted(|ctx, _| {
        if index(ctx) == 0 {
            ctx.submit(market(&a(), Side::Buy, 45.0)).unwrap();
            ctx.submit(market(&b(), Side::Buy, 90.0)).unwrap();
            // Rests far below the market until the liquidation cancels it.
            let limit = OrderType::Limit { price: 10.0 };
            ctx.submit(order(&a(), Side::Buy, 1.0, limit)).unwrap();
        }
    });
    let mut config = config(4);
    config.starting_cash = 1_000.0;
    config.max_leverage = 10.0;
    let others = [traded(&b(), b_bars, Vec::new())];
    let result = run(&config, strategy, &a_bars, &[], &others).unwrap();
    // Bought 45 A at 100.05 and 90 B at 50.03. At bar 2's opens, 89 and 44.5, the account holds
    // 1 000 - 45 * 11.05 - 90 * 5.53 = 5.05, under maintenance of 0.5% of 45 * 89 + 90 * 44.5 =
    // 8 010, which is 40.05: both positions are closed there.
    let liquidations: Vec<(String, Side, f64, f64)> = result
        .fills
        .iter()
        .skip(2)
        .map(|f| (f.instrument.symbol.clone(), f.side, f.qty, f.price))
        .collect();
    assert_eq!(
        liquidations,
        [
            ("AAAUSDT".to_string(), Side::Sell, 45.0, 89.0),
            ("BBBUSDT".to_string(), Side::Sell, 90.0, 44.5),
        ]
    );
    assert!(result.fills.iter().skip(2).all(|f| f.time == 2 * MIN));
    let limit = &result.orders[2];
    assert!(
        matches!(&limit.status, OrderStatus::Cancelled(_)),
        "{:?}",
        limit.status
    );
    assert!(
        result.orders.iter().all(|o| o.status != OrderStatus::Open),
        "an order is still open"
    );
    close(result.equity[2].equity, 0.0);
    close(final_equity(&result), 0.0);
}

#[test]
fn trades_are_built_per_instrument_and_carry_it() {
    let a_bars = vec![
        flat(0, 100.0),
        flat(1, 100.0),
        flat(2, 104.0),
        flat(3, 104.0),
        flat(4, 104.0),
        flat(5, 104.0),
    ];
    let b_bars = vec![
        flat(0, 50.0),
        flat(1, 50.0),
        flat(2, 50.0),
        flat(3, 50.0),
        flat(4, 48.0),
        flat(5, 48.0),
    ];
    // A's trade and B's overlap: A is bought at bar 1 and sold at bar 3, B sold at bar 2 and
    // bought back at bar 4.
    let (strategy, _) = scripted(|ctx, _| match index(ctx) {
        0 => {
            ctx.submit(market(&a(), Side::Buy, 10.0)).unwrap();
        }
        1 => {
            ctx.submit(market(&b(), Side::Sell, 20.0)).unwrap();
        }
        2 => {
            ctx.submit(market(&a(), Side::Sell, 10.0)).unwrap();
        }
        3 => {
            ctx.submit(market(&b(), Side::Buy, 20.0)).unwrap();
        }
        _ => {}
    });
    let others = [traded(&b(), b_bars, Vec::new())];
    let result = run(&config(6), strategy, &a_bars, &[], &others).unwrap();
    let trades: Vec<(String, Millis, Millis, f64, f64, f64)> = result
        .trades
        .iter()
        .map(|t| {
            (
                t.instrument.clone().unwrap().symbol,
                t.entry_time,
                t.exit_time,
                cents(t.entry_price),
                cents(t.exit_price),
                t.qty,
            )
        })
        .collect();
    // A: bought at 100.05, sold at 104 less 5 bps, 103.948, rounded down to 103.94. B: sold at
    // 49.97, bought back at 48 plus 5 bps, 48.024, rounded up to 48.03.
    assert_eq!(
        trades,
        [
            ("AAAUSDT".to_string(), MIN, 3 * MIN, 100.05, 103.94, 10.0),
            ("BBBUSDT".to_string(), 2 * MIN, 4 * MIN, 49.97, 48.03, 20.0),
        ]
    );
    assert!(result.trades.iter().all(|t| !t.open));
    close(result.trades[0].net_pnl, 10.0 * (103.94 - 100.05));
    close(result.trades[1].net_pnl, 20.0 * (49.97 - 48.03));
    // Saved with its instrument, and with no pips, which are counted on FX alone.
    let json = serde_json::to_value(&result.trades[0]).unwrap();
    assert_eq!(json["instrument"]["symbol"], "AAAUSDT");
    assert!(
        result
            .trades
            .iter()
            .all(|t| t.pips.is_none() && t.net_pips.is_none())
    );
}

#[test]
fn the_fingerprint_covers_the_other_instruments_bars_and_funding() {
    let a_bars: Vec<Candle> = (0..4).map(|i| flat(i, 100.0)).collect();
    let b_bars: Vec<Candle> = (0..4).map(|i| flat(i, 50.0)).collect();
    let b_funding = vec![(2 * MIN, 0.0001)];
    let config = config(4);
    let fingerprint = |b_bars: &[Candle], b_funding: &[(Millis, f64)]| {
        let others = [traded(&b(), b_bars.to_vec(), b_funding.to_vec())];
        let (strategy, _) = scripted(|_, _| {});
        let result = run(&config, strategy, &a_bars, &[], &others).unwrap();
        let quoted =
            config_fingerprint_with_traded(&config, &a_bars, &[], &[], &[], &[], &others).unwrap();
        assert_eq!(quoted, result.data_fingerprint);
        result.data_fingerprint
    };
    let base = fingerprint(&b_bars, &b_funding);

    let mut moved = b_bars.clone();
    moved[2].close = 50.01;
    moved[2].high = 50.01;
    let bar_moved = fingerprint(&moved, &b_funding);
    assert_ne!(bar_moved, base, "a bar of B moved");

    let rate_moved = fingerprint(&b_bars, &[(2 * MIN, 0.0002)]);
    assert_ne!(rate_moved, base, "a funding rate of B moved");

    // A bar of B at a time A has no bar counts too.
    let gap: Vec<Candle> = a_bars
        .iter()
        .filter(|b| b.open_time != 2 * MIN)
        .copied()
        .collect();
    let gapped = |b_bars: Vec<Candle>| {
        let (strategy, _) = scripted(|_, _| {});
        let others = [traded(&b(), b_bars, b_funding.clone())];
        run(&config, strategy, &gap, &[], &others)
            .unwrap()
            .data_fingerprint
    };
    let mut extra_moved = b_bars.clone();
    extra_moved[2].high = 51.0;
    assert_ne!(gapped(b_bars), gapped(extra_moved), "B's extra bar moved");
}

#[test]
fn a_series_with_no_row_yet_is_empty_in_a_run_with_several_instruments() {
    let a_bars: Vec<Candle> = (0..4).map(|i| flat(i, 100.0)).collect();
    let b_bars: Vec<Candle> = (0..4).map(|i| flat(i, 50.0)).collect();
    let others = [traded(&b(), b_bars, Vec::new())];
    // Known at bar 2's close.
    let series = [Series {
        name: "s".into(),
        rows: vec![SeriesRow {
            known_at: 3 * MIN,
            value: 1.0,
            period: None,
        }],
    }];
    let mut config = config(4);
    config.series = vec!["s".into()];
    let (strategy, log) = scripted(|ctx, notes| {
        notes.push(format!("{:?}", ctx.series("s").map(<[SeriesRow]>::len)));
    });
    run_strategy_with_traded(&config, strategy, &a_bars, &[], &[], &[], &series, &others).unwrap();
    assert_eq!(
        log.lock().unwrap().notes,
        ["Some(0)", "Some(0)", "Some(1)", "Some(1)"]
    );

    // A row known only after the last bar's close is never read: refused.
    let late = [Series {
        name: "s".into(),
        rows: vec![SeriesRow {
            known_at: 5 * MIN,
            value: 1.0,
            period: None,
        }],
    }];
    let (strategy, _) = scripted(|_, _| {});
    let refused =
        run_strategy_with_traded(&config, strategy, &a_bars, &[], &[], &[], &late, &others)
            .unwrap_err();
    assert!(matches!(refused, EngineError::SeriesData(_)), "{refused}");

    // A run with one instrument still refuses a series that does not cover its first bar.
    config.also_traded = Vec::new();
    let (strategy, _) = scripted(|_, _| {});
    let refused = run_strategy_with_references(&config, strategy, &a_bars, &[], &[], &[], &series)
        .unwrap_err();
    let EngineError::SeriesData(why) = refused else {
        panic!("{refused}");
    };
    assert!(
        why.contains("does not cover the start of the range"),
        "{why}"
    );
}

#[test]
fn a_run_with_one_instrument_saves_no_new_keys() {
    let a_bars: Vec<Candle> = (0..4).map(|i| flat(i, 100.0)).collect();
    let mut config = config(4);
    config.also_traded = Vec::new();
    let (strategy, _) = scripted(|ctx, _| match index(ctx) {
        0 => {
            ctx.submit(market(&a(), Side::Buy, 10.0)).unwrap();
        }
        1 => {
            ctx.submit(market(&a(), Side::Sell, 10.0)).unwrap();
        }
        _ => {}
    });
    let result =
        run_strategy_with_references(&config, strategy, &a_bars, &[], &[], &[], &[]).unwrap();
    assert_eq!(result.trades.len(), 1);
    assert_eq!(result.trades[0].instrument, None);
    let json = serde_json::to_value(&result).unwrap();
    let saved = json["config"].as_object().unwrap();
    assert!(!saved.contains_key("also_traded"));
    assert!(!saved.contains_key("start_when_all_listed"));
    let trade = serde_json::to_value(&result.trades[0]).unwrap();
    assert!(!trade.as_object().unwrap().contains_key("instrument"));
}

#[test]
fn a_run_names_only_the_instruments_it_has_bars_for() {
    let a_bars: Vec<Candle> = (0..4).map(|i| flat(i, 100.0)).collect();
    let b_bars: Vec<Candle> = (0..4).map(|i| flat(i, 50.0)).collect();
    let refused = |config: &BacktestConfig, others: &[AlsoTradedBars]| {
        let (strategy, _) = scripted(|_, _| {});
        match run(config, strategy, &a_bars, &[], others).unwrap_err() {
            EngineError::InvalidTraded(why) => why,
            other => panic!("{other}"),
        }
    };
    assert!(refused(&config(4), &[]).starts_with("no bars were given for"));
    let mut one = config(4);
    one.also_traded = Vec::new();
    assert!(refused(&one, &[traded(&b(), b_bars.clone(), Vec::new())]).contains("does not trade"));
    let mut spot = config(4);
    spot.also_traded[0].id.kind = InstrumentKind::Spot;
    assert!(refused(&spot, &[]).contains("not a perpetual"));
}

#[test]
fn an_instrument_that_stops_trading_is_closed_at_its_last_close_and_takes_no_more_orders() {
    let a_bars: Vec<Candle> = (0..8).map(|i| flat(i, 100.0)).collect();
    let b_bars: Vec<Candle> = (0..5).map(|i| flat(i, 50.0)).collect();
    let leaving = AlsoTradedBars {
        left_at: Some(4 * MIN),
        ..traded(&b(), b_bars.clone(), Vec::new())
    };
    let account = Arc::new(Mutex::new(Vec::new()));
    let seen = account.clone();
    let (strategy, log) = scripted(move |ctx, notes| {
        let i = index(ctx);
        notes.push(format!("{i} left {}", ctx.has_left(&b().id)));
        let b_position = ctx.position_of(&b().id);
        seen.lock()
            .unwrap()
            .push((ctx.cash(), ctx.equity(), b_position));
        if i == 0 {
            ctx.submit(market(&b(), Side::Buy, 10.0)).unwrap();
            let resting = OrderType::Limit { price: 10.0 };
            ctx.submit(order(&b(), Side::Buy, 1.0, resting)).unwrap();
        }
        if i >= 4
            && let Err(e) = ctx.submit(market(&b(), Side::Buy, 1.0))
        {
            notes.push(e.to_string());
        }
    });
    let leaving = [leaving];
    let result = run(&config(8), strategy, &a_bars, &[], &leaving).unwrap();
    let fills: Vec<(Millis, Side, f64, f64)> = result
        .fills
        .iter()
        .map(|f| (f.time, f.side, f.qty, f.price))
        .collect();
    assert_eq!(
        fills,
        [
            (MIN, Side::Buy, 10.0, 50.03),
            (4 * MIN, Side::Sell, 10.0, 49.97)
        ],
        "bought at the open of 50 plus 5 bps, sold at the last close of 50 less 5 bps"
    );
    let cancelled = result.orders.iter().any(
        |o| matches!(&o.status, OrderStatus::Cancelled(why) if why.contains("stopped trading")),
    );
    assert!(cancelled, "{:?}", result.orders);
    let notes = log.lock().unwrap().notes.clone();
    assert!(notes.contains(&"3 left false".to_string()), "{notes:?}");
    assert!(notes.contains(&"4 left true".to_string()), "{notes:?}");
    let refused: Vec<&String> = notes
        .iter()
        .filter(|n| n.contains("stopped trading"))
        .collect();
    assert_eq!(refused.len(), 4, "bars 4 to 7: {notes:?}");
    assert_eq!(result.equity.len(), 8, "the run goes on");
    let account = account.lock().unwrap().clone();
    let (cash, equity, position) = account[3];
    close(cash, 10_000.0);
    close(equity, 9_999.7);
    assert_eq!(position, Some(10.0));
    for (i, &(cash, equity, position)) in account.iter().enumerate().skip(4) {
        close(cash, 9_999.4);
        close(equity, 9_999.4);
        assert_eq!(position, Some(0.0), "bar {i}");
    }
    close(result.equity[3].equity, 9_999.7);
    close(final_equity(&result), 9_999.4);

    let staying = run_quiet(
        &config(8),
        &a_bars,
        &[traded(&b(), b_bars.clone(), Vec::new())],
    );
    let left = run_quiet(&config(8), &a_bars, &leaving);
    assert_ne!(staying.data_fingerprint, left.data_fingerprint);

    let [leaving] = leaving;
    let early = AlsoTradedBars {
        left_at: Some(3 * MIN),
        ..leaving
    };
    let (strategy, _) = scripted(|_, _| {});
    match run(&config(8), strategy, &a_bars, &[], &[early]).unwrap_err() {
        EngineError::InvalidTraded(why) => assert!(why.contains("not the open of its last bar")),
        other => panic!("{other}"),
    }
}

/// `price` rounded to the cent, to compare a price the engine worked out.
fn cents(price: f64) -> f64 {
    (price * 100.0).round() / 100.0
}

fn c() -> Instrument {
    perp("CCC")
}

/// The result of a run whose strategy does nothing.
fn run_quiet(
    config: &BacktestConfig,
    a_bars: &[Candle],
    others: &[AlsoTradedBars],
) -> BacktestResult {
    let (strategy, _) = scripted(|_, _| {});
    run(config, strategy, a_bars, &[], others).unwrap()
}

#[test]
fn the_fingerprint_covers_when_another_instrument_was_listed() {
    // A has bars 10 to 15 and the run starts at 10. B is listed at 5 in X, at 1 in Z and at 13 in
    // Y, and has the same bars from 13 on in all three. In X and Z B has had a bar on every bar of
    // the run; in Y only from 13, so `has_had_bar` differs on bars 10 to 12, and with
    // `start_when_all_listed` X and Z trade from 10 and Y from 13. The bars before the run are
    // never used, so only the listing time tells X from Y; Z, a download with longer history,
    // reads the same as X.
    let a_bars: Vec<Candle> = (10..16).map(|i| flat(i, 100.0)).collect();
    let listed = |from: std::ops::Range<i64>| {
        let bars = from.chain(13..16).map(|i| flat(i, 50.0)).collect();
        [traded(&b(), bars, Vec::new())]
    };
    let (x, y, z) = (listed(5..10), listed(0..0), listed(1..10));
    for when_all_listed in [false, true] {
        let mut config = config(16);
        config.from = 10 * MIN;
        config.start_when_all_listed = when_all_listed;
        let fx = run_quiet(&config, &a_bars, &x).data_fingerprint;
        let fy = run_quiet(&config, &a_bars, &y).data_fingerprint;
        let fz = run_quiet(&config, &a_bars, &z).data_fingerprint;
        assert_ne!(
            fx, fy,
            "B listed before the run and at 13 (start when all listed: {when_all_listed})"
        );
        assert_eq!(
            fx, fz,
            "B listed at 5 and at 1 (start when all listed: {when_all_listed})"
        );
    }
}

#[test]
fn a_hedged_book_is_read_at_the_opens_fills_and_closes_and_not_at_each_worst_price() {
    let a_bars = vec![
        flat(0, 100.0),
        flat(1, 100.0),
        bar(2, 100.0, 100.0, 89.0, 100.0),
        flat(3, 100.0),
    ];
    let b_bars = vec![
        flat(0, 50.0),
        flat(1, 50.0),
        bar(2, 50.0, 55.6, 50.0, 50.0),
        flat(3, 50.0),
    ];
    let (strategy, _) = scripted(|ctx, _| {
        if index(ctx) == 0 {
            ctx.submit(market(&a(), Side::Buy, 45.0)).unwrap();
            ctx.submit(market(&b(), Side::Sell, 90.0)).unwrap();
        }
    });
    let mut config = config(4);
    config.starting_cash = 1_000.0;
    config.max_leverage = 10.0;
    let others = [traded(&b(), b_bars, Vec::new())];
    let result = run(&config, strategy, &a_bars, &[], &others).unwrap();
    // Long 45 A at 100.05 and short 90 B at 49.97. Bar 2 opens and closes at 100 and 50, where
    // the account holds 1 000 - 45 * 0.05 - 90 * 0.03 = 995.05, and the range is read there.
    // With each position at its own worst price at once, A at its low of 89 and B at its high of
    // 55.6, it would hold 1 000 - 45 * 11.05 - 90 * 5.63 = -3.95, under maintenance: the bars do
    // not say the two happened together, so the account is not liquidated.
    assert_eq!(result.fills.len(), 2, "{:?}", result.fills);
    let range = result.equity_extremes[2];
    close(range.low, 995.05);
    close(range.high, 995.05);
    close(final_equity(&result), 995.05);
}

#[test]
fn the_account_is_read_after_each_fill_with_its_fee_paid() {
    let a_bars = vec![flat(0, 100.0), bar(1, 100.0, 101.0, 100.0, 101.0)];
    let b_bars = vec![flat(0, 50.0), bar(1, 50.0, 51.0, 50.0, 51.0)];
    let (strategy, _) = scripted(|ctx, _| {
        if index(ctx) == 0 {
            ctx.submit(market(&a(), Side::Buy, 10.0)).unwrap();
            ctx.submit(market(&b(), Side::Buy, 20.0)).unwrap();
        }
    });
    let mut config = config(2);
    config.starting_cash = 1_000.0;
    config.max_leverage = 10.0;
    config.instrument.fees.per_unit = 0.1;
    config.also_traded[0].fees.per_unit = 0.1;
    let others = [traded(&b(), b_bars, Vec::new())];
    let result = run(&config, strategy, &a_bars, &[], &others).unwrap();
    // 10 A bought at 100.05 for a fee of 1, then 20 B at 50.03 for a fee of 2. Neither fill moves
    // the account but for its fee, and both bars close higher, so the least the account holds in
    // bar 1 is right after the second fill: 1 000 - 1 - 2 = 997. At the closes of 101 and 51 it
    // holds 997 + 10 * (101 - 100.05) + 20 * (51 - 50.03) = 1 025.9.
    assert_eq!(result.fills.len(), 2, "{:?}", result.fills);
    let range = result.equity_extremes[1];
    close(range.low, 997.0);
    close(range.high, 1_025.9);
}

#[test]
fn one_instrument_holding_alone_is_read_at_its_low_and_high() {
    let a_bars: Vec<Candle> = (0..4).map(|i| flat(i, 100.0)).collect();
    let b_bars = vec![
        flat(0, 50.0),
        flat(1, 50.0),
        bar(2, 50.0, 53.0, 46.0, 51.0),
        flat(3, 51.0),
    ];
    let buy = || {
        scripted(|ctx, _| {
            if index(ctx) == 0 {
                ctx.submit(market(&b(), Side::Buy, 10.0)).unwrap();
            }
        })
        .0
    };
    let others = [traded(&b(), b_bars.clone(), Vec::new())];
    let result = run(&config(4), buy(), &a_bars, &[], &others).unwrap();
    // Long 10 B at 50.03, and A neither held nor trading: bar 2 is read at B's low of 46 and high
    // of 53, 10 000 + 10 * (46 - 50.03) and 10 000 + 10 * (53 - 50.03).
    let range = result.equity_extremes[2];
    close(range.low, 9_959.7);
    close(range.high, 10_029.7);
    // As a run that trades B alone reads it.
    let mut alone = config(4);
    alone.instrument = b();
    alone.also_traded = Vec::new();
    let single = run_strategy_with_references(&alone, buy(), &b_bars, &[], &[], &[], &[]).unwrap();
    assert_eq!(single.equity_extremes, result.equity_extremes);
}

#[test]
fn a_liquidation_while_the_first_has_no_bar_closes_another_without_one_at_its_mark() {
    let gap = |price: f64| -> Vec<Candle> {
        [0, 1, 2, 4, 5]
            .into_iter()
            .map(|i| flat(i, price))
            .collect()
    };
    let b_bars = vec![
        flat(0, 50.0),
        flat(1, 50.0),
        flat(2, 50.0),
        flat(3, 39.0),
        flat(4, 39.0),
        flat(5, 39.0),
    ];
    let (strategy, _) = scripted(|ctx, _| {
        if index(ctx) == 0 {
            ctx.submit(market(&b(), Side::Buy, 90.0)).unwrap();
            ctx.submit(market(&c(), Side::Buy, 45.0)).unwrap();
            let limit = OrderType::Limit { price: 10.0 };
            ctx.submit(order(&a(), Side::Buy, 1.0, limit)).unwrap();
            ctx.submit(order(&c(), Side::Buy, 1.0, limit)).unwrap();
        }
    });
    let mut config = config(6);
    config.starting_cash = 1_000.0;
    config.max_leverage = 10.0;
    config.also_traded = vec![b(), c()];
    let others = [
        traded(&b(), b_bars, Vec::new()),
        traded(&c(), gap(100.0), Vec::new()),
    ];
    let result = run(&config, strategy, &gap(100.0), &[], &others).unwrap();
    // Long 90 B at 50.03 and 45 C at 100.05. At 3 only B has a bar, opening at 39, and C is at
    // its last close of 100: the account holds 1 000 - 90 * 11.03 - 45 * 0.05 = 5.05, under
    // maintenance of 0.5% of 90 * 39 + 45 * 100 = 8 010, which is 40.05. Both are closed there.
    let liquidations: Vec<(String, Millis, f64, f64)> = result
        .fills
        .iter()
        .skip(2)
        .map(|f| (f.instrument.symbol.clone(), f.time, f.qty, f.price))
        .collect();
    assert_eq!(
        liquidations,
        [
            ("BBBUSDT".to_string(), 3 * MIN, 90.0, 39.0),
            ("CCCUSDT".to_string(), 3 * MIN, 45.0, 100.0),
        ]
    );
    assert!(
        result.orders.iter().all(|o| o.status != OrderStatus::Open),
        "an order is still open: {:?}",
        result.orders
    );
    // Nothing trades after, and the account stays at zero from A's next bar on.
    let after: Vec<f64> = result.equity.iter().skip(3).map(|p| p.equity).collect();
    assert_eq!(after, [0.0, 0.0]);
}

#[test]
fn another_instrument_holding_alone_is_liquidated_against_the_whole_balance() {
    let a_bars: Vec<Candle> = (0..4).map(|i| flat(i, 100.0)).collect();
    let b_bars = vec![
        flat(0, 50.0),
        flat(1, 50.0),
        bar(2, 50.0, 50.0, 44.0, 49.0),
        flat(3, 49.0),
    ];
    let (strategy, _) = scripted(|ctx, _| {
        if index(ctx) == 0 {
            ctx.submit(market(&b(), Side::Buy, 180.0)).unwrap();
            let limit = OrderType::Limit { price: 10.0 };
            ctx.submit(order(&b(), Side::Buy, 1.0, limit)).unwrap();
            ctx.submit(order(&a(), Side::Buy, 1.0, limit)).unwrap();
        }
    });
    let mut config = config(4);
    config.starting_cash = 1_000.0;
    config.max_leverage = 10.0;
    let others = [traded(&b(), b_bars, Vec::new())];
    let result = run(&config, strategy, &a_bars, &[], &others).unwrap();
    // Long 180 B at 50.03, the 1 000 of cash in the first instrument's book. B alone holds, so
    // bar 2 is read as a run with one instrument reads it, against the whole balance: the
    // account is at maintenance where 1 000 + 180 * (p - 50.03) = 0.005 * 180 * p, at
    // p = 8 005.4 / 179.1 = 44.698, above the low of 44. Without the first book's cash it would
    // be under maintenance at once, and closed at the open of 50.
    let liquidation = &result.fills[1];
    assert_eq!(liquidation.instrument, b().id);
    assert_eq!(liquidation.time, 2 * MIN);
    assert!(
        (liquidation.price - 8_005.4 / 179.1).abs() < 0.01,
        "{}",
        liquidation.price
    );
    assert_eq!(result.fills.len(), 2);
    assert!(
        result.orders.iter().all(|o| o.status != OrderStatus::Open),
        "an order is still open: {:?}",
        result.orders
    );
    close(result.equity[2].equity, 0.0);
    close(final_equity(&result), 0.0);
}

#[test]
fn a_reduce_only_order_is_accepted_over_the_gross_cap() {
    let a_bars = vec![flat(0, 100.0), flat(1, 100.0), flat(2, 90.0), flat(3, 90.0)];
    let b_bars = vec![flat(0, 50.0), flat(1, 50.0), flat(2, 45.0), flat(3, 45.0)];
    let (strategy, log) = scripted(|ctx, notes| match index(ctx) {
        0 => {
            ctx.submit(market(&a(), Side::Buy, 1.0)).unwrap();
            ctx.submit(market(&b(), Side::Buy, 37.0)).unwrap();
        }
        2 => {
            let more = ctx.submit(market(&b(), Side::Buy, 0.1));
            notes.push(format!("{:?}", more.map_err(|r| r.reason)));
            let mut reduce = market(&a(), Side::Sell, 2.0);
            reduce.reduce_only = true;
            notes.push(format!("{:?}", ctx.submit(reduce).map(|_| "accepted")));
        }
        _ => {}
    });
    let mut config = config(4);
    config.starting_cash = 1_000.0;
    let others = [traded(&b(), b_bars, Vec::new())];
    let result = run(&config, strategy, &a_bars, &[], &others).unwrap();
    // Long 1 A at 100.05 and 37 B at 50.03. At bar 2's close the account holds
    // 1 000 - 1 * 10.05 - 37 * 5.03 = 803.84 and a gross notional of 90 + 1 665 = 1 755, over
    // the 2x cap of 1 607.68, as B's 1 665 alone is: an order that adds is refused. The
    // reduce-only sell asks for 2 against the long of 1 A. Held to the cap it would be refused
    // too, since the other instrument alone is over it; as reduce-only it is accepted and
    // closes 1.
    let notes = &log.lock().unwrap().notes;
    assert!(notes[0].contains("over the leverage cap"), "{}", notes[0]);
    assert_eq!(notes[1], r#"Ok("accepted")"#);
    let reduced = result.fills.last().unwrap();
    assert_eq!(
        (reduced.instrument.clone(), reduced.side, reduced.qty),
        (a().id, Side::Sell, 1.0)
    );
}

#[test]
fn a_configuration_trading_several_instruments_is_refused_unless_they_fit_one_account() {
    type Change = fn(&mut BacktestConfig);
    let cases: [(&str, Change); 7] = [
        ("perpetuals or stocks only", |c| {
            c.also_traded[0].id.kind = InstrumentKind::Spot
        }),
        ("of the same kind", |c| {
            c.also_traded[0].id.kind = InstrumentKind::Stock
        }),
        ("another venue", |c| {
            c.also_traded[0].id.venue = Venue::Bybit
        }),
        ("same quote currency", |c| {
            c.also_traded[0].quote = "USDC".into()
        }),
        ("account in their quote currency", |c| {
            c.account_currency = AccountCurrency::Base
        }),
        ("named twice", |c| c.also_traded.push(b())),
        ("cannot also be a reference", |c| c.references.push(b().id)),
    ];
    for (why, change) in cases {
        let mut config = config(4);
        change(&mut config);
        match config.check_traded() {
            Err(EngineError::InvalidTraded(reason)) => {
                assert!(reason.contains(why), "{why}: {reason}")
            }
            other => panic!("{why}: {other:?}"),
        }
    }
    assert!(config(4).check_traded().is_ok());
}

#[test]
fn a_strategy_reads_each_traded_instruments_position_orders_and_funding() {
    let a_bars: Vec<Candle> = (0..4).map(|i| flat(i, 100.0)).collect();
    let b_bars: Vec<Candle> = (0..4).map(|i| flat(i, 50.0)).collect();
    // Due exactly at bar 1's close, so first seen on bar 2.
    let b_funding = vec![(2 * MIN, 0.0001)];
    let a_funding = [(MIN, 0.0002)];
    let (strategy, log) = scripted(|ctx, notes| {
        let (id_a, id_b, id_c) = (a().id, b().id, c().id);
        if index(ctx) == 0 {
            ctx.submit(market(&b(), Side::Buy, 2.0)).unwrap();
            let limit = OrderType::Limit { price: 40.0 };
            ctx.submit(order(&b(), Side::Buy, 1.0, limit)).unwrap();
        }
        let len = |s: Option<&[(Millis, f64)]>| s.map(<[_]>::len);
        let orders = |s: Option<&[td_engine::Order]>| s.map(<[_]>::len);
        notes.push(format!(
            "{} funding {:?} {:?} {:?} position {:?} {:?} {:?} orders {:?} {:?} {:?}",
            index(ctx),
            len(ctx.funding_of(&id_a)),
            len(ctx.funding_of(&id_b)),
            len(ctx.funding_of(&id_c)),
            ctx.position_of(&id_a),
            ctx.position_of(&id_b),
            ctx.position_of(&id_c),
            orders(ctx.open_orders_of(&id_a)),
            orders(ctx.open_orders_of(&id_b)),
            orders(ctx.open_orders_of(&id_c)),
        ));
        assert_eq!(ctx.funding_of(&id_a), Some(ctx.funding()));
        let symbols: Vec<&str> = ctx.traded().map(|i| i.id.symbol.as_str()).collect();
        assert_eq!(symbols, ["AAAUSDT", "BBBUSDT"]);
        assert_eq!(ctx.traded_instrument(&id_b), Some(&b()));
        assert_eq!(ctx.traded_instrument(&id_c), None);
    });
    let others = [traded(&b(), b_bars, b_funding)];
    run(&config(4), strategy, &a_bars, &a_funding, &others).unwrap();
    // The market buy fills at bar 1's open and the limit at 40 rests. A's rate due at 1, bar 0's
    // close, is first seen on bar 1, and B's due at 2 on bar 2.
    assert_eq!(
        log.lock().unwrap().notes,
        [
            "0 funding Some(0) Some(0) None position Some(0.0) Some(0.0) None orders Some(0) Some(2) None",
            "1 funding Some(1) Some(0) None position Some(0.0) Some(2.0) None orders Some(0) Some(1) None",
            "2 funding Some(1) Some(1) None position Some(0.0) Some(2.0) None orders Some(0) Some(1) None",
            "3 funding Some(1) Some(1) None position Some(0.0) Some(2.0) None orders Some(0) Some(1) None",
        ]
    );
}

#[test]
fn the_order_of_the_other_instruments_is_part_of_the_fingerprint() {
    let a_bars: Vec<Candle> = (0..4).map(|i| flat(i, 100.0)).collect();
    let others = [
        traded(&b(), (0..4).map(|i| flat(i, 50.0)).collect(), Vec::new()),
        traded(&c(), (0..4).map(|i| flat(i, 20.0)).collect(), Vec::new()),
    ];
    let mut config = config(4);
    config.also_traded = vec![b(), c()];
    let b_first = run_quiet(&config, &a_bars, &others).data_fingerprint;
    config.also_traded = vec![c(), b()];
    let c_first = run_quiet(&config, &a_bars, &others).data_fingerprint;
    assert_ne!(b_first, c_first);
}
