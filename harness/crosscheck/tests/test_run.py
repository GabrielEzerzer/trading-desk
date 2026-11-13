"""The report's failure labels (`run.failure_labels`) and the real bars' tick-rounding test, on
hand-made comparisons. Offline."""

from __future__ import annotations

import compare
import run
from compare import Bars, Check, Comparison

H = 3_600_000


def cmp(series: str, failing: list[str], **numbers) -> Comparison:
    return Comparison(series, [Check(name, False, "v", "l") for name in failing], numbers=numbers)


def label_of(lines: list[str], series: str, name: str) -> str:
    """The label of the line for check `name` in `series`."""
    for line in lines:
        if f"{series} bars, {name}:**" in line:
            return line.split(f", {series} bars")[0]
    raise AssertionError(f"no line for {series} {name}: {lines}")


ROUNDED = {"tick_rounded_fills": 3, "other_price_diffs": 0}
NOT_ROUNDED = {"tick_rounded_fills": 0, "other_price_diffs": 0}


def test_tick_rounding_labels_only_price_derived_checks_of_the_series_that_shows_it():
    gap = cmp("gapless", ["final equity", "rejected orders", "funding per bar",
                          "fills pair one to one (same bar and side)"], **ROUNDED)
    real = cmp("real", ["final equity difference within the sum of the bounds"], **NOT_ROUNDED)
    lines = run.failure_labels(gap, real)
    assert "cause known, our engine" in lines[0] and "3 on the gapless bars" in lines[0]
    assert "following from our engine's tick rounding" in label_of(lines, "gapless", "final equity")
    for name in ("rejected orders", "funding per bar", "fills pair one to one (same bar and side)"):
        assert label_of(lines, "gapless", name) == "- **Unexplained"
    # The real bars never showed the pattern, so their failure is not put on it.
    assert label_of(lines, "real", "final equity difference within the sum of the bounds") == (
        "- **Unexplained")


def test_tick_rounding_is_not_named_when_another_price_differs():
    gap = cmp("gapless", ["final equity"], tick_rounded_fills=3, other_price_diffs=1)
    lines = run.failure_labels(gap, cmp("real", [], **NOT_ROUNDED))
    assert label_of(lines, "gapless", "final equity") == "- **Unexplained"
    assert not any("cause known, our engine" in line for line in lines)


def test_the_moved_stop_limits_label_only_price_derived_checks():
    gap = cmp("gapless", [run.WALK, "equity curve, every bar",
                          "trades pair one to one (same entry and exit bar, side, open at the end)"],
              **NOT_ROUNDED)
    lines = run.failure_labels(gap, cmp("real", [], **NOT_ROUNDED))
    assert any("Fails, cause known, gapless bars" in line for line in lines)
    assert "following from the moved stop-limit fills" in label_of(
        lines, "gapless", "equity curve, every bar")
    assert label_of(
        lines, "gapless", "trades pair one to one (same entry and exit bar, side, open at the end)"
    ) == "- **Unexplained"


def test_real_tick_rounding_holds_our_price_to_our_model_rounded():
    # Bars with prices off the 0.1 tick. A buy decided on bar 0 fills at bar 1's open 101.05 in
    # our model, 101.1 after our rounding up; Nautilus at bar 0's close 100.95, within bound 1
    # (|101.05 - 100.95|) of the model but not of our rounded price.
    rows = [
        {"open_time": 0, "open": 100.0, "high": 101.0, "low": 99.0, "close": 100.95},
        {"open_time": H, "open": 101.05, "high": 102.0, "low": 100.0, "close": 101.5},
        {"open_time": 2 * H, "open": 101.5, "high": 102.0, "low": 101.0, "close": 101.5},
    ]
    b = Bars(rows, H)

    def pair(ours_price: float, theirs_price: float = 100.95) -> dict:
        p = {"bar": 0, "class": compare.EXACT,
             "ours": {"side": "buy", "qty": 1.0, "price": ours_price, "time": H},
             "theirs": {"side": "buy", "qty": 1.0, "price": theirs_price, "time": H}}
        p["allowance"] = compare.fill_allowance(p, b, {}, 0.0, 0.0, 0.1)
        return p

    assert compare.real_tick_rounding([pair(101.1)], b, 0.1, {}) == (1, 0)
    # Rounded the wrong way for a buy, or off the model by more than the rounding: other.
    assert compare.real_tick_rounding([pair(101.0)], b, 0.1, {}) == (0, 1)
    assert compare.real_tick_rounding([pair(101.3)], b, 0.1, {}) == (0, 1)
    # Ours is the rounding, but Nautilus's price is beyond the bound of the model's unrounded
    # price too: the rounding is not all there is to the failure.
    assert compare.real_tick_rounding([pair(101.1, 100.5)], b, 0.1, {}) == (0, 1)
    # At the model's own price, nothing to count.
    assert compare.real_tick_rounding([pair(101.05)], b, 0.1, {}) == (0, 0)


def test_compare_real_reports_the_tick_rounding():
    rows = [
        {"open_time": 0, "open": 100.0, "high": 101.0, "low": 99.0, "close": 101.05},
        {"open_time": H, "open": 101.05, "high": 102.0, "low": 100.0, "close": 101.5},
        {"open_time": 2 * H, "open": 101.5, "high": 102.0, "low": 101.0, "close": 101.5},
    ]
    b = Bars(rows, H)
    fill = {"side": "buy", "qty": 1.0, "fee": 0.0, "fee_precision": 8}
    ours = {"fills": [dict(fill, time=H, price=101.1)], "trades": [], "funding": [],
            "funding_paid": 0.0, "rejected_orders": [],
            "equity": [{"time": i * H, "equity": 100.0} for i in range(3)]}
    theirs = dict(ours, fills=[dict(fill, time=H, price=101.05)])
    n = compare.compare_real(ours, theirs, b, 0.0, tick=0.1).numbers
    assert (n["tick_rounded_fills"], n["other_price_diffs"]) == (1, 0)
