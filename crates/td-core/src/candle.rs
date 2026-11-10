use serde::{Deserialize, Serialize};

use crate::{CoreError, Millis};

/// One OHLCV bar. `open_time` is the start of the bar's timeframe bucket.
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Candle {
    pub open_time: Millis,
    pub open: f64,
    pub high: f64,
    pub low: f64,
    pub close: f64,
    /// Traded volume in the base asset.
    pub volume: f64,
    /// Traded volume in the quote asset.
    pub quote_volume: f64,
}

impl Candle {
    /// Rejects bars that cannot be real: non-finite values, a high below the open or close, a low
    /// above them, a low at or below zero, or negative volume.
    pub fn validate(&self) -> Result<(), CoreError> {
        let fail = |reason| {
            Err(CoreError::InvalidCandle {
                open_time: self.open_time,
                reason,
            })
        };
        let values = [
            self.open,
            self.high,
            self.low,
            self.close,
            self.volume,
            self.quote_volume,
        ];
        if values.iter().any(|v| !v.is_finite()) {
            return fail("non-finite value");
        }
        if self.high < self.open.max(self.close) {
            return fail("high is below open or close");
        }
        if self.low > self.open.min(self.close) {
            return fail("low is above open or close");
        }
        // The low is the smallest of the four prices once the checks above pass.
        if self.low <= 0.0 {
            return fail("non-positive price");
        }
        if self.volume < 0.0 || self.quote_volume < 0.0 {
            return fail("negative volume");
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candle() -> Candle {
        Candle {
            open_time: 0,
            open: 100.0,
            high: 110.0,
            low: 95.0,
            close: 105.0,
            volume: 3.0,
            quote_volume: 310.0,
        }
    }

    fn reason(c: Candle) -> &'static str {
        match c.validate() {
            Err(CoreError::InvalidCandle { reason, .. }) => reason,
            other => panic!("expected an invalid candle, got {other:?}"),
        }
    }

    #[test]
    fn valid_candle_passes() {
        assert_eq!(candle().validate(), Ok(()));
        // A flat bar with no trades is valid.
        let flat = Candle {
            open: 100.0,
            high: 100.0,
            low: 100.0,
            close: 100.0,
            volume: 0.0,
            quote_volume: 0.0,
            ..candle()
        };
        assert_eq!(flat.validate(), Ok(()));
    }

    #[test]
    fn high_below_open_or_close_is_rejected() {
        assert_eq!(
            reason(Candle {
                high: 104.0,
                ..candle()
            }),
            "high is below open or close"
        );
    }

    #[test]
    fn low_above_open_or_close_is_rejected() {
        assert_eq!(
            reason(Candle {
                low: 101.0,
                ..candle()
            }),
            "low is above open or close"
        );
    }

    #[test]
    fn non_positive_prices_are_rejected() {
        assert_eq!(
            reason(Candle {
                low: 0.0,
                ..candle()
            }),
            "non-positive price"
        );
        assert_eq!(
            reason(Candle {
                low: -1.0,
                ..candle()
            }),
            "non-positive price"
        );
    }

    #[test]
    fn negative_volume_is_rejected() {
        assert_eq!(
            reason(Candle {
                volume: -1.0,
                ..candle()
            }),
            "negative volume"
        );
        assert_eq!(
            reason(Candle {
                quote_volume: -1.0,
                ..candle()
            }),
            "negative volume"
        );
    }

    #[test]
    fn non_finite_values_are_rejected() {
        assert_eq!(
            reason(Candle {
                close: f64::NAN,
                ..candle()
            }),
            "non-finite value"
        );
        assert_eq!(
            reason(Candle {
                high: f64::INFINITY,
                ..candle()
            }),
            "non-finite value"
        );
    }
}
