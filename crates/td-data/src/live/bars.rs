//! Turning closed 1-minute bars, as they arrive, into an ordered stream and completed bars of the
//! session's timeframe. Pure, so recorded frames and the live socket drive the same code.

use td_core::{Candle, InstrumentKind, Millis, Timeframe};

use crate::resample::{LoadedCandles, resample};
use crate::time::MINUTE;

/// Where a closed minute stands against the minutes already taken.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Placement {
    /// At or before a minute already taken: a duplicate or a late repeat. Skip it.
    Stale,
    /// The next minute expected.
    Next,
    /// Later than the next minute expected: the minutes `[from, to)` are missing and should be
    /// backfilled before it.
    Gap { from: Millis, to: Millis },
}

/// Builds bars of one timeframe from closed 1-minute bars fed in order.
///
/// A bar is emitted as soon as its bucket is settled: when its last minute arrives, when a minute
/// of a later bucket arrives, or when [`BarBuilder::settle`] says no more minutes will come for it.
/// The bucket is then built by td-data's resampling rule ([`resample`]), so a bucket missing
/// minutes is dropped or tolerated exactly as it would be when loading from the store.
#[derive(Debug, Clone)]
pub struct BarBuilder {
    timeframe: Timeframe,
    /// The open time of the first minute not yet taken; `None` until the first one.
    next_minute: Option<Millis>,
    /// The minutes taken for the bucket in progress.
    bucket: Vec<Candle>,
}

impl BarBuilder {
    /// A builder that expects its first minute at `start`, or takes whatever comes first when
    /// `start` is `None` (its first bucket is then usually incomplete and dropped).
    pub fn new(timeframe: Timeframe, start: Option<Millis>) -> Self {
        BarBuilder {
            timeframe,
            next_minute: start,
            bucket: Vec::new(),
        }
    }

    /// The open time of the next minute expected.
    pub fn next_minute(&self) -> Option<Millis> {
        self.next_minute
    }

    pub fn place(&self, open_time: Millis) -> Placement {
        match self.next_minute {
            Some(next) if open_time < next => Placement::Stale,
            Some(next) if open_time > next => Placement::Gap {
                from: next,
                to: open_time,
            },
            _ => Placement::Next,
        }
    }

    /// Takes a closed minute. One that is [`Placement::Stale`] is ignored; one past a gap is taken
    /// as if the missing minutes do not exist, so backfill them first. Returns the bars completed.
    pub fn push(&mut self, minute: Candle) -> LoadedCandles {
        if self.place(minute.open_time) == Placement::Stale {
            return LoadedCandles::default();
        }
        let timeframe = self.timeframe;
        let bucket_of = |t: Millis| timeframe.floor(t);
        let mut out = match self.bucket.first() {
            Some(first) if bucket_of(first.open_time) != bucket_of(minute.open_time) => {
                self.finish()
            }
            _ => LoadedCandles::default(),
        };
        self.bucket.push(minute);
        self.next_minute = Some(minute.open_time + MINUTE);
        let bucket_end = bucket_of(minute.open_time) + timeframe.duration_ms();
        if minute.open_time + MINUTE == bucket_end {
            append(&mut out, self.finish());
        }
        out
    }

    /// Says that no minute before `t` will come any more (a backfill up to `t` returned what the
    /// venue has). Moves the next minute expected to `t` and completes a bucket that ends by then.
    pub fn settle(&mut self, t: Millis) -> LoadedCandles {
        if self.next_minute.is_none_or(|next| next < t) {
            self.next_minute = Some(t);
        }
        match self.bucket.first() {
            Some(first)
                if self.timeframe.floor(first.open_time) + self.timeframe.duration_ms() <= t =>
            {
                self.finish()
            }
            _ => LoadedCandles::default(),
        }
    }

    fn finish(&mut self) -> LoadedCandles {
        // shortcut: live streams serve crypto only, which never closes, so a bucket expects every
        // minute and is settled at its last; give the builder the instrument's kind, for both
        // `resample` and settling, when a source with closed hours gets a live stream.
        let loaded = resample(InstrumentKind::Spot, &self.bucket, self.timeframe, false);
        self.bucket.clear();
        loaded
    }
}

fn append(out: &mut LoadedCandles, more: LoadedCandles) {
    out.candles.extend(more.candles);
    out.dropped.extend(more.dropped);
    out.tolerated.extend(more.tolerated);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn minute(i: i64) -> Candle {
        Candle {
            open_time: i * MINUTE,
            open: 10.0 + i as f64,
            high: 20.0 + i as f64,
            low: 5.0,
            close: 11.0 + i as f64,
            volume: 1.0,
            quote_volume: 10.0,
        }
    }

    #[test]
    fn one_minute_bars_pass_straight_through() {
        let mut b = BarBuilder::new(Timeframe::M1, Some(0));
        assert_eq!(b.push(minute(0)).candles, [minute(0)]);
        assert_eq!(b.place(0), Placement::Stale);
        assert_eq!(b.place(MINUTE), Placement::Next);
        assert_eq!(
            b.place(3 * MINUTE),
            Placement::Gap {
                from: MINUTE,
                to: 3 * MINUTE
            }
        );
        assert!(b.push(minute(0)).candles.is_empty(), "a repeat is ignored");
    }

    #[test]
    fn a_bucket_is_emitted_when_its_last_minute_arrives() {
        let mut b = BarBuilder::new(Timeframe::M5, Some(0));
        for i in 0..4 {
            assert!(b.push(minute(i)).candles.is_empty());
        }
        let bar = b.push(minute(4)).candles;
        assert_eq!(bar.len(), 1);
        assert_eq!(
            (bar[0].open_time, bar[0].open, bar[0].close, bar[0].high),
            (0, 10.0, 15.0, 24.0)
        );
        assert_eq!(bar[0].volume, 5.0);
    }

    #[test]
    fn a_bucket_missing_minutes_is_dropped_when_a_later_bucket_starts_or_it_is_settled() {
        let mut b = BarBuilder::new(Timeframe::M5, Some(0));
        b.push(minute(0));
        b.push(minute(1));
        let out = b.push(minute(6));
        assert!(out.candles.is_empty());
        assert_eq!(out.dropped, [0]);
        // The rest of the second bucket never comes: settling past its end drops it.
        let out = b.settle(10 * MINUTE);
        assert_eq!(out.dropped, [5 * MINUTE]);
        assert_eq!(b.next_minute(), Some(10 * MINUTE));
    }

    #[test]
    fn joining_mid_bucket_drops_the_first_bucket() {
        let mut b = BarBuilder::new(Timeframe::M5, None);
        for i in 2..5 {
            let out = b.push(minute(i));
            assert!(out.candles.is_empty());
            if i == 4 {
                assert_eq!(out.dropped, [0]);
            }
        }
    }
}
