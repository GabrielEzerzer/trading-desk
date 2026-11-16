//! Property tests on td-core: rounding to a decimal grid, the order filters, fees, civil dates and
//! timeframe buckets. Each runs from a fixed seed so CI sees the same cases every time.

use proptest::prelude::*;
use proptest::test_runner::{Config, RngSeed};
use td_core::{
    FeeSchedule, Instrument, InstrumentId, InstrumentKind, OrderRequest, OrderType, Rounding, Side,
    TimeInForce, Timeframe, Venue, civil_from_days, days_from_civil,
};

const STEPS: [(i64, i32); 12] = [
    (1, 1),
    (1, 2),
    (1, 3),
    (1, 4),
    (1, 6),
    (1, 8),
    (5, 1),
    (25, 2),
    (5, 3),
    (1, 0),
    (5, 0),
    (10, 0),
];

fn config() -> Config {
    Config {
        cases: 512,
        rng_seed: RngSeed::Fixed(0x7d_c0_4e),
        failure_persistence: None,
        ..Config::default()
    }
}

fn step_of((units, decimals): (i64, i32)) -> f64 {
    units as f64 / 10f64.powi(decimals)
}

fn instrument(
    kind: InstrumentKind,
    tick: f64,
    lot: f64,
    min_qty: f64,
    min_notional: f64,
) -> Instrument {
    Instrument {
        id: InstrumentId {
            venue: Venue::Bybit,
            symbol: "BTCUSDT".into(),
            kind,
        },
        base: "BTC".into(),
        quote: "USDT".into(),
        tick_size: tick,
        lot_size: lot,
        min_qty,
        min_notional,
        fees: FeeSchedule {
            maker: 0.0002,
            taker: 0.00055,
            per_unit: 0.0,
        },
    }
}

proptest! {
    #![proptest_config(config())]

    #[test]
    fn rounding_lands_on_the_grid_brackets_the_value_and_prints_exactly(
        step in prop::sample::select(STEPS.to_vec()),
        fraction in 0.0..1.0f64,
    ) {
        let (tick, decimals) = (step_of(step), step.1 as usize);
        let i = instrument(InstrumentKind::Spot, tick, tick, tick, 0.0);
        let x = fraction * tick * 1e7;
        let down = i.round_price(x, Rounding::Down);
        let up = i.round_price(x, Rounding::Up);
        let nearest = i.round_price(x, Rounding::Nearest);
        prop_assert_eq!(i.round_qty(x), down);
        let slack = tick * 1e-6;
        prop_assert!(down <= x + slack && x - slack <= up, "{} {} {}", down, x, up);
        prop_assert!(up - down <= tick + slack, "{} {} at {}", down, up, tick);
        prop_assert!(nearest == down || nearest == up);
        prop_assert!((nearest - x).abs() <= tick / 2.0 + slack);
        for r in [down, up, nearest] {
            for mode in [Rounding::Down, Rounding::Up, Rounding::Nearest] {
                prop_assert_eq!(i.round_price(r, mode), r);
            }
            let text = format!("{r:.decimals$}");
            prop_assert_eq!(text.parse::<f64>(), Ok(r), "{} printed as {}", r, text);
        }
    }

    #[test]
    fn an_order_passes_exactly_when_it_is_on_the_grid_and_at_the_minimums(
        perpetual in any::<bool>(),
        tick in prop::sample::select(STEPS.to_vec()),
        lot in prop::sample::select(STEPS.to_vec()),
        min_lots in 1..20i64,
        min_notional in prop_oneof![Just(0.0), 1.0..200.0f64],
        price in 0.01..100_000.0f64,
        lots in 0.0..200.0f64,
        off_grid in any::<bool>(),
        reduce_only in any::<bool>(),
        buy in any::<bool>(),
    ) {
        let kind = if perpetual { InstrumentKind::Perpetual } else { InstrumentKind::Spot };
        let (tick, lot) = (step_of(tick), step_of(lot));
        let i = instrument(kind, tick, lot, lot * min_lots as f64, min_notional);
        let last = i.round_price(price, Rounding::Nearest).max(tick);
        let qty = if off_grid { lots * lot } else { i.round_qty(lots * lot) };
        let order = OrderRequest {
            instrument: i.id.clone(),
            side: if buy { Side::Buy } else { Side::Sell },
            qty,
            order_type: OrderType::Market,
            tif: TimeInForce::Gtc,
            reduce_only,
            client_tag: None,
        };
        let on_grid = qty > 0.0 && i.round_qty(qty) == qty;
        let above_min = qty >= i.min_qty * (1.0 - 1e-9);
        let exempt = reduce_only && perpetual;
        let enough_value = exempt || qty * last >= min_notional * (1.0 - 1e-9);
        let passes = i.validate_order(&order, last).is_ok();
        prop_assert_eq!(passes, on_grid && above_min && enough_value, "{:?} at {}", order, last);
    }

    #[test]
    fn fees_on_split_fills_add_up_to_the_fee_on_the_whole(
        price in 0.0..100_000.0f64,
        first in 0.0..1_000.0f64,
        second in 0.0..1_000.0f64,
        rate in 0.0..0.01f64,
        per_unit in prop_oneof![Just(0.0), 0.0..0.1f64],
    ) {
        let fees = FeeSchedule { maker: rate, taker: rate, per_unit };
        let whole = fees.fee(price, first + second, rate);
        let split = fees.fee(price, first, rate) + fees.fee(price, second, rate);
        prop_assert!(whole >= 0.0);
        prop_assert!((whole - split).abs() <= 1e-9 * whole.max(1.0), "{} vs {}", whole, split);
        if per_unit == 0.0 {
            prop_assert_eq!(fees.fee(price, first, rate), price * first * rate);
        }
    }

    #[test]
    fn civil_dates_and_day_counts_are_inverse_and_step_one_day_at_a_time(
        days in -50_000_000..50_000_000i64,
    ) {
        let (year, month, day) = civil_from_days(days);
        prop_assert!((1..=12).contains(&month) && (1..=31).contains(&day));
        prop_assert_eq!(days_from_civil(year, month, day), days);
        let next = civil_from_days(days + 1);
        let expected_next = if next.2 == 1 {
            next.1 == month % 12 + 1 && next.0 == year + i64::from(month == 12)
        } else {
            next == (year, month, day + 1)
        };
        prop_assert!(expected_next, "{:?} then {:?}", (year, month, day), next);
    }

    #[test]
    fn a_bucket_holds_its_time_and_floor_and_ceil_are_idempotent_and_monotone(
        timeframe in prop::sample::select(Timeframe::ALL.to_vec()),
        t in -4_000_000_000_000..4_000_000_000_000i64,
        later in 0..2_000_000_000i64,
    ) {
        let duration = timeframe.duration_ms();
        let floor = timeframe.floor(t);
        prop_assert!(floor <= t && t < floor + duration);
        prop_assert_eq!(timeframe.floor(floor), floor);
        let ceil = timeframe.ceil(t);
        prop_assert!(t <= ceil && ceil - t < duration);
        prop_assert_eq!(timeframe.floor(ceil), ceil);
        prop_assert!(timeframe.floor(t + later) >= floor);
        let day = 86_400_000;
        if timeframe == Timeframe::W1 {
            prop_assert_eq!((floor.div_euclid(day) + 3).rem_euclid(7), 0, "{} is not a Monday", floor);
        } else {
            prop_assert_eq!(floor.rem_euclid(duration), 0);
        }
    }
}
