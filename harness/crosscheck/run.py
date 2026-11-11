"""Runs the cross-check: `uv run python run.py [--case case01 ...] [--fetch]`.

For each case it fetches the data if the export is missing (or with `--fetch`), runs our engine and
NautilusTrader on the real and the gapless bars, measures both equity curves with our metrics,
applies the agreement rule (`compare.py`) and writes `reports/<case>.md`. Outputs go to `out/`.
The exit code is 1 if any case is not PASS (FAIL, or NOT EXERCISED).
"""

from __future__ import annotations

import argparse
import json
import subprocess
import sys
from pathlib import Path

import compare
import exercises
import nautilus_side

HERE = Path(__file__).resolve().parent
REPO = HERE.parent.parent
CASES = HERE / "cases"
OUT = HERE / "out"
REPORTS = HERE / "reports"


def build_example() -> Path:
    """Builds our side (`crates/td-engine/examples/crosscheck.rs`) and returns its executable."""
    proc = subprocess.run(
        ["cargo", "build", "-p", "td-engine", "--example", "crosscheck", "--message-format=json"],
        cwd=REPO,
        capture_output=True,
        text=True,
    )
    if proc.returncode != 0:
        sys.exit(f"cargo build failed:\n{proc.stderr}")
    for line in proc.stdout.splitlines():
        msg = json.loads(line)
        if msg.get("reason") == "compiler-artifact" and msg["target"]["name"] == "crosscheck":
            return Path(msg["executable"])
    sys.exit("cargo build did not report the crosscheck executable")


def ours(exe: Path, *args: str) -> None:
    proc = subprocess.run([str(exe), *args], cwd=HERE, capture_output=True, text=True)
    if proc.returncode != 0:
        sys.exit(f"crosscheck {' '.join(args)} failed:\n{proc.stderr}")


def metrics(exe: Path, result: dict, case: dict, path: Path) -> dict:
    curve = {
        "starting_cash": case["starting_cash"],
        "timeframe": case["timeframe"],
        "equity": [{"time": p["time"], "equity": p["equity"]} for p in result["equity"]],
    }
    curve_path = path.with_suffix(".curve.json")
    curve_path.write_text(json.dumps(curve))
    ours(exe, "metrics", str(curve_path), str(path))
    return json.loads(path.read_text())


def run_case(exe: Path, case_path: Path, refetch: bool) -> bool:
    case = json.loads(case_path.read_text())
    name = case["name"]
    data = HERE / case["data"]
    if refetch or not (data / "meta.json").exists():
        print(f"{name}: fetching data", flush=True)
        ours(exe, "fetch", str(case_path), str(HERE))
    meta = json.loads((data / "meta.json").read_text())
    inst = json.loads((data / "instrument.json").read_text())
    out = OUT / name
    out.mkdir(parents=True, exist_ok=True)
    tf_ms = nautilus_side.TIMEFRAME_MS[case["timeframe"]]

    results = {}
    for series in ("gapless", "real"):
        path = out / f"ours_{series}.json"
        ours(exe, "run", str(case_path), str(HERE), series, str(path))
        mine = json.loads(path.read_text())
        theirs = nautilus_side.run_case(case, HERE, series)
        (out / f"nautilus_{series}.json").write_text(json.dumps(theirs, indent=1))
        bars = compare.Bars(nautilus_side.read_bars(data / f"bars_{series}.csv"), tf_ms)
        maker, taker = case["fees"]["maker"], case["fees"]["taker"]
        if series == "gapless":
            cmp = compare.compare_gapless(
                mine, theirs, bars, inst["tick_size"], maker=maker, taker=taker,
                perpetual=case["kind"] == "perpetual",
            )
        else:
            cmp = compare.compare_real(
                mine, theirs, bars, taker, maker=maker, tick=inst["tick_size"]
            )
        m_ours = metrics(exe, mine, case, out / f"metrics_ours_{series}.json")
        m_theirs = metrics(exe, theirs, case, out / f"metrics_nautilus_{series}.json")
        results[series] = (cmp, mine, theirs, m_ours, m_theirs)

    real_cmp, real_ours, real_theirs = results["real"][:3]
    counts = exercises.count(exercises.Run(
        case=case,
        meta=meta,
        funding_times=[t for t, _ in nautilus_side.read_funding(data / "funding.csv")],
        bars=compare.Bars(nautilus_side.read_bars(data / "bars_real.csv"), tf_ms),
        real=real_cmp,
        ours=real_ours,
        theirs=real_theirs,
        tick=inst["tick_size"],
    ))
    verdict = case_verdict(all(r[0].ok for r in results.values()), counts)
    missing = exercises.not_exercised(counts)
    REPORTS.mkdir(exist_ok=True)
    (REPORTS / f"{name}.md").write_text(
        report(case, meta, inst, results, verdict, counts), encoding="utf-8"
    )
    g, r = results["gapless"][0].numbers, results["real"][0].numbers
    print(
        f"{name}: {verdict}"
        + (f" ({', '.join(missing)})" if missing else "")
        + f"  exercises: {', '.join(f'{c['id']} {c['count']}' for c in counts)}  "
        f"gapless: {g['fills_compared']} fills, max price diff "
        f"{g['max_price_diff']:.6g}, final equity rel {g['final_equity_rel']:.2e}, "
        f"{g['trades_compared']} trades, max P&L diff {g['max_trade_pnl_abs']:.2e}  real: "
        f"{r['fills_compared']} fills, equity diff {r['final_equity_diff']:.4f} vs bound "
        f"{r['bound_sum']:.4f}, {r['trades_compared']} trades, max P&L diff "
        f"{r['max_trade_pnl_diff']:.4f} ({r['max_trade_bound_ratio']:.6g} of its bound)",
        flush=True,
    )
    return verdict == "PASS"


def case_verdict(agree: bool, counts: list[dict]) -> str:
    """FAIL when a check fails; else NOT EXERCISED when an exercise of the case's row has no
    instance; else PASS."""
    if not agree:
        return "FAIL"
    return "NOT EXERCISED" if exercises.not_exercised(counts) else "PASS"


def fmt(v) -> str:
    if v is None:
        return "none"
    if isinstance(v, float):
        return f"{v:.6g}"
    return str(v)


def checks_table(cmp: compare.Comparison) -> str:
    lines = ["| Check | Result | Value | Limit | Note |", "|---|---|---|---|---|"]
    for c in cmp.checks:
        lines.append(
            f"| {c.name} | {'agree' if c.ok else 'FAIL'} | {c.value} | {c.limit} | {c.note} |"
        )
    return "\n".join(lines)


def metrics_table(m_ours: dict, m_theirs: dict) -> str:
    lines = ["| Metric | Ours | Nautilus | Difference |", "|---|---|---|---|"]
    for k in m_ours:
        a, b = m_ours[k], m_theirs.get(k)
        d = "" if not isinstance(a, (int, float)) or not isinstance(b, (int, float)) else fmt(a - b)
        lines.append(f"| {k} | {fmt(a)} | {fmt(b)} | {d} |")
    return "\n".join(lines)


def exercises_table(case: dict, counts: list[dict]) -> str:
    lines = [
        f"What the case's row in issue #29 exercises ({case['about']}), counted on the real bars. "
        "A case with no instance of one of them is NOT EXERCISED, not PASS.",
        "",
        "| Exercise | Counted | Instances | Where |",
        "|---|---|---|---|",
    ]
    for c in counts:
        n = str(c["count"]) if c["count"] else "**0: not exercised**"
        lines.append(f"| `{c['id']}` | {c['what']} | {n} | {c['detail']} |")
    note = case.get("exercise_note")
    if note:
        lines += ["", note]
    return "\n".join(lines)


def report(case: dict, meta: dict, inst: dict, results: dict, verdict: str,
           counts: list[dict]) -> str:
    gap_cmp, gap_ours, gap_theirs, gm_o, gm_t = results["gapless"]
    real_cmp, real_ours, real_theirs, rm_o, rm_t = results["real"]
    g, r = gap_cmp.numbers, real_cmp.numbers
    dropped = meta["dropped_buckets"]
    parts = [
        f"# {case['name']}: {case['title']}",
        "",
        f"**Verdict: {verdict}.**"
        + (f" Not exercised: {', '.join(exercises.not_exercised(counts))}."
           if exercises.not_exercised(counts) else ""),
        "",
        "Generated by `harness/crosscheck/run.py`; do not edit by hand.",
        "",
        "## Exercises",
        "",
        exercises_table(case, counts),
        "",
        "## Setup",
        "",
        f"- Strategy: `{case['strategy']}` with `{json.dumps(case.get('params', {}))}`.",
        f"- Instrument: {meta['instrument']}, tick {inst['tick_size']:g}, lot {inst['lot_size']:g}, "
        f"min qty {inst['min_qty']:g}, min notional {inst['min_notional']:g}.",
        f"- Fees: maker {case['fees']['maker']:g}, taker {case['fees']['taker']:g}. Slippage 0 in "
        "both engines. Starting cash "
        f"{case['starting_cash']:g} USDT. "
        + (
            "Cash account in Nautilus."
            if case["kind"] == "spot"
            else f"Max leverage {case.get('max_leverage', 1.0):g} in ours; a margin account at "
            "10x in Nautilus, which the strategy never comes near."
        ),
        f"- Bars: {meta['bars']} {case['timeframe']} bars from {case['from']} to {case['to']} "
        f"(exclusive), resampled by td-data from the venue's 1-minute candles. "
        f"{len(dropped)} bucket(s) dropped as incomplete"
        + (f" (open times {', '.join(str(t) for t in dropped)})" if dropped else "")
        + f", {len(meta['tolerated_buckets'])} built with a tolerated minute or two missing. "
        f"{meta['bar_prices_off_the_current_tick']} bar prices off the current tick.",
        f"- Funding: {meta['funding_stamps']} stamps, "
        f"{len(meta['funding_stamps_moved_by_snapping'])} moved by snapping to the bar boundary."
        + funding_summary(gap_theirs),
        "",
        "## Gapless bars",
        "",
        "Every open set to the previous close, so a market order filled at the next open (ours) and "
        "one filled at the decision bar's close (Nautilus) get the same price. Everything must match, "
        "outside the ambiguous bars (two or more resting orders reachable inside one bar's range): "
        "a fill on one must still pair, ours at the worse listed level for its side within a tick, "
        "and each trade entered or closed on one must have our P&L at most Nautilus's; its P&L "
        "difference is taken out of our equity from its exit on. A stop-limit triggered inside a "
        "bar that runs past its limit fills at the trigger in ours, as a taker, and is rested by "
        "Nautilus's bar walk, which fills it as a maker at the first later point of the walk where "
        "the limit is marketable, no better than the trigger: usually the trigger, sometimes the "
        "close (expected difference 4). Where that lands on the same bar and our fill follows our "
        "own fill model, Nautilus's fill is held to the walk's price within a tick and the maker "
        "fee, and the walk's price and fee against our model's are taken out of our trade P&L and "
        "equity. "
        "Fills pair by the bar Nautilus makes them on (the decision bar of a market order, the bar a "
        "resting order fills on). Round-trip trades pair by the bar of their entry fill and their "
        "side, and must close on the same bar or both be open at the end; then the same quantity, entry "
        "and exit prices within one tick, fees within 1e-9 relative, P&L net of fees and before "
        "funding within 1e-6 relative, and funding within 1e-9 of the size of its charges. Fees and "
        "P&L may also differ by up to four units of the 8th decimal, Nautilus's rounding of money.",
        "",
        checks_table(gap_cmp),
        "",
        "Metrics from both equity curves, with our formulas (`crosscheck metrics`):",
        "",
        metrics_table(gm_o, gm_t),
        "",
        "## Real bars",
        "",
        "Nautilus fills a market order at the close of the bar it was decided on, our engine at the "
        "next bar's open (expected difference 1). Fills must pair one to one. A fill at the price of "
        "its decision must be within `|open[N+1] - close[N]|` of Nautilus's; a resting order's fill "
        "within one tick plus how far the bar opened through its level; a fill on an ambiguous bar "
        "only to the worse listed level for its side. The final equity difference must stay within "
        "the sum of the bounds: bound 1, `qty * |open[N+1] - close[N]| * (1 + taker)` per fill at "
        "the decision price; bound 2, per ambiguous bar, `qty * (highest - lowest level) + qty * "
        "highest * fee spread`, the fee spread `max(|maker|, |taker|)` for rates of the same sign and `|maker| + |taker|` "
        "otherwise; bound 3, `qty * |open - level| * (1 + taker)` per resting fill the bar opened "
        "through; and the stop-limit walk, `qty * (|walk price - model price| + the dearer * "
        "|model rate - maker|)` per fill held as expected difference 4. Trades must pair one to "
        "one as on the gapless bars, and each trade's P&L (net of fees, before funding) "
        "must stay within the bounds of its fills at our quantity, after Nautilus's P&L is scaled "
        "to our quantity to remove the sizing difference.",
        "",
        checks_table(real_cmp),
        "",
        f"Bound 1 total {r['bound1']:.6f}, bound 2 {r['bound2']:.6f} over {r['ambiguous_bars']} "
        f"ambiguous bar(s), bound 3 {r['bound3']:.6f} over {r['through_fills']} fill(s) the bar "
        f"opened through, stop-limit walk {r['stop_limit_fee']:.6f} over "
        f"{r['stop_limit_maker_fills']} fill(s); largest relative quantity difference between paired fills "
        f"{r['max_qty_rel']:.2e} (sizing reads each engine's own equity, which differs by the earlier "
        "fill price differences). Funding total: ours "
        f"{r['funding_total_ours']:.6f}, Nautilus {r['funding_total_nautilus']:.6f}.",
        "",
        metrics_table(rm_o, rm_t),
        "",
        "## Differences and their reasons",
        "",
    ]
    parts += differences(case, results)
    parts += [
        "",
        "## Numbers",
        "",
        f"- Gapless: {g['fills_compared']} fills compared, max price difference "
        f"{g['max_price_diff']:.6g}, final equity {g['final_equity_ours']:.8f} (ours) vs "
        f"{g['final_equity_nautilus']:.8f} (Nautilus), relative {g['final_equity_rel']:.2e} after "
        f"{g['final_equity_explained']:.6g} of explained differences.",
        f"- Gapless ambiguous bars: {g['ambiguous_bars']}, with {g['ambiguous_fills']} fill(s) and "
        f"{g['ambiguous_trades']} trade(s) on them; ours up to {g['max_ambiguous_shortfall']:.6g} "
        f"below Nautilus's P&L. Stop-limit maker fills: {g['stop_limit_maker_fills']}.",
        f"- Gapless trades: {g['trades_compared']} compared ({g['trades_open']} open at the end), "
        f"max entry or exit price difference {g['max_trade_price_diff']:.6g}, max fee difference "
        f"{g['max_trade_fee_abs']:.2e}, max P&L difference {g['max_trade_pnl_abs']:.2e} (relative "
        f"{g['max_trade_pnl_rel']:.2e}), max funding difference {g['max_trade_funding_rel']:.2e} "
        "of its charges' size.",
        f"- Real: {r['fills_compared']} fills compared, max price difference "
        f"{r['max_price_diff']:.6g}, final equity difference {r['final_equity_diff']:.6f} against a "
        f"bound of {r['bound_sum']:.6f}.",
        f"- Real trades: {r['trades_compared']} compared ({r['trades_open']} open at the end), max "
        f"P&L difference {r['max_trade_pnl_diff']:.6f} with Nautilus scaled to our quantity, at "
        f"most {r['max_trade_bound_ratio']:.8g} of its bound; quantities differ by up to "
        f"{r['max_trade_qty_rel']:.2e} relative.",
        "",
    ]
    return "\n".join(parts)


def funding_summary(theirs: dict) -> str:
    charges = theirs["funding"]
    if not charges:
        return ""
    paid = sum(1 for f in charges if f["payment"] > 0)
    received = sum(1 for f in charges if f["payment"] < 0)
    negative = sum(1 for f in charges if f["rate"] < 0)
    return (
        f" On the gapless bars the position paid funding at {paid} stamps and received it at "
        f"{received} ({negative} stamps had a negative rate)."
    )


def differences(case: dict, results: dict) -> list[str]:
    """The differences seen, each with its reason; anything not covered here fails the case."""
    out = []
    gap_cmp = results["gapless"][0]
    for c in gap_cmp.checks:
        if c.note and c.ok:
            out.append(f"- Gapless, {c.name}: {c.note}.")
    real_cmp = results["real"][0]
    r = real_cmp.numbers
    g = gap_cmp.numbers
    if g["ambiguous_trades"] or r["ambiguous_trades"]:
        out.append(
            f"- Ambiguous bars (expected difference 2): {g['ambiguous_bars']} on the gapless bars "
            f"and {r['ambiguous_bars']} on the real bars, where two or more resting orders are "
            "reachable inside the range. Our engine fills the stop first; Nautilus walks the bar "
            "open, high, low, close and may fill the other order. Our P&L on those trades is up to "
            f"{max(g['max_ambiguous_shortfall'], r['max_ambiguous_shortfall']):.6g} below "
            "Nautilus's and never above it."
        )
    if r["max_price_diff"] > 0:
        out.append(
            f"- Real bars, fill prices: up to {r['max_price_diff']:.6g} apart, each within its "
            "bound: a market fill within its bar's `|open[N+1] - close[N]|` (expected difference "
            "1: Nautilus fills at the decision bar's close, we fill at the next open), a resting "
            "fill within one tick plus how far the bar opened through its level (expected "
            "difference 3)."
        )
    if r["max_qty_rel"] > 0:
        out.append(
            "- Real bars, quantities: paired fills differ by up to "
            f"{r['max_qty_rel']:.2e} relative, because each engine sizes from its own equity, which "
            "already differs by the earlier fill price differences; where a size lands near a lot "
            "boundary the rounding down to the lot moves it by a whole lot."
        )
    if r["trades_compared"] and r["max_trade_qty_rel"] > 0:
        out.append(
            "- Real bars, trades: paired trades differ in quantity by up to "
            f"{r['max_trade_qty_rel']:.2e} relative, for the sizing reason above, so their P&L also "
            "differs by the quantity difference times the price move. Nautilus's P&L is scaled to "
            "our quantity before it is held to the bound 1 of the trade's fills; scaled, the "
            f"largest difference is {r['max_trade_pnl_diff']:.6f}."
        )
    if r["trades_compared"] and r["max_trade_bound_ratio"] > 0:
        out.append(
            "- Real bars, trade P&L: the largest difference reaches "
            f"{r['max_trade_bound_ratio']:.8g} of its trade's bound. Each fill's price differs by "
            "the whole `|open[N+1] - close[N]|` (a fill at the open against one at the close), so "
            "a trade whose entry and exit differences both move its P&L the same way meets its "
            "bound exactly; anything above 1 is within four units of the 8th decimal, Nautilus's "
            "rounding of money."
        )
    if compare.rel(r["funding_total_ours"], r["funding_total_nautilus"]) > compare.FUNDING_REL:
        out.append(
            "- Real bars, funding: totals differ by "
            f"{abs(r['funding_total_ours'] - r['funding_total_nautilus']):.6f}, from the position "
            "sizes above (same stamps, rates and prices)."
        )
    out += failure_labels(gap_cmp, real_cmp)
    if not out:
        out.append("- None: the two engines agree exactly within the tolerances.")
    return out


# The checks whose values follow from the fill prices: the prices themselves, the quantities each
# engine sizes from its equity, the fees on them, the trades' amounts and the equity. Only these
# can follow from a cause that moves prices; any other failing check (pairing, rejections, funding,
# the order of an ambiguous bar's fills) stays unexplained until it is explained on its own.
PRICE_DERIVED = {
    "prices within one tick",
    "same quantity",
    "fees",
    "trades: same quantity",
    "trades: entry and exit prices within one tick",
    "trades: fees",
    "trades: P&L net of fees, before funding",
    "equity curve, every bar",
    "final equity",
    "each price difference within its bound",
    "trades: each P&L difference within its fills' bounds",
    "ambiguous trades: our P&L at most Nautilus's",
    "final equity difference within the sum of the bounds",
}
WALK = "stop-limit fills on the same bar in both engines"


def failure_labels(gap_cmp: compare.Comparison, real_cmp: compare.Comparison) -> list[str]:
    """One line per failing check, each with its cause where one is known for that series and the
    check follows from it, else "Unexplained".

    Two causes are known. Our broker's rounding of off-tick bar prices to the current tick: named
    for a series when its fills test for it (`tick_rounding` on the gapless bars,
    `real_tick_rounding` on the real bars), that is when some fill prices differ by the rounding
    and none differs otherwise. Nautilus's bar walk moving a stop-limit fill to a later bar: named
    by the walk check itself. Either labels only the series' price-derived checks."""
    out = []
    rounding_series = []
    for cmp in (gap_cmp, real_cmp):
        n = cmp.numbers
        failing = any(not c.ok for c in cmp.checks)
        if failing and n.get("tick_rounded_fills", 0) > 0 and n.get("other_price_diffs", 0) == 0:
            rounding_series.append(cmp.series)
    if rounding_series:
        counts = ", ".join(
            f"{c.numbers['tick_rounded_fills']} on the {c.series} bars"
            for c in (gap_cmp, real_cmp) if c.series in rounding_series
        )
        out.append(
            f"- **Fails, cause known, our engine:** fill prices differ by our rounding ({counts}): "
            "every differing price of ours is our fill model's price (on the gapless bars "
            "Nautilus's) rounded to the instrument's current tick against the order, up for a buy "
            "and down for a sell, and none differs otherwise. The bars carry prices off the current "
            "tick (the venue's tick was finer then). Our broker rounds every fill price to the "
            "current tick, slippage 0 included (`slip()` in `crates/td-engine/src/fill.rs`), where "
            "Nautilus fills at the bar's price. Each such fill is up to a tick worse in ours; "
            "quantities, which each engine sizes from its own equity, then differ, and fees, trade "
            "amounts and equity follow. Not fixed in this branch (issue #99)."
        )
    for cmp in (gap_cmp, real_cmp):
        moved = any(c.name == WALK and not c.ok for c in cmp.checks)
        rounding = cmp.series in rounding_series
        for c in cmp.checks:
            if c.ok:
                continue
            derived = c.name in PRICE_DERIVED
            if c.name == WALK:
                out.append(
                    f"- **Fails, cause known, {cmp.series} bars:** {c.value}. Nautilus 1.231 walks "
                    "a bar through its open, high, low and close only, so it triggers a stop-limit "
                    "at the first of those points past the trigger; where the limit is not "
                    "marketable there, the order rests, and here it rests past the bar, where our "
                    "engine fills it at the trigger (expected difference 4). The fill, and every "
                    f"decision the strategy makes from it, then lands a bar later. {c.note}."
                )
            elif rounding and derived:
                out.append(f"- **Fails, following from our engine's tick rounding, {cmp.series} "
                           f"bars, {c.name}:** {c.value} (limit {c.limit}). {c.note}")
            elif moved and derived:
                out.append(f"- **Fails, following from the moved stop-limit fills, {cmp.series} "
                           f"bars, {c.name}:** {c.value} (limit {c.limit}). {c.note}")
            else:
                out.append(f"- **Unexplained, {cmp.series} bars, {c.name}:** {c.value} (limit "
                           f"{c.limit}). {c.note}")
    return out


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--case", action="append", help="case name, e.g. case01 (repeatable)")
    ap.add_argument("--fetch", action="store_true", help="download and export the data again")
    args = ap.parse_args()
    names = args.case or sorted(p.stem for p in CASES.glob("*.json"))
    exe = build_example()
    results = [run_case(exe, CASES / f"{n}.json", args.fetch) for n in names]
    print(f"{sum(results)} of {len(results)} cases pass")
    return 0 if all(results) else 1


if __name__ == "__main__":
    sys.exit(main())
