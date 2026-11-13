//! Instrument metadata: the instruments a source lists, as [`td_core::Instrument`]s with default
//! fees, validated on load. Each source reads its own listing ([`crate::DataSource::instruments`]).
//! [`cached_instruments`] keeps each listing on disk for a day.

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use td_core::{FeeSchedule, Instrument, InstrumentKind, Millis, Venue};

use crate::http::Http;
use crate::source::{DataSource, source};
use crate::time::{DAY, now_ms};
use crate::{DataError, Store, store};

/// How long a listing kept on disk by [`cached_instruments`] is used before it is fetched again.
/// Venues change tick and lot sizes rarely, and list new symbols a few times a month.
pub const INSTRUMENT_CACHE_AGE: Millis = DAY;

/// The default fee schedule for a venue and kind, or [`DataError::KindNotListed`] for a kind the
/// venue does not list.
pub fn default_fees(venue: Venue, kind: InstrumentKind) -> Result<FeeSchedule, DataError> {
    source(venue).default_fees(kind)
}

/// The instruments a venue lists, and those it lists that failed validation.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
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

/// [`fetch_instruments`], kept on disk in the store's data directory, at
/// `instruments/<venue>-<kind>.json`, and read from there while it is less than
/// [`INSTRUMENT_CACHE_AGE`] old. A cache that cannot be read is fetched again, and one that cannot
/// be written is left: the listing is still returned.
pub fn cached_instruments(
    http: &Http,
    store: &Store,
    venue: Venue,
    kind: InstrumentKind,
) -> Result<InstrumentList, DataError> {
    list_cached(source(venue), http, store.root(), kind, now_ms())
}

/// What [`cached_instruments`] keeps in its file.
#[derive(Serialize, Deserialize)]
struct CachedList {
    /// When the listing was fetched, in milliseconds since the Unix epoch.
    fetched_at: Millis,
    list: InstrumentList,
}

/// [`cached_instruments`] from a given source, at `now`.
fn list_cached(
    source: &dyn DataSource,
    http: &Http,
    root: &Path,
    kind: InstrumentKind,
    now: Millis,
) -> Result<InstrumentList, DataError> {
    let path = cache_path(root, source.venue(), kind);
    if source.kinds().contains(&kind) {
        let cached = fs::read(&path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<CachedList>(&bytes).ok())
            .filter(|c| (0..INSTRUMENT_CACHE_AGE).contains(&(now - c.fetched_at)));
        if let Some(cached) = cached {
            return Ok(cached.list);
        }
    }
    let list = list_instruments(source, http, kind)?;
    let cached = CachedList {
        fetched_at: now,
        list,
    };
    if let Ok(json) = serde_json::to_vec(&cached) {
        // A failed write only means the next call fetches again; a torn file reads as no cache.
        let _ = fs::create_dir_all(root.join("instruments")).and_then(|()| fs::write(&path, json));
    }
    Ok(cached.list)
}

fn cache_path(root: &Path, venue: Venue, kind: InstrumentKind) -> PathBuf {
    root.join("instruments")
        .join(format!("{venue}-{kind}.json"))
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

    /// A source that lists one instrument and counts how often it is asked.
    struct Counting {
        files: DailyFiles,
        asked: std::sync::atomic::AtomicUsize,
    }

    impl DataSource for Counting {
        fn venue(&self) -> Venue {
            self.files.venue()
        }
        fn kinds(&self) -> &'static [InstrumentKind] {
            self.files.kinds()
        }
        fn instruments(&self, _: &Http, _: InstrumentKind) -> Result<InstrumentList, DataError> {
            let n = self
                .asked
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            Ok(InstrumentList {
                instruments: Vec::new(),
                rejected: vec![(format!("FETCH{n}"), "a marker".into())],
            })
        }
        fn default_fees(&self, kind: InstrumentKind) -> Result<FeeSchedule, DataError> {
            self.files.default_fees(kind)
        }
        fn minute_history(
            &self,
            http: &Http,
            id: &td_core::InstrumentId,
            from: Millis,
            to: Millis,
            now: Millis,
        ) -> Result<crate::MinuteHistory, DataError> {
            self.files.minute_history(http, id, from, to, now)
        }
    }

    #[test]
    fn a_listing_is_read_from_disk_until_it_is_a_day_old() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let http = Http::new().expect("an HTTP client");
        let source = Counting {
            files: DailyFiles::new(Vec::new()),
            asked: Default::default(),
        };
        let marker = |list: InstrumentList| list.rejected[0].0.clone();
        let list = |now| list_cached(&source, &http, dir.path(), InstrumentKind::Spot, now);
        let t = 1_700_000_000_000;
        assert_eq!(marker(list(t).expect("fetches")), "FETCH0");
        assert!(dir.path().join("instruments/binance-spot.json").is_file());
        // Read from the file, not fetched, up to just under a day later.
        assert_eq!(marker(list(t + DAY - 1).expect("reads")), "FETCH0");
        // A day old: fetched again, and the new listing replaces the file.
        assert_eq!(marker(list(t + DAY).expect("fetches")), "FETCH1");
        assert_eq!(marker(list(t + DAY + 1).expect("reads")), "FETCH1");
        // A file that cannot be read is fetched again.
        fs::write(dir.path().join("instruments/binance-spot.json"), "{").expect("writes");
        assert_eq!(marker(list(t + DAY + 2).expect("fetches")), "FETCH2");
        // A kind the source does not list is refused before the cache.
        let perp = list_cached(&source, &http, dir.path(), InstrumentKind::Perpetual, t);
        assert!(
            matches!(perp, Err(DataError::KindNotListed { .. })),
            "{perp:?}"
        );
    }

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
