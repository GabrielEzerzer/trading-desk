//! Downloads from the real venues. Ignored by default; run with
//! `cargo test -p td-data --test network -- --ignored`.

use td_core::{InstrumentId, InstrumentKind, Timeframe, Venue};
use td_data::time::{DAY, parse_date};
use td_data::{Http, Store, download_candles, download_funding, fetch_instruments};

#[test]
#[ignore = "downloads from Binance and Bybit"]
fn one_day_of_btcusdt_from_each_source() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let store = Store::new(dir.path());
    let http = Http::new().expect("an HTTP client");
    let from = parse_date("2024-01-01").expect("a date");
    let to = from + DAY;
    for venue in [Venue::Binance, Venue::Bybit] {
        for kind in [InstrumentKind::Spot, InstrumentKind::Perpetual] {
            let id = InstrumentId {
                venue,
                symbol: "BTCUSDT".into(),
                kind,
            };
            let report = download_candles(&http, &store, &id, from, to, &mut |_| {})
                .unwrap_or_else(|e| panic!("{id}: {e}"));
            assert!(report.gaps.is_empty(), "{id}: {:?}", report.gaps);
            let candles = store
                .load_candles(&id, Timeframe::M1, from, to, false)
                .expect("loads");
            assert_eq!(candles.candles.len(), 1440, "{id}");
            let hours = store
                .load_candles(&id, Timeframe::H1, from, to, false)
                .expect("loads");
            assert_eq!(hours.candles.len(), 24, "{id}");
            assert!(
                hours.dropped.is_empty() && hours.tolerated.is_empty(),
                "{id}"
            );
            if kind == InstrumentKind::Perpetual {
                let rates = download_funding(&http, &store, &id, from, to)
                    .unwrap_or_else(|e| panic!("{id}: {e}"));
                // Both venues funded BTCUSDT every 8 hours then.
                assert_eq!(rates, 3, "{id}");
                assert_eq!(store.load_funding(&id, from, to).expect("loads").len(), 3);
            }
        }
    }
}

#[test]
#[ignore = "downloads from Binance and Bybit"]
fn instruments_from_each_venue() {
    let http = Http::new().expect("an HTTP client");
    for venue in [Venue::Binance, Venue::Bybit] {
        for kind in [InstrumentKind::Spot, InstrumentKind::Perpetual] {
            let list = fetch_instruments(&http, venue, kind)
                .unwrap_or_else(|e| panic!("{venue} {kind}: {e}"));
            assert!(
                list.instruments.iter().any(|i| i.id.symbol == "BTCUSDT"),
                "{venue} {kind}"
            );
            println!(
                "{venue} {kind}: {} instruments, {} left out {:?}",
                list.instruments.len(),
                list.rejected.len(),
                list.rejected.iter().take(5).collect::<Vec<_>>()
            );
        }
    }
}
