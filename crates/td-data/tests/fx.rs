//! A market with a bid and an ask price that closes at weekends (FX), in a temp directory: gaps and
//! resampling by its trading hours, and its asks in the store.

use std::fs;
use std::path::{Path, PathBuf};

use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
use td_core::{Candle, InstrumentId, InstrumentKind, Millis, Timeframe, Venue};
use td_data::time::{DAY, MINUTE, Month, parse_date};
use td_data::{DataError, Gap, Store, StoredMonth, WriteReport, missing_minutes, resample};

const HOUR: Millis = 60 * MINUTE;

fn usdjpy() -> InstrumentId {
    InstrumentId {
        venue: Venue::Binance,
        symbol: "USDJPY".into(),
        kind: InstrumentKind::Fx,
    }
}

fn btc_spot() -> InstrumentId {
    InstrumentId {
        venue: Venue::Binance,
        symbol: "BTCUSDT".into(),
        kind: InstrumentKind::Spot,
    }
}

/// Monday 2024-01-08 00:00 UTC. That week is in New York standard time, so the FX weekend runs from
/// Friday 2024-01-12 22:00 UTC to Sunday 2024-01-14 22:00 UTC.
fn monday() -> Millis {
    parse_date("2024-01-08").expect("a date")
}

fn friday_close() -> Millis {
    monday() + 4 * DAY + 22 * HOUR
}

fn sunday_open() -> Millis {
    monday() + 6 * DAY + 22 * HOUR
}

/// A bid minute at `t` whose prices move with the minute, so each bar's OHLC is its own.
fn bid(t: Millis) -> Candle {
    let i = (t / MINUTE) as f64;
    let open = 150.0 + (i % 37.0) * 0.01;
    let close = 150.0 + ((i + 11.0) % 37.0) * 0.01;
    Candle {
        open_time: t,
        open,
        high: open.max(close) + 0.004,
        low: open.min(close) - 0.003,
        close,
        volume: 1.0,
        quote_volume: 150.0,
    }
}

/// The ask of [`bid`]'s minute: the bid plus a spread that moves with the minute.
fn ask(t: Millis) -> Candle {
    let b = bid(t);
    let spread = |k: i64| 0.01 + ((t / MINUTE + k) % 5) as f64 * 0.002;
    Candle {
        open: b.open + spread(0),
        high: b.high + spread(1).max(spread(0)).max(spread(2)),
        low: b.low + spread(3).min(spread(0)).min(spread(2)),
        close: b.close + spread(2),
        volume: 0.0,
        quote_volume: 0.0,
        ..b
    }
}

/// The open times of the week from [`monday`] with the weekend absent.
fn fx_week() -> Vec<Millis> {
    (0..7 * DAY / MINUTE)
        .map(|i| monday() + i * MINUTE)
        .filter(|&t| InstrumentKind::Fx.is_open(t))
        .collect()
}

fn bids(times: &[Millis]) -> Vec<Candle> {
    times.iter().copied().map(bid).collect()
}

#[test]
fn an_fx_week_without_its_weekend_has_no_gap_but_a_missing_wednesday_minute_is_one() {
    let (from, to) = (monday(), monday() + 7 * DAY);
    let week = bids(&fx_week());
    assert_eq!(
        week.len() as i64,
        (5 * DAY) / MINUTE,
        "the weekend is 48 hours"
    );
    assert_eq!(missing_minutes(InstrumentKind::Fx, &week, from, to), []);

    let wednesday = monday() + 2 * DAY + 12 * HOUR;
    let holed: Vec<Candle> = week
        .iter()
        .copied()
        .filter(|c| c.open_time != wednesday)
        .collect();
    assert_eq!(
        missing_minutes(InstrumentKind::Fx, &holed, from, to),
        [Gap {
            from: wednesday,
            to: wednesday + MINUTE
        }]
    );

    // The same data as spot, which never closes: the weekend is missing.
    let weekend = Gap {
        from: friday_close(),
        to: sunday_open(),
    };
    assert_eq!(
        missing_minutes(InstrumentKind::Spot, &week, from, to),
        [weekend]
    );
    assert_eq!(
        missing_minutes(InstrumentKind::Spot, &holed, from, to),
        [
            Gap {
                from: wednesday,
                to: wednesday + MINUTE
            },
            weekend
        ]
    );
}

#[test]
fn a_gap_running_into_the_weekend_ends_at_the_close_and_one_out_of_it_starts_at_the_open() {
    let (from, to) = (monday(), monday() + 7 * DAY);
    // Friday from 20:00 UTC and Sunday until 23:00 UTC are missing too.
    let (friday_eight, sunday_eleven) = (friday_close() - 2 * HOUR, sunday_open() + HOUR);
    let holed: Vec<Candle> = bids(&fx_week())
        .into_iter()
        .filter(|c| !(friday_eight..sunday_eleven).contains(&c.open_time))
        .collect();
    assert_eq!(
        missing_minutes(InstrumentKind::Fx, &holed, from, to),
        [
            Gap {
                from: friday_eight,
                to: friday_close()
            },
            Gap {
                from: sunday_open(),
                to: sunday_eleven
            }
        ]
    );
}

#[test]
fn four_hour_buckets_across_the_close_and_the_open_are_complete_and_the_weekend_has_none() {
    let week = bids(&fx_week());
    let loaded = resample(InstrumentKind::Fx, &week, Timeframe::H4, false);
    assert_eq!(loaded.dropped, [] as [Millis; 0], "no bucket is dropped");
    assert_eq!(loaded.tolerated, []);
    let opens: Vec<Millis> = loaded.candles.iter().map(|c| c.open_time).collect();
    // Monday to Friday 20:00 and Sunday 20:00: 5 days of six buckets and one more.
    assert_eq!(opens.len(), 5 * 6 + 1);
    let saturday = monday() + 5 * DAY;
    assert!(
        opens
            .iter()
            .all(|&t| !(saturday..saturday + DAY).contains(&t)),
        "no bar lies wholly inside the weekend"
    );

    // Friday 20:00 holds the two open hours before the close; Sunday 20:00 the two after the open.
    let bar_at = |t: Millis| {
        *loaded
            .candles
            .iter()
            .find(|c| c.open_time == t)
            .unwrap_or_else(|| panic!("a bar at {t}"))
    };
    let friday = bar_at(friday_close() - 2 * HOUR);
    assert_eq!(friday.open, bid(friday_close() - 2 * HOUR).open);
    assert_eq!(friday.close, bid(friday_close() - MINUTE).close);
    assert_eq!(friday.volume, 120.0);
    let sunday = bar_at(sunday_open() - 2 * HOUR);
    assert_eq!(sunday.open, bid(sunday_open()).open);
    assert_eq!(sunday.close, bid(sunday_open() + 2 * HOUR - MINUTE).close);
    assert_eq!(sunday.volume, 120.0);

    // Daily and weekly buckets are whole too: Friday's day ends at the close, Sunday's starts at
    // the open, and the week holds its five open days.
    let days = resample(InstrumentKind::Fx, &week, Timeframe::D1, false);
    assert_eq!((days.candles.len(), days.dropped.len()), (6, 0));
    let weeks = resample(InstrumentKind::Fx, &week, Timeframe::W1, false);
    assert_eq!((weeks.candles.len(), weeks.dropped.len()), (1, 0));
    assert_eq!(weeks.tolerated, []);

    // As spot, the same minutes leave Friday's and Sunday's buckets short of their ends.
    let spot = resample(InstrumentKind::Spot, &week, Timeframe::H4, false);
    assert_eq!(
        spot.dropped,
        [friday_close() - 2 * HOUR, sunday_open() - 2 * HOUR]
    );
}

#[test]
fn an_fx_bucket_tolerates_one_percent_of_its_open_minutes() {
    // Friday 20:00's bucket has 120 open minutes, so one may be missing; two may not.
    let start = friday_close() - 2 * HOUR;
    let without = |gone: &[Millis]| -> Vec<Candle> {
        bids(&fx_week())
            .into_iter()
            .filter(|c| !gone.contains(&c.open_time))
            .collect()
    };
    let one = resample(
        InstrumentKind::Fx,
        &without(&[start + HOUR]),
        Timeframe::H4,
        false,
    );
    assert_eq!(one.tolerated, [(start, 1)]);
    assert_eq!(one.dropped, [] as [Millis; 0]);
    let two = resample(
        InstrumentKind::Fx,
        &without(&[start + HOUR, start + HOUR + MINUTE]),
        Timeframe::H4,
        false,
    );
    assert_eq!(two.dropped, [start]);
    // Its last open minute is 21:59, so missing it drops the bucket whatever the count.
    let last = resample(
        InstrumentKind::Fx,
        &without(&[friday_close() - MINUTE]),
        Timeframe::H4,
        false,
    );
    assert_eq!(last.dropped, [start]);
}

#[test]
fn asks_resample_to_the_exact_ohlc_of_their_minutes_in_the_same_buckets_as_the_bids() {
    let times = fx_week();
    let asks: Vec<Candle> = times.iter().copied().map(ask).collect();
    let bid_bars = resample(InstrumentKind::Fx, &bids(&times), Timeframe::H4, false);
    let ask_bars = resample(InstrumentKind::Fx, &asks, Timeframe::H4, false);
    let opens = |bars: &[Candle]| bars.iter().map(|c| c.open_time).collect::<Vec<_>>();
    assert_eq!(opens(&ask_bars.candles), opens(&bid_bars.candles));
    for bar in &ask_bars.candles {
        let minutes: Vec<&Candle> = asks
            .iter()
            .filter(|c| Timeframe::H4.floor(c.open_time) == bar.open_time)
            .collect();
        let high = minutes.iter().map(|c| c.high).fold(f64::MIN, f64::max);
        let low = minutes.iter().map(|c| c.low).fold(f64::MAX, f64::min);
        assert_eq!(bar.open, minutes[0].open, "{}", bar.open_time);
        assert_eq!(bar.high, high, "{}", bar.open_time);
        assert_eq!(bar.low, low, "{}", bar.open_time);
        assert_eq!(
            bar.close,
            minutes[minutes.len() - 1].close,
            "{}",
            bar.open_time
        );
    }
}

fn asks(times: &[Millis]) -> Vec<Candle> {
    times.iter().copied().map(ask).collect()
}

/// The column names and types of a Parquet file.
fn schema(path: &Path) -> Vec<(String, String)> {
    let file = fs::File::open(path).expect("the file");
    let builder = ParquetRecordBatchReaderBuilder::try_new(file).expect("a Parquet file");
    builder
        .schema()
        .fields()
        .iter()
        .map(|f| (f.name().clone(), f.data_type().to_string()))
        .collect()
}

#[test]
fn bids_and_asks_round_trip_and_the_bid_reader_reads_an_fx_file_as_before() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let store = Store::new(dir.path());
    let id = usdjpy();
    let times = fx_week();
    let report = store
        .write_quotes(&id, &bids(&times), &asks(&times))
        .expect("writes");
    let jan = Month::containing(monday());
    assert_eq!(report.months, [jan]);
    assert_eq!(report.duplicates, 0);
    assert_eq!(report.gaps, [], "the weekend is not a gap");

    assert_eq!(store.read_month(&id, jan).expect("bids"), bids(&times));
    assert_eq!(store.read_month_asks(&id, jan).expect("asks"), asks(&times));
    // Higher timeframes: the asks load into the same buckets as the bids, each the exact OHLC.
    let (from, to) = (monday(), monday() + 7 * DAY);
    let bid_bars = store
        .load_candles(&id, Timeframe::H4, from, to, false)
        .expect("loads");
    let ask_bars = store
        .load_asks(&id, Timeframe::H4, from, to, false)
        .expect("loads");
    assert_eq!(
        bid_bars,
        resample(InstrumentKind::Fx, &bids(&times), Timeframe::H4, false)
    );
    assert_eq!(
        ask_bars,
        resample(InstrumentKind::Fx, &asks(&times), Timeframe::H4, false)
    );
    assert_eq!(bid_bars.candles.len(), ask_bars.candles.len());
    // Coverage counts the weekend as closed, not missing.
    let coverage = store.coverage(&id).expect("coverage");
    assert_eq!(coverage.len(), 1);
    assert_eq!(coverage[0].gaps, []);

    // The file holds the bid candle's columns and then the ask prices.
    let names: Vec<String> = schema(&store.candle_path(&id, jan).expect("a path"))
        .into_iter()
        .map(|(name, _)| name)
        .collect();
    assert_eq!(
        names,
        [
            "open_time",
            "open",
            "high",
            "low",
            "close",
            "volume",
            "quote_volume",
            "ask_open",
            "ask_high",
            "ask_low",
            "ask_close"
        ]
    );
}

#[test]
fn a_second_write_of_quotes_merges_with_the_stored_month() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let store = Store::new(dir.path());
    let id = usdjpy();
    let times = fx_week();
    let (first, second) = (&times[..3000], &times[2990..]);
    store
        .write_quotes(&id, &bids(first), &asks(first))
        .expect("writes");
    // The second batch repeats ten minutes, with new prices on both sides.
    let moved = |c: Candle| Candle {
        open: c.open + 1.0,
        high: c.high + 1.0,
        low: c.low + 1.0,
        close: c.close + 1.0,
        ..c
    };
    let (new_bids, new_asks): (Vec<Candle>, Vec<Candle>) = second
        .iter()
        .map(|&t| (moved(bid(t)), moved(ask(t))))
        .unzip();
    let report = store
        .write_quotes(&id, &new_bids, &new_asks)
        .expect("writes");
    assert_eq!(report.duplicates, 10);
    assert_eq!(report.gaps, []);

    let jan = Month::containing(monday());
    let mut expected_bids = bids(&times[..2990]);
    expected_bids.extend(&new_bids);
    let mut expected_asks = asks(&times[..2990]);
    expected_asks.extend(&new_asks);
    assert_eq!(store.read_month(&id, jan).expect("bids"), expected_bids);
    assert_eq!(
        store.read_month_asks(&id, jan).expect("asks"),
        expected_asks
    );
}

#[test]
fn a_crypto_month_has_the_candle_columns_only() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let store = Store::new(dir.path());
    let id = btc_spot();
    let times: Vec<Millis> = (0..10).map(|i| monday() + i * MINUTE).collect();
    store.write_candles(&id, &bids(&times)).expect("writes");
    let path = store
        .candle_path(&id, Month::containing(monday()))
        .expect("a path");
    let expected: Vec<(String, String)> = [
        ("open_time", "Int64"),
        ("open", "Float64"),
        ("high", "Float64"),
        ("low", "Float64"),
        ("close", "Float64"),
        ("volume", "Float64"),
        ("quote_volume", "Float64"),
    ]
    .into_iter()
    .map(|(n, t)| (n.to_string(), t.to_string()))
    .collect();
    assert_eq!(schema(&path), expected);
}

/// What a refused write leaves: the error, the January month file's bytes unchanged, and no
/// February file.
fn assert_refused(
    store: &Store,
    id: &InstrumentId,
    result: Result<WriteReport, DataError>,
    is_expected: fn(&DataError) -> bool,
    before: &[u8],
) {
    let jan = Month::containing(monday());
    match result {
        Err(e) if is_expected(&e) => {}
        other => panic!("expected a refusal, got {other:?}"),
    }
    let after = fs::read(store.candle_path(id, jan).expect("a path")).expect("the file is there");
    assert_eq!(after, before, "the month file is untouched");
    let feb = store.candle_path(id, jan.next()).expect("a path");
    assert!(!feb.exists(), "no file for February");
}

#[test]
fn bad_quote_batches_are_refused_and_write_nothing() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let store = Store::new(dir.path());
    let id = usdjpy();
    let times = fx_week();
    store
        .write_quotes(&id, &bids(&times[..60]), &asks(&times[..60]))
        .expect("writes");
    let jan = Month::containing(monday());
    let before = fs::read(store.candle_path(&id, jan).expect("a path")).expect("the file");

    // Each batch holds a good minute in January and its bad minute in February, so a write that
    // went ahead month by month would leave a trace.
    let february = parse_date("2024-02-01").expect("a date") + 12 * HOUR;
    let good = times[100];
    let batch =
        |bad_bid: Candle, bad_ask: Candle| (vec![bid(good), bad_bid], vec![ask(good), bad_ask]);

    // FX candles without asks.
    let result = store.write_candles(&id, &bids(&times[100..110]));
    assert_refused(
        &store,
        &id,
        result,
        |e| matches!(e, DataError::QuotesRequired(_)),
        &before,
    );
    let message = store
        .write_candles(&id, &[])
        .map(|_| ())
        .map_err(|e| e.to_string());
    assert_eq!(
        message,
        Err(
            "binance:USDJPY:fx has bid and ask prices, so its candles are written with their \
             asks"
                .into()
        )
    );

    // Bids and asks that do not pair up minute for minute.
    let (b, mut a) = batch(bid(february), ask(february));
    a[1].open_time += MINUTE;
    let result = store.write_quotes(&id, &b, &a);
    assert_refused(
        &store,
        &id,
        result,
        |e| matches!(e, DataError::QuoteMinutes { .. }),
        &before,
    );
    let (b, mut a) = batch(bid(february), ask(february));
    a.pop();
    let result = store.write_quotes(&id, &b, &a);
    assert_refused(
        &store,
        &id,
        result,
        |e| matches!(e, DataError::QuoteMinutes { .. }),
        &before,
    );

    // A crossed open: the ask below the bid.
    let mut crossed = ask(february);
    crossed.open = bid(february).open - 0.001;
    crossed.low = crossed.low.min(crossed.open);
    let (b, a) = batch(bid(february), crossed);
    let result = store.write_quotes(&id, &b, &a);
    assert_refused(
        &store,
        &id,
        result,
        |e| matches!(e, DataError::CrossedQuote { field: "open", .. }),
        &before,
    );

    // A crossed close.
    let mut crossed = ask(february);
    crossed.close = bid(february).close - 0.001;
    crossed.low = crossed.low.min(crossed.close);
    let (b, a) = batch(bid(february), crossed);
    let result = store.write_quotes(&id, &b, &a);
    assert_refused(
        &store,
        &id,
        result,
        |e| matches!(e, DataError::CrossedQuote { field: "close", .. }),
        &before,
    );

    // A minute in closed hours: Saturday 3 February, noon.
    let saturday = february + 2 * DAY;
    assert!(!InstrumentKind::Fx.is_open(saturday));
    let (b, a) = batch(bid(saturday), ask(saturday));
    let result = store.write_quotes(&id, &b, &a);
    assert_refused(
        &store,
        &id,
        result,
        |e| matches!(e, DataError::ClosedMinute { .. }),
        &before,
    );
    let message = store
        .write_quotes(&id, &[bid(saturday)], &[ask(saturday)])
        .map(|_| ())
        .map_err(|e| e.to_string());
    assert_eq!(
        message,
        Err(
            "binance:USDJPY:fx at 2024-02-03 12:00 UTC: the market is closed then, so no candle \
             may open there"
                .into()
        )
    );

    // Quotes for a market that takes none: spot. Its month is untouched too.
    let spot = btc_spot();
    store
        .write_candles(&spot, &bids(&times[..60]))
        .expect("writes");
    let spot_before = fs::read(store.candle_path(&spot, jan).expect("a path")).expect("the file");
    let (b, a) = batch(bid(february), ask(february));
    let result = store.write_quotes(&spot, &b, &a);
    assert_refused(
        &store,
        &spot,
        result,
        |e| matches!(e, DataError::NoQuotes(_)),
        &spot_before,
    );
    assert!(matches!(
        store.read_month_asks(&spot, jan),
        Err(DataError::NoQuotes(_))
    ));
}

#[test]
fn an_fx_month_without_ask_columns_is_refused_by_the_ask_reader() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let store = Store::new(dir.path());
    let (fx, spot) = (usdjpy(), btc_spot());
    let jan = Month::containing(monday());
    // A crypto month file copied into the FX folder: bids only.
    store
        .write_candles(&spot, &bids(&fx_week()[..60]))
        .expect("writes");
    let path = store.candle_path(&fx, jan).expect("a path");
    fs::create_dir_all(path.parent().expect("a folder")).expect("makes the folder");
    fs::copy(store.candle_path(&spot, jan).expect("a path"), &path).expect("copies");
    assert_eq!(store.read_month(&fx, jan).expect("bids").len(), 60);
    let result = store.read_month_asks(&fx, jan);
    assert!(
        matches!(&result, Err(DataError::NoAskColumns { path: p }) if *p == path),
        "{result:?}"
    );
    // And a write of quotes to that month refuses to merge with it.
    let times = &fx_week()[100..110];
    let result = store.write_quotes(&fx, &bids(times), &asks(times));
    assert!(
        matches!(result, Err(DataError::NoAskColumns { .. })),
        "{result:?}"
    );
    // A month with nothing stored has no asks, and no error.
    assert_eq!(store.read_month_asks(&fx, jan.next()).expect("reads"), []);
}

#[test]
fn an_fx_month_is_listed_and_deleted_like_any_other() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let store = Store::new(dir.path());
    let id = usdjpy();
    let times = fx_week();
    store
        .write_quotes(&id, &bids(&times), &asks(&times))
        .expect("writes");
    let jan = Month::containing(monday());
    let path = store.candle_path(&id, jan).expect("a path");
    let bytes = fs::metadata(&path).expect("the file").len();

    let stored = store.stored().expect("lists");
    assert_eq!(stored.unrecognised, Vec::<PathBuf>::new());
    assert_eq!(stored.instruments.len(), 1);
    let listed = &stored.instruments[0];
    assert_eq!(listed.id, id);
    assert_eq!(listed.months, [StoredMonth { month: jan, bytes }]);
    assert_eq!(listed.funding_bytes, None);

    let report = store.delete_months(&id, jan, jan).expect("deletes");
    assert_eq!((report.files, report.bytes), (1, bytes));
    assert!(!path.exists());
    assert_eq!(store.stored().expect("lists").instruments, []);
    // Writing again and deleting the instrument removes it too.
    store
        .write_quotes(&id, &bids(&times[..10]), &asks(&times[..10]))
        .expect("writes");
    assert_eq!(store.delete_instrument(&id).expect("deletes").files, 1);
    assert_eq!(store.stored().expect("lists").instruments, []);
}
