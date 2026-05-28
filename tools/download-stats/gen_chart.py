#!/usr/bin/env python3
"""Maintain a download-count time series and render it as a self-contained SVG.

GitHub exposes only the *current* cumulative download count per release asset (no
history), so this script accumulates one data point per run. The series is stored
in `downloads.json` which is published to GitHub Pages alongside the code graph;
each daily run reads the previously-published series back over HTTP, appends/updates
today's point, and re-renders the chart. No extra branch, no commits to `main`.

The chart is a standalone dark SVG (inline styles only, generic fonts) so it embeds
directly via `<img>` in the README.

Run (in .github/workflows/code-graph.yml):
  python3 tools/download-stats/gen_chart.py --prev prev.json --add 1234 \
      --out-json _site/downloads.json --out-svg _site/downloads.svg
"""
from __future__ import annotations

import argparse
import datetime
import json
import pathlib

W, H = 540, 260
PAD_L, PAD_R, PAD_T, PAD_B = 16, 16, 52, 34  # plot insets


def load_series(prev: str | None) -> list[dict]:
    if not prev:
        return []
    # The workflow always writes prev.json: "[]" on a confirmed 404 (first run) or the
    # fetched body on HTTP 200. A parse failure / non-list here therefore means the
    # published history is corrupt — raise (failing the job, set -e) rather than
    # silently resetting the only persisted history to a single point.
    data = json.loads(pathlib.Path(prev).read_text(encoding="utf-8"))
    if not isinstance(data, list):
        raise ValueError(f"{prev}: expected a JSON list, got {type(data).__name__}")
    return data


def upsert(series: list[dict], date: str, total: int) -> list[dict]:
    by_date = {p["date"]: p for p in series if "date" in p and "total" in p}
    by_date[date] = {"date": date, "total": total}  # replace same-day re-runs
    return sorted(by_date.values(), key=lambda p: p["date"])


def fmt(n: int) -> str:
    return f"{n:,}"


def esc(s: str) -> str:
    return str(s).replace("&", "&amp;").replace("<", "&lt;").replace(">", "&gt;")


def render_svg(series: list[dict]) -> str:
    accent, accent2 = "#0d9488", "#0891b2"  # teal/cyan — readable on a light card
    head = (
        f'<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 {W} {H}" '
        f'font-family="ui-sans-serif,-apple-system,Segoe UI,Roboto,sans-serif">'
        '<defs>'
        '<linearGradient id="bg" x1="0" y1="0" x2="1" y2="1">'
        '<stop offset="0" stop-color="#ffffff"/><stop offset="1" stop-color="#f3f6fa"/>'
        '</linearGradient>'
        f'<linearGradient id="area" x1="0" y1="0" x2="0" y2="1">'
        f'<stop offset="0" stop-color="{accent}" stop-opacity="0.20"/>'
        f'<stop offset="1" stop-color="{accent}" stop-opacity="0"/>'
        '</linearGradient>'
        f'<linearGradient id="line" x1="0" y1="0" x2="1" y2="0">'
        f'<stop offset="0" stop-color="{accent2}"/><stop offset="1" stop-color="{accent}"/>'
        '</linearGradient>'
        '</defs>'
        f'<rect width="{W}" height="{H}" rx="16" fill="url(#bg)"/>'
        f'<rect x="0.5" y="0.5" width="{W-1}" height="{H-1}" rx="15.5" fill="none" '
        'stroke="#d0d7de"/>'
        '<text x="20" y="30" fill="#1f2328" font-size="15" font-weight="700">Total Downloads</text>'
    )

    latest = series[-1]["total"] if series else 0
    big = (
        f'<text x="{W-20}" y="30" text-anchor="end" fill="{accent}" '
        f'font-size="20" font-weight="800">{esc(fmt(latest))}</text>'
    )

    x0, x1 = PAD_L, W - PAD_R
    y0, y1 = PAD_T, H - PAD_B
    grid = "".join(
        f'<line x1="{x0}" y1="{y0 + (y1-y0)*k/3:.1f}" x2="{x1}" '
        f'y2="{y0 + (y1-y0)*k/3:.1f}" stroke="rgba(27,31,36,0.06)"/>'
        for k in range(4)
    )

    # Fewer than two points: a trend line needs at least two, and GitHub exposes only the
    # current cumulative count (no history) so earlier points can't be backfilled — the
    # line draws itself once the daily cron lands a 2nd point. Render the grid (so the card
    # reads as a chart that has started) plus, for the single point, a centred marker and
    # caption, instead of an empty card with one floating line of text.
    if len(series) < 2:
        if not series:
            body = (
                grid
                + f'<text x="{(x0+x1)/2:.1f}" y="{(y0+y1)/2:.1f}" text-anchor="middle" '
                'dominant-baseline="middle" fill="#57606a" font-size="13">'
                "no release downloads yet</text>"
            )
            return head + big + body + "</svg>"
        cx, cy = (x0 + x1) / 2, (y0 + y1) / 2
        d0 = esc(series[0]["date"])
        body = (
            grid
            + f'<circle cx="{cx:.1f}" cy="{cy:.1f}" r="8" fill="{accent}" opacity="0.25"/>'
            + f'<circle cx="{cx:.1f}" cy="{cy:.1f}" r="4.5" fill="{accent}"/>'
            + f'<text x="{cx:.1f}" y="{cy+26:.1f}" text-anchor="middle" fill="#57606a" '
            f'font-size="12">tracking since {d0} · trend line fills in daily</text>'
            + f'<text x="{x0}" y="{H-12}" fill="#8b949e" font-size="11">{d0}</text>'
        )
        return head + big + body + "</svg>"

    totals = [p["total"] for p in series]
    lo, hi = min(totals), max(totals)
    span = (hi - lo) or 1
    n = len(series)

    def px(i: int) -> float:
        return x0 + (x1 - x0) * (i / (n - 1))

    def py(v: int) -> float:
        return y1 - (y1 - y0) * ((v - lo) / span)

    pts = [(px(i), py(t)) for i, t in enumerate(totals)]
    line_d = "M" + " L".join(f"{x:.1f},{y:.1f}" for x, y in pts)
    area_d = (
        f"M{pts[0][0]:.1f},{y1:.1f} L"
        + " L".join(f"{x:.1f},{y:.1f}" for x, y in pts)
        + f" L{pts[-1][0]:.1f},{y1:.1f} Z"
    )

    lx, ly = pts[-1]
    body = (
        grid
        + f'<path d="{area_d}" fill="url(#area)"/>'
        + f'<path d="{line_d}" fill="none" stroke="url(#line)" stroke-width="2.5" '
        'stroke-linejoin="round" stroke-linecap="round"/>'
        + f'<circle cx="{lx:.1f}" cy="{ly:.1f}" r="4.5" fill="{accent}"/>'
        + f'<circle cx="{lx:.1f}" cy="{ly:.1f}" r="8" fill="{accent}" opacity="0.25"/>'
        + f'<text x="{x0}" y="{H-12}" fill="#8b949e" font-size="11">{esc(series[0]["date"])}</text>'
        + f'<text x="{x1}" y="{H-12}" text-anchor="end" fill="#8b949e" font-size="11">'
        f'{esc(series[-1]["date"])}</text>'
        + f'<text x="{x0}" y="{y0-6}" fill="#8b949e" font-size="10">peak {esc(fmt(hi))}</text>'
    )
    return head + big + body + "</svg>"


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--prev", default=None, help="previous downloads.json (series to append to)")
    ap.add_argument("--add", type=int, required=True, help="today's cumulative total downloads")
    ap.add_argument("--date", default=None, help="data point date (default: today UTC)")
    ap.add_argument("--out-json", required=True)
    ap.add_argument("--out-svg", required=True)
    args = ap.parse_args()

    date = args.date or datetime.datetime.now(datetime.timezone.utc).strftime("%Y-%m-%d")
    series = upsert(load_series(args.prev), date, args.add)

    out_json = pathlib.Path(args.out_json)
    out_json.parent.mkdir(parents=True, exist_ok=True)
    out_json.write_text(json.dumps(series, ensure_ascii=False), encoding="utf-8")

    out_svg = pathlib.Path(args.out_svg)
    out_svg.parent.mkdir(parents=True, exist_ok=True)
    out_svg.write_text(render_svg(series), encoding="utf-8")

    print(f"download-stats: {len(series)} point(s), latest total={args.add} on {date}")


if __name__ == "__main__":
    main()
