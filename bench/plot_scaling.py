#!/usr/bin/env python3
"""Plot Argon compile-time and memory scaling from the benchmark CSVs.

The CSVs are produced by the `bench_*` tests in `core/compiler/src/lib.rs`
(see ../bench/README.md for how to run them). Each CSV has the columns

    size,time_s,peak_bytes,n_objects

where `size` is the swept parameter for that axis (number of shapes, number of
coupled constraints, number of instances, or hierarchy depth).

Usage:
    python3 bench/plot_scaling.py                 # reads bench/results/*.csv
    python3 bench/plot_scaling.py --results DIR --out FILE
"""
import argparse
import csv
import math
import os
import sys

# Series in the order we want them drawn. Each entry is
#   (csv_basename, display_label, size_unit, model)
# where `model` is "poly" (fit a power law y ~ n^k) or "exp" (fit y ~ b^n,
# appropriate for the exponentially-scaling hierarchy variant).
SERIES = [
    ("shapes", "Shapes (recursion)", "# rectangles", "poly"),
    ("shapes_loop", "Shapes (for-loop / cons list)", "# rectangles", "poly"),
    ("instances", "Instances", "# instances", "poly"),
    ("constraints", "Coupled constraints", "# coupled rects", "poly"),
    ("hierarchy_single_ref", "Hierarchy (1 child ref)", "depth", "poly"),
    ("hierarchy_double_ref", "Hierarchy (2 child refs)", "depth", "exp"),
]


def load(path):
    xs, ts, ms = [], [], []
    with open(path, newline="") as f:
        for row in csv.DictReader(f):
            xs.append(float(row["size"]))
            ts.append(float(row["time_s"]))
            ms.append(float(row["peak_bytes"]))
    return xs, ts, ms


def _slope(pairs):
    """Least-squares slope of a list of (x, y) points."""
    n = len(pairs)
    if n < 2:
        return float("nan")
    sx = sum(p[0] for p in pairs)
    sy = sum(p[1] for p in pairs)
    sxx = sum(p[0] * p[0] for p in pairs)
    sxy = sum(p[0] * p[1] for p in pairs)
    denom = n * sxx - sx * sx
    if abs(denom) < 1e-12:
        return float("nan")
    return (n * sxy - sx * sy) / denom


def fit_exponent(xs, ys):
    """Power-law exponent: slope of log(y) vs log(x)."""
    return _slope([(math.log(x), math.log(y)) for x, y in zip(xs, ys) if x > 0 and y > 0])


def fit_base(xs, ys):
    """Exponential base b for y ~ b^x: from the slope of log(y) vs x."""
    s = _slope([(x, math.log(y)) for x, y in zip(xs, ys) if y > 0])
    return math.exp(s)


def describe(model, xs, ys):
    """Return (legend_suffix, summary_string) for the fitted scaling model."""
    if model == "exp":
        b = fit_base(xs, ys)
        return f"exp., $\\times{b:.1f}$/step", f"exponential (x{b:.2f} per unit)"
    k = fit_exponent(xs, ys)
    return f"$\\propto n^{{{k:.1f}}}$", f"~n^{k:.2f}"


def main():
    here = os.path.dirname(os.path.abspath(__file__))
    ap = argparse.ArgumentParser()
    ap.add_argument("--results", default=os.path.join(here, "results"))
    ap.add_argument("--out", default=os.path.join(here, "argon_scaling"))
    args = ap.parse_args()

    data = {}
    for key, label, unit, model in SERIES:
        path = os.path.join(args.results, f"{key}.csv")
        if os.path.exists(path):
            xs, ts, ms = load(path)
            if xs:
                data[key] = (label, unit, model, xs, ts, ms)

    if not data:
        sys.exit(
            f"No benchmark CSVs found in {args.results}.\n"
            "Run the benchmarks first (see bench/README.md)."
        )

    # Print a summary table of fitted scaling models.
    print(f"{'series':<30}{'points':>7}  {'time scaling':<22}{'mem scaling':<22}max(time, mem)")
    for key, _, _, _ in SERIES:
        if key not in data:
            continue
        label, unit, model, xs, ts, ms = data[key]
        _, t_desc = describe(model, xs, ts)
        _, m_desc = describe(model, xs, ms)
        print(
            f"{label:<30}{len(xs):>7}  {t_desc:<22}{m_desc:<22}"
            f"{max(ts):.3f} s / {max(ms) / 2**20:.0f} MiB"
        )

    try:
        import matplotlib

        matplotlib.use("Agg")
        import matplotlib.pyplot as plt
    except ImportError:
        sys.exit("\nmatplotlib not installed; printed summary only. `pip install matplotlib` to draw.")

    fig, (ax_t, ax_m) = plt.subplots(1, 2, figsize=(13, 5.2))
    markers = ["o", "s", "^", "D", "v", "P"]
    for (key, _, _, _), marker in zip(SERIES, markers):
        if key not in data:
            continue
        label, unit, model, xs, ts, ms = data[key]
        t_suffix, _ = describe(model, xs, ts)
        m_suffix, _ = describe(model, xs, ms)
        ax_t.plot(xs, ts, marker=marker, label=f"{label}  ({t_suffix})")
        ax_m.plot(xs, [m / 2**20 for m in ms], marker=marker,
                  label=f"{label}  ({m_suffix})")

    for ax in (ax_t, ax_m):
        ax.set_xscale("log")
        ax.set_yscale("log")
        ax.set_xlabel("problem size $n$ (rectangles / constraints / instances / depth)")
        ax.grid(True, which="both", ls=":", alpha=0.4)

    ax_t.set_ylabel("compile time (s)")
    ax_t.set_title("Argon compile-time scaling")
    ax_m.set_ylabel("peak heap allocated (MiB)")
    ax_m.set_title("Argon memory scaling")
    ax_t.legend(fontsize=8, loc="upper left")
    ax_m.legend(fontsize=8, loc="upper left")
    fig.tight_layout()

    for ext in ("png", "pdf"):
        out = f"{args.out}.{ext}"
        fig.savefig(out, dpi=150, bbox_inches="tight")
        print(f"wrote {out}")


if __name__ == "__main__":
    main()
