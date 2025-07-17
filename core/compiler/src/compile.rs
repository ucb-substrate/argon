//! # Argon compiler
//!
//! Pass 1: assign variable IDs/type checking
//! Pass 3: solving
use std::collections::{HashMap, HashSet};

use anyhow::Result;
use derive_where::derive_where;
use enumify::enumify;
use serde::{Deserialize, Serialize};

use crate::ast::{FieldAccessExpr, Typ};
use crate::{
    ast::{
        ArgDecl, Ast, AstMetadata, AstTransformer, BinOpExpr, CallExpr, CellDecl, ComparisonExpr,
        Decl, EmitExpr, EnumValue, Expr, FloatLiteral, Ident, IfExpr, LetBinding, Statement,
    },
    parse::ParseMetadata,
    solver::{LinearExpr, Solver, Var},
};

pub fn compile(input: CompileInput<'_, ParseMetadata>) -> CompiledCell {
    let pass = VarIdTyPass::new();
    let ast = pass.execute(input.clone());
    let input = CompileInput {
        ast: &ast,
        cell: input.cell,
        params: input.params,
    };
    ExecPass::new().execute(input)
}

pub(crate) struct VarIdTyPass<'a> {
    next_id: VarId,
    bindings: Vec<HashMap<&'a str, (VarId, Ty)>>,
}

pub(crate) struct VarIdTyMetadata;

#[derive(Debug, Clone, Copy, Hash, PartialEq, Eq)]
pub enum Ty {
    Bool,
    Float,
    Int,
    Rect,
    Enum,
    Nil,
}

impl AstMetadata for VarIdTyMetadata {
    type Ident = ();
    type EnumDecl = ();
    type CellDecl = ();
    type ConstantDecl = ();
    type LetBinding = VarId;
    type IfExpr = Ty;
    type BinOpExpr = Ty;
    type ComparisonExpr = Ty;
    type FieldAccessExpr = Ty;
    type EnumValue = ();
    type CallExpr = Ty;
    type EmitExpr = Ty;
    type Args = ();
    type KwArgValue = Ty;
    type ArgDecl = ();
    type Typ = ();
    type VarExpr = (VarId, Ty);
}

impl<'a> VarIdTyPass<'a> {
    pub(crate) fn new() -> Self {
        Self {
            // allocate space for the global namespace
            bindings: vec![HashMap::new()],
            next_id: 1,
        }
    }

    fn lookup(&self, name: &str) -> Option<(VarId, Ty)> {
        for map in self.bindings.iter().rev() {
            if let Some(info) = map.get(name) {
                return Some(*info);
            }
        }
        None
    }

    fn alloc(&mut self, name: &'a str, ty: Ty) -> VarId {
        let id = self.next_id;
        self.bindings.last_mut().unwrap().insert(name, (id, ty));
        self.next_id += 1;
        id
    }

    pub(crate) fn execute(
        mut self,
        input: CompileInput<'a, ParseMetadata>,
    ) -> Ast<'a, VarIdTyMetadata> {
        let cell = input
            .ast
            .decls
            .iter()
            .find_map(|d| match d {
                Decl::Cell(
                    v @ CellDecl {
                        name: Ident { name, .. },
                        ..
                    },
                ) if *name == input.cell => Some(v),
                _ => None,
            })
            .expect("top cell not found");

        Ast {
            decls: vec![Decl::Cell(CellDecl {
                name: self.transform_ident(&cell.name),
                args: cell
                    .args
                    .iter()
                    .map(|arg| self.transform_arg_decl(arg))
                    .collect(),
                stmts: cell
                    .stmts
                    .iter()
                    .map(|stmt| self.transform_statement(stmt))
                    .collect(),
                metadata: (),
            })],
        }
    }
}

impl<'a> Expr<'a, VarIdTyMetadata> {
    fn ty(&self) -> Ty {
        match self {
            Expr::If(if_expr) => if_expr.metadata,
            Expr::Comparison(comparison_expr) => comparison_expr.metadata,
            Expr::BinOp(bin_op_expr) => bin_op_expr.metadata,
            Expr::Call(call_expr) => call_expr.metadata,
            Expr::Emit(emit_expr) => emit_expr.metadata,
            Expr::EnumValue(_enum_value) => Ty::Enum,
            Expr::FieldAccess(field_access_expr) => field_access_expr.metadata,
            Expr::Var(var_expr) => var_expr.metadata.1,
            Expr::FloatLiteral(_float_literal) => Ty::Float,
            Expr::Scope(scope) => match &scope.tail {
                Some(expr) => expr.ty(),
                None => Ty::Nil,
            },
        }
    }
}

impl<'a> AstTransformer<'a> for VarIdTyPass<'a> {
    type Input = ParseMetadata;
    type Output = VarIdTyMetadata;

    fn dispatch_ident(
        &mut self,
        _input: &Ident<'a, Self::Input>,
    ) -> <Self::Output as AstMetadata>::Ident {
    }

    fn dispatch_var_expr(
        &mut self,
        input: &crate::ast::VarExpr<'a, Self::Input>,
        _name: &Ident<'a, Self::Output>,
    ) -> <Self::Output as AstMetadata>::VarExpr {
        self.lookup(input.name.name)
            .expect("used variable before declaration")
    }

    fn dispatch_enum_decl(
        &mut self,
        _input: &crate::ast::EnumDecl<'a, Self::Input>,
        _name: &Ident<'a, Self::Output>,
        _variants: &Vec<Ident<'a, Self::Output>>,
    ) -> <Self::Output as AstMetadata>::EnumDecl {
    }

    fn dispatch_cell_decl(
        &mut self,
        input: &CellDecl<'a, Self::Input>,
        name: &Ident<'a, Self::Output>,
        args: &Vec<ArgDecl<'a, Self::Output>>,
        stmts: &Vec<Statement<'a, Self::Output>>,
    ) -> <Self::Output as AstMetadata>::CellDecl {
    }

    fn dispatch_constant_decl(
        &mut self,
        input: &crate::ast::ConstantDecl<'a, Self::Input>,
        name: &Ident<'a, Self::Output>,
        ty: &Ident<'a, Self::Output>,
        value: &Expr<'a, Self::Output>,
    ) -> <Self::Output as AstMetadata>::ConstantDecl {
    }

    fn dispatch_if_expr(
        &mut self,
        input: &IfExpr<'a, Self::Input>,
        cond: &Expr<'a, Self::Output>,
        then: &Expr<'a, Self::Output>,
        else_: &Expr<'a, Self::Output>,
    ) -> <Self::Output as AstMetadata>::IfExpr {
        let cond_ty = cond.ty();
        let then_ty = then.ty();
        let else_ty = else_.ty();
        assert_eq!(cond_ty, Ty::Bool);
        assert_eq!(then_ty, else_ty);
        then_ty
    }

    fn dispatch_bin_op_expr(
        &mut self,
        input: &BinOpExpr<'a, Self::Input>,
        left: &Expr<'a, Self::Output>,
        right: &Expr<'a, Self::Output>,
    ) -> <Self::Output as AstMetadata>::BinOpExpr {
        let left_ty = left.ty();
        let right_ty = right.ty();
        assert_eq!(left_ty, Ty::Float);
        assert_eq!(right_ty, Ty::Float);
        Ty::Float
    }

    fn dispatch_comparison_expr(
        &mut self,
        input: &ComparisonExpr<'a, Self::Input>,
        left: &Expr<'a, Self::Output>,
        right: &Expr<'a, Self::Output>,
    ) -> <Self::Output as AstMetadata>::ComparisonExpr {
        let left_ty = left.ty();
        let right_ty = right.ty();
        assert_eq!(left_ty, right_ty);
        Ty::Bool
    }

    fn dispatch_field_access_expr(
        &mut self,
        input: &crate::ast::FieldAccessExpr<'a, Self::Input>,
        base: &Expr<'a, Self::Output>,
        field: &Ident<'a, Self::Output>,
    ) -> <Self::Output as AstMetadata>::FieldAccessExpr {
        // TODO: For now, only rects can have their float fields accessed.
        let base_ty = base.ty();
        assert_eq!(base_ty, Ty::Rect);
        assert!(["x0", "x1", "y0", "y1"].contains(&field.name));
        Ty::Float
    }

    fn dispatch_enum_value(
        &mut self,
        input: &EnumValue<'a, Self::Input>,
        name: &Ident<'a, Self::Output>,
        variant: &Ident<'a, Self::Output>,
    ) -> <Self::Output as AstMetadata>::EnumValue {
    }

    fn dispatch_call_expr(
        &mut self,
        input: &crate::ast::CallExpr<'a, Self::Input>,
        func: &Ident<'a, Self::Output>,
        args: &crate::ast::Args<'a, Self::Output>,
    ) -> <Self::Output as AstMetadata>::CallExpr {
        match func.name {
            "rect" => Ty::Rect,
            "float" => {
                assert!(args.posargs.is_empty());
                assert!(args.kwargs.is_empty());
                Ty::Float
            }
            "eq" => {
                assert_eq!(args.posargs.len(), 2);
                assert!(args.kwargs.is_empty());
                assert_eq!(args.posargs[0].ty(), Ty::Float);
                assert_eq!(args.posargs[1].ty(), Ty::Float);
                Ty::Nil
            }
            _ => panic!("invalid function"),
        }
    }

    fn dispatch_emit_expr(
        &mut self,
        input: &crate::ast::EmitExpr<'a, Self::Input>,
        value: &Expr<'a, Self::Output>,
    ) -> <Self::Output as AstMetadata>::EmitExpr {
        let ty = value.ty();
        ty
    }

    fn dispatch_args(
        &mut self,
        input: &crate::ast::Args<'a, Self::Input>,
        posargs: &Vec<Expr<'a, Self::Output>>,
        kwargs: &Vec<crate::ast::KwArgValue<'a, Self::Output>>,
    ) -> <Self::Output as AstMetadata>::Args {
    }

    fn dispatch_kw_arg_value(
        &mut self,
        input: &crate::ast::KwArgValue<'a, Self::Input>,
        name: &Ident<'a, Self::Output>,
        value: &Expr<'a, Self::Output>,
    ) -> <Self::Output as AstMetadata>::KwArgValue {
        value.ty()
    }

    fn dispatch_arg_decl(
        &mut self,
        input: &ArgDecl<'a, Self::Input>,
        name: &Ident<'a, Self::Output>,
        ty: &Typ<'a, Self::Output>,
    ) -> <Self::Output as AstMetadata>::ArgDecl {
    }

    fn transform_arg_decl(
        &mut self,
        input: &ArgDecl<'a, Self::Input>,
    ) -> ArgDecl<'a, Self::Output> {
        assert!(
            self.lookup(input.name.name).is_none(),
            "argument should not already be declared"
        );
        self.alloc(
            input.name.name,
            match &input.ty {
                Typ::Float => Ty::Float,
                Typ::Ident(ident) => {
                    unimplemented!()
                }
            },
        );
        let name = self.transform_ident(&input.name);
        let ty = self.transform_typ(&input.ty);
        ArgDecl {
            name,
            ty,
            metadata: (),
        }
    }

    fn enter_scope(&mut self, _input: &crate::ast::Scope<'a, Self::Input>) {
        self.bindings.push(Default::default());
    }

    fn exit_scope(
        &mut self,
        _input: &crate::ast::Scope<'a, Self::Input>,
        _output: &crate::ast::Scope<'a, Self::Output>,
    ) {
        self.bindings.pop();
    }

    fn dispatch_let_binding(
        &mut self,
        input: &LetBinding<'a, Self::Input>,
        name: &Ident<'a, Self::Output>,
        value: &Expr<'a, Self::Output>,
    ) -> <Self::Output as AstMetadata>::LetBinding {
        self.alloc(name.name, value.ty())
    }
}

#[derive_where(Debug, Clone)]
pub struct CompileInput<'a, T: AstMetadata> {
    pub ast: &'a Ast<'a, T>,
    pub cell: &'a str,
    pub params: HashMap<&'a str, f64>,
}

pub type ScopeId = u64;
pub type VarId = u64;
pub type ConstraintVarId = u64;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SourceInfo {
    pub span: cfgrammar::Span,
    pub id: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Rect<T> {
    pub layer: String,
    pub x0: T,
    pub y0: T,
    pub x1: T,
    pub y1: T,
    pub source: Option<SourceInfo>,
}

type FrameId = u64;
type ValueId = u64;

#[derive(Clone, Default)]
struct Frame {
    bindings: HashMap<VarId, ValueId>,
    parent: Option<FrameId>,
}

struct ExecPass<'a> {
    solver: Solver,
    values: HashMap<ValueId, DeferValue<'a, VarIdTyMetadata>>,
    deferred: HashSet<ValueId>,
    emit: Vec<ValueId>,
    frames: HashMap<FrameId, Frame>,
    nil_value: ValueId,
    true_value: ValueId,
    false_value: ValueId,
    global_frame: FrameId,
    next_id: u64,
}

impl<'a> ExecPass<'a> {
    pub(crate) fn new() -> Self {
        Self {
            solver: Solver::new(),
            values: HashMap::from_iter([
                (1, DeferValue::Ready(Value::None)),
                (2, DeferValue::Ready(Value::Bool(true))),
                (3, DeferValue::Ready(Value::Bool(false))),
            ]),
            deferred: Default::default(),
            frames: HashMap::from_iter([(0, Frame::default())]),
            emit: Default::default(),
            nil_value: 1,
            true_value: 2,
            false_value: 3,
            global_frame: 0,
            next_id: 4,
        }
    }

    pub(crate) fn execute(mut self, input: CompileInput<'a, VarIdTyMetadata>) -> CompiledCell {
        self.execute_start(input);
        self.solve();
        while !self.deferred.is_empty() {
            let deferred = self.deferred.clone();
            for vid in deferred.iter().copied() {
                self.eval_partial(vid);
            }
            if self.deferred == deferred {
                panic!("no progress");
            }
            self.solve();
        }
        CompiledCell {
            values: self.emit(),
        }
    }

    fn emit(&mut self) -> Vec<SolvedValue> {
        self.emit
            .iter()
            .map(|vid| {
                let value = &self.values[vid];
                let value = value.as_ref().unwrap_ready();
                match value {
                    Value::Linear(l) => SolvedValue::Float(
                        l.coeffs
                            .iter()
                            .map(|(coeff, var)| coeff * self.solver.value_of(*var).unwrap())
                            .reduce(|a, b| a + b)
                            .unwrap_or(0.)
                            + l.constant,
                    ),
                    Value::Rect(rect) => SolvedValue::Rect(Rect {
                        layer: rect.layer.clone(),
                        x0: self.solver.value_of(rect.x0).unwrap(),
                        y0: self.solver.value_of(rect.y0).unwrap(),
                        x1: self.solver.value_of(rect.x1).unwrap(),
                        y1: self.solver.value_of(rect.y1).unwrap(),
                        source: rect.source.clone(),
                    }),
                    _ => unimplemented!(),
                }
            })
            .collect()
    }

    fn solve(&mut self) {
        self.solver.solve();
    }

    fn value_id(&mut self) -> ValueId {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    fn execute_start(&mut self, input: CompileInput<'a, VarIdTyMetadata>) {
        let cell = input
            .ast
            .decls
            .iter()
            .find_map(|d| match d {
                Decl::Cell(
                    v @ CellDecl {
                        name: Ident { name, .. },
                        ..
                    },
                ) if *name == input.cell => Some(v),
                _ => None,
            })
            .expect("top cell not found");

        for stmt in cell.stmts.iter() {
            self.eval_stmt(self.global_frame, stmt);
        }
    }

    fn eval_stmt(&mut self, frame: FrameId, stmt: &'a Statement<'_, VarIdTyMetadata>) {
        match stmt {
            Statement::LetBinding(binding) => {
                let value = self.eval_expr(frame, &binding.value);
                println!("binding vid: {} {value:?}", binding.name.name);
                self.frames
                    .get_mut(&frame)
                    .unwrap()
                    .bindings
                    .insert(binding.metadata, value);
            }
            Statement::Expr { value, .. } => {
                self.eval_expr(frame, value);
            }
        }
    }

    fn eval_expr(&mut self, frame: FrameId, expr: &'a Expr<'_, VarIdTyMetadata>) -> ValueId {
        match expr {
            Expr::FloatLiteral(f) => {
                let vid = self.value_id();
                self.values
                    .insert(vid, Defer::Ready(Value::Linear(LinearExpr::from(f.value))));
                vid
            }
            Expr::Emit(e) => {
                let vid = self.eval_expr(frame, &e.value);
                self.emit.push(vid);
                vid
            }
            Expr::Var(v) => {
                let var_id = v.metadata.0;
                self.frames[&frame].bindings[&var_id]
            }
            Expr::Call(c) => match c.func.name {
                "rect" => todo!(),
                "float" => {
                    let vid = self.value_id();
                    println!("float vid: {vid:?}");
                    self.values.insert(
                        vid,
                        Defer::Ready(Value::Linear(LinearExpr::from(self.solver.new_var()))),
                    );
                    vid
                }
                "eq" => {
                    assert_eq!(c.args.posargs.len(), 2);
                    assert_eq!(c.args.kwargs.len(), 0);
                    let left = self.eval_expr(frame, &c.args.posargs[0]);
                    let right = self.eval_expr(frame, &c.args.posargs[1]);
                    if let (Defer::Ready(vl), Defer::Ready(vr)) =
                        (&self.values[&left], &self.values[&right])
                    {
                        let expr = vl.as_ref().unwrap_linear().clone()
                            - vr.as_ref().unwrap_linear().clone();
                        self.solver.constrain_eq0(expr);
                        return self.nil_value;
                    }
                    panic!("unsolved argument");
                }
                _ => panic!("invalid function"),
            },
            Expr::If(if_expr) => {
                let cond = self.eval_expr(frame, &if_expr.cond);
                match &self.values[&cond] {
                    Defer::Ready(v) => {
                        if *v.as_ref().unwrap_bool() {
                            self.eval_expr(frame, &if_expr.then)
                        } else {
                            self.eval_expr(frame, &if_expr.else_)
                        }
                    }
                    Defer::Deferred(_) => {
                        // defer
                        let vid = self.value_id();
                        self.deferred.insert(vid);
                        self.values.insert(
                            vid,
                            DeferValue::Deferred(PartialEval {
                                state: PartialEvalState::If(Box::new(PartialIfExpr {
                                    expr: if_expr,
                                    state: IfExprState::Cond(cond),
                                })),
                                assign_to: None,
                                frame,
                            }),
                        );
                        vid
                    }
                }
            }
            Expr::Comparison(comparison_expr) => {
                let left = self.eval_expr(frame, &comparison_expr.left);
                let right = self.eval_expr(frame, &comparison_expr.right);
                if let (Defer::Ready(vl), Defer::Ready(vr)) =
                    (&self.values[&left], &self.values[&right])
                {
                    let lin_vl = vl.as_ref().unwrap_linear();
                    let lin_vr = vr.as_ref().unwrap_linear();
                    if let (Some(vl), Some(vr)) =
                        (self.solver.eval_expr(lin_vl), self.solver.eval_expr(lin_vr))
                    {
                        let res = match comparison_expr.op {
                            crate::ast::ComparisonOp::Eq => {
                                panic!("cannot check equality between floats")
                            }
                            crate::ast::ComparisonOp::Ne => {
                                panic!("cannot check inequality between floats")
                            }
                            crate::ast::ComparisonOp::Geq => vl >= vr,
                            crate::ast::ComparisonOp::Gt => vl > vr,
                            crate::ast::ComparisonOp::Leq => vl <= vr,
                            crate::ast::ComparisonOp::Lt => vl < vr,
                        };
                        return if res {
                            self.true_value
                        } else {
                            self.false_value
                        };
                    }
                    // use solver .solved vars to see if linvlr cud b evaled to a float
                    // if yes => return value bool
                    // else => defer
                }
                let vid = self.value_id();
                self.deferred.insert(vid);
                self.values.insert(
                    vid,
                    DeferValue::Deferred(PartialEval {
                        state: PartialEvalState::Comparison(Box::new(PartialComparisonExpr {
                            expr: comparison_expr,
                            state: ComparisonExprState { left, right },
                        })),
                        assign_to: None,
                        frame,
                    }),
                );
                vid
            }
            Expr::Scope(s) => {
                for stmt in &s.stmts {
                    self.eval_stmt(frame, stmt);
                }
                s.tail
                    .as_ref()
                    .map(|tail| self.eval_expr(frame, tail))
                    .unwrap_or(self.nil_value)
            }
            x => todo!("{x:?}"),
        }
    }

    fn eval_partial(&mut self, vid: ValueId) {
        let v = self.values.remove(&vid);
        if v.is_none() {
            return;
        }
        let mut v = v.unwrap();
        let vref = v.as_mut();
        if vref.is_ready() {
            self.values.insert(vid, v);
            return;
        }
        let vref = vref.unwrap_deferred();
        match &mut vref.state {
            PartialEvalState::If(if_) => {
                match if_.state {
                    IfExprState::Cond(cond) => {
                        self.eval_partial(cond);
                        match &self.values[&cond] {
                            Defer::Ready(val) => {
                                if *val.as_ref().unwrap_bool() {
                                    let then = self.eval_expr(vref.frame, &if_.expr.then);
                                    match &self.values[&then] {
                                        Defer::Ready(v) => {
                                            self.values.insert(vid, Defer::Ready(v.clone()));
                                        }
                                        Defer::Deferred(_) => {
                                            if_.state = IfExprState::Then(then);
                                        }
                                    };
                                } else {
                                    let else_ = self.eval_expr(vref.frame, &if_.expr.else_);
                                    match &self.values[&else_] {
                                        Defer::Ready(v) => {
                                            self.values.insert(vid, Defer::Ready(v.clone()));
                                        }
                                        Defer::Deferred(_) => {
                                            if_.state = IfExprState::Else(else_);
                                        }
                                    };
                                }
                            }
                            Defer::Deferred(_) => {
                                // nothing to do
                            }
                        }
                    }
                    IfExprState::Then(then) => {
                        self.eval_partial(then);
                        match &self.values[&then] {
                            Defer::Ready(val) => {
                                self.values.insert(vid, Defer::Ready(val.clone()));
                            }
                            Defer::Deferred(_) => {
                                // nothing to do
                            }
                        }
                    }
                    IfExprState::Else(else_) => {
                        self.eval_partial(else_);
                        match &self.values[&else_] {
                            Defer::Ready(val) => {
                                self.values.insert(vid, Defer::Ready(val.clone()));
                            }
                            Defer::Deferred(_) => {
                                // nothing to do
                            }
                        }
                    }
                }
            }
            PartialEvalState::Comparison(comparison_expr) => {
                self.eval_partial(comparison_expr.state.left);
                self.eval_partial(comparison_expr.state.right);
                println!(
                    "left vid {:?}, right vid {:?}",
                    comparison_expr.state.left, comparison_expr.state.right
                );
                if let (Defer::Ready(vl), Defer::Ready(vr)) = (
                    &self.values[&comparison_expr.state.left],
                    &self.values[&comparison_expr.state.right],
                ) {
                    let lin_vl = vl.as_ref().unwrap_linear();
                    let lin_vr = vr.as_ref().unwrap_linear();
                    if let (Some(vl), Some(vr)) =
                        (self.solver.eval_expr(lin_vl), self.solver.eval_expr(lin_vr))
                    {
                        let res = match comparison_expr.expr.op {
                            crate::ast::ComparisonOp::Eq => {
                                panic!("cannot check equality between floats")
                            }
                            crate::ast::ComparisonOp::Ne => {
                                panic!("cannot check inequality between floats")
                            }
                            crate::ast::ComparisonOp::Geq => vl >= vr,
                            crate::ast::ComparisonOp::Gt => vl > vr,
                            crate::ast::ComparisonOp::Leq => vl <= vr,
                            crate::ast::ComparisonOp::Lt => vl < vr,
                        };
                        self.values.insert(vid, DeferValue::Ready(Value::Bool(res)));
                    }
                }
            }
            _ => todo!(),
        }

        if !self.values.contains_key(&vid) {
            self.values.insert(vid, v);
        }
        if self.values[&vid].is_ready() {
            self.deferred.remove(&vid);
        }
    }
}

#[enumify]
#[derive(Debug, Clone)]
pub enum Value<'a> {
    EnumValue(EnumValue<'a, VarIdTyMetadata>),
    Linear(LinearExpr),
    Rect(Rect<Var>),
    Bool(bool),
    None,
}

#[derive(Debug, Clone)]
pub enum SolvedValue {
    Float(f64),
    Rect(Rect<f64>),
}

#[derive(Debug, Clone)]
pub struct CompiledCell {
    values: Vec<SolvedValue>,
}

#[enumify(generics_only)]
enum Defer<R, D> {
    Ready(R),
    Deferred(D),
}

type DeferValue<'a, T: AstMetadata> = Defer<Value<'a>, PartialEval<'a, T>>;

struct PartialEval<'a, T: AstMetadata> {
    state: PartialEvalState<'a, T>,
    assign_to: Option<ValueId>,
    frame: FrameId,
}

#[derive(Clone)]
struct ProgressPredicate {
    // sum of products
    terms: Vec<Vec<ConstraintVarId>>,
}

enum PartialEvalState<'a, T: AstMetadata> {
    If(Box<PartialIfExpr<'a, T>>),
    Comparison(Box<PartialComparisonExpr<'a, T>>),
    BinOp(Box<BinOpExpr<'a, T>>),
    Call(Box<PartialCallExpr<'a, T>>),
    Emit(Box<EmitExpr<'a, T>>),
    EnumValue(EnumValue<'a, T>),
    FieldAccess(Box<FieldAccessExpr<'a, T>>),
    Var(Ident<'a, T>),
    FloatLiteral(FloatLiteral),
}

struct PartialIfExpr<'a, T: AstMetadata> {
    expr: &'a IfExpr<'a, T>,
    state: IfExprState,
}

pub enum IfExprState {
    Cond(ValueId),
    Then(ValueId),
    Else(ValueId),
}

struct PartialCallExpr<'a, T: AstMetadata> {
    expr: &'a CallExpr<'a, T>,
    state: CallExprState,
}

pub struct CallExprState {
    posargs: Vec<ValueId>,
    kwargs: Vec<ValueId>,
}

pub struct BinOpExprState<'a, T: AstMetadata> {
    left: PartialEval<'a, T>,
    right: PartialEval<'a, T>,
}

struct PartialComparisonExpr<'a, T: AstMetadata> {
    expr: &'a ComparisonExpr<'a, T>,
    state: ComparisonExprState,
}

pub struct ComparisonExprState {
    left: ValueId,
    right: ValueId,
}

// impl<'a> Scope<'a> {
//     fn lookup(&self, var: VarId) -> Option<&VarBinding<'a>> {
//         self.bindings.get(&var).or_else(|| self.parent.lookup(var))
//     }
// }
//
// struct CellCtx<'a> {
//     cell: Cell,
//     bindings: HashMap<&'a str, Value<'a>>,
//     next_id: u64,
// }
//
// impl<'a> CellCtx<'a> {
//     pub fn new() -> Self {
//         Self {
//             cell: Cell::new(),
//             bindings: HashMap::new(),
//             next_id: 0,
//         }
//     }
//
//     fn alloc_id(&mut self) -> u64 {
//         self.next_id = self.next_id.checked_add(1).unwrap();
//         self.next_id
//     }
//
//     fn compile(mut self, input: CompileInput<'a>) -> Result<CompiledCell> {
//         let cell = input
//             .ast
//             .decls
//             .iter()
//             .find_map(|d| match d {
//                 Decl::Cell(
//                     v @ CellDecl {
//                         name: Ident { name, .. },
//                         ..
//                     },
//                 ) if *name == input.cell => Some(v),
//                 _ => None,
//             })
//             .ok_or_else(|| anyhow!("no cell named `{}`", input.cell))?;
//         for (name, value) in input.params {
//             self.bindings.insert(
//                 name,
//                 Value::Linear(LinearExpr {
//                     coeffs: Vec::new(),
//                     constant: value,
//                 }),
//             );
//         }
//         for stmt in cell.stmts.iter() {
//             match stmt {
//                 Statement::Expr(expr) => {
//                     self.eval(expr)?;
//                 }
//                 Statement::LetBinding { name, value } => {
//                     let value = self.eval(value)?;
//                     self.bindings.insert(name.name, value);
//                 }
//             }
//         }
//         self.cell.solve()
//     }
//
//     fn eval(&mut self, expr: &Expr<'a>) -> Result<Value<'a>> {
//         match expr {
//             Expr::BinOp(expr) => {
//                 let left = self.eval(&expr.left)?.try_linear(expr.left.span())?;
//                 let right = self.eval(&expr.right)?.try_linear(expr.right.span())?;
//                 match expr.op {
//                     BinOp::Add => Ok(Value::Linear(left + right)),
//                     BinOp::Sub => Ok(Value::Linear(left - right)),
//                     op => bail!(
//                         "unsupported binary operator: {op:?} in expression at {:?}",
//                         expr.span
//                     ),
//                 }
//             }
//             Expr::Call(expr) => match expr.func.name {
//                 "Rect" => {
//                     assert_eq!(expr.args.posargs.len(), 1);
//                     let layer = self
//                         .eval(&expr.args.posargs[0])?
//                         .try_enum_value(expr.args.posargs[0].span())?;
//                     let attrs = Attrs {
//                         source: Some(SourceInfo {
//                             span: expr.span,
//                             id: self.alloc_id(),
//                         }),
//                     };
//                     let rect = self.cell.physical_rect(layer.variant.name.into(), attrs);
//                     for arg in expr.args.kwargs.iter() {
//                         let value = self.eval(&arg.value)?;
//                         match arg.name.name {
//                             "x0" => {
//                                 let mut value = value.try_linear(arg.span)?;
//                                 value.coeffs.push((-1., rect.x0));
//                                 self.cell.add_constraint(Constraint::Linear(
//                                     value.into_eq_constraint(ConstraintAttrs {
//                                         span: Some(arg.span),
//                                     }),
//                                 ));
//                             }
//                             "x1" => {
//                                 let mut value = value.try_linear(arg.span)?;
//                                 value.coeffs.push((-1., rect.x1));
//                                 self.cell.add_constraint(Constraint::Linear(
//                                     value.into_eq_constraint(ConstraintAttrs {
//                                         span: Some(arg.span),
//                                     }),
//                                 ));
//                             }
//                             "y0" => {
//                                 let mut value = value.try_linear(arg.span)?;
//                                 value.coeffs.push((-1., rect.y0));
//                                 self.cell.add_constraint(Constraint::Linear(
//                                     value.into_eq_constraint(ConstraintAttrs {
//                                         span: Some(arg.span),
//                                     }),
//                                 ));
//                             }
//                             "y1" => {
//                                 let mut value = value.try_linear(arg.span)?;
//                                 value.coeffs.push((-1., rect.y1));
//                                 self.cell.add_constraint(Constraint::Linear(
//                                     value.into_eq_constraint(ConstraintAttrs {
//                                         span: Some(arg.span),
//                                     }),
//                                 ));
//                             }
//                             arg_name => {
//                                 bail!("unexpected argument: `{arg_name}` at {:?}", arg.name.span)
//                             }
//                         }
//                     }
//                     Ok(Value::Rect(rect))
//                 }
//                 "eq" => {
//                     assert_eq!(expr.args.posargs.len(), 2);
//                     let lhs = self
//                         .eval(&expr.args.posargs[0])?
//                         .try_linear(expr.args.posargs[0].span())?;
//                     let rhs = self
//                         .eval(&expr.args.posargs[1])?
//                         .try_linear(expr.args.posargs[0].span())?;
//                     self.cell
//                         .add_constraint(Constraint::Linear((lhs - rhs).into_eq_constraint(
//                             ConstraintAttrs {
//                                 span: Some(expr.span),
//                             },
//                         )));
//                     Ok(Value::None)
//                 }
//                 f => bail!("unexpected draw call `{f}` at {:?}", expr.span),
//             },
//             Expr::FloatLiteral(v) => Ok(Value::Linear(LinearExpr {
//                 constant: v.value,
//                 coeffs: Vec::new(),
//             })),
//             Expr::Var(v) => Ok(self
//                 .bindings
//                 .get(v.name)
//                 .ok_or_else(|| anyhow!("no variable named `{}`", v.name))?
//                 .clone()),
//             Expr::FieldAccess(expr) => {
//                 let base = self.eval(&expr.base)?;
//                 match base {
//                     Value::Rect(r) => Ok(Value::Linear(LinearExpr::from(match expr.field.name {
//                         "x0" => r.x0,
//                         "x1" => r.x1,
//                         "y0" => r.y0,
//                         "y1" => r.y1,
//                         f => bail!(
//                             "type Rect has no field `{f}` (encountered at {:?})",
//                             expr.field.span
//                         ),
//                     }))),
//                     _ => bail!(
//                         "object no field `{}` (encountered at {:?})",
//                         expr.field.name,
//                         expr.field.span
//                     ),
//                 }
//             }
//             Expr::EnumValue(v) => Ok(Value::EnumValue(v.clone())),
//             Expr::Emit(v) => {
//                 let value = self.eval(&v.value)?;
//                 let rect = value.try_rect(v.span)?;
//                 self.cell.emit_rect(rect.clone());
//                 Ok(Value::Rect(rect))
//             }
//             expr => bail!("cannot evaluate the expression at {:?}", expr.span()),
//         }
//     }
// }
//
// impl<'a> Value<'a> {
//     pub fn try_enum_value(self, espan: cfgrammar::Span) -> Result<EnumValue<'a>> {
//         self.into_enum_value()
//             .ok_or_else(|| anyhow!("expected value to be of type EnumValue at {espan:?}"))
//     }
//     pub fn try_linear(self, espan: cfgrammar::Span) -> Result<LinearExpr> {
//         self.into_linear()
//             .ok_or_else(|| anyhow!("expected value to be of type LinearExpr at {espan:?}"))
//     }
//     pub fn try_rect(self, espan: cfgrammar::Span) -> Result<Rect<Var>> {
//         self.into_rect()
//             .ok_or_else(|| anyhow!("expected value to be of type Rect at {espan:?}"))
//     }
// }
//
// pub fn compile(input: CompileInput) -> Result<SolvedCell> {
//     let ctx = CellCtx::new();
//     ctx.compile(input)
// }
//
