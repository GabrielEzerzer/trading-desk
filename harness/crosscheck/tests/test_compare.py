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


def fill(time: int, side: str, qty: float, price: float, fee: float) -> dict:
    return {"time": time, "side": side, "qty": qty, "price": price, "fee": fee, "fee_precision": 8}


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
    assert problems == ["our sell fill decided on bar 3 has no Nautilus partner"]


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


def test_ambiguous_bars_are_left_out_of_the_exact_comparison():
    ours, theirs = gapless_pair()
    theirs["fills"][1]["price"] += 5.0  # the sell decided on bar 3
    assert not compare_gapless(ours, theirs, bars(), TICK).ok
    cmp = compare_gapless(ours, theirs, bars(), TICK, ambiguous_bars=frozenset({3}))
    assert by_name(cmp)["prices within one tick"].ok
    assert cmp.numbers["fills_compared"] == 1


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
    assert not by_name(cmp)["each price difference within |open[N+1] - close[N]|"].ok


def test_a_final_equity_difference_beyond_the_bounds_fails():
    ours, theirs = real_pair()
    theirs["equity"][-1]["equity"] = ours["equity"][-1]["equity"] + 3.1  # bounds sum to 3.003
    cmp = compare_real(ours, theirs, bars(), taker=0.001)
    assert not cmp.ok
    assert not by_name(cmp)["final equity difference within the sum of the bounds"].ok
    # Lane 2's bounds widen the limit.
    assert compare_real(ours, theirs, bars(), taker=0.001, bound3=0.2).ok


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
REAL_TRADE_PNL = "trades: each P&L difference within its fills' bound 1"
NET_PNL = "trades: net P&L is gross P&L - fees - funding"


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


def test_a_wrong_exported_net_pnl_fails_on_both_series():
    # The P&L checks use gross_pnl - fees, so only this check sees a net_pnl that is off.
    for pair, cmp_of in (
        (gapless_pair, lambda o, t: compare_gapless(o, t, bars(), TICK)),
        (real_pair, lambda o, t: compare_real(o, t, bars(), taker=0.001)),
    ):
        ours, theirs = pair()
        assert by_name(cmp_of(ours, theirs))[NET_PNL].ok
        ours["trades"][0]["net_pnl"] += 1000.0
        cmp = cmp_of(ours, theirs)
        assert not cmp.ok
        assert not by_name(cmp)[NET_PNL].ok and "our trade" in by_name(cmp)[NET_PNL].note


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
