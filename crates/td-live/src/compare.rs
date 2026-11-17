//! `td-live compare`: a live session beside a backtest of the same strategy, parameters,
//! instrument and money settings over the session's own saved bars, as a paper session is
//! compared (`GET /sessions/{id}/comparison`).
//!
//! It reads the session's folder only: `config.json` and the complete lines of `events.jsonl`.
//! It builds no venue client, needs no keys, sends nothing, takes no lock and writes nothing, so it
//! works on a session that is running, halted, faulted or ended.
//!
//! The session's side is rebuilt by replaying its log through a fresh engine session whose broker
//! plays back the broker records the log holds ([`Playback`]), the way a resume replays it but
//! without the venue: the fills are the venue's fills as the session recorded them, with the
//! venue's prices and fees, and the equity is the session's own model of the account from them.
//! The backtest runs on exactly the bars the log holds, warm-up bars included, with the funding
//! the session was fed given at each rate's due time and the reference bars the log holds for
//! each bar; nothing is downloaded. The two are paired
//! with [`td_engine::compare_with_backtest`]. A rate the session was charged on a later bar than
//! the one it was due in lands on another bar in the backtest, as in a paper session's comparison
//! (td-server's `ComparedBars`), and the report counts those rates.
//!
//! Text read from the folder is printed through `clean`, so a control character in a file cannot
//! reach the owner's terminal.

use std::collections::VecDeque;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard};

use serde::Deserialize;
use td_core::{AccountCurrency, Candle, Fill, Millis, OrderId, Side, Timeframe};
use td_data::time::format_minute;
use td_engine::{
    BacktestConfig, Broker, BrokerEvent, EngineError, Order, Portfolio, Recorded, ReferenceBars,
    Session, SessionComparison, compare_with_backtest, run_strategy_with_references,
};

use crate::BrokerRecord;
use crate::session::{
    BuildStrategy, CONFIG_FILE, EVENTS_FILE, Funding, Line, LiveError, MAX_TEXT, SavedConfig,
    check_id, clean, pairs, panic_message, read_json, read_lines, refuse_link, session_dir,
};

/// A call in a broker record, as [`BrokerRecord`] writes it (the broker's own `Call`, whose fields
/// that module keeps private). Read from the record's JSON so the log can be replayed without a
/// broker, which needs a venue client. The tests replay logs written by real runs, so a change to
/// the record's form fails them.
#[derive(Debug, Deserialize)]
enum RecordedCall {
    Submit {
        order: Order,
    },
    Cancel {
        id: OrderId,
        cancelled: bool,
    },
    Bar {
        open_time: Millis,
        events: Vec<BrokerEvent>,
    },
}

#[derive(Deserialize)]
struct RecordCalls {
    calls: Vec<RecordedCall>,
}

fn calls_of(record: &BrokerRecord) -> Result<Vec<RecordedCall>, String> {
    serde_json::to_value(record)
        .and_then(serde_json::from_value::<RecordCalls>)
        .map(|r| r.calls)
        .map_err(|e| format!("a broker record does not read: {e}"))
}

/// What the replay's broker plays back, shared with the loop that queues each bar's calls.
#[derive(Default)]
struct Script {
    calls: VecDeque<RecordedCall>,
    /// Why the replay stopped matching the record, if it did.
    diverged: Option<String>,
    /// Set while the bar the log ends part way through is fed: only there may the record run out.
    open_ended: bool,
    /// Orders the strategy made on that bar after the record ended.
    past_record: usize,
    /// Whether the strategy sent or cancelled anything on that bar after the record ended. The
    /// log's `sending` line is written before a bar's first send or cancel, so the session did.
    sent_past_record: bool,
}

impl Script {
    /// Notes that the strategy made a call the record does not hold: a divergence, unless the log
    /// ends part way through this bar.
    fn ran_out(&mut self, call: String) {
        if self.open_ended {
            self.sent_past_record = true;
        } else {
            self.diverged
                .get_or_insert(format!("the record ends where the strategy {call}"));
        }
    }
}

/// A broker that answers each call with the one the session's record holds for it, and keeps the
/// open orders as the venue broker's replay keeps them.
struct Playback {
    script: Arc<Mutex<Script>>,
    open: Vec<Order>,
}

/// The most of a recorded call a replay error shows. A call's text can be long (a bar's events),
/// and the error's end says where the replay differs, so the call is cut rather than the error,
/// which stays within the [`MAX_TEXT`] the command prints.
const CALL_TEXT: usize = 400;

/// A recorded call as a replay error shows it: cleaned and cut to [`CALL_TEXT`].
fn shown(call: &dyn std::fmt::Debug) -> String {
    clean(&format!("{call:?}"), CALL_TEXT)
}

fn lock(script: &Mutex<Script>) -> MutexGuard<'_, Script> {
    script.lock().unwrap_or_else(|p| p.into_inner())
}

impl Playback {
    fn close(&mut self, id: OrderId) {
        self.open.retain(|o| o.id != id);
    }
}

impl Broker for Playback {
    fn submit(&mut self, order: Order) {
        let mut script = lock(&self.script);
        match script.calls.pop_front() {
            Some(RecordedCall::Submit { order: recorded }) if recorded == order => {}
            None => {
                script.past_record += 1;
                script.ran_out(format!("submitted order {}", order.id.0));
            }
            Some(other) => {
                script.diverged.get_or_insert(format!(
                    "the record has {} where the strategy submitted order {}, or submitted it differently",
                    shown(&other),
                    order.id.0
                ));
            }
        }
        drop(script);
        self.open.push(order);
        self.open.sort_by_key(|o| o.id.0);
    }

    fn cancel(&mut self, id: OrderId) -> bool {
        let mut script = lock(&self.script);
        let cancelled = match script.calls.pop_front() {
            Some(RecordedCall::Cancel {
                id: recorded,
                cancelled,
            }) if recorded == id => cancelled,
            // Past the record the venue broker sends nothing and answers false for an order it
            // may hold.
            None => {
                script.ran_out(format!("cancelled order {}", id.0));
                false
            }
            Some(other) => {
                script.diverged.get_or_insert(format!(
                    "the record has {} where the strategy cancelled order {}",
                    shown(&other),
                    id.0
                ));
                false
            }
        };
        drop(script);
        if cancelled {
            self.close(id);
        }
        cancelled
    }

    fn on_bar(
        &mut self,
        bar: &Candle,
        _ask: &Candle,
        _portfolio: &Portfolio,
        _breached: &dyn Fn(&Portfolio) -> Option<f64>,
    ) -> Vec<BrokerEvent> {
        let mut script = lock(&self.script);
        let events = match script.calls.pop_front() {
            Some(RecordedCall::Bar { open_time, events }) if open_time == bar.open_time => events,
            other => {
                script.diverged.get_or_insert(format!(
                    "the record has {} where the session fed the bar opening at {}",
                    shown(&other),
                    bar.open_time
                ));
                Vec::new()
            }
        };
        drop(script);
        for event in &events {
            match event {
                BrokerEvent::Filled(fill) => self.close(fill.order_id),
                BrokerEvent::Cancelled { id, .. } => self.close(*id),
                BrokerEvent::Placed(order) => {
                    self.open.push(order.clone());
                    self.open.sort_by_key(|o| o.id.0);
                }
            }
        }
        events
    }

    fn open_orders(&self) -> &[Order] {
        &self.open
    }
}

/// The session's side, rebuilt from its log.
struct Replayed {
    session: Session,
    /// Every bar the log holds, warm-up bars first.
    bars: Vec<Candle>,
    /// Each reference instrument's bars the log holds, as the session was fed them.
    references: Vec<ReferenceBars>,
    /// The open time of the first live bar.
    live_from: Option<Millis>,
    /// Every funding rate fed, warm-up bars' included, at its due time, for the backtest.
    due: Vec<(Millis, f64)>,
    /// The funding rates fed with live bars: the ones the session was charged.
    charged: Vec<Funding>,
    /// The bar the log ends part way through, with the orders made after its record ended.
    cut_short: Option<(Millis, usize)>,
}

/// Replays `lines` through a fresh session of the saved strategy. An error when a line does not
/// replay or the replay does not match the record it holds: the strategy's code then makes other
/// orders than the session made, so there is no fair comparison.
fn replay(saved: &SavedConfig, lines: &[Line], build: BuildStrategy) -> Result<Replayed, String> {
    let config = &saved.config;
    let script = Arc::new(Mutex::new(Script::default()));
    let strategy = build(&config.strategy, &config.params).map_err(|e| e.to_string())?;
    let mut session = Session::with_broker(
        strategy,
        config.instrument.clone(),
        config.timeframe,
        saved.money(),
        Box::new(Playback {
            script: script.clone(),
            open: Vec::new(),
        }),
    )
    .map_err(|e| e.to_string())?;
    session
        .name_references(config.references.clone())
        .map_err(|e| e.to_string())?;
    let mut references: Vec<Vec<Candle>> = vec![Vec::new(); config.references.len()];
    let (mut bars, mut live_from, mut due, mut cut_short) = (Vec::new(), None, Vec::new(), None);
    let mut charged = Vec::new();
    let bad = |i: usize, why: String| format!("line {} of the log: {why}", i + 1);
    let queue = |record: &Option<BrokerRecord>, i: usize| -> Result<(), String> {
        let record = record
            .as_ref()
            .ok_or_else(|| bad(i, "a live bar without its broker record".into()))?;
        lock(&script).calls.extend(calls_of(record)?);
        Ok(())
    };
    let mut i = 0;
    while i < lines.len() {
        let line = &lines[i];
        let funding = pairs(&line.funding);
        bars.push(line.bar);
        due.extend(line.funding.iter().map(|f| (f.due, f.rate)));
        for (bars, slot) in references.iter_mut().zip(&line.references) {
            bars.extend(*slot);
        }
        if line.warm_up {
            session
                .feed(line.bar, None, &funding, &line.references, true)
                .map_err(|e| bad(i, e.to_string()))?;
            i += 1;
            continue;
        }
        live_from.get_or_insert(line.bar.open_time);
        // A bar's `sending` line and its own line carry the same rates; this is the first of them.
        charged.extend_from_slice(&line.funding);
        queue(&line.record, i)?;
        let mut next = i + 1;
        if line.sending {
            match lines.get(i + 1) {
                Some(rest) if rest.bar == line.bar && !rest.warm_up && !rest.sending => {
                    queue(&rest.record, i + 1)?;
                    next = i + 2;
                }
                Some(_) => {
                    return Err(bad(
                        i + 1,
                        "a sending line is not followed by its bar".into(),
                    ));
                }
                None => {
                    cut_short = Some(line.bar.open_time);
                    lock(&script).open_ended = true;
                }
            }
        }
        match session.feed(line.bar, None, &funding, &line.references, false) {
            Ok(()) | Err(EngineError::StrategyFailed(_)) => {}
            Err(e) => return Err(bad(i, e.to_string())),
        }
        if let Some(why) = lock(&script).diverged.take() {
            return Err(bad(
                i,
                format!("the log does not replay with the strategy as it is now: {why}"),
            ));
        }
        i = next;
    }
    let script = lock(&script);
    if let Some(left) = script.calls.front() {
        return Err(format!(
            "the log does not replay with the strategy as it is now: the record has {} that \
             the replay never reached",
            shown(left)
        ));
    }
    if let Some(bar) = cut_short.filter(|_| !script.sent_past_record) {
        return Err(format!(
            "the log does not replay with the strategy as it is now: it ends on the sending line \
             of the bar opening at {}, which the session writes before it sends or cancels, and \
             the replay sent and cancelled nothing on that bar",
            format_minute(bar)
        ));
    }
    let cut_short = cut_short.map(|bar| (bar, script.past_record));
    drop(script);
    let references = config
        .references
        .iter()
        .zip(references)
        .map(|(instrument, bars)| ReferenceBars {
            instrument: instrument.clone(),
            bars,
        })
        .collect();
    Ok(Replayed {
        session,
        bars,
        references,
        live_from,
        due,
        charged,
        cut_short,
    })
}

/// Runs `td-live compare` for session `id` in `data_dir`, writing the report to `out`.
pub(crate) fn compare(
    data_dir: &Path,
    id: &str,
    build: BuildStrategy,
    out: &mut dyn FnMut(&str),
) -> Result<(), String> {
    let error = |e: LiveError| e.to_string();
    check_id(id).map_err(error)?;
    let dir = session_dir(data_dir, id).map_err(error)?;
    let (config_path, log_path) = (dir.join(CONFIG_FILE), dir.join(EVENTS_FILE));
    if !config_path.exists() {
        return Err(LiveError::NotFound(dir).to_string());
    }
    refuse_link(&config_path).map_err(error)?;
    refuse_link(&log_path).map_err(error)?;
    let saved: SavedConfig = read_json(&config_path).map_err(error)?;
    let bytes = std::fs::read(&log_path).map_err(|e| format!("{}: {e}", log_path.display()))?;
    let (lines, _) = read_lines(&bytes, &log_path).map_err(error)?;
    // The replay and the backtest run strategy and engine code on the folder's contents.
    let replayed = catch_unwind(AssertUnwindSafe(|| replay(&saved, &lines, build)))
        .map_err(|p| format!("replaying the log panicked: {}", panic_message(&*p)))??;
    let config = &saved.config;
    let (Some(live_from), Some(first), Some(last)) = (
        replayed.live_from,
        replayed.bars.first(),
        replayed.bars.last(),
    ) else {
        return Err(format!(
            "session {id} has not fed a live bar yet, so there is nothing to compare"
        ));
    };
    let to = last
        .open_time
        .checked_add(config.timeframe.duration_ms())
        .ok_or_else(|| {
            format!(
                "the log's last bar opens at {}, which has no end",
                last.open_time
            )
        })?;
    let backtest = BacktestConfig {
        strategy: config.strategy.clone(),
        params: config.params.clone(),
        instrument: config.instrument.clone(),
        timeframe: config.timeframe,
        from: live_from,
        to,
        starting_cash: saved.starting_balance,
        slippage_bps: config.slippage_bps,
        fee_override: config.fees,
        max_leverage: config.max_leverage,
        warmup_from: (first.open_time < live_from).then_some(first.open_time),
        close_at_end: false,
        // As the session's own account (`SavedConfig::money`).
        account_currency: AccountCurrency::Quote,
        references: config.references.clone(),
        series: Vec::new(),
        also_traded: Vec::new(),
        start_when_all_listed: false,
        short_stocks: None,
    };
    let result = catch_unwind(AssertUnwindSafe(|| {
        let strategy = build(&config.strategy, &config.params)?;
        run_strategy_with_references(
            &backtest,
            strategy,
            &replayed.bars,
            &[],
            &replayed.due,
            &replayed.references,
            &[],
        )
    }))
    .map_err(|p| format!("the backtest panicked: {}", panic_message(&*p)))?
    .map_err(|e| format!("the backtest beside the session failed: {e}"))?;
    let session = &replayed.session;
    let recorded = Recorded::of_session(session, saved.starting_balance);
    let comparison = compare_with_backtest(recorded, Recorded::of_backtest(&result));
    let warm_up_bars = replayed
        .bars
        .iter()
        .filter(|b| b.open_time < live_from)
        .count();
    let live_bars = replayed.bars.len() - warm_up_bars;
    report(
        &Report {
            id,
            saved: &saved,
            live_from,
            last: last.open_time,
            live_bars,
            warm_up_bars,
            charged: replayed.charged.len(),
            late_funding: late_funding(&replayed.charged, config.timeframe),
            cut_short: replayed.cut_short,
            comparison: &comparison,
        },
        out,
    );
    Ok(())
}

struct Report<'a> {
    id: &'a str,
    saved: &'a SavedConfig,
    live_from: Millis,
    last: Millis,
    live_bars: usize,
    warm_up_bars: usize,
    /// The funding rates the session was charged, and how many of them on a later bar than the
    /// one they were due in.
    charged: usize,
    late_funding: usize,
    cut_short: Option<(Millis, usize)>,
    comparison: &'a SessionComparison,
}

/// How many of the `charged` rates were charged on a later bar than the one they were due in,
/// counted as a paper session's comparison counts them (td-server's `ComparedBars`).
fn late_funding(charged: &[Funding], timeframe: Timeframe) -> usize {
    charged
        .iter()
        .filter(|f| timeframe.floor(f.due) < timeframe.floor(f.time))
        .count()
}

/// `x` to at most 8 decimals, without trailing zeros: the venue's amounts have no more, and the
/// floating-point rounding past them is not a difference.
fn amount(x: f64) -> String {
    let text = format!("{x:.8}");
    let text = text.trim_end_matches('0').trim_end_matches('.');
    if text == "-0" {
        "0".into()
    } else {
        text.into()
    }
}

/// [`amount`] with its sign.
fn signed(x: f64) -> String {
    let text = amount(x);
    if text.starts_with('-') || text == "0" {
        text
    } else {
        format!("+{text}")
    }
}

fn side(side: Side) -> &'static str {
    match side {
        Side::Buy => "buy",
        Side::Sell => "sell",
    }
}

fn fill_text(fill: &Fill) -> String {
    format!(
        "{} {} at {} at {}, fee {}",
        side(fill.side),
        amount(fill.qty),
        amount(fill.price),
        format_minute(fill.time),
        amount(fill.fee)
    )
}

fn report(r: &Report, out: &mut dyn FnMut(&str)) {
    let config = &r.saved.config;
    let c = r.comparison;
    out(&format!(
        "compared session {} with a backtest of {} on {} {} over the session's own bars: {} live \
         bars from {} to {}, after {} warm-up bars",
        r.id,
        clean(&config.strategy, MAX_TEXT),
        clean(&config.instrument.id.to_string(), MAX_TEXT),
        config.timeframe,
        r.live_bars,
        format_minute(r.live_from),
        format_minute(r.last),
        r.warm_up_bars,
    ));
    out(&format!(
        "what is compared: the fills the venue reported to the session, as its log recorded them, \
         with the fills a backtest of the same strategy, parameters and money settings makes on \
         the same bars. The backtest fills at the bar's prices with the session's slippage \
         setting ({} bps) and charges the instrument's fee schedule; the venue fills at its own \
         prices and charges its own fees, so a difference in price, time or fee can come from \
         the venue, which the backtest cannot model, as well as from the strategy.",
        amount(config.slippage_bps)
    ));
    if let Some((bar, made)) = r.cut_short {
        out(&format!(
            "the log ends part way through the bar opening at {} (the session stopped, or is \
             still sending), and the {made} orders the strategy made on it after the record \
             ended are counted as unfilled",
            format_minute(bar)
        ));
    }
    out(&format!("matched fills: {}", c.fills.len()));
    for pair in &c.fills {
        out(&format!(
            "  {} {}, decided on the bar opening at {}: session {} at {}, backtest {} at {}; time \
             difference {} ms, price difference {}, quantity difference {}, fee difference {}",
            side(pair.session.side),
            amount(pair.session.qty),
            format_minute(pair.decided_at),
            amount(pair.session.price),
            format_minute(pair.session.time),
            amount(pair.backtest.price),
            format_minute(pair.backtest.time),
            pair.time_diff_ms,
            signed(pair.price_diff),
            signed(pair.qty_diff),
            signed(pair.fee_diff),
        ));
    }
    out(&format!(
        "session fills with no backtest partner: {}",
        c.session_only_fills.len()
    ));
    for fill in &c.session_only_fills {
        out(&format!("  {}", fill_text(fill)));
    }
    out(&format!(
        "backtest fills with no session partner: {}",
        c.backtest_only_fills.len()
    ));
    for fill in &c.backtest_only_fills {
        out(&format!("  {}", fill_text(fill)));
    }
    out(&format!(
        "fees: session {}, backtest {}; funding paid: session {}, backtest {}",
        amount(c.session.fees),
        amount(c.backtest.fees),
        amount(c.session.funding),
        amount(c.backtest.funding),
    ));
    out(&format!(
        "funding rates the session was charged: {}, {} of them on a later bar than the one they \
         were due in. The backtest is given each rate at its due time, so each of those is \
         applied on another bar there. That can change funding, equity and the result without \
         any difference in how orders were filled; how much depends on the position and the \
         price when each rate is applied.",
        r.charged, r.late_funding
    ));
    let ratio = c.pnl_ratio.map_or_else(
        || "none, since the backtest's is zero".to_string(),
        |r| format!("{r:.4}"),
    );
    out(&format!(
        "profit and loss: session {}, backtest {}; the session's over the backtest's: {ratio}",
        amount(c.session.pnl),
        amount(c.backtest.pnl),
    ));
}
