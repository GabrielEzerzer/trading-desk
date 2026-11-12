"""The exercise counts of a case and the verdict they give, on hand-made bars. Offline."""

from __future__ import annotations

import pytest

import exercises
import run
from compare import Comparison
from test_compare import H, bars, fill


def make_run(pairs=(), fills=(), funding_times=(), funding=(), exercises_=(), trades=(),
             gap_at=3, kind="perpetual") -> exercises.Run:
    """bars(gap_at=3): bars at 0, 1, 2, 4 and 5 h, so the bar at index 3 (4 h) is the first after
    a dropped bucket; bar 2 closes 108, high 109, and bar 3 opens 111."""
    b = bars(gap_at=gap_at)
    return exercises.Run(
        case={"name": "t", "kind": kind, "params": {}, "exercises": list(exercises_)},
        meta={"dropped_buckets": [3 * H] if gap_at is not None else []},
        funding_times=list(funding_times),
        bars=b,
        real=Comparison("real", pairs=list(pairs)),
        ours={"fills": list(fills), "funding": list(funding), "trades": list(trades), "orders": []},
        theirs={},
        tick=0.1,
    )


def resting_pair(bar: int, side: str, kind: str, level: float) -> dict:
    ot = {"type": kind, ("price" if kind == "limit" else "trigger"): level}
    ours = fill(0, side, 1.0, level, 0.0, at_decision=False, order_type=ot)
    return {"bar": bar, "side": side, "ours": ours, "theirs": dict(ours)}


def counted(run_: exercises.Run, ex: str) -> int:
    return exercises.EXERCISES[ex][1](run_)[0]


def test_a_stop_the_bar_after_dropped_buckets_opened_past_is_through_a_gap():
    # A buy stop at 110: bar 2's high 109 never reached it, bar 3 opened at 111 past it.
    r = make_run(pairs=[resting_pair(3, "buy", "stop", 110.0)])
    assert counted(r, "stop_through_gap") == 1
    assert counted(r, "stop_through_dropped_bucket") == 1
    # Without the dropped bucket the bars run on with no gap: bar 3 opens at 109 and the stop at
    # 108.5, which bar 2's high 109 already reached, is a stop inside the range, not a gap.
    r = make_run(pairs=[resting_pair(3, "buy", "stop", 108.5)], gap_at=None)
    assert counted(r, "stop_through_gap") == 0
    # A stop at 109.5 bar 2 never reached and bar 3 (open 109) reached inside its range only.
    r = make_run(pairs=[resting_pair(3, "buy", "stop", 109.5)], gap_at=None)
    assert counted(r, "stop_through_gap") == 0


def test_a_gap_through_a_stop_away_from_dropped_buckets_counts_only_as_a_gap():
    # bars() without a dropped bucket opens each bar 1.0 above the last close: a buy stop at
    # 108.5 is past bar 2's close 108 and below bar 3's open 109; bar 2's high 109 reached it,
    # so take bar 2's own: a stop at 105.5 between bar 1's close 105 and bar 2's open 106, with
    # bar 1's high 106 reaching it. Then a stop at 105.2 on bar 2 with bar 1's high lowered.
    r = make_run(pairs=[resting_pair(2, "buy", "stop", 105.5)], gap_at=None)
    r.bars.bars[1]["high"] = 105.2
    assert counted(r, "stop_through_gap") == 1
    assert counted(r, "stop_through_dropped_bucket") == 0


def test_a_limit_the_bar_after_dropped_buckets_opened_through_is_counted():
    r = make_run(pairs=[resting_pair(3, "sell", "limit", 110.0)])
    assert counted(r, "limit_through_gap") == 1
    assert counted(r, "limit_through_dropped_bucket") == 1
    # A sell limit at 112, inside bar 3's range above its open: no gap.
    r = make_run(pairs=[resting_pair(3, "sell", "limit", 112.0)])
    assert counted(r, "limit_through_gap") == 0


def test_funding_in_a_gap_is_a_stamp_no_bar_covers():
    funding = [{"bar": 4 * H, "at_open": 0.5, "in_bar": 0.0}]
    r = make_run(funding_times=[2 * H, 3 * H, 3 * H + H // 2], funding=funding)
    n, detail = exercises.EXERCISES["funding_in_gap"][1](r)
    assert n == 2 and "2 charged on an open position" in detail
    # A stamp at a bar's open, or inside a bar (a timeframe longer than the funding interval), is
    # covered by that bar.
    assert counted(make_run(funding_times=[2 * H, 2 * H + H // 2], funding=funding),
                   "funding_in_gap") == 0


def test_an_attached_stop_filled_on_its_entry_bar_is_counted():
    stop = {"type": "stop", "trigger": 105.5}
    fills = [fill(2 * H, "buy", 1.0, 106.0, 0.0),
             fill(2 * H, "sell", 1.0, 105.5, 0.0, at_decision=False, order_type=stop)]
    assert counted(make_run(fills=fills), "stop_on_entry_bar") == 1
    # The stop a bar later is not on the entry bar.
    fills[1]["time"] = 4 * H
    assert counted(make_run(fills=fills), "stop_on_entry_bar") == 0


def test_dropped_buckets_are_counted_with_the_move_across():
    n, detail = exercises.EXERCISES["dropped_buckets"][1](make_run())
    assert n == 1 and "largest move across a gap 3" in detail
    assert counted(make_run(gap_at=None), "dropped_buckets") == 0


def test_a_case_with_an_exercise_never_seen_is_not_exercised_not_pass():
    counts = exercises.count(make_run(exercises_=["dropped_buckets", "funding_in_gap"]))
    assert [(c["id"], c["count"]) for c in counts] == [("dropped_buckets", 1), ("funding_in_gap", 0)]
    assert exercises.not_exercised(counts) == ["funding_in_gap"]
    assert run.case_verdict(True, counts) == "NOT EXERCISED"
    assert run.case_verdict(False, counts) == "FAIL"
    assert run.case_verdict(True, counts[:1]) == "PASS"


def test_an_unknown_exercise_is_an_error_not_a_zero():
    with pytest.raises(KeyError):
        exercises.count(make_run(exercises_=["stops_on_gaps"]))


def test_a_touch_fill_is_a_limit_the_bar_reached_inside_its_range():
    # bars() without a gap: bar 3 opens 109, low 108. A buy limit at 108 is reached at the low
    # exactly; one at 108.5 inside the range; one at 109.5 the bar opened through, not a touch.
    pairs = [resting_pair(3, "buy", "limit", 108.0), resting_pair(3, "buy", "limit", 108.5),
             resting_pair(3, "buy", "limit", 109.5)]
    n, detail = exercises.EXERCISES["touch_fills"][1](make_run(pairs=pairs, gap_at=None))
    assert n == 2 and detail.startswith("1 where the bar's extreme was the limit exactly")
