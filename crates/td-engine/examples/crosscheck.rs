//! Our side of the cross-check against NautilusTrader (issue #29, `harness/crosscheck/`).
//!
//! Three commands, each driven by the Python harness:
//!
//! - `fetch <case.json> <harness dir>`: downloads the case's 1-minute candles (and funding for a
//!   perpetual) with td-data into the harness's own store, `<harness dir>/data/store`, never the
//!   user's default store, then exports the resampled bars, a gapless copy of them and the funding
//!   stamps snapped to the bar boundary to `<harness dir>/<case.data>/`, with the instrument and a
//!   `meta.json` of what the data layer dropped or moved.
//! - `run <case.json> <harness dir> <real|gapless> <out.json>`: runs the case's backtest on the
//!   exported bars through the engine's public API (`run`, or `run_strategy` for the harness's own
//!   strategies) with zero slippage and the case's fees, and writes the fills (each with its
//!   order), every order, the orders resting before each bar, the trades, equity curve, funding
//!   per bar and metrics as JSON.
//! - `metrics <curve.json> <out.json>`: our metrics report for any equity curve, so both engines'
//!   curves are measured with the same formulas.
//!
//! Strategies: registered names (such as `sma_cross`) are built by the registry. The strategies
//! only the cross-check needs live in this file and run through `run_strategy`, so they never
//! appear in the apps' strategy list: `crosscheck_buy_and_hold` and `crosscheck_short_and_hold`
//! (cases 1 to 4), `crosscheck_limit_revert` (case 7), `crosscheck_bracket` (case 8),
//! `crosscheck_stop_limit_breakout` (case 9) and `crosscheck_sma_limit` (case 10). Each has a
//! twin in `nautilus_side.py`.
//!
//! Run it with `cargo run -p td-engine --example crosscheck -- <command> ...`; the harness's
//! `run.py` does that for every case.

use std::collections::BTreeMap;
use std::error::Error;
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use td_core::{
    AccountCurrency, Candle, FeeSchedule, Instrument, InstrumentId, InstrumentKind, Liquidity,
    Millis, OrderId, OrderRequest, OrderType, Side, TimeInForce, Timeframe, Venue,
};
use td_data::{Http, Store, download_candles, download_funding, fetch_instruments};
use td_engine::metrics::{self, EquitySeries};
use td_engine::{
    BacktestConfig, Context, EquityPoint, Params, Session, SessionConfig, Strategy, run,
    run_strategy,
};

type Res<T> = Result<T, Box<dyn Error>>;

/// One cross-check case, as `harness/crosscheck/cases/*.json` gives it.
#[derive(Debug, Clone, Deserialize)]
struct Case {
    name: String,
    strategy: String,
    #[serde(default)]
    params: Params,
    venue: Venue,
    kind: InstrumentKind,
    symbol: String,
    timeframe: Timeframe,
    /// First day, `YYYY-MM-DD` UTC.
    from: String,
    /// Day after the last, `YYYY-MM-DD` UTC: bars open in `[from, to)`.
    to: String,
    starting_cash: f64,
    fees: Fees,
    #[serde(default = "one")]
    max_leverage: f64,
    /// The export folder, relative to the harness directory.
    data: String,
}

#[derive(Debug, Clone, Copy, Deserialize)]
struct Fees {
    maker: f64,
    taker: f64,
}

fn one() -> f64 {
    1.0
}

impl Case {
    fn load(path: &Path) -> Res<Case> {
        let text = fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
        Ok(serde_json::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))?)
    }

    fn id(&self) -> InstrumentId {
        InstrumentId {
            venue: self.venue,
            symbol: self.symbol.clone(),
            kind: self.kind,
        }
    }

    fn range(&self) -> Res<(Millis, Millis)> {
        Ok((
            td_data::time::parse_date(&self.from)?,
            td_data::time::parse_date(&self.to)?,
        ))
    }

    fn fees(&self) -> FeeSchedule {
        FeeSchedule {
            maker: self.fees.maker,
            taker: self.fees.taker,
            per_unit: 0.0,
        }
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let outcome = match args.as_slice() {
        ["fetch", case, dir] => fetch(Path::new(case), Path::new(dir)),
        ["run", case, dir, series, out] => {
            run_case(Path::new(case), Path::new(dir), series, Path::new(out))
        }
        ["metrics", curve, out] => curve_metrics(Path::new(curve), Path::new(out)),
        _ => Err("usage: crosscheck fetch <case.json> <harness dir>\n       \
                  crosscheck run <case.json> <harness dir> <real|gapless> <out.json>\n       \
                  crosscheck metrics <curve.json> <out.json>"
            .into()),
    };
    if let Err(e) = outcome {
        eprintln!("crosscheck: {e}");
        std::process::exit(1);
    }
}

// ---------------------------------------------------------------------------------------------
// fetch
// ---------------------------------------------------------------------------------------------

fn fetch(case_path: &Path, dir: &Path) -> Res<()> {
    let case = Case::load(case_path)?;
    let id = case.id();
    let (from, to) = case.range()?;
    let tf = case.timeframe;
    // The harness's own store: never `Store::open_default`, so the user's data is not touched.
    let store = Store::new(dir.join("data").join("store"));
    let http = Http::new()?;
    let out = dir.join(&case.data);
    fs::create_dir_all(&out)?;

    let listed = fetch_instruments(&http, id.venue, id.kind)?;
    let instrument = listed
        .instruments
        .into_iter()
        .find(|i| i.id == id)
        .ok_or_else(|| format!("{id} is not listed by the venue"))?;
    fs::write(
        out.join("instrument.json"),
        serde_json::to_string_pretty(&instrument)?,
    )?;

    // Held across processes through the downloads and reads, as td-data and the server hold it.
    let _hold = store.hold_for_download(&id)?;
    let report = download_candles(&http, &store, &id, from, to, &mut |line| {
        eprintln!("{line}")
    })?;
    let loaded = store.load_candles(&id, tf, from, to, false)?;
    let bars = loaded.candles;
    if bars.is_empty() {
        return Err(format!("no {tf} bars for {id} in {}..{}", case.from, case.to).into());
    }
    write_bars(&out.join("bars_real.csv"), &bars)?;
    write_bars(&out.join("bars_gapless.csv"), &gapless(&bars))?;

    let mut funding = Vec::new();
    let mut snapped_moved = Vec::new();
    if id.kind == InstrumentKind::Perpetual {
        download_funding(&http, &store, &id, from, to)?;
        for (time, rate) in store.load_funding(&id, from, to)? {
            let snapped = snap(tf, time);
            if snapped != time {
                snapped_moved.push(json!({ "raw": time, "snapped": snapped }));
            }
            funding.push((snapped, rate, time));
        }
    }
    let mut csv = String::from("time,rate,raw_time\n");
    for (time, rate, raw) in &funding {
        writeln!(csv, "{time},{rate},{raw}")?;
    }
    fs::write(out.join("funding.csv"), csv)?;

    let off_tick = bars
        .iter()
        .flat_map(|b| [b.open, b.high, b.low, b.close])
        .filter(|&p| instrument.round_price(p, td_core::Rounding::Nearest) != p)
        .count();
    let meta = json!({
        "instrument": id.to_string(),
        "timeframe": tf,
        "from": from,
        "to": to,
        "bars": bars.len(),
        "first_bar": bars.first().map(|b| b.open_time),
        "last_bar": bars.last().map(|b| b.open_time),
        "dropped_buckets": loaded.dropped,
        "tolerated_buckets": loaded.tolerated,
        "missing_minutes_after_download": report.gaps.iter().map(|g| g.minutes()).sum::<i64>(),
        "missing_files": report.missing_files,
        "funding_stamps": funding.len(),
        "funding_stamps_moved_by_snapping": snapped_moved,
        "bar_prices_off_the_current_tick": off_tick,
        "tick_size": instrument.tick_size,
        "lot_size": instrument.lot_size,
    });
    fs::write(out.join("meta.json"), serde_json::to_string_pretty(&meta)?)?;
    eprintln!(
        "{}: {} bars, {} dropped, {} funding stamps",
        case.name,
        bars.len(),
        loaded.dropped.len(),
        funding.len()
    );
    Ok(())
}

/// The nearest bar boundary of `timeframe` to `t`.
fn snap(timeframe: Timeframe, t: Millis) -> Millis {
    timeframe.floor(t + timeframe.duration_ms() / 2)
}

/// The bars with each open set to the previous close, the high and low widened to contain it, so
/// a market order filled at the next open fills at the close it was decided on.
fn gapless(bars: &[Candle]) -> Vec<Candle> {
    let mut out = bars.to_vec();
    for t in 1..out.len() {
        let open = out[t - 1].close;
        let bar = &mut out[t];
        bar.open = open;
        bar.high = bar.high.max(open);
        bar.low = bar.low.min(open);
    }
    out
}

fn write_bars(path: &Path, bars: &[Candle]) -> Res<()> {
    let mut csv = String::from("open_time,open,high,low,close,volume,quote_volume\n");
    for b in bars {
        writeln!(
            csv,
            "{},{},{},{},{},{},{}",
            b.open_time, b.open, b.high, b.low, b.close, b.volume, b.quote_volume
        )?;
    }
    Ok(fs::write(path, csv)?)
}

fn read_bars(path: &Path) -> Res<Vec<Candle>> {
    let text = fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut bars = Vec::new();
    for (n, line) in text.lines().enumerate().skip(1) {
        let f: Vec<&str> = line.split(',').collect();
        let bad = || format!("{} line {}: {line:?}", path.display(), n + 1);
        if f.len() != 7 {
            return Err(bad().into());
        }
        let num = |i: usize| f[i].parse::<f64>().map_err(|_| bad());
        bars.push(Candle {
            open_time: f[0].parse().map_err(|_| bad())?,
            open: num(1)?,
            high: num(2)?,
            low: num(3)?,
            close: num(4)?,
            volume: num(5)?,
            quote_volume: num(6)?,
        });
    }
    Ok(bars)
}

/// For each bar, the range of `funding` (sorted by time) the session charges with it, as
/// `run_backtest` splits it: the stamps inside the bar's window and, from the second bar on, those
/// in the gap since the previous bar's end. Stamps before the first bar are not charged.
fn funding_ranges(
    timeframe: Timeframe,
    bars: &[Candle],
    funding: &[(Millis, f64)],
) -> Vec<std::ops::Range<usize>> {
    let mut next = 0;
    if let Some(first) = bars.first() {
        while next < funding.len() && funding[next].0 < first.open_time {
            next += 1;
        }
    }
    bars.iter()
        .map(|bar| {
            let end = bar.open_time + timeframe.duration_ms();
            let start = next;
            while next < funding.len() && funding[next].0 < end {
                next += 1;
            }
            start..next
        })
        .collect()
}

fn read_funding(path: &Path) -> Res<Vec<(Millis, f64)>> {
    let text = fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut rates = Vec::new();
    for (n, line) in text.lines().enumerate().skip(1) {
        let f: Vec<&str> = line.split(',').collect();
        let bad = || format!("{} line {}: {line:?}", path.display(), n + 1);
        if f.len() != 3 {
            return Err(bad().into());
        }
        rates.push((
            f[0].parse().map_err(|_| bad())?,
            f[1].parse().map_err(|_| bad())?,
        ));
    }
    Ok(rates)
}

// ---------------------------------------------------------------------------------------------
// run
// ---------------------------------------------------------------------------------------------

/// Buys (or shorts) once at the first bar's close with `fraction` of equity, net of the taker fee,
/// then holds to the end. The quantity rounds down to the lot when submitted.
#[derive(Debug, Clone)]
struct Hold {
    side: Side,
    fraction: f64,
    sent: bool,
}

impl Strategy for Hold {
    fn on_bar(&mut self, ctx: &mut Context, bar: &Candle) {
        if self.sent || ctx.is_warming_up() {
            return;
        }
        self.sent = true;
        let qty = ctx.equity() * self.fraction / (bar.close * (1.0 + ctx.fees().taker));
        let request = OrderRequest {
            instrument: ctx.instrument().id.clone(),
            side: self.side,
            qty,
            order_type: OrderType::Market,
            tif: TimeInForce::Gtc,
            reduce_only: false,
            client_tag: None,
        };
        // A rejected order is recorded in the result's orders; the comparison reports it.
        let _ = ctx.submit(request);
    }
}

fn request(
    ctx: &Context,
    side: Side,
    qty: f64,
    order_type: OrderType,
    reduce_only: bool,
) -> OrderRequest {
    OrderRequest {
        instrument: ctx.instrument().id.clone(),
        side,
        qty,
        order_type,
        tif: TimeInForce::Gtc,
        reduce_only,
        client_tag: None,
    }
}

/// The mean of the last `n` closes, summed oldest first.
fn sma(bars: &[Candle], n: usize) -> f64 {
    bars[bars.len() - n..].iter().map(|b| b.close).sum::<f64>() / n as f64
}

/// The simple mean of the true range over the last `n` bars, as `donchian_breakout` computes it;
/// needs `n + 1` bars.
fn atr(bars: &[Candle], n: usize) -> f64 {
    let tail = &bars[bars.len() - n - 1..];
    let sum: f64 = tail
        .windows(2)
        .map(|w| {
            let (prev, b) = (w[0].close, &w[1]);
            (b.high - b.low)
                .max((b.high - prev).abs())
                .max((b.low - prev).abs())
        })
        .sum();
    sum / n as f64
}

/// Whether the order `id` is still open.
fn is_open(ctx: &Context, id: OrderId) -> bool {
    ctx.open_orders().iter().any(|o| o.id == id)
}

/// What became of an order a strategy keeps track of.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Tracked {
    /// Still open, and younger than its time limit: kept.
    Resting,
    /// Still open after `cancel_after` bars: cancelled.
    Expired,
    /// No order tracked, or it filled or was cancelled.
    Gone,
}

/// Checks the order in `slot`, sent on the bar where `ctx.bars()` had `sent` bars, at a decision
/// with `n` bars: cancels it once it has rested `cancel_after` bars, and empties the slot unless
/// it is still resting.
fn track(
    ctx: &mut Context,
    slot: &mut Option<(OrderId, usize)>,
    n: usize,
    cancel_after: usize,
) -> Tracked {
    let Some((id, sent)) = slot.take() else {
        return Tracked::Gone;
    };
    if !is_open(ctx, id) {
        return Tracked::Gone;
    }
    if n - sent < cancel_after {
        *slot = Some((id, sent));
        return Tracked::Resting;
    }
    ctx.cancel(id);
    Tracked::Expired
}

/// Cancels every open order. The strategies below call it when flat, so an exit left over from
/// the trade just closed (a take-profit after its stop filled, or the stop after its take-profit)
/// never rests on: Nautilus cancels such reduce-only orders itself once the position is flat.
fn cancel_all(ctx: &mut Context) {
    let ids: Vec<OrderId> = ctx.open_orders().iter().map(|o| o.id).collect();
    for id in ids {
        ctx.cancel(id);
    }
}

/// Case 7: a limit entry and a limit exit, each cancelled and sent again at the latest close once
/// it has rested `cancel_after` bars unfilled. Flat, it rests a buy `offset` below the close for
/// `fraction` of the cash (net of the taker fee, as the cash check values it); long, a reduce-only
/// sell `offset` above the close for the whole position.
#[derive(Debug, Clone)]
struct LimitRevert {
    offset: f64,
    cancel_after: usize,
    fraction: f64,
    order: Option<(OrderId, usize)>,
}

impl Strategy for LimitRevert {
    fn on_bar(&mut self, ctx: &mut Context, bar: &Candle) {
        if ctx.is_warming_up() {
            return;
        }
        let n = ctx.bars().len();
        if track(ctx, &mut self.order, n, self.cancel_after) == Tracked::Resting {
            return;
        }
        let position = ctx.position();
        let req = if position == 0.0 {
            let price = bar.close * (1.0 - self.offset);
            let qty = ctx.cash() * self.fraction / (price * (1.0 + ctx.fees().taker));
            request(ctx, Side::Buy, qty, OrderType::Limit { price }, false)
        } else {
            let price = bar.close * (1.0 + self.offset);
            request(ctx, Side::Sell, position, OrderType::Limit { price }, true)
        };
        if let Ok(id) = ctx.submit(req) {
            self.order = Some((id, n));
        }
    }
}

/// Case 8: flat, it enters at market in the direction of the `fast`/`slow` moving averages with a
/// protective stop `stop_atr` ATRs from the close attached, and rests a reduce-only take-profit
/// limit `target_atr` ATRs from the close on the other side; then it waits for one of them. The
/// quantity is fixed, so a trade closed differently by the two engines (on a bar where both exits
/// are reachable, #29's difference 2) does not change the size of any later order.
#[derive(Debug, Clone)]
struct Bracket {
    fast: usize,
    slow: usize,
    atr_period: usize,
    stop_atr: f64,
    target_atr: f64,
    qty: f64,
}

impl Strategy for Bracket {
    fn on_bar(&mut self, ctx: &mut Context, bar: &Candle) {
        let bars = ctx.bars();
        if ctx.is_warming_up() || bars.len() < self.slow.max(self.atr_period + 1) {
            return;
        }
        if ctx.position() != 0.0 {
            return;
        }
        let (fast, slow) = (sma(bars, self.fast), sma(bars, self.slow));
        let atr = atr(bars, self.atr_period);
        cancel_all(ctx);
        if fast == slow || atr <= 0.0 {
            return;
        }
        let (side, exit_side, sign) = if fast > slow {
            (Side::Buy, Side::Sell, 1.0)
        } else {
            (Side::Sell, Side::Buy, -1.0)
        };
        let stop = bar.close - sign * self.stop_atr * atr;
        let target = bar.close + sign * self.target_atr * atr;
        let entry = request(ctx, side, self.qty, OrderType::Market, false);
        if ctx.submit_with_stop(entry, stop).is_ok() {
            let exit = request(
                ctx,
                exit_side,
                self.qty,
                OrderType::Limit { price: target },
                true,
            );
            let _ = ctx.submit(exit);
        }
    }
}

/// Case 9: flat, it rests a stop-limit breakout entry for a fixed quantity: above the highest high
/// of the last `channel` bars while the close is above its `trend` moving average (a buy), below
/// the lowest low while it is under it (a sell). The trigger sits `trigger_gap` beyond that high
/// or low, so it is never in the market when sent, and the limit `limit_offset` beyond the trigger.
/// An entry still resting after `cancel_after` bars is cancelled and placed again. In a position,
/// it exits at market `hold` bars after the bar its entry filled on.
#[derive(Debug, Clone)]
struct StopLimitBreakout {
    channel: usize,
    trend: usize,
    trigger_gap: f64,
    limit_offset: f64,
    cancel_after: usize,
    hold: usize,
    qty: f64,
    entry: Option<(OrderId, usize)>,
    held: usize,
}

impl Strategy for StopLimitBreakout {
    fn on_bar(&mut self, ctx: &mut Context, bar: &Candle) {
        let bars = ctx.bars();
        let n = bars.len();
        if ctx.is_warming_up() || n < self.channel.max(self.trend) {
            return;
        }
        let channel = &bars[n - self.channel..];
        let high = channel.iter().map(|b| b.high).fold(f64::MIN, f64::max);
        let low = channel.iter().map(|b| b.low).fold(f64::MAX, f64::min);
        let above_trend = bar.close > sma(bars, self.trend);
        let position = ctx.position();
        if position != 0.0 {
            self.entry = None;
            self.held += 1;
            if self.held >= self.hold {
                let side = if position > 0.0 {
                    Side::Sell
                } else {
                    Side::Buy
                };
                let exit = request(ctx, side, position.abs(), OrderType::Market, true);
                let _ = ctx.submit(exit);
            }
            return;
        }
        self.held = 0;
        if track(ctx, &mut self.entry, n, self.cancel_after) == Tracked::Resting {
            return;
        }
        let (side, trigger, sign) = if above_trend {
            (Side::Buy, high * (1.0 + self.trigger_gap), 1.0)
        } else {
            (Side::Sell, low * (1.0 - self.trigger_gap), -1.0)
        };
        let price = trigger * (1.0 + sign * self.limit_offset);
        let entry = request(
            ctx,
            side,
            self.qty,
            OrderType::StopLimit { trigger, price },
            false,
        );
        if let Ok(id) = ctx.submit(entry) {
            self.entry = Some((id, n));
        }
    }
}

/// Case 10: `sma_cross`'s position (long above, short below, sized as `sma_cross` sizes it), but
/// reached with a resting limit `offset` beyond the close instead of a market order: below it for
/// a buy, above it for a sell. A limit still resting after `cancel_after` bars is cancelled, and
/// the next decision sends a new one at the latest close.
#[derive(Debug, Clone)]
struct SmaLimit {
    fast: usize,
    slow: usize,
    equity_fraction: f64,
    offset: f64,
    cancel_after: usize,
    order: Option<(OrderId, usize)>,
}

impl Strategy for SmaLimit {
    fn on_bar(&mut self, ctx: &mut Context, bar: &Candle) {
        let bars = ctx.bars();
        let n = bars.len();
        if ctx.is_warming_up() || n < self.slow {
            return;
        }
        let (fast, slow) = (sma(bars, self.fast), sma(bars, self.slow));
        if track(ctx, &mut self.order, n, self.cancel_after) != Tracked::Gone {
            return;
        }
        let can_short = ctx.instrument().id.kind.rules().can_short;
        let want = if fast > slow {
            1.0
        } else if fast < slow && can_short {
            -1.0
        } else {
            0.0
        };
        let position = ctx.position();
        let held = if position > 0.0 {
            1.0
        } else if position < 0.0 {
            -1.0
        } else {
            0.0
        };
        if held == want {
            return;
        }
        let equity = ctx.equity();
        let costs = 1.0 + ctx.fees().taker + ctx.slippage_bps() / 10_000.0;
        // `affordable_qty` of the reference strategies, with their 0.5% headroom.
        let affordable = equity * (1.0 - 0.005) / (bar.close * costs);
        let size = (equity * self.equity_fraction / bar.close).min(affordable);
        let delta = want * size - position;
        let (side, price) = if delta > 0.0 {
            (Side::Buy, bar.close * (1.0 - self.offset))
        } else {
            (Side::Sell, bar.close * (1.0 + self.offset))
        };
        let req = request(ctx, side, delta.abs(), OrderType::Limit { price }, false);
        if let Ok(id) = ctx.submit(req) {
            self.order = Some((id, n));
        }
    }
}

/// The harness's own strategy for `name`, or `None` for a registered one.
fn harness_strategy(name: &str, p: &Params) -> Res<Option<Box<dyn Strategy>>> {
    let strategy: Box<dyn Strategy> = match name {
        "crosscheck_buy_and_hold" | "crosscheck_short_and_hold" => Box::new(Hold {
            side: if name == "crosscheck_buy_and_hold" {
                Side::Buy
            } else {
                Side::Sell
            },
            fraction: p.float("fraction")?,
            sent: false,
        }),
        "crosscheck_limit_revert" => Box::new(LimitRevert {
            offset: p.float("offset")?,
            cancel_after: p.len("cancel_after")?,
            fraction: p.float("fraction")?,
            order: None,
        }),
        "crosscheck_bracket" => Box::new(Bracket {
            fast: p.len("fast")?,
            slow: p.len("slow")?,
            atr_period: p.len("atr_period")?,
            stop_atr: p.float("stop_atr")?,
            target_atr: p.float("target_atr")?,
            qty: p.float("qty")?,
        }),
        "crosscheck_stop_limit_breakout" => Box::new(StopLimitBreakout {
            channel: p.len("channel")?,
            trend: p.len("trend")?,
            trigger_gap: p.float("trigger_gap")?,
            limit_offset: p.float("limit_offset")?,
            cancel_after: p.len("cancel_after")?,
            hold: p.len("hold")?,
            qty: p.float("qty")?,
            entry: None,
            held: 0,
        }),
        "crosscheck_sma_limit" => Box::new(SmaLimit {
            fast: p.len("fast")?,
            slow: p.len("slow")?,
            equity_fraction: p.float("equity_fraction")?,
            offset: p.float("offset")?,
            cancel_after: p.len("cancel_after")?,
            order: None,
        }),
        _ => return Ok(None),
    };
    Ok(Some(strategy))
}

fn build(case: &Case) -> Res<Box<dyn Strategy>> {
    match harness_strategy(&case.strategy, &case.params)? {
        Some(s) => Ok(s),
        None => Ok(td_engine::build_strategy(&case.strategy, &case.params)?),
    }
}

fn run_case(case_path: &Path, dir: &Path, series: &str, out: &Path) -> Res<()> {
    let case = Case::load(case_path)?;
    let data: PathBuf = dir.join(&case.data);
    let bars_file = match series {
        "real" => "bars_real.csv",
        "gapless" => "bars_gapless.csv",
        _ => return Err(format!("series must be real or gapless, got {series:?}").into()),
    };
    let bars = read_bars(&data.join(bars_file))?;
    let funding = if case.kind == InstrumentKind::Perpetual {
        read_funding(&data.join("funding.csv"))?
    } else {
        Vec::new()
    };
    let instrument: Instrument =
        serde_json::from_str(&fs::read_to_string(data.join("instrument.json"))?)?;
    if instrument.id != case.id() {
        return Err(format!("instrument.json is {}, not {}", instrument.id, case.id()).into());
    }
    let (from, to) = case.range()?;
    let config = BacktestConfig {
        strategy: case.strategy.clone(),
        params: case.params.clone(),
        instrument: instrument.clone(),
        timeframe: case.timeframe,
        from,
        to,
        starting_cash: case.starting_cash,
        slippage_bps: 0.0,
        fee_override: Some(case.fees()),
        max_leverage: case.max_leverage,
        warmup_from: None,
        close_at_end: false,
        account_currency: AccountCurrency::Quote,
    };
    let result = match harness_strategy(&case.strategy, &case.params)? {
        Some(strategy) => run_strategy(&config, strategy, &bars, &[], &funding)?,
        None => run(&config, &bars, &[], &funding)?,
    };

    // The result has the run's funding total; the funding of each bar, and the orders resting
    // before each bar, come from the same session fed the same traded bars one at a time, as
    // `run_backtest` feeds them, checked to have made the same fills and paid the same funding.
    let traded: Vec<Candle> = bars
        .iter()
        .filter(|b| (from..to).contains(&b.open_time))
        .copied()
        .collect();
    let mut funding_sorted = funding.clone();
    funding_sorted.sort_by_key(|&(time, _)| time);
    let mut session = Session::new(
        build(&case)?,
        instrument,
        case.timeframe,
        SessionConfig {
            starting_cash: case.starting_cash,
            slippage_bps: 0.0,
            fees: Some(case.fees()),
            max_leverage: case.max_leverage,
            account_currency: AccountCurrency::Quote,
        },
    )?;
    let mut resting = Vec::new();
    for (bar, range) in traded
        .iter()
        .zip(funding_ranges(case.timeframe, &traded, &funding_sorted))
    {
        let open = session.open_orders();
        if !open.is_empty() {
            resting.push(json!({ "bar": bar.open_time, "orders": open }));
        }
        session.on_bar(*bar, &funding_sorted[range])?;
    }
    if session.fills() != result.fills.as_slice() {
        return Err("the session's fills differ from the backtest's".into());
    }
    let session_funding: f64 = session
        .bar_funding()
        .iter()
        .map(|f| f.at_open + f.in_bar)
        .sum();
    if (session_funding - result.metrics.funding_paid).abs()
        > 1e-9 * (1.0 + result.metrics.funding_paid.abs())
    {
        return Err(format!(
            "the session's funding {session_funding} differs from the backtest's {}",
            result.metrics.funding_paid
        )
        .into());
    }
    let orders = session.orders();
    let funding_by_bar: Vec<Value> = traded
        .iter()
        .zip(session.bar_funding())
        .filter(|(_, f)| f.at_open != 0.0 || f.in_bar != 0.0)
        .map(|(b, f)| json!({ "bar": b.open_time, "at_open": f.at_open, "in_bar": f.in_bar }))
        .collect();

    let rejected: Vec<&td_engine::Order> = result
        .orders
        .iter()
        .filter(|o| matches!(o.status, td_engine::OrderStatus::Rejected(_)))
        .collect();
    let output = json!({
        "engine": "trading-desk",
        "case": case.name,
        "series": series,
        "bars": traded.len(),
        "fills": result.fills.iter().map(|f| {
            let order = &orders[f.order_id.0 as usize - 1];
            // Filled at the price the decision met: a market order, or a limit already marketable
            // at its decision close, which fills as a taker on its first bar. Every other fill
            // comes from an order resting on the bar it fills on.
            let at_decision = match order.request.order_type {
                OrderType::Market => true,
                OrderType::Limit { .. } => f.liquidity == Liquidity::Taker,
                _ => false,
            };
            json!({
                "time": f.time,
                "side": f.side,
                "qty": f.qty,
                "price": f.price,
                "fee": f.fee,
                "liquidity": f.liquidity,
                "order_id": f.order_id,
                "order_type": order.request.order_type,
                "reduce_only": order.request.reduce_only,
                "at_decision": at_decision,
            })
        }).collect::<Vec<_>>(),
        "orders": orders,
        "resting": resting,
        "rejected_orders": rejected,
        "trades": result.trades,
        "equity": result.equity,
        "funding": funding_by_bar,
        "funding_paid": result.metrics.funding_paid,
        "metrics": result.metrics,
    });
    fs::write(out, serde_json::to_string_pretty(&output)?)?;
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// metrics
// ---------------------------------------------------------------------------------------------

/// An equity curve to measure: one point per bar, stamped with the bar's open time.
#[derive(Debug, Deserialize, Serialize)]
struct Curve {
    starting_cash: f64,
    timeframe: Timeframe,
    equity: Vec<EquityPoint>,
}

fn curve_metrics(curve_path: &Path, out: &Path) -> Res<()> {
    let curve: Curve = serde_json::from_str(&fs::read_to_string(curve_path)?)?;
    let series = EquitySeries::new(curve.starting_cash, &curve.equity, curve.timeframe);
    let report = metrics::report(&series, &[], &[], &[], 0.0, AccountCurrency::Quote);
    // Only what the curve alone decides: the trade, fee and exposure figures need the fills.
    let full = serde_json::to_value(&report)?;
    let keep = [
        "start_equity",
        "end_equity",
        "total_return",
        "cagr",
        "volatility",
        "sharpe",
        "sortino",
        "max_drawdown",
        "max_drawdown_duration_ms",
        "max_drawdown_duration_bars",
        "calmar",
        "static_drawdown",
        "worst_daily_loss",
    ];
    let picked: BTreeMap<&str, &Value> = keep.iter().map(|k| (*k, &full[*k])).collect();
    fs::write(out, serde_json::to_string_pretty(&picked)?)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bar(open_time: Millis, open: f64, high: f64, low: f64, close: f64) -> Candle {
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
    fn gapless_opens_at_the_previous_close_and_widens_the_range() {
        let bars = [
            bar(0, 10.0, 12.0, 9.0, 11.0),
            bar(1, 13.0, 14.0, 12.5, 13.5),
            bar(2, 13.0, 13.2, 12.0, 12.1),
        ];
        let g = gapless(&bars);
        assert_eq!(g[0], bars[0]);
        assert_eq!((g[1].open, g[1].high, g[1].low), (11.0, 14.0, 11.0));
        assert_eq!((g[2].open, g[2].high, g[2].low), (13.5, 13.5, 12.0));
        assert_eq!(g[2].close, 12.1);
    }

    #[test]
    fn funding_snaps_to_the_nearest_boundary() {
        let h = 3_600_000;
        assert_eq!(snap(Timeframe::H1, 8 * h + 1_000), 8 * h);
        assert_eq!(snap(Timeframe::H1, 8 * h - 60_000), 8 * h);
        assert_eq!(snap(Timeframe::H4, 8 * h), 8 * h);
    }
}
