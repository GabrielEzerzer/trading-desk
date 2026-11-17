//! Short sales of US stocks and ETFs in a backtest: which symbols can be sold short, and the borrow fee.

use serde::{Deserialize, Serialize};
use td_core::{InstrumentId, InstrumentKind, Millis};

use crate::metrics::DAY_MS;

const BORROW_DAYS_PER_YEAR: f64 = 360.0;

#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StockShorting {
    pub shortable: Shortable,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub borrow_rate: f64,
}

fn is_zero(rate: &f64) -> bool {
    *rate == 0.0
}

#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Shortable {
    All,
    Only(Vec<String>),
}

impl StockShorting {
    pub fn new(shortable: Shortable) -> Self {
        StockShorting {
            shortable,
            borrow_rate: 0.0,
        }
    }

    pub fn allows(&self, id: &InstrumentId) -> bool {
        id.kind == InstrumentKind::Stock
            && match &self.shortable {
                Shortable::All => true,
                Shortable::Only(symbols) => symbols.contains(&id.symbol),
            }
    }

    pub fn check(&self, kind: InstrumentKind) -> Result<(), &'static str> {
        if kind != InstrumentKind::Stock {
            return Err("short selling of stocks is set on a run that does not trade stocks");
        }
        if !(self.borrow_rate.is_finite() && self.borrow_rate >= 0.0) {
            return Err("the borrow rate must be a finite number, zero or more");
        }
        Ok(())
    }

    pub(crate) fn borrow_fraction(&self, held_from: Millis, to: Millis) -> f64 {
        let days = to.div_euclid(DAY_MS) - held_from.div_euclid(DAY_MS);
        self.borrow_rate * days as f64 / BORROW_DAYS_PER_YEAR
    }
}

#[cfg(test)]
mod tests {
    use td_core::Venue;

    use super::*;

    fn stock(symbol: &str) -> InstrumentId {
        InstrumentId {
            venue: Venue::Alpaca,
            symbol: symbol.into(),
            kind: InstrumentKind::Stock,
        }
    }

    #[test]
    fn the_list_names_the_symbols_that_can_be_sold_short_and_only_stocks_qualify() {
        let all = StockShorting::new(Shortable::All);
        let only = StockShorting::new(Shortable::Only(vec!["SPY".into()]));
        assert!(all.allows(&stock("AAPL")) && only.allows(&stock("SPY")));
        assert!(!only.allows(&stock("AAPL")));
        let perp = InstrumentId {
            kind: InstrumentKind::Perpetual,
            ..stock("SPY")
        };
        assert!(!all.allows(&perp));
    }

    #[test]
    fn the_borrow_fee_counts_calendar_days_on_a_360_day_year() {
        let s = StockShorting {
            borrow_rate: 0.36,
            ..StockShorting::new(Shortable::All)
        };
        let friday = crate::date(2024, 1, 19);
        let monday = crate::date(2024, 1, 22);
        assert!((s.borrow_fraction(friday, monday) - 0.003).abs() < 1e-15);
        assert!((s.borrow_fraction(friday + DAY_MS - 1, friday + DAY_MS) - 0.001).abs() < 1e-15);
        assert_eq!(
            StockShorting::new(Shortable::All).borrow_fraction(friday, monday),
            0.0
        );
    }

    #[test]
    fn json_names_the_list_or_all_and_the_rate_defaults_to_zero() {
        let all: StockShorting = serde_json::from_str(r#"{"shortable":"all"}"#).unwrap();
        assert_eq!(all, StockShorting::new(Shortable::All));
        assert_eq!(
            serde_json::to_string(&all).unwrap(),
            r#"{"shortable":"all"}"#
        );
        let only: StockShorting =
            serde_json::from_str(r#"{"shortable":{"only":["SPY","QQQ"]},"borrow_rate":0.01}"#)
                .unwrap();
        assert_eq!(
            only.shortable,
            Shortable::Only(vec!["SPY".into(), "QQQ".into()])
        );
        assert_eq!(only.borrow_rate, 0.01);
        assert!(serde_json::from_str::<StockShorting>(r#"{"shortable":"some"}"#).is_err());
        assert!(serde_json::from_str::<StockShorting>(r#"{"borrow_rate":0.0}"#).is_err());
    }

    #[test]
    fn a_run_that_trades_no_stocks_or_a_negative_rate_is_refused() {
        let s = StockShorting::new(Shortable::All);
        assert!(s.check(InstrumentKind::Stock).is_ok());
        assert!(s.check(InstrumentKind::Perpetual).is_err());
        let negative = StockShorting {
            borrow_rate: -0.01,
            ..s
        };
        assert!(negative.check(InstrumentKind::Stock).is_err());
    }
}
