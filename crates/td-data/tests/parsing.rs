//! Parsing the venues' responses and archives, from recorded fixtures.

use td_core::{Candle, FeeSchedule, Instrument, InstrumentId, InstrumentKind, Venue};
use td_data::binance::{
    BINANCE_SPOT_FEES, BINANCE_USDM_FEES, parse_binance_spot, parse_binance_usdm,
};
use td_data::bybit::{BYBIT_LINEAR_FEES, BYBIT_SPOT_FEES, parse_bybit_instruments};
use td_data::{DataError, binance, bybit, validate_instrument};

fn fixture(name: &str) -> Vec<u8> {
    let path = format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"));
    std::fs::read(&path).unwrap_or_else(|e| panic!("{path}: {e}"))
}

fn text(name: &str) -> String {
    String::from_utf8(fixture(name)).expect("fixture is UTF-8")
}

/// Checks the trading rules and fees of one parsed instrument.
fn assert_rules(i: &Instrument, rules: (f64, f64, f64, f64), fees: FeeSchedule) {
    assert_eq!(
        (i.tick_size, i.lot_size, i.min_qty, i.min_notional),
        rules,
        "{}",
        i.id
    );
    assert_eq!(i.fees, fees);
}

#[test]
fn binance_spot_exchange_info() {
    let list = parse_binance_spot(&text("binance_spot_exchange_info.json")).expect("parses");
    let symbols: Vec<&str> = list
        .instruments
        .iter()
        .map(|i| i.id.symbol.as_str())
        .collect();
    // HALTEDBTC is not trading, so it is not listed at all.
    assert_eq!(symbols, ["ETHBTC", "BTCUSDT"]);
    let btc = &list.instruments[1];
    assert_eq!(
        btc.id,
        InstrumentId {
            venue: Venue::Binance,
            symbol: "BTCUSDT".into(),
            kind: InstrumentKind::Spot
        }
    );
    assert_eq!((btc.base.as_str(), btc.quote.as_str()), ("BTC", "USDT"));
    assert_rules(btc, (0.01, 0.00001, 0.00001, 5.0), BINANCE_SPOT_FEES);
    assert_eq!(list.rejected.len(), 1);
    assert_eq!(list.rejected[0].0, "BROKENBTC");
    assert!(
        list.rejected[0].1.contains("tick_size"),
        "{:?}",
        list.rejected
    );
}

#[test]
fn binance_usdm_exchange_info_lists_only_perpetuals() {
    let list = parse_binance_usdm(&text("binance_usdm_exchange_info.json")).expect("parses");
    // The quarterly BTCUSDT_261225 and the TradFi XAUUSDT are left out.
    assert_eq!(list.instruments.len(), 1);
    let btc = &list.instruments[0];
    assert_eq!(btc.id.kind, InstrumentKind::Perpetual);
    assert_eq!(btc.id.symbol, "BTCUSDT");
    assert_rules(btc, (0.1, 0.001, 0.001, 50.0), BINANCE_USDM_FEES);
    assert!(list.rejected.is_empty());
}

#[test]
fn bybit_instruments_info() {
    let spot = parse_bybit_instruments(&text("bybit_spot_instruments.json"), InstrumentKind::Spot)
        .expect("parses");
    assert_eq!(spot.instruments.len(), 1);
    let btc = &spot.instruments[0];
    assert_eq!(
        (btc.id.venue, btc.id.symbol.as_str()),
        (Venue::Bybit, "BTCUSDT")
    );
    assert_rules(btc, (0.1, 0.000001, 0.000001, 5.0), BYBIT_SPOT_FEES);

    let linear = parse_bybit_instruments(
        &text("bybit_linear_instruments.json"),
        InstrumentKind::Perpetual,
    )
    .expect("parses");
    // BTC-26DEC25 is a closed dated future, not a perpetual.
    assert_eq!(linear.instruments.len(), 1);
    let btc = &linear.instruments[0];
    assert_eq!(btc.id.kind, InstrumentKind::Perpetual);
    assert_rules(btc, (0.1, 0.001, 0.001, 5.0), BYBIT_LINEAR_FEES);
}

#[test]
fn bybit_error_responses_are_errors() {
    let body = r#"{"retCode":10001,"retMsg":"params error","result":{},"time":1}"#;
    let err = parse_bybit_instruments(body, InstrumentKind::Spot);
    assert!(matches!(err, Err(DataError::Api { .. })), "{err:?}");
}

#[test]
fn instrument_validation_rejects_what_td_core_cannot_use() {
    let list = parse_binance_usdm(&text("binance_usdm_exchange_info.json")).expect("parses");
    let good = list.instruments[0].clone();
    assert!(validate_instrument(&good).is_ok());
    let cases = [
        Instrument {
            tick_size: 0.0,
            ..good.clone()
        },
        Instrument {
            lot_size: -0.001,
            ..good.clone()
        },
        Instrument {
            min_qty: f64::NAN,
            ..good.clone()
        },
        Instrument {
            min_notional: f64::INFINITY,
            ..good.clone()
        },
        Instrument {
            base: String::new(),
            ..good.clone()
        },
    ];
    for bad in cases {
        assert!(
            matches!(
                validate_instrument(&bad),
                Err(DataError::InvalidInstrument { .. })
            ),
            "{bad:?}"
        );
    }
    let mut path_escape = good;
    path_escape.id.symbol = "../BTC".into();
    assert!(matches!(
        validate_instrument(&path_escape),
        Err(DataError::InvalidSymbol(_))
    ));
}

fn first(candles: &[Candle]) -> Candle {
    *candles.first().expect("at least one candle")
}

#[test]
fn archive_csv_in_milliseconds() {
    let candles =
        binance::parse_kline_csv("ms", &text("binance_spot_klines_ms.csv")).expect("parses");
    assert_eq!(candles.len(), 3);
    assert_eq!(
        first(&candles),
        Candle {
            open_time: 1_704_067_200_000,
            open: 42283.58,
            high: 42298.62,
            low: 42261.02,
            close: 42298.61,
            volume: 35.92724,
            quote_volume: 1519031.6945192,
        }
    );
    assert_eq!(candles[2].open_time, 1_704_067_320_000);
}

#[test]
fn archive_csv_in_microseconds_is_normalised_to_milliseconds() {
    let candles =
        binance::parse_kline_csv("us", &text("binance_spot_klines_us.csv")).expect("parses");
    let times: Vec<i64> = candles.iter().map(|c| c.open_time).collect();
    // 2025-01-01 00:00, 00:01 and 00:02 UTC.
    assert_eq!(
        times,
        [1_735_689_600_000, 1_735_689_660_000, 1_735_689_720_000]
    );
    assert_eq!(first(&candles).quote_volume, 768978.7552247);
    assert_eq!(
        binance::normalise_time(1_735_689_600_000_000),
        1_735_689_600_000
    );
    assert_eq!(
        binance::normalise_time(1_704_067_200_000),
        1_704_067_200_000
    );
}

#[test]
fn archive_csv_header_is_skipped() {
    let candles =
        binance::parse_kline_csv("um", &text("binance_usdm_klines_header.csv")).expect("parses");
    assert_eq!(candles.len(), 3);
    assert_eq!(first(&candles).open, 42314.0);
    assert_eq!(first(&candles).volume, 289.641);
}

#[test]
fn archive_csv_rejects_bad_rows() {
    for bad in [
        "1704067200000,1,2,0.5,1.5\n",
        "1704067200000,x,2,0.5,1.5,1,1704067259999,1,1,1,1,0\n",
        "1704067200001,1,2,0.5,1.5,1,1704067259999,1,1,1,1,0\n",
        "1,2\nopen_time,open\n",
    ] {
        assert!(binance::parse_kline_csv("bad", bad).is_err(), "{bad}");
    }
    // Rows need the first 8 of the 12 fields, up to the quote volume.
    let short = binance::parse_kline_csv("short", "1704067200000,1,2,0.5,1.5\n");
    assert!(
        matches!(&short, Err(DataError::Parse { reason, .. }) if reason == "5 fields, expected at least 8"),
        "{short:?}"
    );
    let eight = "1704067200000,1,2,0.5,1.5,1,1704067259999,1\n";
    assert_eq!(
        binance::parse_kline_csv("eight", eight)
            .map(|c| c.len())
            .ok(),
        Some(1)
    );
}

#[test]
fn archive_zip_matches_its_checksum_and_holds_its_csv() {
    let file = "BTCUSDT-1m-2024-01-01.zip";
    let zip = fixture(file);
    let checksum = text("BTCUSDT-1m-2024-01-01.zip.CHECKSUM");
    binance::verify_checksum(file, &zip, &checksum).expect("checksum matches");
    let csv = binance::read_archive_csv(file, &zip).expect("one CSV named after the zip");
    let candles = binance::parse_kline_csv(file, &csv).expect("parses");
    assert_eq!(candles.len(), 5);
    assert_eq!(first(&candles).open_time, 1_704_067_200_000);
    // A zip under another name holds the wrong member.
    assert!(matches!(
        binance::read_archive_csv("ETHUSDT-1m-2024-01-01.zip", &zip),
        Err(DataError::Archive { .. })
    ));
}

#[test]
fn checksum_mismatch_is_an_error() {
    let file = "BTCUSDT-1m-2024-01-01.zip";
    let mut zip = fixture(file);
    let checksum = text("BTCUSDT-1m-2024-01-01.zip.CHECKSUM");
    let last = zip.len() - 1;
    zip[last] ^= 1;
    assert!(matches!(
        binance::verify_checksum(file, &zip, &checksum),
        Err(DataError::ChecksumMismatch { .. })
    ));
    let zip = fixture(file);
    assert!(matches!(
        binance::verify_checksum(file, &zip, "not a checksum"),
        Err(DataError::Archive { .. })
    ));
    // An upper-case checksum is the same hash.
    binance::verify_checksum(file, &zip, &checksum.to_uppercase()).expect("case-insensitive");
}

#[test]
fn archive_urls_follow_the_published_layout() {
    let id = |kind| InstrumentId {
        venue: Venue::Binance,
        symbol: "BTCUSDT".into(),
        kind,
    };
    let month = td_data::time::Month::parse("2024-01").expect("a month");
    assert_eq!(
        binance::archive_url(
            &id(InstrumentKind::Spot),
            binance::ArchivePeriod::Month(month)
        )
        .expect("binance lists spot"),
        "https://data.binance.vision/data/spot/monthly/klines/BTCUSDT/1m/BTCUSDT-1m-2024-01.zip"
    );
    assert_eq!(
        binance::archive_url(
            &id(InstrumentKind::Perpetual),
            binance::ArchivePeriod::Day(1_704_153_600_000)
        )
        .expect("binance lists perpetuals"),
        "https://data.binance.vision/data/futures/um/daily/klines/BTCUSDT/1m/BTCUSDT-1m-2024-01-02.zip"
    );
}

#[test]
fn bybit_kline_page_is_newest_first() {
    let candles = bybit::parse_kline_page(&text("bybit_kline_page.json")).expect("parses");
    let times: Vec<i64> = candles.iter().map(|c| c.open_time).collect();
    assert_eq!(
        times,
        [
            1_704_067_440_000,
            1_704_067_380_000,
            1_704_067_320_000,
            1_704_067_260_000,
            1_704_067_200_000
        ]
    );
    assert_eq!(
        candles[4],
        Candle {
            open_time: 1_704_067_200_000,
            open: 42284.0,
            high: 42300.0,
            low: 42260.01,
            close: 42300.0,
            volume: 15.059749,
            quote_volume: 636760.29544192,
        }
    );
}

#[test]
fn funding_history_from_both_venues() {
    let binance = binance::parse_funding(&text("binance_funding.json")).expect("parses");
    assert_eq!(binance.len(), 5);
    assert_eq!(binance[0], (1_704_067_200_000, 0.00037409));
    assert_eq!(binance[4], (1_704_182_400_000, 0.00035179));

    let bybit = bybit::parse_funding_page(&text("bybit_funding.json")).expect("parses");
    assert_eq!(bybit.len(), 5);
    // Newest first, as Bybit sends it.
    assert_eq!(bybit[0], (1_704_182_400_000, 0.00049958));
    assert_eq!(bybit[4], (1_704_067_200_000, 0.00043294));
}

#[test]
fn funding_times_are_rounded_to_the_nearest_minute_on_both_venues() {
    // 2024-01-01 08:00 UTC, stamped 8 ms late, 5 ms early and on time.
    let t = 1_704_096_000_000;
    let binance = binance::parse_funding(&format!(
        r#"[{{"symbol":"BTCUSDT","fundingTime":{},"fundingRate":"0.0001"}},
            {{"symbol":"BTCUSDT","fundingTime":{},"fundingRate":"0.0002"}},
            {{"symbol":"BTCUSDT","fundingTime":{},"fundingRate":"0.0003"}}]"#,
        t + 8,
        t + 8 * 3_600_000 - 5,
        t + 16 * 3_600_000,
    ))
    .expect("parses");
    assert_eq!(
        binance,
        [
            (t, 0.0001),
            (t + 8 * 3_600_000, 0.0002),
            (t + 16 * 3_600_000, 0.0003)
        ]
    );

    let bybit = bybit::parse_funding_page(&format!(
        r#"{{"retCode":0,"retMsg":"OK","result":{{"category":"linear","list":[
            {{"symbol":"BTCUSDT","fundingRate":"0.0003","fundingRateTimestamp":"{}"}},
            {{"symbol":"BTCUSDT","fundingRate":"0.0002","fundingRateTimestamp":"{}"}},
            {{"symbol":"BTCUSDT","fundingRate":"0.0001","fundingRateTimestamp":"{}"}}]}}}}"#,
        t + 16 * 3_600_000,
        t + 8 * 3_600_000 - 5,
        t + 7,
    ))
    .expect("parses");
    assert_eq!(
        bybit,
        [
            (t + 16 * 3_600_000, 0.0003),
            (t + 8 * 3_600_000, 0.0002),
            (t, 0.0001)
        ]
    );
}
