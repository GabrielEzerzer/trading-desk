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

The strategies are written again here from their Rust source, decision for decision, including
the lot rounding and the order checks our engine applies when an order is submitted.
"""

from __future__ import annotations

import csv
import json
import math
from decimal import Decimal
from pathlib import Path

from nautilus_trader.backtest.engine import BacktestEngine, BacktestEngineConfig
from nautilus_trader.backtest.models import FillModel
from nautilus_trader.backtest.modules import SimulationModule, SimulationModuleConfig
from nautilus_trader.config import LoggingConfig, StrategyConfig
from nautilus_trader.model.currencies import USDT
from nautilus_trader.model.objects import Currency
from nautilus_trader.model.data import Bar, BarType
from nautilus_trader.model.enums import AccountType, OmsType, OrderSide, TimeInForce
from nautilus_trader.model.identifiers import InstrumentId, Symbol, TraderId, Venue
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


def round_down(value: float, step: float) -> float:
    """td-core's `round_to_step(.., Rounding::Down)`, which `Instrument::round_qty` uses."""
    scale = 10.0 ** step_decimals(step)
    step_units = float(round(step * scale))
    steps = value * scale / step_units
    eps = max(abs(steps) * 4.0 * 2.220446049250313e-16, 1e-9)
    return math.floor(steps + eps) * step_units / scale


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
    lot_size: float
    min_qty: float
    min_notional: float
    max_leverage: float
    strategy: str
    params: dict
    funding: list  # [[time_ms, rate], ...], sorted


class CrosscheckStrategy(Strategy):
    """Records fills, equity and funding the way the harness compares them, and runs the case's
    strategy logic in `decide`."""

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
        self._next_stamp = 0
        self._first_open: int | None = None
        # The position held going into the next bar: after this bar's fills, before the orders
        # decided at its close fill (which Nautilus fills at that close).
        self._carried = 0.0
        self._bar_open = 0
        self.sent = False

    def on_start(self) -> None:
        self.instrument = self.cache.instrument(self.instrument_id)
        self.subscribe_bars(self.bar_type)

    # -- helpers -----------------------------------------------------------------------------

    def position(self) -> float:
        positions = self.cache.positions_open(instrument_id=self.instrument_id)
        return sum(p.signed_qty for p in positions)

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

    def charge(self, bar_open: int, stamp: int, rate: float, position: float, price: float, at: str):
        payment = position * price * rate
        if payment != 0.0:
            self.module.exchange.adjust_account(Money(-payment, USDT))
        self.funding.append(
            {"bar": bar_open, "stamp": stamp, "rate": rate, "position": position,
             "price": price, "payment": payment, "at": at}
        )

    def submit(self, side: OrderSide, qty: float, close: float) -> None:
        """Rounds and checks the order as `Context::submit` does, then sends it as a market order.

        Rejections our engine would make (below the lot, the minimum size or notional) are
        recorded and nothing is sent, as our strategies then do nothing on the bar.
        """
        qty = round_down(qty, self.c.lot_size)
        if not qty > 0.0 or qty < self.c.min_qty or qty * close < self.c.min_notional:
            self.rejections.append(
                {"bar": self._bar_open, "qty": qty, "reason": "below the lot, minimum size or notional"}
            )
            return
        order = self.order_factory.market(
            instrument_id=self.instrument_id,
            order_side=side,
            quantity=Quantity(qty, self.instrument.size_precision),
            time_in_force=TimeInForce.GTC,
        )
        self.submit_order(order)

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
            self.charge(bar_open, stamp, rate, self._carried, o, "open")
            self._next_stamp += 1
        # Due later in the bar: on the position after the bar's fills, at the close.
        now = self.position()
        while self._next_stamp < len(funding) and funding[self._next_stamp][0] < close_time:
            stamp, rate = funding[self._next_stamp]
            self.charge(bar_open, stamp, rate, now, c, "in_bar")
            self._next_stamp += 1
        self._carried = now
        equity = self.account_equity(c)
        self.equity.append({"time": bar_open, "equity": equity})
        self.closes.append(c)
        # An order decided on the last bar never fills in our engine; Nautilus would fill it at
        # this close, so no decision is made here.
        if bar_open >= self.c.last_bar_open:
            return
        self.decide(c, equity, now)

    def decide(self, close: float, equity: float, position: float) -> None:
        s = self.c.strategy
        if s in ("crosscheck_buy_and_hold", "crosscheck_short_and_hold"):
            if self.sent:
                return
            self.sent = True
            qty = equity * self.c.params["fraction"] / (close * (1.0 + self.c.taker))
            side = OrderSide.BUY if s == "crosscheck_buy_and_hold" else OrderSide.SELL
            self.submit(side, qty, close)
        elif s == "sma_cross":
            self.sma_cross(close, equity, position)
        else:
            raise ValueError(f"no Nautilus version of strategy {s!r}")

    def sma_cross(self, close: float, equity: float, position: float) -> None:
        """crates/td-engine/src/strategies/sma_cross.rs, `on_bar`."""
        p = self.c.params
        fast_n, slow_n = int(p.get("fast", 10)), int(p.get("slow", 30))
        frac = float(p.get("equity_fraction", 0.95))
        open_orders = self.cache.orders_open(instrument_id=self.instrument_id)
        if len(self.closes) < slow_n or open_orders:
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
        # affordable_qty with zero slippage: 1.0 + taker + 0.0.
        costs = 1.0 + self.c.taker + 0.0
        affordable = equity * (1.0 - HEADROOM) / (close * costs)
        size = min(equity * frac / close, affordable)
        target = want * size
        delta = target - position
        side = OrderSide.BUY if delta > 0.0 else OrderSide.SELL
        self.submit(side, abs(delta), close)

    def on_order_filled(self, event) -> None:
        self.fills.append(
            {
                "time": event.ts_event // MS,
                "side": "buy" if event.order_side == OrderSide.BUY else "sell",
                "qty": event.last_qty.as_double(),
                "price": event.last_px.as_double(),
                "fee": event.commission.as_double(),
                "fee_currency": event.commission.currency.code,
                "fee_precision": event.commission.currency.precision,
            }
        )

    def on_order_rejected(self, event) -> None:
        self.rejections.append({"time": event.ts_event // MS, "reason": str(event.reason)})

    def on_order_denied(self, event) -> None:
        self.rejections.append({"time": event.ts_event // MS, "reason": str(event.reason)})


# -------------------------------------------------------------------------------------------------
# Building and running the backtest
# -------------------------------------------------------------------------------------------------


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
    result = {
        "engine": "nautilus_trader",
        "case": case["name"],
        "series": series,
        "bars": len(bars),
        "fills": strategy.fills,
        "rejected_orders": strategy.rejections,
        "equity": strategy.equity,
        "funding": strategy.funding,
        "funding_paid": sum(f["payment"] for f in strategy.funding),
        "final_position": strategy.position(),
        "account_rows": len(account),
    }
    engine.dispose()
    return result
