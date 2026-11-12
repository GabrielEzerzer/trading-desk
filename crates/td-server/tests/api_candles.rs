//! Candle history over the API: real HTTP against a server started in-process on 127.0.0.1:0,
//! a temporary fixture store and a fake venue REST source. No network.

use std::sync::{Arc, Mutex};

use reqwest::StatusCode;
use serde_json::Value;
use td_core::{Candle, InstrumentId, InstrumentKind, Millis, Timeframe, Venue};
use td_data::live::Rest;
use td_data::{DataError, DownloadReport, InstrumentList, Store, resample};
use td_server::api::auth::Token;
use td_server::api::candles::{BarSpan, CandlesResponse, MAX_CANDLES};
use td_server::api::source::{LiveSource, MarketSource};
use td_server::api::{ApiConfig, Server};
use tempfile::TempDir;

const MIN: Millis = 60_000;
const HOUR: Millis = 60 * MIN;
/// 2024-01-01 00:00 UTC.
const T0: Millis = 1_704_067_200_000;
const INSTRUMENT: &str = "binance:TESTUSDT:spot";

fn instrument_id() -> InstrumentId {
    InstrumentId {
        venue: Venue::Binance,
        symbol: "TESTUSDT".into(),
        kind: InstrumentKind::Spot,
    }
}

/// The minute `i` minutes after `T0`, on a price wave. The store's minutes trade a volume of 2 and
/// the venue's 3, so a test can tell where each bar came from.
fn minute(i: i64, volume: f64) -> Candle {
    let price = |i: i64| 100.0 + 5.0 * (i as f64 * std::f64::consts::TAU / 30.0).sin();
    let (open, close) = (price(i - 1), price(i));
    Candle {
        open_time: T0 + i * MIN,
        open,
        high: open.max(close) + 0.5,
        low: open.min(close) - 0.5,
        close,
        volume,
        quote_volume: volume * close,
    }
}

fn stored(range: impl Iterator<Item = i64>) -> Vec<Candle> {
    range.map(|i| minute(i, 2.0)).collect()
}

fn at_venue(range: impl Iterator<Item = i64>) -> Vec<Candle> {
    range.map(|i| minute(i, 3.0)).collect()
}

/// Downloads and instrument lists are not used here.
struct NoSource;

impl MarketSource for NoSource {
    fn instruments(&self, _: Venue, _: InstrumentKind) -> Result<InstrumentList, DataError> {
        Err(DataError::NoDataDir)
    }

    fn candles(
        &self,
        _: &Store,
        _: &InstrumentId,
        _: Millis,
        _: Millis,
    ) -> Result<DownloadReport, DataError> {
        Err(DataError::NoDataDir)
    }

    fn funding(
        &self,
        _: &Store,
        _: &InstrumentId,
        _: Millis,
        _: Millis,
    ) -> Result<usize, DataError> {
        Err(DataError::NoDataDir)
    }
}

/// Makes the error a failing venue call returns.
type Failure = fn() -> DataError;

/// The venue's REST klines: its minutes, or bars resampled from them, from `from` on. It serves
/// the bar still open at `now`, as a venue serves the forming bar, and ignores `to`, so the server
/// must keep to its own range. A venue whose source has no live stream (oanda) is refused as the
/// real one refuses it. Records every call.
struct FakeRest {
    minutes: Vec<Candle>,
    now: Millis,
    /// Every call fails with this error instead.
    fail: Option<Failure>,
    calls: Mutex<Vec<(Timeframe, Millis, Millis)>>,
}

impl FakeRest {
    fn new(minutes: Vec<Candle>, now: Millis) -> FakeRest {
        FakeRest {
            minutes,
            now,
            fail: None,
            calls: Mutex::new(Vec::new()),
        }
    }

    fn calls(&self) -> Vec<(Timeframe, Millis, Millis)> {
        self.calls.lock().unwrap().clone()
    }
}

impl Rest for FakeRest {
    fn klines(
        &self,
        id: &InstrumentId,
        timeframe: Timeframe,
        from: Millis,
        to: Millis,
    ) -> Result<Vec<Candle>, DataError> {
        self.calls.lock().unwrap().push((timeframe, from, to));
        if let Some(fail) = self.fail {
            return Err(fail());
        }
        td_data::source(id.venue).live()?;
        let bars = if timeframe == Timeframe::M1 {
            self.minutes.clone()
        } else {
            resample(InstrumentKind::Spot, &self.minutes, timeframe, true).candles
        };
        Ok(bars.into_iter().filter(|c| c.open_time >= from).collect())
    }

    fn funding(
        &self,
        _: &InstrumentId,
        _: Millis,
        _: Millis,
    ) -> Result<Vec<(Millis, f64)>, DataError> {
        Ok(Vec::new())
    }

    fn now(&self) -> Millis {
        self.now
    }
}

struct Live(Arc<FakeRest>);

impl LiveSource for Live {
    fn rest(&self) -> Arc<dyn Rest> {
        self.0.clone()
    }

    /// Never connected to: candle history uses REST only.
    fn stream_url(&self, _: &InstrumentId) -> String {
        "ws://127.0.0.1:9/".into()
    }
}

struct Api {
    server: Server,
    rest: Arc<FakeRest>,
    store: Store,
    _dir: TempDir,
    client: reqwest::Client,
}

impl Api {
    /// A server on a store holding `store_minutes`, with `rest` as the venue.
    async fn start(store_minutes: &[Candle], rest: FakeRest) -> Api {
        let dir = tempfile::tempdir().expect("temp dir");
        let store = Store::new(dir.path());
        if !store_minutes.is_empty() {
            store
                .write_candles(&instrument_id(), store_minutes)
                .expect("writes the fixture");
        }
        let rest = Arc::new(rest);
        let server = Server::start(ApiConfig {
            store: store.clone(),
            source: Arc::new(NoSource),
            live: Arc::new(Live(rest.clone())),
            token: Token::generate().expect("rng"),
            port: 0,
        })
        .await
        .expect("starts");
        Api {
            server,
            rest,
            store,
            _dir: dir,
            client: reqwest::Client::new(),
        }
    }

    fn url(&self, path: &str) -> String {
        format!("http://127.0.0.1:{}{path}", self.server.port())
    }

    async fn send(&self, request: reqwest::RequestBuilder) -> (StatusCode, Value) {
        let response = request.send().await.expect("answers");
        let status = response.status();
        let text = response.text().await.expect("a body");
        (status, serde_json::from_str(&text).unwrap_or(Value::Null))
    }

    async fn get(&self, path: &str) -> (StatusCode, Value) {
        let request = self.client.get(self.url(path)).header(
            "authorization",
            format!("Bearer {}", self.server.token().as_str()),
        );
        self.send(request).await
    }

    /// `GET /candles` for the test instrument with `params` added; must be a 200.
    async fn candles(&self, params: &str) -> CandlesResponse {
        let (status, body) = self
            .get(&format!("/candles?instrument={INSTRUMENT}{params}"))
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        serde_json::from_value(body).expect("a candles response")
    }

    async fn shutdown(self) {
        self.server.shutdown().await.expect("shuts down");
    }
}

fn span(from: Millis, to: Millis, timeframe: Timeframe) -> BarSpan {
    BarSpan {
        from,
        to,
        bars: (to - from) / timeframe.duration_ms(),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn bars_come_from_the_store_exactly_and_oldest_first() {
    let api = Api::start(&stored(0..120), FakeRest::new(Vec::new(), T0 + 10 * HOUR)).await;
    let got = api
        .candles(&format!(
            "&timeframe=1m&from={}&to={}",
            T0 + 10 * MIN,
            T0 + 70 * MIN
        ))
        .await;
    assert_eq!(got.candles, stored(10..70));
    assert_eq!((got.from, got.to), (T0 + 10 * MIN, T0 + 70 * MIN));
    assert!(
        got.from_venue.is_empty() && got.missing.is_empty(),
        "{got:?}"
    );
    assert!(!got.truncated);
    assert_eq!(got.instrument, instrument_id());
    assert_eq!(got.timeframe, Timeframe::M1);
    assert!(api.rest.calls().is_empty(), "the store covered it all");
    api.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn higher_timeframes_aggregate_the_stored_minutes_as_the_store_does() {
    let minutes = stored(0..240);
    let api = Api::start(&minutes, FakeRest::new(Vec::new(), T0 + 10 * HOUR)).await;
    for (timeframe, name) in [(Timeframe::M15, "15m"), (Timeframe::H1, "1h")] {
        let got = api
            .candles(&format!("&timeframe={name}&from={T0}&to={}", T0 + 4 * HOUR))
            .await;
        let from_store = api
            .store
            .load_candles(&instrument_id(), timeframe, T0, T0 + 4 * HOUR, false)
            .expect("loads")
            .candles;
        assert_eq!(got.candles, from_store, "{name}");
        assert_eq!(
            got.candles,
            resample(InstrumentKind::Spot, &minutes, timeframe, false).candles
        );
        assert_eq!(got.candles.len() as i64, 4 * HOUR / timeframe.duration_ms());
        // An aggregated bar is not a copy of its first minute.
        assert_eq!(
            got.candles[0].volume,
            2.0 * (timeframe.duration_ms() / MIN) as f64
        );
    }
    assert!(api.rest.calls().is_empty());
    api.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn daily_and_weekly_bars_over_months_match_the_store() {
    // Six weeks from Monday 2024-01-01: the week of 29 January runs into February, and the store
    // is read in pieces of about a month.
    const DAY: Millis = 24 * HOUR;
    let minutes = stored(0..42 * DAY / MIN);
    let api = Api::start(&minutes, FakeRest::new(Vec::new(), T0 + 50 * DAY)).await;
    for (timeframe, name, bars) in [(Timeframe::D1, "1d", 42), (Timeframe::W1, "1w", 6)] {
        let got = api
            .candles(&format!("&timeframe={name}&from={T0}&to={}", T0 + 42 * DAY))
            .await;
        assert_eq!(
            got.candles,
            resample(InstrumentKind::Spot, &minutes, timeframe, false).candles,
            "{name}"
        );
        assert_eq!(got.candles.len(), bars, "{name}");
        assert!(
            got.missing.is_empty() && got.from_venue.is_empty(),
            "{name}"
        );
    }
    assert!(api.rest.calls().is_empty());
    api.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn from_and_to_between_bar_boundaries_round_up_to_the_next() {
    const DAY: Millis = 24 * HOUR;
    let minutes = stored(0..21 * DAY / MIN);
    let api = Api::start(&minutes, FakeRest::new(Vec::new(), T0 + 30 * DAY)).await;
    // 00:10 to 03:05: the hours opening at 01:00, 02:00 and 03:00.
    let got = api
        .candles(&format!(
            "&timeframe=1h&from={}&to={}",
            T0 + 10 * MIN,
            T0 + 3 * HOUR + 5 * MIN
        ))
        .await;
    let hours = resample(InstrumentKind::Spot, &minutes, Timeframe::H1, false).candles;
    assert_eq!(got.candles, hours[1..4]);
    assert_eq!((got.from, got.to), (T0 + HOUR, T0 + 4 * HOUR));
    // From Wednesday 3 January: the weeks opening on Monday 8 and Monday 15 January.
    let got = api
        .candles(&format!(
            "&timeframe=1w&from={}&to={}",
            T0 + 2 * DAY,
            T0 + 21 * DAY
        ))
        .await;
    let weeks = resample(InstrumentKind::Spot, &minutes, Timeframe::W1, false).candles;
    assert_eq!(got.candles, weeks[1..3]);
    assert_eq!((got.from, got.to), (T0 + 7 * DAY, T0 + 21 * DAY));
    assert!(got.missing.is_empty() && got.from_venue.is_empty());
    assert!(api.rest.calls().is_empty());
    api.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_gap_in_the_store_is_filled_from_the_venue_and_not_stored() {
    let store_minutes: Vec<Candle> = stored((0..30).chain(45..120));
    // The venue lacks two of the minutes the store lacks too.
    let venue = at_venue((0..40).chain(42..120));
    let api = Api::start(&store_minutes, FakeRest::new(venue, T0 + 10 * HOUR)).await;
    let got = api
        .candles(&format!("&timeframe=1m&from={T0}&to={}", T0 + 60 * MIN))
        .await;
    let expected: Vec<Candle> = stored(0..30)
        .into_iter()
        .chain(at_venue((30..40).chain(42..45)))
        .chain(stored(45..60))
        .collect();
    assert_eq!(got.candles, expected);
    let tf = Timeframe::M1;
    assert_eq!(
        got.from_venue,
        [
            span(T0 + 30 * MIN, T0 + 40 * MIN, tf),
            span(T0 + 42 * MIN, T0 + 45 * MIN, tf)
        ]
    );
    assert_eq!(got.missing, [span(T0 + 40 * MIN, T0 + 42 * MIN, tf)]);
    // One request over the store's gap, at the asked timeframe.
    assert_eq!(api.rest.calls(), [(tf, T0 + 30 * MIN, T0 + 45 * MIN)]);
    // Nothing fetched was written to the store.
    let month = api
        .store
        .read_month(&instrument_id(), td_data::time::Month::containing(T0))
        .expect("reads");
    assert_eq!(month, store_minutes);
    api.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn an_hour_the_store_holds_only_part_of_comes_from_the_venue() {
    // The store misses minute 01:40, so its 01:00 hour is incomplete and left out.
    let store_minutes = stored((0..100).chain(101..240));
    let venue = at_venue(0..240);
    let api = Api::start(&store_minutes, FakeRest::new(venue.clone(), T0 + 10 * HOUR)).await;
    let got = api
        .candles(&format!("&timeframe=1h&from={T0}&to={}", T0 + 4 * HOUR))
        .await;
    let tf = Timeframe::H1;
    let from_store = resample(InstrumentKind::Spot, &store_minutes, tf, false).candles;
    let from_venue = resample(InstrumentKind::Spot, &venue, tf, false).candles;
    assert_eq!(
        got.candles,
        [from_store[0], from_venue[1], from_store[1], from_store[2]]
    );
    assert_eq!(got.from_venue, [span(T0 + HOUR, T0 + 2 * HOUR, tf)]);
    assert!(got.missing.is_empty());
    assert_eq!(api.rest.calls(), [(tf, T0 + HOUR, T0 + 2 * HOUR)]);
    api.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_watched_instrument_with_nothing_stored_comes_from_the_venue() {
    // Ten closed hours and the eleventh still forming at 10:05:30.
    let now = T0 + 10 * HOUR + 5 * MIN + 30_000;
    let venue = at_venue(0..606);
    let api = Api::start(&[], FakeRest::new(venue.clone(), now)).await;
    let got = api.candles("&timeframe=1h").await;
    let tf = Timeframe::H1;
    assert_eq!(
        got.candles,
        resample(InstrumentKind::Spot, &venue, tf, false).candles
    );
    assert_eq!(got.candles.len(), 10, "the forming hour is left out");
    assert_eq!(got.to, T0 + 10 * HOUR);
    // No `from`: the latest MAX_CANDLES hour slots, most of them before the venue's first bar.
    let from = T0 + 10 * HOUR - MAX_CANDLES as i64 * HOUR;
    assert_eq!(got.from, from);
    assert!(!got.truncated);
    assert_eq!(got.from_venue, [span(T0, T0 + 10 * HOUR, tf)]);
    assert_eq!(got.missing, [span(from, T0, tf)]);
    assert_eq!(api.rest.calls(), [(tf, from, T0 + 10 * HOUR)]);
    api.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn the_limit_is_capped_and_keeps_the_latest_bars() {
    assert_eq!(MAX_CANDLES, 5_000);
    let api = Api::start(
        &stored(0..6_000),
        FakeRest::new(Vec::new(), T0 + 200 * HOUR),
    )
    .await;
    let range = format!("&timeframe=1m&from={T0}&to={}", T0 + 6_000 * MIN);
    for limit in ["", "&limit=99999"] {
        let got = api.candles(&format!("{range}{limit}")).await;
        assert_eq!(got.candles, stored(1_000..6_000), "limit {limit:?}");
        assert_eq!(got.from, T0 + 1_000 * MIN);
        assert!(got.truncated);
    }
    let got = api.candles(&format!("{range}&limit=10")).await;
    assert_eq!(got.candles, stored(5_990..6_000));
    assert!(got.truncated);
    // `from` may be left out when the limit says how far back to go.
    let got = api
        .candles(&format!("&timeframe=1m&to={}&limit=3", T0 + 100 * MIN))
        .await;
    assert_eq!(got.candles, stored(97..100));
    assert!(!got.truncated);
    assert!(api.rest.calls().is_empty());
    api.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn the_bar_still_open_is_never_returned() {
    // At 01:30:30 the minute opening at 01:30 is still open; both the store and the venue have it.
    let now = T0 + 90 * MIN + 30_000;
    let api = Api::start(&stored(0..91), FakeRest::new(at_venue(0..91), now)).await;
    for to in ["".to_string(), format!("&to={}", T0 + 10 * HOUR)] {
        let got = api.candles(&format!("&timeframe=1m&limit=5{to}")).await;
        assert_eq!(got.candles, stored(85..90), "to {to:?}");
        assert_eq!(got.to, T0 + 90 * MIN);
        assert!(got.from_venue.is_empty() && got.missing.is_empty());
    }
    // The hour opening at 01:00 is still open too.
    let got = api.candles("&timeframe=1h&limit=2").await;
    assert_eq!(
        got.candles,
        resample(InstrumentKind::Spot, &stored(0..60), Timeframe::H1, false).candles
    );
    assert_eq!(got.missing, [span(T0 - HOUR, T0, Timeframe::H1)]);
    api.shutdown().await;
}

/// Monday 4 January 2016, 00:00 UTC.
const MONDAY_2016: Millis = 1_451_865_600_000;

/// Writes an hour of USDJPY bids and asks from [`MONDAY_2016`] to `store`, and answers the bids.
fn store_usdjpy_hour(store: &Store) -> Vec<Candle> {
    let usdjpy = InstrumentId {
        venue: Venue::Oanda,
        symbol: "USDJPY".into(),
        kind: InstrumentKind::Fx,
    };
    let bids: Vec<Candle> = (0..60)
        .map(|i| Candle {
            open_time: MONDAY_2016 + i * MIN,
            open: 119.0,
            high: 119.01,
            low: 118.99,
            close: 119.0,
            volume: 25.0,
            quote_volume: 0.0,
        })
        .collect();
    let asks: Vec<Candle> = bids
        .iter()
        .map(|b| Candle {
            open: b.open + 0.007,
            high: b.high + 0.007,
            low: b.low + 0.007,
            close: b.close + 0.007,
            ..*b
        })
        .collect();
    store
        .write_quotes(&usdjpy, &bids, &asks)
        .expect("writes the fixture");
    bids
}

/// A chart starts no earlier than the venue's earliest download date: OANDA's stored FX minutes
/// from 2016 are served, back to 2007-01-01, while Binance keeps 2017-01-01.
#[tokio::test(flavor = "multi_thread")]
async fn the_earliest_bar_is_the_venues_earliest_download_date() {
    let api = Api::start(&[], FakeRest::new(Vec::new(), T0)).await;
    let monday = MONDAY_2016;
    let bids = store_usdjpy_hour(&api.store);

    let path = |instrument: &str, timeframe: &str, from: Millis, to: Millis| {
        format!("/candles?instrument={instrument}&timeframe={timeframe}&from={from}&to={to}")
    };
    let server = &api;
    let get = |path: String| async move {
        let (status, body) = server.get(&path).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        serde_json::from_value::<CandlesResponse>(body).expect("a candles response")
    };
    let got = get(path("oanda:USDJPY:fx", "1m", monday, monday + HOUR)).await;
    assert_eq!(got.from, monday);
    assert_eq!(got.candles, bids);

    // 2006-06-01 to 2007-03-01: OANDA's range starts at 2007-01-01, Binance's is empty at
    // 2017-01-01.
    let (june_2006, march_2007) = (1_149_120_000_000, 1_172_707_200_000);
    let got = get(path("oanda:USDJPY:fx", "1d", june_2006, march_2007)).await;
    assert_eq!((got.from, got.to), (1_167_609_600_000, march_2007));
    let got = get(path(INSTRUMENT, "1d", june_2006, march_2007)).await;
    assert_eq!((got.from, got.to), (1_483_228_800_000, 1_483_228_800_000));
    assert!(got.candles.is_empty());
    api.shutdown().await;
}

/// OANDA has no live stream, so the venue has no bars for the slots the store lacks: they are
/// reported missing up to the latest bar, and the stored bars come back. The weekend's closed
/// hours have no bar to miss, so they are neither reported nor asked of the venue.
#[tokio::test(flavor = "multi_thread")]
async fn an_oanda_chart_reports_the_slots_the_store_lacks_as_missing() {
    const DAY: Millis = 24 * HOUR;
    let api = Api::start(&[], FakeRest::new(Vec::new(), T0)).await;
    let bids = store_usdjpy_hour(&api.store);
    // From Saturday 2 January to Monday 02:00: the weekend, the stored hour and the one after it.
    // The weekend ends at 22:00 UTC on Sunday, New York's 17:00 in standard time.
    let (saturday, to) = (MONDAY_2016 - 2 * DAY, MONDAY_2016 + 2 * HOUR);
    let sunday_open = MONDAY_2016 - 2 * HOUR;
    let (status, body) = api
        .get(&format!(
            "/candles?instrument=oanda:USDJPY:fx&timeframe=1h&from={saturday}&to={to}"
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let got: CandlesResponse = serde_json::from_value(body).expect("a candles response");
    let tf = Timeframe::H1;
    assert_eq!(
        got.candles,
        resample(InstrumentKind::Fx, &bids, tf, false).candles
    );
    assert_eq!(got.candles.len(), 1);
    assert!(got.from_venue.is_empty(), "{got:?}");
    assert_eq!(
        got.missing,
        [
            span(sunday_open, MONDAY_2016, tf),
            span(MONDAY_2016 + HOUR, to, tf)
        ]
    );
    // The venue was asked for the open hours only, and its refusal kept nothing out of the
    // response.
    assert_eq!(api.rest.calls(), [(tf, sunday_open, to)]);
    api.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn bad_instruments_timeframes_and_ranges_are_400() {
    let api = Api::start(&stored(0..60), FakeRest::new(at_venue(0..60), T0 + HOUR)).await;
    let range = format!("&from={T0}&to={}", T0 + 30 * MIN);
    let paths = [
        format!("/candles?instrument=kraken:BTCUSDT:spot&timeframe=1m{range}"),
        format!("/candles?instrument=binance:BTCUSDT:future&timeframe=1m{range}"),
        format!("/candles?instrument=BTCUSDT&timeframe=1m{range}"),
        format!("/candles?instrument=binance:btcusdt:spot&timeframe=1m{range}"),
        format!("/candles?timeframe=1m{range}"),
        format!("/candles?instrument={INSTRUMENT}&timeframe=2m{range}"),
        format!("/candles?instrument={INSTRUMENT}{range}"),
        format!("/candles?instrument={INSTRUMENT}&timeframe=1m&from={T0}&to={T0}"),
        format!(
            "/candles?instrument={INSTRUMENT}&timeframe=1m&from={}&to={T0}",
            T0 + MIN
        ),
        format!(
            "/candles?instrument={INSTRUMENT}&timeframe=1m&from={}",
            T0 + 2 * HOUR
        ),
        format!("/candles?instrument={INSTRUMENT}&timeframe=1m&from=yesterday"),
        format!("/candles?instrument={INSTRUMENT}&timeframe=1m{range}&limit=0"),
        format!("/candles?instrument={INSTRUMENT}&timeframe=1m{range}&limit=-1"),
    ];
    for path in paths {
        let (status, body) = api.get(&path).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{path}: {body}");
        assert_eq!(body["error"]["code"], "bad_request", "{path}");
    }
    assert!(api.rest.calls().is_empty(), "nothing reached the venue");
    api.shutdown().await;
}

fn venue_status(status: u16, body: &str) -> DataError {
    DataError::Status {
        url: "https://venue.test/klines".into(),
        status,
        body: body.into(),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_venue_failure_is_a_502_with_the_venues_message() {
    let failures: [(Failure, &str); 3] = [
        (
            || venue_status(503, "the venue is down for maintenance"),
            "the venue is down for maintenance",
        ),
        // The venue's rate limit says nothing about the request.
        (
            || venue_status(429, "too many requests"),
            "too many requests",
        ),
        // Bybit answers 200 with a non-zero retCode.
        (
            || DataError::Api {
                url: "https://venue.test/klines".into(),
                message: "retCode 10016: service error".into(),
            },
            "retCode 10016: service error",
        ),
    ];
    for (fail, expected) in failures {
        let rest = FakeRest {
            fail: Some(fail),
            ..FakeRest::new(Vec::new(), T0 + 10 * HOUR)
        };
        let api = Api::start(&stored(0..60), rest).await;
        let (status, body) = api
            .get(&format!(
                "/candles?instrument={INSTRUMENT}&timeframe=1m&from={T0}&to={}",
                T0 + 90 * MIN
            ))
            .await;
        assert_eq!(status, StatusCode::BAD_GATEWAY, "{body}");
        assert_eq!(body["error"]["code"], "venue");
        let message = body["error"]["message"].as_str().expect("a message");
        assert!(message.contains(expected), "{message}");
        // A range the store covers never asks the venue, so it still answers.
        let got = api
            .candles(&format!("&timeframe=1m&from={T0}&to={}", T0 + 60 * MIN))
            .await;
        assert_eq!(got.candles, stored(0..60));
        assert_eq!(api.rest.calls().len(), 1);
        api.shutdown().await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_symbol_the_venue_does_not_list_is_a_400_with_the_venues_message() {
    // Binance answers an unknown symbol with a 400.
    let rest = FakeRest {
        fail: Some(|| venue_status(400, r#"{"code":-1121,"msg":"Invalid symbol."}"#)),
        ..FakeRest::new(Vec::new(), T0 + 10 * HOUR)
    };
    let api = Api::start(&[], rest).await;
    let (status, body) = api
        .get("/candles?instrument=binance:NOSUCHUSDT:spot&timeframe=1h")
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(body["error"]["code"], "bad_request");
    let message = body["error"]["message"].as_str().expect("a message");
    assert!(message.contains("Invalid symbol."), "{message}");
    assert_eq!(api.rest.calls().len(), 1);
    api.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn the_candles_route_needs_the_token() {
    let api = Api::start(&stored(0..60), FakeRest::new(Vec::new(), T0 + HOUR)).await;
    let url = api.url(&format!(
        "/candles?instrument={INSTRUMENT}&timeframe=1m&from={T0}&to={}",
        T0 + 30 * MIN
    ));
    for request in [
        api.client.get(&url),
        api.client.get(&url).header("authorization", "Bearer wrong"),
    ] {
        let (status, body) = api.send(request).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");
        assert_eq!(body["error"]["code"], "unauthorized");
    }
    api.shutdown().await;
}
