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
//!   strategies) with zero slippage and the case's fees, and writes the fills, trades, equity
//!   curve, funding per bar and metrics as JSON.
//! - `metrics <curve.json> <out.json>`: our metrics report for any equity curve, so both engines'
//!   curves are measured with the same formulas.
//!
//! Strategies: registered names (such as `sma_cross`) are built by the registry. The two strategies
//! only the cross-check needs, `crosscheck_buy_and_hold` and `crosscheck_short_and_hold`, live in
//! this file and run through `run_strategy`, so they never appear in the apps' strategy list.
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
    Candle, FeeSchedule, Instrument, InstrumentId, InstrumentKind, Millis, OrderRequest, OrderType,
    Side, TimeInForce, Timeframe, Venue,
};
use td_data::{Http, Store, download_candles, download_funding, fetch_instruments};
use td_engine::metrics::{self, EquitySeries};
use td_engine::{
    BacktestConfig, Context, EquityPoint, Params, SessionConfig, Strategy, run, run_backtest,
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

/// The harness's own strategy for `name`, or `None` for a registered one.
fn harness_strategy(name: &str, params: &Params) -> Res<Option<Box<dyn Strategy>>> {
    let side = match name {
        "crosscheck_buy_and_hold" => Side::Buy,
        "crosscheck_short_and_hold" => Side::Sell,
        _ => return Ok(None),
    };
    Ok(Some(Box::new(Hold {
        side,
        fraction: params.float("fraction")?,
        sent: false,
    })))
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
    };
    let result = match harness_strategy(&case.strategy, &case.params)? {
        Some(strategy) => run_strategy(&config, strategy, &bars, &[], &funding)?,
        None => run(&config, &bars, &[], &funding)?,
    };

    // The result has the run's funding total; the funding of each bar comes from the same session
    // fed the same traded bars, checked to have made the same fills.
    let traded: Vec<Candle> = bars
        .iter()
        .filter(|b| (from..to).contains(&b.open_time))
        .copied()
        .collect();
    let session = run_backtest(
        build(&case)?,
        instrument,
        case.timeframe,
        &traded,
        &[],
        &funding,
        SessionConfig {
            starting_cash: case.starting_cash,
            slippage_bps: 0.0,
            fees: Some(case.fees()),
            max_leverage: case.max_leverage,
        },
    )?;
    if session.fills() != result.fills.as_slice() {
        return Err("the session's fills differ from the backtest's".into());
    }
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
        "fills": result.fills.iter().map(|f| json!({
            "time": f.time,
            "side": f.side,
            "qty": f.qty,
            "price": f.price,
            "fee": f.fee,
            "liquidity": f.liquidity,
        })).collect::<Vec<_>>(),
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
    let report = metrics::report(&series, &[], &[], &[], 0.0);
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
