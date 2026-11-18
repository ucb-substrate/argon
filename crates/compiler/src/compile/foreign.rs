//! Calls to `extern` functions: arguments are encoded as CBOR for the WASM
//! module and its result is decoded against the declared return type.

use ciborium::Value as Cbor;
use ciborium_io::Read as _;
use ciborium_ll::{Decoder, Encoder, Header, simple};

use super::ops::{OpFailure, OpResult};
use super::*;
use crate::solver::Solver;
use crate::wasm::CallError;

/// Calls the `extern` function `name` of the module at `path`.
pub(super) fn call(
    solver: &Solver,
    defs: &TypeDefs,
    layouts: &StructLayouts,
    path: &std::path::Path,
    name: &str,
    args: &[&Value],
    ret: &Ty,
) -> OpResult {
    let mut unsolved = SmallVec::new();
    let mut input = Vec::new();
    let mut encoder = Encoder::from(&mut input);
    write(&mut encoder, Header::Array(Some(args.len())));
    for (position, arg) in args.iter().enumerate() {
        encode(&mut encoder, solver, arg, &mut unsolved)
            .map_err(|error| failure(name, error.message(&format!("argument {}", position + 1))))?;
    }
    if !unsolved.is_empty() {
        return Err(OpFailure::Wait(unsolved));
    }
    let output = crate::wasm::call(path, name, &input).map_err(|error| match error {
        CallError::Cancelled => OpFailure::Error(
            ExecErrorKind::ExternCallCancelled {
                function: name.to_owned(),
            },
            ops::ErrorAt::Expr,
        ),
        CallError::Failed(message) => failure(name, message),
    })?;
    if let Some(value) = decode_ok(defs, layouts, ret, &output) {
        return Ok(value);
    }
    // Anything the direct decoding does not take, errors included, goes
    // through the general decoding, which reports what is wrong.
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
    decode(defs, layouts, ret, value, "result").map_err(|message| failure(name, message))
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

type CborWriter<'w> = Encoder<&'w mut Vec<u8>>;

fn write(encoder: &mut CborWriter<'_>, header: Header) {
    encoder
        .push(header)
        .expect("writing to a `Vec` does not fail");
}

/// A value that cannot be passed to a WASM function, and where it is within
/// an argument, innermost step first.
struct Unencodable<'v> {
    kind: &'static str,
    path: Vec<Step<'v>>,
}

/// One step into a sequence, tuple, or struct.
enum Step<'v> {
    Index(usize),
    Position(usize),
    Field(&'v str),
}

impl Unencodable<'_> {
    fn message(&self, argument: &str) -> String {
        let mut at = argument.to_owned();
        for step in self.path.iter().rev() {
            match step {
                Step::Index(index) => at += &format!("[{index}]"),
                Step::Position(index) => at += &format!(".{index}"),
                Step::Field(field) => at += &format!(".{field}"),
            }
        }
        format!(
            "{at} is a {}, which cannot be passed to a WASM function",
            self.kind
        )
    }
}

/// Writes `value` as CBOR, collecting the solver variables it still depends
/// on. An unsolved number is written as null.
fn encode<'v>(
    encoder: &mut CborWriter<'_>,
    solver: &Solver,
    value: &'v Value,
    unsolved: &mut SmallVec<[Var; 4]>,
) -> Result<(), Unencodable<'v>> {
    let within = |step: Step<'v>| {
        move |mut error: Unencodable<'v>| {
            error.path.push(step);
            error
        }
    };
    match value {
        Value::Nil => write(encoder, Header::Simple(simple::NULL)),
        Value::Bool(value) => write(
            encoder,
            Header::Simple(if *value { simple::TRUE } else { simple::FALSE }),
        ),
        Value::Int(value) => write(
            encoder,
            match u64::try_from(*value) {
                Ok(value) => Header::Positive(value),
                Err(_) => Header::Negative(*value as u64 ^ !0),
            },
        ),
        Value::String(value) => encoder
            .text(value, None)
            .expect("writing to a `Vec` does not fail"),
        Value::Linear(expr) => match solver.eval_expr_exact(expr) {
            Some(value) => write(encoder, Header::Float(value)),
            None => {
                unsolved.extend(
                    expr.coeffs
                        .iter()
                        .map(|(_, var)| *var)
                        .filter(|var| !solver.is_solved(*var)),
                );
                write(encoder, Header::Simple(simple::NULL));
            }
        },
        Value::Seq(items) => {
            write(encoder, Header::Array(Some(items.len())));
            for (index, item) in items.iter().enumerate() {
                encode(encoder, solver, item, unsolved).map_err(within(Step::Index(index)))?;
            }
        }
        Value::Tuple(items) => {
            write(encoder, Header::Array(Some(items.len())));
            for (index, item) in items.iter().enumerate() {
                encode(encoder, solver, item, unsolved).map_err(within(Step::Position(index)))?;
            }
        }
        Value::Struct(value) => {
            write(encoder, Header::Map(Some(value.values().len())));
            for (field, item) in value.fields() {
                encoder
                    .text(field, None)
                    .expect("writing to a `Vec` does not fail");
                encode(encoder, solver, item, unsolved).map_err(within(Step::Field(field)))?;
            }
        }
        other => {
            return Err(Unencodable {
                kind: value_kind(other),
                path: Vec::new(),
            });
        }
    }
    Ok(())
}

/// How deeply [`decode_ok`] nests before leaving a result to [`decode`].
const MAX_DIRECT_DEPTH: usize = 64;

/// A CBOR decoder over a whole encoded result.
struct CborReader<'r> {
    decoder: Decoder<&'r [u8]>,
    len: usize,
}

impl CborReader<'_> {
    fn pull(&mut self) -> Option<Header> {
        self.decoder.pull().ok()
    }

    /// Reads a text item of definite length.
    fn text(&mut self) -> Option<String> {
        let Some(Header::Text(Some(len))) = self.pull() else {
            return None;
        };
        if len > self.len - self.decoder.offset() {
            return None;
        }
        let mut bytes = vec![0; len];
        self.decoder.read_exact(&mut bytes).ok()?;
        String::from_utf8(bytes).ok()
    }
}

/// Decodes `output`, an `Ok` result of type `ty`, without building a CBOR
/// tree. `None` for anything else, including results that [`decode`] would
/// accept in another form, such as struct fields out of order.
fn decode_ok(defs: &TypeDefs, layouts: &StructLayouts, ty: &Ty, output: &[u8]) -> Option<Value> {
    let mut reader = CborReader {
        decoder: Decoder::from(output),
        len: output.len(),
    };
    if !matches!(reader.pull()?, Header::Map(Some(1))) || reader.text()? != "Ok" {
        return None;
    }
    decode_direct(&mut reader, defs, layouts, ty, 0)
}

/// Decodes the next item as a value of type `ty`, as [`decode`] would.
fn decode_direct(
    reader: &mut CborReader<'_>,
    defs: &TypeDefs,
    layouts: &StructLayouts,
    ty: &Ty,
    depth: usize,
) -> Option<Value> {
    if depth > MAX_DIRECT_DEPTH {
        return None;
    }
    if let Ty::String = ty {
        return reader.text().map(Value::String);
    }
    let number = |value: f64| Value::Linear(LinearExpr::from(value));
    Some(match (ty, reader.pull()?) {
        (Ty::Nil, Header::Simple(simple::NULL)) => Value::Nil,
        (Ty::Bool, Header::Simple(simple::FALSE)) => Value::Bool(false),
        (Ty::Bool, Header::Simple(simple::TRUE)) => Value::Bool(true),
        (Ty::Int, Header::Positive(value)) => Value::Int(i64::try_from(value).ok()?),
        (Ty::Int, Header::Negative(value)) => Value::Int(!i64::try_from(value).ok()?),
        (Ty::Float, Header::Float(value)) if value.is_finite() => number(value),
        (Ty::Float, Header::Positive(value)) => number(value as f64),
        (Ty::Float, Header::Negative(value)) => number((-1 - i128::from(value)) as f64),
        (Ty::Seq(item), Header::Array(Some(len))) => seq_value(
            (0..len)
                .map(|_| decode_direct(reader, defs, layouts, item, depth + 1))
                .collect::<Option<Seq>>()?,
        ),
        (Ty::Tuple(tys), Header::Array(Some(len))) if len == tys.len() => Value::Tuple(
            tys.iter()
                .map(|ty| decode_direct(reader, defs, layouts, ty, depth + 1))
                .collect::<Option<Arc<[Value]>>>()?,
        ),
        (Ty::Struct(struct_ty), Header::Map(Some(len))) => {
            let def = defs.get(&struct_ty.def)?.as_struct()?;
            if !def.params.is_empty() || len != def.fields.len() {
                return None;
            }
            let layout = layouts.get(defs, struct_ty)?;
            let values = def
                .fields
                .iter()
                .map(|(field, ty)| {
                    if reader.text()? != *field {
                        return None;
                    }
                    decode_direct(reader, defs, layouts, ty, depth + 1)
                })
                .collect::<Option<SmallVec<_>>>()?;
            Value::Struct(Arc::new(StructValue::new(layout, values)))
        }
        _ => return None,
    })
}

/// Decodes `value` as a value of type `ty`.
fn decode(
    defs: &TypeDefs,
    layouts: &StructLayouts,
    ty: &Ty,
    value: Cbor,
    at: &str,
) -> Result<Value, String> {
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
                .map(|(index, value)| decode(defs, layouts, item, value, &format!("{at}[{index}]")))
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
                    .map(|(index, (ty, value))| {
                        decode(defs, layouts, ty, value, &format!("{at}.{index}"))
                    })
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
            let values = def
                .fields
                .iter()
                .map(|(field, ty)| {
                    let value = entries
                        .shift_remove(field)
                        .ok_or_else(|| format!("{at} is missing field `{field}`"))?;
                    decode(defs, layouts, ty, value, &format!("{at}.{field}"))
                })
                .collect::<Result<SmallVec<_>, String>>()?;
            if let Some(field) = entries.keys().next() {
                return Err(format!(
                    "{at} has field `{field}`, which struct `{}` does not declare",
                    struct_ty.name
                ));
            }
            let layout = layouts
                .get(defs, struct_ty)
                .expect("the declaration is a struct");
            Value::Struct(Arc::new(StructValue::new(layout, values)))
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
        sync::mpsc,
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
  (data (i32.const 48) "\a1\62Ok\a2\61h\f9\44\00\61w\f9\40\00")
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
  (func (export "argon_fn_swapped") (param i32 i32) (result i64)
    (i64.or (i64.shl (i64.const 48) (i64.const 32)) (i64.const 15)))
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
    fn struct_fields_may_arrive_in_any_order() {
        let output = run(r#"
struct Size { w: Float, h: Float }
extern "fixture" fn swapped() -> Size;

cell top() {
    let s = swapped();
    std::layout::rect("met1", x0=0., y0=0., x1=s.w, y1=s.h)!;
}
"#);
        assert_eq!(rects(&output), [[0., 0., 2., 4.]]);
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
        let (reported, slow) = mpsc::channel();
        let control = CallControl::new().on_slow_call(move |name| {
            let _ = reported.send(name.to_owned());
        });
        let canceller = control.clone();
        // Cancels only once the call has reported itself, so it is known to be running.
        let cancel = std::thread::spawn(move || {
            let report = slow.recv_timeout(Duration::from_secs(60));
            canceller.cancel();
            report
        });
        let output = control
            .run(|| run("extern \"fixture\" fn spin() -> Int;\ncell top() { let r = spin(); }\n"));
        assert_eq!(cancel.join().unwrap().as_deref(), Ok("spin"));
        assert!(output.was_cancelled());
        assert!(errors(&output).contains(&"extern function `spin` was cancelled".to_owned()));
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
