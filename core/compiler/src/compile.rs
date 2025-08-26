//! # Argon compiler
//!
//! Pass 1: assign variable IDs/type checking
//! Pass 3: solving
use std::collections::HashMap;

use derive_where::derive_where;
use enumify::enumify;
use indexmap::IndexSet;
use itertools::Itertools;
use serde::{Deserialize, Serialize};

use crate::ast::{BinOp, ConstantDecl, FieldAccessExpr, FnDecl, Scope, UnaryOp};
use crate::{
    ast::{
        ArgDecl, Ast, AstMetadata, AstTransformer, BinOpExpr, CallExpr, CellDecl, ComparisonExpr,
        Decl, EnumValue, Expr, Ident, IfExpr, LetBinding, Statement,
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

#[derive(Debug, Clone)]
pub struct VarIdTyMetadata;

#[enumify]
#[derive(Debug, Clone, Hash, PartialEq, Eq)]
pub enum Ty {
    Bool,
    Float,
    Int,
    Rect,
    Enum,
    Nil,
    Fn(Box<FnTy>),
}

impl Ty {
    pub fn from_name(name: &str) -> Self {
        match name {
            "Float" => Ty::Float,
            "Rect" => Ty::Rect,
            "Int" => Ty::Int,
            name => panic!("invalid type: {name}"),
        }
    }
}

#[derive(Debug, Clone, Hash, PartialEq, Eq)]
pub struct FnTy {
    args: Vec<Ty>,
    ret: Ty,
}

impl AstMetadata for VarIdTyMetadata {
    type Ident = ();
    type EnumDecl = ();
    type StructDecl = ();
    type StructField = ();
    type CellDecl = ();
    type ConstantDecl = ();
    type LetBinding = VarId;
    type FnDecl = VarId;
    type IfExpr = Ty;
    type BinOpExpr = Ty;
    type UnaryOpExpr = Ty;
    type ComparisonExpr = Ty;
    type FieldAccessExpr = Ty;
    type EnumValue = ();
    type CallExpr = (Option<VarId>, Ty);
    type EmitExpr = Ty;
    type Args = ();
    type KwArgValue = Ty;
    type ArgDecl = (VarId, Ty);
    type Scope = Ty;
    type Typ = ();
    type VarExpr = (VarId, Ty);
    type CastExpr = Ty;
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
                return Some(info.clone());
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
        let mut decls = Vec::new();
        for decl in &input.ast.decls {
            if let Decl::Fn(f) = decl {
                decls.push(Decl::Fn(self.transform_fn_decl(f)));
            }
        }
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

        decls.push(Decl::Cell(CellDecl {
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
        }));

        Ast { decls }
    }
}

impl<'a> Expr<'a, VarIdTyMetadata> {
    fn ty(&self) -> Ty {
        match self {
            Expr::If(if_expr) => if_expr.metadata.clone(),
            Expr::Comparison(comparison_expr) => comparison_expr.metadata.clone(),
            Expr::BinOp(bin_op_expr) => bin_op_expr.metadata.clone(),
            Expr::Call(call_expr) => call_expr.metadata.1.clone(),
            Expr::Emit(emit_expr) => emit_expr.metadata.clone(),
            Expr::EnumValue(_enum_value) => Ty::Enum,
            Expr::FieldAccess(field_access_expr) => field_access_expr.metadata.clone(),
            Expr::Var(var_expr) => var_expr.metadata.1.clone(),
            Expr::FloatLiteral(_float_literal) => Ty::Float,
            Expr::IntLiteral(_int_literal) => Ty::Int,
            Expr::Scope(scope) => scope.metadata.clone(),
            Expr::Cast(cast) => cast.metadata.clone(),
            Expr::UnaryOp(unary_op_expr) => unary_op_expr.metadata.clone(),
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
        _variants: &[Ident<'a, Self::Output>],
    ) -> <Self::Output as AstMetadata>::EnumDecl {
    }

    fn dispatch_cell_decl(
        &mut self,
        _input: &CellDecl<'a, Self::Input>,
        _name: &Ident<'a, Self::Output>,
        _args: &[ArgDecl<'a, Self::Output>],
        _stmts: &[Statement<'a, Self::Output>],
    ) -> <Self::Output as AstMetadata>::CellDecl {
        // TODO: Argument checks
    }

    fn dispatch_fn_decl(
        &mut self,
        _input: &FnDecl<'a, Self::Input>,
        name: &Ident<'a, Self::Output>,
        _args: &[ArgDecl<'a, Self::Output>],
        _return_ty: &Ident<'a, Self::Output>,
        _scope: &Scope<'a, Self::Output>,
    ) -> <Self::Output as AstMetadata>::FnDecl {
        // UNUSED
        self.lookup(name.name).unwrap().0
    }

    fn transform_fn_decl(&mut self, input: &FnDecl<'a, Self::Input>) -> FnDecl<'a, Self::Output> {
        assert!(!["crect", "rect", "float"].contains(&input.name.name));
        let args: Vec<_> = input
            .args
            .iter()
            .map(|arg| self.transform_arg_decl(arg))
            .collect();
        let ty = Ty::Fn(Box::new(FnTy {
            args: args.iter().map(|arg| arg.metadata.1.clone()).collect(),
            ret: Ty::from_name(input.return_ty.name),
        }));
        let vid = self.alloc(input.name.name, ty);
        let name = self.transform_ident(&input.name);
        let return_ty = self.transform_ident(&input.return_ty);
        let scope = self.transform_scope(&input.scope);
        FnDecl {
            name,
            args,
            return_ty,
            scope,
            metadata: vid,
        }
    }

    fn dispatch_constant_decl(
        &mut self,
        _input: &ConstantDecl<'a, Self::Input>,
        _name: &Ident<'a, Self::Output>,
        _ty: &Ident<'a, Self::Output>,
        _value: &Expr<'a, Self::Output>,
    ) -> <Self::Output as AstMetadata>::ConstantDecl {
    }

    fn dispatch_if_expr(
        &mut self,
        _input: &IfExpr<'a, Self::Input>,
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
        _input: &BinOpExpr<'a, Self::Input>,
        left: &Expr<'a, Self::Output>,
        right: &Expr<'a, Self::Output>,
    ) -> <Self::Output as AstMetadata>::BinOpExpr {
        let left_ty = left.ty();
        let right_ty = right.ty();
        assert_eq!(left_ty, right_ty);
        assert!([Ty::Float, Ty::Int].contains(&left_ty));
        assert!([Ty::Float, Ty::Int].contains(&right_ty));
        left_ty
    }

    fn dispatch_unary_op_expr(
        &mut self,
        input: &crate::ast::UnaryOpExpr<'a, Self::Input>,
        operand: &Expr<'a, Self::Output>,
    ) -> <Self::Output as AstMetadata>::UnaryOpExpr {
        match input.op {
            UnaryOp::Not => {
                assert_eq!(operand.ty(), Ty::Bool);
                Ty::Bool
            }
            UnaryOp::Neg => {
                let operand_ty = operand.ty();
                assert!([Ty::Float, Ty::Int].contains(&operand_ty));
                operand_ty
            }
        }
    }

    fn dispatch_comparison_expr(
        &mut self,
        _input: &ComparisonExpr<'a, Self::Input>,
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
        _input: &crate::ast::FieldAccessExpr<'a, Self::Input>,
        base: &Expr<'a, Self::Output>,
        field: &Ident<'a, Self::Output>,
    ) -> <Self::Output as AstMetadata>::FieldAccessExpr {
        // TODO: For now, only rects can have their float fields accessed.
        let base_ty = base.ty();
        assert_eq!(base_ty, Ty::Rect);
        match field.name {
            "x0" | "x1" | "y0" | "y1" | "w" | "h" => Ty::Float,
            "layer" => Ty::Enum,
            _ => panic!("invalid field access"),
        }
    }

    fn dispatch_enum_value(
        &mut self,
        _input: &EnumValue<'a, Self::Input>,
        _name: &Ident<'a, Self::Output>,
        _variant: &Ident<'a, Self::Output>,
    ) -> <Self::Output as AstMetadata>::EnumValue {
    }

    fn dispatch_call_expr(
        &mut self,
        _input: &crate::ast::CallExpr<'a, Self::Input>,
        func: &Ident<'a, Self::Output>,
        args: &crate::ast::Args<'a, Self::Output>,
    ) -> <Self::Output as AstMetadata>::CallExpr {
        match func.name {
            "crect" | "rect" => {
                if func.name == "crect" {
                    assert_eq!(args.posargs.len(), 0);
                } else {
                    assert_eq!(args.posargs.len(), 1);
                    assert_eq!(args.posargs[0].ty(), Ty::Enum);
                }
                for kwarg in &args.kwargs {
                    assert!(["x0", "x1", "y0", "y1", "w", "h"].contains(&kwarg.name.name));
                    assert_eq!(kwarg.value.ty(), Ty::Float);
                }
                (None, Ty::Rect)
            }
            "float" => {
                assert!(args.posargs.is_empty());
                assert!(args.kwargs.is_empty());
                (None, Ty::Float)
            }
            "eq" => {
                assert_eq!(args.posargs.len(), 2);
                assert!(args.kwargs.is_empty());
                assert_eq!(args.posargs[0].ty(), Ty::Float);
                assert_eq!(args.posargs[1].ty(), Ty::Float);
                (None, Ty::Nil)
            }
            name => {
                let (varid, ty) = self
                    .lookup(name)
                    .unwrap_or_else(|| panic!("no function named `{name}`"));
                let ty = ty.unwrap_fn();
                assert_eq!(args.posargs.len(), ty.args.len());
                for (arg, arg_ty) in args.posargs.iter().zip(&ty.args) {
                    assert_eq!(&arg.ty(), arg_ty);
                }
                assert!(args.kwargs.is_empty());
                (Some(varid), ty.ret.clone())
            }
        }
    }

    fn dispatch_emit_expr(
        &mut self,
        _input: &crate::ast::EmitExpr<'a, Self::Input>,
        value: &Expr<'a, Self::Output>,
    ) -> <Self::Output as AstMetadata>::EmitExpr {
        value.ty()
    }

    fn dispatch_args(
        &mut self,
        _input: &crate::ast::Args<'a, Self::Input>,
        _posargs: &[Expr<'a, Self::Output>],
        _kwargs: &[crate::ast::KwArgValue<'a, Self::Output>],
    ) -> <Self::Output as AstMetadata>::Args {
    }

    fn dispatch_cast(
        &mut self,
        _input: &crate::ast::CastExpr<'a, Self::Input>,
        _value: &Expr<'a, Self::Output>,
        ty: &Ident<'a, Self::Output>,
    ) -> <Self::Output as AstMetadata>::CastExpr {
        Ty::from_name(ty.name)
    }

    fn dispatch_kw_arg_value(
        &mut self,
        _input: &crate::ast::KwArgValue<'a, Self::Input>,
        _name: &Ident<'a, Self::Output>,
        value: &Expr<'a, Self::Output>,
    ) -> <Self::Output as AstMetadata>::KwArgValue {
        value.ty()
    }

    fn dispatch_arg_decl(
        &mut self,
        input: &ArgDecl<'a, Self::Input>,
        _name: &Ident<'a, Self::Output>,
        _ty: &Ident<'a, Self::Output>,
    ) -> <Self::Output as AstMetadata>::ArgDecl {
        let ty = Ty::from_name(input.ty.name);
        (self.alloc(input.name.name, ty.clone()), ty)
    }

    fn dispatch_scope(
        &mut self,
        _input: &Scope<'a, Self::Input>,
        _stmts: &[Statement<'a, Self::Output>],
        tail: &Option<Expr<'a, Self::Output>>,
    ) -> <Self::Output as AstMetadata>::Scope {
        tail.as_ref().map(|tail| tail.ty()).unwrap_or(Ty::Nil)
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
        _input: &LetBinding<'a, Self::Input>,
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
    pub layer: Option<String>,
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
    deferred: IndexSet<ValueId>,
    emit: Vec<ValueId>,
    frames: HashMap<FrameId, Frame>,
    nil_value: ValueId,
    global_frame: FrameId,
    next_id: u64,
    solve_iters: u64,
}

impl<'a> ExecPass<'a> {
    pub(crate) fn new() -> Self {
        Self {
            solver: Solver::new(),
            values: HashMap::from_iter([(1, DeferValue::Ready(Value::None))]),
            deferred: Default::default(),
            frames: HashMap::from_iter([(0, Frame::default())]),
            emit: Default::default(),
            nil_value: 1,
            global_frame: 0,
            next_id: 2,
            solve_iters: 0,
        }
    }

    pub(crate) fn lookup(&self, frame: FrameId, var: VarId) -> Option<ValueId> {
        let frame = self.frames.get(&frame).expect("no frame found");
        if let Some(val) = frame.bindings.get(&var) {
            Some(*val)
        } else {
            frame.parent.and_then(|frame| self.lookup(frame, var))
        }
    }

    pub(crate) fn execute(mut self, input: CompileInput<'a, VarIdTyMetadata>) -> CompiledCell {
        self.execute_start(input);
        let mut require_progress = false;
        let mut progress = false;
        while !self.deferred.is_empty() {
            let deferred = self.deferred.clone();
            progress = false;
            for vid in deferred.iter().copied() {
                progress = progress || self.eval_partial(vid);
            }

            if require_progress && !progress {
                panic!("no progress");
            }

            require_progress = false;

            if !progress {
                self.solve();
                require_progress = true;
            }
        }
        if progress {
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
                    Value::Linear(l) => SolvedValue::Float(self.solver.eval_expr(l).unwrap()),
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
        self.solve_iters += 1;
        self.solver.solve();
    }

    fn value_id(&mut self) -> ValueId {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    fn frame_id(&mut self) -> FrameId {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    fn alloc_id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    fn execute_start(&mut self, input: CompileInput<'a, VarIdTyMetadata>) {
        for decl in &input.ast.decls {
            if let Decl::Fn(f) = decl {
                let vid = self.value_id();
                self.values
                    .insert(vid, DeferValue::Ready(Value::Fn(f.clone())));
                self.frames
                    .get_mut(&self.global_frame)
                    .unwrap()
                    .bindings
                    .insert(f.metadata, vid);
            }
        }
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

    fn eval_stmt(&mut self, frame: FrameId, stmt: &Statement<'a, VarIdTyMetadata>) {
        match stmt {
            Statement::LetBinding(binding) => {
                let value = self.visit_expr(frame, &binding.value);
                self.frames
                    .get_mut(&frame)
                    .unwrap()
                    .bindings
                    .insert(binding.metadata, value);
            }
            Statement::Expr { value, .. } => {
                self.visit_expr(frame, value);
            }
        }
    }

    fn visit_expr(&mut self, frame: FrameId, expr: &Expr<'a, VarIdTyMetadata>) -> ValueId {
        let partial_eval_state = match expr {
            Expr::FloatLiteral(f) => {
                let vid = self.value_id();
                self.values
                    .insert(vid, Defer::Ready(Value::Linear(LinearExpr::from(f.value))));
                return vid;
            }
            Expr::IntLiteral(i) => {
                let vid = self.value_id();
                self.values.insert(vid, Defer::Ready(Value::Int(i.value)));
                return vid;
            }
            Expr::Var(v) => {
                let var_id = v.metadata.0;
                return self.frames[&frame].bindings[&var_id];
            }
            Expr::Emit(e) => {
                let vid = self.visit_expr(frame, &e.value);
                self.emit.push(vid);
                return vid;
            }
            Expr::Call(c) => {
                if ["rect", "crect", "float", "eq"].contains(&c.func.name) {
                    PartialEvalState::Call(Box::new(PartialCallExpr {
                        expr: c.clone(),
                        state: CallExprState {
                            posargs: c
                                .args
                                .posargs
                                .iter()
                                .map(|arg| self.visit_expr(frame, arg))
                                .collect(),
                            kwargs: c
                                .args
                                .kwargs
                                .iter()
                                .map(|arg| self.visit_expr(frame, &arg.value))
                                .collect(),
                        },
                    }))
                } else {
                    let arg_vals = c
                        .args
                        .posargs
                        .iter()
                        .map(|arg| self.visit_expr(frame, arg))
                        .collect_vec();
                    let val = &self.values[&self.lookup(frame, c.metadata.0.unwrap()).unwrap()]
                        .as_ref()
                        .unwrap_ready()
                        .as_ref()
                        .unwrap_fn();
                    let mut call_frame = Frame {
                        bindings: Default::default(),
                        parent: Some(self.global_frame),
                    };
                    for (arg_val, arg_decl) in arg_vals.iter().zip(&val.args) {
                        call_frame.bindings.insert(arg_decl.metadata.0, *arg_val);
                    }
                    let scope = val.scope.clone();
                    let fid = self.frame_id();
                    self.frames.insert(fid, call_frame);
                    return self.visit_expr(fid, &Expr::Scope(Box::new(scope)));
                }
            }
            Expr::If(if_expr) => {
                let cond = self.visit_expr(frame, &if_expr.cond);
                PartialEvalState::If(Box::new(PartialIfExpr {
                    expr: (**if_expr).clone(),
                    state: IfExprState::Cond(cond),
                }))
            }
            Expr::Comparison(comparison_expr) => {
                let left = self.visit_expr(frame, &comparison_expr.left);
                let right = self.visit_expr(frame, &comparison_expr.right);
                PartialEvalState::Comparison(Box::new(PartialComparisonExpr {
                    expr: (**comparison_expr).clone(),
                    state: ComparisonExprState { left, right },
                }))
            }
            Expr::Scope(s) => {
                for stmt in &s.stmts {
                    self.eval_stmt(frame, stmt);
                }
                return s
                    .tail
                    .as_ref()
                    .map(|tail| self.visit_expr(frame, tail))
                    .unwrap_or(self.nil_value);
            }
            Expr::EnumValue(e) => {
                let vid = self.value_id();
                self.values.insert(
                    vid,
                    Defer::Ready(Value::EnumValue(e.variant.name.to_string())),
                );
                return vid;
            }
            Expr::FieldAccess(f) => {
                let base = self.visit_expr(frame, &f.base);
                PartialEvalState::FieldAccess(Box::new(PartialFieldAccessExpr {
                    expr: (**f).clone(),
                    state: FieldAccessExprState { base },
                }))
            }
            Expr::BinOp(b) => {
                let lhs = self.visit_expr(frame, &b.left);
                let rhs = self.visit_expr(frame, &b.right);
                PartialEvalState::BinOp(PartialBinOp { lhs, rhs, op: b.op })
            }
            Expr::Cast(cast) => {
                let value = self.visit_expr(frame, &cast.value);
                PartialEvalState::Cast(PartialCast {
                    value,
                    ty: cast.metadata.clone(),
                })
            }
            x => todo!("{x:?}"),
        };
        let vid = self.value_id();
        self.deferred.insert(vid);
        self.values.insert(
            vid,
            DeferValue::Deferred(PartialEval {
                state: partial_eval_state,
                frame,
            }),
        );
        vid
    }

    fn eval_partial(&mut self, vid: ValueId) -> bool {
        let v = self.values.remove(&vid);
        if v.is_none() {
            return false;
        }
        let mut v = v.unwrap();
        let vref = v.as_mut();
        if vref.is_ready() {
            self.values.insert(vid, v);
            return false;
        }
        let vref = vref.unwrap_deferred();
        let progress = match &mut vref.state {
            PartialEvalState::Call(c) => match c.expr.func.name {
                "crect" | "rect" => {
                    let layer = c.state.posargs.first().map(|vid| {
                        self.values[vid]
                            .as_ref()
                            .get_ready()
                            .map(|layer| layer.as_ref().unwrap_enum_value().clone())
                    });
                    let layer = match layer {
                        None => Some(None),
                        Some(None) => None,
                        Some(Some(l)) => Some(Some(l)),
                    };
                    if let Some(layer) = layer {
                        let rect = Rect {
                            layer,
                            x0: self.solver.new_var(),
                            y0: self.solver.new_var(),
                            x1: self.solver.new_var(),
                            y1: self.solver.new_var(),
                            source: Some(SourceInfo {
                                span: c.expr.span,
                                id: self.alloc_id(),
                            }),
                        };
                        self.values
                            .insert(vid, Defer::Ready(Value::Rect(rect.clone())));
                        for (kwarg, rhs) in c.expr.args.kwargs.iter().zip(c.state.kwargs.iter()) {
                            let lhs = self.value_id();
                            match kwarg.name.name {
                                "x0" => {
                                    self.values.insert(
                                        lhs,
                                        Defer::Ready(Value::Linear(LinearExpr::from(rect.x0))),
                                    );
                                }
                                "x1" => {
                                    self.values.insert(
                                        lhs,
                                        Defer::Ready(Value::Linear(LinearExpr::from(rect.x1))),
                                    );
                                }
                                "y0" => {
                                    self.values.insert(
                                        lhs,
                                        Defer::Ready(Value::Linear(LinearExpr::from(rect.y0))),
                                    );
                                }
                                "y1" => {
                                    self.values.insert(
                                        lhs,
                                        Defer::Ready(Value::Linear(LinearExpr::from(rect.y1))),
                                    );
                                }
                                "w" => {
                                    self.values.insert(
                                        lhs,
                                        Defer::Ready(Value::Linear(
                                            LinearExpr::from(rect.x1) - LinearExpr::from(rect.x0),
                                        )),
                                    );
                                }
                                "h" => {
                                    self.values.insert(
                                        lhs,
                                        Defer::Ready(Value::Linear(
                                            LinearExpr::from(rect.y1) - LinearExpr::from(rect.y0),
                                        )),
                                    );
                                }
                                x => panic!("unsupported kwarg `{x}`"),
                            };
                            let defer = self.value_id();
                            self.values.insert(
                                defer,
                                DeferValue::Deferred(PartialEval {
                                    state: PartialEvalState::Constraint(PartialConstraint {
                                        lhs,
                                        rhs: *rhs,
                                    }),
                                    frame: vref.frame,
                                }),
                            );
                            self.deferred.insert(defer);
                        }
                        true
                    } else {
                        false
                    }
                }
                "float" => {
                    self.values.insert(
                        vid,
                        Defer::Ready(Value::Linear(LinearExpr::from(self.solver.new_var()))),
                    );
                    true
                }
                "eq" => {
                    if let (Defer::Ready(vl), Defer::Ready(vr)) = (
                        &self.values[&c.state.posargs[0]],
                        &self.values[&c.state.posargs[1]],
                    ) {
                        let expr = vl.as_ref().unwrap_linear().clone()
                            - vr.as_ref().unwrap_linear().clone();
                        self.solver.constrain_eq0(expr);
                        self.values.insert(vid, Defer::Ready(Value::None));
                        true
                    } else {
                        false
                    }
                }
                f => {
                    panic!(
                        "user function calls should never be deferred: attempted to partial_eval {f}"
                    );
                }
            },
            PartialEvalState::BinOp(bin_op) => {
                if let (Defer::Ready(vl), Defer::Ready(vr)) =
                    (&self.values[&bin_op.lhs], &self.values[&bin_op.rhs])
                {
                    match (vl, vr) {
                        (Value::Linear(vl), Value::Linear(vr)) => {
                            let res = match bin_op.op {
                                BinOp::Add => Some(vl.clone() + vr.clone()),
                                BinOp::Sub => Some(vl.clone() - vr.clone()),
                                BinOp::Mul => {
                                    match (self.solver.eval_expr(vl), self.solver.eval_expr(vr)) {
                                        (Some(vl), Some(vr)) => Some((vl * vr).into()),
                                        (Some(vl), None) => Some(vr.clone() * vl),
                                        (None, Some(vr)) => Some(vl.clone() * vr),
                                        (None, None) => None,
                                    }
                                }
                                BinOp::Div => self.solver.eval_expr(vr).map(|rhs| vl.clone() / rhs),
                            };
                            if let Some(res) = res {
                                self.values
                                    .insert(vid, DeferValue::Ready(Value::Linear(res)));
                                true
                            } else {
                                false
                            }
                        }
                        (Value::Int(vl), Value::Int(vr)) => {
                            let res = match bin_op.op {
                                BinOp::Add => vl + vr,
                                BinOp::Sub => vl - vr,
                                BinOp::Mul => vl * vr,
                                BinOp::Div => vl / vr,
                            };
                            self.values.insert(vid, DeferValue::Ready(Value::Int(res)));
                            true
                        }
                        _ => unreachable!(),
                    }
                } else {
                    false
                }
            }
            PartialEvalState::If(if_) => match if_.state {
                IfExprState::Cond(cond) => {
                    if let Defer::Ready(val) = &self.values[&cond] {
                        if *val.as_ref().unwrap_bool() {
                            let then = self.visit_expr(vref.frame, &if_.expr.then);
                            if_.state = IfExprState::Then(then);
                        } else {
                            let else_ = self.visit_expr(vref.frame, &if_.expr.else_);
                            if_.state = IfExprState::Else(else_);
                        }
                        true
                    } else {
                        false
                    }
                }
                IfExprState::Then(then) => {
                    if let Defer::Ready(val) = &self.values[&then] {
                        self.values.insert(vid, Defer::Ready(val.clone()));
                        true
                    } else {
                        false
                    }
                }
                IfExprState::Else(else_) => {
                    if let Defer::Ready(val) = &self.values[&else_] {
                        self.values.insert(vid, Defer::Ready(val.clone()));
                        true
                    } else {
                        false
                    }
                }
            },
            PartialEvalState::Comparison(comparison_expr) => {
                if let (Defer::Ready(vl), Defer::Ready(vr)) = (
                    &self.values[&comparison_expr.state.left],
                    &self.values[&comparison_expr.state.right],
                ) {
                    match (vl, vr) {
                        (Value::Linear(vl), Value::Linear(vr)) => {
                            if let (Some(vl), Some(vr)) =
                                (self.solver.eval_expr(vl), self.solver.eval_expr(vr))
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
                                true
                            } else {
                                false
                            }
                        }
                        (Value::Int(vl), Value::Int(vr)) => {
                            let res = match comparison_expr.expr.op {
                                crate::ast::ComparisonOp::Eq => vl == vr,
                                crate::ast::ComparisonOp::Ne => vl != vr,
                                crate::ast::ComparisonOp::Geq => vl >= vr,
                                crate::ast::ComparisonOp::Gt => vl > vr,
                                crate::ast::ComparisonOp::Leq => vl <= vr,
                                crate::ast::ComparisonOp::Lt => vl < vr,
                            };
                            self.values.insert(vid, DeferValue::Ready(Value::Bool(res)));
                            true
                        }
                        _ => unreachable!(),
                    }
                } else {
                    false
                }
            }
            PartialEvalState::FieldAccess(field_access_expr) => {
                if let Defer::Ready(base) = &self.values[&field_access_expr.state.base] {
                    let rect = base.as_ref().unwrap_rect();
                    let val = match field_access_expr.expr.field.name {
                        "x0" => Value::Linear(LinearExpr::from(rect.x0)),
                        "x1" => Value::Linear(LinearExpr::from(rect.x1)),
                        "y0" => Value::Linear(LinearExpr::from(rect.y0)),
                        "y1" => Value::Linear(LinearExpr::from(rect.y1)),
                        "w" => Value::Linear(LinearExpr::from(rect.x1) - LinearExpr::from(rect.x0)),
                        "h" => Value::Linear(LinearExpr::from(rect.y1) - LinearExpr::from(rect.y0)),
                        "layer" => Value::EnumValue(rect.layer.clone().unwrap()),
                        f => panic!("invalid field `{f}`"),
                    };
                    self.values.insert(vid, DeferValue::Ready(val));
                    true
                } else {
                    false
                }
            }
            PartialEvalState::Constraint(c) => {
                if let (Defer::Ready(vl), Defer::Ready(vr)) =
                    (&self.values[&c.lhs], &self.values[&c.rhs])
                {
                    let lhs = vl.as_ref().unwrap_linear();
                    let rhs = vr.as_ref().unwrap_linear();
                    let expr = lhs.clone() - rhs.clone();
                    self.solver.constrain_eq0(expr);
                    self.values.insert(vid, DeferValue::Ready(Value::None));
                    true
                } else {
                    false
                }
            }
            PartialEvalState::Cast(c) => {
                if let Defer::Ready(val) = &self.values[&c.value] {
                    let value = match (val, &c.ty) {
                        (Value::Int(x), Ty::Float) => {
                            Some(Value::Linear(LinearExpr::from(*x as f64)))
                        }
                        (x @ Value::Int(_), Ty::Int) => Some(x.clone()),
                        (Value::Linear(expr), Ty::Int) => self
                            .solver
                            .eval_expr(expr)
                            .map(|val| Value::Int(val as i64)),
                        (expr @ Value::Linear(_), Ty::Float) => Some(expr.clone()),
                        _ => panic!("invalid cast"),
                    };
                    if let Some(value) = value {
                        self.values.insert(vid, DeferValue::Ready(value));
                        true
                    } else {
                        false
                    }
                } else {
                    false
                }
            }
        };

        self.values.entry(vid).or_insert(v);
        if self.values[&vid].is_ready() {
            self.deferred.swap_remove(&vid);
        }
        progress
    }
}

#[enumify]
#[derive(Debug, Clone)]
pub enum Value<'a> {
    EnumValue(String),
    Linear(LinearExpr),
    Int(i64),
    Rect(Rect<Var>),
    Bool(bool),
    Fn(FnDecl<'a, VarIdTyMetadata>),
    None,
}

#[enumify]
#[derive(Debug, Clone)]
pub enum SolvedValue {
    Float(f64),
    Rect(Rect<f64>),
}

#[derive(Debug, Clone)]
pub struct CompiledCell {
    pub values: Vec<SolvedValue>,
}

#[enumify(generics_only)]
#[derive(Clone, Debug)]
enum Defer<R, D> {
    Ready(R),
    Deferred(D),
}

type DeferValue<'a, T> = Defer<Value<'a>, PartialEval<'a, T>>;

#[derive(Debug, Clone)]
struct PartialEval<'a, T: AstMetadata> {
    state: PartialEvalState<'a, T>,
    frame: FrameId,
}

#[derive(Debug, Clone)]
enum PartialEvalState<'a, T: AstMetadata> {
    If(Box<PartialIfExpr<'a, T>>),
    Comparison(Box<PartialComparisonExpr<'a, T>>),
    BinOp(PartialBinOp),
    Call(Box<PartialCallExpr<'a, T>>),
    FieldAccess(Box<PartialFieldAccessExpr<'a, T>>),
    Constraint(PartialConstraint),
    Cast(PartialCast),
}

#[derive(Debug, Clone)]
struct PartialCast {
    value: ValueId,
    ty: Ty,
}

#[derive(Debug, Clone)]
struct PartialConstraint {
    lhs: ValueId,
    rhs: ValueId,
}

#[derive(Debug, Clone)]
struct PartialBinOp {
    lhs: ValueId,
    rhs: ValueId,
    op: BinOp,
}

#[derive(Debug, Clone)]
struct PartialIfExpr<'a, T: AstMetadata> {
    expr: IfExpr<'a, T>,
    state: IfExprState,
}

#[derive(Debug, Clone)]
pub enum IfExprState {
    Cond(ValueId),
    Then(ValueId),
    Else(ValueId),
}

#[derive(Debug, Clone)]
struct PartialCallExpr<'a, T: AstMetadata> {
    expr: CallExpr<'a, T>,
    state: CallExprState,
}

#[derive(Debug, Clone)]
pub struct CallExprState {
    posargs: Vec<ValueId>,
    kwargs: Vec<ValueId>,
}

#[derive(Debug, Clone)]
struct PartialComparisonExpr<'a, T: AstMetadata> {
    expr: ComparisonExpr<'a, T>,
    state: ComparisonExprState,
}

#[derive(Debug, Clone)]
pub struct ComparisonExprState {
    left: ValueId,
    right: ValueId,
}

#[derive(Debug, Clone)]
struct PartialFieldAccessExpr<'a, T: AstMetadata> {
    expr: FieldAccessExpr<'a, T>,
    state: FieldAccessExprState,
}

#[derive(Debug, Clone)]
pub struct FieldAccessExprState {
    base: ValueId,
}
