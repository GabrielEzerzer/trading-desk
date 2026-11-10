//! The candle store, gap detection and resampling, in a temp directory.

use td_core::{Candle, CoreError, InstrumentId, InstrumentKind, Millis, Timeframe, Venue};
use td_data::time::{MINUTE, Month, parse_date};
use td_data::{DataError, Gap, Store, missing_minutes, resample};

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
        // 00:05-00:09 with 00:07 missing: high 107 (00:08), low 101 (00:09).
        bar(t, 5, [104.0, 107.0, 101.0, 102.0, 9.0, 930.0]),
        // 00:10-00:11 only: incomplete.
        bar(t, 10, [102.0, 104.0, 101.0, 103.0, 2.0, 205.0]),
    ]
}

#[test]
fn resampling_matches_hand_computed_bars() {
    let expected = five_minute_bars();
    assert_eq!(resample(&minutes(), Timeframe::M5, false), expected[..2]);
    assert_eq!(resample(&minutes(), Timeframe::M5, true), expected);
    assert_eq!(resample(&minutes(), Timeframe::M1, false), minutes());
    assert!(resample(&[], Timeframe::H1, true).is_empty());
}

#[test]
fn resampling_to_days_and_weeks_uses_utc_midnight_and_monday() {
    // 2024-01-06 (Saturday) 23:59 and 2024-01-07 (Sunday) 00:00 to 23:59, then Monday 00:00.
    let sunday = day("2024-01-07");
    let mut input = vec![flat(sunday - MINUTE, 1.0)];
    input.extend((0..1440).map(|m| flat(sunday + m * MINUTE, 2.0)));
    input.push(flat(day("2024-01-08"), 3.0));
    let days = resample(&input, Timeframe::D1, false);
    // Saturday's bar holds one minute but is not the trailing bar, so it stays; Monday's bar is
    // the trailing one and incomplete, so it goes.
    assert_eq!(days.len(), 2);
    assert_eq!(days[0].open_time, day("2024-01-06"));
    assert_eq!((days[1].open_time, days[1].volume), (sunday, 1440.0));
    let weeks = resample(&input, Timeframe::W1, true);
    assert_eq!(weeks.len(), 2);
    assert_eq!(weeks[0].open_time, day("2024-01-01"));
    assert_eq!((weeks[0].open, weeks[0].close), (1.0, 2.0));
    assert_eq!(weeks[1].open_time, day("2024-01-08"));
}

#[test]
fn gaps_are_runs_of_missing_minutes() {
    let t = day("2024-01-01");
    let gaps = missing_minutes(&minutes(), t - 2 * MINUTE, t + 14 * MINUTE);
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
    assert!(missing_minutes(&minutes(), t, t + 7 * MINUTE).is_empty());
    assert_eq!(
        missing_minutes(&[], t, t + 3 * MINUTE),
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
    let times: Vec<Millis> = all.iter().map(|c| c.open_time).collect();
    assert_eq!(
        times,
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
fn load_candles_resamples_and_skips_partial_buckets() {
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
    assert_eq!(load(t, t + 60 * MINUTE, false), expected[..2]);
    assert_eq!(load(t, t + 60 * MINUTE, true), expected);
    // A start inside a bucket skips that bucket rather than building it from part of its minutes.
    assert_eq!(load(t + 2 * MINUTE, t + 60 * MINUTE, false), expected[1..2]);
    // An end inside a bucket makes it incomplete.
    assert_eq!(load(t, t + 9 * MINUTE, false), expected[..1]);
    assert_eq!(load(t, t, true), []);
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
