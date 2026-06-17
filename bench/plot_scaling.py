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
# where `model` is "poly" (fit a power law y ~ n^k) or "exp" (fit y ~ b^n). The
# "exp" model is retained for re-running on builds where an axis scales
# exponentially; on the current build every axis is sub-exponential.
SERIES = [
    ("shapes", "Shapes (recursion)", "# rectangles", "poly"),
    ("shapes_loop", "Shapes (for-loop)", "# rectangles", "poly"),
    ("instances", "Instances", "# instances", "poly"),
    ("constraints", "Coupled constraints", "# coupled rects", "poly"),
    ("hierarchy_single_ref", "Hierarchy (1 ref)", "depth", "poly"),
    ("hierarchy_double_ref", "Hierarchy (2 refs)", "depth", "poly"),
]

# Okabe-Ito colorblind-safe palette (black and yellow dropped for line
# contrast on white), ordered to match SERIES. The two "twin" pairs --
# recursion/for-loop and 1-ref/2-ref -- get distinct hues so that their
# near-coincidence on the plot reads as two curves landing on top of each
# other rather than one. Markers are also distinct so the series remain
# separable in grayscale print.
PALETTE = ["#0072B2", "#56B4E9", "#009E73", "#D55E00", "#CC79A7", "#E69F00"]
MARKERS = ["o", "s", "^", "D", "v", "P"]


def apply_pub_style(matplotlib):
    """ACM acmart (sigconf, camera-ready) publication style.

    The critical setting is ``fonttype = 42``: it embeds text as subsetted
    TrueType (Type 42) glyphs instead of matplotlib's default Type 3 fonts,
    which ACM's TAPS pipeline (and IEEE PDF eXpress) reject. The figure uses a
    sans-serif (Arial) face; ``Arial`` is listed first for portability, then the
    metric-identical open clones (Liberation Sans / Arimo), then ``Nimbus Sans``
    (a Helvetica/Arial-metric clone -- the fallback present on this machine when
    Arial proper is not installed). Every text element is >= 8 pt, so the figure,
    dropped into a full-width ``figure*`` at the sigconf text width (~7 in),
    renders all text at 8-9 pt without any rescaling.
    """
    matplotlib.rcParams.update({
        "pdf.fonttype": 42,
        "ps.fonttype": 42,
        "font.family": "sans-serif",
        "font.sans-serif": ["Arial", "Liberation Sans", "Arimo",
                            "Nimbus Sans", "Helvetica", "DejaVu Sans"],
        "mathtext.fontset": "dejavusans",
        "font.size": 8,
        "axes.titlesize": 9,
        "axes.labelsize": 8.5,
        "xtick.labelsize": 8,
        "ytick.labelsize": 8,
        "legend.fontsize": 8,
        "axes.linewidth": 0.7,
        "lines.linewidth": 1.3,
        "lines.markersize": 4.2,
        "grid.linewidth": 0.5,
        "xtick.major.width": 0.7,
        "ytick.major.width": 0.7,
        "xtick.minor.width": 0.5,
        "ytick.minor.width": 0.5,
        "savefig.dpi": 300,
        "figure.dpi": 300,
    })


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
        apply_pub_style(matplotlib)
        import matplotlib.pyplot as plt
    except ImportError:
        sys.exit("\nmatplotlib not installed; printed summary only. `pip install matplotlib` to draw.")

    # Full text width of an ACM acmart sigconf two-column figure* (~7 in).
    fig, (ax_t, ax_m) = plt.subplots(1, 2, figsize=(7.0, 2.9), layout="constrained")
    for (key, _, _, _), marker, color in zip(SERIES, MARKERS, PALETTE):
        if key not in data:
            continue
        label, unit, model, xs, ts, ms = data[key]
        style = dict(marker=marker, color=color,
                     markeredgecolor="white", markeredgewidth=0.5)
        ax_t.plot(xs, ts, label=label, **style)
        ax_m.plot(xs, [m / 2**20 for m in ms], label=label, **style)

    for ax in (ax_t, ax_m):
        ax.set_xscale("log")
        ax.set_yscale("log")
        ax.set_xlabel("problem size n")
        ax.grid(True, which="major", ls=":", alpha=0.45)
        ax.grid(True, which="minor", ls=":", alpha=0.2)
        ax.tick_params(which="both", direction="in", top=True, right=True)

    ax_t.set_ylabel("compile time (s)")
    ax_t.set_title("(a) Compile time")
    ax_m.set_ylabel("peak heap allocated (MiB)")
    ax_m.set_title("(b) Peak heap memory")
    leg_kw = dict(loc="upper left", handlelength=1.6, labelspacing=0.3,
                  borderpad=0.4, handletextpad=0.5, framealpha=0.9,
                  edgecolor="0.7", fancybox=False)
    ax_t.legend(**leg_kw)
    ax_m.legend(**leg_kw)

    # Saved at the figure's native size (constrained layout already reserves
    # room for labels/legend), so the PDF MediaBox stays at the sigconf text
    # width and the figure drops into the paper at width=\textwidth with no
    # font-shrinking rescale.
    for ext in ("png", "pdf"):
        out = f"{args.out}.{ext}"
        fig.savefig(out)
        print(f"wrote {out}")


if __name__ == "__main__":
    main()
