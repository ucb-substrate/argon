pub mod artifact;
pub mod ast;
pub mod cellcache;
pub mod compile;
pub mod diagnostics;
pub mod fingerprint;
pub mod gds;
pub mod gdscache;
pub mod incremental;
pub mod nav;
pub mod parse;
mod parser;
pub mod solver;
pub mod tech;
pub mod workspace;

pub use workspace::WorkspaceConfig;

/// Native stack reserved for compilation.
///
/// A stack overflow aborts the process rather than unwinding, so no
/// `catch_unwind` can turn it into a diagnostic: the stack has to be large
/// enough that the depth limit is what stops the descent.
pub const COMPILE_STACK_SIZE: usize = 1024 * 1024 * 1024;

/// Runs `f` on a thread with [`COMPILE_STACK_SIZE`] of stack, propagating a
/// panic to the caller so `catch_unwind` still sees it.
pub fn run_with_stack<T: Send + 'static>(name: &str, f: impl FnOnce() -> T + Send + 'static) -> T {
    let handle = std::thread::Builder::new()
        .name(name.to_owned())
        .stack_size(COMPILE_STACK_SIZE)
        .spawn(f)
        .expect("spawn compilation thread");
    match handle.join() {
        Ok(value) => value,
        Err(payload) => std::panic::resume_unwind(payload),
    }
}

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
        compile::{
            CellId, CompiledData, ExecErrorKind, MAX_TEXT_LEN, RESERVED_CELL_FIELDS,
            RectInitialCondition, SolvedValue, StaticErrorKind, static_compile,
        },
        parse::{parse_source_text, parse_workspace_with_std, parse_workspace_with_std_and_deps},
    };
    use ::gds::{GdsElement, GdsLibrary};
    use approx::assert_relative_eq;
    use approx::relative_eq;
    use const_format::concatcp;
    use indexmap::IndexMap;
    use pegasus::drc::{DrcParams, run_drc};

    use crate::{
        WorkspaceConfig,
        compile::{CellArg, CompileInput, compile as compile_workspace},
    };
    const EPSILON: f64 = 1e-10;

    const EXAMPLES_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../examples");
    const ARGON_SCOPES: &str = concatcp!(EXAMPLES_DIR, "/scopes/lib.ar");
    const BASIC_TECH: &str = concatcp!(EXAMPLES_DIR, "/tech/basic.tech.toml");
    const ARGON_SKY130_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../pdks/sky130");
    const ARGON_SKY130_LIB: &str = concatcp!(ARGON_SKY130_DIR, "/lib.ar");
    const SKY130_TECH: &str = concatcp!(ARGON_SKY130_DIR, "/sky130.tech.toml");

    fn compile(ast: &crate::parse::WorkspaceParseAst, input: CompileInput<'_>) -> CompileOutput {
        compile_workspace(
            ast,
            input,
            &WorkspaceConfig::default().with_tech(Some(PathBuf::from(BASIC_TECH))),
        )
    }

    fn compile_sky130(
        ast: &crate::parse::WorkspaceParseAst,
        input: CompileInput<'_>,
    ) -> CompileOutput {
        compile_workspace(
            ast,
            input,
            &WorkspaceConfig::default().with_tech(Some(PathBuf::from(SKY130_TECH))),
        )
    }

    /// Compiles `cell` from a one-file workspace without the standard library.
    fn compile_source(source: &str, cell: &str, args: Vec<CellArg>) -> CompileOutput {
        let root = parse_source_text(source, PathBuf::from("/virtual/lib.ar")).unwrap();
        let ast = IndexMap::from([(Vec::new(), root)]);
        compile(
            &ast,
            CompileInput {
                cell: &[cell],
                args,
            },
        )
    }
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
    const ARGON_BBOX_NESTED: &str = concatcp!(EXAMPLES_DIR, "/bbox_nested/lib.ar");
    const ARGON_ROUNDING: &str = concatcp!(EXAMPLES_DIR, "/rounding/lib.ar");
    const ARGON_FLIPPED_RECT: &str = concatcp!(EXAMPLES_DIR, "/flipped_rect/lib.ar");
    const ARGON_SEQ_BASIC: &str = concatcp!(EXAMPLES_DIR, "/seq_basic/lib.ar");
    const ARGON_SEQ_ANY: &str = concatcp!(EXAMPLES_DIR, "/seq_any/lib.ar");
    const ARGON_SEQ_FN: &str = concatcp!(EXAMPLES_DIR, "/seq_fn/lib.ar");
    const ARGON_SEQ_RECUR: &str = concatcp!(EXAMPLES_DIR, "/seq_recur/lib.ar");
    const ARGON_LUB_MATCH: &str = concatcp!(EXAMPLES_DIR, "/lub_match/lib.ar");
    const ARGON_SEQ_CELL: &str = concatcp!(EXAMPLES_DIR, "/seq_cell/lib.ar");
    const ARGON_LIBRARY: &str = concatcp!(EXAMPLES_DIR, "/argon_library/lib.ar");
    const ARGON_PATH_DEPENDENCIES: &str =
        concatcp!(EXAMPLES_DIR, "/path_dependencies/root_library/lib.ar");
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
    const ARGON_POLYGON: &str = concatcp!(EXAMPLES_DIR, "/polygon/lib.ar");
    const ARGON_PATH: &str = concatcp!(EXAMPLES_DIR, "/path/lib.ar");
    const ARGON_KWARGS_FN: &str = concatcp!(EXAMPLES_DIR, "/kwargs_fn/lib.ar");
    const ARGON_KWARGS_CELL: &str = concatcp!(EXAMPLES_DIR, "/kwargs_cell/lib.ar");
    const ARGON_STRUCTS: &str = concatcp!(EXAMPLES_DIR, "/structs/lib.ar");
<<<<<<< HEAD
    const ARGON_RECURSIVE_CELL: &str = concatcp!(EXAMPLES_DIR, "/recursive_cell/lib.ar");
=======
    const ARGON_SHAPE_CELL_ARGS: &str = concatcp!(EXAMPLES_DIR, "/shape_cell_args/lib.ar");
>>>>>>> 8fbeb74 (feat(lang): support rects/tuples/etc. as cell arguments (#256))

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
    //     RUSTFLAGS=... cargo test -p argonc --release -- \
    //         --ignored --test-threads=1 bench_
    //
    // The `stress_*_smoke` tests below run in the normal (debug) test suite and
    // just check that each example still compiles.
    // ---------------------------------------------------------------------
    const ARGON_STRESS_SHAPES: &str = concatcp!(EXAMPLES_DIR, "/stress_shapes/lib.ar");
    const ARGON_STRESS_CONSTRAINTS: &str = concatcp!(EXAMPLES_DIR, "/stress_constraints/lib.ar");
    const ARGON_STRESS_INSTANCES: &str = concatcp!(EXAMPLES_DIR, "/stress_instances/lib.ar");
    const ARGON_STRESS_HIERARCHY: &str = concatcp!(EXAMPLES_DIR, "/stress_hierarchy/lib.ar");
    const ARGON_STRESS_FALLBACKS: &str = concatcp!(EXAMPLES_DIR, "/stress_fallbacks/lib.ar");
    const ARGON_STRESS_FREE: &str = concatcp!(EXAMPLES_DIR, "/stress_free/lib.ar");

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
    ///         cargo test -p argonc --release -- --ignored --test-threads=1 \
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
    /// child cell is also bound to a `let`, so `h{k}` references the type of
    /// `h{k-1}` through two bindings rather than one. Because the structural
    /// cell type is shared (`Arc`-interned) rather than copied per reference,
    /// both variants scale linearly in `depth`.
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

    /// Builds a cyclic tridiagonal system in which every constraint has three
    /// unknowns. Neither unary back-substitution nor the size-2 elimination
    /// pass can reduce it, so it exercises the sparse component solver.
    fn sparse_solver_fixture(
        n: usize,
    ) -> (crate::solver::Solver, Vec<crate::solver::Var>, Vec<f64>) {
        use crate::solver::{LinearExpr, Solver};

        let mut solver = Solver::new();
        let vars: Vec<_> = (0..n).map(|_| solver.new_var()).collect();
        let expected: Vec<_> = (0..n).map(|i| ((i % 11) as f64 - 5.) * 0.1).collect();
        for i in 0..n {
            let previous = (i + n - 1) % n;
            let next = (i + 1) % n;
            // The diagonal magnitude (2) is smaller than the sum of the two
            // off-diagonal magnitudes (2.1), making this a representative
            // general sparse-QR fixture rather than a specially dominant one.
            let rhs = -expected[previous] + 2. * expected[i] - 1.1 * expected[next];
            solver.constrain_eq0(LinearExpr {
                coeffs: vec![(-1., vars[previous]), (2., vars[i]), (-1.1, vars[next])],
                constant: -rhs,
            });
        }
        assert!(vars.iter().all(|&var| !solver.is_solved(var)));
        (solver, vars, expected)
    }

    /// General sparse-QR kernel: a non-diagonally-dominant connected component
    /// with three unknowns per row, timed after fixture construction so only
    /// `Solver::solve()` is measured.
    #[test]
    #[ignore = "scaling benchmark; run in release, serially: cargo test -p argonc --release -- --ignored --test-threads=1 bench_"]
    fn bench_sparse_solver() {
        let _g = bench_guard();
        let mut rows = Vec::new();
        for &n in &bench_sizes(
            "ARGON_BENCH_SPARSE_SOLVER",
            &[256, 512, 1024, 2048, 4096, 8192, 16384],
        ) {
            let n = usize::try_from(n).expect("sparse solver benchmark sizes must be positive");
            let (template, vars, expected) = sparse_solver_fixture(n);
            let mut best = std::time::Duration::MAX;
            let mut peak = 0usize;

            for _ in 0..5 {
                let mut solver = template.clone();
                crate::bench_alloc::reset_peak();
                let base = crate::bench_alloc::live();
                let start = std::time::Instant::now();
                solver.solve();
                best = best.min(start.elapsed());
                peak = peak.max(crate::bench_alloc::peak().saturating_sub(base));

                assert!(solver.fully_solved());
                for (&var, &value) in vars.iter().zip(&expected) {
                    assert!((solver.value_of(var).unwrap() - value).abs() < 1e-8);
                }
            }

            eprintln!(
                "sparse_solver n={n:>6} time={best:>11.3?} peak={:>8.2} MiB",
                peak as f64 / (1usize << 20) as f64
            );
            rows.push((n as f64, best.as_secs_f64(), peak, n));
        }
        write_bench_csv("sparse_solver", &rows);
    }

    /// Builds a connected banded system with `n - 2` independent three-variable
    /// constraints and exactly two degrees of freedom. This is the SSE workload:
    /// compute a particular solution and an orthonormal null-space basis.
    fn sparse_sse_fixture(n: usize) -> (crate::solver::Solver, Vec<crate::solver::Var>) {
        use crate::solver::{LinearExpr, Solver};

        assert!(n >= 4);
        let mut solver = Solver::new();
        let vars: Vec<_> = (0..n).map(|_| solver.new_var()).collect();
        for i in 0..n - 2 {
            solver.constrain_eq0(LinearExpr {
                coeffs: vec![(1., vars[i]), (-0.5, vars[i + 1]), (-0.5, vars[i + 2])],
                constant: 0.,
            });
        }
        (solver, vars)
    }

    /// Sparse SSE kernel: solve an underdetermined component and extract its
    /// two-dimensional null space. Correctness checks orthonormality and A v=0.
    #[test]
    #[ignore = "scaling benchmark; run in release, serially: cargo test -p argonc --release -- --ignored --test-threads=1 bench_"]
    fn bench_sparse_sse() {
        let _g = bench_guard();
        let mut rows = Vec::new();
        for &n in &bench_sizes(
            "ARGON_BENCH_SPARSE_SSE",
            &[256, 512, 1024, 2048, 4096, 8192, 16384],
        ) {
            let n = usize::try_from(n).expect("sparse SSE benchmark sizes must be positive");
            let (template, vars) = sparse_sse_fixture(n);
            let mut best = std::time::Duration::MAX;
            let mut peak = 0usize;

            for _ in 0..5 {
                let mut solver = template.clone();
                crate::bench_alloc::reset_peak();
                let base = crate::bench_alloc::live();
                let start = std::time::Instant::now();
                solver.solve();
                let basis = solver.sparse_nullspace_vecs().unwrap();
                best = best.min(start.elapsed());
                peak = peak.max(crate::bench_alloc::peak().saturating_sub(base));

                assert_eq!(basis.len(), 2);
                let coefficients: Vec<indexmap::IndexMap<_, _>> = basis
                    .iter()
                    .map(|vector| vector.iter().map(|&(value, var)| (var, value)).collect())
                    .collect();
                for (basis_index, vector) in coefficients.iter().enumerate() {
                    let norm: f64 = vector.values().map(|value| value * value).sum();
                    assert!((norm - 1.).abs() < 1e-7);
                    let cross: f64 = coefficients[..basis_index]
                        .iter()
                        .map(|other| {
                            vector
                                .iter()
                                .map(|(var, value)| value * other.get(var).unwrap_or(&0.))
                                .sum::<f64>()
                        })
                        .sum();
                    assert!(cross.abs() < 1e-7);
                    for i in 0..n - 2 {
                        let residual = vector.get(&vars[i]).unwrap_or(&0.)
                            - 0.5 * vector.get(&vars[i + 1]).unwrap_or(&0.)
                            - 0.5 * vector.get(&vars[i + 2]).unwrap_or(&0.);
                        assert!(residual.abs() < 1e-7);
                    }
                }
            }

            eprintln!(
                "sparse_sse    n={n:>6} time={best:>11.3?} peak={:>8.2} MiB",
                peak as f64 / (1usize << 20) as f64
            );
            rows.push((n as f64, best.as_secs_f64(), peak, n));
        }
        write_bench_csv("sparse_sse", &rows);
    }

    /// Axis 1: number of independent shapes in a single cell.
    #[test]
    #[ignore = "scaling benchmark; run in release, serially: cargo test -p argonc --release -- --ignored --test-threads=1 bench_"]
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
    /// representation. Since sequences are backed by a persistent vector and
    /// `range` lowers to a native builtin, this path is linear and is swept to
    /// the same sizes as `bench_shapes` so the two can be compared directly.
    #[test]
    #[ignore = "scaling benchmark; run in release, serially: cargo test -p argonc --release -- --ignored --test-threads=1 bench_"]
    fn bench_shapes_loop() {
        let _g = bench_guard();
        let o = parse_workspace_with_std(ARGON_STRESS_SHAPES);
        assert!(o.static_errors().is_empty(), "{:?}", o.static_errors());
        let ast = o.ast();
        // This variant generates the same geometry as `bench_shapes` but with a
        // `for` loop over `std::range`, so its cost also includes building and
        // iterating the list. That list/`range` path is now linear (persistent-
        // vector sequences + a native `range` builtin), so it sweeps to the same
        // sizes as `bench_shapes` and the two series can be compared directly
        // (override `ARGON_BENCH_SHAPES_LOOP` to change the range).
        let mut rows = Vec::new();
        for &n in &bench_sizes(
            "ARGON_BENCH_SHAPES_LOOP",
            &[500, 1000, 2000, 4000, 8000, 16000, 32000],
        ) {
            let (dt, mem, out) = measure(2, || {
                compile(
                    &ast,
                    CompileInput {
                        cell: &["shapes_loop"],
                        args: vec![CellArg::Int(n)],
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

    /// Axis 2: number of mutually-coupled constraints. The coupled ring is reduced by
    /// the solver's sparse elimination pre-pass (2-variable constraints telescope away);
    /// the general dense SVD remains as a fallback for any irreducible coupled core.
    #[test]
    #[ignore = "scaling benchmark; run in release, serially: cargo test -p argonc --release -- --ignored --test-threads=1 bench_"]
    fn bench_constraints() {
        let _g = bench_guard();
        let o = parse_workspace_with_std(ARGON_STRESS_CONSTRAINTS);
        assert!(o.static_errors().is_empty(), "{:?}", o.static_errors());
        let ast = o.ast();
        let mut rows = Vec::new();
        for &n in &bench_sizes(
            "ARGON_BENCH_CONSTRAINTS",
            &[32, 64, 128, 256, 512, 1024, 2048, 4096, 8192, 16384],
        ) {
            let (dt, mem, out) = measure(1, || {
                compile(
                    &ast,
                    CompileInput {
                        cell: &["constraints"],
                        args: vec![CellArg::Int(n)],
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

    /// Axis 2b: number of pending *fallback* (initial-condition) constraints.
    ///
    /// This is the shape the GUI writes: `LangServer::draw_rect` emits every
    /// coordinate as `x0i`/`y0i`/`x1i`/`y1i` so a later drag can rewrite the
    /// literal in place, making a GUI-authored cell almost entirely fallbacks.
    /// No other axis here covers that, since the other `stress_*` examples use
    /// kwargs that `constrain_eq0` back-substitutes at insertion.
    ///
    /// The cell is *underconstrained* by construction, because
    /// `report_underconstrained` runs its trial solve with compiler defaults
    /// but without author fallbacks, so this axis asserts that output was
    /// produced rather than that the compile was `Valid`.
    #[test]
    #[ignore = "scaling benchmark; run in release, serially: cargo test -p argonc --release -- --ignored --test-threads=1 bench_"]
    fn bench_fallbacks() {
        let _g = bench_guard();
        let o = parse_workspace_with_std(ARGON_STRESS_FALLBACKS);
        assert!(o.static_errors().is_empty(), "{:?}", o.static_errors());
        let ast = o.ast();
        let mut rows = Vec::new();
        for &n in &bench_sizes("ARGON_BENCH_FALLBACKS", &[125, 250, 500, 1000, 2000]) {
            let (dt, mem, out) = measure(1, || {
                compile(
                    &ast,
                    CompileInput {
                        cell: &["fallbacks"],
                        args: vec![CellArg::Int(n)],
                    },
                )
            });
            let nobj = count_objects(&out);
            assert_eq!(nobj, n as usize, "fallbacks(n={n}) lost geometry");
            eprintln!(
                "fallbacks     n={n:>6} objects={nobj:>6} time={dt:>11.3?} peak={:>8.2} MiB",
                mem as f64 / (1usize << 20) as f64
            );
            rows.push((n as f64, dt.as_secs_f64(), mem, nobj));
        }
        write_bench_csv("fallbacks", &rows);
    }

    /// Axis 2c: degrees of freedom the source leaves for the solver to pin.
    ///
    /// `crect` with relative dimensions and no absolute anchor leaves one
    /// degree of freedom per axis per shape, and nothing in the source or in
    /// the compiler defaults says where any of them goes, so the fixpoint loop
    /// falls through to `Solver::force_solution`.
    ///
    /// Reports the solver's share of the compile directly, since this is the
    /// one regime where the solve dominates evaluation rather than the other
    /// way round.
    #[test]
    #[ignore = "scaling benchmark; run in release, serially: cargo test -p argonc --release -- --ignored --test-threads=1 bench_"]
    fn bench_free() {
        let _g = bench_guard();
        let o = parse_workspace_with_std(ARGON_STRESS_FREE);
        assert!(o.static_errors().is_empty(), "{:?}", o.static_errors());
        let ast = o.ast();
        let mut rows = Vec::new();
        for &n in &bench_sizes("ARGON_BENCH_FREE", &[250, 500, 1000, 2000, 4000]) {
            crate::solver::solve_stats::reset();
            let (dt, mem, out) = measure(1, || {
                compile(
                    &ast,
                    CompileInput {
                        cell: &["free_shapes"],
                        args: vec![CellArg::Int(n)],
                    },
                )
            });
            let nobj = count_objects(&out);
            assert_eq!(nobj, n as usize, "free_shapes(n={n}) lost geometry");
            let (calls, body, nanos) = crate::solver::solve_stats::read();
            eprintln!(
                "free          n={n:>6} objects={nobj:>6} time={dt:>11.3?} peak={:>8.2} MiB solve_calls={calls} solve_body={body} solve={:>8.3?}",
                mem as f64 / (1usize << 20) as f64,
                std::time::Duration::from_nanos(nanos as u64),
            );
            rows.push((n as f64, dt.as_secs_f64(), mem, nobj));
        }
        write_bench_csv("free", &rows);
    }

    /// Axis 3: number of instances of a single (cached) leaf cell.
    #[test]
    #[ignore = "scaling benchmark; run in release, serially: cargo test -p argonc --release -- --ignored --test-threads=1 bench_"]
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
    /// references each child once and `double_ref` references it twice. Both
    /// are linear in depth because a cell type is nominal and names its
    /// declaration rather than carrying its fields; `double_ref` is kept as a
    /// regression guard against the old exponential expansion.
    #[test]
    #[ignore = "scaling benchmark; run in release, serially: cargo test -p argonc --release -- --ignored --test-threads=1 bench_"]
    fn bench_hierarchy() {
        let _g = bench_guard();
        // Deep hierarchies are traversed by native recursion in the compiler,
        // so compiling `h{depth}` needs ~O(depth) native stack frames. The
        // default ~2 MiB test-thread stack overflows past ~150 levels, so run
        // the whole axis on a thread with a 512 MiB stack: that reaches a few
        // thousand levels (the sweep below goes to 2048) and leaves headroom
        // to push further via `ARGON_BENCH_HIER_*`. The thread is spawned once,
        // outside the timed `measure()` loop, so it does not perturb timings.
        std::thread::Builder::new()
            .stack_size(512 * 1024 * 1024)
            .spawn(bench_hierarchy_body)
            .unwrap()
            .join()
            .unwrap();
    }

    fn bench_hierarchy_body() {
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("build/bench_hier");
        std::fs::create_dir_all(&dir).unwrap();
        let lib = dir.join("lib.ar");

        let mut rows = Vec::new();
        for depth in bench_sizes(
            "ARGON_BENCH_HIER_SINGLE",
            &[4, 8, 16, 32, 64, 128, 256, 512, 1024, 2048],
        )
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

        // `double_ref` binds the child cell twice. With nominal cell types
        // this scales the same as `single_ref`, so it is swept over the same
        // depths. (Before that fix it expanded exponentially and had to be
        // capped near depth 18.) Override `ARGON_BENCH_HIER_DOUBLE` to push
        // deeper.
        let mut rows = Vec::new();
        for depth in bench_sizes(
            "ARGON_BENCH_HIER_DOUBLE",
            &[4, 8, 16, 32, 64, 128, 256, 512, 1024, 2048],
        )
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
            },
        );
        assert!(
            out.is_valid(),
            "constraints ring should be fully determined: {out:?}"
        );
    }

    /// Fallbacks that share a connected component must still be applied one
    /// per round, in priority order, with a solve between them.
    ///
    /// `x0i` and `x1i` are coupled by the `eq`, so applying both in one round
    /// would assert `x1 = 50` on top of an `x1` that `x0i` has already
    /// determined to be `15`, and report a satisfiable cell as inconsistent.
    /// Applying them in order instead makes `x1i` moot.
    #[test]
    fn coupled_fallbacks_are_not_batched() {
        let root = parse_source_text(
            "cell top() {\n\
             let a = rect(\"met1\", x0i = 5., x1i = 50., y0i = 0., y1i = 8.);\n\
             eq(a.x1, a.x0 + 10.);\n\
             }",
            PathBuf::from("/virtual/lib.ar"),
        )
        .unwrap();
        let std = parse_source_text(
            crate::parse::STD_SOURCE,
            PathBuf::from(crate::parse::STD_PATH),
        )
        .unwrap();
        let ast = IndexMap::from([(Vec::new(), root), (vec!["std".to_owned()], std)]);
        let out = compile(
            &ast,
            CompileInput {
                cell: &["top"],
                args: Vec::new(),
            },
        );
        // Determined only by initial conditions, so it reports as
        // underconstrained while still producing geometry -- see
        // `stress_fallbacks_smoke`.
        let errors = out.unwrap_exec_errors();
        assert!(
            errors
                .errors
                .iter()
                .all(|e| matches!(e.kind, ExecErrorKind::Underconstrained)),
            "expected only underconstrained diagnostics: {:?}",
            errors.errors
        );
        let d = errors
            .output
            .as_ref()
            .expect("geometry despite diagnostics");
        let cell = &d.cells[&d.top];
        let rect = cell
            .objects
            .values()
            .find_map(|o| match o {
                SolvedValue::Rect(r) => Some(r),
                _ => None,
            })
            .expect("the cell emits one rectangle");
        assert_relative_eq!(rect.x0.0, 5., epsilon = EPSILON);
        assert_relative_eq!(
            rect.x1.0,
            15.,
            epsilon = EPSILON,
            // 50. would mean `x1i` was applied alongside `x0i` rather than
            // being made moot by it.
        );
        assert!(
            cell.inconsistent_constraints.is_empty(),
            "coupled initial conditions must not over-constrain the cell"
        );
        // Only the three that actually determined something were recorded;
        // `x1i` was dropped as moot, and the GUI reads this list to decide
        // which literals a drag may rewrite.
        assert_eq!(cell.fallback_constraints_used.len(), 3);
    }

    /// `geometry_digest` must actually discriminate, or the shadow-execution
    /// check (`ARGON_VERIFY_CELL_CACHE`) would pass vacuously.
    #[test]
    fn geometry_digest_distinguishes_what_a_user_can_see() {
        let compile_source = |source: &str| {
            let root = parse_source_text(source, PathBuf::from("/virtual/lib.ar")).unwrap();
            let std = parse_source_text(
                crate::parse::STD_SOURCE,
                PathBuf::from(crate::parse::STD_PATH),
            )
            .unwrap();
            let ast = IndexMap::from([(Vec::new(), root), (vec!["std".to_owned()], std)]);
            compile(
                &ast,
                CompileInput {
                    cell: &["top"],
                    args: Vec::new(),
                },
            )
            .unwrap_valid()
            .geometry_digest()
        };

        let base = "cell top() { let r = rect(\"met1\", x0=0., y0=0., x1=10., y1=10.); }";
        assert_eq!(compile_source(base), compile_source(base), "stable");
        assert_ne!(
            compile_source(base),
            compile_source("cell top() { let r = rect(\"met1\", x0=0., y0=0., x1=11., y1=10.); }"),
            "a moved edge must change the digest"
        );
        assert_ne!(
            compile_source(base),
            compile_source("cell top() { let r = rect(\"met2\", x0=0., y0=0., x1=10., y1=10.); }"),
            "a different layer must change the digest"
        );
        assert_ne!(
            compile_source(base),
            compile_source(
                "cell top() { let r = rect(\"met1\", x0=0., y0=0., x1=10., y1=10.);\n\
                 let s = rect(\"met1\", x0=20., y0=0., x1=30., y1=10.); }"
            ),
            "an extra shape must change the digest"
        );
    }

    #[test]
    fn stress_free_smoke() {
        let o = parse_workspace_with_std(ARGON_STRESS_FREE);
        assert!(o.static_errors().is_empty(), "{:?}", o.static_errors());
        let ast = o.ast();
        let out = compile(
            &ast,
            CompileInput {
                cell: &["free_shapes"],
                args: vec![CellArg::Int(32)],
            },
        );
        // Nothing anchors the geometry, so the cell reports as underconstrained
        // and `force_solution` supplies the rest.
        let errors = out.unwrap_exec_errors();
        assert!(
            errors
                .errors
                .iter()
                .all(|e| matches!(e.kind, ExecErrorKind::Underconstrained)),
            "expected only underconstrained diagnostics: {:?}",
            errors.errors
        );
        let d = errors
            .output
            .as_ref()
            .expect("geometry despite diagnostics");
        let rects = d
            .cells
            .values()
            .flat_map(|c| c.objects.values())
            .filter(|o| matches!(o, SolvedValue::Rect(_)))
            .count();
        assert_eq!(rects, 32);
        // Every shape keeps the width and height the source did constrain,
        // wherever `force_solution` decided to put it.
        for object in d.cells.values().flat_map(|c| c.objects.values()) {
            let SolvedValue::Rect(r) = object else {
                continue;
            };
            assert_relative_eq!(r.x1.0 - r.x0.0, 10., epsilon = EPSILON);
            assert_relative_eq!(r.y1.0 - r.y0.0, 8., epsilon = EPSILON);
        }
    }

    #[test]
    fn stress_fallbacks_smoke() {
        let o = parse_workspace_with_std(ARGON_STRESS_FALLBACKS);
        assert!(o.static_errors().is_empty(), "{:?}", o.static_errors());
        let ast = o.ast();
        let out = compile(
            &ast,
            CompileInput {
                cell: &["fallbacks"],
                args: vec![CellArg::Int(32)],
            },
        );
        // Determined only by author fallbacks, so it reports as
        // underconstrained while still producing every rectangle. Both halves
        // matter: the diagnostic is what makes this a distinct axis from
        // `stress_shapes`, and the geometry is what the benchmark measures.
        let errors = out.unwrap_exec_errors();
        assert!(
            errors
                .errors
                .iter()
                .all(|e| matches!(e.kind, ExecErrorKind::Underconstrained)),
            "expected only underconstrained diagnostics: {:?}",
            errors.errors
        );
        let d = errors
            .output
            .as_ref()
            .expect("geometry despite diagnostics");
        let nrects = d
            .cells
            .values()
            .flat_map(|c| c.objects.values())
            .filter(|o| matches!(o, SolvedValue::Rect(r) if !r.construction))
            .count();
        assert_eq!(nrects, 32, "fallbacks should emit 32 rectangles");
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
            },
        );
        let data = cell.unwrap_valid();
        let compiled = &data.cells[&data.top];
        assert_eq!(compiled.scopes[&compiled.root].name, "cell scopes");
        let child_names = compiled.scopes[&compiled.root]
            .children
            .iter()
            .map(|id| compiled.scopes[id].name.as_str())
            .collect::<Vec<_>>();
        assert_eq!(child_names, ["0 block"]);
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
            },
        );
        println!("{cell:?}");
        let errors = cell.unwrap_exec_errors();
        let error = errors
            .errors
            .iter()
            .find(|error| matches!(error.kind, ExecErrorKind::InconsistentConstraint(_)))
            .expect("expected an inconsistent constraint");
        let span = error
            .span
            .as_ref()
            .expect("inconsistent constraint should retain its source span");
        let source = std::fs::read_to_string(&span.path).unwrap();
        assert_eq!(&source[span.span.start()..span.span.end()], "eq(a, 5.)");
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
            },
        );
        let data = match cells {
            crate::compile::CompileOutput::Valid(data) => data,
            crate::compile::CompileOutput::ExecErrors(output) => output.output.unwrap(),
            crate::compile::CompileOutput::StaticErrors(_) => panic!("static compilation failed"),
            crate::compile::CompileOutput::FatalParseErrors => panic!("parsing failed"),
        };
        let scope_names = data
            .cells
            .values()
            .flat_map(|cell| cell.scopes.values().map(|scope| scope.name.as_str()))
            .collect::<Vec<_>>();
        assert!(scope_names.contains(&"cell top"));
        assert!(scope_names.contains(&"0 cell middle"));
        assert!(scope_names.contains(&"0 cell bot"));
        assert!(scope_names.contains(&"0 fn make_rect"));
        assert!(scope_names.contains(&"1 fn emit_rect"));
        assert!(scope_names.contains(&"1 block"));
        assert!(scope_names.contains(&"2 else"));
    }

    /// `examples/cell_out_of_order` is `examples/hierarchy` with `top` moved
    /// above the `bot` it instantiates. Declaration order does not matter, so
    /// the two produce the same layout.
    #[test]
    fn argon_cell_out_of_order() {
        let digest = |path: &str| {
            let o = parse_workspace_with_std(path);
            assert!(o.static_errors().is_empty());
            let ast = o.ast();
            compile(
                &ast,
                CompileInput {
                    cell: &["top"],
                    args: Vec::new(),
                },
            )
            .unwrap_valid()
            .geometry_digest()
        };
        assert_eq!(
            digest(ARGON_CELL_OUT_OF_ORDER),
            digest(ARGON_HIERARCHY),
            "reordering declarations changed the layout"
        );
    }

    /// Compiles `cell` of a one-file workspace with the standard library.
    fn compile_source(source: &str, cell: &str, args: Vec<CellArg>) -> CompileOutput {
        let root = parse_source_text(source, PathBuf::from("/virtual/lib.ar")).unwrap();
        let std = parse_source_text(
            crate::parse::STD_SOURCE,
            PathBuf::from(crate::parse::STD_PATH),
        )
        .unwrap();
        let ast = IndexMap::from([(Vec::new(), root), (vec!["std".to_owned()], std)]);
        compile(
            &ast,
            CompileInput {
                cell: &[cell],
                args,
            },
        )
    }

    /// The drawn rectangles across every compiled cell.
    fn rect_count(data: &CompiledData) -> usize {
        data.cells
            .values()
            .flat_map(|cell| cell.objects.values())
            .filter(|object| matches!(object, SolvedValue::Rect(rect) if !rect.construction))
            .count()
    }

    #[test]
    fn a_cell_may_read_fields_of_a_cell_declared_below_it() {
        let errors = static_errors_of(
            "cell top() {\n\
                 let l = inst(bot());\n\
                 eq(l.met1.x0, 0.);\n\
                 let w = l.met1.w;\n\
             }\n\
             cell bot() {\n\
                 let met1 = rect(\"met1\", x0=0., y0=0., x1=100., y1=100.);\n\
             }\n",
        );
        assert!(errors.is_empty(), "{errors:?}");
        // A misspelled field of a later cell is still a plain field error.
        assert!(matches!(
            static_errors_of(
                "cell top() {\n\
                     let l = inst(bot());\n\
                     eq(l.met2.x0, 0.);\n\
                 }\n\
                 cell bot() {\n\
                     let met1 = rect(\"met1\", x0=0., y0=0., x1=100., y1=100.);\n\
                 }\n"
            )
            .as_slice(),
            [StaticErrorKind::NoFieldOnTy { .. }]
        ));
    }

    /// `examples/recursive_cell`: `tree(n)` instantiates `tree(n - 1)`, so
    /// `tree(3)` draws four squares across four compiled cells.
    #[test]
    fn a_cell_may_instantiate_itself() {
        let o = parse_workspace_with_std(ARGON_RECURSIVE_CELL);
        assert!(o.static_errors().is_empty());
        let ast = o.ast();
        let data = compile(
            &ast,
            CompileInput {
                cell: &["tree"],
                args: vec![CellArg::Int(3)],
            },
        )
        .unwrap_valid();
        assert_eq!(data.cells.len(), 4);
        assert_eq!(rect_count(&data), 4);
    }

    /// Recursion that never ends is a run-time error, not a hang.
    #[test]
    fn unbounded_cell_recursion_is_reported() {
        // The same arguments name the same cell, so the cycle is caught as
        // soon as it closes.
        let errors = compile_source(
            "cell forever() {\n\
                 let r = rect(\"met1\", x0=0., y0=0., x1=1., y1=1.);\n\
                 let again = inst(forever());\n\
             }\n",
            "forever",
            Vec::new(),
        )
        .unwrap_exec_errors()
        .errors;
        assert!(
            errors.iter().any(|error| matches!(
                &error.kind,
                ExecErrorKind::RecursiveInstantiation { cell } if cell == "forever"
            )),
            "{errors:?}"
        );
        // Ever-changing arguments run into the depth limit instead.
        let errors = crate::run_with_stack("argon-compile", || {
            compile_source(
                "cell forever(n: Int) {\n\
                     let r = rect(\"met1\", x0=0., y0=0., x1=1., y1=1.);\n\
                     let again = inst(forever(n + 1));\n\
                 }\n",
                "forever",
                vec![CellArg::Int(0)],
            )
            .unwrap_exec_errors()
            .errors
        });
        assert!(
            errors
                .iter()
                .any(|error| matches!(error.kind, ExecErrorKind::RecursionLimitExceeded { .. })),
            "{errors:?}"
        );
    }

    #[test]
    fn cells_may_instantiate_each_other() {
        let data = compile_source(
            "cell a(n: Int) {\n\
                 let r = rect(\"met1\", x0=0., y0=0., x1=10., y1=10.);\n\
                 if n > 0 {\n\
                     let b = inst(b(n - 1));\n\
                     eq(b.r.x0, r.x1);\n\
                     eq(b.y, 0.);\n\
                 } else {\n\
                 };\n\
             }\n\
             cell b(n: Int) {\n\
                 let r = rect(\"met2\", x0=0., y0=0., x1=10., y1=10.);\n\
                 if n > 0 {\n\
                     let a = inst(a(n - 1));\n\
                     eq(a.r.x0, r.x1);\n\
                     eq(a.y, 0.);\n\
                 } else {\n\
                 };\n\
             }\n",
            "a",
            vec![CellArg::Int(3)],
        )
        .unwrap_valid();
        assert_eq!(rect_count(&data), 4);
    }

    /// A field is typed when it is first read, so a statement may read a
    /// field declared below it through an instance of the enclosing cell.
    #[test]
    fn a_field_declared_later_in_the_same_cell_is_typed_on_demand() {
        let errors = static_errors_of(
            "cell a(n: Int) {\n\
                 let k = if n > 0 { inst(a(n - 1)).z.w } else { 0. };\n\
                 let z = rect(\"met1\", x0=0., y0=0., x1=10., y1=10.);\n\
             }\n",
        );
        assert!(errors.is_empty(), "{errors:?}");
    }

    /// A statement that reads two untyped fields queues a goal for each. When
    /// the second field's statement reads the first, the first's queued goal
    /// is worked before the second statement is retried.
    #[test]
    fn a_queued_field_read_again_is_typed_before_its_reader_is_retried() {
        let errors = static_errors_of(
            "cell x() {\n\
                 eq(inst(a()).f.x0, inst(b()).g.x0);\n\
             }\n\
             cell a() {\n\
                 let f = rect(\"met1\", x0=0., y0=0., x1=1., y1=1.);\n\
             }\n\
             cell b() {\n\
                 let g = inst(a()).f;\n\
             }\n",
        );
        assert!(errors.is_empty(), "{errors:?}");
    }

    /// A top-level `let` that is in scope but not yet typed hides a module
    /// declaration of the same name, so a statement typed on demand reads the
    /// local rather than the cell.
    #[test]
    fn an_untyped_local_hides_a_module_declaration_of_the_same_name() {
        let errors = static_errors_of(
            "cell user() {\n\
                 let v = inst(top()).a;\n\
             }\n\
             cell top() {\n\
                 let bot = 5.;\n\
                 let a = bot + 1.;\n\
             }\n\
             cell bot() {\n\
                 let r = rect(\"met1\", x0=0., y0=0., x1=1., y1=1.);\n\
             }\n",
        );
        assert!(errors.is_empty(), "{errors:?}");
    }

    /// A statement typed on demand, ahead of the statements above it, sees the
    /// same bindings it would when typed in order: the nearest `let` of each
    /// name above it, typed as it is needed.
    #[test]
    fn a_statement_typed_out_of_order_sees_the_nearest_lets_above_it() {
        let errors = static_errors_of(
            "cell user() {\n\
                 eq(inst(top()).c, 1.);\n\
             }\n\
             cell top() {\n\
                 let r = 1.;\n\
                 let s = r + 1.;\n\
                 let r = rect(\"met1\", x0=0., y0=0., x1=1., y1=1.);\n\
                 let c = r.w + s;\n\
             }\n",
        );
        assert!(errors.is_empty(), "{errors:?}");
    }

    /// A field whose type depends on itself is reported once, at the read that
    /// closes the cycle, and nothing else cascades from it.
    #[test]
    fn a_field_whose_type_depends_on_itself_is_an_error() {
        let errors = static_errors_of("cell a(n: Int) {\n    let f = inst(a(n - 1)).f;\n}\n");
        assert!(
            matches!(
                errors.as_slice(),
                [StaticErrorKind::CyclicCellField { cell, field }] if cell == "a" && field == "f"
            ),
            "{errors:?}"
        );

        let source =
            "cell a() {\n    let f = inst(b()).g;\n}\ncell b() {\n    let g = inst(a()).f;\n}\n";
        let errors = static_errors(source);
        assert!(
            matches!(
                errors.as_slice(),
                [crate::compile::StaticError { kind: StaticErrorKind::CyclicCellField { cell, field }, span }]
                    if cell == "a" && field == "f" && span.span.start() == source.rfind(".f").unwrap() + 1
            ),
            "{errors:?}"
        );

        // Through a local: `z` reads `k`, whose type needs `z`.
        let errors = static_errors_of(
            "cell a(n: Int) {\n    let k = inst(a(n - 1)).z;\n    let z = k;\n}\n",
        );
        assert!(
            matches!(
                errors.as_slice(),
                [StaticErrorKind::CyclicCellField { cell, field }] if cell == "a" && field == "k"
            ),
            "{errors:?}"
        );
    }

    /// Cell types are nominal: two cells with identical fields are different
    /// types, and `Any` is how code accepts either.
    #[test]
    fn cell_types_are_nominal() {
        let cells = "cell a() {\n\
                         let r = rect(\"met1\", x0=0., y0=0., x1=1., y1=1.);\n\
                     }\n\
                     cell b() {\n\
                         let r = rect(\"met1\", x0=0., y0=0., x1=1., y1=1.);\n\
                     }\n";
        let errors = static_errors_of(&format!(
            "{cells}cell top(flag: Bool) {{\n    let c = if flag {{ a() }} else {{ b() }};\n}}\n"
        ));
        assert!(
            matches!(errors.as_slice(), [StaticErrorKind::BranchesDifferentTypes]),
            "{errors:?}"
        );
        let errors = static_errors_of(&format!(
            "{cells}fn erase(c: Any) -> Any {{ c }}\n\
             cell top(flag: Bool) {{\n    let c = inst(if flag {{ erase(a()) }} else {{ erase(b()) }});\n}}\n"
        ));
        assert!(errors.is_empty(), "{errors:?}");
    }

    /// A top-level `let` may re-bind a name: each statement sees the nearest
    /// `let` above it, and the field an instance answers is the last one.
    #[test]
    fn a_repeated_top_level_let_rebinds_sequentially() {
        let cells = "cell top() {\n\
                         let r = rect(\"met1\", x0=0., y0=0., x1=1., y1=1.);\n\
                         let s = r.w;\n\
                         let r = 5.;\n\
                         let t = r + 1.;\n\
                     }\n";
        let errors = static_errors_of(cells);
        assert!(errors.is_empty(), "{errors:?}");
        let errors = static_errors_of(&format!(
            "{cells}cell user() {{\n    let i = inst(top());\n    let v = i.r + 1.;\n}}\n"
        ));
        assert!(errors.is_empty(), "{errors:?}");
        let errors = static_errors_of(&format!(
            "{cells}cell user() {{\n    let i = inst(top());\n    let v = i.r.w;\n}}\n"
        ));
        assert!(
            matches!(errors.as_slice(), [StaticErrorKind::NoFieldOnTy { .. }]),
            "{errors:?}"
        );
    }

    #[test]
    fn cyclic_module_dependencies_report_the_full_cycle() {
        let root_path = PathBuf::from("/virtual/lib.ar");
        let devices_path = PathBuf::from("/virtual/devices.ar");
        let root = parse_source_text(
            "fn root_value() -> Float { devices::device_value() }",
            root_path,
        )
        .unwrap();
        let devices = parse_source_text(
            "fn device_value() -> Float { lib::root_value() }",
            devices_path.clone(),
        )
        .unwrap();
        let ast = IndexMap::from([(Vec::new(), root), (vec!["devices".to_owned()], devices)]);

        let (typed, output) = static_compile(&ast).unwrap();
        assert!(
            typed.is_empty(),
            "an invalid module graph must not be typed"
        );
        assert_eq!(output.errors.len(), 1);
        assert_eq!(output.errors[0].span.path, devices_path);
        assert_eq!(
            output.errors[0].kind.to_string(),
            "cyclic module dependency: lib -> devices -> lib"
        );
    }

    #[test]
    fn use_imports_functions_and_supports_aliases() {
        use crate::ast::{Decl, Expr};

        let root = parse_source_text(
            "use math::double as twice; fn root(x: Float) -> Float { twice(x) }",
            PathBuf::from("/virtual/lib.ar"),
        )
        .unwrap();
        let math = parse_source_text(
            "fn double(x: Float) -> Float { x + x }",
            PathBuf::from("/virtual/math.ar"),
        )
        .unwrap();
        let ast = IndexMap::from([(Vec::new(), root), (vec!["math".to_owned()], math)]);

        let (typed, output) = static_compile(&ast).unwrap();
        assert!(output.errors.is_empty(), "{:#?}", output.errors);

        let Decl::Fn(root) = &typed[&Vec::new()].ast.decls[1] else {
            panic!("expected root function");
        };
        let Expr::Call(call) = root.scope.tail.as_ref().unwrap() else {
            panic!("expected imported function call");
        };
        let Decl::Fn(double) = &typed[&vec!["math".to_owned()]].ast.decls[0] else {
            panic!("expected double function");
        };
        assert_eq!(call.func.path[0].name.as_str(), "twice");
        assert_eq!(call.metadata.0, Some(double.metadata.1));
    }

    #[test]
    fn unresolved_use_item_has_a_targeted_error() {
        let root = parse_source_text(
            "use math::missing; fn root() {}",
            PathBuf::from("/virtual/lib.ar"),
        )
        .unwrap();
        let math = parse_source_text("fn present() {}", PathBuf::from("/virtual/math.ar")).unwrap();
        let ast = IndexMap::from([(Vec::new(), root), (vec!["math".to_owned()], math)]);

        let (_, output) = static_compile(&ast).unwrap();
        assert!(output.errors.iter().any(|error| {
            matches!(
                &error.kind,
                StaticErrorKind::UnresolvedImport { path } if path == "math::missing"
            )
        }));
    }

    #[test]
    fn missing_module_errors_include_the_module_name() {
        let root = parse_source_text(
            "fn root_value() -> Float { missing::value() }",
            PathBuf::from("/virtual/lib.ar"),
        )
        .unwrap();
        let ast = IndexMap::from([(Vec::new(), root)]);

        let (typed, output) = static_compile(&ast).unwrap();
        assert!(
            typed.is_empty(),
            "an invalid module graph must not be typed"
        );
        assert_eq!(output.errors.len(), 1);
        assert_eq!(
            output.errors[0].kind.to_string(),
            "module `missing` does not exist or could not be loaded"
        );
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
            },
        )
        .unwrap_exec_errors()
        .output
        .unwrap();
        let cell = &cells.cells[&cells.top];
        assert!(!cell.fallback_constraints_used.is_empty());
        assert!(
            cell.fallback_constraints_used
                .iter()
                .any(|fallback| matches!(
                    fallback.initial_condition,
                    Some(RectInitialCondition::InstanceX(_))
                ))
        );
        let inst = cell
            .objects
            .values()
            .find_map(SolvedValue::get_instance)
            .expect("top should contain an instance");
        assert_eq!(inst.x_expr.coeffs.len(), 1);
        assert_eq!(inst.y_expr.coeffs.len(), 1);
        assert!(cell.unsolved_vars.contains(&inst.x_expr.coeffs[0].1));
        assert!(!cell.unsolved_vars.contains(&inst.y_expr.coeffs[0].1));
        println!("{cells:#?}");
    }

    /// Verifies the metadata the GUI relies on to persist solution-space-
    /// exploration drags: the used fallback's constraint (`x1 - 100`) and the
    /// source span pointing at the value text `100.` in `x1i=100.`.
    #[test]
    fn argon_sse_basic_fallback_metadata() {
        let o = parse_workspace_with_std(ARGON_SSE_BASIC);
        assert!(o.static_errors().is_empty());
        let ast = o.ast();
        let cells = compile(
            &ast,
            CompileInput {
                cell: &["top"],
                args: Vec::new(),
            },
        )
        .unwrap_exec_errors()
        .output
        .unwrap();
        let used = &cells.cells[&cells.top].fallback_constraints_used;
        // `eq(x1, y1)` leaves a single degree of freedom, pinned by exactly one
        // fallback (the higher-priority `x1i`).
        assert_eq!(used.len(), 1);
        let fb = &used[0];
        assert!(matches!(
            fb.initial_condition,
            Some(RectInitialCondition::X1(_))
        ));
        // Constraint is `x1 - 100`: a single variable with coefficient 1 and a
        // pinned value of 100 (= -constant).
        assert_eq!(fb.constraint.coeffs.len(), 1);
        assert!((fb.constraint.coeffs[0].0 - 1.0).abs() < 1e-9);
        assert!((-fb.constraint.constant - 100.0).abs() < 1e-9);
        // The span addresses exactly the value text in the source.
        let src = std::fs::read_to_string(&fb.span.path).unwrap();
        assert_eq!(&src[fb.span.span.start()..fb.span.span.end()], "100.");
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
        let dimensions = cell
            .objects
            .values()
            .filter_map(|value| value.get_dimension())
            .collect::<Vec<_>>();
        assert_eq!(dimensions.len(), 2);
        assert!(dimensions.iter().all(|dimension| {
            !dimension.p.1.coeffs.is_empty() && !dimension.n.1.coeffs.is_empty()
        }));
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
            },
        );
        println!("{cells:#?}");
        cells.unwrap_valid();
    }

    #[test]
    fn argon_kwargs_fn() {
        let o = parse_workspace_with_std(ARGON_KWARGS_FN);
        assert!(o.static_errors().is_empty());
        let cells = compile(
            &o.ast(),
            CompileInput {
                cell: &["top"],
                args: Vec::new(),
            },
        )
        .unwrap_valid();
        let top = &cells.cells[&cells.top];
        // Every rect's `x1` is a call with omitted, explicit, or
        // parameter-referencing defaults, keyed by its `y0`.
        let mut widths: Vec<(f64, f64)> = top
            .objects
            .values()
            .filter_map(|object| object.get_rect())
            .map(|rect| (rect.y0.0, rect.x1.0))
            .collect();
        widths.sort_by(|a, b| a.0.total_cmp(&b.0));
        let expected = [
            (0., 30.),
            (20., 40.),
            (40., 20.),
            (60., 10.),
            (80., 10.),
            (100., 50.),
        ];
        assert_eq!(widths.len(), expected.len());
        for ((y0, x1), (expected_y0, expected_x1)) in widths.iter().zip(expected) {
            assert_relative_eq!(*y0, expected_y0, epsilon = EPSILON);
            assert_relative_eq!(*x1, expected_x1, epsilon = EPSILON);
        }
    }

    #[test]
    fn argon_kwargs_cell() {
        let o = parse_workspace_with_std(ARGON_KWARGS_CELL);
        assert!(o.static_errors().is_empty());
        // The positional API supplies every parameter, keyword ones included.
        let cells = compile(
            &o.ast(),
            CompileInput {
                cell: &["top"],
                args: vec![CellArg::Float(100.)],
            },
        )
        .unwrap_valid();
        let top = &cells.cells[&cells.top];
        let mut insts: Vec<_> = top
            .objects
            .values()
            .filter_map(|object| object.get_instance())
            .collect();
        insts.sort_by(|a, b| a.x.total_cmp(&b.x));
        let [square, tall, same, met2] = insts.as_slice() else {
            panic!("expected four instances, got {}", insts.len());
        };
        assert_eq!(cells.cells.len(), 5);
        let child_rect = |cell| {
            cells.cells[&cell]
                .objects
                .values()
                .find_map(|object| object.get_rect())
                .cloned()
                .expect("child emits a rect")
        };
        // `child(w)` and `child(w, h=w)` resolve to the same arguments.
        for (inst, x1, y1, layer) in [
            (square, 100., 100., "met1"),
            (tall, 100., 200., "met1"),
            (same, 100., 100., "met1"),
            (met2, 50., 50., "met2"),
        ] {
            let rect = child_rect(inst.cell);
            assert_relative_eq!(rect.x1.0, x1, epsilon = EPSILON);
            assert_relative_eq!(rect.y1.0, y1, epsilon = EPSILON);
            assert_eq!(rect.layer.as_deref(), Some(layer));
        }
    }

    #[test]
    fn argon_structs() {
        let o = parse_workspace_with_std(ARGON_STRUCTS);
        assert!(o.static_errors().is_empty(), "{:?}", o.static_errors());
        let cells = compile(
            &o.ast(),
            CompileInput {
                cell: &["top"],
                args: Vec::new(),
            },
        )
        .unwrap_valid();
        let top = &cells.cells[&cells.top];
        // `via(params)` and `via(wide)` are two parameterizations of `via`.
        assert_eq!(cells.cells.len(), 3);
        let mut insts: Vec<_> = top
            .objects
            .values()
            .filter_map(|object| object.get_instance())
            .collect();
        insts.sort_by(|a, b| a.x.total_cmp(&b.x));
        let [narrow, wide] = insts.as_slice() else {
            panic!("expected two instances, got {}", insts.len());
        };
        let child_rect = |cell| {
            cells.cells[&cell]
                .objects
                .values()
                .find_map(|object| object.get_rect())
                .cloned()
                .expect("via emits a rect")
        };
        // `grow(square(100.), 10.)`: `..s` keeps `h` at 100 while `w` grows.
        let rect = child_rect(narrow.cell);
        assert_eq!(rect.layer.as_deref(), Some("met1"));
        assert_relative_eq!(rect.x1.0, 110., epsilon = EPSILON);
        assert_relative_eq!(rect.y1.0, 100., epsilon = EPSILON);
        // `Size { w: 300., ..size }` inside `ViaParams { .., ..params }`.
        let rect = child_rect(wide.cell);
        assert_eq!(rect.layer.as_deref(), Some("met1"));
        assert_relative_eq!(rect.x1.0, 300., epsilon = EPSILON);
        assert_relative_eq!(rect.y1.0, 100., epsilon = EPSILON);
        assert_relative_eq!(wide.x, 160., epsilon = EPSILON);
        // The outline is sized from a sequence of structs.
        let outline = top
            .objects
            .values()
            .find_map(|object| object.get_rect())
            .expect("top emits the outline");
        assert_eq!(outline.layer.as_deref(), Some("met2"));
        assert_relative_eq!(outline.x1.0, 460., epsilon = EPSILON);
        assert_relative_eq!(outline.y1.0, 100., epsilon = EPSILON);
    }

    /// A struct literal in a cell invocation, as `arc run --cell` and the GUI
    /// open-cell command supply it.
    #[test]
    fn argon_structs_cell_invocation() {
        let mut ast = parse_workspace_with_std(ARGON_STRUCTS).ast();
        let invocation = crate::parse::splice_cell_invocation(
            &mut ast,
            "via(ViaParams { layer: \"met2\", size: geom::Size { w: 40., h: 20. }, n: 1 })",
        )
        .expect("invocation should splice");
        let (typed, errors) = static_compile(&ast).unwrap();
        assert!(errors.errors.is_empty(), "{:?}", errors.errors);
        let config = WorkspaceConfig::default().with_tech(Some(PathBuf::from(BASIC_TECH)));
        let cells =
            crate::compile::execute_cell_invocation(&typed, &invocation, &config).unwrap_valid();
        let rect = cells.cells[&cells.top]
            .objects
            .values()
            .find_map(|object| object.get_rect())
            .expect("via emits a rect");
        assert_eq!(rect.layer.as_deref(), Some("met2"));
        assert_relative_eq!(rect.x1.0, 40., epsilon = EPSILON);
        assert_relative_eq!(rect.y1.0, 20., epsilon = EPSILON);
    }

    /// Struct values passed through the positional cell API are checked
    /// against the declared parameter type, like every other argument.
    #[test]
    fn struct_cell_arguments_are_checked_at_runtime() {
        let ast = parse_workspace_with_std(ARGON_STRUCTS).ast();
        let size = |w: f64, h: f64| CellArg::Struct {
            name: "geom::Size".to_owned(),
            fields: vec![
                ("w".to_owned(), CellArg::Float(w)),
                ("h".to_owned(), CellArg::Float(h)),
            ],
        };
        let params = CellArg::Struct {
            name: "ViaParams".to_owned(),
            fields: vec![
                ("layer".to_owned(), CellArg::String("met1".to_owned())),
                ("size".to_owned(), size(30., 20.)),
                ("n".to_owned(), CellArg::Int(1)),
            ],
        };
        let cells = compile(
            &ast,
            CompileInput {
                cell: &["via"],
                args: vec![params],
            },
        )
        .unwrap_valid();
        let rect = cells.cells[&cells.top]
            .objects
            .values()
            .find_map(|object| object.get_rect())
            .expect("via emits a rect");
        assert_relative_eq!(rect.x1.0, 30., epsilon = EPSILON);
        assert_relative_eq!(rect.y1.0, 20., epsilon = EPSILON);

        let wrong = compile(
            &ast,
            CompileInput {
                cell: &["via"],
                args: vec![size(30., 20.)],
            },
        );
        let errors = wrong.unwrap_exec_errors();
        assert!(
            errors
                .errors
                .iter()
                .any(|e| matches!(e.kind, ExecErrorKind::InvalidCellArgumentType { .. })),
            "{:?}",
            errors.errors
        );
    }

    /// Shapes are passed to cells by value: the caller's solved coordinates
    /// arrive as constants, and `!` draws the ones that were drawn in the
    /// caller.
    #[test]
    fn argon_shape_cell_args() {
        let o = parse_workspace_with_std(ARGON_SHAPE_CELL_ARGS);
        assert!(o.static_errors().is_empty(), "{:?}", o.static_errors());
        let cells = compile(
            &o.ast(),
            CompileInput {
                cell: &["top"],
                args: Vec::new(),
            },
        )
        .unwrap_valid();
        let top = &cells.cells[&cells.top];
        assert_eq!(cells.cells.len(), 3);
        let mut insts: Vec<_> = top
            .objects
            .values()
            .filter_map(|object| object.get_instance())
            .collect();
        insts.sort_by(|a, b| a.x.total_cmp(&b.x));
        let [vias, outline] = insts.as_slice() else {
            panic!("expected two instances, got {}", insts.len());
        };

        let via_array = &cells.cells[&vias.cell];
        let rects = |cell: &crate::compile::CompiledCell| {
            cell.objects
                .values()
                .filter_map(|object| object.get_rect())
                .cloned()
                .collect::<Vec<_>>()
        };
        // The 80 x 40 overlap arrives as construction geometry, untranslated.
        let region = rects(via_array)
            .into_iter()
            .find(|rect| rect.construction && relative_eq!(rect.x0.0, 10.))
            .expect("the region is construction geometry");
        assert_eq!(region.layer, None);
        assert_relative_eq!(region.y0.0, 5., epsilon = EPSILON);
        assert_relative_eq!(region.x1.0, 90., epsilon = EPSILON);
        assert_relative_eq!(region.y1.0, 45., epsilon = EPSILON);
        // Four columns and two rows of vias, centered in the region.
        let drawn: Vec<_> = rects(via_array)
            .into_iter()
            .filter(|rect| !rect.construction)
            .collect();
        assert_eq!(drawn.len(), 8);
        assert!(
            drawn
                .iter()
                .all(|rect| rect.layer.as_deref() == Some("via1"))
        );
        let x0 = drawn.iter().map(|r| r.x0.0).fold(f64::INFINITY, f64::min);
        let x1 = drawn
            .iter()
            .map(|r| r.x1.0)
            .fold(f64::NEG_INFINITY, f64::max);
        let y0 = drawn.iter().map(|r| r.y0.0).fold(f64::INFINITY, f64::min);
        let y1 = drawn
            .iter()
            .map(|r| r.y1.0)
            .fold(f64::NEG_INFINITY, f64::max);
        assert_relative_eq!(x0, 15., epsilon = EPSILON);
        assert_relative_eq!(x1, 85., epsilon = EPSILON);
        assert_relative_eq!(y0, 10., epsilon = EPSILON);
        assert_relative_eq!(y1, 40., epsilon = EPSILON);

        let outline = &cells.cells[&outline.cell];
        // `shape!` draws the polygon on its layer.
        let polygon = outline
            .objects
            .values()
            .find_map(|object| object.get_polygon())
            .expect("outline holds the polygon");
        assert!(!polygon.construction);
        assert_eq!(polygon.layer, "met1");
        assert_eq!(polygon.points.len(), 3);
        assert_relative_eq!(polygon.points[2].0.0, 20., epsilon = EPSILON);
        assert_relative_eq!(polygon.points[2].1.0, 30., epsilon = EPSILON);
        // `guide!` leaves the crect undrawn.
        let rects = rects(outline);
        let guide = rects
            .iter()
            .find(|rect| rect.construction)
            .expect("the guide is construction geometry");
        assert_eq!(guide.layer, None);
        assert_relative_eq!(guide.x0.0, 10., epsilon = EPSILON);
        // The marker sits on the point read out of the polygon.
        let marker = rects
            .iter()
            .find(|rect| !rect.construction)
            .expect("the marker is drawn");
        assert_eq!(marker.layer.as_deref(), Some("met2"));
        assert_relative_eq!(marker.x0.0, 20., epsilon = EPSILON);
        assert_relative_eq!(marker.y0.0, 30., epsilon = EPSILON);
        assert_relative_eq!(marker.x1.0, 25., epsilon = EPSILON);
        assert_relative_eq!(marker.y1.0, 35., epsilon = EPSILON);
    }

    /// Shapes nested in sequences and structs, and proxies read out of an
    /// instance, all arrive as construction geometry; `!` redraws exactly the
    /// ones that were drawn in the caller.
    #[test]
    fn nested_and_proxied_shape_cell_arguments() {
        let source = r#"
struct Pair {
    a: Rect,
    b: Point,
}

cell leaf() {
    let m = rect("met1", x0=0., y0=0., w=10., h=10.);
}

cell child(rs: [Rect], p: Pair, proxied: Rect) {
    let first = head(rs)!;
    let second = head(tail(rs))!;
    let redrawn = proxied!;
    let m = rect("met2", x0=p.b.x, y0=p.b.y, w=p.a.w, h=p.a.h);
}

cell top() {
    let r1 = rect("met1", x0=0., y0=0., w=10., h=10.);
    let r2 = crect(layer="via1", x0=50., y0=50., w=5., h=5.);
    let poly = polygon("met1", 3, x0=0., y0=0., x1=4., y1=0., x2=2., y2=3.);
    let l = inst(leaf(), x=100., y=0.);
    let c = inst(child(cons(r1, cons(r2, [])), Pair { a: r2, b: poly.points[2] }, l.m), x=0., y=0.);
}
"#;
        let cells = compile_source(source, "top", Vec::new()).unwrap_valid();
        let top = &cells.cells[&cells.top];
        let child = top
            .objects
            .values()
            .filter_map(|object| object.get_instance())
            .find(|inst| relative_eq!(inst.x, 0.))
            .expect("child instance");
        let child = &cells.cells[&child.cell];
        let rects: Vec<_> = child
            .objects
            .values()
            .filter_map(|object| object.get_rect())
            .collect();
        let at = |x0: f64, y0: f64| {
            rects
                .iter()
                .filter(|rect| relative_eq!(rect.x0.0, x0) && relative_eq!(rect.y0.0, y0))
                .collect::<Vec<_>>()
        };
        // `r1` was layout in `top`, so `first!` draws it here.
        let [first] = at(0., 0.)[..] else {
            panic!("expected one rect at the origin");
        };
        assert!(!first.construction);
        assert_eq!(first.layer.as_deref(), Some("met1"));
        // `r2` is a crect. It arrives twice, once in the sequence and once in
        // the struct, and `second!` draws neither.
        let second = at(50., 50.);
        assert_eq!(second.len(), 2);
        assert!(
            second
                .iter()
                .all(|rect| rect.construction && rect.layer.as_deref() == Some("via1"))
        );
        // `l.m` is a proxy of drawn geometry, in `top`'s frame, so `!` redraws
        // it where `top` sees it.
        let [redrawn] = at(100., 0.)[..] else {
            panic!("expected one rect at the proxy's position");
        };
        assert!(!redrawn.construction);
        assert_eq!(redrawn.layer.as_deref(), Some("met1"));
        assert_relative_eq!(redrawn.x1.0, 110., epsilon = EPSILON);
        // The struct's point and rect were read as constants.
        let [m] = at(2., 3.)[..] else {
            panic!("expected one rect at the polygon's vertex");
        };
        assert_eq!(m.layer.as_deref(), Some("met2"));
        assert_relative_eq!(m.x1.0, 7., epsilon = EPSILON);
        assert_relative_eq!(m.y1.0, 8., epsilon = EPSILON);
    }

    /// Shape values passed through the positional cell API are checked against
    /// the declared parameter type, and name the cell like every other
    /// argument.
    #[test]
    fn shape_cell_arguments_are_checked_and_name_the_cell() {
        let ast = parse_workspace_with_std(ARGON_SHAPE_CELL_ARGS).ast();
        let region = |x0: f64| CellArg::Rect {
            layer: None,
            drawable: false,
            x0,
            y0: 0.,
            x1: x0 + 90.,
            y1: 40.,
        };
        let via_array = |args: Vec<CellArg>| {
            compile(
                &ast,
                CompileInput {
                    cell: &["via_array"],
                    args,
                },
            )
        };
        let cells =
            via_array(vec![region(0.), CellArg::Float(10.), CellArg::Float(20.)]).unwrap_valid();
        // Five columns and two rows fit a 90 x 40 region.
        let drawn = cells.cells[&cells.top]
            .objects
            .values()
            .filter_map(|object| object.get_rect())
            .filter(|rect| !rect.construction)
            .count();
        assert_eq!(drawn, 10);
        // The same shape names the same cell; a shape elsewhere is a different
        // argument, so it is a different cell.
        let same =
            via_array(vec![region(0.), CellArg::Float(10.), CellArg::Float(20.)]).unwrap_valid();
        assert_eq!(cells.top, same.top);
        let moved =
            via_array(vec![region(100.), CellArg::Float(10.), CellArg::Float(20.)]).unwrap_valid();
        assert_ne!(cells.top, moved.top);

        let mismatch = |args: Vec<CellArg>, index: usize, expected: &str, found: &str| {
            let errors = via_array(args).unwrap_exec_errors().errors;
            assert!(
                errors.iter().any(|error| matches!(
                    &error.kind,
                    ExecErrorKind::InvalidCellArgumentType { index: i, expected: e, found: f }
                        if *i == index && e == expected && f == found
                )),
                "{errors:?}"
            );
        };
        mismatch(
            vec![CellArg::Float(1.), CellArg::Float(10.), CellArg::Float(20.)],
            1,
            "Rect",
            "Float",
        );
        mismatch(
            vec![region(0.), region(0.), CellArg::Float(20.)],
            2,
            "Float",
            "Rect",
        );
    }

    /// A shape built in a cell invocation, as `arc run --cell` and the GUI
    /// open-cell command supply it. The invocation's own geometry is not part
    /// of the output.
    #[test]
    fn argon_shape_cell_invocation() {
        let mut ast = parse_workspace_with_std(ARGON_SHAPE_CELL_ARGS).ast();
        let invocation = crate::parse::splice_cell_invocation(
            &mut ast,
            "via_array(crect(x0=0., y0=0., w=90., h=40.), 10., 20.)",
        )
        .expect("invocation should splice");
        let (typed, errors) = static_compile(&ast).unwrap();
        assert!(errors.errors.is_empty(), "{:?}", errors.errors);
        let config = WorkspaceConfig::default().with_tech(Some(PathBuf::from(BASIC_TECH)));
        let cells =
            crate::compile::execute_cell_invocation(&typed, &invocation, &config).unwrap_valid();
        assert_eq!(cells.cells.len(), 1);
        let top = &cells.cells[&cells.top];
        assert!(top.name.ends_with("via_array"), "{}", top.name);
        let region = top
            .objects
            .values()
            .filter_map(|object| object.get_rect())
            .find(|rect| rect.construction && rect.layer.is_none() && relative_eq!(rect.x1.0, 90.))
            .expect("the region arrives as construction geometry");
        assert_relative_eq!(region.y1.0, 40., epsilon = EPSILON);
        let drawn = top
            .objects
            .values()
            .filter_map(|object| object.get_rect())
            .filter(|rect| !rect.construction)
            .count();
        assert_eq!(drawn, 10);
    }

    /// Tuples are cell arguments like sequences and structs: element by
    /// element, shapes included, and checked against the declared arity and
    /// element types.
    #[test]
    fn tuple_cell_arguments() {
        let source = r#"
struct Placed {
    at: (Float, Float),
    size: (Int, Rect),
}

cell child(span: (Float, Float), p: Placed) {
    let w = span.1 - span.0;
    let m = rect("met2", x0=p.at.0, y0=p.at.1, w=w, h=(p.size.0 as Float));
    let drawn = p.size.1!;
}

cell top() {
    let r = rect("met1", x0=0., y0=0., w=10., h=10.);
    let c = inst(child((5., 25.,), Placed { at: (100., 200.,), size: (3, r,) }), x=0., y=0.);
}
"#;
        let cells = compile_source(source, "top", Vec::new()).unwrap_valid();
        assert_eq!(cells.cells.len(), 2);
        let (_, child) = cells
            .cells
            .iter()
            .find(|(id, _)| **id != cells.top)
            .expect("child cell");
        let rects: Vec<_> = child
            .objects
            .values()
            .filter_map(|object| object.get_rect())
            .collect();
        assert_eq!(rects.len(), 2);
        // Every element of the nested tuples was read as a constant.
        let m = rects
            .iter()
            .find(|rect| rect.layer.as_deref() == Some("met2"))
            .expect("the marker is drawn");
        assert_relative_eq!(m.x0.0, 100., epsilon = EPSILON);
        assert_relative_eq!(m.y0.0, 200., epsilon = EPSILON);
        assert_relative_eq!(m.x1.0, 120., epsilon = EPSILON);
        assert_relative_eq!(m.y1.0, 203., epsilon = EPSILON);
        // The rect inside the tuple is drawable, so `!` draws it.
        let drawn = rects
            .iter()
            .find(|rect| rect.layer.as_deref() == Some("met1"))
            .expect("the tuple's rect is drawn");
        assert!(!drawn.construction);
        assert_relative_eq!(drawn.x1.0, 10., epsilon = EPSILON);

        // Through the positional API the tuple is checked element by element.
        let pair = |a: f64, b: f64| CellArg::Tuple(vec![CellArg::Float(a), CellArg::Float(b)]);
        let placed = |size: CellArg| CellArg::Struct {
            name: "Placed".to_owned(),
            fields: vec![
                ("at".to_owned(), pair(100., 200.)),
                ("size".to_owned(), size),
            ],
        };
        let rect = CellArg::Rect {
            layer: Some("met1".to_owned()),
            drawable: true,
            x0: 0.,
            y0: 0.,
            x1: 10.,
            y1: 10.,
        };
        let root = parse_source_text(source, PathBuf::from("/virtual/lib.ar")).unwrap();
        let ast = IndexMap::from([(Vec::new(), root)]);
        let child = |args: Vec<CellArg>| {
            compile(
                &ast,
                CompileInput {
                    cell: &["child"],
                    args,
                },
            )
        };
        let good = child(vec![
            pair(5., 25.),
            placed(CellArg::Tuple(vec![CellArg::Int(3), rect.clone()])),
        ])
        .unwrap_valid();
        assert_eq!(good.cells.len(), 1);
        for (args, found) in [
            // Wrong arity.
            (
                vec![
                    CellArg::Tuple(vec![CellArg::Float(5.)]),
                    placed(CellArg::Tuple(vec![CellArg::Int(3), rect.clone()])),
                ],
                "tuple",
            ),
            // Wrong element type, nested in a struct field.
            (
                vec![
                    pair(5., 25.),
                    placed(CellArg::Tuple(vec![CellArg::Float(3.), rect.clone()])),
                ],
                "struct",
            ),
        ] {
            let errors = child(args).unwrap_exec_errors().errors;
            assert!(
                errors.iter().any(|error| matches!(
                    &error.kind,
                    ExecErrorKind::InvalidCellArgumentType { found: f, .. } if f == found
                )),
                "{errors:?}"
            );
        }
    }

    /// A shape whose coordinates the caller never pins down is passed once the
    /// caller's solver has settled them, and the caller is the cell reported.
    #[test]
    fn an_unsolved_shape_argument_waits_for_the_caller() {
        let source = r#"
cell child(r: Rect) {
    let v = rect("met1", x0=r.x0, y0=r.y0, w=10., h=10.);
}

cell top() {
    let m = rect("met1", w=50., h=50.);
    let c = inst(child(m), x=0., y=0.);
}
"#;
        let output = compile_source(source, "top", Vec::new()).unwrap_exec_errors();
        let cells = output.output.expect("the layout is still produced");
        assert_eq!(cells.cells.len(), 2);
        assert!(!output.errors.is_empty());
        assert!(
            output.errors.iter().all(|error| {
                matches!(error.kind, ExecErrorKind::Underconstrained) && error.cell == cells.top
            }),
            "{:?}",
            output.errors
        );
    }

    /// Type-checks a one-file workspace and returns the kinds of its static
    /// errors.
    fn static_errors_of(source: &str) -> Vec<StaticErrorKind> {
        let root = parse_source_text(source, PathBuf::from("/virtual/lib.ar")).unwrap();
        let ast = IndexMap::from([(Vec::new(), root)]);
        let (_, output) = static_compile(&ast).unwrap();
        output.errors.into_iter().map(|error| error.kind).collect()
    }

    /// The static errors of `body` as the body of a function that sees a
    /// `Size`, an identically shaped `Other`, and an `Int`.
    fn struct_errors(body: &str) -> Vec<StaticErrorKind> {
        static_errors_of(&format!(
            "struct Size {{ w: Float, h: Float, }}\n\
             struct Other {{ w: Float, h: Float, }}\n\
             fn f(s: Size, o: Other, n: Int) -> Float {{ {body} }}\n"
        ))
    }

    #[test]
    fn struct_literals_are_checked_against_the_declaration() {
        assert!(struct_errors("Size { w: 1., h: 2. }.w").is_empty());
        // Fields may come in any order; `..base` supplies the ones not listed.
        assert!(struct_errors("Size { h: 2., w: 1. }.w").is_empty());
        assert!(struct_errors("Size { w: 1., ..s }.h").is_empty());
        assert!(struct_errors("Size { ..s }.h").is_empty());
        assert!(struct_errors("Size { w: s.w, ..s }.h").is_empty());

        assert!(matches!(
            struct_errors("Size { w: 1. }.w").as_slice(),
            [StaticErrorKind::MissingStructFields { ty, fields }]
                if ty == "struct Size" && fields == "`h`"
        ));
        assert!(matches!(
            struct_errors("Size { w: 1., h: 2., d: 3. }.w").as_slice(),
            [StaticErrorKind::NoFieldOnTy { field, ty }] if field == "d" && ty == "struct Size"
        ));
        assert!(matches!(
            struct_errors("Size { w: 1., w: 2., h: 3. }.w").as_slice(),
            [StaticErrorKind::DuplicateStructField { field }] if field == "w"
        ));
        assert!(matches!(
            struct_errors("Size { w: 1, h: 2. }.w").as_slice(),
            [StaticErrorKind::IncorrectTy { expected, found }]
                if expected == "Float" && found == "Int"
        ));
        // Structs are nominal: a same-shaped struct is not a valid base.
        assert!(matches!(
            struct_errors("Size { w: 1., ..o }.w").as_slice(),
            [StaticErrorKind::IncorrectTy { expected, found }]
                if expected == "struct Size" && found == "struct Other"
        ));
        assert!(matches!(
            struct_errors("Size { w: 1., ..n }.w").as_slice(),
            [StaticErrorKind::IncorrectTy { found, .. }] if found == "Int"
        ));
        assert!(matches!(
            struct_errors("n { w: 1., h: 2. }.w").as_slice(),
            [StaticErrorKind::NotAStruct]
        ));
        assert!(matches!(
            struct_errors("Nope { w: 1., h: 2. }.w").as_slice(),
            [StaticErrorKind::UndeclaredVar { name }] if name == "Nope"
        ));
        assert!(matches!(
            struct_errors("s.d").as_slice(),
            [StaticErrorKind::NoFieldOnTy { field, ty }] if field == "d" && ty == "struct Size"
        ));
        // A shorthand field reads a local of the same name.
        assert!(matches!(
            struct_errors("Size { w, h: 1. }.w").as_slice(),
            [StaticErrorKind::UndeclaredVar { name }] if name == "w"
        ));
        assert!(matches!(
            struct_errors("Size { w: s.w, ..s }.w + Size { ..o }.w").as_slice(),
            [StaticErrorKind::IncorrectTy { .. }]
        ));
        // Structs cannot be compared.
        let errors = struct_errors("if (s == Size { w: 1., h: 1. }) { 1. } else { 2. }");
        assert!(!errors.is_empty());
        assert!(
            errors
                .iter()
                .all(|e| matches!(e, StaticErrorKind::ComparisonInvalidType)),
            "{errors:?}"
        );
    }

    #[test]
    fn struct_declarations_are_checked() {
        assert!(matches!(
            static_errors_of("struct Node { next: Node, }\n").as_slice(),
            [StaticErrorKind::RecursiveStruct { name }] if name == "Node"
        ));
        let errors = static_errors_of("struct A { b: [B], }\nstruct B { a: (Int, A), }\n");
        assert!(
            matches!(errors.as_slice(), [StaticErrorKind::RecursiveStruct { .. }]),
            "{errors:?}"
        );
        assert!(matches!(
            static_errors_of("struct S { a: Nope, }\n").as_slice(),
            [StaticErrorKind::UnknownType]
        ));
        assert!(matches!(
            static_errors_of("struct S { a: Int, a: Float, }\n").as_slice(),
            [StaticErrorKind::DuplicateNameDeclaration]
        ));
        assert!(matches!(
            static_errors_of("struct rect { a: Int, }\n").as_slice(),
            [StaticErrorKind::RedeclarationOfBuiltin]
        ));
        // Forward references that do not close a cycle are fine, in any order.
        assert!(
            static_errors_of(
                "struct Outer { inner: Inner, list: [Inner], pair: (Inner, Int), }\n\
                 struct Inner { x: Float, }\n\
                 fn f(o: Outer) -> Float { o.inner.x + o.list[0].x + o.pair.0.x }\n"
            )
            .is_empty()
        );
    }

    #[test]
    fn top_level_declarations_share_one_namespace() {
        // `bind` overwrote without checking, so `struct Mode` after
        // `enum Mode` quietly won, `struct F` and `fn F` resolved to whichever
        // kind the declaration passes visit last, and two `struct S` kept only
        // the second.
        let duplicates = |source: &str| {
            static_errors(source)
                .into_iter()
                .filter(|error| matches!(error.kind, StaticErrorKind::DuplicateNameDeclaration))
                .collect::<Vec<_>>()
        };
        for source in [
            "enum Mode { A, B }\nstruct Mode { a: Int, }\n",
            "struct Mode { a: Int, }\nenum Mode { A, B }\n",
            "struct F { a: Int, }\nfn F() -> Int { 1 }\n",
            "fn F() -> Int { 1 }\nstruct F { a: Int, }\n",
            "struct S { a: Int, }\nstruct S { b: Float, }\n",
            "enum E { A }\nenum E { B }\n",
            "fn f() -> Int { 1 }\nfn f() -> Float { 1. }\n",
            "cell top() {}\ncell top() {}\n",
            "fn top() -> Int { 1 }\ncell top() {}\n",
            "cell top() {}\nenum top { A }\n",
            // An import takes a name like any declaration does.
            "use std::max;\nfn max(a: Float) -> Float { a }\n",
            "use std::max;\nuse std::min as max;\n",
        ] {
            let errors = duplicates(source);
            assert_eq!(errors.len(), 1, "{source:?}: {errors:?}");
        }

        // The later declaration is the one reported, at its name.
        let source = "enum Mode { A, B }\nstruct Mode { a: Int, }\n";
        let errors = duplicates(source);
        let [error] = errors.as_slice() else {
            unreachable!("checked above")
        };
        let name = source.rfind("Mode").unwrap();
        assert_eq!(error.span.span.start(), name);
        assert_eq!(error.span.span.end(), name + "Mode".len());

        // Every repeat is reported, not just the first.
        assert_eq!(
            duplicates("cell a() {}\ncell a() {}\ncell a() {}\n").len(),
            2
        );

        // Distinct names in any mix of kinds are fine, and a name may be
        // reused across modules: `std` declares `max` too.
        assert!(
            duplicates(
                "enum A { X }\nstruct B { a: A, }\nfn c(b: B) -> A { b.a }\ncell d() {}\n\
                 fn max(a: Float) -> Float { a }\n"
            )
            .is_empty()
        );

        // Modules live in their own namespace, so `mod m;` and `fn m` coexist.
        let root = parse_source_text(
            "mod m;\nfn m() -> Int { m::h() }\n",
            PathBuf::from("/virtual/lib.ar"),
        )
        .unwrap();
        let m = parse_source_text("fn h() -> Int { 2 }\n", PathBuf::from("/virtual/m.ar")).unwrap();
        let ast = IndexMap::from([(Vec::new(), root), (vec!["m".to_owned()], m)]);
        let (_, output) = static_compile(&ast).unwrap();
        assert!(output.errors.is_empty(), "{:?}", output.errors);
    }

    #[test]
    fn argon_library() {
        let o = parse_workspace_with_std(ARGON_LIBRARY);
        assert!(o.static_errors().is_empty());
        let ast = o.ast();
        let cells = compile(
            &ast,
            CompileInput {
                cell: &["test"],
                args: Vec::new(),
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
    fn argon_path_dependencies() {
        let o = parse_workspace_with_std_and_deps(
            ARGON_PATH_DEPENDENCIES,
            [(
                "dependency".to_string(),
                PathBuf::from(EXAMPLES_DIR).join("path_dependencies/dependency_library"),
            )],
        );
        assert!(o.static_errors().is_empty());
        let ast = o.ast();
        let cells = compile(
            &ast,
            CompileInput {
                cell: &["test"],
                args: Vec::new(),
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
    fn argon_sky130_technology_uses_klayout_units() {
        let tech = crate::tech::read_tech(SKY130_TECH).unwrap();

        // SKY130.lyt uses a 0.001 micron DBU, while the technology LEF uses a
        // 0.005 micron manufacturing grid. Argon keeps source coordinates in
        // nm, so those become one DBU per display unit and five DBUs per grid.
        assert_relative_eq!(tech.dbu, 1e-9, epsilon = f64::EPSILON);
        assert_eq!(tech.display_unit, 1);
        assert_eq!(tech.grid, 5);
        assert_relative_eq!(tech.grid_step(), 5., epsilon = f64::EPSILON);
    }

    #[test]
    fn argon_sky130_inverter() {
        let o = parse_workspace_with_std(ARGON_SKY130_LIB);
        assert!(o.static_errors().is_empty());
        let ast = o.ast();
        let cells = compile_sky130(
            &ast,
            CompileInput {
                cell: &["inv"],
                args: vec![
                    CellArg::Float(1_200.),
                    CellArg::Float(2_000.),
                    CellArg::Int(4),
                ],
            },
        );
        println!("cells: {cells:?}");

        assert!(cells.is_valid());

        let work_dir =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("build/argon_sky130_inverter");
        cells
            .to_gds(work_dir.join("layout.gds"))
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
            },
        );
        println!("{cells:#?}");
        let cells = cells.unwrap_valid();
        let cell = &cells.cells[&cells.top];
        assert_eq!(cell.objects.len(), 5);
        let translated_bbox_rect = cell
            .objects
            .values()
            .filter_map(|object| object.get_rect())
            .find(|rect| rect.layer.as_deref() == Some("met3"))
            .expect("met3 should copy the placed instance bbox");
        assert_eq!(translated_bbox_rect.x0.0, 100.);
        assert_eq!(translated_bbox_rect.y0.0, 100.);
        assert_eq!(translated_bbox_rect.x1.0, 200.);
        assert_eq!(translated_bbox_rect.y1.0, 200.);
    }

    #[test]
    fn argon_bbox_nested() {
        let o = parse_workspace_with_std(ARGON_BBOX_NESTED);
        assert!(o.static_errors().is_empty());
        let ast = o.ast();
        let cells = compile(
            &ast,
            CompileInput {
                cell: &["top"],
                args: Vec::new(),
            },
        );
        println!("{cells:#?}");
        let cells = cells.unwrap_valid();
        let cell = &cells.cells[&cells.top];
        let rect_on = |layer: &str| {
            cell.objects
                .values()
                .filter_map(|object| object.get_rect())
                .find(|rect| rect.layer.as_deref() == Some(layer))
                .unwrap_or_else(|| panic!("{layer} should copy a bbox"))
        };

        // `mid` holds `leaf` at x=1000, so the cell bbox must include that offset.
        let cell_bbox = rect_on("met2");
        assert_eq!(cell_bbox.x0.0, 1000.);
        assert_eq!(cell_bbox.y0.0, 0.);
        assert_eq!(cell_bbox.x1.0, 1100.);
        assert_eq!(cell_bbox.y1.0, 100.);

        // Rotating by 90 maps (x, y) to (-y, x); the placement then adds x=500.
        let inst_bbox = rect_on("met3");
        assert_eq!(inst_bbox.x0.0, 400.);
        assert_eq!(inst_bbox.y0.0, 1000.);
        assert_eq!(inst_bbox.x1.0, 500.);
        assert_eq!(inst_bbox.y1.0, 1100.);
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
            },
        );
        println!("{cells:#?}");
        let cells = cells.unwrap_exec_errors();
        assert_eq!(cells.errors.len(), 1);
        let error = cells.errors.first().unwrap();
        assert!(matches!(error.kind, ExecErrorKind::OffGrid { .. }));
        let span = error
            .span
            .as_ref()
            .expect("off-grid error should point to its solver variable");
        let source = std::fs::read_to_string(&span.path).unwrap();
        assert_eq!(&source[span.span.start()..span.span.end()], "float()");
    }

    #[test]
    fn solver_uses_technology_grid() {
        let o = parse_workspace_with_std(ARGON_ROUNDING);
        assert!(o.static_errors().is_empty());
        let ast = o.ast();

        let tech_path = std::env::temp_dir().join(format!(
            "argon-solver-grid-{}.tech.toml",
            std::process::id()
        ));
        let tech = std::fs::read_to_string(BASIC_TECH)
            .unwrap()
            .replace("display_unit = 10", "display_unit = 10000");
        std::fs::write(&tech_path, tech).unwrap();
        let cells = compile_workspace(
            &ast,
            CompileInput {
                cell: &["top"],
                args: Vec::new(),
            },
            &WorkspaceConfig::default().with_tech(Some(tech_path.clone())),
        );
        std::fs::remove_file(tech_path).unwrap();

        assert!(cells.is_valid(), "{cells:#?}");
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
        let cells = compile_sky130(
            &ast,
            CompileInput {
                cell: &["top"],
                args: Vec::new(),
            },
        );
        println!("{cells:#?}");

        let work_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("build/argon_text");
        cells
            .to_gds(work_dir.join("layout.gds"))
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
            },
        );
        println!("{cells:#?}");

        let errors = cells.unwrap_static_errors();
        assert!(errors.errors.iter().any(
            |e| matches!(&e.kind, StaticErrorKind::UndeclaredVar { name } if name == "argument")
        ));
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
            },
        );
        println!("{cells:#?}");

        let errors = cells.unwrap_static_errors();
        assert!(
            errors.errors.iter().any(
                |e| matches!(&e.kind, StaticErrorKind::UndeclaredVar { name } if name == "size")
            )
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
            },
        );
        println!("{cells:#?}");

        let errors = cells.unwrap_exec_errors();
        let error = errors
            .errors
            .iter()
            .find(|error| matches!(error.kind, ExecErrorKind::Underconstrained))
            .expect("expected an underconstrained error");
        let span = error
            .span
            .as_ref()
            .expect("underconstrained error should point to an unsolved variable");
        let source = std::fs::read_to_string(&span.path).unwrap();
        assert_eq!(
            &source[span.span.start()..span.span.end()],
            "inst(bot_cell, angle=90)"
        );
    }

    #[test]
    fn underconstrained_errors_point_to_each_source_expression() {
        let source = r#"
            cell top() {
                let first = rect("met1");
                let second = rect("met1");
            }
        "#;
        let path = PathBuf::from("/virtual/lib.ar");
        let root = parse_source_text(source, path.clone()).unwrap();
        let std = parse_source_text(
            crate::parse::STD_SOURCE,
            PathBuf::from(crate::parse::STD_PATH),
        )
        .unwrap();
        let ast = IndexMap::from([(Vec::new(), root), (vec!["std".to_owned()], std)]);

        let errors = compile(
            &ast,
            CompileInput {
                cell: &["top"],
                args: Vec::new(),
            },
        )
        .unwrap_exec_errors()
        .errors
        .into_iter()
        .filter(|error| matches!(error.kind, ExecErrorKind::Underconstrained))
        .collect::<Vec<_>>();

        assert_eq!(errors.len(), 2);
        let spans = errors
            .iter()
            .map(|error| error.span.as_ref().expect("error should point to a value"))
            .collect::<Vec<_>>();
        assert!(spans.iter().all(|span| span.path == path));
        assert_eq!(
            spans
                .iter()
                .map(|span| &source[span.span.start()..span.span.end()])
                .collect::<Vec<_>>(),
            vec!["rect(\"met1\")", "rect(\"met1\")"]
        );
        assert_ne!(spans[0], spans[1]);
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
    fn argon_polygon_points_are_independently_constrained() {
        let o = parse_workspace_with_std(ARGON_POLYGON);
        assert!(o.static_errors().is_empty(), "{:?}", o.static_errors());
        let ast = o.ast();
        let output = compile(
            &ast,
            CompileInput {
                cell: &["top"],
                args: Vec::new(),
            },
        );
        let gds_path =
            std::env::temp_dir().join(format!("argon-polygon-{}.gds", std::process::id()));
        output.to_gds(&gds_path).expect("export polygon GDS");
        let gds = GdsLibrary::load(gds_path).expect("reload polygon GDS");
        let boundary = gds.structs[0]
            .elems
            .iter()
            .find_map(|element| match element {
                GdsElement::GdsBoundary(boundary) => Some(boundary),
                _ => None,
            })
            .expect("polygon boundary");
        assert_eq!(boundary.xy.len(), 5);
        assert_eq!(boundary.xy.first(), boundary.xy.last());

        let cells = match output {
            CompileOutput::Valid(output) => output,
            CompileOutput::ExecErrors(output) => {
                output.output.expect("underconstrained polygon output")
            }
            output => panic!("polygon should compile: {output:?}"),
        };

        let polygon = cells.cells[&cells.top]
            .objects
            .values()
            .find_map(SolvedValue::get_polygon)
            .expect("compiled polygon");
        let points = polygon
            .points
            .iter()
            .map(|(x, y)| (x.0, y.0))
            .collect::<Vec<_>>();
        assert_eq!(&points[..3], &[(0., 0.), (100., 0.), (150., 75.)]);
        assert!(points[3].0.is_finite() && points[3].1.is_finite());
        assert_ne!(polygon.points[0].0.1, polygon.points[1].0.1);
        let unsolved = &cells.cells[&cells.top].unsolved_vars;
        assert!(
            polygon.points[3]
                .0
                .1
                .coeffs
                .iter()
                .any(|(_, var)| unsolved.contains(var))
        );
        assert!(
            polygon.points[3]
                .1
                .1
                .coeffs
                .iter()
                .any(|(_, var)| unsolved.contains(var))
        );

        let initial = compile(
            &ast,
            CompileInput {
                cell: &["initial_points"],
                args: Vec::new(),
            },
        );
        let initial = match initial {
            CompileOutput::Valid(output) => output,
            CompileOutput::ExecErrors(output) => output.output.expect("fallback polygon output"),
            output => panic!("fallback polygon should compile: {output:?}"),
        };
        let fallbacks = &initial.cells[&initial.top].fallback_constraints_used;
        assert_eq!(fallbacks.len(), 6);
        assert!(fallbacks.iter().all(|fallback| matches!(
            fallback.initial_condition,
            Some(RectInitialCondition::PolygonX(_, _)) | Some(RectInitialCondition::PolygonY(_, _))
        )));
        let source = std::fs::read_to_string(ARGON_POLYGON).unwrap();
        assert!(fallbacks.iter().all(|fallback| {
            source[fallback.span.span.start()..fallback.span.span.end()].ends_with('.')
        }));
    }

    #[test]
    fn polygon_points_require_named_coordinate_fields() {
        let root = parse_source_text(
            r#"
                cell top() {
                    let p = polygon("met1", 3,
                        x0=0., y0=0.,
                        x1=100., y1=0.,
                        x2=50., y2=50.,
                    );
                    eq(p.points[0].0, 0.);
                }
            "#,
            PathBuf::from("/virtual/lib.ar"),
        )
        .unwrap();
        let std = parse_source_text(
            crate::parse::STD_SOURCE,
            PathBuf::from(crate::parse::STD_PATH),
        )
        .unwrap();
        let ast = IndexMap::from([(Vec::new(), root), (vec!["std".to_owned()], std)]);
        let (_, output) = static_compile(&ast).unwrap();
        assert!(output.errors.iter().any(|error| matches!(
            &error.kind,
            StaticErrorKind::CannotIndexFieldAccess { ty } if ty == "Point"
        )));
    }

    #[test]
    fn polygon_constructor_has_one_count_based_signature() {
        let root = parse_source_text(
            r#"
                cell top() {
                    let p = polygon("met1", list(
                        (0., 0.,),
                        (10., 0.,),
                        (0., 10.,),
                    ));
                }
            "#,
            PathBuf::from("/virtual/lib.ar"),
        )
        .unwrap();
        let std = parse_source_text(
            crate::parse::STD_SOURCE,
            PathBuf::from(crate::parse::STD_PATH),
        )
        .unwrap();
        let ast = IndexMap::from([(Vec::new(), root), (vec!["std".to_owned()], std)]);
        let (_, output) = static_compile(&ast).unwrap();
        assert!(output.errors.iter().any(|error| matches!(
            &error.kind,
            StaticErrorKind::IncorrectTy { expected, .. } if expected == "Int"
        )));
    }

    fn static_errors(source: &str) -> Vec<crate::compile::StaticError> {
        let root = parse_source_text(source, PathBuf::from("/virtual/lib.ar")).unwrap();
        let std = parse_source_text(
            crate::parse::STD_SOURCE,
            PathBuf::from(crate::parse::STD_PATH),
        )
        .unwrap();
        let ast = IndexMap::from([(Vec::new(), root), (vec!["std".to_owned()], std)]);
        let (_, output) = static_compile(&ast).unwrap();
        output.errors
    }

    /// Asserts that `source` reports an error accepted by `matches`.
    #[track_caller]
    fn assert_static_error(source: &str, matches: impl Fn(&StaticErrorKind) -> bool) {
        let errors = static_errors(source);
        assert!(
            errors.iter().any(|error| matches(&error.kind)),
            "{errors:?}"
        );
    }

    #[test]
    fn keyword_parameters_type_check() {
        let errors = static_errors(
            r#"
            fn scaled(x: Float, scale: Float = 2., offset: Float = x) -> Float {
                x * scale + offset
            }
            cell child(w: Float, h: Float = w, layer: String = "met1") {
                let body = rect(layer, x0=0., y0=0., x1=w, y1=h);
            }
            cell top(n: Int = 1) {
                let a = scaled(1.);
                let b = scaled(1., offset=0., scale=3.);
                let c = inst(child(10.));
                let d = inst(child(10., layer="met2", h=20.));
            }
            "#,
        );
        assert!(errors.is_empty(), "{errors:?}");
    }

    #[test]
    fn keyword_parameter_declarations_are_checked() {
        assert_static_error(
            "fn f(a: Float, b: Float = 1., c: Float) -> Float { a }",
            |kind| matches!(kind, StaticErrorKind::PositionalParamAfterDefault { name } if name == "c"),
        );
        assert_static_error("fn f(a: Float, a: Int) -> Float { a }", |kind| {
            matches!(kind, StaticErrorKind::DuplicateNameDeclaration)
        });
        assert_static_error("cell c(w: Float, w: Float = 1.) {}", |kind| {
            matches!(kind, StaticErrorKind::DuplicateNameDeclaration)
        });
        assert_static_error("fn f(b: Float = 1) -> Float { b }", |kind| {
            matches!(
                kind,
                StaticErrorKind::IncorrectTy { expected, found } if expected == "Float" && found == "Int"
            )
        });
        // A default sees only the parameters before it.
        assert_static_error(
            "fn f(a: Float = b, b: Float = 1.) -> Float { a }",
            |kind| matches!(kind, StaticErrorKind::UndeclaredVar { name } if name == "b"),
        );
        assert_static_error(
            "fn f(a: Float = a) -> Float { a }",
            |kind| matches!(kind, StaticErrorKind::UndeclaredVar { name } if name == "a"),
        );
    }

    #[test]
    fn keyword_arguments_are_checked_at_calls() {
        let call = |args: &str| {
            format!(
                "fn f(a: Float, b: Float = 1.) -> Float {{ a + b }}\ncell top() {{ let v = f({args}); }}"
            )
        };
        // A keyword parameter cannot be passed positionally.
        assert_static_error(&call("1., 2."), |kind| {
            matches!(
                kind,
                StaticErrorKind::CallIncorrectPositionalArity {
                    expected: 1,
                    found: 2
                }
            )
        });
        // A positional parameter cannot be passed by keyword.
        assert_static_error(&call("a=1."), |kind| {
            matches!(kind, StaticErrorKind::InvalidKwArg)
        });
        assert_static_error(&call("1., zz=2."), |kind| {
            matches!(kind, StaticErrorKind::InvalidKwArg)
        });
        assert_static_error(&call("1., b=2., b=3."), |kind| {
            matches!(kind, StaticErrorKind::DuplicateKwArg)
        });
        assert_static_error(&call("1., b=true"), |kind| {
            matches!(
                kind,
                StaticErrorKind::IncorrectTy { expected, found } if expected == "Float" && found == "Bool"
            )
        });
        assert!(static_errors(&call("1., b=2.")).is_empty());
    }

    fn comparison_source(comparison: &str) -> String {
        format!(
            r#"
                enum E {{ A, B }}
                fn ident(value: Any) -> Any {{ value }}
                cell top() {{
                    let r = rect("met1", x0=0., y0=0., y1=10.);
                    let e = E::A;
                    let anything = ident(3);
                    let s = cons(1, []);
                    let t = cons(2, []);
                    let empty = [];
                    if {comparison} {{ eq(r.x1, 100.); }} else {{ eq(r.x1, 200.); }};
                }}
            "#
        )
    }

    #[test]
    fn comparison_checks_both_operands() {
        // Each of these used to pass `arc check` and then hit an `unreachable!()`
        // in the evaluator, because only the left operand was type checked.
        for comparison in [
            "1. < 2",
            "1 < 2.",
            "1 == E::A",
            r#"1 == "x""#,
            "1 < r",
            "1. == anything",
            "1. < anything",
        ] {
            let errors = static_errors(&comparison_source(comparison));
            assert!(
                errors.iter().any(|error| matches!(
                    error.kind,
                    StaticErrorKind::ComparisonMismatchedTypes
                        | StaticErrorKind::ComparisonInvalidType
                )),
                "`{comparison}` should be rejected as a comparison: {errors:?}"
            );
        }
    }

    #[test]
    fn sequences_only_compare_for_equality_against_seq_nil() {
        // The evaluator has no arm for two populated sequences, and none for
        // ordering a sequence, so everything but `seq == []` must be rejected.
        for comparison in ["s == t", "s != t", "s < t", "s < []", "[] > s"] {
            let errors = static_errors(&comparison_source(comparison));
            assert!(
                errors
                    .iter()
                    .any(|error| matches!(error.kind, StaticErrorKind::SeqMustCompareEqSeqNil)),
                "`{comparison}` should be rejected as a sequence comparison: {errors:?}"
            );
        }
    }

    #[test]
    fn comparison_accepts_matching_operands() {
        for comparison in [
            "1 < 2",
            "1. < 2.",
            "e == E::A",
            "s == []",
            "[] != s",
            "empty == []",
        ] {
            let errors = static_errors(&comparison_source(comparison));
            assert!(
                errors.is_empty(),
                "`{comparison}` should type check: {errors:?}"
            );
        }
    }

    fn fn_decl_source(decl: &str) -> String {
        format!(
            r#"
                enum E {{ A, B }}
                {decl}
                cell top() {{
                    let r = rect("met1", x0=0., y0=0., x1=1., y1=2.)!;
                }}
            "#
        )
    }

    #[test]
    fn fn_body_must_match_declared_return_ty() {
        // Each of these used to pass `arc check`, then abort the evaluator: callers
        // trust the declared return type, so the body's value reaches an `unwrap_*`
        // expecting the declared representation.
        for decl in [
            "fn f() -> Float { }",
            "fn f() -> Bool { 1 }",
            "fn f() -> Float { 1 }",
            "fn f() -> Int { 1. }",
            "fn f() -> Int { E::A }",
            "fn f() -> [Int] { 1 }",
            "fn f() -> (Int, Int) { 1 }",
            "fn f() -> [Int] { cons(1., []) }",
            "fn f(x: Int) { x }",
        ] {
            let errors = static_errors(&fn_decl_source(decl));
            assert!(
                errors
                    .iter()
                    .any(|error| matches!(error.kind, StaticErrorKind::IncorrectTy { .. })),
                "`{decl}` should be rejected: its body does not return the declared type: {errors:?}"
            );
        }
    }

    #[test]
    fn fn_body_matching_declared_return_ty_is_accepted() {
        for decl in [
            "fn f() { }",
            "fn f() -> Float { 1. }",
            "fn f() -> Int { 1 }",
            "fn f() -> Bool { true }",
            "fn f() -> E { E::A }",
            "fn f(x: Int) -> Int { x }",
            "fn f() -> [Int] { cons(1, []) }",
            // An empty sequence inhabits every sequence type, as `is_eq_ty` allows.
            "fn f() -> [Int] { [] }",
            "fn f() -> (Int, Float) { (1, 2.,) }",
            // A trailing semicolon makes the body's value `()`, matching no return type.
            "fn f() { let x = 1; }",
            "fn f() -> Any { 1 }",
            "fn f(x: Any) -> Int { x }",
        ] {
            let errors = static_errors(&fn_decl_source(decl));
            assert!(errors.is_empty(), "`{decl}` should type check: {errors:?}");
        }
    }

    #[test]
    fn argon_path_dimensions_are_constrained_and_exported() {
        let root = parse_source_text(
            r#"
                cell top() {
                    let route = path("met1", 3,
                        width=20.,
                        begin_extension=5.,
                        end_extension=7.,
                        x0=0., y0=0.,
                        x1=100., y1=0.,
                        y2=50.,
                    );
                    eq(route.x2, route.points[1].x);
                    eq(route.width, 20.);
                    eq(route.begin_extension, 5.);
                    eq(route.end_extension, 7.);
                }
            "#,
            PathBuf::from("/virtual/lib.ar"),
        )
        .unwrap();
        let std = parse_source_text(
            crate::parse::STD_SOURCE,
            PathBuf::from(crate::parse::STD_PATH),
        )
        .unwrap();
        let ast = IndexMap::from([(Vec::new(), root), (vec!["std".to_owned()], std)]);
        let output = compile(
            &ast,
            CompileInput {
                cell: &["top"],
                args: Vec::new(),
            },
        );
        let CompileOutput::Valid(cells) = &output else {
            panic!("path should be fully constrained: {output:?}");
        };
        let path = cells.cells[&cells.top]
            .objects
            .values()
            .find_map(SolvedValue::get_path)
            .expect("compiled path");
        assert_eq!(path.width.0, 20.);
        assert_eq!(path.begin_extension.0, 5.);
        assert_eq!(path.end_extension.0, 7.);
        assert_eq!(
            path.points
                .iter()
                .map(|(x, y)| (x.0, y.0))
                .collect::<Vec<_>>(),
            [(0., 0.), (100., 0.), (100., 50.)]
        );

        let gds_path = std::env::temp_dir().join(format!("argon-path-{}.gds", std::process::id()));
        output.to_gds(&gds_path).expect("export path GDS");
        let gds = GdsLibrary::load(gds_path).expect("reload path GDS");
        let path = gds.structs[0]
            .elems
            .iter()
            .find_map(|element| match element {
                GdsElement::GdsPath(path) => Some(path),
                _ => None,
            })
            .expect("GDS path element");
        assert_eq!(path.width, Some(200));
        assert_eq!(path.path_type, Some(4));
        assert_eq!(path.begin_extn, Some(50));
        assert_eq!(path.end_extn, Some(70));
        assert_eq!(path.xy.len(), 3);
    }

    #[test]
    fn argon_path_example_compiles() {
        let parsed = parse_workspace_with_std(ARGON_PATH);
        assert!(
            parsed.static_errors().is_empty(),
            "{:?}",
            parsed.static_errors()
        );
        let ast = parsed.ast();
        for cell_name in ["top", "initial_path", "custom_extensions"] {
            let output = compile(
                &ast,
                CompileInput {
                    cell: &[cell_name],
                    args: Vec::new(),
                },
            );
            let output = match output {
                CompileOutput::Valid(output) => output,
                CompileOutput::ExecErrors(output) => output
                    .output
                    .unwrap_or_else(|| panic!("path example cell `{cell_name}` needs output")),
                output => panic!("path example cell `{cell_name}` should compile: {output:?}"),
            };
            let path = output.cells[&output.top]
                .objects
                .values()
                .find_map(SolvedValue::get_path)
                .expect("example cell should contain a path");
            if cell_name == "custom_extensions" {
                assert_eq!(path.begin_extension.0, 5.);
                assert_eq!(path.end_extension.0, 10.);
            }
            if cell_name == "initial_path" {
                let fallbacks = &output.cells[&output.top].fallback_constraints_used;
                assert!(fallbacks.iter().any(|fallback| matches!(
                    fallback.initial_condition,
                    Some(RectInitialCondition::PathBeginExtension(_))
                )));
                assert!(fallbacks.iter().any(|fallback| matches!(
                    fallback.initial_condition,
                    Some(RectInitialCondition::PathEndExtension(_))
                )));
            }
        }
    }

    #[test]
    fn sharp_path_corner_matches_klayout_cutoff_outline() {
        let root = parse_source_text(
            r#"
                cell top() {
                    path("met1", 4,
                        width=20.,
                        x0=0., y0=0.,
                        x1=100., y1=0.,
                        x2=150., y2=75.,
                        x3=110.3, y3=14.9,
                    );
                }
            "#,
            PathBuf::from("/virtual/lib.ar"),
        )
        .unwrap();
        let std = parse_source_text(
            crate::parse::STD_SOURCE,
            PathBuf::from(crate::parse::STD_PATH),
        )
        .unwrap();
        let ast = IndexMap::from([(Vec::new(), root), (vec!["std".to_owned()], std)]);
        let output = compile(
            &ast,
            CompileInput {
                cell: &["top"],
                args: Vec::new(),
            },
        )
        .unwrap_valid();
        let path = output.cells[&output.top]
            .objects
            .values()
            .find_map(SolvedValue::get_path)
            .unwrap();
        let outline = path.outline().unwrap();
        let bbox = path.bbox().unwrap();

        assert_eq!(outline.len(), 11);
        assert_relative_eq!(bbox.x0, 0., epsilon = EPSILON);
        assert_relative_eq!(bbox.y0, -10., epsilon = EPSILON);
        assert_relative_eq!(bbox.x1, 163.85563301818058, epsilon = EPSILON);
        assert_relative_eq!(bbox.y1, 88.86750490563072, epsilon = EPSILON);
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
            },
        );
        println!("{cells:#?}");

        let cells = cells.unwrap_exec_errors().output.unwrap();
        let cell = &cells.cells[&cells.top];
        println!("SSE basis = {:?}", cell.sse_basis);
        let crate::compile::SseBasis::Nullspace(vectors) = &cell.sse_basis else {
            panic!("sparse SSE system should expose a null-space basis");
        };
        assert_eq!(vectors.len(), 1);
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
        let cells = compile_sky130(
            &ast,
            CompileInput {
                cell: &["diff_vco_top"],
                args: vec![],
            },
        );
        println!("cells: {cells:?}");

        assert!(cells.is_valid());

        let work_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("build/argon_sky130_vco");
        let gds_path = work_dir.join("layout.gds");
        cells.to_gds(&gds_path).expect("Failed to write to GDS");

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

    // -----------------------------------------------------------------------
    // Regression tests for inputs that used to abort the compiler.
    //
    // Every case below reached a panic, a stack overflow, an allocation abort,
    // or a non-terminating solve from source that `--check` accepted. They are
    // grouped here because the invariant they share is the point: a user's
    // source must never take down the compiler, only produce a diagnostic.

    /// Writes `source` to a scratch library and parses it with the standard
    /// library, as the CLI does.
    fn scratch_workspace(
        name: &str,
        source: &str,
    ) -> (tempfile::TempDir, crate::parse::ParseOutput) {
        let dir = tempfile::tempdir().expect("create scratch workspace");
        let lib = dir.path().join("lib.ar");
        std::fs::write(&lib, source).expect("write scratch library");
        let _ = name;
        let output = parse_workspace_with_std(&lib);
        (dir, output)
    }

    /// The static errors `--check` would report for `source`.
    fn check_source(source: &str) -> Vec<StaticErrorKind> {
        let (_dir, output) = scratch_workspace("check", source);
        let parse_errors = output.static_errors();
        if !parse_errors.is_empty() {
            return parse_errors.into_iter().map(|error| error.kind).collect();
        }
        let (_, errors) = static_compile(&output.ast()).expect("source must parse");
        errors.errors.into_iter().map(|error| error.kind).collect()
    }

    /// Executes `top()` in `source` and returns the execution errors, which is
    /// empty when the cell compiles cleanly.
    fn run_source(source: &str) -> Vec<ExecErrorKind> {
        let (_dir, output) = scratch_workspace("run", source);
        assert!(
            output.static_errors().is_empty(),
            "source must parse to exercise the evaluator: {:#?}",
            output.static_errors()
        );
        let compiled = compile(
            &output.ast(),
            CompileInput {
                cell: &["top"],
                args: Vec::new(),
            },
        );
        match compiled {
            CompileOutput::ExecErrors(errors) => {
                errors.errors.into_iter().map(|error| error.kind).collect()
            }
            CompileOutput::StaticErrors(errors) => {
                panic!("source must check cleanly to exercise the evaluator: {errors:#?}")
            }
            _ => Vec::new(),
        }
    }

    fn assert_reports(errors: &[ExecErrorKind], predicate: impl Fn(&ExecErrorKind) -> bool) {
        assert!(
            errors.iter().any(predicate),
            "expected a diagnostic, got {errors:#?}"
        );
    }

    #[test]
    fn bbox_without_arguments_reports_arity_instead_of_panicking() {
        // Indexing the argument slice after `assert_eq_arity` merely records a
        // diagnostic made a one-token typo crash `--check`, the path the
        // language server runs on every keystroke.
        let errors = check_source("cell top() { let b = bbox(); }");
        assert!(
            errors.iter().any(|error| matches!(
                error,
                StaticErrorKind::CallIncorrectPositionalArity {
                    expected: 1,
                    found: 0
                }
            )),
            "{errors:#?}"
        );
    }

    #[test]
    fn mismatched_branch_types_are_rejected() {
        // `Ty::lub` used to widen a mismatch to `Ty::Any`, which satisfies
        // every later check and deferred the failure to an evaluator `unwrap`.
        for source in [
            "cell top() { let a = float(); let n = if a < 1. { 1 } else { 2. }; eq(a, n); }",
            "enum E { A, B }
             cell top() { let v = E::A; let n = match v { E::A => 1, E::B => 2., }; }",
        ] {
            let errors = check_source(source);
            assert!(
                errors
                    .iter()
                    .any(|error| matches!(error, StaticErrorKind::BranchesDifferentTypes)),
                "{errors:#?}"
            );
        }
    }

    #[test]
    fn heterogeneous_list_elements_are_rejected() {
        let errors = check_source("cell top() { let a = list(1., 2); }");
        assert!(
            errors
                .iter()
                .any(|error| matches!(error, StaticErrorKind::IncorrectTy { .. })),
            "{errors:#?}"
        );
    }

    #[test]
    fn match_on_any_still_checks_its_arms() {
        // The arm checks used to be conditional on a statically known enum
        // scrutinee, which `Any` never is -- and `Any` is the common case,
        // because cell and instance types cannot be named.
        let non_exhaustive = check_source(
            "enum E { A, B }
             cell top(v: Any) { let n = match v { E::A => 1, }; }",
        );
        assert!(
            non_exhaustive
                .iter()
                .any(|error| matches!(error, StaticErrorKind::MatchArmsNotComprehensive)),
            "{non_exhaustive:#?}"
        );

        let duplicated = check_source(
            "enum E { A, B }
             cell top(v: Any) { let n = match v { E::A => 1, E::A => 2, E::B => 3, }; }",
        );
        assert!(
            duplicated
                .iter()
                .any(|error| matches!(error, StaticErrorKind::DuplicateMatchArm)),
            "{duplicated:#?}"
        );
    }

    #[test]
    fn only_layout_elements_can_be_emitted() {
        // `!` imposed no type restriction, while the evaluator asserted the
        // value was a single element.
        for source in [
            "cell top() { let v = 1!; }",
            "cell top() { let v = \"hi\"!; }",
            "cell bot() { let r = rect(\"met1\", x0=0., y0=0., x1=1., y1=1.)!; }
             cell top() { let v = bot()!; }",
            "cell top() {
                 let a = rect(\"met1\", x0=0., y0=0., x1=1., y1=1.);
                 let b = rect(\"met2\", x0=0., y0=0., x1=1., y1=1.);
                 let s = list(a, b)!;
             }",
        ] {
            let errors = check_source(source);
            assert!(
                errors
                    .iter()
                    .any(|error| matches!(error, StaticErrorKind::CannotEmit(_))),
                "{source}\n{errors:#?}"
            );
        }
    }

    #[test]
    fn integer_arithmetic_is_checked() {
        // Raw operators panicked on a zero divisor in every profile and, on
        // overflow, panicked in debug but wrapped silently in release.
        assert_reports(&run_source("cell top() { let n = 1 / 0; }"), |error| {
            matches!(error, ExecErrorKind::DivideByZero(_))
        });
        assert_reports(&run_source("cell top() { let n = 5 % 0; }"), |error| {
            matches!(error, ExecErrorKind::DivideByZero(_))
        });
        assert_reports(
            &run_source("cell top() { let n = 9223372036854775807 + 1; }"),
            |error| matches!(error, ExecErrorKind::IntegerOverflow(_)),
        );
        assert_reports(
            &run_source("cell top() { let n = -(-9223372036854775807 - 1); }"),
            |error| matches!(error, ExecErrorKind::IntegerOverflow(_)),
        );
    }

    #[test]
    fn non_finite_values_are_rejected_before_the_solver() {
        // A non-finite coefficient reaching the dense SVD fallback never
        // converges: `Matrix::svd` passes `max_niter = 0`, which that loop
        // treats as no limit at all. A hang is not catchable, so the value has
        // to be rejected where it is created.
        assert_reports(
            &run_source(
                "cell top() {
                     let a = rect(\"met1\", x0=0., y0=0., y1=1.);
                     eq(a.x1, 1. / 0.);
                 }",
            ),
            |error| matches!(error, ExecErrorKind::NonFiniteValue),
        );
    }

    #[test]
    fn shape_point_counts_are_bounded() {
        // The count had only a lower bound, so a large one aborted the process
        // on allocation failure, bypassing diagnostics entirely.
        assert_reports(
            &run_source(
                "cell top() { let p = polygon(\"met1\", 1000000000000000, x0=0., y0=0.)!; }",
            ),
            |error| matches!(error, ExecErrorKind::LimitExceeded { .. }),
        );
        assert_reports(
            &run_source(
                "cell top() { let p = path(\"met1\", 1000000000000000, width=1., x0=0., y0=0.)!; }",
            ),
            |error| matches!(error, ExecErrorKind::LimitExceeded { .. }),
        );
    }

    #[test]
    fn eager_function_recursion_reports_a_limit() {
        // `if`/`match` branches are deferred onto the worklist, but a `fn` call
        // is inlined eagerly, so a recursive call outside a branch has no
        // terminating case and used to abort the process.
        //
        // Run on a compilation-sized stack, as the CLI and the analyzer worker
        // do: the depth limit is what must stop the descent, and the default
        // test-thread stack is too small to reach it.
        let errors = crate::run_with_stack("argon-compile", || {
            run_source(
                "fn f(n: Int) -> Int { let m = f(n-1); if n <= 0 { 0 } else { m } }
                 cell top() { let k = f(3); }",
            )
        });
        assert_reports(&errors, |error| {
            matches!(error, ExecErrorKind::RecursionLimitExceeded { .. })
        });
    }

    // -----------------------------------------------------------------------
    // Diagnostics-quality regressions. Each case produced a message that was
    // unusable rather than merely imperfect: unbounded in size, naming the
    // wrong thing, or contradicting itself.

    #[test]
    fn hierarchical_type_errors_stay_readable() {
        // `Ty` had no `Display`, so nine `#[error]` strings formatted it with
        // `{:?}` -- and `Debug` re-expands the `Arc`-shared `CellTy` DAG into a
        // tree, making one message exponential in hierarchy depth. A depth-8
        // binary hierarchy produced 30 KB, depth 20 produced 122 MB, and the
        // analyzer pushed the whole string into an LSP diagnostic on every
        // keystroke.
        let depth = 16;
        let mut source =
            String::from("cell h0() { let r = rect(\"met1\", x0=0., y0=0., x1=1., y1=1.)!; }\n");
        for k in 1..=depth {
            source.push_str(&format!(
                "cell h{k}() {{ let p = inst(h{}()); let q = inst(h{}()); }}\n",
                k - 1,
                k - 1
            ));
        }
        source.push_str(&format!(
            "fn takes_float(v: Float) -> Float {{ v }}
             cell top() {{ let a = inst(h{depth}()); let b = takes_float(a); }}"
        ));

        let errors = check_source(&source);
        let message = errors
            .iter()
            .find_map(|error| match error {
                StaticErrorKind::IncorrectTy { found, .. } => Some(found.clone()),
                _ => None,
            })
            .unwrap_or_else(|| panic!("{errors:#?}"));
        assert_eq!(message, format!("Inst(h{depth})"));
    }

    #[test]
    fn an_undeclared_name_reports_one_error() {
        // `Ty::Unknown` documents that it "suppresses type checking of
        // dependent properties", but `is_eq_ty` special-cased only `Ty::Any`
        // and fell through to `==` for `Unknown`, so every already-diagnosed
        // expression produced a second `expected .., found Unknown`.
        //
        // Six predicates encode "this type satisfies every check". Teaching
        // only `is_eq_ty`, `assert_ty_is_cell` and `assert_ty_is_enum` about
        // `Unknown` left the other three still reporting a second error, so
        // they all go through `Ty::is_wildcard` now and all are covered here.
        for source in [
            "cell top() {
                 let a = float();
                 eq(a, undeclared_thing);
                 let r = rect(\"met1\", x0=0., y0=0., x1=1., y1=a)!;
             }",
            "cell top() { let c = missing_cell(); let i = inst(c); }",
            "cell top() { let a = undeclared_thing + 1.; }",
            "cell top() { let a = 1. + undeclared_thing; }",
            "cell top() { let a = -undeclared_thing; }",
            "cell top() { let a = undeclared_thing < 1.; }",
            "cell top() { let a = if undeclared_thing { 1. } else { 2. }; }",
            "cell top() { let a = undeclared_thing as Int; }",
        ] {
            let errors = check_source(source);
            assert_eq!(
                errors.len(),
                1,
                "an already-diagnosed name must not cascade\n{source}\n{errors:#?}"
            );
            assert!(
                matches!(errors[0], StaticErrorKind::UndeclaredVar { .. }),
                "{source}\n{errors:#?}"
            );
        }
    }

    #[test]
    fn a_match_that_names_no_enum_is_reported() {
        // `dispatch_match_expr` returned `lub_ty.unwrap_or_default()` --
        // `Ty::Unknown` -- with no diagnostic when neither the scrutinee nor
        // any arm pattern resolved to an enum. That was survivable only while
        // `is_eq_ty` compared `Unknown` structurally; once `Unknown` satisfies
        // every check it silently suppressed the caller's checks too, and
        // `--check` accepted a program the evaluator refuses.
        let errors = check_source(
            "enum E { A, B }
             fn pick(v: Any, k: Float) -> Float { match v { k => 1., } }
             cell top() {
                 let n = pick(E::A, 2.);
                 let r = rect(\"met1\", x0=0., y0=0., x1=n, y1=1.)!;
             }",
        );
        assert!(
            errors
                .iter()
                .any(|error| matches!(error, StaticErrorKind::NotAnEnum)),
            "{errors:#?}"
        );

        // A match that does name an enum still type checks.
        assert!(
            check_source(
                "enum E { A, B }
                 cell top() {
                     let e = E::A;
                     let w = match e { E::A => 1., E::B => 2., };
                     let r = rect(\"met1\", x0=0., y0=0., x1=w, y1=1.)!;
                 }"
            )
            .is_empty()
        );
    }

    #[test]
    fn same_named_declarations_render_distinctly() {
        // `Display for Ty` printed a cell's bare declared name and an enum's
        // variants alone, but `CellTy` equality excludes the name and `EnumTy`
        // equality keys on `id` -- so a mismatch between two distinct types
        // rendered as `expected type Inst(child), found Inst(child)`, a
        // message that refutes itself.
        let errors = check_source(
            "enum Dir { N, S }
             enum Side { N, S }
             fn takes_side(s: Side) -> Side { s }
             cell top() { let d = takes_side(Dir::N); }",
        );
        assert!(
            errors.iter().any(|error| matches!(
                error,
                StaticErrorKind::IncorrectTy { expected, found } if expected != found
            )),
            "{errors:#?}"
        );
    }

    #[test]
    fn a_misspelled_field_on_a_cell_is_not_a_placement_problem() {
        // The `Ty::Cell`/`Ty::CellFn` arms fired without consulting the field
        // map, so a typo was reported as "place it with `inst(...)` first" --
        // advice that cannot help, because placing the cell reports the typo.
        let errors = check_source(
            "cell child() { let r = rect(\"met1\", x0=0., y0=0., x1=1., y1=1.)!; }
             cell top() {
                 let c = child();
                 text(\"t\", \"text.label\", c.nonexistent.x0, 0.);
             }",
        );
        assert!(
            errors
                .iter()
                .any(|error| matches!(error, StaticErrorKind::NoFieldOnTy { field, .. } if field == "nonexistent")),
            "{errors:#?}"
        );
        assert!(
            !errors
                .iter()
                .any(|error| matches!(error, StaticErrorKind::CellFieldBeforePlacement { .. })),
            "{errors:#?}"
        );
    }

    #[test]
    fn reading_a_field_of_an_unplaced_cell_says_so() {
        // `dispatch_field_access_expr` had no `Ty::Cell` arm, so this fell
        // through to the generic no-field error -- which, because `Ty` printed
        // with `{:?}`, asserted that `r` was missing while printing the field
        // map `{"r": Rect}` that contained it. The evaluator refuses the same
        // read, so the type checker now agrees with it.
        let errors = check_source(
            "cell child() { let r = rect(\"met1\", x0=0., y0=0., x1=10., y1=10.)!; }
             cell top() {
                 let c = child();
                 text(\"t\", \"text.label\", c.r.x0, 0.);
             }",
        );
        assert!(
            errors.iter().any(|error| matches!(
                error,
                StaticErrorKind::CellFieldBeforePlacement { cell, field }
                    if cell == "child" && field == "r"
            )),
            "{errors:#?}"
        );

        // The same read on an uncalled cell function names the call it needs.
        let errors = check_source(
            "cell child() { let r = rect(\"met1\", x0=0., y0=0., x1=10., y1=10.)!; }
             cell top() { text(\"t\", \"text.label\", child.r.x0, 0.); }",
        );
        assert!(
            errors.iter().any(|error| matches!(
                error,
                StaticErrorKind::CellFnFieldAccess { cell, .. } if cell == "child"
            )),
            "{errors:#?}"
        );
    }

    #[test]
    fn a_cell_field_named_x_reports_the_real_reason() {
        // The check hardcoded `["x", "y"]` a second time and reported
        // `RedeclarationOfBuiltin` -- but neither name is in `BUILTINS`, so
        // the message pointed at a builtin that does not exist. `x` and `y`
        // are among the most natural names in a layout language.
        for name in RESERVED_CELL_FIELDS {
            let errors = check_source(&format!("cell top() {{ let {name} = 1.; }}"));
            assert!(
                errors.iter().any(|error| matches!(
                    error,
                    StaticErrorKind::ReservedCellField { name: n } if n == name
                )),
                "{errors:#?}"
            );
            assert!(
                !errors
                    .iter()
                    .any(|error| matches!(error, StaticErrorKind::RedeclarationOfBuiltin)),
                "{errors:#?}"
            );
        }

        // A nested `let` never becomes a cell field, so it stays legal, as do
        // the other binding forms -- `x` is only reserved where it would
        // publish a field.
        for source in [
            "cell top() { let w = if true { let x = 1.; x } else { 2. }; }",
            "fn f(x: Float) -> Float { x + 1. }",
            "cell top(x: Float) { let r = rect(\"met1\", x0=x, y0=0., x1=1., y1=1.)!; }",
            "cell top() { for x in std::range(3) { float(); } }",
        ] {
            let errors = check_source(source);
            assert!(
                !errors
                    .iter()
                    .any(|error| matches!(error, StaticErrorKind::ReservedCellField { .. })),
                "{source}\n{errors:#?}"
            );
        }
    }

    #[test]
    fn a_misspelled_instance_field_names_the_field() {
        // Reported `empty bbox`, carrying the repo's own
        // `// TODO: More descriptive error`. Instances are normally passed as
        // `Any` (cell types cannot be named), so this is where an ordinary
        // misspelling lands.
        assert_reports(
            &run_source(
                "fn left_edge(i: Any) -> Any { i.wdie.x0 }
                 cell child() { let wide = rect(\"met1\", x0=0., y0=0., x1=10., y1=10.); }
                 cell top() {
                     let i = inst(child());
                     eq(i.x, 0.); eq(i.y, 0.);
                     let r = rect(\"met1\", x0=left_edge(i), y0=0., x1=10., y1=10.);
                 }",
            ),
            |error| {
                matches!(error, ExecErrorKind::NoFieldOnInstance { field, cell }
                if field == "wdie" && cell == "child")
            },
        );
    }

    #[test]
    fn reading_the_layer_of_a_construction_rect_reports_one_error() {
        // Used `InvalidRotation` -- a copy-paste, reported as "non-Manhattan
        // rotation" -- and then continued with a fabricated `Value::String("")`
        // that produced a second, unrelated error about the empty-string layer
        // being absent from the technology file.
        let errors = run_source(
            "cell top() {
                 let c = crect(x0=0., y0=0., x1=10., y1=10.);
                 let r = rect(c.layer, x0=0., y0=0., x1=10., y1=10.)!;
             }",
        );
        assert_reports(
            &errors,
            |error| matches!(error, ExecErrorKind::EmptyField { field } if field == "layer"),
        );
        assert!(
            !errors
                .iter()
                .any(|error| matches!(error, ExecErrorKind::IllegalLayer { .. })),
            "the fabricated empty layer must not cascade: {errors:#?}"
        );
        assert!(
            !errors
                .iter()
                .any(|error| matches!(error, ExecErrorKind::InvalidRotation)),
            "{errors:#?}"
        );
    }

    #[test]
    fn path_extensions_are_constrainable() {
        // Extensions became solver variables only when a kwarg named them,
        // unlike `width`, which always does. `eq(p.begin_extension, 5.)`
        // therefore degenerated to `0 - 5 = 0` and reported a bare
        // "inconsistent constraint" with nothing to say the extension had
        // never become a variable.
        let data = compile_top(
            "cell top() {
                 let p = path(\"met1\", 2, width=1., x0=0., y0=0., x1=10., y1=0.);
                 eq(p.begin_extension, 5.);
             }",
        );
        let path = layout_objects(&data, data.top)
            .into_iter()
            .find_map(SolvedValue::get_path)
            .expect("top should contain a path");
        assert_eq!(path.begin_extension.0, 5.);
        // The unnamed extension still defaults to zero, and because that
        // default is the compiler's rather than the author's it must not make
        // the cell underconstrained.
        assert_eq!(path.end_extension.0, 0.);
        assert!(
            data.cells[&data.top].unsolved_vars.is_empty(),
            "{:#?}",
            data.cells[&data.top].unsolved_vars
        );
    }

    #[test]
    fn a_value_waiting_on_a_defaulted_extension_is_still_evaluated() {
        // Applying the compiler defaults and `continue`ing skipped
        // `update_var_dependents`, so a value blocked on an extension the
        // default had just solved was never re-queued. The solver was then
        // fully solved with `deferred` empty, the loop exited, and `emit`
        // panicked on the still-deferred value -- an internal compiler error
        // on a program that compiled before extensions became variables.
        let data = compile_top(
            "cell top() {
                 let p = path(\"met1\", 2, width=1., x0=0., y0=0., x1=10., y1=0.);
                 let z = 1. / (p.begin_extension + 1.);
                 let r = rect(\"met1\", x0=0., y0=0., x1=z, y1=1.)!;
             }",
        );
        let rect = layout_objects(&data, data.top)
            .into_iter()
            .find_map(SolvedValue::get_rect)
            .expect("top should contain a rect");
        assert_eq!(rect.x1.0, 1.);
    }

    #[test]
    fn any_typed_arguments_report_a_type_error_rather_than_panicking() {
        // `Ty::Any` satisfies every static check, so each builtin has to test
        // the runtime type itself.
        for source in [
            "fn mk() -> Any { 3 }
             cell top() { text(mk(), \"met1.label\", 0., 0.); }",
            "fn e(a: Any, b: Any) { eq(a, b); }
             cell top() { e(1, 2); }",
            "fn mk() -> Any { 3 }
             cell top() { let r = rect(\"met1\", x0=0., y0=0., x1=mk(), y1=1.)!; }",
            "fn f(c: Any) -> Any { inst(c) }
             cell top() { let i = f(5.); }",
            "fn f(c: Any) -> Any { inst(c) }
             cell top() { let i = f(5.); let v = i.foo; }",
        ] {
            assert_reports(&run_source(source), |error| {
                matches!(error, ExecErrorKind::InvalidType)
            });
        }
    }

    #[test]
    fn flat_operator_chains_are_depth_limited() {
        // The parser's depth guard only covered *nested* input. A flat chain is
        // folded iteratively, so it kept `self.depth` at 1 while building an
        // arbitrarily deep tree that the post-parse AST walks then recursed
        // over -- an uncatchable stack overflow at around 900 terms.
        for source in [
            format!("cell top() {{ let v = {}; }}", vec!["1"; 2000].join("+")),
            format!("cell top() {{ let v = {}; }}", vec!["1"; 2000].join("*")),
            format!(
                "cell top() {{ let a = rect(\"met1\"); let v = a{}; }}",
                ".f".repeat(2000)
            ),
        ] {
            let (_dir, output) = scratch_workspace("chain", &source);
            assert!(
                output
                    .static_errors()
                    .iter()
                    .any(|error| error.kind.to_string().contains("nesting too deep")),
                "{:#?}",
                output.static_errors()
            );
        }
    }

    #[test]
    fn bbox_excludes_construction_geometry() {
        // `bbox` used to include construction geometry that the GDS exporter
        // drops, so a placement computed from it did not match the output.
        let source = "cell bot() {
                          let r = rect(\"met1\", x0=0., y0=0., x1=10., y1=10.)!;
                          let c = crect(layer=\"met2\", x0=-500., y0=-500., x1=500., y1=500.);
                      }
                      cell top() {
                          let b = bot();
                          let bb = bbox(b);
                          let m = rect(\"met3\", x0=bb.x0, y0=bb.y0, x1=bb.x1, y1=bb.y1)!;
                      }";
        let (_dir, output) = scratch_workspace("bbox", source);
        assert!(output.static_errors().is_empty());
        let compiled = compile(
            &output.ast(),
            CompileInput {
                cell: &["top"],
                args: Vec::new(),
            },
        );
        let data = compiled.unwrap_valid();
        let top = &data.cells[&data.top];
        let drawn = top
            .objects
            .values()
            .find_map(|object| match object {
                SolvedValue::Rect(rect) if !rect.construction => Some(rect),
                _ => None,
            })
            .expect("top emits one drawn rectangle");
        assert_relative_eq!(drawn.x0.0, 0., epsilon = EPSILON);
        assert_relative_eq!(drawn.y0.0, 0., epsilon = EPSILON);
        assert_relative_eq!(drawn.x1.0, 10., epsilon = EPSILON);
        assert_relative_eq!(drawn.y1.0, 10., epsilon = EPSILON);
    }

    /// Every layout object in `cell`, in the order the exporter would walk it.
    fn layout_objects(data: &CompiledData, cell: CellId) -> Vec<&SolvedValue> {
        data.cells[&cell]
            .objects
            .values()
            .filter(|object| object.is_layout())
            .collect()
    }

    fn compile_top(source: &str) -> CompiledData {
        let (_dir, output) = scratch_workspace("layout", source);
        assert!(
            output.static_errors().is_empty(),
            "{:#?}",
            output.static_errors()
        );
        compile(
            &output.ast(),
            CompileInput {
                cell: &["top"],
                args: Vec::new(),
            },
        )
        .unwrap_valid()
    }

    #[test]
    fn reading_a_coordinate_through_an_instance_does_not_duplicate_geometry() {
        // `inst.member` builds a transformed proxy of the child's shape in the
        // parent so the coordinate has something to name. The proxy landed in
        // the parent's object map but never in its emission list, so the GUI
        // (which walks emissions) looked right while the exporter (which walks
        // objects) drew a phantom shape on top of every SREF -- once per
        // field-access expression, so the multiplicity grew with coding style
        // rather than with the design.
        let data = compile_top(
            "cell bot() {
                 let m = rect(\"met1\", x0=0., y0=0., x1=100., y1=100.);
                 let p = polygon(\"met2\", 3, x0=0., y0=0., x1=10., y1=0., x2=5., y2=10.);
                 let q = path(\"met3\", 2, width=10., begin_extension=0., end_extension=0.,
                              x0=0., y0=0., x1=50., y1=0.);
             }
             cell top() {
                 let i = inst(bot(), x=0., y=0.);
                 let r0 = rect(\"met4\", x0=i.m.x0, y0=200., x1=i.m.x1, y1=250.);
                 let r1 = rect(\"met4\", x0=i.p.points[0].x, y0=300., x1=i.p.points[1].x, y1=350.);
                 let r2 = rect(\"met4\", x0=i.q.points[0].x, y0=400., x1=i.q.points[1].x, y1=450.);
             }",
        );
        let objects = layout_objects(&data, data.top);
        let rects = objects
            .iter()
            .filter(|object| matches!(object, SolvedValue::Rect(_)))
            .count();
        let instances = objects
            .iter()
            .filter(|object| matches!(object, SolvedValue::Instance(_)))
            .count();
        assert_eq!(rects, 3, "only the three declared rects are layout");
        assert_eq!(instances, 1);
        assert_eq!(
            objects.len(),
            4,
            "no proxy polygon or path is drawn in the parent: {objects:#?}"
        );
    }

    #[test]
    fn a_construction_instance_contributes_no_geometry_through_a_projection() {
        // The strictly worse variant: the instance itself is suppressed, so a
        // proxy of its geometry appeared in the parent with no corresponding
        // struct anywhere in the file.
        let data = compile_top(
            "cell bot() { let m = rect(\"met1\", x0=0., y0=0., x1=100., y1=50.); }
             cell top() {
                 let i = inst(bot(), x=1000., y=0., construction=true);
                 let r = rect(\"met2\", x0=i.m.x0, y0=0., x1=i.m.x1, y1=1.);
             }",
        );
        assert_eq!(layout_objects(&data, data.top).len(), 1);
    }

    #[test]
    fn emitting_a_projection_flattens_that_one_shape_into_the_parent() {
        // `!` on a projection is an explicit request to draw the child's shape
        // in the parent as well, so it opts back out of construction. The
        // second case goes through a function that *selects* a proxy built
        // earlier rather than building one, which is why the decision is made
        // from the resolved emission list rather than where the proxy is
        // constructed.
        let data = compile_top(
            "fn second(lst: [Any]) -> Any { head(tail(lst)) }
             cell bot() {
                 let m = rect(\"met1\", x0=0., y0=0., x1=100., y1=100.);
                 let n = rect(\"met1\", x0=200., y0=0., x1=300., y1=100.);
                 let both = list(m, n);
             }
             cell top() {
                 let i = inst(bot(), x=1000., y=0.);
                 i.m!;
                 second(i.both)!;
                 let unused = i.n.x0;
             }",
        );
        let drawn = layout_objects(&data, data.top)
            .iter()
            .filter_map(|object| match object {
                SolvedValue::Rect(rect) => Some(rect.x0.0),
                _ => None,
            })
            .collect::<Vec<_>>();
        // `i.m!` and `second(i.both)!` are drawn; the bare `i.n.x0` read is not.
        assert_eq!(drawn.len(), 2, "{drawn:?}");
        assert!(drawn.contains(&1000.), "{drawn:?}");
        assert!(drawn.contains(&1200.), "{drawn:?}");
    }

    #[test]
    fn out_of_range_coordinates_are_rejected_rather_than_saturated() {
        // `f64 as i32` saturates, so an unchecked coordinate became
        // `2147483647` in the GDS -- collapsing both edges of this rect onto
        // one point -- while the run still reported success.
        let errors = run_source(
            "cell top() { let a = rect(\"met1\", x0=3000000000., y0=0., x1=4000000000., y1=10.); }",
        );
        assert_reports(&errors, |error| {
            matches!(error, ExecErrorKind::CoordinateOutOfRange { .. })
        });
    }

    #[test]
    fn text_coordinates_are_checked_against_the_grid() {
        // A text position can be built entirely from constants, so it is the
        // one coordinate that never becomes a solver variable -- and the
        // solver's grid check only looks at variables. The label used to be
        // snapped silently on export.
        let errors = run_source(
            "cell top() {
                 let a = rect(\"met1\", x0=0., y0=0., x1=10., y1=10.);
                 text(\"t\", \"met1\", 0.04, 0.06);
             }",
        );
        assert_reports(&errors, |error| {
            matches!(error, ExecErrorKind::OffGrid { .. })
        });
        assert!(
            run_source(
                "cell top() {
                     let a = rect(\"met1\", x0=0., y0=0., x1=10., y1=10.);
                     text(\"t\", \"met1\", 0.1, 5.);
                 }"
            )
            .is_empty(),
            "an on-grid label still compiles"
        );
    }

    #[test]
    fn off_grid_diagnostics_carry_the_value_and_where_it_snapped_to() {
        // Rounding each variable in isolation can break a constraint coupling
        // them; the bare "invalid rounding" text gave an author no way to tell
        // floating-point noise from an unrepresentable layout.
        let errors = run_source(
            "cell top() {
                 let a = rect(\"met1\", y0=0., y1=10., x0=0.);
                 let b = rect(\"met2\", y0=0., y1=10., x0=0.);
                 eq(a.x1 + b.x1, 1.);
                 eq(a.x1 - b.x1, 0.9);
             }",
        );
        let reported = errors
            .iter()
            .find_map(|error| match error {
                ExecErrorKind::OffGrid {
                    value,
                    snapped,
                    grid,
                } => Some((*value, *snapped, *grid)),
                _ => None,
            })
            .expect("the coupled solution rounds off grid");
        let (value, snapped, grid) = reported;
        assert_relative_eq!(grid, 0.1, epsilon = EPSILON);
        assert_relative_eq!(value, 0.05, epsilon = 1e-9);
        assert_relative_eq!(snapped, 0., epsilon = EPSILON);
    }

    #[test]
    fn negative_path_dimensions_are_rejected() {
        // The width was silently absolutized and the extensions passed
        // straight through as a negative BGNEXTN/ENDEXTN.
        let errors = run_source(
            "cell top() {
                 let p = path(\"met1\", 2, width=-10., begin_extension=0., end_extension=0.,
                              x0=0., y0=0., x1=100., y1=0.);
             }",
        );
        assert_reports(&errors, |error| {
            matches!(error, ExecErrorKind::NegativePathWidth(_))
        });
        let errors = run_source(
            "cell top() {
                 let p = path(\"met1\", 2, width=10., begin_extension=-5., end_extension=-5.,
                              x0=0., y0=0., x1=100., y1=0.);
             }",
        );
        assert_eq!(
            errors
                .iter()
                .filter(|error| matches!(error, ExecErrorKind::NegativePathExtension { .. }))
                .count(),
            2,
            "both ends are reported: {errors:#?}"
        );
    }

    #[test]
    fn text_outside_the_gds_character_set_or_length_is_rejected() {
        // A GDS STRING record is a byte string with no encoding negotiation,
        // and its length limit is counted in bytes, not characters.
        let errors = run_source(
            "cell top() {
                 let a = rect(\"met1\", x0=0., y0=0., x1=10., y1=10.);
                 text(\"héllo\", \"met1\", 0., 5.);
             }",
        );
        assert_reports(&errors, |error| {
            matches!(error, ExecErrorKind::NonAsciiText { character: 'é' })
        });
        let long = "a".repeat(MAX_TEXT_LEN + 1);
        let errors = run_source(&format!(
            "cell top() {{
                 let a = rect(\"met1\", x0=0., y0=0., x1=10., y1=10.);
                 text(\"{long}\", \"met1\", 0., 5.);
             }}"
        ));
        assert_reports(&errors, |error| {
            matches!(error, ExecErrorKind::TextTooLong { .. })
        });
    }

    #[test]
    fn a_descending_range_counts_down_and_a_zero_step_is_rejected() {
        // `range_full`'s loop was guarded by `if step > 0` with no `else`, so
        // the natural descending range silently produced an empty sequence.
        let data = compile_top(
            "cell top() {
                 for i in range_full(3, 0, -1) {
                     rect(\"met1\", x0=(i as Float) * 10., y0=0., x1=(i as Float) * 10. + 5., y1=5.);
                 }
             }",
        );
        let mut lefts = layout_objects(&data, data.top)
            .iter()
            .filter_map(|object| match object {
                SolvedValue::Rect(rect) => Some(rect.x0.0),
                _ => None,
            })
            .collect::<Vec<_>>();
        lefts.sort_by(f64::total_cmp);
        assert_eq!(lefts, vec![10., 20., 30.]);

        assert_reports(
            &run_source("cell top() { for i in range_full(0, 3, 0) { float(); } }"),
            |error| matches!(error, ExecErrorKind::ZeroRangeStep),
        );
    }

    /// Compiles `source`, keeping the layout even when the cell is
    /// underconstrained -- which an `x0i=` initial condition makes it by
    /// design.
    fn compile_top_underconstrained(source: &str) -> CompiledData {
        let (_dir, output) = scratch_workspace("layout", source);
        assert!(
            output.static_errors().is_empty(),
            "{:#?}",
            output.static_errors()
        );
        match compile(
            &output.ast(),
            CompileInput {
                cell: &["top"],
                args: Vec::new(),
            },
        ) {
            CompileOutput::Valid(output) => output,
            CompileOutput::ExecErrors(output) => {
                assert!(
                    output
                        .errors
                        .iter()
                        .all(|error| matches!(error.kind, ExecErrorKind::Underconstrained)),
                    "{:#?}",
                    output.errors
                );
                output.output.expect("underconstrained cells still emit")
            }
            output => panic!("should compile: {output:?}"),
        }
    }

    #[test]
    fn extension_defaults_do_not_over_constrain_a_satisfiable_system() {
        // Every applicable default was applied in one batch, each tested
        // against solver state that had not yet seen the others. These two
        // paths have four extension variables, rank-2 author constraints and
        // so two degrees of freedom; applying all four zeroes contradicted
        // `... = 40.` and reported `inconsistent constraint` on a system that
        // is satisfiable at 20 per path.
        let data = compile_top(
            "cell top() {
                 let p = path(\"met1\", 2, width=1., x0=0., y0=0., x1=10., y1=0.);
                 let q = path(\"met1\", 2, width=1., x0=0., y0=5., x1=10., y1=5.);
                 eq(p.begin_extension + p.end_extension
                    + q.begin_extension + q.end_extension, 40.);
                 eq(p.begin_extension + p.end_extension,
                    q.begin_extension + q.end_extension);
             }",
        );
        let sums = layout_objects(&data, data.top)
            .iter()
            .filter_map(|object| match object {
                SolvedValue::Path(path) => Some(path.begin_extension.0 + path.end_extension.0),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(sums, vec![20., 20.]);
    }

    #[test]
    fn an_initial_condition_outranks_a_compiler_default() {
        // Defaults drained at the first stall, before the fallback heap was
        // popped, so the compiler's zero for `p.begin_extension` propagated
        // through `eq` and determined `r.x0`. The author's `x0i=5.` was then
        // skipped as moot: the rect emitted at 0, and because the fallback
        // never fired it never reached `fallback_constraints_used`, which is
        // what the GUI writes a drag back to source through.
        let data = compile_top_underconstrained(
            "cell top() {
                 let p = path(\"met1\", 2, width=1., x0=0., y0=0., x1=10., y1=0.);
                 let r = rect(\"met1\", x0i=5., y0=0., x1=10., y1=10.);
                 eq(p.begin_extension, r.x0);
             }",
        );
        let x0 = layout_objects(&data, data.top)
            .iter()
            .find_map(|object| match object {
                SolvedValue::Rect(rect) => Some(rect.x0.0),
                _ => None,
            })
            .expect("compiled rect");
        assert_eq!(x0, 5.);
        assert!(
            !data.cells[&data.top].fallback_constraints_used.is_empty(),
            "the initial condition must be recorded for the GUI write-back"
        );
    }
    #[test]
    fn an_execution_error_does_not_suppress_the_rest_of_the_cell() {
        // Reporting and returning `Err(())` unwound out of `execute_cell`, so
        // one bad field read hid every other diagnostic in the cell and left
        // `output` as `None` -- which the analyzer reads as "the cell would
        // not open", blanking the GUI canvas over a single typo.
        let (_dir, output) = scratch_workspace(
            "run",
            "cell top() {
                 let c = crect(x0=0., y0=0., x1=10., y1=10.);
                 let r = rect(c.layer, x0=0., y0=0., x1=10., y1=10.)!;
                 let bad = rect(\"no_such_layer\", x0=0., y0=0., x1=1., y1=1.)!;
                 let free = rect(\"met1\", x0=0., y0=0., y1=1.)!;
             }",
        );
        let CompileOutput::ExecErrors(output) = compile(
            &output.ast(),
            CompileInput {
                cell: &["top"],
                args: Vec::new(),
            },
        ) else {
            panic!("the cell has errors to report");
        };
        assert!(
            output.output.is_some(),
            "the layout must survive so the GUI still draws it"
        );
        let kinds = output
            .errors
            .iter()
            .map(|error| error.kind.clone())
            .collect::<Vec<_>>();
        assert_reports(
            &kinds,
            |kind| matches!(kind, ExecErrorKind::EmptyField { field } if field == "layer"),
        );
        assert_reports(&kinds, |kind| {
            matches!(kind, ExecErrorKind::Underconstrained)
        });
        assert_reports(
            &kinds,
            |kind| matches!(kind, ExecErrorKind::IllegalLayer { layer, .. } if layer == "no_such_layer"),
        );
    }

    #[test]
    fn a_poisoned_value_reports_once_however_often_it_is_read() {
        // Poison propagates silently: the three rects below each read a value
        // whose diagnostic was already recorded, and a second error from any
        // of them would be noise about a mistake the author has already been
        // told about.
        let errors = run_source(
            "cell top() {
                 let c = crect(x0=0., y0=0., x1=10., y1=10.);
                 let l = c.layer;
                 let r1 = rect(l, x0=0., y0=0., x1=1., y1=1.)!;
                 let r2 = rect(l, x0=2., y0=0., x1=3., y1=1.)!;
                 let r3 = rect(l, x0=4., y0=0., x1=5., y1=1.)!;
             }",
        );
        assert_eq!(errors.len(), 1, "one mistake, one diagnostic: {errors:#?}");
        assert!(
            matches!(&errors[0], ExecErrorKind::EmptyField { field } if field == "layer"),
            "{errors:#?}"
        );
    }

    // -----------------------------------------------------------------------
    // Boolean operators: `&&`, `||`, and `!`.

    #[test]
    fn boolean_operators_check_and_evaluate() {
        assert!(check_source("cell top() { let a = !true && (false || true); }").is_empty());

        // Every condition below must hold. If one does not, the `else` branch
        // adds a second constraint on `x` that contradicts the first, and the
        // solve fails -- so a wrong truth value cannot pass silently.
        for cond in [
            "true && true",
            "!(true && false)",
            "!(false && true)",
            "!(false && false)",
            "true || true",
            "true || false",
            "false || true",
            "!(false || false)",
            "!false",
            "!!true",
            "true == true",
            "true != false",
            "!(true == false)",
            "p > 2. && p < 4.",
            "p < 2. || p > 2.5",
        ] {
            let source = format!(
                "cell top() {{
                     let p = float();
                     eq(p, 3.);
                     let q = float();
                     eq(q, 1.);
                     if {cond} {{ }} else {{ eq(q, 2.); }};
                 }}"
            );
            assert!(
                run_source(&source).is_empty(),
                "`{cond}` should evaluate to true"
            );
            // The negation must fail, or the check above proves nothing.
            let negated = source.replace(&format!("if {cond}"), &format!("if !({cond})"));
            assert!(
                !run_source(&negated).is_empty(),
                "`!({cond})` should evaluate to false"
            );
        }
    }

    #[test]
    fn boolean_operators_short_circuit() {
        // The right operand is an arbitrary expression that may divide by zero,
        // create constraints, or emit geometry, so it must not be evaluated
        // when the left operand already decides the result -- the same reason
        // `if` defers its branches.
        assert!(run_source("cell top() { let a = false && (1 / 0 == 1); }").is_empty());
        assert!(run_source("cell top() { let a = true || (1 / 0 == 1); }").is_empty());
        assert_reports(
            &run_source("cell top() { let a = true && (1 / 0 == 1); }"),
            |error| matches!(error, ExecErrorKind::DivideByZero(_)),
        );
        assert_reports(
            &run_source("cell top() { let a = false || (1 / 0 == 1); }"),
            |error| matches!(error, ExecErrorKind::DivideByZero(_)),
        );

        // A short-circuited operand emits no geometry either.
        let emits = "(rect(\"met1\", x0=0., y0=0., x1=1., y1=1.)!.x1 > 0.)";
        for (cond, drawn) in [
            (format!("false && {emits}"), 0),
            (format!("true && {emits}"), 1),
            (format!("true || {emits}"), 0),
            (format!("false || {emits}"), 1),
        ] {
            let data = compile_top(&format!("cell top() {{ let a = {cond}; }}"));
            assert_eq!(
                layout_objects(&data, data.top).len(),
                drawn,
                "`{cond}` should emit {drawn} shape(s)"
            );
        }
    }

    #[test]
    fn boolean_operators_reject_non_bool_operands() {
        // `!` used to parse and then report `unimplemented`; `&&` and `||` did
        // not lex at all.
        for source in [
            "cell top() { let a = 1 && true; }",
            "cell top() { let a = true && 1; }",
            "cell top() { let a = true || 2.; }",
            "cell top() { let a = !1; }",
            "cell top() { let a = !\"s\"; }",
        ] {
            let errors = check_source(source);
            assert!(
                errors
                    .iter()
                    .any(|error| matches!(error, StaticErrorKind::BoolOpInvalidType)),
                "{source}: {errors:#?}"
            );
        }
    }

    #[test]
    fn booleans_compare_for_equality_only() {
        // `Ty::Bool` was missing from the comparison whitelist, so even
        // `true == false` was rejected.
        assert!(check_source("cell top() { let a = true == false; }").is_empty());
        assert!(check_source("cell top() { let a = true != false; }").is_empty());
        for source in [
            "cell top() { let a = true < false; }",
            "cell top() { let a = true >= false; }",
        ] {
            let errors = check_source(source);
            assert!(
                errors
                    .iter()
                    .any(|error| matches!(error, StaticErrorKind::BoolNotOrd)),
                "{source}: {errors:#?}"
            );
        }
    }
}
pub mod cli;
