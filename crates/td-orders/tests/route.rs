//! The order route against the mock venue on 127.0.0.1: real HTTP, real signing, no network.

use std::net::TcpListener;
use std::time::{Duration, Instant};

use td_core::{
    CoreError, FeeSchedule, Instrument, InstrumentId, InstrumentKind, Liquidity, OrderRequest,
    OrderType, Side, TimeInForce, Venue,
};
use td_mock_bybit::{Fault, IdRule, MockVenue, TEST_KEY, TEST_SECRET};
use td_orders::{
    Action, CancelOutcome, Client, ClientOrderId, DemoKeys, Endpoint, Error, ExecKind, OrderStatus,
    Timing,
};

const SYMBOL: &str = "BTCUSDT";
const PRICE: f64 = 65000.0;
const CREATE: &str = "/v5/order/create";
const CANCEL: &str = "/v5/order/cancel";
const AMEND: &str = "/v5/order/amend";

fn instrument() -> Instrument {
    Instrument {
        id: InstrumentId {
            venue: Venue::Bybit,
            symbol: SYMBOL.into(),
            kind: InstrumentKind::Perpetual,
        },
        base: "BTC".into(),
        quote: "USDT".into(),
        tick_size: 0.1,
        lot_size: 0.001,
        min_qty: 0.002,
        min_notional: 100.0,
        fees: FeeSchedule {
            maker: 0.0002,
            taker: 0.00055,
            per_unit: 0.0,
        },
    }
}

/// Short timeouts so the failure tests run in seconds. The request timeout leaves room for a
/// loaded test machine (a late answer would send a test down the unknown-outcome path) and is
/// well under the mock's hang.
fn timing() -> Timing {
    Timing {
        connect_timeout: Duration::from_secs(1),
        request_timeout: Duration::from_millis(1500),
        recv_window: Duration::from_millis(1000),
        settle: Duration::from_millis(50),
    }
}

const HANG: Duration = Duration::from_millis(3000);

fn keys() -> DemoKeys {
    DemoKeys::for_loopback(TEST_KEY, TEST_SECRET).unwrap()
}

fn client_for(mock: &MockVenue) -> Client {
    Client::with_timing(Endpoint::Loopback(mock.port()), keys(), timing()).unwrap()
}

fn setup() -> (MockVenue, Client) {
    let mock = MockVenue::start();
    mock.add_instrument(&instrument(), PRICE);
    let client = client_for(&mock);
    (mock, client)
}

fn id(s: &str) -> ClientOrderId {
    ClientOrderId::new(s).unwrap()
}

fn market(side: Side, qty: f64, reduce_only: bool) -> OrderRequest {
    OrderRequest {
        instrument: instrument().id,
        side,
        qty,
        order_type: OrderType::Market,
        tif: TimeInForce::Ioc,
        reduce_only,
        client_tag: None,
    }
}

fn stop(side: Side, qty: f64, trigger: f64) -> OrderRequest {
    OrderRequest {
        order_type: OrderType::Stop { trigger },
        tif: TimeInForce::Gtc,
        reduce_only: true,
        ..market(side, qty, true)
    }
}

/// Opens a 0.004 long and rests a reduce-only sell stop at 60000 under `stop_id`.
fn long_with_stop(mock: &MockVenue, client: &Client, stop_id: &str) {
    let i = instrument();
    client
        .place(&i, &market(Side::Buy, 0.004, false), &id("open-1"), PRICE)
        .unwrap();
    client
        .place(&i, &stop(Side::Sell, 0.004, 60000.0), &id(stop_id), PRICE)
        .unwrap();
    assert_eq!(mock.orders_with_link_id(stop_id)[0].status, "Untriggered");
}

const FAULTS: [fn() -> Fault; 5] = [
    || Fault::Hang(HANG),
    || Fault::DropBeforeApply,
    || Fault::DropAfterApply,
    || Fault::ServerError,
    || Fault::ServerErrorAfterApply,
];

#[test]
fn a_market_order_fills_and_every_read_sees_it() {
    let (mock, client) = setup();
    let i = instrument();
    let since = mock.now() - 1000;
    let placed = client
        .place(&i, &market(Side::Buy, 0.002, false), &id("m-1"), PRICE)
        .unwrap();
    assert_eq!(placed.client_id.as_str(), "m-1");
    assert_eq!(mock.position(SYMBOL), 0.002);

    let position = client.position(&i).unwrap();
    assert_eq!(position.size, 0.002);
    assert_eq!(position.avg_price, PRICE);

    let order = client.order_by_client_id(&i, &id("m-1")).unwrap().unwrap();
    assert_eq!(order.order_id, placed.order_id);
    assert_eq!(order.status, OrderStatus::Filled);
    assert_eq!(order.filled_qty, 0.002);
    assert_eq!(order.avg_price, Some(PRICE));
    assert!(client.open_orders(&i).unwrap().is_empty());

    let execs = client.executions_since(&i, since).unwrap();
    assert_eq!(execs.len(), 1);
    let e = &execs[0];
    assert_eq!(e.kind, ExecKind::Trade);
    assert_eq!(e.client_id, "m-1");
    assert_eq!(e.order_id, placed.order_id);
    assert_eq!(e.side, Some(Side::Buy));
    assert_eq!((e.price, e.qty), (PRICE, 0.002));
    assert_eq!(e.liquidity, Liquidity::Taker);
    assert!((e.fee - PRICE * 0.002 * 0.00055).abs() < 1e-8);

    mock.set_price(SYMBOL, 66000.0);
    let equity = client.equity().unwrap();
    assert_eq!(equity.coin, "USDT");
    assert!((equity.wallet_balance - (100_000.0 - e.fee)).abs() < 1e-6);
    assert!((equity.unrealised_pnl - 2.0).abs() < 1e-6);
    assert!((equity.equity - (equity.wallet_balance + 2.0)).abs() < 1e-6);
    assert!((equity.account_total_equity_usd - equity.equity).abs() < 1e-6);

    assert!(
        client
            .order_by_client_id(&i, &id("never"))
            .unwrap()
            .is_none()
    );
}

#[test]
fn a_reduce_only_market_order_closes_and_never_opens() {
    let (mock, client) = setup();
    let i = instrument();
    let err = client
        .place(&i, &market(Side::Sell, 0.002, true), &id("r-0"), PRICE)
        .unwrap_err();
    assert!(matches!(err, Error::Rejected { code: 110017, .. }), "{err}");
    assert_eq!(mock.position(SYMBOL), 0.0);

    client
        .place(&i, &market(Side::Buy, 0.003, false), &id("r-1"), PRICE)
        .unwrap();
    // More than the position: the venue cuts it to the position.
    client
        .place(&i, &market(Side::Sell, 0.005, true), &id("r-2"), PRICE)
        .unwrap();
    assert_eq!(mock.position(SYMBOL), 0.0);
    assert_eq!(client.position(&i).unwrap().size, 0.0);
}

#[test]
fn a_stop_rests_amends_and_triggers() {
    let (mock, client) = setup();
    let i = instrument();
    long_with_stop(&mock, &client, "s-1");

    let open = client.open_orders(&i).unwrap();
    assert_eq!(open.len(), 1);
    assert_eq!(open[0].status, OrderStatus::Untriggered);
    assert_eq!(open[0].trigger_price, Some(60000.0));
    assert!(open[0].reduce_only);
    assert_eq!(open[0].side, Side::Sell);

    client
        .amend_stop(&i, &id("s-1"), Some(61000.5), Some(0.002))
        .unwrap();
    let o = client.order_by_client_id(&i, &id("s-1")).unwrap().unwrap();
    assert_eq!((o.trigger_price, o.qty), (Some(61000.5), 0.002));
    client
        .amend_stop(&i, &id("s-1"), None, Some(0.003))
        .unwrap();
    assert_eq!(mock.orders_with_link_id("s-1")[0].qty, 0.003);

    mock.set_price(SYMBOL, 62000.0);
    assert_eq!(mock.orders_with_link_id("s-1")[0].status, "Untriggered");
    mock.set_price(SYMBOL, 61000.0);
    let o = client.order_by_client_id(&i, &id("s-1")).unwrap().unwrap();
    assert_eq!(o.status, OrderStatus::Filled);
    assert_eq!(client.position(&i).unwrap().size, 0.001);
    assert!(client.open_orders(&i).unwrap().is_empty());

    let err = client
        .amend_stop(&i, &id("s-1"), Some(59000.0), None)
        .unwrap_err();
    assert!(matches!(err, Error::OrderClosed { .. }), "{err}");
}

#[test]
fn a_cancel_cancels_once_and_then_reports_the_closed_order() {
    let (mock, client) = setup();
    let i = instrument();
    long_with_stop(&mock, &client, "c-1");
    assert_eq!(
        client.cancel(&i, &id("c-1")).unwrap(),
        CancelOutcome::Cancelled
    );
    assert_eq!(mock.orders_with_link_id("c-1")[0].status, "Deactivated");
    assert_eq!(
        client.cancel(&i, &id("c-1")).unwrap(),
        CancelOutcome::AlreadyClosed(OrderStatus::Deactivated)
    );
    let err = client.cancel(&i, &id("no-such")).unwrap_err();
    assert!(matches!(err, Error::OrderNotFound { .. }), "{err}");
    let err = client
        .amend_stop(&i, &id("no-such"), Some(59000.0), None)
        .unwrap_err();
    assert!(matches!(err, Error::OrderNotFound { .. }), "{err}");
}

#[test]
fn the_same_id_twice_never_makes_two_orders() {
    for rule in [IdRule::Always, IdRule::OpenOnly] {
        let (mock, client) = setup();
        mock.set_id_rule(rule);
        let i = instrument();
        let order = market(Side::Buy, 0.002, false);
        client.place(&i, &order, &id("dup-1"), PRICE).unwrap();
        let err = client.place(&i, &order, &id("dup-1"), PRICE).unwrap_err();
        assert!(matches!(err, Error::ClientIdAlreadyUsed { .. }), "{err}");
        assert_eq!(mock.request_count(CREATE), 1, "the second was not sent");

        // A new client (a restarted process) is stopped by the venue while the order rests.
        long_with_stop(&mock, &client, "dup-stop");
        let other = client_for(&mock);
        let err = other
            .place(
                &i,
                &stop(Side::Sell, 0.004, 60000.0),
                &id("dup-stop"),
                PRICE,
            )
            .unwrap_err();
        assert!(matches!(err, Error::DuplicateClientId { .. }), "{err}");
        assert_eq!(mock.orders_with_link_id("dup-stop").len(), 1);
        assert_eq!(mock.orders_with_link_id("dup-1").len(), 1);
    }
    // With the documented rule the venue also refuses a closed order's id.
    let (mock, client) = setup();
    let i = instrument();
    client
        .place(&i, &market(Side::Buy, 0.002, false), &id("dup-2"), PRICE)
        .unwrap();
    let err = client_for(&mock)
        .place(&i, &market(Side::Buy, 0.002, false), &id("dup-2"), PRICE)
        .unwrap_err();
    assert!(matches!(err, Error::DuplicateClientId { .. }), "{err}");
    assert_eq!(mock.orders_with_link_id("dup-2").len(), 1);
}

#[test]
fn a_place_whose_answer_is_lost_ends_with_exactly_one_order() {
    for rule in [IdRule::Always, IdRule::OpenOnly] {
        for (n, fault) in FAULTS.iter().enumerate() {
            let (mock, client) = setup();
            mock.set_id_rule(rule);
            let i = instrument();
            let fault = fault();
            let link = format!("lost-{n}");
            mock.inject(CREATE, fault.clone());
            let placed = client
                .place(&i, &market(Side::Buy, 0.002, false), &id(&link), PRICE)
                .unwrap_or_else(|e| panic!("{fault:?} {rule:?}: {e}"));
            let orders = mock.orders_with_link_id(&link);
            assert_eq!(orders.len(), 1, "{fault:?} {rule:?}");
            assert_eq!(orders[0].order_id, placed.order_id, "{fault:?}");
            assert_eq!(orders[0].status, "Filled");
            assert_eq!(mock.position(SYMBOL), 0.002, "{fault:?} {rule:?}");
            let expected_sends = match fault {
                Fault::DropBeforeApply | Fault::ServerError => 2,
                _ => 1,
            };
            assert_eq!(mock.request_count(CREATE), expected_sends, "{fault:?}");
        }
    }
}

#[test]
fn a_lost_place_is_found_from_executions_when_the_order_lists_lag() {
    // After a venue restart the real-time list has no closed orders and the history is late;
    // only the execution shows the order went through. The weaker id rule would accept a resend,
    // so a resend here would make a second order.
    let (mock, client) = setup();
    mock.set_id_rule(IdRule::OpenOnly);
    mock.set_closed_cache(false);
    mock.set_history_delay(60_000);
    let i = instrument();
    mock.inject(CREATE, Fault::DropAfterApply);
    client
        .place(&i, &market(Side::Buy, 0.002, false), &id("lag-1"), PRICE)
        .unwrap();
    assert_eq!(mock.orders_with_link_id("lag-1").len(), 1);
    assert_eq!(mock.request_count(CREATE), 1);
}

#[test]
fn a_lost_place_with_a_venue_that_cannot_be_asked_is_unknown_and_never_resent() {
    let (mock, client) = setup();
    let i = instrument();
    mock.inject(CREATE, Fault::DropAfterApply);
    mock.inject_always("/v5/order/realtime", Fault::DropBeforeApply);
    let err = client
        .place(&i, &market(Side::Buy, 0.002, false), &id("unk-1"), PRICE)
        .unwrap_err();
    assert!(
        matches!(
            err,
            Error::OutcomeUnknown {
                action: Action::Place,
                ..
            }
        ),
        "{err}"
    );
    assert_eq!(mock.request_count(CREATE), 1, "nothing was resent");
    assert_eq!(mock.orders_with_link_id("unk-1").len(), 1);
    // The id stays spent: the caller resolves it later, never reuses it.
    mock.clear_faults();
    let err = client
        .place(&i, &market(Side::Buy, 0.002, false), &id("unk-1"), PRICE)
        .unwrap_err();
    assert!(matches!(err, Error::ClientIdAlreadyUsed { .. }), "{err}");
    let order = client.order_by_client_id(&i, &id("unk-1")).unwrap();
    assert_eq!(order.unwrap().status, OrderStatus::Filled);
}

#[test]
fn a_cancel_whose_answer_is_lost_ends_cancelled() {
    for (n, fault) in FAULTS.iter().enumerate() {
        let (mock, client) = setup();
        let i = instrument();
        let link = format!("cl-{n}");
        long_with_stop(&mock, &client, &link);
        let fault = fault();
        mock.inject(CANCEL, fault.clone());
        let outcome = client
            .cancel(&i, &id(&link))
            .unwrap_or_else(|e| panic!("{fault:?}: {e}"));
        assert_eq!(outcome, CancelOutcome::Cancelled, "{fault:?}");
        let orders = mock.orders_with_link_id(&link);
        assert_eq!(orders.len(), 1);
        assert_eq!(orders[0].status, "Deactivated", "{fault:?}");
    }
}

#[test]
fn a_cancel_with_a_venue_that_cannot_be_asked_is_unknown() {
    let (mock, client) = setup();
    let i = instrument();
    long_with_stop(&mock, &client, "cu-1");
    mock.inject(CANCEL, Fault::DropBeforeApply);
    mock.inject_always("/v5/order/realtime", Fault::IpBlock);
    let err = client.cancel(&i, &id("cu-1")).unwrap_err();
    assert!(
        matches!(
            err,
            Error::OutcomeUnknown {
                action: Action::Cancel,
                ..
            }
        ),
        "{err}"
    );
}

#[test]
fn an_amend_whose_answer_is_lost_ends_with_the_new_values() {
    for (n, fault) in FAULTS.iter().enumerate() {
        let (mock, client) = setup();
        let i = instrument();
        let link = format!("am-{n}");
        long_with_stop(&mock, &client, &link);
        let fault = fault();
        mock.inject(AMEND, fault.clone());
        client
            .amend_stop(&i, &id(&link), Some(61000.0), Some(0.003))
            .unwrap_or_else(|e| panic!("{fault:?}: {e}"));
        let orders = mock.orders_with_link_id(&link);
        assert_eq!(orders.len(), 1);
        assert_eq!(orders[0].trigger, Some(61000.0), "{fault:?}");
        assert_eq!(orders[0].qty, 0.003, "{fault:?}");
        assert_eq!(orders[0].status, "Untriggered");
    }
}

#[test]
fn an_amend_with_a_venue_that_cannot_be_asked_is_unknown() {
    let (mock, client) = setup();
    let i = instrument();
    long_with_stop(&mock, &client, "au-1");
    mock.inject(AMEND, Fault::Hang(HANG));
    mock.inject_always("/v5/order/realtime", Fault::ServerError);
    let err = client
        .amend_stop(&i, &id("au-1"), Some(61000.0), None)
        .unwrap_err();
    assert!(
        matches!(
            err,
            Error::OutcomeUnknown {
                action: Action::Amend,
                ..
            }
        ),
        "{err}"
    );
}

#[test]
fn rate_limits_wait_and_go_again_but_an_ip_block_is_returned() {
    let (mock, client) = setup();
    let i = instrument();
    mock.inject(
        CREATE,
        Fault::RateLimit {
            reset_after_ms: 250,
        },
    );
    let started = Instant::now();
    client
        .place(&i, &market(Side::Buy, 0.002, false), &id("rl-1"), PRICE)
        .unwrap();
    assert!(started.elapsed() >= Duration::from_millis(200));
    assert_eq!(mock.request_count(CREATE), 2);
    assert_eq!(mock.orders_with_link_id("rl-1").len(), 1);

    mock.inject(CREATE, Fault::Http429);
    client
        .place(&i, &market(Side::Buy, 0.002, false), &id("rl-2"), PRICE)
        .unwrap();
    assert_eq!(mock.orders_with_link_id("rl-2").len(), 1);

    mock.inject(CREATE, Fault::IpBlock);
    let err = client
        .place(&i, &market(Side::Buy, 0.002, false), &id("rl-3"), PRICE)
        .unwrap_err();
    assert!(
        matches!(err, Error::RateLimited { ip_block: true, .. }),
        "{err}"
    );
    assert!(mock.orders_with_link_id("rl-3").is_empty());
    // The venue did not take it, so the id is free again.
    client
        .place(&i, &market(Side::Buy, 0.002, false), &id("rl-3"), PRICE)
        .unwrap();

    mock.inject_always(CREATE, Fault::RateLimit { reset_after_ms: 10 });
    let err = client
        .place(&i, &market(Side::Buy, 0.002, false), &id("rl-4"), PRICE)
        .unwrap_err();
    assert!(
        matches!(
            err,
            Error::RateLimited {
                ip_block: false,
                ..
            }
        ),
        "{err}"
    );
    assert!(mock.orders_with_link_id("rl-4").is_empty());
}

#[test]
fn a_timestamp_rejection_resyncs_the_clock_and_retries_once() {
    let (mock, client) = setup();
    let i = instrument();
    let syncs = || mock.request_count("/v5/market/time");
    assert_eq!(syncs(), 1, "synced when built");

    // The venue's clock jumps ahead past the receive window.
    mock.set_skew(5_000);
    assert_eq!(client.position(&i).unwrap().size, 0.0);
    assert_eq!(syncs(), 2);
    assert!((client.server_now() - mock.now()).abs() < 200);

    mock.inject(CREATE, Fault::TimestampReject);
    client
        .place(&i, &market(Side::Buy, 0.002, false), &id("ts-1"), PRICE)
        .unwrap();
    assert_eq!(syncs(), 3);
    assert_eq!(mock.orders_with_link_id("ts-1").len(), 1);

    mock.inject_always(CREATE, Fault::TimestampReject);
    let err = client
        .place(&i, &market(Side::Buy, 0.002, false), &id("ts-2"), PRICE)
        .unwrap_err();
    assert!(matches!(err, Error::TimestampRejected { .. }), "{err}");
    assert!(mock.orders_with_link_id("ts-2").is_empty());
}

#[test]
fn a_hedge_mode_account_is_refused_and_left_alone() {
    let (mock, client) = setup();
    mock.set_hedge_mode(true);
    let i = instrument();
    let err = client.position(&i).unwrap_err();
    assert!(matches!(err, Error::HedgeMode { .. }), "{err}");
    let err = client
        .place(&i, &market(Side::Buy, 0.002, false), &id("h-1"), PRICE)
        .unwrap_err();
    assert!(matches!(err, Error::HedgeMode { .. }), "{err}");
    assert!(mock.orders().is_empty());
}

#[test]
fn orders_off_the_venue_filters_are_refused_before_sending() {
    let (mock, client) = setup();
    let i = instrument();
    let refused = |order: OrderRequest, last: f64| {
        client
            .place(&i, &order, &id("f-1"), last)
            .expect_err("refused")
    };
    let err = refused(market(Side::Buy, 0.0025, false), PRICE);
    assert!(
        matches!(err, Error::Filter(CoreError::QtyOffLot { .. })),
        "{err}"
    );
    let err = refused(market(Side::Buy, 0.1 + 0.2, false), PRICE);
    assert!(
        matches!(err, Error::Filter(CoreError::QtyOffLot { .. })),
        "{err}"
    );
    let err = refused(market(Side::Buy, 0.001, false), PRICE);
    assert!(
        matches!(err, Error::Filter(CoreError::QtyBelowMin { .. })),
        "{err}"
    );
    let err = refused(market(Side::Buy, 0.002, false), 40000.0);
    assert!(
        matches!(err, Error::Filter(CoreError::NotionalBelowMin { .. })),
        "{err}"
    );
    let err = refused(stop(Side::Sell, 0.002, 60000.05), PRICE);
    assert!(
        matches!(err, Error::Filter(CoreError::PriceOffTick { .. })),
        "{err}"
    );
    let err = refused(stop(Side::Sell, 0.002, 66000.0), PRICE);
    assert!(matches!(err, Error::StopWouldTrigger { .. }), "{err}");
    let err = refused(stop(Side::Buy, 0.002, 64000.0), PRICE);
    assert!(matches!(err, Error::StopWouldTrigger { .. }), "{err}");
    let limit = OrderRequest {
        order_type: OrderType::Limit { price: 64000.0 },
        ..market(Side::Buy, 0.002, false)
    };
    assert!(matches!(refused(limit, PRICE), Error::Unsupported { .. }));
    let opening_stop = OrderRequest {
        reduce_only: false,
        ..stop(Side::Buy, 0.002, 66000.0)
    };
    assert!(matches!(
        refused(opening_stop, PRICE),
        Error::Unsupported { .. }
    ));
    let mut spot = instrument();
    spot.id.kind = InstrumentKind::Spot;
    let err = client
        .place(&spot, &market(Side::Buy, 0.002, false), &id("f-2"), PRICE)
        .unwrap_err();
    assert!(matches!(err, Error::Unsupported { .. }), "{err}");
    for (trigger, qty) in [
        (Some(60000.05), None),
        (None, Some(0.0025)),
        (None, Some(0.001)),
    ] {
        let err = client.amend_stop(&i, &id("f-3"), trigger, qty).unwrap_err();
        assert!(matches!(err, Error::Filter(_)), "{err}");
    }
    assert_eq!(mock.request_count(CREATE), 0);
    assert_eq!(mock.request_count(AMEND), 0);

    // A reduce-only order below the minimum notional is the venue's call, so it is sent.
    client
        .place(&i, &market(Side::Buy, 0.002, false), &id("f-4"), PRICE)
        .unwrap();
    client
        .place(&i, &market(Side::Sell, 0.002, true), &id("f-5"), 40000.0)
        .unwrap();
    assert_eq!(mock.position(SYMBOL), 0.0);
}

#[test]
fn executions_are_read_to_the_last_page_and_across_windows() {
    let (mock, client) = setup();
    let i = instrument();
    let now = mock.now();
    mock.seed_trades(SYMBOL, 250, now - 3_600_000, 1000);
    mock.seed_trades(SYMBOL, 3, now - 9 * 86_400_000, 1000);
    mock.add_funding(SYMBOL, 0.002, 0.0065);

    let execs = client.executions_since(&i, now - 2 * 3_600_000).unwrap();
    assert_eq!(execs.len(), 251);
    assert!(execs.windows(2).all(|w| w[0].time <= w[1].time));
    let funding = execs.last().unwrap();
    assert_eq!(funding.kind, ExecKind::Funding);
    assert_eq!(funding.client_id, "");
    assert_eq!(mock.request_count("/v5/execution/list"), 3);

    let all = client.executions_since(&i, now - 10 * 86_400_000).unwrap();
    assert_eq!(all.len(), 254);
    let mut ids: Vec<_> = all.iter().map(|e| e.exec_id.clone()).collect();
    ids.dedup();
    assert_eq!(ids.len(), 254);
}

#[test]
fn a_redirect_is_never_followed() {
    let (mock, client) = setup();
    let elsewhere = TcpListener::bind("127.0.0.1:0").unwrap();
    elsewhere.set_nonblocking(true).unwrap();
    let port = elsewhere.local_addr().unwrap().port();
    mock.inject(
        "/v5/position/list",
        Fault::Redirect(format!("http://127.0.0.1:{port}/v5/position/list")),
    );
    let err = client.position(&instrument()).unwrap_err();
    assert!(
        matches!(err, Error::HttpStatus { status: 302, .. }),
        "{err}"
    );
    std::thread::sleep(Duration::from_millis(100));
    assert!(
        elsewhere.accept().is_err(),
        "the redirect target was contacted"
    );

    mock.inject(
        CREATE,
        Fault::Redirect(format!("http://127.0.0.1:{port}{CREATE}")),
    );
    let err = client
        .place(
            &instrument(),
            &market(Side::Buy, 0.002, false),
            &id("rd-1"),
            PRICE,
        )
        .unwrap_err();
    assert!(
        matches!(err, Error::HttpStatus { status: 302, .. }),
        "{err}"
    );
    std::thread::sleep(Duration::from_millis(100));
    assert!(
        elsewhere.accept().is_err(),
        "the redirect target was contacted"
    );
}

#[test]
fn loopback_keys_cannot_reach_the_demo_service() {
    let err = Client::new(Endpoint::Demo, keys()).unwrap_err();
    assert_eq!(err, Error::LoopbackKeysOnDemo);
}

#[test]
fn timing_outside_its_ranges_is_refused() {
    let mock = MockVenue::start();
    for bad in [
        Timing {
            request_timeout: Duration::from_secs(120),
            ..timing()
        },
        Timing {
            connect_timeout: Duration::from_millis(1),
            ..timing()
        },
        Timing {
            recv_window: Duration::from_secs(600),
            ..timing()
        },
    ] {
        let err = Client::with_timing(Endpoint::Loopback(mock.port()), keys(), bad).unwrap_err();
        assert!(matches!(err, Error::InvalidTiming { .. }), "{err}");
    }
}

#[test]
fn requests_carry_only_the_user_agent_and_the_signing_headers() {
    let (mock, client) = setup();
    let i = instrument();
    client
        .place(&i, &market(Side::Buy, 0.002, false), &id("ua-1"), PRICE)
        .unwrap();
    client.position(&i).unwrap();
    let allowed = [
        "host",
        "user-agent",
        "accept",
        "accept-encoding",
        "content-type",
        "content-length",
        "x-bapi-api-key",
        "x-bapi-timestamp",
        "x-bapi-recv-window",
        "x-bapi-sign",
    ];
    let requests = mock.requests();
    assert!(requests.len() >= 3);
    for r in requests {
        for (name, value) in &r.headers {
            assert!(allowed.contains(&name.as_str()), "{} sent {name}", r.path);
            if name == "user-agent" {
                assert_eq!(value, "trading-desk");
            }
        }
    }
}

#[test]
fn no_error_or_debug_output_shows_a_credential() {
    let mock = MockVenue::start();
    mock.add_instrument(&instrument(), PRICE);
    mock.set_echo_credentials(true);
    let client = client_for(&mock);
    let i = instrument();
    let mut errors: Vec<Error> = Vec::new();

    // Venue rejections whose messages echo the key and the signature.
    errors.push(
        client
            .place(&i, &market(Side::Sell, 0.002, true), &id("x-1"), PRICE)
            .unwrap_err(),
    );
    errors.push(client.cancel(&i, &id("x-2")).unwrap_err());
    // A signature error, whose message echoes the signed text with the key in it.
    let wrong = Client::with_timing(
        Endpoint::Loopback(mock.port()),
        DemoKeys::for_loopback(TEST_KEY, "not-the-secret").unwrap(),
        timing(),
    )
    .unwrap();
    errors.push(wrong.position(&i).unwrap_err());
    errors.push(
        wrong
            .place(&i, &market(Side::Buy, 0.002, false), &id("x-3"), PRICE)
            .unwrap_err(),
    );
    // Transport failures, an unknown outcome, rate limits, timestamps.
    mock.inject(CREATE, Fault::DropAfterApply);
    mock.inject_always("/v5/order/realtime", Fault::DropBeforeApply);
    errors.push(
        client
            .place(&i, &market(Side::Buy, 0.002, false), &id("x-4"), PRICE)
            .unwrap_err(),
    );
    mock.clear_faults();
    mock.inject(CREATE, Fault::IpBlock);
    errors.push(
        client
            .place(&i, &market(Side::Buy, 0.002, false), &id("x-5"), PRICE)
            .unwrap_err(),
    );
    mock.inject_always(CREATE, Fault::TimestampReject);
    errors.push(
        client
            .place(&i, &market(Side::Buy, 0.002, false), &id("x-6"), PRICE)
            .unwrap_err(),
    );
    mock.clear_faults();
    mock.inject_always("/v5/position/list", Fault::ServerError);
    errors.push(client.position(&i).unwrap_err());
    mock.clear_faults();
    errors.push(Client::new(Endpoint::Demo, keys()).unwrap_err());

    let signatures: Vec<String> = mock
        .requests()
        .into_iter()
        .flat_map(|r| r.headers)
        .filter(|(n, _)| n == "x-bapi-sign")
        .map(|(_, v)| v)
        .collect();
    assert!(signatures.len() > 5);
    let mut shown: Vec<String> = errors
        .iter()
        .flat_map(|e| [format!("{e}"), format!("{e:?}"), format!("{e:#?}")])
        .collect();
    shown.push(format!("{client:?}"));
    shown.push(format!("{client:#?}"));
    shown.push(format!("{:?}", keys()));
    for text in &shown {
        assert!(!text.contains(TEST_KEY), "key in: {text}");
        assert!(!text.contains(TEST_SECRET), "secret in: {text}");
        for s in &signatures {
            assert!(!text.contains(s.as_str()), "signature in: {text}");
        }
    }
    // The withheld messages still say which request failed.
    assert!(
        errors
            .iter()
            .any(|e| matches!(e, Error::Rejected { code: 10004, .. }))
    );
    assert!(
        errors
            .iter()
            .any(|e| matches!(e, Error::Rejected { code: 110017, .. }))
    );
}
