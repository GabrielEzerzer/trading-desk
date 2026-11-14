//! A smoke test of the order route against the venue's demo-trading service. It sends real orders
//! to the demo account, so it runs only with `--confirm-send-orders`:
//!
//! ```text
//! cargo run -p td-orders --example demo-smoke -- --confirm-send-orders --symbol <SYMBOL>
//! ```
//!
//! The keys come from `BYBIT_DEMO_API_KEY` and `BYBIT_DEMO_API_SECRET`. The symbol must be a USDT
//! perpetual that is trading. Its tick size, quantity step, minimum quantity and minimum notional
//! come from the venue's public instruments info, and its last and mark prices from its public
//! ticker, both read from the demo host. The order quantity is the smallest the venue allows: the
//! minimum quantity, raised only as far as the minimum notional requires at the last price. Nothing
//! is sent when the last and mark prices are more than 2% apart, or when the order is worth more
//! than [`MAX_NOTIONAL`] at the higher of the two.
//!
//! Steps: check the symbol is flat with no open orders; market buy and confirm the fill from the
//! executions; rest a reduce-only stop far below the price, read it back, amend it and cancel it;
//! send the first order's client id again from a new client and expect the venue to refuse it (an
//! observation of the venue; the route relies on it nowhere); close with a reduce-only market
//! sell; reconcile. Each step prints one line, and the last line is PASS or FAIL. Nothing printed
//! contains a key, a signature or a response body. A failure once the buy may be on the account
//! says that a position, and possibly a stop, may be left open.

use std::process::ExitCode;
use std::time::Duration;

use td_core::{
    FeeSchedule, Instrument, InstrumentId, InstrumentKind, OrderRequest, OrderType, Rounding, Side,
    TimeInForce, Venue,
};
use td_orders::{
    CancelOutcome, Client, ClientOrderId, DemoKeys, Endpoint, Error, ExecKind, SETTLE_COIN,
    venue_text,
};

/// The most one order may be worth, in the settle coin. Checked before any order is sent.
const MAX_NOTIONAL: f64 = 250.0;
/// How far apart, as a fraction of the lower, the last and mark prices may be.
const MAX_PRICE_GAP: f64 = 0.02;

const USAGE: &str = "usage: demo-smoke --confirm-send-orders --symbol <SYMBOL>";

fn parse_args() -> Result<String, String> {
    let mut confirmed = false;
    let mut symbol = None;
    let mut raw = std::env::args().skip(1);
    while let Some(arg) = raw.next() {
        match arg.as_str() {
            "--confirm-send-orders" => confirmed = true,
            "--symbol" if symbol.is_none() => {
                symbol = Some(
                    raw.next()
                        .ok_or(format!("--symbol needs a value\n{USAGE}"))?,
                );
            }
            other => return Err(format!("unexpected argument {other:?}\n{USAGE}")),
        }
    }
    if !confirmed {
        return Err(format!(
            "refusing to send orders without --confirm-send-orders\n{USAGE}"
        ));
    }
    let symbol = symbol.ok_or(format!("missing --symbol\n{USAGE}"))?;
    if symbol.is_empty() || !symbol.bytes().all(|b| b.is_ascii_alphanumeric()) {
        return Err("--symbol must be letters and digits only".into());
    }
    Ok(symbol)
}

fn main() -> ExitCode {
    let symbol = match parse_args() {
        Ok(s) => s,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::from(2);
        }
    };
    let mut bought = false;
    match run(&symbol, &mut bought) {
        Ok(()) => {
            println!("PASS");
            ExitCode::SUCCESS
        }
        Err(e) => {
            println!("FAIL: {}", failure(&e, &symbol, bought));
            ExitCode::FAILURE
        }
    }
}

/// A failure line's text: once the buy may be on the account, it says what may be left open.
fn failure(err: &str, symbol: &str, bought: bool) -> String {
    if bought {
        format!(
            "{err}. A position on {symbol}, and possibly a reduce-only stop, may be open on the \
             demo account; check them and close them by hand"
        )
    } else {
        err.to_string()
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

/// The smallest order the venue allows, and what it is worth at the higher of the last and mark
/// prices: the minimum quantity, raised to the next step on the grid only when the minimum notional
/// at the last price needs more. The small slack keeps a quantity already on the grid from being
/// pushed a step up by float noise in the division. A wrong last price would both set the
/// quantity and hide it from the ceiling, so prices more than [`MAX_PRICE_GAP`] apart are refused
/// and the ceiling is checked at the higher one.
fn order_size(instrument: &Instrument, last: f64, mark: f64) -> Result<(f64, f64), String> {
    let gap_ok = last > 0.0 && mark > 0.0 && (last - mark).abs() <= MAX_PRICE_GAP * last.min(mark);
    if !gap_ok {
        return Err(format!(
            "the last price {last} and the mark price {mark} are more than {}% apart; nothing \
             was sent",
            MAX_PRICE_GAP * 100.0
        ));
    }
    let needed = instrument.min_qty.max(instrument.min_notional / last);
    let qty =
        instrument.round_qty((needed / instrument.lot_size - 1e-9).ceil() * instrument.lot_size);
    let high = last.max(mark);
    let notional = qty * high;
    // Written so that a NaN quantity or price fails the check.
    if !(notional.is_finite() && notional <= MAX_NOTIONAL) {
        return Err(format!(
            "the smallest order, {qty} at {high}, is worth {notional:.2} {SETTLE_COIN}, which is not \
             within the {MAX_NOTIONAL} ceiling; nothing was sent"
        ));
    }
    Ok((qty, notional))
}

/// Runs the steps. `bought` is set once the market buy may be on the account.
fn run(symbol: &str, bought: &mut bool) -> Result<(), String> {
    let client = Client::new(Endpoint::Demo, DemoKeys::from_env().map_err(e("keys"))?)
        .map_err(e("connect"))?;
    let start = client.server_now();

    let info = client
        .instrument_info(symbol)
        .map_err(e("instrument info"))?;
    check(
        info.contract_type == "LinearPerpetual"
            && info.status == "Trading"
            && info.settle_coin == SETTLE_COIN,
        format!(
            "{symbol} is a {:?} {:?} contract settled in {:?}, not a trading {SETTLE_COIN} \
             perpetual",
            venue_text(&info.status),
            venue_text(&info.contract_type),
            venue_text(&info.settle_coin)
        ),
    )?;
    let ticker = client.ticker(symbol).map_err(e("ticker"))?;
    let (price, mark) = (ticker.last_price, ticker.mark_price);
    let instrument = Instrument {
        id: InstrumentId {
            venue: Venue::Bybit,
            symbol: symbol.into(),
            kind: InstrumentKind::Perpetual,
        },
        base: info.base_coin,
        quote: info.quote_coin,
        tick_size: info.tick_size,
        lot_size: info.qty_step,
        min_qty: info.min_order_qty,
        min_notional: info.min_notional,
        // Fees are not used to send orders.
        fees: FeeSchedule {
            maker: 0.0,
            taker: 0.0,
            per_unit: 0.0,
        },
    };
    let (qty, notional) = order_size(&instrument, price, mark)?;
    let run_id = std::process::id();
    let id =
        |tag: &str| ClientOrderId::new(format!("smoke-{run_id}-{tag}")).map_err(e("client id"));
    step(
        "connect",
        format!(
            "clock synced, order size {qty} at last price {price} (mark {mark}), worth at most \
             {notional:.2} {SETTLE_COIN}"
        ),
    );

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
    let buy = client.place(&instrument, &market(Side::Buy, false), &buy_id, price);
    *bought = matches!(buy, Ok(_) | Err(Error::OutcomeUnknown { .. }));
    let buy = buy.map_err(e("market buy"))?;
    let mut fills = Vec::new();
    for _ in 0..10 {
        fills = client
            .executions_since(&instrument, start - 1000)
            .map_err(e("executions"))?
            .into_iter()
            .filter(|x| x.kind == ExecKind::Trade && x.client_id == buy_id.as_str())
            .collect();
        if (fills.iter().map(|x| x.qty).sum::<f64>() - qty).abs() < instrument.lot_size / 2.0 {
            break;
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    let filled: f64 = fills.iter().map(|x| x.qty).sum();
    check(
        (filled - qty).abs() < instrument.lot_size / 2.0,
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
        (position.size - qty).abs() < instrument.lot_size / 2.0,
        format!("the position is {} after the buy", position.size),
    )?;
    let last = if position.mark_price > 0.0 {
        position.mark_price
    } else {
        price
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

    // A new client has no record of the id, so only the venue can refuse it. This observes how the
    // venue treats a closed order's id; the route never sends an id twice and does not depend on it.
    let second = Client::new(Endpoint::Demo, DemoKeys::from_env().map_err(e("keys"))?)
        .map_err(e("second client"))?;
    match second.place(&instrument, &market(Side::Buy, false), &buy_id, last) {
        Err(Error::DuplicateClientId { .. }) => step("repeat id", "the venue refused the used id"),
        Ok(p) => {
            return Err(format!(
                "the venue accepted a used client id as order {:?}; close it by hand",
                venue_text(&p.order_id)
            ));
        }
        Err(other) => {
            return Err(format!(
                "repeat id: expected a duplicate-id refusal, got {other}"
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

#[cfg(test)]
mod tests {
    use super::*;

    fn instrument(lot_size: f64, min_qty: f64, min_notional: f64) -> Instrument {
        Instrument {
            id: InstrumentId {
                venue: Venue::Bybit,
                symbol: "BTCUSDT".into(),
                kind: InstrumentKind::Perpetual,
            },
            base: "BTC".into(),
            quote: "USDT".into(),
            tick_size: 0.1,
            lot_size,
            min_qty,
            min_notional,
            fees: FeeSchedule {
                maker: 0.0,
                taker: 0.0,
                per_unit: 0.0,
            },
        }
    }

    #[test]
    fn a_last_price_far_from_the_mark_is_refused() {
        // A last price of 1 would size the order at 5 and value it at 5, under the ceiling.
        let i = instrument(0.001, 0.001, 5.0);
        let err = order_size(&i, 1.0, 65000.0).unwrap_err();
        assert!(err.contains("apart"), "{err}");
        assert!(order_size(&i, 65000.0, 1.0).is_err());
        assert!(order_size(&i, 65000.0, 66400.0).is_err(), "2.15% apart");
        assert!(order_size(&i, 65000.0, 66200.0).is_ok(), "1.85% apart");
    }

    #[test]
    fn the_ceiling_is_checked_at_the_higher_price() {
        // 0.0051 at the last price 49000 is worth 249.90, under the ceiling; at the mark price
        // 49900, 1.8% higher, it is worth 254.49, over it.
        let i = instrument(0.0001, 0.001, 249.0);
        let err = order_size(&i, 49000.0, 49900.0).unwrap_err();
        assert!(err.contains("ceiling"), "{err}");
        let (qty, notional) = order_size(&i, 49000.0, 49000.0).unwrap();
        assert_eq!(qty, 0.0051);
        assert!((notional - 249.9).abs() < 1e-9, "{notional}");
    }

    #[test]
    fn the_quantity_is_the_minimum_raised_only_as_far_as_the_notional_needs() {
        let (qty, notional) = order_size(&instrument(0.001, 0.001, 5.0), 65000.0, 65010.0).unwrap();
        assert_eq!(qty, 0.001);
        assert!((notional - 65.01).abs() < 1e-9, "{notional}");
        // 100 at 65000 needs 0.00154, so the next step up; 130 needs exactly 0.002, which stays.
        for min_notional in [100.0, 130.0] {
            let i = instrument(0.001, 0.001, min_notional);
            assert_eq!(order_size(&i, 65000.0, 65000.0).unwrap().0, 0.002);
        }
    }

    #[test]
    fn a_size_that_is_not_a_number_is_refused() {
        // A quantity step of 1e-16 is finer than the grid rounding handles, and the quantity
        // comes out as NaN; a ceiling check written as `notional > MAX_NOTIONAL` lets it through.
        let i = instrument(1e-16, 0.001, 5.0);
        let err = order_size(&i, 65000.0, 65000.0).unwrap_err();
        assert!(err.contains("ceiling"), "{err}");
    }

    #[test]
    fn a_failure_after_the_buy_says_what_may_be_left_open() {
        assert_eq!(failure("ticker: x", "BTCUSDT", false), "ticker: x");
        let shown = failure("stop: x", "BTCUSDT", true);
        assert!(
            shown.starts_with("stop: x. A position on BTCUSDT"),
            "{shown}"
        );
        assert!(shown.contains("reduce-only stop"), "{shown}");
    }
}
