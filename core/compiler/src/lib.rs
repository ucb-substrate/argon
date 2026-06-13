mod antlr;
pub mod ast;
pub mod compile;
pub mod config;
pub mod gds;
pub mod layer;
pub mod parse;
pub mod solver;

/// A global allocator that tracks live and peak heap usage so that the scaling
/// benchmarks in the test module can report memory consumption alongside
/// runtime. It forwards every request to the system allocator and only adds
/// atomic byte counters, so behavior is otherwise unchanged.
///
/// This allocator is only compiled into the test binary (`cfg(test)`); release
/// and library builds use the default allocator. The counters are process-wide,
/// so the benchmarks that read them must be run serially
/// (`--test-threads=1`); see `bench/README.md`.
#[cfg(test)]
mod bench_alloc {
    use std::alloc::{GlobalAlloc, Layout, System};
    use std::sync::atomic::{AtomicUsize, Ordering};

    pub static LIVE: AtomicUsize = AtomicUsize::new(0);
    pub static PEAK: AtomicUsize = AtomicUsize::new(0);

    pub struct Tracking;

    #[inline]
    fn record_growth(delta: usize) {
        let live = LIVE.fetch_add(delta, Ordering::Relaxed) + delta;
        PEAK.fetch_max(live, Ordering::Relaxed);
    }

    unsafe impl GlobalAlloc for Tracking {
        unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
            let ptr = unsafe { System.alloc(layout) };
            if !ptr.is_null() {
                record_growth(layout.size());
            }
            ptr
        }

        unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
            unsafe { System.dealloc(ptr, layout) };
            LIVE.fetch_sub(layout.size(), Ordering::Relaxed);
        }

        unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
            let ptr = unsafe { System.alloc_zeroed(layout) };
            if !ptr.is_null() {
                record_growth(layout.size());
            }
            ptr
        }

        unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
            let new_ptr = unsafe { System.realloc(ptr, layout, new_size) };
            if !new_ptr.is_null() {
                if new_size >= layout.size() {
                    record_growth(new_size - layout.size());
                } else {
                    LIVE.fetch_sub(layout.size() - new_size, Ordering::Relaxed);
                }
            }
            new_ptr
        }
    }

    /// Resets the peak counter to the current live usage. Call this immediately
    /// before the region of interest, then read [`peak`] afterwards.
    pub fn reset_peak() {
        PEAK.store(LIVE.load(Ordering::Relaxed), Ordering::Relaxed);
    }

    pub fn live() -> usize {
        LIVE.load(Ordering::Relaxed)
    }

    pub fn peak() -> usize {
        PEAK.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
#[global_allocator]
static BENCH_ALLOC: bench_alloc::Tracking = bench_alloc::Tracking;

#[cfg(test)]
mod tests {

    use std::path::PathBuf;

    use crate::{
        compile::{ExecErrorKind, SolvedValue, StaticErrorKind},
        gds::GdsMap,
        parse::parse_workspace_with_std,
    };
    use ::gds::GdsUnits;
    use approx::assert_relative_eq;
    use approx::relative_eq;
    use const_format::concatcp;
    use pegasus::drc::{DrcParams, run_drc};

    use crate::compile::{CellArg, CompileInput, compile};
    const EPSILON: f64 = 1e-10;

    const EXAMPLES_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../examples");
    const ARGON_SCOPES: &str = concatcp!(EXAMPLES_DIR, "/scopes/lib.ar");
    const BASIC_LYP: &str = concatcp!(EXAMPLES_DIR, "/lyp/basic.lyp");
    const ARGON_SKY130_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../pdks/sky130");
    const ARGON_SKY130_LIB: &str = concatcp!(ARGON_SKY130_DIR, "/lib.ar");
    const SKY130_LYP: &str = concatcp!(ARGON_SKY130_DIR, "/sky130.lyp");
    const ARGON_IMMEDIATE: &str = concatcp!(EXAMPLES_DIR, "/immediate/lib.ar");
    const ARGON_IF: &str = concatcp!(EXAMPLES_DIR, "/if/lib.ar");
    const ARGON_IF_INCONSISTENT: &str = concatcp!(EXAMPLES_DIR, "/if_inconsistent/lib.ar");
    const ARGON_VIA: &str = concatcp!(EXAMPLES_DIR, "/via/lib.ar");
    const ARGON_VIA_ARRAY: &str = concatcp!(EXAMPLES_DIR, "/via_array/lib.ar");
    const ARGON_FUNC_OUT_OF_ORDER: &str = concatcp!(EXAMPLES_DIR, "/func_out_of_order/lib.ar");
    const ARGON_HIERARCHY: &str = concatcp!(EXAMPLES_DIR, "/hierarchy/lib.ar");
    const ARGON_NESTED_INST: &str = concatcp!(EXAMPLES_DIR, "/nested_inst/lib.ar");
    const ARGON_CELL_OUT_OF_ORDER: &str = concatcp!(EXAMPLES_DIR, "/cell_out_of_order/lib.ar");
    const ARGON_FALLBACK_BASIC: &str = concatcp!(EXAMPLES_DIR, "/fallback_basic/lib.ar");
    const ARGON_FALLBACK_INST: &str = concatcp!(EXAMPLES_DIR, "/fallback_inst/lib.ar");
    const ARGON_BOOL_LITERAL: &str = concatcp!(EXAMPLES_DIR, "/bool_literal/lib.ar");
    const ARGON_DIMENSIONS: &str = concatcp!(EXAMPLES_DIR, "/dimensions/lib.ar");
    const ARGON_PARAM_FLOAT: &str = concatcp!(EXAMPLES_DIR, "/param_float/lib.ar");
    const ARGON_PARAM_INT: &str = concatcp!(EXAMPLES_DIR, "/param_int/lib.ar");
    const ARGON_ENUMERATIONS: &str = concatcp!(EXAMPLES_DIR, "/enumerations/lib.ar");
    const ARGON_BBOX: &str = concatcp!(EXAMPLES_DIR, "/bbox/lib.ar");
    const ARGON_ROUNDING: &str = concatcp!(EXAMPLES_DIR, "/rounding/lib.ar");
    const ARGON_FLIPPED_RECT: &str = concatcp!(EXAMPLES_DIR, "/flipped_rect/lib.ar");
    const ARGON_SEQ_BASIC: &str = concatcp!(EXAMPLES_DIR, "/seq_basic/lib.ar");
    const ARGON_SEQ_ANY: &str = concatcp!(EXAMPLES_DIR, "/seq_any/lib.ar");
    const ARGON_SEQ_FN: &str = concatcp!(EXAMPLES_DIR, "/seq_fn/lib.ar");
    const ARGON_SEQ_RECUR: &str = concatcp!(EXAMPLES_DIR, "/seq_recur/lib.ar");
    const ARGON_LUB_MATCH: &str = concatcp!(EXAMPLES_DIR, "/lub_match/lib.ar");
    const ARGON_SEQ_CELL: &str = concatcp!(EXAMPLES_DIR, "/seq_cell/lib.ar");
    const ARGON_WORKSPACE: &str = concatcp!(EXAMPLES_DIR, "/argon_workspace/lib.ar");
    const ARGON_EXTERNAL_MODS: &str = concatcp!(EXAMPLES_DIR, "/external_mods/main_crate/lib.ar");
    const ARGON_TEXT: &str = concatcp!(EXAMPLES_DIR, "/text/lib.ar");
    const ARGON_ANY_TYPE: &str = concatcp!(EXAMPLES_DIR, "/any_type/lib.ar");
    const ARGON_SEQ_INDEX: &str = concatcp!(EXAMPLES_DIR, "/seq_index/lib.ar");
    const ARGON_SEQ_CONSTRUCTOR: &str = concatcp!(EXAMPLES_DIR, "/seq_constructor/lib.ar");
    const ARGON_FUNC_BAD_ARG_REUSE: &str = concatcp!(EXAMPLES_DIR, "/func_bad_arg_reuse/lib.ar");
    const ARGON_CELL_BAD_ARG_REUSE: &str = concatcp!(EXAMPLES_DIR, "/cell_bad_arg_reuse/lib.ar");
    const ARGON_PARTIALLY_CONSTRAINED_INST: &str =
        concatcp!(EXAMPLES_DIR, "/partially_constrained_inst/lib.ar");
    const ARGON_INVALID_CAST: &str = concatcp!(EXAMPLES_DIR, "/invalid_cast/lib.ar");
    const ARGON_TUPLE_BASIC: &str = concatcp!(EXAMPLES_DIR, "/tuple_basic/lib.ar");
    const ARGON_TUPLE_ANY: &str = concatcp!(EXAMPLES_DIR, "/tuple_any/lib.ar");
    const ARGON_FOR_LOOP_BASIC: &str = concatcp!(EXAMPLES_DIR, "/for_loop_basic/lib.ar");
    const ARGON_RANGE_PERF: &str = concatcp!(EXAMPLES_DIR, "/range_perf/lib.ar");
    const ARGON_SSE_BASIC: &str = concatcp!(EXAMPLES_DIR, "/sse_basic/lib.ar");
    const ARGON_PRECEDENCE: &str = concatcp!(EXAMPLES_DIR, "/precedence/lib.ar");

    // ---------------------------------------------------------------------
    // Scaling / stress benchmarks.
    //
    // These exercise Argon along the axes raised in review: number of shapes,
    // number of (coupled) constraints, number of cell instances, and depth of
    // hierarchy. Each `bench_*` test sweeps a size parameter, records compile
    // time and peak heap usage, and writes a CSV to `bench/results/` that
    // `bench/plot_scaling.py` turns into the scaling figure.
    //
    // The `bench_*` tests are `#[ignore]`d because the larger sizes take well
    // over 6 s in a debug build. Run them in release, serially (peak-memory
    // tracking is process-global), e.g.:
    //
    //     RUSTFLAGS=... cargo test -p compiler --release -- \
    //         --ignored --test-threads=1 bench_
    //
    // The `stress_*_smoke` tests below run in the normal (debug) test suite and
    // just check that each example still compiles.
    // ---------------------------------------------------------------------
    const ARGON_STRESS_SHAPES: &str = concatcp!(EXAMPLES_DIR, "/stress_shapes/lib.ar");
    const ARGON_STRESS_CONSTRAINTS: &str = concatcp!(EXAMPLES_DIR, "/stress_constraints/lib.ar");
    const ARGON_STRESS_INSTANCES: &str = concatcp!(EXAMPLES_DIR, "/stress_instances/lib.ar");
    const ARGON_STRESS_HIERARCHY: &str = concatcp!(EXAMPLES_DIR, "/stress_hierarchy/lib.ar");

    use crate::compile::CompileOutput;

    /// Serializes the memory/timing-sensitive benchmarks. Even when the test
    /// runner is given multiple threads, holding this lock ensures only one
    /// `bench_*` body runs at a time, so the process-global allocator counters
    /// and wall-clock timings are not perturbed by a concurrent benchmark.
    static BENCH_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn bench_guard() -> std::sync::MutexGuard<'static, ()> {
        // Recover from poisoning: a panic in one benchmark should not wedge the
        // others, and the lock guards only measurement isolation.
        BENCH_LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Runs `f` `reps` times, returning the minimum wall-clock time (robust to
    /// noise on a shared machine), the maximum peak heap growth observed during
    /// a run, and the result of the final run.
    fn measure<R>(reps: u32, f: impl Fn() -> R) -> (std::time::Duration, usize, R) {
        assert!(reps >= 1);
        let mut best = std::time::Duration::MAX;
        let mut peak = 0usize;
        let mut result = None;
        for _ in 0..reps {
            // Free the previous run's result so each measurement starts from
            // the same baseline.
            drop(result.take());
            crate::bench_alloc::reset_peak();
            let base = crate::bench_alloc::live();
            let start = std::time::Instant::now();
            let r = f();
            best = best.min(start.elapsed());
            peak = peak.max(crate::bench_alloc::peak().saturating_sub(base));
            result = Some(r);
        }
        (best, peak, result.unwrap())
    }

    fn count_objects(o: &CompileOutput) -> usize {
        let data = match o {
            CompileOutput::Valid(d) => Some(d),
            CompileOutput::ExecErrors(e) => e.output.as_ref(),
            _ => None,
        };
        data.map(|d| d.cells.values().map(|c| c.objects.len()).sum())
            .unwrap_or(0)
    }

    fn count_cells(o: &CompileOutput) -> usize {
        match o {
            CompileOutput::Valid(d) => d.cells.len(),
            CompileOutput::ExecErrors(e) => e.output.as_ref().map(|d| d.cells.len()).unwrap_or(0),
            _ => 0,
        }
    }

    /// Sweep sizes for a benchmark axis. Returns `default` unless the named
    /// environment variable is set to a comma-separated list of sizes, in which
    /// case that list is used. This keeps the benchmarks general-purpose: the
    /// same test can be re-run at a larger (or smaller) scale without editing
    /// the source, e.g. after a compiler optimization changes how an axis
    /// scales:
    ///
    ///     ARGON_BENCH_SHAPES_LOOP=500,1000,2000,4000,8000,16000,32000 \
    ///         cargo test -p compiler --release -- --ignored --test-threads=1 \
    ///         --nocapture bench_shapes_loop
    ///
    /// The defaults are chosen so the whole suite runs in a few minutes and
    /// stays within a few GiB on the current build; they are not assumptions
    /// about how any axis "should" scale.
    fn bench_sizes(env_var: &str, default: &[i64]) -> Vec<i64> {
        match std::env::var(env_var) {
            Ok(s) if !s.trim().is_empty() => s
                .split(',')
                .filter_map(|x| x.trim().parse::<i64>().ok())
                .collect(),
            _ => default.to_vec(),
        }
    }

    fn write_bench_csv(name: &str, rows: &[(f64, f64, usize, usize)]) {
        use std::fmt::Write;
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../bench/results");
        std::fs::create_dir_all(&dir).unwrap();
        let mut s = String::from("size,time_s,peak_bytes,n_objects\n");
        for (size, t, mem, nobj) in rows {
            writeln!(s, "{size},{t},{mem},{nobj}").unwrap();
        }
        let path = dir.join(format!("{name}.csv"));
        std::fs::write(&path, s).unwrap();
        eprintln!("wrote {}", path.display());
    }

    /// Generates a workspace of `depth + 1` cells `h0..h{depth}` where each
    /// `h{k}` instantiates `h{k-1}`. With `double_ref = false` the child is
    /// referenced by a single (instance) binding; with `double_ref = true` the
    /// child cell is also bound to a `let`, which makes the structural cell type
    /// of `h{k}` contain two copies of the type of `h{k-1}`.
    fn gen_hier(depth: usize, double_ref: bool) -> String {
        let mut s =
            String::from("cell h0() {\n    rect(\"met1\", x0=0., y0=0., x1=10., y1=10.);\n}\n");
        for k in 1..=depth {
            let body = if double_ref {
                format!("    let child = h{}();\n    let i = inst(child);\n", k - 1)
            } else {
                format!("    let i = inst(h{}());\n", k - 1)
            };
            s.push_str(&format!(
                "cell h{k}() {{\n    rect(\"met1\", x0=0., y0=0., x1=10., y1=10.);\n{body}    eq(i.x, 0.);\n    eq(i.y, 10.);\n}}\n",
            ));
        }
        s
    }

    /// Axis 1: number of independent shapes in a single cell.
    #[test]
    #[ignore = "scaling benchmark; run in release, serially: cargo test -p compiler --release -- --ignored --test-threads=1 bench_"]
    fn bench_shapes() {
        let _g = bench_guard();
        let o = parse_workspace_with_std(ARGON_STRESS_SHAPES);
        assert!(o.static_errors().is_empty(), "{:?}", o.static_errors());
        let ast = o.ast();
        let mut rows = Vec::new();
        for &n in &bench_sizes(
            "ARGON_BENCH_SHAPES",
            &[500, 1000, 2000, 4000, 8000, 16000, 32000],
        ) {
            let (dt, mem, out) = measure(3, || {
                compile(
                    &ast,
                    CompileInput {
                        cell: &["shapes"],
                        args: vec![CellArg::Int(n)],
                        lyp_file: &PathBuf::from(BASIC_LYP),
                    },
                )
            });
            assert!(out.is_valid(), "shapes(n={n}) invalid");
            let nobj = count_objects(&out);
            eprintln!(
                "shapes        n={n:>6} objects={nobj:>6} time={dt:>11.3?} peak={:>8.2} MiB",
                mem as f64 / (1usize << 20) as f64
            );
            rows.push((n as f64, dt.as_secs_f64(), mem, nobj));
        }
        write_bench_csv("shapes", &rows);
    }

    /// Axis 1b: the same geometry generated with an idiomatic `for` loop over
    /// `std::range`, which additionally exercises Argon's functional list
    /// representation (`cons`). Capped at a smaller size because list
    /// construction is super-linear.
    #[test]
    #[ignore = "scaling benchmark; run in release, serially: cargo test -p compiler --release -- --ignored --test-threads=1 bench_"]
    fn bench_shapes_loop() {
        let _g = bench_guard();
        let o = parse_workspace_with_std(ARGON_STRESS_SHAPES);
        assert!(o.static_errors().is_empty(), "{:?}", o.static_errors());
        let ast = o.ast();
        // This variant generates the same geometry as `bench_shapes` but with a
        // `for` loop over `std::range`, so its cost also includes building and
        // iterating the list. The default sweep is kept smaller than
        // `bench_shapes` only so the default run stays bounded in memory on the
        // current build; override `ARGON_BENCH_SHAPES_LOOP` to sweep to the same
        // sizes as `bench_shapes` (e.g. to compare the two after changes to the
        // list representation).
        let mut rows = Vec::new();
        for &n in &bench_sizes("ARGON_BENCH_SHAPES_LOOP", &[250, 500, 1000, 2000]) {
            let (dt, mem, out) = measure(2, || {
                compile(
                    &ast,
                    CompileInput {
                        cell: &["shapes_loop"],
                        args: vec![CellArg::Int(n)],
                        lyp_file: &PathBuf::from(BASIC_LYP),
                    },
                )
            });
            assert!(out.is_valid(), "shapes_loop(n={n}) invalid");
            let nobj = count_objects(&out);
            eprintln!(
                "shapes_loop   n={n:>6} objects={nobj:>6} time={dt:>11.3?} peak={:>8.2} MiB",
                mem as f64 / (1usize << 20) as f64
            );
            rows.push((n as f64, dt.as_secs_f64(), mem, nobj));
        }
        write_bench_csv("shapes_loop", &rows);
    }

    /// Axis 2: number of mutually-coupled constraints solved by the general
    /// (dense) linear-constraint solver.
    #[test]
    #[ignore = "scaling benchmark; run in release, serially: cargo test -p compiler --release -- --ignored --test-threads=1 bench_"]
    fn bench_constraints() {
        let _g = bench_guard();
        let o = parse_workspace_with_std(ARGON_STRESS_CONSTRAINTS);
        assert!(o.static_errors().is_empty(), "{:?}", o.static_errors());
        let ast = o.ast();
        let mut rows = Vec::new();
        for &n in &bench_sizes("ARGON_BENCH_CONSTRAINTS", &[32, 64, 128, 256, 512, 1024]) {
            let (dt, mem, out) = measure(1, || {
                compile(
                    &ast,
                    CompileInput {
                        cell: &["constraints"],
                        args: vec![CellArg::Int(n)],
                        lyp_file: &PathBuf::from(BASIC_LYP),
                    },
                )
            });
            assert!(out.is_valid(), "constraints(n={n}) invalid");
            let nobj = count_objects(&out);
            eprintln!(
                "constraints   n={n:>6} objects={nobj:>6} time={dt:>11.3?} peak={:>8.2} MiB",
                mem as f64 / (1usize << 20) as f64
            );
            rows.push((n as f64, dt.as_secs_f64(), mem, nobj));
        }
        write_bench_csv("constraints", &rows);
    }

    /// Axis 3: number of instances of a single (cached) leaf cell.
    #[test]
    #[ignore = "scaling benchmark; run in release, serially: cargo test -p compiler --release -- --ignored --test-threads=1 bench_"]
    fn bench_instances() {
        let _g = bench_guard();
        let o = parse_workspace_with_std(ARGON_STRESS_INSTANCES);
        assert!(o.static_errors().is_empty(), "{:?}", o.static_errors());
        let ast = o.ast();
        let mut rows = Vec::new();
        for &n in &bench_sizes(
            "ARGON_BENCH_INSTANCES",
            &[500, 1000, 2000, 4000, 8000, 16000, 32000, 64000],
        ) {
            let (dt, mem, out) = measure(3, || {
                compile(
                    &ast,
                    CompileInput {
                        cell: &["instances"],
                        args: vec![CellArg::Int(n)],
                        lyp_file: &PathBuf::from(BASIC_LYP),
                    },
                )
            });
            assert!(out.is_valid(), "instances(n={n}) invalid");
            let nobj = count_objects(&out);
            eprintln!(
                "instances     n={n:>6} objects={nobj:>6} time={dt:>11.3?} peak={:>8.2} MiB",
                mem as f64 / (1usize << 20) as f64
            );
            rows.push((n as f64, dt.as_secs_f64(), mem, nobj));
        }
        write_bench_csv("instances", &rows);
    }

    /// Axis 4: depth of cell hierarchy. Two series are produced: `single_ref`
    /// references each child once (polynomial), and `double_ref` references it
    /// twice, which triggers exponential structural-type expansion.
    #[test]
    #[ignore = "scaling benchmark; run in release, serially: cargo test -p compiler --release -- --ignored --test-threads=1 bench_"]
    fn bench_hierarchy() {
        let _g = bench_guard();
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("build/bench_hier");
        std::fs::create_dir_all(&dir).unwrap();
        let lib = dir.join("lib.ar");

        let mut rows = Vec::new();
        for depth in bench_sizes("ARGON_BENCH_HIER_SINGLE", &[4, 8, 16, 32, 48, 64, 96, 128])
            .into_iter()
            .map(|d| d as usize)
        {
            std::fs::write(&lib, gen_hier(depth, false)).unwrap();
            let o = parse_workspace_with_std(&lib);
            assert!(o.static_errors().is_empty(), "{:?}", o.static_errors());
            let ast = o.ast();
            let cellname = format!("h{depth}");
            let (dt, mem, out) = measure(2, || {
                compile(
                    &ast,
                    CompileInput {
                        cell: &[&cellname],
                        args: vec![],
                        lyp_file: &PathBuf::from(BASIC_LYP),
                    },
                )
            });
            assert!(out.is_valid(), "hierarchy single-ref depth={depth} invalid");
            let nobj = count_objects(&out);
            eprintln!(
                "hier(1 ref)   depth={depth:>4} cells={:>4} time={dt:>11.3?} peak={:>8.2} MiB",
                count_cells(&out),
                mem as f64 / (1usize << 20) as f64
            );
            rows.push((depth as f64, dt.as_secs_f64(), mem, nobj));
        }
        write_bench_csv("hierarchy_single_ref", &rows);

        // `double_ref` binds the child cell twice, which (on the current build)
        // makes the structural cell type grow quickly with depth, so the
        // default sweep is kept shallow to stay within a few GiB. Override
        // `ARGON_BENCH_HIER_DOUBLE` to push deeper.
        let mut rows = Vec::new();
        for depth in bench_sizes("ARGON_BENCH_HIER_DOUBLE", &[2, 4, 6, 8, 10, 12, 14, 16, 18])
            .into_iter()
            .map(|d| d as usize)
        {
            std::fs::write(&lib, gen_hier(depth, true)).unwrap();
            let o = parse_workspace_with_std(&lib);
            assert!(o.static_errors().is_empty(), "{:?}", o.static_errors());
            let ast = o.ast();
            let cellname = format!("h{depth}");
            let (dt, mem, out) = measure(1, || {
                compile(
                    &ast,
                    CompileInput {
                        cell: &[&cellname],
                        args: vec![],
                        lyp_file: &PathBuf::from(BASIC_LYP),
                    },
                )
            });
            assert!(out.is_valid(), "hierarchy double-ref depth={depth} invalid");
            let nobj = count_objects(&out);
            eprintln!(
                "hier(2 refs)  depth={depth:>4} time={dt:>11.3?} peak={:>8.2} MiB",
                mem as f64 / (1usize << 20) as f64
            );
            rows.push((depth as f64, dt.as_secs_f64(), mem, nobj));
        }
        write_bench_csv("hierarchy_double_ref", &rows);
    }

    // --- Smoke tests (run in the normal suite; keep these fast) ---

    #[test]
    fn stress_shapes_smoke() {
        let o = parse_workspace_with_std(ARGON_STRESS_SHAPES);
        assert!(o.static_errors().is_empty(), "{:?}", o.static_errors());
        let ast = o.ast();
        for cell in ["shapes", "shapes_loop"] {
            let out = compile(
                &ast,
                CompileInput {
                    cell: &[cell],
                    args: vec![CellArg::Int(64)],
                    lyp_file: &PathBuf::from(BASIC_LYP),
                },
            );
            let d = out.unwrap_valid();
            let nrects = d
                .cells
                .values()
                .flat_map(|c| c.objects.values())
                .filter(|o| matches!(o, SolvedValue::Rect(r) if !r.construction))
                .count();
            assert_eq!(nrects, 64, "{cell} should emit 64 rectangles");
        }
    }

    #[test]
    fn stress_constraints_smoke() {
        let o = parse_workspace_with_std(ARGON_STRESS_CONSTRAINTS);
        assert!(o.static_errors().is_empty(), "{:?}", o.static_errors());
        let ast = o.ast();
        let out = compile(
            &ast,
            CompileInput {
                cell: &["constraints"],
                args: vec![CellArg::Int(32)],
                lyp_file: &PathBuf::from(BASIC_LYP),
            },
        );
        assert!(
            out.is_valid(),
            "constraints ring should be fully determined: {out:?}"
        );
    }

    #[test]
    fn stress_instances_smoke() {
        let o = parse_workspace_with_std(ARGON_STRESS_INSTANCES);
        assert!(o.static_errors().is_empty(), "{:?}", o.static_errors());
        let ast = o.ast();
        let out = compile(
            &ast,
            CompileInput {
                cell: &["instances"],
                args: vec![CellArg::Int(64)],
                lyp_file: &PathBuf::from(BASIC_LYP),
            },
        );
        let d = out.unwrap_valid();
        let ninsts = d
            .cells
            .values()
            .flat_map(|c| c.objects.values())
            .filter(|o| matches!(o, SolvedValue::Instance(_)))
            .count();
        assert_eq!(ninsts, 64, "instances(64) should place 64 instances");
    }

    #[test]
    fn stress_hierarchy_smoke() {
        let o = parse_workspace_with_std(ARGON_STRESS_HIERARCHY);
        assert!(o.static_errors().is_empty(), "{:?}", o.static_errors());
        let ast = o.ast();
        let out = compile(
            &ast,
            CompileInput {
                cell: &["h8"],
                args: vec![],
                lyp_file: &PathBuf::from(BASIC_LYP),
            },
        );
        let d = out.unwrap_valid();
        // h0..h8 = 9 cells of hierarchy.
        assert_eq!(d.cells.len(), 9, "h8 should instantiate 9 cells deep");
    }

    #[test]
    fn argon_scopes() {
        let o = parse_workspace_with_std(ARGON_SCOPES);
        assert!(o.static_errors().is_empty());
        let ast = o.ast();
        let cell = compile(
            &ast,
            CompileInput {
                cell: &["scopes"],
                args: Vec::new(),
                lyp_file: &PathBuf::from(BASIC_LYP),
            },
        );
        println!("{cell:?}");
    }

    #[test]
    fn argon_immediate() {
        let o = parse_workspace_with_std(ARGON_IMMEDIATE);
        assert!(o.static_errors().is_empty());
        let ast = o.ast();
        let cell = compile(
            &ast,
            CompileInput {
                cell: &["immediate"],
                args: Vec::new(),
                lyp_file: &PathBuf::from(BASIC_LYP),
            },
        );
        println!("{cell:?}");
    }

    #[test]
    fn argon_if() {
        let o = parse_workspace_with_std(ARGON_IF);
        assert!(o.static_errors().is_empty());
        let ast = o.ast();
        let cell = compile(
            &ast,
            CompileInput {
                cell: &["if_test"],
                args: Vec::new(),
                lyp_file: &PathBuf::from(BASIC_LYP),
            },
        );
        println!("{cell:?}");
    }

    #[test]
    fn argon_if_inconsistent() {
        let o = parse_workspace_with_std(ARGON_IF_INCONSISTENT);
        assert!(o.static_errors().is_empty());
        let ast = o.ast();
        let cell = compile(
            &ast,
            CompileInput {
                cell: &["if_test"],
                args: Vec::new(),
                lyp_file: &PathBuf::from(BASIC_LYP),
            },
        );
        println!("{cell:?}");
        cell.unwrap_exec_errors();
    }

    #[test]
    fn argon_via() {
        let o = parse_workspace_with_std(ARGON_VIA);
        assert!(o.static_errors().is_empty());
        let ast = o.ast();
        let cell = compile(
            &ast,
            CompileInput {
                cell: &["via"],
                args: Vec::new(),
                lyp_file: &PathBuf::from(BASIC_LYP),
            },
        );
        println!("{cell:?}");
    }

    #[test]
    fn argon_via_array() {
        let o = parse_workspace_with_std(ARGON_VIA_ARRAY);
        assert!(o.static_errors().is_empty());
        let ast = o.ast();
        let cell = compile(
            &ast,
            CompileInput {
                cell: &["vias"],
                args: Vec::new(),
                lyp_file: &PathBuf::from(BASIC_LYP),
            },
        );
        println!("{cell:?}");
        let cell = cell.unwrap_valid();
        let cell = &cell.cells[&cell.top];
        let n_rects = cell
            .objects
            .iter()
            .filter(|(_, o)| {
                if let SolvedValue::Rect(r) = &o {
                    !r.construction
                } else {
                    false
                }
            })
            .count();
        assert_eq!(n_rects, 27);
    }

    #[test]
    fn argon_func_out_of_order() {
        let o = parse_workspace_with_std(ARGON_FUNC_OUT_OF_ORDER);
        assert!(o.static_errors().is_empty());
        let ast = o.ast();
        let cell = compile(
            &ast,
            CompileInput {
                cell: &["test"],
                args: Vec::new(),
                lyp_file: &PathBuf::from(BASIC_LYP),
            },
        );
        println!("{cell:?}");
    }

    #[test]
    fn argon_hierarchy() {
        let o = parse_workspace_with_std(ARGON_HIERARCHY);
        assert!(o.static_errors().is_empty());
        let ast = o.ast();
        let cells = compile(
            &ast,
            CompileInput {
                cell: &["top"],
                args: Vec::new(),
                lyp_file: &PathBuf::from(BASIC_LYP),
            },
        );
        println!("{cells:#?}");
    }

    #[test]
    fn argon_nested_inst() {
        let o = parse_workspace_with_std(ARGON_NESTED_INST);
        assert!(o.static_errors().is_empty());
        let ast = o.ast();
        let cells = compile(
            &ast,
            CompileInput {
                cell: &["top"],
                args: Vec::new(),
                lyp_file: &PathBuf::from(BASIC_LYP),
            },
        );
        println!("{cells:#?}");
    }

    #[test]
    #[ignore = "not supported"]
    fn argon_cell_out_of_order() {
        let o = parse_workspace_with_std(ARGON_CELL_OUT_OF_ORDER);
        assert!(o.static_errors().is_empty());
        let ast = o.ast();
        let cells = compile(
            &ast,
            CompileInput {
                cell: &["top"],
                args: Vec::new(),
                lyp_file: &PathBuf::from(BASIC_LYP),
            },
        );
        println!("{cells:#?}");
    }

    #[test]
    fn argon_fallback_basic() {
        let o = parse_workspace_with_std(ARGON_FALLBACK_BASIC);
        assert!(o.static_errors().is_empty());
        let ast = o.ast();
        let cells = compile(
            &ast,
            CompileInput {
                cell: &["top"],
                args: Vec::new(),
                lyp_file: &PathBuf::from(BASIC_LYP),
            },
        )
        .unwrap_exec_errors()
        .output
        .unwrap();
        println!("{cells:#?}");
        assert!(!cells.cells[&cells.top].fallback_constraints_used.is_empty());
    }

    #[test]
    fn argon_fallback_inst() {
        let o = parse_workspace_with_std(ARGON_FALLBACK_INST);
        assert!(o.static_errors().is_empty());
        let ast = o.ast();
        let cells = compile(
            &ast,
            CompileInput {
                cell: &["top"],
                args: Vec::new(),
                lyp_file: &PathBuf::from(BASIC_LYP),
            },
        )
        .unwrap_exec_errors()
        .output
        .unwrap();
        assert!(!cells.cells[&cells.top].fallback_constraints_used.is_empty());
        println!("{cells:#?}");
    }

    #[test]
    fn argon_bool_literal() {
        let o = parse_workspace_with_std(ARGON_BOOL_LITERAL);
        assert!(o.static_errors().is_empty());
        let ast = o.ast();
        let cells = compile(
            &ast,
            CompileInput {
                cell: &["top"],
                args: Vec::new(),
                lyp_file: &PathBuf::from(BASIC_LYP),
            },
        );
        println!("{cells:#?}");
        let cells = cells.unwrap_valid();
        let cell = &cells.cells[&cells.top];
        let emit = cell.scopes[&cell.root]
            .children
            .iter()
            .flat_map(|s| cell.scopes[s].emit.iter())
            .collect::<Vec<_>>();
        assert_eq!(emit.len(), 1);
        let (obj, _) = emit.first().unwrap();
        assert_eq!(
            cell.objects[obj]
                .as_ref()
                .unwrap_rect()
                .layer
                .as_ref()
                .unwrap(),
            "met1"
        );
    }

    #[test]
    fn argon_dimensions() {
        let o = parse_workspace_with_std(ARGON_DIMENSIONS);
        assert!(o.static_errors().is_empty());
        let ast = o.ast();
        let cells = compile(
            &ast,
            CompileInput {
                cell: &["top"],
                args: Vec::new(),
                lyp_file: &PathBuf::from(BASIC_LYP),
            },
        );
        println!("{cells:#?}");
        let cells = cells.unwrap_valid();
        let cell = &cells.cells[&cells.top];
        assert_eq!(cell.objects.len(), 3);
        let r = cell.objects.iter().find_map(|(_, v)| v.get_rect()).unwrap();
        assert_eq!(r.layer.as_ref().unwrap(), "met1");
        assert_relative_eq!(r.x0.0, 0., epsilon = EPSILON);
        assert_relative_eq!(r.y0.0, 0., epsilon = EPSILON);
        assert_relative_eq!(r.x1.0, 200., epsilon = EPSILON);
        assert_relative_eq!(r.y1.0, 100., epsilon = EPSILON);
    }

    #[test]
    fn argon_param_float() {
        let o = parse_workspace_with_std(ARGON_PARAM_FLOAT);
        assert!(o.static_errors().is_empty());
        let ast = o.ast();
        let cells = compile(
            &ast,
            CompileInput {
                cell: &["top"],
                args: vec![CellArg::Float(50.), CellArg::Float(20.)],
                lyp_file: &PathBuf::from(BASIC_LYP),
            },
        );
        println!("{cells:#?}");
        cells.unwrap_valid();
    }

    #[test]
    fn argon_param_int() {
        let o = parse_workspace_with_std(ARGON_PARAM_INT);
        assert!(o.static_errors().is_empty());
        let ast = o.ast();
        let cells = compile(
            &ast,
            CompileInput {
                cell: &["top"],
                args: vec![CellArg::Int(50), CellArg::Int(20)],
                lyp_file: &PathBuf::from(BASIC_LYP),
            },
        );
        println!("{cells:#?}");
        cells.unwrap_valid();
    }

    #[test]
    fn argon_workspace() {
        let o = parse_workspace_with_std(ARGON_WORKSPACE);
        assert!(o.static_errors().is_empty());
        let ast = o.ast();
        let cells = compile(
            &ast,
            CompileInput {
                cell: &["test"],
                args: Vec::new(),
                lyp_file: &PathBuf::from(BASIC_LYP),
            },
        );
        println!("{cells:?}");
        let cells = cells.unwrap_valid();
        let cell = &cells.cells[&cells.top];
        assert_eq!(cell.objects.len(), 1);
        let r = cell.objects.iter().next().unwrap().1.as_ref().unwrap_rect();
        assert_relative_eq!(r.x0.0, 0., epsilon = EPSILON);
        assert_relative_eq!(r.y0.0, 0., epsilon = EPSILON);
        assert_relative_eq!(r.x1.0, 10., epsilon = EPSILON);
        assert_relative_eq!(r.y1.0, 15., epsilon = EPSILON);
    }

    #[test]
    fn argon_external_mods() {
        let o = parse_workspace_with_std(ARGON_EXTERNAL_MODS);
        assert!(o.static_errors().is_empty());
        let ast = o.ast();
        let cells = compile(
            &ast,
            CompileInput {
                cell: &["test"],
                args: Vec::new(),
                lyp_file: &PathBuf::from(BASIC_LYP),
            },
        );
        println!("{cells:?}");
        let cells = cells.unwrap_valid();
        let cell = &cells.cells[&cells.top];
        assert_eq!(cell.objects.len(), 1);
        let r = cell.objects.iter().next().unwrap().1.as_ref().unwrap_rect();
        assert_relative_eq!(r.x0.0, 0., epsilon = EPSILON);
        assert_relative_eq!(r.y0.0, 0., epsilon = EPSILON);
        assert_relative_eq!(r.x1.0, 10., epsilon = EPSILON);
        assert_relative_eq!(r.y1.0, 20., epsilon = EPSILON);
    }

    #[test]
    fn argon_sky130_inverter() {
        let o = parse_workspace_with_std(ARGON_SKY130_LIB);
        assert!(o.static_errors().is_empty());
        let ast = o.ast();
        let cells = compile(
            &ast,
            CompileInput {
                cell: &["inv"],
                args: vec![
                    CellArg::Float(1_200.),
                    CellArg::Float(2_000.),
                    CellArg::Int(4),
                ],
                lyp_file: &PathBuf::from(SKY130_LYP),
            },
        );
        println!("cells: {cells:?}");

        assert!(cells.is_valid());

        let work_dir =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("build/argon_sky130_inverter");
        cells
            .to_gds(
                GdsMap::from_lyp(SKY130_LYP).expect("failed to create GDS map"),
                GdsUnits::new(1e-3, 1e-9),
                work_dir.join("layout.gds"),
            )
            .expect("Failed to write to GDS");
    }

    #[test]
    fn argon_enumerations() {
        let o = parse_workspace_with_std(ARGON_ENUMERATIONS);
        assert!(o.static_errors().is_empty());
        let ast = o.ast();
        let cells = compile(
            &ast,
            CompileInput {
                cell: &["top"],
                args: Vec::new(),
                lyp_file: &PathBuf::from(BASIC_LYP),
            },
        );
        println!("{cells:?}");
        let cells = cells.unwrap_valid();
        let cell = &cells.cells[&cells.top];
        assert_eq!(cell.objects.len(), 1);
        let r = cell.objects.iter().next().unwrap().1.as_ref().unwrap_rect();
        assert_eq!(r.layer.as_deref(), Some("met2"));
        assert_relative_eq!(r.x0.0, 100., epsilon = EPSILON);
        assert_relative_eq!(r.y0.0, 0., epsilon = EPSILON);
        assert_relative_eq!(r.x1.0, 300., epsilon = EPSILON);
        assert_relative_eq!(r.y1.0, 400., epsilon = EPSILON);
    }

    #[test]
    fn argon_bbox() {
        let o = parse_workspace_with_std(ARGON_BBOX);
        assert!(o.static_errors().is_empty());
        let ast = o.ast();
        let cells = compile(
            &ast,
            CompileInput {
                cell: &["top"],
                args: Vec::new(),
                lyp_file: &PathBuf::from(BASIC_LYP),
            },
        );
        println!("{cells:#?}");
        let cells = cells.unwrap_valid();
        let cell = &cells.cells[&cells.top];
        assert_eq!(cell.objects.len(), 5);
    }

    #[test]
    fn argon_rounding() {
        let o = parse_workspace_with_std(ARGON_ROUNDING);
        assert!(o.static_errors().is_empty());
        let ast = o.ast();
        let cells = compile(
            &ast,
            CompileInput {
                cell: &["top"],
                args: Vec::new(),
                lyp_file: &PathBuf::from(BASIC_LYP),
            },
        );
        println!("{cells:#?}");
        let cells = cells.unwrap_exec_errors();
        assert_eq!(cells.errors.len(), 1);
        assert!(matches!(
            cells.errors.first().unwrap().kind,
            ExecErrorKind::InvalidRounding(_)
        ));
    }

    #[test]
    fn argon_flipped_rect() {
        let o = parse_workspace_with_std(ARGON_FLIPPED_RECT);
        assert!(o.static_errors().is_empty());
        let ast = o.ast();
        let cells = compile(
            &ast,
            CompileInput {
                cell: &["top"],
                args: Vec::new(),
                lyp_file: &PathBuf::from(BASIC_LYP),
            },
        );
        println!("{cells:#?}");
        let cells = cells.unwrap_exec_errors();
        assert_eq!(cells.errors.len(), 2);
        assert!(matches!(
            cells.errors[0].kind,
            ExecErrorKind::FlippedRect(_)
        ));
        assert!(matches!(
            cells.errors[1].kind,
            ExecErrorKind::FlippedRect(_)
        ));
    }

    #[test]
    fn argon_seq_basic() {
        let o = parse_workspace_with_std(ARGON_SEQ_BASIC);
        assert!(o.static_errors().is_empty());
        let ast = o.ast();
        let cells = compile(
            &ast,
            CompileInput {
                cell: &["top"],
                args: Vec::new(),
                lyp_file: &PathBuf::from(BASIC_LYP),
            },
        );
        println!("{cells:#?}");
        let cells = cells.unwrap_valid();
        let cell = &cells.cells[&cells.top];
        assert_eq!(cell.objects.len(), 1);
        let r = cell.objects.iter().find_map(|(_, v)| v.get_rect()).unwrap();
        assert_eq!(r.layer.as_ref().unwrap(), "met1");
        assert_relative_eq!(r.x0.0, 0., epsilon = EPSILON);
        assert_relative_eq!(r.y0.0, 0., epsilon = EPSILON);
        assert_relative_eq!(r.x1.0, 400., epsilon = EPSILON);
        assert_relative_eq!(r.y1.0, 200., epsilon = EPSILON);
    }

    #[test]
    fn argon_seq_any() {
        let o = parse_workspace_with_std(ARGON_SEQ_ANY);
        assert!(o.static_errors().is_empty());
        let ast = o.ast();
        let cells = compile(
            &ast,
            CompileInput {
                cell: &["top"],
                args: Vec::new(),
                lyp_file: &PathBuf::from(BASIC_LYP),
            },
        );
        println!("{cells:#?}");
        let cells = cells.unwrap_valid();
        let cell = &cells.cells[&cells.top];
        assert_eq!(cell.objects.len(), 1);
        let r = cell.objects.iter().find_map(|(_, v)| v.get_rect()).unwrap();
        assert_eq!(r.layer.as_ref().unwrap(), "met1");
        assert_relative_eq!(r.x0.0, 0., epsilon = EPSILON);
        assert_relative_eq!(r.y0.0, 0., epsilon = EPSILON);
        assert_relative_eq!(r.x1.0, 400., epsilon = EPSILON);
        assert_relative_eq!(r.y1.0, 200., epsilon = EPSILON);
    }

    #[test]
    fn argon_seq_fn() {
        let o = parse_workspace_with_std(ARGON_SEQ_FN);
        assert!(o.static_errors().is_empty());
        let ast = o.ast();
        let cells = compile(
            &ast,
            CompileInput {
                cell: &["top"],
                args: Vec::new(),
                lyp_file: &PathBuf::from(BASIC_LYP),
            },
        );
        println!("{cells:#?}");
        let cells = cells.unwrap_valid();
        let cell = &cells.cells[&cells.top];
        assert_eq!(cell.objects.len(), 1);
        let r = cell.objects.iter().find_map(|(_, v)| v.get_rect()).unwrap();
        assert_eq!(r.layer.as_ref().unwrap(), "met1");
        assert_relative_eq!(r.x0.0, 0., epsilon = EPSILON);
        assert_relative_eq!(r.y0.0, 0., epsilon = EPSILON);
        assert_relative_eq!(r.x1.0, 400., epsilon = EPSILON);
        assert_relative_eq!(r.y1.0, 1250., epsilon = EPSILON);
    }

    #[test]
    fn argon_seq_recur() {
        let o = parse_workspace_with_std(ARGON_SEQ_RECUR);
        assert!(o.static_errors().is_empty());
        let ast = o.ast();
        let cells = compile(
            &ast,
            CompileInput {
                cell: &["top"],
                args: Vec::new(),
                lyp_file: &PathBuf::from(BASIC_LYP),
            },
        );
        println!("{cells:#?}");
        let cells = cells.unwrap_valid();
        let cell = &cells.cells[&cells.top];
        assert_eq!(cell.objects.len(), 1);
        let r = cell.objects.iter().find_map(|(_, v)| v.get_rect()).unwrap();
        assert_eq!(r.layer.as_ref().unwrap(), "met1");
        assert_relative_eq!(r.x0.0, 0., epsilon = EPSILON);
        assert_relative_eq!(r.y0.0, 0., epsilon = EPSILON);
        assert_relative_eq!(r.x1.0, 400., epsilon = EPSILON);
        assert_relative_eq!(r.y1.0, 1200., epsilon = EPSILON);
    }

    #[test]
    fn argon_lub_match() {
        let o = parse_workspace_with_std(ARGON_LUB_MATCH);
        assert!(o.static_errors().is_empty());
        let ast = o.ast();
        let cells = compile(
            &ast,
            CompileInput {
                cell: &["top"],
                args: Vec::new(),
                lyp_file: &PathBuf::from(BASIC_LYP),
            },
        );
        println!("{cells:#?}");
        let cells = cells.unwrap_valid();
        let cell = &cells.cells[&cells.top];
        assert_eq!(cell.objects.len(), 1);
        let r = cell.objects.iter().find_map(|(_, v)| v.get_rect()).unwrap();
        assert_eq!(r.layer.as_ref().unwrap(), "met1");
        assert_relative_eq!(r.x0.0, 0., epsilon = EPSILON);
        assert_relative_eq!(r.y0.0, 0., epsilon = EPSILON);
        assert_relative_eq!(r.x1.0, 400., epsilon = EPSILON);
        assert_relative_eq!(r.y1.0, 200., epsilon = EPSILON);
    }

    #[test]
    fn argon_seq_cell() {
        let o = parse_workspace_with_std(ARGON_SEQ_CELL);
        assert!(o.static_errors().is_empty());
        let ast = o.ast();
        let cells = compile(
            &ast,
            CompileInput {
                cell: &["top"],
                args: Vec::new(),
                lyp_file: &PathBuf::from(BASIC_LYP),
            },
        );
        println!("{cells:#?}");
        let cells = cells.unwrap_valid();
        let cell = &cells.cells[&cells.top];
        assert!(cell.objects.len() >= 3);
        let inst = cell
            .objects
            .iter()
            .find_map(|(_, v)| v.get_instance())
            .unwrap();
        assert_relative_eq!(inst.x, 2000., epsilon = EPSILON);
        assert_relative_eq!(inst.y, 3000., epsilon = EPSILON);
    }

    #[test]
    fn argon_text() {
        let o = parse_workspace_with_std(ARGON_TEXT);
        assert!(o.static_errors().is_empty());
        let ast = o.ast();
        let cells = compile(
            &ast,
            CompileInput {
                cell: &["top"],
                args: Vec::new(),
                lyp_file: &PathBuf::from(SKY130_LYP),
            },
        );
        println!("{cells:#?}");

        let work_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("build/argon_text");
        cells
            .to_gds(
                GdsMap::from_lyp(SKY130_LYP).expect("failed to create GDS map"),
                GdsUnits::new(1e-3, 1e-9),
                work_dir.join("layout.gds"),
            )
            .expect("Failed to write to GDS");

        let cells = cells.unwrap_valid();
        let cell = &cells.cells[&cells.top];
        assert_eq!(cell.objects.len(), 2);
        let t = cell.objects.iter().find_map(|(_, v)| v.get_text()).unwrap();
        assert_eq!(t.layer, "met1.label");
        assert_eq!(t.text, "mytext");
        assert_relative_eq!(t.x, 0., epsilon = EPSILON);
        assert_relative_eq!(t.y, 10., epsilon = EPSILON);
    }

    #[test]
    fn argon_any_type_inst() {
        let o = parse_workspace_with_std(ARGON_ANY_TYPE);
        assert!(o.static_errors().is_empty());
        let ast = o.ast();
        let cells = compile(
            &ast,
            CompileInput {
                cell: &["top"],
                args: Vec::new(),
                lyp_file: &PathBuf::from(BASIC_LYP),
            },
        );
        println!("{cells:#?}");

        let cells = cells.unwrap_valid();
        let cell = &cells.cells[&cells.top];
        assert_eq!(cell.objects.len(), 3);

        let r = cell.objects.iter().find_map(|(_, v)| v.get_rect()).unwrap();
        assert_eq!(r.layer.as_ref().unwrap(), "met1");
        assert_relative_eq!(r.x0.0, 200., epsilon = EPSILON);
        assert_relative_eq!(r.y0.0, 0., epsilon = EPSILON);
        assert_relative_eq!(r.x1.0, 300., epsilon = EPSILON);
        assert_relative_eq!(r.y1.0, 500., epsilon = EPSILON);
    }

    #[test]
    fn argon_seq_index() {
        let o = parse_workspace_with_std(ARGON_SEQ_INDEX);
        assert!(o.static_errors().is_empty());
        let ast = o.ast();
        let cells = compile(
            &ast,
            CompileInput {
                cell: &["top"],
                args: Vec::new(),
                lyp_file: &PathBuf::from(BASIC_LYP),
            },
        );
        println!("{cells:#?}");

        let cells = cells.unwrap_valid();
        let cell = &cells.cells[&cells.top];
        assert_eq!(cell.objects.len(), 3);

        let r = cell.objects.iter().find_map(|(_, v)| v.get_rect()).unwrap();
        assert_eq!(r.layer.as_ref().unwrap(), "met1");
        assert_relative_eq!(r.x0.0, 200., epsilon = EPSILON);
        assert_relative_eq!(r.y0.0, 0., epsilon = EPSILON);
        assert_relative_eq!(r.x1.0, 300., epsilon = EPSILON);
        assert_relative_eq!(r.y1.0, 500., epsilon = EPSILON);
    }

    #[test]
    fn argon_seq_constructor() {
        let o = parse_workspace_with_std(ARGON_SEQ_CONSTRUCTOR);
        assert!(o.static_errors().is_empty());
        let ast = o.ast();
        let cells = compile(
            &ast,
            CompileInput {
                cell: &["top"],
                args: Vec::new(),
                lyp_file: &PathBuf::from(BASIC_LYP),
            },
        );
        println!("{cells:#?}");

        let cells = cells.unwrap_valid();
        let cell = &cells.cells[&cells.top];
        assert_eq!(cell.objects.len(), 3);

        let r = cell.objects.iter().find_map(|(_, v)| v.get_rect()).unwrap();
        assert_eq!(r.layer.as_ref().unwrap(), "met1");
        assert_relative_eq!(r.x0.0, 200., epsilon = EPSILON);
        assert_relative_eq!(r.y0.0, 0., epsilon = EPSILON);
        assert_relative_eq!(r.x1.0, 300., epsilon = EPSILON);
        assert_relative_eq!(r.y1.0, 500., epsilon = EPSILON);
    }

    #[test]
    fn argon_func_bad_arg_reuse() {
        let o = parse_workspace_with_std(ARGON_FUNC_BAD_ARG_REUSE);
        assert!(o.static_errors().is_empty());
        let ast = o.ast();
        let cells = compile(
            &ast,
            CompileInput {
                cell: &["top"],
                args: Vec::new(),
                lyp_file: &PathBuf::from(BASIC_LYP),
            },
        );
        println!("{cells:#?}");

        let errors = cells.unwrap_static_errors();
        assert!(
            errors
                .errors
                .iter()
                .any(|e| matches!(e.kind, StaticErrorKind::UndeclaredVar))
        );
    }

    #[test]
    fn argon_cell_bad_arg_reuse() {
        let o = parse_workspace_with_std(ARGON_CELL_BAD_ARG_REUSE);
        assert!(o.static_errors().is_empty());
        let ast = o.ast();
        let cells = compile(
            &ast,
            CompileInput {
                cell: &["top"],
                args: Vec::new(),
                lyp_file: &PathBuf::from(BASIC_LYP),
            },
        );
        println!("{cells:#?}");

        let errors = cells.unwrap_static_errors();
        assert!(
            errors
                .errors
                .iter()
                .any(|e| matches!(e.kind, StaticErrorKind::UndeclaredVar))
        );
    }

    #[test]
    fn argon_partially_constrained_inst() {
        let o = parse_workspace_with_std(ARGON_PARTIALLY_CONSTRAINED_INST);
        assert!(o.static_errors().is_empty());
        let ast = o.ast();
        let cells = compile(
            &ast,
            CompileInput {
                cell: &["top"],
                args: Vec::new(),
                lyp_file: &PathBuf::from(BASIC_LYP),
            },
        );
        println!("{cells:#?}");

        let errors = cells.unwrap_exec_errors();
        assert!(
            errors
                .errors
                .iter()
                .any(|e| matches!(e.kind, ExecErrorKind::Underconstrained))
        );
    }

    #[test]
    fn argon_invalid_cast() {
        let o = parse_workspace_with_std(ARGON_INVALID_CAST);
        assert!(o.static_errors().is_empty());
        let ast = o.ast();
        let cells = compile(
            &ast,
            CompileInput {
                cell: &["top"],
                args: Vec::new(),
                lyp_file: &PathBuf::from(BASIC_LYP),
            },
        );
        println!("{cells:#?}");

        let errors = cells.unwrap_exec_errors();
        assert!(
            errors
                .errors
                .iter()
                .any(|e| matches!(e.kind, ExecErrorKind::InvalidCast))
        );
    }

    #[test]
    fn argon_tuple_basic() {
        let o = parse_workspace_with_std(ARGON_TUPLE_BASIC);
        assert!(o.static_errors().is_empty());
        let ast = o.ast();
        let cells = compile(
            &ast,
            CompileInput {
                cell: &["top"],
                args: Vec::new(),
                lyp_file: &PathBuf::from(BASIC_LYP),
            },
        );
        println!("{cells:#?}");

        let cells = cells.unwrap_valid();
        let cell = &cells.cells[&cells.top];
        assert_eq!(cell.objects.len(), 2);

        let r = cell
            .objects
            .iter()
            .find_map(|(_, v)| v.get_rect().filter(|&r| r.layer == Some("met1".into())))
            .unwrap();
        assert_relative_eq!(r.x0.0, 100., epsilon = EPSILON);
        assert_relative_eq!(r.y0.0, 200., epsilon = EPSILON);
        assert_relative_eq!(r.x1.0, 300., epsilon = EPSILON);
        assert_relative_eq!(r.y1.0, 400., epsilon = EPSILON);

        let r = cell
            .objects
            .iter()
            .find_map(|(_, v)| v.get_rect().filter(|&r| r.layer == Some("met2".into())))
            .unwrap();
        assert_relative_eq!(r.x0.0, 3., epsilon = EPSILON);
        assert_relative_eq!(r.y0.0, 5., epsilon = EPSILON);
        assert_relative_eq!(r.x1.0, 25., epsilon = EPSILON);
        assert_relative_eq!(r.y1.0, 53., epsilon = EPSILON);
    }

    #[test]
    fn argon_tuple_any() {
        let o = parse_workspace_with_std(ARGON_TUPLE_ANY);
        assert!(o.static_errors().is_empty());
        let ast = o.ast();
        let cells = compile(
            &ast,
            CompileInput {
                cell: &["top"],
                args: Vec::new(),
                lyp_file: &PathBuf::from(BASIC_LYP),
            },
        );
        println!("{cells:#?}");

        let cells = cells.unwrap_valid();
        let cell = &cells.cells[&cells.top];
        assert_eq!(cell.objects.len(), 1);

        let r = cell.objects.iter().find_map(|(_, v)| v.get_rect()).unwrap();
        assert_eq!(r.layer.as_ref().unwrap(), "met1");
        assert_relative_eq!(r.x0.0, 60., epsilon = EPSILON);
        assert_relative_eq!(r.y0.0, 40., epsilon = EPSILON);
        assert_relative_eq!(r.x1.0, 140., epsilon = EPSILON);
        assert_relative_eq!(r.y1.0, 150., epsilon = EPSILON);
    }

    #[test]
    fn argon_for_loop_basic() {
        let o = parse_workspace_with_std(ARGON_FOR_LOOP_BASIC);
        assert!(o.static_errors().is_empty());
        let ast = o.ast();
        let cells = compile(
            &ast,
            CompileInput {
                cell: &["top"],
                args: Vec::new(),
                lyp_file: &PathBuf::from(BASIC_LYP),
            },
        );
        println!("{cells:#?}");

        let cells = cells.unwrap_valid();
        let cell = &cells.cells[&cells.top];
        assert_eq!(cell.objects.len(), 5);

        for w in [500., 300., 800., 200., 1400.] {
            let r = cell
                .objects
                .iter()
                .find_map(|(_, v)| {
                    v.get_rect()
                        .filter(|r| relative_eq!(r.x1.0, w, epsilon = EPSILON))
                })
                .unwrap();
            assert_eq!(r.layer.as_ref().unwrap(), "met1");
            assert_relative_eq!(r.x0.0, 0., epsilon = EPSILON);
            assert_relative_eq!(r.y0.0, 0., epsilon = EPSILON);
            assert_relative_eq!(r.x1.0, w, epsilon = EPSILON);
            assert_relative_eq!(r.y1.0, 100., epsilon = EPSILON);
        }
    }

    /// Regression guard against O(n^2) `for` loops over `range`.
    ///
    /// Under the old `cons`-based `range`, building `range(20000)` cloned and
    /// front-inserted a growing `Vec` per element (~2e8 element copies) and took
    /// many seconds; with the persistent-vector backing for `Value::Seq` plus the
    /// native `range_full` builtin it is O(n) and completes near-instantly. The
    /// generous time bound separates the linear fix from an O(n^2) regression
    /// (which would take minutes) without being flaky across build profiles.
    #[test]
    fn argon_range_perf() {
        let o = parse_workspace_with_std(ARGON_RANGE_PERF);
        assert!(o.static_errors().is_empty());
        let ast = o.ast();
        let start = std::time::Instant::now();
        let cells = compile(
            &ast,
            CompileInput {
                cell: &["top"],
                args: Vec::new(),
                lyp_file: &PathBuf::from(BASIC_LYP),
            },
        );
        let elapsed = start.elapsed();
        let cells = cells.unwrap_valid();
        let cell = &cells.cells[&cells.top];
        assert_eq!(cell.objects.len(), 20000);
        assert!(
            elapsed < std::time::Duration::from_secs(30),
            "compiling `for i in std::range(20000)` took {elapsed:?}; \
             expected near-linear time (O(n^2) regression in `range`/`cons`?)"
        );
    }

    #[test]
    fn argon_sse_basic() {
        let o = parse_workspace_with_std(ARGON_SSE_BASIC);
        assert!(o.static_errors().is_empty());
        let ast = o.ast();
        let cells = compile(
            &ast,
            CompileInput {
                cell: &["top"],
                args: Vec::new(),
                lyp_file: &PathBuf::from(BASIC_LYP),
            },
        );
        println!("{cells:#?}");

        let cells = cells.unwrap_exec_errors().output.unwrap();
        let cell = &cells.cells[&cells.top];
        println!("rowspace vecs = {:?}", cell.rowspace_vecs);
        assert_eq!(cell.rowspace_vecs.len(), 1);
    }

    #[test]
    fn argon_precedence() {
        let o = parse_workspace_with_std(ARGON_PRECEDENCE);
        assert!(o.static_errors().is_empty());
        let ast = o.ast();
        let cells = compile(
            &ast,
            CompileInput {
                cell: &["precedence"],
                args: Vec::new(),
                lyp_file: &PathBuf::from(BASIC_LYP),
            },
        );
        println!("{cells:#?}");

        let cells = cells.unwrap_valid();
        let cell = &cells.cells[&cells.top];
        assert_eq!(
            cell.objects
                .first()
                .unwrap()
                .1
                .clone()
                .unwrap_rect()
                .x0
                .0
                .round() as i64,
            -8
        );
    }

    #[test]
    #[ignore = "requires Pegasus"]
    fn argon_sky130_vco() {
        let o = parse_workspace_with_std(ARGON_SKY130_LIB);
        assert!(o.static_errors().is_empty());
        let ast = o.ast();
        let cells = compile(
            &ast,
            CompileInput {
                cell: &["diff_vco_top"],
                args: vec![],
                lyp_file: &PathBuf::from(SKY130_LYP),
            },
        );
        println!("cells: {cells:?}");

        assert!(cells.is_valid());

        let work_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("build/argon_sky130_vco");
        let gds_path = work_dir.join("layout.gds");
        cells
            .to_gds(
                GdsMap::from_lyp(SKY130_LYP).expect("failed to create GDS map"),
                GdsUnits::new(1e-3, 1e-9),
                &gds_path,
            )
            .expect("Failed to write to GDS");

        use sky130::{sky130_drc, sky130_drc_rules_path};

        let drc_dir = work_dir.join("drc");
        let data = run_drc(&DrcParams {
            work_dir: &drc_dir,
            layout_path: &gds_path,
            cell_name: "diff_vco_top",
            rules_dir: &sky130_drc(),
            rules_path: &sky130_drc_rules_path(),
        })
        .expect("failed to run drc");
        assert!(data.rule_checks.is_empty());
    }
}
