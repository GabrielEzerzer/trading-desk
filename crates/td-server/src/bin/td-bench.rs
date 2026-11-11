//! Times the engine's real work on synthetic data, so a speed change can be measured.
//!
//! Reads the frozen workloads in `harness/evals/perf-workloads.json`, writes each workload's
//! synthetic 1-minute candles (and a perpetual's funding) through the real [`Store`] once, then
//! times each workload `--reps` times: for `load`, the data check and load a run makes; for the
//! others, only the engine call, on data loaded beforehand. Prints one `WORKLOAD` line per workload
//! and one `BENCH` line to stdout; progress goes to stderr. A workload whose result differs between
//! repetitions stops the run with a non-zero exit.

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
    BacktestConfig, BacktestResult, EngineError, Objective, ParamAxis, Params, SweepMode,
    SweepResult, SweepSpec, TesterError, WalkForwardResult, WalkForwardSpec, WindowSpec,
};
use td_server::api::error::ApiError;
use td_server::api::runs::{RunData, check_run_data, load_run_data};

/// Changes whenever the generator's output changes, so datasets made by an older one are not
/// reused.
const GENERATOR: &str = "g1";

/// Every dataset starts at 2022-01-01T00:00Z.
const DATASET_START: Month = Month {
    year: 2022,
    month: 1,
};

/// The file that marks a dataset as completely written.
const MARKER: &str = "complete";

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
    /// Where the synthetic datasets are written (default: `<temp dir>/td-bench-data`).
    #[arg(long)]
    data_dir: Option<PathBuf>,
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
    median_ms: f64,
    /// SHA-256 of the result's JSON, in hex; the same on every repetition.
    checksum: String,
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
    let data_dir = args
        .data_dir
        .clone()
        .unwrap_or_else(|| std::env::temp_dir().join("td-bench-data"));
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
        let store = ensure_dataset(&data_dir, w, from, to)?;
        eprintln!("running {} ({} reps)", w.name, args.reps);
        let m = bench(w, &store, from, to, args.reps)?;
        total_ms += m.min_ms;
        checksums.push_str(&m.checksum);
        lines.push(workload_line(w, args.reps, &m));
    }
    for line in &lines {
        println!("{line}");
    }
    println!(
        "{}",
        bench_line(args.split.as_str(), lines.len(), total_ms, &checksums)
    );
    Ok(())
}

fn workload_line(w: &Workload, reps: usize, m: &Measured) -> String {
    format!(
        "WORKLOAD name={} split={} reps={reps} min_ms={:.1} median_ms={:.1} checksum={}",
        w.name,
        w.split,
        m.min_ms,
        m.median_ms,
        &m.checksum[..16]
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
    // Loaded once, outside the timed section, for the modes that time only the engine.
    let data = match w.mode {
        Mode::Load => None,
        _ => Some(load_run_data(store, &config).map_err(api_error)?),
    };
    for rep in 0..reps {
        let (ms, json) = match (&w.mode, &data) {
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
            (_, None) => unreachable!("data is loaded for every mode but load"),
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
    times.sort_by(f64::total_cmp);
    let n = times.len();
    let median_ms = if n % 2 == 1 {
        times[n / 2]
    } else {
        (times[n / 2 - 1] + times[n / 2]) / 2.0
    };
    Ok(Measured {
        min_ms: times[0],
        median_ms,
        checksum: checksum.unwrap_or_default(),
    })
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

/// The store holding a workload's dataset, writing the dataset first unless its marker says it is
/// complete. Each dataset has a store of its own, so datasets of one instrument never mix.
fn ensure_dataset(
    data_dir: &Path,
    w: &Workload,
    from: Millis,
    to: Millis,
) -> Result<Store, String> {
    let dir = data_dir.join(format!("{GENERATOR}-{}-s{}-{}mo", w.kind, w.seed, w.months));
    let store = Store::new(&dir);
    if dir.join(MARKER).exists() {
        return Ok(store);
    }
    if dir.exists() {
        // A dataset whose writing stopped part-way: start it again.
        fs::remove_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    }
    eprintln!("writing dataset {}", dir.display());
    write_dataset(&store, w.kind, w.seed, from, to)?;
    fs::write(dir.join(MARKER), "").map_err(|e| format!("{}: {e}", dir.display()))?;
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

    #[test]
    fn a_tiny_backtest_gives_the_same_checksum_twice_and_a_parsable_summary() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path());
        let w = workload(Mode::Backtest);
        let (from, to) = (DATASET_START.start(), DATASET_START.start() + 3 * DAY);
        write_dataset(&store, w.kind, w.seed, from, to).unwrap();

        let first = bench(&w, &store, from, to, 2).unwrap();
        let second = bench(&w, &store, from, to, 1).unwrap();
        assert_eq!(first.checksum, second.checksum);
        assert_eq!(first.checksum.len(), 64);
        assert!(first.min_ms <= first.median_ms);

        let line = workload_line(&w, 2, &first);
        let f = fields(&line, "WORKLOAD");
        let keys: Vec<&str> = f.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(
            keys,
            ["name", "split", "reps", "min_ms", "median_ms", "checksum"]
        );
        assert_eq!(f[5].1, first.checksum[..16]);
        f[3].1.parse::<f64>().unwrap();

        let summary = bench_line("dev", 1, first.min_ms, &first.checksum);
        let f = fields(&summary, "BENCH");
        assert_eq!(f[0], ("split".into(), "dev".into()));
        assert_eq!(f[1], ("workloads".into(), "1".into()));
        f[2].1.parse::<f64>().unwrap();
        assert_eq!(f[3].1.len(), 16);
    }

    #[test]
    fn the_workloads_file_parses() {
        let path =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../harness/evals/perf-workloads.json");
        let file: WorkloadFile = serde_json::from_str(&fs::read_to_string(path).unwrap()).unwrap();
        assert_eq!(file.workloads.len(), 8);
        for w in &file.workloads {
            assert!(w.split == "dev" || w.split == "test", "{}", w.name);
            if let Mode::Sweep { sweep } | Mode::WalkForward { sweep, .. } = &w.mode {
                sweep_spec(&base_config(w, 0, 1), sweep).check().unwrap();
            }
        }
    }
}
