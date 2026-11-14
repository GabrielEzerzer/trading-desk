//! A smoke test of the order route against the venue's demo-trading service. It sends real orders
//! to the demo account, so it runs only with `--confirm-send-orders`:
//!
//! ```text
//! cargo run -p td-orders --example demo-smoke -- --confirm-send-orders \
//!     --symbol BTCUSDT --price 65000 --tick 0.1 --lot 0.001 --min-qty 0.001 --min-notional 5
//! ```
//!
//! The keys come from `BYBIT_DEMO_API_KEY` and `BYBIT_DEMO_API_SECRET`. `--price` is roughly the
//! current price, used for the notional check and to place the stop far below it; the filters are
//! the symbol's `tickSize`, `qtyStep`, `minOrderQty` and `minNotionalValue` from the venue's
//! instruments-info page. The order size is the smallest the filters allow.
//!
//! Steps: check the symbol is flat with no open orders; market buy and confirm the fill from the
//! executions; rest a reduce-only stop far below the price, read it back, amend it and cancel it;
//! resend the first order's client id and expect a refusal; close with a reduce-only market sell;
//! reconcile. Each step prints one line, and the last line is PASS or FAIL. Nothing printed
//! contains a key, a signature or a response body.

use std::process::ExitCode;
use std::time::Duration;

use td_core::{
    FeeSchedule, Instrument, InstrumentId, InstrumentKind, OrderRequest, OrderType, Rounding, Side,
    TimeInForce, Venue,
};
use td_orders::{CancelOutcome, Client, ClientOrderId, DemoKeys, Endpoint, Error, ExecKind};

const USAGE: &str = "usage: demo-smoke --confirm-send-orders --symbol <SYMBOL> --price <PRICE> \
--tick <TICK> --lot <LOT> --min-qty <MIN_QTY> --min-notional <MIN_NOTIONAL>";

struct Args {
    symbol: String,
    price: f64,
    tick: f64,
    lot: f64,
    min_qty: f64,
    min_notional: f64,
}

fn parse_args() -> Result<Args, String> {
    let raw: Vec<String> = std::env::args().skip(1).collect();
    if !raw.iter().any(|a| a == "--confirm-send-orders") {
        return Err(format!(
            "refusing to send orders without --confirm-send-orders\n{USAGE}"
        ));
    }
    let value = |name: &str| -> Result<String, String> {
        let at = raw
            .iter()
            .position(|a| a == name)
            .ok_or(format!("missing {name}\n{USAGE}"))?;
        raw.get(at + 1)
            .cloned()
            .ok_or(format!("{name} needs a value\n{USAGE}"))
    };
    let number = |name: &str| -> Result<f64, String> {
        let v: f64 = value(name)?
            .parse()
            .map_err(|_| format!("{name} is not a number"))?;
        if v.is_finite() && v >= 0.0 {
            Ok(v)
        } else {
            Err(format!("{name} must be a finite number, zero or more"))
        }
    };
    let args = Args {
        symbol: value("--symbol")?,
        price: number("--price")?,
        tick: number("--tick")?,
        lot: number("--lot")?,
        min_qty: number("--min-qty")?,
        min_notional: number("--min-notional")?,
    };
    if args.price == 0.0 || args.tick == 0.0 || args.lot == 0.0 {
        return Err("--price, --tick and --lot must be above zero".into());
    }
    Ok(args)
}

fn main() -> ExitCode {
    let args = match parse_args() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::from(2);
        }
    };
    match run(&args) {
        Ok(()) => {
            println!("PASS");
            ExitCode::SUCCESS
        }
        Err(e) => {
            println!("FAIL: {e}");
            ExitCode::FAILURE
        }
    }
}

fn step(name: &str, detail: impl std::fmt::Display) {
    println!("ok   {name}: {detail}");
}

fn check(ok: bool, what: impl Into<String>) -> Result<(), String> {
    if ok { Ok(()) } else { Err(what.into()) }
}

/// Labels an error with the step it came from.
fn e(what: &'static str) -> impl Fn(Error) -> String {
    move |err| format!("{what}: {err}")
}

fn run(args: &Args) -> Result<(), String> {
    let instrument = Instrument {
        id: InstrumentId {
            venue: Venue::Bybit,
            symbol: args.symbol.clone(),
            kind: InstrumentKind::Perpetual,
        },
        base: String::new(),
        quote: "USDT".into(),
        tick_size: args.tick,
        lot_size: args.lot,
        min_qty: args.min_qty,
        min_notional: args.min_notional,
        // Fees are not used to send orders.
        fees: FeeSchedule {
            maker: 0.0,
            taker: 0.0,
            per_unit: 0.0,
        },
    };
    // The smallest quantity that meets the minimum quantity and, with a 10% margin for the price
    // moving, the minimum notional.
    let by_notional = (args.min_notional * 1.1 / args.price / args.lot).ceil() * args.lot;
    let qty = instrument.round_qty(args.min_qty.max(by_notional).max(args.lot));
    let run_id = std::process::id();
    let id =
        |tag: &str| ClientOrderId::new(format!("smoke-{run_id}-{tag}")).map_err(e("client id"));

    let client = Client::new(Endpoint::Demo, DemoKeys::from_env().map_err(e("keys"))?)
        .map_err(e("connect"))?;
    let start = client.server_now();
    step("connect", format!("clock synced, order size {qty}"));

    let position = client.position(&instrument).map_err(e("position"))?;
    check(
        position.size == 0.0,
        format!("the symbol is not flat: {}", position.size),
    )?;
    let open = client.open_orders(&instrument).map_err(e("open orders"))?;
    check(
        open.is_empty(),
        format!("{} open orders on the symbol", open.len()),
    )?;
    step("flat", "no position and no open orders");

    let market = |side, reduce_only| OrderRequest {
        instrument: instrument.id.clone(),
        side,
        qty,
        order_type: OrderType::Market,
        tif: TimeInForce::Ioc,
        reduce_only,
        client_tag: None,
    };
    let buy_id = id("buy")?;
    let buy = client
        .place(&instrument, &market(Side::Buy, false), &buy_id, args.price)
        .map_err(e("market buy"))?;
    let mut fills = Vec::new();
    for _ in 0..10 {
        fills = client
            .executions_since(&instrument, start - 1000)
            .map_err(e("executions"))?
            .into_iter()
            .filter(|x| x.kind == ExecKind::Trade && x.client_id == buy_id.as_str())
            .collect();
        if (fills.iter().map(|x| x.qty).sum::<f64>() - qty).abs() < args.lot / 2.0 {
            break;
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    let filled: f64 = fills.iter().map(|x| x.qty).sum();
    check(
        (filled - qty).abs() < args.lot / 2.0,
        format!("the buy filled {filled} of {qty}"),
    )?;
    check(
        fills.iter().all(|x| x.order_id == buy.order_id),
        "a fill names another order",
    )?;
    step(
        "market buy",
        format!("filled {filled} in {} executions", fills.len()),
    );

    let position = client.position(&instrument).map_err(e("position"))?;
    check(
        (position.size - qty).abs() < args.lot / 2.0,
        format!("the position is {} after the buy", position.size),
    )?;
    let last = if position.mark_price > 0.0 {
        position.mark_price
    } else {
        args.price
    };

    let stop_id = id("stop")?;
    let trigger = instrument.round_price(last * 0.5, Rounding::Down);
    let stop = OrderRequest {
        order_type: OrderType::Stop { trigger },
        tif: TimeInForce::Gtc,
        ..market(Side::Sell, true)
    };
    client
        .place(&instrument, &stop, &stop_id, last)
        .map_err(e("stop"))?;
    let resting = client
        .order_by_client_id(&instrument, &stop_id)
        .map_err(e("read stop"))?
        .ok_or("the stop is not listed")?;
    check(
        resting.status.is_open() && resting.trigger_price == Some(trigger) && resting.reduce_only,
        format!(
            "the stop reads back as {:?} at {:?}",
            resting.status, resting.trigger_price
        ),
    )?;
    step("stop", format!("resting at {trigger}"));

    let moved = instrument.round_price(last * 0.4, Rounding::Down);
    client
        .amend_stop(&instrument, &stop_id, Some(moved), None)
        .map_err(e("amend"))?;
    let amended = client
        .order_by_client_id(&instrument, &stop_id)
        .map_err(e("read amended stop"))?
        .ok_or("the amended stop is not listed")?;
    check(
        amended.trigger_price == Some(moved),
        format!("the amended trigger reads {:?}", amended.trigger_price),
    )?;
    step("amend", format!("trigger moved to {moved}"));

    let outcome = client.cancel(&instrument, &stop_id).map_err(e("cancel"))?;
    check(
        outcome == CancelOutcome::Cancelled,
        format!("the cancel answered {outcome:?}"),
    )?;
    let open = client.open_orders(&instrument).map_err(e("open orders"))?;
    check(open.is_empty(), "an order is still open after the cancel")?;
    step("cancel", "no open orders");

    // A new client has no record of the id, so the venue is the one that must refuse it.
    let second = Client::new(Endpoint::Demo, DemoKeys::from_env().map_err(e("keys"))?)
        .map_err(e("second client"))?;
    match second.place(&instrument, &market(Side::Buy, false), &buy_id, last) {
        Err(Error::DuplicateClientId { .. }) => step("resend", "the venue refused the used id"),
        Ok(p) => {
            return Err(format!(
                "the venue accepted a used client id as order {}; close it by hand",
                p.order_id
            ));
        }
        Err(other) => {
            return Err(format!(
                "resend: expected a duplicate-id refusal, got {other}"
            ));
        }
    }

    let close_id = id("close")?;
    client
        .place(&instrument, &market(Side::Sell, true), &close_id, last)
        .map_err(e("close"))?;
    step("close", "reduce-only market sell sent");

    let mut position = client.position(&instrument).map_err(e("position"))?;
    for _ in 0..10 {
        if position.size == 0.0 {
            break;
        }
        std::thread::sleep(Duration::from_millis(500));
        position = client.position(&instrument).map_err(e("position"))?;
    }
    check(
        position.size == 0.0,
        format!("the position is {} after closing", position.size),
    )?;
    let open = client.open_orders(&instrument).map_err(e("open orders"))?;
    check(
        open.is_empty(),
        format!("{} open orders at the end", open.len()),
    )?;
    let execs = client
        .executions_since(&instrument, start - 1000)
        .map_err(e("executions"))?;
    let seen = |id: &ClientOrderId| execs.iter().any(|x| x.client_id == id.as_str());
    check(
        seen(&buy_id) && seen(&close_id),
        "the executions miss the buy or the close",
    )?;
    let equity = client.equity().map_err(e("equity"))?;
    step(
        "reconcile",
        format!(
            "flat, no open orders, {} executions, {} equity {:.2}",
            execs.len(),
            equity.coin,
            equity.equity
        ),
    );
    Ok(())
}
