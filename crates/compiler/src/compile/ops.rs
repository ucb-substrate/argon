//! Operations on ready values, shared by deferred evaluation and the direct
//! evaluation of pure functions so that the two compute identical results.

use super::*;
use crate::solver::{LinearExpr, Solver};

/// Why an operation on ready values gave no value.
pub(super) enum OpFailure {
    /// It needs these solver variables solved first.
    Wait(SmallVec<[Var; 4]>),
    /// It fails with this error, reported at this part of the expression.
    Error(ExecErrorKind, ErrorAt),
}

/// The part of an expression an error is reported at.
#[derive(Clone, Copy)]
pub(super) enum ErrorAt {
    Expr,
    /// The base of a field access or an index.
    Base,
    /// The index of an index expression.
    Index,
    /// The field name of a field access.
    Field,
}

pub(super) type OpResult<T = Value> = Result<T, OpFailure>;

fn wait<'e>(exprs: impl IntoIterator<Item = &'e LinearExpr>) -> OpFailure {
    OpFailure::Wait(
        exprs
            .into_iter()
            .flat_map(|expr| expr.coeffs.iter().map(|(_, var)| *var))
            .collect(),
    )
}

fn error(kind: ExecErrorKind) -> OpFailure {
    OpFailure::Error(kind, ErrorAt::Expr)
}

pub(super) fn arith(solver: &Solver, op: ArithOp, left: &Value, right: &Value) -> OpResult {
    match (left, right) {
        (Value::Linear(vl), Value::Linear(vr)) => {
            let res = match op {
                ArithOp::Add => LinearExpr::sum(vl, vr),
                ArithOp::Sub => LinearExpr::difference(vl, vr),
                ArithOp::Mul => match (solver.eval_expr_exact(vl), solver.eval_expr_exact(vr)) {
                    (Some(vl), Some(vr)) => (vl * vr).into(),
                    (Some(vl), None) => vr.clone() * vl,
                    (None, Some(vr)) => vl.clone() * vr,
                    (None, None) => return Err(wait([vl, vr])),
                },
                ArithOp::Div => match solver.eval_expr_exact(vr) {
                    Some(rhs) => vl.clone() / rhs,
                    None => return Err(wait([vr])),
                },
                ArithOp::Rem => return Err(error(ExecErrorKind::InvalidType)),
            };
            // Division by zero is the realistic source. A non-finite
            // coefficient must be rejected where it is created: downstream it
            // hangs the dense SVD (uncatchable), saturates to `i32::MAX` in
            // GDS, and turns `as Int` into `i64::MAX`.
            if !res.is_finite() {
                return Err(error(ExecErrorKind::NonFiniteValue));
            }
            Ok(Value::Linear(res))
        }
        (Value::Int(vl), Value::Int(vr)) => {
            // Raw operators would panic on a zero divisor in every profile
            // and, on overflow, panic in debug but wrap silently in release.
            if matches!(op, ArithOp::Div | ArithOp::Rem) && *vr == 0 {
                return Err(error(ExecErrorKind::DivideByZero(
                    match op {
                        ArithOp::Div => "division",
                        _ => "remainder",
                    }
                    .to_owned(),
                )));
            }
            let res = match op {
                ArithOp::Add => vl.checked_add(*vr),
                ArithOp::Sub => vl.checked_sub(*vr),
                ArithOp::Mul => vl.checked_mul(*vr),
                ArithOp::Div => vl.checked_div(*vr),
                ArithOp::Rem => vl.checked_rem(*vr),
            };
            res.map(Value::Int).ok_or_else(|| {
                error(ExecErrorKind::IntegerOverflow(
                    match op {
                        ArithOp::Add => "+",
                        ArithOp::Sub => "-",
                        ArithOp::Mul => "*",
                        ArithOp::Div => "/",
                        ArithOp::Rem => "%",
                    }
                    .to_owned(),
                ))
            })
        }
        _ => Err(error(ExecErrorKind::InvalidType)),
    }
}

pub(super) fn unary(op: UnaryOp, value: &Value) -> OpResult {
    match (value, op) {
        (Value::Linear(v), UnaryOp::Neg) => Ok(Value::Linear(LinearExpr {
            coeffs: v.coeffs.iter().map(|(coeff, var)| (-coeff, *var)).collect(),
            constant: -v.constant,
        })),
        (Value::Bool(v), UnaryOp::Not) => Ok(Value::Bool(!*v)),
        // `-i64::MIN` has no `Int` representation.
        (Value::Int(v), UnaryOp::Neg) => v
            .checked_neg()
            .map(Value::Int)
            .ok_or_else(|| error(ExecErrorKind::IntegerOverflow("-".to_owned()))),
        _ => Err(error(ExecErrorKind::InvalidType)),
    }
}

pub(super) fn compare(
    solver: &Solver,
    op: ComparisonOp,
    left: &Value,
    right: &Value,
) -> OpResult<bool> {
    let ordered = |ord: std::cmp::Ordering| match op {
        ComparisonOp::Eq => Some(ord.is_eq()),
        ComparisonOp::Ne => Some(ord.is_ne()),
        ComparisonOp::Geq => Some(ord.is_ge()),
        ComparisonOp::Gt => Some(ord.is_gt()),
        ComparisonOp::Leq => Some(ord.is_le()),
        ComparisonOp::Lt => Some(ord.is_lt()),
    };
    let equality = |eq: bool| match op {
        ComparisonOp::Eq => Some(eq),
        ComparisonOp::Ne => Some(!eq),
        // Only equality is defined for these operand types.
        _ => None,
    };
    // `Ty::Any` satisfies the static comparison checks, so an operand pair or
    // an operator that those checks would have rejected can still arrive here.
    let res = match (left, right) {
        (Value::Linear(vl), Value::Linear(vr)) => {
            let (Some(el), Some(er)) = (solver.eval_expr(vl), solver.eval_expr(vr)) else {
                return Err(wait([vl, vr]));
            };
            match op {
                // Float equality is meaningless against a solved value, and
                // is rejected statically whenever the type is known.
                ComparisonOp::Eq | ComparisonOp::Ne => None,
                _ => el.partial_cmp(&er).and_then(ordered),
            }
        }
        (Value::Int(vl), Value::Int(vr)) => ordered(vl.cmp(vr)),
        (Value::Bool(vl), Value::Bool(vr)) => equality(vl == vr),
        (Value::Enum(_), Value::Enum(_)) => values_equal(left, right).and_then(equality),
        (Value::Nil, Value::Nil) => equality(true),
        (Value::SeqNil, Value::SeqNil) => equality(true),
        (Value::Seq(x), Value::SeqNil) | (Value::SeqNil, Value::Seq(x)) => equality(x.is_empty()),
        _ => None,
    };
    res.ok_or_else(|| error(ExecErrorKind::InvalidType))
}

pub(super) fn cast(solver: &Solver, value: &Value, ty: &Ty) -> OpResult {
    match (value, ty) {
        (Value::Int(x), Ty::Float) => Ok(Value::Linear(LinearExpr::from(*x as f64))),
        (x @ Value::Int(_), Ty::Int) => Ok(x.clone()),
        (Value::Linear(expr), Ty::Int) => {
            // `f64 as i64` saturates rather than failing, so a non-finite
            // input would silently become `i64::MAX`.
            if let Some(val) = solver.eval_expr(expr)
                && !val.is_finite()
            {
                return Err(error(ExecErrorKind::NonFiniteValue));
            }
            match solver.eval_expr_exact(expr) {
                Some(val) => Ok(Value::Int(val as i64)),
                None => Err(wait([expr])),
            }
        }
        (expr @ Value::Linear(_), Ty::Float) => Ok(expr.clone()),
        _ => Err(error(ExecErrorKind::InvalidCast)),
    }
}

/// A field of any value but an instance, whose fields are built in the
/// parent cell.
pub(super) fn field(base: &Value, field: &str) -> OpResult {
    let points = |points: &[(LinearExpr, LinearExpr)]| {
        Value::Seq(Arc::new(
            points
                .iter()
                .map(|(x, y)| Value::Point(Box::new((x.clone(), y.clone()))))
                .collect(),
        ))
    };
    let coordinate = |points: &[(LinearExpr, LinearExpr)], name: &str| {
        let coordinate = polygon_coordinate(name)
            .filter(|coordinate| !coordinate.initial)
            .ok_or_else(|| error(ExecErrorKind::InvalidType))?;
        let point = points.get(coordinate.index).ok_or(OpFailure::Error(
            ExecErrorKind::IndexOutOfBounds,
            ErrorAt::Field,
        ))?;
        Ok(Value::Linear(match coordinate.axis {
            PolygonAxis::X => point.0.clone(),
            PolygonAxis::Y => point.1.clone(),
        }))
    };
    match base {
        Value::Rect(rect) => Ok(match field {
            "x0" => Value::Linear(rect.x0.clone()),
            "x1" => Value::Linear(rect.x1.clone()),
            "y0" => Value::Linear(rect.y0.clone()),
            "y1" => Value::Linear(rect.y1.clone()),
            "w" => Value::Linear(LinearExpr::difference(&rect.x1, &rect.x0)),
            "h" => Value::Linear(LinearExpr::difference(&rect.y1, &rect.y0)),
            // A construction rect has no layer. A fabricated `""` would add a
            // second, unrelated error about that layer being undefined.
            "layer" => Value::String(rect.layer.clone().ok_or_else(|| {
                error(ExecErrorKind::EmptyField {
                    field: "layer".to_string(),
                })
            })?),
            _ => return Err(error(ExecErrorKind::InvalidType)),
        }),
        Value::Polygon(polygon) => match field {
            "points" => Ok(points(&polygon.points)),
            "layer" => Ok(Value::String(polygon.layer.clone())),
            name => coordinate(&polygon.points, name),
        },
        Value::Path(path) => match field {
            "points" => Ok(points(&path.points)),
            "layer" => Ok(Value::String(path.layer.clone())),
            "width" => Ok(Value::Linear(path.width.clone())),
            "begin_extension" => Ok(Value::Linear(path.begin_extension.clone())),
            "end_extension" => Ok(Value::Linear(path.end_extension.clone())),
            name => coordinate(&path.points, name),
        },
        Value::Point(point) => match field {
            "x" => Ok(Value::Linear(point.0.clone())),
            "y" => Ok(Value::Linear(point.1.clone())),
            _ => Err(error(ExecErrorKind::InvalidType)),
        },
        // The base may have arrived as `Any`, so the field was never checked
        // against the struct.
        Value::Struct(value) => value
            .fields
            .get(field)
            .cloned()
            .ok_or_else(|| error(ExecErrorKind::InvalidType)),
        _ => Err(error(ExecErrorKind::InvalidType)),
    }
}

/// Element `index` of a tuple, `None` when the index does not fit a `usize`.
pub(super) fn tuple_field(base: &Value, index: Option<usize>) -> OpResult {
    match base {
        Value::Tuple(items) => index
            .and_then(|index| items.get(index))
            .cloned()
            .ok_or_else(|| error(ExecErrorKind::InvalidType)),
        _ => Err(error(ExecErrorKind::InvalidType)),
    }
}

pub(super) fn index(base: &Value, index: &Value) -> OpResult {
    let Value::Seq(seq) = base else {
        return Err(OpFailure::Error(ExecErrorKind::InvalidType, ErrorAt::Base));
    };
    let Value::Int(index) = index else {
        return Err(OpFailure::Error(ExecErrorKind::InvalidType, ErrorAt::Index));
    };
    usize::try_from(*index)
        .ok()
        .and_then(|index| seq.get(index))
        .cloned()
        .ok_or_else(|| error(ExecErrorKind::IndexOutOfBounds))
}

/// A struct literal's value. `explicit` gives the value of each field the
/// literal names; the rest come from `base`.
pub(super) fn struct_lit<'v>(
    defs: &TypeDefs,
    ty: &StructTy,
    explicit: impl Fn(&str) -> Option<&'v Value>,
    base: Option<&Value>,
) -> OpResult {
    // The static check proved the base to be this struct unless it was typed
    // `Any`.
    let base = match base {
        None => None,
        Some(Value::Struct(value)) if value.name == ty.name => Some(&value.fields),
        Some(_) => return Err(OpFailure::Error(ExecErrorKind::InvalidType, ErrorAt::Base)),
    };
    let def = defs
        .get(&ty.def)
        .and_then(AdtDef::as_struct)
        .ok_or_else(|| error(ExecErrorKind::InvalidType))?;
    // Declaration order, whatever order the literal used: `CellArg::Struct`
    // fields are matched pairwise against the type's.
    let fields = def
        .fields
        .keys()
        .map(|name| {
            let value = match explicit(name) {
                Some(value) => Some(value.clone()),
                None => base.and_then(|base| base.get(name).cloned()),
            };
            value.map(|value| (name.clone(), value))
        })
        .collect::<Option<IndexMap<_, _>>>()
        .ok_or_else(|| error(ExecErrorKind::InvalidType))?;
    Ok(Value::Struct(Arc::new(StructValue {
        name: ty.name.clone(),
        fields,
    })))
}

pub(super) fn cons(head: &Value, tail: &Value) -> OpResult {
    let seq = match tail {
        Value::SeqNil => {
            let mut seq = Seq::new();
            seq.push_back(head.clone());
            seq
        }
        Value::Seq(seq) => {
            let mut seq = (**seq).clone();
            seq.push_front(head.clone());
            seq
        }
        _ => return Err(error(ExecErrorKind::InvalidType)),
    };
    Ok(Value::Seq(Arc::new(seq)))
}

pub(super) fn list<'v>(items: impl IntoIterator<Item = &'v Value>) -> Value {
    Value::Seq(Arc::new(items.into_iter().cloned().collect()))
}

pub(super) fn range_full(start: &Value, stop: &Value, step: &Value) -> OpResult {
    let (Value::Int(start), Value::Int(stop), Value::Int(step)) = (start, stop, step) else {
        return Err(error(ExecErrorKind::InvalidType));
    };
    // A zero step never reaches `stop`, so there is no sequence it could mean.
    if *step == 0 {
        return Err(error(ExecErrorKind::ZeroRangeStep));
    }
    let mut seq = Seq::new();
    let mut i = *start;
    while if *step > 0 { i < *stop } else { i > *stop } {
        if seq.len() >= MAX_SEQ_LEN {
            return Err(error(ExecErrorKind::LimitExceeded {
                what: "sequence length".to_owned(),
                limit: MAX_SEQ_LEN,
            }));
        }
        seq.push_back(Value::Int(i));
        // A wrapping `i` would reverse the comparison and loop forever; the
        // correct result is the elements produced so far.
        match i.checked_add(*step) {
            Some(next) => i = next,
            None => break,
        }
    }
    Ok(Value::Seq(Arc::new(seq)))
}

pub(super) fn head(list: &Value) -> OpResult {
    match list {
        Value::SeqNil => Err(error(ExecErrorKind::HeadEmptyList)),
        Value::Seq(seq) => seq
            .front()
            .cloned()
            .ok_or_else(|| error(ExecErrorKind::HeadEmptyList)),
        _ => Err(error(ExecErrorKind::InvalidType)),
    }
}

pub(super) fn tail(list: &Value) -> OpResult {
    match list {
        Value::Seq(seq) if !seq.is_empty() => {
            let mut seq = (**seq).clone();
            seq.pop_front();
            Ok(Value::Seq(Arc::new(seq)))
        }
        Value::SeqNil | Value::Seq(_) => Err(error(ExecErrorKind::TailEmptyList)),
        _ => Err(error(ExecErrorKind::InvalidType)),
    }
}

/// `max_float`, `min_float`, `max_int` or `min_int`: the second value when the
/// first is less, the first for `max` and the second for `min` on ties.
pub(super) fn min_max(solver: &Solver, max: bool, a: &Value, b: &Value) -> OpResult {
    let less = match (a, b) {
        (Value::Int(x), Value::Int(y)) => x < y,
        (Value::Linear(x), Value::Linear(y)) => {
            let (Some(x), Some(y)) = (solver.eval_expr(x), solver.eval_expr(y)) else {
                return Err(wait([x, y]));
            };
            x.partial_cmp(&y)
                .ok_or_else(|| error(ExecErrorKind::InvalidType))?
                .is_lt()
        }
        _ => return Err(error(ExecErrorKind::InvalidType)),
    };
    Ok(if less == max { b.clone() } else { a.clone() })
}

pub(super) fn sequence(builtin: Builtin, args: &[&Value]) -> OpResult {
    let name = match builtin {
        Builtin::SeqLen => "seq_len",
        Builtin::SeqConcat => "seq_concat",
        Builtin::SeqFlatten => "seq_flatten",
        Builtin::SeqSum => "seq_sum",
        Builtin::SeqAny => "seq_any",
        Builtin::SeqAll => "seq_all",
        _ => unreachable!("not a sequence builtin"),
    };
    seq_builtin(name, args).ok_or_else(|| error(ExecErrorKind::InvalidType))
}

/// `max_via_array` on its ten arguments. See [`crate::via_array`].
pub(super) fn max_via_array(solver: &Solver, args: &[&Value]) -> OpResult {
    // Sizes, enclosures and the two shapes' coordinates, then the extension
    // directions and the two flags.
    let mut exprs = SmallVec::<[&LinearExpr; 14]>::new();
    let mut dirs = SmallVec::<[i64; 2]>::new();
    let mut flags = SmallVec::<[bool; 2]>::new();
    for (index, value) in args.iter().enumerate() {
        match (index, value) {
            (0 | 1, Value::Linear(expr)) => exprs.push(expr),
            (2 | 3, Value::Tuple(pair)) => match pair.as_slice() {
                [Value::Linear(a), Value::Linear(b)] => exprs.extend([a, b]),
                _ => return Err(error(ExecErrorKind::InvalidType)),
            },
            (4 | 5, Value::Rect(r)) => exprs.extend([&r.x0, &r.y0, &r.x1, &r.y1]),
            (6 | 7, Value::Int(n)) => dirs.push(*n),
            (8 | 9, Value::Bool(b)) => flags.push(*b),
            _ => return Err(error(ExecErrorKind::InvalidType)),
        }
    }
    let Some(x) = exprs
        .iter()
        .map(|expr| solver.eval_expr_exact(expr))
        .collect::<Option<Vec<f64>>>()
    else {
        return Err(wait(exprs.iter().copied()));
    };
    let choice = crate::via_array::max_via_array(
        &crate::via_array::ViaArrayQuery {
            size: x[0],
            space: x[1],
            bot_enc: (x[2], x[3]),
            top_enc: (x[4], x[5]),
            bot: [x[6], x[7], x[8], x[9]],
            top: [x[10], x[11], x[12], x[13]],
            bot_ext_dir: dirs[0],
            top_ext_dir: dirs[1],
            longer: flags[0],
            later_wins_ties: flags[1],
        },
        solver.grid(),
    );
    let float = |v: f64| Value::Linear(LinearExpr::from(v));
    Ok(Value::Tuple(vec![
        Value::Int(choice.0),
        Value::Int(choice.1),
        float(choice.2),
        float(choice.3),
        float(choice.4),
        float(choice.5),
        Value::Int(choice.6),
        float(choice.7),
    ]))
}
