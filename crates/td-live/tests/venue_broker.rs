//! A real `Session` on a `VenueBroker` against the mock venue on 127.0.0.1. The strategy side is a
//! test double that submits a scripted list of orders.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use td_core::{
    Candle, FeeSchedule, Instrument, InstrumentId, InstrumentKind, Millis, OrderId, OrderRequest,
    OrderType, Side, TimeInForce, Timeframe, Venue,
};
use td_engine::{BrokerEvent, Context, OrderStatus, Session, SessionConfig, Strategy};
use td_live::{BrokerRecord, FaultKind, VenueBroker, VenueHandle};
use td_mock_bybit::{Fault, MockVenue, TEST_KEY, TEST_SECRET};
use td_orders::{Client, ClientOrderId, DemoKeys, Endpoint, Timing};

const SYMBOL: &str = "BTCUSDT";
const PRICE: f64 = 65000.0;
const CREATE: &str = "/v5/order/create";
const CANCEL: &str = "/v5/order/cancel";
const AMEND: &str = "/v5/order/amend";
const REALTIME: &str = "/v5/order/realtime";
const HOUR: Millis = 3_600_000;

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

fn client_for(mock: &MockVenue) -> Arc<Client> {
    let timing = Timing {
        connect_timeout: Duration::from_secs(1),
        request_timeout: Duration::from_millis(1500),
        recv_window: Duration::from_millis(1000),
        settle: Duration::from_millis(50),
    };
    let keys = DemoKeys::for_loopback(TEST_KEY, TEST_SECRET).unwrap();
    Arc::new(Client::with_timing(Endpoint::Loopback(mock.port()), keys, timing).unwrap())
}

fn market(side: Side, qty: f64, reduce_only: bool) -> OrderRequest {
    OrderRequest {
        instrument: instrument().id,
        side,
        qty,
        order_type: OrderType::Market,
        tif: TimeInForce::Gtc,
        reduce_only,
        client_tag: None,
    }
}

fn stop(side: Side, qty: f64, trigger: f64) -> OrderRequest {
    OrderRequest {
        order_type: OrderType::Stop { trigger },
        reduce_only: true,
        ..market(side, qty, true)
    }
}

#[derive(Clone)]
enum Act {
    Submit(OrderRequest),
    SubmitWithStop(OrderRequest, f64),
    Cancel(u64),
}

/// Submits the scripted orders on the bars they are keyed to (0 is the first bar fed), and
/// writes down what each call returned.
struct Script {
    acts: Vec<(usize, Act)>,
    bar: usize,
    results: Arc<Mutex<Vec<String>>>,
}

impl Strategy for Script {
    fn on_bar(&mut self, ctx: &mut Context, _bar: &Candle) {
        for (at, act) in self.acts.iter().filter(|(at, _)| *at == self.bar) {
            let result = match act {
                Act::Submit(r) => format!("{at}: submit {:?}", ctx.submit(r.clone())),
                Act::SubmitWithStop(r, t) => {
                    format!(
                        "{at}: submit_with_stop {:?}",
                        ctx.submit_with_stop(r.clone(), *t)
                    )
                }
                Act::Cancel(id) => format!("{at}: cancel {}", ctx.cancel(OrderId(*id))),
            };
            self.results.lock().unwrap().push(result);
        }
        self.bar += 1;
    }
}

fn config(starting_cash: f64, max_leverage: f64) -> SessionConfig {
    SessionConfig {
        starting_cash,
        slippage_bps: 0.0,
        fees: None,
        max_leverage,
        account_currency: Default::default(),
    }
}

struct Rig {
    mock: MockVenue,
    session: Session,
    handle: VenueHandle,
    results: Arc<Mutex<Vec<String>>>,
    acts: Vec<(usize, Act)>,
    /// Open time of the first bar.
    start: Millis,
    fed: usize,
}

impl Rig {
    /// A session whose first bar is the last closed hour, so its orders are fresh.
    fn new(acts: Vec<(usize, Act)>) -> Rig {
        let mock = MockVenue::start();
        mock.add_instrument(&instrument(), PRICE);
        let start = Timeframe::H1.floor(mock.now()) - HOUR;
        Rig::on(mock, acts, start, config(10_000.0, 1.0))
    }

    fn on(mock: MockVenue, acts: Vec<(usize, Act)>, start: Millis, config: SessionConfig) -> Rig {
        let (session, handle, results) = Rig::session(&mock, &acts, config);
        Rig {
            mock,
            session,
            handle,
            results,
            acts,
            start,
            fed: 0,
        }
    }

    fn session(
        mock: &MockVenue,
        acts: &[(usize, Act)],
        config: SessionConfig,
    ) -> (Session, VenueHandle, Arc<Mutex<Vec<String>>>) {
        let (broker, handle) =
            VenueBroker::new(client_for(mock), instrument(), Timeframe::H1, "t").unwrap();
        let results = Arc::new(Mutex::new(Vec::new()));
        let script = Script {
            acts: acts.to_vec(),
            bar: 0,
            results: results.clone(),
        };
        let session = Session::with_broker(
            Box::new(script),
            instrument(),
            Timeframe::H1,
            config,
            Box::new(broker),
        )
        .unwrap();
        (session, handle, results)
    }

    fn candle(&self, i: usize, low: f64) -> Candle {
        Candle {
            open_time: self.start + i as Millis * HOUR,
            open: PRICE,
            high: PRICE,
            low,
            close: PRICE,
            volume: 1.0,
            quote_volume: PRICE,
        }
    }

    fn bar(&mut self) {
        self.bar_with_low(PRICE);
    }

    fn bar_with_low(&mut self, low: f64) {
        let candle = self.candle(self.fed, low);
        self.session.on_bar(candle, &[]).unwrap();
        self.fed += 1;
    }

    fn status(&self, id: u64) -> OrderStatus {
        self.session.orders()[id as usize - 1].status.clone()
    }

    fn sends(&self) -> usize {
        self.mock.request_count(CREATE)
            + self.mock.request_count(CANCEL)
            + self.mock.request_count(AMEND)
    }

    fn fault_kind(&self) -> Option<FaultKind> {
        self.handle.fault().map(|f| f.kind)
    }

    fn results(&self) -> Vec<String> {
        self.results.lock().unwrap().clone()
    }
}

fn close(a: f64, b: f64) -> bool {
    (a - b).abs() < 1e-9
}

#[test]
fn a_market_entry_and_exit_match_the_venue_fills_fees_and_position() {
    let mut rig = Rig::new(vec![
        (0, Act::Submit(market(Side::Buy, 0.004, false))),
        (2, Act::Submit(market(Side::Sell, 0.004, true))),
    ]);
    rig.bar();
    assert_eq!(rig.mock.position(SYMBOL), 0.004, "sent at once");
    assert_eq!(
        rig.session.portfolio().position(),
        0.0,
        "reported on the next bar"
    );
    rig.bar();
    assert_eq!(rig.session.portfolio().position(), 0.004);
    assert_eq!(rig.status(1), OrderStatus::Filled);
    rig.mock.set_price(SYMBOL, 65100.0);
    rig.bar();
    rig.bar();
    assert_eq!(rig.mock.position(SYMBOL), 0.0);
    assert_eq!(rig.session.portfolio().position(), 0.0);
    let venue = rig.mock.executions();
    let fills = rig.session.fills();
    assert_eq!(fills.len(), 2);
    assert_eq!(venue.len(), 2);
    for (fill, exec) in fills.iter().zip(&venue) {
        assert!(close(fill.price, exec.price), "{fill:?} {exec:?}");
        assert!(close(fill.qty, exec.qty));
        assert!(close(fill.fee, exec.fee));
    }
    assert_eq!(fills[1].price, 65100.0);
    let fees: f64 = venue.iter().map(|e| e.fee).sum();
    assert!(close(rig.session.portfolio().fees_paid(), fees));
    assert_eq!(
        fills[0].time,
        rig.start + HOUR,
        "timed at the open of the bar reporting it"
    );
    assert_eq!(rig.handle.fault(), None);
}

#[test]
fn an_attached_stop_rests_at_once_triggers_between_bars_and_the_session_sees_the_fill() {
    let mut rig = Rig::new(vec![(
        0,
        Act::SubmitWithStop(market(Side::Buy, 0.004, false), 64000.0),
    )]);
    rig.bar();
    let resting = rig.mock.orders_with_link_id("t-o2");
    assert_eq!(resting.len(), 1);
    assert_eq!(
        resting[0].status, "Untriggered",
        "the stop rests before the next bar"
    );
    assert_eq!(resting[0].qty, 0.004);
    assert_eq!(rig.status(2), OrderStatus::Attached);
    rig.bar();
    assert_eq!(rig.status(1), OrderStatus::Filled);
    assert_eq!(rig.status(2), OrderStatus::Open);
    assert_eq!(rig.session.open_orders().len(), 1);
    assert_eq!(rig.handle.resting_orders()[0].id, OrderId(2));
    rig.mock.set_price(SYMBOL, 63900.0);
    assert_eq!(
        rig.mock.position(SYMBOL),
        0.0,
        "the stop triggered between bars"
    );
    rig.bar();
    assert_eq!(rig.status(2), OrderStatus::Filled);
    assert_eq!(rig.session.portfolio().position(), 0.0);
    assert_eq!(rig.session.fills()[1].price, 63900.0);
    assert!(rig.session.open_orders().is_empty());
    assert_eq!(rig.handle.fault(), None);
}

#[test]
fn a_stop_the_venue_will_not_take_is_replaced_by_a_close_at_market() {
    let mut rig = Rig::new(vec![(
        0,
        Act::SubmitWithStop(market(Side::Buy, 0.004, false), 64000.0),
    )]);
    // The entry fills below its stop's trigger, so the stop would trigger at once.
    rig.mock.set_price(SYMBOL, 63500.0);
    rig.bar();
    assert!(rig.mock.orders_with_link_id("t-o2").is_empty());
    assert_eq!(rig.mock.orders_with_link_id("t-c2")[0].status, "Filled");
    assert_eq!(rig.mock.position(SYMBOL), 0.0);
    rig.bar();
    assert_eq!(rig.status(1), OrderStatus::Filled);
    assert_eq!(
        rig.status(2),
        OrderStatus::Filled,
        "the close is the stop's fill"
    );
    assert_eq!(rig.session.portfolio().position(), 0.0);
    assert_eq!(rig.session.fills().len(), 2);
    assert_eq!(rig.handle.fault(), None);
}

#[test]
fn a_partial_fill_is_reported_and_its_stop_grows_with_the_fill() {
    let mut rig = Rig::new(vec![(
        0,
        Act::SubmitWithStop(market(Side::Buy, 0.004, false), 60000.0),
    )]);
    rig.mock.cap_next_fill(0.002);
    rig.bar();
    assert_eq!(rig.mock.orders_with_link_id("t-o2")[0].qty, 0.002);
    rig.bar();
    assert_eq!(rig.session.portfolio().position(), 0.002);
    assert_eq!(rig.session.orders()[1].request.qty, 0.002);
    rig.mock.fill_rest("t-o1");
    rig.bar();
    assert_eq!(rig.session.portfolio().position(), 0.004);
    assert_eq!(rig.mock.orders_with_link_id("t-o2")[0].qty, 0.004);
    assert_eq!(rig.session.orders()[1].request.qty, 0.004);
    let fills = rig.session.fills();
    assert_eq!(fills.len(), 2);
    assert!(
        fills
            .iter()
            .all(|f| f.order_id == OrderId(1) && f.qty == 0.002)
    );
    rig.bar();
    assert_eq!(rig.handle.fault(), None);
}

#[test]
fn unsupported_order_types_are_cancelled_with_a_reason_and_never_sent() {
    let limit = OrderRequest {
        order_type: OrderType::Limit { price: 64000.0 },
        ..market(Side::Buy, 0.004, false)
    };
    let opening_stop = OrderRequest {
        reduce_only: false,
        ..stop(Side::Buy, 0.004, 66000.0)
    };
    let mut rig = Rig::new(vec![
        (0, Act::Submit(limit)),
        (0, Act::Submit(opening_stop)),
    ]);
    rig.bar();
    assert_eq!(
        rig.status(1),
        OrderStatus::Open,
        "cancelled on the next bar"
    );
    rig.bar();
    let reason = |id| match rig.status(id) {
        OrderStatus::Cancelled(r) => r,
        other => panic!("{other:?}"),
    };
    assert!(reason(1).contains("limit"), "{}", reason(1));
    assert!(reason(2).contains("reduce-only"), "{}", reason(2));
    assert_eq!(rig.mock.request_count(CREATE), 0);
    assert_eq!(rig.handle.fault(), None);
}

#[test]
fn a_resting_stop_cancels_and_one_that_already_filled_does_not() {
    let mut rig = Rig::new(vec![
        (0, Act::Submit(market(Side::Buy, 0.004, false))),
        (1, Act::Submit(stop(Side::Sell, 0.004, 60000.0))),
        (1, Act::Submit(stop(Side::Sell, 0.004, 64000.0))),
        (2, Act::Cancel(2)),
    ]);
    rig.bar();
    rig.bar();
    rig.bar();
    assert_eq!(rig.results()[3], "2: cancel true");
    assert!(matches!(rig.status(2), OrderStatus::Cancelled(_)));
    assert_eq!(
        rig.mock.orders_with_link_id("t-o2")[0].status,
        "Deactivated"
    );
    // Stop 3 triggers between bars; a manual cancel then finds it filled.
    rig.mock.set_price(SYMBOL, 63900.0);
    assert!(!rig.session.cancel(OrderId(3), "manual"));
    assert_eq!(rig.status(3), OrderStatus::Open);
    rig.bar();
    assert_eq!(
        rig.status(3),
        OrderStatus::Filled,
        "its fill arrives with the executions"
    );
    assert_eq!(rig.session.portfolio().position(), 0.0);
    assert_eq!(rig.handle.fault(), None);
}

#[test]
fn a_place_whose_answer_is_lost_after_it_reached_the_venue_is_one_order_filled_once() {
    let mut rig = Rig::new(vec![(0, Act::Submit(market(Side::Buy, 0.004, false)))]);
    rig.mock.inject(CREATE, Fault::DropAfterApply);
    rig.mock.inject_always(REALTIME, Fault::DropBeforeApply);
    rig.bar();
    rig.mock.clear_faults();
    assert_eq!(rig.mock.orders_with_link_id("t-o1").len(), 1);
    rig.bar();
    rig.bar();
    assert_eq!(rig.mock.request_count(CREATE), 1, "never sent again");
    assert_eq!(rig.status(1), OrderStatus::Filled);
    assert_eq!(rig.session.fills().len(), 1, "reported once");
    assert_eq!(rig.session.portfolio().position(), 0.004);
    assert_eq!(rig.handle.fault(), None);
}

#[test]
fn a_place_whose_answer_is_lost_before_it_reached_the_venue_is_cancelled_not_resent() {
    let mut rig = Rig::new(vec![(
        0,
        Act::SubmitWithStop(market(Side::Buy, 0.004, false), 60000.0),
    )]);
    rig.mock.inject(CREATE, Fault::DropBeforeApply);
    rig.mock.inject_always(REALTIME, Fault::DropBeforeApply);
    rig.bar();
    rig.mock.clear_faults();
    rig.bar();
    assert!(
        matches!(rig.status(1), OrderStatus::Cancelled(_)),
        "{:?}",
        rig.status(1)
    );
    assert!(matches!(rig.status(2), OrderStatus::Cancelled(_)));
    assert_eq!(rig.mock.request_count(CREATE), 1);
    assert!(rig.mock.orders().is_empty());
    assert_eq!(rig.handle.fault(), None);
}

#[test]
fn an_unknown_outcome_the_venue_cannot_settle_faults_after_three_asks() {
    let mut rig = Rig::new(vec![(0, Act::Submit(market(Side::Buy, 0.004, false)))]);
    rig.mock.inject(CREATE, Fault::DropBeforeApply);
    rig.mock.inject_always(REALTIME, Fault::DropBeforeApply);
    for _ in 0..3 {
        assert_eq!(rig.fault_kind(), None);
        rig.bar();
    }
    // Bar 0 left it unknown; bars 1 and 2 could not ask (nor read), bar 3 faults.
    rig.bar();
    assert!(
        matches!(
            rig.fault_kind(),
            Some(FaultKind::UnresolvedOrder | FaultKind::VenueUnreadable)
        ),
        "{:?}",
        rig.handle.fault()
    );
    assert_eq!(rig.mock.request_count(CREATE), 1);
}

#[test]
fn a_stale_decision_is_not_sent() {
    let mock = MockVenue::start();
    mock.add_instrument(&instrument(), PRICE);
    let start = Timeframe::H1.floor(mock.now()) - 10 * HOUR;
    let acts = vec![(0, Act::Submit(market(Side::Buy, 0.004, false)))];
    let mut rig = Rig::on(mock, acts, start, config(10_000.0, 1.0));
    rig.bar();
    rig.bar();
    match rig.status(1) {
        OrderStatus::Cancelled(r) => assert!(r.contains("more than one bar"), "{r}"),
        other => panic!("{other:?}"),
    }
    assert_eq!(rig.mock.request_count(CREATE), 0);
    assert_eq!(rig.handle.fault(), None);
}

#[test]
fn a_stale_decision_the_venue_already_holds_is_adopted() {
    let mock = MockVenue::start();
    mock.add_instrument(&instrument(), PRICE);
    let start = Timeframe::H1.floor(mock.now()) - 10 * HOUR;
    let mut rig = Rig::on(mock, vec![], start, config(10_000.0, 1.0));
    rig.bar();
    // A manual order accepted at the first bar's close reaches the session hours later, after
    // an earlier attempt already sent it under the same id.
    let earlier = client_for(&rig.mock);
    let id = ClientOrderId::new("t-o1").unwrap();
    earlier
        .place(&instrument(), &market(Side::Buy, 0.004, false), &id, PRICE)
        .unwrap();
    let order = rig
        .session
        .submit(market(Side::Buy, 0.004, false), start + HOUR)
        .unwrap();
    assert_eq!(order.id, OrderId(1));
    rig.bar();
    assert_eq!(
        rig.status(1),
        OrderStatus::Filled,
        "{:?}",
        rig.handle.fault()
    );
    assert_eq!(rig.session.portfolio().position(), 0.004);
    assert_eq!(rig.mock.request_count(CREATE), 1, "only the earlier send");
    assert_eq!(rig.handle.fault(), None);
}

#[test]
fn a_foreign_execution_faults_and_a_fault_stops_all_sending() {
    let mut rig = Rig::new(vec![
        (0, Act::Submit(market(Side::Buy, 0.004, false))),
        (2, Act::Submit(market(Side::Buy, 0.004, false))),
    ]);
    rig.bar();
    rig.mock.seed_trades(SYMBOL, 1, rig.mock.now(), 1);
    rig.bar();
    let fault = rig.handle.fault().expect("faulted");
    assert_eq!(fault.kind, FaultKind::ForeignExecution);
    assert!(fault.message.contains("seed-"), "{fault}");
    let sends = rig.sends();
    rig.bar();
    rig.bar();
    assert_eq!(rig.sends(), sends, "nothing sent after the fault");
    match rig.status(2) {
        OrderStatus::Cancelled(r) => assert!(r.contains("fault"), "{r}"),
        other => panic!("{other:?}"),
    }
    // Nothing at the venue was touched: the position stays.
    assert_eq!(rig.mock.position(SYMBOL), 0.004);
}

#[test]
fn a_position_the_venue_disagrees_with_faults() {
    let mut rig = Rig::new(vec![(0, Act::Submit(market(Side::Buy, 0.004, false)))]);
    rig.bar();
    rig.mock.set_position(SYMBOL, 0.006, PRICE);
    rig.bar();
    let fault = rig.handle.fault().expect("faulted");
    assert_eq!(fault.kind, FaultKind::PositionMismatch);
    assert!(
        fault.message.contains("0.006") && fault.message.contains("0.004"),
        "{fault}"
    );
    assert_eq!(rig.mock.position(SYMBOL), 0.006, "no flatten");
}

#[test]
fn an_open_order_the_broker_does_not_know_faults() {
    let mut rig = Rig::new(vec![]);
    let other = client_for(&rig.mock);
    let id = ClientOrderId::new("zz-o1").unwrap();
    other
        .place(&instrument(), &stop(Side::Sell, 0.004, 60000.0), &id, PRICE)
        .unwrap();
    rig.bar();
    let fault = rig.handle.fault().expect("faulted");
    assert_eq!(fault.kind, FaultKind::UnknownOpenOrder);
    assert!(fault.message.contains("zz-o1"), "{fault}");
    assert_eq!(rig.mock.request_count(CANCEL), 0, "no cancel");
}

#[test]
fn a_bar_on_which_the_session_would_liquidate_faults_in_that_bar() {
    let mock = MockVenue::start();
    mock.add_instrument(&instrument(), PRICE);
    let start = Timeframe::H1.floor(mock.now()) - HOUR;
    let acts = vec![(0, Act::Submit(market(Side::Buy, 0.004, false)))];
    // 260 of position on 100 of cash: the session's model liquidates near 40200.
    let mut rig = Rig::on(mock, acts, start, config(100.0, 5.0));
    rig.bar();
    rig.bar();
    assert_eq!(rig.handle.fault(), None);
    rig.bar_with_low(40000.0);
    let fault = rig.handle.fault().expect("faulted");
    assert_eq!(fault.kind, FaultKind::SessionLiquidation, "{fault}");
    assert_eq!(
        rig.mock.position(SYMBOL),
        0.004,
        "the venue position is left alone"
    );
}

/// Runs `bars` live bars, taking a record after each, and returns the records as stored text.
fn run_recorded(rig: &mut Rig, bars: usize) -> Vec<String> {
    (0..bars)
        .map(|_| {
            rig.bar();
            serde_json::to_string(&rig.handle.take_record()).unwrap()
        })
        .collect()
}

/// A second session on the same venue, same prefix and script, replaying `records`.
fn replayed(rig: &Rig, records: &[String]) -> (Session, VenueHandle) {
    let (mut session, handle, _) = Rig::session(&rig.mock, &rig.acts, config(10_000.0, 1.0));
    for (i, text) in records.iter().enumerate() {
        let record: BrokerRecord = serde_json::from_str(text).unwrap();
        handle.replay(record);
        session.on_bar(rig.candle(i, PRICE), &[]).unwrap();
    }
    (session, handle)
}

#[test]
fn a_replay_then_go_live_on_an_unchanged_venue_sends_nothing() {
    let mut rig = Rig::new(vec![
        (
            0,
            Act::SubmitWithStop(market(Side::Buy, 0.004, false), 60000.0),
        ),
        (2, Act::Submit(market(Side::Sell, 0.002, true))),
    ]);
    let records = run_recorded(&mut rig, 4);
    assert_eq!(rig.handle.fault(), None);
    // Building a client reads the venue's clock; nothing else may be asked during a replay.
    let asked = |mock: &MockVenue| {
        mock.requests()
            .iter()
            .filter(|r| r.path != "/v5/market/time")
            .count()
    };
    let requests = asked(&rig.mock);
    let sends = rig.sends();
    let (mut session, handle) = replayed(&rig, &records);
    assert_eq!(
        asked(&rig.mock),
        requests,
        "a replay asks the venue nothing"
    );
    handle.go_live().unwrap();
    assert_eq!(rig.sends(), sends, "going live sends and cancels nothing");
    assert_eq!(session.orders(), rig.session.orders());
    assert_eq!(session.fills(), rig.session.fills());
    assert_eq!(
        session.portfolio().position(),
        rig.session.portfolio().position()
    );
    assert_eq!(session.open_orders(), rig.session.open_orders());
    // It carries on live from where the record left off.
    rig.mock.set_price(SYMBOL, 59900.0);
    session.on_bar(rig.candle(4, PRICE), &[]).unwrap();
    assert_eq!(handle.fault(), None);
    assert_eq!(
        session.portfolio().position(),
        0.0,
        "the stop's fill is reported"
    );
}

#[test]
fn a_replay_whose_last_order_was_unresolved_adopts_it_at_go_live() {
    let mut rig = Rig::new(vec![
        (0, Act::Submit(market(Side::Buy, 0.004, false))),
        (1, Act::Submit(market(Side::Sell, 0.004, true))),
    ]);
    let mut records = run_recorded(&mut rig, 1);
    rig.mock.inject(CREATE, Fault::DropAfterApply);
    rig.mock.inject_always(REALTIME, Fault::DropBeforeApply);
    records.extend(run_recorded(&mut rig, 1));
    rig.mock.clear_faults();
    assert_eq!(
        rig.mock.position(SYMBOL),
        0.0,
        "the close reached the venue"
    );
    let creates = rig.mock.request_count(CREATE);
    // The live run stops here; a restarted session replays both records.
    let (mut session, handle) = replayed(&rig, &records);
    handle.go_live().unwrap();
    assert_eq!(
        rig.mock.request_count(CREATE),
        creates,
        "adopted, not resent"
    );
    session.on_bar(rig.candle(2, PRICE), &[]).unwrap();
    assert_eq!(handle.fault(), None);
    assert_eq!(session.orders()[1].status, OrderStatus::Filled);
    assert_eq!(session.portfolio().position(), 0.0);
    assert_eq!(session.fills().len(), 2);
}

#[test]
fn a_replay_that_differs_from_the_record_faults() {
    let mut rig = Rig::new(vec![(0, Act::Submit(market(Side::Buy, 0.004, false)))]);
    let records = run_recorded(&mut rig, 1);
    // A session that submits nothing on bar 0.
    let (mut session, handle, _) = Rig::session(&rig.mock, &[], config(10_000.0, 1.0));
    handle.replay(serde_json::from_str(&records[0]).unwrap());
    session.on_bar(rig.candle(0, PRICE), &[]).unwrap();
    session.on_bar(rig.candle(1, PRICE), &[]).unwrap();
    assert_eq!(
        handle.fault().map(|f| f.kind),
        Some(FaultKind::ReplayDiverged)
    );
    assert!(handle.go_live().is_err());
}

#[test]
fn records_hold_the_events_the_session_was_given() {
    let mut rig = Rig::new(vec![(0, Act::Submit(market(Side::Buy, 0.004, false)))]);
    rig.bar();
    rig.handle.take_record();
    rig.bar();
    let text = serde_json::to_string(&rig.handle.take_record()).unwrap();
    let fill = BrokerEvent::Filled(rig.session.fills()[0].clone());
    assert!(
        text.contains(&serde_json::to_string(&fill).unwrap()),
        "{text}"
    );
}
