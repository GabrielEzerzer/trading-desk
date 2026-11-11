//! A market with a bid and an ask price that closes at weekends (FX), in a temp directory: gaps and
//! resampling by its trading hours.

use td_core::{Candle, InstrumentKind, Millis, Timeframe};
use td_data::time::{DAY, MINUTE, parse_date};
use td_data::{Gap, missing_minutes, resample};

const HOUR: Millis = 60 * MINUTE;

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
