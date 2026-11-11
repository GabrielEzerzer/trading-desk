//! Verdict rules and verdicts (#32, on the layers of #35).
//!
//! A verdict is read in three layers, and a later layer only counts when the earlier ones pass:
//!
//! 1. **Integrity**: any line that neither passes nor warns makes the verdict `invalid`. A
//!    warning (I4, too good to be true) stays on the verdict for the user to acknowledge.
//! 2. **Evidence**, the same for every strategy ([`evaluate_evidence`]): a sample below the
//!    minimum gives `not_enough_evidence`, any failure `no_trade`, and a luck-test p between the
//!    pass and inconclusive thresholds `not_enough_evidence`, in that order.
//! 3. **Kind**, the strategy's own [`VerdictRule`] ([`evaluate`]): any failure gives `no_trade`.
//!
//! A sample too small to judge outranks every failure, in either layer; [`combine`] gives the
//! exact order.
//!
//! Every criterion is one [`Criterion`] line with its value, its limit and its outcome, so the GUI
//! can write "Max drawdown 4.1% against a 6% limit". The battery computes the inputs
//! ([`KindInputs`], [`EvidenceInputs`]) on the stitched walk-forward out-of-sample curve; this
//! module only judges them.

use serde::{Deserialize, Serialize};
use td_core::InstrumentId;

use crate::plan::{EvidenceSettings, MIN_PROFITABLE_WINDOWS, PlanError};

// ---------------------------------------------------------------------------------------------
// Rules
// ---------------------------------------------------------------------------------------------

#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuleKind {
    PropFirm,
    Portfolio,
    HighRisk,
}

/// What counts as tradable for one strategy: a kind with that strategy's limits. Each kind's
/// limits default to #35's; a strategy overrides a field by naming it.
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum VerdictRule {
    /// Traded on a prop firm's account, judged against the firm's loss limits.
    PropFirm {
        firm: FirmLimits,
        limits: PropFirmLimits,
    },
    /// Held as part of a portfolio, judged on risk-adjusted return against a benchmark. Long/short
    /// and funding-carry strategies on perpetuals use `cash`.
    Portfolio {
        benchmark: PortfolioBenchmark,
        limits: PortfolioLimits,
    },
    /// A high-risk play, judged on expectancy and risk of ruin.
    HighRisk { limits: HighRiskLimits },
}

/// A prop firm's own rules. Losses and drawdowns are fractions of the starting balance.
///
/// shortcut: no time limit on the challenge; add one if a firm the desk trades on has it.
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FirmLimits {
    /// The largest loss allowed within one day.
    pub daily_loss: f64,
    /// The largest drawdown allowed, measured as `drawdown` says.
    pub max_drawdown: f64,
    pub drawdown: DrawdownMode,
    /// The profit that passes the challenge.
    pub profit_target: f64,
}

#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DrawdownMode {
    /// From the starting balance.
    Static,
    /// From the running peak of equity.
    Trailing,
}

#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PropFirmLimits {
    /// The worst daily loss and the max drawdown, measured inside bars, must stay within this
    /// share of the firm's limits.
    pub share_of_firm_limits: f64,
    /// The chance of reaching the profit target before breaching a limit must be at least this.
    pub min_pass_probability: f64,
    /// The chance of breaching a limit within `breach_horizon_days` must be at most this.
    pub max_breach_probability: f64,
    pub breach_horizon_days: u32,
}

impl Default for PropFirmLimits {
    fn default() -> Self {
        PropFirmLimits {
            share_of_firm_limits: 0.5,
            min_pass_probability: 0.6,
            max_breach_probability: 0.1,
            breach_horizon_days: 90,
        }
    }
}

#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PortfolioBenchmark {
    /// Buy and hold of the same instrument over the same out-of-sample bars.
    BuyAndHold,
    /// Holding the quote asset: no drawdown, and alpha is the strategy's own return.
    Cash,
}

#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PortfolioLimits {
    /// Out-of-sample Sharpe at least this.
    pub min_sharpe: f64,
    /// Max drawdown, as a fraction of its peak, at most this.
    pub max_drawdown: f64,
    /// Max drawdown at most this share of the benchmark's. Not judged against `cash`, which has
    /// none.
    pub max_drawdown_vs_benchmark: f64,
    /// Annualised alpha against the benchmark strictly above this.
    pub min_alpha: f64,
    /// The bootstrap chance that the mean daily return is zero or less, at most this.
    pub max_p_mean_not_positive: f64,
}

impl Default for PortfolioLimits {
    fn default() -> Self {
        PortfolioLimits {
            min_sharpe: 1.0,
            max_drawdown: 0.30,
            max_drawdown_vs_benchmark: 0.75,
            min_alpha: 0.0,
            max_p_mean_not_positive: 0.10,
        }
    }
}

#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HighRiskLimits {
    /// Closed out-of-sample trades, at least this many.
    pub min_trades: u32,
    /// Mean net P&L per closed trade strictly above this, in the quote asset.
    pub min_expectancy: f64,
    /// The bootstrap's lower bound on that mean strictly above this.
    pub min_expectancy_lower_bound: f64,
    /// Profit factor at least this.
    pub min_profit_factor: f64,
    /// The chance of losing `ruin_drawdown` of the account, at most this.
    pub max_risk_of_ruin: f64,
    /// The drawdown from the running peak of equity that counts as ruin, above 0 and below 1.
    pub ruin_drawdown: f64,
    /// The out-of-sample return with the single best trade removed, strictly above this.
    pub min_return_without_best_trade: f64,
}

impl Default for HighRiskLimits {
    fn default() -> Self {
        HighRiskLimits {
            min_trades: 50,
            min_expectancy: 0.0,
            min_expectancy_lower_bound: 0.0,
            min_profit_factor: 1.3,
            max_risk_of_ruin: 0.05,
            ruin_drawdown: 0.5,
            min_return_without_best_trade: 0.0,
        }
    }
}

impl VerdictRule {
    pub fn kind(&self) -> RuleKind {
        match self {
            VerdictRule::PropFirm { .. } => RuleKind::PropFirm,
            VerdictRule::Portfolio { .. } => RuleKind::Portfolio,
            VerdictRule::HighRisk { .. } => RuleKind::HighRisk,
        }
    }

    /// Refuses limits that are not numbers or out of their range. A strategy may loosen or
    /// tighten its kind's limits; only the evidence layer has floors.
    pub fn validate(&self) -> Result<(), PlanError> {
        match self {
            VerdictRule::PropFirm { firm, limits } => {
                share("firm.daily_loss", firm.daily_loss)?;
                share("firm.max_drawdown", firm.max_drawdown)?;
                positive("firm.profit_target", firm.profit_target)?;
                share("limits.share_of_firm_limits", limits.share_of_firm_limits)?;
                probability("limits.min_pass_probability", limits.min_pass_probability)?;
                probability(
                    "limits.max_breach_probability",
                    limits.max_breach_probability,
                )?;
                if limits.breach_horizon_days == 0 {
                    return Err(PlanError(
                        "limits.breach_horizon_days must be at least 1".into(),
                    ));
                }
            }
            VerdictRule::Portfolio { limits, .. } => {
                finite("limits.min_sharpe", limits.min_sharpe)?;
                share("limits.max_drawdown", limits.max_drawdown)?;
                positive(
                    "limits.max_drawdown_vs_benchmark",
                    limits.max_drawdown_vs_benchmark,
                )?;
                finite("limits.min_alpha", limits.min_alpha)?;
                probability(
                    "limits.max_p_mean_not_positive",
                    limits.max_p_mean_not_positive,
                )?;
            }
            VerdictRule::HighRisk { limits } => {
                finite("limits.min_expectancy", limits.min_expectancy)?;
                finite(
                    "limits.min_expectancy_lower_bound",
                    limits.min_expectancy_lower_bound,
                )?;
                finite("limits.min_profit_factor", limits.min_profit_factor)?;
                probability("limits.max_risk_of_ruin", limits.max_risk_of_ruin)?;
                if !(limits.ruin_drawdown > 0.0 && limits.ruin_drawdown < 1.0) {
                    return Err(PlanError(format!(
                        "limits.ruin_drawdown must be above 0 and below 1, got {}: ruin is a fall                          from the peak of less than the whole account",
                        limits.ruin_drawdown
                    )));
                }
                finite(
                    "limits.min_return_without_best_trade",
                    limits.min_return_without_best_trade,
                )?;
            }
        }
        Ok(())
    }
}

fn finite(name: &str, v: f64) -> Result<(), PlanError> {
    if v.is_finite() {
        Ok(())
    } else {
        Err(PlanError(format!(
            "{name} must be a finite number, got {v}"
        )))
    }
}

fn positive(name: &str, v: f64) -> Result<(), PlanError> {
    if v > 0.0 && v.is_finite() {
        Ok(())
    } else {
        Err(PlanError(format!("{name} must be above 0, got {v}")))
    }
}

/// Above 0 and at most 1.
fn share(name: &str, v: f64) -> Result<(), PlanError> {
    if v > 0.0 && v <= 1.0 {
        Ok(())
    } else {
        Err(PlanError(format!(
            "{name} must be above 0 and at most 1, got {v}"
        )))
    }
}

fn probability(name: &str, v: f64) -> Result<(), PlanError> {
    if (0.0..=1.0).contains(&v) {
        Ok(())
    } else {
        Err(PlanError(format!(
            "{name} must be between 0 and 1, got {v}"
        )))
    }
}

// ---------------------------------------------------------------------------------------------
// Criterion lines
// ---------------------------------------------------------------------------------------------

#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Layer {
    Integrity,
    Evidence,
    Kind,
}

/// How a value is held against its limit.
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Comparison {
    /// `value >= limit`
    AtLeast,
    /// `value > limit`
    Above,
    /// `value <= limit`
    AtMost,
}

impl Comparison {
    /// Whether `value` meets `limit`. A missing or NaN value never does.
    pub fn holds(self, value: Option<f64>, limit: f64) -> bool {
        value.is_some_and(|v| match self {
            Comparison::AtLeast => v >= limit,
            Comparison::Above => v > limit,
            Comparison::AtMost => v <= limit,
        })
    }
}

/// How the GUI writes a value and its limit.
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Unit {
    /// A fraction, written as a percentage: 0.041 is 4.1%.
    Percent,
    /// A plain number: a Sharpe ratio, a profit factor, a p-value.
    Ratio,
    Count,
    Days,
    /// An amount in the instrument's quote asset.
    Quote,
}

#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Pass,
    /// Passes, with a warning the user must acknowledge before trusting the verdict, such as I4's
    /// "too good to be true". It never changes the verdict's state; the line stays on the verdict,
    /// with its `note`, for the GUI to show.
    Warn,
    Fail,
    /// Not enough evidence to judge: a sample below the minimum, a luck-test p between the
    /// thresholds or not computed, or a line with no limit to judge against.
    Inconclusive,
}

/// One criterion of a verdict.
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Criterion {
    pub layer: Layer,
    /// A stable key, such as `max_drawdown`.
    pub id: String,
    /// What the GUI shows, such as "Max drawdown".
    pub name: String,
    /// `None` when the battery could not compute it; the criterion then does not pass.
    pub value: Option<f64>,
    pub comparison: Comparison,
    /// `None` when there is no limit to hold the value against, such as a drawdown against a
    /// benchmark whose drawdown is missing; the line is then inconclusive and `note` says why.
    pub limit: Option<f64>,
    pub unit: Unit,
    pub outcome: Outcome,
    /// Why the line warns or is inconclusive, in words the GUI shows. `None` when the value and
    /// the limit say it all.
    pub note: Option<String>,
}

impl Criterion {
    /// A line that passes when `value` meets `limit` and fails otherwise.
    pub fn new(
        layer: Layer,
        id: &str,
        name: impl Into<String>,
        value: Option<f64>,
        comparison: Comparison,
        limit: f64,
        unit: Unit,
    ) -> Criterion {
        let outcome = if comparison.holds(value, limit) {
            Outcome::Pass
        } else {
            Outcome::Fail
        };
        Criterion {
            layer,
            id: id.to_string(),
            name: name.into(),
            value,
            comparison,
            limit: Some(limit),
            unit,
            outcome,
            note: None,
        }
    }

    /// A line with no limit to hold `value` against: inconclusive, with `note` saying why.
    pub fn without_limit(
        layer: Layer,
        id: &str,
        name: impl Into<String>,
        value: Option<f64>,
        comparison: Comparison,
        unit: Unit,
        note: impl Into<String>,
    ) -> Criterion {
        Criterion {
            layer,
            id: id.to_string(),
            name: name.into(),
            value,
            comparison,
            limit: None,
            unit,
            outcome: Outcome::Inconclusive,
            note: Some(note.into()),
        }
    }

    /// The same line with `note`.
    pub fn with_note(mut self, note: impl Into<String>) -> Criterion {
        self.note = Some(note.into());
        self
    }

    /// The same line, inconclusive instead of failed.
    pub fn inconclusive_on_fail(mut self) -> Criterion {
        if self.outcome == Outcome::Fail {
            self.outcome = Outcome::Inconclusive;
        }
        self
    }

    /// The same line, a warning instead of failed: for an integrity check the user acknowledges
    /// rather than one that makes the verdict `invalid`, such as I4. A line with no value stays
    /// failed: a check that could not be measured is not a warning about what it measured.
    pub fn warning_on_fail(mut self) -> Criterion {
        if self.outcome == Outcome::Fail && self.value.is_some() {
            self.outcome = Outcome::Warn;
        }
        self
    }

    /// The same line with `note` if it warns and has no note yet.
    pub fn with_warning_note(self, note: impl Into<String>) -> Criterion {
        if self.outcome == Outcome::Warn && self.note.is_none() {
            self.with_note(note)
        } else {
            self
        }
    }
}

/// The lines that measure the sample's size, by layer and id: R2's three, and a kind's own
/// minimum trade count. Below its minimum such a line is inconclusive, and [`combine`] reads it
/// before any failure.
const SAMPLE_LINES: [(Layer, &str); 4] = [
    (Layer::Evidence, "oos_trades"),
    (Layer::Evidence, "oos_days"),
    (Layer::Evidence, "windows"),
    (Layer::Kind, "trades"),
];

// ---------------------------------------------------------------------------------------------
// The integrity layer
// ---------------------------------------------------------------------------------------------

/// I1: the largest share of the period's open buckets the data may lack (#35).
pub const MAX_DROPPED_SHARE: f64 = 0.01;

/// I4: an out-of-sample Sharpe ratio above this is too good to be true (#35).
pub const TOO_GOOD_SHARPE: f64 = 4.0;

/// The integrity checks of the data and the engine (I1 to I3), computed by the battery on the
/// plan's backtest before anything else runs. A check the battery could not make is `None` and
/// fails, which makes the verdict `invalid`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct IntegrityInputs {
    /// I1: the buckets of the period's open hours that have no bar, over all of them.
    pub dropped_share: Option<f64>,
    /// I2: 0 when two runs of the same backtest gave identical fills and equity, 1 otherwise.
    pub repeat_differences: Option<usize>,
    /// I3: the orders decided on the first half of the period that differ between a run on
    /// that half alone and the full run, counting an order only one of them has.
    pub prefix_differences: Option<usize>,
}

/// The integrity checks read on the stitched walk-forward out-of-sample curve (I4 and I5),
/// computed once it exists.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct OosIntegrityInputs {
    /// I4: the annualised Sharpe ratio of the curve's daily returns.
    pub sharpe: Option<f64>,
    /// I5: the highest notional over equity inside any bar.
    pub peak_leverage: Option<f64>,
    /// I5: the lowest equity inside any bar, over the starting cash. Zero is a bankrupt account:
    /// a liquidation leaves the account at exactly zero.
    pub lowest_equity: Option<f64>,
    /// Whether the stitched out-of-sample curve exists. A curve can exist without the equity
    /// ranges inside its bars that I5 reads (a test run that kept none), and the lines then say
    /// so instead of blaming the sample.
    #[serde(default)]
    pub curve: bool,
}

/// Judges I1 to I3: one line each, and any line that fails makes the verdict `invalid`.
pub fn evaluate_integrity(x: &IntegrityInputs) -> Vec<Criterion> {
    let line = |id, name: &str, value, limit, unit| {
        Criterion::new(
            Layer::Integrity,
            id,
            name,
            value,
            Comparison::AtMost,
            limit,
            unit,
        )
    };
    let count = |n: Option<usize>| n.map(|n| n as f64);
    vec![
        line(
            "dropped_buckets",
            "Dropped buckets",
            x.dropped_share,
            MAX_DROPPED_SHARE,
            Unit::Percent,
        ),
        line(
            "determinism",
            "Repeat runs with different fills or equity",
            count(x.repeat_differences),
            0.0,
            Unit::Count,
        ),
        line(
            "prefix_consistency",
            "Orders on the first half that differ from the full run's",
            count(x.prefix_differences),
            0.0,
            Unit::Count,
        ),
    ]
}

/// Judges I4 and I5 on the out-of-sample curve against the plan's leverage cap.
///
/// - **I4**, a Sharpe ratio above [`TOO_GOOD_SHARPE`], warns: the user acknowledges it.
/// - **I5** has two lines. Equity reaching zero inside a bar (a bankrupt or liquidated account)
///   fails, so the verdict is `invalid`: a stitched curve that went through zero says nothing
///   about the strategy's edge. Leverage above the cap warns: the engine refuses orders that would
///   take the position over the cap, so leverage above it at the worst point inside a bar comes
///   from the price moving against a position after its entry (a short in a rally), not from a
///   broken run.
///
/// A value that could not be measured warns with a note instead of failing. With no
/// out-of-sample curve, or one too short for a Sharpe ratio, the sample lines of the evidence
/// layer already make that verdict `not_enough_evidence`, and the data behind it passed I1 to
/// I3. A curve without equity ranges inside its bars leaves I5 unchecked, and its lines say that.
pub fn evaluate_oos_integrity(x: &OosIntegrityInputs, max_leverage: f64) -> Vec<Criterion> {
    let unmeasured = |c: Criterion, note: &str| {
        if c.value.is_none() {
            Criterion {
                outcome: Outcome::Warn,
                ..c
            }
            .with_note(note.to_string())
        } else {
            c
        }
    };
    let small = |what: &str| {
        format!("Not measured: {what}. The sample is too small for a verdict either way.")
    };
    let inside_bars = if x.curve {
        "Not measured: the out-of-sample curve has no equity ranges inside its bars, which this          check reads."
            .to_string()
    } else {
        small("there is no out-of-sample curve")
    };
    vec![
        unmeasured(
            Criterion::new(
                Layer::Integrity,
                "too_good_to_be_true",
                "Out-of-sample Sharpe",
                x.sharpe,
                Comparison::AtMost,
                TOO_GOOD_SHARPE,
                Unit::Ratio,
            )
            .warning_on_fail(),
            &small("the out-of-sample curve is missing or too short for a Sharpe ratio"),
        )
        .with_warning_note("Check the strategy before trusting a Sharpe this high."),
        unmeasured(
            Criterion::new(
                Layer::Integrity,
                "lowest_equity",
                "Lowest equity inside a bar",
                x.lowest_equity,
                Comparison::Above,
                0.0,
                Unit::Percent,
            ),
            &inside_bars,
        ),
        unmeasured(
            Criterion::new(
                Layer::Integrity,
                "peak_leverage",
                "Peak leverage inside a bar",
                x.peak_leverage,
                Comparison::AtMost,
                max_leverage,
                Unit::Ratio,
            )
            .warning_on_fail(),
            &inside_bars,
        )
        .with_warning_note(
            "Leverage rose above the plan's cap inside a bar, from the price moving against an \
             open position.",
        ),
    ]
}

// ---------------------------------------------------------------------------------------------
// Kind rules
// ---------------------------------------------------------------------------------------------

/// The out-of-sample statistics the kind rules read, computed by the battery on the stitched
/// walk-forward out-of-sample curve. Fractions are of equity unless said otherwise. A statistic
/// the battery could not compute is `None` and fails its criterion. Each kind reads only its own
/// fields.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct KindInputs {
    // portfolio
    /// Annualised Sharpe ratio of daily returns.
    pub sharpe: Option<f64>,
    /// Max drawdown from the running peak, at bar closes.
    pub max_drawdown: Option<f64>,
    /// The buy-and-hold benchmark's max drawdown over the same bars.
    pub benchmark_max_drawdown: Option<f64>,
    /// Annualised alpha against the rule's benchmark.
    pub alpha: Option<f64>,
    /// The block bootstrap's chance that the mean daily return is zero or less (R7).
    pub p_mean_not_positive: Option<f64>,
    // high_risk
    /// Closed trades.
    pub trades: usize,
    /// Mean net P&L per closed trade, in the quote asset.
    pub expectancy: Option<f64>,
    /// The bootstrap's lower bound on the expectancy.
    pub expectancy_lower_bound: Option<f64>,
    pub profit_factor: Option<f64>,
    /// The chance of a drawdown of the rule's `ruin_drawdown` from the running peak.
    pub risk_of_ruin: Option<f64>,
    /// The total return with the single best trade removed.
    pub return_without_best_trade: Option<f64>,
    // prop_firm: fractions of the starting balance, measured inside bars
    /// The worst loss within one day.
    pub worst_daily_loss: Option<f64>,
    /// Max drawdown from the starting balance.
    pub intrabar_drawdown_static: Option<f64>,
    /// Max drawdown from the running peak.
    pub intrabar_drawdown_trailing: Option<f64>,
    /// The chance of reaching the firm's profit target before breaching a limit.
    pub pass_probability: Option<f64>,
    /// The chance of breaching a limit within the rule's horizon.
    pub breach_probability: Option<f64>,
}

/// Judges `inputs` against `rule`: one line per criterion of the rule's kind.
pub fn evaluate(rule: &VerdictRule, x: &KindInputs) -> Vec<Criterion> {
    use Comparison::{Above, AtLeast, AtMost};
    let line = |id, name: &str, value, cmp, limit, unit| {
        Criterion::new(Layer::Kind, id, name, value, cmp, limit, unit)
    };
    match rule {
        VerdictRule::PropFirm { firm, limits } => {
            let share = limits.share_of_firm_limits;
            let (drawdown, measured) = match firm.drawdown {
                DrawdownMode::Static => (x.intrabar_drawdown_static, "from the starting balance"),
                DrawdownMode::Trailing => (x.intrabar_drawdown_trailing, "from the peak"),
            };
            vec![
                line(
                    "worst_daily_loss",
                    "Worst daily loss",
                    x.worst_daily_loss,
                    AtMost,
                    share * firm.daily_loss,
                    Unit::Percent,
                ),
                line(
                    "max_drawdown",
                    &format!("Max drawdown {measured}"),
                    drawdown,
                    AtMost,
                    share * firm.max_drawdown,
                    Unit::Percent,
                ),
                line(
                    "pass_probability",
                    "Chance of passing the challenge",
                    x.pass_probability,
                    AtLeast,
                    limits.min_pass_probability,
                    Unit::Percent,
                ),
                line(
                    "breach_probability",
                    &format!(
                        "Chance of breaching a limit within {} days",
                        limits.breach_horizon_days
                    ),
                    x.breach_probability,
                    AtMost,
                    limits.max_breach_probability,
                    Unit::Percent,
                ),
            ]
        }
        VerdictRule::Portfolio { benchmark, limits } => {
            let mut lines = vec![
                line(
                    "sharpe",
                    "Out-of-sample Sharpe",
                    x.sharpe,
                    AtLeast,
                    limits.min_sharpe,
                    Unit::Ratio,
                ),
                line(
                    "max_drawdown",
                    "Max drawdown",
                    x.max_drawdown,
                    AtMost,
                    limits.max_drawdown,
                    Unit::Percent,
                ),
            ];
            if *benchmark == PortfolioBenchmark::BuyAndHold {
                let (id, name) = (
                    "drawdown_vs_benchmark",
                    "Max drawdown against the benchmark's",
                );
                lines.push(match x.benchmark_max_drawdown {
                    // The limit is the benchmark's drawdown scaled by the share, so the line
                    // reads as a drawdown against a drawdown.
                    Some(d) => line(
                        id,
                        name,
                        x.max_drawdown,
                        AtMost,
                        limits.max_drawdown_vs_benchmark * d,
                        Unit::Percent,
                    ),
                    None => Criterion::without_limit(
                        Layer::Kind,
                        id,
                        name,
                        x.max_drawdown,
                        AtMost,
                        Unit::Percent,
                        "The benchmark's drawdown could not be computed, so there is no limit to \
                         hold the drawdown against.",
                    ),
                });
            }
            lines.push(line(
                "alpha",
                "Alpha",
                x.alpha,
                Above,
                limits.min_alpha,
                Unit::Percent,
            ));
            lines.push(line(
                "p_mean_not_positive",
                "Chance the mean return is not positive",
                x.p_mean_not_positive,
                AtMost,
                limits.max_p_mean_not_positive,
                Unit::Percent,
            ));
            lines
        }
        VerdictRule::HighRisk { limits } => vec![
            // A trade count is the sample's size: too few trades is not enough evidence, not a
            // failure.
            line(
                "trades",
                "Trades",
                Some(x.trades as f64),
                AtLeast,
                f64::from(limits.min_trades),
                Unit::Count,
            )
            .inconclusive_on_fail(),
            line(
                "expectancy",
                "Expectancy per trade",
                x.expectancy,
                Above,
                limits.min_expectancy,
                Unit::Quote,
            ),
            line(
                "expectancy_lower_bound",
                "Expectancy per trade, bootstrap lower bound",
                x.expectancy_lower_bound,
                Above,
                limits.min_expectancy_lower_bound,
                Unit::Quote,
            ),
            line(
                "profit_factor",
                "Profit factor",
                x.profit_factor,
                AtLeast,
                limits.min_profit_factor,
                Unit::Ratio,
            ),
            line(
                "risk_of_ruin",
                "Risk of ruin",
                x.risk_of_ruin,
                AtMost,
                limits.max_risk_of_ruin,
                Unit::Percent,
            ),
            line(
                "return_without_best_trade",
                "Return without the best trade",
                x.return_without_best_trade,
                Above,
                limits.min_return_without_best_trade,
                Unit::Percent,
            ),
        ],
    }
}

// ---------------------------------------------------------------------------------------------
// The evidence layer
// ---------------------------------------------------------------------------------------------

/// The statistics the evidence layer reads (#35, R1 to R6), computed by the battery.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct EvidenceInputs {
    /// R1: total return of the stitched out-of-sample curve.
    pub oos_return: Option<f64>,
    /// R1: the share of walk-forward windows with a positive return.
    pub profitable_windows: Option<f64>,
    /// R2: closed out-of-sample trades.
    pub oos_trades: usize,
    /// R2: days of bars covered by the stitched out-of-sample curve, as in
    /// [`MinSample`](crate::MinSample): 24 hours of the market's open bars, so the days it is
    /// closed do not count. On data with no gaps this is the windows times `test_days`.
    pub oos_days: f64,
    /// R2: walk-forward windows.
    pub windows: usize,
    /// R3: the luck test's p. `None` when it was cancelled or not run.
    pub luck_p: Option<f64>,
    /// R4: the out-of-sample return with fees and slippage multiplied by the cost stress.
    pub stressed_return: Option<f64>,
    /// R5: the neighbours' median objective over the best set's.
    pub plateau_median_ratio: Option<f64>,
    /// R5: the share of neighbours with a positive return.
    pub plateau_positive_share: Option<f64>,
    /// R6: how many start offsets gave a result of the same sign as the main run.
    pub start_offsets_same_sign: usize,
}

/// Judges `inputs` against the evidence settings: one line per criterion of R1 to R6. The R2
/// lines are inconclusive instead of failed, and so is the luck test's between its thresholds or
/// with no p.
pub fn evaluate_evidence(settings: &EvidenceSettings, x: &EvidenceInputs) -> Vec<Criterion> {
    use Comparison::{Above, AtLeast, AtMost};
    let line = |id, name: &str, value, cmp, limit, unit| {
        Criterion::new(Layer::Evidence, id, name, value, cmp, limit, unit)
    };
    let min = &settings.min_sample;
    let luck = &settings.luck;
    let mut luck_line = line(
        "luck_p",
        "Luck test p-value",
        x.luck_p,
        AtMost,
        luck.pass_p,
        Unit::Ratio,
    );
    if luck_line.outcome == Outcome::Fail && x.luck_p.is_none_or(|p| p <= luck.inconclusive_p) {
        luck_line.outcome = Outcome::Inconclusive;
    }
    vec![
        line(
            "oos_return",
            "Out-of-sample return",
            x.oos_return,
            Above,
            0.0,
            Unit::Percent,
        ),
        line(
            "profitable_windows",
            "Profitable walk-forward windows",
            x.profitable_windows,
            AtLeast,
            MIN_PROFITABLE_WINDOWS,
            Unit::Percent,
        ),
        line(
            "oos_trades",
            "Out-of-sample trades",
            Some(x.oos_trades as f64),
            AtLeast,
            f64::from(min.trades),
            Unit::Count,
        )
        .inconclusive_on_fail(),
        line(
            "oos_days",
            "Out-of-sample days",
            Some(x.oos_days),
            AtLeast,
            f64::from(min.days),
            Unit::Days,
        )
        .inconclusive_on_fail(),
        line(
            "windows",
            "Walk-forward windows",
            Some(x.windows as f64),
            AtLeast,
            f64::from(min.windows),
            Unit::Count,
        )
        .inconclusive_on_fail(),
        luck_line,
        line(
            "stressed_return",
            &format!("Return at {}x costs", settings.cost_stress),
            x.stressed_return,
            Above,
            0.0,
            Unit::Percent,
        ),
        line(
            "plateau_median_ratio",
            "Neighbouring parameters' median against the best",
            x.plateau_median_ratio,
            AtLeast,
            settings.plateau.min_median_ratio,
            Unit::Ratio,
        ),
        line(
            "plateau_positive_share",
            "Profitable neighbouring parameters",
            x.plateau_positive_share,
            AtLeast,
            settings.plateau.min_positive_share,
            Unit::Percent,
        ),
        line(
            "start_offsets",
            "Start offsets with the same sign",
            Some(x.start_offsets_same_sign as f64),
            AtLeast,
            f64::from(settings.start_offsets),
            Unit::Count,
        ),
    ]
}

// ---------------------------------------------------------------------------------------------
// Verdicts
// ---------------------------------------------------------------------------------------------

#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VerdictState {
    Trade,
    NoTrade,
    NotEnoughEvidence,
    /// An integrity check failed, or a layer that had to be judged has no lines.
    Invalid,
}

/// Combines the lines of the three layers into a state. The first rule that applies decides:
///
/// 1. `invalid` if the integrity layer has no lines, or a line that neither passes nor warns.
/// 2. `invalid` if the evidence layer has no lines.
/// 3. `not_enough_evidence` if a sample line is inconclusive: R2's trades, days or windows, or a
///    kind's own minimum trade count (high_risk's 50 trades). A sample too small to confirm an
///    edge cannot confirm a failure either, so it outranks every failure, in either layer.
/// 4. `no_trade` if an evidence line fails.
/// 5. `not_enough_evidence` if an evidence line is inconclusive: the luck test, with p between
///    its thresholds or not computed. A clear failure outranks it, since no p could rescue the
///    strategy.
/// 6. `invalid` if the kind layer has no lines.
/// 7. `no_trade` if a kind line fails.
/// 8. `not_enough_evidence` if a kind line is inconclusive, such as a drawdown against a
///    benchmark whose drawdown is missing.
/// 9. `trade`.
///
/// A warning ([`Outcome::Warn`]) never changes the state. A layer that is reached but has no
/// lines is `invalid`, since nothing checked it, so a battery that stops early need not fill the
/// later layers; it should still fill the kind layer once it has the out-of-sample curve, since
/// rule 3 reads a kind's trade count.
pub fn combine(criteria: &[Criterion]) -> VerdictState {
    use Layer::{Evidence, Integrity, Kind};
    use Outcome::{Fail, Inconclusive, Pass, Warn};
    let layer = |l: Layer| criteria.iter().filter(move |c| c.layer == l);
    let any = |l: Layer, o: Outcome| layer(l).any(|c| c.outcome == o);
    let empty = |l: Layer| layer(l).next().is_none();
    let short_sample = |c: &Criterion| {
        c.outcome == Inconclusive && SAMPLE_LINES.contains(&(c.layer, c.id.as_str()))
    };
    if empty(Integrity) || layer(Integrity).any(|c| !matches!(c.outcome, Pass | Warn)) {
        return VerdictState::Invalid;
    }
    if empty(Evidence) {
        return VerdictState::Invalid;
    }
    if criteria.iter().any(short_sample) {
        return VerdictState::NotEnoughEvidence;
    }
    if any(Evidence, Fail) {
        return VerdictState::NoTrade;
    }
    if any(Evidence, Inconclusive) {
        return VerdictState::NotEnoughEvidence;
    }
    if empty(Kind) {
        return VerdictState::Invalid;
    }
    if any(Kind, Fail) {
        return VerdictState::NoTrade;
    }
    if any(Kind, Inconclusive) {
        return VerdictState::NotEnoughEvidence;
    }
    VerdictState::Trade
}

/// One instrument's verdict.
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Verdict {
    pub instrument: InstrumentId,
    pub state: VerdictState,
    /// Every line of every layer that was judged, integrity first.
    pub criteria: Vec<Criterion>,
}

impl Verdict {
    pub fn new(instrument: InstrumentId, criteria: Vec<Criterion>) -> Verdict {
        Verdict {
            instrument,
            state: combine(&criteria),
            criteria,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use td_core::{InstrumentKind, Venue};

    /// The ids of the lines that did not pass, with their outcome.
    fn not_passing(lines: &[Criterion]) -> Vec<(&str, Outcome)> {
        lines
            .iter()
            .filter(|c| c.outcome != Outcome::Pass)
            .map(|c| (c.id.as_str(), c.outcome))
            .collect()
    }

    fn ids(lines: &[Criterion]) -> Vec<&str> {
        lines.iter().map(|c| c.id.as_str()).collect()
    }

    // Portfolio

    fn portfolio(benchmark: PortfolioBenchmark) -> VerdictRule {
        VerdictRule::Portfolio {
            benchmark,
            limits: PortfolioLimits::default(),
        }
    }

    fn good_portfolio() -> KindInputs {
        KindInputs {
            sharpe: Some(1.5),
            max_drawdown: Some(0.10),
            benchmark_max_drawdown: Some(0.25),
            alpha: Some(0.05),
            p_mean_not_positive: Some(0.02),
            ..KindInputs::default()
        }
    }

    #[test]
    fn portfolio_passes_with_good_inputs() {
        let lines = evaluate(
            &portfolio(PortfolioBenchmark::BuyAndHold),
            &good_portfolio(),
        );
        assert_eq!(
            ids(&lines),
            [
                "sharpe",
                "max_drawdown",
                "drawdown_vs_benchmark",
                "alpha",
                "p_mean_not_positive"
            ]
        );
        assert_eq!(not_passing(&lines), []);
        assert!(lines.iter().all(|c| c.layer == Layer::Kind));
        let dd = &lines[1];
        assert_eq!(
            (dd.name.as_str(), dd.value, dd.limit),
            ("Max drawdown", Some(0.10), Some(0.30))
        );
        assert_eq!(
            (dd.comparison, dd.unit),
            (Comparison::AtMost, Unit::Percent)
        );
        // The benchmark line's limit is 75% of the benchmark's drawdown.
        assert_eq!(lines[2].limit, Some(0.1875));
        assert!(lines.iter().all(|c| c.note.is_none()));
    }

    #[test]
    fn portfolio_fails_on_each_criterion() {
        type Tweak = fn(&mut KindInputs);
        let rule = portfolio(PortfolioBenchmark::BuyAndHold);
        let cases: [(&str, Tweak); 6] = [
            ("sharpe", |x| x.sharpe = Some(0.99)),
            ("sharpe", |x| x.sharpe = None),
            ("max_drawdown", |x| {
                // Within 75% of a 50% benchmark drawdown, but above 30%.
                x.benchmark_max_drawdown = Some(0.5);
                x.max_drawdown = Some(0.35);
            }),
            ("drawdown_vs_benchmark", |x| x.max_drawdown = Some(0.19)),
            ("alpha", |x| x.alpha = Some(-0.01)),
            ("p_mean_not_positive", |x| {
                x.p_mean_not_positive = Some(0.11);
            }),
        ];
        for (id, tweak) in cases {
            let mut x = good_portfolio();
            tweak(&mut x);
            assert_eq!(
                not_passing(&evaluate(&rule, &x)),
                [(id, Outcome::Fail)],
                "{id}"
            );
        }
    }

    #[test]
    fn portfolio_limits_are_inclusive_except_alpha() {
        let rule = portfolio(PortfolioBenchmark::BuyAndHold);
        let edge = KindInputs {
            sharpe: Some(1.0),
            max_drawdown: Some(0.1875),
            benchmark_max_drawdown: Some(0.25),
            alpha: Some(0.0),
            p_mean_not_positive: Some(0.10),
            ..KindInputs::default()
        };
        assert_eq!(
            not_passing(&evaluate(&rule, &edge)),
            [("alpha", Outcome::Fail)]
        );
        let at_30 = KindInputs {
            max_drawdown: Some(0.30),
            benchmark_max_drawdown: Some(0.5),
            ..good_portfolio()
        };
        assert_eq!(not_passing(&evaluate(&rule, &at_30)), []);
    }

    /// Without the benchmark's drawdown there is no limit to judge against: the line is
    /// inconclusive, shows the strategy's own drawdown with no limit, and says why.
    #[test]
    fn a_missing_benchmark_drawdown_is_inconclusive_with_no_limit() {
        let x = KindInputs {
            benchmark_max_drawdown: None,
            ..good_portfolio()
        };
        let lines = evaluate(&portfolio(PortfolioBenchmark::BuyAndHold), &x);
        assert_eq!(
            not_passing(&lines),
            [("drawdown_vs_benchmark", Outcome::Inconclusive)]
        );
        let line = &lines[2];
        assert_eq!((line.value, line.limit), (Some(0.10), None));
        assert!(
            line.note
                .as_deref()
                .is_some_and(|n| n.contains("benchmark's drawdown could not be computed")),
            "{line:?}"
        );
        let json = serde_json::to_value(line).unwrap();
        assert_eq!(json["limit"], serde_json::Value::Null);
        assert_eq!(json["outcome"], "inconclusive");
    }

    #[test]
    fn a_cash_benchmark_has_no_drawdown_comparison() {
        let x = KindInputs {
            benchmark_max_drawdown: None,
            max_drawdown: Some(0.25),
            ..good_portfolio()
        };
        let lines = evaluate(&portfolio(PortfolioBenchmark::Cash), &x);
        assert_eq!(
            ids(&lines),
            ["sharpe", "max_drawdown", "alpha", "p_mean_not_positive"]
        );
        assert_eq!(not_passing(&lines), []);
    }

    #[test]
    fn a_strategy_can_override_its_kinds_limits() {
        let rule = VerdictRule::Portfolio {
            benchmark: PortfolioBenchmark::Cash,
            limits: PortfolioLimits {
                min_sharpe: 2.0,
                ..PortfolioLimits::default()
            },
        };
        let lines = evaluate(&rule, &good_portfolio());
        assert_eq!(not_passing(&lines), [("sharpe", Outcome::Fail)]);
        assert_eq!(lines[0].limit, Some(2.0));
    }

    // Prop firm

    fn prop(drawdown: DrawdownMode) -> VerdictRule {
        VerdictRule::PropFirm {
            firm: FirmLimits {
                daily_loss: 0.05,
                max_drawdown: 0.10,
                drawdown,
                profit_target: 0.08,
            },
            limits: PropFirmLimits::default(),
        }
    }

    fn good_prop() -> KindInputs {
        KindInputs {
            worst_daily_loss: Some(0.01),
            intrabar_drawdown_static: Some(0.03),
            intrabar_drawdown_trailing: Some(0.20),
            pass_probability: Some(0.7),
            breach_probability: Some(0.05),
            ..KindInputs::default()
        }
    }

    #[test]
    fn prop_firm_passes_with_good_inputs() {
        let lines = evaluate(&prop(DrawdownMode::Static), &good_prop());
        assert_eq!(
            ids(&lines),
            [
                "worst_daily_loss",
                "max_drawdown",
                "pass_probability",
                "breach_probability"
            ]
        );
        assert_eq!(not_passing(&lines), []);
        // Half the firm's limits.
        assert_eq!((lines[0].limit, lines[1].limit), (Some(0.025), Some(0.05)));
        assert_eq!(lines[1].value, Some(0.03));
        assert_eq!(lines[1].name, "Max drawdown from the starting balance");
        assert_eq!(lines[3].name, "Chance of breaching a limit within 90 days");
    }

    #[test]
    fn prop_firm_fails_on_each_criterion() {
        type Tweak = fn(&mut KindInputs);
        let rule = prop(DrawdownMode::Static);
        let cases: [(&str, Tweak); 6] = [
            ("worst_daily_loss", |x| x.worst_daily_loss = Some(0.026)),
            ("worst_daily_loss", |x| x.worst_daily_loss = None),
            ("max_drawdown", |x| x.intrabar_drawdown_static = Some(0.051)),
            ("pass_probability", |x| x.pass_probability = Some(0.59)),
            ("breach_probability", |x| x.breach_probability = Some(0.11)),
            ("breach_probability", |x| {
                x.breach_probability = Some(f64::NAN);
            }),
        ];
        for (id, tweak) in cases {
            let mut x = good_prop();
            tweak(&mut x);
            assert_eq!(
                not_passing(&evaluate(&rule, &x)),
                [(id, Outcome::Fail)],
                "{id}"
            );
        }
    }

    #[test]
    fn prop_firm_limits_are_inclusive() {
        let edge = KindInputs {
            worst_daily_loss: Some(0.025),
            intrabar_drawdown_static: Some(0.05),
            pass_probability: Some(0.6),
            breach_probability: Some(0.1),
            ..KindInputs::default()
        };
        assert_eq!(
            not_passing(&evaluate(&prop(DrawdownMode::Static), &edge)),
            []
        );
    }

    #[test]
    fn prop_firm_reads_the_drawdown_the_firm_measures() {
        let lines = evaluate(&prop(DrawdownMode::Trailing), &good_prop());
        assert_eq!(not_passing(&lines), [("max_drawdown", Outcome::Fail)]);
        assert_eq!(lines[1].value, Some(0.20));
        assert_eq!(lines[1].name, "Max drawdown from the peak");
        let x = KindInputs {
            intrabar_drawdown_trailing: Some(0.04),
            intrabar_drawdown_static: Some(0.9),
            ..good_prop()
        };
        assert_eq!(
            not_passing(&evaluate(&prop(DrawdownMode::Trailing), &x)),
            []
        );
    }

    // High risk

    fn high_risk() -> VerdictRule {
        VerdictRule::HighRisk {
            limits: HighRiskLimits::default(),
        }
    }

    fn good_high_risk() -> KindInputs {
        KindInputs {
            trades: 80,
            expectancy: Some(12.0),
            expectancy_lower_bound: Some(1.0),
            profit_factor: Some(1.6),
            risk_of_ruin: Some(0.01),
            return_without_best_trade: Some(0.04),
            ..KindInputs::default()
        }
    }

    #[test]
    fn high_risk_passes_with_good_inputs() {
        let lines = evaluate(&high_risk(), &good_high_risk());
        assert_eq!(
            ids(&lines),
            [
                "trades",
                "expectancy",
                "expectancy_lower_bound",
                "profit_factor",
                "risk_of_ruin",
                "return_without_best_trade"
            ]
        );
        assert_eq!(not_passing(&lines), []);
        assert_eq!((lines[0].value, lines[0].limit), (Some(80.0), Some(50.0)));
    }

    /// Each criterion fails on its own, except the trade count: too few trades is a sample too
    /// small to judge, so that line is inconclusive.
    #[test]
    fn high_risk_fails_on_each_criterion() {
        use Outcome::{Fail, Inconclusive};
        type Tweak = fn(&mut KindInputs);
        let cases: [(&str, Outcome, Tweak); 7] = [
            ("trades", Inconclusive, |x| x.trades = 49),
            ("expectancy", Fail, |x| x.expectancy = Some(-0.5)),
            ("expectancy_lower_bound", Fail, |x| {
                x.expectancy_lower_bound = Some(-3.0);
            }),
            ("profit_factor", Fail, |x| x.profit_factor = Some(1.29)),
            ("profit_factor", Fail, |x| x.profit_factor = None),
            ("risk_of_ruin", Fail, |x| x.risk_of_ruin = Some(0.051)),
            ("return_without_best_trade", Fail, |x| {
                x.return_without_best_trade = Some(-0.01);
            }),
        ];
        for (id, outcome, tweak) in cases {
            let mut x = good_high_risk();
            tweak(&mut x);
            assert_eq!(
                not_passing(&evaluate(&high_risk(), &x)),
                [(id, outcome)],
                "{id}"
            );
        }
    }

    /// Trades, profit factor and risk of ruin pass at their limits; expectancy, its lower bound
    /// and the return without the best trade must be above zero, so zero fails.
    #[test]
    fn high_risk_edges() {
        let at_limits = KindInputs {
            trades: 50,
            profit_factor: Some(1.3),
            risk_of_ruin: Some(0.05),
            ..good_high_risk()
        };
        assert_eq!(not_passing(&evaluate(&high_risk(), &at_limits)), []);
        let zeros = KindInputs {
            expectancy: Some(0.0),
            expectancy_lower_bound: Some(0.0),
            return_without_best_trade: Some(0.0),
            ..good_high_risk()
        };
        assert_eq!(
            not_passing(&evaluate(&high_risk(), &zeros)),
            [
                ("expectancy", Outcome::Fail),
                ("expectancy_lower_bound", Outcome::Fail),
                ("return_without_best_trade", Outcome::Fail)
            ]
        );
    }

    #[test]
    fn rules_are_validated() {
        assert_eq!(high_risk().validate(), Ok(()));
        assert_eq!(prop(DrawdownMode::Trailing).validate(), Ok(()));
        assert_eq!(portfolio(PortfolioBenchmark::Cash).validate(), Ok(()));
        let firm = FirmLimits {
            daily_loss: 0.05,
            max_drawdown: 0.1,
            drawdown: DrawdownMode::Static,
            profit_target: 0.1,
        };
        let bad = [
            VerdictRule::PropFirm {
                firm: FirmLimits {
                    daily_loss: 0.0,
                    ..firm
                },
                limits: PropFirmLimits::default(),
            },
            VerdictRule::PropFirm {
                firm,
                limits: PropFirmLimits {
                    min_pass_probability: 1.5,
                    ..PropFirmLimits::default()
                },
            },
            VerdictRule::PropFirm {
                firm,
                limits: PropFirmLimits {
                    breach_horizon_days: 0,
                    ..PropFirmLimits::default()
                },
            },
            VerdictRule::Portfolio {
                benchmark: PortfolioBenchmark::BuyAndHold,
                limits: PortfolioLimits {
                    min_sharpe: f64::NAN,
                    ..PortfolioLimits::default()
                },
            },
            VerdictRule::HighRisk {
                limits: HighRiskLimits {
                    max_risk_of_ruin: -0.1,
                    ..HighRiskLimits::default()
                },
            },
        ];
        for rule in bad {
            assert!(rule.validate().is_err(), "{rule:?}");
        }
    }

    /// Ruin is a fall from the peak of less than the whole account: a drawdown of 1 can never be
    /// simulated, so it would leave the risk-of-ruin line unmeasured and failing.
    #[test]
    fn a_ruin_drawdown_of_the_whole_account_is_refused() {
        let rule = |ruin_drawdown| VerdictRule::HighRisk {
            limits: HighRiskLimits {
                ruin_drawdown,
                ..HighRiskLimits::default()
            },
        };
        assert_eq!(rule(0.5).validate(), Ok(()));
        assert_eq!(rule(0.99).validate(), Ok(()));
        for bad in [1.0, 1.5, 0.0, f64::NAN] {
            let err = rule(bad).validate().unwrap_err();
            assert!(
                err.0.contains("ruin_drawdown must be above 0 and below 1"),
                "{bad}: {err}"
            );
        }
    }

    #[test]
    fn rules_serialize_with_their_kind() {
        let rule = portfolio(PortfolioBenchmark::BuyAndHold);
        let json = serde_json::to_value(rule).unwrap();
        assert_eq!(json["kind"], "portfolio");
        assert_eq!(json["benchmark"], "buy_and_hold");
        assert_eq!(json["limits"]["min_sharpe"], 1.0);
        assert_eq!(serde_json::from_value::<VerdictRule>(json).unwrap(), rule);
    }

    // The evidence layer

    fn good_evidence() -> EvidenceInputs {
        EvidenceInputs {
            oos_return: Some(0.2),
            profitable_windows: Some(0.75),
            oos_trades: 40,
            oos_days: 240.0,
            windows: 6,
            luck_p: Some(0.01),
            stressed_return: Some(0.1),
            plateau_median_ratio: Some(0.8),
            plateau_positive_share: Some(0.9),
            start_offsets_same_sign: 8,
        }
    }

    #[test]
    fn evidence_passes_with_good_inputs() {
        let lines = evaluate_evidence(&EvidenceSettings::default(), &good_evidence());
        assert_eq!(lines.len(), 10);
        assert_eq!(not_passing(&lines), []);
        assert!(lines.iter().all(|c| c.layer == Layer::Evidence));
        let stressed = lines.iter().find(|c| c.id == "stressed_return").unwrap();
        assert_eq!(stressed.name, "Return at 2x costs");
    }

    #[test]
    fn evidence_lines_fail_or_are_inconclusive() {
        use Outcome::{Fail, Inconclusive};
        type Tweak = fn(&mut EvidenceInputs);
        let cases: [(&str, Outcome, Tweak); 14] = [
            ("oos_return", Fail, |x| x.oos_return = Some(0.0)),
            ("profitable_windows", Fail, |x| {
                x.profitable_windows = Some(0.49);
            }),
            ("oos_trades", Inconclusive, |x| x.oos_trades = 29),
            ("oos_days", Inconclusive, |x| x.oos_days = 179.9),
            ("windows", Inconclusive, |x| x.windows = 3),
            ("luck_p", Inconclusive, |x| x.luck_p = Some(0.051)),
            ("luck_p", Inconclusive, |x| x.luck_p = Some(0.10)),
            ("luck_p", Inconclusive, |x| x.luck_p = None),
            ("luck_p", Fail, |x| x.luck_p = Some(0.1001)),
            ("luck_p", Fail, |x| x.luck_p = Some(f64::NAN)),
            ("stressed_return", Fail, |x| x.stressed_return = Some(-0.01)),
            ("plateau_median_ratio", Fail, |x| {
                x.plateau_median_ratio = Some(0.59);
            }),
            ("plateau_positive_share", Fail, |x| {
                x.plateau_positive_share = Some(0.79);
            }),
            ("start_offsets", Fail, |x| x.start_offsets_same_sign = 7),
        ];
        for (id, outcome, tweak) in cases {
            let mut x = good_evidence();
            tweak(&mut x);
            let lines = evaluate_evidence(&EvidenceSettings::default(), &x);
            assert_eq!(not_passing(&lines), [(id, outcome)], "{id} {outcome:?}");
        }
    }

    #[test]
    fn evidence_limits_are_inclusive_at_the_minimum() {
        let edge = EvidenceInputs {
            profitable_windows: Some(0.5),
            oos_trades: 30,
            oos_days: 180.0,
            windows: 4,
            luck_p: Some(0.05),
            plateau_median_ratio: Some(0.6),
            plateau_positive_share: Some(0.8),
            ..good_evidence()
        };
        let lines = evaluate_evidence(&EvidenceSettings::default(), &edge);
        assert_eq!(not_passing(&lines), []);
    }

    #[test]
    fn tuned_evidence_settings_move_the_limits() {
        let mut settings = EvidenceSettings::default();
        settings.min_sample.trades = 50;
        settings.luck.pass_p = 0.02;
        settings.luck.inconclusive_p = 0.04;
        let x = EvidenceInputs {
            luck_p: Some(0.05),
            ..good_evidence()
        };
        let lines = evaluate_evidence(&settings, &x);
        assert_eq!(
            not_passing(&lines),
            [
                ("oos_trades", Outcome::Inconclusive),
                ("luck_p", Outcome::Fail)
            ]
        );
    }

    // Combining the layers

    fn line(layer: Layer, outcome: Outcome) -> Criterion {
        line_id(layer, "x", outcome)
    }

    fn line_id(layer: Layer, id: &str, outcome: Outcome) -> Criterion {
        let mut c = Criterion::new(
            layer,
            id,
            "X",
            Some(1.0),
            Comparison::AtLeast,
            0.0,
            Unit::Ratio,
        );
        c.outcome = outcome;
        c
    }

    #[test]
    fn layers_combine_into_four_states() {
        use Layer::{Evidence, Integrity, Kind};
        use Outcome::{Fail, Inconclusive, Pass};
        use VerdictState::{Invalid, NoTrade, NotEnoughEvidence, Trade};
        type Case = (&'static str, Vec<(Layer, Outcome)>, VerdictState);
        let cases: [Case; 12] = [
            (
                "all pass",
                vec![(Integrity, Pass), (Evidence, Pass), (Kind, Pass)],
                Trade,
            ),
            (
                "integrity fails, whatever follows",
                vec![
                    (Integrity, Pass),
                    (Integrity, Fail),
                    (Evidence, Inconclusive),
                    (Kind, Pass),
                ],
                Invalid,
            ),
            (
                "integrity inconclusive",
                vec![(Integrity, Inconclusive), (Evidence, Pass), (Kind, Pass)],
                Invalid,
            ),
            (
                "no integrity lines",
                vec![(Evidence, Pass), (Kind, Pass)],
                Invalid,
            ),
            (
                "integrity fails before any later layer",
                vec![(Integrity, Fail)],
                Invalid,
            ),
            (
                "evidence inconclusive",
                vec![
                    (Integrity, Pass),
                    (Evidence, Pass),
                    (Evidence, Inconclusive),
                    (Kind, Pass),
                ],
                NotEnoughEvidence,
            ),
            (
                "evidence inconclusive with the kind layer not run",
                vec![(Integrity, Pass), (Evidence, Inconclusive)],
                NotEnoughEvidence,
            ),
            (
                "no evidence lines",
                vec![(Integrity, Pass), (Kind, Pass)],
                Invalid,
            ),
            (
                "evidence fails",
                vec![(Integrity, Pass), (Evidence, Fail), (Kind, Pass)],
                NoTrade,
            ),
            (
                "evidence fails with the kind layer not run",
                vec![(Integrity, Pass), (Evidence, Fail)],
                NoTrade,
            ),
            (
                "kind fails",
                vec![
                    (Integrity, Pass),
                    (Evidence, Pass),
                    (Kind, Pass),
                    (Kind, Fail),
                ],
                NoTrade,
            ),
            (
                "no kind lines",
                vec![(Integrity, Pass), (Evidence, Pass)],
                Invalid,
            ),
        ];
        for (name, lines, want) in cases {
            let criteria: Vec<Criterion> = lines.iter().map(|&(l, o)| line(l, o)).collect();
            assert_eq!(combine(&criteria), want, "{name}");
        }
        assert_eq!(combine(&[]), Invalid);
    }

    /// One case per step of `combine`'s order, each with the next step's condition also present,
    /// so a case only passes when its step comes first.
    #[test]
    fn combine_reads_its_rules_in_order() {
        use Layer::{Evidence, Integrity, Kind};
        use Outcome::{Fail, Inconclusive, Pass, Warn};
        use VerdictState::{Invalid, NoTrade, NotEnoughEvidence, Trade};
        type Case = (
            &'static str,
            Vec<(Layer, &'static str, Outcome)>,
            VerdictState,
        );
        let cases: [Case; 12] = [
            (
                "1. integrity fails before a short sample",
                vec![
                    (Integrity, "x", Fail),
                    (Evidence, "oos_trades", Inconclusive),
                ],
                Invalid,
            ),
            (
                "3. a short R2 sample before an evidence failure",
                vec![
                    (Integrity, "x", Pass),
                    (Evidence, "oos_return", Fail),
                    (Evidence, "oos_days", Inconclusive),
                    (Kind, "x", Pass),
                ],
                NotEnoughEvidence,
            ),
            (
                "3. a short R2 window count before an evidence failure",
                vec![
                    (Integrity, "x", Pass),
                    (Evidence, "windows", Inconclusive),
                    (Evidence, "oos_return", Fail),
                ],
                NotEnoughEvidence,
            ),
            (
                "3. a kind's short trade count before an evidence failure",
                vec![
                    (Integrity, "x", Pass),
                    (Evidence, "oos_return", Fail),
                    (Kind, "trades", Inconclusive),
                ],
                NotEnoughEvidence,
            ),
            (
                "4. an evidence failure before an inconclusive luck test",
                vec![
                    (Integrity, "x", Pass),
                    (Evidence, "oos_return", Fail),
                    (Evidence, "luck_p", Inconclusive),
                    (Kind, "x", Pass),
                ],
                NoTrade,
            ),
            (
                "5. an inconclusive luck test before a kind failure",
                vec![
                    (Integrity, "x", Pass),
                    (Evidence, "luck_p", Inconclusive),
                    (Kind, "profit_factor", Fail),
                ],
                NotEnoughEvidence,
            ),
            (
                "5. an inconclusive luck test before missing kind lines",
                vec![(Integrity, "x", Pass), (Evidence, "luck_p", Inconclusive)],
                NotEnoughEvidence,
            ),
            (
                "6. missing kind lines",
                vec![(Integrity, "x", Pass), (Evidence, "x", Pass)],
                Invalid,
            ),
            (
                "7. a kind failure before an inconclusive kind line",
                vec![
                    (Integrity, "x", Pass),
                    (Evidence, "x", Pass),
                    (Kind, "drawdown_vs_benchmark", Inconclusive),
                    (Kind, "alpha", Fail),
                ],
                NoTrade,
            ),
            (
                "8. an inconclusive kind line that is not a sample",
                vec![
                    (Integrity, "x", Pass),
                    (Evidence, "x", Pass),
                    (Kind, "drawdown_vs_benchmark", Inconclusive),
                ],
                NotEnoughEvidence,
            ),
            (
                "9. a warning does not change a trade",
                vec![
                    (Integrity, "x", Pass),
                    (Integrity, "too_good", Warn),
                    (Evidence, "x", Pass),
                    (Kind, "x", Pass),
                ],
                Trade,
            ),
            (
                "1. integrity with only a warning still counts as checked",
                vec![
                    (Integrity, "too_good", Warn),
                    (Evidence, "oos_return", Fail),
                ],
                NoTrade,
            ),
        ];
        for (name, lines, want) in cases {
            let criteria: Vec<Criterion> =
                lines.iter().map(|&(l, id, o)| line_id(l, id, o)).collect();
            assert_eq!(combine(&criteria), want, "{name}");
        }
    }

    /// An I4-style check warns instead of failing: the line keeps its outcome and note on the
    /// verdict, and the state is what the other lines make it.
    #[test]
    fn a_warning_stays_on_the_verdict_without_changing_its_state() {
        let too_good = Criterion::new(
            Layer::Integrity,
            "too_good_to_be_true",
            "Out-of-sample Sharpe",
            Some(5.2),
            Comparison::AtMost,
            4.0,
            Unit::Ratio,
        )
        .warning_on_fail()
        .with_note("Check the strategy before trusting a Sharpe this high.");
        assert_eq!(too_good.outcome, Outcome::Warn);
        let mut criteria = vec![too_good];
        criteria.extend(evaluate_evidence(
            &EvidenceSettings::default(),
            &good_evidence(),
        ));
        criteria.extend(evaluate(&high_risk(), &good_high_risk()));
        let id = InstrumentId {
            venue: Venue::Binance,
            symbol: "BTCUSDT".into(),
            kind: InstrumentKind::Spot,
        };
        let v = Verdict::new(id, criteria);
        assert_eq!(v.state, VerdictState::Trade);
        assert_eq!(v.criteria[0].outcome, Outcome::Warn);
        let json = serde_json::to_value(&v.criteria[0]).unwrap();
        assert_eq!(json["outcome"], "warn");
        assert_eq!(
            json["note"],
            "Check the strategy before trusting a Sharpe this high."
        );
        // A passing check does not warn.
        let calm = Criterion::new(
            Layer::Integrity,
            "too_good_to_be_true",
            "Out-of-sample Sharpe",
            Some(2.0),
            Comparison::AtMost,
            4.0,
            Unit::Ratio,
        )
        .warning_on_fail();
        assert_eq!(calm.outcome, Outcome::Pass);
        // A check with no value stays failed rather than warning.
        let unmeasured = Criterion::new(
            Layer::Integrity,
            "too_good_to_be_true",
            "Out-of-sample Sharpe",
            None,
            Comparison::AtMost,
            4.0,
            Unit::Ratio,
        )
        .warning_on_fail();
        assert_eq!(unmeasured.outcome, Outcome::Fail);
    }

    /// The evaluators' own lines combine as #35 says: a sample below the minimum and a luck p
    /// between the thresholds give not_enough_evidence, a kind failure gives no_trade.
    #[test]
    fn evaluated_lines_combine_into_a_verdict() {
        let integrity = vec![Criterion::new(
            Layer::Integrity,
            "dropped_buckets",
            "Dropped buckets",
            Some(0.0),
            Comparison::AtMost,
            0.01,
            Unit::Percent,
        )];
        let rule = high_risk();
        let settings = EvidenceSettings::default();
        let verdict = |evidence: EvidenceInputs, kind: KindInputs| {
            let mut criteria = integrity.clone();
            criteria.extend(evaluate_evidence(&settings, &evidence));
            criteria.extend(evaluate(&rule, &kind));
            let id = InstrumentId {
                venue: Venue::Binance,
                symbol: "BTCUSDT".into(),
                kind: InstrumentKind::Spot,
            };
            Verdict::new(id, criteria)
        };
        let v = verdict(good_evidence(), good_high_risk());
        assert_eq!(v.state, VerdictState::Trade);
        assert_eq!(v.instrument.symbol, "BTCUSDT");
        assert_eq!(v.criteria.len(), 1 + 10 + 6);
        let few = EvidenceInputs {
            oos_trades: 12,
            ..good_evidence()
        };
        assert_eq!(
            verdict(few, good_high_risk()).state,
            VerdictState::NotEnoughEvidence
        );
        let lucky = EvidenceInputs {
            luck_p: Some(0.08),
            ..good_evidence()
        };
        assert_eq!(
            verdict(lucky, good_high_risk()).state,
            VerdictState::NotEnoughEvidence
        );
        let unlucky = EvidenceInputs {
            luck_p: Some(0.3),
            ..good_evidence()
        };
        assert_eq!(
            verdict(unlucky, good_high_risk()).state,
            VerdictState::NoTrade
        );
        let weak = KindInputs {
            profit_factor: Some(1.1),
            ..good_high_risk()
        };
        assert_eq!(verdict(good_evidence(), weak).state, VerdictState::NoTrade);

        // A clear evidence failure outranks an inconclusive luck test: the sample is big enough,
        // the out-of-sample return is -12% with a quarter of the windows profitable, and the
        // stressed return is negative, so no p could rescue it.
        for luck_p in [Some(0.07), None] {
            let losing = EvidenceInputs {
                oos_return: Some(-0.12),
                profitable_windows: Some(0.25),
                stressed_return: Some(-0.2),
                luck_p,
                ..good_evidence()
            };
            assert_eq!(
                verdict(losing, good_high_risk()).state,
                VerdictState::NoTrade,
                "luck_p {luck_p:?}"
            );
        }
        // A high-risk strategy with 40 out-of-sample trades clears R2's 30 but not its kind's 50:
        // not enough evidence, even when the evidence layer also fails.
        let forty = |e: EvidenceInputs| {
            verdict(
                EvidenceInputs {
                    oos_trades: 40,
                    ..e
                },
                KindInputs {
                    trades: 40,
                    ..good_high_risk()
                },
            )
        };
        assert_eq!(
            forty(good_evidence()).state,
            VerdictState::NotEnoughEvidence
        );
        let losing = EvidenceInputs {
            oos_return: Some(-0.12),
            ..good_evidence()
        };
        assert_eq!(forty(losing).state, VerdictState::NotEnoughEvidence);
    }

    #[test]
    fn integrity_lines_fail_on_bad_data_and_warn_on_suspicious_results() {
        let clean = IntegrityInputs {
            dropped_share: Some(0.01),
            repeat_differences: Some(0),
            prefix_differences: Some(0),
        };
        assert!(not_passing(&evaluate_integrity(&clean)).is_empty());
        let bad = IntegrityInputs {
            dropped_share: Some(0.011),
            repeat_differences: Some(1),
            prefix_differences: None,
        };
        assert_eq!(
            not_passing(&evaluate_integrity(&bad)),
            vec![
                ("dropped_buckets", Outcome::Fail),
                ("determinism", Outcome::Fail),
                ("prefix_consistency", Outcome::Fail),
            ]
        );

        let calm = OosIntegrityInputs {
            sharpe: Some(2.0),
            peak_leverage: Some(1.0),
            lowest_equity: Some(0.8),
            curve: true,
        };
        assert!(not_passing(&evaluate_oos_integrity(&calm, 1.0)).is_empty());
        // A Sharpe above the limit and leverage above the cap warn; a bankrupt curve fails.
        let wild = OosIntegrityInputs {
            sharpe: Some(TOO_GOOD_SHARPE + 0.1),
            peak_leverage: Some(1.2),
            lowest_equity: Some(0.0),
            curve: true,
        };
        let lines = evaluate_oos_integrity(&wild, 1.0);
        assert_eq!(
            not_passing(&lines),
            vec![
                ("too_good_to_be_true", Outcome::Warn),
                ("lowest_equity", Outcome::Fail),
                ("peak_leverage", Outcome::Warn),
            ]
        );
        assert!(
            lines
                .iter()
                .all(|l| l.outcome != Outcome::Warn || l.note.is_some())
        );
        // Nothing measured: the warnings stay warnings, with a note, and the bankruptcy check
        // warns too rather than failing a run that never had a curve.
        let lines = evaluate_oos_integrity(&OosIntegrityInputs::default(), 1.0);
        assert!(
            lines
                .iter()
                .all(|l| l.outcome == Outcome::Warn && l.note.is_some())
        );
        let note = |lines: &[Criterion], id: &str| {
            let l = lines.iter().find(|l| l.id == id).unwrap();
            assert_eq!(l.outcome, Outcome::Warn, "{id}");
            l.note.clone().unwrap()
        };
        assert!(note(&lines, "lowest_equity").contains("no out-of-sample curve"));
        // A curve with no ranges inside its bars: the lines read on those ranges say so, not
        // that the sample is too small.
        let no_ranges = OosIntegrityInputs {
            sharpe: Some(1.0),
            curve: true,
            ..OosIntegrityInputs::default()
        };
        let lines = evaluate_oos_integrity(&no_ranges, 1.0);
        for id in ["lowest_equity", "peak_leverage"] {
            let n = note(&lines, id);
            assert!(n.contains("no equity ranges inside its bars"), "{id}: {n}");
            assert!(!n.contains("too small"), "{id}: {n}");
        }
    }
}
