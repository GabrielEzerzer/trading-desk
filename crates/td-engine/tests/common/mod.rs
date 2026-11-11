//! Fixtures shared by the integration tests.
#![allow(dead_code)]

use std::sync::{Arc, Mutex};

use td_core::{
    Candle, FeeSchedule, Instrument, InstrumentId, InstrumentKind, OrderId, OrderRequest,
    OrderType, Side, TimeInForce, Venue,
};
use td_engine::{Context, OrderRejected, SessionConfig, Strategy};

pub const MIN: i64 = 60_000;

/// Tick 0.01, lot 0.001, min qty 0.001, min notional 1, maker 0.1%, taker 0.2%.
pub fn instrument(kind: InstrumentKind) -> Instrument {
    Instrument {
        id: InstrumentId {
            venue: Venue::Binance,
            symbol: "TESTUSDT".into(),
            kind,
        },
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
    }
}

pub fn spot() -> Instrument {
    instrument(InstrumentKind::Spot)
}

pub fn perp() -> Instrument {
    instrument(InstrumentKind::Perpetual)
}

/// A one-minute bar `i` minutes after the epoch.
pub fn bar(i: i64, open: f64, high: f64, low: f64, close: f64) -> Candle {
    Candle {
        open_time: i * MIN,
        open,
        high,
        low,
        close,
        volume: 1.0,
        quote_volume: close,
    }
}

pub fn config(slippage_bps: f64) -> SessionConfig {
    SessionConfig {
        starting_cash: 10_000.0,
        slippage_bps,
        fees: None,
        max_leverage: 1.0,
    }
}

pub fn req(inst: &Instrument, side: Side, qty: f64, order_type: OrderType) -> OrderRequest {
    OrderRequest {
        instrument: inst.id.clone(),
        side,
        qty,
        order_type,
        tif: TimeInForce::Gtc,
        reduce_only: false,
        client_tag: None,
    }
}

pub fn reduce_only(mut r: OrderRequest) -> OrderRequest {
    r.reduce_only = true;
    r
}

pub fn ioc(mut r: OrderRequest) -> OrderRequest {
    r.tif = TimeInForce::Ioc;
    r
}

pub type SubmitLog = Arc<Mutex<Vec<Result<OrderId, OrderRejected>>>>;

/// Submits planned orders at the close of the given bar index and records what `submit` returned.
pub struct Script {
    pub plan: Vec<(usize, OrderRequest)>,
    pub log: SubmitLog,
}

impl Script {
    pub fn new(plan: Vec<(usize, OrderRequest)>) -> (Box<Script>, SubmitLog) {
        let log = SubmitLog::default();
        let script = Script {
            plan,
            log: log.clone(),
        };
        (Box::new(script), log)
    }
}

impl Strategy for Script {
    fn on_bar(&mut self, ctx: &mut Context, _bar: &Candle) {
        let index = ctx.bars().len() - 1;
        for (at, r) in &self.plan {
            if *at == index {
                let result = ctx.submit(r.clone());
                self.log.lock().unwrap().push(result);
            }
        }
    }
}

#[track_caller]
pub fn approx(actual: f64, expected: f64) {
    assert!(
        (actual - expected).abs() < 1e-9,
        "expected {expected}, got {actual}"
    );
}
