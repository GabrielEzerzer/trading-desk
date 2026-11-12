//! The bar still forming on a chart: the closed minutes of the current bucket so far and the
//! minute in progress, as the venue's stream updates it. Display only: a feed that drives a paper
//! session never builds it, and every closed bar still comes from [`super::BarBuilder`].

use std::collections::BTreeMap;

use td_core::{Candle, Millis, Timeframe};

use crate::time::MINUTE;

/// Builds the bar in progress of one timeframe from the stream's minutes, closed and in progress.
///
/// It also keeps the bucket before the current one, because a stream may send the first update of
/// a bucket's next minute before the closed update of its last one.
#[derive(Debug, Clone)]
pub struct FormingBar {
    timeframe: Timeframe,
    /// The bucket being built; `None` before the first minute.
    current: Option<Bucket>,
    /// The bucket before it, which takes only closed minutes.
    previous: Option<Bucket>,
}

/// One bucket's minutes.
#[derive(Debug, Clone)]
struct Bucket {
    open_time: Millis,
    /// The closed minutes, by open time.
    closed: BTreeMap<Millis, Candle>,
    /// The minute in progress, later than every closed one.
    open: Option<Candle>,
    start: Start,
}

/// Whether a bucket's minutes before the first one seen are known.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Start {
    /// It was seen from its first minute, they were fetched, or their fetch was given up.
    Known,
    /// It was joined after it started, and they are still to fetch.
    Missing,
    /// Their fetch failed; it is tried once more when the bucket's last minute closes.
    Failed,
}

impl Bucket {
    /// A bucket opening at `open_time`, whose first minute seen opens at `first`.
    fn new(open_time: Millis, first: Millis) -> Self {
        Bucket {
            open_time,
            closed: BTreeMap::new(),
            open: None,
            start: if first == open_time {
                Start::Known
            } else {
                Start::Missing
            },
        }
    }

    fn push(&mut self, minute: Candle, closed: bool) -> bool {
        let t = minute.open_time;
        if closed {
            self.closed.insert(t, minute);
            if self.open.is_some_and(|open| open.open_time <= t) {
                self.open = None;
            }
            return true;
        }
        let behind = self.open.is_some_and(|open| open.open_time > t)
            || self
                .closed
                .last_key_value()
                .is_some_and(|(last, _)| *last >= t);
        if behind {
            return false;
        }
        self.open = Some(minute);
        true
    }

    /// The open time of the first minute seen.
    fn first_seen(&self) -> Option<Millis> {
        self.closed
            .keys()
            .next()
            .copied()
            .or(self.open.map(|open| open.open_time))
    }

    fn bar(&self) -> Option<Candle> {
        let mut minutes = self.closed.values().chain(self.open.as_ref());
        let first = minutes.next()?;
        let mut bar = Candle {
            open_time: self.open_time,
            ..*first
        };
        for m in minutes {
            bar.high = bar.high.max(m.high);
            bar.low = bar.low.min(m.low);
            bar.close = m.close;
            bar.volume += m.volume;
            bar.quote_volume += m.quote_volume;
        }
        Some(bar)
    }
}

impl FormingBar {
    pub fn new(timeframe: Timeframe) -> Self {
        FormingBar {
            timeframe,
            current: None,
            previous: None,
        }
    }

    /// Takes a minute from the stream, closed or in progress. A minute of a later bucket starts
    /// that bucket, and the current one becomes the previous. Returns the open time of the bucket
    /// the minute changed, or `None` when it changes nothing: it belongs to an earlier bucket (or,
    /// in progress, to the previous one), or it is an in-progress update of a minute that has
    /// closed or is older than one taken.
    pub fn push(&mut self, minute: Candle, closed: bool) -> Option<Millis> {
        let open_time = self.timeframe.floor(minute.open_time);
        if self
            .current
            .as_ref()
            .is_none_or(|c| open_time > c.open_time)
        {
            self.previous = self.current.take();
            self.current = Some(Bucket::new(open_time, minute.open_time));
        }
        let bucket = match (&mut self.current, &mut self.previous) {
            (Some(c), _) if c.open_time == open_time => c,
            (_, Some(p)) if closed && p.open_time == open_time => p,
            _ => return None,
        };
        bucket.push(minute, closed).then_some(open_time)
    }

    /// The minutes `[from, to)` to fetch before the bar of the bucket opening at `bucket` is whole:
    /// its minutes before the first one seen, when the stream was joined after it started and they
    /// were not asked for yet, or their fetch failed and its last minute has now closed.
    pub fn missing_start(&self, bucket: Millis) -> Option<(Millis, Millis)> {
        let b = self.get(bucket)?;
        let ask = match b.start {
            Start::Known => false,
            Start::Missing => true,
            Start::Failed => {
                let last = bucket + self.timeframe.duration_ms() - MINUTE;
                b.closed.contains_key(&last)
            }
        };
        let first = b.first_seen()?;
        (ask && first > bucket).then_some((bucket, first))
    }

    /// Takes the closed minutes fetched for [`FormingBar::missing_start`]. Minutes outside the
    /// bucket, or not before the first one seen, are ignored. The start counts as known afterwards
    /// even when the venue had none of them.
    pub fn fill(&mut self, bucket: Millis, minutes: impl IntoIterator<Item = Candle>) {
        let end = bucket + self.timeframe.duration_ms();
        let Some(b) = self.get_mut(bucket) else {
            return;
        };
        let first = b.first_seen().unwrap_or(end);
        for minute in minutes {
            if (bucket..first).contains(&minute.open_time) {
                b.closed.entry(minute.open_time).or_insert(minute);
            }
        }
        b.start = Start::Known;
    }

    /// Says the fetch for [`FormingBar::missing_start`] failed. After a first failure the minutes
    /// are asked for once more when the bucket's last minute closes; after a second the bar goes
    /// without them.
    pub fn fetch_failed(&mut self, bucket: Millis) {
        if let Some(b) = self.get_mut(bucket) {
            b.start = match b.start {
                Start::Missing => Start::Failed,
                Start::Known | Start::Failed => Start::Known,
            };
        }
    }

    /// Whether the minute opening at `t` is the last of its bucket.
    pub fn ends_bucket(&self, t: Millis) -> bool {
        t + MINUTE == self.timeframe.floor(t) + self.timeframe.duration_ms()
    }

    /// The bar so far of the bucket opening at `bucket`, the current or the previous one: the first
    /// minute's open, the highest high, the lowest low, the latest close and the volumes summed.
    /// `None` for another bucket, or before any minute.
    pub fn bar(&self, bucket: Millis) -> Option<Candle> {
        self.get(bucket)?.bar()
    }

    fn get(&self, bucket: Millis) -> Option<&Bucket> {
        [&self.current, &self.previous]
            .into_iter()
            .flatten()
            .find(|b| b.open_time == bucket)
    }

    fn get_mut(&mut self, bucket: Millis) -> Option<&mut Bucket> {
        [&mut self.current, &mut self.previous]
            .into_iter()
            .flatten()
            .find(|b| b.open_time == bucket)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn minute(i: i64, close: f64) -> Candle {
        Candle {
            open_time: i * MINUTE,
            open: close - 1.0,
            high: close + 2.0,
            low: close - 3.0,
            close,
            volume: 1.0,
            quote_volume: close,
        }
    }

    #[test]
    fn a_one_minute_bar_is_the_minute_in_progress() {
        let mut f = FormingBar::new(Timeframe::M1);
        assert_eq!(f.bar(3 * MINUTE), None);
        assert_eq!(f.push(minute(3, 10.0), false), Some(3 * MINUTE));
        assert_eq!(f.missing_start(3 * MINUTE), None);
        assert_eq!(f.bar(3 * MINUTE), Some(minute(3, 10.0)));
        assert!(f.push(minute(3, 11.0), false).is_some());
        assert_eq!(f.bar(3 * MINUTE), Some(minute(3, 11.0)));
        assert_eq!(
            f.push(minute(4, 12.0), false),
            Some(4 * MINUTE),
            "the next minute starts a bar"
        );
        assert_eq!(f.bar(4 * MINUTE), Some(minute(4, 12.0)));
    }

    #[test]
    fn a_longer_bar_joins_the_closed_minutes_and_the_one_in_progress() {
        let mut f = FormingBar::new(Timeframe::M5);
        let b = 5 * MINUTE;
        assert!(f.push(minute(5, 10.0), true).is_some());
        assert!(f.push(minute(6, 14.0), true).is_some());
        assert!(f.push(minute(7, 9.0), false).is_some());
        let bar = f.bar(b).expect("a bar");
        assert_eq!(bar.open_time, b);
        assert_eq!(
            (bar.open, bar.high, bar.low, bar.close),
            (9.0, 16.0, 6.0, 9.0)
        );
        assert_eq!((bar.volume, bar.quote_volume), (3.0, 33.0));
        // The minute in progress closes, then the next one starts.
        assert!(f.push(minute(7, 9.5), true).is_some());
        assert!(f.push(minute(8, 8.0), false).is_some());
        let bar = f.bar(b).expect("a bar");
        assert_eq!((bar.close, bar.volume), (8.0, 4.0));
    }

    #[test]
    fn late_and_older_updates_change_nothing() {
        let mut f = FormingBar::new(Timeframe::M5);
        let b = 10 * MINUTE;
        f.push(minute(10, 10.0), true);
        f.push(minute(11, 11.0), false);
        let before = f.bar(b);
        assert_eq!(f.push(minute(4, 1.0), true), None, "an earlier bucket");
        assert_eq!(
            f.push(minute(10, 1.0), false),
            None,
            "a minute already closed"
        );
        f.push(minute(11, 11.5), true);
        assert_eq!(
            f.push(minute(11, 1.0), false),
            None,
            "in progress after its close"
        );
        assert_ne!(f.bar(b), before);
        assert_eq!(f.bar(b).map(|b| b.close), Some(11.5));
    }

    #[test]
    fn the_last_minute_closing_after_the_next_bucket_starts_ends_its_own_bucket() {
        let mut f = FormingBar::new(Timeframe::M5);
        f.push(minute(13, 10.0), true);
        f.push(minute(14, 11.0), false);
        // The next minute's first update comes before minute 14's close.
        assert_eq!(f.push(minute(15, 20.0), false), Some(15 * MINUTE));
        assert_eq!(
            f.push(minute(14, 12.0), false),
            None,
            "in progress, the previous bucket takes nothing"
        );
        assert_eq!(f.push(minute(14, 12.5), true), Some(10 * MINUTE));
        let bar = f.bar(10 * MINUTE).expect("the previous bucket");
        assert_eq!((bar.close, bar.volume), (12.5, 2.0));
        assert_eq!(f.bar(15 * MINUTE), Some(minute(15, 20.0)));
        f.push(minute(20, 1.0), false);
        assert_eq!(f.bar(10 * MINUTE), None, "two buckets back is gone");
    }

    #[test]
    fn a_bucket_joined_late_asks_for_its_first_minutes_once() {
        let mut f = FormingBar::new(Timeframe::M15);
        let b = 15 * MINUTE;
        f.push(minute(18, 20.0), false);
        assert_eq!(f.missing_start(b), Some((b, 18 * MINUTE)));
        // Minutes from elsewhere are ignored.
        f.fill(
            b,
            [
                minute(12, 99.0),
                minute(15, 15.0),
                minute(16, 30.0),
                minute(17, 17.0),
                minute(18, 99.0),
            ],
        );
        assert_eq!(f.missing_start(b), None);
        let bar = f.bar(b).expect("a bar");
        assert_eq!(bar.open_time, b);
        assert_eq!(
            (bar.open, bar.high, bar.close, bar.volume),
            (14.0, 32.0, 20.0, 4.0)
        );
        // A venue with none of them still settles the question.
        let mut g = FormingBar::new(Timeframe::M15);
        g.push(minute(16, 20.0), true);
        g.fill(b, []);
        assert_eq!(g.missing_start(b), None);
        // A bucket seen from its first minute needs nothing.
        g.push(minute(30, 5.0), false);
        assert_eq!(g.missing_start(30 * MINUTE), None);
    }

    #[test]
    fn a_failed_fetch_of_the_first_minutes_is_asked_again_once_when_the_bucket_ends() {
        let mut f = FormingBar::new(Timeframe::M5);
        f.push(minute(2, 10.0), false);
        assert_eq!(f.missing_start(0), Some((0, 2 * MINUTE)));
        f.fetch_failed(0);
        f.push(minute(2, 10.0), true);
        f.push(minute(3, 10.0), true);
        assert_eq!(f.missing_start(0), None, "not before the bucket ends");
        f.push(minute(4, 10.0), true);
        assert_eq!(f.missing_start(0), Some((0, 2 * MINUTE)));
        f.fetch_failed(0);
        assert_eq!(f.missing_start(0), None, "and not a third time");
    }

    #[test]
    fn knows_the_last_minute_of_a_bucket() {
        let f = FormingBar::new(Timeframe::H1);
        assert!(f.ends_bucket(59 * MINUTE));
        assert!(!f.ends_bucket(58 * MINUTE));
        assert!(FormingBar::new(Timeframe::M1).ends_bucket(7 * MINUTE));
    }
}
