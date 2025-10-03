//! # Argon compiler
//!
//! Pass 1: assign variable IDs/type checking
//! Pass 3: solving
use std::collections::VecDeque;
use std::io::BufReader;
use std::path::Path;

use enumify::enumify;
use geometry::transform::{Rotation, TransformationMatrix};
use indexmap::{IndexMap, IndexSet};
use itertools::Itertools;
use serde::{Deserialize, Serialize};

use crate::ast::{BinOp, ConstantDecl, FieldAccessExpr, FnDecl, Scope, UnaryOp};
use crate::layer::LayerProperties;
use crate::parse::ParseAst;
use crate::{
    ast::{
        ArgDecl, Ast, AstMetadata, AstTransformer, BinOpExpr, CallExpr, CellDecl, ComparisonExpr,
        Decl, EnumValue, Expr, Ident, IfExpr, LetBinding, Statement,
    },
    parse::ParseMetadata,
    solver::{LinearExpr, Solver, Var},
};

pub fn compile(ast: &ParseAst<'_>, input: CompileInput<'_>) -> CompileOutput {
    let pass = VarIdTyPass::new(ast);
    let (ast, errors) = pass.execute();
    if !errors.is_empty() {
        return CompileOutput::StaticErrors(StaticErrorCompileOutput { errors });
    };
    let input = CompileInput {
        cell: input.cell,
        args: input.args,
        lyp_file: input.lyp_file,
    };

    let res = ExecPass::new(&ast).execute(input);
    check_layers(&res);
    res
}

fn check_layers(output: &CompileOutput) {
    if let CompileOutput::Valid(output) = output {
        let mut layers = IndexSet::new();
        for layer in output.layers.layers.iter() {
            layers.insert(layer.name.clone());
        }
        for (_, cell) in output.cells.iter() {
            for (_, obj) in cell.objects.iter() {
                if let SolvedValue::Rect(r) = obj
                    && let Some(layer) = &r.layer
                    && !layers.contains(layer)
                {
                    panic!("unknown layer `{layer}`");
                }
            }
        }
    }
}

#[derive(Default)]
pub(crate) struct VarIdTyFrame<'a> {
    var_bindings: IndexMap<&'a str, (VarId, Ty)>,
    scope_bindings: IndexSet<&'a str>,
}

pub(crate) struct VarIdTyPass<'a> {
    ast: &'a ParseAst<'a>,
    next_id: VarId,
    bindings: Vec<VarIdTyFrame<'a>>,
    errors: Vec<StaticError>,
}

#[derive(Debug, Clone)]
pub struct VarIdTyMetadata;

#[enumify]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Ty {
    Bool,
    Float,
    Int,
    Rect,
    Enum,
    String,
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
            "()" => Ty::Nil,
            name => panic!("invalid type: {name}"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FnTy {
    args: Vec<Ty>,
    ret: Ty,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CellFnTy {
    args: Vec<Ty>,
    data: IndexMap<String, Ty>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CellTy {
    data: IndexMap<String, Ty>,
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
    pub(crate) fn new(ast: &'a ParseAst<'a>) -> Self {
        Self {
            ast,
            // allocate space for the global namespace
            bindings: vec![VarIdTyFrame::default()],
            next_id: 1,
            errors: Vec::new(),
        }
    }

    fn lookup(&self, name: &str) -> Option<(VarId, Ty)> {
        for frame in self.bindings.iter().rev() {
            if let Some(info) = frame.var_bindings.get(name) {
                return Some(info.clone());
            }
        }
        None
    }

    fn alloc(&mut self, name: &'a str, ty: Ty) -> VarId {
        let id = self.next_id;
        self.bindings
            .last_mut()
            .unwrap()
            .var_bindings
            .insert(name, (id, ty));
        self.next_id += 1;
        id
    }

    pub(crate) fn execute(mut self) -> (Ast<&'a str, VarIdTyMetadata>, Vec<StaticError>) {
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

        (
            Ast {
                decls,
                span: self.ast.span,
            },
            self.errors,
        )
    }

    fn declare_fn_decl(&mut self, input: &FnDecl<&'a str, ParseMetadata>) {
        if ["crect", "rect", "float", "eq", "dimension", "inst"].contains(&input.name.name) {
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
            ret: if let Some(return_ty) = &input.return_ty {
                Ty::from_name(return_ty.name)
            } else {
                Ty::Nil
            },
        }));
        self.alloc(input.name.name, ty);
    }
}

impl<S> Expr<S, VarIdTyMetadata> {
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
            Expr::FloatLiteral(_) => Ty::Float,
            Expr::IntLiteral(_) => Ty::Int,
            Expr::BoolLiteral(_) => Ty::Bool,
            Expr::StringLiteral(_) => Ty::String,
            Expr::Scope(scope) => scope.metadata.clone(),
            Expr::Cast(cast) => cast.metadata.clone(),
            Expr::UnaryOp(unary_op_expr) => unary_op_expr.metadata.clone(),
        }
    }
}

impl<'a> AstTransformer for VarIdTyPass<'a> {
    type InputMetadata = ParseMetadata;
    type OutputMetadata = VarIdTyMetadata;
    type InputS = &'a str;
    type OutputS = &'a str;

    fn dispatch_ident(
        &mut self,
        _input: &Ident<&'a str, Self::InputMetadata>,
    ) -> <Self::OutputMetadata as AstMetadata>::Ident {
    }

    fn dispatch_var_expr(
        &mut self,
        input: &crate::ast::VarExpr<&'a str, Self::InputMetadata>,
        _name: &Ident<&'a str, Self::OutputMetadata>,
    ) -> <Self::OutputMetadata as AstMetadata>::VarExpr {
        self.lookup(input.name.name)
            .expect("used variable before declaration")
    }

    fn dispatch_enum_decl(
        &mut self,
        _input: &crate::ast::EnumDecl<&'a str, Self::InputMetadata>,
        _name: &Ident<&'a str, Self::OutputMetadata>,
        _variants: &[Ident<&'a str, Self::OutputMetadata>],
    ) -> <Self::OutputMetadata as AstMetadata>::EnumDecl {
    }

    fn dispatch_cell_decl(
        &mut self,
        _input: &CellDecl<&'a str, Self::InputMetadata>,
        name: &Ident<&'a str, Self::OutputMetadata>,
        _args: &[ArgDecl<&'a str, Self::OutputMetadata>],
        _scope: &Scope<&'a str, Self::OutputMetadata>,
    ) -> <Self::OutputMetadata as AstMetadata>::CellDecl {
        // TODO: Argument checks
        // UNUSED
        self.lookup(name.name).unwrap().0
    }

    fn dispatch_fn_decl(
        &mut self,
        _input: &FnDecl<&'a str, Self::InputMetadata>,
        name: &Ident<&'a str, Self::OutputMetadata>,
        _args: &[ArgDecl<&'a str, Self::OutputMetadata>],
        _return_ty: &Option<Ident<&'a str, Self::OutputMetadata>>,
        _scope: &Scope<&'a str, Self::OutputMetadata>,
    ) -> <Self::OutputMetadata as AstMetadata>::FnDecl {
        // UNUSED
        self.lookup(name.name).unwrap().0
    }

    fn transform_fn_decl(
        &mut self,
        input: &FnDecl<&'a str, Self::InputMetadata>,
    ) -> FnDecl<&'a str, Self::OutputMetadata> {
        let (varid, _) = self.lookup(input.name.name).unwrap();
        let args: Vec<_> = input
            .args
            .iter()
            .map(|arg| self.transform_arg_decl(arg))
            .collect();
        let name = self.transform_ident(&input.name);
        let return_ty = input
            .return_ty
            .as_ref()
            .map(|ident| self.transform_ident(ident));
        let scope = self.transform_scope(&input.scope);
        FnDecl {
            name,
            args,
            return_ty,
            scope,
            span: input.span,
            metadata: varid,
        }
    }

    fn transform_cell_decl(
        &mut self,
        input: &CellDecl<&'a str, Self::InputMetadata>,
    ) -> CellDecl<&'a str, Self::OutputMetadata> {
        if ["crect", "rect", "float", "eq", "dimension", "inst"].contains(&input.name.name) {
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
            span: input.span,
            metadata: vid,
        }
    }

    fn dispatch_constant_decl(
        &mut self,
        _input: &ConstantDecl<&'a str, Self::InputMetadata>,
        _name: &Ident<&'a str, Self::OutputMetadata>,
        _ty: &Ident<&'a str, Self::OutputMetadata>,
        _value: &Expr<&'a str, Self::OutputMetadata>,
    ) -> <Self::OutputMetadata as AstMetadata>::ConstantDecl {
    }

    fn dispatch_if_expr(
        &mut self,
        input: &IfExpr<&'a str, Self::InputMetadata>,
        cond: &Expr<&'a str, Self::OutputMetadata>,
        then: &Scope<&'a str, Self::OutputMetadata>,
        else_: &Scope<&'a str, Self::OutputMetadata>,
    ) -> <Self::OutputMetadata as AstMetadata>::IfExpr {
        if let Some(scope_annotation) = &input.scope_annotation {
            let bindings = self.bindings.last_mut().unwrap();
            if bindings.scope_bindings.contains(scope_annotation.name) {
                self.errors.push(StaticError {
                    span: scope_annotation.span,
                    kind: StaticErrorKind::DuplicateNameDeclaration,
                });
            }
            bindings.scope_bindings.insert(scope_annotation.name);
        }
        let cond_ty = cond.ty();
        let then_ty = then.metadata.clone();
        let else_ty = else_.metadata.clone();
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
        input: &BinOpExpr<&'a str, Self::InputMetadata>,
        left: &Expr<&'a str, Self::OutputMetadata>,
        right: &Expr<&'a str, Self::OutputMetadata>,
    ) -> <Self::OutputMetadata as AstMetadata>::BinOpExpr {
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
        input: &crate::ast::UnaryOpExpr<&'a str, Self::InputMetadata>,
        operand: &Expr<&'a str, Self::OutputMetadata>,
    ) -> <Self::OutputMetadata as AstMetadata>::UnaryOpExpr {
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
        input: &ComparisonExpr<&'a str, Self::InputMetadata>,
        left: &Expr<&'a str, Self::OutputMetadata>,
        right: &Expr<&'a str, Self::OutputMetadata>,
    ) -> <Self::OutputMetadata as AstMetadata>::ComparisonExpr {
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
        _input: &crate::ast::FieldAccessExpr<&'a str, Self::InputMetadata>,
        base: &Expr<&'a str, Self::OutputMetadata>,
        field: &Ident<&'a str, Self::OutputMetadata>,
    ) -> <Self::OutputMetadata as AstMetadata>::FieldAccessExpr {
        let base_ty = base.ty();
        match base_ty {
            Ty::Rect => match field.name {
                "x0" | "x1" | "y0" | "y1" | "w" | "h" => Ty::Float,
                "layer" => Ty::String,
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
        _input: &EnumValue<&'a str, Self::InputMetadata>,
        _name: &Ident<&'a str, Self::OutputMetadata>,
        _variant: &Ident<&'a str, Self::OutputMetadata>,
    ) -> <Self::OutputMetadata as AstMetadata>::EnumValue {
    }

    fn dispatch_call_expr(
        &mut self,
        _input: &crate::ast::CallExpr<&'a str, Self::InputMetadata>,
        func: &Ident<&'a str, Self::OutputMetadata>,
        args: &crate::ast::Args<&'a str, Self::OutputMetadata>,
    ) -> <Self::OutputMetadata as AstMetadata>::CallExpr {
        match func.name {
            "crect" | "rect" => {
                if func.name == "crect" {
                    assert_eq!(args.posargs.len(), 0);
                } else {
                    assert_eq!(args.posargs.len(), 1);
                    assert_eq!(args.posargs[0].ty(), Ty::String);
                }
                for kwarg in &args.kwargs {
                    assert!(
                        ["x0", "x1", "y0", "y1", "x0i", "x1i", "y0i", "y1i", "w", "h"]
                            .contains(&kwarg.name.name)
                    );
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
            "dimension" => {
                assert_eq!(args.posargs.len(), 7);
                for (i, arg) in args.posargs.iter().enumerate() {
                    if i == 6 {
                        assert_eq!(arg.ty(), Ty::Bool);
                    } else {
                        assert_eq!(arg.ty(), Ty::Float);
                    }
                }
                (None, Ty::Nil)
            }
            "inst" => {
                assert_eq!(args.posargs.len(), 1);
                for kwarg in &args.kwargs {
                    assert!(["reflect", "angle", "x", "y", "xi", "yi"].contains(&kwarg.name.name));
                    let expected_ty = match kwarg.name.name {
                        "reflect" => Ty::Bool,
                        "angle" => Ty::Int,
                        "x" | "y" | "xi" | "yi" => Ty::Float,
                        _ => unreachable!(),
                    };
                    assert_eq!(kwarg.value.ty(), expected_ty);
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
        _input: &crate::ast::EmitExpr<&'a str, Self::InputMetadata>,
        value: &Expr<&'a str, Self::OutputMetadata>,
    ) -> <Self::OutputMetadata as AstMetadata>::EmitExpr {
        value.ty()
    }

    fn dispatch_args(
        &mut self,
        _input: &crate::ast::Args<&'a str, Self::InputMetadata>,
        _posargs: &[Expr<&'a str, Self::OutputMetadata>],
        _kwargs: &[crate::ast::KwArgValue<&'a str, Self::OutputMetadata>],
    ) -> <Self::OutputMetadata as AstMetadata>::Args {
    }

    fn dispatch_cast(
        &mut self,
        _input: &crate::ast::CastExpr<&'a str, Self::InputMetadata>,
        _value: &Expr<&'a str, Self::OutputMetadata>,
        ty: &Ident<&'a str, Self::OutputMetadata>,
    ) -> <Self::OutputMetadata as AstMetadata>::CastExpr {
        Ty::from_name(ty.name)
    }

    fn dispatch_kw_arg_value(
        &mut self,
        _input: &crate::ast::KwArgValue<&'a str, Self::InputMetadata>,
        _name: &Ident<&'a str, Self::OutputMetadata>,
        value: &Expr<&'a str, Self::OutputMetadata>,
    ) -> <Self::OutputMetadata as AstMetadata>::KwArgValue {
        value.ty()
    }

    fn dispatch_arg_decl(
        &mut self,
        input: &ArgDecl<&'a str, Self::InputMetadata>,
        _name: &Ident<&'a str, Self::OutputMetadata>,
        _ty: &Ident<&'a str, Self::OutputMetadata>,
    ) -> <Self::OutputMetadata as AstMetadata>::ArgDecl {
        let ty = Ty::from_name(input.ty.name);
        (self.alloc(input.name.name, ty.clone()), ty)
    }

    fn dispatch_scope(
        &mut self,
        _input: &Scope<&'a str, Self::InputMetadata>,
        _stmts: &[Statement<&'a str, Self::OutputMetadata>],
        tail: &Option<Expr<&'a str, Self::OutputMetadata>>,
    ) -> <Self::OutputMetadata as AstMetadata>::Scope {
        tail.as_ref().map(|tail| tail.ty()).unwrap_or(Ty::Nil)
    }

    fn enter_scope(&mut self, input: &crate::ast::Scope<&'a str, Self::InputMetadata>) {
        if let Some(scope_annotation) = &input.scope_annotation {
            let bindings = self.bindings.last_mut().unwrap();
            if bindings.scope_bindings.contains(scope_annotation.name) {
                self.errors.push(StaticError {
                    span: scope_annotation.span,
                    kind: StaticErrorKind::DuplicateNameDeclaration,
                });
            }
            bindings.scope_bindings.insert(scope_annotation.name);
        }
        self.bindings.push(Default::default());
    }

    fn exit_scope(
        &mut self,
        _input: &crate::ast::Scope<&'a str, Self::InputMetadata>,
        _output: &crate::ast::Scope<&'a str, Self::OutputMetadata>,
    ) {
        self.bindings.pop();
    }

    fn dispatch_let_binding(
        &mut self,
        _input: &LetBinding<&'a str, Self::InputMetadata>,
        name: &Ident<&'a str, Self::OutputMetadata>,
        value: &Expr<&'a str, Self::OutputMetadata>,
    ) -> <Self::OutputMetadata as AstMetadata>::LetBinding {
        self.alloc(name.name, value.ty())
    }

    fn transform_s(&mut self, s: &Self::InputS) -> Self::OutputS {
        *s
    }
}

#[derive(Debug, Clone)]
pub enum CellArg {
    Float(f64),
    Int(i64),
}

#[derive(Debug, Clone)]
pub struct CompileInput<'a> {
    pub cell: &'a str,
    pub args: Vec<CellArg>,
    pub lyp_file: &'a Path,
}

pub type VarId = u64;
pub type ConstraintVarId = u64;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompiledEmit {
    pub span: cfgrammar::Span,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BasicRect<T> {
    pub layer: Option<String>,
    pub x0: T,
    pub y0: T,
    pub x1: T,
    pub y1: T,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Rect<T> {
    pub layer: Option<String>,
    pub id: ObjectId,
    pub x0: T,
    pub y0: T,
    pub x1: T,
    pub y1: T,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Dimension<T> {
    pub id: ObjectId,
    pub p: T,
    pub n: T,
    pub value: T,
    pub coord: T,
    pub pstop: T,
    pub nstop: T,
    pub horiz: bool,
    pub span: Option<cfgrammar::Span>,
}

type FrameId = u64;
type ValueId = u64;
pub type CellId = u64;

/// Sequence number.
#[derive(Copy, Clone, Eq, PartialEq, Hash, Debug, Serialize, Deserialize, Ord, PartialOrd)]
pub struct SeqNum(u64);

#[derive(Copy, Clone, Eq, PartialEq, Hash, Debug, Serialize, Deserialize)]
pub struct ObjectId(u64);

#[derive(Copy, Clone, Eq, PartialEq, Hash, Debug, Serialize, Deserialize)]
pub struct ScopeId(u64);

#[derive(Copy, Clone, Eq, PartialEq, Hash, Debug, Serialize, Deserialize)]
pub(crate) struct DynLoc {
    pub(crate) cell: CellId,
    pub(crate) frame: FrameId,
    pub(crate) scope: ScopeId,
    pub(crate) seq_num: SeqNum,
}

#[derive(Clone)]
struct Frame {
    bindings: IndexMap<VarId, ValueId>,
    parent: Option<FrameId>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Emit {
    value: ValueId,
    scope: ScopeId,
    span: cfgrammar::Span,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct ExecScope {
    parent: Option<ScopeId>,
    static_parent: Option<(ScopeId, SeqNum)>,
    name: String,
    span: cfgrammar::Span,
    bindings: IndexMap<SeqNum, (String, ValueId)>,
}

struct CellState {
    solve_iters: u64,
    solver: Solver,
    fields: IndexMap<String, ValueId>,
    emit: Vec<Emit>,
    objects: IndexMap<ObjectId, Object>,
    deferred: IndexSet<ValueId>,
    root_scope: ScopeId,
    scopes: IndexMap<ScopeId, ExecScope>,
    fallback_constraints: Vec<LinearExpr>,
    fallback_constraints_used: Vec<LinearExpr>,
    nullspace_vecs: Option<Vec<Vec<f64>>>,
}

struct ExecPass<'a> {
    ast: &'a Ast<&'a str, VarIdTyMetadata>,
    cell_states: IndexMap<CellId, CellState>,
    values: IndexMap<ValueId, DeferValue<'a, VarIdTyMetadata>>,
    frames: IndexMap<FrameId, Frame>,
    nil_value: ValueId,
    true_value: ValueId,
    false_value: ValueId,
    global_frame: FrameId,
    next_id: u64,
    // A stack of cells being evaluated.
    //
    // The first element of this stack is the root cell.
    // the last element of this stack is the current cell.
    partial_cells: VecDeque<CellId>,
    compiled_cells: IndexMap<CellId, CompiledCell>,
}

enum ExecScopeName {
    // Exact name has been specified.
    Specified(String),
    // Exact name has not been specified, need to generate unique identifier based on prefix.
    Prefix(String),
}

impl<'a> ExecPass<'a> {
    pub(crate) fn new(ast: &'a Ast<&'a str, VarIdTyMetadata>) -> Self {
        Self {
            ast,
            cell_states: IndexMap::new(),
            values: IndexMap::from_iter([
                (1, DeferValue::Ready(Value::None)),
                (2, DeferValue::Ready(Value::Bool(true))),
                (3, DeferValue::Ready(Value::Bool(false))),
            ]),
            frames: IndexMap::from_iter([(
                0,
                Frame {
                    bindings: Default::default(),
                    parent: None,
                },
            )]),
            nil_value: 1,
            true_value: 2,
            false_value: 3,
            global_frame: 0,
            next_id: 4,
            partial_cells: VecDeque::new(),
            compiled_cells: IndexMap::new(),
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
        let cell_id = self.execute_cell(input.cell, input.args);
        let layers =
            klayout_lyp::from_reader(BufReader::new(std::fs::File::open(input.lyp_file).unwrap()))
                .unwrap()
                .into();
        CompileOutput::Valid(ValidCompileOutput {
            cells: self.compiled_cells,
            top: cell_id,
            layers,
        })
    }

    pub(crate) fn execute_cell(&mut self, cell: &'a str, args: Vec<CellArg>) -> CellId {
        let cell_decl = self
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

        let mut frame = Frame {
            bindings: Default::default(),
            parent: Some(self.global_frame),
        };
        assert_eq!(args.len(), cell_decl.args.len());
        for (val, decl) in args.into_iter().zip(cell_decl.args.iter()) {
            let vid = self.value_id();
            let val = match val {
                CellArg::Int(i) => Value::Int(i),
                CellArg::Float(f) => Value::Linear(LinearExpr::from(f)),
            };
            self.values.insert(vid, DeferValue::Ready(val));
            frame.bindings.insert(decl.metadata.0, vid);
        }
        let fid = self.frame_id();
        self.frames.insert(fid, frame);

        let root_scope = ExecScope {
            parent: None,
            static_parent: None,
            span: cell_decl.scope.span,
            name: format!("cell {cell}"),
            bindings: Default::default(),
        };
        let root_scope_id = self.scope_id();

        let cell_id = self.alloc_id();
        self.partial_cells.push_back(cell_id);
        assert!(
            self.cell_states
                .insert(
                    cell_id,
                    CellState {
                        solve_iters: 0,
                        solver: Solver::new(),
                        fields: Default::default(),
                        emit: Vec::new(),
                        deferred: Default::default(),
                        scopes: IndexMap::from_iter([(root_scope_id, root_scope)]),
                        fallback_constraints: Vec::new(),
                        fallback_constraints_used: Vec::new(),
                        root_scope: root_scope_id,
                        nullspace_vecs: None,
                        objects: Default::default(),
                    }
                )
                .is_none()
        );

        let mut seq_num = SeqNum::new();
        for stmt in cell_decl.scope.stmts.iter() {
            let loc = DynLoc {
                cell: cell_id,
                frame: fid,
                scope: root_scope_id,
                seq_num,
            };
            match stmt {
                Statement::LetBinding(binding) => {
                    let value = self.visit_expr(loc, &binding.value);
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
                    self.cell_state_mut(loc.cell)
                        .scopes
                        .get_mut(&loc.scope)
                        .unwrap()
                        .bindings
                        .insert(loc.seq_num, (binding.name.name.to_string(), value));
                    seq_num = seq_num.next();
                }
                Statement::Expr { value, .. } => {
                    self.visit_expr(loc, value);
                }
            }
        }

        let mut require_progress = false;
        let mut progress = false;
        while {
            let state = self.cell_state(cell_id);
            !state.deferred.is_empty() || !state.solver.fully_solved()
        } {
            let deferred = self.cell_state(cell_id).deferred.clone();
            progress = false;
            for vid in deferred {
                progress = progress || self.eval_partial(vid);
            }

            if require_progress && !progress {
                let state = self.cell_state_mut(cell_id);
                if state.nullspace_vecs.is_none() {
                    state.nullspace_vecs = Some(state.solver.nullspace_vecs());
                }
                let mut constraint_added = false;
                while let Some(expr) = state.fallback_constraints.pop() {
                    if expr
                        .coeffs
                        .iter()
                        .any(|(c, v)| c.abs() > 1e-6 && !state.solver.is_solved(*v))
                    {
                        state.fallback_constraints_used.push(expr.clone());
                        state.solver.constrain_eq0(expr);
                        constraint_added = true;
                        break;
                    }
                }
                if !constraint_added {
                    panic!("no progress");
                }
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
        let emit_obj = |obj: &Object| -> SolvedValue {
            match obj {
                Object::Rect(rect) => SolvedValue::Rect(Rect {
                    id: rect.id,
                    layer: rect.layer.clone(),
                    x0: (state.solver.value_of(rect.x0).unwrap(), rect.x0),
                    y0: (state.solver.value_of(rect.y0).unwrap(), rect.y0),
                    x1: (state.solver.value_of(rect.x1).unwrap(), rect.x1),
                    y1: (state.solver.value_of(rect.y1).unwrap(), rect.y1),
                }),
                Object::Dimension(dim) => SolvedValue::Dimension(Dimension {
                    id: dim.id,
                    p: (state.solver.value_of(dim.p).unwrap(), dim.p),
                    n: (state.solver.value_of(dim.n).unwrap(), dim.n),
                    value: (state.solver.value_of(dim.value).unwrap(), dim.value),
                    coord: (state.solver.value_of(dim.coord).unwrap(), dim.coord),
                    pstop: (state.solver.value_of(dim.pstop).unwrap(), dim.pstop),
                    nstop: (state.solver.value_of(dim.nstop).unwrap(), dim.nstop),
                    horiz: dim.horiz,
                    span: dim.span,
                }),
                Object::Inst(inst) => SolvedValue::Instance(SolvedInstance {
                    id: inst.id,
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
                }),
            }
        };
        let emit_value = |vid: ValueId| -> Option<ObjectId> {
            let value = &self.values[&vid];
            let value = value.as_ref().unwrap_ready();
            match value {
                Value::Rect(r) => Some(r.id),
                Value::Inst(i) => Some(i.id),
                _ => None,
            }
        };

        let mut ccell = CompiledCell {
            scopes: IndexMap::new(),
            root: state.root_scope,
            fallback_constraints_used: state.fallback_constraints_used.clone(),
            nullspace_vecs: state.nullspace_vecs.clone().unwrap_or_default(),
            objects: IndexMap::new(),
        };
        fn add_scope(cell: &mut CompiledCell, state: &CellState, id: ScopeId, scope: &ExecScope) {
            if cell.scopes.contains_key(&id) {
                return;
            }
            if let Some(p) = scope.parent {
                add_scope(cell, state, p, &state.scopes[&p]);
                cell.scopes.get_mut(&p).unwrap().children.insert(id);
            }
            if let Some((p, _)) = scope.static_parent {
                add_scope(cell, state, p, &state.scopes[&p]);
            }
            cell.scopes.insert(
                id,
                CompiledScope {
                    static_parent: scope.static_parent,
                    bindings: Default::default(),
                    children: Default::default(),
                    name: scope.name.clone(),
                    span: scope.span,
                    emit: Vec::new(),
                },
            );
        }
        for (id, scope) in state.scopes.iter() {
            add_scope(&mut ccell, state, *id, scope);
        }

        for (id, obj) in state.objects.iter() {
            ccell.objects.insert(*id, emit_obj(obj));
        }

        for emit in state.emit.iter() {
            let obj_id = emit_value(emit.value).unwrap();
            ccell
                .scopes
                .get_mut(&emit.scope)
                .unwrap()
                .emit
                .push((obj_id, CompiledEmit { span: emit.span }));
        }

        for (id, scope) in state.scopes.iter() {
            for (seq_num, (name, value)) in scope.bindings.iter() {
                if let Some(obj_id) = emit_value(*value) {
                    ccell
                        .scopes
                        .get_mut(id)
                        .unwrap()
                        .bindings
                        .insert(*seq_num, (name.clone(), obj_id));
                }
            }
        }

        ccell
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

    fn scope_id(&mut self) -> ScopeId {
        ScopeId(self.alloc_id())
    }

    fn object_id(&mut self) -> ObjectId {
        ObjectId(self.alloc_id())
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

    fn eval_stmt(&mut self, loc: DynLoc, stmt: &Statement<&'a str, VarIdTyMetadata>) {
        match stmt {
            Statement::LetBinding(binding) => {
                let value = self.visit_expr(loc, &binding.value);
                self.frames
                    .get_mut(&loc.frame)
                    .unwrap()
                    .bindings
                    .insert(binding.metadata, value);
                self.cell_state_mut(loc.cell)
                    .scopes
                    .get_mut(&loc.scope)
                    .unwrap()
                    .bindings
                    .insert(loc.seq_num, (binding.name.name.to_string(), value));
            }
            Statement::Expr { value, .. } => {
                self.visit_expr(loc, value);
            }
        }
    }

    /// Create a new execution scope.
    ///
    /// parent is the dynamic parent scope.
    fn create_exec_scope(
        &mut self,
        cell_id: CellId,
        parent: ScopeId,
        static_parent: Option<(ScopeId, SeqNum)>,
        name: ExecScopeName,
        span: cfgrammar::Span,
    ) -> ScopeId {
        let id = self.scope_id();
        let name = match name {
            ExecScopeName::Specified(name) => name,
            ExecScopeName::Prefix(prefix) => format!("{} {}", prefix, id.0),
        };
        self.cell_state_mut(cell_id).scopes.insert(
            id,
            ExecScope {
                parent: Some(parent),
                static_parent,
                name,
                span,
                bindings: Default::default(),
            },
        );
        id
    }

    /// Create a new execution scope.
    ///
    /// The scope is inserted in the execution trace at the location specified by `loc`.
    /// The static and dynamic parents of the new scope both point to `loc`.
    fn create_exec_scope_at_loc(
        &mut self,
        loc: DynLoc,
        name: ExecScopeName,
        span: cfgrammar::Span,
    ) -> ScopeId {
        self.create_exec_scope(
            loc.cell,
            loc.scope,
            Some((loc.scope, loc.seq_num)),
            name,
            span,
        )
    }

    fn visit_scope_expr_inner(
        &mut self,
        cell_id: CellId,
        frame: FrameId,
        scope: ScopeId,
        s: &Scope<&'a str, VarIdTyMetadata>,
    ) -> ValueId {
        let mut seq_num = SeqNum::new();
        for stmt in &s.stmts {
            let loc = DynLoc {
                cell: cell_id,
                frame,
                scope,
                seq_num,
            };
            self.eval_stmt(loc, stmt);
            if matches!(stmt, Statement::LetBinding(_)) {
                seq_num = seq_num.next();
            }
        }

        let loc = DynLoc {
            cell: cell_id,
            frame,
            scope,
            seq_num,
        };
        s.tail
            .as_ref()
            .map(|tail| self.visit_expr(loc, tail))
            .unwrap_or(self.nil_value)
    }

    fn visit_expr(&mut self, loc: DynLoc, expr: &Expr<&'a str, VarIdTyMetadata>) -> ValueId {
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
            Expr::BoolLiteral(b) => {
                return if b.value {
                    self.true_value
                } else {
                    self.false_value
                };
            }
            Expr::StringLiteral(s) => {
                let vid = self.value_id();
                self.values
                    .insert(vid, Defer::Ready(Value::String(s.value.to_string())));
                return vid;
            }
            Expr::Var(v) => {
                let var_id = v.metadata.0;
                return self.lookup(loc.frame, var_id).unwrap();
            }
            Expr::Emit(e) => {
                let value = self.visit_expr(loc, &e.value);
                self.cell_state_mut(loc.cell).emit.push(Emit {
                    scope: loc.scope,
                    value,
                    span: e.span,
                });
                return value;
            }
            Expr::Call(c) => {
                if ["rect", "crect", "float", "inst", "eq", "dimension"].contains(&c.func.name) {
                    PartialEvalState::Call(Box::new(PartialCallExpr {
                        expr: c.clone(),
                        state: CallExprState {
                            posargs: c
                                .args
                                .posargs
                                .iter()
                                .map(|arg| self.visit_expr(loc, arg))
                                .collect(),
                            kwargs: c
                                .args
                                .kwargs
                                .iter()
                                .map(|arg| self.visit_expr(loc, &arg.value))
                                .collect(),
                        },
                    }))
                } else {
                    let arg_vals = c
                        .args
                        .posargs
                        .iter()
                        .map(|arg| self.visit_expr(loc, arg))
                        .collect_vec();
                    let val = &self.values[&self
                        .lookup(
                            loc.frame,
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
                            let new_scope = val.scope.clone();
                            let scope = self.create_exec_scope(
                                loc.cell,
                                loc.scope,
                                None,
                                ExecScopeName::Specified(format!("fn {}", val.name.name)),
                                val.scope.span,
                            );
                            let fid = self.frame_id();
                            self.frames.insert(fid, call_frame);
                            return self.visit_scope_expr_inner(loc.cell, fid, scope, &new_scope);
                        }
                        ValueRef::CellFn(_) => PartialEvalState::Call(Box::new(PartialCallExpr {
                            expr: c.clone(),
                            state: CallExprState {
                                posargs: arg_vals,
                                kwargs: c
                                    .args
                                    .kwargs
                                    .iter()
                                    .map(|arg| self.visit_expr(loc, &arg.value))
                                    .collect(),
                            },
                        })),
                        _ => todo!("cannot call value: not a function or cell generator"),
                    }
                }
            }
            Expr::If(if_expr) => {
                let cond = self.visit_expr(loc, &if_expr.cond);
                PartialEvalState::If(Box::new(PartialIfExpr {
                    expr: (**if_expr).clone(),
                    state: IfExprState::Cond(cond),
                }))
            }
            Expr::Comparison(comparison_expr) => {
                let left = self.visit_expr(loc, &comparison_expr.left);
                let right = self.visit_expr(loc, &comparison_expr.right);
                PartialEvalState::Comparison(Box::new(PartialComparisonExpr {
                    expr: (**comparison_expr).clone(),
                    state: ComparisonExprState { left, right },
                }))
            }
            Expr::Scope(s) => {
                let scope = self.create_exec_scope_at_loc(
                    loc,
                    if let Some(scope_annotation) = &s.scope_annotation {
                        ExecScopeName::Specified(scope_annotation.name.to_string())
                    } else {
                        ExecScopeName::Prefix("scope".to_string())
                    },
                    s.span,
                );
                return self.visit_scope_expr_inner(loc.cell, loc.frame, scope, s);
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
                let base = self.visit_expr(loc, &f.base);
                PartialEvalState::FieldAccess(Box::new(PartialFieldAccessExpr {
                    expr: (**f).clone(),
                    state: FieldAccessExprState { base },
                }))
            }
            Expr::BinOp(b) => {
                let lhs = self.visit_expr(loc, &b.left);
                let rhs = self.visit_expr(loc, &b.right);
                PartialEvalState::BinOp(PartialBinOp { lhs, rhs, op: b.op })
            }
            Expr::UnaryOp(u) => {
                let operand = self.visit_expr(loc, &u.operand);
                PartialEvalState::UnaryOp(PartialUnaryOp { operand, op: u.op })
            }
            Expr::Cast(cast) => {
                let value = self.visit_expr(loc, &cast.value);
                PartialEvalState::Cast(PartialCast {
                    value,
                    ty: cast.metadata.clone(),
                })
            }
        };
        let vid = self.value_id();
        self.cell_state_mut(loc.cell).deferred.insert(vid);
        self.values.insert(
            vid,
            DeferValue::Deferred(PartialEval {
                state: partial_eval_state,
                loc,
            }),
        );
        vid
    }

    fn eval_partial(&mut self, vid: ValueId) -> bool {
        let v = self.values.swap_remove(&vid);
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
        let state = self.cell_states.get_mut(&vref.loc.cell).unwrap();
        let progress = match &mut vref.state {
            PartialEvalState::Call(c) => match c.expr.func.name {
                "crect" | "rect" => {
                    let layer = c.state.posargs.first().map(|vid| {
                        self.values[vid]
                            .as_ref()
                            .get_ready()
                            .map(|layer| layer.as_ref().unwrap_string().clone())
                    });
                    let layer = match layer {
                        None => Some(None),
                        Some(None) => None,
                        Some(Some(l)) => Some(Some(l)),
                    };
                    if let Some(layer) = layer {
                        let id = self.object_id();
                        let state = self.cell_states.get_mut(&vref.loc.cell).unwrap();
                        let rect = Rect {
                            id,
                            layer,
                            x0: state.solver.new_var(),
                            y0: state.solver.new_var(),
                            x1: state.solver.new_var(),
                            y1: state.solver.new_var(),
                        };
                        self.values
                            .insert(vid, Defer::Ready(Value::Rect(rect.clone())));
                        state.objects.insert(rect.id, rect.clone().into());
                        for (kwarg, rhs) in c.expr.args.kwargs.iter().zip(c.state.kwargs.iter()) {
                            let lhs = self.value_id();
                            match kwarg.name.name {
                                "x0" | "x0i" => {
                                    self.values.insert(
                                        lhs,
                                        Defer::Ready(Value::Linear(LinearExpr::from(rect.x0))),
                                    );
                                }
                                "x1" | "x1i" => {
                                    self.values.insert(
                                        lhs,
                                        Defer::Ready(Value::Linear(LinearExpr::from(rect.x1))),
                                    );
                                }
                                "y0" | "y0i" => {
                                    self.values.insert(
                                        lhs,
                                        Defer::Ready(Value::Linear(LinearExpr::from(rect.y0))),
                                    );
                                }
                                "y1" | "y1i" => {
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
                                        fallback: kwarg.name.name.ends_with('i'),
                                    }),
                                    loc: vref.loc,
                                }),
                            );
                            self.cell_state_mut(vref.loc.cell).deferred.insert(defer);
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
                "dimension" => {
                    let args = c
                        .state
                        .posargs
                        .iter()
                        .map(|vid| self.values[vid].get_ready())
                        .collect::<Option<Vec<_>>>();
                    if let Some(args) = args {
                        assert_eq!(args.len(), 7);
                        let id = object_id(&mut self.next_id);
                        let state = self.cell_states.get_mut(&vref.loc.cell).unwrap();
                        let dim = Dimension {
                            id,
                            p: state.solver.new_var(),
                            n: state.solver.new_var(),
                            value: state.solver.new_var(),
                            coord: state.solver.new_var(),
                            pstop: state.solver.new_var(),
                            nstop: state.solver.new_var(),
                            horiz: *args[6].as_ref().unwrap_bool(),
                            span: Some(c.expr.span),
                        };
                        state.objects.insert(dim.id, dim.clone().into());
                        for (var, rhs) in [dim.p, dim.n, dim.value, dim.coord, dim.pstop, dim.nstop]
                            .iter()
                            .zip(args.iter().take(6))
                        {
                            let expr = LinearExpr::from(*var) - rhs.as_ref().unwrap_linear();
                            state.solver.constrain_eq0(expr);
                        }
                        let expr = LinearExpr::from(dim.p)
                            - LinearExpr::from(dim.n)
                            - LinearExpr::from(dim.value);
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
                        let id = object_id(&mut self.next_id);
                        let state = self.cell_states.get_mut(&vref.loc.cell).unwrap();
                        let inst = Instance {
                            id,
                            x: state.solver.new_var(),
                            y: state.solver.new_var(),
                            cell: *c.state.posargs.first().unwrap(),
                            reflect: refl.unwrap_or_default(),
                            angle: angle.unwrap_or_default(),
                        };
                        state.objects.insert(inst.id, inst.clone().into());
                        for (kwarg, rhs) in c.expr.args.kwargs.iter().zip(c.state.kwargs.iter()) {
                            let lhs = self.value_id();
                            match kwarg.name.name {
                                "x" | "xi" => {
                                    self.values.insert(
                                        lhs,
                                        Defer::Ready(Value::Linear(LinearExpr::from(inst.x))),
                                    );
                                }
                                "y" | "yi" => {
                                    self.values.insert(
                                        lhs,
                                        Defer::Ready(Value::Linear(LinearExpr::from(inst.y))),
                                    );
                                }
                                _ => continue,
                            };
                            let defer = self.value_id();
                            self.values.insert(
                                defer,
                                DeferValue::Deferred(PartialEval {
                                    state: PartialEvalState::Constraint(PartialConstraint {
                                        lhs,
                                        rhs: *rhs,
                                        fallback: kwarg.name.name.ends_with('i'),
                                    }),
                                    loc: vref.loc,
                                }),
                            );
                            self.cell_state_mut(vref.loc.cell).deferred.insert(defer);
                        }
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
                            self.values[v].get_ready().and_then(|v| match v {
                                Value::Linear(v) => state.solver.eval_expr(v).map(CellArg::Float),
                                Value::Int(i) => Some(CellArg::Int(*i)),
                                _ => unreachable!(),
                            })
                        })
                        .collect::<Option<Vec<CellArg>>>();
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
                        _ => todo!(),
                    }
                } else {
                    false
                }
            }
            PartialEvalState::UnaryOp(unary_op) => {
                if let Defer::Ready(v) = &self.values[&unary_op.operand] {
                    match v {
                        Value::Linear(v) => {
                            let res = match unary_op.op {
                                UnaryOp::Neg => LinearExpr {
                                    coeffs: v
                                        .coeffs
                                        .iter()
                                        .map(|(coeff, var)| (-coeff, *var))
                                        .collect(),
                                    constant: -v.constant,
                                },
                                _ => unreachable!(),
                            };
                            self.values
                                .insert(vid, DeferValue::Ready(Value::Linear(res)));
                            true
                        }
                        Value::Int(v) => {
                            let res = match unary_op.op {
                                UnaryOp::Neg => -v,
                                _ => unreachable!(),
                            };
                            self.values.insert(vid, DeferValue::Ready(Value::Int(res)));
                            true
                        }
                        _ => todo!(),
                    }
                } else {
                    false
                }
            }
            PartialEvalState::If(if_) => match if_.state {
                IfExprState::Cond(cond) => {
                    if let Defer::Ready(val) = &self.values[&cond] {
                        if *val.as_ref().unwrap_bool() {
                            let scope = self.create_exec_scope_at_loc(
                                vref.loc,
                                if let Some(scope_annotation) = &if_.expr.scope_annotation {
                                    ExecScopeName::Specified(format!(
                                        "{} if",
                                        scope_annotation.name
                                    ))
                                } else {
                                    ExecScopeName::Prefix("if".to_string())
                                },
                                if_.expr.then.span,
                            );
                            let then = self.visit_scope_expr_inner(
                                vref.loc.cell,
                                vref.loc.frame,
                                scope,
                                &if_.expr.then,
                            );
                            if_.state = IfExprState::Then(then);
                        } else {
                            let scope = self.create_exec_scope_at_loc(
                                vref.loc,
                                if let Some(scope_annotation) = &if_.expr.scope_annotation {
                                    ExecScopeName::Specified(format!(
                                        "{} else",
                                        scope_annotation.name
                                    ))
                                } else {
                                    ExecScopeName::Prefix("else".to_string())
                                },
                                if_.expr.else_.span,
                            );
                            let else_ = self.visit_scope_expr_inner(
                                vref.loc.cell,
                                vref.loc.frame,
                                scope,
                                &if_.expr.else_,
                            );
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
                                "layer" => Value::String(rect.layer.clone().unwrap()),
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
                                        match cell.field(field).unwrap() {
                                            SolvedValue::Rect(rect) => {
                                                let id = object_id(&mut self.next_id);
                                                let state = self
                                                    .cell_states
                                                    .get_mut(&vref.loc.cell)
                                                    .unwrap();
                                                let xrect = Rect {
                                                    id,
                                                    layer: rect.layer.clone(),
                                                    x0: state.solver.new_var(),
                                                    y0: state.solver.new_var(),
                                                    x1: state.solver.new_var(),
                                                    y1: state.solver.new_var(),
                                                };
                                                state
                                                    .objects
                                                    .insert(xrect.id, xrect.clone().into());
                                                let rect = rect
                                                    .to_float()
                                                    .transform(inst.reflect, inst.angle);
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
                                                let state = self.cell_state_mut(vref.loc.cell);
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
                                                let id = object_id(&mut self.next_id);
                                                let state = self
                                                    .cell_states
                                                    .get_mut(&vref.loc.cell)
                                                    .unwrap();
                                                let oinst = Instance {
                                                    id,
                                                    cell: cinst.cell_vid,
                                                    x: state.solver.new_var(),
                                                    y: state.solver.new_var(),
                                                    angle,
                                                    reflect,
                                                };
                                                state
                                                    .objects
                                                    .insert(oinst.id, oinst.clone().into());
                                                let dx = LinearExpr::from(inst.x) + cx
                                                    - LinearExpr::from(oinst.x);
                                                let dy = LinearExpr::from(inst.y) + cy
                                                    - LinearExpr::from(oinst.y);
                                                state.solver.constrain_eq0(dx);
                                                state.solver.constrain_eq0(dy);
                                                Some(Value::Inst(oinst))
                                            }
                                            _ => unreachable!(),
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
                    if c.fallback {
                        state.fallback_constraints.push(expr);
                    } else {
                        state.solver.constrain_eq0(expr);
                    }
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

        let cell_id = vref.loc.cell;
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
    String(String),
    Linear(LinearExpr),
    Int(i64),
    Rect(Rect<Var>),
    Bool(bool),
    Fn(FnDecl<&'a str, VarIdTyMetadata>),
    /// A cell generator.
    ///
    /// Example:
    /// ```argon
    /// cell mycell() {
    ///   // ...
    /// }
    /// ```
    ///
    /// `mycell` is a value of type `CellFn`.
    CellFn(CellDecl<&'a str, VarIdTyMetadata>),
    /// A particular parameterization of a cell.
    ///
    /// Example:
    /// ```argon
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
    /// ```argon
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

impl<'a> Value<'a> {
    pub fn to_obj(&self) -> Option<Object> {
        match self {
            Self::Rect(r) => Some(Object::Rect(r.clone())),
            Self::Inst(i) => Some(Object::Inst(i.clone())),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Instance {
    pub id: ObjectId,
    pub x: Var,
    pub y: Var,
    pub cell: ValueId,
    pub reflect: bool,
    pub angle: Rotation,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SolvedInstance {
    pub id: ObjectId,
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
    Rect(Rect<(f64, Var)>),
    Dimension(Dimension<(f64, Var)>),
    Instance(SolvedInstance),
}

#[enumify]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Object {
    Rect(Rect<Var>),
    Dimension(Dimension<Var>),
    Inst(Instance),
}

impl From<Rect<Var>> for Object {
    fn from(value: Rect<Var>) -> Self {
        Self::Rect(value)
    }
}

impl From<Dimension<Var>> for Object {
    fn from(value: Dimension<Var>) -> Self {
        Self::Dimension(value)
    }
}

impl From<Instance> for Object {
    fn from(value: Instance) -> Self {
        Self::Inst(value)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompiledScope {
    pub static_parent: Option<(ScopeId, SeqNum)>,
    pub bindings: IndexMap<SeqNum, (String, ObjectId)>,
    /// Dynamic children.
    pub children: IndexSet<ScopeId>,
    pub name: String,
    pub span: cfgrammar::Span,
    /// Objects emitted in this scope.
    pub emit: Vec<(ObjectId, CompiledEmit)>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompiledCell {
    pub scopes: IndexMap<ScopeId, CompiledScope>,
    pub root: ScopeId,
    pub objects: IndexMap<ObjectId, SolvedValue>,
    pub fallback_constraints_used: Vec<LinearExpr>,
    pub nullspace_vecs: Vec<Vec<f64>>,
}

impl CompiledCell {
    pub fn field(&self, name: &str) -> Option<&SolvedValue> {
        let scope = &self.scopes[&self.root];
        scope.bindings.values().find_map(|(n, o)| {
            if n == name {
                Some(&self.objects[o])
            } else {
                None
            }
        })
    }
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
#[enumify]
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
    pub cells: IndexMap<CellId, CompiledCell>,
    pub top: CellId,
    pub layers: LayerProperties,
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
    loc: DynLoc,
}

#[derive(Debug, Clone)]
enum PartialEvalState<'a, T: AstMetadata> {
    If(Box<PartialIfExpr<'a, T>>),
    Comparison(Box<PartialComparisonExpr<'a, T>>),
    BinOp(PartialBinOp),
    UnaryOp(PartialUnaryOp),
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
    fallback: bool,
}

#[derive(Debug, Clone)]
struct PartialBinOp {
    lhs: ValueId,
    rhs: ValueId,
    op: BinOp,
}

#[derive(Debug, Clone)]
struct PartialUnaryOp {
    operand: ValueId,
    op: UnaryOp,
}

#[derive(Debug, Clone)]
struct PartialIfExpr<'a, T: AstMetadata> {
    expr: IfExpr<&'a str, T>,
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
    expr: CallExpr<&'a str, T>,
    state: CallExprState,
}

#[derive(Debug, Clone)]
pub struct CallExprState {
    posargs: Vec<ValueId>,
    kwargs: Vec<ValueId>,
}

#[derive(Debug, Clone)]
struct PartialComparisonExpr<'a, T: AstMetadata> {
    expr: ComparisonExpr<&'a str, T>,
    state: ComparisonExprState,
}

#[derive(Debug, Clone)]
pub struct ComparisonExprState {
    left: ValueId,
    right: ValueId,
}

#[derive(Debug, Clone)]
struct PartialFieldAccessExpr<'a, T: AstMetadata> {
    expr: FieldAccessExpr<&'a str, T>,
    state: FieldAccessExprState,
}

#[derive(Debug, Clone)]
pub struct FieldAccessExprState {
    base: ValueId,
}

pub fn ifmatvec(mat: TransformationMatrix, pt: (f64, f64)) -> (f64, f64) {
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

impl Rect<(f64, Var)> {
    pub fn to_float(&self) -> Rect<f64> {
        Rect {
            id: self.id,
            layer: self.layer.clone(),
            x0: self.x0.0,
            y0: self.y0.0,
            x1: self.x1.0,
            y1: self.y1.0,
        }
    }
}

impl Rect<f64> {
    fn transform(&self, reflect_vert: bool, angle: Rotation) -> Self {
        let mat = tmat(angle, reflect_vert);
        let p0p = ifmatvec(mat, (self.x0, self.y0));
        let p1p = ifmatvec(mat, (self.x1, self.y1));
        Self {
            id: self.id,
            layer: self.layer.clone(),
            x0: p0p.0.min(p1p.0),
            y0: p0p.1.min(p1p.1),
            x1: p0p.0.max(p1p.0),
            y1: p0p.1.max(p1p.1),
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

impl SeqNum {
    #[inline]
    fn new() -> Self {
        Self(0)
    }

    #[inline]
    fn next(&self) -> Self {
        Self(self.0 + 1)
    }

    /// The sequence number corresponding to the end of a scope.
    ///
    /// Currently implemented as [`u64::MAX`].
    #[inline]
    fn end() -> Self {
        Self(u64::MAX)
    }
}

fn object_id(id: &mut u64) -> ObjectId {
    let next_id = *id;
    *id += 1;
    ObjectId(next_id)
}

impl ValidCompileOutput {
    pub fn reachable_objs(&self, cell: CellId, scope: ScopeId) -> IndexMap<ObjectId, String> {
        let mut set = Default::default();
        self.reachable_objs_inner(cell, scope, SeqNum::end(), "", &mut set);
        set
    }

    fn reachable_objs_inner(
        &self,
        cell_id: CellId,
        scope_id: ScopeId,
        seq_num: SeqNum,
        name_prefix: &str,
        set: &mut IndexMap<ObjectId, String>,
    ) {
        let cell = &self.cells[&cell_id];
        let scope = &cell.scopes[&scope_id];
        if let Some((parent, seq_num)) = scope.static_parent {
            self.reachable_objs_inner(cell_id, parent, seq_num, name_prefix, set);
        }
        for (item_num, (name, obj)) in scope.bindings.iter() {
            if *item_num < seq_num {
                match &cell.objects[obj] {
                    SolvedValue::Rect(r) => {
                        set.insert(r.id, format!("{}{}", name_prefix, name));
                    }
                    SolvedValue::Instance(inst) => {
                        set.insert(inst.id, format!("{}{}", name_prefix, name));
                    }
                    _ => (),
                }
            }
        }
    }
}
