"""The agreement rule on small hand-made results: pairing, tolerances and bounds. Offline."""

from __future__ import annotations

import copy

import compare
from compare import Bars, compare_gapless, compare_real, pair_fills

H = 3_600_000
TICK = 0.1


def bars(gap_at: int | None = None) -> Bars:
    """Six hourly bars; with `gap_at`, the bar at that index is missing. Opens differ from the
    previous close by 1.0 so bound 1 is 1.0 per unit."""
    rows = []
    close = 100.0
    for i in range(6):
        if i == gap_at:
            close += 2.0
            continue
        open_ = close + 1.0 if i else 100.0
        close = open_ + 2.0
        rows.append(
            {"open_time": i * H, "open": open_, "high": close + 1, "low": open_ - 1, "close": close}
        )
    return Bars(rows, H)


def fill(time: int, side: str, qty: float, price: float, fee: float, **more) -> dict:
    return {
        "time": time, "side": side, "qty": qty, "price": price, "fee": fee, "fee_precision": 8,
        **more,
    }


def trade(
    entry: int,
    exit_: int,
    side: str,
    qty: float,
    entry_price: float,
    exit_price: float,
    fees: float,
    funding: float = 0.0,
    open_: bool = False,
) -> dict:
    """A round trip in the fields both engines export; `gross_pnl` follows from the prices."""
    sign = 1.0 if side == "long" else -1.0
    gross = sign * qty * (exit_price - entry_price)
    return {
        "side": side,
        "entry_time": entry,
        "exit_time": exit_,
        "open": open_,
        "qty": qty,
        "entry_price": entry_price,
        "exit_price": exit_price,
        "gross_pnl": gross,
        "fees": fees,
        "funding": funding,
        "funding_gross": abs(funding),
        "net_pnl": gross - fees - funding,
        "fills": 1 if open_ else 2,
        "fee_precision": 8,
    }


def result(
    fills: list[dict], equity: list[float], funding=None, funding_paid=0.0, trades=None
) -> dict:
    return {
        "fills": fills,
        "trades": trades or [],
        "equity": [{"time": i * H, "equity": e} for i, e in enumerate(equity)],
        "funding": funding or [],
        "funding_paid": funding_paid,
        "rejected_orders": [],
    }


def gapless_pair() -> tuple[dict, dict]:
    """A buy decided on bar 1 and a sell decided on bar 3: ours fills at the next open, Nautilus
    at the decision bar's close, the same instant and price on gapless bars."""
    ours = result(
        [fill(2 * H, "buy", 1.5, 106.0, 0.159), fill(4 * H, "sell", 1.5, 112.0, 0.168)],
        [10_000.0, 10_000.0, 9_999.8, 10_003.0, 10_008.7, 10_008.7],
        trades=[trade(2 * H, 4 * H, "long", 1.5, 106.0, 112.0, 0.327)],
    )
    theirs = copy.deepcopy(ours)
    theirs["fills"][0]["time"] = 2 * H  # close of bar 1 is the open of bar 2
    return ours, theirs


def by_name(cmp: compare.Comparison) -> dict[str, compare.Check]:
    return {c.name.split(" (")[0]: c for c in cmp.checks}


def test_fills_pair_by_decision_bar_across_a_gap():
    b = bars(gap_at=3)
    # Decided on bar 2 (index 2, open 2h): Nautilus fills at its close, 3h; we fill at the next
    # bar's open, 4h, since the 3h bar is missing.
    ours = [fill(4 * H, "buy", 1.0, 110.0, 0.1)]
    theirs = [fill(3 * H, "buy", 1.0, 109.0, 0.1)]
    pairs, problems = pair_fills(ours, theirs, b)
    assert problems == []
    assert [(p["bar"], p["side"]) for p in pairs] == [(2, "buy")]


def test_an_unpaired_fill_is_a_problem():
    b = bars()
    ours = [fill(2 * H, "buy", 1.0, 106.0, 0.1), fill(4 * H, "sell", 1.0, 112.0, 0.1)]
    theirs = [fill(2 * H, "buy", 1.0, 106.0, 0.1)]
    pairs, problems = pair_fills(ours, theirs, b)
    assert len(pairs) == 1
    assert problems == ["our sell fill of bar 3 has no Nautilus partner"]


def test_a_side_mismatch_on_the_same_bar_does_not_pair():
    b = bars()
    pairs, problems = pair_fills(
        [fill(2 * H, "buy", 1.0, 106.0, 0.1)], [fill(2 * H, "sell", 1.0, 106.0, 0.1)], b
    )
    assert pairs == [] and len(problems) == 2


def test_identical_gapless_results_agree():
    ours, theirs = gapless_pair()
    cmp = compare_gapless(ours, theirs, bars(), TICK)
    assert cmp.ok, [c for c in cmp.checks if not c.ok]
    assert cmp.numbers["fills_compared"] == 2


def test_a_price_two_ticks_off_fails_on_gapless_bars():
    ours, theirs = gapless_pair()
    theirs["fills"][1]["price"] += 2 * TICK
    cmp = compare_gapless(ours, theirs, bars(), TICK)
    assert not cmp.ok
    assert not by_name(cmp)["prices within one tick"].ok


def test_a_price_within_one_tick_agrees():
    ours, theirs = gapless_pair()
    theirs["fills"][1]["price"] += TICK
    assert by_name(compare_gapless(ours, theirs, bars(), TICK))["prices within one tick"].ok


def test_a_quantity_difference_fails_on_gapless_bars():
    ours, theirs = gapless_pair()
    theirs["fills"][0]["qty"] = 1.501
    assert not by_name(compare_gapless(ours, theirs, bars(), TICK))["same quantity"].ok


def test_fee_rounded_to_eight_decimals_is_explained_but_a_real_fee_difference_fails():
    ours, theirs = gapless_pair()
    ours["fills"][0]["fee"] = 0.1234567852
    theirs["fills"][0]["fee"] = 0.12345679  # Nautilus's Money keeps 8 decimals
    check = by_name(compare_gapless(ours, theirs, bars(), TICK))["fees"]
    assert check.ok and "explained" in check.note
    theirs["fills"][0]["fee"] = 0.1235
    assert not by_name(compare_gapless(ours, theirs, bars(), TICK))["fees"].ok


def test_funding_must_match_per_bar_and_by_when_it_was_charged():
    ours, theirs = gapless_pair()
    ours["funding"] = [{"bar": 3 * H, "at_open": 0.0159, "in_bar": 0.0}]
    theirs["funding"] = [{"bar": 3 * H, "at": "open", "payment": 0.0159}]
    assert by_name(compare_gapless(ours, theirs, bars(), TICK))["funding per bar"].ok
    # The same amount charged at the close instead of the open is a difference.
    theirs["funding"] = [{"bar": 3 * H, "at": "in_bar", "payment": 0.0159}]
    assert not by_name(compare_gapless(ours, theirs, bars(), TICK))["funding per bar"].ok


def test_final_equity_beyond_one_part_in_a_million_fails():
    ours, theirs = gapless_pair()
    theirs["equity"][-1]["equity"] *= 1 + 2e-6
    cmp = compare_gapless(ours, theirs, bars(), TICK)
    assert not by_name(cmp)["final equity"].ok
    theirs["equity"][-1]["equity"] = ours["equity"][-1]["equity"] * (1 + 5e-7)
    assert by_name(compare_gapless(ours, theirs, bars(), TICK))["final equity"].ok


def real_pair() -> tuple[dict, dict]:
    """The same two orders on real bars: we pay the next open (1.0 above the close for the buy,
    1.0 above for the sell too), Nautilus the decision bar's close."""
    ours = result(
        [fill(2 * H, "buy", 1.5, 106.0, 0.159), fill(4 * H, "sell", 1.5, 112.0, 0.168)],
        [10_000.0, 10_000.0, 9_999.0, 10_002.0, 10_007.7, 10_007.7],
        trades=[trade(2 * H, 4 * H, "long", 1.5, 106.0, 112.0, 0.327)],
    )
    theirs = result(
        [fill(2 * H, "buy", 1.5, 105.0, 0.1575), fill(4 * H, "sell", 1.5, 111.0, 0.1665)],
        [10_000.0, 10_000.0, 10_000.0, 10_003.0, 10_007.7, 10_007.7],
        trades=[trade(2 * H, 4 * H, "long", 1.5, 105.0, 111.0, 0.324)],
    )
    return ours, theirs


def test_real_bars_within_bound_one_agree():
    ours, theirs = real_pair()
    cmp = compare_real(ours, theirs, bars(), taker=0.001)
    assert cmp.ok, [c for c in cmp.checks if not c.ok]
    # 1.5 * 1.0 * 1.001 per fill
    assert abs(cmp.numbers["bound1"] - 2 * 1.5 * 1.001) < 1e-12


def test_a_real_price_difference_beyond_the_open_close_gap_fails():
    ours, theirs = real_pair()
    theirs["fills"][0]["price"] = 104.0  # 2.0 from ours, the gap is 1.0
    cmp = compare_real(ours, theirs, bars(), taker=0.001)
    assert not by_name(cmp)["each price difference within its bound"].ok


def test_a_final_equity_difference_beyond_the_bounds_fails():
    ours, theirs = real_pair()
    theirs["equity"][-1]["equity"] = ours["equity"][-1]["equity"] + 3.1  # bounds sum to 3.003
    cmp = compare_real(ours, theirs, bars(), taker=0.001)
    assert not cmp.ok
    assert not by_name(cmp)["final equity difference within the sum of the bounds"].ok


def test_a_rejection_only_one_engine_makes_fails():
    ours, theirs = real_pair()
    theirs["rejected_orders"] = [{"bar": 0, "reason": "below the minimum notional"}]
    assert not by_name(compare_real(ours, theirs, bars(), taker=0.001))["rejected orders"].ok


def test_a_six_decimal_commission_is_named_in_the_fee_note():
    ours, theirs = gapless_pair()
    ours["fills"][0]["fee"] = 0.1234564
    theirs["fills"][0]["fee"] = 0.123456
    theirs["fills"][0]["fee_precision"] = 6
    check = by_name(compare_gapless(ours, theirs, bars(), TICK))["fees"]
    assert check.ok and "the 6 decimals" in check.note


def no_fills() -> tuple[dict, dict]:
    """Neither engine fills anything (both rejected the only order): a flat, identical result."""
    ours = result([], [10.0] * 6)
    ours["rejected_orders"] = [{"bar": 0, "reason": "below the minimum notional"}]
    return ours, copy.deepcopy(ours)


def test_no_fills_on_either_side_fails_on_gapless_bars():
    ours, theirs = no_fills()
    cmp = compare_gapless(ours, theirs, bars(), TICK)
    assert not cmp.ok
    assert not by_name(cmp)["fills pair one to one"].ok


def test_no_fills_on_either_side_fails_on_real_bars():
    ours, theirs = no_fills()
    cmp = compare_real(ours, theirs, bars(), taker=0.001)
    assert not cmp.ok
    assert not by_name(cmp)["fills pair one to one"].ok


def test_a_perpetual_case_with_no_funding_charged_fails():
    ours, theirs = gapless_pair()
    # An empty funding series agrees bar by bar, but a perpetual case must have charged something.
    assert by_name(compare_gapless(ours, theirs, bars(), TICK))["funding per bar"].ok
    assert not by_name(compare_gapless(ours, theirs, bars(), TICK, perpetual=True))["funding per bar"].ok
    ours["funding"] = [{"bar": 3 * H, "at_open": 0.0159, "in_bar": 0.0}]
    theirs["funding"] = [{"bar": 3 * H, "at": "open", "payment": 0.0159}]
    assert by_name(compare_gapless(ours, theirs, bars(), TICK, perpetual=True))["funding per bar"].ok


# -- round-trip trades -----------------------------------------------------------------------------

TRADES_PAIR = "trades pair one to one"
TRADE_PNL = "trades: P&L net of fees, before funding"
REAL_TRADE_PNL = "trades: each P&L difference within its fills' bounds"


def test_matching_trades_agree_on_gapless_bars():
    ours, theirs = gapless_pair()
    cmp = compare_gapless(ours, theirs, bars(), TICK)
    checks = by_name(cmp)
    for name in (TRADES_PAIR, "trades: same quantity", "trades: entry and exit prices within one "
                 "tick", "trades: fees", TRADE_PNL, "trades: funding"):
        assert checks[name].ok, checks[name]
    assert cmp.numbers["trades_compared"] == 1


def test_a_trade_pnl_mismatch_fails_on_gapless_bars():
    ours, theirs = gapless_pair()
    # A trade that nets 0.001: Nautilus's money rounding (a few units of the 8th decimal) is beyond
    # 1e-6 relative there, and explained...
    for r in (ours, theirs):
        r["trades"][0]["gross_pnl"] = r["trades"][0]["fees"] + 0.001
    theirs["trades"][0]["gross_pnl"] += 3e-8
    check = by_name(compare_gapless(ours, theirs, bars(), TICK))[TRADE_PNL]
    assert check.ok and "rounding" in check.note
    # ...a real difference is not, even with the fills and the equity curve agreeing.
    theirs["trades"][0]["gross_pnl"] += 0.01
    cmp = compare_gapless(ours, theirs, bars(), TICK)
    assert not cmp.ok
    assert not by_name(cmp)[TRADE_PNL].ok
    assert by_name(cmp)["final equity"].ok


def test_a_trade_quantity_mismatch_fails_on_gapless_bars():
    ours, theirs = gapless_pair()
    theirs["trades"][0]["qty"] = 1.501
    assert not by_name(compare_gapless(ours, theirs, bars(), TICK))["trades: same quantity"].ok


def test_a_trade_price_beyond_one_tick_fails_on_gapless_bars():
    ours, theirs = gapless_pair()
    theirs["trades"][0]["exit_price"] += 2 * TICK
    check = by_name(compare_gapless(ours, theirs, bars(), TICK))[
        "trades: entry and exit prices within one tick"
    ]
    assert not check.ok


def test_a_trade_fee_mismatch_fails_on_gapless_bars():
    ours, theirs = gapless_pair()
    theirs["trades"][0]["fees"] += 1e-4
    assert not by_name(compare_gapless(ours, theirs, bars(), TICK))["trades: fees"].ok


def flip_pair() -> tuple[dict, dict]:
    """Long 1.5 decided on bar 1, then one sell of 3.0 decided on bar 3 that flips it to a short
    1.5 still open after the last bar (close 115). Both engines split the flip fill: 1.5 closes the
    long, 1.5 opens the short, and the fee 0.336 is shared half and half. Funding due at the flip
    bar's open (0.02) belongs to the long, held going into the bar; funding later in that bar
    (0.03) to the short."""
    fills = [fill(2 * H, "buy", 1.5, 106.0, 0.159), fill(4 * H, "sell", 3.0, 112.0, 0.336)]
    equity = [10_000.0] * 6
    ours = result(
        fills,
        equity,
        funding=[{"bar": 4 * H, "at_open": 0.02, "in_bar": 0.03}],
        funding_paid=0.05,
        trades=[
            trade(2 * H, 4 * H, "long", 1.5, 106.0, 112.0, 0.159 + 0.168, funding=0.02),
            trade(4 * H, 5 * H, "short", 1.5, 112.0, 115.0, 0.168, funding=0.03, open_=True),
        ],
    )
    theirs = copy.deepcopy(ours)
    theirs["funding"] = [
        {"bar": 4 * H, "at": "open", "payment": 0.02},
        {"bar": 4 * H, "at": "in_bar", "payment": 0.03},
    ]
    return ours, theirs


def test_a_flip_pairs_as_one_closed_trade_and_one_new_trade():
    ours, theirs = flip_pair()
    cmp = compare_gapless(ours, theirs, bars(), TICK, perpetual=True)
    assert cmp.ok, [c for c in cmp.checks if not c.ok]
    assert cmp.numbers["trades_compared"] == 2 and cmp.numbers["trades_open"] == 1
    pairs, problems = compare.pair_trades(ours["trades"], theirs["trades"], bars())
    assert problems == []
    assert [(p["bar"], p["exit_bar"], p["side"]) for p in pairs] == [(1, 3, "long"), (3, None, "short")]


def test_a_flip_left_unsplit_fails():
    ours, theirs = flip_pair()
    # One side reports the flip as the long closing with nothing opened.
    theirs["trades"] = theirs["trades"][:1]
    check = by_name(compare_gapless(ours, theirs, bars(), TICK, perpetual=True))[TRADES_PAIR]
    assert not check.ok
    assert "our short trade entered on bar 3 has no Nautilus partner" in check.note


def test_funding_given_to_the_wrong_side_of_a_flip_fails():
    ours, theirs = flip_pair()
    # The same total, with the funding due at the flip bar's open given to the new short.
    theirs["trades"][0]["funding"], theirs["trades"][1]["funding"] = 0.0, 0.05
    theirs["trades"][0]["funding_gross"], theirs["trades"][1]["funding_gross"] = 0.0, 0.05
    check = by_name(compare_gapless(ours, theirs, bars(), TICK, perpetual=True))["trades: funding"]
    assert not check.ok


def test_a_missing_trade_fails():
    ours, theirs = gapless_pair()
    theirs["trades"] = []
    cmp = compare_gapless(ours, theirs, bars(), TICK)
    assert not by_name(cmp)[TRADES_PAIR].ok
    assert not compare_real(ours, theirs, bars(), taker=0.001).ok


def test_a_trade_closed_on_another_bar_fails():
    ours, theirs = gapless_pair()
    theirs["trades"][0]["exit_time"] = 5 * H
    check = by_name(compare_gapless(ours, theirs, bars(), TICK))[TRADES_PAIR]
    assert not check.ok and "closes on bar 3 in ours, 4 in Nautilus" in check.note


def test_a_trade_open_at_the_end_on_one_side_only_fails():
    ours, theirs = flip_pair()
    # Nautilus closed the short at the last close instead of leaving it open.
    theirs["trades"][1]["open"] = False
    check = by_name(compare_gapless(ours, theirs, bars(), TICK, perpetual=True))[TRADES_PAIR]
    assert not check.ok and "open at the end only in ours" in check.note


def test_no_trades_on_a_case_with_fills_fails():
    ours, theirs = gapless_pair()
    ours["trades"], theirs["trades"] = [], []
    for cmp in (compare_gapless(ours, theirs, bars(), TICK), compare_real(ours, theirs, bars(), 0.001)):
        check = by_name(cmp)[TRADES_PAIR]
        assert not check.ok and "although the case has fills" in check.note


def test_real_trades_within_their_fills_bound_agree():
    ours, theirs = real_pair()
    cmp = compare_real(ours, theirs, bars(), taker=0.001)
    assert by_name(cmp)[REAL_TRADE_PNL].ok
    assert cmp.numbers["trades_compared"] == 1


def test_a_real_trade_pnl_beyond_its_bound_fails():
    ours, theirs = real_pair()
    # The bound is 1.5 * (1.0 + 1.0) * 1.001 = 3.003; the final equity bound is not affected.
    theirs["trades"][0]["gross_pnl"] += 3.1
    assert not by_name(compare_real(ours, theirs, bars(), taker=0.001))[REAL_TRADE_PNL].ok


def test_a_real_quantity_difference_is_scaled_out_but_not_a_price_one():
    ours, theirs = real_pair()
    # Nautilus sized 3.0 instead of 1.5 from its own equity: its P&L is twice ours, beyond the
    # bound of 3.003 unscaled, and agrees once scaled to our quantity.
    t = theirs["trades"][0]
    t["qty"], t["gross_pnl"], t["fees"] = 3.0, t["gross_pnl"] * 2, t["fees"] * 2
    assert abs(compare.pnl(t) - compare.pnl(ours["trades"][0])) > 3.003
    assert by_name(compare_real(ours, theirs, bars(), taker=0.001))[REAL_TRADE_PNL].ok
    # 7.0 at Nautilus's quantity is 3.5 at ours: beyond the bound.
    t["gross_pnl"] += 7.0
    assert not by_name(compare_real(ours, theirs, bars(), taker=0.001))[REAL_TRADE_PNL].ok


def test_a_real_trade_with_more_fills_than_the_scaling_allows_fails():
    ours, theirs = real_pair()
    theirs["trades"][0]["fills"] = 3
    check = by_name(compare_real(ours, theirs, bars(), taker=0.001))[REAL_TRADE_PNL]
    assert not check.ok and "3 Nautilus fills" in check.note


# -- resting orders: fill bars, ambiguous bars, bound 3, the stop-limit fee -------------------------
#
# bars() without a gap: bar 2 opens 106, high 109, low 105, close 108; bar 3 opens 109, high 112,
# low 108, close 111; bar 4 opens 112, high 115, low 111, close 114.

MAKER, TAKER = 0.0002, 0.0005


def order(oid: int, side: str, qty: float, kind: str, price=None, trigger=None, submitted=1,
          stop=None) -> dict:
    """One of our open orders as the export lists it before a bar; decided on bar `submitted`."""
    ot = {"type": kind}
    if price is not None:
        ot["price"] = price
    if trigger is not None:
        ot["trigger"] = trigger
    return {
        "id": oid,
        "request": {"side": side, "qty": qty, "order_type": ot, "reduce_only": False},
        "submitted_at": submitted * H,
        "attached_stop": None if stop is None else {"id": oid + 1, "trigger": stop},
        "status": {"status": "open"},
    }


def resting(bar: int, *orders: dict) -> dict:
    return {"bar": bar * H, "orders": list(orders)}


def amb(*snaps: dict) -> dict:
    return compare.ambiguous_bars(list(snaps), bars(), MAKER, TAKER)


def test_a_resting_fill_belongs_to_the_bar_it_fills_on():
    # A limit sell filling inside bar 3: ours stamped with bar 3's open, Nautilus with its close.
    ours = [fill(3 * H, "sell", 1.0, 111.0, 0.1, at_decision=False)]
    theirs = [fill(4 * H, "sell", 1.0, 111.0, 0.1)]
    pairs, problems = pair_fills(ours, theirs, bars())
    assert problems == [] and pairs[0]["bar"] == 3
    # Taken as a fill at the price of its decision, it would belong to bar 2 and not pair.
    ours[0]["at_decision"] = True
    assert pair_fills(ours, theirs, bars())[1] != []


def test_fills_pair_in_bar_order_whatever_order_they_happened_in():
    # Our limit's fill inside bar 3 is listed before the market fill at bar 3's open, which
    # belongs to bar 2.
    ours = [
        fill(3 * H, "sell", 1.0, 111.0, 0.1, at_decision=False),
        fill(3 * H, "buy", 1.0, 109.0, 0.1),
    ]
    theirs = [fill(3 * H, "buy", 1.0, 109.0, 0.1), fill(4 * H, "sell", 1.0, 111.0, 0.1)]
    pairs, problems = pair_fills(ours, theirs, bars())
    assert problems == []
    assert [(p["bar"], p["side"]) for p in pairs] == [(2, "buy"), (3, "sell")]


def test_a_stop_and_a_target_both_inside_the_range_make_the_bar_ambiguous():
    target = order(1, "sell", 2.0, "limit", price=111.0)
    stop = order(2, "sell", 1.0, "stop", trigger=108.5)
    found = amb(resting(3, target, stop))
    assert list(found) == [3]
    # Bound 2: the larger order's qty times the spread of the levels, plus both fees on the higher.
    assert abs(found[3]["bound"] - (2.0 * 2.5 + 2.0 * 111.0 * (MAKER + TAKER))) < 1e-12
    # One of them alone is not ambiguous, nor is a target the bar opens at (it fills first).
    assert amb(resting(3, target)) == {}
    assert amb(resting(3, order(1, "sell", 2.0, "limit", price=109.0), stop)) == {}
    # Nor one out of the range.
    assert amb(resting(3, order(1, "sell", 2.0, "limit", price=112.5), stop)) == {}


def test_a_buy_limit_and_a_buy_stop_on_both_sides_of_the_open_are_ambiguous():
    limit = order(1, "buy", 1.0, "limit", price=108.5)
    assert list(amb(resting(3, limit, order(3, "buy", 1.0, "stop", trigger=111.0)))) == [3]
    # A buy stop below the open fills at the open, so it is not reachable inside the range.
    assert amb(resting(3, limit, order(3, "buy", 1.0, "stop", trigger=108.8))) == {}


def test_a_stop_limit_counts_as_a_stop_until_triggered_then_as_a_limit():
    sell_stop = order(5, "sell", 1.0, "stop", trigger=108.5)
    # Decided on bar 1; bar 2's high 109 has not reached 110, so on bar 3 it is still a stop at 110.
    untriggered = order(1, "buy", 1.0, "stop_limit", price=110.5, trigger=110.0)
    assert list(amb(resting(3, untriggered, sell_stop))) == [3]
    # Trigger 108.6 was reached on bar 2: on bar 3 it is a limit buy at 108.7, below the open 109
    # and above the low 108, so reachable; at 110.5 it fills at the open.
    triggered = order(1, "buy", 1.0, "stop_limit", price=108.7, trigger=108.6)
    assert list(amb(resting(3, triggered, sell_stop))) == [3]
    triggered["request"]["order_type"]["price"] = 110.5
    assert amb(resting(3, triggered, sell_stop)) == {}


def test_an_entry_and_its_attached_stop_on_the_same_bar_are_ambiguous_with_a_target():
    entry = order(1, "buy", 1.0, "market", stop=108.5)
    target = order(3, "sell", 1.0, "limit", price=111.0)
    assert list(amb(resting(3, entry, target))) == [3]
    # A limit entry the bar never reaches never places its stop.
    unfilled = order(1, "buy", 1.0, "limit", price=107.0, stop=106.0)
    assert amb(resting(3, unfilled, target)) == {}


def bracket_pair(ours_exit: float, theirs_exit: float) -> tuple[dict, dict]:
    """Long 1.0 bought at the market (decided on bar 1, 106.0), with a stop at 108.5 and a target
    at 111.0 both inside bar 3. Ours exits at `ours_exit`, Nautilus at `theirs_exit`."""

    def side(exit_price: float, exit_time: int, liquidity: str, rate: float) -> dict:
        fee_in, fee_out = 106.0 * TAKER, exit_price * rate
        gross = exit_price - 106.0
        eq = [10_000.0, 10_000.0, 10_000.0 - fee_in] + [10_000.0 + gross - fee_in - fee_out] * 3
        return result(
            [
                fill(2 * H, "buy", 1.0, 106.0, fee_in, liquidity="taker"),
                fill(exit_time, "sell", 1.0, exit_price, fee_out, liquidity=liquidity),
            ],
            eq,
            trades=[trade(2 * H, exit_time, "long", 1.0, 106.0, exit_price, fee_in + fee_out)],
        )

    ours = side(ours_exit, 3 * H, "taker", TAKER)
    ours["fills"][1]["at_decision"] = False
    ours["fills"][1]["order_type"] = {"type": "stop", "trigger": 108.5}
    ours["resting"] = [
        resting(
            3,
            order(2, "sell", 1.0, "stop", trigger=108.5),
            order(3, "sell", 1.0, "limit", price=111.0),
        ),
    ]
    theirs = side(theirs_exit, 4 * H, "maker", MAKER)
    return ours, theirs


AMBIGUOUS_PNL = "ambiguous trades: our P&L at most Nautilus's"


def test_an_ambiguous_trade_with_our_pnl_below_nautilus_is_explained():
    ours, theirs = bracket_pair(108.5, 111.0)
    cmp = compare_gapless(ours, theirs, bars(), TICK, maker=MAKER, taker=TAKER)
    assert cmp.ok, [c for c in cmp.checks if not c.ok]
    assert cmp.numbers["ambiguous_fills"] == 1 and cmp.numbers["ambiguous_trades"] == 1
    # The difference is taken out of our equity from the exit bar on.
    explained = (108.5 - 108.5 * TAKER) - (111.0 - 111.0 * MAKER)
    assert abs(cmp.numbers["final_equity_explained"] - explained) < 1e-9


def test_an_ambiguous_trade_with_our_pnl_above_nautilus_fails():
    ours, theirs = bracket_pair(111.0, 108.5)
    cmp = compare_gapless(ours, theirs, bars(), TICK, maker=MAKER, taker=TAKER)
    assert not by_name(cmp)[AMBIGUOUS_PNL].ok
    assert not compare_real(ours, theirs, bars(), TAKER, maker=MAKER, tick=TICK).ok


def test_without_the_resting_orders_the_same_trade_fails_the_exact_comparison():
    ours, theirs = bracket_pair(108.5, 111.0)
    ours["resting"] = []
    names = by_name(compare_gapless(ours, theirs, bars(), TICK, maker=MAKER, taker=TAKER))
    assert not names["prices within one tick"].ok
    assert not names[TRADE_PNL].ok and not names["final equity"].ok


def test_an_ambiguous_entry_leaves_the_bars_to_its_exit_out_of_the_curve():
    ours, theirs = bracket_pair(108.5, 111.0)
    # The curves differ on bar 2, before the exit, by an amount the trade does not explain.
    theirs["equity"][2]["equity"] += 1.0
    assert not by_name(compare_gapless(ours, theirs, bars(), TICK, maker=MAKER, taker=TAKER))[
        "equity curve, every bar"
    ].ok
    # With the entry on an ambiguous bar (a limit entry and its attached stop, and another order,
    # all inside bar 1's range), bars 1 and 2 are not compared.
    for r, t in ((ours, 1 * H), (theirs, 2 * H)):
        r["fills"][0]["time"] = t
        r["trades"][0]["entry_time"] = t
    ours["fills"][0]["at_decision"] = False
    ours["fills"][0]["order_type"] = {"type": "limit", "price": 102.5}
    ours["resting"].append(
        resting(1, order(7, "buy", 1.0, "limit", price=102.5, submitted=0, stop=102.2),
                order(9, "buy", 1.0, "stop", trigger=104.0, submitted=0))
    )
    cmp = compare_gapless(ours, theirs, bars(), TICK, maker=MAKER, taker=TAKER)
    assert by_name(cmp)["equity curve, every bar"].ok, by_name(cmp)["equity curve, every bar"]


def test_ambiguous_trades_on_real_bars_add_bound_two():
    ours, theirs = bracket_pair(108.5, 111.0)
    cmp = compare_real(ours, theirs, bars(), TAKER, maker=MAKER, tick=TICK)
    assert cmp.ok, [c for c in cmp.checks if not c.ok]
    assert cmp.numbers["bound2"] > 2.5
    ours["resting"] = []
    assert not compare_real(ours, theirs, bars(), TAKER, maker=MAKER, tick=TICK).ok


def stop_limit_pair(limit: float = 110.5, their_fee=None) -> tuple[dict, dict]:
    """A stop-limit buy, trigger 110.0, limit `limit`, triggered inside bar 3 (high 112), filled at
    110.0: ours as a taker, Nautilus as a maker. Closed at the market, decided on bar 4."""
    fee_o = 110.0 * TAKER
    fee_t = 110.0 * MAKER if their_fee is None else their_fee
    fee_x = 115.0 * TAKER
    ot = {"type": "stop_limit", "price": limit, "trigger": 110.0}

    def side(fee_in: float, entry_time: int, liquidity: str) -> dict:
        net = 115.0 - 110.0 - fee_in - fee_x
        return result(
            [
                fill(entry_time, "buy", 1.0, 110.0, fee_in, liquidity=liquidity),
                fill(5 * H, "sell", 1.0, 115.0, fee_x, liquidity="taker"),
            ],
            [10_000.0] * 3 + [10_000.0 - fee_in] * 2 + [10_000.0 + net],
            trades=[trade(entry_time, 5 * H, "long", 1.0, 110.0, 115.0, fee_in + fee_x)],
        )

    ours = side(fee_o, 3 * H, "taker")
    ours["fills"][0].update(at_decision=False, order_type=ot)
    ours["resting"] = [resting(3, order(1, "buy", 1.0, "stop_limit", price=limit, trigger=110.0))]
    theirs = side(fee_t, 4 * H, "maker")
    return ours, theirs


def test_a_stop_limit_maker_fee_is_expected_difference_four():
    ours, theirs = stop_limit_pair()
    cmp = compare_gapless(ours, theirs, bars(), TICK, maker=MAKER, taker=TAKER)
    assert cmp.ok, [c for c in cmp.checks if not c.ok]
    assert cmp.numbers["stop_limit_maker_fills"] == 1
    assert "expected difference 4" in by_name(cmp)["fees"].note
    real = compare_real(ours, theirs, bars(), TAKER, maker=MAKER, tick=TICK)
    assert real.ok, [c for c in real.checks if not c.ok]
    assert abs(real.numbers["stop_limit_fee"] - 110.0 * (TAKER - MAKER)) < 1e-12


def test_a_stop_limit_maker_fee_off_the_maker_rate_fails():
    ours, theirs = stop_limit_pair(their_fee=110.0 * 0.0003)
    cmp = compare_gapless(ours, theirs, bars(), TICK, maker=MAKER, taker=TAKER)
    assert not by_name(cmp)["fees"].ok


def test_a_maker_fill_of_a_stop_limit_whose_limit_the_bar_never_passed_fails():
    # Limit 112.5 is above the high 112: Nautilus would have filled it as a taker, like ours.
    ours, theirs = stop_limit_pair(limit=112.5)
    cmp = compare_gapless(ours, theirs, bars(), TICK, maker=MAKER, taker=TAKER)
    assert cmp.numbers["stop_limit_maker_fills"] == 0
    assert not by_name(cmp)["fees"].ok


def through_pair(limit: float) -> tuple[dict, dict]:
    """real_pair with the exit a resting limit sell at `limit`, filled on bar 4 (open 112) at the
    limit in both engines."""
    ours, theirs = real_pair()
    for r, t in ((ours, 4 * H), (theirs, 5 * H)):
        r["fills"][1].update(time=t, price=limit, fee=1.5 * limit * 0.001)
        tr = r["trades"][0]
        tr.update(exit_time=t, exit_price=limit)
        tr["gross_pnl"] = 1.5 * (limit - tr["entry_price"])
        tr["fees"] = r["fills"][0]["fee"] + r["fills"][1]["fee"]
    ours["fills"][1].update(at_decision=False, order_type={"type": "limit", "price": limit})
    return ours, theirs


FINAL_REAL = "final equity difference within the sum of the bounds"


def test_a_limit_the_bar_opened_through_adds_bound_three():
    ours, theirs = through_pair(111.0)  # bar 4 opens 1.0 above it
    cmp = compare_real(ours, theirs, bars(), taker=0.001, tick=TICK)
    assert cmp.ok, [c for c in cmp.checks if not c.ok]
    assert abs(cmp.numbers["bound3"] - 1.5 * 1.0 * 1.001) < 1e-12
    assert abs(cmp.numbers["bound_sum"] - 2 * 1.5 * 1.001) < 1e-12
    # A final equity difference within bound 1 plus bound 3 passes, beyond it fails.
    theirs["equity"][-1]["equity"] = ours["equity"][-1]["equity"] + 3.0
    assert by_name(compare_real(ours, theirs, bars(), taker=0.001, tick=TICK))[FINAL_REAL].ok
    theirs["equity"][-1]["equity"] = ours["equity"][-1]["equity"] + 3.1
    assert not by_name(compare_real(ours, theirs, bars(), taker=0.001, tick=TICK))[FINAL_REAL].ok


def test_a_limit_inside_the_range_has_no_bound_three():
    ours, theirs = through_pair(112.5)  # inside bar 4's range, above the open
    theirs["equity"][-1]["equity"] = ours["equity"][-1]["equity"] + 3.0
    cmp = compare_real(ours, theirs, bars(), taker=0.001, tick=TICK)
    assert cmp.numbers["bound3"] == 0.0
    assert not by_name(cmp)[FINAL_REAL].ok


def test_a_resting_fill_beyond_a_tick_plus_its_bound_three_fails():
    ours, theirs = through_pair(111.0)
    theirs["fills"][1]["price"] = 111.0 + 1.0 + 2 * TICK
    cmp = compare_real(ours, theirs, bars(), taker=0.001, tick=TICK)
    assert not by_name(cmp)["each price difference within its bound"].ok


def test_our_trades_that_do_not_follow_from_our_fills_cannot_be_paired():
    ours, theirs = gapless_pair()
    ours["trades"][0]["entry_time"] = 3 * H
    check = by_name(compare_gapless(ours, theirs, bars(), TICK))[TRADES_PAIR]
    assert not check.ok and "do not follow from our fills" in check.note
