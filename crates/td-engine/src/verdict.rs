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
#[allow(
    clippy::large_enum_variant,
    reason = "a rule is made once per strategy and copied rarely; boxing the firm's rules would \
              cost the type its Copy"
)]
pub enum VerdictRule {
    /// Traded on a prop firm's account, judged against the firm's loss limits.
    PropFirm {
        firm: FirmLimits,
        limits: PropFirmLimits,
        /// What one attempt costs and what a funded account pays, so the battery can price an
        /// attempt in money and the kind gains its value line. `None`, and absent from a rule
        /// saved before the field existed, prices nothing. Left out of JSON when `None`, so a rule
        /// without terms is written as it was before the field existed.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        attempt: Option<AttemptTerms>,
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

/// A prop firm's own rules. Losses and drawdowns are fractions of the starting balance. Each
/// optional rule is left out of JSON when it is off, so a firm without it is written as it was
/// before the rule existed.
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
    /// The profit that passes the challenge, or its first phase when `second_target` is set.
    pub profit_target: f64,
    /// The profit target of a second phase, which starts from a fresh balance on the day after
    /// the first phase passes. Both phases share the rule's `pass_horizon_days`. `None` is a
    /// one-phase challenge.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub second_target: Option<f64>,
    /// The fewest trading days each phase must hold before its target can pass it: days that
    /// pass `trading_day`'s test (without one, days on which the strategy filled an order),
    /// counted within the phase. 0 sets no minimum.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub min_trading_days: u32,
    /// The best-day rule: a phase passes only once its largest single day's profit is at most
    /// this share of the phase's total profit. `None` sets no such rule.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub best_day_share: Option<f64>,
    /// The day cap: the most one day counts toward an evaluation phase's target, as a share of
    /// that phase's target. A winning day counts its profit up to `day_target_share *
    /// profit_target` (or `second_target` in the second phase); a losing day counts its whole
    /// loss; the phase passes once the counted profit reaches the target. Profit above the cap
    /// stays in the account, and the loss limits act on the account. Independent of
    /// `best_day_share`: with both set, both must hold. The funded account has no such rule.
    /// `None` counts every day in full.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub day_target_share: Option<f64>,
    /// The firm's own test for a trading day, which counts for `min_trading_days` and for the
    /// funded account's `days_to_first_payout`. `None` counts any day with a fill.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trading_day: Option<TradingDayTest>,
    /// For a trailing drawdown, the level the floor stops rising at, as a fraction of the starting
    /// balance above the start: 0 stops it at the starting balance. `None` lets it trail for ever.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trailing_floor_lock: Option<f64>,
}

/// A firm's test for a trading day. A day counts when one of its fills passes the test; each fill
/// is judged on its own, so several small closes on one day do not add up to a large one. With
/// every field off, any fill passes, as without a test.
///
/// A fill closes a trade when it reduces a position: the closed quantity is the part of the
/// position it takes off, so a partial close counts by its own size, and a fill that flips the
/// position closes all of the old one (and opens a new one, which is not a close). Positions and
/// their average entry prices are tracked per instrument across the fills.
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TradingDayTest {
    /// Only a closing fill can count, on the day the close happens: a day that only opens or
    /// adds to a position is not a trading day. Setting either smallest value below implies it.
    #[serde(default, skip_serializing_if = "is_false")]
    pub needs_close: bool,
    /// The smallest value of the closed part of the trade, as a share of the starting balance:
    /// the closed quantity times the position's average entry price, in the account currency at
    /// that price, at the size the strategy was tested at. Since it is judged at that size, a
    /// rule that sets it is refused with an `attempt` whose `evaluation_size` or `funded_size`
    /// is not 1, until the flags are built per size. `None` sets no smallest size.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_trade_size: Option<f64>,
    /// The smallest profit or loss of the closed part, as a share of its own value, before fees:
    /// `|exit price - average entry price| / average entry price`. `None` sets no smallest move.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_trade_move: Option<f64>,
}

impl TradingDayTest {
    /// Whether a fill passes. `close` is `None` for a fill that closes nothing, and for a closing
    /// fill holds the closed part's value, as a share of the starting balance, and its move, as a
    /// share of that value.
    pub(crate) fn passes(&self, close: Option<(f64, f64)>) -> bool {
        let needs_close =
            self.needs_close || self.min_trade_size.is_some() || self.min_trade_move.is_some();
        match close {
            None => !needs_close,
            Some((closed_value, moved)) => {
                self.min_trade_size.is_none_or(|m| closed_value >= m)
                    && self.min_trade_move.is_none_or(|m| moved >= m)
            }
        }
    }
}

/// The terms of one attempt at a prop-firm challenge: its fees, the funded account it leads to
/// and the size traded in each. Money is in the account currency; fractions are of the starting
/// balance. A firm's numbers are plan input; none are built in.
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AttemptTerms {
    /// The starting balance of the evaluation and of the funded account, which turns fractions
    /// into money.
    pub account_size: f64,
    /// The fee paid to start the evaluation.
    pub evaluation_fee: f64,
    /// A fee charged on passing the evaluation.
    #[serde(default)]
    pub pass_fee: f64,
    /// Whether the evaluation fee is paid back with the first payout.
    #[serde(default)]
    pub refund_fee_with_first_payout: bool,
    /// The trader's share of each payout's profit, above 0 and at most 1.
    pub profit_split: f64,
    /// The trading days the funded account must hold before its first payout: the first payout
    /// may be asked for at the close of the day on which the account has held this many trading
    /// days, as the firm's `trading_day` test counts them (without one, days with a fill).
    pub days_to_first_payout: u32,
    /// The days from one payout to the earliest next one, traded or not.
    pub payout_interval_days: u32,
    /// The smallest payout the firm makes, after the split.
    #[serde(default)]
    pub min_payout: f64,
    /// The most profit one payout may count, as a fraction of the starting balance. A payout
    /// counts only the profit above the start, or above the floor if that is higher, and never
    /// moves the floor. `None` sets no cap.
    #[serde(default)]
    pub payout_cap: Option<f64>,
    /// The most payouts one funded account makes. `None` sets no limit.
    #[serde(default)]
    pub max_payouts: Option<u32>,
    /// Whether the first payout fixes the floor for the rest of the account: at the starting
    /// balance, or where the floor stands at that payout if that is higher.
    #[serde(default)]
    pub payout_locks_floor: bool,
    /// Days the funded account is simulated for after a pass.
    pub funded_horizon_days: u32,
    /// The size traded in the evaluation, as a multiple of the size the strategy was tested at:
    /// each daily return is multiplied by it.
    #[serde(default = "one")]
    pub evaluation_size: f64,
    /// The same for the funded account.
    #[serde(default = "one")]
    pub funded_size: f64,
    /// The factor the mean daily return is scaled by for the second set of figures, between 0
    /// and 1: 0.5 asks what an attempt is worth if the edge is half what the test shows.
    #[serde(default = "default_reduced_edge_factor")]
    pub reduced_edge_factor: f64,
}

fn is_zero(n: &u32) -> bool {
    *n == 0
}

fn is_false(b: &bool) -> bool {
    !*b
}

fn one() -> f64 {
    1.0
}

fn default_reduced_edge_factor() -> f64 {
    0.5
}

impl AttemptTerms {
    /// Refuses terms that are not numbers or out of their range, naming the field.
    pub fn validate(&self) -> Result<(), PlanError> {
        positive("attempt.account_size", self.account_size)?;
        non_negative("attempt.evaluation_fee", self.evaluation_fee)?;
        non_negative("attempt.pass_fee", self.pass_fee)?;
        share("attempt.profit_split", self.profit_split)?;
        non_negative("attempt.min_payout", self.min_payout)?;
        if let Some(cap) = self.payout_cap {
            positive("attempt.payout_cap", cap)?;
        }
        for (name, days) in [
            ("days_to_first_payout", Some(self.days_to_first_payout)),
            ("payout_interval_days", Some(self.payout_interval_days)),
            ("funded_horizon_days", Some(self.funded_horizon_days)),
            ("max_payouts", self.max_payouts),
        ] {
            if days == Some(0) {
                return Err(PlanError(format!("attempt.{name} must be at least 1")));
            }
        }
        positive("attempt.evaluation_size", self.evaluation_size)?;
        positive("attempt.funded_size", self.funded_size)?;
        probability("attempt.reduced_edge_factor", self.reduced_edge_factor)?;
        Ok(())
    }
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
    /// The chance of reaching the profit target within `pass_horizon_days`, before breaching a
    /// limit, must be at least this.
    pub min_pass_probability: f64,
    /// Days a simulated challenge runs for the pass odds: a path that has not reached the target
    /// by then counts as not passed. 90 by default, a quarter; a plan for a challenge with no time
    /// limit sets how long it is willing to run one. A rule saved before the field existed reads
    /// as 90, the horizon it was judged on.
    #[serde(default = "default_pass_horizon_days")]
    pub pass_horizon_days: u32,
    /// The chance of breaching a limit within `breach_horizon_days` must be at most this.
    pub max_breach_probability: f64,
    pub breach_horizon_days: u32,
}

fn default_pass_horizon_days() -> u32 {
    90
}

impl Default for PropFirmLimits {
    fn default() -> Self {
        PropFirmLimits {
            share_of_firm_limits: 0.5,
            min_pass_probability: 0.6,
            pass_horizon_days: default_pass_horizon_days(),
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
    /// The 95th percentile of the max drawdown over resampled paths of the daily returns (days
    /// drawn with replacement in blocks, as R7 does), at most this. The default is above `max_drawdown`'s because the 95th percentile of
    /// resampled paths sits well above the drawdown of the one path that happened, and grows with
    /// the length of the out-of-sample period; at 0.45 the line fails a strategy whose drawdown was
    /// small only because its losing days happened not to cluster, not every strategy near the
    /// historical limit. A rule saved before this limit existed reads with the default.
    #[serde(default = "default_max_resampled_drawdown")]
    pub max_resampled_drawdown: f64,
}

fn default_max_resampled_drawdown() -> f64 {
    PortfolioLimits::default().max_resampled_drawdown
}

impl Default for PortfolioLimits {
    fn default() -> Self {
        PortfolioLimits {
            min_sharpe: 1.0,
            max_drawdown: 0.30,
            max_drawdown_vs_benchmark: 0.75,
            min_alpha: 0.0,
            max_p_mean_not_positive: 0.10,
            max_resampled_drawdown: 0.45,
        }
    }
}

#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HighRiskLimits {
    /// Closed out-of-sample trades, at least this many.
    pub min_trades: u32,
    /// Mean net P&L per closed trade strictly above this, in the run's account currency (on a
    /// plan's runs, the one its backtest settings name).
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
            VerdictRule::PropFirm {
                firm,
                limits,
                attempt,
            } => {
                share("firm.daily_loss", firm.daily_loss)?;
                share("firm.max_drawdown", firm.max_drawdown)?;
                positive("firm.profit_target", firm.profit_target)?;
                if let Some(target) = firm.second_target {
                    positive("firm.second_target", target)?;
                }
                if let Some(best) = firm.best_day_share {
                    share("firm.best_day_share", best)?;
                }
                if let Some(cap) = firm.day_target_share {
                    share("firm.day_target_share", cap)?;
                }
                if let Some(test) = firm.trading_day {
                    if let Some(size) = test.min_trade_size {
                        positive("firm.trading_day.min_trade_size", size)?;
                    }
                    if let Some(moved) = test.min_trade_move {
                        positive("firm.trading_day.min_trade_move", moved)?;
                    }
                }
                if let Some(lock) = firm.trailing_floor_lock {
                    if firm.drawdown != DrawdownMode::Trailing {
                        return Err(PlanError(
                            "firm.trailing_floor_lock needs a trailing drawdown: a static floor \
                             never rises"
                                .to_string(),
                        ));
                    }
                    non_negative("firm.trailing_floor_lock", lock)?;
                }
                if let Some(terms) = attempt {
                    terms.validate()?;
                    // The trading-day flags are built once, at the tested size: see the shortcut
                    // on `battery::traded_days`.
                    if firm.trading_day.is_some_and(|t| t.min_trade_size.is_some()) {
                        for (name, size) in [
                            ("evaluation_size", terms.evaluation_size),
                            ("funded_size", terms.funded_size),
                        ] {
                            if size != 1.0 {
                                return Err(PlanError(format!(
                                    "firm.trading_day.min_trade_size needs attempt.{name} of 1: \
                                     the smallest trade size is judged at the size the strategy \
                                     was tested at"
                                )));
                            }
                        }
                    }
                }
                share("limits.share_of_firm_limits", limits.share_of_firm_limits)?;
                probability("limits.min_pass_probability", limits.min_pass_probability)?;
                probability(
                    "limits.max_breach_probability",
                    limits.max_breach_probability,
                )?;
                for (name, days) in [
                    ("pass_horizon_days", limits.pass_horizon_days),
                    ("breach_horizon_days", limits.breach_horizon_days),
                ] {
                    if days == 0 {
                        return Err(PlanError(format!("limits.{name} must be at least 1")));
                    }
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
                share(
                    "limits.max_resampled_drawdown",
                    limits.max_resampled_drawdown,
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
                        "limits.ruin_drawdown must be above 0 and below 1, got {}: ruin is a fall \
                         from the peak of less than the whole account",
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

/// A limit of a strategy's rule that is looser than its kind's default (#113), so a verdict
/// judged under it can say so. A tightened limit is not listed.
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LoosenedLimit {
    /// The limit's field in the kind's limits, such as `min_sharpe`.
    pub name: String,
    /// The rule's value.
    pub value: f64,
    /// The kind's default.
    pub default: f64,
}

/// Which way a limit loosens.
#[derive(Clone, Copy)]
enum Looser {
    /// A lower value lets more through: a minimum, or a horizon the bad outcome must happen in.
    Lower,
    /// A higher value lets more through: a maximum, or a horizon the good outcome may take.
    Higher,
}

impl VerdictRule {
    /// Every limit of the rule's kind that is looser than the kind's default, in the order of the
    /// limits' fields. The firm's own limits in a prop-firm rule and a portfolio's benchmark have
    /// no default and are never listed.
    pub fn loosened(&self) -> Vec<LoosenedLimit> {
        use Looser::{Higher, Lower};
        let limits: Vec<(&str, f64, f64, Looser)> = match self {
            VerdictRule::PropFirm { limits: l, .. } => {
                let d = PropFirmLimits::default();
                vec![
                    (
                        "share_of_firm_limits",
                        l.share_of_firm_limits,
                        d.share_of_firm_limits,
                        Higher,
                    ),
                    (
                        "min_pass_probability",
                        l.min_pass_probability,
                        d.min_pass_probability,
                        Lower,
                    ),
                    // More days to reach the target raise the pass odds.
                    (
                        "pass_horizon_days",
                        f64::from(l.pass_horizon_days),
                        f64::from(d.pass_horizon_days),
                        Higher,
                    ),
                    (
                        "max_breach_probability",
                        l.max_breach_probability,
                        d.max_breach_probability,
                        Higher,
                    ),
                    // Fewer days to breach in lower the breach odds.
                    (
                        "breach_horizon_days",
                        f64::from(l.breach_horizon_days),
                        f64::from(d.breach_horizon_days),
                        Lower,
                    ),
                ]
            }
            VerdictRule::Portfolio { limits: l, .. } => {
                let d = PortfolioLimits::default();
                vec![
                    ("min_sharpe", l.min_sharpe, d.min_sharpe, Lower),
                    ("max_drawdown", l.max_drawdown, d.max_drawdown, Higher),
                    (
                        "max_drawdown_vs_benchmark",
                        l.max_drawdown_vs_benchmark,
                        d.max_drawdown_vs_benchmark,
                        Higher,
                    ),
                    ("min_alpha", l.min_alpha, d.min_alpha, Lower),
                    (
                        "max_p_mean_not_positive",
                        l.max_p_mean_not_positive,
                        d.max_p_mean_not_positive,
                        Higher,
                    ),
                    (
                        "max_resampled_drawdown",
                        l.max_resampled_drawdown,
                        d.max_resampled_drawdown,
                        Higher,
                    ),
                ]
            }
            VerdictRule::HighRisk { limits: l } => {
                let d = HighRiskLimits::default();
                vec![
                    (
                        "min_trades",
                        f64::from(l.min_trades),
                        f64::from(d.min_trades),
                        Lower,
                    ),
                    ("min_expectancy", l.min_expectancy, d.min_expectancy, Lower),
                    (
                        "min_expectancy_lower_bound",
                        l.min_expectancy_lower_bound,
                        d.min_expectancy_lower_bound,
                        Lower,
                    ),
                    (
                        "min_profit_factor",
                        l.min_profit_factor,
                        d.min_profit_factor,
                        Lower,
                    ),
                    (
                        "max_risk_of_ruin",
                        l.max_risk_of_ruin,
                        d.max_risk_of_ruin,
                        Higher,
                    ),
                    // A deeper fall counted as ruin makes ruin rarer.
                    ("ruin_drawdown", l.ruin_drawdown, d.ruin_drawdown, Higher),
                    (
                        "min_return_without_best_trade",
                        l.min_return_without_best_trade,
                        d.min_return_without_best_trade,
                        Lower,
                    ),
                ]
            }
        };
        limits
            .into_iter()
            .filter(|&(_, value, default, looser)| match looser {
                Lower => value < default,
                Higher => value > default,
            })
            .map(|(name, value, default, _)| LoosenedLimit {
                name: name.to_string(),
                value,
                default,
            })
            .collect()
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

fn non_negative(name: &str, v: f64) -> Result<(), PlanError> {
    if v >= 0.0 && v.is_finite() {
        Ok(())
    } else {
        Err(PlanError(format!(
            "{name} must be a finite number of at least 0, got {v}"
        )))
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
    /// An amount of money in the run's account currency: the instrument's quote asset unless the
    /// run is on a base-currency account. The name is kept so saved verdicts still load.
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
/// plan's backtest before anything else runs. I2 and I3 then also run on each parameter set the
/// walk-forward chose, and hold the worst result over every set checked. A check the battery
/// could not make is `None` and fails, which makes the verdict `invalid`.
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct IntegrityInputs {
    /// I1: the buckets of the period's open hours that have no bar, over all of them.
    pub dropped_share: Option<f64>,
    /// I2: 0 when two runs of each backtest checked gave identical fills and equity, 1
    /// otherwise.
    pub repeat_differences: Option<usize>,
    /// I3: the orders decided on the first half of the period that differ between a run on
    /// that half alone and the full run, counting an order only one of them has; the most over
    /// the backtests checked.
    pub prefix_differences: Option<usize>,
}

/// The integrity checks read on the stitched walk-forward out-of-sample curve (I4 and I5),
/// computed once it exists.
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
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
        "Not measured: the out-of-sample curve has no equity ranges inside its bars, which this \
         check reads."
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
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
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
    /// The 95th percentile of the max drawdown over resampled paths of the daily returns (days
    /// drawn with replacement in blocks, as R7 does).
    pub resampled_drawdown: Option<f64>,
    // high_risk
    /// Closed trades.
    pub trades: usize,
    /// Mean net P&L per closed trade, in the run's account currency.
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
    /// The value of one attempt in the account currency with the edge reduced by the rule's
    /// `reduced_edge_factor`, for a rule with attempt terms.
    #[serde(default)]
    pub attempt_value: Option<f64>,
}

/// Judges `inputs` against `rule`: one line per criterion of the rule's kind.
pub fn evaluate(rule: &VerdictRule, x: &KindInputs) -> Vec<Criterion> {
    use Comparison::{Above, AtLeast, AtMost};
    let line = |id, name: &str, value, cmp, limit, unit| {
        Criterion::new(Layer::Kind, id, name, value, cmp, limit, unit)
    };
    match rule {
        VerdictRule::PropFirm {
            firm,
            limits,
            attempt,
        } => {
            let share = limits.share_of_firm_limits;
            let (drawdown, measured) = match firm.drawdown {
                DrawdownMode::Static => (x.intrabar_drawdown_static, "from the starting balance"),
                DrawdownMode::Trailing => (x.intrabar_drawdown_trailing, "from the peak"),
            };
            let mut lines = vec![
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
                    &format!(
                        "Chance of passing the challenge within {} days",
                        limits.pass_horizon_days
                    ),
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
            ];
            if let Some(terms) = attempt {
                lines.push(line(
                    "attempt_value",
                    &format!(
                        "Value of one attempt with the mean daily return scaled by {}",
                        terms.reduced_edge_factor
                    ),
                    x.attempt_value,
                    Above,
                    0.0,
                    Unit::Quote,
                ));
            }
            lines
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
                "resampled_drawdown",
                "Resampled max drawdown (95th percentile)",
                x.resampled_drawdown,
                AtMost,
                limits.max_resampled_drawdown,
                Unit::Percent,
            ));
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
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
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
    /// R4: the out-of-sample return with fees, the per-unit commission, slippage and on a market
    /// with quotes each bar's spread multiplied by the cost stress.
    pub stressed_return: Option<f64>,
    /// R5: the neighbours' median objective over R1's, both out of sample
    /// ([`crate::PlateauReport`]).
    pub plateau_median_ratio: Option<f64>,
    /// R5: the share of neighbours with a positive out-of-sample return.
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
    /// Costs the runs behind the verdict leave out, each named, so a verdict that rests on one is
    /// visible. They do not change the state. Empty in a verdict saved before warnings existed.
    #[serde(default)]
    pub warnings: Vec<String>,
}

impl Verdict {
    pub fn new(instrument: InstrumentId, criteria: Vec<Criterion>) -> Verdict {
        Verdict {
            instrument,
            state: combine(&criteria),
            criteria,
            warnings: Vec::new(),
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

    // Loosened limits

    /// A minimum lowered and a maximum raised are listed with both values; the same limits
    /// tightened, and the defaults themselves, are not.
    #[test]
    fn only_limits_looser_than_the_default_are_listed() {
        let rule = |limits| VerdictRule::Portfolio {
            benchmark: PortfolioBenchmark::BuyAndHold,
            limits,
        };
        assert!(portfolio(PortfolioBenchmark::Cash).loosened().is_empty());
        let loose = rule(PortfolioLimits {
            min_sharpe: 0.5,
            max_drawdown: 0.4,
            ..PortfolioLimits::default()
        });
        assert_eq!(
            loose.loosened(),
            [
                LoosenedLimit {
                    name: "min_sharpe".into(),
                    value: 0.5,
                    default: 1.0
                },
                LoosenedLimit {
                    name: "max_drawdown".into(),
                    value: 0.4,
                    default: 0.30
                },
            ]
        );
        let tight = rule(PortfolioLimits {
            min_sharpe: 1.5,
            max_drawdown: 0.2,
            ..PortfolioLimits::default()
        });
        assert!(tight.loosened().is_empty(), "{:?}", tight.loosened());
    }

    /// Integer limits and horizons go the right way too: fewer trades and a longer pass horizon
    /// loosen, a longer breach horizon tightens and a shorter one loosens.
    #[test]
    fn counts_and_horizons_loosen_in_their_own_direction() {
        let high_risk = VerdictRule::HighRisk {
            limits: HighRiskLimits {
                min_trades: 30,
                max_risk_of_ruin: 0.01,
                ..HighRiskLimits::default()
            },
        };
        let names = |rule: VerdictRule| -> Vec<String> {
            rule.loosened().into_iter().map(|l| l.name).collect()
        };
        assert_eq!(names(high_risk), ["min_trades"]);
        let firm = FirmLimits {
            daily_loss: 0.05,
            max_drawdown: 0.1,
            drawdown: DrawdownMode::Static,
            profit_target: 0.08,
            second_target: None,
            min_trading_days: 0,
            best_day_share: None,
            day_target_share: None,
            trading_day: None,
            trailing_floor_lock: None,
        };
        let prop = |limits| VerdictRule::PropFirm {
            firm,
            limits,
            attempt: None,
        };
        let longer = prop(PropFirmLimits {
            pass_horizon_days: 180,
            breach_horizon_days: 180,
            ..PropFirmLimits::default()
        });
        assert_eq!(names(longer), ["pass_horizon_days"]);
        let shorter = prop(PropFirmLimits {
            pass_horizon_days: 30,
            breach_horizon_days: 30,
            ..PropFirmLimits::default()
        });
        assert_eq!(names(shorter), ["breach_horizon_days"]);
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
            resampled_drawdown: Some(0.18),
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
                "resampled_drawdown",
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
        let resampled = &lines[3];
        assert_eq!(
            (
                resampled.name.as_str(),
                resampled.value,
                resampled.limit,
                resampled.comparison,
                resampled.unit,
            ),
            (
                "Resampled max drawdown (95th percentile)",
                Some(0.18),
                Some(0.45),
                Comparison::AtMost,
                Unit::Percent,
            )
        );
        assert!(lines.iter().all(|c| c.note.is_none()));
    }

    #[test]
    fn portfolio_fails_on_each_criterion() {
        type Tweak = fn(&mut KindInputs);
        let rule = portfolio(PortfolioBenchmark::BuyAndHold);
        let cases: [(&str, Tweak); 9] = [
            ("sharpe", |x| x.sharpe = Some(0.99)),
            ("sharpe", |x| x.sharpe = None),
            ("max_drawdown", |x| {
                // Within 75% of a 50% benchmark drawdown, but above 30%.
                x.benchmark_max_drawdown = Some(0.5);
                x.max_drawdown = Some(0.35);
            }),
            ("drawdown_vs_benchmark", |x| x.max_drawdown = Some(0.19)),
            // The historical drawdown passes both its lines; more than 5% of resampled paths
            // fell past the limit.
            ("resampled_drawdown", |x| x.resampled_drawdown = Some(0.46)),
            // Like the other bootstrap line, a missing value fails with no value.
            ("resampled_drawdown", |x| x.resampled_drawdown = None),
            ("alpha", |x| x.alpha = Some(-0.01)),
            ("p_mean_not_positive", |x| {
                x.p_mean_not_positive = Some(0.11);
            }),
            ("p_mean_not_positive", |x| x.p_mean_not_positive = None),
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
            resampled_drawdown: Some(0.45),
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
            [
                "sharpe",
                "max_drawdown",
                "resampled_drawdown",
                "alpha",
                "p_mean_not_positive"
            ]
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

        let rule = VerdictRule::Portfolio {
            benchmark: PortfolioBenchmark::Cash,
            limits: PortfolioLimits {
                max_resampled_drawdown: 0.15,
                ..PortfolioLimits::default()
            },
        };
        let lines = evaluate(&rule, &good_portfolio());
        assert_eq!(not_passing(&lines), [("resampled_drawdown", Outcome::Fail)]);
        assert_eq!(lines[2].limit, Some(0.15));
    }

    /// A resampled drawdown above the limit makes the verdict no_trade even when the historical
    /// drawdown passes.
    #[test]
    fn a_resampled_drawdown_above_its_limit_gives_no_trade() {
        let integrity = vec![Criterion::new(
            Layer::Integrity,
            "dropped_buckets",
            "Dropped buckets",
            Some(0.0),
            Comparison::AtMost,
            0.01,
            Unit::Percent,
        )];
        let rule = portfolio(PortfolioBenchmark::BuyAndHold);
        let verdict = |kind: KindInputs| {
            let mut criteria = integrity.clone();
            criteria.extend(evaluate_evidence(
                &EvidenceSettings::default(),
                &good_evidence(),
            ));
            criteria.extend(evaluate(&rule, &kind));
            let id = InstrumentId {
                venue: Venue::Binance,
                symbol: "BTCUSDT".into(),
                kind: InstrumentKind::Spot,
            };
            Verdict::new(id, criteria)
        };
        assert_eq!(verdict(good_portfolio()).state, VerdictState::Trade);
        let unlucky = KindInputs {
            resampled_drawdown: Some(0.50),
            ..good_portfolio()
        };
        let v = verdict(unlucky);
        assert_eq!(v.state, VerdictState::NoTrade);
        let line = v
            .criteria
            .iter()
            .find(|c| c.id == "resampled_drawdown")
            .unwrap();
        assert_eq!(
            (line.value, line.limit, line.outcome),
            (Some(0.50), Some(0.45), Outcome::Fail)
        );
    }

    /// A rule saved before the resampled drawdown limit existed still reads, with the default
    /// limit; an unknown field is still refused.
    #[test]
    fn portfolio_limits_saved_without_the_resampled_drawdown_read_with_its_default() {
        let old = serde_json::json!({
            "kind": "portfolio",
            "benchmark": "cash",
            "limits": {
                "min_sharpe": 1.5,
                "max_drawdown": 0.2,
                "max_drawdown_vs_benchmark": 0.75,
                "min_alpha": 0.0,
                "max_p_mean_not_positive": 0.1
            }
        });
        let rule: VerdictRule = serde_json::from_value(old.clone()).unwrap();
        let VerdictRule::Portfolio { limits, .. } = rule else {
            panic!("{rule:?}");
        };
        assert_eq!(limits.max_resampled_drawdown, 0.45);
        assert_eq!((limits.min_sharpe, limits.max_drawdown), (1.5, 0.2));
        let mut unknown = old;
        unknown["limits"]["max_drawdown_typo"] = serde_json::json!(0.1);
        assert!(serde_json::from_value::<VerdictRule>(unknown).is_err());

        // Kind inputs saved before the field existed read it as missing.
        let kind: KindInputs = serde_json::from_value(serde_json::json!({
            "sharpe": 1.2,
            "max_drawdown": 0.1,
            "benchmark_max_drawdown": null,
            "alpha": 0.05,
            "p_mean_not_positive": 0.02,
            "trades": 40,
            "expectancy": null,
            "expectancy_lower_bound": null,
            "profit_factor": null,
            "risk_of_ruin": null,
            "return_without_best_trade": null,
            "worst_daily_loss": null,
            "intrabar_drawdown_static": null,
            "intrabar_drawdown_trailing": null,
            "pass_probability": null,
            "breach_probability": null
        }))
        .unwrap();
        assert_eq!(kind.resampled_drawdown, None);
        assert_eq!(kind.sharpe, Some(1.2));
    }

    // Prop firm

    fn prop(drawdown: DrawdownMode) -> VerdictRule {
        VerdictRule::PropFirm {
            firm: FirmLimits {
                daily_loss: 0.05,
                max_drawdown: 0.10,
                drawdown,
                profit_target: 0.08,
                second_target: None,
                min_trading_days: 0,
                best_day_share: None,
                day_target_share: None,
                trading_day: None,
                trailing_floor_lock: None,
            },
            limits: PropFirmLimits::default(),
            attempt: None,
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
            second_target: None,
            min_trading_days: 0,
            best_day_share: None,
            day_target_share: None,
            trading_day: None,
            trailing_floor_lock: None,
        };
        let bad = [
            VerdictRule::PropFirm {
                firm: FirmLimits {
                    daily_loss: 0.0,
                    ..firm
                },
                limits: PropFirmLimits::default(),
                attempt: None,
            },
            VerdictRule::PropFirm {
                firm,
                limits: PropFirmLimits {
                    min_pass_probability: 1.5,
                    ..PropFirmLimits::default()
                },
                attempt: None,
            },
            VerdictRule::PropFirm {
                firm,
                limits: PropFirmLimits {
                    breach_horizon_days: 0,
                    ..PropFirmLimits::default()
                },
                attempt: None,
            },
            VerdictRule::PropFirm {
                firm,
                limits: PropFirmLimits {
                    pass_horizon_days: 0,
                    ..PropFirmLimits::default()
                },
                attempt: None,
            },
            VerdictRule::Portfolio {
                benchmark: PortfolioBenchmark::BuyAndHold,
                limits: PortfolioLimits {
                    min_sharpe: f64::NAN,
                    ..PortfolioLimits::default()
                },
            },
            VerdictRule::Portfolio {
                benchmark: PortfolioBenchmark::BuyAndHold,
                limits: PortfolioLimits {
                    max_resampled_drawdown: 0.0,
                    ..PortfolioLimits::default()
                },
            },
            VerdictRule::Portfolio {
                benchmark: PortfolioBenchmark::BuyAndHold,
                limits: PortfolioLimits {
                    max_resampled_drawdown: 1.5,
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
        assert_eq!(
            rule(1.0).validate().unwrap_err().0,
            "limits.ruin_drawdown must be above 0 and below 1, got 1: ruin is a fall from the \
             peak of less than the whole account"
        );
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
            assert_eq!(
                n,
                "Not measured: the out-of-sample curve has no equity ranges inside its bars, \
                 which this check reads.",
                "{id}"
            );
        }
    }

    // Prop-firm attempts

    /// Neutral attempt terms that validate.
    fn attempt_terms() -> AttemptTerms {
        AttemptTerms {
            account_size: 100_000.0,
            evaluation_fee: 500.0,
            pass_fee: 0.0,
            refund_fee_with_first_payout: false,
            profit_split: 0.8,
            days_to_first_payout: 14,
            payout_interval_days: 14,
            min_payout: 100.0,
            payout_cap: None,
            max_payouts: None,
            payout_locks_floor: false,
            funded_horizon_days: 180,
            evaluation_size: 1.0,
            funded_size: 1.0,
            reduced_edge_factor: 0.5,
        }
    }

    fn with_attempt(terms: AttemptTerms) -> VerdictRule {
        let VerdictRule::PropFirm { firm, limits, .. } = prop(DrawdownMode::Trailing) else {
            unreachable!()
        };
        VerdictRule::PropFirm {
            firm,
            limits,
            attempt: Some(terms),
        }
    }

    #[test]
    fn a_prop_firm_rule_saved_before_the_new_fields_reads_and_is_judged_as_before() {
        let saved = r#"{"kind":"prop_firm","firm":{"daily_loss":0.05,"max_drawdown":0.1,
            "drawdown":"trailing","profit_target":0.08},"limits":{"share_of_firm_limits":0.5,
            "min_pass_probability":0.6,"pass_horizon_days":90,"max_breach_probability":0.1,
            "breach_horizon_days":90}}"#;
        let rule: VerdictRule = serde_json::from_str(saved).unwrap();
        // Every new field reads as off: one phase, no minimum days, no best-day rule, a floor
        // that trails for ever, and no attempt terms.
        assert_eq!(rule, prop(DrawdownMode::Trailing));
        assert_eq!(rule.validate(), Ok(()));
        // Written back, it is exactly the JSON it was saved as: a field that is off is left out.
        // td-server's verdict list compares the registered rule's JSON with the rule saved in each
        // verdict record, so a verdict saved before the new fields stays current.
        let original: serde_json::Value = serde_json::from_str(saved).unwrap();
        assert_eq!(serde_json::to_value(rule).unwrap(), original);
        let lines = evaluate(&rule, &good_prop());
        assert_eq!(
            ids(&lines),
            [
                "worst_daily_loss",
                "max_drawdown",
                "pass_probability",
                "breach_probability"
            ]
        );
        // Half the firm's 5% and 10%, and the rule's 0.6 and 0.1, as on main.
        let limits: Vec<Option<f64>> = lines.iter().map(|c| c.limit).collect();
        assert_eq!(limits, [Some(0.025), Some(0.05), Some(0.6), Some(0.1)]);
        // good_prop's trailing drawdown is 0.20, over the 0.05 limit; the rest pass.
        assert_eq!(not_passing(&lines), [("max_drawdown", Outcome::Fail)]);
        // A second target cannot stand without the first: a firm without `profit_target` does
        // not read.
        let no_first = r#"{"daily_loss":0.05,"max_drawdown":0.1,"drawdown":"static",
            "second_target":0.05}"#;
        assert!(serde_json::from_str::<FirmLimits>(no_first).is_err());
    }

    #[test]
    fn the_day_cap_and_the_trading_day_test_are_left_out_of_json_when_off() {
        // A rule with every earlier optional field set is written back exactly as it was saved,
        // with neither new field.
        let saved = r#"{"kind":"prop_firm","firm":{"daily_loss":0.05,"max_drawdown":0.1,
            "drawdown":"trailing","profit_target":0.08,"second_target":0.05,"min_trading_days":4,
            "best_day_share":0.5,"trailing_floor_lock":0.0},"limits":{"share_of_firm_limits":0.5,
            "min_pass_probability":0.6,"pass_horizon_days":90,"max_breach_probability":0.1,
            "breach_horizon_days":90}}"#;
        let rule: VerdictRule = serde_json::from_str(saved).unwrap();
        let VerdictRule::PropFirm { firm, .. } = rule else {
            unreachable!()
        };
        assert_eq!((firm.day_target_share, firm.trading_day), (None, None));
        let original: serde_json::Value = serde_json::from_str(saved).unwrap();
        assert_eq!(serde_json::to_value(rule).unwrap(), original);
        // Set, they read and write back; a test's fields that are off are left out of it too.
        let set = r#"{"daily_loss":0.05,"max_drawdown":0.1,"drawdown":"static",
            "profit_target":0.08,"day_target_share":0.4,
            "trading_day":{"needs_close":true,"min_trade_move":0.001}}"#;
        let firm: FirmLimits = serde_json::from_str(set).unwrap();
        assert_eq!(firm.day_target_share, Some(0.4));
        assert_eq!(
            firm.trading_day,
            Some(TradingDayTest {
                needs_close: true,
                min_trade_size: None,
                min_trade_move: Some(0.001),
            })
        );
        let original: serde_json::Value = serde_json::from_str(set).unwrap();
        assert_eq!(serde_json::to_value(firm).unwrap(), original);
        // A misspelt field of the test does not read.
        let typo = r#"{"daily_loss":0.05,"max_drawdown":0.1,"drawdown":"static",
            "profit_target":0.08,"trading_day":{"need_close":true}}"#;
        assert!(serde_json::from_str::<FirmLimits>(typo).is_err());
    }

    #[test]
    fn attempt_terms_read_with_their_defaults() {
        let saved = r#"{"account_size":100000,"evaluation_fee":500,"profit_split":0.8,
            "days_to_first_payout":14,"payout_interval_days":14,"min_payout":100,
            "funded_horizon_days":180}"#;
        let terms: AttemptTerms = serde_json::from_str(saved).unwrap();
        assert_eq!(terms, attempt_terms());
        assert_eq!(with_attempt(terms).validate(), Ok(()));
    }

    #[test]
    fn each_new_field_out_of_range_is_refused_by_name() {
        type Tweak = fn(&mut FirmLimits, &mut AttemptTerms);
        let cases: [(&str, Tweak); 27] = [
            ("firm.second_target", |f, _| f.second_target = Some(0.0)),
            ("firm.second_target", |f, _| {
                f.second_target = Some(f64::NAN)
            }),
            ("firm.best_day_share", |f, _| f.best_day_share = Some(0.0)),
            ("firm.best_day_share", |f, _| f.best_day_share = Some(1.5)),
            ("firm.day_target_share", |f, _| {
                f.day_target_share = Some(0.0)
            }),
            ("firm.day_target_share", |f, _| {
                f.day_target_share = Some(f64::NAN)
            }),
            ("firm.day_target_share", |f, _| {
                f.day_target_share = Some(1.5)
            }),
            ("firm.trading_day.min_trade_size", |f, _| {
                f.trading_day = Some(TradingDayTest {
                    needs_close: false,
                    min_trade_size: Some(0.0),
                    min_trade_move: None,
                })
            }),
            ("firm.trading_day.min_trade_move", |f, _| {
                f.trading_day = Some(TradingDayTest {
                    needs_close: true,
                    min_trade_size: None,
                    min_trade_move: Some(f64::INFINITY),
                })
            }),
            ("firm.trailing_floor_lock", |f, _| {
                f.trailing_floor_lock = Some(-0.01)
            }),
            ("firm.trailing_floor_lock", |f, _| {
                f.trailing_floor_lock = Some(f64::INFINITY)
            }),
            ("firm.trailing_floor_lock", |f, _| {
                f.drawdown = DrawdownMode::Static;
                f.trailing_floor_lock = Some(0.0);
            }),
            ("attempt.account_size", |_, t| t.account_size = 0.0),
            ("attempt.evaluation_fee", |_, t| t.evaluation_fee = -1.0),
            ("attempt.pass_fee", |_, t| t.pass_fee = f64::NAN),
            ("attempt.profit_split", |_, t| t.profit_split = 0.0),
            ("attempt.profit_split", |_, t| t.profit_split = 1.5),
            ("attempt.min_payout", |_, t| t.min_payout = f64::INFINITY),
            ("attempt.payout_cap", |_, t| t.payout_cap = Some(0.0)),
            ("attempt.days_to_first_payout", |_, t| {
                t.days_to_first_payout = 0
            }),
            ("attempt.payout_interval_days", |_, t| {
                t.payout_interval_days = 0
            }),
            ("attempt.funded_horizon_days", |_, t| {
                t.funded_horizon_days = 0
            }),
            ("attempt.max_payouts", |_, t| t.max_payouts = Some(0)),
            ("attempt.evaluation_size", |_, t| t.evaluation_size = 0.0),
            ("attempt.funded_size", |_, t| t.funded_size = f64::INFINITY),
            ("attempt.reduced_edge_factor", |_, t| {
                t.reduced_edge_factor = 1.5
            }),
            ("attempt.reduced_edge_factor", |_, t| {
                t.reduced_edge_factor = f64::NAN
            }),
        ];
        for (name, tweak) in cases {
            let VerdictRule::PropFirm {
                mut firm, limits, ..
            } = prop(DrawdownMode::Trailing)
            else {
                unreachable!()
            };
            let mut terms = attempt_terms();
            tweak(&mut firm, &mut terms);
            let rule = VerdictRule::PropFirm {
                firm,
                limits,
                attempt: Some(terms),
            };
            match rule.validate() {
                Err(PlanError(message)) => assert!(message.contains(name), "{name}: {message}"),
                Ok(()) => panic!("{name} was accepted: {rule:?}"),
            }
        }
        // The edges of each range are allowed: a floor locked at the start, a best day of the
        // whole profit, a full split, no fees, a factor of 0 or 1.
        let mut terms = attempt_terms();
        terms.profit_split = 1.0;
        terms.evaluation_fee = 0.0;
        for factor in [0.0, 1.0] {
            terms.reduced_edge_factor = factor;
            let VerdictRule::PropFirm {
                mut firm, limits, ..
            } = with_attempt(terms)
            else {
                unreachable!()
            };
            firm.trailing_floor_lock = Some(0.0);
            firm.best_day_share = Some(1.0);
            firm.day_target_share = Some(1.0);
            let rule = VerdictRule::PropFirm {
                firm,
                limits,
                attempt: Some(terms),
            };
            assert_eq!(rule.validate(), Ok(()));
        }
        // A smallest trade size is judged at the size the strategy was tested at, so it is refused
        // with an attempt traded at another size, naming both fields.
        let min_size = |min_trade_size| {
            Some(TradingDayTest {
                needs_close: false,
                min_trade_size,
                min_trade_move: Some(0.001),
            })
        };
        let rule = |trading_day, terms: Option<AttemptTerms>| {
            let VerdictRule::PropFirm {
                mut firm, limits, ..
            } = prop(DrawdownMode::Trailing)
            else {
                unreachable!()
            };
            firm.trading_day = trading_day;
            VerdictRule::PropFirm {
                firm,
                limits,
                attempt: terms,
            }
        };
        let sized = |evaluation_size, funded_size| AttemptTerms {
            evaluation_size,
            funded_size,
            ..attempt_terms()
        };
        for (name, terms) in [
            ("attempt.evaluation_size", sized(0.5, 1.0)),
            ("attempt.funded_size", sized(1.0, 2.0)),
            ("attempt.evaluation_size", sized(2.0, 0.5)),
        ] {
            match rule(min_size(Some(0.01)), Some(terms)).validate() {
                Err(PlanError(message)) => assert!(
                    message.contains("firm.trading_day.min_trade_size") && message.contains(name),
                    "{name}: {message}"
                ),
                Ok(()) => panic!("{name} was accepted with a smallest trade size"),
            }
        }
        // Accepted: no attempt, both sizes at exactly 1, or a test with no smallest trade size.
        assert_eq!(rule(min_size(Some(0.01)), None).validate(), Ok(()));
        assert_eq!(
            rule(min_size(Some(0.01)), Some(sized(1.0, 1.0))).validate(),
            Ok(())
        );
        assert_eq!(
            rule(min_size(None), Some(sized(0.5, 2.0))).validate(),
            Ok(())
        );
    }

    #[test]
    fn the_attempt_value_line_needs_attempt_terms_and_a_value_above_zero() {
        // Without attempt terms there is no fifth line, whatever the inputs hold.
        let inputs = KindInputs {
            attempt_value: Some(1_000.0),
            ..good_prop()
        };
        let plain = evaluate(&prop(DrawdownMode::Static), &inputs);
        assert!(!ids(&plain).contains(&"attempt_value"), "{plain:?}");
        let rule = with_attempt(attempt_terms());
        let line = |value| {
            let lines = evaluate(
                &rule,
                &KindInputs {
                    attempt_value: value,
                    ..good_prop()
                },
            );
            assert_eq!(lines.len(), 5);
            lines[4].clone()
        };
        let pass = line(Some(0.01));
        assert_eq!(pass.id, "attempt_value");
        assert_eq!(
            pass.name,
            "Value of one attempt with the mean daily return scaled by 0.5"
        );
        assert_eq!((pass.limit, pass.unit), (Some(0.0), Unit::Quote));
        assert_eq!(pass.outcome, Outcome::Pass);
        for value in [Some(0.0), Some(-500.0), None] {
            assert_eq!(line(value).outcome, Outcome::Fail, "{value:?}");
        }
    }
}
