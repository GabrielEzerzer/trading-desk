"""What a case exercises, counted: each case file lists the exercises of its row in issue #29
(`exercises`, ids from `EXERCISES` below), and the report counts the instances of each in the run.
A case whose checks all agree but which has no instance of one of its exercises is reported as
NOT EXERCISED, never PASS: agreement on something that never happened shows nothing.

Every count is taken on the real bars, from the paired fills and trades of the real comparison and
from our engine's result, except where a description says otherwise. A count comes with a detail
line: where the instances are, or how large.
"""

from __future__ import annotations

from dataclasses import dataclass
from typing import Callable

from compare import AMBIGUOUS, STOP_LIMIT_MAKER, Bars, Comparison, through


@dataclass
class Run:
    """What the counters read: the case, its export's meta and funding stamps (snapped times), the
    real bars, the real comparison and both engines' real results."""

    case: dict
    meta: dict
    funding_times: list[int]
    bars: Bars
    real: Comparison
    ours: dict
    theirs: dict
    tick: float


def _kind(f: dict) -> str:
    return "market" if f.get("at_decision", True) else (f.get("order_type") or {}).get("type", "")


def _resting(run: Run, kind: str) -> list[dict]:
    return [p for p in run.real.pairs if not p["ours"].get("at_decision", True)
            and _kind(p["ours"]) == kind]


def after_gap(bars: Bars, i: int) -> bool:
    """Whether bar `i` is the first after one or more buckets dropped from the bars."""
    return i > 0 and bars.bars[i]["open_time"] - bars.bars[i - 1]["open_time"] > bars.tf


def gaps(bars: Bars) -> list[int]:
    """The bars that open after dropped buckets, by index."""
    return [i for i in range(1, len(bars.bars)) if after_gap(bars, i)]


def stops_through_gap(run: Run) -> list[tuple[dict, float]]:
    """Stop fills on a bar that opened past the trigger where the bar before had not reached it:
    the price went through the stop between the two bars. Each with how far past it opened."""
    out = []
    for p in _resting(run, "stop"):
        i, buy = p["bar"], p["ours"]["side"] == "buy"
        if i == 0:
            continue
        trigger = p["ours"]["order_type"]["trigger"]
        b, prev = run.bars.bars[i], run.bars.bars[i - 1]
        opened_past = b["open"] > trigger if buy else b["open"] < trigger
        reached_before = prev["high"] >= trigger if buy else prev["low"] <= trigger
        if opened_past and not reached_before:
            out.append((p, abs(b["open"] - trigger)))
    return out


def limits_through_gap(run: Run) -> list[tuple[dict, float]]:
    """Resting limit fills on a bar that opened through the limit (bound 3, expected difference
    3), each with how far through it opened."""
    out = []
    for p in _resting(run, "limit"):
        gap = through(p, run.bars)
        if gap > 0:
            out.append((p, gap))
    return out


def _through_detail(found: list[tuple[dict, float]], run: Run, only_gaps: bool) -> tuple[int, str]:
    if only_gaps:
        found = [(p, d) for p, d in found if after_gap(run.bars, p["bar"])]
    if not found:
        return 0, ""
    worst = max(found, key=lambda x: x[1])
    at_gaps = sum(1 for p, _ in found if after_gap(run.bars, p["bar"]))
    return len(found), (
        f"largest {worst[1]:g} past the level on bar {worst[0]['bar']}; {at_gaps} on a bar after "
        "dropped buckets"
    )


def stop_on_entry_bar(run: Run) -> tuple[int, str]:
    fills = run.ours["fills"]
    bars = []
    for k, f in enumerate(fills):
        if _kind(f) != "stop":
            continue
        # The entry is stamped like the stop with the bar's open: a market entry at the open, a
        # resting entry inside the bar. Our engine lists the entry before its stop.
        if any(g["time"] == f["time"] and g["side"] != f["side"] for g in fills[:k]):
            bars.append(run.bars.index_of(f["time"]))
    return len(bars), f"bars {', '.join(str(b) for b in bars[:8])}" if bars else ""


def ambiguous_fill_bars(run: Run) -> tuple[int, str]:
    bars = sorted({p["bar"] for p in run.real.pairs if p.get("class") == AMBIGUOUS})
    return len(bars), f"bars {', '.join(str(b) for b in bars[:8])}" if bars else ""


def dropped_gaps(run: Run) -> tuple[int, str]:
    found = gaps(run.bars)
    if not found:
        return 0, ""
    moves = [(abs(run.bars.bars[i]["open"] - run.bars.bars[i - 1]["close"]), i) for i in found]
    move, i = max(moves)
    return len(found), (
        f"{len(run.meta.get('dropped_buckets', []))} bucket(s) in {len(found)} gap(s); largest move "
        f"across a gap {move:g} ({move / run.bars.bars[i - 1]['close']:.2%}) before bar {i}"
    )


def funding_in_gap(run: Run) -> tuple[int, str]:
    """Funding stamps no bar covers, between the first and last bar: a stamp inside dropped
    buckets, charged by both engines at the next bar's open."""
    first, last = run.bars.opens[0], run.bars.opens[-1]
    found = []
    for t in run.funding_times:
        if not first <= t <= last:
            continue
        i = run.bars.index_of(t)
        covered = i is not None or any(o <= t < o + run.bars.tf for o in run.bars.opens)
        if not covered:
            found.append(t)
    charged = {f["bar"] for f in run.ours["funding"] if f["at_open"] or f["in_bar"]}
    on_position = 0
    for t in found:
        nxt = next((o for o in run.bars.opens if o > t), None)
        if nxt in charged:
            on_position += 1
    return len(found), (
        f"stamps at {', '.join(str(t) for t in found[:4])}; {on_position} charged on an open "
        "position" if found else ""
    )


def _funding(run: Run, sign: int) -> tuple[int, str]:
    n = sum(1 for f in run.ours["funding"] for v in (f["at_open"], f["in_bar"]) if v * sign > 0)
    return n, ""


def flips(run: Run) -> tuple[int, str]:
    position, n = 0.0, 0
    for f in run.ours["fills"]:
        after = position + (f["qty"] if f["side"] == "buy" else -f["qty"])
        if abs(after) <= 1e-9 * (abs(position) + f["qty"]):
            after = 0.0
        if position and after and (after > 0) != (position > 0):
            n += 1
        position = after
    return n, ""


def _count(pred: Callable[[dict], bool]) -> Callable[[Run], tuple[int, str]]:
    return lambda run: (sum(1 for p in run.real.pairs if pred(p)), "")


def _trades(pred: Callable[[dict], bool]) -> Callable[[Run], tuple[int, str]]:
    return lambda run: (sum(1 for t in run.real.trades if pred(t)), "")


def _opening(run: Run) -> list[dict]:
    """Our fills that open a trade (the entry of each of our trades), by time."""
    times = {t["entry_time"] for t in run.ours["trades"]}
    return [f for f in run.ours["fills"] if f["time"] in times]


def benchmark(run: Run) -> tuple[int, str]:
    trades = run.ours["trades"]
    held = [t for t in trades if t["open"] and run.bars.decided_at_next_open(t["entry_time"]) == 0]
    return len(held), "one entry decided on the first bar, held to the end" if held else ""


def cancels(run: Run) -> tuple[int, str]:
    n = sum(1 for o in run.ours.get("orders", []) if o["status"]["status"] == "cancelled")
    return n, ""


def touch_fills(run: Run) -> tuple[int, str]:
    """Resting limit fills on a bar that reached the limit inside its range, not at the open: both
    engines fill them on touch, with no queue ahead and no need to trade through the limit
    (`prob_fill_on_limit=1.0` in Nautilus). The detail says how many bars only just reached it."""
    n = exact = within_tick = 0
    for p in _resting(run, "limit"):
        b, price = run.bars.bars[p["bar"]], p["ours"]["order_type"]["price"]
        buy = p["ours"]["side"] == "buy"
        if (b["open"] <= price) if buy else (b["open"] >= price):
            continue  # opened through: a gap fill, not a touch
        n += 1
        reach = (price - b["low"]) if buy else (b["high"] - price)
        exact += abs(reach) <= run.tick * 1e-6
        within_tick += reach <= run.tick * (1.0 + 1e-6)
    return n, (f"{exact} where the bar's extreme was the limit exactly, {within_tick} within a "
               "tick of it") if n else ""


def sized_entries(param: str) -> Callable[[Run], tuple[int, str]]:
    """Our entries, for a strategy that sizes them from `param` (rounded down to the lot)."""
    def count(run: Run) -> tuple[int, str]:
        if param not in run.case.get("params", {}):
            return 0, f"the strategy has no `{param}` parameter"
        return len(_opening(run)), ""
    return count


# id -> (what is counted, counter)
EXERCISES: dict[str, tuple[str, Callable[[Run], tuple[int, str]]]] = {
    "market_fills": ("paired fills at the price of their decision (market orders)",
                     _count(lambda p: p["ours"].get("at_decision", True))),
    "fees": ("paired fills with a fee charged", _count(lambda p: p["ours"]["fee"] != 0.0)),
    "marking": ("bars our trades were held, marked at each close",
                lambda run: (sum(t.get("bars_held", 0) for t in run.ours["trades"]), "")),
    "benchmark": ("buy and hold from the first bar to the end", benchmark),
    "round_trips": ("closed round trips paired", _trades(lambda t: not t["ours"]["open"])),
    "cash": ("buys on a spot cash account, sized from the cash",
             lambda run: (0, "not a spot case") if run.case["kind"] != "spot" else _count(
                 lambda p: p["side"] == "buy")(run)),
    "lot_rounding": ("entries sized from equity, rounded down to the lot",
                     sized_entries("equity_fraction")),
    "flips": ("fills that take the position through zero", flips),
    "funding_paid": ("funding charges the position paid (our engine)",
                     lambda run: _funding(run, 1)),
    "funding_received": ("funding charges the position received (our engine)",
                         lambda run: _funding(run, -1)),
    "funding_at_open": ("funding charged at a bar's open, on the position held into the bar",
                        lambda run: (sum(1 for f in run.ours["funding"] if f["at_open"]), "")),
    "short_trades": ("short round trips paired", _trades(lambda t: t["side"] == "short")),
    "stop_through_gap": (
        "stop fills on a bar that opened past the trigger the bar before had not reached",
        lambda run: _through_detail(stops_through_gap(run), run, False)),
    "stop_through_dropped_bucket": (
        "of those, on the first bar after dropped buckets",
        lambda run: _through_detail(stops_through_gap(run), run, True)),
    "stop_on_entry_bar": ("attached stops filled on the bar their entry filled on",
                          stop_on_entry_bar),
    "ambiguous_bars": ("ambiguous bars with a fill on them (expected difference 2)",
                       ambiguous_fill_bars),
    "reduce_only_exits": ("reduce-only exit fills paired",
                          _count(lambda p: p["ours"].get("reduce_only", False))),
    "risk_sizing": ("entries sized from the risk to the stop", sized_entries("risk")),
    "resting_limits": ("resting limit fills paired", lambda run: (len(_resting(run, "limit")), "")),
    "touch_fills": ("resting limit fills on a bar that reached the limit inside its range, filled "
                    "on touch", touch_fills),
    "maker_fee": ("paired fills charged the maker fee in both engines",
                  _count(lambda p: p["ours"].get("liquidity") == "maker"
                         and p["theirs"].get("liquidity") == "maker")),
    "cancels": ("orders cancelled (our engine)", cancels),
    "stop_limit_triggers": ("stop-limit fills paired",
                            lambda run: (len(_resting(run, "stop_limit")), "")),
    "stop_limit_rested": (
        "stop-limit fills Nautilus's walk rests after the trigger (expected difference 4)",
        _count(lambda p: p.get("class") == STOP_LIMIT_MAKER)),
    "dropped_buckets": ("gaps of dropped buckets inside the run", dropped_gaps),
    "funding_in_gap": ("funding stamps inside dropped buckets", funding_in_gap),
    "limit_through_gap": ("resting limit fills on a bar that opened through the limit "
                          "(expected difference 3)",
                          lambda run: _through_detail(limits_through_gap(run), run, False)),
    "limit_through_dropped_bucket": (
        "of those, on the first bar after dropped buckets",
        lambda run: _through_detail(limits_through_gap(run), run, True)),
}


def count(run: Run) -> list[dict]:
    """The case's exercises with their counts: `{id, what, count, detail}` each. An id not in
    `EXERCISES` is an error, so a typo in a case file cannot pass as zero."""
    out = []
    for ex in run.case.get("exercises", []):
        if ex not in EXERCISES:
            raise KeyError(f"{run.case['name']}: unknown exercise {ex!r}")
        what, counter = EXERCISES[ex]
        n, detail = counter(run)
        out.append({"id": ex, "what": what, "count": n, "detail": detail})
    return out


def not_exercised(counts: list[dict]) -> list[str]:
    return [c["id"] for c in counts if c["count"] == 0]
