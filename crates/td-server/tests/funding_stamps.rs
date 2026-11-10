//! Funding read from a venue response with a stamp a few milliseconds after a bar's open is
//! charged like funding stamped exactly at the open: on the position held going into the bar,
//! before that bar's fills. No network: the responses are written inline.

use td_core::{
    Candle, FeeSchedule, Instrument, InstrumentId, InstrumentKind, Millis, OrderRequest, OrderType,
    Side, TimeInForce, Timeframe, Venue,
};
use td_data::{binance, bybit};
use td_engine::{Context, SessionConfig, Strategy, run_backtest};

const MIN: Millis = 60_000;
/// 2024-01-01 00:00 UTC, a funding time on both venues.
const T: Millis = 1_704_067_200_000;
const RATE: f64 = 0.01;

fn perp() -> Instrument {
    Instrument {
        id: InstrumentId {
            venue: Venue::Binance,
            symbol: "TESTUSDT".into(),
            kind: InstrumentKind::Perpetual,
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
        },
    }
}

/// Submits a market order of 1 on `side` at the close of each listed bar index.
struct Plan(Vec<(usize, Side)>);

impl Strategy for Plan {
    fn on_bar(&mut self, ctx: &mut Context, _bar: &Candle) {
        let index = ctx.bars().len() - 1;
        for &(at, side) in &self.0 {
            if at == index {
                let request = OrderRequest {
                    instrument: perp().id,
                    side,
                    qty: 1.0,
                    order_type: OrderType::Market,
                    tif: TimeInForce::Gtc,
                    reduce_only: false,
                    client_tag: None,
                };
                ctx.submit(request).expect("accepted");
            }
        }
    }
}

/// The funding paid over three flat one-minute bars at 100, the last opening at `T`, by `plan`.
fn funding_paid(plan: Vec<(usize, Side)>, funding: &[(Millis, f64)]) -> f64 {
    let bars: Vec<Candle> = (0..3)
        .map(|n| Candle {
            open_time: T - (2 - n) * MIN,
            open: 100.0,
            high: 100.0,
            low: 100.0,
            close: 100.0,
            volume: 1.0,
            quote_volume: 100.0,
        })
        .collect();
    let config = SessionConfig {
        starting_cash: 10_000.0,
        slippage_bps: 0.0,
        fees: Some(FeeSchedule {
            maker: 0.0,
            taker: 0.0,
        }),
        max_leverage: 1.0,
    };
    let s = run_backtest(
        Box::new(Plan(plan)),
        perp(),
        Timeframe::M1,
        &bars,
        funding,
        config,
    )
    .expect("runs");
    s.portfolio().funding_paid()
}

/// Funding at `T` stamped `T + 7 ms`, as each venue sends it.
fn late_stamps() -> [(&'static str, Vec<(Millis, f64)>); 2] {
    let binance = binance::parse_funding(&format!(
        r#"[{{"symbol":"TESTUSDT","fundingTime":{},"fundingRate":"{RATE}"}}]"#,
        T + 7
    ))
    .expect("parses");
    let bybit = bybit::parse_funding_page(&format!(
        r#"{{"retCode":0,"retMsg":"OK","result":{{"category":"linear","list":[
            {{"symbol":"TESTUSDT","fundingRate":"{RATE}","fundingRateTimestamp":"{}"}}]}}}}"#,
        T + 7
    ))
    .expect("parses");
    [("binance", binance), ("bybit", bybit)]
}

#[test]
fn a_long_held_into_the_stamp_and_closed_at_that_open_pays_it() {
    // Bought at bar 1's open, sold at bar 2's open, at T. Held going into bar 2, so the funding
    // at T is charged on it at the open price: 1 * 100 * 0.01 = 1.00.
    let plan = || vec![(0, Side::Buy), (1, Side::Sell)];
    assert!((funding_paid(plan(), &[(T, RATE)]) - 1.0).abs() < 1e-12);
    for (venue, funding) in late_stamps() {
        let paid = funding_paid(plan(), &funding);
        assert!((paid - 1.0).abs() < 1e-12, "{venue}: paid {paid}");
    }
}

#[test]
fn a_long_opened_at_the_stamp_does_not_pay_it() {
    // Bought at bar 2's open, at T. Flat going into bar 2, so nothing is paid.
    let plan = || vec![(1, Side::Buy)];
    assert_eq!(funding_paid(plan(), &[(T, RATE)]), 0.0);
    for (venue, funding) in late_stamps() {
        let paid = funding_paid(plan(), &funding);
        assert_eq!(paid, 0.0, "{venue}: paid {paid}");
    }
}
