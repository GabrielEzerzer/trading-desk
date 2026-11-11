"""Runs the cross-check: `uv run python run.py [--case case01 ...] [--fetch]`.

For each case it fetches the data if the export is missing (or with `--fetch`), runs our engine and
NautilusTrader on the real and the gapless bars, measures both equity curves with our metrics,
applies the agreement rule (`compare.py`) and writes `reports/<case>.md`. Outputs go to `out/`.
The exit code is 1 if any case fails.
"""

from __future__ import annotations

import argparse
import json
import subprocess
import sys
from pathlib import Path

import compare
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
        if series == "gapless":
            cmp = compare.compare_gapless(
                mine, theirs, bars, inst["tick_size"], perpetual=case["kind"] == "perpetual"
            )
        else:
            cmp = compare.compare_real(mine, theirs, bars, case["fees"]["taker"])
        m_ours = metrics(exe, mine, case, out / f"metrics_ours_{series}.json")
        m_theirs = metrics(exe, theirs, case, out / f"metrics_nautilus_{series}.json")
        results[series] = (cmp, mine, theirs, m_ours, m_theirs)

    ok = all(r[0].ok for r in results.values())
    REPORTS.mkdir(exist_ok=True)
    (REPORTS / f"{name}.md").write_text(report(case, meta, inst, results, ok), encoding="utf-8")
    g, r = results["gapless"][0].numbers, results["real"][0].numbers
    print(
        f"{name}: {'PASS' if ok else 'FAIL'}  gapless: {g['fills_compared']} fills, max price diff "
        f"{g['max_price_diff']:.6g}, final equity rel {g['final_equity_rel']:.2e}  real: "
        f"{r['fills_compared']} fills, equity diff {r['final_equity_diff']:.4f} vs bound "
        f"{r['bound_sum']:.4f}",
        flush=True,
    )
    return ok


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


def report(case: dict, meta: dict, inst: dict, results: dict, ok: bool) -> str:
    gap_cmp, gap_ours, gap_theirs, gm_o, gm_t = results["gapless"]
    real_cmp, real_ours, real_theirs, rm_o, rm_t = results["real"]
    g, r = gap_cmp.numbers, real_cmp.numbers
    dropped = meta["dropped_buckets"]
    parts = [
        f"# {case['name']}: {case['title']}",
        "",
        f"**Verdict: {'PASS' if ok else 'FAIL'}.** Exercises: {case['exercises']}.",
        "",
        "Generated by `harness/crosscheck/run.py`; do not edit by hand.",
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
        "one filled at the decision bar's close (Nautilus) get the same price. Everything must match.",
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
        "next bar's open (expected difference 1). Fills must pair one to one, each price difference "
        "must stay within `|open[N+1] - close[N]|`, and the final equity difference within the sum "
        "of bound 1, `qty * |open[N+1] - close[N]| * (1 + taker)` over the fills (the taker factor "
        "is the fee on that price difference). Cases 1 to 4 have no stops or limits, so bounds 2 "
        "and 3 are zero.",
        "",
        checks_table(real_cmp),
        "",
        f"Bound 1 total {r['bound1']:.6f}; largest relative quantity difference between paired fills "
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
        f"{g['final_equity_nautilus']:.8f} (Nautilus), relative {g['final_equity_rel']:.2e}.",
        f"- Real: {r['fills_compared']} fills compared, max price difference "
        f"{r['max_price_diff']:.6g}, final equity difference {r['final_equity_diff']:.6f} against a "
        f"bound of {r['bound_sum']:.6f}.",
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
    if r["max_price_diff"] > 0:
        out.append(
            f"- Real bars, fill prices: up to {r['max_price_diff']:.6g} apart, each within its bar's "
            "`|open[N+1] - close[N]|`: expected difference 1 (Nautilus fills at the decision bar's "
            "close, we fill at the next open)."
        )
    if r["max_qty_rel"] > 0:
        out.append(
            "- Real bars, quantities: paired fills differ by up to "
            f"{r['max_qty_rel']:.2e} relative, because each engine sizes from its own equity, which "
            "already differs by the earlier fill price differences; where a size lands near a lot "
            "boundary the rounding down to the lot moves it by a whole lot."
        )
    if compare.rel(r["funding_total_ours"], r["funding_total_nautilus"]) > compare.FUNDING_REL:
        out.append(
            "- Real bars, funding: totals differ by "
            f"{abs(r['funding_total_ours'] - r['funding_total_nautilus']):.6f}, from the position "
            "sizes above (same stamps, rates and prices)."
        )
    for cmp in (gap_cmp, real_cmp):
        for c in cmp.checks:
            if not c.ok:
                out.append(f"- **Unexplained, {cmp.series} bars, {c.name}:** {c.value} (limit "
                           f"{c.limit}). {c.note}")
    if not out:
        out.append("- None: the two engines agree exactly within the tolerances.")
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
