"""The agreement rule of issue #29, applied to one case's two results.

Both results have the same shape (see `README.md`): `fills` with `time`, `side`, `qty`, `price`
and `fee`; `equity` with one `{time, equity}` per bar, stamped with the bar's open; `funding`.

Fills are paired by the bar they were decided on. Nautilus fills a market order at the close of
that bar, so its decision bar is the one that closes at the fill time; our engine fills it at the
next bar's open, so its decision bar is the bar before the one that opens at the fill time. Across
a gap in the data the two fill times differ, but the decision bar is the same.

On the gapless bars (`open[t+1] = close[t]`) everything must match within the tolerances below. On
the real bars the fills must pair one to one and each price difference must stay within bound 1,
`|open[N+1] - close[N]|` per unit, and the final equity difference within the sum of the bounds.

Round-trip trades are paired the same way, by the decision bar of their entry fill and their side,
and must close on the same decision bar (or both still be open at the end). Both engines split a
fill that flips the position in the same way: the open quantity closes the old trade, the rest
opens the new one, and the fee is shared in proportion (see `nautilus_side.round_trips`). A trade's
P&L is compared net of fees and before funding, `gross_pnl - fees`, which is the realized P&L
Nautilus computes itself; funding is compared on its own, per trade, on the gapless bars.
Lane 2's cases add the bars where the order inside a bar is ambiguous (left out of the exact
comparison, with our P&L on them at most Nautilus's) and bound 3, limits through a gap: both enter
here as arguments.
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
    """The case's bars, for finding decision bars and the open/close gap after each."""

    def __init__(self, bars: list[dict], timeframe_ms: int) -> None:
        self.bars = bars
        self.tf = timeframe_ms
        self.opens = [b["open_time"] for b in bars]
        self.by_close = {b["open_time"] + timeframe_ms: i for i, b in enumerate(bars)}

    def decided_at_next_open(self, fill_time: int) -> int | None:
        """Our engine: the bar before the one that opens at `fill_time`."""
        i = bisect.bisect_left(self.opens, fill_time)
        if i >= len(self.opens) or self.opens[i] != fill_time or i == 0:
            return None
        return i - 1

    def decided_at_close(self, fill_time: int) -> int | None:
        """Nautilus: the bar that closes at `fill_time`."""
        return self.by_close.get(fill_time)

    def gap_after(self, n: int) -> float:
        """|open[N+1] - close[N]|, the per-unit price bound of a market fill decided on bar N."""
        return abs(self.bars[n + 1]["open"] - self.bars[n]["close"])


def pair_fills(ours: list[dict], theirs: list[dict], bars: Bars) -> tuple[list[dict], list[str]]:
    """Pairs fills by decision bar and side, in order. Returns the pairs and the problems: fills
    left without a partner, or a decision bar that cannot be found."""
    problems: list[str] = []

    def keyed(fills: list[dict], decided, who: str) -> list[tuple[int, str, dict]]:
        out = []
        for f in fills:
            n = decided(f["time"])
            if n is None:
                problems.append(f"{who} fill at {f['time']} has no decision bar")
                continue
            out.append((n, f["side"], f))
        return out

    a = keyed(ours, bars.decided_at_next_open, "our")
    b = keyed(theirs, bars.decided_at_close, "Nautilus")
    pairs = []
    i = j = 0
    while i < len(a) and j < len(b):
        (na, sa, fa), (nb, sb, fb) = a[i], b[j]
        if na == nb and sa == sb:
            pairs.append({"bar": na, "side": sa, "ours": fa, "theirs": fb})
            i += 1
            j += 1
        elif (na, sa) < (nb, sb):
            problems.append(f"our {sa} fill decided on bar {na} has no Nautilus partner")
            i += 1
        else:
            problems.append(f"Nautilus {sb} fill decided on bar {nb} has no partner of ours")
            j += 1
    for n, s, _ in a[i:]:
        problems.append(f"our {s} fill decided on bar {n} has no Nautilus partner")
    for n, s, _ in b[j:]:
        problems.append(f"Nautilus {s} fill decided on bar {n} has no partner of ours")
    return pairs, problems


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


def fee_check(pairs: list[dict]) -> Check:
    """Fees within 1e-9 relative, or within half a unit of the currency precision Nautilus rounds
    its commission to (`Money`), which is the explained difference."""
    worst = 0.0
    worst_abs = 0.0
    unexplained = 0
    rounded = 0
    precisions: set[int] = set()
    for p in pairs:
        fo, ft = p["ours"]["fee"], p["theirs"]["fee"]
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
    note = ""
    if rounded:
        decimals = ", ".join(str(d) for d in sorted(precisions))
        note = (
            f"{rounded} fee(s) beyond 1e-9 relative differ by less than half a unit of the "
            f"{decimals} decimals Nautilus rounds a commission to (its Money type): explained"
        )
    return Check(
        "fees",
        unexplained == 0,
        f"max rel {worst:.2e}, max abs {worst_abs:.2e}",
        f"rel {FEE_REL:.0e}",
        note,
    )


def pairing_check(pairs: list[dict], problems: list[str], n_ours: int, n_theirs: int) -> Check:
    """Every fill has a partner, and there is at least one pair: a case where neither engine
    fills anything compares nothing and must not pass."""
    note = "; ".join(problems[:5]) or ("" if pairs else "no fills paired, so nothing was compared")
    return Check(
        "fills pair one to one (same decision bar and side)",
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


def pnl(trade: dict) -> float:
    """A trade's P&L net of fees, before funding: what both engines compute without funding."""
    return trade["gross_pnl"] - trade["fees"]


def money_floor(trade: dict) -> float:
    """MONEY_UNITS units of the last decimal Nautilus keeps the trade's amounts to."""
    return MONEY_UNITS * 10.0 ** -trade.get("fee_precision", 8)


def pair_trades(ours: list[dict], theirs: list[dict], bars: Bars) -> tuple[list[dict], list[str]]:
    """Pairs round trips by the decision bar of their entry fill and their side, in order, as fills
    are paired. A pair must also close on the same decision bar, or both be open at the end; a pair
    that differs there is a problem, not a pair. Returns the pairs and the problems."""
    problems: list[str] = []

    def keyed(trades: list[dict], decided, who: str) -> list[tuple[int, str, int | None, dict]]:
        out = []
        for t in trades:
            n = decided(t["entry_time"])
            x = None if t["open"] else decided(t["exit_time"])
            if n is None or (x is None and not t["open"]):
                problems.append(
                    f"{who} {t['side']} trade {t['entry_time']}..{t['exit_time']} has no decision bar"
                )
                continue
            out.append((n, t["side"], x, t))
        return out

    a = keyed(ours, bars.decided_at_next_open, "our")
    b = keyed(theirs, bars.decided_at_close, "Nautilus")
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
        "trades pair one to one (same entry and exit decision bar, side, open at the end)",
        bool(pairs) and not problems,
        f"{len(pairs)} paired of {n_ours} ours / {n_theirs} Nautilus",
        "all",
        note,
    )


def gapless_trade_checks(pairs: list[dict], tick: float) -> tuple[list[Check], dict]:
    """The exact comparison of paired trades: quantity, entry and exit prices, fees, P&L net of fees
    and before funding, and funding."""
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

    def amount_check(name: str, value, limit_rel: float) -> tuple[Check, float, float]:
        worst_rel = worst_abs = 0.0
        bad, rounded = [], 0
        for p in pairs:
            a, b = value(p["ours"]), value(p["theirs"])
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

    fee_c, _, fee_abs = amount_check("trades: fees", lambda t: t["fees"], FEE_REL)
    pnl_c, pnl_rel, pnl_abs = amount_check("trades: P&L net of fees, before funding", pnl, PNL_REL)
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


def real_trade_checks(pairs: list[dict], bars: Bars, taker: float) -> tuple[list[Check], dict]:
    """Each paired trade's P&L (net of fees, before funding) within the bound 1 of its entry and
    exit fills, `qty * |open[N+1] - close[N]| * (1 + taker)` each; an open trade's exit is the last
    close in both engines, so it adds nothing.

    The engines size from their own equity, which the earlier fill price differences already moved
    apart, so a pair's quantities can differ by a lot. That difference is the sizing difference the
    fills section explains, not a price difference bound 1 covers: Nautilus's P&L is first scaled to
    our quantity, which removes it and leaves `q * (diff_exit - diff_entry)` and the fees on those
    differences, both covered by bound 1 at our quantity.
    shortcut: the scaling is exact only for a trade with one entry fill (or the opening share of a
    flip) and one exit fill, as in cases 1 to 4; a Nautilus trade with more fills fails here. A
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
        gaps = bars.gap_after(p["bar"]) + (0.0 if o["open"] else bars.gap_after(p["exit_bar"]))
        bound = o["qty"] * gaps * (1.0 + taker)
        scaled = pnl(t) * o["qty"] / t["qty"]
        diff = abs(pnl(o) - scaled)
        worst_diff = max(worst_diff, diff)
        if bound > 0:
            worst_ratio = max(worst_ratio, diff / bound)
        if diff > bound * (1.0 + SLACK_REL) + money_floor(t):
            over.append(f"bar {p['bar']}: diff {diff:.6g} > bound {bound:.6g}")
    note = "; ".join((multi + over)[:5])
    check = Check(
        "trades: each P&L difference within its fills' bound 1",
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


def compare_gapless(
    ours: dict,
    theirs: dict,
    bars: Bars,
    tick: float,
    ambiguous_bars: frozenset[int] = frozenset(),
    perpetual: bool = False,
) -> Comparison:
    """The exact comparison on the gapless bars, outside `ambiguous_bars` (decision bar indices;
    a trade entered or closed on one is left out too).

    With `perpetual`, both engines must charge funding on at least one bar, so a case meant to
    exercise funding cannot pass on an empty funding series."""
    cmp = Comparison("gapless")
    of = [f for f in ours["fills"] if bars.decided_at_next_open(f["time"]) not in ambiguous_bars]
    tf = [f for f in theirs["fills"] if bars.decided_at_close(f["time"]) not in ambiguous_bars]
    pairs, problems = pair_fills(of, tf, bars)
    cmp.pairs = pairs
    cmp.checks.append(pairing_check(pairs, problems, len(of), len(tf)))
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
    price_diffs = [abs(p["ours"]["price"] - p["theirs"]["price"]) for p in pairs]
    max_price = max(price_diffs, default=0.0)
    cmp.checks.append(
        Check(
            "prices within one tick",
            max_price <= PRICE_TICKS * tick * (1.0 + SLACK_REL),
            f"max {max_price:.10g}",
            f"{PRICE_TICKS * tick:g}",
        )
    )
    cmp.checks.append(fee_check(pairs))
    cmp.checks.append(rejections_check(ours, theirs))

    def clear(t: dict, decided) -> bool:
        return decided(t["entry_time"]) not in ambiguous_bars and (
            t["open"] or decided(t["exit_time"]) not in ambiguous_bars
        )

    ot = [t for t in ours["trades"] if clear(t, bars.decided_at_next_open)]
    tt = [t for t in theirs["trades"] if clear(t, bars.decided_at_close)]
    trade_pairs, trade_problems = pair_trades(ot, tt, bars)
    cmp.checks.append(
        trade_pairing_check(trade_pairs, trade_problems, len(ot), len(tt), bool(of or tf))
    )
    trade_checks, trade_numbers = gapless_trade_checks(trade_pairs, tick)
    cmp.checks += trade_checks

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
    eq_o, eq_t = ours["equity"], theirs["equity"]
    same_times = [p["time"] for p in eq_o] == [p["time"] for p in eq_t]
    worst_point = max((rel(a["equity"], b["equity"]) for a, b in zip(eq_o, eq_t)), default=0.0)
    cmp.checks.append(
        Check(
            "equity curve, every bar",
            same_times and worst_point <= EQUITY_REL,
            f"{len(eq_o)} vs {len(eq_t)} points, max rel {worst_point:.2e}",
            f"rel {EQUITY_REL:.0e}",
            "" if same_times else "the curves have different bar times",
        )
    )
    final_o, final_t = eq_o[-1]["equity"], eq_t[-1]["equity"]
    final_rel = rel(final_o, final_t)
    cmp.checks.append(
        Check(
            "final equity",
            final_rel <= EQUITY_REL,
            f"{final_o:.8f} vs {final_t:.8f} (rel {final_rel:.2e})",
            f"rel {EQUITY_REL:.0e}",
        )
    )
    cmp.numbers = {
        "fills_compared": len(pairs),
        "max_price_diff": max_price,
        "final_equity_ours": final_o,
        "final_equity_nautilus": final_t,
        "final_equity_rel": final_rel,
        "max_point_rel": worst_point,
        "funding_rows": len(rows),
        "max_funding_rel": worst_f,
        **trade_numbers,
    }
    return cmp


def compare_real(
    ours: dict,
    theirs: dict,
    bars: Bars,
    taker: float,
    bound2: float = 0.0,
    bound3: float = 0.0,
) -> Comparison:
    """The bounded comparison on the real bars.

    Bound 1 per fill is `qty * |open[N+1] - close[N]| * (1 + taker)`: the price difference, and the
    taker fee the engines charge on it. `bound2` and `bound3` are lane 2's totals (ambiguous bars,
    limits through a gap); zero for market-order cases.
    """
    cmp = Comparison("real")
    pairs, problems = pair_fills(ours["fills"], theirs["fills"], bars)
    cmp.pairs = pairs
    cmp.checks.append(pairing_check(pairs, problems, len(ours["fills"]), len(theirs["fills"])))
    cmp.checks.append(rejections_check(ours, theirs))
    over = []
    bound1 = 0.0
    max_price = 0.0
    max_ratio = 0.0
    for p in pairs:
        gap = bars.gap_after(p["bar"])
        diff = abs(p["ours"]["price"] - p["theirs"]["price"])
        max_price = max(max_price, diff)
        limit = gap + SLACK_REL * p["ours"]["price"]
        if diff > limit:
            over.append(f"bar {p['bar']}: diff {diff:.10g} > gap {gap:.10g}")
        if gap > 0:
            max_ratio = max(max_ratio, diff / gap)
        qty = max(p["ours"]["qty"], p["theirs"]["qty"])
        bound1 += qty * gap * (1.0 + taker)
    cmp.checks.append(
        Check(
            "each price difference within |open[N+1] - close[N]|",
            not over,
            f"max diff {max_price:.10g}",
            "per fill",
            "; ".join(over[:5]),
        )
    )
    trade_pairs, trade_problems = pair_trades(ours["trades"], theirs["trades"], bars)
    cmp.checks.append(
        trade_pairing_check(
            trade_pairs,
            trade_problems,
            len(ours["trades"]),
            len(theirs["trades"]),
            bool(ours["fills"] or theirs["fills"]),
        )
    )
    trade_checks, trade_numbers = real_trade_checks(trade_pairs, bars, taker)
    cmp.checks += trade_checks
    final_o, final_t = ours["equity"][-1]["equity"], theirs["equity"][-1]["equity"]
    diff = abs(final_o - final_t)
    limit = bound1 + bound2 + bound3
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
        "final_equity_ours": final_o,
        "final_equity_nautilus": final_t,
        "final_equity_diff": diff,
        "bound_sum": limit,
        "bound1": bound1,
        "max_qty_rel": qty_diff,
        "funding_total_ours": ours["funding_paid"],
        "funding_total_nautilus": theirs["funding_paid"],
        **trade_numbers,
    }
    return cmp
