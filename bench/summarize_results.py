#!/usr/bin/env python3
"""Collapse hyperfine JSON exports from run_benchmarks.sh into one table."""

import json
import statistics
import sys
from pathlib import Path

TOOLS = [
    (["rymd"], "Rymd (full in-memory GUI model)"),
    (["gdu-tree"], "gdu --depth (full retained tree)"),
    (["gdu-light", "gdu-full"], "gdu -n (lightweight aggregate)"),
    (["dua-aggregate", "dua"], "dua aggregate (aggregate traversal)"),
    (["gdu-summarize"], "gdu -s (totals only, lower bound)"),
]


def load(path: Path):
    try:
        data = json.loads(path.read_text())
    except (OSError, ValueError):
        return None
    results = data.get("results") or []
    if not results:
        return None
    r = results[0]
    return {
        "mean": r["mean"] * 1000.0,
        "min": r["min"] * 1000.0,
        "max": r["max"] * 1000.0,
        "stddev": r.get("stddev", 0.0) * 1000.0,
    }


def diskonaut_mean(path: Path):
    if not path.exists():
        return None
    vals = []
    for line in path.read_text().splitlines():
        line = line.strip()
        try:
            vals.append(float(line))
        except ValueError:
            continue
    if not vals:
        return None
    return {
        "mean": statistics.mean(vals),
        "min": min(vals),
        "max": max(vals),
        "stddev": statistics.stdev(vals) if len(vals) > 1 else 0.0,
    }


def main() -> int:
    base = Path(sys.argv[1])
    print(f"# {base}\n")
    for tree_dir in sorted(p for p in base.iterdir() if p.is_dir()):
        name = tree_dir.name
        print(f"\n## {name}")
        print("| tool | mean ms | min | max | stddev |")
        print("| --- | --- | --- | --- | --- |")
        for slugs, label in TOOLS:
            s = None
            for slug in slugs:
                s = load(tree_dir / f"{slug}.json")
                if s:
                    break
            if not s:
                continue
            print(
                f"| {label} | {s['mean']:.1f} | {s['min']:.1f} "
                f"| {s['max']:.1f} | {s['stddev']:.1f} |"
            )
        d = diskonaut_mean(tree_dir / "diskonaut-runs.txt")
        if d:
            print(
                f"| diskonaut (TUI) | {d['mean']:.1f} | {d['min']:.1f} "
                f"| {d['max']:.1f} | {d['stddev']:.1f} |"
            )
    return 0


if __name__ == "__main__":
    sys.exit(main())
