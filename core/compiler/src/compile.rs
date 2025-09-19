//! # Argon compiler
//!
//! Pass 1: assign variable IDs/type checking
//! Pass 3: solving
use std::collections::{HashMap, VecDeque};

use enumify::enumify;
use geometry::transform::{Rotation, TransformationMatrix};
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

pub fn compile(ast: &Ast<'_, ParseMetadata>, input: CompileInput<'_>) -> CompileOutput {
    let pass = VarIdTyPass::new(ast);
    let ast = pass.execute();
    let input = CompileInput {
        cell: input.cell,
        params: input.params,
    };

    ExecPass::new(&ast).execute(input)
}

pub(crate) struct VarIdTyPass<'a> {
    ast: &'a Ast<'a, ParseMetadata>,
    next_id: VarId,
    bindings: Vec<HashMap<&'a str, (VarId, Ty)>>,
    errors: Vec<StaticError>,
}

#[derive(Debug, Clone)]
pub struct VarIdTyMetadata;

#[enumify]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Ty {
    Bool,
    Float,
    Int,
    Rect,
    Enum,
    Cell(Box<CellTy>),
    Inst(Box<CellTy>),
    Nil,
    Fn(Box<FnTy>),
    CellFn(Box<CellFnTy>),
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FnTy {
    args: Vec<Ty>,
    ret: Ty,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CellFnTy {
    args: Vec<Ty>,
    data: HashMap<String, Ty>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CellTy {
    data: HashMap<String, Ty>,
}

impl AstMetadata for VarIdTyMetadata {
    type Ident = ();
    type EnumDecl = ();
    type StructDecl = ();
    type StructField = ();
    type CellDecl = VarId;
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
    pub(crate) fn new(ast: &'a Ast<'a, ParseMetadata>) -> Self {
        Self {
            ast,
            // allocate space for the global namespace
            bindings: vec![HashMap::new()],
            next_id: 1,
            errors: Vec::new(),
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

    pub(crate) fn execute(mut self) -> Ast<'a, VarIdTyMetadata> {
        let mut decls = Vec::new();
        for decl in &self.ast.decls {
            if let Decl::Fn(f) = decl {
                self.declare_fn_decl(f);
            }
        }
        for decl in &self.ast.decls {
            match decl {
                Decl::Fn(f) => {
                    decls.push(Decl::Fn(self.transform_fn_decl(f)));
                }
                Decl::Cell(c) => {
                    decls.push(Decl::Cell(self.transform_cell_decl(c)));
                }
                _ => todo!(),
            }
        }

        Ast { decls }
    }

    fn declare_fn_decl(&mut self, input: &FnDecl<'a, ParseMetadata>) {
        if ["crect", "rect", "float", "eq", "inst"].contains(&input.name.name) {
            self.errors.push(StaticError {
                span: input.name.span,
                kind: StaticErrorKind::RedeclarationOfBuiltin,
            });
            return;
        }
        let args: Vec<_> = input
            .args
            .iter()
            .map(|arg| self.transform_arg_decl(arg))
            .collect();
        let ty = Ty::Fn(Box::new(FnTy {
            args: args.iter().map(|arg| arg.metadata.1.clone()).collect(),
            ret: Ty::from_name(input.return_ty.name),
        }));
        self.alloc(input.name.name, ty);
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
        name: &Ident<'a, Self::Output>,
        _args: &[ArgDecl<'a, Self::Output>],
        _scope: &Scope<'a, Self::Output>,
    ) -> <Self::Output as AstMetadata>::CellDecl {
        // TODO: Argument checks
        // UNUSED
        self.lookup(name.name).unwrap().0
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
        let (varid, _) = self.lookup(input.name.name).unwrap();
        let args: Vec<_> = input
            .args
            .iter()
            .map(|arg| self.transform_arg_decl(arg))
            .collect();
        let name = self.transform_ident(&input.name);
        let return_ty = self.transform_ident(&input.return_ty);
        let scope = self.transform_scope(&input.scope);
        FnDecl {
            name,
            args,
            return_ty,
            scope,
            metadata: varid,
        }
    }

    fn transform_cell_decl(
        &mut self,
        input: &CellDecl<'a, Self::Input>,
    ) -> CellDecl<'a, Self::Output> {
        if ["crect", "rect", "float", "eq", "inst"].contains(&input.name.name) {
            self.errors.push(StaticError {
                span: input.name.span,
                kind: StaticErrorKind::RedeclarationOfBuiltin,
            });
        }
        let args: Vec<_> = input
            .args
            .iter()
            .map(|arg| self.transform_arg_decl(arg))
            .collect();
        let scope = self.transform_scope(&input.scope);
        if let Some(tail) = scope.tail.as_ref() {
            self.errors.push(StaticError {
                span: tail.span(),
                kind: StaticErrorKind::CellWithTailExpr,
            });
        }
        let ty = Ty::CellFn(Box::new(CellFnTy {
            args: args.iter().map(|arg| arg.metadata.1.clone()).collect(),
            data: scope
                .stmts
                .iter()
                .filter_map(|stmt| {
                    if let Statement::LetBinding(lt) = stmt {
                        if ["x", "y"].contains(&lt.name.name) {
                            self.errors.push(StaticError {
                                span: lt.name.span,
                                kind: StaticErrorKind::RedeclarationOfBuiltin,
                            });
                        }
                        Some((lt.name.name.to_string(), lt.value.ty()))
                    } else {
                        None
                    }
                })
                .collect(),
        }));
        let vid = self.alloc(input.name.name, ty);
        let name = self.transform_ident(&input.name);
        CellDecl {
            name,
            scope,
            args,
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
        input: &IfExpr<'a, Self::Input>,
        cond: &Expr<'a, Self::Output>,
        then: &Expr<'a, Self::Output>,
        else_: &Expr<'a, Self::Output>,
    ) -> <Self::Output as AstMetadata>::IfExpr {
        let cond_ty = cond.ty();
        let then_ty = then.ty();
        let else_ty = else_.ty();
        assert_eq!(cond_ty, Ty::Bool);
        if cond_ty != Ty::Bool {
            self.errors.push(StaticError {
                span: cond.span(),
                kind: StaticErrorKind::IfCondNotBool,
            });
        }
        if then_ty != else_ty {
            self.errors.push(StaticError {
                span: input.span,
                kind: StaticErrorKind::BranchesDifferentTypes,
            });
        }
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
        if left_ty != right_ty {
            self.errors.push(StaticError {
                span: input.span,
                kind: StaticErrorKind::BinOpMismatchedTypes,
            });
        }
        if ![Ty::Float, Ty::Int].contains(&left_ty) {
            self.errors.push(StaticError {
                span: left.span(),
                kind: StaticErrorKind::BinOpInvalidType,
            });
        }
        if ![Ty::Float, Ty::Int].contains(&right_ty) {
            self.errors.push(StaticError {
                span: right.span(),
                kind: StaticErrorKind::BinOpInvalidType,
            });
        }
        left_ty
    }

    fn dispatch_unary_op_expr(
        &mut self,
        input: &crate::ast::UnaryOpExpr<'a, Self::Input>,
        operand: &Expr<'a, Self::Output>,
    ) -> <Self::Output as AstMetadata>::UnaryOpExpr {
        match input.op {
            UnaryOp::Not => {
                if operand.ty() != Ty::Bool {
                    self.errors.push(StaticError {
                        span: operand.span(),
                        kind: StaticErrorKind::UnaryOpInvalidType,
                    });
                }
                Ty::Bool
            }
            UnaryOp::Neg => {
                let operand_ty = operand.ty();
                if ![Ty::Float, Ty::Int].contains(&operand_ty) {
                    self.errors.push(StaticError {
                        span: operand.span(),
                        kind: StaticErrorKind::UnaryOpInvalidType,
                    });
                }
                operand_ty
            }
        }
    }

    fn dispatch_comparison_expr(
        &mut self,
        input: &ComparisonExpr<'a, Self::Input>,
        left: &Expr<'a, Self::Output>,
        right: &Expr<'a, Self::Output>,
    ) -> <Self::Output as AstMetadata>::ComparisonExpr {
        let left_ty = left.ty();
        let right_ty = right.ty();
        if left_ty != right_ty {
            self.errors.push(StaticError {
                span: input.span,
                kind: StaticErrorKind::BinOpMismatchedTypes,
            });
        }
        Ty::Bool
    }

    fn dispatch_field_access_expr(
        &mut self,
        _input: &crate::ast::FieldAccessExpr<'a, Self::Input>,
        base: &Expr<'a, Self::Output>,
        field: &Ident<'a, Self::Output>,
    ) -> <Self::Output as AstMetadata>::FieldAccessExpr {
        let base_ty = base.ty();
        match base_ty {
            Ty::Rect => match field.name {
                "x0" | "x1" | "y0" | "y1" | "w" | "h" => Ty::Float,
                "layer" => Ty::Enum,
                _ => panic!("invalid field access"),
            },
            Ty::Inst(c) => match field.name {
                "x" | "y" => Ty::Float,
                name => c
                    .data
                    .get(name)
                    .unwrap_or_else(|| panic!("no field `{name}` on cell instance"))
                    .clone(),
            },
            ty => panic!("cannot access fields of object of type {ty:?}"),
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
            "inst" => {
                assert_eq!(args.posargs.len(), 1);
                for kwarg in &args.kwargs {
                    assert!(["reflect", "angle"].contains(&kwarg.name.name));
                    if kwarg.name.name == "reflect" {
                        assert_eq!(kwarg.value.ty(), Ty::Bool);
                    } else if kwarg.name.name == "angle" {
                        assert_eq!(kwarg.value.ty(), Ty::Int);
                    }
                }
                assert!(matches!(args.posargs[0].ty(), Ty::Cell(_)));
                if let Ty::Cell(c) = args.posargs[0].ty() {
                    (None, Ty::Inst(c.clone()))
                } else {
                    panic!("the argument to inst must be a cell");
                }
            }
            name => {
                let (varid, ty) = self
                    .lookup(name)
                    .unwrap_or_else(|| panic!("no function or cell named `{name}`"));
                match ty {
                    Ty::Fn(ty) => {
                        assert_eq!(args.posargs.len(), ty.args.len());
                        for (arg, arg_ty) in args.posargs.iter().zip(&ty.args) {
                            assert_eq!(&arg.ty(), arg_ty);
                        }
                        assert!(args.kwargs.is_empty());
                        (Some(varid), ty.ret.clone())
                    }
                    Ty::CellFn(ty) => {
                        assert_eq!(args.posargs.len(), ty.args.len());
                        for (arg, arg_ty) in args.posargs.iter().zip(&ty.args) {
                            assert_eq!(&arg.ty(), arg_ty);
                        }
                        assert!(args.kwargs.is_empty());
                        (
                            Some(varid),
                            Ty::Cell(Box::new(CellTy {
                                data: ty.data.clone(),
                            })),
                        )
                    }
                    ty => panic!("cannot invoke an object of type {ty:?}"),
                }
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

#[derive(Debug, Clone)]
pub struct CompileInput<'a> {
    pub cell: &'a str,
    pub params: Vec<f64>,
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
pub type CellId = u64;

#[derive(Clone, Default)]
struct Frame {
    bindings: HashMap<VarId, ValueId>,
    parent: Option<FrameId>,
}

struct CellState {
    solve_iters: u64,
    solver: Solver,
    fields: HashMap<String, ValueId>,
    emit: Vec<ValueId>,
    deferred: IndexSet<ValueId>,
}

struct ExecPass<'a> {
    ast: &'a Ast<'a, VarIdTyMetadata>,
    cell_states: HashMap<CellId, CellState>,
    values: HashMap<ValueId, DeferValue<'a, VarIdTyMetadata>>,
    frames: HashMap<FrameId, Frame>,
    nil_value: ValueId,
    global_frame: FrameId,
    next_id: u64,
    // A stack of cells being evaluated.
    //
    // The first element of this stack is the root cell.
    // the last element of this stack is the current cell.
    partial_cells: VecDeque<CellId>,
    compiled_cells: HashMap<CellId, CompiledCell>,
}

impl<'a> ExecPass<'a> {
    pub(crate) fn new(ast: &'a Ast<'a, VarIdTyMetadata>) -> Self {
        Self {
            ast,
            cell_states: HashMap::new(),
            values: HashMap::from_iter([(1, DeferValue::Ready(Value::None))]),
            frames: HashMap::from_iter([(0, Frame::default())]),
            nil_value: 1,
            global_frame: 0,
            next_id: 2,
            partial_cells: VecDeque::new(),
            compiled_cells: HashMap::new(),
        }
    }

    pub(crate) fn lookup(&self, frame: FrameId, var: VarId) -> Option<ValueId> {
        let frame = self
            .frames
            .get(&frame)
            .expect("no frame found for frame ID");
        if let Some(val) = frame.bindings.get(&var) {
            Some(*val)
        } else {
            frame.parent.and_then(|frame| self.lookup(frame, var))
        }
    }

    pub(crate) fn execute(mut self, input: CompileInput<'a>) -> CompileOutput {
        self.declare_globals();
        let cell_id = self.execute_cell(input.cell, input.params);
        CompileOutput::Valid(ValidCompileOutput {
            cells: self.compiled_cells,
            top: cell_id,
        })
    }

    pub(crate) fn execute_cell(&mut self, cell: &'a str, params: Vec<f64>) -> CellId {
        let cell_id = self.alloc_id();
        self.partial_cells.push_back(cell_id);
        assert!(
            self.cell_states
                .insert(
                    cell_id,
                    CellState {
                        solve_iters: 0,
                        solver: Solver::new(),
                        fields: HashMap::new(),
                        emit: Vec::new(),
                        deferred: Default::default(),
                    }
                )
                .is_none()
        );
        self.visit_cell_body(cell_id, cell, params);
        let mut require_progress = false;
        let mut progress = false;
        while !self.cell_state(cell_id).deferred.is_empty() {
            let deferred = self.cell_state(cell_id).deferred.clone();
            progress = false;
            for vid in deferred {
                progress = progress || self.eval_partial(vid);
            }

            if require_progress && !progress {
                panic!("no progress");
            }

            require_progress = false;

            if !progress {
                let state = self.cell_state_mut(cell_id);
                state.solve_iters += 1;
                state.solver.solve();
                require_progress = true;
            }
        }
        if progress {
            let state = self.cell_state_mut(cell_id);
            state.solve_iters += 1;
            state.solver.solve();
        }
        self.partial_cells
            .pop_back()
            .expect("failed to pop cell id");

        let cell = self.emit(cell_id);
        assert!(self.compiled_cells.insert(cell_id, cell).is_none());
        cell_id
    }

    fn emit(&mut self, cell: CellId) -> CompiledCell {
        let state = self.cell_states.get(&cell).expect("cell not found");
        let emit_val = |vid: ValueId| {
            let value = &self.values[&vid];
            let value = value.as_ref().unwrap_ready();
            match value {
                Value::Linear(l) => Some(SolvedValue::Float(state.solver.eval_expr(l).unwrap())),
                Value::Rect(rect) => Some(SolvedValue::Rect(Rect {
                    layer: rect.layer.clone(),
                    x0: state.solver.value_of(rect.x0).unwrap(),
                    y0: state.solver.value_of(rect.y0).unwrap(),
                    x1: state.solver.value_of(rect.x1).unwrap(),
                    y1: state.solver.value_of(rect.y1).unwrap(),
                    source: rect.source.clone(),
                })),
                Value::Int(x) => Some(SolvedValue::Int(*x)),
                Value::Inst(inst) => Some(SolvedValue::Instance(SolvedInstance {
                    x: state.solver.value_of(inst.x).unwrap(),
                    y: state.solver.value_of(inst.y).unwrap(),
                    angle: inst.angle,
                    reflect: false,
                    cell: *self.values[&inst.cell]
                        .as_ref()
                        .unwrap_ready()
                        .as_ref()
                        .unwrap_cell(),
                    cell_vid: inst.cell,
                })),
                _ => None,
            }
        };
        let values = state
            .emit
            .iter()
            .copied()
            .map(|v| emit_val(v).unwrap())
            .collect();
        let fields = state
            .fields
            .iter()
            .map(|(k, v)| (k.clone(), emit_val(*v)))
            .filter_map(|(k, v)| v.map(|v| (k, v)))
            .collect();
        CompiledCell { values, fields }
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

    fn cell_state(&self, cell_id: CellId) -> &CellState {
        self.cell_states
            .get(&cell_id)
            .expect("no cell state found for cell ID")
    }

    fn cell_state_mut(&mut self, cell_id: CellId) -> &mut CellState {
        self.cell_states
            .get_mut(&cell_id)
            .expect("no cell state found for cell ID")
    }

    fn declare_globals(&mut self) {
        for decl in &self.ast.decls {
            match decl {
                Decl::Fn(f) => {
                    let vid = self.value_id();
                    assert!(
                        self.values
                            .insert(vid, DeferValue::Ready(Value::Fn(f.clone())))
                            .is_none()
                    );
                    assert!(
                        self.frames
                            .get_mut(&self.global_frame)
                            .unwrap()
                            .bindings
                            .insert(f.metadata, vid)
                            .is_none()
                    );
                }
                Decl::Cell(c) => {
                    let vid = self.value_id();
                    assert!(
                        self.values
                            .insert(vid, DeferValue::Ready(Value::CellFn(c.clone())))
                            .is_none()
                    );
                    assert!(
                        self.frames
                            .get_mut(&self.global_frame)
                            .unwrap()
                            .bindings
                            .insert(c.metadata, vid)
                            .is_none()
                    );
                }
                _ => (),
            }
        }
    }

    fn visit_cell_body(&mut self, cell_id: CellId, cell: &'a str, _params: Vec<f64>) {
        // TODO: use params
        let cell = self
            .ast
            .decls
            .iter()
            .find_map(|d| match d {
                Decl::Cell(
                    v @ CellDecl {
                        name: Ident { name, .. },
                        ..
                    },
                ) if *name == cell => Some(v),
                _ => None,
            })
            .expect("cell not found");

        let frame = Frame {
            bindings: Default::default(),
            parent: Some(self.global_frame),
        };
        let fid = self.frame_id();
        self.frames.insert(fid, frame);

        for stmt in cell.scope.stmts.iter() {
            match stmt {
                Statement::LetBinding(binding) => {
                    let value = self.visit_expr(cell_id, fid, &binding.value);
                    self.frames
                        .get_mut(&fid)
                        .unwrap()
                        .bindings
                        .insert(binding.metadata, value);
                    self.cell_states
                        .get_mut(&cell_id)
                        .unwrap()
                        .fields
                        .insert(binding.name.name.to_string(), value);
                }
                Statement::Expr { value, .. } => {
                    self.visit_expr(cell_id, fid, value);
                }
            }
        }
    }

    fn eval_stmt(&mut self, cell: CellId, frame: FrameId, stmt: &Statement<'a, VarIdTyMetadata>) {
        match stmt {
            Statement::LetBinding(binding) => {
                let value = self.visit_expr(cell, frame, &binding.value);
                self.frames
                    .get_mut(&frame)
                    .unwrap()
                    .bindings
                    .insert(binding.metadata, value);
            }
            Statement::Expr { value, .. } => {
                self.visit_expr(cell, frame, value);
            }
        }
    }

    fn visit_expr(
        &mut self,
        cell_id: CellId,
        frame: FrameId,
        expr: &Expr<'a, VarIdTyMetadata>,
    ) -> ValueId {
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
                let vid = self.visit_expr(cell_id, frame, &e.value);
                self.cell_state_mut(cell_id).emit.push(vid);
                return vid;
            }
            Expr::Call(c) => {
                if ["rect", "crect", "float", "inst", "eq"].contains(&c.func.name) {
                    PartialEvalState::Call(Box::new(PartialCallExpr {
                        expr: c.clone(),
                        state: CallExprState {
                            posargs: c
                                .args
                                .posargs
                                .iter()
                                .map(|arg| self.visit_expr(cell_id, frame, arg))
                                .collect(),
                            kwargs: c
                                .args
                                .kwargs
                                .iter()
                                .map(|arg| self.visit_expr(cell_id, frame, &arg.value))
                                .collect(),
                        },
                    }))
                } else {
                    let arg_vals = c
                        .args
                        .posargs
                        .iter()
                        .map(|arg| self.visit_expr(cell_id, frame, arg))
                        .collect_vec();
                    let val = &self.values[&self
                        .lookup(
                            frame,
                            c.metadata
                                .0
                                .expect("no var ID assigned to function being called"),
                        )
                        .unwrap()]
                        .as_ref()
                        .unwrap_ready()
                        .as_ref();
                    match val {
                        ValueRef::Fn(val) => {
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
                            return self.visit_expr(cell_id, fid, &Expr::Scope(Box::new(scope)));
                        }
                        ValueRef::CellFn(_) => PartialEvalState::Call(Box::new(PartialCallExpr {
                            expr: c.clone(),
                            state: CallExprState {
                                posargs: arg_vals,
                                kwargs: c
                                    .args
                                    .kwargs
                                    .iter()
                                    .map(|arg| self.visit_expr(cell_id, frame, &arg.value))
                                    .collect(),
                            },
                        })),
                        _ => todo!("cannot call value: not a function or cell generator"),
                    }
                }
            }
            Expr::If(if_expr) => {
                let cond = self.visit_expr(cell_id, frame, &if_expr.cond);
                PartialEvalState::If(Box::new(PartialIfExpr {
                    expr: (**if_expr).clone(),
                    state: IfExprState::Cond(cond),
                }))
            }
            Expr::Comparison(comparison_expr) => {
                let left = self.visit_expr(cell_id, frame, &comparison_expr.left);
                let right = self.visit_expr(cell_id, frame, &comparison_expr.right);
                PartialEvalState::Comparison(Box::new(PartialComparisonExpr {
                    expr: (**comparison_expr).clone(),
                    state: ComparisonExprState { left, right },
                }))
            }
            Expr::Scope(s) => {
                for stmt in &s.stmts {
                    self.eval_stmt(cell_id, frame, stmt);
                }
                return s
                    .tail
                    .as_ref()
                    .map(|tail| self.visit_expr(cell_id, frame, tail))
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
                let base = self.visit_expr(cell_id, frame, &f.base);
                PartialEvalState::FieldAccess(Box::new(PartialFieldAccessExpr {
                    expr: (**f).clone(),
                    state: FieldAccessExprState { base },
                }))
            }
            Expr::BinOp(b) => {
                let lhs = self.visit_expr(cell_id, frame, &b.left);
                let rhs = self.visit_expr(cell_id, frame, &b.right);
                PartialEvalState::BinOp(PartialBinOp { lhs, rhs, op: b.op })
            }
            Expr::Cast(cast) => {
                let value = self.visit_expr(cell_id, frame, &cast.value);
                PartialEvalState::Cast(PartialCast {
                    value,
                    ty: cast.metadata.clone(),
                })
            }
            x => todo!("{x:?}"),
        };
        let vid = self.value_id();
        self.cell_state_mut(cell_id).deferred.insert(vid);
        self.values.insert(
            vid,
            DeferValue::Deferred(PartialEval {
                state: partial_eval_state,
                frame,
                cell: cell_id,
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
        let state = self.cell_states.get_mut(&vref.cell).unwrap();
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
                            x0: state.solver.new_var(),
                            y0: state.solver.new_var(),
                            x1: state.solver.new_var(),
                            y1: state.solver.new_var(),
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
                                    cell: vref.cell,
                                }),
                            );
                            self.cell_state_mut(vref.cell).deferred.insert(defer);
                        }
                        true
                    } else {
                        false
                    }
                }
                "float" => {
                    self.values.insert(
                        vid,
                        Defer::Ready(Value::Linear(LinearExpr::from(state.solver.new_var()))),
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
                        state.solver.constrain_eq0(expr);
                        self.values.insert(vid, Defer::Ready(Value::None));
                        true
                    } else {
                        false
                    }
                }
                "inst" => {
                    let refl = c
                        .expr
                        .args
                        .kwargs
                        .iter()
                        .zip(c.state.kwargs.iter())
                        .find_map(|(kwarg, vid)| {
                            if kwarg.name.name == "reflect" {
                                Some(
                                    self.values[vid]
                                        .as_ref()
                                        .get_ready()
                                        .map(|refl| *refl.as_ref().unwrap_bool()),
                                )
                            } else {
                                None
                            }
                        });
                    let refl = match refl {
                        None => Some(None),
                        Some(None) => None,
                        Some(Some(l)) => Some(Some(l)),
                    };
                    let angle = c
                        .expr
                        .args
                        .kwargs
                        .iter()
                        .zip(c.state.kwargs.iter())
                        .find_map(|(kwarg, vid)| {
                            if kwarg.name.name == "angle" {
                                Some(self.values[vid].as_ref().get_ready().map(|refl| {
                                    match (*refl.as_ref().unwrap_int() + 360) % 360 {
                                        0 => Rotation::R0,
                                        90 => Rotation::R90,
                                        180 => Rotation::R180,
                                        270 => Rotation::R270,
                                        x => panic!("angle {x} must be a multiple of 90 degrees between 0 and 360 degrees"),
                                    }
                                }))
                            } else {
                                None
                            }
                        });
                    let angle = match angle {
                        None => Some(None),
                        Some(None) => None,
                        Some(Some(l)) => Some(Some(l)),
                    };
                    if let (Some(refl), Some(angle)) = (refl, angle) {
                        let inst = Instance {
                            x: state.solver.new_var(),
                            y: state.solver.new_var(),
                            cell: *c.state.posargs.first().unwrap(),
                            reflect: refl.unwrap_or_default(),
                            angle: angle.unwrap_or_default(),
                        };
                        self.values.insert(vid, Defer::Ready(Value::Inst(inst)));
                        true
                    } else {
                        false
                    }
                }
                cell => {
                    // Must be calling a cell generator.
                    // User functions are never deferred.
                    let arg_vals = c
                        .state
                        .posargs
                        .iter()
                        .map(|v| {
                            self.values[v]
                                .get_ready()
                                .and_then(|v| state.solver.eval_expr(v.as_ref().unwrap_linear()))
                        })
                        .collect::<Option<Vec<f64>>>();
                    if let Some(arg_vals) = arg_vals {
                        let cell = self.execute_cell(cell, arg_vals);
                        self.values.insert(vid, Defer::Ready(Value::Cell(cell)));
                        true
                    } else {
                        false
                    }
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
                                    match (state.solver.eval_expr(vl), state.solver.eval_expr(vr)) {
                                        (Some(vl), Some(vr)) => Some((vl * vr).into()),
                                        (Some(vl), None) => Some(vr.clone() * vl),
                                        (None, Some(vr)) => Some(vl.clone() * vr),
                                        (None, None) => None,
                                    }
                                }
                                BinOp::Div => {
                                    state.solver.eval_expr(vr).map(|rhs| vl.clone() / rhs)
                                }
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
                            let then = self.visit_expr(vref.cell, vref.frame, &if_.expr.then);
                            if_.state = IfExprState::Then(then);
                        } else {
                            let else_ = self.visit_expr(vref.cell, vref.frame, &if_.expr.else_);
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
                                (state.solver.eval_expr(vl), state.solver.eval_expr(vr))
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
                    match base.as_ref() {
                        ValueRef::Rect(rect) => {
                            let val = match field_access_expr.expr.field.name {
                                "x0" => Value::Linear(LinearExpr::from(rect.x0)),
                                "x1" => Value::Linear(LinearExpr::from(rect.x1)),
                                "y0" => Value::Linear(LinearExpr::from(rect.y0)),
                                "y1" => Value::Linear(LinearExpr::from(rect.y1)),
                                "w" => Value::Linear(
                                    LinearExpr::from(rect.x1) - LinearExpr::from(rect.x0),
                                ),
                                "h" => Value::Linear(
                                    LinearExpr::from(rect.y1) - LinearExpr::from(rect.y0),
                                ),
                                "layer" => Value::EnumValue(rect.layer.clone().unwrap()),
                                f => panic!("invalid field `{f}`"),
                            };
                            self.values.insert(vid, DeferValue::Ready(val));
                            true
                        }
                        ValueRef::Inst(inst) => {
                            let val = match field_access_expr.expr.field.name {
                                "x" => Some(Value::Linear(LinearExpr::from(inst.x))),
                                "y" => Some(Value::Linear(LinearExpr::from(inst.y))),
                                field => {
                                    if let Defer::Ready(cell) = &self.values[&inst.cell] {
                                        let cell_id = *cell.as_ref().unwrap_cell();
                                        // When a cell is ready, it must have been fully
                                        // solved/compiled, and therefore it will be in the
                                        // compiled cell map.
                                        let cell = &self.compiled_cells[&cell_id];
                                        match &cell.fields[field] {
                                            SolvedValue::Rect(rect) => {
                                                let xrect = Rect {
                                                    layer: rect.layer.clone(),
                                                    x0: state.solver.new_var(),
                                                    y0: state.solver.new_var(),
                                                    x1: state.solver.new_var(),
                                                    y1: state.solver.new_var(),
                                                    source: None, // TODO
                                                };
                                                let rect = rect.transform(inst.reflect, inst.angle);
                                                let dx0 = LinearExpr::from(xrect.x0)
                                                    - rect.x0
                                                    - LinearExpr::from(inst.x);
                                                let dx1 = LinearExpr::from(xrect.x1)
                                                    - rect.x1
                                                    - LinearExpr::from(inst.x);
                                                let dy0 = LinearExpr::from(xrect.y0)
                                                    - rect.y0
                                                    - LinearExpr::from(inst.y);
                                                let dy1 = LinearExpr::from(xrect.y1)
                                                    - rect.y1
                                                    - LinearExpr::from(inst.y);
                                                let state = self.cell_state_mut(vref.cell);
                                                state.solver.constrain_eq0(dx0);
                                                state.solver.constrain_eq0(dx1);
                                                state.solver.constrain_eq0(dy0);
                                                state.solver.constrain_eq0(dy1);
                                                Some(Value::Rect(xrect))
                                            }
                                            SolvedValue::Instance(cinst) => {
                                                let (angle, reflect, cx, cy) = cascade(
                                                    inst.angle,
                                                    inst.reflect,
                                                    cinst.angle,
                                                    cinst.reflect,
                                                    cinst.x,
                                                    cinst.y,
                                                );
                                                let oinst = Instance {
                                                    cell: cinst.cell_vid,
                                                    x: state.solver.new_var(),
                                                    y: state.solver.new_var(),
                                                    angle,
                                                    reflect,
                                                };
                                                let dx = LinearExpr::from(inst.x) + cx
                                                    - LinearExpr::from(oinst.x);
                                                let dy = LinearExpr::from(inst.y) + cy
                                                    - LinearExpr::from(oinst.y);
                                                state.solver.constrain_eq0(dx);
                                                state.solver.constrain_eq0(dy);
                                                Some(Value::Inst(oinst))
                                            }
                                            _ => todo!(),
                                        }
                                    } else {
                                        None
                                    }
                                }
                            };
                            if let Some(val) = val {
                                self.values.insert(vid, DeferValue::Ready(val));
                                true
                            } else {
                                false
                            }
                        }
                        _ => {
                            panic!(
                                "field access expressions only supported on objects of type Rect or Inst"
                            )
                        }
                    }
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
                    state.solver.constrain_eq0(expr);
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
                        (Value::Linear(expr), Ty::Int) => state
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

        let cell_id = vref.cell;
        self.values.entry(vid).or_insert(v);
        if self.values[&vid].is_ready() {
            self.cell_state_mut(cell_id).deferred.swap_remove(&vid);
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
    /// A cell generator.
    ///
    /// Example:
    /// ```
    /// cell mycell() {
    ///   // ...
    /// }
    /// ```
    ///
    /// `mycell` is a value of type `CellFn`.
    CellFn(CellDecl<'a, VarIdTyMetadata>),
    /// A particular parameterization of a cell.
    ///
    /// Example:
    /// ```
    /// cell mycell() {
    ///   // ...
    /// }
    ///
    /// let val = mycell();
    /// ```
    ///
    /// `val` is a value of type `Cell`.
    Cell(CellId),
    /// An instantiation of a cell value.
    ///
    /// Example:
    /// ```
    /// cell mycell() {
    ///   // ...
    /// }
    ///
    /// let mycell_inst = inst(mycell(), x=0, y=0);
    /// ```
    ///
    /// `mycell_inst` is a value of type `Inst`.
    Inst(Instance),
    None,
}

#[derive(Debug, Clone)]
pub struct Instance {
    pub x: Var,
    pub y: Var,
    pub cell: ValueId,
    pub reflect: bool,
    pub angle: Rotation,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SolvedInstance {
    pub x: f64,
    pub y: f64,
    pub angle: Rotation,
    pub reflect: bool,
    pub cell: CellId,
    /// The value ID of the cell being instantiated.
    ///
    /// For compiler internal use only.
    cell_vid: ValueId,
}

#[enumify]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum SolvedValue {
    Int(i64),
    Float(f64),
    Rect(Rect<f64>),
    Instance(SolvedInstance),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompiledCell {
    pub values: Vec<SolvedValue>,
    pub fields: HashMap<String, SolvedValue>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StaticError {
    pub span: cfgrammar::Span,
    pub kind: StaticErrorKind,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum StaticErrorKind {
    /// Multiple declarations with the same name.
    ///
    /// For example, two cells named `my_cell`.
    DuplicateNameDeclaration,
    /// Attempted to declare an object with the same name as a built-in object.
    ///
    /// For example, users cannot declare cells or functions named `rect`.
    RedeclarationOfBuiltin,
    /// A cell had an expression in tail position, which is not permitted.
    CellWithTailExpr,
    /// If conditions must have type bool.
    IfCondNotBool,
    /// Branches in expresssions must evaluate to the same type.
    BranchesDifferentTypes,
    /// The operands in a binary expression must have the same type.
    BinOpMismatchedTypes,
    /// A type cannot be used in a binary expression.
    BinOpInvalidType,
    /// A type cannot be used in a unary operation.
    UnaryOpInvalidType,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum CompileOutput {
    StaticErrors(StaticErrorCompileOutput),
    Valid(ValidCompileOutput),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StaticErrorCompileOutput {
    pub errors: Vec<StaticError>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ValidCompileOutput {
    pub cells: HashMap<CellId, CompiledCell>,
    pub top: CellId,
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
    cell: CellId,
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

fn ifmatvec(mat: TransformationMatrix, pt: (f64, f64)) -> (f64, f64) {
    (
        mat[0][0] as f64 * pt.0 + mat[0][1] as f64 * pt.1,
        mat[1][0] as f64 * pt.0 + mat[1][1] as f64 * pt.1,
    )
}

fn tmat(rot: Rotation, refv: bool) -> TransformationMatrix {
    let mut mat = TransformationMatrix::identity();
    if refv {
        mat = mat.reflect_vert()
    }
    mat = mat.rotate(rot);
    mat
}

fn imat(mat: TransformationMatrix) -> (Rotation, bool) {
    let refv = mat[1][0] == mat[0][1] && mat[0][0] == -mat[1][1];
    let rot = match (mat[0][0], mat[1][0]) {
        (1, 0) => Rotation::R0,
        (0, 1) => Rotation::R90,
        (-1, 0) => Rotation::R180,
        (0, -1) => Rotation::R270,
        _ => panic!("invalid rotation matrix"),
    };
    (rot, refv)
}

impl Rect<f64> {
    fn transform(&self, reflect_vert: bool, angle: Rotation) -> Self {
        let mat = tmat(angle, reflect_vert);
        let p0p = ifmatvec(mat, (self.x0, self.y0));
        let p1p = ifmatvec(mat, (self.x1, self.y1));
        Self {
            layer: self.layer.clone(),
            x0: p0p.0.min(p1p.0),
            y0: p0p.1.min(p1p.1),
            x1: p0p.0.max(p1p.0),
            y1: p0p.1.max(p1p.1),
            source: self.source.clone(),
        }
    }
}

fn cascade(
    rot: Rotation,
    refv: bool,
    crot: Rotation,
    crefv: bool,
    cx: f64,
    cy: f64,
) -> (Rotation, bool, f64, f64) {
    let mat = tmat(rot, refv);
    let cmat = tmat(crot, crefv);
    let (x, y) = ifmatvec(mat, (cx, cy));
    let (rot, refv) = imat(mat * cmat);
    (rot, refv, x, y)
}
