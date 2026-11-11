//! Where market data comes from. Everything that differs between data sources sits behind
//! [`DataSource`], and [`source`] maps a venue to its source. A new source is one module that
//! implements the trait and one line in [`source`]; a new ticker on a source needs no code.
//!
//! The download loop ([`crate::download_candles`]), the instrument list
//! ([`crate::fetch_instruments`]) and live feeds ([`crate::live`]) are written once against the
//! trait.

use td_core::{Candle, FeeSchedule, InstrumentId, InstrumentKind, Millis, Timeframe, Venue};

use crate::live::{Greeting, KlineUpdate};
use crate::{DataError, Http, InstrumentList, binance, bybit, oanda};

/// The source that serves `venue`.
pub fn source(venue: Venue) -> &'static dyn DataSource {
    match venue {
        Venue::Binance => &binance::Binance,
        Venue::Bybit => &bybit::Bybit,
        Venue::Oanda => &oanda::Oanda,
    }
}

/// What one call for minute history fetched.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct MinuteHistory {
    /// The 1-minute candles fetched, oldest first. They may reach outside the range asked for;
    /// the caller keeps the ones inside it. For a kind whose rules require quotes these are the
    /// bids.
    pub candles: Vec<Candle>,
    /// For a kind whose rules require quotes, the ask candle of each minute in `candles`, in the
    /// same order; empty for any other kind. Their volumes are not stored.
    pub asks: Vec<Candle>,
    /// Files the source publishes history in that it does not have (before the symbol listed, or
    /// not yet published).
    pub missing_files: Vec<String>,
}

/// One market data source: its instruments, their history, and optionally funding and a live
/// stream. Every method that takes an [`Http`] blocks.
pub trait DataSource: Send + Sync {
    /// The venue this source serves.
    fn venue(&self) -> Venue;

    /// The kinds of instrument the source lists.
    fn kinds(&self) -> &'static [InstrumentKind];

    /// The instruments of `kind` open for trading, with default fees, each validated with
    /// [`crate::validate_instrument`]. `kind` is one of [`DataSource::kinds`]. A source with no
    /// listing endpoint returns its fixed list.
    fn instruments(&self, http: &Http, kind: InstrumentKind) -> Result<InstrumentList, DataError>;

    /// The fee schedule an instrument of `kind` gets unless told otherwise, or
    /// [`DataError::KindNotListed`] for a kind not in [`DataSource::kinds`].
    fn default_fees(&self, kind: InstrumentKind) -> Result<FeeSchedule, DataError>;

    /// The 1-minute candles of `id` with open times in `[from, to)`, which lies inside one
    /// calendar month and ends no later than `now`, with their asks when `id`'s kind requires
    /// quotes. A source that publishes history as files names the ones it lacks in
    /// [`MinuteHistory::missing_files`] rather than failing.
    fn minute_history(
        &self,
        http: &Http,
        id: &InstrumentId,
        from: Millis,
        to: Millis,
        now: Millis,
    ) -> Result<MinuteHistory, DataError>;

    /// A perpetual's funding rates `(funding time, rate)` in `[from, to)`, oldest first, with the
    /// times on the minute. A source without funding keeps the default, which says so.
    fn funding(
        &self,
        _http: &Http,
        _id: &InstrumentId,
        _from: Millis,
        _to: Millis,
    ) -> Result<Vec<(Millis, f64)>, DataError> {
        Err(DataError::NoFunding(self.venue()))
    }

    /// The source's live stream. A source without one keeps the default, which says so.
    fn live(&self) -> Result<&dyn LiveStream, DataError> {
        Err(DataError::NoLiveStream(self.venue()))
    }
}

/// A source's live data: a WebSocket stream of 1-minute bars, and the recent bars over REST that
/// a feed backfills from and a paper session warms up on.
pub trait LiveStream: Send + Sync {
    /// The stream of `id`'s 1-minute bars.
    fn url(&self, id: &InstrumentId) -> String;

    /// What a connection sends once it opens. The default sends nothing.
    fn greeting(&self, _id: &InstrumentId) -> Greeting {
        Greeting::default()
    }

    /// Reads one text frame. Frames that carry no bar (acknowledgements, pongs) give an empty
    /// list; a refused subscription or a bar that does not read is an error.
    fn parse(&self, text: &str) -> Result<Vec<KlineUpdate>, DataError>;

    /// Closed bars of `timeframe` with open times in `[from, to)`, oldest first. A bar still in
    /// progress is left out.
    fn klines(
        &self,
        http: &Http,
        id: &InstrumentId,
        timeframe: Timeframe,
        from: Millis,
        to: Millis,
    ) -> Result<Vec<Candle>, DataError>;
}

/// A fake source for tests, shaped like an FX history provider: one file of 1-minute candles per
/// day, a fixed list of spot instruments, no funding and no live stream. It leaves out the minutes
/// its kind is closed, and returns asks when told to.
#[cfg(test)]
pub(crate) mod fake {
    use std::sync::Mutex;

    use super::*;
    use crate::time::{DAY, MINUTE, format_date};

    pub(crate) struct DailyFiles {
        /// The day files it does not have, by the midnight they start at.
        pub missing_days: Vec<Millis>,
        /// The kind whose open minutes it returns.
        pub kind: InstrumentKind,
        /// Whether it returns an ask for each minute.
        pub asks: bool,
        /// Each `(from, to)` asked of [`DataSource::minute_history`].
        pub asked: Mutex<Vec<(Millis, Millis)>>,
    }

    impl DailyFiles {
        pub(crate) fn new(missing_days: Vec<Millis>) -> Self {
            DailyFiles {
                missing_days,
                kind: InstrumentKind::Spot,
                asks: false,
                asked: Mutex::new(Vec::new()),
            }
        }

        /// A source of FX minutes, closed at weekends, with or without their asks.
        pub(crate) fn fx(asks: bool) -> Self {
            DailyFiles {
                kind: InstrumentKind::Fx,
                asks,
                ..DailyFiles::new(Vec::new())
            }
        }
    }

    /// The ask of [`candle`]'s minute.
    pub(crate) fn ask(open_time: Millis) -> Candle {
        Candle {
            open: 1.0002,
            high: 1.0002,
            low: 1.0002,
            close: 1.0002,
            volume: 0.0,
            quote_volume: 0.0,
            ..candle(open_time)
        }
    }

    pub(crate) fn candle(open_time: Millis) -> Candle {
        Candle {
            open_time,
            open: 1.0,
            high: 1.0,
            low: 1.0,
            close: 1.0,
            volume: 1.0,
            quote_volume: 1.0,
        }
    }

    impl DataSource for DailyFiles {
        fn venue(&self) -> Venue {
            Venue::Binance
        }

        fn kinds(&self) -> &'static [InstrumentKind] {
            &[InstrumentKind::Spot]
        }

        fn instruments(&self, _: &Http, _: InstrumentKind) -> Result<InstrumentList, DataError> {
            Ok(InstrumentList::default())
        }

        fn default_fees(&self, _: InstrumentKind) -> Result<FeeSchedule, DataError> {
            Ok(FeeSchedule {
                maker: 0.0,
                taker: 0.0,
                per_unit: 0.0,
            })
        }

        /// Every day file overlapping `[from, to)` whole, so the candles reach outside the range
        /// when it starts or ends inside a day.
        fn minute_history(
            &self,
            _: &Http,
            _: &InstrumentId,
            from: Millis,
            to: Millis,
            _: Millis,
        ) -> Result<MinuteHistory, DataError> {
            self.asked.lock().expect("lock").push((from, to));
            let mut history = MinuteHistory::default();
            let mut day = from.div_euclid(DAY) * DAY;
            while day < to {
                if self.missing_days.contains(&day) {
                    history
                        .missing_files
                        .push(format!("{}.csv.gz", format_date(day)));
                } else {
                    let minutes = (0..DAY / MINUTE)
                        .map(|m| day + m * MINUTE)
                        .filter(|&t| self.kind.is_open(t));
                    for t in minutes {
                        history.candles.push(candle(t));
                        if self.asks {
                            history.asks.push(ask(t));
                        }
                    }
                }
                day += DAY;
            }
            Ok(history)
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::binance::{BINANCE_SPOT_FEES, BINANCE_USDM_FEES};
    use crate::bybit::{BYBIT_LINEAR_FEES, BYBIT_SPOT_FEES};

    fn btc(venue: Venue, kind: InstrumentKind) -> InstrumentId {
        InstrumentId {
            venue,
            symbol: "BTCUSDT".into(),
            kind,
        }
    }

    /// Neither crypto venue lists FX: each place a request for an FX instrument reaches says so
    /// with `KindNotListed` before it would touch the network.
    #[test]
    fn crypto_venues_refuse_fx_with_kind_not_listed() {
        let http = Http::new().expect("an HTTP client");
        let not_listed = |result: Result<(), DataError>, venue: Venue, what: &str| match result {
            Err(e @ DataError::KindNotListed { .. }) => {
                assert_eq!(
                    e.to_string(),
                    format!("{venue} does not list fx instruments")
                );
            }
            other => panic!("{venue} {what}: {other:?}"),
        };
        for venue in [Venue::Binance, Venue::Bybit] {
            let source = source(venue);
            let id = InstrumentId {
                venue,
                symbol: "USDJPY".into(),
                kind: InstrumentKind::Fx,
            };
            assert!(!source.kinds().contains(&InstrumentKind::Fx));
            let fees = source.default_fees(InstrumentKind::Fx).map(|_| ());
            not_listed(fees, venue, "fees");
            let listed = crate::fetch_instruments(&http, venue, InstrumentKind::Fx);
            not_listed(listed.map(|_| ()), venue, "instruments");
            let history = source.minute_history(&http, &id, 0, 60_000, 120_000);
            not_listed(history.map(|_| ()), venue, "history");
            let live = source.live().expect("a live stream");
            let klines = live.klines(&http, &id, td_core::Timeframe::M1, 0, 60_000);
            not_listed(klines.map(|_| ()), venue, "klines");
            assert_eq!(live.url(&id), "", "{venue} has no stream for fx");
        }
    }

    /// The streams, greetings and fees each venue had before its code moved behind the trait.
    #[test]
    fn each_venue_is_served_by_its_own_source_with_its_streams_and_fees() {
        use InstrumentKind::{Perpetual, Spot};
        let cases = [
            (
                Venue::Binance,
                Spot,
                "wss://stream.binance.com:9443/ws/btcusdt@kline_1m",
                BINANCE_SPOT_FEES,
            ),
            (
                Venue::Binance,
                Perpetual,
                "wss://fstream.binance.com/market/ws/btcusdt@kline_1m",
                BINANCE_USDM_FEES,
            ),
            (
                Venue::Bybit,
                Spot,
                "wss://stream.bybit.com/v5/public/spot",
                BYBIT_SPOT_FEES,
            ),
            (
                Venue::Bybit,
                Perpetual,
                "wss://stream.bybit.com/v5/public/linear",
                BYBIT_LINEAR_FEES,
            ),
        ];
        for (venue, kind, url, fees) in cases {
            let source = source(venue);
            assert_eq!(source.venue(), venue);
            assert_eq!(source.kinds(), [Spot, Perpetual]);
            assert_eq!(source.default_fees(kind).ok(), Some(fees), "{venue} {kind}");
            let live = source.live().expect("a live stream");
            let id = btc(venue, kind);
            assert_eq!(live.url(&id), url);
            let greeting = live.greeting(&id);
            match venue {
                Venue::Binance => {
                    assert_eq!(greeting.subscribe, None);
                    assert_eq!(greeting.heartbeat, None);
                }
                Venue::Bybit => {
                    let subscribe = r#"{"op":"subscribe","args":["kline.1.BTCUSDT"]}"#;
                    assert_eq!(greeting.subscribe.as_deref(), Some(subscribe));
                    let ping = (Duration::from_secs(20), r#"{"op":"ping"}"#.to_string());
                    assert_eq!(greeting.heartbeat, Some(ping));
                }
                Venue::Oanda => unreachable!("not among the cases"),
            }
        }
    }
}
