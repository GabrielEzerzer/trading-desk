//! Where market data comes from. Everything that differs between data sources sits behind
//! [`DataSource`], and [`source`] maps a venue to its source. A new source is one module that
//! implements the trait and one line in [`source`]; a new ticker on a source needs no code.
//!
//! The download loop ([`crate::download_candles`]), the instrument list
//! ([`crate::fetch_instruments`]) and live feeds ([`crate::live`]) are written once against the
//! trait.

use td_core::{Candle, FeeSchedule, InstrumentId, InstrumentKind, Millis, Timeframe, Venue};

use crate::live::{Greeting, KlineUpdate};
use crate::{DataError, Http, InstrumentList, binance, bybit};

/// The source that serves `venue`.
pub fn source(venue: Venue) -> &'static dyn DataSource {
    match venue {
        Venue::Binance => &binance::Binance,
        Venue::Bybit => &bybit::Bybit,
    }
}

/// What one call for minute history fetched.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct MinuteHistory {
    /// The 1-minute candles fetched, oldest first. They may reach outside the range asked for;
    /// the caller keeps the ones inside it.
    pub candles: Vec<Candle>,
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

    /// The fee schedule an instrument of `kind` gets unless told otherwise. `kind` is one of
    /// [`DataSource::kinds`].
    fn default_fees(&self, kind: InstrumentKind) -> FeeSchedule;

    /// The 1-minute candles of `id` with open times in `[from, to)`, which lies inside one
    /// calendar month and ends no later than `now`. A source that publishes history as files
    /// names the ones it lacks in [`MinuteHistory::missing_files`] rather than failing.
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
/// day, a fixed list of spot instruments, no funding and no live stream.
#[cfg(test)]
pub(crate) mod fake {
    use std::sync::Mutex;

    use super::*;
    use crate::time::{DAY, MINUTE, format_date};

    pub(crate) struct DailyFiles {
        /// The day files it does not have, by the midnight they start at.
        pub missing_days: Vec<Millis>,
        /// Each `(from, to)` asked of [`DataSource::minute_history`].
        pub asked: Mutex<Vec<(Millis, Millis)>>,
    }

    impl DailyFiles {
        pub(crate) fn new(missing_days: Vec<Millis>) -> Self {
            DailyFiles {
                missing_days,
                asked: Mutex::new(Vec::new()),
            }
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

        fn default_fees(&self, _: InstrumentKind) -> FeeSchedule {
            FeeSchedule {
                maker: 0.0,
                taker: 0.0,
            }
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
                    let minutes = (0..DAY / MINUTE).map(|m| candle(day + m * MINUTE));
                    history.candles.extend(minutes);
                }
                day += DAY;
            }
            Ok(history)
        }
    }
}
