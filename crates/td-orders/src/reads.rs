//! Reads: position, open orders, one order by client id, executions, equity.
//!
//! Visibility, from the v5 pages read on 2026-11-14:
//! - `GET /v5/order/realtime` lists open orders (conditional ones included) and, when asked by
//!   `orderLinkId`, also recent closed ones (the most recent 500 per account and category). That
//!   closed-order cache is cleared when the venue restarts.
//! - `GET /v5/order/history` covers 7 days by default, keeps cancelled, rejected and deactivated
//!   orders for 24 hours only, and "may delay" because order creation and cancellation are
//!   asynchronous.
//! - `GET /v5/execution/list` returns executions newest first, 100 a page at most, paged by
//!   `cursor`, over a window of at most 7 days.
//! - `GET /v5/position/list` with a symbol returns the symbol's positions even when flat;
//!   `positionIdx` 0 is one-way mode, 1 and 2 the two sides of hedge mode.
//! - `GET /v5/account/wallet-balance?accountType=UNIFIED` gives per coin `walletBalance`,
//!   `unrealisedPnl` and `equity` (wallet balance less spot borrowing plus unrealised P&L and
//!   option value), and the account's `totalEquity` in USD.

use std::collections::HashSet;

use td_core::{Instrument, Millis};

use crate::orders::{CATEGORY, check_instrument};
use crate::wire::{
    self, Equity, Execution, Page, Position, RawExecution, RawOrder, RawPosition, RawWallet,
    VenueOrder,
};
use crate::{Client, ClientOrderId, Error};

const POSITIONS: &str = "/v5/position/list";
const REALTIME: &str = "/v5/order/realtime";
const HISTORY: &str = "/v5/order/history";
const EXECUTIONS: &str = "/v5/execution/list";
const WALLET: &str = "/v5/account/wallet-balance";

/// The coin USDT perpetuals settle in.
pub const SETTLE_COIN: &str = "USDT";
/// The longest span one executions request may cover.
const WINDOW_MS: Millis = 7 * 24 * 60 * 60 * 1000;
/// A bound on pages per listing, so a venue that keeps answering with cursors cannot loop forever.
const MAX_PAGES: usize = 10_000;

impl Client {
    /// The account's one-way position on the instrument (size zero when flat). An account in
    /// hedge mode on the symbol is [`Error::HedgeMode`].
    pub fn position(&self, instrument: &Instrument) -> Result<Position, Error> {
        check_instrument(instrument)?;
        let symbol = &instrument.id.symbol;
        let value = self.get(
            POSITIONS,
            vec![("category", CATEGORY.into()), ("symbol", symbol.clone())],
        )?;
        let page: Page<RawPosition> = wire::from_value(POSITIONS, value)?;
        let mut found = None;
        for raw in page.list.into_iter().filter(|p| &p.symbol == symbol) {
            if raw.position_idx != 0 {
                return Err(Error::HedgeMode {
                    symbol: symbol.clone(),
                });
            }
            if found.is_some() {
                return Err(Error::BadResponse {
                    endpoint: POSITIONS,
                    reason: "two one-way positions for one symbol".into(),
                });
            }
            found = Some(raw.into_position(POSITIONS)?);
        }
        Ok(found.unwrap_or(Position {
            symbol: symbol.clone(),
            size: 0.0,
            avg_price: 0.0,
            unrealised_pnl: 0.0,
            mark_price: 0.0,
        }))
    }

    /// The account's open orders on the instrument, resting conditional orders included, every
    /// page.
    pub fn open_orders(&self, instrument: &Instrument) -> Result<Vec<VenueOrder>, Error> {
        check_instrument(instrument)?;
        let query = vec![
            ("category", CATEGORY.to_string()),
            ("symbol", instrument.id.symbol.clone()),
            ("limit", "50".into()),
        ];
        let raws: Vec<RawOrder> = self.all_pages(REALTIME, query)?;
        raws.into_iter().map(|r| r.into_order(REALTIME)).collect()
    }

    /// The order with client id `id`, open or closed: the real-time list first, then the order
    /// history. `None` when neither lists it; a just-placed order may take a moment to appear in
    /// the history.
    pub fn order_by_client_id(
        &self,
        instrument: &Instrument,
        id: &ClientOrderId,
    ) -> Result<Option<VenueOrder>, Error> {
        check_instrument(instrument)?;
        self.order_by_client_id_with(instrument, id, false)
    }

    pub(crate) fn order_by_client_id_with(
        &self,
        instrument: &Instrument,
        id: &ClientOrderId,
        once: bool,
    ) -> Result<Option<VenueOrder>, Error> {
        for path in [REALTIME, HISTORY] {
            let query = vec![
                ("category", CATEGORY.to_string()),
                ("symbol", instrument.id.symbol.clone()),
                ("orderLinkId", id.as_str().to_string()),
            ];
            let value = if once {
                self.get_once(path, query)?
            } else {
                self.get(path, query)?
            };
            let page: Page<RawOrder> = wire::from_value(path, value)?;
            // Asked by orderLinkId the venue lists that order only; check rather than trust it.
            if let Some(raw) = page.list.into_iter().next() {
                let order = raw.into_order(path)?;
                if order.client_id == id.as_str() {
                    return Ok(Some(order));
                }
            }
        }
        Ok(None)
    }

    /// The executions for one client id in the last 7 days, one request.
    pub(crate) fn executions_for_id(
        &self,
        instrument: &Instrument,
        id: &ClientOrderId,
    ) -> Result<Vec<Execution>, Error> {
        let query = vec![
            ("category", CATEGORY.to_string()),
            ("symbol", instrument.id.symbol.clone()),
            ("orderLinkId", id.as_str().to_string()),
        ];
        let page: Page<RawExecution> =
            wire::from_value(EXECUTIONS, self.get_once(EXECUTIONS, query)?)?;
        page.list
            .into_iter()
            .map(|r| r.into_execution(EXECUTIONS))
            .filter(|e| e.as_ref().map_or(true, |e| e.client_id == id.as_str()))
            .collect()
    }

    /// Every execution on the instrument at or after `since`, oldest first (ties by execution
    /// id), each once. Spans longer than the venue's 7-day window are read window by window, and
    /// each window to its last page. Trades, funding fees, liquidations and the rest are told
    /// apart by [`Execution::kind`].
    pub fn executions_since(
        &self,
        instrument: &Instrument,
        since: Millis,
    ) -> Result<Vec<Execution>, Error> {
        check_instrument(instrument)?;
        let now = self.server_now();
        let mut out = Vec::new();
        let mut seen = HashSet::new();
        let mut start = since;
        loop {
            let end = start + WINDOW_MS;
            let query = vec![
                ("category", CATEGORY.to_string()),
                ("symbol", instrument.id.symbol.clone()),
                ("startTime", start.to_string()),
                ("endTime", end.to_string()),
                ("limit", "100".into()),
            ];
            let raws: Vec<RawExecution> = self.all_pages(EXECUTIONS, query)?;
            for raw in raws {
                let e = raw.into_execution(EXECUTIONS)?;
                if e.time >= since && seen.insert(e.exec_id.clone()) {
                    out.push(e);
                }
            }
            if end >= now {
                break;
            }
            start = end + 1;
        }
        out.sort_by(|a, b| a.time.cmp(&b.time).then_with(|| a.exec_id.cmp(&b.exec_id)));
        Ok(out)
    }

    /// The account's equity in USDT, and its total equity in USD across coins.
    pub fn equity(&self) -> Result<Equity, Error> {
        let value = self.get(
            WALLET,
            vec![
                ("accountType", "UNIFIED".into()),
                ("coin", SETTLE_COIN.into()),
            ],
        )?;
        let page: Page<RawWallet> = wire::from_value(WALLET, value)?;
        let account = page.list.into_iter().next().ok_or(Error::BadResponse {
            endpoint: WALLET,
            reason: "no account in the answer".into(),
        })?;
        let total = wire::num(WALLET, "totalEquity", &account.total_equity)?;
        let (wallet_balance, unrealised_pnl) =
            match account.coin.into_iter().find(|c| c.coin == SETTLE_COIN) {
                Some(c) => (
                    wire::num(WALLET, "walletBalance", &c.wallet_balance)?,
                    wire::num_or_zero(WALLET, "unrealisedPnl", &c.unrealised_pnl)?,
                ),
                // The venue lists only coins the account holds.
                None => (0.0, 0.0),
            };
        Ok(Equity {
            coin: SETTLE_COIN.into(),
            wallet_balance,
            unrealised_pnl,
            equity: wallet_balance + unrealised_pnl,
            account_total_equity_usd: total,
        })
    }

    /// Reads every page of a cursor-paged list.
    fn all_pages<T: serde::de::DeserializeOwned>(
        &self,
        path: &'static str,
        query: Vec<(&'static str, String)>,
    ) -> Result<Vec<T>, Error> {
        let mut out = Vec::new();
        let mut cursor = String::new();
        let mut cursors = HashSet::new();
        for _ in 0..MAX_PAGES {
            let mut q = query.clone();
            if !cursor.is_empty() {
                q.push(("cursor", cursor.clone()));
            }
            let page: Page<T> = wire::from_value(path, self.get(path, q)?)?;
            let empty = page.list.is_empty();
            out.extend(page.list);
            if page.next_page_cursor.is_empty() || empty {
                return Ok(out);
            }
            if !cursors.insert(page.next_page_cursor.clone()) {
                return Err(Error::BadResponse {
                    endpoint: path,
                    reason: "the venue repeated a page cursor".into(),
                });
            }
            cursor = page.next_page_cursor;
        }
        Err(Error::BadResponse {
            endpoint: path,
            reason: "more pages than the route reads".into(),
        })
    }
}
