//! The candle store, gap detection and resampling, in a temp directory.

use td_core::{Candle, CoreError, InstrumentId, InstrumentKind, Millis, Timeframe, Venue};
use td_data::time::{DAY, MINUTE, Month, parse_date};
use td_data::{
    DataError, DeleteReport, Gap, LoadedCandles, Store, StoredData, missing_minutes, resample,
};

const HOUR: Millis = 60 * MINUTE;

fn btc(kind: InstrumentKind) -> InstrumentId {
    InstrumentId {
        venue: Venue::Binance,
        symbol: "BTCUSDT".into(),
        kind,
    }
}

fn day(text: &str) -> Millis {
    parse_date(text).expect("a date")
}

/// A 1-minute candle at `minute` minutes after `start`.
fn bar(
    start: Millis,
    minute: i64,
    [open, high, low, close, volume, quote_volume]: [f64; 6],
) -> Candle {
    Candle {
        open_time: start + minute * MINUTE,
        open,
        high,
        low,
        close,
        volume,
        quote_volume,
    }
}

fn flat(t: Millis, price: f64) -> Candle {
    Candle {
        open_time: t,
        open: price,
        high: price,
        low: price,
        close: price,
        volume: 1.0,
        quote_volume: price,
    }
}

/// Flat 1-minute candles with volume 1, one at each of `minutes` minutes after `start`.
fn flat_minutes(start: Millis, minutes: impl IntoIterator<Item = i64>) -> Vec<Candle> {
    minutes
        .into_iter()
        .map(|m| flat(start + m * MINUTE, 1.0))
        .collect()
}

fn open_times(candles: &[Candle]) -> Vec<Millis> {
    candles.iter().map(|c| c.open_time).collect()
}

/// Twelve minutes from 2024-01-01 00:00 with 00:07 missing.
fn minutes() -> Vec<Candle> {
    let t = day("2024-01-01");
    vec![
        bar(t, 0, [100.0, 102.0, 99.0, 101.0, 1.0, 100.0]),
        bar(t, 1, [101.0, 103.0, 100.0, 102.0, 2.0, 204.0]),
        bar(t, 2, [102.0, 102.5, 98.0, 99.0, 3.0, 300.0]),
        bar(t, 3, [99.0, 100.0, 97.0, 98.0, 1.0, 98.0]),
        bar(t, 4, [98.0, 105.0, 98.0, 104.0, 4.0, 400.0]),
        bar(t, 5, [104.0, 104.0, 103.0, 103.5, 1.0, 104.0]),
        bar(t, 6, [103.5, 106.0, 103.0, 105.0, 2.0, 210.0]),
        bar(t, 8, [105.0, 107.0, 104.0, 106.0, 1.0, 106.0]),
        bar(t, 9, [106.0, 106.0, 101.0, 102.0, 5.0, 510.0]),
        bar(t, 10, [102.0, 103.0, 101.0, 102.5, 1.0, 102.0]),
        bar(t, 11, [102.5, 104.0, 102.0, 103.0, 1.0, 103.0]),
    ]
}

/// The 5-minute bars of [`minutes`], computed by hand.
fn five_minute_bars() -> [Candle; 3] {
    let t = day("2024-01-01");
    [
        // 00:00-00:04: open of 00:00, high 105 (00:04), low 97 (00:03), close of 00:04.
        bar(t, 0, [100.0, 105.0, 97.0, 104.0, 11.0, 1102.0]),
        // 00:05-00:09 with 00:07 missing: high 107 (00:08), low 101 (00:09). Incomplete, as a
        // 5-minute bar tolerates no missing minute.
        bar(t, 5, [104.0, 107.0, 101.0, 102.0, 9.0, 930.0]),
        // 00:10-00:11 only: incomplete.
        bar(t, 10, [102.0, 104.0, 101.0, 103.0, 2.0, 205.0]),
    ]
}

#[test]
fn resampling_matches_hand_computed_bars() {
    let t = day("2024-01-01");
    let expected = five_minute_bars();
    let strict = resample(InstrumentKind::Spot, &minutes(), Timeframe::M5, false);
    assert_eq!(strict.candles, expected[..1]);
    assert_eq!(strict.dropped, [t + 5 * MINUTE, t + 10 * MINUTE]);
    assert!(strict.tolerated.is_empty());
    let all = resample(InstrumentKind::Spot, &minutes(), Timeframe::M5, true);
    assert_eq!(all.candles, expected);
    assert!(all.dropped.is_empty());
    assert_eq!(
        resample(InstrumentKind::Spot, &minutes(), Timeframe::M1, false),
        LoadedCandles {
            candles: minutes(),
            ..LoadedCandles::default()
        }
    );
    assert_eq!(
        resample(InstrumentKind::Spot, &[], Timeframe::H1, true),
        LoadedCandles::default()
    );
}

#[test]
fn resampling_to_days_and_weeks_uses_utc_midnight_and_monday() {
    // 2024-01-06 (Saturday) 23:59 and 2024-01-07 (Sunday) 00:00 to 23:59, then Monday 00:00.
    let sunday = day("2024-01-07");
    let mut input = vec![flat(sunday - MINUTE, 1.0)];
    input.extend((0..1440).map(|m| flat(sunday + m * MINUTE, 2.0)));
    input.push(flat(day("2024-01-08"), 3.0));
    let days = resample(InstrumentKind::Spot, &input, Timeframe::D1, false);
    // Saturday's bucket holds only its last minute and Monday's only its first, so both are
    // dropped as incomplete.
    assert_eq!(days.dropped, [day("2024-01-06"), day("2024-01-08")]);
    assert_eq!(days.candles.len(), 1);
    assert_eq!(
        (days.candles[0].open_time, days.candles[0].volume),
        (sunday, 1440.0)
    );
    let weeks = resample(InstrumentKind::Spot, &input, Timeframe::W1, true).candles;
    assert_eq!(weeks.len(), 2);
    assert_eq!(weeks[0].open_time, day("2024-01-01"));
    assert_eq!((weeks[0].open, weeks[0].close), (1.0, 2.0));
    assert_eq!(weeks[1].open_time, day("2024-01-08"));
}

#[test]
fn gaps_are_runs_of_missing_minutes() {
    let t = day("2024-01-01");
    let gaps = missing_minutes(
        InstrumentKind::Spot,
        &minutes(),
        t - 2 * MINUTE,
        t + 14 * MINUTE,
    );
    assert_eq!(
        gaps,
        [
            Gap {
                from: t - 2 * MINUTE,
                to: t
            },
            Gap {
                from: t + 7 * MINUTE,
                to: t + 8 * MINUTE
            },
            Gap {
                from: t + 12 * MINUTE,
                to: t + 14 * MINUTE
            },
        ]
    );
    assert_eq!(gaps[0].minutes(), 2);
    assert!(missing_minutes(InstrumentKind::Spot, &minutes(), t, t + 7 * MINUTE).is_empty());
    assert_eq!(
        missing_minutes(InstrumentKind::Spot, &[], t, t + 3 * MINUTE),
        [Gap {
            from: t,
            to: t + 3 * MINUTE
        }]
    );
}

#[test]
fn store_round_trip_across_a_month_boundary() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let store = Store::new(dir.path());
    let id = btc(InstrumentKind::Spot);
    let feb = day("2024-02-01");
    // Given out of order, with 02-01 00:01 missing.
    let input = [
        flat(feb, 3.0),
        flat(feb - 2 * MINUTE, 1.0),
        flat(feb + 2 * MINUTE, 4.0),
        flat(feb - MINUTE, 2.0),
    ];
    let report = store.write_candles(&id, &input).expect("writes");
    let jan = Month::parse("2024-01").expect("a month");
    let february = Month::parse("2024-02").expect("a month");
    assert_eq!(report.months, [jan, february]);
    assert_eq!(report.duplicates, 0);
    assert_eq!(
        report.gaps,
        [Gap {
            from: feb + MINUTE,
            to: feb + 2 * MINUTE
        }]
    );

    let path = store.candle_path(&id, jan).expect("a path");
    assert!(path.ends_with("candles/binance/spot/BTCUSDT/1m/2024-01.parquet"));
    assert!(path.exists());
    let leftovers: Vec<_> = std::fs::read_dir(path.parent().expect("a folder"))
        .expect("listable")
        .filter_map(Result::ok)
        .filter(|e| e.file_name().to_string_lossy().ends_with(".tmp"))
        .collect();
    assert!(leftovers.is_empty(), "temp files left: {leftovers:?}");

    assert_eq!(
        store.read_month(&id, jan).expect("reads"),
        [flat(feb - 2 * MINUTE, 1.0), flat(feb - MINUTE, 2.0)]
    );
    assert_eq!(store.months(&id).expect("lists"), [jan, february]);
    let all = store
        .load_candles(
            &id,
            Timeframe::M1,
            day("2024-01-01"),
            day("2024-03-01"),
            false,
        )
        .expect("loads");
    assert_eq!(
        open_times(&all.candles),
        [feb - 2 * MINUTE, feb - MINUTE, feb, feb + 2 * MINUTE]
    );

    let coverage = store.coverage(&id).expect("coverage");
    assert_eq!(coverage.len(), 2);
    assert_eq!((coverage[0].month, coverage[0].minutes), (jan, 2));
    assert!(coverage[0].gaps.is_empty());
    assert_eq!(
        (coverage[1].first, coverage[1].last),
        (feb, feb + 2 * MINUTE)
    );
    assert_eq!(coverage[1].gaps.len(), 1);

    // Another instrument kind is a separate series.
    assert!(
        store
            .coverage(&btc(InstrumentKind::Perpetual))
            .expect("coverage")
            .is_empty()
    );
}

#[test]
fn writes_deduplicate_by_open_time_and_the_newest_wins() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let store = Store::new(dir.path());
    let id = btc(InstrumentKind::Spot);
    let t = day("2024-01-01");
    store
        .write_candles(&id, &[flat(t, 1.0), flat(t + MINUTE, 1.0)])
        .expect("writes");
    // One overlaps the stored data, and the batch repeats one of its own.
    let report = store
        .write_candles(
            &id,
            &[
                flat(t + MINUTE, 2.0),
                flat(t + 2 * MINUTE, 3.0),
                flat(t + 2 * MINUTE, 4.0),
            ],
        )
        .expect("writes");
    assert_eq!(report.duplicates, 2);
    let month = Month::containing(t);
    assert_eq!(
        store.read_month(&id, month).expect("reads"),
        [
            flat(t, 1.0),
            flat(t + MINUTE, 2.0),
            flat(t + 2 * MINUTE, 4.0)
        ]
    );
}

#[test]
fn invalid_candles_are_refused_and_nothing_is_written() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let store = Store::new(dir.path());
    let id = btc(InstrumentKind::Spot);
    let t = day("2024-01-01");
    let bad_high = Candle {
        high: 0.5,
        ..flat(t + MINUTE, 1.0)
    };
    let err = store.write_candles(&id, &[flat(t, 1.0), bad_high]);
    assert!(
        matches!(
            err,
            Err(DataError::Candle {
                source: CoreError::InvalidCandle { .. },
                ..
            })
        ),
        "{err:?}"
    );
    assert!(store.months(&id).expect("lists").is_empty());
    let off_minute = flat(t + 1, 1.0);
    assert!(store.write_candles(&id, &[off_minute]).is_err());
    let mut escape = id.clone();
    escape.symbol = "../x".into();
    assert!(matches!(
        store.write_candles(&escape, &[flat(t, 1.0)]),
        Err(DataError::InvalidSymbol(_))
    ));
}

#[test]
fn load_candles_resamples_and_drops_incomplete_buckets() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let store = Store::new(dir.path());
    let id = btc(InstrumentKind::Spot);
    store.write_candles(&id, &minutes()).expect("writes");
    let t = day("2024-01-01");
    let expected = five_minute_bars();
    let load = |from, to, incomplete| {
        store
            .load_candles(&id, Timeframe::M5, from, to, incomplete)
            .expect("loads")
    };
    let strict = load(t, t + 60 * MINUTE, false);
    assert_eq!(strict.candles, expected[..1]);
    assert_eq!(strict.dropped, [t + 5 * MINUTE, t + 10 * MINUTE]);
    assert_eq!(load(t, t + 60 * MINUTE, true).candles, expected);
    // A start or an end inside a bucket leaves the bucket missing minutes, so it is dropped like
    // one with minutes missing from the store...
    assert_eq!(load(t + 2 * MINUTE, t + 5 * MINUTE, false).dropped, [t]);
    assert_eq!(load(t, t + 4 * MINUTE, false).dropped, [t]);
    // ...or built from the minutes in the range when incomplete bars are asked for.
    assert_eq!(
        load(t + 2 * MINUTE, t + 5 * MINUTE, true).candles,
        [bar(t, 0, [102.0, 105.0, 97.0, 104.0, 8.0, 798.0])]
    );
    assert_eq!(load(t, t, true), LoadedCandles::default());
}

#[test]
fn a_bucket_missing_its_first_minute_is_dropped() {
    // Hourly bars loaded from 10:00 while the stored minutes start at 10:30: no 10:00 bar built
    // from 10:30's open and half the hour's volume.
    let dir = tempfile::tempdir().expect("a temp dir");
    let store = Store::new(dir.path());
    let id = btc(InstrumentKind::Spot);
    let ten = day("2024-01-01") + 10 * HOUR;
    store
        .write_candles(&id, &flat_minutes(ten, 30..120))
        .expect("writes");
    let loaded = store
        .load_candles(&id, Timeframe::H1, ten, ten + 2 * HOUR, false)
        .expect("loads");
    assert_eq!(open_times(&loaded.candles), [ten + HOUR]);
    assert_eq!(loaded.dropped, [ten]);
    // One missing first minute is enough.
    let loaded = resample(
        InstrumentKind::Spot,
        &flat_minutes(ten, 1..120),
        Timeframe::H1,
        false,
    );
    assert_eq!(open_times(&loaded.candles), [ten + HOUR]);
    assert_eq!(loaded.dropped, [ten]);
}

#[test]
fn an_hour_missing_one_middle_minute_is_dropped() {
    let t = day("2024-01-01");
    let input = flat_minutes(t, (0..120).filter(|&m| m != 30));
    let loaded = resample(InstrumentKind::Spot, &input, Timeframe::H1, false);
    assert_eq!(open_times(&loaded.candles), [t + HOUR]);
    assert_eq!(loaded.dropped, [t]);
    assert!(loaded.tolerated.is_empty());
}

#[test]
fn a_day_missing_up_to_one_percent_of_its_middle_minutes_is_tolerated_and_reported() {
    let t = day("2024-01-01");
    let day_missing = |start: Millis, missing: i64| {
        flat_minutes(
            start,
            (0..1440).filter(move |m| !(600..600 + missing).contains(m)),
        )
    };
    let mut input = day_missing(t, 10);
    // 14 minutes is the most a day may miss; 15 is one too many.
    input.extend(day_missing(t + DAY, 14));
    input.extend(day_missing(t + 2 * DAY, 15));
    input.extend(day_missing(t + 3 * DAY, 0));
    let loaded = resample(InstrumentKind::Spot, &input, Timeframe::D1, false);
    assert_eq!(open_times(&loaded.candles), [t, t + DAY, t + 3 * DAY]);
    assert_eq!(loaded.tolerated, [(t, 10), (t + DAY, 14)]);
    assert_eq!(loaded.dropped, [t + 2 * DAY]);
    // A tolerated bar is built from the minutes that are there.
    assert_eq!(loaded.candles[0].volume, 1430.0);
    // Asking for incomplete bars keeps the day missing 15 and still reports the tolerated ones.
    let all = resample(InstrumentKind::Spot, &input, Timeframe::D1, true);
    assert_eq!(
        open_times(&all.candles),
        [t, t + DAY, t + 2 * DAY, t + 3 * DAY]
    );
    assert_eq!(all.tolerated, loaded.tolerated);
    assert!(all.dropped.is_empty());
}

#[test]
fn a_day_missing_its_first_or_last_minute_is_dropped() {
    let t = day("2024-01-01");
    let mut input = flat_minutes(t, 1..1440);
    input.extend(flat_minutes(t + DAY, 0..1439));
    input.extend(flat_minutes(t + 2 * DAY, 0..1440));
    let loaded = resample(InstrumentKind::Spot, &input, Timeframe::D1, false);
    assert_eq!(open_times(&loaded.candles), [t + 2 * DAY]);
    assert_eq!(loaded.dropped, [t, t + DAY]);
    assert!(loaded.tolerated.is_empty());
}

#[test]
fn include_incomplete_keeps_every_bucket() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let store = Store::new(dir.path());
    let id = btc(InstrumentKind::Spot);
    let t = day("2024-01-01");
    // Three hours with 01:30 missing, loaded from 00:15 to 02:45, so every bucket misses minutes.
    store
        .write_candles(&id, &flat_minutes(t, (0..180).filter(|&m| m != 90)))
        .expect("writes");
    let (from, to) = (t + 15 * MINUTE, t + 165 * MINUTE);
    let all = store
        .load_candles(&id, Timeframe::H1, from, to, true)
        .expect("loads");
    assert_eq!(open_times(&all.candles), [t, t + HOUR, t + 2 * HOUR]);
    let volumes: Vec<f64> = all.candles.iter().map(|c| c.volume).collect();
    assert_eq!(volumes, [45.0, 59.0, 45.0]);
    assert!(all.dropped.is_empty() && all.tolerated.is_empty());
    let strict = store
        .load_candles(&id, Timeframe::H1, from, to, false)
        .expect("loads");
    assert!(strict.candles.is_empty());
    assert_eq!(strict.dropped, [t, t + HOUR, t + 2 * HOUR]);
}

#[test]
fn funding_round_trip() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let store = Store::new(dir.path());
    let id = InstrumentId {
        venue: Venue::Bybit,
        ..btc(InstrumentKind::Perpetual)
    };
    let h8 = 8 * 60 * MINUTE;
    let t = day("2024-01-01");
    assert_eq!(
        store
            .write_funding(&id, &[(t + h8, 0.0002), (t, 0.0001)])
            .expect("writes"),
        2
    );
    assert_eq!(
        store
            .write_funding(&id, &[(t + h8, 0.0003), (t + 2 * h8, -0.0001)])
            .expect("writes"),
        3
    );
    let path = store.funding_path(&id).expect("a path");
    assert!(path.ends_with("funding/bybit/BTCUSDT.parquet"));
    assert_eq!(
        store.load_funding(&id, t, t + 3 * h8).expect("loads"),
        [(t, 0.0001), (t + h8, 0.0003), (t + 2 * h8, -0.0001)]
    );
    assert_eq!(
        store.load_funding(&id, t + 1, t + 2 * h8).expect("loads"),
        [(t + h8, 0.0003)]
    );
    assert!(matches!(
        store.write_funding(&id, &[(t, f64::NAN)]),
        Err(DataError::Parse { .. })
    ));
    assert!(matches!(
        store.load_funding(&btc(InstrumentKind::Spot), t, t + h8),
        Err(DataError::NotPerpetual(_))
    ));
}

/// The paths of every file and folder under `root`, relative to it with `/` separators, sorted.
fn tree(root: &std::path::Path) -> Vec<String> {
    fn walk(root: &std::path::Path, dir: &std::path::Path, out: &mut Vec<String>) {
        for entry in std::fs::read_dir(dir).expect("listable") {
            let path = entry.expect("an entry").path();
            let relative = path.strip_prefix(root).expect("under the root");
            out.push(relative.to_string_lossy().replace('\\', "/"));
            if path.is_dir() {
                walk(root, &path, out);
            }
        }
    }
    let mut out = Vec::new();
    walk(root, root, &mut out);
    out.sort();
    out
}

fn month(text: &str) -> Month {
    Month::parse(text).expect("a month")
}

fn file_size(path: std::path::PathBuf) -> u64 {
    std::fs::metadata(path).expect("a file").len()
}

/// Stores an hour of candles in each of 2024-01, 2024-02 and 2024-03 for `id`, and two funding
/// rates when it is a perpetual.
fn store_three_months(store: &Store, id: &InstrumentId) {
    for m in ["2024-01", "2024-02", "2024-03"] {
        store
            .write_candles(id, &flat_minutes(month(m).start(), 0..60))
            .expect("writes");
    }
    if id.kind == InstrumentKind::Perpetual {
        let t = day("2024-01-01");
        store
            .write_funding(id, &[(t, 0.0001), (t + 8 * HOUR, 0.0002)])
            .expect("writes");
    }
}

#[test]
fn the_listing_shows_each_instrument_with_its_months_and_bytes_and_reports_strays() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let store = Store::new(dir.path());
    assert_eq!(store.stored().expect("lists"), StoredData::default());
    let perp = btc(InstrumentKind::Perpetual);
    let spot = btc(InstrumentKind::Spot);
    store_three_months(&store, &perp);
    store
        .write_candles(&spot, &flat_minutes(month("2024-02").start(), 0..10))
        .expect("writes");
    // A perpetual with funding stored but no candles.
    let eth = InstrumentId {
        venue: Venue::Bybit,
        symbol: "ETHUSDT".into(),
        kind: InstrumentKind::Perpetual,
    };
    store
        .write_funding(&eth, &[(day("2024-01-01"), 0.0001)])
        .expect("writes");
    // Things the store never writes.
    let root = dir.path();
    for stray in [
        "candles/kraken/spot/BTCUSDT/1m",
        "candles/binance/future/BTCUSDT/1m",
        "candles/binance/spot/btc..usdt/1m",
        "candles/binance/spot/BTCUSDT/5m",
    ] {
        std::fs::create_dir_all(root.join(stray)).expect("a folder");
    }
    for stray in [
        "candles/binance/spot/BTCUSDT/1m/notes.txt",
        "candles/binance/spot/BTCUSDT/1m/2024-13.parquet",
        "funding/bybit/odd name.parquet",
    ] {
        std::fs::write(root.join(stray), b"x").expect("writes");
    }

    let stored = store.stored().expect("lists");
    let ids: Vec<String> = stored
        .instruments
        .iter()
        .map(|i| i.id.to_string())
        .collect();
    assert_eq!(
        ids,
        [
            "binance:BTCUSDT:spot",
            "binance:BTCUSDT:perpetual",
            "bybit:ETHUSDT:perpetual"
        ]
    );
    let perp_listed = &stored.instruments[1];
    let months: Vec<Month> = perp_listed.months.iter().map(|m| m.month).collect();
    assert_eq!(
        months,
        [month("2024-01"), month("2024-02"), month("2024-03")]
    );
    for m in &perp_listed.months {
        assert!(m.bytes > 0);
        assert_eq!(
            m.bytes,
            file_size(store.candle_path(&perp, m.month).unwrap())
        );
    }
    let funding_bytes = file_size(store.funding_path(&perp).unwrap());
    assert_eq!(perp_listed.funding_bytes, Some(funding_bytes));
    assert_eq!(
        perp_listed.bytes(),
        perp_listed.months.iter().map(|m| m.bytes).sum::<u64>() + funding_bytes
    );
    assert_eq!(stored.instruments[0].funding_bytes, None);
    assert_eq!(stored.instruments[0].months.len(), 1);
    assert!(stored.instruments[2].months.is_empty());
    assert!(stored.instruments[2].funding_bytes.is_some());
    // Coverage reports the same bytes per month.
    let coverage: Vec<u64> = store
        .coverage(&perp)
        .expect("covers")
        .iter()
        .map(|m| m.bytes)
        .collect();
    let listed: Vec<u64> = perp_listed.months.iter().map(|m| m.bytes).collect();
    assert_eq!(coverage, listed);

    let strays: Vec<String> = stored
        .unrecognised
        .iter()
        .map(|p| p.to_string_lossy().replace('\\', "/"))
        .collect();
    assert_eq!(
        strays,
        [
            "candles/binance/future",
            "candles/binance/spot/BTCUSDT/1m/2024-13.parquet",
            "candles/binance/spot/BTCUSDT/1m/notes.txt",
            "candles/binance/spot/BTCUSDT/5m",
            "candles/binance/spot/btc..usdt",
            "candles/kraken",
            "funding/bybit/odd name.parquet",
        ]
    );
}

#[test]
fn deleting_a_month_range_leaves_the_other_months_readable() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let store = Store::new(dir.path());
    let id = btc(InstrumentKind::Perpetual);
    store_three_months(&store, &id);
    let feb = store.candle_path(&id, month("2024-02")).unwrap();
    let feb_bytes = file_size(feb.clone());
    // A temp file a crashed writer left beside the February file.
    std::fs::write(feb.with_extension("parquet.999.0.tmp"), b"half").expect("writes");

    let report = store
        .delete_months(&id, month("2024-02"), month("2024-02"))
        .expect("deletes");
    assert_eq!(
        report,
        DeleteReport {
            files: 2,
            bytes: feb_bytes + 4
        }
    );
    assert_eq!(
        store.months(&id).expect("lists"),
        [month("2024-01"), month("2024-03")]
    );
    for m in ["2024-01", "2024-03"] {
        assert_eq!(store.read_month(&id, month(m)).expect("reads").len(), 60);
    }
    let february: Vec<String> = tree(dir.path())
        .into_iter()
        .filter(|f| f.contains("2024-02"))
        .collect();
    assert_eq!(
        february,
        ["candles/binance/perpetual/BTCUSDT/1m/2024-02.parquet.lock"],
        "the February file and the temp file are gone, and its lock file stays"
    );
    assert_eq!(
        store
            .load_funding(&id, i64::MIN, i64::MAX)
            .expect("loads")
            .len(),
        2
    );

    // A range reaching past what is stored deletes only what is there.
    let jan_bytes = file_size(store.candle_path(&id, month("2024-01")).unwrap());
    let report = store
        .delete_months(&id, month("2023-01"), month("2024-02"))
        .expect("deletes");
    assert_eq!(
        report,
        DeleteReport {
            files: 1,
            bytes: jan_bytes
        }
    );
    assert_eq!(store.months(&id).expect("lists"), [month("2024-03")]);
    // Deleting what is not there is not an error.
    let none = store
        .delete_months(&id, month("2025-01"), month("2025-12"))
        .expect("deletes");
    assert_eq!(none, DeleteReport::default());
    // A deleted month can be written again.
    store
        .write_candles(&id, &flat_minutes(month("2024-02").start(), 0..5))
        .expect("writes");
    assert_eq!(
        store
            .read_month(&id, month("2024-02"))
            .expect("reads")
            .len(),
        5
    );
}

#[test]
fn deleting_a_whole_perpetual_removes_its_candles_and_funding_and_leaves_its_lock_files() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let store = Store::new(dir.path());
    let perp = btc(InstrumentKind::Perpetual);
    let spot = btc(InstrumentKind::Spot);
    store_three_months(&store, &perp);
    store_three_months(&store, &spot);
    let perp_bytes = store
        .stored()
        .expect("lists")
        .instruments
        .iter()
        .find(|i| i.id == perp)
        .expect("listed")
        .bytes();

    let report = store.delete_instrument(&perp).expect("deletes");
    assert_eq!(
        report,
        DeleteReport {
            files: 4,
            bytes: perp_bytes
        }
    );
    assert!(store.months(&perp).expect("lists").is_empty());
    assert!(
        store
            .load_funding(&perp, i64::MIN, i64::MAX)
            .expect("loads")
            .is_empty()
    );
    let stored = store.stored().expect("lists");
    let ids: Vec<InstrumentId> = stored.instruments.into_iter().map(|i| i.id).collect();
    assert_eq!(ids, std::slice::from_ref(&spot));
    assert_eq!(stored.unrecognised, Vec::<std::path::PathBuf>::new());
    // Of the perpetual, only its lock files are left, empty, as writers leave them.
    let perp_files: Vec<String> = tree(dir.path())
        .into_iter()
        .filter(|f| f.contains("perpetual") || f.starts_with("funding"))
        .filter(|f| dir.path().join(f).is_file())
        .collect();
    assert_eq!(
        perp_files,
        [
            "candles/binance/perpetual/BTCUSDT/1m/2024-01.parquet.lock",
            "candles/binance/perpetual/BTCUSDT/1m/2024-02.parquet.lock",
            "candles/binance/perpetual/BTCUSDT/1m/2024-03.parquet.lock",
            "funding/binance/BTCUSDT.parquet.lock",
        ]
    );
    for file in &perp_files {
        assert_eq!(file_size(dir.path().join(file)), 0, "{file}");
    }

    // Deleting what is not stored removes nothing and creates nothing, not even a folder or a
    // lock file.
    let before = tree(dir.path());
    let eth = InstrumentId {
        symbol: "ETHUSDT".into(),
        ..perp.clone()
    };
    for id in [&perp, &eth] {
        assert_eq!(
            store.delete_instrument(id).expect("deletes nothing"),
            DeleteReport::default()
        );
        assert_eq!(
            store
                .delete_months(id, month("2020-01"), month("2030-12"))
                .expect("deletes nothing"),
            DeleteReport::default()
        );
    }
    assert_eq!(tree(dir.path()), before);

    // The deleted month and funding can be written and read again.
    store
        .write_candles(&perp, &flat_minutes(month("2024-02").start(), 0..5))
        .expect("writes");
    assert_eq!(
        store
            .read_month(&perp, month("2024-02"))
            .expect("reads")
            .len(),
        5
    );
    let t = day("2024-02-01");
    store.write_funding(&perp, &[(t, 0.0003)]).expect("writes");
    assert_eq!(
        store
            .load_funding(&perp, i64::MIN, i64::MAX)
            .expect("loads"),
        [(t, 0.0003)]
    );
}

#[test]
fn a_lock_file_with_bytes_in_it_holds_up_no_write_or_delete() {
    // The store never writes into a lock file, and what one holds does not matter: writes and
    // deletes lock it as it is, and a delete leaves it as it was.
    let dir = tempfile::tempdir().expect("a temp dir");
    let store = Store::new(dir.path());
    let id = btc(InstrumentKind::Spot);
    let jan = month("2024-01");
    store
        .write_candles(&id, &flat_minutes(jan.start(), 0..5))
        .expect("writes");
    let lock_file = store
        .candle_path(&id, jan)
        .unwrap()
        .with_extension("parquet.lock");
    std::fs::write(&lock_file, b"x").expect("writes");
    store
        .write_candles(&id, &flat_minutes(jan.start(), 5..10))
        .expect("writes");
    assert_eq!(store.read_month(&id, jan).expect("reads").len(), 10);
    assert_eq!(
        store.delete_months(&id, jan, jan).expect("deletes").files,
        1
    );
    assert_eq!(std::fs::read(&lock_file).expect("stays"), b"x");
    assert!(store.months(&id).expect("lists").is_empty());
}

/// Makes `link` a link to the folder `target`: a junction on Windows, which needs no privilege,
/// and a symlink elsewhere.
fn link_dir(target: &std::path::Path, link: &std::path::Path) {
    #[cfg(windows)]
    {
        let status = std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(link)
            .arg(target)
            .stdout(std::process::Stdio::null())
            .status()
            .expect("cmd runs");
        assert!(status.success(), "mklink /J {link:?} {target:?}: {status}");
    }
    #[cfg(unix)]
    std::os::unix::fs::symlink(target, link).expect("a symlink");
}

/// The store's [`StoredData::unrecognised`] paths with `/` separators.
fn strays(stored: &StoredData) -> Vec<String> {
    stored
        .unrecognised
        .iter()
        .map(|p| p.to_string_lossy().replace('\\', "/"))
        .collect()
}

#[test]
fn a_delete_does_not_follow_a_link_out_of_the_store() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let store = Store::new(dir.path().join("store"));
    // Outside the store, a folder laid out as an instrument's folder is, holding a month file, and
    // a funding file beside it.
    let outside = dir.path().join("outside");
    std::fs::create_dir_all(outside.join("1m")).expect("a folder");
    std::fs::write(outside.join("1m/2024-01.parquet"), b"keep").expect("writes");
    std::fs::write(outside.join("XRPUSDT.parquet"), b"keep").expect("writes");
    let spot = InstrumentId {
        venue: Venue::Binance,
        symbol: "XRPUSDT".into(),
        kind: InstrumentKind::Spot,
    };
    let perp = InstrumentId {
        kind: InstrumentKind::Perpetual,
        ..spot.clone()
    };
    let parent = |path: std::path::PathBuf| path.parent().expect("a folder").to_path_buf();
    let month_dir = |id: &InstrumentId| parent(store.candle_path(id, month("2024-01")).unwrap());
    // Links inside the store to it: the spot symbol folder, the perpetual's month folder, and the
    // venue's funding folder.
    for (link, target) in [
        (parent(month_dir(&spot)), outside.clone()),
        (month_dir(&perp), outside.join("1m")),
        (parent(store.funding_path(&perp).unwrap()), outside.clone()),
    ] {
        std::fs::create_dir_all(parent(link.clone())).expect("a folder");
        link_dir(&target, &link);
    }

    for id in [&spot, &perp] {
        assert_eq!(
            store.delete_instrument(id).expect("deletes nothing"),
            DeleteReport::default(),
            "{id}"
        );
        assert_eq!(
            store
                .delete_months(id, month("2024-01"), month("2024-01"))
                .expect("deletes nothing"),
            DeleteReport::default(),
            "{id}"
        );
    }
    assert_eq!(
        tree(&outside),
        ["1m", "1m/2024-01.parquet", "XRPUSDT.parquet"],
        "nothing outside was removed or added"
    );
    for file in ["1m/2024-01.parquet", "XRPUSDT.parquet"] {
        assert_eq!(std::fs::read(outside.join(file)).expect("reads"), b"keep");
    }
}

#[test]
fn a_file_named_1m_is_reported_and_the_rest_of_the_store_is_listed() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let store = Store::new(dir.path());
    let spot = btc(InstrumentKind::Spot);
    store
        .write_candles(&spot, &flat_minutes(month("2024-01").start(), 0..10))
        .expect("writes");
    let stray = dir.path().join("candles/binance/spot/ETHUSDT/1m");
    std::fs::create_dir_all(stray.parent().unwrap()).expect("a folder");
    std::fs::write(&stray, b"x").expect("writes");

    let stored = store.stored().expect("lists");
    let ids: Vec<InstrumentId> = stored.instruments.iter().map(|i| i.id.clone()).collect();
    assert_eq!(ids, [spot]);
    assert_eq!(strays(&stored), ["candles/binance/spot/ETHUSDT/1m"]);
}

#[test]
fn a_1m_link_is_reported_and_not_read() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let store = Store::new(dir.path().join("store"));
    let outside = dir.path().join("outside");
    std::fs::create_dir_all(&outside).expect("a folder");
    std::fs::write(outside.join("2024-01.parquet"), b"x").expect("writes");
    let eth = InstrumentId {
        symbol: "ETHUSDT".into(),
        ..btc(InstrumentKind::Spot)
    };
    let month_file = store.candle_path(&eth, month("2024-01")).unwrap();
    let link = month_file.parent().unwrap();
    std::fs::create_dir_all(link.parent().unwrap()).expect("a folder");
    link_dir(&outside, link);

    let stored = store.stored().expect("lists");
    assert!(
        stored.instruments.is_empty(),
        "nothing is read through the link: {stored:?}"
    );
    assert_eq!(strays(&stored), ["candles/binance/spot/ETHUSDT/1m"]);
}

#[test]
fn a_candles_or_funding_link_is_reported_and_neither_listed_nor_deleted_through() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let store = Store::new(dir.path().join("store"));
    let outside = dir.path().join("outside");
    let perp = btc(InstrumentKind::Perpetual);
    // Month and funding files laid out as the store lays them out, in a folder outside it.
    for (top, file) in [
        (
            "candles",
            store.candle_path(&perp, month("2024-01")).unwrap(),
        ),
        ("funding", store.funding_path(&perp).unwrap()),
    ] {
        let below = file.strip_prefix(store.root().join(top)).expect("under it");
        let target = outside.join(top);
        std::fs::create_dir_all(target.join(below).parent().unwrap()).expect("a folder");
        std::fs::write(target.join(below), b"keep").expect("writes");
        std::fs::create_dir_all(store.root()).expect("a folder");
        link_dir(&target, &store.root().join(top));
    }
    let before = tree(&outside);

    let stored = store.stored().expect("lists");
    assert!(
        stored.instruments.is_empty(),
        "nothing is listed through the links: {stored:?}"
    );
    assert_eq!(strays(&stored), ["candles", "funding"]);
    assert_eq!(
        store.delete_instrument(&perp).expect("deletes nothing"),
        DeleteReport::default()
    );
    assert_eq!(tree(&outside), before, "nothing outside was removed");
}

/// A file another program holds open without sharing cannot be removed on Windows; elsewhere an
/// open file can be.
#[cfg(windows)]
#[test]
fn a_delete_that_stops_part_way_says_how_much_it_removed() {
    use std::os::windows::fs::OpenOptionsExt;
    let dir = tempfile::tempdir().expect("a temp dir");
    let store = Store::new(dir.path());
    let id = btc(InstrumentKind::Spot);
    store_three_months(&store, &id);
    let jan_bytes = file_size(store.candle_path(&id, month("2024-01")).unwrap());
    let held = std::fs::File::options()
        .read(true)
        .share_mode(0)
        .open(store.candle_path(&id, month("2024-02")).unwrap())
        .expect("opens");

    let error = store
        .delete_instrument(&id)
        .expect_err("February cannot be removed")
        .to_string();
    assert!(
        error.contains(&format!("files removed before it: 1, {jan_bytes} bytes")),
        "{error}"
    );
    drop(held);
    assert_eq!(
        store.months(&id).expect("lists"),
        [month("2024-02"), month("2024-03")]
    );
}

#[test]
fn a_hostile_symbol_cannot_make_a_delete_leave_the_store() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let store = Store::new(dir.path().join("store"));
    // A file outside the store that an escaping path could reach.
    std::fs::write(dir.path().join("keep.parquet"), b"keep").expect("writes");
    store_three_months(&store, &btc(InstrumentKind::Perpetual));
    let before = tree(dir.path());
    for symbol in [
        "..",
        "../..",
        "../../../keep",
        "..\\..\\..\\keep",
        "/",
        "C:",
        "C:\\",
        "BTC/USDT",
        "",
        ".",
        "btcusdt",
    ] {
        let id = InstrumentId {
            symbol: symbol.into(),
            ..btc(InstrumentKind::Perpetual)
        };
        assert!(
            matches!(
                store.delete_instrument(&id),
                Err(DataError::InvalidSymbol(_))
            ),
            "{symbol:?}"
        );
        assert!(
            matches!(
                store.delete_months(&id, month("2000-01"), month("2099-12")),
                Err(DataError::InvalidSymbol(_))
            ),
            "{symbol:?}"
        );
    }
    assert_eq!(tree(dir.path()), before, "nothing was touched");
}

#[test]
fn a_temp_file_from_a_crashed_first_write_of_a_month_is_listed_and_deleted() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let store = Store::new(dir.path());
    let id = btc(InstrumentKind::Spot);
    store_three_months(&store, &id);
    // A writer killed before its first rename of April leaves its lock file and its temp file, and
    // no April file. A temp file beside a month file is a writer's business and is not listed.
    let april = store.candle_path(&id, month("2024-04")).unwrap();
    std::fs::write(april.with_extension("parquet.lock"), b"").expect("writes");
    std::fs::write(april.with_extension("parquet.999.0.tmp"), b"half").expect("writes");
    let feb = store.candle_path(&id, month("2024-02")).unwrap();
    std::fs::write(feb.with_extension("parquet.999.1.tmp"), b"half").expect("writes");

    let stored = store.stored().expect("lists");
    assert_eq!(
        strays(&stored),
        ["candles/binance/spot/BTCUSDT/1m/2024-04.parquet.999.0.tmp"]
    );
    let months: Vec<Month> = stored.instruments[0]
        .months
        .iter()
        .map(|m| m.month)
        .collect();
    assert_eq!(
        months,
        [month("2024-01"), month("2024-02"), month("2024-03")]
    );

    // A delete of April removes the leftover, and leaves the lock file.
    let report = store
        .delete_months(&id, month("2024-04"), month("2024-04"))
        .expect("deletes");
    assert_eq!(report, DeleteReport { files: 1, bytes: 4 });
    assert!(!april.with_extension("parquet.999.0.tmp").exists());
    assert!(april.with_extension("parquet.lock").exists());
    assert_eq!(
        strays(&store.stored().expect("lists")),
        Vec::<String>::new()
    );

    // And a delete of the whole instrument removes one too.
    std::fs::write(april.with_extension("parquet.999.2.tmp"), b"half").expect("writes");
    let report = store.delete_instrument(&id).expect("deletes");
    assert_eq!(
        report.files,
        3 + 2,
        "three month files, the February and April temp files"
    );
    let left: Vec<String> = tree(dir.path())
        .into_iter()
        .filter(|f| f.ends_with(".tmp") || f.ends_with(".parquet"))
        .collect();
    assert_eq!(left, Vec::<String>::new());
}

#[test]
fn a_write_of_a_month_removes_a_crashed_writers_temp_file() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let store = Store::new(dir.path());
    let id = btc(InstrumentKind::Spot);
    // A writer killed before its first rename of April left its lock file and its temp file.
    let april = store.candle_path(&id, month("2024-04")).unwrap();
    std::fs::create_dir_all(april.parent().unwrap()).expect("a folder");
    std::fs::write(april.with_extension("parquet.lock"), b"").expect("writes");
    std::fs::write(april.with_extension("parquet.999.0.tmp"), b"half").expect("writes");
    // The next download of April writes the month, and the leftover goes with it.
    let written = flat_minutes(month("2024-04").start(), 0..10);
    store.write_candles(&id, &written).expect("writes");
    let left: Vec<String> = tree(dir.path())
        .into_iter()
        .filter(|f| f.ends_with(".tmp"))
        .collect();
    assert_eq!(left, Vec::<String>::new());
    assert_eq!(
        store.read_month(&id, month("2024-04")).expect("reads"),
        written
    );
}

#[test]
fn a_write_over_two_months_with_a_later_month_unreadable_writes_neither() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let store = Store::new(dir.path());
    let id = btc(InstrumentKind::Spot);
    let jan = month("2024-01");
    store
        .write_candles(&id, &flat_minutes(jan.start(), 0..60))
        .expect("writes");
    let january = store.candle_path(&id, jan).unwrap();
    let before = std::fs::read(&january).expect("the file");
    // February's file is not Parquet.
    let february = store.candle_path(&id, jan.next()).unwrap();
    std::fs::write(&february, b"not parquet").expect("writes");

    let mut batch = flat_minutes(jan.start(), 60..70);
    batch.extend(flat_minutes(jan.next().start(), 0..10));
    let result = store.write_candles(&id, &batch);
    assert!(result.is_err(), "{result:?}");
    assert!(
        std::fs::read(&january).expect("the file") == before,
        "January is untouched"
    );
    assert_eq!(std::fs::read(&february).expect("the file"), b"not parquet");
}
