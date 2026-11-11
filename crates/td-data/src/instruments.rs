//! Instrument metadata: the instruments a source lists, as [`td_core::Instrument`]s with default
//! fees, validated on load. Each source reads its own listing ([`crate::DataSource::instruments`]).

use td_core::{FeeSchedule, Instrument, InstrumentKind, Venue};

use crate::http::Http;
use crate::source::{DataSource, source};
use crate::{DataError, store};

/// The default fee schedule for a venue and kind, or [`DataError::KindNotListed`] for a kind the
/// venue does not list.
pub fn default_fees(venue: Venue, kind: InstrumentKind) -> Result<FeeSchedule, DataError> {
    source(venue).default_fees(kind)
}

/// The instruments a venue lists, and those it lists that failed validation.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct InstrumentList {
    pub instruments: Vec<Instrument>,
    /// `(symbol, reason)` for each listed instrument that was left out.
    pub rejected: Vec<(String, String)>,
}

impl InstrumentList {
    pub(crate) fn push(&mut self, symbol: &str, built: Result<Instrument, String>) {
        match built.and_then(|i| {
            validate_instrument(&i)
                .map(|()| i)
                .map_err(|e| e.to_string())
        }) {
            Ok(instrument) => self.instruments.push(instrument),
            Err(reason) => self.rejected.push((symbol.to_string(), reason)),
        }
    }
}

/// Checks what td-core assumes of an instrument: positive, finite tick and lot sizes and minimums,
/// named assets, and a symbol that can name a store folder.
pub fn validate_instrument(i: &Instrument) -> Result<(), DataError> {
    let fail = |reason: String| DataError::InvalidInstrument {
        symbol: i.id.symbol.clone(),
        reason,
    };
    store::check_symbol(&i.id.symbol)?;
    for (field, value) in [
        ("tick_size", i.tick_size),
        ("lot_size", i.lot_size),
        ("min_qty", i.min_qty),
        ("min_notional", i.min_notional),
    ] {
        if !(value.is_finite() && value > 0.0) {
            return Err(fail(format!(
                "{field} must be a finite number above zero, got {value}"
            )));
        }
    }
    if i.base.is_empty() || i.quote.is_empty() {
        return Err(fail("base and quote assets must be named".into()));
    }
    Ok(())
}

/// Fetches the instruments a venue lists for one kind. Only instruments open for trading are
/// listed.
pub fn fetch_instruments(
    http: &Http,
    venue: Venue,
    kind: InstrumentKind,
) -> Result<InstrumentList, DataError> {
    list_instruments(source(venue), http, kind)
}

/// [`fetch_instruments`] from a given source.
fn list_instruments(
    source: &dyn DataSource,
    http: &Http,
    kind: InstrumentKind,
) -> Result<InstrumentList, DataError> {
    if !source.kinds().contains(&kind) {
        return Err(DataError::KindNotListed {
            venue: source.venue(),
            kind,
        });
    }
    source.instruments(http, kind)
}

/// Reads a trading rule given as text.
pub(crate) fn number(name: &str, text: &str) -> Result<f64, String> {
    text.parse::<f64>()
        .map_err(|_| format!("{name} {text:?} is not a number"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::source::fake::DailyFiles;

    #[test]
    fn a_kind_the_source_does_not_list_is_an_error() {
        let http = Http::new().expect("an HTTP client");
        let source = DailyFiles::new(Vec::new());
        let listed = list_instruments(&source, &http, InstrumentKind::Spot);
        assert_eq!(listed.expect("lists"), InstrumentList::default());
        let result = list_instruments(&source, &http, InstrumentKind::Perpetual);
        assert!(
            matches!(
                result,
                Err(DataError::KindNotListed {
                    venue: Venue::Binance,
                    kind: InstrumentKind::Perpetual
                })
            ),
            "{result:?}"
        );
    }
}
