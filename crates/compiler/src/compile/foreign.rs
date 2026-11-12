//! Calls to `extern` functions: arguments are encoded as CBOR for the WASM
//! module and its result is decoded against the declared return type.

use ciborium::Value as Cbor;

use super::ops::{OpFailure, OpResult};
use super::*;
use crate::solver::Solver;
use crate::wasm::CallError;

/// Calls the `extern` function `name` of the module at `path`.
pub(super) fn call(
    solver: &Solver,
    defs: &TypeDefs,
    path: &std::path::Path,
    name: &str,
    args: &[&Value],
    ret: &Ty,
) -> OpResult {
    let mut unsolved = SmallVec::new();
    let encoded = args
        .iter()
        .enumerate()
        .map(|(position, arg)| {
            encode(
                solver,
                arg,
                &mut unsolved,
                &format!("argument {}", position + 1),
            )
        })
        .collect::<Result<Vec<_>, _>>()
        .map_err(|message| failure(name, message))?;
    if !unsolved.is_empty() {
        return Err(OpFailure::Wait(unsolved));
    }
    let mut input = Vec::new();
    ciborium::into_writer(&Cbor::Array(encoded), &mut input)
        .map_err(|error| failure(name, error.to_string()))?;
    let output = crate::wasm::call(path, name, &input).map_err(|error| match error {
        CallError::Cancelled => OpFailure::Error(
            ExecErrorKind::ExternCallCancelled {
                function: name.to_owned(),
            },
            ops::ErrorAt::Expr,
        ),
        CallError::Failed(message) => failure(name, message),
    })?;
    let result: Cbor = ciborium::from_reader(output.as_slice())
        .map_err(|error| failure(name, format!("invalid result encoding: {error}")))?;
    let value = match result {
        Cbor::Map(mut entries) if entries.len() == 1 => {
            let (tag, value) = entries.pop().unwrap();
            match tag.as_text() {
                Some("Ok") => value,
                Some("Err") => {
                    let message = match value {
                        Cbor::Text(message) => message,
                        other => format!("{other:?}"),
                    };
                    return Err(failure(name, message));
                }
                _ => return Err(failure(name, "result is not `Ok` or `Err`".to_owned())),
            }
        }
        _ => return Err(failure(name, "result is not `Ok` or `Err`".to_owned())),
    };
    decode(defs, ret, value, "result").map_err(|message| failure(name, message))
}

fn failure(name: &str, message: String) -> OpFailure {
    OpFailure::Error(
        ExecErrorKind::ExternCall {
            function: name.to_owned(),
            message,
        },
        ops::ErrorAt::Expr,
    )
}

/// Encodes `value`, collecting the solver variables it still depends on.
fn encode(
    solver: &Solver,
    value: &Value,
    unsolved: &mut SmallVec<[Var; 4]>,
    at: &str,
) -> Result<Cbor, String> {
    Ok(match value {
        Value::Nil => Cbor::Null,
        Value::Bool(value) => Cbor::Bool(*value),
        Value::Int(value) => Cbor::Integer((*value).into()),
        Value::String(value) => Cbor::Text(value.clone()),
        Value::Linear(expr) => match solver.eval_expr_exact(expr) {
            Some(value) => Cbor::Float(value),
            None => {
                unsolved.extend(
                    expr.coeffs
                        .iter()
                        .map(|(_, var)| *var)
                        .filter(|var| !solver.is_solved(*var)),
                );
                Cbor::Null
            }
        },
        Value::Seq(items) => Cbor::Array(
            items
                .iter()
                .enumerate()
                .map(|(index, item)| encode(solver, item, unsolved, &format!("{at}[{index}]")))
                .collect::<Result<_, _>>()?,
        ),
        Value::Tuple(items) => Cbor::Array(
            items
                .iter()
                .enumerate()
                .map(|(index, item)| encode(solver, item, unsolved, &format!("{at}.{index}")))
                .collect::<Result<_, _>>()?,
        ),
        Value::Struct(value) => Cbor::Map(
            value
                .fields
                .iter()
                .map(|(field, item)| {
                    Ok((
                        Cbor::Text(field.clone()),
                        encode(solver, item, unsolved, &format!("{at}.{field}"))?,
                    ))
                })
                .collect::<Result<_, String>>()?,
        ),
        other => {
            return Err(format!(
                "{at} is a {}, which cannot be passed to a WASM function",
                value_kind(other)
            ));
        }
    })
}

/// Decodes `value` as a value of type `ty`.
fn decode(defs: &TypeDefs, ty: &Ty, value: Cbor, at: &str) -> Result<Value, String> {
    let mismatch =
        |value: &Cbor| format!("{at} should be {}, found {}", ty_name(ty), cbor_kind(value));
    Ok(match (ty, value) {
        (Ty::Nil, Cbor::Null) => Value::Nil,
        (Ty::Bool, Cbor::Bool(value)) => Value::Bool(value),
        (Ty::Int, Cbor::Integer(value)) => {
            Value::Int(i64::try_from(value).map_err(|_| format!("{at} does not fit in an `Int`"))?)
        }
        (Ty::Float, Cbor::Float(value)) => float(value, at)?,
        (Ty::Float, Cbor::Integer(value)) => float(i128::from(value) as f64, at)?,
        (Ty::String, Cbor::Text(value)) => Value::String(value),
        (Ty::Seq(item), Cbor::Array(items)) => seq_value(
            items
                .into_iter()
                .enumerate()
                .map(|(index, value)| decode(defs, item, value, &format!("{at}[{index}]")))
                .collect::<Result<Seq, _>>()?,
        ),
        (Ty::Tuple(tys), Cbor::Array(items)) => {
            if items.len() != tys.len() {
                return Err(format!(
                    "{at} should have {} elements, found {}",
                    tys.len(),
                    items.len()
                ));
            }
            Value::Tuple(
                tys.iter()
                    .zip(items)
                    .enumerate()
                    .map(|(index, (ty, value))| decode(defs, ty, value, &format!("{at}.{index}")))
                    .collect::<Result<Arc<[Value]>, _>>()?,
            )
        }
        (Ty::Struct(struct_ty), Cbor::Map(entries)) => {
            let Some(AdtDef::Struct(def)) = defs.get(&struct_ty.def) else {
                return Err(format!("{at}: unknown struct `{}`", struct_ty.name));
            };
            if !def.params.is_empty() {
                return Err(format!(
                    "{at}: generic struct `{}` cannot be returned by a WASM function",
                    struct_ty.name
                ));
            }
            let mut entries = entries
                .into_iter()
                .map(|(key, value)| match key {
                    Cbor::Text(key) => Ok((key, value)),
                    other => Err(format!("{at} has a non-string key {}", cbor_kind(&other))),
                })
                .collect::<Result<IndexMap<String, Cbor>, _>>()?;
            let fields = def
                .fields
                .iter()
                .map(|(field, ty)| {
                    let value = entries
                        .shift_remove(field)
                        .ok_or_else(|| format!("{at} is missing field `{field}`"))?;
                    Ok((
                        field.clone(),
                        decode(defs, ty, value, &format!("{at}.{field}"))?,
                    ))
                })
                .collect::<Result<FxIndexMap<_, _>, String>>()?;
            if let Some(field) = entries.keys().next() {
                return Err(format!(
                    "{at} has field `{field}`, which struct `{}` does not declare",
                    struct_ty.name
                ));
            }
            Value::Struct(Arc::new(StructValue {
                name: struct_ty.name.clone(),
                fields,
            }))
        }
        (
            Ty::Nil
            | Ty::Bool
            | Ty::Int
            | Ty::Float
            | Ty::String
            | Ty::Seq(_)
            | Ty::Tuple(_)
            | Ty::Struct(_),
            value,
        ) => return Err(mismatch(&value)),
        (ty, _) => {
            return Err(format!(
                "{at} has type {}, which a WASM function cannot return",
                ty_name(ty)
            ));
        }
    })
}

fn float(value: f64, at: &str) -> Result<Value, String> {
    if !value.is_finite() {
        return Err(format!("{at} is not a finite number"));
    }
    Ok(Value::Linear(LinearExpr::from(value)))
}

fn ty_name(ty: &Ty) -> String {
    match ty {
        Ty::Nil => "`()`".to_owned(),
        Ty::Bool => "a `Bool`".to_owned(),
        Ty::Int => "an `Int`".to_owned(),
        Ty::Float => "a `Float`".to_owned(),
        Ty::String => "a `String`".to_owned(),
        Ty::Seq(_) => "a sequence".to_owned(),
        Ty::Tuple(_) => "a tuple".to_owned(),
        Ty::Struct(ty) => format!("struct `{}`", ty.name),
        other => format!("`{other:?}`"),
    }
}

fn cbor_kind(value: &Cbor) -> &'static str {
    match value {
        Cbor::Integer(_) => "an integer",
        Cbor::Bytes(_) => "bytes",
        Cbor::Float(_) => "a float",
        Cbor::Text(_) => "a string",
        Cbor::Bool(_) => "a bool",
        Cbor::Null => "null",
        Cbor::Tag(..) => "a tagged value",
        Cbor::Array(_) => "an array",
        Cbor::Map(_) => "a map",
        _ => "an unknown value",
    }
}

fn value_kind(value: &Value) -> &'static str {
    match value {
        Value::Enum(_) | Value::Ctor(_) => "enum value",
        Value::Rect(_) => "rect",
        Value::Polygon(_) => "polygon",
        Value::Path(_) => "path",
        Value::Point(_) => "point",
        Value::Fn(_) | Value::CellFn(_) => "function",
        Value::Cell(_) => "cell",
        Value::Inst(_) => "instance",
        _ => "value",
    }
}

#[cfg(all(test, not(target_family = "wasm")))]
mod tests {
    use std::{
        path::{Path, PathBuf},
        sync::{Arc, Mutex},
        time::Duration,
    };

    use crate::compile::{CompileInput, CompileOutput, SolvedValue, compile};
    use crate::incremental::IncrementalCompiler;
    use crate::parse::{parse_source_text, with_std};
    use crate::wasm::CallControl;
    use crate::workspace::WorkspaceConfig;

    /// A module whose `echo` functions return their argument array, so a call
    /// round-trips every argument through both encodings.
    const FIXTURE: &str = r#"
(module
  (memory (export "memory") 1)
  (global $heap (mut i32) (i32.const 4096))
  (data (i32.const 0) "\a1\62Ok")
  (data (i32.const 16) "\a1\63Err\64nope")
  (data (i32.const 32) "boom")
  (func (export "argon_abi_version") (result i32) (i32.const 0))
  (func (export "argon_alloc") (param $len i32) (result i32)
    (local $ptr i32)
    (local.set $ptr (global.get $heap))
    (global.set $heap (i32.add (global.get $heap) (local.get $len)))
    (local.get $ptr))
  (func (export "argon_panic_message") (result i64)
    (i64.or (i64.shl (i64.const 32) (i64.const 32)) (i64.const 4)))
  (func $echo (param $ptr i32) (param $len i32) (result i64)
    (memory.copy (i32.const 1024) (i32.const 0) (i32.const 4))
    (memory.copy (i32.const 1028) (local.get $ptr) (local.get $len))
    (i64.or
      (i64.shl (i64.const 1024) (i64.const 32))
      (i64.extend_i32_u (i32.add (local.get $len) (i32.const 4)))))
  (export "argon_fn_echo" (func $echo))
  (export "argon_fn_echo_float" (func $echo))
  (export "argon_fn_echo_as_string" (func $echo))
  (func (export "argon_fn_fail") (param i32 i32) (result i64)
    (i64.or (i64.shl (i64.const 16) (i64.const 32)) (i64.const 10)))
  (func (export "argon_fn_boom") (param i32 i32) (result i64) (unreachable))
  (func (export "argon_fn_spin") (param i32 i32) (result i64)
    (loop $again (br $again))
    (i64.const 0)))
"#;

    /// A configuration for a library in `dir` with the fixture module.
    fn config(dir: &Path) -> WorkspaceConfig {
        let wasm = dir.join("fixture.wasm");
        std::fs::write(&wasm, wat::parse_str(FIXTURE).unwrap()).unwrap();
        let tech =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../examples/tech/basic.tech.toml");
        WorkspaceConfig::new(dir.join("lib.ar"))
            .with_tech(Some(tech))
            .with_wasm_modules([("fixture".to_owned(), wasm)])
    }

    fn run(source: &str) -> CompileOutput {
        let dir = tempfile::tempdir().unwrap();
        let config = config(dir.path());
        let root = parse_source_text(source, PathBuf::from("/virtual/lib.ar")).unwrap();
        compile(
            &with_std(root),
            CompileInput {
                cell: &["top"],
                args: Vec::new(),
            },
            &config,
        )
    }

    fn rects(output: &CompileOutput) -> Vec<[f64; 4]> {
        let CompileOutput::Valid(data) = output else {
            panic!("unexpected compile output: {output:#?}");
        };
        data.cells[&data.top]
            .objects
            .values()
            .filter_map(|object| match object {
                SolvedValue::Rect(rect) => Some([rect.x0.0, rect.y0.0, rect.x1.0, rect.y1.0]),
                _ => None,
            })
            .collect()
    }

    fn errors(output: &CompileOutput) -> Vec<String> {
        let CompileOutput::ExecErrors(errors) = output else {
            panic!("unexpected compile output: {output:#?}");
        };
        errors
            .errors
            .iter()
            .map(|error| error.kind.to_string())
            .collect()
    }

    #[test]
    fn values_round_trip() {
        let output = run(r#"
struct Size { w: Float, h: Float }
extern "fixture" fn echo(n: Int, f: Float, s: String, b: Bool, items: [Int], size: Size)
    -> (Int, Float, String, Bool, [Int], Size);

cell top() {
    let r = echo(3, 2., "s", true, [1, 2], Size { w: 2., h: 4. });
    if r.3 {
        std::layout::rect("met1", x0=0., y0=0., x1=r.5.w + (r.0 as Float) + (r.4[1] as Float), y1=r.1 + r.5.h)!;
    }
}
"#);
        assert_eq!(rects(&output), [[0., 0., 7., 6.]]);
    }

    #[test]
    fn calls_wait_for_their_arguments_to_be_solved() {
        let output = run(r#"
extern "fixture" fn echo_float(f: Float) -> (Float,);

cell top() {
    let w = float();
    let r = echo_float(w);
    std::layout::rect("met1", x0=0., y0=0., x1=r.0, y1=1.)!;
    eq(w, 4.);
}
"#);
        assert_eq!(rects(&output), [[0., 0., 4., 1.]]);
    }

    #[test]
    fn failures_are_reported_at_the_call() {
        let cases = [
            (
                "fail() -> Int",
                "fail()",
                "extern function `fail` failed: nope",
            ),
            (
                "boom() -> Int",
                "boom()",
                "extern function `boom` failed: panic: boom",
            ),
            (
                "echo_as_string(n: Int) -> (String,)",
                "echo_as_string(1)",
                "result.0 should be a `String`, found an integer",
            ),
        ];
        for (signature, call, expected) in cases {
            let output = run(&format!(
                "extern \"fixture\" fn {signature};\ncell top() {{ let r = {call}; }}\n"
            ));
            let errors = errors(&output);
            assert!(
                errors.iter().any(|error| error.contains(expected)),
                "{call}: expected {expected:?} in {errors:?}"
            );
        }
    }

    #[test]
    fn a_running_call_reports_itself_and_stops_when_cancelled() {
        let slow = Arc::new(Mutex::new(Vec::new()));
        let reported = slow.clone();
        let control = CallControl::new()
            .on_slow_call(move |name| reported.lock().unwrap().push(name.to_owned()));
        let canceller = control.clone();
        let cancel = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(300));
            canceller.cancel();
        });
        let output = control
            .run(|| run("extern \"fixture\" fn spin() -> Int;\ncell top() { let r = spin(); }\n"));
        cancel.join().unwrap();
        assert!(output.was_cancelled());
        assert!(errors(&output).contains(&"extern function `spin` was cancelled".to_owned()));
        assert_eq!(*slow.lock().unwrap(), ["spin"]);
    }

    #[test]
    fn a_cancelled_call_is_not_cached() {
        let dir = tempfile::tempdir().unwrap();
        let config = config(dir.path());
        let source = r#"
extern "fixture" fn echo_float(f: Float) -> (Float,);

cell top() {
    let r = echo_float(3.);
    std::layout::rect("met1", x0=0., y0=0., x1=r.0, y1=1.)!;
}
"#;
        std::fs::write(dir.path().join("lib.ar"), source).unwrap();
        let mut compiler = IncrementalCompiler::new();
        let top = ["top".to_owned()];
        let cancelled = CallControl::new();
        cancelled.cancel();
        let output = cancelled.run(|| compiler.compile_cell(&config, &top, Vec::new()));
        assert!(output.was_cancelled());
        let output = compiler.compile_cell(&config, &top, Vec::new());
        assert_eq!(rects(&output), [[0., 0., 3., 1.]]);
    }

    #[test]
    fn a_module_missing_from_the_manifest_is_reported() {
        let output = run(r#"
extern "missing" fn f() -> Int;
cell top() { let r = f(); }
"#);
        assert!(
            errors(&output)
                .iter()
                .any(|error| error.contains("no WASM module named `missing`"))
        );
    }

    #[test]
    fn unsupported_signature_types_are_reported_at_the_declaration() {
        let cases = [
            ("extern \"fixture\" fn f(r: Rect) -> Int;", "`Rect`"),
            (
                "struct Holder { inner: [Rect] }\nextern \"fixture\" fn f() -> Holder;",
                "`Rect`",
            ),
            (
                "enum Mode { A, B }\nextern \"fixture\" fn f(m: Mode) -> Int;",
                "`Mode`",
            ),
        ];
        for (declaration, ty) in cases {
            let output = run(&format!("{declaration}\ncell top() {{}}\n"));
            let CompileOutput::StaticErrors(errors) = &output else {
                panic!("{declaration}: unexpected compile output: {output:#?}");
            };
            assert!(
                errors.errors.iter().any(|error| {
                    let message = error.kind.to_string();
                    message.starts_with("an extern function cannot take or return")
                        && message.contains(ty)
                }),
                "{declaration}: {:?}",
                errors.errors
            );
        }
    }

    #[test]
    fn extern_declarations_are_checked_when_parsed() {
        for source in [
            "extern fn f() -> Int;",
            "extern \"m\" fn f(a: Int = 1) -> Int;",
            "extern \"m\" fn f<T>(a: T) -> T;",
            "extern \"m\" fn f() -> Int { 1 }",
        ] {
            assert!(
                parse_source_text(source, PathBuf::from("/virtual/lib.ar")).is_err(),
                "{source} should not parse"
            );
        }
    }
}
