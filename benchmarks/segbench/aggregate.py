#!/usr/bin/env python3
"""Aggregate segbench CSVs into per-series medians and speedups.

    python3 aggregate.py results/scaling.csv results/locality.csv

Prints JSON keyed by "<op>_<dist>[_<mode>]", each a list of points ordered by
thread count with median / min / max and speedup against that series' own
single-thread median. Unknown modes are named rather than dropped, so adding a
mode to the harness needs no change here.
"""
import csv
import json
import statistics
import sys

OP = {0: "read", 100: "write"}  # anything else is a mix, named by its write %


def series_name(write_pct, dist, mode):
    op = OP.get(write_pct, f"mix{write_pct}")
    name = f"{op}_{dist}"
    return name if mode == "base" else f"{name}_{mode}"


def main(paths):
    series = {}
    for path in paths:
        with open(path) as f:
            rows = (l for l in f if not l.startswith("#") and l.strip() != "DONE")
            for r in csv.DictReader(rows):
                key = series_name(int(r["write_pct"]), r["dist"], r.get("mode", "base"))
                series.setdefault(key, {}).setdefault(int(r["threads"]), []).append(
                    float(r["mops"])
                )

    out = {}
    for key, by_threads in sorted(series.items()):
        points = []
        for t in sorted(by_threads):
            vals = by_threads[t]
            points.append(
                {
                    "t": t,
                    "med": round(statistics.median(vals), 3),
                    "min": round(min(vals), 3),
                    "max": round(max(vals), 3),
                    "n": len(vals),
                }
            )
        base = next((p["med"] for p in points if p["t"] == 1), points[0]["med"])
        for p in points:
            p["speedup"] = round(p["med"] / base, 2)
        out[key] = points
    return out


if __name__ == "__main__":
    if len(sys.argv) < 2:
        sys.exit(__doc__)
    print(json.dumps(main(sys.argv[1:]), indent=2))
