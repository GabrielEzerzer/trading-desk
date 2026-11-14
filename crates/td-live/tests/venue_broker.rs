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
const HISTORY: &str = "/v5/order/history";
const EXECUTIONS: &str = "/v5/execution/list";
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

/// The instrument under another symbol.
fn instrument_named(symbol: &str) -> Instrument {
    let mut instrument = instrument();
    instrument.id.symbol = symbol.into();
    instrument.base = symbol.trim_end_matches("USDT").into();
    instrument
}

/// A mock venue listing the instrument.
fn venue() -> Arc<MockVenue> {
    let mock = Arc::new(MockVenue::start());
    mock.add_instrument(&instrument(), PRICE);
    mock
}

fn client_for(mock: &MockVenue) -> Arc<Client> {
    client_timed(mock, Duration::from_millis(1000), Duration::from_millis(50))
}

/// A client whose lookup after a lost answer runs exactly the route's fewest rounds, three: its
/// pause is as long as its receive window, so the window has passed by the third round. The window
/// is wide enough that a slow machine does not see the mock expire its requests.
fn counted_client(mock: &MockVenue) -> Arc<Client> {
    client_timed(mock, Duration::from_millis(500), Duration::from_millis(500))
}

fn client_timed(mock: &MockVenue, recv_window: Duration, settle: Duration) -> Arc<Client> {
    let timing = Timing {
        connect_timeout: Duration::from_secs(1),
        request_timeout: Duration::from_millis(1500),
        recv_window,
        settle,
        operation_budget: Duration::from_secs(5),
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
    /// Runs test code at that point of the bar, after the broker's reads; a replay leaves it out.
    Run(Arc<dyn Fn() + Send + Sync>),
}

fn run(f: impl Fn() + Send + Sync + 'static) -> Act {
    Act::Run(Arc::new(f))
}

/// An act that loses the answer to the next create (`fault` says whether the venue applied it)
/// and makes every read by client id fail until the faults are cleared.
fn lose_next_create(mock: &Arc<MockVenue>, fault: Fault) -> Act {
    let mock = mock.clone();
    run(move || {
        mock.inject(CREATE, fault.clone());
        mock.inject_always(REALTIME, Fault::DropBeforeApply);
    })
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
                Act::Run(f) => {
                    f();
                    format!("{at}: run")
                }
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
    mock: Arc<MockVenue>,
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
        Rig::with(venue(), acts)
    }

    /// [`Rig::new`] on `mock`, for acts that need the mock.
    fn with(mock: Arc<MockVenue>, acts: Vec<(usize, Act)>) -> Rig {
        let start = Timeframe::H1.floor(mock.now()) - HOUR;
        Rig::on(mock, acts, start, config(10_000.0, 1.0))
    }

    /// [`Rig::with`] on a [`counted_client`], for faults that must outlast a lookup exactly.
    fn counted(mock: Arc<MockVenue>, acts: Vec<(usize, Act)>) -> Rig {
        let start = Timeframe::H1.floor(mock.now()) - HOUR;
        let client = counted_client(&mock);
        let (session, handle, results) =
            session_with(client, &acts, config(10_000.0, 1.0), "t", instrument());
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

    fn on(
        mock: Arc<MockVenue>,
        acts: Vec<(usize, Act)>,
        start: Millis,
        config: SessionConfig,
    ) -> Rig {
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
        session_on(mock, acts, config, "t", instrument())
    }

    /// Just before the close of the last bar fed: a manual order accepted then is decided on it.
    fn accepted(&self) -> Millis {
        self.start + self.fed as Millis * HOUR - 1
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

/// A session under `prefix` on `instrument`, running the scripted acts on a venue broker.
fn session_on(
    mock: &MockVenue,
    acts: &[(usize, Act)],
    config: SessionConfig,
    prefix: &str,
    instrument: Instrument,
) -> (Session, VenueHandle, Arc<Mutex<Vec<String>>>) {
    session_with(client_for(mock), acts, config, prefix, instrument)
}

/// [`session_on`] through `client`.
fn session_with(
    client: Arc<Client>,
    acts: &[(usize, Act)],
    config: SessionConfig,
    prefix: &str,
    instrument: Instrument,
) -> (Session, VenueHandle, Arc<Mutex<Vec<String>>>) {
    let (broker, handle) =
        VenueBroker::new(client, instrument.clone(), Timeframe::H1, prefix).unwrap();
    let results = Arc::new(Mutex::new(Vec::new()));
    let script = Script {
        acts: acts.to_vec(),
        bar: 0,
        results: results.clone(),
    };
    let session = Session::with_broker(
        Box::new(script),
        instrument,
        Timeframe::H1,
        config,
        Box::new(broker),
    )
    .unwrap();
    (session, handle, results)
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
    let mock = venue();
    let lose = lose_next_create(&mock, Fault::DropAfterApply);
    let mut rig = Rig::with(
        mock,
        vec![(0, lose), (0, Act::Submit(market(Side::Buy, 0.004, false)))],
    );
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
    let mock = venue();
    let lose = lose_next_create(&mock, Fault::DropBeforeApply);
    let mut rig = Rig::with(
        mock,
        vec![
            (0, lose),
            (
                0,
                Act::SubmitWithStop(market(Side::Buy, 0.004, false), 60000.0),
            ),
        ],
    );
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
    let mock = venue();
    let lose = lose_next_create(&mock, Fault::DropBeforeApply);
    let mut rig = Rig::with(
        mock,
        vec![(0, lose), (0, Act::Submit(market(Side::Buy, 0.004, false)))],
    );
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
    let mock = venue();
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
    let mock = venue();
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
    let mock = venue();
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
    let acts: Vec<_> = rig
        .acts
        .iter()
        .filter(|(_, act)| !matches!(act, Act::Run(_)))
        .cloned()
        .collect();
    let (mut session, handle, _) = Rig::session(&rig.mock, &acts, config(10_000.0, 1.0));
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

fn record(text: &str) -> BrokerRecord {
    serde_json::from_str(text).unwrap()
}

#[test]
fn after_a_fault_go_live_sends_no_order_and_no_cancel() {
    let mut rig = Rig::new(vec![
        (0, Act::Submit(market(Side::Buy, 0.004, false))),
        (1, Act::Submit(stop(Side::Sell, 0.004, 60000.0))),
    ]);
    let mut records = run_recorded(&mut rig, 2);
    // A manual order decided on a later bar whose create is lost before it reaches the venue.
    let accepted = rig.start + 3 * HOUR + 1;
    rig.mock.inject(CREATE, Fault::DropBeforeApply);
    rig.mock.inject_always(REALTIME, Fault::DropBeforeApply);
    let manual = market(Side::Buy, 0.002, false);
    assert_eq!(
        rig.session.submit(manual.clone(), accepted).unwrap().id,
        OrderId(3)
    );
    rig.mock.clear_faults();
    records.push(serde_json::to_string(&rig.handle.take_record()).unwrap());
    // Another session that reuses the prefix sends the same id before the restart.
    let other = client_for(&rig.mock);
    other
        .place(
            &instrument(),
            &manual,
            &ClientOrderId::new("t-o3").unwrap(),
            PRICE,
        )
        .unwrap();
    let sends = rig.sends();
    let (mut session, handle) = replayed(&rig, &records[..2]);
    handle.replay(record(&records[2]));
    session.submit(manual.clone(), accepted).unwrap();
    // Past the record: one more order and a cancel of the resting stop, which answers false.
    session.submit(manual, accepted).unwrap();
    assert!(!session.cancel(OrderId(2), "manual"));
    let fault = handle.go_live().expect_err("the reused id faults");
    assert_eq!(fault.kind, FaultKind::IdCollision, "{fault}");
    assert_eq!(rig.sends(), sends, "nothing placed or cancelled after it");
    assert!(rig.mock.orders_with_link_id("t-o4").is_empty());
    assert_eq!(
        rig.mock.orders_with_link_id("t-o2")[0].status,
        "Untriggered"
    );
}

#[test]
fn a_stop_for_a_fill_confirmed_in_the_faulting_bar_is_still_placed_and_nothing_else_is() {
    let mock = venue();
    let lose = lose_next_create(&mock, Fault::DropAfterApply);
    let mut rig = Rig::with(
        mock,
        vec![
            (0, lose),
            (
                0,
                Act::SubmitWithStop(market(Side::Buy, 0.004, false), 60000.0),
            ),
            (1, Act::Submit(market(Side::Buy, 0.002, false))),
        ],
    );
    rig.bar();
    rig.mock.clear_faults();
    assert_eq!(rig.mock.position(SYMBOL), 0.004);
    assert!(
        rig.mock.orders_with_link_id("t-o2").is_empty(),
        "the outcome of the entry is unknown, so its stop waits"
    );
    let sends = rig.sends();
    // A trade of another session lands after the entry's fill, so the bar that reads the fill
    // faults too.
    rig.mock.seed_trades(SYMBOL, 1, rig.mock.now() + 1, 1);
    rig.bar();
    assert_eq!(rig.fault_kind(), Some(FaultKind::ForeignExecution));
    let resting = rig.mock.orders_with_link_id("t-o2");
    assert_eq!(resting.len(), 1, "the confirmed fill is protected");
    assert_eq!(resting[0].status, "Untriggered");
    assert_eq!(resting[0].qty, 0.004);
    assert_eq!(rig.sends(), sends + 1, "and nothing else goes out");
    rig.bar();
    assert_eq!(rig.sends(), sends + 1);
    assert!(matches!(rig.status(3), OrderStatus::Cancelled(_)));
}

#[test]
fn a_replayed_cancel_of_an_order_whose_outcome_is_unknown_is_refused_as_live() {
    let mut rig = Rig::new(vec![]);
    let mut records = run_recorded(&mut rig, 1);
    rig.mock.inject(CREATE, Fault::DropBeforeApply);
    rig.mock.inject_always(REALTIME, Fault::DropBeforeApply);
    let manual = market(Side::Buy, 0.004, false);
    let accepted = rig.accepted();
    rig.session.submit(manual.clone(), accepted).unwrap();
    rig.mock.clear_faults();
    records.push(serde_json::to_string(&rig.handle.take_record()).unwrap());
    let (mut session, handle) = replayed(&rig, &records[..1]);
    handle.replay(record(&records[1]));
    session.submit(manual, accepted).unwrap();
    assert!(
        !session.cancel(OrderId(1), "manual"),
        "the live path refuses to cancel an order whose outcome is unknown"
    );
    handle.go_live().unwrap();
    session.on_bar(rig.candle(1, PRICE), &[]).unwrap();
    assert_eq!(handle.fault(), None);
    match &session.orders()[0].status {
        OrderStatus::Cancelled(r) => assert!(r.contains("restarted"), "{r}"),
        other => panic!("{other:?}"),
    }
    assert_eq!(session.portfolio().position(), 0.0);
    assert_eq!(rig.mock.position(SYMBOL), 0.0);
    assert_eq!(
        rig.mock.request_count(CREATE),
        1,
        "the lost create only: nothing is sent from the record"
    );
    assert_eq!(rig.mock.request_count(CANCEL), 0);
}

#[test]
fn a_record_made_under_another_prefix_or_instrument_is_refused() {
    let mut rig = Rig::new(vec![(0, Act::Submit(market(Side::Buy, 0.004, false)))]);
    let records = run_recorded(&mut rig, 1);
    let sends = rig.sends();
    let (mut session, handle, _) = session_on(
        &rig.mock,
        &rig.acts,
        config(10_000.0, 1.0),
        "u",
        instrument(),
    );
    handle.replay(record(&records[0]));
    session.on_bar(rig.candle(0, PRICE), &[]).unwrap();
    assert_eq!(
        handle.fault().map(|f| f.kind),
        Some(FaultKind::ReplayDiverged)
    );
    assert!(handle.go_live().is_err());
    assert_eq!(rig.sends(), sends);
    let (_, handle, _) = session_on(
        &rig.mock,
        &[],
        config(10_000.0, 1.0),
        "t",
        instrument_named("ETHUSDT"),
    );
    handle.replay(record(&records[0]));
    let fault = handle.fault().expect("refused");
    assert_eq!(fault.kind, FaultKind::ReplayDiverged);
    assert!(fault.message.contains("instrument"), "{fault}");
}

type Edit = fn(&mut serde_json::Value, &serde_json::Value);

#[test]
fn an_edited_record_is_refused_and_nothing_is_sent() {
    let mock = venue();
    let lose = lose_next_create(&mock, Fault::DropBeforeApply);
    let mut rig = Rig::with(
        mock,
        vec![(0, lose), (0, Act::Submit(market(Side::Buy, 0.004, false)))],
    );
    let records = run_recorded(&mut rig, 1);
    rig.mock.clear_faults();
    let sell = serde_json::to_value(Side::Sell).unwrap();
    let edits: [(&str, Edit); 3] = [
        ("quantity", |order, _| {
            order["order"]["request"]["qty"] = serde_json::json!(0.003)
        }),
        ("side", |order, sell| {
            order["order"]["request"]["side"] = sell.clone()
        }),
        ("negative sent", |order, _| {
            order["sent"] = serde_json::json!(-0.004)
        }),
    ];
    for (what, edit) in edits {
        let mut value: serde_json::Value = serde_json::from_str(&records[0]).unwrap();
        edit(&mut value["book"]["orders"]["1"], &sell);
        let creates = rig.mock.request_count(CREATE);
        let (_session, handle) = replayed(&rig, &[value.to_string()]);
        assert_eq!(
            handle.fault().map(|f| f.kind),
            Some(FaultKind::ReplayDiverged),
            "{what}"
        );
        assert!(handle.go_live().is_err(), "{what}");
        assert_eq!(rig.mock.request_count(CREATE), creates, "{what}");
    }
}

#[test]
fn a_record_whose_cursor_is_near_the_lowest_time_does_not_panic() {
    let mut rig = Rig::new(vec![]);
    let records = run_recorded(&mut rig, 1);
    let mut value: serde_json::Value = serde_json::from_str(&records[0]).unwrap();
    value["book"]["floor"] = serde_json::json!(i64::MIN);
    value["book"]["cursor"] = serde_json::json!(i64::MIN + 5);
    let sends = rig.sends();
    let (mut session, handle) = replayed(&rig, &[value.to_string()]);
    let _ = handle.go_live();
    session.on_bar(rig.candle(1, PRICE), &[]).unwrap();
    assert_eq!(rig.sends(), sends);
}

/// An entry of 0.004 that fills 0.002 at first, so its stop is placed for 0.002; then the rest
/// fills and resizing the stop is refused, and so is the next create when `create_fails`.
/// Returns the rig after the bar that reads the rest, with the records of every bar.
fn failed_resize(create_fails: bool) -> (Rig, Vec<String>) {
    let mut rig = Rig::new(vec![(
        0,
        Act::SubmitWithStop(market(Side::Buy, 0.004, false), 60000.0),
    )]);
    rig.mock.cap_next_fill(0.002);
    let mut records = run_recorded(&mut rig, 2);
    assert_eq!(rig.mock.orders_with_link_id("t-o2")[0].qty, 0.002);
    rig.mock.fill_rest("t-o1");
    rig.mock.inject(AMEND, Fault::Answer(10001));
    if create_fails {
        rig.mock.inject(CREATE, Fault::Answer(10001));
    }
    records.extend(run_recorded(&mut rig, 1));
    (rig, records)
}

#[test]
fn a_fault_survives_a_restart_and_go_live_sends_nothing() {
    let (rig, records) = failed_resize(true);
    let fault = rig.handle.fault().expect("faulted");
    assert_eq!(fault.kind, FaultKind::ProtectionFailed);
    assert!(fault.message.contains("not covered"), "{fault}");
    assert_eq!(rig.mock.position(SYMBOL), 0.004);
    assert_eq!(rig.mock.orders_with_link_id("t-o2")[0].qty, 0.002);
    let sends = rig.sends();
    let (_session, handle) = replayed(&rig, &records);
    assert_eq!(handle.go_live(), Err(fault));
    assert_eq!(rig.sends(), sends);
}

#[test]
fn a_stop_that_cannot_be_resized_gets_a_cover_for_the_rest_before_the_fault() {
    let (rig, _) = failed_resize(false);
    let fault = rig.handle.fault().expect("faulted");
    assert_eq!(fault.kind, FaultKind::ProtectionFailed);
    assert!(fault.message.contains("t-s2 now covers"), "{fault}");
    let cover = rig.mock.orders_with_link_id("t-s2");
    assert_eq!(cover.len(), 1);
    assert_eq!(cover[0].status, "Untriggered");
    assert_eq!(cover[0].qty, 0.002);
    assert_eq!(cover[0].side, Side::Sell);
    assert_eq!(cover[0].trigger, Some(60000.0));
    assert!(cover[0].reduce_only);
}

#[test]
fn while_the_venue_cannot_be_read_an_opening_order_is_not_sent() {
    let mut rig = Rig::new(vec![
        (0, Act::Submit(market(Side::Buy, 0.004, false))),
        (1, Act::Submit(market(Side::Buy, 0.002, false))),
    ]);
    rig.bar();
    rig.mock.inject_always(EXECUTIONS, Fault::DropBeforeApply);
    rig.bar();
    assert_eq!(rig.mock.request_count(CREATE), 1);
    rig.mock.clear_faults();
    rig.bar();
    match rig.status(2) {
        OrderStatus::Cancelled(r) => assert!(r.contains("could not be read"), "{r}"),
        other => panic!("{other:?}"),
    }
    assert_eq!(rig.status(1), OrderStatus::Filled);
    assert_eq!(rig.handle.fault(), None);
}

#[test]
fn an_accepted_entry_whose_fill_cannot_be_confirmed_gets_its_stop_at_once() {
    let mock = venue();
    let blind = mock.clone();
    let mut rig = Rig::with(
        mock,
        vec![
            (
                0,
                run(move || {
                    blind.inject_always(REALTIME, Fault::DropBeforeApply);
                    blind.inject_always(HISTORY, Fault::DropBeforeApply);
                }),
            ),
            (
                0,
                Act::SubmitWithStop(market(Side::Buy, 0.004, false), 60000.0),
            ),
        ],
    );
    rig.bar();
    rig.mock.clear_faults();
    let resting = rig.mock.orders_with_link_id("t-o2");
    assert_eq!(resting.len(), 1, "{:?}", rig.handle.fault());
    assert_eq!(resting[0].status, "Untriggered");
    assert_eq!(resting[0].qty, 0.004);
    rig.bar();
    assert_eq!(rig.status(2), OrderStatus::Open);
    assert_eq!(rig.handle.fault(), None);
}

#[test]
fn a_record_handed_to_a_broker_that_acted_live_faults() {
    let mut rig = Rig::new(vec![]);
    rig.bar();
    let record = rig.handle.take_record();
    rig.handle.replay(record);
    assert_eq!(rig.fault_kind(), Some(FaultKind::ReplayDiverged));
}

#[test]
fn resolving_an_unknown_order_does_not_read_old_executions_again() {
    let mut rig = Rig::new(vec![(0, Act::Submit(market(Side::Buy, 0.004, false)))]);
    rig.bar();
    rig.bar();
    // A manual stop whose create reaches the venue but whose answer is lost.
    rig.mock.inject(CREATE, Fault::DropAfterApply);
    rig.mock.inject_always(REALTIME, Fault::DropBeforeApply);
    let accepted = rig.accepted();
    rig.session
        .submit(stop(Side::Sell, 0.004, 60000.0), accepted)
        .unwrap();
    rig.mock.clear_faults();
    // A later order fills two minutes on, so the entry's execution leaves the lookback.
    rig.mock.set_skew(120_000);
    rig.session
        .submit(market(Side::Buy, 0.002, false), accepted)
        .unwrap();
    rig.bar();
    assert_eq!(rig.handle.fault(), None);
    rig.bar();
    assert_eq!(rig.handle.fault(), None);
    assert_eq!(rig.status(2), OrderStatus::Open);
    assert_eq!(rig.status(3), OrderStatus::Filled);
    assert_eq!(rig.session.portfolio().position(), 0.006);
}

#[test]
fn a_cancel_past_the_record_of_an_order_at_the_venue_answers_false_and_sends_nothing() {
    let mut rig = Rig::new(vec![
        (0, Act::Submit(market(Side::Buy, 0.004, false))),
        (1, Act::Submit(stop(Side::Sell, 0.004, 60000.0))),
    ]);
    let records = run_recorded(&mut rig, 2);
    let sends = rig.sends();
    let (mut session, handle) = replayed(&rig, &records);
    assert!(!session.cancel(OrderId(2), "manual"));
    handle.go_live().unwrap();
    assert_eq!(rig.sends(), sends, "nothing sent from the record");
    session.on_bar(rig.candle(2, PRICE), &[]).unwrap();
    assert_eq!(handle.fault(), None);
    assert_eq!(session.orders()[1].status, OrderStatus::Open);
    assert_eq!(
        rig.mock.orders_with_link_id("t-o2")[0].status,
        "Untriggered"
    );
    assert_eq!(rig.sends(), sends);
}

#[test]
fn a_cancelled_stop_that_filled_partly_first_reports_its_fill() {
    let mut rig = Rig::new(vec![
        (0, Act::Submit(market(Side::Buy, 0.004, false))),
        (1, Act::Submit(stop(Side::Sell, 0.004, 60000.0))),
    ]);
    rig.bar();
    rig.bar();
    rig.mock.cancel_after_partial_fill("t-o2", 0.002);
    assert!(rig.session.cancel(OrderId(2), "manual"));
    rig.bar();
    assert_eq!(rig.handle.fault(), None);
    assert_eq!(rig.session.portfolio().position(), 0.002);
    assert_eq!(rig.mock.position(SYMBOL), 0.002);
    assert_eq!(rig.status(2), OrderStatus::Filled);
}

#[test]
fn go_live_adopts_an_entry_and_the_stop_it_placed_before_the_restart() {
    let slot: Arc<Mutex<Option<VenueHandle>>> = Arc::new(Mutex::new(None));
    let taken: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let (from, into) = (slot.clone(), taken.clone());
    // The record is taken in the middle of bar 1, before its orders go out, and the live run
    // stops after them.
    let mut rig = Rig::new(vec![
        (
            1,
            run(move || {
                let handle = from.lock().unwrap().clone().unwrap();
                let text = serde_json::to_string(&handle.take_record()).unwrap();
                into.lock().unwrap().push(text);
            }),
        ),
        (
            1,
            Act::SubmitWithStop(market(Side::Buy, 0.004, false), 60000.0),
        ),
    ]);
    *slot.lock().unwrap() = Some(rig.handle.clone());
    let mut records = run_recorded(&mut rig, 1);
    rig.bar();
    records.extend(taken.lock().unwrap().iter().cloned());
    assert_eq!(rig.mock.orders_with_link_id("t-o2").len(), 1);
    let creates = rig.mock.request_count(CREATE);
    let (mut session, handle) = replayed(&rig, &records);
    handle.go_live().unwrap();
    assert_eq!(
        rig.mock.request_count(CREATE),
        creates,
        "nothing created twice"
    );
    assert_eq!(rig.mock.orders_with_link_id("t-o2").len(), 1);
    let resting: Vec<_> = handle.resting_orders().iter().map(|o| o.id).collect();
    assert_eq!(
        resting,
        [OrderId(1)],
        "the entry until its fill is reported; the stop waits for the bar that announces it"
    );
    session.on_bar(rig.candle(2, PRICE), &[]).unwrap();
    assert_eq!(handle.fault(), None);
    assert_eq!(session.orders()[0].status, OrderStatus::Filled);
    assert_eq!(session.orders()[1].status, OrderStatus::Open);
    assert_eq!(handle.resting_orders()[0].id, OrderId(2));
    assert_eq!(rig.mock.request_count(CREATE), creates);
}

/// An entry of 0.004 decided on bar 1 that fills 0.002 at first, so its stop is placed for
/// 0.002, and then fills the rest; the live run stops there. The last record is taken in the
/// middle of bar 1 before the entry goes out, so a replay submits the entry past the record.
fn restart_after_partial_fill() -> (Rig, Vec<String>) {
    let slot: Arc<Mutex<Option<VenueHandle>>> = Arc::new(Mutex::new(None));
    let taken: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let (from, into) = (slot.clone(), taken.clone());
    let mut rig = Rig::new(vec![
        (
            1,
            run(move || {
                let handle = from.lock().unwrap().clone().unwrap();
                let text = serde_json::to_string(&handle.take_record()).unwrap();
                into.lock().unwrap().push(text);
            }),
        ),
        (
            1,
            Act::SubmitWithStop(market(Side::Buy, 0.004, false), 60000.0),
        ),
    ]);
    *slot.lock().unwrap() = Some(rig.handle.clone());
    let mut records = run_recorded(&mut rig, 1);
    rig.mock.cap_next_fill(0.002);
    rig.bar();
    records.extend(taken.lock().unwrap().iter().cloned());
    assert_eq!(rig.mock.orders_with_link_id("t-o2")[0].qty, 0.002);
    rig.mock.fill_rest("t-o1");
    (rig, records)
}

#[test]
fn a_stop_from_before_a_restart_that_cannot_be_read_faults_and_nothing_is_sent() {
    let (rig, records) = restart_after_partial_fill();
    // The entry's lookup passes; the stop's fails once.
    rig.mock.inject(REALTIME, Fault::Hang(Duration::ZERO));
    rig.mock.inject(REALTIME, Fault::Answer(10001));
    let sends = rig.sends();
    let (_session, handle) = replayed(&rig, &records);
    let fault = handle.go_live().expect_err("faulted");
    assert_eq!(fault.kind, FaultKind::UnresolvedOrder, "{fault}");
    assert!(fault.message.contains("t-o2"), "{fault}");
    assert_eq!(rig.sends(), sends, "nothing placed or resized");
    assert_eq!(rig.mock.orders_with_link_id("t-o2")[0].qty, 0.002);
}

#[test]
fn a_stop_from_before_a_restart_is_resized_from_the_venues_quantity() {
    let (rig, records) = restart_after_partial_fill();
    // The entry's lookup passes; the stop's finds it in neither list, so it is placed again and
    // the venue refuses the id it already holds.
    rig.mock.inject(REALTIME, Fault::Hang(Duration::ZERO));
    rig.mock.inject(REALTIME, Fault::Answer(0));
    rig.mock.inject(HISTORY, Fault::Answer(0));
    let creates = rig.mock.request_count(CREATE);
    let (mut session, handle) = replayed(&rig, &records);
    handle.go_live().unwrap();
    assert_eq!(rig.mock.request_count(CREATE), creates + 1);
    let resting = rig.mock.orders_with_link_id("t-o2");
    assert_eq!(resting.len(), 1, "refused as a duplicate");
    assert_eq!(resting[0].qty, 0.004, "the stop covers the whole fill");
    session.on_bar(rig.candle(2, PRICE), &[]).unwrap();
    assert_eq!(handle.fault(), None);
    assert_eq!(session.orders()[1].status, OrderStatus::Open);
    assert_eq!(session.orders()[1].request.qty, 0.004);
    assert_eq!(session.portfolio().position(), 0.004);
}

#[test]
fn a_resting_stop_the_venue_holds_with_another_quantity_or_trigger_faults() {
    for (trigger, qty, shown) in [(None, Some(0.003), "0.003"), (Some(59000.0), None, "59000")] {
        let mut rig = Rig::new(vec![(
            0,
            Act::SubmitWithStop(market(Side::Buy, 0.004, false), 60000.0),
        )]);
        rig.bar();
        rig.bar();
        assert_eq!(rig.status(2), OrderStatus::Open);
        // Changed at the venue by someone other than the broker.
        client_for(&rig.mock)
            .amend_stop(
                &instrument(),
                &ClientOrderId::new("t-o2").unwrap(),
                trigger,
                qty,
            )
            .unwrap();
        rig.bar();
        let fault = rig.handle.fault().expect("faulted");
        assert_eq!(fault.kind, FaultKind::UnknownOpenOrder, "{fault}");
        assert!(
            fault.message.contains("t-o2") && fault.message.contains(shown),
            "{fault}"
        );
    }
}

/// Faults that hide the venue's order lists, so an entry's fill cannot be confirmed, and refuse
/// the create after the next one: the entry goes out and its stop is refused.
fn blind_and_refuse_the_stop(mock: &Arc<MockVenue>) -> Act {
    let mock = mock.clone();
    run(move || {
        mock.inject_always(REALTIME, Fault::DropBeforeApply);
        mock.inject_always(HISTORY, Fault::DropBeforeApply);
        mock.inject(CREATE, Fault::Hang(Duration::ZERO));
        mock.inject(CREATE, Fault::Answer(10001));
    })
}

#[test]
fn a_stop_refused_before_its_entrys_fill_is_confirmed_closes_nothing_and_goes_out_on_the_fill() {
    let mock = venue();
    let blind = blind_and_refuse_the_stop(&mock);
    let mut rig = Rig::with(
        mock,
        vec![
            (0, blind),
            (
                0,
                Act::SubmitWithStop(market(Side::Buy, 0.004, false), 60000.0),
            ),
        ],
    );
    rig.bar();
    rig.mock.clear_faults();
    assert_eq!(rig.mock.position(SYMBOL), 0.004);
    assert!(rig.mock.orders_with_link_id("t-o2").is_empty());
    assert!(
        rig.mock.orders_with_link_id("t-c2").is_empty(),
        "no close for a fill nobody confirmed"
    );
    assert_eq!(rig.handle.fault(), None);
    rig.bar();
    let stop = rig.mock.orders_with_link_id("t-s2");
    assert_eq!(stop.len(), 1, "{:?}", rig.handle.fault());
    assert_eq!(stop[0].status, "Untriggered");
    assert_eq!(stop[0].qty, 0.004);
    assert_eq!(stop[0].side, Side::Sell);
    assert_eq!(stop[0].trigger, Some(60000.0));
    assert!(stop[0].reduce_only);
    assert_eq!(rig.status(2), OrderStatus::Open);
    assert_eq!(rig.handle.fault(), None);
    // It works as the stop: it triggers and the session sees the fill.
    rig.mock.set_price(SYMBOL, 59900.0);
    rig.bar();
    assert_eq!(rig.status(2), OrderStatus::Filled);
    assert_eq!(rig.session.portfolio().position(), 0.0);
    assert_eq!(rig.mock.position(SYMBOL), 0.0);
    assert_eq!(rig.handle.fault(), None);
}

#[test]
fn a_stop_refused_before_its_entrys_fill_is_confirmed_leaves_a_position_already_held_alone() {
    let mock = venue();
    let blind = blind_and_refuse_the_stop(&mock);
    let mut rig = Rig::with(
        mock,
        vec![
            (0, Act::Submit(market(Side::Buy, 0.004, false))),
            (1, blind),
            (
                1,
                Act::SubmitWithStop(market(Side::Buy, 0.002, false), 60000.0),
            ),
        ],
    );
    rig.bar();
    rig.bar();
    rig.mock.clear_faults();
    assert_eq!(rig.mock.position(SYMBOL), 0.006, "nothing reduced");
    assert!(rig.mock.orders_with_link_id("t-c3").is_empty());
    rig.bar();
    assert_eq!(rig.mock.orders_with_link_id("t-s3")[0].qty, 0.002);
    assert_eq!(rig.session.portfolio().position(), 0.006);
    assert_eq!(rig.handle.fault(), None);
}

#[test]
fn an_entry_that_closes_unfilled_owes_nothing_for_a_stop_that_did_not_rest() {
    let mock = venue();
    let blind = blind_and_refuse_the_stop(&mock);
    mock.cap_next_fill(0.0);
    let mut rig = Rig::with(
        mock,
        vec![
            (0, blind),
            (
                0,
                Act::SubmitWithStop(market(Side::Buy, 0.004, false), 60000.0),
            ),
        ],
    );
    rig.bar();
    rig.mock.clear_faults();
    rig.mock.cancel_after_partial_fill("t-o1", 0.0);
    rig.bar();
    assert_eq!(rig.handle.fault(), None);
    assert!(matches!(rig.status(1), OrderStatus::Cancelled(_)));
    assert!(matches!(rig.status(2), OrderStatus::Cancelled(_)));
    assert!(rig.mock.orders_with_link_id("t-s2").is_empty());
    assert_eq!(
        rig.mock.request_count(CREATE),
        2,
        "the entry and the refused stop"
    );
    rig.bar();
    assert_eq!(rig.handle.fault(), None);
}

/// Faults that hide the venue's order lists, so an entry's fill cannot be confirmed and its stop
/// is placed for the quantity sent, and cap the entry's fill at `fill`.
fn blind_with_fill(mock: &Arc<MockVenue>, fill: f64) -> Act {
    let mock = mock.clone();
    run(move || {
        mock.inject_always(REALTIME, Fault::DropBeforeApply);
        mock.inject_always(HISTORY, Fault::DropBeforeApply);
        mock.cap_next_fill(fill);
    })
}

#[test]
fn an_entry_that_closes_unfilled_after_its_stop_was_placed_faults() {
    let mock = venue();
    let blind = blind_with_fill(&mock, 0.0);
    let mut rig = Rig::with(
        mock,
        vec![
            (0, blind),
            (
                0,
                Act::SubmitWithStop(market(Side::Buy, 0.004, false), 60000.0),
            ),
        ],
    );
    rig.bar();
    rig.mock.clear_faults();
    assert_eq!(
        rig.mock.orders_with_link_id("t-o2")[0].status,
        "Untriggered"
    );
    rig.mock.cancel_after_partial_fill("t-o1", 0.0);
    rig.bar();
    let fault = rig.handle.fault().expect("faulted");
    assert_eq!(fault.kind, FaultKind::UnknownOpenOrder, "{fault}");
    assert!(
        fault.message.contains("closed unfilled") && fault.message.contains("t-o2"),
        "{fault}"
    );
}

#[test]
fn a_stop_placed_before_the_fill_is_confirmed_shrinks_to_a_partial_fill() {
    let mock = venue();
    let blind = blind_with_fill(&mock, 0.002);
    let mut rig = Rig::with(
        mock,
        vec![
            (0, blind),
            (
                0,
                Act::SubmitWithStop(market(Side::Buy, 0.004, false), 60000.0),
            ),
        ],
    );
    rig.bar();
    rig.mock.clear_faults();
    assert_eq!(rig.mock.orders_with_link_id("t-o2")[0].qty, 0.004);
    rig.bar();
    assert_eq!(rig.mock.orders_with_link_id("t-o2")[0].qty, 0.002);
    assert_eq!(rig.session.orders()[1].request.qty, 0.002);
    assert_eq!(rig.status(2), OrderStatus::Open);
    assert_eq!(rig.session.portfolio().position(), 0.002);
    rig.bar();
    assert_eq!(rig.handle.fault(), None);
}

/// Faults that let the entry's create through, lose the answer to its stop's create before it
/// reaches the venue, and refuse the cover when `refuse_cover`.
fn lose_the_stop(mock: &Arc<MockVenue>, refuse_cover: bool) -> Act {
    let mock = mock.clone();
    run(move || {
        mock.inject(CREATE, Fault::Hang(Duration::ZERO));
        mock.inject(CREATE, Fault::DropBeforeApply);
        if refuse_cover {
            mock.inject(CREATE, Fault::Answer(10001));
        }
    })
}

#[test]
fn a_confirmed_fill_whose_stop_the_venue_does_not_show_gets_a_cover_and_a_fault_and_no_close() {
    let mock = venue();
    let lose = lose_the_stop(&mock, false);
    let mut rig = Rig::with(
        mock,
        vec![
            (0, lose),
            (
                0,
                Act::SubmitWithStop(market(Side::Buy, 0.004, false), 60000.0),
            ),
        ],
    );
    rig.bar();
    let fault = rig.handle.fault().expect("faulted");
    assert_eq!(fault.kind, FaultKind::ProtectionFailed);
    assert!(
        fault
            .message
            .contains("t-o2 is unknown and the venue does not show it")
            && fault.message.contains("covered by t-s2")
            && fault.message.contains("t-o2 may also rest"),
        "{fault}"
    );
    assert!(rig.mock.orders_with_link_id("t-o2").is_empty());
    let cover = rig.mock.orders_with_link_id("t-s2");
    assert_eq!(cover.len(), 1);
    assert_eq!(cover[0].status, "Untriggered");
    assert_eq!(cover[0].qty, 0.004);
    assert_eq!(cover[0].side, Side::Sell);
    assert_eq!(cover[0].trigger, Some(60000.0));
    assert!(cover[0].reduce_only);
    assert!(rig.mock.orders_with_link_id("t-c2").is_empty(), "no close");
    assert_eq!(rig.mock.position(SYMBOL), 0.004);
}

#[test]
fn a_confirmed_fill_whose_stop_outcome_is_unknown_and_cover_refused_is_closed_and_faults() {
    let mock = venue();
    let lose = lose_the_stop(&mock, true);
    let mut rig = Rig::with(
        mock,
        vec![
            (0, lose),
            (
                0,
                Act::SubmitWithStop(market(Side::Buy, 0.004, false), 60000.0),
            ),
        ],
    );
    rig.bar();
    assert!(rig.mock.orders_with_link_id("t-s2").is_empty());
    assert_eq!(rig.mock.orders_with_link_id("t-c2")[0].status, "Filled");
    assert_eq!(rig.mock.position(SYMBOL), 0.0);
    // A stop the venue refused outright and a close that took do not fault
    // (a_stop_the_venue_will_not_take_is_replaced_by_a_close_at_market); a stop whose outcome is
    // unknown may still rest, so this one does, at once.
    let fault = rig.handle.fault().expect("faulted");
    assert_eq!(fault.kind, FaultKind::ProtectionFailed);
    assert!(
        fault.message.contains("t-o2 is unknown")
            && fault.message.contains("closed at market under t-c2"),
        "{fault}"
    );
}

/// Faults that hide the venue's order lists, so an entry's fill cannot be confirmed, let the
/// entry's create through, and lose the answer to its stop's create after the venue applied it.
fn blind_and_lose_the_stop(mock: &Arc<MockVenue>) -> Act {
    let mock = mock.clone();
    run(move || {
        mock.inject_always(REALTIME, Fault::DropBeforeApply);
        mock.inject_always(HISTORY, Fault::DropBeforeApply);
        mock.inject(CREATE, Fault::Hang(Duration::ZERO));
        mock.inject(CREATE, Fault::DropAfterApply);
    })
}

#[test]
fn a_stop_of_unknown_outcome_before_the_fill_is_confirmed_is_looked_up_not_sent_again() {
    let mock = venue();
    let lose = blind_and_lose_the_stop(&mock);
    let mut rig = Rig::with(
        mock,
        vec![
            (0, lose),
            (
                0,
                Act::SubmitWithStop(market(Side::Buy, 0.004, false), 60000.0),
            ),
        ],
    );
    rig.bar();
    rig.mock.clear_faults();
    assert_eq!(rig.handle.fault(), None);
    assert_eq!(
        rig.mock.orders_with_link_id("t-o2").len(),
        1,
        "it reached the venue"
    );
    rig.bar();
    assert_eq!(rig.handle.fault(), None);
    let resting = rig.mock.orders_with_link_id("t-o2");
    assert_eq!(resting[0].status, "Untriggered");
    assert_eq!(resting[0].qty, 0.004);
    assert!(
        rig.mock.orders_with_link_id("t-s2").is_empty(),
        "nothing sent under s"
    );
    assert_eq!(rig.mock.request_count(CREATE), 2);
    assert_eq!(rig.status(2), OrderStatus::Open);
    // It works as the stop.
    rig.mock.set_price(SYMBOL, 59900.0);
    rig.bar();
    assert_eq!(rig.status(2), OrderStatus::Filled);
    assert_eq!(rig.session.portfolio().position(), 0.0);
    assert_eq!(rig.handle.fault(), None);
}

#[test]
fn a_stop_of_unknown_outcome_for_a_confirmed_fill_is_looked_up_not_sent_again() {
    let mock = venue();
    let faults = mock.clone();
    let lose = run(move || {
        faults.inject(CREATE, Fault::Hang(Duration::ZERO));
        faults.inject(CREATE, Fault::DropAfterApply);
        // The entry's fill is read; then the route's three lookup rounds after the stop's create
        // cannot ask the venue, and the broker's own read after them can.
        faults.inject(REALTIME, Fault::Hang(Duration::ZERO));
        for _ in 0..3 {
            faults.inject(REALTIME, Fault::DropBeforeApply);
        }
    });
    let mut rig = Rig::counted(
        mock,
        vec![
            (0, lose),
            (
                0,
                Act::SubmitWithStop(market(Side::Buy, 0.004, false), 60000.0),
            ),
        ],
    );
    rig.bar();
    assert_eq!(rig.handle.fault(), None);
    let resting = rig.mock.orders_with_link_id("t-o2");
    assert_eq!(resting.len(), 1);
    assert_eq!(resting[0].status, "Untriggered");
    assert_eq!(resting[0].qty, 0.004);
    assert!(
        rig.mock.orders_with_link_id("t-s2").is_empty(),
        "nothing sent under s"
    );
    assert_eq!(rig.mock.request_count(CREATE), 2);
    rig.bar();
    assert_eq!(rig.handle.fault(), None);
    assert_eq!(rig.status(2), OrderStatus::Open);
}

#[test]
fn a_stop_of_unknown_outcome_the_venue_cannot_be_asked_about_sends_nothing_and_faults() {
    let mock = venue();
    let faults = mock.clone();
    let lose = run(move || {
        faults.inject(CREATE, Fault::Hang(Duration::ZERO));
        faults.inject(CREATE, Fault::DropBeforeApply);
        // The entry's fill is read; after that no order can be.
        faults.inject(REALTIME, Fault::Hang(Duration::ZERO));
        faults.inject_always(REALTIME, Fault::DropBeforeApply);
    });
    let mut rig = Rig::with(
        mock,
        vec![
            (0, lose),
            (
                0,
                Act::SubmitWithStop(market(Side::Buy, 0.004, false), 60000.0),
            ),
        ],
    );
    rig.bar();
    let fault = rig.handle.fault().expect("faulted");
    assert_eq!(fault.kind, FaultKind::ProtectionFailed);
    assert!(
        fault.message.contains("could not be asked") && fault.message.contains("nothing was sent"),
        "{fault}"
    );
    assert_eq!(rig.mock.request_count(CREATE), 2, "the entry and the stop");
    assert!(rig.mock.orders_with_link_id("t-s2").is_empty());
    assert!(rig.mock.orders_with_link_id("t-c2").is_empty());
    assert_eq!(rig.mock.position(SYMBOL), 0.004);
}

#[test]
fn a_stop_not_sent_before_the_fill_is_confirmed_goes_out_under_its_own_id_on_the_fill() {
    let mock = venue();
    let faults = mock.clone();
    let refuse = run(move || {
        faults.inject_always(REALTIME, Fault::DropBeforeApply);
        faults.inject_always(HISTORY, Fault::DropBeforeApply);
        faults.inject(CREATE, Fault::Hang(Duration::ZERO));
        // Refused before processing, again after the clock resync: not sent, the id is free.
        faults.inject(CREATE, Fault::TimestampReject);
        faults.inject(CREATE, Fault::TimestampReject);
    });
    let mut rig = Rig::with(
        mock,
        vec![
            (0, refuse),
            (
                0,
                Act::SubmitWithStop(market(Side::Buy, 0.004, false), 60000.0),
            ),
        ],
    );
    rig.bar();
    rig.mock.clear_faults();
    assert_eq!(rig.mock.position(SYMBOL), 0.004);
    assert!(rig.mock.orders_with_link_id("t-o2").is_empty());
    assert_eq!(rig.handle.fault(), None);
    rig.bar();
    assert_eq!(rig.handle.fault(), None);
    let stop = rig.mock.orders_with_link_id("t-o2");
    assert_eq!(stop.len(), 1, "sent again under the same id");
    assert_eq!(stop[0].status, "Untriggered");
    assert_eq!(stop[0].qty, 0.004);
    assert!(rig.mock.orders_with_link_id("t-s2").is_empty());
    assert_eq!(rig.status(2), OrderStatus::Open);
}

/// A live run whose entry with a stop is applied at the venue but whose answer is lost, so its
/// outcome is unknown when the run stops after bar 0; then a restarted session whose going live
/// cannot read the venue. Returns the live rig and the restarted session.
fn restart_unsettled(fill: Option<f64>) -> (Rig, Session, VenueHandle) {
    let mock = venue();
    let lose = lose_next_create(&mock, Fault::DropAfterApply);
    if let Some(qty) = fill {
        mock.cap_next_fill(qty);
    }
    let mut rig = Rig::with(
        mock,
        vec![
            (0, lose),
            (
                0,
                Act::SubmitWithStop(market(Side::Buy, 0.004, false), 60000.0),
            ),
        ],
    );
    let records = run_recorded(&mut rig, 1);
    rig.mock.clear_faults();
    let (session, handle) = replayed(&rig, &records);
    rig.mock.inject_always(REALTIME, Fault::DropBeforeApply);
    handle.go_live().unwrap();
    rig.mock.clear_faults();
    (rig, session, handle)
}

#[test]
fn after_a_restart_whose_lookup_failed_an_entry_the_venue_holds_unfilled_gets_no_stop() {
    let (rig, mut session, handle) = restart_unsettled(Some(0.0));
    assert_eq!(rig.mock.orders_with_link_id("t-o1")[0].status, "New");
    let sends = rig.sends();
    session.on_bar(rig.candle(1, PRICE), &[]).unwrap();
    assert_eq!(handle.fault(), None);
    assert_eq!(
        rig.sends(),
        sends,
        "no stop for a fill the venue does not list"
    );
    assert!(rig.mock.orders_with_link_id("t-o2").is_empty());
    assert_eq!(session.orders()[0].status, OrderStatus::Open);
}

#[test]
fn after_a_restart_whose_lookup_failed_a_refused_stop_closes_nothing_and_faults() {
    let (rig, mut session, handle) = restart_unsettled(None);
    assert_eq!(rig.mock.position(SYMBOL), 0.004);
    let creates = rig.mock.request_count(CREATE);
    rig.mock.inject(CREATE, Fault::Answer(10001));
    session.on_bar(rig.candle(1, PRICE), &[]).unwrap();
    let fault = handle.fault().expect("faulted");
    assert_eq!(fault.kind, FaultKind::ProtectionFailed);
    assert!(fault.message.contains("no close at market"), "{fault}");
    assert_eq!(rig.mock.request_count(CREATE), creates + 1, "only the stop");
    assert!(rig.mock.orders_with_link_id("t-c2").is_empty());
    assert_eq!(rig.mock.position(SYMBOL), 0.004);
}

#[test]
fn a_replay_of_a_stop_refused_before_the_fill_and_then_placed_as_its_cover_sends_nothing() {
    let mock = venue();
    let blind = blind_and_refuse_the_stop(&mock);
    let mut rig = Rig::with(
        mock,
        vec![
            (0, blind),
            (
                0,
                Act::SubmitWithStop(market(Side::Buy, 0.004, false), 60000.0),
            ),
        ],
    );
    let mut records = run_recorded(&mut rig, 1);
    rig.mock.clear_faults();
    records.extend(run_recorded(&mut rig, 1));
    assert_eq!(rig.handle.fault(), None);
    assert!(records[0].contains("\"Unplaced\"") && records[1].contains("\"Cover\""));
    // Saved before orders carried a `carried` flag: it loads as false.
    let records: Vec<String> = records
        .iter()
        .map(|text| {
            let mut value: serde_json::Value = serde_json::from_str(text).unwrap();
            for order in value["book"]["orders"]
                .as_object_mut()
                .unwrap()
                .values_mut()
            {
                order.as_object_mut().unwrap().remove("carried").unwrap();
            }
            value.to_string()
        })
        .collect();
    let sends = rig.sends();
    let (mut session, handle) = replayed(&rig, &records);
    handle.go_live().unwrap();
    assert_eq!(rig.sends(), sends, "going live sends and cancels nothing");
    assert_eq!(session.orders(), rig.session.orders());
    assert_eq!(session.open_orders(), rig.session.open_orders());
    // It carries on live, with the stop resting under its cover id.
    rig.mock.set_price(SYMBOL, 59900.0);
    session.on_bar(rig.candle(2, PRICE), &[]).unwrap();
    assert_eq!(handle.fault(), None);
    assert_eq!(session.orders()[1].status, OrderStatus::Filled);
    assert_eq!(session.portfolio().position(), 0.0);
}

#[test]
fn a_stop_id_the_venue_already_holds_as_another_stop_faults_and_nothing_else_is_sent() {
    for (side, trigger, shown) in [(Side::Sell, 59000.0, "59000"), (Side::Buy, 66000.0, "Buy")] {
        let mock = venue();
        let other = mock.clone();
        // Another session reusing the prefix places the id first.
        let squat = run(move || {
            client_for(&other)
                .place(
                    &instrument(),
                    &stop(side, 0.004, trigger),
                    &ClientOrderId::new("t-o2").unwrap(),
                    PRICE,
                )
                .unwrap();
        });
        let mut rig = Rig::with(
            mock,
            vec![
                (0, squat),
                (
                    0,
                    Act::SubmitWithStop(market(Side::Buy, 0.004, false), 60000.0),
                ),
            ],
        );
        rig.bar();
        let fault = rig.handle.fault().expect("faulted");
        assert_eq!(fault.kind, FaultKind::UnknownOpenOrder, "{fault}");
        assert!(
            fault.message.contains("t-o2") && fault.message.contains(shown),
            "{fault}"
        );
        assert_eq!(
            rig.mock.request_count(CREATE),
            3,
            "the other stop, the entry and the stop refused as a duplicate"
        );
        assert!(rig.mock.orders_with_link_id("t-s2").is_empty());
        assert!(rig.mock.orders_with_link_id("t-c2").is_empty());
    }
}

#[test]
fn a_cover_id_the_venue_already_holds_as_that_cover_counts_as_placed() {
    let mock = venue();
    let other = mock.clone();
    let act = run(move || {
        client_for(&other)
            .place(
                &instrument(),
                &stop(Side::Sell, 0.004, 60000.0),
                &ClientOrderId::new("t-s2").unwrap(),
                PRICE,
            )
            .unwrap();
        other.inject(CREATE, Fault::Hang(Duration::ZERO));
        other.inject(CREATE, Fault::DropBeforeApply);
    });
    let mut rig = Rig::with(
        mock,
        vec![
            (0, act),
            (
                0,
                Act::SubmitWithStop(market(Side::Buy, 0.004, false), 60000.0),
            ),
        ],
    );
    rig.bar();
    let fault = rig.handle.fault().expect("faulted");
    assert!(fault.message.contains("covered by t-s2"), "{fault}");
    assert_eq!(rig.mock.orders_with_link_id("t-s2").len(), 1);
    assert!(rig.mock.orders_with_link_id("t-c2").is_empty());
}

#[test]
fn after_a_restart_an_order_the_venue_does_not_hold_is_refused_not_sent() {
    let mut rig = Rig::new(vec![]);
    let records = run_recorded(&mut rig, 1);
    let creates = rig.mock.request_count(CREATE);
    let (mut session, handle) = replayed(&rig, &records);
    // Past the record: an order the stopped run never sent.
    session
        .submit(market(Side::Buy, 0.004, false), rig.accepted())
        .unwrap();
    handle.go_live().unwrap();
    assert_eq!(rig.mock.request_count(CREATE), creates);
    session.on_bar(rig.candle(1, PRICE), &[]).unwrap();
    assert_eq!(handle.fault(), None);
    match &session.orders()[0].status {
        OrderStatus::Cancelled(r) => assert!(r.contains("restarted"), "{r}"),
        other => panic!("{other:?}"),
    }
    assert_eq!(rig.mock.request_count(CREATE), creates);
    assert_eq!(rig.mock.position(SYMBOL), 0.0);
}

#[test]
fn a_record_holding_an_order_of_unknown_outcome_as_not_open_is_refused() {
    let mock = venue();
    let lose = lose_next_create(&mock, Fault::DropBeforeApply);
    let mut rig = Rig::with(
        mock,
        vec![(0, lose), (0, Act::Submit(market(Side::Buy, 0.004, false)))],
    );
    let records = run_recorded(&mut rig, 1);
    rig.mock.clear_faults();
    let mut value: serde_json::Value = serde_json::from_str(&records[0]).unwrap();
    let order = &mut value["book"]["orders"]["1"];
    order["at"] = serde_json::json!("Intent");
    order["open"] = serde_json::json!(false);
    let creates = rig.mock.request_count(CREATE);
    let (_session, handle) = replayed(&rig, &[value.to_string()]);
    let fault = handle.fault().expect("refused");
    assert_eq!(fault.kind, FaultKind::ReplayDiverged);
    assert!(fault.message.contains("outcome is not known"), "{fault}");
    assert!(handle.go_live().is_err());
    assert_eq!(rig.mock.request_count(CREATE), creates);
}
