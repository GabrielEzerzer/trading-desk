use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::{CoreError, Millis};

const MINUTE: Millis = 60_000;
const HOUR: Millis = 60 * MINUTE;
const DAY: Millis = 24 * HOUR;
const WEEK: Millis = 7 * DAY;
/// 1970-01-01 was a Thursday; weekly bars open on Monday 00:00 UTC, four days later.
const FIRST_MONDAY: Millis = 4 * DAY;

/// A candle interval. Serialized and displayed as `1m`, `5m`, `15m`, `30m`, `1h`, `4h`, `1d`, `1w`.
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Timeframe {
    #[serde(rename = "1m")]
    M1,
    #[serde(rename = "5m")]
    M5,
    #[serde(rename = "15m")]
    M15,
    #[serde(rename = "30m")]
    M30,
    #[serde(rename = "1h")]
    H1,
    #[serde(rename = "4h")]
    H4,
    #[serde(rename = "1d")]
    D1,
    #[serde(rename = "1w")]
    W1,
}

impl Timeframe {
    pub const ALL: [Timeframe; 8] = [
        Timeframe::M1,
        Timeframe::M5,
        Timeframe::M15,
        Timeframe::M30,
        Timeframe::H1,
        Timeframe::H4,
        Timeframe::D1,
        Timeframe::W1,
    ];

    pub fn duration_ms(self) -> Millis {
        match self {
            Timeframe::M1 => MINUTE,
            Timeframe::M5 => 5 * MINUTE,
            Timeframe::M15 => 15 * MINUTE,
            Timeframe::M30 => 30 * MINUTE,
            Timeframe::H1 => HOUR,
            Timeframe::H4 => 4 * HOUR,
            Timeframe::D1 => DAY,
            Timeframe::W1 => WEEK,
        }
    }

    /// The open time of the bar that contains `t`. Days start at 00:00 UTC and weeks on Monday
    /// 00:00 UTC, as on Binance and Bybit.
    pub fn floor(self, t: Millis) -> Millis {
        let origin = if self == Timeframe::W1 {
            FIRST_MONDAY
        } else {
            0
        };
        t - (t - origin).rem_euclid(self.duration_ms())
    }

    /// The open time of the first bar that opens at or after `t`: `t` itself on a bar's open, the
    /// next bar's open otherwise.
    pub fn ceil(self, t: Millis) -> Millis {
        let start = self.floor(t);
        if start == t {
            t
        } else {
            start + self.duration_ms()
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Timeframe::M1 => "1m",
            Timeframe::M5 => "5m",
            Timeframe::M15 => "15m",
            Timeframe::M30 => "30m",
            Timeframe::H1 => "1h",
            Timeframe::H4 => "4h",
            Timeframe::D1 => "1d",
            Timeframe::W1 => "1w",
        }
    }
}

impl fmt::Display for Timeframe {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Timeframe {
    type Err = CoreError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Timeframe::ALL
            .into_iter()
            .find(|tf| tf.as_str() == s)
            .ok_or_else(|| CoreError::UnknownTimeframe(s.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // 2024-03-14 15:47:23.456 UTC, a Thursday.
    const T: Millis = 1_710_431_243_456;

    #[test]
    fn floor_to_minutes_hours_and_days() {
        assert_eq!(Timeframe::M1.floor(T), 1_710_431_220_000); // 15:47:00
        assert_eq!(Timeframe::M15.floor(T), 1_710_431_100_000); // 15:45:00
        assert_eq!(Timeframe::H4.floor(T), 1_710_417_600_000); // 12:00:00
        assert_eq!(Timeframe::D1.floor(T), 1_710_374_400_000); // 00:00:00
    }

    #[test]
    fn floor_to_week_lands_on_monday() {
        assert_eq!(Timeframe::W1.floor(T), 1_710_115_200_000); // Monday 2024-03-11 00:00
        assert_eq!(Timeframe::W1.floor(1_710_115_200_000), 1_710_115_200_000);
        assert_eq!(Timeframe::W1.floor(1_710_115_199_999), 1_709_510_400_000); // Monday 2024-03-04
    }

    #[test]
    fn floor_is_idempotent_and_handles_times_before_the_epoch() {
        for tf in Timeframe::ALL {
            let start = tf.floor(T);
            assert_eq!(tf.floor(start), start);
            assert!(start <= T && T < start + tf.duration_ms());
        }
        assert_eq!(Timeframe::M1.floor(-1), -60_000);
    }

    #[test]
    fn ceil_is_the_next_open_or_the_open_itself() {
        assert_eq!(Timeframe::M1.ceil(T), 1_710_431_280_000); // 15:48:00
        assert_eq!(Timeframe::H4.ceil(T), 1_710_432_000_000); // 16:00:00
        assert_eq!(Timeframe::W1.ceil(T), 1_710_720_000_000); // Monday 2024-03-18
        for tf in Timeframe::ALL {
            let start = tf.floor(T);
            assert_eq!(tf.ceil(start), start, "{tf}");
            assert_eq!(tf.ceil(start + 1), start + tf.duration_ms(), "{tf}");
            assert_eq!(tf.ceil(start - 1), start, "{tf}");
        }
        assert_eq!(Timeframe::M1.ceil(-1), 0);
    }

    #[test]
    fn parse_and_display_round_trip() {
        for tf in Timeframe::ALL {
            assert_eq!(tf.to_string().parse::<Timeframe>(), Ok(tf));
        }
        assert_eq!("1h".parse::<Timeframe>(), Ok(Timeframe::H1));
        assert_eq!(
            "2h".parse::<Timeframe>(),
            Err(CoreError::UnknownTimeframe("2h".into()))
        );
    }

    #[test]
    fn serde_uses_the_same_strings() {
        for tf in Timeframe::ALL {
            let json = serde_json::to_string(&tf).unwrap();
            assert_eq!(json, format!("\"{tf}\""));
            assert_eq!(serde_json::from_str::<Timeframe>(&json).unwrap(), tf);
        }
    }
}
