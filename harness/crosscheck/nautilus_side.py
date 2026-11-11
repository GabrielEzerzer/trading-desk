"""The NautilusTrader side of the cross-check: the same case on the same exported bars.

Configuration, per issue #29:

- the exported bars, as EXTERNAL LAST bars stamped at their close, so each bar is matched
  open, high, low, close (Nautilus's default ordering, ``bar_adaptive_high_low_ordering=False``)
  before the strategy sees it;
- ``FillModel(prob_fill_on_limit=1.0, prob_slippage=0.0)``, and the case's maker and taker fees on
  the instrument, charged by Nautilus's default maker/taker fee model;
- a cash account for spot and a margin account at 10x for a perpetual, which our strategies never
  come near, so the leverage never binds;
- funding charged into the Nautilus account (``SimulatedExchange.adjust_account``, the hook its own
  FX rollover module uses) at the same stamps and on the same position as our engine charges it:
  a stamp at or before a bar's open on the position held going into the bar, at the open, and a
  stamp inside the bar on the position after the bar's fills, at the close. Nautilus 1.231 has no
  funding settlement of its own for perpetuals in a backtest.

The round trips come from Nautilus's own positions (the positions report's rows: the open and closed
positions in the cache plus the snapshots a NETTING account takes of each closed position before it
reuses its ID); see `round_trips`.

The strategies are written again here from their Rust source, decision for decision, including
the rounding and the order checks our engine applies when an order is submitted, against a mirror
of our engine's open orders (see `CrosscheckStrategy`). Market, limit, stop and stop-limit orders
map to Nautilus's own types, all good till cancelled; an attached protective stop is a reduce-only
stop-market order sent when its entry fills.
"""

from __future__ import annotations

import csv
import json
import math
from decimal import ROUND_HALF_UP, Decimal
from pathlib import Path

from nautilus_trader.backtest.engine import BacktestEngine, BacktestEngineConfig
from nautilus_trader.backtest.models import FillModel
from nautilus_trader.backtest.modules import SimulationModule, SimulationModuleConfig
from nautilus_trader.config import LoggingConfig, StrategyConfig
from nautilus_trader.model.currencies import USDT
from nautilus_trader.model.objects import Currency
from nautilus_trader.model.data import Bar, BarType
from nautilus_trader.model.enums import (
    AccountType,
    LiquiditySide,
    OmsType,
    OrderSide,
    TimeInForce,
)
from nautilus_trader.model.identifiers import (
    ClientOrderId,
    InstrumentId,
    Symbol,
    TraderId,
    Venue,
)
from nautilus_trader.model.instruments import CryptoPerpetual, CurrencyPair
from nautilus_trader.model.objects import Money, Price, Quantity
from nautilus_trader.trading.strategy import Strategy

MS = 1_000_000  # nanoseconds per millisecond
TIMEFRAME_MS = {
    "1m": 60_000,
    "5m": 300_000,
    "15m": 900_000,
    "30m": 1_800_000,
    "1h": 3_600_000,
    "4h": 14_400_000,
    "1d": 86_400_000,
}
BAR_SPEC = {
    "1m": "1-MINUTE",
    "5m": "5-MINUTE",
    "15m": "15-MINUTE",
    "30m": "30-MINUTE",
    "1h": "1-HOUR",
    "4h": "4-HOUR",
    "1d": "1-DAY",
}
# The reference strategies' unspent budget fraction (crates/td-engine/src/strategies/mod.rs).
HEADROOM = 0.005
# td-core's relative tolerance on the minimum quantity and notional.
TOLERANCE = 1e-9
LEVERAGE = Decimal(10)


# -------------------------------------------------------------------------------------------------
# Our engine's rounding and order checks, ported from td-core and td-engine
# -------------------------------------------------------------------------------------------------


def step_decimals(step: float) -> int:
    """td-core's `step_decimals`: the decimal places of a step such as 0.01, capped at 15."""
    for d in range(15):
        scaled = step * 10.0**d
        whole = float(round(scaled))
        if whole >= 1.0 and abs(scaled - whole) <= 1e-9 * whole:
            return d
    return 15


def round_step(value: float, step: float, mode: str) -> float:
    """td-core's `round_to_step`: `mode` is "down", "up" or "nearest" (half away from zero, as
    Rust's `f64::round`; Python's `round` would round half to even)."""
    scale = 10.0 ** step_decimals(step)
    step_units = float(round(step * scale))
    steps = value * scale / step_units
    eps = max(abs(steps) * 4.0 * 2.220446049250313e-16, 1e-9)
    if mode == "down":
        n = math.floor(steps + eps)
    elif mode == "up":
        n = math.ceil(steps - eps)
    else:
        n = float(Decimal(steps).to_integral_value(rounding=ROUND_HALF_UP))
    return n * step_units / scale


def reference_price(rec: dict, last: float) -> float:
    """The price an order is valued at before it fills (td-engine's `reference_price`): its limit
    price (a stop-limit's too), its trigger for a stop, or `last` for a market order."""
    if rec["kind"] == "market":
        return last
    if rec["kind"] == "stop":
        return rec["trigger"]
    return rec["price"]


def sma(closes: list[float], n: int) -> float:
    """The mean of the last n closes, summed oldest first as Rust's `Iterator::sum` does.

    Python's `sum` compensates rounding on floats (3.12+), which can differ in the last bit.
    """
    total = 0.0
    for c in closes[len(closes) - n :]:
        total += c
    return total / n


def direction(x: float) -> float:
    return 1.0 if x > 0.0 else (-1.0 if x < 0.0 else 0.0)


def atr(bars: list[dict], n: int) -> float:
    """The simple mean of the true range over the last n bars, as `donchian_breakout` computes it
    (and the example's `atr`), summed in the same order; needs n + 1 bars."""
    tail = bars[len(bars) - n - 1 :]
    total = 0.0
    for prev, b in zip(tail, tail[1:]):
        p = prev["close"]
        total += max(max(b["high"] - b["low"], abs(b["high"] - p)), abs(b["low"] - p))
    return total / n


def highest(bars: list[dict]) -> float:
    return max(b["high"] for b in bars)


def lowest(bars: list[dict]) -> float:
    return min(b["low"] for b in bars)


# -------------------------------------------------------------------------------------------------
# Funding
# -------------------------------------------------------------------------------------------------


class FundingModule(SimulationModule):
    """Gives the strategy the simulated exchange, to charge funding into its account."""

    def __init__(self) -> None:
        super().__init__(SimulationModuleConfig())

    def pre_process(self, data) -> None:  # noqa: D102
        pass

    def process(self, ts_now: int) -> None:  # noqa: D102
        pass

    def log_diagnostics(self, logger) -> None:  # noqa: D102
        pass

    def reset(self) -> None:  # noqa: D102
        pass


# -------------------------------------------------------------------------------------------------
# Strategies
# -------------------------------------------------------------------------------------------------


class CrosscheckConfig(StrategyConfig, frozen=True):
    instrument_id: str
    bar_type: str
    timeframe_ms: int
    last_bar_open: int
    kind: str
    taker: float
    tick_size: float
    lot_size: float
    min_qty: float
    min_notional: float
    max_leverage: float
    strategy: str
    params: dict
    funding: list  # [[time_ms, rate], ...], sorted


class CrosscheckStrategy(Strategy):
    """Records fills, equity and funding the way the harness compares them, and runs the case's
    strategy logic in `decide`.

    Orders go through `send`, a port of our `Context::submit`: the same rounding and checks,
    against `self.orders`, a mirror of our engine's open orders at the decision (an order leaves it
    when it fills, when the strategy cancels it, and when Nautilus cancels or rejects it). A
    protective stop attached to an entry is sent when the entry fills, as our broker places it
    then. Nautilus rejects a reduce-only order sent while there is no position, so an exit decided
    together with its entry (`transmit=False`) passes our checks at the decision and joins the
    open orders then, but is sent to Nautilus when the entry fills.
    """

    def __init__(self, config: CrosscheckConfig, module: FundingModule | None) -> None:
        super().__init__(config)
        self.c = config
        self.instrument_id = InstrumentId.from_str(config.instrument_id)
        self.bar_type = BarType.from_str(config.bar_type)
        self.module = module
        self.fills: list[dict] = []
        self.rejections: list[dict] = []
        self.equity: list[dict] = []
        self.funding: list[dict] = []
        self.closes: list[float] = []
        self.bars: list[dict] = []
        self._next_stamp = 0
        self._first_open: int | None = None
        # The position held going into the next bar: after this bar's fills, before the orders
        # decided at its close fill (which Nautilus fills at that close). `_carried_opened` is the
        # opening time of Nautilus's position then, which names the round trip funding belongs to.
        self._carried = 0.0
        self._carried_opened: int | None = None
        self._bar_open = 0
        self._close = 0.0
        self._final = False
        self.sent = False
        # Our engine's open orders, by our numbering; `_by_coid` maps Nautilus's ids back.
        self.orders: dict[int, dict] = {}
        self._by_coid: dict[str, int] = {}
        self._next_id = 1
        self.s: dict = {}  # the strategy's own state

    def on_start(self) -> None:
        self.instrument = self.cache.instrument(self.instrument_id)
        self.subscribe_bars(self.bar_type)

    # -- helpers -----------------------------------------------------------------------------

    def position(self) -> float:
        positions = self.cache.positions_open(instrument_id=self.instrument_id)
        return sum(p.signed_qty for p in positions)

    def position_opened(self) -> int | None:
        """The opening time (ms) of the open position, which identifies its round trip: a NETTING
        account reuses the position ID, but each fill opens at most one position."""
        positions = self.cache.positions_open(instrument_id=self.instrument_id)
        return positions[0].ts_opened // MS if positions else None

    def cash(self) -> float:
        account = self.portfolio.account(self.instrument_id.venue)
        return account.balance_total(USDT).as_double()

    def account_equity(self, close: float) -> float:
        account = self.portfolio.account(self.instrument_id.venue)
        cash = account.balance_total(USDT).as_double()
        if self.c.kind == "spot":
            base = account.balance_total(self.instrument.base_currency)
            return cash + (base.as_double() if base is not None else 0.0) * close
        upnl = 0.0
        for p in self.cache.positions_open(instrument_id=self.instrument_id):
            upnl += p.unrealized_pnl(Price(close, self.instrument.price_precision)).as_double()
        return cash + upnl

    def charge(
        self, bar_open: int, stamp: int, rate: float, position: float, price: float, at: str,
        opened: int | None,
    ):
        """Charges one stamp's funding. `opened` names the Nautilus position it was charged on
        (its opening time), so `round_trips` can give each round trip its funding."""
        payment = position * price * rate
        if payment != 0.0:
            self.module.exchange.adjust_account(Money(-payment, USDT))
        self.funding.append(
            {"bar": bar_open, "stamp": stamp, "rate": rate, "position": position,
             "price": price, "payment": payment, "at": at, "trade": opened if position else None}
        )

    # -- orders: our Context::submit, ported ---------------------------------------------------

    def reject(self, reason: str, qty: float) -> None:
        self.rejections.append({"bar": self._bar_open, "qty": qty, "reason": reason})

    def validate(self, rec: dict) -> str | None:
        """td-core's `Instrument::validate_order` on a rounded order; None when it passes."""
        c = self.c
        qty = rec["qty"]
        if not qty > 0.0:
            return "qty must be above zero"
        for name in ("price", "trigger"):
            if rec.get(name) is not None and not rec[name] > 0.0:
                return f"{name} must be above zero"
        if qty < c.min_qty * (1.0 - TOLERANCE):
            return "qty below the minimum"
        exempt = rec["reduce_only"] and c.kind == "perpetual"
        notional = qty * reference_price(rec, self._close)
        if not exempt and notional < c.min_notional * (1.0 - TOLERANCE):
            return "notional below the minimum"
        return None

    def check(self, rec: dict) -> str | None:
        """`Context::prepare`'s checks after the rounding; None when the order passes."""
        reason = self.validate(rec)
        if reason:
            return reason
        close = self._close
        spot = self.c.kind == "spot"
        if spot and rec["side"] == "sell":
            # Open sells count against the holding.
            pending = sum(o["qty"] for o in self.orders.values() if o["side"] == "sell")
            if rec["qty"] > (self.position() - pending) * (1.0 + 1e-9):
                return "spot cannot sell more than it holds"
        if spot and rec["side"] == "buy":
            # Open buys count against the cash, each at its reference price plus the taker fee.
            def cost(o: dict) -> float:
                return o["qty"] * reference_price(o, close) * (1.0 + self.c.taker)

            pending = sum(cost(o) for o in self.orders.values() if o["side"] == "buy")
            if cost(rec) + pending > self.cash() * (1.0 + 1e-9):
                return "spot cannot buy for more than its cash"
        if not spot and not rec["reduce_only"]:
            # The leverage cap, with the open orders on this side.
            position = self.position()
            side = [o for o in self.orders.values() if o["side"] == rec["side"]] + [rec]
            total = sum(o["qty"] for o in side)
            plain = sorted(
                ((o["qty"], reference_price(o, close)) for o in side if not o["reduce_only"]),
                key=lambda qp: -qp[1],
            )
            plain_qty = sum(q for q, _ in plain)
            plain_value = sum(q * p for q, p in plain)
            sign = 1.0 if rec["side"] == "buy" else -1.0
            notional = None
            if sign * position < 0.0:
                if total > abs(position) * (1.0 + 1e-9):
                    left = min(plain_qty, total - abs(position))
                    notional = 0.0
                    for q, p in plain:
                        take = min(q, left)
                        notional += take * p
                        left -= take
                        if left <= 0.0:
                            break
            else:
                notional = abs(position * close + sign * plain_value)
            if notional is not None:
                allowed = self.c.max_leverage * self.account_equity(close)
                if notional > allowed * (1.0 + 1e-9):
                    return "over the leverage cap"
        return None

    def check_stop(self, entry: dict, trigger: float) -> str | None:
        """`Context::check_stop` for a stop attached to `entry`."""
        if entry["reduce_only"]:
            return "a reduce-only order cannot have a protective stop attached"
        if self.c.kind == "spot" and entry["side"] == "sell":
            return "spot cannot attach a protective stop to a sell"
        stop = {"qty": entry["qty"], "kind": "stop", "price": None, "trigger": trigger,
                "reduce_only": True}
        reason = self.validate(stop)
        if reason:
            return f"protective stop: {reason}"
        price = reference_price(entry, self._close)
        wrong = trigger >= price if entry["side"] == "buy" else trigger <= price
        return "a protective stop must be beyond its entry" if wrong else None

    def send(
        self, side: str, qty: float, kind: str, price: float | None = None,
        trigger: float | None = None, reduce_only: bool = False, stop: float | None = None,
        transmit: bool = True,
    ) -> int | None:
        """`Context::submit` (or `submit_with_stop` with `stop`): rounds the order as our engine
        does (quantity down to the lot; limit prices down for a buy and up for a sell; triggers to
        the nearest tick), applies its checks, and sends it to Nautilus. Returns our id for the
        order, or None if our engine would reject it, recorded as a rejection. With `transmit`
        False the order joins the open orders now but is sent when the order before it fills."""
        c = self.c
        limit_mode = "down" if side == "buy" else "up"
        rec = {
            "side": side,
            "qty": round_step(qty, c.lot_size, "down"),
            "kind": kind,
            "price": None if price is None else round_step(price, c.tick_size, limit_mode),
            "trigger": None if trigger is None else round_step(trigger, c.tick_size, "nearest"),
            "reduce_only": reduce_only,
            "sent": len(self.bars),
            "attach": None,
            "then": [],
            "coid": None,
        }
        reason = self.check(rec)
        if reason is None and stop is not None:
            stop = round_step(stop, c.tick_size, "nearest")
            reason = self.check_stop(rec, stop)
            rec["attach"] = stop
        if reason is not None:
            self.reject(reason, rec["qty"])
            return None
        if self._final:
            # Decided on the last bar: our engine accepts it and it never fills, while Nautilus
            # would fill a market order at this close, so it is not sent.
            return None
        oid = self._next_id
        # An attached stop takes the next id in our engine.
        self._next_id += 2 if stop is not None else 1
        rec["id"] = oid
        if transmit:
            self.orders[oid] = rec
            self.transmit(rec)
        else:
            before = max(self.orders)
            self.orders[oid] = rec
            self.orders[before]["then"].append(oid)
        return oid

    def transmit(self, rec: dict) -> None:
        f = self.order_factory
        side = OrderSide.BUY if rec["side"] == "buy" else OrderSide.SELL
        qty = Quantity(rec["qty"], self.instrument.size_precision)
        pp = self.instrument.price_precision
        common = dict(instrument_id=self.instrument_id, order_side=side, quantity=qty,
                      time_in_force=TimeInForce.GTC, reduce_only=rec["reduce_only"])
        if rec["kind"] == "market":
            order = f.market(**common)
        elif rec["kind"] == "limit":
            order = f.limit(price=Price(rec["price"], pp), **common)
        elif rec["kind"] == "stop":
            order = f.stop_market(trigger_price=Price(rec["trigger"], pp), **common)
        elif rec["kind"] == "stop_limit":
            order = f.stop_limit(price=Price(rec["price"], pp),
                                 trigger_price=Price(rec["trigger"], pp), **common)
        else:
            raise ValueError(rec["kind"])
        rec["coid"] = order.client_order_id.value
        self._by_coid[rec["coid"]] = rec["id"]
        self.submit_order(order)

    def cancel(self, oid: int | None) -> bool:
        """`Context::cancel`: the order leaves the open orders at once, as in our engine."""
        rec = self.orders.pop(oid, None) if oid is not None else None
        if rec is None:
            return False
        for then in rec["then"]:
            self.orders.pop(then, None)
        if rec["coid"] is not None:
            order = self.cache.order(ClientOrderId(rec["coid"]))
            if order is not None and order.is_open:
                self.cancel_order(order)
        return True

    def cancel_all(self) -> None:
        for oid in list(self.orders):
            self.cancel(oid)

    def track(self, slot: str, cancel_after: int) -> str:
        """The example's `track`: "resting", "expired" (cancelled now) or "gone"."""
        oid = self.s.get(slot)
        self.s[slot] = None
        if oid is None or oid not in self.orders:
            return "gone"
        if len(self.bars) - self.orders[oid]["sent"] < cancel_after:
            self.s[slot] = oid
            return "resting"
        self.cancel(oid)
        return "expired"

    # -- events ------------------------------------------------------------------------------

    def on_bar(self, bar: Bar) -> None:
        close_time = bar.ts_event // MS
        bar_open = close_time - self.c.timeframe_ms
        o, c = bar.open.as_double(), bar.close.as_double()
        self._bar_open = bar_open
        if self._first_open is None:
            self._first_open = bar_open
        funding = self.c.funding
        # Stamps before the first bar are not charged.
        while self._next_stamp < len(funding) and funding[self._next_stamp][0] < self._first_open:
            self._next_stamp += 1
        # Due at the open or in a gap before it: on the position going into the bar, at the open.
        while self._next_stamp < len(funding) and funding[self._next_stamp][0] <= bar_open:
            stamp, rate = funding[self._next_stamp]
            self.charge(bar_open, stamp, rate, self._carried, o, "open", self._carried_opened)
            self._next_stamp += 1
        # Due later in the bar: on the position after the bar's fills, at the close.
        now = self.position()
        now_opened = self.position_opened()
        while self._next_stamp < len(funding) and funding[self._next_stamp][0] < close_time:
            stamp, rate = funding[self._next_stamp]
            self.charge(bar_open, stamp, rate, now, c, "in_bar", now_opened)
            self._next_stamp += 1
        self._carried = now
        self._carried_opened = now_opened
        equity = self.account_equity(c)
        self.equity.append({"time": bar_open, "equity": equity})
        self.closes.append(c)
        self.bars.append(
            {"open": o, "high": bar.high.as_double(), "low": bar.low.as_double(), "close": c}
        )
        self._close = c
        # An order decided on the last bar never fills in our engine; Nautilus would fill a market
        # order at this close, so the decision is made but nothing is sent (see `send`).
        self._final = bar_open >= self.c.last_bar_open
        self.decide(c, equity, now)

    def decide(self, close: float, equity: float, position: float) -> None:
        s = self.c.strategy
        if s in ("crosscheck_buy_and_hold", "crosscheck_short_and_hold"):
            if self.sent or self._final:
                return
            self.sent = True
            qty = equity * self.c.params["fraction"] / (close * (1.0 + self.c.taker))
            self.send("buy" if s == "crosscheck_buy_and_hold" else "sell", qty, "market")
        elif s == "sma_cross":
            self.sma_cross(close, equity, position)
        elif s == "donchian_breakout":
            self.donchian(close, position)
        elif s == "crosscheck_limit_revert":
            self.limit_revert(close, position)
        elif s == "crosscheck_bracket":
            self.bracket(close, position)
        elif s == "crosscheck_stop_limit_breakout":
            self.stop_limit_breakout(close, position)
        elif s == "crosscheck_sma_limit":
            self.sma_limit(close, equity, position)
        else:
            raise ValueError(f"no Nautilus version of strategy {s!r}")

    def affordable(self, budget: float, price: float) -> float:
        """`affordable_qty` with zero slippage: costs of 1.0 + taker + 0.0."""
        return budget * (1.0 - HEADROOM) / (price * (1.0 + self.c.taker + 0.0))

    def sma_cross(self, close: float, equity: float, position: float) -> None:
        """crates/td-engine/src/strategies/sma_cross.rs, `on_bar`."""
        p = self.c.params
        fast_n, slow_n = int(p.get("fast", 10)), int(p.get("slow", 30))
        frac = float(p.get("equity_fraction", 0.95))
        if len(self.closes) < slow_n or self.orders:
            return
        fast, slow = sma(self.closes, fast_n), sma(self.closes, slow_n)
        can_short = bool(p.get("allow_short", False)) and self.c.kind == "perpetual"
        if fast > slow:
            want = 1.0
        elif fast < slow and can_short:
            want = -1.0
        else:
            want = 0.0
        if direction(position) == want:
            return
        size = min(equity * frac / close, self.affordable(equity, close))
        delta = want * size - position
        self.send("buy" if delta > 0.0 else "sell", abs(delta), "market")

    def donchian(self, close: float, position: float) -> None:
        """crates/td-engine/src/strategies/donchian.rs, `on_bar`."""
        p = self.c.params
        entry_n, exit_n, atr_n = int(p["entry"]), int(p["exit"]), int(p["atr_period"])
        bars = self.bars
        n = len(bars)
        if n < max(entry_n, exit_n, atr_n) + 1:
            return
        # Wait while an entry or exit order is in flight.
        stop = self.s.get("stop")
        if any(oid != stop for oid in self.orders):
            return
        previous = bars[: n - 1]
        if position == 0.0:
            self.s["stop"] = None
            channel = previous[len(previous) - entry_n :]
            a = atr(bars, atr_n)
            if a <= 0.0:
                return
            can_short = bool(p["allow_short"]) and self.c.kind == "perpetual"
            distance = float(p["atr_stop"]) * a
            if close > highest(channel):
                side, trigger = "buy", close - distance
            elif can_short and close < lowest(channel):
                side, trigger = "sell", close + distance
            else:
                return
            if round_step(trigger, self.c.tick_size, "nearest") <= 0.0:
                return
            # Flat here, so the cash is the whole equity.
            affordable = self.affordable(self.cash(), close)
            qty = min(float(p["risk"]) * self.account_equity(close) / distance, affordable)
            oid = self.send(side, qty, "market", stop=trigger)
            if oid is not None:
                self.s["stop"] = oid + 1
        else:
            channel = previous[len(previous) - exit_n :]
            leave = close < lowest(channel) if position > 0.0 else close > highest(channel)
            if leave:
                if self.s.get("stop") is not None:
                    self.cancel(self.s["stop"])
                    self.s["stop"] = None
                side = "sell" if position > 0.0 else "buy"
                self.send(side, abs(position), "market", reduce_only=True)

    def limit_revert(self, close: float, position: float) -> None:
        """`LimitRevert` in crates/td-engine/examples/crosscheck.rs."""
        p = self.c.params
        if self.track("order", int(p["cancel_after"])) == "resting":
            return
        offset = float(p["offset"])
        if position == 0.0:
            price = close * (1.0 - offset)
            qty = self.cash() * float(p["fraction"]) / (price * (1.0 + self.c.taker))
            oid = self.send("buy", qty, "limit", price=price)
        else:
            price = close * (1.0 + offset)
            oid = self.send("sell", position, "limit", price=price, reduce_only=True)
        if oid is not None:
            self.s["order"] = oid

    def bracket(self, close: float, position: float) -> None:
        """`Bracket` in crates/td-engine/examples/crosscheck.rs."""
        p = self.c.params
        fast_n, slow_n, atr_n = int(p["fast"]), int(p["slow"]), int(p["atr_period"])
        if len(self.bars) < max(slow_n, atr_n + 1) or position != 0.0:
            return
        fast, slow = sma(self.closes, fast_n), sma(self.closes, slow_n)
        a = atr(self.bars, atr_n)
        self.cancel_all()
        if fast == slow or a <= 0.0:
            return
        sign = 1.0 if fast > slow else -1.0
        side, exit_side = ("buy", "sell") if fast > slow else ("sell", "buy")
        stop = close - sign * float(p["stop_atr"]) * a
        target = close + sign * float(p["target_atr"]) * a
        qty = float(p["qty"])
        if self.send(side, qty, "market", stop=stop) is not None:
            self.send(exit_side, qty, "limit", price=target, reduce_only=True, transmit=False)

    def stop_limit_breakout(self, close: float, position: float) -> None:
        """`StopLimitBreakout` in crates/td-engine/examples/crosscheck.rs."""
        p = self.c.params
        channel_n, trend_n = int(p["channel"]), int(p["trend"])
        n = len(self.bars)
        if n < max(channel_n, trend_n):
            return
        channel = self.bars[n - channel_n :]
        high, low = highest(channel), lowest(channel)
        above_trend = close > sma(self.closes, trend_n)
        if position != 0.0:
            self.s["entry"] = None
            self.s["held"] = self.s.get("held", 0) + 1
            if self.s["held"] >= int(p["hold"]):
                side = "sell" if position > 0.0 else "buy"
                self.send(side, abs(position), "market", reduce_only=True)
            return
        self.s["held"] = 0
        if self.track("entry", int(p["cancel_after"])) == "resting":
            return
        gap, offset = float(p["trigger_gap"]), float(p["limit_offset"])
        if above_trend:
            side, trigger, sign = "buy", high * (1.0 + gap), 1.0
        else:
            side, trigger, sign = "sell", low * (1.0 - gap), -1.0
        price = trigger * (1.0 + sign * offset)
        oid = self.send(side, float(p["qty"]), "stop_limit", price=price, trigger=trigger)
        if oid is not None:
            self.s["entry"] = oid

    def sma_limit(self, close: float, equity: float, position: float) -> None:
        """`SmaLimit` in crates/td-engine/examples/crosscheck.rs."""
        p = self.c.params
        fast_n, slow_n = int(p["fast"]), int(p["slow"])
        if len(self.closes) < slow_n:
            return
        fast, slow = sma(self.closes, fast_n), sma(self.closes, slow_n)
        if self.track("order", int(p["cancel_after"])) != "gone":
            return
        can_short = self.c.kind == "perpetual"
        if fast > slow:
            want = 1.0
        elif fast < slow and can_short:
            want = -1.0
        else:
            want = 0.0
        if direction(position) == want:
            return
        size = min(equity * float(p["equity_fraction"]) / close, self.affordable(equity, close))
        delta = want * size - position
        offset = float(p["offset"])
        if delta > 0.0:
            side, price = "buy", close * (1.0 - offset)
        else:
            side, price = "sell", close * (1.0 + offset)
        oid = self.send(side, abs(delta), "limit", price=price)
        if oid is not None:
            self.s["order"] = oid

    # -- order events --------------------------------------------------------------------------

    def _rec(self, event) -> dict | None:
        oid = self._by_coid.get(event.client_order_id.value)
        return None if oid is None else self.orders.get(oid)

    def on_order_filled(self, event) -> None:
        rec = self._rec(event)
        side = "buy" if event.order_side == OrderSide.BUY else "sell"
        self.fills.append(
            {
                "time": event.ts_event // MS,
                "side": side,
                "qty": event.last_qty.as_double(),
                "price": event.last_px.as_double(),
                "fee": event.commission.as_double(),
                "fee_currency": event.commission.currency.code,
                "fee_precision": event.commission.currency.precision,
                "liquidity": "maker" if event.liquidity_side == LiquiditySide.MAKER else "taker",
                "order_id": self._by_coid.get(event.client_order_id.value),
                "order_kind": None if rec is None else rec["kind"],
            }
        )
        if rec is None:
            return
        order = self.cache.order(event.client_order_id)
        if order is None or order.is_closed:
            self.orders.pop(rec["id"], None)
        if rec["attach"] is not None:
            # Our broker places the attached stop when its entry fills, for the filled quantity.
            stop = {
                "id": rec["id"] + 1, "side": "sell" if side == "buy" else "buy",
                "qty": event.last_qty.as_double(), "kind": "stop", "price": None,
                "trigger": rec["attach"], "reduce_only": True, "sent": rec["sent"],
                "attach": None, "then": [], "coid": None,
            }
            rec["attach"] = None
            self.orders[stop["id"]] = stop
            self.transmit(stop)
        for then in rec["then"]:
            if then in self.orders:
                self.transmit(self.orders[then])
        rec["then"] = []

    def _gone(self, event, rejected: bool) -> None:
        rec = self._rec(event)
        if rec is not None:
            self.orders.pop(rec["id"], None)
            for then in rec["then"]:
                self.orders.pop(then, None)
        if rejected:
            self.rejections.append({"time": event.ts_event // MS, "reason": str(event.reason)})

    def on_order_canceled(self, event) -> None:
        self._gone(event, rejected=False)

    def on_order_expired(self, event) -> None:
        self._gone(event, rejected=False)

    def on_order_rejected(self, event) -> None:
        self._gone(event, rejected=True)

    def on_order_denied(self, event) -> None:
        self._gone(event, rejected=True)


# -------------------------------------------------------------------------------------------------
# Building and running the backtest
# -------------------------------------------------------------------------------------------------


def round_trips(positions: list, last_bar: dict, funding: list[dict]) -> list[dict]:
    """Nautilus's round trips, from its own positions, in our engine's trade fields.

    On a NETTING account a fill that takes the position through zero is split by Nautilus's
    execution engine (`_flip_position`): the quantity of the open position closes it, the rest opens
    the next one, and the commission is shared in proportion to those quantities. Our engine splits
    a flip the same way (`td_engine::trades`), so both sides give one closed trade and one new trade
    per flip, the new one entered at the flip fill.

    - `gross_pnl` is the position's realized P&L (net of its commissions in Nautilus) plus those
      commissions, so `gross_pnl - fees` is Nautilus's own number, as it is ours.
    - A position still open after the last bar is reported the way our engine reports its open
      trade: exit at the last bar's open time and close, `gross_pnl - fees` its realized P&L (minus
      the entry commission) plus its unrealized P&L at that close.
    - `funding` is not Nautilus's, which has none: it is the harness's charges (`charge`) made while
      this position was the one held, as the strategy recorded them. `funding_gross` is the sum of
      their sizes, the scale the comparison measures a funding difference against.
    """
    paid: dict[int, float] = {}
    gross: dict[int, float] = {}
    for f in funding:
        if f["trade"] is None and f["payment"] != 0.0:
            raise ValueError(f"funding at {f['stamp']} was charged with no position to give it to")
        if f["trade"] is not None:
            paid[f["trade"]] = paid.get(f["trade"], 0.0) + f["payment"]
            gross[f["trade"]] = gross.get(f["trade"], 0.0) + abs(f["payment"])
    out = []
    for p in sorted(positions, key=lambda p: p.ts_opened):
        commissions = p.commissions()
        if any(m.currency != USDT for m in commissions):
            raise ValueError(f"position {p.id} has a commission not in USDT: {commissions}")
        fees = sum(m.as_double() for m in commissions)
        realized = p.realized_pnl.as_double() if p.realized_pnl is not None else 0.0
        opened = p.ts_opened // MS
        if p.is_open:
            close = last_bar["close"]
            pnl = realized + p.unrealized_pnl(Price(close, p.price_precision)).as_double()
            exit_time, exit_price = last_bar["open_time"], close
        else:
            pnl = realized
            exit_time, exit_price = p.ts_closed // MS, p.avg_px_close
        paid_here = paid.get(opened, 0.0)
        out.append(
            {
                "side": "long" if p.entry == OrderSide.BUY else "short",
                "entry_time": opened,
                "exit_time": exit_time,
                "open": bool(p.is_open),
                "qty": p.peak_qty.as_double(),
                "entry_price": p.avg_px_open,
                "exit_price": exit_price,
                "gross_pnl": pnl + fees,
                "fees": fees,
                "funding": paid_here,
                "funding_gross": gross.get(opened, 0.0),
                "net_pnl": pnl - paid_here,
                "fills": p.event_count,
                "fee_precision": USDT.precision,
            }
        )
    times = [t["entry_time"] for t in out]
    if len(set(times)) != len(times):
        raise ValueError("two Nautilus positions opened at the same time")
    unclaimed = set(paid) - set(times)
    if unclaimed:
        raise ValueError(f"funding charged on positions not in the report: {sorted(unclaimed)}")
    return out


def read_bars(path: Path) -> list[dict]:
    with path.open(newline="") as f:
        return [
            {
                "open_time": int(r["open_time"]),
                "open": float(r["open"]),
                "high": float(r["high"]),
                "low": float(r["low"]),
                "close": float(r["close"]),
                "volume": float(r["volume"]),
            }
            for r in csv.DictReader(f)
        ]


def read_funding(path: Path) -> list[list]:
    if not path.exists():
        return []
    with path.open(newline="") as f:
        return [[int(r["time"]), float(r["rate"])] for r in csv.DictReader(f)]


def make_instrument(case: dict, inst: dict):
    venue = Venue(case["venue"].upper())
    base = Currency.from_str(inst["base"])
    quote = Currency.from_str(inst["quote"])
    price_precision = step_decimals(inst["tick_size"])
    size_precision = step_decimals(inst["lot_size"])
    common = dict(
        raw_symbol=Symbol(case["symbol"]),
        base_currency=base,
        quote_currency=quote,
        price_precision=price_precision,
        size_precision=size_precision,
        price_increment=Price(inst["tick_size"], price_precision),
        size_increment=Quantity(inst["lot_size"], size_precision),
        min_quantity=Quantity(inst["min_qty"], size_precision),
        min_notional=Money(inst["min_notional"], quote),
        maker_fee=Decimal(repr(case["fees"]["maker"])),
        taker_fee=Decimal(repr(case["fees"]["taker"])),
        ts_event=0,
        ts_init=0,
    )
    if case["kind"] == "spot":
        return CurrencyPair(
            instrument_id=InstrumentId(Symbol(case["symbol"]), venue),
            margin_init=Decimal(0),
            margin_maint=Decimal(0),
            **common,
        )
    return CryptoPerpetual(
        instrument_id=InstrumentId(Symbol(case["symbol"] + "-PERP"), venue),
        settlement_currency=quote,
        is_inverse=False,
        margin_init=Decimal("0.1"),
        margin_maint=Decimal("0.05"),
        **common,
    )


def run_case(case: dict, harness: Path, series: str) -> dict:
    data = harness / case["data"]
    inst = json.loads((data / "instrument.json").read_text())
    bars = read_bars(data / f"bars_{series}.csv")
    funding = read_funding(data / "funding.csv") if case["kind"] == "perpetual" else []
    tf_ms = TIMEFRAME_MS[case["timeframe"]]

    instrument = make_instrument(case, inst)
    venue = instrument.id.venue
    engine = BacktestEngine(
        BacktestEngineConfig(
            trader_id=TraderId("CROSSCHECK-001"),
            logging=LoggingConfig(log_level="ERROR"),
        )
    )
    module = FundingModule()
    spot = case["kind"] == "spot"
    engine.add_venue(
        venue=venue,
        oms_type=OmsType.NETTING,
        account_type=AccountType.CASH if spot else AccountType.MARGIN,
        base_currency=None if spot else USDT,
        starting_balances=[Money(case["starting_cash"], USDT)],
        default_leverage=None if spot else LEVERAGE,
        fill_model=FillModel(prob_fill_on_limit=1.0, prob_slippage=0.0, random_seed=42),
        modules=[module],
        bar_execution=True,
        bar_adaptive_high_low_ordering=False,
    )
    engine.add_instrument(instrument)

    bar_type = BarType.from_str(f"{instrument.id}-{BAR_SPEC[case['timeframe']]}-LAST-EXTERNAL")
    pp, sp = instrument.price_precision, instrument.size_precision
    nbars = []
    for b in bars:
        ts = (b["open_time"] + tf_ms) * MS
        nbars.append(
            Bar(
                bar_type=bar_type,
                open=Price(b["open"], pp),
                high=Price(b["high"], pp),
                low=Price(b["low"], pp),
                close=Price(b["close"], pp),
                volume=Quantity(b["volume"], sp),
                ts_event=ts,
                ts_init=ts,
            )
        )
    engine.add_data(nbars)

    config = CrosscheckConfig(
        instrument_id=str(instrument.id),
        bar_type=str(bar_type),
        timeframe_ms=tf_ms,
        last_bar_open=bars[-1]["open_time"],
        kind=case["kind"],
        taker=case["fees"]["taker"],
        tick_size=inst["tick_size"],
        lot_size=inst["lot_size"],
        min_qty=inst["min_qty"],
        min_notional=inst["min_notional"],
        max_leverage=case.get("max_leverage", 1.0),
        strategy=case["strategy"],
        params=case.get("params", {}),
        funding=funding,
    )
    strategy = CrosscheckStrategy(config, module)
    engine.add_strategy(strategy)
    engine.run()

    account = engine.trader.generate_account_report(venue)
    # The positions report's rows: the cache's positions and the NETTING snapshots of closed ones.
    positions = engine.cache.positions(instrument_id=instrument.id)
    positions += engine.cache.position_snapshots()
    result = {
        "engine": "nautilus_trader",
        "case": case["name"],
        "series": series,
        "bars": len(bars),
        "fills": strategy.fills,
        "trades": round_trips(positions, bars[-1], strategy.funding),
        "rejected_orders": strategy.rejections,
        "equity": strategy.equity,
        "funding": strategy.funding,
        "funding_paid": sum(f["payment"] for f in strategy.funding),
        "final_position": strategy.position(),
        "account_rows": len(account),
    }
    engine.dispose()
    return result
