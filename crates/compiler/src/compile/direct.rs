//! Direct evaluation of calls to pure functions: the call is evaluated to a
//! value with no value IDs, frames, scopes or worklist.
//!
//! It is used only without debug information, which keeps the scopes the
//! editor shows. Anything that would wait on the solver or report an error
//! gives up instead, and the ordinary evaluation of the call handles it. A
//! pure function has no effects, so evaluating it again is safe.

use super::ops;
use super::*;

/// How deeply directly evaluated calls may nest; deeper recursion is left to
/// ordinary evaluation.
const MAX_DIRECT_DEPTH: u32 = 16;

/// Variables bound in one directly evaluated call, innermost last.
type Env = SmallVec<[(VarId, Value); 8]>;

/// Whether calls to a function can be evaluated directly.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Purity {
    Unknown,
    /// Being checked; assumed pure, so that recursion does not loop.
    Checking,
    Pure,
    /// Creates geometry, constraints or instances, or calls something that
    /// does.
    Impure,
    /// Pure, but recursion through it nested too deeply.
    TooDeep,
}

impl<'a> ExecPass<'a> {
    /// Evaluates a call to the function `index` directly, given the values
    /// of its explicit arguments in parameter order. `None` when it cannot
    /// be, for whatever reason.
    pub(super) fn call_directly(
        &mut self,
        cell: CellId,
        index: DeclIndex,
        explicit: &[Option<ValueId>],
    ) -> Option<Value> {
        if !self.is_pure(index) {
            return None;
        }
        let args = explicit
            .iter()
            .map(|arg| match arg {
                Some(arg) => self.values.get(arg)?.get_ready().cloned().map(Some),
                None => Some(None),
            })
            .collect::<Option<SmallVec<[Option<Value>; 4]>>>()?;
        self.direct_too_deep = false;
        self.direct_fn(cell, index, args, 0)
    }

    fn direct_fn(
        &mut self,
        cell: CellId,
        index: DeclIndex,
        args: SmallVec<[Option<Value>; 4]>,
        depth: u32,
    ) -> Option<Value> {
        if depth > MAX_DIRECT_DEPTH {
            self.direct_too_deep = true;
        }
        if self.direct_too_deep {
            self.purity[index.0 as usize] = Purity::TooDeep;
            return None;
        }
        let decl = self.fn_decls[index.0 as usize];
        let mut env = Env::new();
        for (param, arg) in decl.args.iter().zip(args) {
            let value = match arg {
                Some(value) => value,
                // A default sees the parameters before it.
                None => self.direct_expr(cell, &mut env, param.default.as_ref()?, depth)?,
            };
            env.push((param.metadata.0, value));
        }
        let value = self.direct_scope(cell, &mut env, &decl.scope, depth);
        if self.direct_too_deep {
            self.purity[index.0 as usize] = Purity::TooDeep;
        }
        value
    }

    fn direct_scope(
        &mut self,
        cell: CellId,
        env: &mut Env,
        scope: &'a Scope<Substr, VarIdTyMetadata>,
        depth: u32,
    ) -> Option<Value> {
        let mark = env.len();
        for stmt in &scope.stmts {
            match stmt {
                Statement::LetBinding(binding) => {
                    let value = self.direct_expr(cell, env, &binding.value, depth)?;
                    env.push((binding.metadata.0, value));
                }
                Statement::Expr { value, .. } => {
                    self.direct_expr(cell, env, value, depth)?;
                }
                Statement::ForLoop(f) => {
                    self.direct_for(cell, env, f, false, depth)?;
                }
            }
        }
        let value = match &scope.tail {
            Some(tail) => self.direct_expr(cell, env, tail, depth)?,
            None => Value::Nil,
        };
        env.truncate(mark);
        Some(value)
    }

    /// The value bound to `var`: a local, or a global declaration.
    fn direct_lookup<'e>(&'e self, env: &'e Env, var: VarId) -> Option<&'e Value> {
        if let Some((_, value)) = env.iter().rev().find(|(bound, _)| *bound == var) {
            return Some(value);
        }
        let vid = self.frames[&self.global_frame].bindings.get(&var)?;
        self.values.get(vid)?.get_ready()
    }

    /// Calls `f` with the value of `expr`, borrowing it when `expr` names a
    /// variable rather than cloning it.
    fn with_operand<R>(
        &mut self,
        cell: CellId,
        env: &mut Env,
        expr: &'a Expr<Substr, VarIdTyMetadata>,
        depth: u32,
        f: impl FnOnce(&Self, &Value) -> Option<R>,
    ) -> Option<R> {
        if let Expr::IdentPath(path) = expr {
            let value = self.direct_lookup(env, path.metadata.0?)?;
            return f(self, value);
        }
        let value = self.direct_expr(cell, env, expr, depth)?;
        f(self, &value)
    }

    fn direct_expr(
        &mut self,
        cell: CellId,
        env: &mut Env,
        expr: &'a Expr<Substr, VarIdTyMetadata>,
        depth: u32,
    ) -> Option<Value> {
        match expr {
            Expr::Nil(_) => Some(Value::Nil),
            Expr::SeqNil(_) => Some(Value::SeqNil),
            Expr::FloatLiteral(f) => Some(Value::Linear(LinearExpr::from(f.value))),
            Expr::IntLiteral(i) => Some(Value::Int(i.value)),
            Expr::BoolLiteral(b) => Some(Value::Bool(b.value)),
            Expr::StringLiteral(s) => Some(Value::String(s.value.to_string())),
            Expr::IdentPath(path) => self.direct_lookup(env, path.metadata.0?).cloned(),
            Expr::ListComp(comp) => self.direct_for(cell, env, &comp.for_loop, true, depth),
            Expr::Emit(_) => None,
            Expr::Call(c) => self.direct_call(cell, env, c, depth),
            Expr::If(if_expr) => {
                let Value::Bool(cond) = self.direct_expr(cell, env, &if_expr.cond, depth)? else {
                    return None;
                };
                if cond {
                    self.direct_scope(cell, env, &if_expr.then, depth)
                } else if let Some(else_) = &if_expr.else_ {
                    self.direct_scope(cell, env, else_, depth)
                } else {
                    Some(Value::Nil)
                }
            }
            Expr::Match(match_expr) => {
                let scrutinee = self.direct_expr(cell, env, &match_expr.scrutinee, depth)?;
                let Value::Enum(value) = &scrutinee else {
                    return None;
                };
                let arm = match_expr
                    .arms
                    .iter()
                    .find(|arm| pattern_matches(&arm.pattern, &scrutinee))?;
                let mark = env.len();
                match &arm.pattern {
                    Pattern::Wildcard { .. } => {}
                    Pattern::Binding { metadata, .. } => env.push((metadata.0, scrutinee.clone())),
                    Pattern::Variant { fields, .. } => {
                        for (field, element) in fields.iter().zip(&value.payload) {
                            if let Pattern::Binding { metadata, .. } = field {
                                env.push((metadata.0, element.clone()));
                            }
                        }
                    }
                }
                let value = self.direct_expr(cell, env, &arm.expr, depth)?;
                env.truncate(mark);
                Some(value)
            }
            Expr::Scope(scope) => self.direct_scope(cell, env, scope, depth),
            Expr::FieldAccess(f) => self.with_operand(cell, env, &f.base, depth, |_, base| {
                ops::field(base, f.field.name.as_str()).ok()
            }),
            Expr::IndexFieldAccess(f) => self.with_operand(cell, env, &f.base, depth, |_, base| {
                ops::tuple_field(base, usize::try_from(f.field.value).ok()).ok()
            }),
            Expr::Index(i) => {
                let index = self.direct_expr(cell, env, &i.index, depth)?;
                self.with_operand(cell, env, &i.base, depth, |_, base| {
                    ops::index(base, &index).ok()
                })
            }
            Expr::BinOp(b) => match b.op {
                BinOp::Arith(op) => {
                    let left = self.direct_expr(cell, env, &b.left, depth)?;
                    let right = self.direct_expr(cell, env, &b.right, depth)?;
                    ops::arith(&self.cell_state(cell).solver, op, &left, &right).ok()
                }
                BinOp::Cmp(op) => {
                    let left = self.direct_expr(cell, env, &b.left, depth)?;
                    let right = self.direct_expr(cell, env, &b.right, depth)?;
                    ops::compare(&self.cell_state(cell).solver, op, &left, &right)
                        .ok()
                        .map(Value::Bool)
                }
                BinOp::Bool(op) => {
                    let Value::Bool(left) = self.direct_expr(cell, env, &b.left, depth)? else {
                        return None;
                    };
                    let decided = match op {
                        BoolOp::And => !left,
                        BoolOp::Or => left,
                    };
                    if decided {
                        return Some(Value::Bool(left));
                    }
                    match self.direct_expr(cell, env, &b.right, depth)? {
                        right @ Value::Bool(_) => Some(right),
                        _ => None,
                    }
                }
            },
            Expr::UnaryOp(u) => {
                let operand = self.direct_expr(cell, env, &u.operand, depth)?;
                ops::unary(u.op, &operand).ok()
            }
            Expr::Cast(cast) => {
                let value = self.direct_expr(cell, env, &cast.value, depth)?;
                ops::cast(&self.cell_state(cell).solver, &value, &cast.metadata).ok()
            }
            Expr::Tuple(tuple) => Some(Value::Tuple(
                tuple
                    .items
                    .iter()
                    .map(|item| self.direct_expr(cell, env, item, depth))
                    .collect::<Option<_>>()?,
            )),
            Expr::StructLit(lit) => {
                let Ty::Struct(ty) = &lit.metadata else {
                    return None;
                };
                let fields = lit
                    .fields
                    .iter()
                    .map(|field| self.direct_expr(cell, env, &field.value, depth))
                    .collect::<Option<SmallVec<[Value; 8]>>>()?;
                let base = match &lit.base {
                    Some(base) => Some(self.direct_expr(cell, env, base, depth)?),
                    None => None,
                };
                let explicit = |name: &str| {
                    lit.fields
                        .iter()
                        .zip(&fields)
                        .find(|(field, _)| field.name.name == *name)
                        .map(|(_, value)| value)
                };
                ops::struct_lit(self.defs, ty, explicit, base.as_ref()).ok()
            }
        }
    }

    fn direct_call(
        &mut self,
        cell: CellId,
        env: &mut Env,
        c: &'a CallExpr<Substr, VarIdTyMetadata>,
        depth: u32,
    ) -> Option<Value> {
        let Some(callee) = c.metadata.0 else {
            let builtin = Builtin::from_name(&c.func.path.last().unwrap().name)?;
            if !c.args.kwargs.is_empty() {
                return None;
            }
            let args = c
                .args
                .posargs
                .iter()
                .map(|arg| self.direct_expr(cell, env, arg, depth))
                .collect::<Option<SmallVec<[Value; 4]>>>()?;
            let refs = args.iter().collect::<SmallVec<[&Value; 4]>>();
            let solver = &self.cell_state(cell).solver;
            return match builtin {
                Builtin::Cons => ops::cons(refs.first()?, refs.get(1)?).ok(),
                Builtin::List => Some(ops::list(refs)),
                Builtin::RangeFull => {
                    ops::range_full(refs.first()?, refs.get(1)?, refs.get(2)?).ok()
                }
                Builtin::Head => ops::head(refs.first()?).ok(),
                Builtin::Tail => ops::tail(refs.first()?).ok(),
                Builtin::MaxFloat | Builtin::MinFloat | Builtin::MaxInt | Builtin::MinInt => {
                    let max = matches!(builtin, Builtin::MaxFloat | Builtin::MaxInt);
                    ops::min_max(solver, max, refs.first()?, refs.get(1)?).ok()
                }
                Builtin::SeqLen
                | Builtin::SeqConcat
                | Builtin::SeqFlatten
                | Builtin::SeqSum
                | Builtin::SeqAny
                | Builtin::SeqAll => ops::sequence(builtin, &refs).ok(),
                Builtin::MaxViaArray => ops::max_via_array(solver, &refs).ok(),
                _ => None,
            };
        };
        match self.direct_lookup(env, callee)? {
            Value::Fn(index) => {
                let index = *index;
                if !self.is_pure(index) {
                    return None;
                }
                let decl = self.fn_decls[index.0 as usize];
                // Matched to parameters as `explicit_args` matches them.
                let mut args = SmallVec::<[Option<Value>; 4]>::new();
                for (position, param) in decl.args.iter().enumerate() {
                    let arg = match c.args.posargs.get(position) {
                        Some(arg) => Some(arg),
                        None => c
                            .args
                            .kwargs
                            .iter()
                            .find(|kwarg| kwarg.name.name == param.name.name)
                            .map(|kwarg| &kwarg.value),
                    };
                    args.push(match arg {
                        Some(arg) => Some(self.direct_expr(cell, env, arg, depth)?),
                        None => None,
                    });
                }
                self.direct_fn(cell, index, args, depth + 1)
            }
            Value::Ctor(ctor) => {
                let variant = ctor.variant.clone();
                let payload = c
                    .args
                    .posargs
                    .iter()
                    .map(|arg| self.direct_expr(cell, env, arg, depth))
                    .collect::<Option<Vec<_>>>()?;
                Some(Value::Enum(Arc::new(EnumValue { variant, payload })))
            }
            _ => None,
        }
    }

    fn direct_for(
        &mut self,
        cell: CellId,
        env: &mut Env,
        f: &'a ForLoop<Substr, VarIdTyMetadata>,
        collect: bool,
        depth: u32,
    ) -> Option<Value> {
        let items = match self.direct_expr(cell, env, &f.seq, depth)? {
            Value::Seq(items) => items,
            Value::SeqNil => Arc::new(Seq::new()),
            _ => return None,
        };
        let mut seq = Seq::new();
        for item in items.iter() {
            let mark = env.len();
            env.push((f.metadata, item.clone()));
            let keep = match &f.filter {
                Some(filter) => match self.direct_expr(cell, env, filter, depth)? {
                    Value::Bool(keep) => keep,
                    _ => return None,
                },
                None => true,
            };
            if keep {
                let value = self.direct_scope(cell, env, &f.body, depth)?;
                if collect {
                    seq.push_back(value);
                }
            }
            env.truncate(mark);
        }
        Some(if !collect {
            Value::Nil
        } else if seq.is_empty() {
            Value::SeqNil
        } else {
            Value::Seq(Arc::new(seq))
        })
    }

    /// Whether calls to the function `index` may be evaluated directly.
    fn is_pure(&mut self, index: DeclIndex) -> bool {
        if self.purity.len() < self.fn_decls.len() {
            self.purity.resize(self.fn_decls.len(), Purity::Unknown);
        }
        match self.purity[index.0 as usize] {
            Purity::Pure | Purity::Checking => return true,
            Purity::Impure | Purity::TooDeep => return false,
            Purity::Unknown => {}
        }
        self.purity[index.0 as usize] = Purity::Checking;
        let pure = self.scope_is_pure(&self.fn_decls[index.0 as usize].scope);
        // A function checked meanwhile assumed this one pure. If it is not,
        // the check `direct_call` makes before each call catches that.
        self.purity[index.0 as usize] = if pure { Purity::Pure } else { Purity::Impure };
        pure
    }

    fn scope_is_pure(&mut self, scope: &'a Scope<Substr, VarIdTyMetadata>) -> bool {
        scope.stmts.iter().all(|stmt| match stmt {
            Statement::LetBinding(binding) => self.expr_is_pure(&binding.value),
            Statement::Expr { value, .. } => self.expr_is_pure(value),
            Statement::ForLoop(f) => self.for_is_pure(f),
        }) && scope
            .tail
            .as_ref()
            .is_none_or(|tail| self.expr_is_pure(tail))
    }

    fn for_is_pure(&mut self, f: &'a ForLoop<Substr, VarIdTyMetadata>) -> bool {
        self.expr_is_pure(&f.seq)
            && f.filter
                .as_ref()
                .is_none_or(|filter| self.expr_is_pure(filter))
            && self.scope_is_pure(&f.body)
    }

    fn expr_is_pure(&mut self, expr: &'a Expr<Substr, VarIdTyMetadata>) -> bool {
        match expr {
            Expr::Emit(_) => false,
            Expr::Call(c) => {
                let args_pure = c.args.posargs.iter().all(|arg| self.expr_is_pure(arg))
                    && c.args
                        .kwargs
                        .iter()
                        .all(|kwarg| self.expr_is_pure(&kwarg.value));
                if !args_pure {
                    return false;
                }
                let Some(callee) = c.metadata.0 else {
                    return Builtin::from_name(&c.func.path.last().unwrap().name)
                        .is_some_and(Builtin::is_pure);
                };
                // A callee bound to a local is not known until the call.
                let Some(vid) = self.frames[&self.global_frame].bindings.get(&callee) else {
                    return false;
                };
                match self.values.get(vid).and_then(Defer::get_ready) {
                    Some(Value::Fn(index)) => {
                        let index = *index;
                        self.is_pure(index)
                    }
                    Some(Value::Ctor(_)) => true,
                    _ => false,
                }
            }
            Expr::If(if_expr) => {
                self.expr_is_pure(&if_expr.cond)
                    && self.scope_is_pure(&if_expr.then)
                    && if_expr
                        .else_
                        .as_ref()
                        .is_none_or(|else_| self.scope_is_pure(else_))
            }
            Expr::Match(match_expr) => {
                self.expr_is_pure(&match_expr.scrutinee)
                    && match_expr
                        .arms
                        .iter()
                        .all(|arm| self.expr_is_pure(&arm.expr))
            }
            Expr::Scope(scope) => self.scope_is_pure(scope),
            Expr::ListComp(comp) => self.for_is_pure(&comp.for_loop),
            Expr::FieldAccess(f) => self.expr_is_pure(&f.base),
            Expr::IndexFieldAccess(f) => self.expr_is_pure(&f.base),
            Expr::Index(i) => self.expr_is_pure(&i.base) && self.expr_is_pure(&i.index),
            Expr::BinOp(b) => self.expr_is_pure(&b.left) && self.expr_is_pure(&b.right),
            Expr::UnaryOp(u) => self.expr_is_pure(&u.operand),
            Expr::Cast(cast) => self.expr_is_pure(&cast.value),
            Expr::Tuple(tuple) => tuple.items.iter().all(|item| self.expr_is_pure(item)),
            Expr::StructLit(lit) => {
                lit.fields
                    .iter()
                    .all(|field| self.expr_is_pure(&field.value))
                    && lit.base.as_ref().is_none_or(|base| self.expr_is_pure(base))
            }
            Expr::Nil(_)
            | Expr::SeqNil(_)
            | Expr::FloatLiteral(_)
            | Expr::IntLiteral(_)
            | Expr::BoolLiteral(_)
            | Expr::StringLiteral(_)
            | Expr::IdentPath(_) => true,
        }
    }
}

impl Builtin {
    /// Whether the builtin only computes a value: no geometry, constraints,
    /// instances or reads of compiled cells.
    fn is_pure(self) -> bool {
        matches!(
            self,
            Builtin::List
                | Builtin::Cons
                | Builtin::Head
                | Builtin::Tail
                | Builtin::RangeFull
                | Builtin::SeqLen
                | Builtin::SeqConcat
                | Builtin::SeqFlatten
                | Builtin::SeqSum
                | Builtin::SeqAny
                | Builtin::SeqAll
                | Builtin::MaxFloat
                | Builtin::MinFloat
                | Builtin::MaxInt
                | Builtin::MinInt
                | Builtin::MaxViaArray
        )
    }
}
