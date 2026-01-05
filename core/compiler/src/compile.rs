//! # Argon compiler
//
//! Pass 1: import resolution
//! Pass 2: assign variable IDs/type checking
//! Pass 3: solving
use std::collections::{BinaryHeap, VecDeque};
use std::io::BufReader;
use std::path::{Path, PathBuf};

use arcstr::Substr;
use enumify::enumify;
use geometry::transform::{Rotation, TransformationMatrix};
use indexmap::{IndexMap, IndexSet};
use itertools::Itertools;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::ast::annotated::AnnotatedAst;
use crate::ast::{
    BinOp, ComparisonOp, ConstantDecl, EnumDecl, FieldAccessExpr, FnDecl, IdentPath, KwArgValue,
    MatchExpr, ModPath, Scope, Span, TySpec, TySpecKind, UnaryOp, UnaryOpExpr, WorkspaceAst,
};
use crate::layer::LayerProperties;
use crate::parse::WorkspaceParseAst;
use crate::solver::{ConstraintId, Var};
use crate::{
    ast::{
        ArgDecl, Ast, AstMetadata, AstTransformer, BinOpExpr, CallExpr, CellDecl, ComparisonExpr,
        Decl, Expr, Ident, IfExpr, LetBinding, Statement,
    },
    parse::ParseMetadata,
    solver::{LinearExpr, Solver},
};

pub const BUILTINS: [&str; 11] = [
    "cons",
    "head",
    "tail",
    "crect",
    "rect",
    "text",
    "float",
    "eq",
    "dimension",
    "inst",
    "bbox",
];

pub fn static_compile(
    ast: &WorkspaceParseAst,
) -> Option<(WorkspaceAst<VarIdTyMetadata>, StaticErrorCompileOutput)> {
    if !ast.contains_key(&vec![]) {
        return None;
    }
    let (dag, mut errors) = construct_dag(ast);
    let (ast, new_errors) = execute_var_id_ty_pass(ast, &dag);
    errors.extend(new_errors);
    Some((ast, StaticErrorCompileOutput { errors }))
}

pub fn dynamic_compile(
    ast: &WorkspaceAst<VarIdTyMetadata>,
    input: CompileInput<'_>,
) -> CompileOutput {
    let res = ExecPass::new(ast).execute(input);
    let (data, mut errors) = match res {
        CompileOutput::ExecErrors(ExecErrorCompileOutput { errors, output }) => {
            if let Some(output) = output {
                (output, errors)
            } else {
                return CompileOutput::ExecErrors(ExecErrorCompileOutput { errors, output });
            }
        }
        CompileOutput::Valid(v) => (v, Vec::new()),
        o => return o,
    };
    check_layers(&data, &mut errors);
    if errors.is_empty() {
        CompileOutput::Valid(data)
    } else {
        CompileOutput::ExecErrors(ExecErrorCompileOutput {
            errors,
            output: Some(data),
        })
    }
}

pub fn compile(ast: &WorkspaceParseAst, input: CompileInput<'_>) -> CompileOutput {
    let (ast, static_output) = if let Some(static_output) = static_compile(ast) {
        static_output
    } else {
        return CompileOutput::FatalParseErrors;
    };
    if !static_output.errors.is_empty() {
        return CompileOutput::StaticErrors(static_output);
    };

    dynamic_compile(&ast, input)
}

type ModDag<'a> = IndexMap<&'a ModPath, IndexSet<&'a ModPath>>;

pub(crate) struct ImportPass<'a> {
    ast: &'a WorkspaceParseAst,
    current_path: &'a ModPath,
    deps: IndexSet<&'a ModPath>,
    errors: Vec<StaticError>,
}

pub(crate) fn construct_dag(ast: &WorkspaceParseAst) -> (ModDag<'_>, Vec<StaticError>) {
    let mut errors = Vec::new();
    (
        ast.keys()
            .map(|path| {
                let (children, new_errors) = ImportPass::new(ast, path).execute();
                errors.extend(new_errors);

                (path, children)
            })
            .collect(),
        errors,
    )
}

impl<'a> ImportPass<'a> {
    fn new(ast: &'a WorkspaceParseAst, current_path: &'a ModPath) -> Self {
        Self {
            ast,
            current_path,
            deps: Default::default(),
            errors: Default::default(),
        }
    }

    fn span(&self, span: cfgrammar::Span) -> Span {
        Span {
            path: self.ast[self.current_path].path.clone(),
            span,
        }
    }

    pub(crate) fn execute(mut self) -> (IndexSet<&'a ModPath>, Vec<StaticError>) {
        for decl in &self.ast[self.current_path].ast.decls {
            match decl {
                Decl::Fn(f) => {
                    self.transform_fn_decl(f);
                }
                Decl::Cell(c) => {
                    self.transform_cell_decl(c);
                }
                Decl::Mod(_) => {}
                Decl::Enum(_) => {}
                _ => todo!(),
            }
        }

        (self.deps, self.errors)
    }
}

impl<'a> AstTransformer for ImportPass<'a> {
    type InputMetadata = ParseMetadata;
    type OutputMetadata = ParseMetadata;
    type InputS = Substr;
    type OutputS = Substr;

    fn dispatch_ident(
        &mut self,
        _input: &Ident<Self::InputS, Self::InputMetadata>,
    ) -> <Self::OutputMetadata as AstMetadata>::Ident {
    }

    fn dispatch_ident_path(
        &mut self,
        _input: &IdentPath<Self::InputS, Self::InputMetadata>,
    ) -> <Self::OutputMetadata as AstMetadata>::IdentPath {
    }

    fn dispatch_enum_decl(
        &mut self,
        _input: &crate::ast::EnumDecl<Self::InputS, Self::InputMetadata>,
        _name: &Ident<Self::OutputS, Self::OutputMetadata>,
        _variants: &[Ident<Self::OutputS, Self::OutputMetadata>],
    ) -> <Self::OutputMetadata as AstMetadata>::EnumDecl {
    }

    fn dispatch_cell_decl(
        &mut self,
        _input: &CellDecl<Self::InputS, Self::InputMetadata>,
        _name: &Ident<Self::OutputS, Self::OutputMetadata>,
        _args: &[ArgDecl<Self::OutputS, Self::OutputMetadata>],
        _scope: &Scope<Self::OutputS, Self::OutputMetadata>,
    ) -> <Self::OutputMetadata as AstMetadata>::CellDecl {
    }

    fn dispatch_fn_decl(
        &mut self,
        _input: &FnDecl<Self::InputS, Self::InputMetadata>,
        _name: &Ident<Self::OutputS, Self::OutputMetadata>,
        _args: &[ArgDecl<Self::OutputS, Self::OutputMetadata>],
        _return_ty: &Option<TySpec<Self::OutputS, Self::OutputMetadata>>,
        _scope: &Scope<Self::OutputS, Self::OutputMetadata>,
    ) -> <Self::OutputMetadata as AstMetadata>::FnDecl {
    }

    fn dispatch_constant_decl(
        &mut self,
        _input: &ConstantDecl<Self::InputS, Self::InputMetadata>,
        _name: &Ident<Self::OutputS, Self::OutputMetadata>,
        _ty: &Ident<Self::OutputS, Self::OutputMetadata>,
        _value: &Expr<Self::OutputS, Self::OutputMetadata>,
    ) -> <Self::OutputMetadata as AstMetadata>::ConstantDecl {
    }

    fn dispatch_let_binding(
        &mut self,
        _input: &LetBinding<Self::InputS, Self::InputMetadata>,
        _name: &Ident<Self::OutputS, Self::OutputMetadata>,
        _value: &Expr<Self::OutputS, Self::OutputMetadata>,
    ) -> <Self::OutputMetadata as AstMetadata>::LetBinding {
    }

    fn dispatch_if_expr(
        &mut self,
        _input: &IfExpr<Self::InputS, Self::InputMetadata>,
        _cond: &Expr<Self::OutputS, Self::OutputMetadata>,
        _then: &Scope<Self::OutputS, Self::OutputMetadata>,
        _else_: &Scope<Self::OutputS, Self::OutputMetadata>,
    ) -> <Self::OutputMetadata as AstMetadata>::IfExpr {
    }

    fn dispatch_match_expr(
        &mut self,
        _input: &crate::ast::MatchExpr<Self::InputS, Self::InputMetadata>,
        _scrutinee: &Expr<Self::OutputS, Self::OutputMetadata>,
        _arms: &[crate::ast::MatchArm<Self::OutputS, Self::OutputMetadata>],
    ) -> <Self::OutputMetadata as AstMetadata>::MatchExpr {
    }

    fn dispatch_bin_op_expr(
        &mut self,
        _input: &BinOpExpr<Self::InputS, Self::InputMetadata>,
        _left: &Expr<Self::OutputS, Self::OutputMetadata>,
        _right: &Expr<Self::OutputS, Self::OutputMetadata>,
    ) -> <Self::OutputMetadata as AstMetadata>::BinOpExpr {
    }

    fn dispatch_unary_op_expr(
        &mut self,
        _input: &crate::ast::UnaryOpExpr<Self::InputS, Self::InputMetadata>,
        _operand: &Expr<Self::OutputS, Self::OutputMetadata>,
    ) -> <Self::OutputMetadata as AstMetadata>::UnaryOpExpr {
    }

    fn dispatch_comparison_expr(
        &mut self,
        _input: &ComparisonExpr<Self::InputS, Self::InputMetadata>,
        _left: &Expr<Self::OutputS, Self::OutputMetadata>,
        _right: &Expr<Self::OutputS, Self::OutputMetadata>,
    ) -> <Self::OutputMetadata as AstMetadata>::ComparisonExpr {
    }

    fn dispatch_cast(
        &mut self,
        _input: &crate::ast::CastExpr<Self::InputS, Self::InputMetadata>,
        _value: &Expr<Self::OutputS, Self::OutputMetadata>,
        _ty: &TySpec<Self::OutputS, Self::OutputMetadata>,
    ) -> <Self::OutputMetadata as AstMetadata>::CastExpr {
    }

    fn dispatch_field_access_expr(
        &mut self,
        _input: &FieldAccessExpr<Self::InputS, Self::InputMetadata>,
        _base: &Expr<Self::OutputS, Self::OutputMetadata>,
        _field: &Ident<Self::OutputS, Self::OutputMetadata>,
    ) -> <Self::OutputMetadata as AstMetadata>::FieldAccessExpr {
    }

    fn dispatch_call_expr(
        &mut self,
        _input: &CallExpr<Self::InputS, Self::InputMetadata>,
        func: &IdentPath<Self::OutputS, Self::OutputMetadata>,
        _args: &crate::ast::Args<Self::OutputS, Self::OutputMetadata>,
    ) -> <Self::OutputMetadata as AstMetadata>::CallExpr {
        if func.path[0].name != "std" {
            let path = if func.path[0].name == "crate" {
                func.path
                    .iter()
                    .skip(1)
                    .dropping_back(1)
                    .map(|ident| ident.name.to_string())
                    .collect_vec()
            } else {
                self.current_path
                    .iter()
                    .cloned()
                    .chain(
                        func.path
                            .iter()
                            .dropping_back(1)
                            .map(|ident| ident.name.to_string()),
                    )
                    .collect_vec()
            };
            if let Some((path_ref, _)) = self.ast.get_key_value(&path) {
                self.deps.insert(path_ref);
            } else {
                self.errors.push(StaticError {
                    span: self.span(func.span),
                    kind: StaticErrorKind::InvalidMod,
                });
            }
        }
    }

    fn dispatch_emit_expr(
        &mut self,
        _input: &crate::ast::EmitExpr<Self::InputS, Self::InputMetadata>,
        _value: &Expr<Self::OutputS, Self::OutputMetadata>,
    ) -> <Self::OutputMetadata as AstMetadata>::EmitExpr {
    }

    fn dispatch_args(
        &mut self,
        _input: &crate::ast::Args<Self::InputS, Self::InputMetadata>,
        _posargs: &[Expr<Self::OutputS, Self::OutputMetadata>],
        _kwargs: &[crate::ast::KwArgValue<Self::OutputS, Self::OutputMetadata>],
    ) -> <Self::OutputMetadata as AstMetadata>::Args {
    }

    fn dispatch_kw_arg_value(
        &mut self,
        _input: &crate::ast::KwArgValue<Self::InputS, Self::InputMetadata>,
        _name: &Ident<Self::OutputS, Self::OutputMetadata>,
        _value: &Expr<Self::OutputS, Self::OutputMetadata>,
    ) -> <Self::OutputMetadata as AstMetadata>::KwArgValue {
    }

    fn dispatch_arg_decl(
        &mut self,
        _input: &ArgDecl<Self::InputS, Self::InputMetadata>,
        _name: &Ident<Self::OutputS, Self::OutputMetadata>,
        _ty: &TySpec<Self::OutputS, Self::OutputMetadata>,
    ) -> <Self::OutputMetadata as AstMetadata>::ArgDecl {
    }

    fn dispatch_scope(
        &mut self,
        _input: &Scope<Self::InputS, Self::InputMetadata>,
        _stmts: &[Statement<Self::OutputS, Self::OutputMetadata>],
        _tail: &Option<Expr<Self::OutputS, Self::OutputMetadata>>,
    ) -> <Self::OutputMetadata as AstMetadata>::Scope {
    }

    fn transform_s(&mut self, s: &Self::InputS) -> Self::OutputS {
        s.clone()
    }
}

fn check_layers(data: &CompiledData, errs: &mut Vec<ExecError>) {
    let mut layers = IndexSet::new();
    for layer in data.layers.layers.iter() {
        layers.insert(layer.name.clone());
    }
    for (cell_id, cell) in data.cells.iter() {
        for (_, obj) in cell.objects.iter() {
            if let SolvedValue::Rect(r) = obj
                && let Some(layer) = &r.layer
                && !layers.contains(layer)
            {
                errs.push(ExecError {
                    span: r.span.clone(),
                    cell: *cell_id,
                    kind: ExecErrorKind::IllegalLayer(layer.clone()),
                })
            }
        }
    }
}

#[derive(Default, Debug)]
pub(crate) struct VarIdTyFrame {
    var_bindings: IndexMap<Substr, (VarId, Ty)>,
    scope_bindings: IndexSet<Substr>,
}

pub(crate) struct VarIdTyPass<'a> {
    ast: &'a AnnotatedAst<ParseMetadata>,
    mod_bindings: &'a IndexMap<&'a ModPath, VarIdTyFrame>,
    current_path: &'a ModPath,
    next_id: VarId,
    bindings: Vec<VarIdTyFrame>,
    errors: Vec<StaticError>,
}

pub(crate) fn execute_var_id_ty_pass<'a>(
    ast: &'a WorkspaceParseAst,
    dag: &'a ModDag<'a>,
) -> (WorkspaceAst<VarIdTyMetadata>, Vec<StaticError>) {
    let mut mod_bindings = IndexMap::new();
    let mut workspace_ast = IndexMap::new();
    let mut errors = Vec::new();
    let mut next_id = 1;
    let std_mod_path = vec!["std".to_string()];
    let std_mod_path = ast.get_key_value(&std_mod_path).map(|(k, _)| k);
    if let Some((root, _)) = ast.get_key_value(&vec![]) {
        for path in [std_mod_path, Some(root)].iter().flatten() {
            execute_var_id_ty_pass_inner(
                ast,
                dag,
                path,
                &mut mod_bindings,
                &mut workspace_ast,
                &mut errors,
                &mut next_id,
            );
        }
    }
    (workspace_ast, errors)
}
pub(crate) fn execute_var_id_ty_pass_inner<'a>(
    ast: &'a WorkspaceParseAst,
    dag: &'a ModDag<'a>,
    current_path: &'a ModPath,
    mod_bindings: &mut IndexMap<&'a ModPath, VarIdTyFrame>,
    workspace_ast: &mut WorkspaceAst<VarIdTyMetadata>,
    errors: &mut Vec<StaticError>,
    next_id: &mut VarId,
) {
    // TODO: fix hacky way to track visited modules.
    if mod_bindings.contains_key(&current_path) {
        return;
    }
    mod_bindings.insert(current_path, VarIdTyFrame::default());

    if current_path
        .first()
        .map(|path| path == "std")
        .unwrap_or(true)
    {
        for children in &dag[&current_path] {
            execute_var_id_ty_pass_inner(
                ast,
                dag,
                children,
                mod_bindings,
                workspace_ast,
                errors,
                next_id,
            );
        }
    }

    let mut pass = VarIdTyPass {
        ast: &ast[current_path],
        mod_bindings,
        current_path,
        next_id: *next_id,
        bindings: vec![VarIdTyFrame::default()],
        errors: vec![],
    };
    let ast = pass.execute();
    workspace_ast.insert(current_path.clone(), ast);
    errors.extend(pass.errors);
    *next_id = pass.next_id;
    mod_bindings.insert(current_path, pass.bindings.into_iter().next().unwrap());
}

#[derive(Debug, Clone)]
pub struct VarIdTyMetadata;

#[enumify]
#[derive(Default, Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Ty {
    /// A type that does not exist; usually encountered due to user error.
    ///
    /// Suppresses type checking of dependent properties.
    #[default]
    Unknown,
    /// Catch-all any type.
    ///
    /// Should eventually be removed.
    Any,
    Bool,
    Float,
    Int,
    Rect,
    String,
    Cell(Box<CellTy>),
    Inst(Box<CellTy>),
    Nil,
    SeqNil,
    Fn(Box<FnTy>),
    /// An enum variant type, e.g. the type of `MyEnum::MyVariant`.
    Enum(EnumTy),
    CellFn(Box<CellFnTy>),
    Seq(Box<Ty>),
}

impl Ty {
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "Float" => Some(Ty::Float),
            "Rect" => Some(Ty::Rect),
            "Any" => Some(Ty::Any),
            "Int" => Some(Ty::Int),
            "String" => Some(Ty::String),
            "()" => Some(Ty::Nil),
            "[]" => Some(Ty::SeqNil),
            _ => None,
        }
    }

    /// Computes the least upper bound (LUB) of self and other.
    /// For use in type promotion.
    pub fn lub(&self, other: &Self) -> Option<Self> {
        match (self, other) {
            // Unknown promotes to any type.
            (Ty::Unknown, other) | (other, Ty::Unknown) => Some(other.clone()),
            // SeqNil promotes to any sequence type.
            (Ty::SeqNil, Ty::Seq(inner)) | (Ty::Seq(inner), Ty::SeqNil) => {
                Some(Ty::Seq(inner.clone()))
            }
            // No other types promote.
            (a, b) => {
                if a == b {
                    Some(a.clone())
                } else {
                    None
                }
            }
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnumTy {
    id: EnumId,
    variants: IndexSet<String>,
}

impl AstMetadata for VarIdTyMetadata {
    type Ident = ();
    type IdentPath = (Option<VarId>, Ty);
    type EnumDecl = ();
    type StructDecl = ();
    type StructField = ();
    type CellDecl = (PathBuf, VarId);
    type ConstantDecl = ();
    type LetBinding = VarId;
    type FnDecl = (PathBuf, VarId);
    type IfExpr = Ty;
    type MatchExpr = Ty;
    type BinOpExpr = Ty;
    type UnaryOpExpr = Ty;
    type ComparisonExpr = Ty;
    type FieldAccessExpr = Ty;
    type CallExpr = (Option<VarId>, Ty);
    type EmitExpr = Ty;
    type Args = ();
    type KwArgValue = Ty;
    type ArgDecl = (VarId, Ty);
    type Scope = Ty;
    type Typ = ();
    type CastExpr = Ty;
}

impl<'a> VarIdTyPass<'a> {
    fn span(&self, span: cfgrammar::Span) -> Span {
        Span {
            path: self.ast.path.clone(),
            span,
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

    fn alloc_id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    fn alloc(&mut self, name: &Substr, ty: Ty) -> VarId {
        let id = self.alloc_id();
        self.bindings
            .last_mut()
            .unwrap()
            .var_bindings
            .insert(name.clone(), (id, ty));
        id
    }

    fn execute(&mut self) -> AnnotatedAst<VarIdTyMetadata> {
        let mut decls = Vec::new();
        for decl in &self.ast.ast.decls {
            match decl {
                Decl::Fn(f) => self.declare_fn_decl(f),
                Decl::Enum(e) => self.declare_enum_decl(e),
                _ => (),
            }
        }

        for decl in &self.ast.ast.decls {
            match decl {
                Decl::Fn(f) => {
                    decls.push(Decl::Fn(self.transform_fn_decl(f)));
                }
                Decl::Cell(c) => {
                    decls.push(Decl::Cell(self.transform_cell_decl(c)));
                }
                Decl::Mod(m) => {
                    decls.push(Decl::Mod(self.transform_mod_decl(m)));
                }
                Decl::Enum(e) => {
                    decls.push(Decl::Enum(self.transform_enum_decl(e)));
                }
                _ => todo!(),
            }
        }

        AnnotatedAst::new(
            self.ast.text.clone(),
            &Ast {
                decls,
                span: self.ast.ast.span,
            },
            self.ast.path.clone(),
        )
    }

    fn declare_fn_decl(&mut self, input: &'a FnDecl<Substr, ParseMetadata>) {
        if BUILTINS.contains(&input.name.name.as_str()) {
            self.errors.push(StaticError {
                span: self.span(input.name.span),
                kind: StaticErrorKind::RedeclarationOfBuiltin,
            });
        }
        let args: Vec<_> = input
            .args
            .iter()
            .map(|arg| {
                let ty_spec = self.transform_ty_spec(&arg.ty);
                self.ty_from_spec(&ty_spec)
            })
            .collect();
        let ty = Ty::Fn(Box::new(FnTy {
            args,
            ret: if let Some(return_ty) = &input.return_ty {
                self.ty_from_spec(return_ty)
            } else {
                Ty::Nil
            },
        }));
        self.alloc(&input.name.name, ty);
    }

    fn declare_enum_decl(&mut self, input: &'a EnumDecl<Substr, ParseMetadata>) {
        if BUILTINS.contains(&input.name.name.as_str()) {
            self.errors.push(StaticError {
                span: self.span(input.name.span),
                kind: StaticErrorKind::RedeclarationOfBuiltin,
            });
            return;
        }
        let mut variants = IndexSet::with_capacity(input.variants.len());
        for variant in input.variants.iter() {
            if variants.contains(variant.name.as_str()) {
                self.errors.push(StaticError {
                    span: self.span(variant.span),
                    kind: StaticErrorKind::DuplicateNameDeclaration,
                });
            }
            variants.insert(variant.name.to_string());
        }
        let ty = Ty::Enum(EnumTy {
            id: self.alloc_id(),
            variants,
        });
        self.alloc(&input.name.name, ty);
    }

    fn ty_from_spec<M: AstMetadata>(&mut self, spec: &TySpec<Substr, M>) -> Ty {
        match &spec.kind {
            TySpecKind::Ident(ident) => Ty::from_name(ident.name.as_str()).unwrap_or_else(|| {
                if let Some((_, ty)) = self.lookup(ident.name.as_str()) {
                    ty
                } else {
                    self.errors.push(StaticError {
                        span: self.span(ident.span),
                        kind: StaticErrorKind::UnknownType,
                    });
                    Ty::Unknown
                }
            }),
            TySpecKind::Seq(inner) => Ty::Seq(Box::new(self.ty_from_spec(inner))),
        }
    }

    fn no_field_on_ty<M: AstMetadata>(&mut self, field: &Ident<Substr, M>, ty: Ty) -> Ty {
        self.errors.push(StaticError {
            span: self.span(field.span),
            kind: StaticErrorKind::NoFieldOnTy {
                field: field.name.to_string(),
                ty,
            },
        });
        Ty::Unknown
    }

    fn assert_eq_ty(&mut self, span: cfgrammar::Span, found: &Ty, expected: &Ty) {
        if *found != *expected && !(*found == Ty::Any || *expected == Ty::Any) {
            self.errors.push(StaticError {
                span: self.span(span),
                kind: StaticErrorKind::IncorrectTy {
                    found: found.clone(),
                    expected: expected.clone(),
                },
            });
        }
    }

    fn assert_ty_is_cell(&mut self, span: cfgrammar::Span, ty: &Ty) {
        if !matches!(ty, Ty::Cell(_) | Ty::Any) {
            self.errors.push(StaticError {
                span: self.span(span),
                kind: StaticErrorKind::IncorrectTyCategory {
                    found: ty.clone(),
                    expected: "Cell".into(),
                },
            });
        }
    }

    fn assert_ty_is_enum(&mut self, span: cfgrammar::Span, ty: &Ty) {
        if !matches!(ty, Ty::Enum(_) | Ty::Any) {
            self.errors.push(StaticError {
                span: self.span(span),
                kind: StaticErrorKind::IncorrectTyCategory {
                    found: ty.clone(),
                    expected: "Enum".into(),
                },
            });
        }
    }

    fn assert_eq_arity(&mut self, span: cfgrammar::Span, found: usize, expected: usize) {
        if found != expected {
            self.errors.push(StaticError {
                span: self.span(span),
                kind: StaticErrorKind::CallIncorrectPositionalArity { expected, found },
            });
        }
    }

    fn typecheck_kwargs(
        &mut self,
        kwargs: &[KwArgValue<Substr, VarIdTyMetadata>],
        kwarg_defs: IndexMap<&str, Ty>,
    ) {
        let mut defined = IndexSet::new();
        for kwarg in kwargs {
            let mut cont = false;
            if !kwarg_defs.contains_key(&kwarg.name.name.as_str()) {
                self.errors.push(StaticError {
                    span: self.span(kwarg.name.span),
                    kind: StaticErrorKind::InvalidKwArg,
                });
                cont = true;
            }
            if defined.contains(&&kwarg.name.name) {
                self.errors.push(StaticError {
                    span: self.span(kwarg.name.span),
                    kind: StaticErrorKind::DuplicateKwArg,
                });
                cont = true;
            }
            defined.insert(&kwarg.name.name);
            if !cont {
                self.assert_eq_ty(
                    kwarg.value.span(),
                    &kwarg.value.ty(),
                    kwarg_defs.get(&kwarg.name.name.as_str()).unwrap(),
                );
            }
        }
    }

    fn typecheck_posargs(
        &mut self,
        call_span: cfgrammar::Span,
        args: &[Expr<Substr, VarIdTyMetadata>],
        arg_defs: &[Ty],
    ) {
        self.assert_eq_arity(call_span, args.len(), arg_defs.len());
        for (found, expected) in args.iter().zip(arg_defs) {
            self.assert_eq_ty(found.span(), &found.ty(), expected);
        }
    }

    fn typecheck_args(
        &mut self,
        call_span: cfgrammar::Span,
        args: &crate::ast::Args<Substr, VarIdTyMetadata>,
        arg_defs: &[Ty],
        kwarg_defs: IndexMap<&str, Ty>,
    ) {
        self.typecheck_posargs(call_span, &args.posargs, arg_defs);
        self.typecheck_kwargs(&args.kwargs, kwarg_defs);
    }

    fn typecheck_call(
        &mut self,
        lookup: Option<(VarId, Ty)>,
        call_span: cfgrammar::Span,
        args: &crate::ast::Args<Substr, VarIdTyMetadata>,
    ) -> (Option<VarId>, Ty) {
        if let Some((varid, ty)) = lookup {
            match ty {
                Ty::Fn(ty) => {
                    self.typecheck_args(call_span, args, &ty.args, IndexMap::new());
                    (Some(varid), ty.ret.clone())
                }
                Ty::CellFn(ty) => {
                    self.typecheck_args(call_span, args, &ty.args, IndexMap::new());
                    (
                        Some(varid),
                        Ty::Cell(Box::new(CellTy {
                            data: ty.data.clone(),
                        })),
                    )
                }
                ty => {
                    self.errors.push(StaticError {
                        span: self.span(call_span),
                        kind: StaticErrorKind::CannotCall(ty),
                    });
                    (None, Ty::Unknown)
                }
            }
        } else {
            self.errors.push(StaticError {
                span: self.span(call_span),
                kind: StaticErrorKind::UndeclaredVar,
            });
            (None, Ty::Unknown)
        }
    }
}

impl<S> Expr<S, VarIdTyMetadata> {
    fn ty(&self) -> Ty {
        match self {
            Expr::If(if_expr) => if_expr.metadata.clone(),
            Expr::Match(match_expr) => match_expr.metadata.clone(),
            Expr::Comparison(comparison_expr) => comparison_expr.metadata.clone(),
            Expr::BinOp(bin_op_expr) => bin_op_expr.metadata.clone(),
            Expr::Call(call_expr) => call_expr.metadata.1.clone(),
            Expr::Emit(emit_expr) => emit_expr.metadata.clone(),
            Expr::IdentPath(path) => path.metadata.1.clone(),
            Expr::FieldAccess(field_access_expr) => field_access_expr.metadata.clone(),
            Expr::Nil(_) => Ty::Nil,
            Expr::SeqNil(_) => Ty::SeqNil,
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
    type InputS = Substr;
    type OutputS = Substr;

    fn dispatch_ident(
        &mut self,
        _input: &Ident<Substr, Self::InputMetadata>,
    ) -> <Self::OutputMetadata as AstMetadata>::Ident {
    }

    fn dispatch_ident_path(
        &mut self,
        input: &IdentPath<Self::InputS, Self::InputMetadata>,
    ) -> <Self::OutputMetadata as AstMetadata>::IdentPath {
        // Currently, ident path exprs are either single variables or enum values.
        // Parser grammar ensures paths cannot be empty.
        assert!(!input.path.is_empty());
        if input.path.len() == 1 {
            if let Some((varid, ty)) = self.lookup(&input.path[0].name) {
                (Some(varid), ty)
            } else {
                self.errors.push(StaticError {
                    span: self.span(input.span),
                    kind: StaticErrorKind::UndeclaredVar,
                });
                (None, Ty::Unknown)
            }
        } else {
            // look up enum
            let path = match input.path[0].name.as_str() {
                "std" => {
                    vec!["std".to_string()]
                }
                "crate" => input
                    .path
                    .iter()
                    .skip(1)
                    .dropping_back(2)
                    .map(|ident| ident.name.to_string())
                    .collect_vec(),
                _ => self
                    .current_path
                    .iter()
                    .cloned()
                    .chain(
                        input
                            .path
                            .iter()
                            .dropping_back(2)
                            .map(|ident| ident.name.to_string()),
                    )
                    .collect_vec(),
            };
            let enum_ = &input.path[input.path.len() - 2];
            let lookup = if path.is_empty() {
                self.lookup(&enum_.name)
            } else {
                self.mod_bindings
                    .get(&path)
                    .as_ref()
                    .and_then(|mod_binding| {
                        mod_binding.var_bindings.get(enum_.name.as_str()).cloned()
                    })
            };
            if let Some((_, ty)) = lookup {
                if let Ty::Enum(ref e) = ty {
                    let variant = &input.path.last().unwrap().name;
                    if !e.variants.contains(variant.as_str()) {
                        self.errors.push(StaticError {
                            span: self.span(enum_.span),
                            kind: StaticErrorKind::InvalidVariant(variant.to_string()),
                        });
                    }
                    (None, ty)
                } else {
                    self.errors.push(StaticError {
                        span: self.span(enum_.span),
                        kind: StaticErrorKind::NotAnEnum,
                    });
                    (None, Ty::Unknown)
                }
            } else {
                self.errors.push(StaticError {
                    span: self.span(enum_.span),
                    kind: StaticErrorKind::NotAnEnum,
                });
                (None, Ty::Unknown)
            }
        }
    }

    fn dispatch_enum_decl(
        &mut self,
        _input: &crate::ast::EnumDecl<Substr, Self::InputMetadata>,
        _name: &Ident<Substr, Self::OutputMetadata>,
        _variants: &[Ident<Substr, Self::OutputMetadata>],
    ) -> <Self::OutputMetadata as AstMetadata>::EnumDecl {
    }

    fn dispatch_cell_decl(
        &mut self,
        _input: &CellDecl<Substr, Self::InputMetadata>,
        name: &Ident<Substr, Self::OutputMetadata>,
        _args: &[ArgDecl<Substr, Self::OutputMetadata>],
        _scope: &Scope<Substr, Self::OutputMetadata>,
    ) -> <Self::OutputMetadata as AstMetadata>::CellDecl {
        // TODO: Argument checks
        (self.ast.path.clone(), self.lookup(&name.name).unwrap().0)
    }

    fn dispatch_fn_decl(
        &mut self,
        _input: &FnDecl<Substr, Self::InputMetadata>,
        name: &Ident<Substr, Self::OutputMetadata>,
        _args: &[ArgDecl<Substr, Self::OutputMetadata>],
        _return_ty: &Option<TySpec<Substr, Self::OutputMetadata>>,
        _scope: &Scope<Substr, Self::OutputMetadata>,
    ) -> <Self::OutputMetadata as AstMetadata>::FnDecl {
        (self.ast.path.clone(), self.lookup(&name.name).unwrap().0)
    }

    fn transform_fn_decl(
        &mut self,
        input: &FnDecl<Substr, Self::InputMetadata>,
    ) -> FnDecl<Substr, Self::OutputMetadata> {
        let name = self.transform_ident(&input.name);
        let return_ty = input
            .return_ty
            .as_ref()
            .map(|spec| self.transform_ty_spec(spec));
        // TODO: this code is mostly duplicated from `transform_scope`.
        self.enter_scope(&input.scope);
        let scope_annotation = input
            .scope
            .scope_annotation
            .as_ref()
            .map(|ident| self.transform_ident(ident));
        let args: Vec<_> = input
            .args
            .iter()
            .map(|arg| self.transform_arg_decl(arg))
            .collect();
        let stmts = input
            .scope
            .stmts
            .iter()
            .map(|stmt| self.transform_statement(stmt))
            .collect_vec();
        let tail = input
            .scope
            .tail
            .as_ref()
            .map(|stmt| self.transform_expr(stmt));
        let metadata = self.dispatch_scope(&input.scope, &stmts, &tail);
        let scope = Scope {
            scope_annotation,
            span: input.span,
            stmts,
            tail,
            metadata,
        };
        self.exit_scope(&input.scope, &scope);
        let metadata = self.dispatch_fn_decl(input, &name, &args, &return_ty, &scope);
        FnDecl {
            name,
            args,
            return_ty,
            scope,
            span: input.span,
            metadata,
        }
    }

    fn transform_cell_decl(
        &mut self,
        input: &CellDecl<Substr, Self::InputMetadata>,
    ) -> CellDecl<Substr, Self::OutputMetadata> {
        if BUILTINS.contains(&input.name.name.as_str()) {
            self.errors.push(StaticError {
                span: self.span(input.name.span),
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
                span: self.span(tail.span()),
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
                        if ["x", "y"].contains(&lt.name.name.as_str()) {
                            self.errors.push(StaticError {
                                span: self.span(lt.name.span),
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
        self.alloc(&input.name.name, ty);
        let name = self.transform_ident(&input.name);
        let metadata = self.dispatch_cell_decl(input, &name, &args, &scope);
        CellDecl {
            name,
            scope,
            args,
            span: input.span,
            metadata,
        }
    }

    fn transform_call_expr(
        &mut self,
        input: &CallExpr<Self::InputS, Self::InputMetadata>,
    ) -> CallExpr<Self::OutputS, Self::OutputMetadata> {
        let scope_annotation = input
            .scope_annotation
            .as_ref()
            .map(|anno| self.transform_ident(anno));
        let func = IdentPath {
            path: input
                .func
                .path
                .iter()
                .map(|ident| self.transform_ident(ident))
                .collect(),
            metadata: (None, Ty::Unknown),
            span: input.func.span,
        };
        let args = self.transform_args(&input.args);
        let metadata = self.dispatch_call_expr(input, &func, &args);
        CallExpr {
            scope_annotation,
            func,
            args,
            span: input.span,
            metadata,
        }
    }

    fn dispatch_constant_decl(
        &mut self,
        _input: &ConstantDecl<Substr, Self::InputMetadata>,
        _name: &Ident<Substr, Self::OutputMetadata>,
        _ty: &Ident<Substr, Self::OutputMetadata>,
        _value: &Expr<Substr, Self::OutputMetadata>,
    ) -> <Self::OutputMetadata as AstMetadata>::ConstantDecl {
    }

    fn dispatch_if_expr(
        &mut self,
        input: &IfExpr<Substr, Self::InputMetadata>,
        cond: &Expr<Substr, Self::OutputMetadata>,
        then: &Scope<Substr, Self::OutputMetadata>,
        else_: &Scope<Substr, Self::OutputMetadata>,
    ) -> <Self::OutputMetadata as AstMetadata>::IfExpr {
        if let Some(scope_annotation) = &input.scope_annotation {
            let span = self.span(scope_annotation.span);
            let bindings = self.bindings.last_mut().unwrap();
            if bindings.scope_bindings.contains(&scope_annotation.name) {
                self.errors.push(StaticError {
                    span,
                    kind: StaticErrorKind::DuplicateNameDeclaration,
                });
            }
            bindings
                .scope_bindings
                .insert(scope_annotation.name.clone());
        }
        let cond_ty = cond.ty();
        let then_ty = then.metadata.clone();
        let else_ty = else_.metadata.clone();
        if cond_ty != Ty::Bool {
            self.errors.push(StaticError {
                span: self.span(cond.span()),
                kind: StaticErrorKind::IfCondNotBool,
            });
        }
        let lub_ty = then_ty.lub(&else_ty);
        if let Some(lub_ty) = lub_ty {
            lub_ty
        } else {
            self.errors.push(StaticError {
                span: self.span(input.span),
                kind: StaticErrorKind::BranchesDifferentTypes,
            });
            then_ty
        }
    }

    fn dispatch_match_expr(
        &mut self,
        input: &crate::ast::MatchExpr<Self::InputS, Self::InputMetadata>,
        scrutinee: &Expr<Self::OutputS, Self::OutputMetadata>,
        arms: &[crate::ast::MatchArm<Self::OutputS, Self::OutputMetadata>],
    ) -> <Self::OutputMetadata as AstMetadata>::MatchExpr {
        let scrutinee_ty = scrutinee.ty();
        self.assert_ty_is_enum(scrutinee.span(), &scrutinee_ty);
        let mut lub_ty: Option<Ty> = None;

        if let Ty::Enum(ref e) = scrutinee_ty {
            let mut covered = IndexSet::new();
            let mut remaining = e.variants.clone();
            for arm in arms.iter() {
                let arm_ty = &arm.pattern.metadata.1;
                self.assert_eq_ty(arm.pattern.span, arm_ty, &scrutinee_ty);

                let variant = arm.pattern.path.last().unwrap().name.clone();
                remaining.swap_remove(variant.as_str());
                if !covered.insert(variant) {
                    self.errors.push(StaticError {
                        span: self.span(arm.pattern.span),
                        kind: StaticErrorKind::DuplicateMatchArm,
                    });
                }

                if let Some(ref inner) = lub_ty {
                    if let Some(lub) = inner.lub(&arm.expr.ty()) {
                        lub_ty = Some(lub);
                    } else {
                        self.errors.push(StaticError {
                            span: self.span(arm.expr.span()),
                            kind: StaticErrorKind::BranchesDifferentTypes,
                        });
                    }
                } else {
                    lub_ty = Some(arm.expr.ty());
                }
            }

            if !remaining.is_empty() {
                self.errors.push(StaticError {
                    span: self.span(input.span),
                    kind: StaticErrorKind::MatchArmsNotComprehensive,
                });
            }
        }

        lub_ty.unwrap_or_default()
    }

    fn dispatch_bin_op_expr(
        &mut self,
        input: &BinOpExpr<Substr, Self::InputMetadata>,
        left: &Expr<Substr, Self::OutputMetadata>,
        right: &Expr<Substr, Self::OutputMetadata>,
    ) -> <Self::OutputMetadata as AstMetadata>::BinOpExpr {
        let left_ty = left.ty();
        let right_ty = right.ty();
        if left_ty != right_ty {
            self.errors.push(StaticError {
                span: self.span(input.span),
                kind: StaticErrorKind::BinOpMismatchedTypes,
            });
        }
        if ![Ty::Float, Ty::Int].contains(&left_ty) {
            self.errors.push(StaticError {
                span: self.span(left.span()),
                kind: StaticErrorKind::BinOpInvalidType,
            });
        }
        if ![Ty::Float, Ty::Int].contains(&right_ty) {
            self.errors.push(StaticError {
                span: self.span(right.span()),
                kind: StaticErrorKind::BinOpInvalidType,
            });
        }
        left_ty
    }

    fn dispatch_unary_op_expr(
        &mut self,
        input: &crate::ast::UnaryOpExpr<Substr, Self::InputMetadata>,
        operand: &Expr<Substr, Self::OutputMetadata>,
    ) -> <Self::OutputMetadata as AstMetadata>::UnaryOpExpr {
        match input.op {
            UnaryOp::Not => {
                self.errors.push(StaticError {
                    span: self.span(input.span),
                    kind: StaticErrorKind::Unimplemented,
                });
                Ty::Bool
            }
            UnaryOp::Neg => {
                let operand_ty = operand.ty();
                if ![Ty::Float, Ty::Int].contains(&operand_ty) {
                    self.errors.push(StaticError {
                        span: self.span(operand.span()),
                        kind: StaticErrorKind::UnaryOpInvalidType,
                    });
                }
                operand_ty
            }
        }
    }

    fn dispatch_comparison_expr(
        &mut self,
        input: &ComparisonExpr<Substr, Self::InputMetadata>,
        left: &Expr<Substr, Self::OutputMetadata>,
        right: &Expr<Substr, Self::OutputMetadata>,
    ) -> <Self::OutputMetadata as AstMetadata>::ComparisonExpr {
        let left_ty = left.ty();
        let right_ty = right.ty();
        let lub_ty = left_ty.lub(&right_ty);
        if let Some(lub_ty) = lub_ty {
            if left_ty == Ty::Float
                && (input.op == ComparisonOp::Eq || input.op == ComparisonOp::Ne)
            {
                self.errors.push(StaticError {
                    span: self.span(input.span),
                    kind: StaticErrorKind::FloatEquality,
                });
            }
            if matches!(left_ty, Ty::Enum(_))
                && (input.op != ComparisonOp::Eq && input.op != ComparisonOp::Ne)
            {
                self.errors.push(StaticError {
                    span: self.span(input.span),
                    kind: StaticErrorKind::EnumsNotOrd,
                });
            }
            if matches!(left_ty, Ty::Nil)
                && matches!(right_ty, Ty::Nil)
                && (input.op != ComparisonOp::Eq && input.op != ComparisonOp::Ne)
            {
                self.errors.push(StaticError {
                    span: self.span(input.span),
                    kind: StaticErrorKind::NilNotOrd,
                });
            }
            if matches!(left_ty, Ty::SeqNil)
                && matches!(right_ty, Ty::SeqNil)
                && (input.op != ComparisonOp::Eq && input.op != ComparisonOp::Ne)
            {
                self.errors.push(StaticError {
                    span: self.span(input.span),
                    kind: StaticErrorKind::SeqNilNotOrd,
                });
            }
            if matches!(lub_ty, Ty::Seq(_))
                && (input.op != ComparisonOp::Eq && input.op != ComparisonOp::Ne)
                && !(left_ty == Ty::SeqNil || right_ty == Ty::SeqNil)
            {
                self.errors.push(StaticError {
                    span: self.span(input.span),
                    kind: StaticErrorKind::SeqMustCompareEqSeqNil,
                });
            }
            if !matches!(
                left_ty,
                Ty::Float | Ty::Int | Ty::Enum(_) | Ty::Seq(_) | Ty::Nil | Ty::SeqNil
            ) {
                self.errors.push(StaticError {
                    span: self.span(input.span),
                    kind: StaticErrorKind::ComparisonInvalidType,
                });
            }
        } else {
            self.errors.push(StaticError {
                span: self.span(input.span),
                kind: StaticErrorKind::BinOpMismatchedTypes,
            });
        }
        Ty::Bool
    }

    fn dispatch_field_access_expr(
        &mut self,
        _input: &crate::ast::FieldAccessExpr<Substr, Self::InputMetadata>,
        base: &Expr<Substr, Self::OutputMetadata>,
        field: &Ident<Substr, Self::OutputMetadata>,
    ) -> <Self::OutputMetadata as AstMetadata>::FieldAccessExpr {
        let base_ty = base.ty();
        match base_ty {
            Ty::Rect => match field.name.as_str() {
                "x0" | "x1" | "y0" | "y1" | "w" | "h" => Ty::Float,
                "layer" => Ty::String,
                _ => self.no_field_on_ty(field, Ty::Rect),
            },
            Ty::Inst(ref c) => match field.name.as_str() {
                "x" | "y" => Ty::Float,
                name => c
                    .data
                    .get(name)
                    .cloned()
                    .unwrap_or_else(|| self.no_field_on_ty(field, base_ty.clone())),
            },
            // Propagate any and unknown types without throwing an error.
            Ty::Any => Ty::Any,
            Ty::Unknown => Ty::Unknown,
            _ => self.no_field_on_ty(field, base_ty.clone()),
        }
    }

    fn dispatch_call_expr(
        &mut self,
        input: &crate::ast::CallExpr<Substr, Self::InputMetadata>,
        func: &IdentPath<Substr, Self::OutputMetadata>,
        args: &crate::ast::Args<Substr, Self::OutputMetadata>,
    ) -> <Self::OutputMetadata as AstMetadata>::CallExpr {
        if let Some(scope_annotation) = &input.scope_annotation {
            let span = self.span(scope_annotation.span);
            let bindings = self.bindings.last_mut().unwrap();
            if bindings.scope_bindings.contains(&scope_annotation.name) {
                self.errors.push(StaticError {
                    span,
                    kind: StaticErrorKind::DuplicateNameDeclaration,
                });
            }
            bindings
                .scope_bindings
                .insert(scope_annotation.name.clone());
        }
        if func.path.len() == 1 {
            match func.path[0].name.as_str() {
                name @ "crect" | name @ "rect" => {
                    let kwarg_defs = if name == "crect" {
                        self.typecheck_posargs(input.span, &args.posargs, &[]);
                        IndexMap::from_iter([
                            ("x0", Ty::Float),
                            ("x1", Ty::Float),
                            ("y0", Ty::Float),
                            ("y1", Ty::Float),
                            ("x0i", Ty::Float),
                            ("x1i", Ty::Float),
                            ("y0i", Ty::Float),
                            ("y1i", Ty::Float),
                            ("w", Ty::Float),
                            ("h", Ty::Float),
                            ("layer", Ty::String),
                        ])
                    } else {
                        self.typecheck_posargs(input.span, &args.posargs, &[Ty::String]);
                        IndexMap::from_iter([
                            ("x0", Ty::Float),
                            ("x1", Ty::Float),
                            ("y0", Ty::Float),
                            ("y1", Ty::Float),
                            ("x0i", Ty::Float),
                            ("x1i", Ty::Float),
                            ("y0i", Ty::Float),
                            ("y1i", Ty::Float),
                            ("w", Ty::Float),
                            ("h", Ty::Float),
                        ])
                    };
                    self.typecheck_kwargs(&args.kwargs, kwarg_defs);
                    (None, Ty::Rect)
                }
                "text" => {
                    // text, layer, x, y
                    self.typecheck_posargs(
                        input.span,
                        &args.posargs,
                        &[Ty::String, Ty::String, Ty::Float, Ty::Float],
                    );
                    self.typecheck_kwargs(&args.kwargs, IndexMap::default());
                    (None, Ty::Nil)
                }
                "cons" => {
                    self.assert_eq_arity(input.span, args.posargs.len(), 2);
                    if args.posargs.len() == 2 {
                        let seqty = Ty::Seq(Box::new(args.posargs[0].ty()));
                        let tailty = args.posargs[1].ty();
                        if !(tailty == Ty::SeqNil || tailty == seqty) {
                            self.errors.push(StaticError {
                                span: self.span(args.posargs[1].span()),
                                kind: StaticErrorKind::IncorrectTy {
                                    found: tailty,
                                    expected: seqty.clone(),
                                },
                            });
                        }
                        (None, seqty)
                    } else {
                        (None, Ty::SeqNil)
                    }
                }
                "head" => {
                    self.assert_eq_arity(input.span, args.posargs.len(), 1);
                    if args.posargs.len() == 1 {
                        let argty = args.posargs[0].ty();
                        if let Ty::Seq(i) = argty {
                            (None, *i)
                        } else {
                            self.errors.push(StaticError {
                                span: self.span(input.span),
                                kind: StaticErrorKind::IncorrectTyCategory {
                                    found: argty,
                                    expected: "Seq".to_string(),
                                },
                            });
                            (None, Ty::Nil)
                        }
                    } else {
                        (None, Ty::Nil)
                    }
                }
                "tail" => {
                    self.assert_eq_arity(input.span, args.posargs.len(), 1);
                    if args.posargs.len() == 1 {
                        let argty = args.posargs[0].ty();
                        if let Ty::Seq(_) = argty {
                            (None, argty)
                        } else {
                            self.errors.push(StaticError {
                                span: self.span(input.span),
                                kind: StaticErrorKind::IncorrectTyCategory {
                                    found: argty,
                                    expected: "Seq".to_string(),
                                },
                            });
                            (None, Ty::Nil)
                        }
                    } else {
                        (None, Ty::Nil)
                    }
                }
                "bbox" => {
                    self.assert_eq_arity(input.span, args.posargs.len(), 1);
                    let argty = args.posargs[0].ty();
                    if !matches!(argty, Ty::Cell(_) | Ty::Inst(_)) {
                        self.errors.push(StaticError {
                            span: self.span(input.span),
                            kind: StaticErrorKind::IncorrectTyCategory {
                                found: argty,
                                expected: "Cell/Inst".to_string(),
                            },
                        });
                    }
                    (None, Ty::Rect)
                }
                "float" => {
                    self.typecheck_args(input.span, args, &[], IndexMap::new());
                    (None, Ty::Float)
                }
                "eq" => {
                    self.typecheck_args(input.span, args, &[Ty::Float, Ty::Float], IndexMap::new());
                    (None, Ty::Nil)
                }
                "dimension" => {
                    self.typecheck_args(
                        input.span,
                        args,
                        &[
                            Ty::Float,
                            Ty::Float,
                            Ty::Float,
                            Ty::Float,
                            Ty::Float,
                            Ty::Float,
                            Ty::Bool,
                        ],
                        IndexMap::new(),
                    );
                    (None, Ty::Nil)
                }
                "inst" => {
                    self.assert_eq_arity(input.span, args.posargs.len(), 1);
                    self.typecheck_kwargs(
                        &args.kwargs,
                        IndexMap::from_iter([
                            ("reflect", Ty::Bool),
                            ("angle", Ty::Int),
                            ("x", Ty::Float),
                            ("y", Ty::Float),
                            ("xi", Ty::Float),
                            ("yi", Ty::Float),
                            ("construction", Ty::Bool),
                        ]),
                    );
                    if let Some(ty) = args.posargs.first() {
                        self.assert_ty_is_cell(ty.span(), &ty.ty());
                        if let Ty::Cell(c) = ty.ty() {
                            (None, Ty::Inst(c.clone()))
                        } else {
                            (None, Ty::Unknown)
                        }
                    } else {
                        (None, Ty::Unknown)
                    }
                }
                name => self.typecheck_call(self.lookup(name), input.span, args),
            }
        } else {
            let path = match func.path[0].name.as_str() {
                "std" => {
                    vec!["std".to_string()]
                }
                "crate" => func
                    .path
                    .iter()
                    .skip(1)
                    .dropping_back(1)
                    .map(|ident| ident.name.to_string())
                    .collect_vec(),
                _ => self
                    .current_path
                    .iter()
                    .cloned()
                    .chain(
                        func.path
                            .iter()
                            .dropping_back(1)
                            .map(|ident| ident.name.to_string()),
                    )
                    .collect_vec(),
            };
            let name = &func.path.last().unwrap().name;
            let lookup = self
                .mod_bindings
                .get(&path)
                .as_ref()
                .and_then(|mod_binding| mod_binding.var_bindings.get(name).cloned());
            self.typecheck_call(lookup, input.span, args)
        }
    }

    fn dispatch_emit_expr(
        &mut self,
        _input: &crate::ast::EmitExpr<Substr, Self::InputMetadata>,
        value: &Expr<Substr, Self::OutputMetadata>,
    ) -> <Self::OutputMetadata as AstMetadata>::EmitExpr {
        value.ty()
    }

    fn dispatch_args(
        &mut self,
        _input: &crate::ast::Args<Substr, Self::InputMetadata>,
        _posargs: &[Expr<Substr, Self::OutputMetadata>],
        _kwargs: &[crate::ast::KwArgValue<Substr, Self::OutputMetadata>],
    ) -> <Self::OutputMetadata as AstMetadata>::Args {
    }

    fn dispatch_cast(
        &mut self,
        input: &crate::ast::CastExpr<Substr, Self::InputMetadata>,
        value: &Expr<Substr, Self::OutputMetadata>,
        ty: &TySpec<Substr, Self::OutputMetadata>,
    ) -> <Self::OutputMetadata as AstMetadata>::CastExpr {
        let ty = self.ty_from_spec(ty);
        match (value.ty(), &ty) {
            (Ty::Int, Ty::Float)
            | (Ty::Int, Ty::Int)
            | (Ty::Float, Ty::Int)
            | (Ty::Float, Ty::Float) => (),
            (_, Ty::Unknown) => (),
            _ => {
                self.errors.push(StaticError {
                    span: self.span(input.span),
                    kind: StaticErrorKind::InvalidCast,
                });
            }
        };
        ty
    }

    fn dispatch_kw_arg_value(
        &mut self,
        _input: &crate::ast::KwArgValue<Substr, Self::InputMetadata>,
        _name: &Ident<Substr, Self::OutputMetadata>,
        value: &Expr<Substr, Self::OutputMetadata>,
    ) -> <Self::OutputMetadata as AstMetadata>::KwArgValue {
        value.ty()
    }

    fn dispatch_arg_decl(
        &mut self,
        input: &ArgDecl<Substr, Self::InputMetadata>,
        _name: &Ident<Substr, Self::OutputMetadata>,
        _ty: &TySpec<Substr, Self::OutputMetadata>,
    ) -> <Self::OutputMetadata as AstMetadata>::ArgDecl {
        let ty = self.ty_from_spec(&input.ty);
        (self.alloc(&input.name.name, ty.clone()), ty)
    }

    fn dispatch_scope(
        &mut self,
        _input: &Scope<Substr, Self::InputMetadata>,
        _stmts: &[Statement<Substr, Self::OutputMetadata>],
        tail: &Option<Expr<Substr, Self::OutputMetadata>>,
    ) -> <Self::OutputMetadata as AstMetadata>::Scope {
        tail.as_ref().map(|tail| tail.ty()).unwrap_or(Ty::Nil)
    }

    fn enter_scope(&mut self, input: &crate::ast::Scope<Substr, Self::InputMetadata>) {
        if let Some(scope_annotation) = &input.scope_annotation {
            let span = self.span(scope_annotation.span);
            let bindings = self.bindings.last_mut().unwrap();
            if bindings.scope_bindings.contains(&scope_annotation.name) {
                self.errors.push(StaticError {
                    span,
                    kind: StaticErrorKind::DuplicateNameDeclaration,
                });
            }
            bindings
                .scope_bindings
                .insert(scope_annotation.name.clone());
        }
        self.bindings.push(Default::default());
    }

    fn exit_scope(
        &mut self,
        _input: &crate::ast::Scope<Substr, Self::InputMetadata>,
        _output: &crate::ast::Scope<Substr, Self::OutputMetadata>,
    ) {
        self.bindings.pop();
    }

    fn dispatch_let_binding(
        &mut self,
        _input: &LetBinding<Substr, Self::InputMetadata>,
        name: &Ident<Substr, Self::OutputMetadata>,
        value: &Expr<Substr, Self::OutputMetadata>,
    ) -> <Self::OutputMetadata as AstMetadata>::LetBinding {
        self.alloc(&name.name, value.ty())
    }

    fn transform_s(&mut self, s: &Self::InputS) -> Self::OutputS {
        s.clone()
    }
}

#[derive(Debug, Clone)]
pub enum CellArg {
    Float(f64),
    Int(i64),
    Seq(Vec<CellArg>),
}

impl CellArg {
    pub fn from_value(val: &Value, solver: &Solver) -> Option<Self> {
        match val {
            Value::Linear(v) => solver.eval_expr(v).map(CellArg::Float),
            Value::Int(i) => Some(CellArg::Int(*i)),
            Value::Seq(s) => s
                .iter()
                .map(|v| Self::from_value(v, solver))
                .collect::<Option<Vec<_>>>()
                .map(CellArg::Seq),
            _ => unreachable!(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct CompileInput<'a> {
    /// Full path to cell.
    pub cell: &'a [&'a str],
    pub args: Vec<CellArg>,
    pub lyp_file: &'a Path,
}

pub type VarId = u64;
pub type ConstraintVarId = u64;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompiledEmit {
    pub span: Span,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BasicRect<T> {
    pub layer: Option<String>,
    pub x0: T,
    pub y0: T,
    pub x1: T,
    pub y1: T,
    pub construction: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Rect<T> {
    pub layer: Option<String>,
    pub id: ObjectId,
    pub x0: T,
    pub y0: T,
    pub x1: T,
    pub y1: T,
    pub construction: bool,
    pub span: Option<Span>,
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
    pub constraint: ConstraintId,
    pub span: Option<Span>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Text<T> {
    pub id: ObjectId,
    pub text: String,
    pub layer: String,
    pub x: T,
    pub y: T,
    pub span: Option<Span>,
}

type FrameId = u64;
type ValueId = u64;
pub type CellId = u64;
pub type EnumId = u64;

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
    span: Span,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct ObjectEmit {
    object: ObjectId,
    scope: ScopeId,
    span: Span,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct ExecScope {
    parent: Option<ScopeId>,
    static_parent: Option<(ScopeId, SeqNum)>,
    name: String,
    span: Span,
    bindings: IndexMap<SeqNum, (String, ValueId)>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct FallbackConstraint {
    priority: i32,
    constraint: LinearExpr,
    span: Span,
}

impl PartialEq for FallbackConstraint {
    fn eq(&self, other: &Self) -> bool {
        self.priority == other.priority
    }
}

impl Eq for FallbackConstraint {}

impl PartialOrd for FallbackConstraint {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for FallbackConstraint {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.priority.cmp(&other.priority)
    }
}

struct CellState {
    solve_iters: u64,
    solver: Solver,
    fields: IndexMap<String, ValueId>,
    emit: Vec<Emit>,
    object_emit: Vec<ObjectEmit>,
    objects: IndexMap<ObjectId, Object>,
    deferred: IndexSet<ValueId>,
    root_scope: ScopeId,
    scopes: IndexMap<ScopeId, ExecScope>,
    fallback_constraints: BinaryHeap<FallbackConstraint>,
    fallback_constraints_used: Vec<LinearExpr>,
    unsolved_vars: Option<IndexSet<Var>>,
    constraint_span_map: IndexMap<ConstraintId, Span>,
}

struct ExecPass<'a> {
    ast: &'a WorkspaceAst<VarIdTyMetadata>,
    cell_states: IndexMap<CellId, CellState>,
    values: IndexMap<ValueId, DeferValue<VarIdTyMetadata>>,
    frames: IndexMap<FrameId, Frame>,
    nil_value: ValueId,
    seq_nil_value: ValueId,
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
    errors: Vec<ExecError>,
}

enum ExecScopeName {
    // Exact name has been specified.
    Specified(String),
    // Exact name has not been specified, need to generate unique identifier based on prefix.
    Prefix(String),
}

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
            span: scope.span.clone(),
            emit: Vec::new(),
        },
    );
}

impl<'a> ExecPass<'a> {
    pub(crate) fn new(ast: &'a WorkspaceAst<VarIdTyMetadata>) -> Self {
        Self {
            ast,
            cell_states: IndexMap::new(),
            values: IndexMap::from_iter([
                (1, DeferValue::Ready(Value::Nil)),
                (2, DeferValue::Ready(Value::Bool(true))),
                (3, DeferValue::Ready(Value::Bool(false))),
                (4, DeferValue::Ready(Value::SeqNil)),
            ]),
            frames: IndexMap::from_iter([(
                5,
                Frame {
                    bindings: Default::default(),
                    parent: None,
                },
            )]),
            nil_value: 1,
            true_value: 2,
            false_value: 3,
            seq_nil_value: 4,
            global_frame: 5,
            next_id: 6,
            partial_cells: VecDeque::new(),
            compiled_cells: IndexMap::new(),
            errors: Vec::new(),
        }
    }

    fn span(&self, loc: &DynLoc, span: cfgrammar::Span) -> Span {
        Span {
            path: self.cell_state(loc.cell).scopes[&loc.scope]
                .span
                .path
                .clone(),
            span,
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
        let path = match input.cell[0] {
            "std" => {
                vec!["std".to_string()]
            }
            "crate" => input
                .cell
                .iter()
                .skip(1)
                .dropping_back(1)
                .map(|ident| ident.to_string())
                .collect_vec(),
            _ => input
                .cell
                .iter()
                .dropping_back(1)
                .map(|ident| ident.to_string())
                .collect_vec(),
        };
        if let Some((_, vid)) = self.ast[&path].ast.decls.iter().find_map(|d| match d {
            Decl::Cell(
                v @ CellDecl {
                    name: Ident { name, .. },
                    ..
                },
            ) if name == input.cell.last().unwrap() => Some(v.metadata.clone()),
            _ => None,
        }) {
            let cell_id = match self.execute_cell(vid, input.args, Some("TOP")) {
                Ok(cell_id) => cell_id,
                Err(()) => {
                    return CompileOutput::ExecErrors(ExecErrorCompileOutput {
                        errors: self.errors,
                        output: None,
                    });
                }
            };
            let layers = if let Ok(layers) = std::fs::File::open(input.lyp_file)
                .map_err(|_| ())
                .and_then(|f| klayout_lyp::from_reader(BufReader::new(f)).map_err(|_| ()))
            {
                layers.into()
            } else {
                return CompileOutput::StaticErrors(StaticErrorCompileOutput {
                    errors: vec![StaticError {
                        span: Span {
                            path: self.ast[&vec![]].path.clone(),
                            span: cfgrammar::Span::new(0, 0),
                        },
                        kind: StaticErrorKind::InvalidLyp,
                    }],
                });
            };
            if self.errors.is_empty() {
                CompileOutput::Valid(CompiledData {
                    cells: self.compiled_cells,
                    top: cell_id,
                    layers,
                })
            } else {
                CompileOutput::ExecErrors(ExecErrorCompileOutput {
                    errors: self.errors,
                    output: Some(CompiledData {
                        cells: self.compiled_cells,
                        top: cell_id,
                        layers,
                    }),
                })
            }
        } else {
            CompileOutput::ExecErrors(ExecErrorCompileOutput {
                errors: vec![ExecError {
                    span: None,
                    cell: 0, // TODO: don't use dummy cell ID
                    kind: ExecErrorKind::InvalidCell,
                }],
                output: None,
            })
        }
    }

    pub(crate) fn execute_cell(
        &mut self,
        cell: VarId,
        args: Vec<CellArg>,
        scope_annotation: Option<&str>,
    ) -> Result<CellId, ()> {
        let mut frame = Frame {
            bindings: Default::default(),
            parent: Some(self.global_frame),
        };
        let cell_decl = self.values[&self.lookup(self.global_frame, cell).unwrap()]
            .as_ref()
            .unwrap_ready()
            .as_ref()
            .unwrap_cell_fn()
            .clone();
        let root_scope_id = self.scope_id();
        let root_scope = ExecScope {
            parent: None,
            static_parent: None,
            span: Span {
                path: cell_decl.metadata.0.clone(),
                span: cell_decl.scope.span,
            },
            name: if let Some(anno) = scope_annotation {
                format!("{} cell {}", anno, &cell_decl.name.name)
            } else {
                format!("cell {} {}", &cell_decl.name.name, root_scope_id.0)
            },
            bindings: Default::default(),
        };

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
                        object_emit: Vec::new(),
                        deferred: Default::default(),
                        scopes: IndexMap::from_iter([(root_scope_id, root_scope)]),
                        fallback_constraints: Default::default(),
                        fallback_constraints_used: Vec::new(),
                        root_scope: root_scope_id,
                        unsolved_vars: Default::default(),
                        objects: Default::default(),
                        constraint_span_map: IndexMap::new(),
                    }
                )
                .is_none()
        );
        if args.len() != cell_decl.args.len() {
            self.errors.push(ExecError {
                span: None,
                cell: cell_id,
                kind: ExecErrorKind::InvalidCell,
            });
            return Ok(cell_id);
        }
        for (val, decl) in args.into_iter().zip(cell_decl.args.iter()) {
            let vid = self.value_id();
            let val = Value::from_arg(&val);
            self.values.insert(vid, DeferValue::Ready(val));
            frame.bindings.insert(decl.metadata.0, vid);
        }
        let fid = self.frame_id();
        self.frames.insert(fid, frame);

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
                progress = progress || self.eval_partial(vid)?;
            }

            if require_progress && !progress {
                let state = self.cell_state_mut(cell_id);
                if state.unsolved_vars.is_none() {
                    state.unsolved_vars = Some(state.solver.unsolved_vars());
                    self.errors.push(ExecError {
                        span: None,
                        cell: cell_id,
                        kind: ExecErrorKind::Underconstrained,
                    });
                }
                let mut constraint_added = false;
                let state = self.cell_state_mut(cell_id);
                while let Some(FallbackConstraint {
                    constraint, span, ..
                }) = state.fallback_constraints.pop()
                {
                    if constraint
                        .coeffs
                        .iter()
                        .any(|(c, v)| c.abs() > 1e-6 && !state.solver.is_solved(*v))
                    {
                        state.fallback_constraints_used.push(constraint.clone());
                        let constraint_id = state.solver.constrain_eq0(constraint);
                        state.constraint_span_map.insert(constraint_id, span);
                        constraint_added = true;
                        break;
                    }
                }
                if !constraint_added {
                    state.solver.force_solution();
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

        let state = self.cell_state_mut(cell_id);
        if progress {
            state.solve_iters += 1;
            state.solver.solve();
        }
        for constraint in state.solver.inconsistent_constraints().clone() {
            let span = self
                .cell_state(cell_id)
                .constraint_span_map
                .get(&constraint)
                .cloned();
            self.errors.push(ExecError {
                span,
                cell: cell_id,
                kind: ExecErrorKind::InconsistentConstraint(constraint),
            });
        }

        self.partial_cells
            .pop_back()
            .expect("failed to pop cell id");

        let cell = self.emit(cell_id);
        assert!(self.compiled_cells.insert(cell_id, cell).is_none());
        Ok(cell_id)
    }

    fn emit(&mut self, cell: CellId) -> CompiledCell {
        let state = self.cell_states.get(&cell).expect("cell not found");
        let mut emit_obj = |obj: &Object| -> SolvedValue {
            match obj {
                Object::Rect(rect) => {
                    let x0 = state.solver.eval_expr(&rect.x0).unwrap();
                    let y0 = state.solver.eval_expr(&rect.y0).unwrap();
                    let x1 = state.solver.eval_expr(&rect.x1).unwrap();
                    let y1 = state.solver.eval_expr(&rect.y1).unwrap();
                    if x0 > x1 {
                        self.errors.push(ExecError {
                            span: rect.span.clone(),
                            cell,
                            kind: ExecErrorKind::FlippedRect("x0 > x1".to_string()),
                        });
                    }
                    if y0 > y1 {
                        self.errors.push(ExecError {
                            span: rect.span.clone(),
                            cell,
                            kind: ExecErrorKind::FlippedRect("y0 > y1".to_string()),
                        });
                    }
                    SolvedValue::Rect(Rect {
                        id: rect.id,
                        layer: rect.layer.clone(),
                        x0: (x0, rect.x0.clone()),
                        y0: (y0, rect.y0.clone()),
                        x1: (x1, rect.x1.clone()),
                        y1: (y1, rect.y1.clone()),
                        construction: rect.construction,
                        span: rect.span.clone(),
                    })
                }
                Object::Text(text) => SolvedValue::Text(Text {
                    id: text.id,
                    text: text.text.clone(),
                    layer: text.layer.clone(),
                    x: state.solver.eval_expr(&text.x).unwrap(),
                    y: state.solver.eval_expr(&text.y).unwrap(),
                    span: text.span.clone(),
                }),
                Object::Dimension(dim) => SolvedValue::Dimension(Dimension {
                    id: dim.id,
                    p: state.solver.eval_expr(&dim.p).unwrap(),
                    n: state.solver.eval_expr(&dim.n).unwrap(),
                    value: state.solver.eval_expr(&dim.value).unwrap(),
                    coord: state.solver.eval_expr(&dim.coord).unwrap(),
                    pstop: state.solver.eval_expr(&dim.pstop).unwrap(),
                    nstop: state.solver.eval_expr(&dim.nstop).unwrap(),
                    horiz: dim.horiz,
                    constraint: dim.constraint,
                    span: dim.span.clone(),
                }),
                Object::Inst(inst) => SolvedValue::Instance(SolvedInstance {
                    id: inst.id,
                    x: state.solver.eval_expr(&inst.x).unwrap(),
                    y: state.solver.eval_expr(&inst.y).unwrap(),
                    angle: inst.angle,
                    reflect: inst.reflect,
                    construction: inst.construction,
                    cell: *self.values[&inst.cell]
                        .as_ref()
                        .unwrap_ready()
                        .as_ref()
                        .unwrap_cell(),
                    span: inst.span.clone(),
                    cell_vid: inst.cell,
                }),
            }
        };
        let emit_value = |vid: ValueId| -> Option<Arrayed<ObjectId>> {
            let value = &self.values[&vid];
            value.as_ref().unwrap_ready().obj_ids()
        };

        let mut ccell = CompiledCell {
            scopes: IndexMap::new(),
            root: state.root_scope,
            fallback_constraints_used: state.fallback_constraints_used.clone(),
            unsolved_vars: state.unsolved_vars.clone().unwrap_or_default(),
            inconsistent_constraints: state.solver.inconsistent_constraints().clone(),
            objects: IndexMap::new(),
        };
        for (id, scope) in state.scopes.iter() {
            add_scope(&mut ccell, state, *id, scope);
        }

        for (id, obj) in state.objects.iter() {
            ccell.objects.insert(*id, emit_obj(obj));
        }

        for emit in state.emit.iter() {
            let obj_id = emit_value(emit.value).unwrap().unwrap_elem();
            ccell.scopes.get_mut(&emit.scope).unwrap().emit.push((
                obj_id,
                CompiledEmit {
                    span: emit.span.clone(),
                },
            ));
        }

        for emit in state.object_emit.iter() {
            ccell.scopes.get_mut(&emit.scope).unwrap().emit.push((
                emit.object,
                CompiledEmit {
                    span: emit.span.clone(),
                },
            ));
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
        for ast in self.ast.values() {
            for decl in &ast.ast.decls {
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
                                .insert(f.metadata.1, vid)
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
                                .insert(c.metadata.1, vid)
                                .is_none()
                        );
                    }
                    _ => (),
                }
            }
        }
    }

    fn eval_stmt(&mut self, loc: DynLoc, stmt: &Statement<Substr, VarIdTyMetadata>) {
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
        span: Span,
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
        span: Span,
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
        s: &Scope<Substr, VarIdTyMetadata>,
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

    fn visit_expr(&mut self, loc: DynLoc, expr: &Expr<Substr, VarIdTyMetadata>) -> ValueId {
        let partial_eval_state = match expr {
            Expr::Nil(_) => {
                return self.nil_value;
            }
            Expr::SeqNil(_) => {
                return self.seq_nil_value;
            }
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
            Expr::IdentPath(path) => {
                if let Some(var_id) = path.metadata.0 {
                    return self.lookup(loc.frame, var_id).unwrap();
                } else {
                    // must be an enum value
                    assert!(path.path.len() >= 2);
                    let vid = self.value_id();
                    self.values.insert(
                        vid,
                        Defer::Ready(Value::EnumValue(path.path.last().unwrap().name.to_string())),
                    );
                    return vid;
                }
            }
            Expr::Emit(e) => {
                let value = self.visit_expr(loc, &e.value);
                let span = self.span(&loc, e.span);
                self.cell_state_mut(loc.cell).emit.push(Emit {
                    scope: loc.scope,
                    value,
                    span,
                });
                return value;
            }
            Expr::Call(c) => {
                if BUILTINS.contains(&c.func.path.last().unwrap().name.as_str()) {
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
                                if let Some(anno) = &c.scope_annotation {
                                    ExecScopeName::Specified(format!(
                                        "{} fn {}",
                                        anno.name, val.name.name
                                    ))
                                } else {
                                    ExecScopeName::Prefix(format!("fn {}", val.name.name))
                                },
                                Span {
                                    path: val.metadata.0.clone(),
                                    span: val.scope.span,
                                },
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
            Expr::Match(match_expr) => {
                let scrutinee = self.visit_expr(loc, &match_expr.scrutinee);
                PartialEvalState::Match(Box::new(PartialMatchExpr {
                    expr: (**match_expr).clone(),
                    state: MatchExprState::Scrutinee(scrutinee),
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
                    self.span(&loc, s.span),
                );
                return self.visit_scope_expr_inner(loc.cell, loc.frame, scope, s);
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
                PartialEvalState::UnaryOp(PartialUnaryOp {
                    operand,
                    op: u.op,
                    expr: u.clone(),
                })
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

    fn eval_partial(&mut self, vid: ValueId) -> Result<bool, ()> {
        let v = self.values.swap_remove(&vid);
        if v.is_none() {
            return Ok(false);
        }
        let mut v = v.unwrap();
        let vref = v.as_mut();
        if vref.is_ready() {
            self.values.insert(vid, v);
            return Ok(false);
        }
        let vref = vref.unwrap_deferred();
        let state = self.cell_states.get_mut(&vref.loc.cell).unwrap();
        let progress = match &mut vref.state {
            PartialEvalState::Call(c) => match c.expr.func.path.last().unwrap().name.as_str() {
                f @ "crect" | f @ "rect" => {
                    let layer = if f == "crect" {
                        c.expr
                            .args
                            .kwargs
                            .iter()
                            .zip(c.state.kwargs.iter())
                            .find(|(k, _)| k.name.name == "layer")
                            .map(|(_, vid)| {
                                self.values[vid]
                                    .as_ref()
                                    .get_ready()
                                    .map(|layer| layer.as_ref().unwrap_string().clone())
                            })
                    } else {
                        c.state.posargs.first().map(|vid| {
                            self.values[vid]
                                .as_ref()
                                .get_ready()
                                .map(|layer| layer.as_ref().unwrap_string().clone())
                        })
                    };
                    let layer = match layer {
                        None => Some(None),
                        Some(None) => None,
                        Some(Some(l)) => Some(Some(l)),
                    };
                    if let Some(layer) = layer {
                        let id = self.object_id();
                        let span = self.span(&vref.loc, c.expr.span);
                        let state = self.cell_state_mut(vref.loc.cell);
                        let rect = Rect {
                            id,
                            layer,
                            x0: state.solver.new_var().into(),
                            y0: state.solver.new_var().into(),
                            x1: state.solver.new_var().into(),
                            y1: state.solver.new_var().into(),
                            construction: f == "crect",
                            span: Some(span.clone()),
                        };
                        state.objects.insert(rect.id, rect.clone().into());
                        state.emit.push(Emit {
                            scope: vref.loc.scope,
                            value: vid,
                            span,
                        });
                        self.values
                            .insert(vid, Defer::Ready(Value::Rect(rect.clone())));
                        for (kwarg, rhs) in c.expr.args.kwargs.iter().zip(c.state.kwargs.iter()) {
                            let lhs = self.value_id();
                            let priority = match kwarg.name.name.as_str() {
                                "x0" | "x0i" => {
                                    self.values
                                        .insert(lhs, Defer::Ready(Value::Linear(rect.x0.clone())));
                                    6
                                }
                                "x1" | "x1i" => {
                                    self.values
                                        .insert(lhs, Defer::Ready(Value::Linear(rect.x1.clone())));
                                    5
                                }
                                "y0" | "y0i" => {
                                    self.values
                                        .insert(lhs, Defer::Ready(Value::Linear(rect.y0.clone())));
                                    4
                                }
                                "y1" | "y1i" => {
                                    self.values
                                        .insert(lhs, Defer::Ready(Value::Linear(rect.y1.clone())));
                                    3
                                }
                                "w" => {
                                    self.values.insert(
                                        lhs,
                                        Defer::Ready(Value::Linear(
                                            rect.x1.clone() - rect.x0.clone(),
                                        )),
                                    );
                                    2
                                }
                                "h" => {
                                    self.values.insert(
                                        lhs,
                                        Defer::Ready(Value::Linear(
                                            rect.y1.clone() - rect.y0.clone(),
                                        )),
                                    );
                                    1
                                }
                                "layer" => {
                                    continue;
                                }
                                x => unreachable!("unsupported kwarg `{x}`"),
                            };
                            let span = self.span(&vref.loc, kwarg.span);
                            let defer = self.value_id();
                            self.values.insert(
                                defer,
                                DeferValue::Deferred(PartialEval {
                                    state: PartialEvalState::Constraint(PartialConstraint {
                                        lhs,
                                        rhs: *rhs,
                                        fallback: kwarg.name.name.ends_with('i'),
                                        priority,
                                        span,
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
                "text" => {
                    let args = c
                        .state
                        .posargs
                        .iter()
                        .map(|vid| self.values[vid].get_ready())
                        .collect::<Option<Vec<_>>>();
                    if let Some(mut args) = args {
                        assert_eq!(args.len(), 4);
                        let id = object_id(&mut self.next_id);
                        let span = self.span(&vref.loc, c.expr.span);
                        let state = self.cell_states.get_mut(&vref.loc.cell).unwrap();
                        let y = args.pop().unwrap().as_ref().unwrap_linear().clone();
                        let x = args.pop().unwrap().as_ref().unwrap_linear().clone();
                        let layer = args.pop().unwrap().as_ref().unwrap_string().clone();
                        let text_val = args.pop().unwrap().as_ref().unwrap_string().clone();
                        let text = Text {
                            id,
                            text: text_val,
                            layer,
                            x,
                            y,
                            span: Some(span.clone()),
                        };
                        state.object_emit.push(ObjectEmit {
                            scope: vref.loc.scope,
                            object: text.id,
                            span,
                        });
                        state.objects.insert(text.id, text.clone().into());
                        self.values.insert(vid, Defer::Ready(Value::Nil));
                        true
                    } else {
                        false
                    }
                }
                "bbox" => {
                    let arg = &self.values[&c.state.posargs[0]];
                    if let Some(val) = arg.get_ready() {
                        let span = self.span(&vref.loc, c.expr.span);
                        let r = match val {
                            Value::Inst(i) => {
                                if let Defer::Ready(cell) = &self.values[&i.cell] {
                                    let cell_id = cell.as_ref().unwrap_cell();
                                    Some(
                                        self.bbox(*cell_id)
                                            .map(|r| r.transform(i.reflect, i.angle)),
                                    )
                                } else {
                                    None
                                }
                            }
                            Value::Cell(c) => Some(self.bbox(*c)),
                            _ => {
                                self.errors.push(ExecError {
                                    span: Some(span.clone()),
                                    cell: vref.loc.cell,
                                    kind: ExecErrorKind::InvalidType,
                                });
                                return Err(());
                            }
                        };
                        if let Some(r) = r {
                            if let Some(r) = r {
                                let id = object_id(&mut self.next_id);
                                let state = self.cell_states.get_mut(&vref.loc.cell).unwrap();
                                let orect = Rect {
                                    id,
                                    layer: None,
                                    x0: r.x0.into(),
                                    y0: r.y0.into(),
                                    x1: r.x1.into(),
                                    y1: r.y1.into(),
                                    construction: true,
                                    span: Some(span.clone()),
                                };
                                state.objects.insert(orect.id, orect.clone().into());
                                state.emit.push(Emit {
                                    scope: vref.loc.scope,
                                    value: vid,
                                    span,
                                });
                                self.values.insert(vid, Defer::Ready(Value::Rect(orect)));
                                true
                            } else {
                                // default to a zero rectangle
                                self.errors.push(ExecError {
                                    span: Some(span.clone()),
                                    cell: vref.loc.cell,
                                    kind: ExecErrorKind::EmptyBbox,
                                });
                                let id = object_id(&mut self.next_id);
                                let state = self.cell_states.get_mut(&vref.loc.cell).unwrap();
                                let orect = Rect {
                                    id,
                                    layer: None,
                                    x0: 0.0.into(),
                                    y0: 0.0.into(),
                                    x1: 0.0.into(),
                                    y1: 0.0.into(),
                                    construction: true,
                                    span: Some(span),
                                };
                                state.objects.insert(orect.id, orect.clone().into());
                                self.values.insert(vid, Defer::Ready(Value::Rect(orect)));
                                true
                            }
                        } else {
                            false
                        }
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
                        let constraint = state.solver.constrain_eq0(expr);

                        state.constraint_span_map.insert(
                            constraint,
                            Span {
                                path: state.scopes[&vref.loc.scope].span.path.clone(),
                                span: c.expr.span,
                            },
                        );
                        self.values.insert(vid, Defer::Ready(Value::Nil));
                        true
                    } else {
                        false
                    }
                }
                "cons" => {
                    if let (Defer::Ready(head), Defer::Ready(tail)) = (
                        &self.values[&c.state.posargs[0]],
                        &self.values[&c.state.posargs[1]],
                    ) {
                        let val = match tail {
                            Value::SeqNil => {
                                vec![head.clone()]
                            }
                            Value::Seq(s) => {
                                let mut s = s.clone();
                                s.insert(0, head.clone());
                                s
                            }
                            _ => {
                                let span = self.span(&vref.loc, c.expr.span);
                                self.errors.push(ExecError {
                                    span: Some(span.clone()),
                                    cell: vref.loc.cell,
                                    kind: ExecErrorKind::InvalidType,
                                });
                                return Err(());
                            }
                        };
                        self.values.insert(vid, Defer::Ready(Value::Seq(val)));
                        true
                    } else {
                        false
                    }
                }
                "head" => {
                    if let Defer::Ready(head) = &self.values[&c.state.posargs[0]] {
                        let val = match head {
                            Value::SeqNil => Value::Nil,
                            Value::Seq(s) => s.first().cloned().unwrap_or(Value::Nil),
                            _ => {
                                let span = self.span(&vref.loc, c.expr.span);
                                self.errors.push(ExecError {
                                    span: Some(span.clone()),
                                    cell: vref.loc.cell,
                                    kind: ExecErrorKind::InvalidType,
                                });
                                return Err(());
                            }
                        };
                        self.values.insert(vid, Defer::Ready(val));
                        true
                    } else {
                        false
                    }
                }
                "tail" => {
                    if let Defer::Ready(lst) = &self.values[&c.state.posargs[0]] {
                        let val = match lst {
                            Value::SeqNil => Value::SeqNil,
                            Value::Seq(s) => Value::Seq(s[1..].to_vec()),
                            _ => {
                                let span = self.span(&vref.loc, c.expr.span);
                                self.errors.push(ExecError {
                                    span: Some(span.clone()),
                                    cell: vref.loc.cell,
                                    kind: ExecErrorKind::InvalidType,
                                });
                                return Err(());
                            }
                        };
                        self.values.insert(vid, Defer::Ready(val));
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
                    if let Some(mut args) = args {
                        assert_eq!(args.len(), 7);
                        let id = object_id(&mut self.next_id);
                        let span = self.span(&vref.loc, c.expr.span);
                        let state = self.cell_states.get_mut(&vref.loc.cell).unwrap();
                        let horiz = *args.pop().unwrap().as_ref().unwrap_bool();
                        let mut arg = || args.pop().unwrap().as_ref().unwrap_linear().clone();
                        let (nstop, pstop, coord, value, n, p) =
                            (arg(), arg(), arg(), arg(), arg(), arg());
                        let expr = p.clone() - n.clone() - value.clone();
                        let constraint = state.solver.constrain_eq0(expr);
                        let dim = Dimension {
                            id,
                            horiz,
                            nstop,
                            pstop,
                            coord,
                            value,
                            n,
                            p,
                            constraint,
                            span: Some(span.clone()),
                        };
                        state.constraint_span_map.insert(constraint, span.clone());
                        state.object_emit.push(ObjectEmit {
                            scope: vref.loc.scope,
                            object: dim.id,
                            span,
                        });
                        state.objects.insert(dim.id, dim.clone().into());
                        self.values.insert(vid, Defer::Ready(Value::Nil));
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
                                let span = self.span(&vref.loc, kwarg.value.span());
                                Some(self.values[vid].as_ref().get_ready().map(|refl| {
                                    match ((*refl.as_ref().unwrap_int() % 360) + 360) % 360 {
                                        0 => Rotation::R0,
                                        90 => Rotation::R90,
                                        180 => Rotation::R180,
                                        270 => Rotation::R270,
                                        _ => {
                                            self.errors.push(ExecError {
                                                span: Some(span),
                                                cell: vref.loc.cell,
                                                kind: ExecErrorKind::InvalidRotation,
                                            });
                                            Rotation::R0
                                        }
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
                    let construction = c
                        .expr
                        .args
                        .kwargs
                        .iter()
                        .zip(c.state.kwargs.iter())
                        .find_map(|(kwarg, vid)| {
                            if kwarg.name.name == "construction" {
                                Some(
                                    self.values[vid]
                                        .as_ref()
                                        .get_ready()
                                        .map(|v| *v.as_ref().unwrap_bool()),
                                )
                            } else {
                                None
                            }
                        });
                    let construction = match construction {
                        None => Some(None),
                        Some(None) => None,
                        Some(Some(v)) => Some(Some(v)),
                    };
                    if let (Some(refl), Some(angle), Some(construction)) =
                        (refl, angle, construction)
                    {
                        let id = object_id(&mut self.next_id);
                        let span = self.span(&vref.loc, c.expr.span);
                        let state = self.cell_states.get_mut(&vref.loc.cell).unwrap();
                        let inst = Instance {
                            id,
                            x: state.solver.new_var().into(),
                            y: state.solver.new_var().into(),
                            cell: *c.state.posargs.first().unwrap(),
                            reflect: refl.unwrap_or_default(),
                            angle: angle.unwrap_or_default(),
                            construction: construction.unwrap_or_default(),
                            span: span.clone(),
                        };
                        state.emit.push(Emit {
                            scope: vref.loc.scope,
                            value: vid,
                            span,
                        });
                        state.objects.insert(inst.id, inst.clone().into());
                        for (kwarg, rhs) in c.expr.args.kwargs.iter().zip(c.state.kwargs.iter()) {
                            let lhs = self.value_id();
                            let priority = match kwarg.name.name.as_str() {
                                "x" | "xi" => {
                                    self.values
                                        .insert(lhs, Defer::Ready(Value::Linear(inst.x.clone())));
                                    2
                                }
                                "y" | "yi" => {
                                    self.values
                                        .insert(lhs, Defer::Ready(Value::Linear(inst.y.clone())));
                                    1
                                }
                                _ => continue,
                            };
                            let span = self.span(&vref.loc, kwarg.span);
                            let defer = self.value_id();
                            self.values.insert(
                                defer,
                                DeferValue::Deferred(PartialEval {
                                    state: PartialEvalState::Constraint(PartialConstraint {
                                        lhs,
                                        rhs: *rhs,
                                        fallback: kwarg.name.name.ends_with('i'),
                                        priority,
                                        span,
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
                _ => {
                    // Must be calling a cell generator.
                    // User functions are never deferred.
                    let arg_vals = c
                        .state
                        .posargs
                        .iter()
                        .map(|v| {
                            self.values[v]
                                .get_ready()
                                .and_then(|v| CellArg::from_value(v, &state.solver))
                        })
                        .collect::<Option<Vec<CellArg>>>();
                    if let Some(arg_vals) = arg_vals {
                        let cell = self.execute_cell(
                            c.expr.metadata.0.unwrap(),
                            arg_vals,
                            c.expr
                                .scope_annotation
                                .as_ref()
                                .map(|anno| anno.name.as_str()),
                        )?;
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
                                _ => {
                                    let span = self.span(&vref.loc, unary_op.expr.span);
                                    self.errors.push(ExecError {
                                        span: Some(span.clone()),
                                        cell: vref.loc.cell,
                                        kind: ExecErrorKind::InvalidType,
                                    });
                                    return Err(());
                                }
                            };
                            self.values
                                .insert(vid, DeferValue::Ready(Value::Linear(res)));
                            true
                        }
                        Value::Int(v) => {
                            let res = match unary_op.op {
                                UnaryOp::Neg => -v,
                                _ => {
                                    let span = self.span(&vref.loc, unary_op.expr.span);
                                    self.errors.push(ExecError {
                                        span: Some(span.clone()),
                                        cell: vref.loc.cell,
                                        kind: ExecErrorKind::InvalidType,
                                    });
                                    return Err(());
                                }
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
                                self.span(&vref.loc, if_.expr.then.span),
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
                                self.span(&vref.loc, if_.expr.else_.span),
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
            PartialEvalState::Match(match_) => match match_.state {
                MatchExprState::Scrutinee(scrutinee) => {
                    if let Defer::Ready(val) = &self.values[&scrutinee] {
                        let variant = val.as_ref().unwrap_enum_value();
                        let arm = match_
                            .expr
                            .arms
                            .iter()
                            .find(|arm| *variant == arm.pattern.path.last().unwrap().name)
                            .unwrap();
                        let value = self.visit_expr(vref.loc, &arm.expr);
                        match_.state = MatchExprState::Value(value);
                        true
                    } else {
                        false
                    }
                }
                MatchExprState::Value(value) => {
                    if let Defer::Ready(val) = &self.values[&value] {
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
                                        unreachable!("cannot check equality between floats")
                                    }
                                    crate::ast::ComparisonOp::Ne => {
                                        unreachable!("cannot check inequality between floats")
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
                        (Value::EnumValue(vl), Value::EnumValue(vr)) => {
                            let res = match comparison_expr.expr.op {
                                crate::ast::ComparisonOp::Eq => vl == vr,
                                crate::ast::ComparisonOp::Ne => vl != vr,
                                _ => unreachable!(),
                            };
                            self.values.insert(vid, DeferValue::Ready(Value::Bool(res)));
                            true
                        }
                        (Value::Nil, Value::Nil) => {
                            let res = match comparison_expr.expr.op {
                                crate::ast::ComparisonOp::Eq => true,
                                crate::ast::ComparisonOp::Ne => false,
                                _ => unreachable!(),
                            };
                            self.values.insert(vid, DeferValue::Ready(Value::Bool(res)));
                            true
                        }
                        (Value::SeqNil, Value::SeqNil) => {
                            let res = match comparison_expr.expr.op {
                                crate::ast::ComparisonOp::Eq => true,
                                crate::ast::ComparisonOp::Ne => false,
                                _ => unreachable!(),
                            };
                            self.values.insert(vid, DeferValue::Ready(Value::Bool(res)));
                            true
                        }
                        (Value::Seq(x), Value::SeqNil) | (Value::SeqNil, Value::Seq(x)) => {
                            let res = match comparison_expr.expr.op {
                                crate::ast::ComparisonOp::Eq => x.is_empty(),
                                crate::ast::ComparisonOp::Ne => !x.is_empty(),
                                _ => unreachable!(),
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
                            let val = match field_access_expr.expr.field.name.as_str() {
                                "x0" => Value::Linear(rect.x0.clone()),
                                "x1" => Value::Linear(rect.x1.clone()),
                                "y0" => Value::Linear(rect.y0.clone()),
                                "y1" => Value::Linear(rect.y1.clone()),
                                "w" => Value::Linear(rect.x1.clone() - rect.x0.clone()),
                                "h" => Value::Linear(rect.y1.clone() - rect.y0.clone()),
                                "layer" => {
                                    if let Some(layer) = rect.layer.clone() {
                                        Value::String(layer)
                                    } else {
                                        let span =
                                            self.span(&vref.loc, field_access_expr.expr.span);
                                        self.errors.push(ExecError {
                                            span: Some(span),
                                            cell: vref.loc.cell,
                                            kind: ExecErrorKind::InvalidRotation,
                                        });
                                        Value::String("".to_string())
                                    }
                                }
                                _ => {
                                    let span = self.span(&vref.loc, field_access_expr.expr.span);
                                    self.errors.push(ExecError {
                                        span: Some(span.clone()),
                                        cell: vref.loc.cell,
                                        kind: ExecErrorKind::InvalidType,
                                    });
                                    return Err(());
                                }
                            };
                            self.values.insert(vid, DeferValue::Ready(val));
                            true
                        }
                        ValueRef::Inst(inst) => {
                            let val = match field_access_expr.expr.field.name.as_str() {
                                "x" => Some(Value::Linear(inst.x.clone())),
                                "y" => Some(Value::Linear(inst.y.clone())),
                                field => {
                                    if let Defer::Ready(cell) = &self.values[&inst.cell] {
                                        let cell_id = *cell.as_ref().unwrap_cell();
                                        // When a cell is ready, it must have been fully
                                        // solved/compiled, and therefore it will be in the
                                        // compiled cell map.
                                        let cell = &self.compiled_cells[&cell_id];
                                        let state =
                                            self.cell_states.get_mut(&vref.loc.cell).unwrap();
                                        let obj_id = &mut self.next_id;
                                        Some(Value::from_array(cell.field(field).unwrap().map(
                                            &mut move |v| match v {
                                                SolvedValue::Rect(rect) => {
                                                    let id = object_id(obj_id);
                                                    let rect = rect
                                                        .to_float()
                                                        .transform(inst.reflect, inst.angle);
                                                    let xrect = Rect {
                                                        id,
                                                        layer: rect.layer.clone(),
                                                        x0: LinearExpr::add(
                                                            rect.x0,
                                                            inst.x.clone(),
                                                        ),
                                                        y0: LinearExpr::add(
                                                            rect.y0,
                                                            inst.y.clone(),
                                                        ),
                                                        x1: LinearExpr::add(
                                                            rect.x1,
                                                            inst.x.clone(),
                                                        ),
                                                        y1: LinearExpr::add(
                                                            rect.y1,
                                                            inst.y.clone(),
                                                        ),
                                                        construction: rect.construction,
                                                        span: rect.span.clone(),
                                                    };
                                                    state
                                                        .objects
                                                        .insert(xrect.id, xrect.clone().into());
                                                    Value::Rect(xrect)
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
                                                    let id = object_id(obj_id);
                                                    let oinst = Instance {
                                                        id,
                                                        cell: cinst.cell_vid,
                                                        x: LinearExpr::add(inst.x.clone(), cx),
                                                        y: LinearExpr::add(inst.y.clone(), cy),
                                                        angle,
                                                        reflect,
                                                        construction: cinst.construction,
                                                        span: cinst.span.clone(),
                                                    };
                                                    state
                                                        .objects
                                                        .insert(oinst.id, oinst.clone().into());
                                                    Value::Inst(oinst)
                                                }
                                                _ => unreachable!(),
                                            },
                                        )))
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
                            let span = self.span(&vref.loc, field_access_expr.expr.span);
                            self.errors.push(ExecError {
                                span: Some(span),
                                cell: vref.loc.cell,
                                kind: ExecErrorKind::InvalidType,
                            });
                            return Err(());
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
                        state.fallback_constraints.push(FallbackConstraint {
                            priority: c.priority,
                            constraint: expr,
                            span: c.span.clone(),
                        });
                    } else {
                        let constraint = state.solver.constrain_eq0(expr);
                        state.constraint_span_map.insert(constraint, c.span.clone());
                    }
                    self.values.insert(vid, DeferValue::Ready(Value::Nil));
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
                        _ => unreachable!("invalid cast"),
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
        Ok(progress)
    }

    pub fn bbox(&self, cell: CellId) -> Option<Rect<f64>> {
        let mut bbox = None;
        let cell = &self.compiled_cells[&cell];
        for (_, o) in cell.objects.iter() {
            match o {
                SolvedValue::Rect(r) => bbox = bbox_union(bbox, Some(r.to_float())),
                SolvedValue::Instance(i) => {
                    let cell_bbox = self.bbox(i.cell).map(|r| r.transform(i.reflect, i.angle));
                    bbox = bbox_union(bbox, cell_bbox);
                }
                _ => (),
            }
        }
        bbox
    }
}

#[enumify]
#[derive(Debug, Clone)]
pub enum Value {
    EnumValue(String),
    String(String),
    Linear(LinearExpr),
    Int(i64),
    Rect(Rect<LinearExpr>),
    Bool(bool),
    Fn(FnDecl<Substr, VarIdTyMetadata>),
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
    CellFn(CellDecl<Substr, VarIdTyMetadata>),
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
    Seq(Vec<Value>),
    SeqNil,
    Nil,
}

impl Value {
    pub fn to_obj(&self) -> Option<Object> {
        match self {
            Self::Rect(r) => Some(Object::Rect(r.clone())),
            Self::Inst(i) => Some(Object::Inst(i.clone())),
            _ => None,
        }
    }

    pub fn from_arg(arg: &CellArg) -> Self {
        match arg {
            CellArg::Int(i) => Value::Int(*i),
            CellArg::Float(f) => Value::Linear(LinearExpr::from(*f)),
            CellArg::Seq(v) => Value::Seq(v.iter().map(Self::from_arg).collect()),
        }
    }

    fn obj_ids(&self) -> Option<Arrayed<ObjectId>> {
        match self {
            Value::Rect(r) => Some(Arrayed::Elem(r.id)),
            Value::Inst(i) => Some(Arrayed::Elem(i.id)),
            Value::Seq(s) => Some(Arrayed::Array(
                s.iter().map(|v| v.obj_ids()).collect::<Option<Vec<_>>>()?,
            )),
            _ => None,
        }
    }

    fn from_array(arr: Arrayed<Value>) -> Self {
        match arr {
            Arrayed::Elem(v) => v,
            Arrayed::Array(s) => Self::Seq(s.into_iter().map(Value::from_array).collect()),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Instance {
    pub id: ObjectId,
    pub x: LinearExpr,
    pub y: LinearExpr,
    pub cell: ValueId,
    pub reflect: bool,
    pub angle: Rotation,
    pub construction: bool,
    pub span: Span,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SolvedInstance {
    pub id: ObjectId,
    pub x: f64,
    pub y: f64,
    pub angle: Rotation,
    pub reflect: bool,
    pub construction: bool,
    pub cell: CellId,
    pub span: Span,
    /// The value ID of the cell being instantiated.
    ///
    /// For compiler internal use only.
    cell_vid: ValueId,
}

#[enumify]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum SolvedValue {
    Rect(Rect<(f64, LinearExpr)>),
    Text(Text<f64>),
    Dimension(Dimension<f64>),
    Instance(SolvedInstance),
}

#[enumify]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Object {
    Rect(Rect<LinearExpr>),
    Text(Text<LinearExpr>),
    Dimension(Dimension<LinearExpr>),
    Inst(Instance),
}

impl From<Rect<LinearExpr>> for Object {
    fn from(value: Rect<LinearExpr>) -> Self {
        Self::Rect(value)
    }
}

impl From<Text<LinearExpr>> for Object {
    fn from(value: Text<LinearExpr>) -> Self {
        Self::Text(value)
    }
}

impl From<Dimension<LinearExpr>> for Object {
    fn from(value: Dimension<LinearExpr>) -> Self {
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
    pub bindings: IndexMap<SeqNum, (String, Arrayed<ObjectId>)>,
    /// Dynamic children.
    pub children: IndexSet<ScopeId>,
    pub name: String,
    pub span: Span,
    /// Objects emitted in this scope.
    pub emit: Vec<(ObjectId, CompiledEmit)>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompiledCell {
    pub scopes: IndexMap<ScopeId, CompiledScope>,
    pub root: ScopeId,
    pub objects: IndexMap<ObjectId, SolvedValue>,
    pub fallback_constraints_used: Vec<LinearExpr>,
    pub unsolved_vars: IndexSet<Var>,
    pub inconsistent_constraints: IndexSet<ConstraintId>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[enumify]
pub enum Arrayed<T> {
    Elem(T),
    Array(Vec<Arrayed<T>>),
}

impl<T> Arrayed<T> {
    pub fn map<U, F>(&self, f: &mut F) -> Arrayed<U>
    where
        F: FnMut(&T) -> U,
    {
        match self {
            Self::Elem(x) => Arrayed::Elem(f(x)),
            Self::Array(x) => Arrayed::Array(x.iter().map(|x| x.map(f)).collect()),
        }
    }

    pub fn for_each<F>(&self, f: &mut F)
    where
        F: FnMut(&T),
    {
        match self {
            Self::Elem(x) => f(x),
            Self::Array(x) => x.iter().for_each(|x| x.for_each(f)),
        }
    }
}

impl CompiledCell {
    pub fn field(&self, name: &str) -> Option<Arrayed<&SolvedValue>> {
        let scope = &self.scopes[&self.root];
        scope.bindings.values().find_map(|(n, o)| {
            if n == name {
                Some(o.map(&mut |id| &self.objects[id]))
            } else {
                None
            }
        })
    }
}

pub fn bbox_union(b1: Option<Rect<f64>>, b2: Option<Rect<f64>>) -> Option<Rect<f64>> {
    match (b1, b2) {
        (Some(r1), Some(r2)) => Some(Rect {
            layer: None,
            x0: r1.x0.min(r2.x0),
            y0: r1.y0.min(r2.y0),
            x1: r1.x1.max(r2.x1),
            y1: r1.y1.max(r2.y1),
            id: r1.id,
            construction: true,
            span: None,
        }),
        (Some(r), None) | (None, Some(r)) => Some(r),
        (None, None) => None,
    }
}

pub fn bbox_text_union(b: Option<Rect<f64>>, t: &Text<f64>) -> Option<Rect<f64>> {
    match b {
        Some(r) => Some(Rect {
            layer: None,
            x0: r.x0.min(t.x),
            y0: r.y0.min(t.y),
            x1: r.x1.max(t.x),
            y1: r.y1.max(t.y),
            id: r.id,
            construction: true,
            span: None,
        }),
        None => Some(Rect {
            layer: None,
            x0: t.x,
            y0: t.y,
            x1: t.x,
            y1: t.y,
            id: t.id,
            construction: true,
            span: None,
        }),
    }
}

pub fn bbox_dim_union(bbox: Option<Rect<f64>>, dim: &Dimension<f64>) -> Option<Rect<f64>> {
    let perp_max = dim.coord.max(dim.pstop).max(dim.nstop);
    let perp_min = dim.coord.min(dim.pstop).min(dim.nstop);
    let par_max = dim.n.max(dim.p);
    let par_min = dim.n.min(dim.p);
    let (xmin, xmax, ymin, ymax) = if dim.horiz {
        (par_min, par_max, perp_min, perp_max)
    } else {
        (perp_min, perp_max, par_min, par_max)
    };
    match bbox {
        Some(r) => Some(Rect {
            layer: None,
            x0: r.x0.min(xmin),
            y0: r.y0.min(ymin),
            x1: r.x1.max(xmax),
            y1: r.y1.max(ymax),
            id: r.id,
            construction: true,
            span: None,
        }),
        None => Some(Rect {
            layer: None,
            x0: xmin,
            y0: ymin,
            x1: xmax,
            y1: ymax,
            id: ObjectId(0), // FIXME: should not need to allocate an object ID
            construction: true,
            span: None,
        }),
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StaticError {
    pub span: Span,
    pub kind: StaticErrorKind,
}

#[derive(Debug, Clone, Serialize, Deserialize, Error)]
pub enum StaticErrorKind {
    /// Multiple declarations with the same name.
    ///
    /// For example, two cells named `my_cell`.
    #[error("duplicate name declaration")]
    DuplicateNameDeclaration,
    /// Attempted to declare an object with the same name as a built-in object.
    ///
    /// For example, users cannot declare cells or functions named `rect`.
    #[error("redeclaration of built-in object")]
    RedeclarationOfBuiltin,
    /// Attempted to treat a non-enum object (e.g. a function or local variable) like an enum using the "::"
    /// operator.
    #[error("expected an enum")]
    NotAnEnum,
    /// Attempted to create a value using an enum variant that is not declared by the enum.
    #[error("not a variant of the enum: {0}")]
    InvalidVariant(String),
    /// A cell had an expression in tail position, which is not permitted.
    #[error("cells may not have an expression in tail position")]
    CellWithTailExpr,
    /// If conditions must have type bool.
    #[error("if conditions must have type bool")]
    IfCondNotBool,
    /// Branches in expresssions must evaluate to the same type.
    #[error("branches must evaluate to same type")]
    BranchesDifferentTypes,
    /// Multiple match arms with matching patterns.
    #[error("match arms must be distinct")]
    DuplicateMatchArm,
    /// Match arms must be comprehensive.
    #[error("match arms must be comprehensive")]
    MatchArmsNotComprehensive,
    /// The operands in a binary expression must have the same type.
    #[error("operands of binary expression must have the same type")]
    BinOpMismatchedTypes,
    /// Cannot compare equality or inequality of floating point numbers.
    #[error("cannot compare equality or inequality of floating point numbers")]
    FloatEquality,
    /// Cannot perform greater/less than comparisons on enum values.
    #[error("cannot perform greater/less than comparisons on enum values")]
    EnumsNotOrd,
    /// Cannot perform greater/less than comparisons on nil values.
    #[error("cannot perform greater/less than comparisons on nil")]
    NilNotOrd,
    /// Cannot perform greater/less than comparisons on seq nil values.
    #[error("cannot perform greater/less than comparisons on seq nil")]
    SeqNilNotOrd,
    /// Must compare sequences for equality/inequality to nil.
    #[error("sequences can only be compared for equality/inequality to seq nil (`[]`)")]
    SeqMustCompareEqSeqNil,
    /// A type cannot be used in a binary expression.
    #[error("type cannot be used in a binary expression")]
    BinOpInvalidType,
    /// A type cannot be used in a unary operation.
    #[error("type cannot be used in a unary operation")]
    UnaryOpInvalidType,
    /// A type cannot be used in a comparison expression.
    #[error("type cannot be used in comparison expression")]
    ComparisonInvalidType,
    /// An unknown type, i.e. a type that has not been declared.
    #[error("unknown type")]
    UnknownType,
    /// No field on object of the given type.
    #[error("no field {field} on type {ty:?}")]
    NoFieldOnTy { field: String, ty: Ty },
    /// Incorrect type.
    #[error("expected type {expected:?}, found {found:?}")]
    IncorrectTy { expected: Ty, found: Ty },
    /// Incorrect type category.
    #[error("expected type category {expected}, found {found:?}")]
    IncorrectTyCategory { found: Ty, expected: String },
    /// Called a function or cell with the wrong number of positional arguments.
    #[error("expected {expected} position arguments, found {found}")]
    CallIncorrectPositionalArity { expected: usize, found: usize },
    /// Invalid keyword argument.
    #[error("invalid keyword argument")]
    InvalidKwArg,
    /// Duplicate keyword argument.
    #[error("duplicate keyword argument")]
    DuplicateKwArg,
    /// Identifier used without being declared.
    #[error("identifier used without being declared")]
    UndeclaredVar,
    /// Attempted to use an object of the given type as the function of a call expression.
    #[error("cannot call type {0:?}")]
    CannotCall(Ty),
    /// Cannot perform the requested type cast.
    #[error("invalid type cast")]
    InvalidCast,
    /// Module doesn't exist.
    #[error("module doesn't exist")]
    InvalidMod,
    /// Error during lexing.
    #[error("error during lexing")]
    LexError,
    /// Error during parsing.
    #[error("error during parsing")]
    ParseError,
    /// Invalid LYP file.
    #[error("invalid LYP file")]
    InvalidLyp,
    /// Unimplemented.
    #[error("unimplemented")]
    Unimplemented,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecError {
    pub span: Option<Span>,
    pub cell: CellId,
    pub kind: ExecErrorKind,
}

#[derive(Debug, Clone, Serialize, Deserialize, Error)]
pub enum ExecErrorKind {
    /// A non-Manhattan rotation.
    #[error("non-Manhattan rotation")]
    InvalidRotation,
    /// An invalid cell was specified for execution.
    #[error("invalid cell")]
    InvalidCell,
    /// A cell is underconstrained.
    #[error("cell is underconstrained")]
    Underconstrained,
    /// Illegal layer (not defined in layer properties).
    #[error("layer {0} is not defined in layer properties")]
    IllegalLayer(String),
    /// Inconsistent constraint.
    #[error("inconsistent constraint")]
    InconsistentConstraint(ConstraintId),
    /// A cell or instance had an empty bounding box.
    #[error("empty bbox")]
    EmptyBbox,
    /// Field is empty (analogous to None).
    #[error("empty field (field was not assigned a value)")]
    EmptyField,
    /// Edges of a rect are in the wrong order (e.g. x0 > x1 or y0 > y1).
    #[error("rect edges are in the wrong order: {0}")]
    FlippedRect(String),
    /// Operation on an incompatible type, usually due to erroneous use of `Any`.
    #[error("operation on an incompatible type (check usage of `Any`)")]
    InvalidType,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[enumify]
pub enum CompileOutput {
    FatalParseErrors,
    StaticErrors(StaticErrorCompileOutput),
    ExecErrors(ExecErrorCompileOutput),
    Valid(CompiledData),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StaticErrorCompileOutput {
    pub errors: Vec<StaticError>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecErrorCompileOutput {
    pub errors: Vec<ExecError>,
    pub output: Option<CompiledData>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompiledData {
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

type DeferValue<T> = Defer<Value, PartialEval<T>>;

#[derive(Debug, Clone)]
struct PartialEval<T: AstMetadata> {
    state: PartialEvalState<T>,
    loc: DynLoc,
}

#[derive(Debug, Clone)]
enum PartialEvalState<T: AstMetadata> {
    If(Box<PartialIfExpr<T>>),
    Match(Box<PartialMatchExpr<T>>),
    Comparison(Box<PartialComparisonExpr<T>>),
    BinOp(PartialBinOp),
    UnaryOp(PartialUnaryOp<T>),
    Call(Box<PartialCallExpr<T>>),
    FieldAccess(Box<PartialFieldAccessExpr<T>>),
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
    priority: i32,
    span: Span,
}

#[derive(Debug, Clone)]
struct PartialBinOp {
    lhs: ValueId,
    rhs: ValueId,
    op: BinOp,
}

#[derive(Debug, Clone)]
struct PartialUnaryOp<T: AstMetadata> {
    operand: ValueId,
    op: UnaryOp,
    expr: Box<UnaryOpExpr<Substr, T>>,
}

#[derive(Debug, Clone)]
struct PartialIfExpr<T: AstMetadata> {
    expr: IfExpr<Substr, T>,
    state: IfExprState,
}

#[derive(Debug, Clone)]
struct PartialMatchExpr<T: AstMetadata> {
    expr: MatchExpr<Substr, T>,
    state: MatchExprState,
}

#[derive(Debug, Clone)]
pub enum IfExprState {
    Cond(ValueId),
    Then(ValueId),
    Else(ValueId),
}

#[derive(Debug, Clone)]
pub enum MatchExprState {
    Scrutinee(ValueId),
    Value(ValueId),
}

#[derive(Debug, Clone)]
struct PartialCallExpr<T: AstMetadata> {
    expr: CallExpr<Substr, T>,
    state: CallExprState,
}

#[derive(Debug, Clone)]
pub struct CallExprState {
    posargs: Vec<ValueId>,
    kwargs: Vec<ValueId>,
}

#[derive(Debug, Clone)]
struct PartialComparisonExpr<T: AstMetadata> {
    expr: ComparisonExpr<Substr, T>,
    state: ComparisonExprState,
}

#[derive(Debug, Clone)]
pub struct ComparisonExprState {
    left: ValueId,
    right: ValueId,
}

#[derive(Debug, Clone)]
struct PartialFieldAccessExpr<T: AstMetadata> {
    expr: FieldAccessExpr<Substr, T>,
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

impl<T> Rect<(f64, T)> {
    pub fn to_float(&self) -> Rect<f64> {
        Rect {
            id: self.id,
            layer: self.layer.clone(),
            x0: self.x0.0,
            y0: self.y0.0,
            x1: self.x1.0,
            y1: self.y1.0,
            construction: self.construction,
            span: self.span.clone(),
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
            construction: self.construction,
            span: None,
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

impl CompiledData {
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
                obj.for_each(&mut |obj| match &cell.objects[obj] {
                    SolvedValue::Rect(r) => {
                        set.insert(r.id, format!("{}{}", name_prefix, name));
                    }
                    SolvedValue::Instance(inst) => {
                        set.insert(inst.id, format!("{}{}", name_prefix, name));
                    }
                    _ => (),
                });
            }
        }
    }
}
