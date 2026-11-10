//! Where market data comes from. The server reaches the venues only through [`MarketSource`] and
//! [`LiveSource`], so tests can hand it fakes and never touch the network.

use std::sync::Arc;

use td_core::{InstrumentId, InstrumentKind, Millis, Venue};
use td_data::live::{Rest, VenueRest};
use td_data::{DataError, DownloadReport, Http, InstrumentList, Store};

/// Venue access. Every method blocks, and is called from a blocking thread, never from the async
/// runtime.
pub trait MarketSource: Send + Sync + 'static {
    /// The instruments a venue lists for one kind.
    fn instruments(&self, venue: Venue, kind: InstrumentKind) -> Result<InstrumentList, DataError>;

    /// Downloads the 1-minute candles of `id` with open times in `[from, to)` into `store`. The
    /// server calls it once per calendar month of a download, so a cancel takes effect between
    /// months.
    fn candles(
        &self,
        store: &Store,
        id: &InstrumentId,
        from: Millis,
        to: Millis,
    ) -> Result<DownloadReport, DataError>;

    /// Downloads the funding rates of a perpetual in `[from, to)` into `store`; returns how many.
    fn funding(
        &self,
        store: &Store,
        id: &InstrumentId,
        from: Millis,
        to: Millis,
    ) -> Result<usize, DataError>;
}

/// The real venues, through td-data.
///
/// Each call builds its own blocking HTTP client on the calling (blocking) thread and drops it
/// there: a reqwest blocking client must never be dropped inside the async runtime.
#[derive(Debug, Default, Clone, Copy)]
pub struct VenueSource;

impl MarketSource for VenueSource {
    fn instruments(&self, venue: Venue, kind: InstrumentKind) -> Result<InstrumentList, DataError> {
        td_data::fetch_instruments(&Http::new()?, venue, kind)
    }

    fn candles(
        &self,
        store: &Store,
        id: &InstrumentId,
        from: Millis,
        to: Millis,
    ) -> Result<DownloadReport, DataError> {
        td_data::download_candles(&Http::new()?, store, id, from, to, &mut |_| {})
    }

    fn funding(
        &self,
        store: &Store,
        id: &InstrumentId,
        from: Millis,
        to: Millis,
    ) -> Result<usize, DataError> {
        td_data::download_funding(&Http::new()?, store, id, from, to)
    }
}

/// Live market data, for paper sessions and the `candles:` WebSocket topic: the venue's REST calls
/// a live feed needs, and the WebSocket stream of an instrument's 1-minute bars.
pub trait LiveSource: Send + Sync + 'static {
    fn rest(&self) -> Arc<dyn Rest>;

    /// The stream for `id`, serving its venue's message format.
    fn stream_url(&self, id: &InstrumentId) -> String;
}

/// The venues' public REST APIs and streams.
#[derive(Debug, Default, Clone, Copy)]
pub struct VenueLive;

impl LiveSource for VenueLive {
    fn rest(&self) -> Arc<dyn Rest> {
        Arc::new(VenueRest)
    }

    fn stream_url(&self, id: &InstrumentId) -> String {
        td_data::live::stream_url(id)
    }
}
