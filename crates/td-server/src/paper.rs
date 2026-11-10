//! A paper session's state and its files: the engine [`Session`] it feeds, the config it was made
//! from, and the append-only log of every bar fed, which is what a resume replays.
//!
//! Files live in `<data_dir>/paper/<session_id>/`: `config.json`, written once and synced to disk
//! before it takes its name, and `events.jsonl`, one [`FedBar`] per line. A line is appended after
//! its bar is applied, so the log never holds a bar the session refused. Each append goes straight
//! to the operating system, so it survives the process dying. Each live bar's line is also synced
//! to disk, with the warm-up lines before it, so a power cut loses at most the bar being written
//! and any warm-up lines not yet followed by a live bar; a resume fetches those again.

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use td_core::{Candle, FeeSchedule, Fill, Instrument, Millis, Timeframe};
use td_engine::{EquityPoint, Order, Params, Portfolio, Session, SessionConfig, build_strategy};

use crate::PaperError;

const CONFIG_FILE: &str = "config.json";
const EVENTS_FILE: &str = "events.jsonl";

/// What a paper session runs: a registered strategy with its parameters, on one instrument and
/// timeframe, with the same money settings as a backtest.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PaperConfig {
    pub strategy: String,
    pub params: Params,
    /// The instrument with the trading rules it had when the session was made. Fixed for the
    /// session's life, so a resume replays against the same rules.
    pub instrument: Instrument,
    pub timeframe: Timeframe,
    pub starting_cash: f64,
    pub slippage_bps: f64,
    /// Replaces the instrument's fee schedule when set.
    pub fees: Option<FeeSchedule>,
    /// The largest perpetual position allowed, as its notional over equity, as in a backtest:
    /// 1.0 means 1x, the default, which a config saved without it also gets. Spot ignores it.
    #[serde(default = "default_max_leverage")]
    pub max_leverage: f64,
}

fn default_max_leverage() -> f64 {
    1.0
}

/// A funding rate fed with a bar.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Funding {
    /// The time the engine was given, inside the bar's window.
    pub time: Millis,
    pub rate: f64,
    /// The venue's funding time. Earlier than `time` when the rate was not yet published when its
    /// own bar closed and was charged on this later bar.
    pub due: Millis,
}

/// One line of `events.jsonl`: a bar and the funding fed with it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FedBar {
    pub bar: Candle,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub funding: Vec<Funding>,
    /// Fed during warm-up with [`Session::warm_up`]: trading is disabled and no funding is charged.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub warm_up: bool,
}

/// Where a paper session is.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "status", content = "message", rename_all = "snake_case")]
pub enum Status {
    /// Feeding recent history to the strategy before trading.
    WarmingUp,
    /// Connected to the stream and trading on each closed bar.
    Live,
    /// Not connected to the stream, before the first connection or after a drop, or waiting on a
    /// REST backfill. Bars resume, missed ones first, once it is back.
    Reconnecting,
    Stopped,
    /// Stopped by an error.
    Error(String),
}

/// One strategy paper trading one instrument, and its files.
pub struct PaperSession {
    id: String,
    data_dir: PathBuf,
    config: PaperConfig,
    session: Session,
    log: File,
    log_path: PathBuf,
    status: Status,
    funding: Vec<Funding>,
    /// Open time of the first bar fed live, after warm-up.
    live_from: Option<Millis>,
    dropped: Vec<Millis>,
    warnings: Vec<String>,
}

/// The folder of a paper session.
pub fn paper_dir(data_dir: &Path, id: &str) -> PathBuf {
    data_dir.join("paper").join(id)
}

impl PaperSession {
    /// Makes a new session in `<data_dir>/paper/<id>/` and writes its config. The strategy and
    /// money settings are checked before anything is written.
    pub fn create(data_dir: &Path, id: &str, config: PaperConfig) -> Result<Self, PaperError> {
        check_id(id)?;
        let session = build_session(&config)?;
        let dir = paper_dir(data_dir, id);
        let config_path = dir.join(CONFIG_FILE);
        if config_path.exists() {
            return Err(PaperError::Exists(dir));
        }
        fs::create_dir_all(&dir).map_err(PaperError::io(&dir))?;
        let json = serde_json::to_vec_pretty(&config).map_err(|e| PaperError::BadFile {
            path: config_path.clone(),
            reason: e.to_string(),
        })?;
        let temp = dir.join(format!("{CONFIG_FILE}.tmp"));
        // Synced before the rename, so a crash cannot leave an empty or partial config in place.
        File::create(&temp)
            .and_then(|mut file| file.write_all(&json).and_then(|()| file.sync_all()))
            .map_err(PaperError::io(&temp))?;
        fs::rename(&temp, &config_path).map_err(PaperError::io(&config_path))?;
        let log_path = dir.join(EVENTS_FILE);
        let log = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&log_path)
            .map_err(PaperError::io(&log_path))?;
        Ok(PaperSession {
            id: id.to_string(),
            data_dir: data_dir.to_path_buf(),
            config,
            session,
            log,
            log_path,
            status: Status::Stopped,
            funding: Vec::new(),
            live_from: None,
            dropped: Vec::new(),
            warnings: Vec::new(),
        })
    }

    /// Opens a saved session: builds a fresh engine session from its config and replays its log,
    /// which gives exactly the state it had when it stopped.
    ///
    /// A last line without its newline was torn by a crash mid-write: it is ignored, cut from the
    /// file so later lines append cleanly, and noted in [`PaperSession::warnings`]. Its bar is
    /// fetched again when the session goes live. Any other line that does not read is an error.
    pub fn open(data_dir: &Path, id: &str) -> Result<Self, PaperError> {
        check_id(id)?;
        let dir = paper_dir(data_dir, id);
        let config_path = dir.join(CONFIG_FILE);
        let text = fs::read_to_string(&config_path).map_err(PaperError::io(&config_path))?;
        let config: PaperConfig = serde_json::from_str(&text).map_err(|e| PaperError::BadFile {
            path: config_path.clone(),
            reason: e.to_string(),
        })?;
        let log_path = dir.join(EVENTS_FILE);
        let bytes = fs::read(&log_path).map_err(PaperError::io(&log_path))?;
        let complete = bytes.iter().rposition(|&b| b == b'\n').map_or(0, |i| i + 1);
        let mut warnings = Vec::new();
        if complete < bytes.len() {
            warnings.push(format!(
                "{}: ignored a torn last line of {} bytes",
                log_path.display(),
                bytes.len() - complete
            ));
            let file = OpenOptions::new()
                .write(true)
                .open(&log_path)
                .map_err(PaperError::io(&log_path))?;
            file.set_len(complete as u64)
                .map_err(PaperError::io(&log_path))?;
        }
        let log = OpenOptions::new()
            .append(true)
            .open(&log_path)
            .map_err(PaperError::io(&log_path))?;
        let mut paper = PaperSession {
            id: id.to_string(),
            data_dir: data_dir.to_path_buf(),
            session: build_session(&config)?,
            config,
            log,
            log_path: log_path.clone(),
            status: Status::Stopped,
            funding: Vec::new(),
            live_from: None,
            dropped: Vec::new(),
            warnings,
        };
        let text = String::from_utf8_lossy(&bytes[..complete]);
        for (index, line) in text.lines().enumerate() {
            let bad = |reason: String| PaperError::BadFile {
                path: log_path.clone(),
                reason: format!("line {}: {reason}", index + 1),
            };
            let fed: FedBar = serde_json::from_str(line).map_err(|e| bad(e.to_string()))?;
            paper.apply(&fed).map_err(|e| bad(e.to_string()))?;
        }
        Ok(paper)
    }

    /// Feeds one bar with its funding to the engine session, then appends it to the log. A
    /// warm-up bar goes through [`Session::warm_up`], with trading disabled. A live bar's line is
    /// synced to disk before this returns; warm-up lines are synced with the first live one. On
    /// an engine error nothing changes; on a log error the bar has been applied but may not be
    /// saved, so stop the session.
    pub fn feed(
        &mut self,
        bar: Candle,
        funding: Vec<Funding>,
        warm_up: bool,
    ) -> Result<(), PaperError> {
        let fed = FedBar {
            bar,
            funding,
            warm_up,
        };
        self.apply(&fed)?;
        let mut line = serde_json::to_string(&fed).map_err(|e| PaperError::BadFile {
            path: self.log_path.clone(),
            reason: e.to_string(),
        })?;
        line.push('\n');
        // A `File` has no buffer: the write reaches the operating system at once.
        self.log
            .write_all(line.as_bytes())
            .and_then(|()| {
                if warm_up {
                    Ok(())
                } else {
                    self.log.sync_data()
                }
            })
            .map_err(PaperError::io(&self.log_path))
    }

    fn apply(&mut self, fed: &FedBar) -> Result<(), PaperError> {
        let funding: Vec<(Millis, f64)> = fed.funding.iter().map(|f| (f.time, f.rate)).collect();
        if fed.warm_up {
            self.session.warm_up(fed.bar, &funding)?;
        } else {
            self.session.on_bar(fed.bar, &funding)?;
            self.live_from.get_or_insert(fed.bar.open_time);
            // The engine charges no funding on a warm-up bar.
            self.funding.extend(&fed.funding);
        }
        Ok(())
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn data_dir(&self) -> &Path {
        &self.data_dir
    }

    pub fn config(&self) -> &PaperConfig {
        &self.config
    }

    pub fn status(&self) -> &Status {
        &self.status
    }

    pub(crate) fn set_status(&mut self, status: Status) {
        self.status = status;
    }

    /// The engine session, for anything the accessors below do not cover.
    pub fn session(&self) -> &Session {
        &self.session
    }

    pub fn portfolio(&self) -> &Portfolio {
        self.session.portfolio()
    }

    pub fn open_orders(&self) -> &[Order] {
        self.session.open_orders()
    }

    /// Every order submitted, rejected ones included.
    pub fn orders(&self) -> &[Order] {
        self.session.orders()
    }

    pub fn fills(&self) -> &[Fill] {
        self.session.fills()
    }

    /// Equity at each live bar's close. Warm-up bars record none, so the series starts at
    /// [`PaperSession::live_from`] and is shorter than the bars by the warm-up bars.
    pub fn equity(&self) -> &[EquityPoint] {
        self.session.equity()
    }

    pub fn last_bar(&self) -> Option<&Candle> {
        self.session.bars().last()
    }

    /// Every funding rate charged, in order. Funding fed with a warm-up bar is not charged.
    pub fn funding(&self) -> &[Funding] {
        &self.funding
    }

    /// Open time of the first bar fed after warm-up.
    pub fn live_from(&self) -> Option<Millis> {
        self.live_from
    }

    /// Open time of the next bar the session expects: the end of the last bar fed.
    pub fn next_bar_time(&self) -> Option<Millis> {
        self.last_bar()
            .map(|bar| bar.open_time + self.config.timeframe.duration_ms())
    }

    /// Buckets the live feed left out because too many of their minutes were missing.
    pub fn dropped(&self) -> &[Millis] {
        &self.dropped
    }

    pub(crate) fn push_dropped(&mut self, open_time: Millis) {
        self.dropped.push(open_time);
    }

    /// Things that went wrong without stopping the session: a torn log line, a frame that did not
    /// read, a funding rate that could not be fetched.
    pub fn warnings(&self) -> &[String] {
        &self.warnings
    }

    pub(crate) fn warn(&mut self, warning: String) {
        // shortcut: only the latest warnings are kept, so a noisy stream cannot grow memory
        // without bound; persist them if the history matters.
        const KEEP: usize = 100;
        if self.warnings.len() == KEEP {
            self.warnings.remove(0);
        }
        self.warnings.push(warning);
    }
}

fn build_session(config: &PaperConfig) -> Result<Session, PaperError> {
    let strategy = build_strategy(&config.strategy, &config.params)?;
    let money = SessionConfig {
        starting_cash: config.starting_cash,
        slippage_bps: config.slippage_bps,
        fees: config.fees,
        max_leverage: config.max_leverage,
    };
    Ok(Session::new(
        strategy,
        config.instrument.clone(),
        config.timeframe,
        money,
    )?)
}

fn check_id(id: &str) -> Result<(), PaperError> {
    let ok = (1..=64).contains(&id.len())
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    if ok {
        Ok(())
    } else {
        Err(PaperError::InvalidId(id.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use td_core::{
        InstrumentId, InstrumentKind, OrderRequest, OrderType, Side, TimeInForce, Venue,
    };
    use td_engine::{Context, OrderStatus, Strategy};

    use super::*;

    const MIN: Millis = 60_000;

    /// Tries to buy on every bar, warm-up bars included.
    struct AlwaysBuy;

    impl Strategy for AlwaysBuy {
        fn on_bar(&mut self, ctx: &mut Context, _bar: &Candle) {
            let _ = ctx.submit(OrderRequest {
                instrument: ctx.instrument().id.clone(),
                side: Side::Buy,
                qty: 1.0,
                order_type: OrderType::Market,
                tif: TimeInForce::Gtc,
                reduce_only: false,
                client_tag: None,
            });
        }
    }

    fn bar(i: i64) -> Candle {
        Candle {
            open_time: i * MIN,
            open: 100.0,
            high: 101.0,
            low: 99.0,
            close: 100.0,
            volume: 1.0,
            quote_volume: 100.0,
        }
    }

    #[test]
    fn a_strategy_cannot_trade_on_warm_up_bars() {
        let config = PaperConfig {
            strategy: "sma_cross".into(),
            params: Params::new(),
            instrument: Instrument {
                id: InstrumentId {
                    venue: Venue::Binance,
                    symbol: "TESTUSDT".into(),
                    kind: InstrumentKind::Perpetual,
                },
                base: "TEST".into(),
                quote: "USDT".into(),
                tick_size: 0.01,
                lot_size: 0.001,
                min_qty: 0.001,
                min_notional: 1.0,
                fees: FeeSchedule {
                    maker: 0.001,
                    taker: 0.002,
                },
            },
            timeframe: Timeframe::M1,
            starting_cash: 10_000.0,
            slippage_bps: 0.0,
            fees: None,
            max_leverage: 1.0,
        };
        let dir = tempfile::tempdir().expect("temp dir");
        let mut paper = PaperSession::create(dir.path(), "warm", config.clone()).expect("creates");
        // The registry has no strategy that trades while warming up, so swap one in.
        paper.session = Session::new(
            Box::new(AlwaysBuy),
            config.instrument.clone(),
            config.timeframe,
            SessionConfig {
                starting_cash: config.starting_cash,
                slippage_bps: config.slippage_bps,
                fees: config.fees,
                max_leverage: config.max_leverage,
            },
        )
        .expect("session");

        let rate = Funding {
            time: 0,
            rate: 0.01,
            due: 0,
        };
        paper.feed(bar(0), vec![rate], true).expect("warm-up bar");
        for i in 1..5 {
            paper.feed(bar(i), Vec::new(), true).expect("warm-up bar");
        }
        assert_eq!(paper.orders().len(), 5, "the strategy tried on every bar");
        for order in paper.orders() {
            assert_eq!(
                order.status,
                OrderStatus::Rejected("warm-up: trading is disabled".into())
            );
        }
        assert!(paper.fills().is_empty() && paper.open_orders().is_empty());
        assert!(paper.equity().is_empty() && paper.funding().is_empty());
        assert_eq!(
            paper.portfolio(),
            &Portfolio::new(InstrumentKind::Perpetual, 10_000.0)
        );
        assert_eq!(paper.live_from(), None);

        // Live, its orders go through and fill on the next bar.
        paper.feed(bar(5), Vec::new(), false).expect("live bar");
        paper.feed(bar(6), Vec::new(), false).expect("live bar");
        assert_eq!(paper.fills().len(), 1);
        assert_eq!(paper.equity().len(), 2);
        assert_eq!(paper.live_from(), Some(5 * MIN));

        // A warm-up bar after a live one is refused and never reaches the log.
        assert!(paper.feed(bar(7), Vec::new(), true).is_err());
        let log = fs::read_to_string(&paper.log_path).expect("reads");
        assert_eq!(log.lines().count(), 7);
        assert_eq!(
            log.lines()
                .filter(|l| l.contains(r#""warm_up":true"#))
                .count(),
            5
        );
    }
}
