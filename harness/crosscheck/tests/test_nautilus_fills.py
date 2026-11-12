"""Pins the NautilusTrader 1.231 bar-execution behaviours the agreement rule relies on, on hand-made
bars, with the fill model the cross-check uses (`prob_fill_on_limit=1.0`, `prob_slippage=0`, the
default open, high, low, close walk). An upgrade that changes any of them fails here first, before
it shows up as an unexplained difference in a case.

Each bar is processed after the strategy's `on_bar` of the bar before it, so an order sent on bar 0
meets bar 1 first. A fill's `bar` is the index of the bar whose close stamps it.
"""

from __future__ import annotations

from decimal import Decimal

import pytest
from nautilus_trader.backtest.engine import BacktestEngine, BacktestEngineConfig
from nautilus_trader.backtest.models import FillModel
from nautilus_trader.config import LoggingConfig, StrategyConfig
from nautilus_trader.model.currencies import USDT
from nautilus_trader.model.data import Bar, BarType
from nautilus_trader.model.enums import AccountType, LiquiditySide, OmsType, OrderSide
from nautilus_trader.model.identifiers import TraderId
from nautilus_trader.model.objects import Money, Price, Quantity
from nautilus_trader.trading.strategy import Strategy

from nautilus_side import make_instrument

H = 3_600_000
MS = 1_000_000


class PlanConfig(StrategyConfig, frozen=True):
    bar_type: str
    plan: list  # per bar index: orders as [kind, side, price, trigger, reduce_only]


class Plan(Strategy):
    def __init__(self, config: PlanConfig) -> None:
        super().__init__(config)
        self.plan = config.plan
        self.i = 0
        self.events: list[tuple] = []

    def on_start(self) -> None:
        self.bt = BarType.from_str(self.config.bar_type)
        self.inst = self.cache.instrument(self.bt.instrument_id)
        self.subscribe_bars(self.bt)

    def on_bar(self, bar: Bar) -> None:
        for kind, side, price, trigger, reduce_only in self.plan[self.i] if self.i < len(self.plan) else []:
            s = OrderSide.BUY if side == "buy" else OrderSide.SELL
            q = Quantity(1, self.inst.size_precision)
            f, iid = self.order_factory, self.bt.instrument_id
            if kind == "market":
                o = f.market(iid, s, q, reduce_only=reduce_only)
            elif kind == "limit":
                o = f.limit(iid, s, q, Price(price, 1), reduce_only=reduce_only)
            elif kind == "stop":
                o = f.stop_market(iid, s, q, Price(trigger, 1), reduce_only=reduce_only)
            else:
                o = f.stop_limit(iid, s, q, Price(price, 1), Price(trigger, 1), reduce_only=reduce_only)
            self.submit_order(o)
        self.i += 1

    def on_order_filled(self, e) -> None:
        maker = e.liquidity_side == LiquiditySide.MAKER
        side = "buy" if e.order_side == OrderSide.BUY else "sell"
        self.events.append(("fill", e.ts_event // MS // H - 1, side, e.last_px.as_double(),
                            "maker" if maker else "taker"))

    def on_order_rejected(self, e) -> None:
        self.events.append(("rejected", str(e.reason)))


def run(bars: list[tuple], plan: list) -> list[tuple]:
    case = {"venue": "binance", "kind": "perpetual", "symbol": "BTCUSDT",
            "fees": {"maker": 0.0002, "taker": 0.0005}}
    spec = {"base": "BTC", "quote": "USDT", "tick_size": 0.1, "lot_size": 0.001, "min_qty": 0.001,
            "min_notional": 0.0}
    inst = make_instrument(case, spec)
    eng = BacktestEngine(BacktestEngineConfig(trader_id=TraderId("PIN-001"),
                                              logging=LoggingConfig(log_level="ERROR")))
    eng.add_venue(
        venue=inst.id.venue, oms_type=OmsType.NETTING, account_type=AccountType.MARGIN,
        base_currency=USDT, starting_balances=[Money(1_000_000, USDT)],
        default_leverage=Decimal(10),
        fill_model=FillModel(prob_fill_on_limit=1.0, prob_slippage=0.0, random_seed=42),
        bar_execution=True,
    )
    eng.add_instrument(inst)
    bt = BarType.from_str(f"{inst.id}-1-HOUR-LAST-EXTERNAL")
    data = []
    for i, (o, h, lo, c) in enumerate(bars):
        ts = (i + 1) * H * MS  # stamped with its close
        data.append(Bar(bt, Price(o, 1), Price(h, 1), Price(lo, 1), Price(c, 1),
                        Quantity(1000, 3), ts, ts))
    eng.add_data(data)
    strategy = Plan(PlanConfig(bar_type=str(bt), plan=plan))
    eng.add_strategy(strategy)
    eng.run()
    events = strategy.events
    eng.dispose()
    return events


FLAT = (99.0, 99.5, 98.5, 99.0)
STOP_LIMIT_BUY = [[["stop_limit", "buy", 101.0, 100.0, False]]]  # trigger 100, limit 101


@pytest.mark.filterwarnings("ignore")
@pytest.mark.parametrize(
    ("name", "bars", "plan", "expected"),
    [
        (
            "a stop-limit whose bar stays at or below the limit fills at the trigger as a taker",
            [FLAT, (99, 100.5, 98, 99.5)], STOP_LIMIT_BUY, [("fill", 1, "buy", 100.0, "taker")],
        ),
        (
            # Expected difference 4: ours fills the same price as a taker.
            "a stop-limit whose bar runs past the limit fills at the trigger as a maker",
            [FLAT, (99, 102, 98, 99.5)], STOP_LIMIT_BUY, [("fill", 1, "buy", 100.0, "maker")],
        ),
        (
            # Ours fills at the open, 100.5: the per-unit difference is the gap past the trigger.
            "a stop-limit the bar opens past, short of the limit, fills at the trigger as a taker",
            [FLAT, (100.5, 100.8, 100.2, 100.5)], STOP_LIMIT_BUY,
            [("fill", 1, "buy", 100.0, "taker")],
        ),
        (
            # Ours triggers it, finds the limit not marketable at the open, and fills it at the
            # limit on the next bar that reaches it: the two fills land on different bars.
            "a stop-limit the bar opens past its limit fills on that bar at the low as a maker",
            [FLAT, (102, 103, 100.5, 102), (102, 102.5, 100, 101)], STOP_LIMIT_BUY,
            [("fill", 1, "buy", 100.5, "maker")],
        ),
        (
            "a stop-limit the walk triggers past its limit fills where the price comes back to it",
            [(101, 101.5, 100.5, 101), (101, 101.2, 98.5, 99.5)],
            [[["stop_limit", "sell", 99.0, 100.0, False]]],
            [("fill", 1, "sell", 99.5, "maker")],
        ),
        (
            "a stop-limit the walk triggers past its limit rests into the next bar, no better "
            "than its trigger",
            [(101, 101.5, 100.5, 101), (101, 101.2, 98.5, 98.8), (98.8, 99.6, 98.6, 99.0)],
            [[["stop_limit", "sell", 99.0, 100.0, False]]],
            [("fill", 2, "sell", 99.6, "maker")],
        ),
        (
            "a limit the bar opens through fills at its own price as a maker",
            [FLAT, (97, 97.5, 96, 97)], [[["limit", "buy", 98.0, None, False]]],
            [("fill", 1, "buy", 98.0, "maker")],
        ),
        (
            "a limit the low only touches fills",
            [FLAT, (99, 99.5, 98, 99)], [[["limit", "buy", 98.0, None, False]]],
            [("fill", 1, "buy", 98.0, "maker")],
        ),
        (
            "a market order fills at the close of the bar it was sent on",
            [FLAT, (99.5, 100, 99, 99.8)], [[["market", "buy", None, None, False]]],
            [("fill", 0, "buy", 99.0, "taker")],
        ),
        (
            # Expected difference 2: our broker fills the stop first.
            "a long bracket on a bar that reaches both fills the target first",
            [FLAT, FLAT, (99, 101, 97, 99)],
            [[["market", "buy", None, None, False]],
             [["stop", "sell", None, 98.0, True], ["limit", "sell", 100.5, None, True]]],
            [("fill", 0, "buy", 99.0, "taker"), ("fill", 2, "sell", 100.5, "maker")],
        ),
        (
            "a short bracket on a bar that reaches both fills the stop first",
            [FLAT, FLAT, (99, 101, 97, 99)],
            [[["market", "sell", None, None, False]],
             [["stop", "buy", None, 100.5, True], ["limit", "buy", 98.0, None, True]]],
            [("fill", 0, "sell", 99.0, "taker"), ("fill", 2, "buy", 100.5, "taker")],
        ),
        (
            # Ours cancels such an order at fill time instead: the strategies here never send one.
            "a reduce-only order with no position is rejected",
            [FLAT, (99, 101, 97, 99)], [[["limit", "sell", 100.5, None, True]]],
            [("rejected", "REDUCE_ONLY LIMIT SELL order would have increased position")],
        ),
    ],
)
def test_nautilus_bar_execution(name, bars, plan, expected):
    assert run(bars, plan) == expected, name
