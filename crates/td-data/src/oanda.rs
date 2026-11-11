//! OANDA as a [`DataSource`]: FX spot pairs from a fixed table, and their 1-minute bid and ask
//! candles from OANDA's v20 REST API on a practice account. No funding and no live stream.
//!
//! The request is `GET /v3/instruments/{instrument}/candles` with `price=BA`, `granularity=M1`,
//! `from` and `count`, as OANDA's v20 OpenAPI spec (<https://github.com/oanda/v20-openapi>, read
//! on 2026-11-11) describes it: `time` is "the start time of the candlestick", a candle is
//! `complete` once "its ending time is not in the future", prices are decimal strings, and an
//! error body carries `errorMessage`. With `Accept-Datetime-Format: UNIX`, times are seconds since
//! the epoch as a decimal string. What the spec does not say, and was observed: a page holds at
//! most 5,000 candles, and a minute with no price has no candle.
//!
//! The token is read from `OANDA_API_TOKEN` when a download starts. It travels only in the
//! `Authorization` header, marked sensitive, and is never put in a URL, an error or any output.

use std::fmt;
use std::time::Duration;

use reqwest::header::{AUTHORIZATION, HeaderMap, HeaderName, HeaderValue};
use serde::Deserialize;
use td_core::{Candle, FeeSchedule, Instrument, InstrumentId, InstrumentKind, Millis, Venue};

use crate::http::Http;
use crate::source::{DataSource, MinuteHistory};
use crate::time::{MINUTE, Month, format_minute};
use crate::{DataError, InstrumentList};

/// The practice account's REST host.
const HOST: &str = "https://api-fxpractice.oanda.com";
/// The variable the token is read from.
const TOKEN_VAR: &str = "OANDA_API_TOKEN";
/// The most candles one request returns.
const PAGE: usize = 5_000;
/// The shortest run of open minutes without a candle that is taken for a closure or an outage
/// rather than a quiet market, and so left missing instead of filled. Quiet stretches in an open FX
/// market are minutes long: the gaps inside the week in OANDA's USD/JPY minutes are 1 to 5 minutes.
/// A market holiday the weekend rule does not know, such as 25 December, is a whole day.
const CLOSURE_MINUTES: i64 = 60;
/// The pauses before each retry of a page after a failed connection, a 429 or a 5xx: five tries
/// over about 40 seconds.
const RETRY_DELAYS: [Duration; 4] = [
    Duration::from_secs(2),
    Duration::from_secs(5),
    Duration::from_secs(10),
    Duration::from_secs(20),
];

/// The pairs served, as the desk names them: the seven majors and the main crosses. Each is its
/// base currency's three letters then its quote currency's; OANDA puts a `_` between the two.
///
/// shortcut: a fixed table, add a row for a new pair; read OANDA's account instruments if many
/// more are needed.
const PAIRS: [&str; 20] = [
    "EURUSD", "USDJPY", "GBPUSD", "USDCHF", "AUDUSD", "USDCAD", "NZDUSD", "EURJPY", "GBPJPY",
    "AUDJPY", "CADJPY", "CHFJPY", "NZDJPY", "EURGBP", "EURCHF", "EURAUD", "EURCAD", "GBPCHF",
    "GBPAUD", "AUDNZD",
];

/// OANDA charges FX through the spread, with no commission, so every fee is 0.
const FX_FEES: FeeSchedule = FeeSchedule {
    maker: 0.0,
    taker: 0.0,
    per_unit: 0.0,
};

/// OANDA's v20 REST API on a practice account: FX pairs only.
#[derive(Debug, Clone, Copy, Default)]
pub struct Oanda;

impl DataSource for Oanda {
    fn venue(&self) -> Venue {
        Venue::Oanda
    }

    fn kinds(&self) -> &'static [InstrumentKind] {
        &[InstrumentKind::Fx]
    }

    /// The fixed table: no request is made.
    fn instruments(&self, _: &Http, kind: InstrumentKind) -> Result<InstrumentList, DataError> {
        fx_only(kind)?;
        let mut list = InstrumentList::default();
        for symbol in PAIRS {
            list.push(symbol, Ok(instrument(symbol)));
        }
        Ok(list)
    }

    fn default_fees(&self, kind: InstrumentKind) -> Result<FeeSchedule, DataError> {
        fx_only(kind)?;
        Ok(FX_FEES)
    }

    /// [`history`] from the practice host, with the token read from the environment.
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
            |token, cursor| get_page(http, HOST, token, &oanda_name(&id.symbol), cursor),
        )
        .map(|(history, _)| history)
    }
}

/// [`Oanda::instruments`]' row for `symbol`: base and quote from its letters, a tick of 0.001 for
/// a JPY-quoted pair and 0.00001 for the rest, and quantities in whole units of the base currency
/// from 1, OANDA's smallest order. The minimum order value is one tick, which a unit of any pair is
/// worth far more than, so the minimum quantity is the rule that binds.
fn instrument(symbol: &str) -> Instrument {
    let (base, quote) = symbol.split_at(3);
    let tick_size = if quote == "JPY" { 0.001 } else { 0.00001 };
    Instrument {
        id: InstrumentId {
            venue: Venue::Oanda,
            symbol: symbol.to_string(),
            kind: InstrumentKind::Fx,
        },
        base: base.to_string(),
        quote: quote.to_string(),
        tick_size,
        lot_size: 1.0,
        min_qty: 1.0,
        min_notional: tick_size,
        fees: FX_FEES,
    }
}

/// OANDA's name for one of the [`PAIRS`]: `USDJPY` is `USD_JPY`.
fn oanda_name(symbol: &str) -> String {
    let (base, quote) = symbol.split_at(3);
    format!("{base}_{quote}")
}

fn fx_only(kind: InstrumentKind) -> Result<(), DataError> {
    match kind {
        InstrumentKind::Fx => Ok(()),
        InstrumentKind::Spot | InstrumentKind::Perpetual => Err(DataError::KindNotListed {
            venue: Venue::Oanda,
            kind,
        }),
    }
}

/// An OANDA API token. Neither its debug output nor any error shows it.
pub(crate) struct Token(String);

impl fmt::Debug for Token {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Token(..)")
    }
}

impl Token {
    /// Reads `OANDA_API_TOKEN` through `lookup` (the process environment outside tests), without
    /// the whitespace a copied token can carry. Unset or blank is [`DataError::MissingOandaToken`].
    fn from_lookup(lookup: impl Fn(&str) -> Option<String>) -> Result<Self, DataError> {
        lookup(TOKEN_VAR)
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
            .map(Token)
            .ok_or(DataError::MissingOandaToken)
    }

    /// `text` with every copy of the token replaced, for text an answer from OANDA brought back.
    fn scrub(&self, text: &str) -> String {
        text.replace(&self.0, "[token]")
    }
}

/// What [`history`] left out or added, reported on standard error.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct Counts {
    /// Candles whose ask was below their bid on the open, high, low or close.
    pub crossed: usize,
    /// Candles in the kind's closed hours.
    pub closed_minutes: usize,
    /// Open minutes without a candle, filled with a flat candle at the last close.
    pub filled_minutes: usize,
    /// Candles still forming (`complete: false`).
    pub incomplete: usize,
}

/// [`Oanda::minute_history`]: the bid and ask candles of `id` with open times in `[from, to)`,
/// read a page of up to 5,000 at a time from `from` on, each page starting a minute after the
/// last candle of the one before, until a page reaches `to`, holds the candle still forming, or
/// is empty.
///
/// The token is read through `env` before any request, and the call fails without it. `get`
/// sends one request for the page starting at a time and answers its status and body; a failed
/// connection, a 429 or a 5xx is tried again after each of `delays`, and any other failure fails
/// the whole call, so a month never comes back short of a page OANDA failed to serve.
///
/// A page is refused whole when it names another instrument or granularity, starts before the time
/// asked, or holds a time or a price that does not read, a candle that is not valid, or a time
/// that is not after the one before it. Then, by [`fill`]: a candle still forming, in closed
/// hours or with the ask crossed below the bid is left out and counted, and an open minute
/// without a candle is filled, but never in a run of an hour or more without one, and never at or
/// after the candle still forming, even when the local clock is ahead of OANDA's.
pub(crate) fn history(
    id: &InstrumentId,
    from: Millis,
    to: Millis,
    now: Millis,
    env: impl Fn(&str) -> Option<String>,
    delays: &[Duration],
    mut get: impl FnMut(&Token, Millis) -> Result<(u16, Vec<u8>), DataError>,
) -> Result<(MinuteHistory, Counts), DataError> {
    fx_only(id.kind)?;
    if !PAIRS.contains(&id.symbol.as_str()) {
        return Err(DataError::InvalidInstrument {
            symbol: id.symbol.clone(),
            reason: "not one of the FX pairs read from oanda".into(),
        });
    }
    let token = Token::from_lookup(env)?;
    let name = oanda_name(&id.symbol);
    let mut counts = Counts::default();
    let mut quotes = Vec::new();
    let mut previous: Option<Millis> = None;
    let mut cursor = from;
    // `to`, or the open time of the candle still forming: OANDA's clock says that minute has not
    // ended, whatever the local clock says, so nothing from it on is filled.
    let mut end = to;
    while cursor < to {
        let url = page_url(HOST, &name, cursor);
        let body = with_retries(delays, || {
            let (status, body) = get(&token, cursor)?;
            answer(&url, status, body, &token)
        })?;
        let page = parse_page(&url, &body, &name, &id.symbol)?;
        let (Some(first), Some(last)) = (page.first(), page.last()) else {
            break;
        };
        if first.bid.open_time < cursor {
            return Err(DataError::parse(
                &url,
                format!(
                    "it starts at {} UTC, before the {} UTC asked for",
                    format_minute(first.bid.open_time),
                    format_minute(cursor)
                ),
            ));
        }
        let last_time = last.bid.open_time;
        let forming = page.iter().find(|q| !q.complete).map(|q| q.bid.open_time);
        for q in page {
            if previous.is_some_and(|p| q.bid.open_time <= p) {
                return Err(DataError::parse(
                    &url,
                    format!(
                        "the candle at {} UTC is not after the one before it",
                        format_minute(q.bid.open_time)
                    ),
                ));
            }
            previous = Some(q.bid.open_time);
            if !q.complete {
                counts.incomplete += 1;
            } else if (from..to).contains(&q.bid.open_time) {
                quotes.push(q);
            }
        }
        if let Some(forming) = forming {
            end = forming.min(to);
            break;
        }
        if last_time + MINUTE >= to {
            break;
        }
        cursor = last_time + MINUTE;
    }
    let history = fill(id.kind, end, now, quotes, &mut counts);
    if counts != Counts::default() {
        eprintln!(
            "td-data: {id} {}: left out {} candles with the ask below the bid, {} in the closed \
             hours and {} still forming; filled {} open minutes without a candle at the last close",
            Month::containing(from),
            counts.crossed,
            counts.closed_minutes,
            counts.incomplete,
            counts.filled_minutes
        );
    }
    Ok((history, counts))
}

/// One minute from a page: its bid and ask candles and whether it is complete.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Quote {
    bid: Candle,
    ask: Candle,
    complete: bool,
}

/// The candles of `quotes` (complete, oldest first, inside `[_, to)`) as
/// [`crate::Store::write_quotes`] takes them, from the first one up to `to` or the last complete
/// minute before `now`, whichever is earlier:
///
/// - a candle in `kind`'s closed hours is left out and counted, and so is one whose ask is below
///   its bid on any of its four prices;
/// - an open minute with no candle is filled with a flat candle on each side at that side's last
///   close, with no volume, and counted. Before the first candle there is nothing to fill at, and
///   closed hours clear the last close, so the minutes after a weekend's open stay missing until
///   its first candle: a fill never reaches across closed hours;
/// - a run of [`CLOSURE_MINUTES`] or more open minutes with no candle, up to the next candle or
///   closed hours, is a closure or an outage, such as a market holiday: it is left missing whole,
///   so it shows in the download's gaps and in coverage, and nothing is filled again until the
///   next candle that is kept. A crossed candle still ends such a run, as OANDA was quoting then.
///
/// The minutes are open by [`InstrumentKind::is_open`], which agrees with the closed spans the
/// store's coverage counts open minutes by.
///
/// shortcut: a month's first minutes before its first candle stay missing, because each call
/// starts with nothing to fill at; carry the last close in from the month before if those gaps
/// matter.
///
/// shortcut: a run is measured only up to the end of the read (the month's end or the present), so
/// a closure that starts less than an hour before that is filled up to it. A download that runs in
/// a closure's first hour stores flat candles with no volume from the closure's start up to the
/// present, and a later download leaves them there, because [`crate::Store::write_quotes`] merges
/// and never deletes; only deleting the month and downloading it again clears them. Leave unfilled
/// a trailing run that has no candle after it if a closure ever starts in a month's last hour or a
/// download runs in a closure's first hour.
fn fill(
    kind: InstrumentKind,
    to: Millis,
    now: Millis,
    quotes: Vec<Quote>,
    counts: &mut Counts,
) -> MinuteHistory {
    let mut history = MinuteHistory::default();
    let Some(first) = quotes.first().map(|q| q.bid.open_time) else {
        return history;
    };
    let flat = |open_time, price| Candle {
        open_time,
        open: price,
        high: price,
        low: price,
        close: price,
        volume: 0.0,
        quote_volume: 0.0,
    };
    // A minute is complete once it has ended; the one forming at `now` is never filled.
    let end = to.min(now - now.rem_euclid(MINUTE));
    let mut quotes = quotes.into_iter().peekable();
    let mut last: Option<(f64, f64)> = None;
    let mut t = first;
    while t < end {
        let quote = quotes.next_if(|q| q.bid.open_time == t);
        let minute = t;
        t += MINUTE;
        if !kind.is_open(minute) {
            counts.closed_minutes += usize::from(quote.is_some());
            last = None;
            continue;
        }
        let (bid, ask) = match quote {
            Some(q) if !crossed(&q.bid, &q.ask) => (q.bid, q.ask),
            quote => {
                counts.crossed += usize::from(quote.is_some());
                // A run that stays open with no candle for `CLOSURE_MINUTES` is a closure: clearing
                // the last close at its first minute leaves all of it unfilled.
                let next = quotes.peek().map_or(end, |q| q.bid.open_time);
                if quote.is_none()
                    && last.is_some()
                    && (0..CLOSURE_MINUTES)
                        .map(|i| minute + i * MINUTE)
                        .all(|m| m < next && kind.is_open(m))
                {
                    last = None;
                }
                let Some((bid, ask)) = last else { continue };
                counts.filled_minutes += 1;
                (flat(minute, bid), flat(minute, ask))
            }
        };
        last = Some((bid.close, ask.close));
        history.candles.push(bid);
        history.asks.push(ask);
    }
    history
}

/// Whether `ask` is below `bid` on its open, high, low or close.
fn crossed(bid: &Candle, ask: &Candle) -> bool {
    ask.open < bid.open || ask.high < bid.high || ask.low < bid.low || ask.close < bid.close
}

/// The URL of the page of `instrument` (OANDA's name) starting at `from`. It holds no secret.
/// `includeFirst` is set because the spec names no default, and the candle at `from` must be in
/// the page.
fn page_url(base: &str, instrument: &str, from: Millis) -> String {
    format!(
        "{base}/v3/instruments/{instrument}/candles?price=BA&granularity=M1&count={PAGE}\
         &includeFirst=true&from={}.{:03}",
        from.div_euclid(1000),
        from.rem_euclid(1000)
    )
}

/// One request for the page of `instrument` starting at `from`, at `base` (the practice host in
/// use): its status and body. The token goes in the `Authorization` header, marked sensitive.
fn get_page(
    http: &Http,
    base: &str,
    token: &Token,
    instrument: &str,
    from: Millis,
) -> Result<(u16, Vec<u8>), DataError> {
    let mut authorization =
        HeaderValue::from_str(&format!("Bearer {}", token.0)).map_err(|_| {
            DataError::parse(
                TOKEN_VAR,
                "it holds a character an HTTP header cannot carry",
            )
        })?;
    authorization.set_sensitive(true);
    let mut headers = HeaderMap::new();
    headers.insert(AUTHORIZATION, authorization);
    headers.insert(
        HeaderName::from_static("accept-datetime-format"),
        HeaderValue::from_static("UNIX"),
    );
    http.get_with_headers(&page_url(base, instrument, from), headers)
}

/// Reads OANDA's answer for the page at `url`: its body as text, or an error that keeps only the
/// status and OANDA's `errorMessage`. Either way any copy of the token is taken out first, so no
/// error built from the answer can show it.
///
/// shortcut: a copy of the token in a 2xx body written with JSON escapes (`t...`) is not
/// found; scrub the decoded strings instead if a server ever sends one back that way.
fn answer(url: &str, status: u16, body: Vec<u8>, token: &Token) -> Result<String, DataError> {
    if (200..300).contains(&status) {
        let text = String::from_utf8(body)
            .map_err(|_| DataError::parse(url, "the answer is not UTF-8 text"))?;
        return Ok(token.scrub(&text));
    }
    #[derive(Deserialize)]
    struct ErrorBody {
        #[serde(rename = "errorMessage")]
        error_message: String,
    }
    let message = serde_json::from_slice::<ErrorBody>(&body)
        .map(|e| token.scrub(&e.error_message).chars().take(200).collect())
        .unwrap_or_default();
    let url = url.to_string();
    Err(match status {
        401 | 403 => DataError::OandaRefused {
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

/// Calls `get` until it gives anything but a passing failure (a failed connection, a 429 or a
/// 5xx), sleeping each of `delays` in turn between tries, and returns the last answer.
fn with_retries<T>(
    delays: &[Duration],
    mut get: impl FnMut() -> Result<T, DataError>,
) -> Result<T, DataError> {
    for &delay in delays {
        match get() {
            Err(e) if passing(&e) => std::thread::sleep(delay),
            answer => return answer,
        }
    }
    get()
}

fn passing(e: &DataError) -> bool {
    match e {
        DataError::Http { .. } => true,
        DataError::Status { status, .. } => *status == 429 || *status >= 500,
        _ => false,
    }
}

#[derive(Deserialize)]
struct Page {
    instrument: String,
    granularity: String,
    candles: Vec<RawCandle>,
}

#[derive(Deserialize)]
struct RawCandle {
    time: String,
    complete: bool,
    volume: u64,
    bid: RawPrices,
    ask: RawPrices,
}

#[derive(Deserialize)]
struct RawPrices {
    o: String,
    h: String,
    l: String,
    c: String,
}

/// The minutes of one page from `url`, in its order. `name` is the instrument as OANDA names it
/// and `symbol` as the desk does, for errors.
fn parse_page(url: &str, body: &str, name: &str, symbol: &str) -> Result<Vec<Quote>, DataError> {
    let page: Page = serde_json::from_str(body).map_err(|e| DataError::parse(url, e))?;
    if page.instrument != name || page.granularity != "M1" {
        return Err(DataError::parse(
            url,
            format!(
                "it holds {} {} candles, not {name} M1",
                page.instrument, page.granularity
            ),
        ));
    }
    page.candles
        .into_iter()
        .map(|raw| {
            let open_time = parse_time(&raw.time)
                .filter(|t| t.rem_euclid(MINUTE) == 0)
                .ok_or_else(|| {
                    DataError::parse(url, format!("{:?} is not a time on a minute", raw.time))
                })?;
            let side = |p: &RawPrices| -> Result<Candle, DataError> {
                let price = |text: &str| {
                    parse_price(text).ok_or_else(|| {
                        DataError::parse(
                            url,
                            format!(
                                "the candle at {} UTC has the price {text:?}",
                                format_minute(open_time)
                            ),
                        )
                    })
                };
                // The volume is OANDA's count of prices in the minute, the same on both sides.
                let candle = Candle {
                    open_time,
                    open: price(&p.o)?,
                    high: price(&p.h)?,
                    low: price(&p.l)?,
                    close: price(&p.c)?,
                    volume: raw.volume as f64,
                    quote_volume: 0.0,
                };
                candle.validate().map_err(|source| DataError::Candle {
                    instrument: format!("oanda:{symbol}"),
                    source,
                })?;
                Ok(candle)
            };
            Ok(Quote {
                bid: side(&raw.bid)?,
                ask: side(&raw.ask)?,
                complete: raw.complete,
            })
        })
        .collect()
}

/// A time in OANDA's UNIX form, seconds since the epoch with up to nine decimals, in milliseconds.
/// A time that is not on a whole second does not read.
fn parse_time(text: &str) -> Option<Millis> {
    let (seconds, fraction) = text.split_once('.').unwrap_or((text, ""));
    let digits = |s: &str| s.bytes().all(|b| b.is_ascii_digit());
    if seconds.is_empty() || !digits(seconds) || fraction.len() > 9 {
        return None;
    }
    if !fraction.bytes().all(|b| b == b'0') {
        return None;
    }
    seconds.parse::<Millis>().ok()?.checked_mul(1000)
}

/// A price as OANDA writes it: digits with at most one decimal point.
fn parse_price(text: &str) -> Option<f64> {
    let plain = !text.is_empty()
        && text.bytes().all(|b| b.is_ascii_digit() || b == b'.')
        && text.bytes().filter(|&b| b == b'.').count() <= 1;
    if plain { text.parse().ok() } else { None }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use serde_json::{Value, json};

    use super::*;
    use crate::Store;
    use crate::download::download_candles_from;
    use crate::http::stub;
    use crate::time::{DAY, parse_date};

    const HOUR: Millis = 60 * MINUTE;
    const NO_WAIT: [Duration; 4] = [Duration::ZERO; 4];
    const TOKEN: &str = "test-token-0123456789abcdef";

    fn usdjpy() -> InstrumentId {
        InstrumentId {
            venue: Venue::Oanda,
            symbol: "USDJPY".into(),
            kind: InstrumentKind::Fx,
        }
    }

    fn env(name: &str) -> Option<String> {
        (name == TOKEN_VAR).then(|| TOKEN.to_string())
    }

    fn far_future() -> Millis {
        parse_date("2030-01-01").expect("a date")
    }

    /// One candle as OANDA sends it: the bid moves with the minute in thousandths and the ask is 7
    /// points above it on every price.
    fn raw(t: Millis, complete: bool) -> Value {
        let i = t / MINUTE;
        let bid = 119.0 + (i % 37) as f64 * 0.01;
        let prices = |p: f64| {
            json!({
                "o": format!("{p:.3}"),
                "h": format!("{:.3}", p + 0.004),
                "l": format!("{:.3}", p - 0.003),
                "c": format!("{:.3}", p + 0.001),
            })
        };
        json!({
            "complete": complete,
            "volume": 25,
            "time": format!("{}.000000000", t / 1000),
            "bid": prices(bid),
            "ask": prices(bid + 0.007),
        })
    }

    fn page(candles: &[Value]) -> Vec<u8> {
        json!({ "instrument": "USD_JPY", "granularity": "M1", "candles": candles })
            .to_string()
            .into_bytes()
    }

    /// A fake OANDA serving `candles` (oldest first, each with a `time`): a page is the first
    /// 5,000 at or after the time asked. Each time asked is noted in `asked`.
    fn serve<'a>(
        candles: &'a [Value],
        asked: &'a RefCell<Vec<Millis>>,
    ) -> impl FnMut(&Token, Millis) -> Result<(u16, Vec<u8>), DataError> + 'a {
        move |token, from| {
            assert_eq!(token.0, TOKEN);
            asked.borrow_mut().push(from);
            // A time that does not read is served as if at the time asked.
            let time = |c: &Value| c["time"].as_str().and_then(parse_time).unwrap_or(from);
            let start = candles.partition_point(|c| time(c) < from);
            let end = (start + PAGE).min(candles.len());
            Ok((200, page(&candles[start..end])))
        }
    }

    /// Every open minute in `[from, to)` except `missing`, as complete candles.
    fn open_minutes(from: Millis, to: Millis, missing: &[Millis]) -> Vec<Value> {
        (from / MINUTE..to / MINUTE)
            .map(|i| i * MINUTE)
            .filter(|&t| InstrumentKind::Fx.is_open(t) && !missing.contains(&t))
            .map(|t| raw(t, true))
            .collect()
    }

    fn times(candles: &[Candle]) -> Vec<Millis> {
        candles.iter().map(|c| c.open_time).collect()
    }

    #[test]
    fn a_month_is_read_page_by_page_and_trimmed_at_its_end() {
        let (jan, feb) = (
            parse_date("2016-01-01").expect("a date"),
            parse_date("2016-02-01").expect("a date"),
        );
        // From Monday 4 January to past the month's end, with three quiet minutes.
        let monday = parse_date("2016-01-04").expect("a date");
        let quiet = [
            monday + 9 * HOUR,
            monday + 9 * HOUR + MINUTE,
            feb - 5 * MINUTE,
        ];
        let served = open_minutes(monday, feb + 3 * DAY, &quiet);
        let asked = RefCell::new(Vec::new());
        let (history, counts) = history(
            &usdjpy(),
            jan,
            feb,
            far_future(),
            env,
            &NO_WAIT,
            serve(&served, &asked),
        )
        .expect("reads");

        // Every open minute from the first candle to the month's end: the quiet ones filled.
        let expected: Vec<Millis> = (monday / MINUTE..feb / MINUTE)
            .map(|i| i * MINUTE)
            .filter(|&t| InstrumentKind::Fx.is_open(t))
            .collect();
        assert_eq!(times(&history.candles), expected);
        assert_eq!(times(&history.asks), expected);
        assert_eq!(
            counts,
            Counts {
                filled_minutes: 3,
                ..Counts::default()
            }
        );
        // The pages: the first from `from`, each next from a minute after the last candle of the
        // one before, and the last one reaching into February, which is left out.
        let in_january = served.len() - open_minutes(feb, feb + 3 * DAY, &[]).len();
        let pages = in_january.div_ceil(PAGE);
        assert!(pages > 1);
        let asked = asked.borrow();
        assert_eq!(asked.len(), pages);
        assert_eq!(asked[0], jan);
        let time = |c: &Value| parse_time(c["time"].as_str().expect("a time")).expect("reads");
        for (k, &from) in asked.iter().enumerate().skip(1) {
            assert_eq!(from, time(&served[k * PAGE - 1]) + MINUTE);
        }
        // The prices are OANDA's, each ask 7 points above its bid.
        let first = &history.candles[0];
        let price = format!("{:.3}", 119.0 + ((monday / MINUTE) % 37) as f64 * 0.01);
        assert_eq!(first.open, price.parse::<f64>().expect("a price"));
        assert_eq!(first.volume, 25.0);
        for (bid, ask) in history.candles.iter().zip(&history.asks) {
            assert!(
                (ask.close - bid.close - 0.007).abs() < 1e-9,
                "{bid:?} {ask:?}"
            );
        }
    }

    #[test]
    fn the_candle_still_forming_is_dropped_and_ends_the_read() {
        let monday = parse_date("2024-01-08").expect("a date");
        let forming = monday + 10 * HOUR;
        let mut served = open_minutes(monday + 9 * HOUR, forming, &[forming - MINUTE]);
        served.push(raw(forming, false));
        // The local clock agreeing with OANDA's, and two minutes ahead of it: either way neither
        // the forming minute nor any after it is filled.
        for now in [forming + 30_000, forming + 2 * MINUTE + 30_000] {
            let asked = RefCell::new(Vec::new());
            let (history, counts) = history(
                &usdjpy(),
                monday,
                now,
                now,
                env,
                &NO_WAIT,
                serve(&served, &asked),
            )
            .expect("reads");
            assert_eq!(counts.incomplete, 1);
            // The minute before it had no candle and is filled; the forming minute is not.
            assert_eq!(
                history.candles.last().map(|c| c.open_time),
                Some(forming - MINUTE),
                "now {now}"
            );
            assert_eq!(history.candles.last().map(|c| c.volume), Some(0.0));
            assert_eq!(counts.filled_minutes, 1);
            assert_eq!((history.candles.len(), history.asks.len()), (60, 60));
            assert_eq!(*asked.borrow(), [monday]);
        }
    }

    #[test]
    fn a_gap_is_filled_in_open_hours_but_never_across_the_weekend() {
        // Friday 8 January 2016: New York standard time, so the market closes at 22:00 UTC and
        // opens again on Sunday at 22:00 UTC.
        let friday = parse_date("2016-01-08").expect("a date");
        let sunday = friday + 2 * DAY;
        let close = friday + 22 * HOUR;
        let open = sunday + 22 * HOUR;
        // Friday's last two minutes and Sunday's first three are quiet; a candle on Saturday is
        // in closed hours.
        let mut served = open_minutes(friday + 20 * HOUR, close - 2 * MINUTE, &[]);
        served.push(raw(friday + DAY + 3 * HOUR, true));
        served.extend(open_minutes(open + 3 * MINUTE, open + HOUR, &[]));
        let asked = RefCell::new(Vec::new());
        let (history, counts) = history(
            &usdjpy(),
            friday,
            open + HOUR,
            far_future(),
            env,
            &NO_WAIT,
            serve(&served, &asked),
        )
        .expect("reads");

        let t = times(&history.candles);
        // Friday's quiet minutes are filled at the last close, up to the close and no further.
        let friday_minutes = 120;
        assert_eq!(t[..friday_minutes].first(), Some(&(friday + 20 * HOUR)));
        assert_eq!(t[friday_minutes - 1], close - MINUTE);
        let last_real = history.candles[friday_minutes - 3];
        for c in &history.candles[friday_minutes - 2..friday_minutes] {
            assert_eq!(
                (c.open, c.low, c.volume),
                (last_real.close, last_real.close, 0.0)
            );
        }
        // Nothing in the weekend, and Sunday starts at its first candle, not at the open.
        assert_eq!(t[friday_minutes], open + 3 * MINUTE);
        assert_eq!(t.len(), friday_minutes + 57);
        assert_eq!(
            counts,
            Counts {
                closed_minutes: 1,
                filled_minutes: 2,
                ..Counts::default()
            }
        );
    }

    #[test]
    fn a_quiet_hour_is_left_missing_whole_and_filling_starts_again_at_the_next_candle() {
        // Monday 8 January 2024 from 09:00 UTC: 59 quiet minutes, a candle at 10:00, 60 quiet
        // minutes, a candle at 11:01, then 2 quiet minutes.
        let nine = parse_date("2024-01-08").expect("a date") + 9 * HOUR;
        let minutes = |from: Millis, n: i64| (0..n).map(move |i| from + i * MINUTE);
        let (short, long, resumed) = (nine + MINUTE, nine + HOUR + MINUTE, nine + 122 * MINUTE);
        let quiet: Vec<Millis> = minutes(short, 59)
            .chain(minutes(long, 60))
            .chain(minutes(resumed, 2))
            .collect();
        let to = nine + 3 * HOUR;
        let served = open_minutes(nine, to, &quiet);
        let asked = RefCell::new(Vec::new());
        let (history, counts) = history(
            &usdjpy(),
            nine,
            to,
            far_future(),
            env,
            &NO_WAIT,
            serve(&served, &asked),
        )
        .expect("reads");

        // The hour is left out whole; the 59 minutes before it and the 2 after it are filled.
        let expected: Vec<Millis> = minutes(nine, 180)
            .filter(|t| !(long..long + HOUR).contains(t))
            .collect();
        assert_eq!(times(&history.candles), expected);
        assert_eq!(times(&history.asks), expected);
        assert_eq!(
            counts,
            Counts {
                filled_minutes: 61,
                ..Counts::default()
            }
        );
        // Each filled minute is flat at the close of the candle before its stretch.
        let at = |t: Millis| {
            let i = history.candles.iter().position(|c| c.open_time == t);
            &history.candles[i.expect("stored")]
        };
        for (stretch, n, before) in [(short, 59, nine), (resumed, 2, long + HOUR)] {
            let close = at(before).close;
            for t in minutes(stretch, n) {
                assert_eq!((at(t).open, at(t).low, at(t).volume), (close, close, 0.0));
            }
        }
    }

    #[test]
    fn a_crossed_candle_ends_a_quiet_run() {
        // A candle at 09:00, a crossed one at 09:01, 59 quiet minutes, then a candle at 10:01:
        // OANDA was quoting at 09:01, so the run without a candle is 59 minutes and is filled.
        let nine = parse_date("2024-01-08").expect("a date") + 9 * HOUR;
        let quiet: Vec<Millis> = (2..61).map(|i| nine + i * MINUTE).collect();
        let mut served = open_minutes(nine, nine + 2 * HOUR, &quiet);
        served[1]["ask"]["l"] = json!("118.000");
        let asked = RefCell::new(Vec::new());
        let (history, counts) = history(
            &usdjpy(),
            nine,
            nine + 2 * HOUR,
            far_future(),
            env,
            &NO_WAIT,
            serve(&served, &asked),
        )
        .expect("reads");
        assert_eq!(history.candles.len(), 120);
        assert_eq!(
            counts,
            Counts {
                crossed: 1,
                filled_minutes: 60,
                ..Counts::default()
            }
        );
    }

    #[test]
    fn a_crossed_candle_is_left_out_and_its_minute_filled() {
        let monday = parse_date("2024-01-08").expect("a date") + 9 * HOUR;
        let mut served = open_minutes(monday, monday + 3 * MINUTE, &[]);
        served[1]["ask"]["l"] = json!("118.000");
        let asked = RefCell::new(Vec::new());
        let (history, counts) = history(
            &usdjpy(),
            monday,
            monday + 3 * MINUTE,
            far_future(),
            env,
            &NO_WAIT,
            serve(&served, &asked),
        )
        .expect("reads");
        assert_eq!(counts.crossed, 1);
        assert_eq!(counts.filled_minutes, 1);
        assert_eq!(history.candles[1].volume, 0.0);
        assert_eq!(history.candles[1].open, history.candles[0].close);
        assert_eq!(history.asks[1].low, history.asks[0].close);
    }

    #[test]
    fn a_page_that_does_not_read_or_goes_back_in_time_is_refused() {
        let monday = parse_date("2024-01-08").expect("a date") + 9 * HOUR;
        let good = open_minutes(monday, monday + 3 * MINUTE, &[]);
        let read = |candles: Vec<Value>| {
            let asked = RefCell::new(Vec::new());
            history(
                &usdjpy(),
                monday,
                monday + 3 * MINUTE,
                far_future(),
                env,
                &NO_WAIT,
                serve(&candles, &asked),
            )
            .map(|_| ())
        };
        let refused = |candles: Vec<Value>, says: &str| {
            let error = read(candles).expect_err("refused");
            assert!(error.to_string().contains(says), "{error}");
        };
        let with = |i: usize, field: &str, side: &str, value: Value| {
            let mut c = good.clone();
            c[i][side][field] = value;
            c
        };
        refused(with(1, "c", "bid", json!("1.2.3")), "\"1.2.3\"");
        refused(with(1, "o", "ask", json!("-119.1")), "\"-119.1\"");
        refused(with(1, "h", "bid", json!("NaN")), "\"NaN\"");
        refused(with(1, "h", "bid", json!(119.5)), "cannot parse");
        // A high below the close is not a candle.
        refused(with(1, "h", "bid", json!("100.000")), "high is below");
        let mut c = good.clone();
        c[1]["time"] = json!("2024-01-08T09:01:00.000000000Z");
        refused(c, "not a time on a minute");
        let mut c = good.clone();
        c[1]["time"] = json!(format!("{}.000000000", (monday + 61_000) / 1000));
        refused(c, "not a time on a minute");
        let mut c = good.clone();
        c[2] = c[1].clone();
        refused(c, "not after the one before");
        let mut c = good.clone();
        c.swap(1, 2);
        refused(c, "not after the one before");
        // A page that starts before the time asked, as from a server that ignores `from`.
        let early = open_minutes(monday - 2 * MINUTE, monday + 3 * MINUTE, &[]);
        let mut ignores_from = |_: &Token, _| Ok((200, page(&early)));
        let result = history(
            &usdjpy(),
            monday,
            monday + 3 * MINUTE,
            far_future(),
            env,
            &NO_WAIT,
            &mut ignores_from,
        );
        let says = "it starts at 2024-01-08 08:58 UTC, before the 2024-01-08 09:00 UTC asked for";
        assert!(
            matches!(&result, Err(e) if e.to_string().contains(says)),
            "{result:?}"
        );

        let mut wrong = |_: &Token, _| {
            Ok((
                200,
                json!({ "instrument": "EUR_USD", "granularity": "M1", "candles": [] })
                    .to_string()
                    .into_bytes(),
            ))
        };
        let result = history(
            &usdjpy(),
            monday,
            monday + DAY,
            far_future(),
            env,
            &NO_WAIT,
            &mut wrong,
        );
        assert!(result.is_err_and(|e| e.to_string().contains("EUR_USD M1 candles, not USD_JPY")),);
    }

    /// Every error built from a page's text (serde's, the instrument or granularity, a time, a
    /// price) shows `[token]` where the page held the token.
    #[test]
    fn a_page_that_does_not_read_never_shows_the_token() {
        let monday = parse_date("2024-01-08").expect("a date") + 9 * HOUR;
        let good = open_minutes(monday, monday + 3 * MINUTE, &[]);
        let candles_with = |field: &str, side: Option<&str>| {
            let mut c = good.clone();
            match side {
                Some(side) => c[1][side][field] = json!(TOKEN),
                None => c[1][field] = json!(TOKEN),
            }
            c
        };
        let good_page = json!({ "instrument": "USD_JPY", "granularity": "M1", "candles": good });
        let with = |key: &str, value: Value| {
            let mut p = good_page.clone();
            p[key] = value;
            p
        };
        let bodies = [
            with("instrument", json!(format!("USD_JPY {TOKEN}"))),
            with("granularity", json!(TOKEN)),
            with("candles", json!(candles_with("time", None))),
            with("candles", json!(candles_with("o", Some("bid")))),
            // A string where serde wants a number, which serde's own error quotes.
            with("candles", json!(candles_with("volume", None))),
        ];
        for body in bodies {
            let body = body.to_string().into_bytes();
            let mut get = |_: &Token, _| Ok((200, body.clone()));
            let error = history(
                &usdjpy(),
                monday,
                monday + 3 * MINUTE,
                far_future(),
                env,
                &NO_WAIT,
                &mut get,
            )
            .expect_err("refused");
            let shown = format!("{error} {error:?}");
            assert!(!shown.contains(TOKEN), "{shown}");
            assert!(shown.contains("[token]"), "{shown}");
        }
        // A body that is not UTF-8 is not JSON, and is refused before it is read.
        let token = Token(TOKEN.into());
        let error = answer("u", 200, vec![b'{', 0xff, b'}'], &token).expect_err("refused");
        assert!(error.to_string().contains("not UTF-8"), "{error}");
    }

    #[test]
    fn times_and_prices_read_only_in_oandas_forms() {
        assert_eq!(parse_time("1452055800.000000000"), Some(1_452_055_800_000));
        assert_eq!(parse_time("1452055800"), Some(1_452_055_800_000));
        for bad in [
            "",
            ".0",
            "1452055800.5",
            "-60.0",
            "1e9",
            "1452055800.0000000000",
        ] {
            assert_eq!(parse_time(bad), None, "{bad}");
        }
        assert_eq!(parse_price("119.273"), Some(119.273));
        for bad in ["", "1.2.3", "-1.0", "1e3", "inf", " 1.0"] {
            assert_eq!(parse_price(bad), None, "{bad}");
        }
        let monday = parse_date("2024-01-08").expect("a date");
        assert_eq!(
            page_url("https://h", "USD_JPY", monday + 61_234),
            "https://h/v3/instruments/USD_JPY/candles?price=BA&granularity=M1&count=5000\
             &includeFirst=true&from=1704672061.234"
        );
        assert_eq!(oanda_name("USDJPY"), "USD_JPY");
    }

    #[test]
    fn a_401_is_the_token_error_keeps_only_the_message_and_is_not_retried() {
        let asked = RefCell::new(0);
        let mut refuse = |_: &Token, _| {
            *asked.borrow_mut() += 1;
            let body = json!({
                "errorMessage": format!("Insufficient authorization to perform request. {TOKEN}"),
                "headers": { "Authorization": format!("Bearer {TOKEN}") },
            });
            Ok((401, body.to_string().into_bytes()))
        };
        let monday = parse_date("2024-01-08").expect("a date");
        let error = history(
            &usdjpy(),
            monday,
            monday + DAY,
            far_future(),
            env,
            &NO_WAIT,
            &mut refuse,
        )
        .expect_err("refused");
        let shown = format!("{error} {error:?}");
        assert!(
            matches!(&error, DataError::OandaRefused { status: 401, message, .. }
                if message == "Insufficient authorization to perform request. [token]"),
            "{shown}"
        );
        assert!(shown.contains("OANDA_API_TOKEN"), "{shown}");
        assert!(!shown.contains(TOKEN), "{shown}");
        assert!(!shown.contains("Bearer"), "{shown}");
        assert_eq!(*asked.borrow(), 1);

        // A 403 is the same error; a 400 is OANDA's message under its status; a body that is not
        // OANDA's error shape keeps nothing.
        let token = Token(TOKEN.into());
        let forbidden = answer("u", 403, br#"{"errorMessage":"no"}"#.to_vec(), &token);
        assert!(matches!(
            forbidden,
            Err(DataError::OandaRefused { status: 403, .. })
        ));
        let bad = answer(
            "u",
            400,
            br#"{"errorMessage":"Invalid value specified for 'from'"}"#.to_vec(),
            &token,
        );
        assert!(
            matches!(&bad, Err(DataError::Status { status: 400, body, .. }) if body == "Invalid value specified for 'from'")
        );
        let odd = answer(
            "u",
            502,
            format!("<html>{TOKEN}</html>").into_bytes(),
            &token,
        );
        assert!(
            matches!(&odd, Err(DataError::Status { status: 502, body, .. }) if body.is_empty())
        );
    }

    #[test]
    fn without_a_token_the_source_fails_before_any_request() {
        let monday = parse_date("2024-01-08").expect("a date");
        let envs: [fn(&str) -> Option<String>; 2] = [|_| None, |_| Some("  \n".to_string())];
        for env in envs {
            let asked = RefCell::new(Vec::new());
            let result = history(
                &usdjpy(),
                monday,
                monday + DAY,
                far_future(),
                env,
                &NO_WAIT,
                serve(&[], &asked),
            );
            let message = result.map(|_| ()).map_err(|e| e.to_string());
            assert_eq!(
                message,
                Err(
                    "oanda's history needs an API token: set OANDA_API_TOKEN to the token of an \
                     OANDA practice account"
                        .into()
                )
            );
            assert!(asked.borrow().is_empty());
        }
        // A token copied with a trailing newline is read without it.
        let token = Token::from_lookup(|_| Some(format!("{TOKEN}\n"))).expect("a token");
        assert_eq!(token.0, TOKEN);
        assert_eq!(format!("{token:?}"), "Token(..)");
    }

    #[test]
    fn a_429_or_5xx_is_retried_and_one_that_persists_fails_the_month() {
        let monday = parse_date("2024-01-08").expect("a date");
        let calls = RefCell::new(0);
        let mut busy = |_: &Token, _| {
            *calls.borrow_mut() += 1;
            Ok((503, br#"{"errorMessage":"busy"}"#.to_vec()))
        };
        let result = history(
            &usdjpy(),
            monday,
            monday + DAY,
            far_future(),
            env,
            &NO_WAIT,
            &mut busy,
        );
        assert!(
            matches!(&result, Err(DataError::Status { status: 503, body, .. }) if body == "busy"),
            "{result:?}"
        );
        assert_eq!(*calls.borrow(), 5);

        let token = Token(TOKEN.into());
        let mut calls = 0;
        let result = with_retries(&NO_WAIT, || {
            calls += 1;
            let status = if calls < 3 { 429 } else { 200 };
            answer("u", status, b"[]".to_vec(), &token)
        });
        assert_eq!((result.ok(), calls), (Some("[]".to_string()), 3));
    }

    /// The request [`get_page`] sends, read back by a local one-shot server: the path and query,
    /// the bearer token in the `Authorization` header and nowhere else, UNIX times, and gzip.
    #[test]
    fn a_page_request_carries_the_token_only_in_its_authorization_header() {
        let http = Http::new().expect("an HTTP client");
        let (base, server) = stub::serve_once(&[], page(&[]));
        let token = Token(TOKEN.into());
        let monday = parse_date("2024-01-08").expect("a date");
        let (status, body) = get_page(&http, &base, &token, "USD_JPY", monday).expect("an answer");
        assert_eq!((status, body), (200, page(&[])));
        // The server lower-cases what it read.
        let request = server.join().expect("the server");
        let lower = TOKEN.to_lowercase();
        let first = request.lines().next().expect("a request line");
        assert_eq!(
            first,
            "get /v3/instruments/usd_jpy/candles?price=ba&granularity=m1&count=5000\
             &includefirst=true&from=1704672000.000 http/1.1"
        );
        let lines: Vec<&str> = request.lines().filter(|l| l.contains(&lower)).collect();
        assert_eq!(lines, [format!("authorization: bearer {lower}")]);
        assert!(
            request.lines().any(|l| l == "accept-datetime-format: unix"),
            "{request}"
        );
        let accepts = request
            .lines()
            .find_map(|l| l.strip_prefix("accept-encoding: "))
            .unwrap_or_default();
        assert!(accepts.contains("gzip"), "{request}");

        // A failed connection's error names the URL, never the token.
        let closed = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let base = format!("http://{}", closed.local_addr().expect("an address"));
        drop(closed);
        let error = get_page(&http, &base, &token, "USD_JPY", monday).expect_err("no server");
        let shown = format!("{error} {error:?}");
        assert!(matches!(error, DataError::Http { .. }), "{shown}");
        assert!(!shown.contains(TOKEN), "{shown}");
    }

    #[test]
    fn only_fx_is_listed_with_no_funding_and_no_live_stream() {
        let http = Http::new().expect("an HTTP client");
        let source = crate::source(Venue::Oanda);
        assert_eq!(source.venue(), Venue::Oanda);
        assert_eq!(source.kinds(), [InstrumentKind::Fx]);
        for kind in [InstrumentKind::Spot, InstrumentKind::Perpetual] {
            let id = InstrumentId { kind, ..usdjpy() };
            let refused =
                |e: DataError| e.to_string() == format!("oanda does not list {kind} instruments");
            assert!(source.default_fees(kind).is_err_and(refused));
            assert!(source.instruments(&http, kind).is_err_and(refused));
            assert!(
                source
                    .minute_history(&http, &id, 0, MINUTE, DAY)
                    .is_err_and(refused)
            );
        }
        let fees = source.default_fees(InstrumentKind::Fx).expect("fees");
        assert_eq!((fees.maker, fees.taker, fees.per_unit), (0.0, 0.0, 0.0));
        assert!(matches!(
            source.funding(&http, &usdjpy(), 0, DAY),
            Err(DataError::NoFunding(Venue::Oanda))
        ));
        assert!(matches!(
            source.live(),
            Err(DataError::NoLiveStream(Venue::Oanda))
        ));
        // A pair outside the table is refused before the token is read or any request.
        let gold = InstrumentId {
            symbol: "XAUUSD".into(),
            ..usdjpy()
        };
        let refused = source.minute_history(&http, &gold, 0, MINUTE, DAY);
        assert!(
            matches!(&refused, Err(DataError::InvalidInstrument { symbol, .. }) if symbol == "XAUUSD"),
            "{refused:?}"
        );
    }

    #[test]
    fn the_pair_table_has_ticks_by_quote_currency_and_one_unit_minimums() {
        let http = Http::new().expect("an HTTP client");
        let list =
            crate::fetch_instruments(&http, Venue::Oanda, InstrumentKind::Fx).expect("the table");
        assert!(list.rejected.is_empty(), "{:?}", list.rejected);
        assert_eq!(list.instruments.len(), 20);
        let find = |symbol: &str| {
            list.instruments
                .iter()
                .find(|i| i.id.symbol == symbol)
                .unwrap_or_else(|| panic!("{symbol}"))
                .clone()
        };
        let jpy = find("USDJPY");
        assert_eq!((jpy.base.as_str(), jpy.quote.as_str()), ("USD", "JPY"));
        assert_eq!(jpy.tick_size, 0.001);
        assert_eq!(find("GBPJPY").tick_size, 0.001);
        let eur = find("EURUSD");
        assert_eq!((eur.base.as_str(), eur.quote.as_str()), ("EUR", "USD"));
        assert_eq!(eur.tick_size, 0.00001);
        for i in &list.instruments {
            assert_eq!((i.lot_size, i.min_qty), (1.0, 1.0), "{}", i.id);
            assert_eq!((i.id.venue, i.id.kind), (Venue::Oanda, InstrumentKind::Fx));
        }
        assert_eq!(
            jpy.round_price(150.1234, td_core::Rounding::Nearest),
            150.123
        );
        assert_eq!(
            eur.round_price(1.098_716, td_core::Rounding::Nearest),
            1.09872
        );
    }

    /// [`history`] on a fake OANDA as a source, for the download path.
    struct FakeOanda(Vec<Value>);

    impl DataSource for FakeOanda {
        fn venue(&self) -> Venue {
            Venue::Oanda
        }

        fn kinds(&self) -> &'static [InstrumentKind] {
            Oanda.kinds()
        }

        fn instruments(
            &self,
            http: &Http,
            kind: InstrumentKind,
        ) -> Result<InstrumentList, DataError> {
            Oanda.instruments(http, kind)
        }

        fn default_fees(&self, kind: InstrumentKind) -> Result<FeeSchedule, DataError> {
            Oanda.default_fees(kind)
        }

        fn minute_history(
            &self,
            _: &Http,
            id: &InstrumentId,
            from: Millis,
            to: Millis,
            now: Millis,
        ) -> Result<MinuteHistory, DataError> {
            let asked = RefCell::new(Vec::new());
            history(id, from, to, now, env, &NO_WAIT, serve(&self.0, &asked)).map(|(h, _)| h)
        }
    }

    #[test]
    fn a_download_stores_bids_with_asks_across_months_and_the_weekend_is_no_gap() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let store = Store::new(dir.path());
        let http = Http::new().expect("an HTTP client");
        // Thursday 28 January to Wednesday 3 February 2016, with one quiet minute on Monday and
        // Sunday's first candle at the 22:00 open.
        let from = parse_date("2016-01-28").expect("a date");
        let to = from + 7 * DAY;
        let feb = parse_date("2016-02-01").expect("a date");
        let quiet = feb + 10 * HOUR;
        let served = open_minutes(from, to, &[quiet]);
        let id = usdjpy();
        let report = download_candles_from(
            &FakeOanda(served.clone()),
            &http,
            &store,
            &id,
            from,
            to,
            &mut |_| {},
        )
        .expect("downloads");

        let open: Vec<Millis> = (from / MINUTE..to / MINUTE)
            .map(|i| i * MINUTE)
            .filter(|&t| InstrumentKind::Fx.is_open(t))
            .collect();
        let (jan, febm) = (Month::containing(from), Month::containing(feb));
        let in_jan = open.iter().filter(|&&t| t < feb).count();
        assert_eq!(report.months, [(jan, in_jan), (febm, open.len() - in_jan)]);
        assert!(report.missing_files.is_empty());
        assert_eq!(report.gaps, []);

        let mut bids = store.read_month(&id, jan).expect("bids");
        bids.extend(store.read_month(&id, febm).expect("bids"));
        assert_eq!(times(&bids), open);
        let filled = bids.iter().find(|c| c.open_time == quiet).expect("filled");
        assert_eq!(filled.volume, 0.0);
        let asks = store.read_month_asks(&id, febm).expect("asks");
        assert_eq!(asks.len(), open.len() - in_jan);
        for (bid, ask) in bids[in_jan..].iter().zip(&asks) {
            assert_eq!(bid.open_time, ask.open_time);
            assert!((ask.close - bid.close - 0.007).abs() < 1e-9);
        }

        // The store's coverage counts the same open minutes: none missing in either month.
        let coverage = store.coverage(&id).expect("coverage");
        assert_eq!(coverage.len(), 2);
        assert_eq!(coverage[0].first, from);
        assert_eq!(coverage[1].minutes, open.len() - in_jan);
        assert!(coverage.iter().all(|c| c.gaps.is_empty()), "{coverage:?}");
        let path = store.candle_path(&id, jan).expect("a path");
        let relative = path.strip_prefix(dir.path()).expect("inside the store");
        assert!(
            relative.starts_with(std::path::Path::new("candles").join("oanda").join("fx")),
            "{}",
            relative.display()
        );
    }
}
