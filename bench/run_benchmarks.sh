#!/usr/bin/env bash
#
# Run the Argon scaling benchmarks and regenerate their collateral.
#
# This rebuilds the compiler in release, runs every `bench_*` test serially,
# writes one CSV per axis to bench/results/, and redraws the figure
# bench/argon_scaling.{png,pdf} (and prints a fitted-scaling summary table)
# from those CSVs.
#
# The benchmarks MUST run serially (`--test-threads=1`): peak-memory tracking
# uses a process-global allocator, so concurrent tests would corrupt each
# other's measurements. They are `#[ignore]`'d (the larger sizes exceed 6 s in
# a debug build), so we pass `--ignored`, and the `bench_` name filter keeps us
# from picking up the other ignored ("not supported") tests.
#
# Usage:
#   bench/run_benchmarks.sh                  # run all axes
#   bench/run_benchmarks.sh bench_shapes     # run a single axis (libtest filter)
#
# Sweep sizes are configurable per axis: set a comma-separated list in the
# matching environment variable to override the built-in default. The vars pass
# straight through to the tests, e.g.
#   ARGON_BENCH_CONSTRAINTS=64,128,256 bench/run_benchmarks.sh bench_constraints
#
# Recognised vars: ARGON_BENCH_SHAPES, ARGON_BENCH_SHAPES_LOOP,
#   ARGON_BENCH_INSTANCES, ARGON_BENCH_CONSTRAINTS, ARGON_BENCH_HIER_SINGLE,
#   ARGON_BENCH_HIER_DOUBLE.
#
# Note: building `-p compiler` does not need the GUI's linker flags, so this
# script sets no RUSTFLAGS (any you already export are passed through, and are
# harmless for the compiler crate).
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
FILTER="${1:-bench_}"

cd "$REPO_ROOT"
SECONDS=0

echo ">> Running Argon scaling benchmarks (filter: '$FILTER')"
echo "   profile: release | serial (--test-threads=1) | --ignored"
echo "   CSVs   -> $SCRIPT_DIR/results/<axis>.csv"
echo "   figure -> $SCRIPT_DIR/argon_scaling.{png,pdf}"
echo

cargo test -p compiler --release -- --ignored --test-threads=1 --nocapture "$FILTER"

echo
echo ">> Benchmarks finished in ${SECONDS}s. Regenerating figure and summary..."
echo

if command -v python3 >/dev/null 2>&1; then
    # plot_scaling.py prints the summary table before importing matplotlib, so
    # the table appears even when matplotlib is missing. Don't abort the whole
    # run if only the figure step fails -- the CSVs are the primary collateral.
    if ! python3 "$SCRIPT_DIR/plot_scaling.py"; then
        echo "!! plot_scaling.py exited non-zero (matplotlib not installed?);" >&2
        echo "   CSVs were still regenerated. \`pip install matplotlib\` to draw." >&2
    fi
else
    echo "!! python3 not found; skipping figure. CSVs were still regenerated." >&2
fi

echo
echo ">> Regenerated CSVs:"
ls -1 "$SCRIPT_DIR"/results/*.csv 2>/dev/null | sed 's|^|     |' || echo "     (none)"
echo
echo ">> Done in ${SECONDS}s."
echo "   bench/README.md's results table and interpretation are hand-written;"
echo "   review them against the summary above if absolute numbers changed."
