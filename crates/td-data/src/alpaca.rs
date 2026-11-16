//! Alpaca as a [`DataSource`]: US stocks and ETFs, active and delisted, from the paper trading API's assets list, and
//! their split- and dividend-adjusted daily bars from the data API (<https://docs.alpaca.markets/us/reference/>).
//! Each bar is stored as the candle of its session's last minute ([`td_core::us_bar_minute`]); keys never leave headers.

use std::collections::HashSet;
use std::fmt;
use std::time::Duration;

use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use serde::Deserialize;
use td_core::{
    Candle, FeeSchedule, Instrument, InstrumentId, InstrumentKind, Millis, Venue,
    new_york_offset_hours, us_bar_minute,
};

use crate::http::Http;
use crate::oanda::{RETRY_DELAYS, with_retries};
use crate::source::{DataSource, MinuteHistory};
use crate::time::{DAY, MINUTE, format_date, format_minute};
use crate::{DataError, InstrumentList};

const DATA_HOST: &str = "https://data.alpaca.markets";
const TRADING_HOST: &str = "https://paper-api.alpaca.markets";
const KEY_ID_VAR: &str = "APCA_API_KEY_ID";
const SECRET_VAR: &str = "APCA_API_SECRET_KEY";
const PAGE: usize = 10_000;
const ADJUSTMENT: &str = "all";
const RECENT: Millis = 15 * MINUTE;
const LOT: f64 = 1e-9;
const MIN_NOTIONAL: f64 = 1.0;

const STOCK_FEES: FeeSchedule = FeeSchedule {
    maker: 0.0,
    taker: 0.0,
    per_unit: 0.0,
};

#[derive(Debug, Clone, Copy, Default)]
pub struct Alpaca;

impl DataSource for Alpaca {
    fn venue(&self) -> Venue {
        Venue::Alpaca
    }

    fn kinds(&self) -> &'static [InstrumentKind] {
        &[InstrumentKind::Stock]
    }

    fn instruments(&self, http: &Http, kind: InstrumentKind) -> Result<InstrumentList, DataError> {
        stock_only(kind)?;
        assets(
            |name| std::env::var(name).ok(),
            &RETRY_DELAYS,
            |keys, url| get_page(http, keys, url),
        )
    }

    fn default_fees(&self, kind: InstrumentKind) -> Result<FeeSchedule, DataError> {
        stock_only(kind)?;
        Ok(STOCK_FEES)
    }

    fn adjustment(&self) -> Option<&'static str> {
        Some(ADJUSTMENT)
    }

    fn lists_delisted(&self) -> bool {
        true
    }

    fn minute_history(
        &self,
        http: &Http,
        id: &InstrumentId,
        from: Millis,
        to: Millis,
        now: Millis,
    ) -> Result<MinuteHistory, DataError> {
        history(
            id,
            from,
            to,
            now,
            |name| std::env::var(name).ok(),
            &RETRY_DELAYS,
            |keys, url| get_page(http, keys, url),
        )
    }
}

fn instrument(symbol: &str) -> Instrument {
    Instrument {
        id: InstrumentId {
            venue: Venue::Alpaca,
            symbol: symbol.to_string(),
            kind: InstrumentKind::Stock,
        },
        base: symbol.to_string(),
        quote: "USD".into(),
        tick_size: 0.01,
        lot_size: LOT,
        min_qty: LOT,
        min_notional: MIN_NOTIONAL,
        fees: STOCK_FEES,
    }
}

pub(crate) fn assets(
    env: impl Fn(&str) -> Option<String>,
    delays: &[Duration],
    mut get: impl FnMut(&Keys, &str) -> Result<(u16, Vec<u8>), DataError>,
) -> Result<InstrumentList, DataError> {
    let keys = Keys::from_lookup(env)?;
    let url = format!("{TRADING_HOST}/v2/assets?status=all&asset_class=us_equity");
    let body = with_retries(delays, || {
        let (status, body) = get(&keys, &url)?;
        answer(&url, status, body, &keys)
    })?;
    parse_assets(&url, &body)
}

fn parse_assets(url: &str, body: &str) -> Result<InstrumentList, DataError> {
    #[derive(Deserialize)]
    struct Asset {
        symbol: String,
        class: String,
    }
    let assets: Vec<Asset> = serde_json::from_str(body).map_err(|e| DataError::parse(url, e))?;
    let mut list = InstrumentList::default();
    let mut seen = HashSet::new();
    for asset in assets {
        if asset.class == "us_equity" && seen.insert(asset.symbol.clone()) {
            list.push(&asset.symbol, Ok(instrument(&asset.symbol)));
        }
    }
    Ok(list)
}

fn stock_only(kind: InstrumentKind) -> Result<(), DataError> {
    match kind {
        InstrumentKind::Stock => Ok(()),
        _ => Err(DataError::KindNotListed {
            venue: Venue::Alpaca,
            kind,
        }),
    }
}

pub(crate) struct Keys {
    id: String,
    secret: String,
}

impl fmt::Debug for Keys {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Keys(..)")
    }
}

impl Keys {
    fn from_lookup(lookup: impl Fn(&str) -> Option<String>) -> Result<Self, DataError> {
        let read = |name: &str| {
            lookup(name)
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty())
        };
        match (read(KEY_ID_VAR), read(SECRET_VAR)) {
            (Some(id), Some(secret)) => Ok(Keys { id, secret }),
            _ => Err(DataError::MissingAlpacaKeys),
        }
    }

    fn scrub(&self, text: &str) -> String {
        text.replace(&self.secret, "[secret]")
            .replace(&self.id, "[key id]")
    }
}

pub(crate) fn history(
    id: &InstrumentId,
    from: Millis,
    to: Millis,
    now: Millis,
    env: impl Fn(&str) -> Option<String>,
    delays: &[Duration],
    mut get: impl FnMut(&Keys, &str) -> Result<(u16, Vec<u8>), DataError>,
) -> Result<MinuteHistory, DataError> {
    stock_only(id.kind)?;
    let keys = Keys::from_lookup(env)?;
    let mut history = MinuteHistory::default();
    let end = to.min(now - RECENT);
    if from - DAY >= end {
        return Ok(history);
    }
    let mut token: Option<String> = None;
    let mut tokens = HashSet::new();
    let mut previous: Option<Millis> = None;
    loop {
        let url = page_url(
            DATA_HOST,
            &id.symbol,
            from - DAY,
            end,
            now,
            token.as_deref(),
        );
        let body = with_retries(delays, || {
            let (status, body) = get(&keys, &url)?;
            answer(&url, status, body, &keys)
        })?;
        let page = parse_page(&url, &body, &id.symbol)?;
        for candle in page.candles {
            if previous.is_some_and(|p| candle.open_time <= p) {
                return Err(DataError::parse(
                    &url,
                    format!(
                        "the bar of {} is not after the one before it",
                        format_date(candle.open_time)
                    ),
                ));
            }
            previous = Some(candle.open_time);
            if (from..to).contains(&candle.open_time) && candle.open_time + MINUTE <= now - RECENT {
                history.candles.push(candle);
            }
        }
        match page.next_page_token {
            Some(next) if !next.is_empty() => {
                if !tokens.insert(next.clone()) {
                    return Err(DataError::parse(
                        &url,
                        "it gives a next_page_token an earlier page gave, so its pages never end",
                    ));
                }
                token = Some(next);
            }
            _ => return Ok(history),
        }
    }
}

fn page_url(
    base: &str,
    symbol: &str,
    start: Millis,
    end: Millis,
    now: Millis,
    token: Option<&str>,
) -> String {
    let time = |t: Millis| format_minute(t).replace(' ', "T") + ":00Z";
    let mut url = format!(
        "{base}/v2/stocks/{symbol}/bars?timeframe=1Day&start={}&end={}&limit={PAGE}\
         &adjustment={ADJUSTMENT}&feed=sip&sort=asc&asof={}",
        time(start),
        time(end),
        format_date(now)
    );
    if let Some(token) = token {
        url.push_str("&page_token=");
        url.extend(token.bytes().map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                char::from(b).to_string()
            }
            _ => format!("%{b:02X}"),
        }));
    }
    url
}

fn get_page(http: &Http, keys: &Keys, url: &str) -> Result<(u16, Vec<u8>), DataError> {
    let header = |value: &str, var: &str| {
        let mut value = HeaderValue::from_str(value).map_err(|_| {
            DataError::parse(var, "it holds a character an HTTP header cannot carry")
        })?;
        value.set_sensitive(true);
        Ok::<_, DataError>(value)
    };
    let mut headers = HeaderMap::new();
    headers.insert(
        HeaderName::from_static("apca-api-key-id"),
        header(&keys.id, KEY_ID_VAR)?,
    );
    headers.insert(
        HeaderName::from_static("apca-api-secret-key"),
        header(&keys.secret, SECRET_VAR)?,
    );
    http.get_with_headers(url, headers)
}

fn answer(url: &str, status: u16, body: Vec<u8>, keys: &Keys) -> Result<String, DataError> {
    if (200..300).contains(&status) {
        let text = String::from_utf8(body)
            .map_err(|_| DataError::parse(url, "the answer is not UTF-8 text"))?;
        return Ok(keys.scrub(&text));
    }
    #[derive(Deserialize)]
    struct ErrorBody {
        message: String,
    }
    let message = serde_json::from_slice::<ErrorBody>(&body)
        .map(|e| keys.scrub(&e.message).chars().take(200).collect())
        .unwrap_or_default();
    let url = url.to_string();
    Err(match status {
        401 | 403 => DataError::AlpacaRefused {
            url,
            status,
            message,
        },
        _ => DataError::Status {
            url,
            status,
            body: message,
        },
    })
}

struct Page {
    candles: Vec<Candle>,
    next_page_token: Option<String>,
}

fn parse_page(url: &str, body: &str, symbol: &str) -> Result<Page, DataError> {
    #[derive(Deserialize)]
    struct Raw {
        #[serde(deserialize_with = "Option::deserialize")]
        bars: Option<Vec<RawBar>>,
        symbol: String,
        #[serde(default)]
        next_page_token: Option<String>,
    }
    #[derive(Deserialize)]
    struct RawBar {
        t: String,
        o: f64,
        h: f64,
        l: f64,
        c: f64,
        v: f64,
    }
    let raw: Raw = serde_json::from_str(body).map_err(|e| DataError::parse(url, e))?;
    if raw.symbol != symbol {
        return Err(DataError::parse(
            url,
            format!("it holds bars of {}, not {symbol}", raw.symbol),
        ));
    }
    let candles = raw
        .bars
        .unwrap_or_default()
        .into_iter()
        .map(|bar| {
            let day = parse_midnight_new_york(&bar.t).ok_or_else(|| {
                DataError::parse(
                    url,
                    format!(
                        "{:?} is not a daily bar's time, midnight in New York",
                        bar.t
                    ),
                )
            })?;
            let minute = us_bar_minute(day).ok_or_else(|| {
                DataError::parse(
                    url,
                    format!(
                        "it has a bar for {}, which the US market calendar has as closed",
                        format_date(day * DAY)
                    ),
                )
            })?;
            let candle = Candle {
                open_time: minute.start,
                open: bar.o,
                high: bar.h,
                low: bar.l,
                close: bar.c,
                volume: bar.v,
                quote_volume: 0.0,
            };
            candle.validate().map_err(|source| DataError::Candle {
                instrument: format!("alpaca:{symbol}"),
                source,
            })?;
            Ok(candle)
        })
        .collect::<Result<_, DataError>>()?;
    Ok(Page {
        candles,
        next_page_token: raw.next_page_token,
    })
}

fn parse_midnight_new_york(text: &str) -> Option<i64> {
    let b = text.as_bytes();
    let shape = b.len() == 20
        && [4, 7].iter().all(|&i| b[i] == b'-')
        && b[10] == b'T'
        && [13, 16].iter().all(|&i| b[i] == b':')
        && b[19] == b'Z';
    if !shape {
        return None;
    }
    let number = |range: std::ops::Range<usize>| text.get(range)?.parse::<u32>().ok();
    let (year, month, day) = (number(0..4)?, number(5..7)?, number(8..10)?);
    let time = (number(11..13)?, number(14..16)?, number(17..19)?);
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    let date = td_core::days_from_civil(i64::from(year), month, day);
    if td_core::civil_from_days(date) != (i64::from(year), month, day) {
        return None;
    }
    let hour = u32::try_from(new_york_offset_hours(date)).ok()?;
    (time == (hour, 0, 0)).then_some(date)
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::sync::Mutex;

    use super::*;
    use crate::Store;
    use crate::download::download_candles_from;
    use crate::http::stub;
    use crate::time::{Month, parse_date};

    const PAGE_ONE: &str = include_str!("../../../mock/alpaca/aapl-2024-01-page-1.json");
    const PAGE_TWO: &str = include_str!("../../../mock/alpaca/aapl-2024-01-page-2.json");
    const FORBIDDEN: &str = include_str!("../../../mock/alpaca/forbidden.json");
    const EMPTY: &str = include_str!("../../../mock/alpaca/empty.json");
    const ASSETS: &str = include_str!("../../../mock/alpaca/assets.json");

    fn aapl() -> InstrumentId {
        InstrumentId {
            venue: Venue::Alpaca,
            symbol: "AAPL".into(),
            kind: InstrumentKind::Stock,
        }
    }

    fn env(name: &str) -> Option<String> {
        match name {
            KEY_ID_VAR => Some("PKTESTKEYID".into()),
            SECRET_VAR => Some("testsecretvalue".into()),
            _ => None,
        }
    }

    fn date(text: &str) -> Millis {
        parse_date(text).expect("a date")
    }

    fn serve(
        asked: &RefCell<Vec<String>>,
    ) -> impl FnMut(&Keys, &str) -> Result<(u16, Vec<u8>), DataError> {
        move |_, url| {
            asked.borrow_mut().push(url.to_string());
            let body = if url.contains("page_token=") {
                PAGE_TWO
            } else {
                PAGE_ONE
            };
            Ok((200, body.as_bytes().to_vec()))
        }
    }

    fn days(candles: &[Candle]) -> Vec<String> {
        candles.iter().map(|c| format_date(c.open_time)).collect()
    }

    #[test]
    fn a_month_is_read_page_by_page_with_each_bar_at_its_sessions_last_minute() {
        let asked = RefCell::new(Vec::new());
        let (from, to) = (date("2024-01-01"), date("2024-02-01"));
        let history = history(
            &aapl(),
            from,
            to,
            date("2026-01-01"),
            env,
            &[],
            serve(&asked),
        )
        .expect("reads");
        assert_eq!(history.candles.len(), 21);
        assert_eq!(days(&history.candles)[..2], ["2024-01-02", "2024-01-03"]);
        assert!(
            !days(&history.candles).contains(&"2024-01-15".to_string()),
            "MLK day"
        );
        assert_eq!(
            format_minute(history.candles[0].open_time),
            "2024-01-02 20:59",
            "15:59 New York in winter"
        );
        assert_eq!(history.candles[0].close, 185.64);
        assert!(history.asks.is_empty());
        let asked = asked.into_inner();
        assert_eq!(asked.len(), 2);
        assert_eq!(
            asked[0],
            "https://data.alpaca.markets/v2/stocks/AAPL/bars?timeframe=1Day\
             &start=2023-12-31T00:00:00Z&end=2024-02-01T00:00:00Z&limit=10000&adjustment=all\
             &feed=sip&sort=asc&asof=2026-01-01"
        );
        assert!(asked[1].ends_with("&page_token=QUFQTHxEfDIwMjQtMDEtMTJUMDU6MDA6MDBa"));
    }

    #[test]
    fn bars_outside_the_range_or_before_their_session_closed_are_left_out() {
        let asked = RefCell::new(Vec::new());
        let (from, to) = (date("2024-01-10"), date("2024-01-20"));
        let got = history(
            &aapl(),
            from,
            to,
            date("2026-01-01"),
            env,
            &[],
            serve(&asked),
        )
        .expect("reads");
        assert_eq!(
            days(&got.candles).first().map(String::as_str),
            Some("2024-01-10")
        );
        assert_eq!(
            days(&got.candles).last().map(String::as_str),
            Some("2024-01-19")
        );

        let jan_31_close = us_bar_minute(date("2024-01-31") / DAY)
            .expect("a session")
            .end;
        let asked = RefCell::new(Vec::new());
        let to = date("2024-02-01");
        let history = history(
            &aapl(),
            from,
            to,
            jan_31_close - MINUTE,
            env,
            &[],
            serve(&asked),
        )
        .expect("reads");
        assert_eq!(
            days(&history.candles).last().map(String::as_str),
            Some("2024-01-30")
        );
        assert!(
            asked.borrow()[0].contains("&end=2024-01-31T20:44:00Z"),
            "{:?}",
            asked.borrow()
        );
    }

    #[test]
    fn a_bar_on_a_closed_day_or_at_another_time_is_refused() {
        let bar = |t: &str| {
            format!(
                r#"{{"bars":[{{"t":"{t}","o":1.0,"h":1.0,"l":1.0,"c":1.0,"v":1,"n":1,"vw":1.0}}],"symbol":"AAPL","next_page_token":null}}"#
            )
        };
        for (t, reason) in [
            (
                "2024-01-15T05:00:00Z",
                "which the US market calendar has as closed",
            ),
            (
                "2024-01-13T05:00:00Z",
                "which the US market calendar has as closed",
            ),
            ("2024-01-16T00:00:00Z", "midnight in New York"),
            ("2024-01-16T04:00:00Z", "midnight in New York"),
            ("2024-07-16T05:00:00Z", "midnight in New York"),
            ("2024-01-16", "midnight in New York"),
        ] {
            let body = bar(t);
            let e = history(
                &aapl(),
                date("2024-01-01"),
                date("2024-02-01"),
                date("2026-01-01"),
                env,
                &[],
                |_, _| Ok((200, body.clone().into_bytes())),
            )
            .unwrap_err();
            assert!(e.to_string().contains(reason), "{t}: {e}");
        }
        let other = PAGE_TWO.replace("\"AAPL\"", "\"MSFT\"");
        let e = history(
            &aapl(),
            date("2024-01-01"),
            date("2024-02-01"),
            date("2026-01-01"),
            env,
            &[],
            |_, _| Ok((200, other.clone().into_bytes())),
        )
        .unwrap_err();
        assert!(e.to_string().contains("bars of MSFT"), "{e}");
    }

    #[test]
    fn midnight_in_new_york_is_four_or_five_utc_by_the_date() {
        let day = |text: &str| Some(date(text) / DAY);
        assert_eq!(
            parse_midnight_new_york("2024-01-16T05:00:00Z"),
            day("2024-01-16")
        );
        assert_eq!(
            parse_midnight_new_york("2024-07-16T04:00:00Z"),
            day("2024-07-16")
        );
        assert_eq!(
            parse_midnight_new_york("2024-03-11T04:00:00Z"),
            day("2024-03-11"),
            "the Monday after summer time starts"
        );
        assert_eq!(
            parse_midnight_new_york("2024-11-04T05:00:00Z"),
            day("2024-11-04"),
            "the Monday after summer time ends"
        );
        assert_eq!(parse_midnight_new_york("2024-03-11T05:00:00Z"), None);
        assert_eq!(parse_midnight_new_york("2024-11-01T05:00:00Z"), None);
    }

    #[test]
    fn a_page_without_a_bars_key_is_refused() {
        let body = r#"{"symbol":"AAPL","next_page_token":null}"#;
        let e = history(
            &aapl(),
            date("2024-01-01"),
            date("2024-02-01"),
            date("2026-01-01"),
            env,
            &[],
            |_, _| Ok((200, body.as_bytes().to_vec())),
        )
        .unwrap_err();
        assert!(e.to_string().contains("missing field `bars`"), "{e}");
    }

    #[test]
    fn a_page_token_given_twice_is_refused() {
        let looping = r#"{"bars":[],"symbol":"AAPL","next_page_token":"QUFQTA"}"#;
        let mut asked = 0;
        let e = history(
            &aapl(),
            date("2024-01-01"),
            date("2024-02-01"),
            date("2026-01-01"),
            env,
            &[],
            |_, _| {
                asked += 1;
                assert!(asked <= 3, "it keeps asking");
                Ok((200, looping.as_bytes().to_vec()))
            },
        )
        .unwrap_err();
        assert_eq!(asked, 2);
        assert!(e.to_string().contains("next_page_token"), "{e}");
    }

    #[test]
    fn an_empty_answer_is_no_bars() {
        let history = history(
            &aapl(),
            date("2024-01-01"),
            date("2024-02-01"),
            date("2026-01-01"),
            env,
            &[],
            |_, _| Ok((200, EMPTY.as_bytes().to_vec())),
        )
        .expect("reads");
        assert!(history.candles.is_empty());
    }

    #[test]
    fn without_both_keys_the_source_fails_before_any_request() {
        for missing in [KEY_ID_VAR, SECRET_VAR] {
            let lookup = |name: &str| {
                if name == missing {
                    Some("  ".into())
                } else {
                    env(name)
                }
            };
            let e = history(
                &aapl(),
                date("2024-01-01"),
                date("2024-02-01"),
                date("2026-01-01"),
                lookup,
                &[],
                |_, _| panic!("no request without keys"),
            )
            .unwrap_err();
            assert!(matches!(e, DataError::MissingAlpacaKeys), "{e}");
            assert!(
                e.to_string()
                    .contains("APCA_API_KEY_ID and APCA_API_SECRET_KEY"),
                "{e}"
            );
        }
    }

    #[test]
    fn a_403_is_refused_without_retrying_and_never_shows_the_keys() {
        let mut tries = 0;
        let e = history(
            &aapl(),
            date("2024-01-01"),
            date("2024-02-01"),
            date("2026-01-01"),
            env,
            &[Duration::ZERO; 3],
            |_, _| {
                tries += 1;
                let body =
                    FORBIDDEN.replace("forbidden", "forbidden for PKTESTKEYID testsecretvalue");
                Ok((403, body.into_bytes()))
            },
        )
        .unwrap_err();
        assert_eq!(tries, 1);
        let shown = e.to_string();
        assert!(
            matches!(e, DataError::AlpacaRefused { status: 403, .. }),
            "{shown}"
        );
        assert!(
            !shown.contains("PKTESTKEYID") && !shown.contains("testsecretvalue"),
            "{shown}"
        );
        assert!(shown.contains("[key id]"), "{shown}");
    }

    #[test]
    fn a_429_or_5xx_is_retried() {
        let mut answers = vec![(429, b"{}".to_vec()), (502, Vec::new())].into_iter();
        let history = history(
            &aapl(),
            date("2024-01-10"),
            date("2024-01-12"),
            date("2026-01-01"),
            env,
            &[Duration::ZERO; 4],
            |_, url| {
                let page = if url.contains("page_token=") {
                    PAGE_TWO
                } else {
                    PAGE_ONE
                };
                Ok(answers.next().unwrap_or((200, page.as_bytes().to_vec())))
            },
        )
        .expect("reads after retries");
        assert_eq!(days(&history.candles), ["2024-01-10", "2024-01-11"]);
    }

    #[test]
    fn a_request_carries_the_keys_only_in_its_headers() {
        let (base, server) = stub::serve_once(&[], EMPTY.as_bytes().to_vec());
        let keys = Keys::from_lookup(env).expect("keys");
        let url = page_url(
            &base,
            "AAPL",
            date("2024-01-01"),
            date("2024-02-01"),
            date("2026-01-01"),
            None,
        );
        let (status, _) = get_page(&Http::new().expect("a client"), &keys, &url).expect("answers");
        assert_eq!(status, 200);
        let request = server.join().expect("the server");
        let line = request.lines().next().expect("a request line");
        assert!(
            !line.contains("pktestkeyid") && !line.contains("testsecretvalue"),
            "{line}"
        );
        assert!(
            request.contains("apca-api-key-id: pktestkeyid"),
            "{request}"
        );
        assert!(
            request.contains("apca-api-secret-key: testsecretvalue"),
            "{request}"
        );
        assert!(!format!("{keys:?}").contains("PKTESTKEYID"));
    }

    #[test]
    fn every_listed_stock_active_or_delisted_trades_in_fractional_shares_without_commission() {
        let mut asked = Vec::new();
        let list = assets(env, &[], |_, url| {
            asked.push(url.to_string());
            Ok((200, ASSETS.as_bytes().to_vec()))
        })
        .expect("the list");
        assert_eq!(
            asked,
            ["https://paper-api.alpaca.markets/v2/assets?status=all&asset_class=us_equity"]
        );
        let symbols: Vec<&str> = list
            .instruments
            .iter()
            .map(|i| i.id.symbol.as_str())
            .collect();
        assert_eq!(symbols, ["AAPL", "MSFT", "SPY", "META", "TWTR"]);
        assert_eq!(list.rejected.len(), 1);
        assert_eq!(list.rejected[0].0, "BRK.B");
        let spy = &list.instruments[2];
        assert_eq!((spy.base.as_str(), spy.quote.as_str()), ("SPY", "USD"));
        assert_eq!(
            (spy.tick_size, spy.lot_size, spy.min_qty, spy.min_notional),
            (0.01, 1e-9, 1e-9, 1.0)
        );
        assert_eq!(spy.fees, STOCK_FEES);
        let http = Http::new().expect("a client");
        assert!(matches!(
            Alpaca.instruments(&http, InstrumentKind::Spot),
            Err(DataError::KindNotListed { .. })
        ));
        assert_eq!(Alpaca.adjustment(), Some("all"));
        assert!(Alpaca.lists_delisted());
        assert!(Alpaca.live().is_err());
    }

    #[test]
    fn the_assets_list_without_keys_or_refused_never_shows_them() {
        let e = assets(|_| None, &[], |_, _| panic!("no request without keys")).unwrap_err();
        assert!(matches!(e, DataError::MissingAlpacaKeys), "{e}");
        let e = assets(env, &[], |_, _| {
            let body = FORBIDDEN.replace("forbidden", "forbidden for PKTESTKEYID testsecretvalue");
            Ok((403, body.into_bytes()))
        })
        .unwrap_err();
        let shown = e.to_string();
        assert!(matches!(e, DataError::AlpacaRefused { .. }), "{shown}");
        assert!(
            !shown.contains("PKTESTKEYID") && !shown.contains("testsecretvalue"),
            "{shown}"
        );
    }

    struct FakeAlpaca {
        pages: Mutex<Vec<String>>,
    }

    impl FakeAlpaca {
        fn new(pages: [&str; 2]) -> Self {
            FakeAlpaca {
                pages: Mutex::new(pages.map(String::from).to_vec()),
            }
        }
    }

    impl DataSource for FakeAlpaca {
        fn venue(&self) -> Venue {
            Venue::Alpaca
        }

        fn kinds(&self) -> &'static [InstrumentKind] {
            Alpaca.kinds()
        }

        fn instruments(&self, _: &Http, kind: InstrumentKind) -> Result<InstrumentList, DataError> {
            stock_only(kind)?;
            parse_assets("assets.json", ASSETS)
        }

        fn default_fees(&self, kind: InstrumentKind) -> Result<FeeSchedule, DataError> {
            Alpaca.default_fees(kind)
        }

        fn adjustment(&self) -> Option<&'static str> {
            Alpaca.adjustment()
        }

        fn lists_delisted(&self) -> bool {
            Alpaca.lists_delisted()
        }

        fn minute_history(
            &self,
            _: &Http,
            id: &InstrumentId,
            from: Millis,
            to: Millis,
            now: Millis,
        ) -> Result<MinuteHistory, DataError> {
            let pages = self.pages.lock().expect("lock").clone();
            history(id, from, to, now, env, &[], |_, url| {
                let page = pages.get(usize::from(url.contains("page_token=")));
                Ok((200, page.map_or(EMPTY, String::as_str).as_bytes().to_vec()))
            })
        }
    }

    #[test]
    fn a_download_stores_trading_days_and_records_the_adjustment_and_empty_months() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let store = Store::new(dir.path());
        let source = FakeAlpaca::new([PAGE_ONE, PAGE_TWO]);
        let http = Http::new().expect("a client");
        let report = download_candles_from(
            &source,
            &http,
            &store,
            &aapl(),
            date("2023-12-01"),
            date("2024-02-01"),
            &mut |_| {},
        )
        .expect("downloads");
        let jan = Month::parse("2024-01").expect("a month");
        let dec = Month::parse("2023-12").expect("a month");
        assert!(report.gaps.is_empty(), "{:?}", report.gaps);
        let closures = store
            .closures(&aapl(), dec.start(), jan.end())
            .expect("reads");
        assert_eq!(
            closures.closed_in(dec.start(), jan.end()),
            20 * MINUTE,
            "December's trading days, without Christmas, had no bar and are closed"
        );
        assert_eq!(store.read_month(&aapl(), jan).expect("reads").len(), 21);
        assert_eq!(
            store.adjustment(&aapl()).expect("reads"),
            Some("all".into())
        );
        assert!(
            store
                .recorded_empty(&aapl(), dec.start(), dec.end())
                .expect("reads"),
            "December had no bars and is recorded"
        );
        let coverage = store.coverage(&aapl()).expect("coverage");
        assert_eq!(coverage.len(), 1);
        assert_eq!((coverage[0].minutes, coverage[0].open_minutes), (21, 21));
    }

    #[test]
    fn a_symbol_alpaca_does_not_list_is_refused_before_any_bars_are_asked_for() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let store = Store::new(dir.path());
        let source = FakeAlpaca::new([PAGE_ONE, PAGE_TWO]);
        let http = Http::new().expect("a client");
        let unlisted = InstrumentId {
            symbol: "NOSUCH".into(),
            ..aapl()
        };
        let e = download_candles_from(
            &source,
            &http,
            &store,
            &unlisted,
            date("2024-01-01"),
            date("2024-02-01"),
            &mut |_| {},
        )
        .unwrap_err();
        assert_eq!(
            e.to_string(),
            "instrument NOSUCH: alpaca lists no stock by that symbol, active or delisted"
        );
        assert!(store.months(&unlisted).expect("reads").is_empty());
        assert!(dir.path().join("instruments/alpaca-stock.json").is_file());
        let twtr = InstrumentId {
            symbol: "TWTR".into(),
            ..aapl()
        };
        let empty = EMPTY.replace("AAPL", "TWTR");
        *source.pages.lock().expect("lock") = vec![empty.clone(), empty];
        download_candles_from(
            &source,
            &http,
            &store,
            &twtr,
            date("2024-01-01"),
            date("2024-02-01"),
            &mut |_| {},
        )
        .expect("a delisted stock downloads");
    }

    #[test]
    fn a_download_after_the_adjusted_prices_moved_is_refused() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let store = Store::new(dir.path());
        let http = Http::new().expect("a client");
        let (from, to) = (date("2024-01-01"), date("2024-02-01"));
        let source = FakeAlpaca::new([PAGE_ONE, PAGE_TWO]);
        download_candles_from(&source, &http, &store, &aapl(), from, to, &mut |_| {})
            .expect("downloads");
        download_candles_from(&source, &http, &store, &aapl(), from, to, &mut |_| {})
            .expect("the same prices download again");

        let moved = PAGE_TWO.replace("\"c\":183.63", "\"c\":183.64");
        assert_ne!(moved, PAGE_TWO);
        *source.pages.lock().expect("lock") = vec![PAGE_ONE.into(), moved];
        let e = download_candles_from(
            &source,
            &http,
            &store,
            &aapl(),
            to,
            date("2024-03-01"),
            &mut |_| {},
        )
        .unwrap_err();
        assert!(matches!(e, DataError::AdjustmentChanged { .. }), "{e}");
        assert!(e.to_string().contains("delete"), "{e}");
    }

    #[test]
    fn a_store_holding_another_adjustment_is_refused() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let store = Store::new(dir.path());
        store.record_adjustment(&aapl(), "raw").expect("records");
        let source = FakeAlpaca::new([PAGE_ONE, PAGE_TWO]);
        let e = download_candles_from(
            &source,
            &Http::new().expect("a client"),
            &store,
            &aapl(),
            date("2024-01-01"),
            date("2024-02-01"),
            &mut |_| {},
        )
        .unwrap_err();
        assert!(e.to_string().contains("raw"), "{e}");
    }

    fn live_env(name: &str) -> Option<String> {
        std::env::var(name).ok()
    }

    fn stock(symbol: &str) -> InstrumentId {
        InstrumentId {
            venue: Venue::Alpaca,
            symbol: symbol.into(),
            kind: InstrumentKind::Stock,
        }
    }

    #[test]
    #[ignore = "downloads from Alpaca with the keys in APCA_API_KEY_ID and APCA_API_SECRET_KEY"]
    fn live_alpaca_lists_delisted_stocks_and_serves_their_adjusted_daily_bars() {
        let http = Http::new().expect("a client");
        let keys = Keys::from_lookup(live_env).expect("the keys are set");
        let get = |url: &str| {
            let (status, body) = get_page(&http, &keys, url).expect("an answer");
            answer(url, status, body, &keys).expect("a 2xx answer")
        };

        let list = assets(live_env, &RETRY_DELAYS, |k, url| get_page(&http, k, url))
            .expect("the assets list");
        let listed: HashSet<&str> = list
            .instruments
            .iter()
            .map(|i| i.id.symbol.as_str())
            .collect();
        eprintln!(
            "assets with status=all: {} listed, {} left out",
            listed.len(),
            list.rejected.len()
        );
        let url = format!("{TRADING_HOST}/v2/assets?status=inactive&asset_class=us_equity");
        let raw: Vec<serde_json::Value> = serde_json::from_str(&get(&url)).expect("JSON");
        let inactive: HashSet<&str> = raw
            .iter()
            .filter(|a| a["status"] == "inactive")
            .filter_map(|a| a["symbol"].as_str())
            .collect();
        eprintln!("assets with status=inactive: {}", inactive.len());
        let delistings = [
            ("TWTR", "2022-10-27"),
            ("XLNX", "2022-02-11"),
            ("CELG", "2019-11-20"),
        ];
        for (symbol, _) in delistings {
            eprintln!(
                "{symbol}: listed {}, inactive {}",
                listed.contains(symbol),
                inactive.contains(symbol)
            );
        }
        assert!(listed.contains("AAPL") && listed.contains("META"));
        let &(gone, last_day) = delistings
            .iter()
            .find(|(s, _)| inactive.contains(s) && listed.contains(s))
            .expect("a delisted stock in the inactive assets");

        let dir = tempfile::tempdir().expect("a temp dir");
        let store = Store::new(dir.path());
        let (from, to) = (date("2016-01-01"), date("2024-01-01"));
        let mut firsts = Vec::new();
        for symbol in ["AAPL", "META", gone] {
            let id = stock(symbol);
            let report = download_candles_from(&Alpaca, &http, &store, &id, from, to, &mut |_| {})
                .expect("downloads");
            let candles: Vec<Candle> = store
                .months(&id)
                .expect("months")
                .into_iter()
                .flat_map(|m| store.read_month(&id, m).expect("a month"))
                .collect();
            let (first, last) = (candles[0], candles[candles.len() - 1]);
            eprintln!(
                "{symbol}: {} bars from {} (close {}) to {} (close {}), {} gaps",
                candles.len(),
                format_date(first.open_time),
                first.close,
                format_date(last.open_time),
                last.close,
                report.gaps.len()
            );
            firsts.push((symbol, format_date(first.open_time), last));
        }
        assert_eq!(firsts[0].1, "2016-01-04");
        assert_eq!(firsts[1].1, "2016-01-04", "META's bars from when it was FB");
        let last = firsts[2].2;
        assert_eq!(format_date(last.open_time), last_day, "{gone}'s last bar");
        let after = date(last_day) + DAY;
        assert!(
            store
                .recorded_empty(&stock(gone), after, to)
                .expect("records"),
            "downloads recorded nothing after {gone}'s last bar"
        );

        let june = (date("2023-06-01"), date("2023-07-01"));
        let fetch = || {
            let get = |k: &Keys, url: &str| get_page(&http, k, url);
            history(
                &aapl(),
                june.0,
                june.1,
                crate::time::now_ms(),
                live_env,
                &RETRY_DELAYS,
                get,
            )
            .expect("a month")
            .candles
        };
        let bits = |candles: &[Candle]| -> Vec<[u64; 5]> {
            candles
                .iter()
                .map(|c| [c.open, c.high, c.low, c.close, c.volume].map(f64::to_bits))
                .collect()
        };
        let (one, two) = (fetch(), fetch());
        assert_eq!(bits(&one), bits(&two), "two downloads of June 2023");
        eprintln!(
            "two downloads of AAPL 2023-06: {} bars each, the same bits",
            one.len()
        );
        let next = (to, date("2024-02-01"));
        download_candles_from(&Alpaca, &http, &store, &aapl(), next.0, next.1, &mut |_| {})
            .expect("a later download, after comparing the latest stored month again");
        eprintln!("a download of 2024-01 fetched 2023-12 again and found it unchanged");

        let small = |token: Option<&str>| {
            let (start, end) = (date("2023-01-01"), date("2023-02-01"));
            page_url(DATA_HOST, "AAPL", start, end, crate::time::now_ms(), token)
                .replace("limit=10000", "limit=5")
        };
        let first_url = small(None);
        assert!(first_url.contains("feed=sip"), "{first_url}");
        let page_one = parse_page(&first_url, &get(&first_url), "AAPL").expect("page one");
        let token = page_one.next_page_token.clone().expect("a next_page_token");
        let second_url = small(Some(&token));
        let page_two = parse_page(&second_url, &get(&second_url), "AAPL").expect("page two");
        eprintln!(
            "pages of 5 on the sip feed: a token of {} characters; {:?} then {:?}",
            token.len(),
            days(&page_one.candles),
            days(&page_two.candles)
        );
        assert_eq!((page_one.candles.len(), page_two.candles.len()), (5, 5));
        assert!(page_two.candles[0].open_time > page_one.candles[4].open_time);
        assert_ne!(page_two.next_page_token.as_deref(), Some(token.as_str()));
    }
}
