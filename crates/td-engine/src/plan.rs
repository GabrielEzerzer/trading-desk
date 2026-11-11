//! A strategy's test plan (#32): the data it is tested on, the tests the battery runs on it, and
//! the settings of the common evidence layer every plan carries (#35, R1 to R6).
//!
//! The backtest, the sweep and the walk-forward always run, because the integrity and evidence
//! layers are built on them: the backtest gives the data, determinism and prefix checks, the sweep
//! gives the parameter plateau and is what the luck test repeats on shuffled bars, and the stitched
//! walk-forward is the out-of-sample curve every later layer is judged on. None of them is an
//! `Option`, so a plan cannot leave one out. The single out-of-sample split is optional: it is
//! reported, never a gate, since one split is one draw.
//!
//! [`EvidenceSettings`] holds plain numbers with no switch: a plan can tune them but not turn them
//! off. [`TestPlan::validate`] refuses values looser than #35's: the minimum sample, the p
//! thresholds, the cost stress, the plateau and the start offsets can only be made stricter, and
//! the permutation count can go down to [`MIN_PERMUTATIONS`] for a quicker luck test.

use serde::{Deserialize, Serialize};
use td_core::{
    AccountCurrency, FeeSchedule, Instrument, InstrumentId, Millis, Timeframe, days_from_civil,
};

use crate::metrics::DAY_MS;
use crate::{
    BacktestConfig, MAX_RUNS, Objective, OosSpec, ParamAxis, Params, Split, SweepMode, SweepSpec,
    WalkForwardSpec, WindowMode, WindowSpec, default_slippage_bps,
};

/// The fewest permutations the luck test (R3) may run. With 49 the smallest p it can report is
/// 1/50 = 0.02, still below the 0.05 pass threshold; #35 measured its run cost at this count.
pub const MIN_PERMUTATIONS: u32 = 49;

/// The share of walk-forward windows that must be profitable (R1). Fixed by #35, not tunable.
pub const MIN_PROFITABLE_WINDOWS: f64 = 0.5;

/// Why a plan or a rule was refused.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
#[error("{0}")]
pub struct PlanError(pub String);

fn refuse<T>(message: impl Into<String>) -> Result<T, PlanError> {
    Err(PlanError(message.into()))
}

/// R6 moves the walk-forward windows back by `start_offsets` shifts. The windows step by one test
/// window, so a shift moves their edges by the shift modulo the test length: a test window of
/// `n` bars has `n - 1` edge positions besides the main run's, and more offsets than that would
/// repeat a run.
pub(crate) fn check_start_offsets(test_bars: usize, start_offsets: u32) -> Result<(), String> {
    if test_bars <= start_offsets as usize {
        return Err(format!(
            "the walk-forward test window is {test_bars} bars, too short for {start_offsets} start \
             offsets: R6 needs at least {} bars per test window to move the window edges to \
             {start_offsets} different places",
            u64::from(start_offsets) + 1
        ));
    }
    Ok(())
}

/// Midnight UTC at the start of a date, for writing a plan's period: `date(2024, 1, 1)`.
pub fn date(year: i64, month: u32, day: u32) -> Millis {
    days_from_civil(year, month, day) * DAY_MS
}

/// Everything the battery needs to test a strategy, with no data.
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TestPlan {
    /// Each instrument is tested on its own and gets its own verdict.
    pub instruments: Vec<InstrumentId>,
    pub timeframe: Timeframe,
    pub period: Period,
    pub backtest: BacktestSettings,
    pub sweep: SweepPlan,
    pub walk_forward: WalkForwardPlan,
    /// A single in-sample / out-of-sample split, reported only. `None` leaves it out.
    pub out_of_sample: Option<Split>,
    pub evidence: EvidenceSettings,
}

/// Bars that open at or after `from` and before `to`. Both must be bar boundaries.
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Period {
    pub from: Millis,
    pub to: Millis,
}

/// What every run of the plan shares.
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BacktestSettings {
    /// Starting balance in the quote asset: a plan's runs are on a quote-currency account (see
    /// [`TestPlan::backtest_config`]).
    pub starting_cash: f64,
    /// The perpetual leverage cap, as in [`BacktestConfig::max_leverage`]; 1.0 is 1x.
    pub max_leverage: f64,
    /// Slippage set for some of the plan's instruments. An instrument not listed here uses the
    /// backtest config's default slippage for that instrument (#35: 2 bps on BTC and ETH, 5 bps
    /// on other large caps); an empty list uses the default everywhere.
    pub slippage: Vec<SlippageOverride>,
}

/// One instrument's slippage, overriding the backtest config's default for it.
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SlippageOverride {
    /// One of the plan's instruments.
    pub instrument: InstrumentId,
    /// Slippage on market and stop fills, in basis points of price.
    pub bps: f64,
}

impl BacktestSettings {
    /// The slippage the plan sets for `instrument`, or `None` when it leaves it to the backtest
    /// config's default.
    pub fn slippage_bps(&self, instrument: &InstrumentId) -> Option<f64> {
        self.slippage
            .iter()
            .find(|o| &o.instrument == instrument)
            .map(|o| o.bps)
    }
}

/// The parameter grid. It always runs as a full grid, because the plateau test (R5) reads each
/// set's neighbours on it.
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SweepPlan {
    /// Parameters the sweep does not vary; the rest take the strategy's defaults.
    pub fixed: Params,
    pub axes: Vec<ParamAxis>,
    pub objective: Objective,
    /// See [`SweepSpec::min_trades`].
    pub min_trades: usize,
}

/// Rolling walk-forward windows, in days of bars: a day is 24 hours of bars of the plan's
/// timeframe, so on an FX pair, closed at weekends, a window spans more calendar days. Each test
/// segment starts where the previous one ended, so the stitched out-of-sample curve has no holes.
/// Both lengths must be whole bars of the timeframe.
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WalkForwardPlan {
    pub train_days: u32,
    pub test_days: u32,
}

/// The common evidence layer (#35, R2 to R6). Every field is a number: there is no way to switch a
/// test off. The defaults are #35's.
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EvidenceSettings {
    pub min_sample: MinSample,
    pub luck: LuckTest,
    /// R4: fees and slippage are multiplied by this, and the strategy must stay profitable.
    pub cost_stress: f64,
    pub plateau: Plateau,
    /// R6: how many different start offsets must all give a result of the same sign.
    pub start_offsets: u32,
}

/// R2: below any of these, the verdict is `not_enough_evidence`.
///
/// A day here is a day of bars, as in [`WalkForwardPlan`]: 24 hours of the market's open bars,
/// so the days a market is closed (an FX pair's weekends) do not count. It is the unit the
/// walk-forward windows are laid in, so a run's out-of-sample days are its windows times
/// `test_days`.
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MinSample {
    /// Closed out-of-sample trades.
    pub trades: u32,
    /// Days of bars covered by the stitched out-of-sample curve, as defined above.
    pub days: u32,
    /// Walk-forward windows.
    pub windows: u32,
}

/// R3: the same sweep repeated on bar permutations.
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LuckTest {
    pub permutations: u32,
    /// p at or below this passes.
    pub pass_p: f64,
    /// p above `pass_p` and at or below this gives `not_enough_evidence`; above it fails.
    pub inconclusive_p: f64,
}

/// R5: how the best parameter set's neighbours on the grid must do.
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Plateau {
    /// The neighbours' median objective over the best set's must be at least this.
    pub min_median_ratio: f64,
    /// At least this share of the neighbours must be profitable.
    pub min_positive_share: f64,
}

impl Default for EvidenceSettings {
    fn default() -> Self {
        EvidenceSettings {
            min_sample: MinSample {
                trades: 30,
                days: 180,
                windows: 4,
            },
            luck: LuckTest {
                permutations: 199,
                pass_p: 0.05,
                inconclusive_p: 0.10,
            },
            cost_stress: 2.0,
            plateau: Plateau {
                min_median_ratio: 0.6,
                min_positive_share: 0.8,
            },
            start_offsets: 8,
        }
    }
}

impl EvidenceSettings {
    /// Refuses settings looser than #35's defaults, except a permutation count down to
    /// [`MIN_PERMUTATIONS`], and settings that could never pass. Comparisons are written so that
    /// NaN fails them.
    pub fn validate(&self) -> Result<(), PlanError> {
        let floor = EvidenceSettings::default();
        let m = &self.min_sample;
        if m.trades < floor.min_sample.trades
            || m.days < floor.min_sample.days
            || m.windows < floor.min_sample.windows
        {
            return refuse(format!(
                "the minimum sample can only be raised: at least {} trades, {} days and {} windows",
                floor.min_sample.trades, floor.min_sample.days, floor.min_sample.windows
            ));
        }
        let l = &self.luck;
        if l.permutations < MIN_PERMUTATIONS {
            return refuse(format!(
                "the luck test needs at least {MIN_PERMUTATIONS} permutations, got {}",
                l.permutations
            ));
        }
        let smallest_p = 1.0 / (f64::from(l.permutations) + 1.0);
        if !(l.pass_p > 0.0 && l.pass_p <= floor.luck.pass_p) {
            return refuse(format!(
                "the luck test's pass threshold must be above 0 and at most {}, got {}",
                floor.luck.pass_p, l.pass_p
            ));
        }
        if !(l.inconclusive_p >= l.pass_p && l.inconclusive_p <= floor.luck.inconclusive_p) {
            return refuse(format!(
                "the luck test's inconclusive threshold must be between the pass threshold and {}, \
                 got {}",
                floor.luck.inconclusive_p, l.inconclusive_p
            ));
        }
        if smallest_p > l.pass_p {
            return refuse(format!(
                "with {} permutations the smallest p is {smallest_p}, above the pass threshold {}: \
                 the luck test could never pass",
                l.permutations, l.pass_p
            ));
        }
        if !(self.cost_stress >= floor.cost_stress && self.cost_stress.is_finite()) {
            return refuse(format!(
                "the cost stress must be at least {}x, got {}",
                floor.cost_stress, self.cost_stress
            ));
        }
        let p = &self.plateau;
        if !(p.min_median_ratio >= floor.plateau.min_median_ratio && p.min_median_ratio <= 1.0) {
            return refuse(format!(
                "the plateau's median ratio must be between {} and 1, got {}",
                floor.plateau.min_median_ratio, p.min_median_ratio
            ));
        }
        if !(p.min_positive_share >= floor.plateau.min_positive_share
            && p.min_positive_share <= 1.0)
        {
            return refuse(format!(
                "the plateau's positive share must be between {} and 1, got {}",
                floor.plateau.min_positive_share, p.min_positive_share
            ));
        }
        if self.start_offsets < floor.start_offsets {
            return refuse(format!(
                "at least {} start offsets, got {}",
                floor.start_offsets, self.start_offsets
            ));
        }
        Ok(())
    }
}

impl WalkForwardPlan {
    /// The windows in bars of `timeframe`.
    pub fn window_spec(&self, timeframe: Timeframe) -> WindowSpec {
        let bars = |days: u32| (i64::from(days) * DAY_MS / timeframe.duration_ms()) as usize;
        let test = bars(self.test_days);
        WindowSpec {
            train_bars: bars(self.train_days),
            test_bars: test,
            step_bars: test,
            mode: WindowMode::Rolling,
        }
    }
}

impl TestPlan {
    /// The backtest of `strategy` on `instrument` over the plan's period, with the sweep's fixed
    /// parameters. The slippage is the plan's for `instrument`, or [`default_slippage_bps`] for it
    /// when the plan sets none.
    pub fn backtest_config(&self, strategy: &str, instrument: Instrument) -> BacktestConfig {
        let slippage_bps = self
            .backtest
            .slippage_bps(&instrument.id)
            .unwrap_or_else(|| default_slippage_bps(&instrument));
        BacktestConfig {
            strategy: strategy.to_string(),
            params: self.sweep.fixed.clone(),
            instrument,
            timeframe: self.timeframe,
            from: self.period.from,
            to: self.period.to,
            starting_cash: self.backtest.starting_cash,
            slippage_bps,
            fee_override: None,
            max_leverage: self.backtest.max_leverage,
            warmup_from: None,
            close_at_end: false,
            // shortcut: a plan's runs are on a quote-currency account; add the account currency
            // to `BacktestSettings` when the first plan for an FX pair on a base account is written.
            account_currency: AccountCurrency::Quote,
        }
    }

    /// The plan's grid sweep over the whole period, with the slippage of
    /// [`TestPlan::backtest_config`].
    pub fn sweep_spec(&self, strategy: &str, instrument: Instrument) -> SweepSpec {
        SweepSpec {
            base: self.backtest_config(strategy, instrument),
            axes: self.sweep.axes.clone(),
            mode: SweepMode::Grid,
            objective: self.sweep.objective,
            min_trades: self.sweep.min_trades,
        }
    }

    pub fn walk_forward_spec(&self, strategy: &str, instrument: Instrument) -> WalkForwardSpec {
        WalkForwardSpec {
            sweep: self.sweep_spec(strategy, instrument),
            windows: self.walk_forward.window_spec(self.timeframe),
        }
    }

    pub fn oos_spec(&self, strategy: &str, instrument: Instrument) -> Option<OosSpec> {
        self.out_of_sample.map(|split| OosSpec {
            sweep: self.sweep_spec(strategy, instrument),
            split,
        })
    }

    /// Checks the plan without data: the instruments, the period, the backtest settings, the
    /// sweep and walk-forward specs as the tester checks them, that the period holds enough
    /// walk-forward windows and out-of-sample days for the minimum sample, and the evidence
    /// settings. Gaps in the data can still leave a run short of the minimum sample; the battery
    /// then gives `not_enough_evidence`.
    pub fn validate(&self, strategy: &str) -> Result<(), PlanError> {
        let Some(first) = self.instruments.first() else {
            return refuse("the plan names no instruments");
        };
        for (i, id) in self.instruments.iter().enumerate() {
            if self.instruments[..i].contains(id) {
                return refuse(format!("{id} is listed twice"));
            }
        }
        let (tf, Period { from, to }) = (self.timeframe, self.period);
        if from >= to {
            return refuse("the period must end after it starts");
        }
        if tf.floor(from) != from || tf.floor(to) != to {
            return refuse(format!(
                "the period must start and end on {tf} bar boundaries"
            ));
        }
        let b = &self.backtest;
        if !(b.starting_cash > 0.0 && b.starting_cash.is_finite()) {
            return refuse("the starting cash must be a positive number");
        }
        for (i, o) in b.slippage.iter().enumerate() {
            let id = &o.instrument;
            if !self.instruments.contains(id) {
                return refuse(format!(
                    "the slippage for {id} names an instrument the plan does not test"
                ));
            }
            if b.slippage[..i].iter().any(|p| &p.instrument == id) {
                return refuse(format!("the slippage for {id} is set twice"));
            }
            if !(o.bps >= 0.0 && o.bps.is_finite()) {
                return refuse(format!("the slippage for {id} must be zero or more"));
            }
        }
        if !(b.max_leverage > 0.0 && b.max_leverage.is_finite()) {
            return refuse("the leverage cap must be a positive number");
        }
        let bar = tf.duration_ms();
        let w = &self.walk_forward;
        for (name, days) in [("train_days", w.train_days), ("test_days", w.test_days)] {
            let span = i64::from(days) * DAY_MS;
            if days == 0 || span % bar != 0 {
                return refuse(format!(
                    "walk_forward.{name} must be a whole number of {tf} bars, at least one"
                ));
            }
        }
        // The tester's own checks: the strategy, the fixed and swept parameters, the run count
        // and that a train window is long enough for the objective. They do not read the
        // instrument's trading rules, so a stand-in carries the id.
        let stand_in = Instrument {
            id: first.clone(),
            base: String::new(),
            quote: String::new(),
            tick_size: 0.01,
            lot_size: 0.001,
            min_qty: 0.001,
            min_notional: 0.0,
            fees: FeeSchedule {
                maker: 0.0,
                taker: 0.0,
                per_unit: 0.0,
            },
        };
        let wf = self.walk_forward_spec(strategy, stand_in.clone());
        wf.check().map_err(|e| PlanError(e.to_string()))?;
        check_start_offsets(wf.windows.test_bars, self.evidence.start_offsets)
            .map_err(PlanError)?;
        if let Some(oos) = self.oos_spec(strategy, stand_in) {
            if let Split::Fraction(f) = oos.split
                && !(f > 0.0 && f < 1.0)
            {
                return refuse("the out-of-sample split fraction must be between 0 and 1");
            }
            if let Split::At(t) = oos.split
                && !(from < t && t < to)
            {
                return refuse("the out-of-sample split must fall inside the period");
            }
            oos.check().map_err(|e| PlanError(e.to_string()))?;
        }
        self.evidence.validate()?;
        // The luck test repeats the sweep once per permutation; the battery refuses it past the
        // tester's run limit, so a plan that would reach it is refused here.
        let per_sweep = wf.sweep.runs().map_err(|e| PlanError(e.to_string()))?;
        let luck_runs = per_sweep as u128 * (u128::from(self.evidence.luck.permutations) + 1);
        if luck_runs > MAX_RUNS as u128 {
            return refuse(format!(
                "the luck test would run {luck_runs} backtests ({per_sweep} parameter sets times \
                 {} sweeps), above the limit of {MAX_RUNS}: shrink the grid or run fewer \
                 permutations",
                self.evidence.luck.permutations + 1
            ));
        }
        // The windows as the tester lays them, on data with no gaps: the bars of the period's
        // open hours, so an FX pair, closed at weekends, has fewer than a crypto pair.
        let ws = &wf.windows;
        let min = &self.evidence.min_sample;
        for id in &self.instruments {
            let closed: Millis = id
                .kind
                .closed_spans(from, to)
                .iter()
                .map(|s| s.end - s.start)
                .sum();
            let bars = ((to - from - closed) / bar) as usize;
            let windows = if bars < ws.train_bars + ws.test_bars {
                0
            } else {
                (bars - ws.train_bars - ws.test_bars) / ws.step_bars + 1
            };
            let oos_days = windows as u64 * u64::from(w.test_days);
            if windows < min.windows as usize || oos_days < u64::from(min.days) {
                return refuse(format!(
                    "on {id} the period holds {windows} walk-forward windows and {oos_days} days \
                     of out-of-sample bars, below the minimum sample of {} windows and {} days: \
                     lengthen the period or shorten the windows",
                    min.windows, min.days
                ));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ParamSpace, ParamValue, find_strategy};

    fn example_plan() -> TestPlan {
        find_strategy("sma_cross").unwrap().plan
    }

    #[test]
    fn dates_are_midnight_utc() {
        assert_eq!(date(1970, 1, 1), 0);
        assert_eq!(date(2023, 1, 1), 1_672_531_200_000);
        assert_eq!(date(2024, 3, 1), 1_709_251_200_000);
        assert_eq!(date(1969, 12, 31), -DAY_MS);
    }

    #[test]
    fn the_default_evidence_settings_and_stricter_ones_are_valid() {
        let d = EvidenceSettings::default();
        assert_eq!(d.validate(), Ok(()));
        let mut stricter = d;
        stricter.min_sample = MinSample {
            trades: 60,
            days: 365,
            windows: 6,
        };
        stricter.luck = LuckTest {
            permutations: MIN_PERMUTATIONS,
            pass_p: 0.02,
            inconclusive_p: 0.05,
        };
        stricter.cost_stress = 3.0;
        stricter.plateau = Plateau {
            min_median_ratio: 1.0,
            min_positive_share: 1.0,
        };
        stricter.start_offsets = 12;
        assert_eq!(stricter.validate(), Ok(()));
    }

    #[test]
    fn evidence_settings_looser_than_the_floors_are_refused() {
        type Tweak = fn(&mut EvidenceSettings);
        let cases: [(&str, Tweak); 17] = [
            ("trades", |e| e.min_sample.trades = 29),
            ("days", |e| e.min_sample.days = 179),
            ("windows", |e| e.min_sample.windows = 3),
            ("no windows", |e| e.min_sample.windows = 0),
            ("permutations", |e| {
                e.luck.permutations = MIN_PERMUTATIONS - 1
            }),
            ("no permutations", |e| e.luck.permutations = 0),
            ("pass_p above 0.05", |e| e.luck.pass_p = 0.051),
            ("pass_p zero", |e| e.luck.pass_p = 0.0),
            ("pass_p NaN", |e| e.luck.pass_p = f64::NAN),
            ("inconclusive_p above 0.10", |e| {
                e.luck.inconclusive_p = 0.11
            }),
            ("inconclusive_p below pass_p", |e| {
                e.luck.inconclusive_p = 0.04
            }),
            ("unreachable pass_p", |e| {
                e.luck.permutations = MIN_PERMUTATIONS;
                e.luck.pass_p = 0.01;
            }),
            ("cost stress", |e| e.cost_stress = 1.9),
            ("cost stress infinite", |e| e.cost_stress = f64::INFINITY),
            ("plateau ratio", |e| e.plateau.min_median_ratio = 0.59),
            ("plateau share", |e| e.plateau.min_positive_share = 0.79),
            ("start offsets", |e| e.start_offsets = 7),
        ];
        for (name, tweak) in cases {
            let mut e = EvidenceSettings::default();
            tweak(&mut e);
            assert!(e.validate().is_err(), "{name} was accepted");
            let mut plan = example_plan();
            plan.evidence = e;
            assert!(plan.validate("sma_cross").is_err(), "{name}: plan accepted");
        }
    }

    /// A plan cannot switch a test off: there is no field to do it with, and JSON that tries is
    /// refused.
    #[test]
    fn a_plan_cannot_switch_the_evidence_layer_off() {
        let plan = example_plan();
        let json = serde_json::to_value(&plan).unwrap();
        assert_eq!(
            serde_json::from_value::<TestPlan>(json.clone()).unwrap(),
            plan
        );
        let attempts = [
            ("/evidence", "enabled", serde_json::json!(false)),
            ("/evidence/luck", "enabled", serde_json::json!(false)),
            ("/evidence", "luck", serde_json::Value::Null),
            ("/evidence", "plateau", serde_json::Value::Null),
            ("", "walk_forward", serde_json::Value::Null),
            ("", "sweep", serde_json::Value::Null),
            ("", "skip_evidence", serde_json::json!(true)),
        ];
        for (at, key, value) in attempts {
            let mut j = json.clone();
            j.pointer_mut(at).unwrap()[key] = value.clone();
            assert!(
                serde_json::from_value::<TestPlan>(j).is_err(),
                "{at}/{key} = {value} was accepted"
            );
        }
    }

    #[test]
    fn the_example_plan_builds_the_tester_specs() {
        let plan = example_plan();
        assert_eq!(plan.validate("sma_cross"), Ok(()));
        let w = plan.walk_forward.window_spec(Timeframe::H1);
        assert_eq!(
            w,
            WindowSpec {
                train_bars: 180 * 24,
                test_bars: 60 * 24,
                step_bars: 60 * 24,
                mode: WindowMode::Rolling,
            }
        );
        let inst = Instrument {
            id: plan.instruments[0].clone(),
            base: "BTC".into(),
            quote: "USDT".into(),
            tick_size: 0.01,
            lot_size: 0.00001,
            min_qty: 0.00001,
            min_notional: 5.0,
            fees: FeeSchedule {
                maker: 0.001,
                taker: 0.001,
                per_unit: 0.0,
            },
        };
        let wf = plan.walk_forward_spec("sma_cross", inst.clone());
        assert_eq!(wf.sweep.mode, SweepMode::Grid);
        assert_eq!(wf.sweep.base.from, plan.period.from);
        assert_eq!(wf.sweep.base.starting_cash, 10_000.0);
        assert_eq!(wf.sweep.base.instrument, inst);
        assert_eq!(wf.check(), Ok(()));
        assert_eq!(
            plan.oos_spec("sma_cross", inst).unwrap().split,
            Split::Fraction(0.7)
        );
    }

    /// An instrument the plan sets no slippage for takes the backtest config's default; one it
    /// sets takes the plan's value. JSON naming the old single field is refused.
    #[test]
    fn slippage_is_the_backtest_default_unless_the_plan_overrides_it() {
        let mut plan = example_plan();
        assert!(plan.backtest.slippage.is_empty());
        let instrument = |id: &InstrumentId| Instrument {
            id: id.clone(),
            base: id.symbol.trim_end_matches("USDT").into(),
            quote: "USDT".into(),
            tick_size: 0.01,
            lot_size: 0.00001,
            min_qty: 0.00001,
            min_notional: 5.0,
            fees: FeeSchedule {
                maker: 0.001,
                taker: 0.001,
                per_unit: 0.0,
            },
        };
        let (btc, eth) = (plan.instruments[0].clone(), plan.instruments[1].clone());
        let slip = |plan: &TestPlan, id: &InstrumentId| {
            plan.backtest_config("sma_cross", instrument(id))
                .slippage_bps
        };
        assert_eq!((slip(&plan, &btc), slip(&plan, &eth)), (2.0, 2.0));
        plan.backtest.slippage = vec![SlippageOverride {
            instrument: eth.clone(),
            bps: 7.5,
        }];
        assert_eq!(plan.validate("sma_cross"), Ok(()));
        assert_eq!(plan.backtest.slippage_bps(&btc), None);
        assert_eq!(plan.backtest.slippage_bps(&eth), Some(7.5));
        assert_eq!((slip(&plan, &btc), slip(&plan, &eth)), (2.0, 7.5));
        let wf = plan.walk_forward_spec("sma_cross", instrument(&eth));
        assert_eq!(wf.sweep.base.slippage_bps, 7.5);

        let json = serde_json::to_value(&plan).unwrap();
        assert_eq!(
            serde_json::from_value::<TestPlan>(json.clone()).unwrap(),
            plan
        );
        let mut old = json.clone();
        old["backtest"]["slippage_bps"] = serde_json::json!(2.0);
        assert!(serde_json::from_value::<TestPlan>(old).is_err());
        let mut unknown = json;
        unknown["backtest"]["slippage"][0]["spread"] = serde_json::json!(1.0);
        assert!(serde_json::from_value::<TestPlan>(unknown).is_err());
    }

    #[test]
    fn invalid_plans_are_refused() {
        type Tweak = fn(&mut TestPlan);
        let cases: [(&str, Tweak); 16] = [
            ("no instruments", |p| p.instruments.clear()),
            ("an instrument twice", |p| {
                let first = p.instruments[0].clone();
                p.instruments.push(first);
            }),
            ("an empty period", |p| p.period.to = p.period.from),
            ("a period off the bar boundaries", |p| {
                p.period.from += 60_000;
            }),
            ("no starting cash", |p| p.backtest.starting_cash = 0.0),
            ("negative slippage", |p| {
                p.backtest.slippage = vec![SlippageOverride {
                    instrument: p.instruments[0].clone(),
                    bps: -1.0,
                }];
            }),
            ("slippage for an instrument the plan does not test", |p| {
                let mut other = p.instruments[0].clone();
                other.symbol = "SOLUSDT".into();
                p.backtest.slippage = vec![SlippageOverride {
                    instrument: other,
                    bps: 5.0,
                }];
            }),
            ("slippage set twice for one instrument", |p| {
                let o = SlippageOverride {
                    instrument: p.instruments[0].clone(),
                    bps: 3.0,
                };
                p.backtest.slippage = vec![o.clone(), o];
            }),
            ("no leverage", |p| p.backtest.max_leverage = 0.0),
            ("no test days", |p| p.walk_forward.test_days = 0),
            ("days that are not whole weekly bars", |p| {
                p.timeframe = Timeframe::W1;
                p.period = Period {
                    from: date(2018, 1, 1),
                    to: date(2024, 12, 30),
                };
                p.walk_forward.train_days = 365;
            }),
            ("an unknown swept parameter", |p| {
                p.sweep.axes[0].name = "nope".into();
            }),
            ("a parameter both fixed and swept", |p| {
                p.sweep.fixed.set("fast", ParamValue::Int(5));
            }),
            ("a swept value out of bounds", |p| {
                p.sweep.axes[0].space = ParamSpace::Range {
                    from: 0.0,
                    to: 10.0,
                    step: 5.0,
                };
            }),
            ("a split fraction of 1", |p| {
                p.out_of_sample = Some(Split::Fraction(1.0));
            }),
            ("too few windows for the minimum sample", |p| {
                p.period.to = date(2024, 1, 1);
            }),
        ];
        assert_eq!(example_plan().validate("sma_cross"), Ok(()));
        for (name, tweak) in cases {
            let mut plan = example_plan();
            tweak(&mut plan);
            assert!(plan.validate("sma_cross").is_err(), "{name} was accepted");
        }
        assert!(
            example_plan().validate("nope").is_err(),
            "an unknown strategy"
        );
    }

    /// The luck test repeats the sweep once per permutation, and a plan whose luck test would
    /// pass the tester's run limit is refused before the battery spends hours on the steps
    /// before it: a 25 x 25 grid at 199 permutations is 625 x 200 = 125,000 runs.
    #[test]
    fn a_luck_test_over_the_run_limit_is_refused() {
        let mut plan = example_plan();
        let axis = |name: &str, from: f64| ParamAxis {
            name: name.into(),
            space: ParamSpace::Range {
                from,
                to: from + 24.0,
                step: 1.0,
            },
        };
        plan.sweep.axes = vec![axis("fast", 1.0), axis("slow", 30.0)];
        plan.evidence.luck.permutations = 199;
        let err = plan.validate("sma_cross").unwrap_err();
        assert_eq!(
            err.0,
            "the luck test would run 125000 backtests (625 parameter sets times 200 sweeps), \
             above the limit of 100000: shrink the grid or run fewer permutations"
        );
        // 159 permutations are 625 x 160 = 100,000 runs, exactly the limit.
        assert_eq!(625 * 160, MAX_RUNS);
        plan.evidence.luck.permutations = 159;
        assert_eq!(plan.validate("sma_cross"), Ok(()));
    }

    /// R6's windows step by one test window, so a shift moves their edges by the shift modulo
    /// the test length: eight start offsets need at least nine bars per test window, or two of
    /// them repeat the main run or each other.
    #[test]
    fn test_windows_too_short_for_the_start_offsets_are_refused() {
        let mut plan = example_plan();
        assert_eq!(plan.evidence.start_offsets, 8);
        plan.timeframe = Timeframe::D1;
        plan.walk_forward.test_days = 8;
        assert_eq!(
            plan.validate("sma_cross").unwrap_err().0,
            "the walk-forward test window is 8 bars, too short for 8 start offsets: R6 needs at \
             least 9 bars per test window to move the window edges to 8 different places"
        );
        plan.walk_forward.test_days = 9;
        assert_eq!(plan.validate("sma_cross"), Ok(()));
    }

    /// An FX pair is a valid instrument, and its weekends count against the minimum sample: a
    /// period that holds four windows of round-the-clock bars holds fewer of FX bars.
    #[test]
    fn fx_plans_count_only_the_open_hours() {
        let fx = InstrumentId {
            venue: td_core::Venue::Binance,
            symbol: "EURUSD".into(),
            kind: td_core::InstrumentKind::Fx,
        };
        let mut plan = example_plan();
        plan.period = Period {
            from: date(2023, 1, 1),
            to: date(2023, 1, 1) + 420 * DAY_MS,
        };
        assert_eq!(plan.validate("sma_cross"), Ok(()));
        plan.instruments.push(fx.clone());
        let err = plan.validate("sma_cross").unwrap_err();
        assert!(err.0.contains("EURUSD"), "{err}");
        plan.instruments = vec![fx];
        plan.period.to = date(2023, 1, 1) + 600 * DAY_MS;
        assert_eq!(plan.validate("sma_cross"), Ok(()));
    }

    /// The minimum-sample check counts windows as the tester lays them: a period that holds
    /// exactly the minimum passes, one bar less fails.
    #[test]
    fn the_period_must_hold_the_minimum_sample_exactly() {
        let mut plan = example_plan();
        // A 180-day train window and 4 test windows of 60 days: 4 windows, 240 days.
        plan.period = Period {
            from: date(2023, 1, 1),
            to: date(2023, 1, 1) + 420 * DAY_MS,
        };
        assert_eq!(plan.validate("sma_cross"), Ok(()));
        plan.period.to -= 3_600_000;
        assert!(plan.validate("sma_cross").is_err());
        // 4 windows of 40 days are 160 days, below 180.
        plan.walk_forward.test_days = 40;
        plan.period.to = plan.period.from + (180 + 4 * 40) * DAY_MS;
        assert!(plan.validate("sma_cross").is_err());
        plan.period.to += 40 * DAY_MS;
        assert_eq!(plan.validate("sma_cross"), Ok(()));
    }
}
