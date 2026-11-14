//! A session set beside a backtest of the same strategy, parameters, instrument and bars: orders
//! and fills paired one to one, the costs and the result on each side, and every difference named.
//!
//! The comparison reads only what a session records ([`Recorded`]): its orders, fills, equity
//! points, starting balance and the funding it was charged. A paper session's [`Session`] gives
//! those, and so can any other session that keeps the same records, such as one whose orders go
//! to a venue, so the comparison does not depend on how the session's orders were filled.
//!
//! Orders and fills are paired the way the NautilusTrader cross-check (`harness/crosscheck`) pairs
//! fills: by a key and side, in the order they happened, merging the two lists sorted by that key,
//! so an item with no partner on the other side is left over rather than shifting every pair after
//! it. The key here is the open time of the bar the order was decided on
//! ([`Order::submitted_at`]): both sides decide on the same bars, while the time a fill lands can
//! differ between them. A fill's key is that of its order, or its own time when its order is not
//! in the record.
//!
//! shortcut: one fill per order is assumed for the pairing; an order filled in parts on one side
//! leaves its later parts unmatched. Pair by order and sum the parts when a venue session fills in
//! parts.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use td_core::{Fill, Millis, Side};

use crate::{BacktestResult, EquityPoint, Order, OrderStatus, Session};

/// Two amounts closer than this, relative to the larger of them or to 1 when both are smaller,
/// count as equal: a difference that small is floating-point rounding, not a real one.
const TOLERANCE: f64 = 1e-9;

/// What a session recorded, the part a comparison reads.
#[derive(Debug, Clone, Copy)]
pub struct Recorded<'a> {
    /// Every order submitted, rejected ones included, in submission order.
    pub orders: &'a [Order],
    pub fills: &'a [Fill],
    /// Equity at each live bar's close.
    pub equity: &'a [EquityPoint],
    pub starting_cash: f64,
    /// Net funding paid; negative when the position received funding.
    pub funding_paid: f64,
}

impl<'a> Recorded<'a> {
    /// The record of an engine session that started with `starting_cash`.
    pub fn of_session(session: &'a Session, starting_cash: f64) -> Recorded<'a> {
        Recorded {
            orders: session.orders(),
            fills: session.fills(),
            equity: session.equity(),
            starting_cash,
            funding_paid: session.portfolio().funding_paid(),
        }
    }

    /// The record of a backtest.
    pub fn of_backtest(result: &'a BacktestResult) -> Recorded<'a> {
        Recorded {
            orders: &result.orders,
            fills: &result.fills,
            equity: &result.equity,
            starting_cash: result.config.starting_cash,
            funding_paid: result.metrics.funding_paid,
        }
    }
}

/// A session beside its backtest.
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionComparison {
    pub session: SideSummary,
    pub backtest: SideSummary,
    /// The session's P&L over the backtest's; `null` when the backtest's P&L is zero. Negative
    /// when one made money and the other lost it.
    pub pnl_ratio: Option<f64>,
    /// Fills paired one to one, in the order of their key.
    pub fills: Vec<FillPair>,
    /// Fills of the session with no partner in the backtest.
    pub session_only_fills: Vec<Fill>,
    /// Fills of the backtest with no partner in the session.
    pub backtest_only_fills: Vec<Fill>,
    /// Orders paired one to one, in the order of their key.
    pub orders: Vec<OrderPair>,
    pub session_only_orders: Vec<Order>,
    pub backtest_only_orders: Vec<Order>,
    /// Every difference found, each named; empty when the two sides agree.
    pub differences: Vec<Difference>,
}

/// One side's costs and result.
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SideSummary {
    /// Orders submitted, rejected ones included.
    pub orders: usize,
    pub fills: usize,
    /// Sum of fill fees; negative if rebates outweighed fees.
    pub fees: f64,
    /// Net funding paid; negative when the position received funding.
    pub funding: f64,
    pub starting_equity: f64,
    /// Equity at the last live bar's close, or the starting equity before the first.
    pub final_equity: f64,
    /// `final_equity - starting_equity`.
    pub pnl: f64,
}

/// A session fill and the backtest fill paired with it, with the session's value less the
/// backtest's for each field.
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FillPair {
    /// The open time of the bar the fills' orders were decided on.
    pub decided_at: Millis,
    pub session: Fill,
    pub backtest: Fill,
    pub time_diff_ms: Millis,
    pub price_diff: f64,
    pub qty_diff: f64,
    pub fee_diff: f64,
}

/// A session order and the backtest order paired with it.
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OrderPair {
    pub session: Order,
    pub backtest: Order,
}

/// What a [`Difference`] is about.
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DifferenceKind {
    /// A paired fill landed at another time.
    FillTime,
    /// A paired fill was at another price.
    FillPrice,
    /// A paired fill was for another quantity.
    FillQty,
    /// A paired fill paid another fee.
    FillFee,
    /// A fill of the session has no partner in the backtest.
    SessionOnlyFill,
    /// A fill of the backtest has no partner in the session.
    BacktestOnlyFill,
    /// A paired order asked for another quantity, type, price or reduce-only setting.
    OrderRequest,
    /// A paired order ended in another state: filled on one side and cancelled on the other, say.
    OrderStatus,
    /// An order of the session has no partner in the backtest.
    SessionOnlyOrder,
    /// An order of the backtest has no partner in the session.
    BacktestOnlyOrder,
    /// The fees paid in total differ.
    Fees,
    /// The funding paid in total differs.
    Funding,
    /// The final equity, and so the P&L, differs.
    Result,
}

/// One difference between the session and the backtest, in a plain sentence.
#[cfg_attr(feature = "ts", derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Difference {
    pub kind: DifferenceKind,
    pub message: String,
}

/// Compares what a session recorded with a backtest of the same strategy, parameters, instrument
/// and bars (see the module docs for the pairing).
pub fn compare_with_backtest(session: Recorded, backtest: Recorded) -> SessionComparison {
    let mut differences = Vec::new();
    let mut note = |kind, message: String| differences.push(Difference { kind, message });

    let (order_pairs, session_only_orders, backtest_only_orders) =
        pair_by_key(session.orders, backtest.orders, order_key, order_key);
    let mut orders = Vec::with_capacity(order_pairs.len());
    for (s, b) in order_pairs {
        let at = s.submitted_at;
        let (sr, br) = (&s.request, &b.request);
        if !(same(sr.qty, br.qty)
            && sr.order_type == br.order_type
            && sr.reduce_only == br.reduce_only)
        {
            note(
                DifferenceKind::OrderRequest,
                format!(
                    "the {} order decided on the bar at {at} asks for {} {:?}{} in the session \
                     and {} {:?}{} in the backtest",
                    side_name(sr.side),
                    sr.qty,
                    sr.order_type,
                    if sr.reduce_only { " reduce-only" } else { "" },
                    br.qty,
                    br.order_type,
                    if br.reduce_only { " reduce-only" } else { "" },
                ),
            );
        }
        if status_name(&s.status) != status_name(&b.status) {
            note(
                DifferenceKind::OrderStatus,
                format!(
                    "the {} order decided on the bar at {at} is {} in the session and {} in the \
                     backtest",
                    side_name(sr.side),
                    status_name(&s.status),
                    status_name(&b.status),
                ),
            );
        }
        orders.push(OrderPair {
            session: s.clone(),
            backtest: b.clone(),
        });
    }
    for o in &session_only_orders {
        note(
            DifferenceKind::SessionOnlyOrder,
            format!(
                "the session's {} order {} decided on the bar at {} ({}) has no partner in the \
                 backtest",
                side_name(o.request.side),
                o.id.0,
                o.submitted_at,
                status_name(&o.status),
            ),
        );
    }
    for o in &backtest_only_orders {
        note(
            DifferenceKind::BacktestOnlyOrder,
            format!(
                "the backtest's {} order {} decided on the bar at {} ({}) has no partner in the \
                 session",
                side_name(o.request.side),
                o.id.0,
                o.submitted_at,
                status_name(&o.status),
            ),
        );
    }

    let session_key = decision_times(session.orders);
    let backtest_key = decision_times(backtest.orders);
    let key = |times: &HashMap<u64, Millis>, f: &Fill| {
        (times.get(&f.order_id.0).copied().unwrap_or(f.time), f.side)
    };
    // Each side's fills are keyed by its own orders.
    let (fill_pairs, session_only_fills, backtest_only_fills) = pair_by_key(
        session.fills,
        backtest.fills,
        |f| key(&session_key, f),
        |f| key(&backtest_key, f),
    );
    let mut fills = Vec::with_capacity(fill_pairs.len());
    for (s, b) in fill_pairs {
        let decided_at = key(&session_key, s).0;
        let what = format!(
            "the {} fill of the order decided on the bar at {decided_at}",
            side_name(s.side)
        );
        if s.time != b.time {
            note(
                DifferenceKind::FillTime,
                format!(
                    "{what} landed at {} in the session and {} in the backtest, {} ms apart",
                    s.time,
                    b.time,
                    s.time - b.time
                ),
            );
        }
        if !same(s.price, b.price) {
            note(
                DifferenceKind::FillPrice,
                format!(
                    "{what} was at {} in the session and {} in the backtest",
                    s.price, b.price
                ),
            );
        }
        if !same(s.qty, b.qty) {
            note(
                DifferenceKind::FillQty,
                format!(
                    "{what} was for {} in the session and {} in the backtest",
                    s.qty, b.qty
                ),
            );
        }
        if !same(s.fee, b.fee) {
            note(
                DifferenceKind::FillFee,
                format!(
                    "{what} paid a fee of {} in the session and {} in the backtest",
                    s.fee, b.fee
                ),
            );
        }
        fills.push(FillPair {
            decided_at,
            session: s.clone(),
            backtest: b.clone(),
            time_diff_ms: s.time - b.time,
            price_diff: s.price - b.price,
            qty_diff: s.qty - b.qty,
            fee_diff: s.fee - b.fee,
        });
    }
    for f in &session_only_fills {
        note(
            DifferenceKind::SessionOnlyFill,
            format!(
                "the session's {} fill of {} at {} at {} has no partner in the backtest",
                side_name(f.side),
                f.qty,
                f.price,
                f.time
            ),
        );
    }
    for f in &backtest_only_fills {
        note(
            DifferenceKind::BacktestOnlyFill,
            format!(
                "the backtest's {} fill of {} at {} at {} has no partner in the session",
                side_name(f.side),
                f.qty,
                f.price,
                f.time
            ),
        );
    }

    let (s, b) = (summary(session), summary(backtest));
    if !same(s.fees, b.fees) {
        note(
            DifferenceKind::Fees,
            format!(
                "fees came to {} in the session and {} in the backtest",
                s.fees, b.fees
            ),
        );
    }
    if !same(s.funding, b.funding) {
        note(
            DifferenceKind::Funding,
            format!(
                "funding came to {} in the session and {} in the backtest",
                s.funding, b.funding
            ),
        );
    }
    if !same(s.final_equity, b.final_equity) {
        note(
            DifferenceKind::Result,
            format!(
                "the session ended with equity {} (P&L {}) and the backtest with {} (P&L {})",
                s.final_equity, s.pnl, b.final_equity, b.pnl
            ),
        );
    }
    SessionComparison {
        pnl_ratio: (b.pnl != 0.0).then(|| s.pnl / b.pnl),
        session: s,
        backtest: b,
        fills,
        session_only_fills: session_only_fills.into_iter().cloned().collect(),
        backtest_only_fills: backtest_only_fills.into_iter().cloned().collect(),
        orders,
        session_only_orders: session_only_orders.into_iter().cloned().collect(),
        backtest_only_orders: backtest_only_orders.into_iter().cloned().collect(),
        differences,
    }
}

fn order_key(order: &Order) -> (Millis, Side) {
    (order.submitted_at, order.request.side)
}

/// The open time of the bar each order was decided on, by order id.
fn decision_times(orders: &[Order]) -> HashMap<u64, Millis> {
    orders.iter().map(|o| (o.id.0, o.submitted_at)).collect()
}

/// Pairs `a` with `b` by `(time, side)` keys, `key_a` giving `a`'s and `key_b` giving `b`'s, in
/// order: each list is sorted by its key, keeping
/// the order items of one key happened in, then merged, the first unpaired item of a key pairing
/// with the first of the same key on the other side. Returns the pairs and the items of each side
/// left without a partner.
#[allow(clippy::type_complexity)]
fn pair_by_key<'a, T>(
    a: &'a [T],
    b: &'a [T],
    key_a: impl Fn(&T) -> (Millis, Side),
    key_b: impl Fn(&T) -> (Millis, Side),
) -> (Vec<(&'a T, &'a T)>, Vec<&'a T>, Vec<&'a T>) {
    let sorted = |items: &'a [T], key: &dyn Fn(&T) -> (Millis, Side)| {
        let mut keyed: Vec<((Millis, u8), &'a T)> = items
            .iter()
            .map(|item| {
                let (time, side) = key(item);
                ((time, side_rank(side)), item)
            })
            .collect();
        // Stable, so items of one key keep the order they happened in.
        keyed.sort_by_key(|&(k, _)| k);
        keyed
    };
    let (a, b) = (sorted(a, &key_a), sorted(b, &key_b));
    let (mut pairs, mut only_a, mut only_b) = (Vec::new(), Vec::new(), Vec::new());
    let (mut i, mut j) = (0, 0);
    while i < a.len() && j < b.len() {
        let ((ka, x), (kb, y)) = (a[i], b[j]);
        if ka == kb {
            pairs.push((x, y));
            i += 1;
            j += 1;
        } else if ka < kb {
            only_a.push(x);
            i += 1;
        } else {
            only_b.push(y);
            j += 1;
        }
    }
    only_a.extend(a[i..].iter().map(|&(_, x)| x));
    only_b.extend(b[j..].iter().map(|&(_, y)| y));
    (pairs, only_a, only_b)
}

fn side_rank(side: Side) -> u8 {
    match side {
        Side::Buy => 0,
        Side::Sell => 1,
    }
}

fn side_name(side: Side) -> &'static str {
    match side {
        Side::Buy => "buy",
        Side::Sell => "sell",
    }
}

/// The state an order ended in, without the reason a cancel or rejection gives, which can be
/// worded differently on each side.
fn status_name(status: &OrderStatus) -> &'static str {
    match status {
        OrderStatus::Open => "open",
        OrderStatus::Attached => "attached to an entry that has not filled",
        OrderStatus::Filled => "filled",
        OrderStatus::Cancelled(_) => "cancelled",
        OrderStatus::Rejected(_) => "rejected",
    }
}

fn same(a: f64, b: f64) -> bool {
    (a - b).abs() <= TOLERANCE * a.abs().max(b.abs()).max(1.0)
}

fn summary(r: Recorded) -> SideSummary {
    let final_equity = r.equity.last().map_or(r.starting_cash, |p| p.equity);
    SideSummary {
        orders: r.orders.len(),
        fills: r.fills.len(),
        fees: r.fills.iter().map(|f| f.fee).sum(),
        funding: r.funding_paid,
        starting_equity: r.starting_cash,
        final_equity,
        pnl: final_equity - r.starting_cash,
    }
}

#[cfg(test)]
mod tests {
    use td_core::{
        AccountCurrency, Candle, FeeSchedule, Instrument, InstrumentId, InstrumentKind, Liquidity,
        OrderId, OrderRequest, OrderType, TimeInForce, Timeframe, Venue,
    };

    use super::*;
    use crate::{Context, SessionConfig, Strategy, run_backtest};

    const MIN: Millis = 60_000;

    fn id() -> InstrumentId {
        InstrumentId {
            venue: Venue::Binance,
            symbol: "TESTUSDT".into(),
            kind: InstrumentKind::Perpetual,
        }
    }

    fn order(n: u64, side: Side, decided: i64, status: OrderStatus) -> Order {
        Order {
            id: OrderId(n),
            request: OrderRequest {
                instrument: id(),
                side,
                qty: 1.0,
                order_type: OrderType::Market,
                tif: TimeInForce::Gtc,
                reduce_only: false,
                client_tag: None,
            },
            submitted_at: decided * MIN,
            decision_close: 100.0,
            attached_stop: None,
            status,
        }
    }

    fn fill(order: u64, side: Side, at: Millis, price: f64) -> Fill {
        Fill {
            order_id: OrderId(order),
            instrument: id(),
            side,
            time: at,
            price,
            qty: 1.0,
            fee: price * 0.001,
            liquidity: Liquidity::Taker,
        }
    }

    fn record<'a>(
        orders: &'a [Order],
        fills: &'a [Fill],
        equity: &'a [EquityPoint],
        funding_paid: f64,
    ) -> Recorded<'a> {
        Recorded {
            orders,
            fills,
            equity,
            starting_cash: 1_000.0,
            funding_paid,
        }
    }

    fn kinds(c: &SessionComparison) -> Vec<DifferenceKind> {
        c.differences.iter().map(|d| d.kind).collect()
    }

    #[test]
    fn fills_pair_by_the_bar_their_order_was_decided_on_and_a_missing_one_shifts_nothing() {
        use OrderStatus::Filled;
        // The session misses the backtest's buy decided on bar 2, and its fills land later than
        // the backtest's, as a venue's would.
        let s_orders = [
            order(1, Side::Buy, 1, Filled),
            order(2, Side::Sell, 3, Filled),
        ];
        let s_fills = [
            fill(1, Side::Buy, 2 * MIN + 500, 100.0),
            fill(2, Side::Sell, 4 * MIN + 500, 102.0),
        ];
        let b_orders = [
            order(1, Side::Buy, 1, Filled),
            order(2, Side::Buy, 2, Filled),
            order(3, Side::Sell, 3, Filled),
        ];
        let b_fills = [
            fill(1, Side::Buy, 2 * MIN, 100.0),
            fill(2, Side::Buy, 3 * MIN, 101.0),
            fill(3, Side::Sell, 4 * MIN, 102.0),
        ];
        let c = compare_with_backtest(
            record(&s_orders, &s_fills, &[], 0.0),
            record(&b_orders, &b_fills, &[], 0.0),
        );
        let paired: Vec<(Millis, Millis, f64)> = c
            .fills
            .iter()
            .map(|p| (p.decided_at, p.time_diff_ms, p.price_diff))
            .collect();
        assert_eq!(paired, [(MIN, 500, 0.0), (3 * MIN, 500, 0.0)]);
        assert!(c.session_only_fills.is_empty());
        assert_eq!(c.backtest_only_fills, [b_fills[1].clone()]);
        assert_eq!(c.orders.len(), 2);
        assert_eq!(c.backtest_only_orders, [b_orders[1].clone()]);
        // One fewer buy in the session: a lower fee total too.
        assert_eq!(
            kinds(&c),
            [
                DifferenceKind::BacktestOnlyOrder,
                DifferenceKind::FillTime,
                DifferenceKind::FillTime,
                DifferenceKind::BacktestOnlyFill,
                DifferenceKind::Fees,
            ]
        );
    }

    #[test]
    fn every_kind_of_difference_is_named() {
        use OrderStatus::{Cancelled, Filled};
        let mut bigger = order(2, Side::Sell, 2, Filled);
        bigger.request.qty = 2.0;
        let s_orders = [
            order(1, Side::Buy, 1, Filled),
            bigger,
            order(3, Side::Buy, 3, Cancelled("by hand".into())),
            order(4, Side::Buy, 5, Filled),
        ];
        let mut s_sell = fill(2, Side::Sell, 3 * MIN, 99.0);
        s_sell.qty = 2.0;
        let s_fills = [
            fill(1, Side::Buy, 2 * MIN, 100.5),
            s_sell,
            fill(4, Side::Buy, 6 * MIN, 103.0),
        ];
        let b_orders = [
            order(1, Side::Buy, 1, Filled),
            order(2, Side::Sell, 2, Filled),
            order(3, Side::Buy, 3, Filled),
            order(4, Side::Sell, 4, Filled),
        ];
        let b_fills = [
            fill(1, Side::Buy, 2 * MIN, 100.0),
            fill(2, Side::Sell, 3 * MIN + 1, 99.0),
            fill(3, Side::Buy, 4 * MIN, 101.0),
            fill(4, Side::Sell, 5 * MIN, 102.0),
        ];
        let s_equity = [EquityPoint {
            time: 5 * MIN,
            equity: 990.0,
        }];
        let b_equity = [EquityPoint {
            time: 5 * MIN,
            equity: 1_010.0,
        }];
        let c = compare_with_backtest(
            record(&s_orders, &s_fills, &s_equity, 1.5),
            record(&b_orders, &b_fills, &b_equity, 1.0),
        );
        let found = kinds(&c);
        use DifferenceKind as D;
        for kind in [
            D::FillTime,
            D::FillPrice,
            D::FillQty,
            D::FillFee,
            D::SessionOnlyFill,
            D::BacktestOnlyFill,
            D::OrderRequest,
            D::OrderStatus,
            D::SessionOnlyOrder,
            D::BacktestOnlyOrder,
            D::Fees,
            D::Funding,
            D::Result,
        ] {
            assert!(found.contains(&kind), "{kind:?} not named in {found:?}");
        }
        // Bar 1's buy filled half a point dearer in the session, with a dearer fee.
        let first = &c.fills[0];
        assert_eq!((first.price_diff, first.time_diff_ms), (0.5, 0));
        assert!((first.fee_diff - 0.0005).abs() < 1e-12);
        assert_eq!((c.session.pnl, c.backtest.pnl), (-10.0, 10.0));
        assert_eq!(c.pnl_ratio, Some(-1.0));
        let cancelled = "the buy order decided on the bar at 180000 is cancelled in the session \
                         and filled in the backtest";
        assert!(
            c.differences.iter().any(|d| d.message == cancelled),
            "{:#?}",
            c.differences
        );
    }

    /// Buys on its second bar and sells on its fourth.
    struct InAndOut;

    impl Strategy for InAndOut {
        fn on_bar(&mut self, ctx: &mut Context, _bar: &Candle) {
            let side = match ctx.bars().len() {
                2 => Side::Buy,
                4 => Side::Sell,
                _ => return,
            };
            let _ = ctx.submit(OrderRequest {
                instrument: ctx.instrument().id.clone(),
                side,
                qty: 1.0,
                order_type: OrderType::Market,
                tif: TimeInForce::Gtc,
                reduce_only: false,
                client_tag: None,
            });
        }
    }

    #[test]
    fn the_same_run_twice_has_no_differences() {
        let instrument = Instrument {
            id: id(),
            base: "TEST".into(),
            quote: "USDT".into(),
            tick_size: 0.01,
            lot_size: 0.001,
            min_qty: 0.001,
            min_notional: 1.0,
            fees: FeeSchedule {
                maker: 0.001,
                taker: 0.002,
                per_unit: 0.0,
            },
        };
        let config = SessionConfig {
            starting_cash: 1_000.0,
            slippage_bps: 1.0,
            fees: None,
            max_leverage: 1.0,
            account_currency: AccountCurrency::Quote,
        };
        let bars: Vec<Candle> = (0..6)
            .map(|i| Candle {
                open_time: i * MIN,
                open: 100.0 + i as f64,
                high: 102.0 + i as f64,
                low: 99.0 + i as f64,
                close: 101.0 + i as f64,
                volume: 1.0,
                quote_volume: 100.0,
            })
            .collect();
        let funding = [(3 * MIN, 0.0001)];
        let run = || {
            run_backtest(
                Box::new(InAndOut),
                instrument.clone(),
                Timeframe::M1,
                &bars,
                &[],
                &funding,
                config,
            )
            .unwrap()
        };
        let (a, b) = (run(), run());
        let c = compare_with_backtest(
            Recorded::of_session(&a, 1_000.0),
            Recorded::of_session(&b, 1_000.0),
        );
        assert_eq!(c.differences, []);
        assert_eq!((c.fills.len(), c.orders.len()), (2, 2));
        assert!(c.session.funding != 0.0, "the funding was charged");
        assert_eq!(c.pnl_ratio, Some(1.0));
    }
}
