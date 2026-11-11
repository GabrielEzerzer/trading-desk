"""The agreement rule of issue #29, applied to one case's two results.

Both results have the same shape (see `README.md`): `fills` with `time`, `side`, `qty`, `price`,
`fee` and `liquidity`; `equity` with one `{time, equity}` per bar, stamped with the bar's open;
`funding`. Ours also has, per fill, whether it filled at the price of its decision (`at_decision`)
and its order's type, and `resting`: the orders open before each bar.

Fills are paired by the bar whose processing makes them in Nautilus, their "bar":

- a fill at the price of its decision (a market order, or a limit already marketable when it was
  sent) belongs to its decision bar. Nautilus fills it at that bar's close; our engine at the next
  bar's open, so its bar is the one before the bar that opens at the fill time. Across a gap in the
  data the two fill times differ, but the bar is the same;
- a fill of a resting order (a limit, a stop, a stop-limit, an attached stop) belongs to the bar it
  fills on: our engine stamps it with that bar's open, Nautilus with its close.

On the gapless bars (`open[t+1] = close[t]`) everything must match within the tolerances below. On
the real bars the fills must pair one to one, each price difference must stay within its bound and
the final equity difference within the sum of the bounds:

- bound 1, a fill at the price of its decision: `qty * |open[N+1] - close[N]| * (1 + taker)`;
- bound 2, an ambiguous bar (two or more resting orders reachable inside its range, so the order in
  which the price met them is unknown): `qty * (highest level - lowest level)` plus the largest fee
  either engine can charge on the dearest level, `qty * level * (|maker| + |taker|)`;
- bound 3, a resting order the bar opened through: `qty * |open - level| * (1 + taker)`, where the
  level is a limit's price, or a stop-limit's trigger when it fills at the trigger point;
- the stop-limit fee difference (expected difference 4, below), `qty * price * |taker - maker|`.

Ambiguous bars are left out of the exact comparison: their fills must still pair one to one, but
not on price or fee, and each trade that enters or exits on one must have our P&L at most
Nautilus's (our engine fills the stop first, the worse order), within the money floor; on the real
bars within that and the bounds of its other fills. The P&L difference of each such trade is taken
out of our equity from its exit bar on, and the bars from an ambiguous entry to the exit are not
compared, so the rest of the curve is still held to the exact rule.

Expected difference 4, the stop-limit in Nautilus's bar walk: Nautilus 1.231 moves the price
through a bar's open, high, low and close only, so it triggers a stop-limit at the first of those
points past the trigger, not at the trigger. If the limit is marketable there it fills at the
trigger as a taker, as ours does. If not, the order rests and fills as a maker at the first later
point where the limit is marketable, at that point's price but no better than the trigger
(`nautilus_stop_limit`, pinned in `tests/test_nautilus_fills.py`). Our engine follows the price
through the trigger itself and fills there as a taker. Where Nautilus's fill lands on the same bar,
its price must be the predicted one within a tick and its fee the maker fee, and the value
difference (price and fee) is taken out of our trade P&L and equity from that bar on; on the real
bars it adds `qty * (|price difference| + price * |taker - maker|)` to the bounds. Where it lands on
a later bar the fills cannot pair, and the case fails with that cause named.

Round-trip trades are paired the same way, by the bar of their entry fill and their side, and must
close on the same bar (or both still be open at the end). Our trades get their bars from our fills,
walked as our portfolio walks them; Nautilus's from their open and close times. Both engines split a
fill that flips the position in the same way: the open quantity closes the old trade, the rest
opens the new one, and the fee is shared in proportion (see `nautilus_side.round_trips`). A trade's
P&L is compared net of fees and before funding, `gross_pnl - fees`, which is the realized P&L
Nautilus computes itself; funding is compared on its own, per trade, on the gapless bars.
"""

from __future__ import annotations

import bisect
from dataclasses import dataclass, field

PRICE_TICKS = 1.0
FEE_REL = 1e-9
FUNDING_REL = 1e-9
EQUITY_REL = 1e-6
QTY_REL = 1e-9
PNL_REL = 1e-6
# Float noise allowed on top of a bound: values pass through decimal text and fixed-point money.
SLACK_REL = 1e-9
# A trade's P&L and fees in Nautilus are sums of amounts each rounded to its Money type's decimals
# (8 for USDT). The worst trade, entered and closed by flips, carries an entry fee that is a flip's
# rounded commission less its rounded first share (two half units), an exit fee that is a rounded
# first share (one), and the realized P&L rounded after the exit fill (one), or its unrealized P&L
# rounded instead if still open: four half units, two units of the last decimal. The floor allows
# twice that, four units, as the absolute limit under the relative ones for a P&L or fee near 0.
MONEY_UNITS = 4
# Our portfolio's flat snap: a position within this fraction of the quantities involved is zero.
FLAT_REL = 1e-9

EXACT, AMBIGUOUS, STOP_LIMIT_MAKER = "exact", "ambiguous", "stop_limit_maker"


@dataclass
class Check:
    """One line of a report: what was compared, the worst value, the limit, and the outcome."""

    name: str
    ok: bool
    value: str
    limit: str
    note: str = ""


@dataclass
class Comparison:
    series: str
    checks: list[Check] = field(default_factory=list)
    pairs: list[dict] = field(default_factory=list)
    numbers: dict = field(default_factory=dict)

    @property
    def ok(self) -> bool:
        return all(c.ok for c in self.checks)


def rel(a: float, b: float) -> float:
    """|a - b| relative to the larger magnitude; 0 when both are 0."""
    scale = max(abs(a), abs(b))
    return 0.0 if scale == 0.0 else abs(a - b) / scale


class Bars:
    """The case's bars, for finding a fill's bar and the open/close gap after each."""

    def __init__(self, bars: list[dict], timeframe_ms: int) -> None:
        self.bars = bars
        self.tf = timeframe_ms
        self.opens = [b["open_time"] for b in bars]
        self.by_close = {b["open_time"] + timeframe_ms: i for i, b in enumerate(bars)}

    def index_of(self, open_time: int) -> int | None:
        i = bisect.bisect_left(self.opens, open_time)
        return i if i < len(self.opens) and self.opens[i] == open_time else None

    def decided_at_next_open(self, fill_time: int) -> int | None:
        """Our engine, a fill at the price of its decision: the bar before the one that opens at
        `fill_time`."""
        i = self.index_of(fill_time)
        return None if i is None or i == 0 else i - 1

    def decided_at_close(self, fill_time: int) -> int | None:
        """Nautilus: the bar that closes at `fill_time`."""
        return self.by_close.get(fill_time)

    def gap_after(self, n: int) -> float:
        """|open[N+1] - close[N]|, the per-unit price bound of a market fill decided on bar N."""
        return abs(self.bars[n + 1]["open"] - self.bars[n]["close"])


def our_fill_bar(f: dict, bars: Bars) -> int | None:
    """The bar of one of our fills: the decision bar for a fill at the price of its decision, else
    the bar it filled on. A fill without `at_decision` is a market fill."""
    if f.get("at_decision", True):
        return bars.decided_at_next_open(f["time"])
    return bars.index_of(f["time"])


def their_fill_bar(f: dict, bars: Bars) -> int | None:
    return bars.decided_at_close(f["time"])


def _merge(a: list[tuple], b: list[tuple], what: str) -> tuple[list[tuple], list[str]]:
    """Pairs two lists of (bar, side, item) sorted by (bar, side), in order."""
    problems, pairs = [], []
    i = j = 0
    while i < len(a) and j < len(b):
        (na, sa, fa), (nb, sb, fb) = a[i], b[j]
        if na == nb and sa == sb:
            pairs.append((na, sa, fa, fb))
            i += 1
            j += 1
        elif (na, sa) < (nb, sb):
            problems.append(f"our {sa} {what} of bar {na} has no Nautilus partner")
            i += 1
        else:
            problems.append(f"Nautilus {sb} {what} of bar {nb} has no partner of ours")
            j += 1
    problems += [f"our {s} {what} of bar {n} has no Nautilus partner" for n, s, _ in a[i:]]
    problems += [f"Nautilus {s} {what} of bar {n} has no partner of ours" for n, s, _ in b[j:]]
    return pairs, problems


def pair_fills(ours: list[dict], theirs: list[dict], bars: Bars) -> tuple[list[dict], list[str]]:
    """Pairs fills by bar and side, in order. Returns the pairs and the problems: fills left
    without a partner, or a bar that cannot be found."""
    problems: list[str] = []

    def keyed(fills: list[dict], bar_of, who: str) -> list[tuple[int, str, dict]]:
        out = []
        for f in fills:
            n = bar_of(f, bars)
            if n is None:
                problems.append(f"{who} fill at {f['time']} has no bar")
                continue
            out.append((n, f["side"], f))
        # Stable: fills of one bar and side keep the order they happened in. Our engine makes a
        # limit's fill through a gap before the fills at that bar's open, which belong to the bar
        # before it, so our list is not in bar order as it stands.
        return sorted(out, key=lambda x: (x[0], x[1]))

    merged, more = _merge(keyed(ours, our_fill_bar, "our"), keyed(theirs, their_fill_bar, "Nautilus"),
                          "fill")
    pairs = [{"bar": n, "side": s, "ours": fo, "theirs": ft} for n, s, fo, ft in merged]
    return pairs, problems + more


# -------------------------------------------------------------------------------------------------
# Ambiguous bars, bound 3 and the stop-limit fee
# -------------------------------------------------------------------------------------------------


def _in_range_limit(buy: bool, price: float, b: dict) -> float | None:
    """A resting limit the bar reaches inside its range, not at the open: its price, else None."""
    if buy:
        return price if b["low"] <= price < b["open"] else None
    return price if b["open"] < price <= b["high"] else None


def _in_range_stop(buy: bool, trigger: float, b: dict) -> float | None:
    """A stop the bar triggers inside its range, not at the open: its trigger, else None."""
    if buy:
        return trigger if b["open"] < trigger <= b["high"] else None
    return trigger if b["low"] <= trigger < b["open"] else None


def _triggered_before(order: dict, bars: Bars, i: int) -> bool:
    """Whether a stop-limit still open before bar `i` was triggered on an earlier bar (our engine
    then rests it as a limit): a bar after its decision bar reached its trigger."""
    ot, buy = order["request"]["order_type"], order["request"]["side"] == "buy"
    d = bars.index_of(order["submitted_at"])
    start = 0 if d is None else d + 1
    for b in bars.bars[start:i]:
        if (b["high"] >= ot["trigger"]) if buy else (b["low"] <= ot["trigger"]):
            return True
    return False


def _level(order: dict, bars: Bars, i: int) -> float | None:
    """Where the order would fill inside bar `i`'s range, if it can, else None."""
    req = order["request"]
    ot, buy, b = req["order_type"], req["side"] == "buy", bars.bars[i]
    kind = ot["type"]
    if kind == "limit":
        return _in_range_limit(buy, ot["price"], b)
    if kind == "stop":
        return _in_range_stop(buy, ot["trigger"], b)
    if kind == "stop_limit":
        if _triggered_before(order, bars, i):
            return _in_range_limit(buy, ot["price"], b)
        return _in_range_stop(buy, ot["trigger"], b)
    return None


def _fills_on(order: dict, bars: Bars, i: int) -> bool:
    """Whether the order fills somewhere on bar `i`, at the open or inside the range."""
    req = order["request"]
    ot, buy, b = req["order_type"], req["side"] == "buy", bars.bars[i]
    kind = ot["type"]
    if kind == "market":
        return True
    if kind == "limit" or (kind == "stop_limit" and _triggered_before(order, bars, i)):
        return b["low"] <= ot["price"] if buy else b["high"] >= ot["price"]
    return b["high"] >= ot["trigger"] if buy else b["low"] <= ot["trigger"]


def ambiguous_bars(resting: list[dict], bars: Bars, maker: float, taker: float) -> dict[int, dict]:
    """The bars where two or more of our resting orders are reachable inside the range (not at the
    open, which comes first in both engines), so which the price met first is unknown: index ->
    the levels and bound 2.

    The orders are those open before the bar, and the protective stop of an entry among them that
    fills on the bar, which our broker checks against the whole bar after the entry's fill.

    Bound 2 of a bar: the engines can fill different ones of those orders, so a fill's price can
    differ by up to the spread of their levels, `qty * (highest - lowest)`, and its fee by up to
    the fee on the dearest level at both rates, `qty * highest * (|maker| + |taker|)` (a maker fee
    against a taker fee, of either sign), with `qty` the largest order's.
    """
    out: dict[int, dict] = {}
    for snap in resting:
        i = bars.index_of(snap["bar"])
        if i is None:
            continue
        reach: list[tuple[float, float]] = []
        for o in snap["orders"]:
            req = o["request"]
            level = _level(o, bars, i)
            if level is not None:
                reach.append((level, req["qty"]))
            stop = o.get("attached_stop")
            if stop and _fills_on(o, bars, i):
                stop_level = _in_range_stop(req["side"] != "buy", stop["trigger"], bars.bars[i])
                if stop_level is not None:
                    reach.append((stop_level, req["qty"]))
        if len(reach) >= 2:
            levels = [lv for lv, _ in reach]
            qty = max(q for _, q in reach)
            hi, lo = max(levels), min(levels)
            bound = qty * (hi - lo) + qty * hi * (abs(maker) + abs(taker))
            out[i] = {"levels": sorted(levels), "qty": qty, "bound": bound}
    return out


def through(pair: dict, bars: Bars) -> float:
    """Bound 3 per unit: how far the bar opened through a resting order's level, for a fill the
    bar's open went past. A limit then fills at its own price in both engines (Nautilus 1.231 too,
    so no difference is expected), while a venue would fill it at the open: `|open - price|`. A
    stop-limit filling at its trigger point (as a taker; at the open when the bar opened past the
    trigger) fills at the trigger in Nautilus: `|open - trigger|`. 0 for anything else."""
    o = pair["ours"]
    if o.get("at_decision", True):
        return 0.0
    ot = o.get("order_type") or {}
    buy, b = o["side"] == "buy", bars.bars[pair["bar"]]
    kind = ot.get("type")
    if kind == "limit" or (kind == "stop_limit" and o.get("liquidity") == "maker"):
        price = ot["price"]
        opened_past = b["open"] < price if buy else b["open"] > price
        return abs(price - b["open"]) if opened_past else 0.0
    if kind == "stop_limit":
        trigger = ot["trigger"]
        opened_past = b["open"] > trigger if buy else b["open"] < trigger
        return abs(b["open"] - trigger) if opened_past else 0.0
    return 0.0


def nautilus_stop_limit(order: dict, bars: Bars) -> tuple[int, float, str] | None:
    """Where Nautilus 1.231 fills one of our stop-limit orders, from the bar after its decision:
    (bar, price, liquidity), or None if it never fills in the bars given.

    Nautilus walks each bar open, high, low, close and triggers the order at the first point past
    its trigger. If the limit is marketable at that point, it fills at the trigger as a taker.
    Otherwise it rests, and fills as a maker at the first later point where the limit is
    marketable, at that point's price but no better than the trigger: `max(point, trigger)` for a
    buy, `min(point, trigger)` for a sell."""
    req = order["request"]
    ot, buy = req["order_type"], req["side"] == "buy"
    trigger, limit = ot["trigger"], ot["price"]
    d = bars.index_of(order["submitted_at"])
    triggered = False
    for j in range(0 if d is None else d + 1, len(bars.bars)):
        b = bars.bars[j]
        for point in (b["open"], b["high"], b["low"], b["close"]):
            marketable = point <= limit if buy else point >= limit
            if not triggered:
                if point >= trigger if buy else point <= trigger:
                    triggered = True
                    if marketable:
                        return j, trigger, "taker"
                continue
            if marketable:
                return j, max(point, trigger) if buy else min(point, trigger), "maker"
    return None


def open_orders(resting: list[dict]) -> dict[int, dict]:
    """Our orders by id, from the open orders listed before each bar."""
    return {o["id"]: o for snap in resting for o in snap["orders"]}


def walked_elsewhere(fills: list[dict], bars: Bars, orders: dict[int, dict]) -> list[str]:
    """Our stop-limit fills that Nautilus's bar walk fills on another bar (expected difference 4,
    the part that cannot pair)."""
    out = []
    for f in fills:
        order = orders.get(f.get("order_id"))
        if order is None or order["request"]["order_type"]["type"] != "stop_limit":
            continue
        n = our_fill_bar(f, bars)
        predicted = nautilus_stop_limit(order, bars)
        if predicted is not None and predicted[0] != n:
            out.append(
                f"our {f['side']} stop-limit fill of bar {n} at {f['price']:g}: Nautilus's walk "
                f"fills it on bar {predicted[0]} at {predicted[1]:g}"
            )
    return out


def walk_check(fills: list[dict], bars: Bars, orders: dict[int, dict]) -> Check | None:
    """Fails, naming each fill, when Nautilus's bar walk moves a stop-limit fill to another bar:
    the triggered order rests past the bar in Nautilus, where our engine fills it at the trigger.
    The cause is known (expected difference 4) but the fills, and what the strategy decides from
    them, cannot be paired."""
    moved = walked_elsewhere(fills, bars, orders)
    if not moved:
        return None
    return Check(
        "stop-limit fills on the same bar in both engines",
        False,
        f"{len(moved)} moved by Nautilus's bar walk",
        "none",
        "; ".join(moved[:5]),
    )


def classify(pairs: list[dict], bars: Bars, ambiguous: dict[int, dict],
             orders: dict[int, dict] | None = None) -> None:
    """Sets each pair's `class`: ambiguous (a resting fill on an ambiguous bar), the stop-limit in
    Nautilus's bar walk (with `predicted`, Nautilus's expected price), or exact."""
    orders = orders or {}
    for p in pairs:
        o = p["ours"]
        if not o.get("at_decision", True) and p["bar"] in ambiguous:
            p["class"] = AMBIGUOUS
            continue
        p["class"] = EXACT
        order = orders.get(o.get("order_id"))
        if order is None or order["request"]["order_type"]["type"] != "stop_limit":
            continue
        predicted = nautilus_stop_limit(order, bars)
        if predicted is not None and predicted[0] == p["bar"] and predicted[2] == "maker":
            p["class"] = STOP_LIMIT_MAKER
            p["predicted"] = predicted[1]


def value_shift(p: dict) -> float:
    """Our equity less Nautilus's from a fill pair onwards, with the same quantity: the price
    difference on the quantity, against the fill's side, and the fee difference."""
    o, t = p["ours"], p["theirs"]
    sign = 1.0 if o["side"] == "buy" else -1.0
    return sign * o["qty"] * (t["price"] - o["price"]) + (t["fee"] - o["fee"])


def expected_fee(p: dict, maker: float) -> float:
    """The fee Nautilus should charge on the pair: ours, or the maker fee on its own fill for the
    stop-limit fee difference."""
    if p.get("class") == STOP_LIMIT_MAKER:
        return p["theirs"]["qty"] * p["theirs"]["price"] * maker
    return p["ours"]["fee"]


# -------------------------------------------------------------------------------------------------
# Fills
# -------------------------------------------------------------------------------------------------


def funding_by_bar(ours: list[dict], theirs: list[dict]) -> list[dict]:
    """Our funding per bar (`at_open`, `in_bar`) beside Nautilus's charges summed the same way."""
    rows: dict[int, dict] = {}

    def row(bar: int) -> dict:
        return rows.setdefault(
            bar, {"bar": bar, "ours_open": 0.0, "ours_in": 0.0, "theirs_open": 0.0, "theirs_in": 0.0}
        )

    for f in ours:
        r = row(f["bar"])
        r["ours_open"] += f["at_open"]
        r["ours_in"] += f["in_bar"]
    for f in theirs:
        r = row(f["bar"])
        r["theirs_open" if f["at"] == "open" else "theirs_in"] += f["payment"]
    return [rows[k] for k in sorted(rows)]


def fee_check(pairs: list[dict], maker: float = 0.0) -> Check:
    """Fees within 1e-9 relative of the expected fee (ours, or the maker fee for the stop-limit
    fee difference), or within half a unit of the currency precision Nautilus rounds its commission
    to (`Money`), which is the explained difference."""
    worst = 0.0
    worst_abs = 0.0
    unexplained = 0
    rounded = 0
    precisions: set[int] = set()
    for p in pairs:
        fo, ft = expected_fee(p, maker), p["theirs"]["fee"]
        r = rel(fo, ft)
        worst = max(worst, r)
        worst_abs = max(worst_abs, abs(fo - ft))
        if r <= FEE_REL:
            continue
        precision = p["theirs"].get("fee_precision", 8)
        half_unit = 0.5 * 10.0 ** -precision
        if abs(fo - ft) <= half_unit * (1.0 + SLACK_REL):
            rounded += 1
            precisions.add(precision)
        else:
            unexplained += 1
    notes = []
    if rounded:
        decimals = ", ".join(str(d) for d in sorted(precisions))
        notes.append(
            f"{rounded} fee(s) beyond 1e-9 relative differ by less than half a unit of the "
            f"{decimals} decimals Nautilus rounds a commission to (its Money type): explained"
        )
    makers = sum(1 for p in pairs if p.get("class") == STOP_LIMIT_MAKER)
    if makers:
        notes.append(
            f"{makers} stop-limit fill(s) rest in Nautilus's bar walk and carry its maker fee "
            "against our taker fee: expected difference 4, held to the maker fee exactly"
        )
    return Check(
        "fees",
        unexplained == 0,
        f"max rel {worst:.2e}, max abs {worst_abs:.2e}",
        f"rel {FEE_REL:.0e}",
        "; ".join(notes),
    )


def pairing_check(pairs: list[dict], problems: list[str], n_ours: int, n_theirs: int) -> Check:
    """Every fill has a partner, and there is at least one pair: a case where neither engine
    fills anything compares nothing and must not pass."""
    note = "; ".join(problems[:5]) or ("" if pairs else "no fills paired, so nothing was compared")
    return Check(
        "fills pair one to one (same bar and side)",
        bool(pairs) and not problems,
        f"{len(pairs)} paired of {n_ours} ours / {n_theirs} Nautilus",
        "all",
        note,
    )


def rejections_check(ours: dict, theirs: dict) -> Check:
    """Both engines reject the same number of orders (our checks are mirrored on the Nautilus
    side, so a rejection only one engine makes is a difference to explain)."""
    a, b = len(ours.get("rejected_orders", [])), len(theirs.get("rejected_orders", []))
    return Check("rejected orders", a == b, f"{a} ours, {b} Nautilus", "equal")


# -------------------------------------------------------------------------------------------------
# Trades
# -------------------------------------------------------------------------------------------------


def pnl(trade: dict) -> float:
    """A trade's P&L net of fees, before funding: what both engines compute without funding."""
    return trade["gross_pnl"] - trade["fees"]


def money_floor(trade: dict) -> float:
    """MONEY_UNITS units of the last decimal Nautilus keeps the trade's amounts to."""
    return MONEY_UNITS * 10.0 ** -trade.get("fee_precision", 8)


def our_trade_bars(trades: list[dict], fills: list[dict], bars: Bars) -> list[tuple] | None:
    """The (entry bar, exit bar) of each of our trades, from our fills walked as our portfolio
    walks them: a fill that takes the position from flat opens a trade, one that takes it to flat
    or through zero closes it (and a flip opens the next). None when the walk does not give the
    trades' own entry times, so the trades cannot be keyed."""
    walk: list[list] = []
    position = 0.0
    for f in fills:
        n = our_fill_bar(f, bars)
        qty = f["qty"]
        after = position + (qty if f["side"] == "buy" else -qty)
        if abs(after) <= FLAT_REL * (abs(position) + qty):
            after = 0.0
        if position != 0.0 and (after == 0.0 or (after > 0.0) != (position > 0.0)):
            walk[-1][2] = n
        if after != 0.0 and (position == 0.0 or (after > 0.0) != (position > 0.0)):
            walk.append([f["time"], n, None])
        position = after
    if [w[0] for w in walk] != [t["entry_time"] for t in trades]:
        return None
    return [(n, x) for _, n, x in walk]


def pair_trades(
    ours: list[dict], theirs: list[dict], bars: Bars, our_fills: list[dict] | None = None
) -> tuple[list[dict], list[str]]:
    """Pairs round trips by the bar of their entry fill and their side, in order, as fills are
    paired. A pair must also close on the same bar, or both be open at the end; a pair that differs
    there is a problem, not a pair. Our trades take their bars from `our_fills` (see
    `our_trade_bars`); without them, every fill is taken to be at the price of its decision.
    Returns the pairs and the problems."""
    problems: list[str] = []
    our_bars = None
    if our_fills is not None and ours:
        our_bars = our_trade_bars(ours, our_fills, bars)
        if our_bars is None:
            problems.append("our trades do not follow from our fills, so they cannot be given bars")
            return [], problems
    if our_bars is None:
        our_bars = [
            (bars.decided_at_next_open(t["entry_time"]),
             None if t["open"] else bars.decided_at_next_open(t["exit_time"]))
            for t in ours
        ]

    def keyed(trades, trade_bars, who):
        out = []
        for t, (n, x) in zip(trades, trade_bars):
            if n is None or (x is None and not t["open"]):
                problems.append(f"{who} {t['side']} trade {t['entry_time']}..{t['exit_time']} has no bar")
                continue
            out.append((n, t["side"], x, t))
        return out

    their_bars = [
        (bars.decided_at_close(t["entry_time"]),
         None if t["open"] else bars.decided_at_close(t["exit_time"]))
        for t in theirs
    ]
    a = keyed(ours, our_bars, "our")
    b = keyed(theirs, their_bars, "Nautilus")
    pairs = []
    i = j = 0
    while i < len(a) and j < len(b):
        (na, sa, xa, ta), (nb, sb, xb, tb) = a[i], b[j]
        if na == nb and sa == sb:
            if ta["open"] != tb["open"]:
                which = "ours" if ta["open"] else "Nautilus's"
                problems.append(f"{sa} trade entered on bar {na} is open at the end only in {which}")
            elif xa != xb:
                problems.append(
                    f"{sa} trade entered on bar {na} closes on bar {xa} in ours, {xb} in Nautilus"
                )
            else:
                pairs.append({"bar": na, "exit_bar": xa, "side": sa, "ours": ta, "theirs": tb})
            i += 1
            j += 1
        elif (na, sa) < (nb, sb):
            problems.append(f"our {sa} trade entered on bar {na} has no Nautilus partner")
            i += 1
        else:
            problems.append(f"Nautilus {sb} trade entered on bar {nb} has no partner of ours")
            j += 1
    for n, s, _, _ in a[i:]:
        problems.append(f"our {s} trade entered on bar {n} has no Nautilus partner")
    for n, s, _, _ in b[j:]:
        problems.append(f"Nautilus {s} trade entered on bar {n} has no partner of ours")
    return pairs, problems


def trade_pairing_check(
    pairs: list[dict], problems: list[str], n_ours: int, n_theirs: int, has_fills: bool
) -> Check:
    """Every trade has a partner, and there is at least one pair: a case with fills but no trades
    to compare would pass on nothing, so it fails (a case without fills fails on its fills)."""
    note = "; ".join(problems[:5])
    if not pairs and not note:
        note = "no trades paired, so nothing was compared" + (
            " although the case has fills" if has_fills else ""
        )
    return Check(
        "trades pair one to one (same entry and exit bar, side, open at the end)",
        bool(pairs) and not problems,
        f"{len(pairs)} paired of {n_ours} ours / {n_theirs} Nautilus",
        "all",
        note,
    )


def trade_fills(trade_pairs: list[dict], fill_pairs: list[dict]) -> None:
    """Gives each trade pair the fill pairs of its entry and exit (`fills`, with the share of each
    fill's quantity that belongs to the trade: below 1 for a flip fill), and whether one of them is
    on an ambiguous bar (`ambiguous`, and `ambiguous_entry` for the entry)."""
    by_key: dict[tuple, list[dict]] = {}
    for p in fill_pairs:
        by_key.setdefault((p["bar"], p["side"]), []).append(p)
    for tp in trade_pairs:
        long = tp["side"] == "long"
        keys = [(tp["bar"], "buy" if long else "sell")]
        if not tp["ours"]["open"]:
            keys.append((tp["exit_bar"], "sell" if long else "buy"))
        tp["fills"] = []
        for role, key in zip(("entry", "exit"), keys):
            candidates = by_key.get(key, [])
            # Several fills of one bar and side: the trade's own is the one whose quantity covers
            # it, which is all of them for the strategies here (one order per side per bar).
            for p in candidates:
                share = min(1.0, tp["ours"]["qty"] / p["ours"]["qty"]) if p["ours"]["qty"] else 1.0
                tp["fills"].append((role, p, share))
        tp["ambiguous"] = any(p.get("class") == AMBIGUOUS for _, p, _ in tp["fills"])
        tp["ambiguous_entry"] = any(
            p.get("class") == AMBIGUOUS for role, p, _ in tp["fills"] if role == "entry"
        )
        walked = [(p, share) for _, p, share in tp["fills"] if p.get("class") == STOP_LIMIT_MAKER]
        tp["fee_adj"] = sum(share * (p["theirs"]["fee"] - p["ours"]["fee"]) for p, share in walked)
        # Our P&L less Nautilus's from the walked fills: the trade's share of each fill's value
        # difference, against the trade's own side (a fill that closes a trade has the opposite
        # side to it, which `value_shift` already accounts for).
        tp["pnl_adj"] = sum(share * value_shift(p) for p, share in walked)
        tp["walked_price"] = any(p["ours"]["price"] != p["theirs"]["price"] for p, _ in walked)


def gapless_trade_checks(pairs: list[dict], tick: float) -> tuple[list[Check], dict]:
    """The exact comparison of paired trades: quantity, entry and exit prices, fees, P&L net of fees
    and before funding, and funding. A trade with a stop-limit fee difference (`fee_adj`, Nautilus's
    fee less ours) is compared with that difference taken out."""
    checks = []
    qty_bad = [p for p in pairs if rel(p["ours"]["qty"], p["theirs"]["qty"]) > QTY_REL]
    checks.append(
        Check(
            "trades: same quantity",
            not qty_bad,
            f"{len(qty_bad)} differ",
            f"rel {QTY_REL:.0e}",
            "; ".join(
                f"bar {p['bar']}: {p['ours']['qty']} vs {p['theirs']['qty']}" for p in qty_bad[:5]
            ),
        )
    )
    price = 0.0
    for p in pairs:
        if p.get("walked_price"):
            continue  # its fills' prices are held to Nautilus's walk instead
        price = max(price, abs(p["ours"]["entry_price"] - p["theirs"]["entry_price"]))
        price = max(price, abs(p["ours"]["exit_price"] - p["theirs"]["exit_price"]))
    checks.append(
        Check(
            "trades: entry and exit prices within one tick",
            price <= PRICE_TICKS * tick * (1.0 + SLACK_REL),
            f"max {price:.10g}",
            f"{PRICE_TICKS * tick:g}",
        )
    )

    def amount_check(name: str, ours_value, theirs_value, limit_rel: float):
        worst_rel = worst_abs = 0.0
        bad, rounded = [], 0
        for p in pairs:
            a, b = ours_value(p), theirs_value(p)
            r, d = rel(a, b), abs(a - b)
            worst_rel, worst_abs = max(worst_rel, r), max(worst_abs, d)
            if r <= limit_rel:
                continue
            if d <= money_floor(p["theirs"]) * (1.0 + SLACK_REL):
                rounded += 1
            else:
                bad.append(f"bar {p['bar']}: {a:.10g} vs {b:.10g}")
        note = "; ".join(bad[:5])
        if rounded and not bad:
            note = (
                f"{rounded} trade(s) beyond {limit_rel:.0e} relative differ by at most "
                f"{MONEY_UNITS} units of the last decimal Nautilus keeps money to: rounding, explained"
            )
        check = Check(
            name,
            not bad,
            f"max rel {worst_rel:.2e}, max abs {worst_abs:.2e}",
            f"rel {limit_rel:.0e}, or {MONEY_UNITS} units of the last decimal",
            note,
        )
        return check, worst_rel, worst_abs

    fee_c, _, fee_abs = amount_check(
        "trades: fees",
        lambda p: p["ours"]["fees"] + p.get("fee_adj", 0.0),
        lambda p: p["theirs"]["fees"],
        FEE_REL,
    )
    pnl_c, pnl_rel, pnl_abs = amount_check(
        "trades: P&L net of fees, before funding",
        lambda p: pnl(p["ours"]) - p.get("pnl_adj", 0.0),
        lambda p: pnl(p["theirs"]),
        PNL_REL,
    )
    checks += [fee_c, pnl_c]

    # Funding per trade, against the size of the charges that make it up: a trade's funding can sum
    # to near 0 from charges of both signs, which a plain relative difference would blow up.
    worst_f = 0.0
    bad_f = []
    for p in pairs:
        a, b = p["ours"]["funding"], p["theirs"]["funding"]
        scale = max(p["theirs"].get("funding_gross", 0.0), abs(a), abs(b))
        r = 0.0 if scale == 0.0 else abs(a - b) / scale
        worst_f = max(worst_f, r)
        if r > FUNDING_REL:
            bad_f.append(f"bar {p['bar']}: {a:.10g} vs {b:.10g}")
    checks.append(
        Check(
            "trades: funding",
            not bad_f,
            f"max {worst_f:.2e} of the charges' total size",
            f"{FUNDING_REL:.0e}",
            "; ".join(bad_f[:5]),
        )
    )
    numbers = {
        "trades_compared": len(pairs),
        "trades_open": sum(1 for p in pairs if p["ours"]["open"]),
        "max_trade_price_diff": price,
        "max_trade_fee_abs": fee_abs,
        "max_trade_pnl_rel": pnl_rel,
        "max_trade_pnl_abs": pnl_abs,
        "max_trade_funding_rel": worst_f,
    }
    return checks, numbers


def ambiguous_trade_check(pairs: list[dict], allowance=lambda p: 0.0) -> tuple[Check, dict]:
    """Each trade that enters or exits on an ambiguous bar: our P&L (net of fees, before funding,
    less any stop-limit fee difference) at most Nautilus's, within the money floor and
    `allowance(pair)` (the real bars' bounds on its other fills). Our engine takes the worse order
    inside such a bar, so it may come out behind Nautilus, never ahead."""
    over = []
    worst_gap = 0.0  # the largest amount by which ours is below Nautilus's
    for p in pairs:
        a = pnl(p["ours"]) - p.get("pnl_adj", 0.0)
        b = pnl(p["theirs"]) * (p["ours"]["qty"] / p["theirs"]["qty"] if p["theirs"]["qty"] else 1.0)
        worst_gap = max(worst_gap, b - a)
        if a > b + money_floor(p["theirs"]) + allowance(p) * (1.0 + SLACK_REL):
            over.append(f"bar {p['bar']}: ours {a:.10g} above Nautilus's {b:.10g}")
    check = Check(
        "ambiguous trades: our P&L at most Nautilus's",
        not over,
        f"{len(pairs)} trade(s), ours up to {worst_gap:.6g} below",
        "ours <= Nautilus",
        "; ".join(over[:5]),
    )
    return check, {"ambiguous_trades": len(pairs), "max_ambiguous_shortfall": worst_gap}


# -------------------------------------------------------------------------------------------------
# The two comparisons
# -------------------------------------------------------------------------------------------------


def compare_gapless(
    ours: dict,
    theirs: dict,
    bars: Bars,
    tick: float,
    maker: float = 0.0,
    taker: float = 0.0,
    ambiguous: dict[int, dict] | None = None,
    perpetual: bool = False,
) -> Comparison:
    """The exact comparison on the gapless bars, outside the ambiguous bars (computed from our
    `resting` orders unless given).

    With `perpetual`, both engines must charge funding on at least one bar, so a case meant to
    exercise funding cannot pass on an empty funding series."""
    cmp = Comparison("gapless")
    amb = ambiguous if ambiguous is not None else ambiguous_bars(
        ours.get("resting", []), bars, maker, taker
    )
    orders = open_orders(ours.get("resting", []))
    pairs, problems = pair_fills(ours["fills"], theirs["fills"], bars)
    classify(pairs, bars, amb, orders)
    cmp.pairs = pairs
    cmp.checks.append(pairing_check(pairs, problems, len(ours["fills"]), len(theirs["fills"])))
    walk = walk_check(ours["fills"], bars, orders)
    if walk:
        cmp.checks.append(walk)
    qty_bad = [p for p in pairs if rel(p["ours"]["qty"], p["theirs"]["qty"]) > QTY_REL]
    cmp.checks.append(
        Check(
            "same quantity",
            not qty_bad,
            f"{len(qty_bad)} differ",
            f"rel {QTY_REL:.0e}",
            "; ".join(
                f"bar {p['bar']}: {p['ours']['qty']} vs {p['theirs']['qty']}" for p in qty_bad[:5]
            ),
        )
    )
    exact = [p for p in pairs if p["class"] != AMBIGUOUS]
    n_amb_fills = len(pairs) - len(exact)
    # A walked stop-limit is held to Nautilus's predicted price, the others to ours.
    price_diffs = [
        abs((p["predicted"] if p["class"] == STOP_LIMIT_MAKER else p["ours"]["price"])
            - p["theirs"]["price"])
        for p in exact
    ]
    max_price = max(price_diffs, default=0.0)
    cmp.checks.append(
        Check(
            "prices within one tick",
            max_price <= PRICE_TICKS * tick * (1.0 + SLACK_REL),
            f"max {max_price:.10g}",
            f"{PRICE_TICKS * tick:g}",
            f"{n_amb_fills} fill(s) on ambiguous bars left out" if n_amb_fills else "",
        )
    )
    cmp.checks.append(fee_check(exact, maker))
    cmp.checks.append(rejections_check(ours, theirs))

    trade_pairs, trade_problems = pair_trades(ours["trades"], theirs["trades"], bars, ours["fills"])
    cmp.checks.append(
        trade_pairing_check(
            trade_pairs, trade_problems, len(ours["trades"]), len(theirs["trades"]),
            bool(ours["fills"] or theirs["fills"]),
        )
    )
    trade_fills(trade_pairs, pairs)
    clear_trades = [t for t in trade_pairs if not t["ambiguous"]]
    amb_trades = [t for t in trade_pairs if t["ambiguous"]]
    trade_checks, trade_numbers = gapless_trade_checks(clear_trades, tick)
    cmp.checks += trade_checks
    amb_numbers = {"ambiguous_trades": 0, "max_ambiguous_shortfall": 0.0}
    if amb_trades:
        amb_check, amb_numbers = ambiguous_trade_check(amb_trades)
        cmp.checks.append(amb_check)

    rows = funding_by_bar(ours["funding"], theirs["funding"])
    worst_f = 0.0
    for r in rows:
        worst_f = max(worst_f, rel(r["ours_open"], r["theirs_open"]), rel(r["ours_in"], r["theirs_in"]))
    total_o, total_t = ours["funding_paid"], theirs["funding_paid"]
    charged = any(r["ours_open"] or r["ours_in"] for r in rows) and any(
        r["theirs_open"] or r["theirs_in"] for r in rows
    )
    cmp.checks.append(
        Check(
            "funding per bar",
            worst_f <= FUNDING_REL and (charged or not perpetual),
            f"{len(rows)} bars, max rel {worst_f:.2e}; total {total_o:.10g} vs {total_t:.10g}",
            f"rel {FUNDING_REL:.0e}",
            "" if charged or not perpetual else "no funding charged on a perpetual case",
        )
    )

    # Our equity less the explained differences, from the bar each is realized on: the stop-limit
    # fee differences at their fills, and each ambiguous trade's P&L difference at its exit.
    eq_o, eq_t = ours["equity"], theirs["equity"]
    last = len(eq_o) - 1
    shifts: list[tuple[int, float]] = [
        (p["bar"], value_shift(p)) for p in pairs if p["class"] == STOP_LIMIT_MAKER
    ]
    skipped: set[int] = set()
    for t in amb_trades:
        exit_bar = last if t["ours"]["open"] else t["exit_bar"]
        shifts.append((exit_bar, (pnl(t["ours"]) - t["pnl_adj"]) - pnl(t["theirs"])))
        if t["ambiguous_entry"]:
            skipped.update(range(t["bar"], exit_bar))
    explained = [0.0] * len(eq_o)
    for bar, amount in shifts:
        for k in range(bar, len(explained)):
            explained[k] += amount
    same_times = [p["time"] for p in eq_o] == [p["time"] for p in eq_t]
    worst_point = max(
        (rel(a["equity"] - explained[k], b["equity"])
         for k, (a, b) in enumerate(zip(eq_o, eq_t)) if k not in skipped),
        default=0.0,
    )
    notes = []
    if not same_times:
        notes.append("the curves have different bar times")
    if shifts:
        notes.append(
            f"ours taken less {explained[-1]:.6g} of explained differences from the bar each is "
            "realized on (stop-limits in Nautilus's bar walk, ambiguous trades)"
        )
    if skipped:
        notes.append(f"{len(skipped)} bar(s) from an ambiguous entry to its exit not compared")
    cmp.checks.append(
        Check(
            "equity curve, every bar",
            same_times and worst_point <= EQUITY_REL,
            f"{len(eq_o)} vs {len(eq_t)} points, max rel {worst_point:.2e}",
            f"rel {EQUITY_REL:.0e}",
            "; ".join(notes),
        )
    )
    final_o, final_t = eq_o[-1]["equity"], eq_t[-1]["equity"]
    final_adj = final_o - explained[-1]
    final_rel = rel(final_adj, final_t)
    cmp.checks.append(
        Check(
            "final equity",
            final_rel <= EQUITY_REL,
            f"{final_o:.8f} vs {final_t:.8f}"
            + (f" (ours {final_adj:.8f} after the explained differences)" if shifts else "")
            + f" (rel {final_rel:.2e})",
            f"rel {EQUITY_REL:.0e}",
        )
    )
    cmp.numbers = {
        "fills_compared": len(pairs),
        "fills_exact": len(exact),
        "max_price_diff": max_price,
        "final_equity_ours": final_o,
        "final_equity_nautilus": final_t,
        "final_equity_explained": explained[-1] if explained else 0.0,
        "final_equity_rel": final_rel,
        "max_point_rel": worst_point,
        "funding_rows": len(rows),
        "max_funding_rel": worst_f,
        "ambiguous_bars": len(amb),
        "ambiguous_fills": n_amb_fills,
        "stop_limit_maker_fills": sum(1 for p in pairs if p["class"] == STOP_LIMIT_MAKER),
        **trade_numbers,
        **amb_numbers,
    }
    return cmp


def fill_allowance(p: dict, bars: Bars, ambiguous: dict[int, dict], maker: float, taker: float,
                   tick: float) -> dict:
    """What a real-bar fill pair may differ by: `per_unit` on price (None: not checked), and its
    contribution to each bound at the larger of the two quantities."""
    o, t = p["ours"], p["theirs"]
    qty = max(o["qty"], t["qty"])
    out = {"per_unit": None, "bound1": 0.0, "bound3": 0.0, "fee4": 0.0, "fee4_per_unit": 0.0}
    if p["class"] == AMBIGUOUS:
        return out  # bound 2, per bar
    if o.get("at_decision", True):
        gap = bars.gap_after(p["bar"])
        out["per_unit"] = gap
        out["bound1"] = qty * gap * (1.0 + taker)
        return out
    gap = through(p, bars)
    out["per_unit"] = gap + PRICE_TICKS * tick
    out["bound3"] = qty * gap * (1.0 + taker)
    if p["class"] == STOP_LIMIT_MAKER:
        # Expected difference 4: Nautilus's price is held to its walk's (`predicted`, within a
        # tick), so the fills differ by the price difference and the fee at the other rate.
        out["per_unit"] = PRICE_TICKS * tick
        out["fee4_per_unit"] = abs(o["price"] - t["price"]) + max(o["price"], t["price"]) * abs(
            taker - maker
        )
        out["fee4"] = qty * out["fee4_per_unit"]
    return out


def real_trade_checks(pairs: list[dict], taker: float) -> tuple[list[Check], dict]:
    """Each paired trade's P&L (net of fees, before funding) within the bounds of its fills (bound
    1 of a fill at the price of its decision, bound 3 of a resting fill the bar opened through, the
    stop-limit fee difference, bound 2 of an ambiguous bar), at our quantity; an open trade's exit
    is the last close in both engines, so it adds nothing.

    The engines size from their own equity, which the earlier fill price differences already moved
    apart, so a pair's quantities can differ by a lot. That difference is the sizing difference the
    fills section explains, not a price difference the bounds cover: Nautilus's P&L is first scaled
    to our quantity, which removes it and leaves `q * (diff_exit - diff_entry)` and the fees on
    those differences, covered by the bounds at our quantity.
    shortcut: the scaling is exact only for a trade with one entry fill (or the opening share of a
    flip) and one exit fill, as in every case here; a Nautilus trade with more fills fails here. A
    strategy that scales in or out needs the bound summed over each trade's fills.
    """
    over, multi = [], []
    worst_diff = worst_ratio = qty_rel = 0.0
    for p in pairs:
        o, t = p["ours"], p["theirs"]
        if t.get("fills", 1) > (1 if t["open"] else 2):
            multi.append(f"bar {p['bar']}: {t['fills']} Nautilus fills")
            continue
        qty_rel = max(qty_rel, rel(o["qty"], t["qty"]))
        bound = 0.0
        for _, fp, _ in p["fills"]:
            a = fp["allowance"]
            if fp["class"] == AMBIGUOUS:
                bound += fp["bound2"]
            elif a["per_unit"] is not None:
                # The trade's own quantity: for a flip fill, its share of the fill.
                bound += o["qty"] * a["per_unit"] * (1.0 + taker)
            bound += o["qty"] * a["fee4_per_unit"]
        p["bound"] = bound
        scaled = pnl(t) * o["qty"] / t["qty"]
        diff = abs(pnl(o) - scaled)
        worst_diff = max(worst_diff, diff)
        if bound > 0:
            worst_ratio = max(worst_ratio, diff / bound)
        if diff > bound * (1.0 + SLACK_REL) + money_floor(t):
            over.append(f"bar {p['bar']}: diff {diff:.6g} > bound {bound:.6g}")
    note = "; ".join((multi + over)[:5])
    check = Check(
        "trades: each P&L difference within its fills' bounds",
        not over and not multi,
        f"max diff {worst_diff:.6g} (Nautilus scaled to our quantity), max {worst_ratio:.8g} of "
        "its bound",
        "per trade",
        note,
    )
    numbers = {
        "trades_compared": len(pairs),
        "trades_open": sum(1 for p in pairs if p["ours"]["open"]),
        "max_trade_pnl_diff": worst_diff,
        "max_trade_bound_ratio": worst_ratio,
        "max_trade_qty_rel": qty_rel,
    }
    return [check], numbers


def compare_real(
    ours: dict,
    theirs: dict,
    bars: Bars,
    taker: float,
    maker: float = 0.0,
    tick: float = 0.0,
    ambiguous: dict[int, dict] | None = None,
) -> Comparison:
    """The bounded comparison on the real bars (see the module's description of the bounds).

    A fill at the price of its decision must be within `|open[N+1] - close[N]|` of Nautilus's; a
    resting fill within one tick plus how far the bar opened through its level (bound 3 per unit);
    a fill on an ambiguous bar is held only to its bar's bound 2.
    """
    cmp = Comparison("real")
    amb = ambiguous if ambiguous is not None else ambiguous_bars(
        ours.get("resting", []), bars, maker, taker
    )
    orders = open_orders(ours.get("resting", []))
    pairs, problems = pair_fills(ours["fills"], theirs["fills"], bars)
    classify(pairs, bars, amb, orders)
    cmp.pairs = pairs
    cmp.checks.append(pairing_check(pairs, problems, len(ours["fills"]), len(theirs["fills"])))
    walk = walk_check(ours["fills"], bars, orders)
    if walk:
        cmp.checks.append(walk)
    cmp.checks.append(rejections_check(ours, theirs))
    over = []
    bound1 = bound3 = fee4 = 0.0
    max_price = 0.0
    max_ratio = 0.0
    for p in pairs:
        a = fill_allowance(p, bars, amb, maker, taker, tick)
        p["allowance"] = a
        p["bound2"] = amb[p["bar"]]["bound"] if p["class"] == AMBIGUOUS else 0.0
        bound1 += a["bound1"]
        bound3 += a["bound3"]
        fee4 += a["fee4"]
        if a["per_unit"] is None:
            continue
        reference = p["predicted"] if p["class"] == STOP_LIMIT_MAKER else p["ours"]["price"]
        diff = abs(reference - p["theirs"]["price"])
        max_price = max(max_price, abs(p["ours"]["price"] - p["theirs"]["price"]))
        limit = a["per_unit"] + SLACK_REL * p["ours"]["price"]
        if diff > limit:
            over.append(f"bar {p['bar']}: diff {diff:.10g} > {a['per_unit']:.10g}")
        if a["per_unit"] > 0:
            max_ratio = max(max_ratio, diff / a["per_unit"])
    bound2 = sum(v["bound"] for v in amb.values())
    cmp.checks.append(
        Check(
            "each price difference within its bound",
            not over,
            f"max diff {max_price:.10g}",
            "per fill: |open[N+1] - close[N]| at the decision price; one tick plus bound 3 per "
            "unit for a resting order; one tick from Nautilus's walk for a stop-limit it rests",
            "; ".join(over[:5]),
        )
    )
    trade_pairs, trade_problems = pair_trades(ours["trades"], theirs["trades"], bars, ours["fills"])
    cmp.checks.append(
        trade_pairing_check(
            trade_pairs,
            trade_problems,
            len(ours["trades"]),
            len(theirs["trades"]),
            bool(ours["fills"] or theirs["fills"]),
        )
    )
    trade_fills(trade_pairs, pairs)
    trade_checks, trade_numbers = real_trade_checks(trade_pairs, taker)
    cmp.checks += trade_checks
    amb_trades = [t for t in trade_pairs if t["ambiguous"]]
    amb_numbers = {"ambiguous_trades": 0, "max_ambiguous_shortfall": 0.0}
    if amb_trades:
        # Within the bounds of the trade's fills other than bound 2.
        amb_check, amb_numbers = ambiguous_trade_check(
            amb_trades,
            lambda t: max(0.0, t.get("bound", 0.0) - sum(fp["bound2"] for _, fp, _ in t["fills"])),
        )
        cmp.checks.append(amb_check)
    final_o, final_t = ours["equity"][-1]["equity"], theirs["equity"][-1]["equity"]
    diff = abs(final_o - final_t)
    limit = bound1 + bound2 + bound3 + fee4
    cmp.checks.append(
        Check(
            "final equity difference within the sum of the bounds",
            diff <= limit * (1.0 + SLACK_REL) + 1e-6,
            f"{final_o:.8f} vs {final_t:.8f}, diff {diff:.6f}",
            f"{limit:.6f}",
        )
    )
    qty_diff = max((rel(p["ours"]["qty"], p["theirs"]["qty"]) for p in pairs), default=0.0)
    cmp.numbers = {
        "fills_compared": len(pairs),
        "max_price_diff": max_price,
        "max_price_ratio": max_ratio,
        "final_equity_ours": final_o,
        "final_equity_nautilus": final_t,
        "final_equity_diff": diff,
        "bound_sum": limit,
        "bound1": bound1,
        "bound2": bound2,
        "bound3": bound3,
        "stop_limit_fee": fee4,
        "ambiguous_bars": len(amb),
        "ambiguous_fills": sum(1 for p in pairs if p["class"] == AMBIGUOUS),
        "stop_limit_maker_fills": sum(1 for p in pairs if p["class"] == STOP_LIMIT_MAKER),
        "through_fills": sum(1 for p in pairs if p["allowance"]["bound3"] > 0),
        "max_qty_rel": qty_diff,
        "funding_total_ours": ours["funding_paid"],
        "funding_total_nautilus": theirs["funding_paid"],
        **trade_numbers,
        **amb_numbers,
    }
    return cmp
