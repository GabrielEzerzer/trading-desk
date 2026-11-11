//! Property tests over random bars, random orders and random funding.
//!
//! - Equity recorded at each close equals cash plus the position marked at that close, where cash
//!   and position are rebuilt independently from the fills and funding. Funding due at a bar's
//!   open is owed by the position before that bar's fills, at the open; funding due later in the
//!   bar by the position after them, at the close.
//! - No order fills on the bar it was submitted on, and every fill lands on a bar's open time.
//! - Spot positions and spot cash never go below zero.
//! - Equity never goes below zero: perpetuals run with random leverage caps and sizes, so some
//!   runs are liquidated, and a perpetual account left flat with a negative balance is floored at
//!   zero.
//! - An attached stop waits exactly while its entry is open, and fills only once its entry has.

mod common;

use common::*;
use proptest::prelude::*;
use td_core::{Candle, InstrumentKind, OrderType, Side, Timeframe};
use td_engine::{Context, OrderStatus, Session, SessionConfig, Strategy as EngineStrategy};

fn leverage_config(slippage: f64, max_leverage: f64) -> SessionConfig {
    SessionConfig {
        max_leverage,
        ..config(slippage)
    }
}

#[derive(Debug, Clone)]
struct Action {
    buy: bool,
    qty: f64,
    kind: u8,
    offset: f64,
    limit_offset: f64,
    ioc: bool,
    reduce_only: bool,
    /// Distance of an attached stop from the entry's price, if the entry has one.
    stop: Option<f64>,
}

struct Random {
    actions: Vec<Vec<Action>>,
    /// Multiplies every action's quantity, so some runs size far above 1x.
    size: f64,
}

impl EngineStrategy for Random {
    fn on_bar(&mut self, ctx: &mut Context, bar: &Candle) {
        let index = ctx.bars().len() - 1;
        for a in &self.actions[index] {
            let trigger = bar.close + a.offset;
            let order_type = match a.kind {
                0 => OrderType::Market,
                1 => OrderType::Limit { price: trigger },
                2 => OrderType::Stop { trigger },
                _ => OrderType::StopLimit {
                    trigger,
                    price: trigger + a.limit_offset,
                },
            };
            let side = if a.buy { Side::Buy } else { Side::Sell };
            let mut r = req(ctx.instrument(), side, a.qty * self.size, order_type);
            r.reduce_only = a.reduce_only;
            if a.ioc {
                r = ioc(r);
            }
            // Rejections are part of the test: they must leave the books consistent.
            let _ = match a.stop {
                Some(distance) => {
                    let entry = match order_type {
                        OrderType::Market => bar.close,
                        OrderType::Limit { price } | OrderType::StopLimit { price, .. } => price,
                        OrderType::Stop { trigger } => trigger,
                    };
                    let stop = entry - side.sign() * distance;
                    ctx.submit_with_stop(r, stop).map(|(entry, _)| entry)
                }
                None => ctx.submit(r),
            };
        }
        // Now and then cancel the oldest open order.
        if index % 7 == 3
            && let Some(id) = ctx.open_orders().first().map(|o| o.id)
        {
            ctx.cancel(id);
        }
    }
}

fn action() -> impl Strategy<Value = Action> {
    (
        any::<bool>(),
        1u32..5000,
        0u8..4,
        -3.0..3.0f64,
        -1.0..1.0f64,
        prop::bool::weighted(0.2),
        prop::bool::weighted(0.2),
        prop::option::weighted(0.3, 0.0..3.0f64),
    )
        .prop_map(
            |(buy, milli, kind, offset, limit_offset, ioc, reduce_only, stop)| Action {
                buy,
                qty: milli as f64 / 1000.0,
                kind,
                offset,
                limit_offset,
                ioc,
                reduce_only,
                stop,
            },
        )
}

/// Per bar: gap, body and wicks in percent, funding rates due at the open and later in the bar,
/// and up to two orders.
type BarSpec = (f64, f64, f64, f64, (Option<f64>, Option<f64>), Vec<Action>);

fn bar_spec() -> impl Strategy<Value = BarSpec> {
    let rate = || prop::option::of(-0.001..0.001f64);
    (
        -1.0..1.0f64,
        -3.0..3.0f64,
        0.0..2.0f64,
        0.0..2.0f64,
        (rate(), rate()),
        prop::collection::vec(action(), 0..3),
    )
}

fn build_bars(specs: &[BarSpec]) -> Vec<Candle> {
    let mut prev = 100.0;
    specs
        .iter()
        .enumerate()
        .map(|(n, &(gap, body, up, down, ..))| {
            let open = prev * (1.0 + gap / 100.0);
            let close = open * (1.0 + body / 100.0);
            prev = close;
            bar(
                n as i64,
                open,
                open.max(close) * (1.0 + up / 100.0),
                open.min(close) * (1.0 - down / 100.0),
                close,
            )
        })
        .collect()
}

fn check(
    kind: InstrumentKind,
    slippage: f64,
    max_leverage: f64,
    size: f64,
    specs: Vec<BarSpec>,
) -> Result<(), TestCaseError> {
    let bars = build_bars(&specs);
    let actions = specs.iter().map(|s| s.5.clone()).collect();
    let inst = instrument(kind);
    let mut session = Session::new(
        Box::new(Random { actions, size }),
        inst,
        Timeframe::M1,
        leverage_config(slippage, max_leverage),
    )
    .unwrap();
    let mut funding = Vec::new();
    for (b, spec) in bars.iter().zip(&specs) {
        let (at_open, later) = spec.4;
        let events: Vec<(i64, f64)> = if kind.rules().funding {
            (at_open.map(|rate| (b.open_time, rate)).into_iter())
                .chain(later.map(|rate| (b.open_time + 30_000, rate)))
                .collect()
        } else {
            vec![]
        };
        session.on_bar(*b, &events).unwrap();
        funding.push(events);
    }

    // Rebuild cash (spot-style: buys pay the notional) and position from fills and funding.
    let mut cash = 10_000.0;
    let mut position = 0.0;
    let fills = session.fills();
    for (t, b) in bars.iter().enumerate() {
        for &(time, rate) in &funding[t] {
            if time == b.open_time {
                cash -= position * b.open * rate;
            }
        }
        for f in fills.iter().filter(|f| f.time == b.open_time) {
            let dq = f.side.sign() * f.qty;
            cash -= dq * f.price + f.fee;
            position += dq;
        }
        // A perpetual account left flat with a negative balance after the bar's fills is floored
        // at zero. Flat, the spot-style cash rebuilt here equals the wallet balance.
        if kind == InstrumentKind::Perpetual && position.abs() < 1e-9 && cash < 0.0 {
            cash = 0.0;
        }
        for &(time, rate) in &funding[t] {
            if time > b.open_time {
                cash -= position * b.close * rate;
            }
        }
        let expected = cash + position * b.close;
        let recorded = session.equity()[t].equity;
        prop_assert!(
            (recorded - expected).abs() <= 1e-7 * (1.0 + expected.abs()),
            "bar {t}: recorded {recorded}, rebuilt {expected}"
        );
        prop_assert!(recorded >= 0.0, "bar {t}: equity {recorded}");
        if kind == InstrumentKind::Spot {
            prop_assert!(position >= -1e-9, "bar {t}: spot position {position}");
            prop_assert!(cash >= 0.0, "bar {t}: spot cash {cash}");
        }
    }
    prop_assert!((session.portfolio().position() - position).abs() < 1e-9);

    for f in fills {
        let order = &session.orders()[f.order_id.0 as usize - 1];
        // A liquidation is decided and filled on the same bar.
        if order.request.client_tag.as_deref() == Some("liquidation") {
            continue;
        }
        prop_assert!(
            f.time > order.submitted_at,
            "order {} submitted on the bar at {} filled at {}",
            f.order_id.0,
            order.submitted_at,
            f.time
        );
        prop_assert!(bars.iter().any(|b| b.open_time == f.time));
    }

    let orders = session.orders();
    for entry in orders {
        let Some(stop) = entry.attached_stop else {
            continue;
        };
        let child = &orders[stop.id.0 as usize - 1];
        prop_assert_eq!(
            entry.status == OrderStatus::Open,
            child.status == OrderStatus::Attached,
            "entry {} is {:?} and its stop {:?}",
            entry.id.0,
            &entry.status,
            &child.status
        );
        prop_assert!(child.request.reduce_only && child.request.side != entry.request.side);
        let entry_fill = fills.iter().find(|f| f.order_id == entry.id);
        for f in fills.iter().filter(|f| f.order_id == stop.id) {
            prop_assert!(
                entry_fill.is_some_and(|e| e.time <= f.time),
                "stop {} filled at {} before its entry",
                stop.id.0,
                f.time
            );
        }
    }
    Ok(())
}

proptest! {
    #[test]
    fn spot_books_balance_and_never_fill_early(
        slippage in 0.0..20.0f64,
        size in 1.0..30.0f64,
        specs in prop::collection::vec(bar_spec(), 1..60),
    ) {
        check(InstrumentKind::Spot, slippage, 1.0, size, specs)?;
    }

    #[test]
    fn perpetual_books_balance_and_never_fill_early(
        slippage in 0.0..20.0f64,
        max_leverage in 0.5..50.0f64,
        size in 1.0..200.0f64,
        specs in prop::collection::vec(bar_spec(), 1..60),
    ) {
        check(InstrumentKind::Perpetual, slippage, max_leverage, size, specs)?;
    }
}
