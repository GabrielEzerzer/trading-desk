//! Property tests on the route's guards: the hosts a base URL may name, the client id rules, the
//! decimals an order is written in, and which orders the create body refuses before sending.

use proptest::prelude::*;
use proptest::test_runner::{Config, RngSeed};
use td_core::{
    FeeSchedule, Instrument, InstrumentId, InstrumentKind, OrderRequest, OrderType, Rounding, Side,
    TimeInForce, Venue,
};

use crate::guard::check_base;
use crate::orders::order_body;
use crate::wire::decimal;
use crate::{ClientOrderId, Endpoint, Error};

const DEMO: [&str; 2] = ["api-demo.bybit.com", "API-Demo.Bybit.COM"];
const LOOPBACK: [&str; 4] = ["127.0.0.1", "127.1", "2130706433", "0x7f.1"];
const OTHER: [&str; 9] = [
    "localhost",
    "127.0.0.2",
    "api.bybit.com",
    "api-testnet.bybit.com",
    "api-demo.bybit.com.",
    "api-demo.bybit.com.evil.example",
    "evil-api-demo.bybit.com",
    "[::1]",
    "0.0.0.0",
];

fn config() -> Config {
    Config {
        cases: 512,
        rng_seed: RngSeed::Fixed(0x7d_0e_de),
        failure_persistence: None,
        ..Config::default()
    }
}

fn perpetual() -> Instrument {
    Instrument {
        id: InstrumentId {
            venue: Venue::Bybit,
            symbol: "BTCUSDT".into(),
            kind: InstrumentKind::Perpetual,
        },
        base: "BTC".into(),
        quote: "USDT".into(),
        tick_size: 0.1,
        lot_size: 0.001,
        min_qty: 0.001,
        min_notional: 5.0,
        fees: FeeSchedule {
            maker: 0.0002,
            taker: 0.00055,
            per_unit: 0.0,
        },
    }
}

fn host() -> impl Strategy<Value = &'static str> {
    prop_oneof![
        prop::sample::select(DEMO.to_vec()),
        prop::sample::select(LOOPBACK.to_vec()),
        prop::sample::select(OTHER.to_vec()),
    ]
}

proptest! {
    #![proptest_config(config())]

    #[test]
    fn a_base_url_passes_only_as_the_demo_host_or_a_loopback_port(
        scheme in prop::sample::select(vec!["http", "https", "HTTP", "Https", "wss", "ftp"]),
        user in prop_oneof![3 => Just(""), 1 => prop::sample::select(vec!["u@", "u:p@", "api-demo.bybit.com@"])],
        host in host(),
        port in prop_oneof![Just(None), Just(Some(0u16)), Just(Some(443)), any::<u16>().prop_map(Some)],
        tail in prop_oneof![3 => prop::sample::select(vec!["", "/"]), 1 => prop::sample::select(vec!["/v5", "?x=1", "#f", "/?", "/#"])],
    ) {
        let scheme_lower = scheme.to_ascii_lowercase();
        prop_assume!(!(scheme_lower == "http" && port == Some(80)));
        let port_text = port.map_or(String::new(), |p| format!(":{p}"));
        let text = format!("{scheme}://{user}{host}{port_text}{tail}");
        let bare = user.is_empty() && (tail.is_empty() || tail == "/");
        let demo = scheme_lower == "https" && DEMO.contains(&host) && matches!(port, None | Some(443));
        let loopback = scheme_lower == "http"
            && LOOPBACK.contains(&host)
            && port.is_some_and(|p| p != 0);
        match check_base(&text) {
            Ok(url) => {
                prop_assert!(bare && (demo || loopback), "{} passed", text);
                let allowed = url.as_str() == "https://api-demo.bybit.com/"
                    || port.is_some_and(|p| url.as_str() == format!("http://127.0.0.1:{p}/"));
                prop_assert!(allowed, "{} passed as {}", text, url);
            }
            Err(e) => {
                prop_assert!(!(bare && (demo || loopback)), "{} was refused", text);
                let host_refused = matches!(e, Error::HostRefused { .. });
                prop_assert!(host_refused, "{} gave {:?}", text, e);
            }
        }
    }

    #[test]
    fn a_loopback_endpoint_builds_for_every_port_but_zero(port in any::<u16>()) {
        prop_assume!(port != 80);
        prop_assert_eq!(Endpoint::Loopback(port).base_url().is_ok(), port != 0);
    }

    #[test]
    fn a_client_id_passes_exactly_when_it_meets_the_venue_rules(
        id in prop_oneof!["[A-Za-z0-9_-]{0,40}", any::<String>()],
    ) {
        let valid = (1..=36).contains(&id.len())
            && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
        match ClientOrderId::new(id.clone()) {
            Ok(made) => {
                prop_assert!(valid, "{:?} passed", id);
                prop_assert_eq!(made.as_str(), id.as_str());
                prop_assert_eq!(made.to_string(), id);
            }
            Err(_) => prop_assert!(!valid, "{:?} was refused", id),
        }
    }

    #[test]
    fn a_value_on_the_grid_is_written_with_exactly_the_steps_decimals(
        step in prop::sample::select(vec![(1i64, 1u32), (1, 2), (1, 3), (1, 5), (1, 8), (5, 1), (25, 2), (5, 3), (1, 0), (5, 0)]),
        count in 0..10_000_000i64,
    ) {
        let (units, decimals) = step;
        let scale = 10i64.pow(decimals);
        let value = (count * units) as f64 / scale as f64;
        let whole = count * units;
        let expected = if decimals == 0 {
            whole.to_string()
        } else {
            format!("{}.{:0width$}", whole / scale, whole % scale, width = decimals as usize)
        };
        let text = decimal(value, units as f64 / scale as f64);
        prop_assert_eq!(text.as_deref().ok(), Some(expected.as_str()), "{}", value);
        prop_assert_eq!(expected.parse::<f64>(), Ok(value));
    }

    #[test]
    fn a_stop_is_sent_only_reduce_only_good_till_cancelled_and_on_the_far_side_of_the_market(
        price in 100.0..100_000.0f64,
        ticks_away in -3..=3i64,
        lots in 1..10_000i64,
        stop in any::<bool>(),
        buy in any::<bool>(),
        reduce_only in any::<bool>(),
        gtc in any::<bool>(),
    ) {
        let i = perpetual();
        let last = i.round_price(price, Rounding::Nearest);
        let trigger = i.round_price(last + ticks_away as f64 * i.tick_size, Rounding::Nearest);
        let qty = i.round_qty(lots as f64 * i.lot_size);
        let side = if buy { Side::Buy } else { Side::Sell };
        let order = OrderRequest {
            instrument: i.id.clone(),
            side,
            qty,
            order_type: if stop { OrderType::Stop { trigger } } else { OrderType::Market },
            tif: if gtc { TimeInForce::Gtc } else { TimeInForce::Ioc },
            reduce_only,
            client_tag: None,
        };
        let id = ClientOrderId::new("prop-1").expect("a valid id");
        let price_for_value = if stop { trigger } else { last };
        let enough_value = reduce_only || qty * price_for_value >= i.min_notional * (1.0 - 1e-9);
        let far_side = if buy { trigger > last } else { trigger < last };
        let sendable = enough_value && (!stop || (reduce_only && gtc && far_side));
        let body = order_body(&i, &order, &id, last);
        prop_assert_eq!(body.is_ok(), sendable, "{:?} at {}: {:?}", order, last, body);
        if stop && reduce_only && gtc && !far_side {
            let would_trigger = matches!(body, Err(Error::StopWouldTrigger { .. }));
            prop_assert!(would_trigger, "{:?}", body);
        }
        if let Ok(body) = body {
            prop_assert_eq!(body["side"].as_str(), Some(if buy { "Buy" } else { "Sell" }));
            prop_assert_eq!(body["reduceOnly"].as_bool(), Some(reduce_only));
            prop_assert_eq!(body["qty"].as_str().map(str::parse::<f64>), Some(Ok(qty)));
            if stop {
                prop_assert_eq!(body["triggerDirection"].as_i64(), Some(if buy { 1 } else { 2 }));
                prop_assert_eq!(body["triggerPrice"].as_str().map(str::parse::<f64>), Some(Ok(trigger)));
            } else {
                prop_assert!(body.get("triggerPrice").is_none());
            }
        }
    }
}
