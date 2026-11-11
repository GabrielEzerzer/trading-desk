//! Times the engine's real work on synthetic data, so a speed change can be measured.
//!
//! Reads the frozen workloads in `harness/evals/perf-workloads.json`, writes each workload's
//! synthetic 1-minute candles (and a perpetual's funding) through the real [`Store`] into a fresh
//! temporary directory, removed when the run ends, then times each workload `--reps` times: for
//! `load`, the data check and load a run makes; for `lab_backtest`, the whole of a Strategy Lab
//! backtest as the server does it (the coverage read the Lab asks for first, check, load, run,
//! save, the job-result read, and the saved-run read as a later session makes it); for the
//! others, only the engine call, on data loaded beforehand. Prints one `WORKLOAD`
//! line per workload, a `PHASES` line per `lab_backtest` workload, and one `BENCH` line to stdout;
//! progress goes to stderr. A workload whose result differs between repetitions stops the run
//! with a non-zero exit.

use std::fs;
use std::ops::ControlFlow;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Instant;

use clap::{Parser, ValueEnum};
use serde::{Deserialize, Serialize};
use td_core::{
    Candle, Instrument, InstrumentId, InstrumentKind, Millis, Rounding, Timeframe, Venue,
};
use td_data::Store;
use td_data::binance::sha256_hex;
use td_data::time::{MINUTE, Month};
use td_engine::{
    BacktestConfig, BacktestResult, EngineError, Objective, ParamAxis, Params, SavedRun, SweepMode,
    SweepResult, SweepSpec, TesterError, WalkForwardResult, WalkForwardSpec, WindowSpec,
};
use td_server::api::data::read_coverage;
use td_server::api::error::ApiError;
use td_server::api::jobs::JobError;
use td_server::api::runs::{
    RunData, RunDone, check_run, check_run_data, load_run_data, read_run, save_run,
};

/// Every dataset starts at 2022-01-01T00:00Z.
const DATASET_START: Month = Month {
    year: 2022,
    month: 1,
};

#[derive(Parser)]
#[command(about = "Times the engine's real work on synthetic data")]
struct Args {
    /// Which workloads to run.
    #[arg(long, value_enum, default_value_t = SplitArg::Dev)]
    split: SplitArg,
    /// Repetitions of each workload.
    #[arg(long, default_value_t = 5)]
    reps: usize,
    /// Run only workloads whose name contains this.
    #[arg(long)]
    filter: Option<String>,
    /// The workloads file (default: `harness/evals/perf-workloads.json` in the workspace).
    #[arg(long)]
    workloads: Option<PathBuf>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum SplitArg {
    Dev,
    Test,
    All,
}

impl SplitArg {
    fn as_str(self) -> &'static str {
        match self {
            SplitArg::Dev => "dev",
            SplitArg::Test => "test",
            SplitArg::All => "all",
        }
    }
}

#[derive(Debug, Deserialize)]
struct WorkloadFile {
    workloads: Vec<Workload>,
}

#[derive(Debug, Clone, Deserialize)]
struct Workload {
    name: String,
    split: String,
    seed: u64,
    kind: InstrumentKind,
    months: u32,
    timeframe: Timeframe,
    strategy: String,
    #[serde(default)]
    params: Params,
    starting_cash: f64,
    slippage_bps: f64,
    #[serde(flatten)]
    mode: Mode,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
enum Mode {
    Load,
    Backtest,
    Sweep {
        sweep: SweepSettings,
    },
    WalkForward {
        sweep: SweepSettings,
        windows: WindowSpec,
    },
    /// A Strategy Lab backtest: the strategy's default parameters, overlaid with `params`.
    LabBacktest,
}

#[derive(Debug, Clone, Deserialize)]
struct SweepSettings {
    axes: Vec<ParamAxis>,
    objective: Objective,
    min_trades: usize,
}

/// One workload's timings and the checksum of its result.
#[derive(Debug, Clone, PartialEq)]
struct Measured {
    min_ms: f64,
    /// The first repetition's time. Far above `min_ms`, it says later repetitions reused
    /// something the first one had to make.
    first_ms: f64,
    median_ms: f64,
    /// SHA-256 of the result's JSON, in hex; the same on every repetition.
    checksum: String,
    /// For `lab_backtest`, each phase's time in the fastest repetition.
    phases: Option<Phases>,
}

/// The phases of a `lab_backtest` repetition, in milliseconds, in the order they run.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
struct Phases {
    coverage: f64,
    check: f64,
    load: f64,
    run: f64,
    save: f64,
    result: f64,
    open: f64,
}

impl Phases {
    fn total(&self) -> f64 {
        self.coverage + self.check + self.load + self.run + self.save + self.result + self.open
    }
}

fn main() -> ExitCode {
    let args = Args::parse();
    match bench_all(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

fn bench_all(args: &Args) -> Result<(), String> {
    if args.reps == 0 {
        return Err("--reps must be at least 1".into());
    }
    let path = args.workloads.clone().unwrap_or_else(|| {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../harness/evals/perf-workloads.json")
    });
    let text = fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    let file: WorkloadFile =
        serde_json::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))?;
    // Fresh for every run, so no dataset an older binary wrote is ever timed; removed when the
    // run ends, with an error too.
    let data_dir = tempfile::Builder::new()
        .prefix("td-bench-data-")
        .tempdir()
        .map_err(|e| format!("cannot make the data directory: {e}"))?;
    let chosen: Vec<&Workload> = file
        .workloads
        .iter()
        .filter(|w| args.split == SplitArg::All || w.split == args.split.as_str())
        .filter(|w| {
            args.filter
                .as_ref()
                .is_none_or(|f| w.name.contains(f.as_str()))
        })
        .collect();
    if chosen.is_empty() {
        return Err("no workload matches the split and filter".into());
    }
    let mut lines = Vec::new();
    let mut total_ms = 0.0;
    let mut checksums = String::new();
    for w in chosen {
        let (from, to) = dataset_range(w.months);
        let store = ensure_dataset(data_dir.path(), w, from, to)?;
        eprintln!("running {} ({} reps)", w.name, args.reps);
        let m = bench(w, &store, from, to, args.reps)?;
        total_ms += m.min_ms;
        checksums.push_str(&m.checksum);
        lines.push(workload_line(w, args.reps, &m));
        if let Some(p) = &m.phases {
            lines.push(phases_line(w, p));
        }
    }
    let workloads = lines.iter().filter(|l| l.starts_with("WORKLOAD")).count();
    for line in &lines {
        println!("{line}");
    }
    println!(
        "{}",
        bench_line(args.split.as_str(), workloads, total_ms, &checksums)
    );
    Ok(())
}

fn workload_line(w: &Workload, reps: usize, m: &Measured) -> String {
    format!(
        "WORKLOAD name={} split={} reps={reps} min_ms={:.1} first_ms={:.1} median_ms={:.1} \
         checksum={}",
        w.name,
        w.split,
        m.min_ms,
        m.first_ms,
        m.median_ms,
        &m.checksum[..16]
    )
}

fn phases_line(w: &Workload, p: &Phases) -> String {
    format!(
        "PHASES name={} coverage_ms={:.1} check_ms={:.1} load_ms={:.1} run_ms={:.1} save_ms={:.1} \
         result_ms={:.1} open_ms={:.1}",
        w.name, p.coverage, p.check, p.load, p.run, p.save, p.result, p.open
    )
}

/// The summary line; `checksums` is every workload's full checksum, in file order, concatenated.
fn bench_line(split: &str, workloads: usize, total_ms: f64, checksums: &str) -> String {
    format!(
        "BENCH split={split} workloads={workloads} total_ms={total_ms:.1} checksum={}",
        &sha256_hex(checksums.as_bytes())[..16]
    )
}

/// `months` whole months from [`DATASET_START`].
fn dataset_range(months: u32) -> (Millis, Millis) {
    let end = (0..months).fold(DATASET_START, |m, _| m.next());
    (DATASET_START.start(), end.start())
}

// ---------------------------------------------------------------------------------------------
// Timing
// ---------------------------------------------------------------------------------------------

/// Runs a workload `reps` times over `from..to` and checks that every repetition gives the same
/// result.
fn bench(
    w: &Workload,
    store: &Store,
    from: Millis,
    to: Millis,
    reps: usize,
) -> Result<Measured, String> {
    let config = base_config(w, from, to);
    let mut times = Vec::with_capacity(reps);
    let mut checksum: Option<String> = None;
    let mut fastest: Option<(f64, Phases)> = None;
    // Loaded once, outside the timed section, for the modes that time only the engine.
    let data = match w.mode {
        Mode::Load | Mode::LabBacktest => None,
        _ => Some(load_run_data(store, &config).map_err(api_error)?),
    };
    for rep in 0..reps {
        // A store of its own for each repetition, as after an app start: whatever a store comes
        // to keep in memory is not carried from one repetition to the next.
        let store = &Store::new(store.root());
        let (ms, json) = match (&w.mode, &data) {
            (Mode::LabBacktest, _) => {
                let (phases, json) = lab_backtest(&lab_config(&config)?, store, rep)?;
                let ms = phases.total();
                if fastest.is_none_or(|(best, _)| ms < best) {
                    fastest = Some((ms, phases));
                }
                (ms, json)
            }
            (Mode::Load, _) => {
                let start = Instant::now();
                check_run_data(store, &config).map_err(api_error)?;
                let loaded = load_run_data(store, &config).map_err(api_error)?;
                let ms = elapsed_ms(start);
                eprintln!(
                    "  rep {rep}: {} bars, {} funding rates",
                    loaded.bars.len(),
                    loaded.funding.len()
                );
                (
                    ms,
                    to_json(&(&loaded.bars, &loaded.funding, &loaded.report))?,
                )
            }
            (Mode::Backtest, Some(data)) => {
                let start = Instant::now();
                let result = engine_run(&config, data).map_err(|e| e.to_string())?;
                let ms = elapsed_ms(start);
                eprintln!("  rep {rep}: {} trades", result.trades.len());
                (ms, to_json(&result)?)
            }
            (Mode::Sweep { sweep }, Some(data)) => {
                let spec = sweep_spec(&config, sweep);
                let pool = own_pool()?;
                let start = Instant::now();
                let result = pool
                    .install(|| engine_sweep(&spec, data))
                    .map_err(|e| e.to_string())?;
                let ms = elapsed_ms(start);
                let trading = result
                    .rows
                    .iter()
                    .filter(|r| r.metrics.as_ref().is_some_and(|m| m.trades > 0))
                    .count();
                let errors = result.rows.iter().filter(|r| r.error.is_some()).count();
                eprintln!(
                    "  rep {rep}: {} runs, {trading} with trades, {errors} refused",
                    result.rows.len()
                );
                (ms, to_json(&result)?)
            }
            (Mode::WalkForward { sweep, windows }, Some(data)) => {
                let spec = WalkForwardSpec {
                    sweep: sweep_spec(&config, sweep),
                    windows: *windows,
                };
                let pool = own_pool()?;
                let start = Instant::now();
                let result = pool
                    .install(|| engine_walk_forward(&spec, data))
                    .map_err(|e| e.to_string())?;
                let ms = elapsed_ms(start);
                let with_trades = result
                    .windows
                    .iter()
                    .filter(|r| !r.out_of_sample.trades.is_empty())
                    .count();
                let train_trades: Vec<usize> =
                    result.windows.iter().map(|r| r.in_sample.trades).collect();
                eprintln!(
                    "  rep {rep}: {} windows, {with_trades} with test trades, {} test trades, \
                     chosen train trades {train_trades:?}",
                    result.windows.len(),
                    result.trades.len()
                );
                (ms, to_json(&result)?)
            }
            (_, None) => unreachable!("data is loaded for every engine-only mode"),
        };
        let hash = sha256_hex(&json);
        match &checksum {
            Some(first) if *first != hash => {
                return Err(format!(
                    "{}: repetition {rep} gave a different result ({} vs {})",
                    w.name,
                    &hash[..16],
                    &first[..16]
                ));
            }
            Some(_) => {}
            None => checksum = Some(hash),
        }
        times.push(ms);
    }
    let first_ms = times.first().copied().unwrap_or_default();
    times.sort_by(f64::total_cmp);
    let n = times.len();
    let median_ms = if n % 2 == 1 {
        times[n / 2]
    } else {
        (times[n / 2 - 1] + times[n / 2]) / 2.0
    };
    Ok(Measured {
        min_ms: times[0],
        first_ms,
        median_ms,
        checksum: checksum.unwrap_or_default(),
        phases: fastest.map(|(_, p)| p),
    })
}

/// The Lab's configuration: every parameter the strategy has, at its default unless the workload
/// sets it, as the Lab sends the defaults it reads from `GET /strategies`.
fn lab_config(base: &BacktestConfig) -> Result<BacktestConfig, String> {
    let entry = td_engine::find_strategy(&base.strategy)
        .ok_or_else(|| format!("no strategy {}", base.strategy))?;
    let params = base
        .params
        .resolve(&entry.params)
        .map_err(|e| e.to_string())?;
    Ok(BacktestConfig {
        params,
        ..base.clone()
    })
}

/// One Lab backtest, timed phase by phase as the server runs it: the `GET /data/coverage` the Lab
/// asks for before every run, `start`'s data check, the job (`execute`: load, run, save), the
/// Lab's `GET /jobs/{id}/result`, then opening the saved run with `GET /runs/{id}`. The Lab opens
/// a saved run only in a later session, so that phase reads through a store of its own and shares
/// nothing the earlier phases kept in memory. Saves into an empty runs directory made and removed
/// outside the timed phases. Returns the phases and the JSON to checksum: the coverage, the saved
/// backtest as read back and the run's check, without the run id, which carries the save time.
fn lab_backtest(
    config: &BacktestConfig,
    store: &Store,
    rep: usize,
) -> Result<(Phases, Vec<u8>), String> {
    let runs = tempfile::Builder::new()
        .prefix("td-bench-runs-")
        .tempdir()
        .map_err(|e| format!("cannot make the runs directory: {e}"))?;
    let later_session = Store::new(store.root());
    let mut p = Phases::default();
    let mut lap = Instant::now();
    let mut split = |phase: &mut f64| {
        *phase = elapsed_ms(lap);
        lap = Instant::now();
    };

    let coverage = read_coverage(store, config.instrument.id.clone()).map_err(api_error)?;
    to_json(&coverage)?;
    split(&mut p.coverage);
    check_run_data(store, config).map_err(api_error)?;
    split(&mut p.check);
    let data = load_run_data(store, config).map_err(api_error)?;
    split(&mut p.load);
    let result = engine_run(config, &data).map_err(|e| e.to_string())?;
    split(&mut p.run);
    let trades = result.trades.len();
    let saved = SavedRun::Backtest(Box::new(result));
    let id = save_run(&saved, &data.report, runs.path()).map_err(job_error)?;
    let done = RunDone {
        summary: saved.summary(id.clone()),
        run_id: id,
        dropped_buckets: data.report.dropped.len(),
    };
    split(&mut p.save);
    let detail = read_run(runs.path(), done.run_id.as_str()).map_err(api_error)?;
    to_json(&detail)?;
    split(&mut p.result);
    let mut opened = read_run(runs.path(), done.run_id.as_str()).map_err(api_error)?;
    opened.check = Some(check_run(&later_session, &opened.run));
    to_json(&opened)?;
    split(&mut p.open);

    eprintln!("  rep {rep}: {trades} trades, check {:?}", opened.check);
    Ok((p, to_json(&(&coverage, &opened.run, &opened.check))?))
}

fn elapsed_ms(start: Instant) -> f64 {
    start.elapsed().as_secs_f64() * 1000.0
}

fn to_json(value: &impl Serialize) -> Result<Vec<u8>, String> {
    serde_json::to_vec(value).map_err(|e| format!("cannot serialise the result: {e}"))
}

fn api_error(e: ApiError) -> String {
    format!("{}: {}", e.body.code, e.body.message)
}

fn job_error(e: JobError) -> String {
    match e {
        JobError::Cancelled => "cancelled".into(),
        JobError::Failed(body) => format!("{}: {}", body.code, body.message),
    }
}

/// A rayon pool of its own, as the server builds one per tester run (`own_pool` in
/// `api/runs.rs`). Built outside the timed section: only the engine call is timed.
fn own_pool() -> Result<rayon::ThreadPool, String> {
    rayon::ThreadPoolBuilder::new()
        .build()
        .map_err(|e| format!("cannot start the run's threads: {e}"))
}

/// A progress callback that never stops the run, standing in for the server's.
fn keep_going(_done: usize, _total: usize) -> ControlFlow<()> {
    ControlFlow::Continue(())
}

// The engine calls, one function each, so a change to their signatures is a few-line edit here.

fn engine_run(config: &BacktestConfig, data: &RunData) -> Result<BacktestResult, EngineError> {
    td_engine::run(config, &data.bars, &data.funding)
}

fn engine_sweep(spec: &SweepSpec, data: &RunData) -> Result<SweepResult, TesterError> {
    td_engine::sweep(spec, &data.bars, &data.funding, Some(&keep_going))
}

fn engine_walk_forward(
    spec: &WalkForwardSpec,
    data: &RunData,
) -> Result<WalkForwardResult, TesterError> {
    td_engine::walk_forward(spec, &data.bars, &data.funding, Some(&keep_going))
}

fn base_config(w: &Workload, from: Millis, to: Millis) -> BacktestConfig {
    BacktestConfig {
        strategy: w.strategy.clone(),
        params: w.params.clone(),
        instrument: instrument(w.kind),
        timeframe: w.timeframe,
        from,
        to,
        starting_cash: w.starting_cash,
        slippage_bps: w.slippage_bps,
        fee_override: None,
        max_leverage: 1.0,
        warmup_from: None,
        close_at_end: false,
    }
}

fn sweep_spec(base: &BacktestConfig, s: &SweepSettings) -> SweepSpec {
    SweepSpec {
        base: base.clone(),
        axes: s.axes.clone(),
        mode: SweepMode::Grid,
        objective: s.objective,
        min_trades: s.min_trades,
    }
}

/// Binance BTCUSDT with the venue's trading rules: spot or the USDⓈ-M perpetual.
fn instrument(kind: InstrumentKind) -> Instrument {
    let (tick_size, lot_size, min_notional) = match kind {
        InstrumentKind::Perpetual => (0.1, 0.001, 100.0),
        _ => (0.01, 0.00001, 5.0),
    };
    Instrument {
        id: InstrumentId {
            venue: Venue::Binance,
            symbol: "BTCUSDT".into(),
            kind,
        },
        base: "BTC".into(),
        quote: "USDT".into(),
        tick_size,
        lot_size,
        min_qty: lot_size,
        min_notional,
        fees: td_data::default_fees(Venue::Binance, kind).expect("Binance lists spot and perps"),
    }
}

// ---------------------------------------------------------------------------------------------
// Synthetic data
// ---------------------------------------------------------------------------------------------

/// The store holding a workload's dataset in this run's `data_dir`, writing the dataset first
/// unless an earlier workload of the run wrote it. Each dataset has a store of its own, so
/// datasets of one instrument never mix.
fn ensure_dataset(
    data_dir: &Path,
    w: &Workload,
    from: Millis,
    to: Millis,
) -> Result<Store, String> {
    let dir = data_dir.join(format!("{}-s{}-{}mo", w.kind, w.seed, w.months));
    let written = dir.exists();
    let store = Store::new(&dir);
    if !written {
        eprintln!("writing dataset {}", dir.display());
        write_dataset(&store, w.kind, w.seed, from, to)?;
    }
    Ok(store)
}

/// Writes a dataset's 1-minute candles over `from..to` a month at a time, and a perpetual's
/// funding rates every 8 hours.
fn write_dataset(
    store: &Store,
    kind: InstrumentKind,
    seed: u64,
    from: Millis,
    to: Millis,
) -> Result<(), String> {
    let inst = instrument(kind);
    let mut walk = Walk::new(seed, &inst);
    for month in Month::range(from, to) {
        let (start, end) = (month.start().max(from), month.end().min(to));
        let candles = walk.candles(start, end);
        store
            .write_candles(&inst.id, &candles)
            .map_err(|e| format!("writing {month}: {e}"))?;
    }
    if kind.rules().funding {
        store
            .write_funding(&inst.id, &funding(seed, from, to))
            .map_err(|e| format!("writing funding: {e}"))?;
    }
    Ok(())
}

/// splitmix64: a seeded integer PRNG.
struct Rng(u64);

impl Rng {
    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    /// Uniform in `[0, 1)`.
    fn unit(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }

    /// Roughly standard normal: the sum of four uniforms, centred and scaled to unit variance.
    /// Plain arithmetic only, so the same seed gives the same bits on every platform.
    fn normal(&mut self) -> f64 {
        let sum = self.unit() + self.unit() + self.unit() + self.unit();
        (sum - 2.0) * 1.732_050_807_568_877_2
    }
}

/// A geometric random walk of 1-minute candles, priced on the instrument's tick.
struct Walk {
    rng: Rng,
    inst: Instrument,
    /// The last close, on the tick.
    price: f64,
}

/// Return volatility per minute: about 60% a year, close to BTC's.
const MINUTE_VOL: f64 = 0.0008;

impl Walk {
    fn new(seed: u64, inst: &Instrument) -> Walk {
        Walk {
            rng: Rng(seed),
            inst: inst.clone(),
            price: inst.round_price(40_000.0, Rounding::Nearest),
        }
    }

    /// One candle per minute in `from..to`, each opening at the previous close.
    fn candles(&mut self, from: Millis, to: Millis) -> Vec<Candle> {
        (0..(to - from) / MINUTE)
            .map(|i| {
                let open = self.price;
                let close = self.inst.round_price(
                    open * (1.0 + MINUTE_VOL * self.rng.normal()),
                    Rounding::Nearest,
                );
                let up = MINUTE_VOL * self.rng.unit();
                let down = MINUTE_VOL * self.rng.unit();
                let high = self
                    .inst
                    .round_price(open.max(close) * (1.0 + up), Rounding::Up);
                let low = self
                    .inst
                    .round_price(open.min(close) * (1.0 - down), Rounding::Down);
                let volume = 1.0 + 20.0 * self.rng.unit();
                self.price = close;
                Candle {
                    open_time: from + i * MINUTE,
                    open,
                    high,
                    low,
                    close,
                    volume,
                    quote_volume: volume * close,
                }
            })
            .collect()
    }
}

/// Funding rates at 00:00, 08:00 and 16:00 UTC in `from..to` (`from` is midnight), each within
/// three basis points of zero.
fn funding(seed: u64, from: Millis, to: Millis) -> Vec<(Millis, f64)> {
    const EIGHT_HOURS: Millis = 8 * 60 * MINUTE;
    let mut rng = Rng(seed ^ 0xf00d);
    (0..(to - from + EIGHT_HOURS - 1) / EIGHT_HOURS)
        .map(|k| (from + k * EIGHT_HOURS, (rng.unit() - 0.5) * 0.0006))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use td_data::time::DAY;

    fn workload(mode: Mode) -> Workload {
        Workload {
            name: "tiny".into(),
            split: "dev".into(),
            seed: 7,
            kind: InstrumentKind::Spot,
            months: 0,
            timeframe: Timeframe::M5,
            strategy: "sma_cross".into(),
            params: Params::new(),
            starting_cash: 10_000.0,
            slippage_bps: 1.0,
            mode,
        }
    }

    /// `key=value` pairs of an output line, after its tag.
    fn fields(line: &str, tag: &str) -> Vec<(String, String)> {
        let mut parts = line.split(' ');
        assert_eq!(parts.next(), Some(tag));
        parts
            .map(|p| {
                let (k, v) = p.split_once('=').expect("key=value");
                (k.to_string(), v.to_string())
            })
            .collect()
    }

    #[test]
    fn the_generator_is_deterministic_and_its_candles_are_valid() {
        let inst = instrument(InstrumentKind::Perpetual);
        let (from, to) = (DATASET_START.start(), DATASET_START.start() + 2 * DAY);
        let a = Walk::new(3, &inst).candles(from, to);
        let b = Walk::new(3, &inst).candles(from, to);
        assert_eq!(a, b);
        assert_ne!(a, Walk::new(4, &inst).candles(from, to));
        assert_eq!(a.len(), 2 * 24 * 60);
        for (i, c) in a.iter().enumerate() {
            c.validate().unwrap();
            assert!(c.low <= c.open.min(c.close) && c.open.max(c.close) <= c.high);
            assert!(c.low > 0.0 && c.volume > 0.0);
            assert_eq!(c.open_time, from + i as i64 * MINUTE);
            if i > 0 {
                assert_eq!(c.open, a[i - 1].close);
            }
        }
        assert_eq!(funding(3, from, to).len(), 6);
    }

    /// Runs `w` twice (one bench of two repetitions, which fails if they differ, then one more)
    /// over a month of its dataset and checks both give the same checksum.
    fn bench_twice(w: &Workload) -> Measured {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path());
        let (from, to) = dataset_range(1);
        write_dataset(&store, w.kind, w.seed, from, to).unwrap();
        let first = bench(w, &store, from, to, 2).unwrap();
        let second = bench(w, &store, from, to, 1).unwrap();
        assert_eq!(first.checksum, second.checksum);
        assert_eq!(first.checksum.len(), 64);
        assert!(first.min_ms <= first.median_ms);
        first
    }

    fn tiny_sweep() -> SweepSettings {
        serde_json::from_value(serde_json::json!({
            "axes": [
                { "name": "fast", "space": { "kind": "values", "values": [5, 10] } },
                { "name": "slow", "space": { "kind": "values", "values": [20, 40] } }
            ],
            "objective": { "metric": "total_return", "direction": "maximize" },
            "min_trades": 0
        }))
        .unwrap()
    }

    #[test]
    fn a_tiny_load_gives_the_same_checksum_twice() {
        assert!(bench_twice(&workload(Mode::Load)).phases.is_none());
    }

    #[test]
    fn a_tiny_sweep_gives_the_same_checksum_twice() {
        let w = Workload {
            timeframe: Timeframe::H1,
            ..workload(Mode::Sweep {
                sweep: tiny_sweep(),
            })
        };
        bench_twice(&w);
    }

    #[test]
    fn a_tiny_walk_forward_gives_the_same_checksum_twice() {
        let windows = serde_json::from_value(serde_json::json!(
            { "train_bars": 240, "test_bars": 120, "step_bars": 120, "mode": "rolling" }
        ))
        .unwrap();
        let w = Workload {
            timeframe: Timeframe::H1,
            ..workload(Mode::WalkForward {
                sweep: tiny_sweep(),
                windows,
            })
        };
        bench_twice(&w);
    }

    #[test]
    fn a_tiny_lab_backtest_gives_the_same_checksum_twice_and_a_parsable_phase_line() {
        let w = Workload {
            kind: InstrumentKind::Perpetual,
            ..workload(Mode::LabBacktest)
        };
        let m = bench_twice(&w);
        let p = m.phases.expect("a lab backtest times its phases");
        assert!((p.total() - m.min_ms).abs() < 1e-9);
        let line = phases_line(&w, &p);
        let f = fields(&line, "PHASES");
        let keys: Vec<&str> = f.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(
            keys,
            [
                "name",
                "coverage_ms",
                "check_ms",
                "load_ms",
                "run_ms",
                "save_ms",
                "result_ms",
                "open_ms"
            ]
        );
        for (_, v) in &f[1..] {
            v.parse::<f64>().unwrap();
        }
    }

    #[test]
    fn a_tiny_backtest_gives_the_same_checksum_twice_and_a_parsable_summary() {
        let w = workload(Mode::Backtest);
        let first = bench_twice(&w);

        let line = workload_line(&w, 2, &first);
        let f = fields(&line, "WORKLOAD");
        let keys: Vec<&str> = f.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(
            keys,
            [
                "name",
                "split",
                "reps",
                "min_ms",
                "first_ms",
                "median_ms",
                "checksum"
            ]
        );
        assert_eq!(f[6].1, first.checksum[..16]);
        f[3].1.parse::<f64>().unwrap();

        let summary = bench_line("dev", 1, first.min_ms, &first.checksum);
        let f = fields(&summary, "BENCH");
        assert_eq!(f[0], ("split".into(), "dev".into()));
        assert_eq!(f[1], ("workloads".into(), "1".into()));
        f[2].1.parse::<f64>().unwrap();
        assert_eq!(f[3].1.len(), 16);
    }

    /// The workloads file is frozen for the speed hillclimb: its hash changes only together with a
    /// new baseline. `\r` is dropped first, since Windows checks the file out with CRLF.
    #[test]
    fn the_workloads_file_is_unchanged() {
        let path =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../harness/evals/perf-workloads.json");
        let text = fs::read_to_string(path).unwrap().replace('\r', "");
        assert_eq!(
            sha256_hex(text.as_bytes()),
            "12b983ff0a16224d7c953306081ffa5678f7a67aec35fcb238517fe9d42e93f7"
        );
    }

    #[test]
    fn the_workloads_file_parses() {
        let path =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../harness/evals/perf-workloads.json");
        let file: WorkloadFile = serde_json::from_str(&fs::read_to_string(path).unwrap()).unwrap();
        assert_eq!(file.workloads.len(), 10);
        for w in &file.workloads {
            assert!(w.split == "dev" || w.split == "test", "{}", w.name);
            if let Mode::Sweep { sweep } | Mode::WalkForward { sweep, .. } = &w.mode {
                sweep_spec(&base_config(w, 0, 1), sweep).check().unwrap();
            }
        }
    }
}
