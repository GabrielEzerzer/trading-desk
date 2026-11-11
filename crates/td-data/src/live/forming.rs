//! The bar still forming on a chart: the closed minutes of the current bucket so far and the
//! minute in progress, as the venue's stream updates it. Display only: a feed that drives a paper
//! session never builds it, and every closed bar still comes from [`super::BarBuilder`].

use std::collections::BTreeMap;

use td_core::{Candle, Millis, Timeframe};

use crate::time::MINUTE;

/// Builds the bar in progress of one timeframe from the stream's minutes, closed and in progress.
#[derive(Debug, Clone)]
pub struct FormingBar {
    timeframe: Timeframe,
    /// The open time of the bucket being built; `None` before the first minute.
    bucket: Option<Millis>,
    /// The bucket's closed minutes, by open time.
    closed: BTreeMap<Millis, Candle>,
    /// The minute in progress, later than every closed one.
    open: Option<Candle>,
    /// Whether the bucket's minutes before the first one seen are known: it was seen from its first
    /// minute, or they were asked for with [`FormingBar::fill`].
    start_known: bool,
}

impl FormingBar {
    pub fn new(timeframe: Timeframe) -> Self {
        FormingBar {
            timeframe,
            bucket: None,
            closed: BTreeMap::new(),
            open: None,
            start_known: false,
        }
    }

    /// Takes a minute from the stream, closed or in progress. A minute of a later bucket starts
    /// that bucket. Returns false when the minute changes nothing: it belongs to an earlier bucket,
    /// or it is an in-progress update of a minute that has closed or is older than one taken.
    pub fn push(&mut self, minute: Candle, closed: bool) -> bool {
        let bucket = self.timeframe.floor(minute.open_time);
        match self.bucket {
            Some(current) if bucket < current => return false,
            Some(current) if bucket == current => {}
            _ => {
                self.bucket = Some(bucket);
                self.closed.clear();
                self.open = None;
                self.start_known = minute.open_time == bucket;
            }
        }
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

    /// The minutes `[from, to)` to fetch before the bar is whole: the bucket's minutes before the
    /// first one seen, when the stream was joined after the bucket started and they were not asked
    /// for yet.
    pub fn missing_start(&self) -> Option<(Millis, Millis)> {
        if self.start_known {
            return None;
        }
        let bucket = self.bucket?;
        let first = self
            .closed
            .keys()
            .next()
            .copied()
            .or(self.open.map(|open| open.open_time))?;
        (first > bucket).then_some((bucket, first))
    }

    /// Takes the closed minutes fetched for [`FormingBar::missing_start`]. Minutes outside the
    /// bucket, or not before the first one seen, are ignored. The start counts as known afterwards
    /// even when the venue had none of them.
    pub fn fill(&mut self, minutes: impl IntoIterator<Item = Candle>) {
        let Some(bucket) = self.bucket else {
            return;
        };
        let first = self
            .closed
            .keys()
            .next()
            .copied()
            .or(self.open.map(|open| open.open_time))
            .unwrap_or(bucket + self.timeframe.duration_ms());
        for minute in minutes {
            if (bucket..first).contains(&minute.open_time) {
                self.closed.entry(minute.open_time).or_insert(minute);
            }
        }
        self.start_known = true;
    }

    /// Whether the minute opening at `t` is the last of its bucket.
    pub fn ends_bucket(&self, t: Millis) -> bool {
        t + MINUTE == self.timeframe.floor(t) + self.timeframe.duration_ms()
    }

    /// The open time of the bucket being built.
    pub fn bucket(&self) -> Option<Millis> {
        self.bucket
    }

    /// The bar so far, opening at the bucket's start: the first minute's open, the highest high,
    /// the lowest low, the latest close and the volumes summed. `None` before any minute.
    pub fn bar(&self) -> Option<Candle> {
        let bucket = self.bucket?;
        let mut minutes = self.closed.values().chain(self.open.as_ref());
        let first = minutes.next()?;
        let mut bar = Candle {
            open_time: bucket,
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
        assert_eq!(f.bar(), None);
        assert!(f.push(minute(3, 10.0), false));
        assert_eq!(f.missing_start(), None);
        assert_eq!(f.bar(), Some(minute(3, 10.0)));
        assert!(f.push(minute(3, 11.0), false));
        assert_eq!(f.bar(), Some(minute(3, 11.0)));
        assert!(
            f.push(minute(4, 12.0), false),
            "the next minute starts a bar"
        );
        assert_eq!(f.bar(), Some(minute(4, 12.0)));
    }

    #[test]
    fn a_longer_bar_joins_the_closed_minutes_and_the_one_in_progress() {
        let mut f = FormingBar::new(Timeframe::M5);
        assert!(f.push(minute(5, 10.0), true));
        assert!(f.push(minute(6, 14.0), true));
        assert!(f.push(minute(7, 9.0), false));
        let bar = f.bar().expect("a bar");
        assert_eq!(bar.open_time, 5 * MINUTE);
        assert_eq!(
            (bar.open, bar.high, bar.low, bar.close),
            (9.0, 16.0, 6.0, 9.0)
        );
        assert_eq!((bar.volume, bar.quote_volume), (3.0, 33.0));
        // The minute in progress closes, then the next one starts.
        assert!(f.push(minute(7, 9.5), true));
        assert!(f.push(minute(8, 8.0), false));
        let bar = f.bar().expect("a bar");
        assert_eq!((bar.close, bar.volume), (8.0, 4.0));
    }

    #[test]
    fn late_and_older_updates_change_nothing() {
        let mut f = FormingBar::new(Timeframe::M5);
        f.push(minute(10, 10.0), true);
        f.push(minute(11, 11.0), false);
        let before = f.bar();
        assert!(!f.push(minute(4, 1.0), true), "an earlier bucket");
        assert!(!f.push(minute(10, 1.0), false), "a minute already closed");
        f.push(minute(11, 11.5), true);
        assert!(
            !f.push(minute(11, 1.0), false),
            "in progress after its close"
        );
        assert_ne!(f.bar(), before);
        assert_eq!(f.bar().map(|b| b.close), Some(11.5));
    }

    #[test]
    fn a_bucket_joined_late_asks_for_its_first_minutes_once() {
        let mut f = FormingBar::new(Timeframe::M15);
        f.push(minute(18, 20.0), false);
        assert_eq!(f.missing_start(), Some((15 * MINUTE, 18 * MINUTE)));
        // Minutes from elsewhere are ignored.
        f.fill([
            minute(12, 99.0),
            minute(15, 15.0),
            minute(16, 30.0),
            minute(17, 17.0),
            minute(18, 99.0),
        ]);
        assert_eq!(f.missing_start(), None);
        let bar = f.bar().expect("a bar");
        assert_eq!(bar.open_time, 15 * MINUTE);
        assert_eq!(
            (bar.open, bar.high, bar.close, bar.volume),
            (14.0, 32.0, 20.0, 4.0)
        );
        // A venue with none of them still settles the question.
        let mut g = FormingBar::new(Timeframe::M15);
        g.push(minute(16, 20.0), true);
        g.fill([]);
        assert_eq!(g.missing_start(), None);
        // A bucket seen from its first minute needs nothing.
        g.push(minute(30, 5.0), false);
        assert_eq!(g.missing_start(), None);
    }

    #[test]
    fn knows_the_last_minute_of_a_bucket() {
        let f = FormingBar::new(Timeframe::H1);
        assert!(f.ends_bucket(59 * MINUTE));
        assert!(!f.ends_bucket(58 * MINUTE));
        assert!(FormingBar::new(Timeframe::M1).ends_bucket(7 * MINUTE));
    }
}
