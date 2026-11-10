// Adapted from tipping-point engine/src/venue/binance/rest.rs at 7dc6d12 (reading the
// PRICE_FILTER and LOT_SIZE filters from exchangeInfo).

//! Instrument metadata: trading rules from Binance's and Bybit's public endpoints, as
//! [`td_core::Instrument`]s with default fees, validated on load.

use std::time::Duration;

use serde::Deserialize;
use td_core::{FeeSchedule, Instrument, InstrumentId, InstrumentKind, Venue};

use crate::http::Http;
use crate::{DataError, store};

// Base-tier fees: Binance's regular user (VIP 0) rates without the BNB discount, and Bybit's
// non-VIP rates, from the venues' fee pages (binance.com/en/fee/schedule and Bybit's "Trading Fee
// Structure" help article). Written on 2026-11-10 from the rates as known then; the pages could
// not be fetched that day to re-check them, so check before relying on them.

/// Binance spot: 0.1% maker, 0.1% taker.
pub const BINANCE_SPOT_FEES: FeeSchedule = FeeSchedule {
    maker: 0.001,
    taker: 0.001,
};
/// Binance USD-M futures: 0.02% maker, 0.05% taker.
pub const BINANCE_USDM_FEES: FeeSchedule = FeeSchedule {
    maker: 0.0002,
    taker: 0.0005,
};
/// Bybit spot: 0.1% maker, 0.1% taker.
pub const BYBIT_SPOT_FEES: FeeSchedule = FeeSchedule {
    maker: 0.001,
    taker: 0.001,
};
/// Bybit linear perpetuals: 0.02% maker, 0.055% taker.
pub const BYBIT_LINEAR_FEES: FeeSchedule = FeeSchedule {
    maker: 0.0002,
    taker: 0.00055,
};

/// The default fee schedule for a venue and kind.
pub fn default_fees(venue: Venue, kind: InstrumentKind) -> FeeSchedule {
    match (venue, kind) {
        (Venue::Binance, InstrumentKind::Spot) => BINANCE_SPOT_FEES,
        (Venue::Binance, InstrumentKind::Perpetual) => BINANCE_USDM_FEES,
        (Venue::Bybit, InstrumentKind::Spot) => BYBIT_SPOT_FEES,
        (Venue::Bybit, InstrumentKind::Perpetual) => BYBIT_LINEAR_FEES,
    }
}

/// The instruments a venue lists, and those it lists that failed validation.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct InstrumentList {
    pub instruments: Vec<Instrument>,
    /// `(symbol, reason)` for each listed instrument that was left out.
    pub rejected: Vec<(String, String)>,
}

impl InstrumentList {
    fn push(&mut self, symbol: &str, built: Result<Instrument, String>) {
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

/// Fetches the instruments a venue lists for one kind: Binance spot or USD-M perpetuals, Bybit spot
/// or linear perpetuals. Only instruments open for trading are listed.
pub fn fetch_instruments(
    http: &Http,
    venue: Venue,
    kind: InstrumentKind,
) -> Result<InstrumentList, DataError> {
    match (venue, kind) {
        (Venue::Binance, InstrumentKind::Spot) => {
            let body = http.get_text("https://api.binance.com/api/v3/exchangeInfo", &[])?;
            parse_binance_spot(&body)
        }
        (Venue::Binance, InstrumentKind::Perpetual) => {
            let body = http.get_text("https://fapi.binance.com/fapi/v1/exchangeInfo", &[])?;
            parse_binance_usdm(&body)
        }
        (Venue::Bybit, _) => {
            let url = "https://api.bybit.com/v5/market/instruments-info";
            let mut list = InstrumentList::default();
            let mut cursor = String::new();
            loop {
                let mut query = vec![
                    ("category", bybit_category(kind).to_string()),
                    ("limit", "1000".to_string()),
                ];
                if !cursor.is_empty() {
                    query.push(("cursor", cursor.clone()));
                }
                let body = http.get_text(url, &query)?;
                let page = bybit_page(url, &body)?;
                add_bybit(&mut list, kind, page.list);
                if page.next_page_cursor.is_empty() {
                    return Ok(list);
                }
                cursor = page.next_page_cursor;
                std::thread::sleep(Duration::from_millis(100));
            }
        }
    }
}

pub(crate) fn bybit_category(kind: InstrumentKind) -> &'static str {
    match kind {
        InstrumentKind::Spot => "spot",
        InstrumentKind::Perpetual => "linear",
    }
}

#[derive(Debug, Deserialize)]
struct BinanceInfo {
    symbols: Vec<BinanceSymbol>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct BinanceSymbol {
    symbol: String,
    status: String,
    base_asset: String,
    quote_asset: String,
    /// USD-M only: `PERPETUAL`, `CURRENT_QUARTER`, `TRADIFI_PERPETUAL`, ...
    contract_type: Option<String>,
    filters: Vec<BinanceFilter>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct BinanceFilter {
    filter_type: String,
    tick_size: Option<String>,
    step_size: Option<String>,
    min_qty: Option<String>,
    /// Spot's NOTIONAL and MIN_NOTIONAL filters.
    min_notional: Option<String>,
    /// USD-M's MIN_NOTIONAL filter.
    notional: Option<String>,
}

/// Reads `/api/v3/exchangeInfo`: every spot symbol with status `TRADING`.
pub fn parse_binance_spot(body: &str) -> Result<InstrumentList, DataError> {
    parse_binance(body, InstrumentKind::Spot)
}

/// Reads `/fapi/v1/exchangeInfo`: every `PERPETUAL` contract with status `TRADING`. Quarterly
/// futures and TradFi perpetuals are left out.
pub fn parse_binance_usdm(body: &str) -> Result<InstrumentList, DataError> {
    parse_binance(body, InstrumentKind::Perpetual)
}

fn parse_binance(body: &str, kind: InstrumentKind) -> Result<InstrumentList, DataError> {
    let info: BinanceInfo =
        serde_json::from_str(body).map_err(|e| DataError::parse("Binance exchangeInfo", e))?;
    let mut list = InstrumentList::default();
    for s in &info.symbols {
        let listed = s.status == "TRADING"
            && match kind {
                InstrumentKind::Spot => true,
                InstrumentKind::Perpetual => s.contract_type.as_deref() == Some("PERPETUAL"),
            };
        if listed {
            list.push(&s.symbol, binance_instrument(s, kind));
        }
    }
    Ok(list)
}

fn binance_instrument(s: &BinanceSymbol, kind: InstrumentKind) -> Result<Instrument, String> {
    let filter = |name: &str| s.filters.iter().find(|f| f.filter_type == name);
    let field = |name: &str, value: Option<&Option<String>>| -> Result<f64, String> {
        let text = value
            .and_then(Option::as_deref)
            .ok_or_else(|| format!("no {name} filter value"))?;
        number(name, text)
    };
    let price = filter("PRICE_FILTER");
    let lot = filter("LOT_SIZE");
    let min_notional = match kind {
        // Spot moved from MIN_NOTIONAL to NOTIONAL in 2023; read whichever is there.
        InstrumentKind::Spot => {
            let f = filter("NOTIONAL").or_else(|| filter("MIN_NOTIONAL"));
            field("NOTIONAL.minNotional", f.map(|f| &f.min_notional))?
        }
        InstrumentKind::Perpetual => field(
            "MIN_NOTIONAL.notional",
            filter("MIN_NOTIONAL").map(|f| &f.notional),
        )?,
    };
    Ok(Instrument {
        id: InstrumentId {
            venue: Venue::Binance,
            symbol: s.symbol.clone(),
            kind,
        },
        base: s.base_asset.clone(),
        quote: s.quote_asset.clone(),
        tick_size: field("PRICE_FILTER.tickSize", price.map(|f| &f.tick_size))?,
        lot_size: field("LOT_SIZE.stepSize", lot.map(|f| &f.step_size))?,
        min_qty: field("LOT_SIZE.minQty", lot.map(|f| &f.min_qty))?,
        min_notional,
        fees: default_fees(Venue::Binance, kind),
    })
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct BybitResponse {
    ret_code: i64,
    ret_msg: String,
    /// Read only once `ret_code` says the call succeeded: error responses carry `{}` here.
    #[serde(default)]
    result: serde_json::Value,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct BybitInstrumentPage {
    list: Vec<BybitInstrument>,
    #[serde(default)]
    next_page_cursor: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct BybitInstrument {
    symbol: String,
    status: String,
    base_coin: String,
    quote_coin: String,
    /// Linear only: `LinearPerpetual` or `LinearFutures`.
    contract_type: Option<String>,
    price_filter: BybitPriceFilter,
    lot_size_filter: BybitLotFilter,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct BybitPriceFilter {
    tick_size: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct BybitLotFilter {
    min_order_qty: String,
    /// Spot's quantity step.
    base_precision: Option<String>,
    /// Spot's minimum order value.
    min_order_amt: Option<String>,
    /// Linear's quantity step.
    qty_step: Option<String>,
    /// Linear's minimum order value.
    min_notional_value: Option<String>,
}

/// Unwraps a Bybit v5 response, turning a non-zero `retCode` into an error.
pub(crate) fn bybit_result<T: serde::de::DeserializeOwned>(
    url: &str,
    body: &str,
) -> Result<T, DataError> {
    let response: BybitResponse =
        serde_json::from_str(body).map_err(|e| DataError::parse(url, e))?;
    if response.ret_code != 0 {
        return Err(DataError::Api {
            url: url.to_string(),
            message: format!("retCode {}: {}", response.ret_code, response.ret_msg),
        });
    }
    serde_json::from_value(response.result).map_err(|e| DataError::parse(url, e))
}

fn bybit_page(url: &str, body: &str) -> Result<BybitInstrumentPage, DataError> {
    bybit_result(url, body)
}

/// Reads one page of `/v5/market/instruments-info`: every instrument with status `Trading`, and
/// for linear only the `LinearPerpetual` contracts.
pub fn parse_bybit_instruments(
    body: &str,
    kind: InstrumentKind,
) -> Result<InstrumentList, DataError> {
    let page = bybit_page("Bybit instruments-info", body)?;
    let mut list = InstrumentList::default();
    add_bybit(&mut list, kind, page.list);
    Ok(list)
}

fn add_bybit(list: &mut InstrumentList, kind: InstrumentKind, page: Vec<BybitInstrument>) {
    for s in page {
        let listed = s.status == "Trading"
            && match kind {
                InstrumentKind::Spot => true,
                InstrumentKind::Perpetual => s.contract_type.as_deref() == Some("LinearPerpetual"),
            };
        if listed {
            list.push(&s.symbol, bybit_instrument(&s, kind));
        }
    }
}

fn bybit_instrument(s: &BybitInstrument, kind: InstrumentKind) -> Result<Instrument, String> {
    let lot = &s.lot_size_filter;
    let (step, min_value) = match kind {
        InstrumentKind::Spot => (&lot.base_precision, &lot.min_order_amt),
        InstrumentKind::Perpetual => (&lot.qty_step, &lot.min_notional_value),
    };
    let required = |name: &str, value: &Option<String>| -> Result<f64, String> {
        number(name, value.as_deref().ok_or_else(|| format!("no {name}"))?)
    };
    Ok(Instrument {
        id: InstrumentId {
            venue: Venue::Bybit,
            symbol: s.symbol.clone(),
            kind,
        },
        base: s.base_coin.clone(),
        quote: s.quote_coin.clone(),
        tick_size: number("tickSize", &s.price_filter.tick_size)?,
        lot_size: required("quantity step", step)?,
        min_qty: number("minOrderQty", &lot.min_order_qty)?,
        min_notional: required("minimum order value", min_value)?,
        fees: default_fees(Venue::Bybit, kind),
    })
}

fn number(name: &str, text: &str) -> Result<f64, String> {
    text.parse::<f64>()
        .map_err(|_| format!("{name} {text:?} is not a number"))
}
