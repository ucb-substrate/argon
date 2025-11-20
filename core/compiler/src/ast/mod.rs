use std::{fmt::Debug, path::PathBuf};

use derive_where::derive_where;
use indexmap::IndexMap;
use itertools::Itertools;
use serde::{Deserialize, Serialize, de::DeserializeOwned};

use crate::ast::annotated::AnnotatedAst;

pub mod annotated;

pub type ModPath = Vec<String>;
pub type WorkspaceAst<T> = IndexMap<ModPath, AnnotatedAst<T>>;

#[derive(Debug, Clone, Hash, PartialEq, Eq, Serialize, Deserialize)]
pub struct Span {
    pub path: PathBuf,
    pub span: cfgrammar::Span,
}

#[derive_where(Debug, Clone, Serialize, Deserialize; S)]
pub struct Ast<S, T: AstMetadata> {
    pub decls: Vec<Decl<S, T>>,
    pub span: cfgrammar::Span,
}

#[derive_where(Debug, Clone, Serialize, Deserialize; S)]
pub struct ModDecl<S, T: AstMetadata> {
    pub ident: Ident<S, T>,
    pub span: cfgrammar::Span,
}

#[derive_where(Debug, Clone, Serialize, Deserialize; S)]
pub enum Decl<S, T: AstMetadata> {
    Enum(EnumDecl<S, T>),
    Struct(StructDecl<S, T>),
    Constant(ConstantDecl<S, T>),
    Cell(CellDecl<S, T>),
    Mod(ModDecl<S, T>),
    Fn(FnDecl<S, T>),
}

#[derive_where(Debug, Clone, Serialize, Deserialize; S)]
pub struct IdentPath<S, T: AstMetadata> {
    pub path: Vec<Ident<S, T>>,
    pub metadata: T::IdentPath,
    pub span: cfgrammar::Span,
}

#[derive_where(Debug, Clone, Serialize, Deserialize; S)]
pub struct Ident<S, T: AstMetadata> {
    pub span: cfgrammar::Span,
    pub name: S,
    pub metadata: T::Ident,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct NilLiteral {
    pub span: cfgrammar::Span,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct SeqNilLiteral {
    pub span: cfgrammar::Span,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct FloatLiteral {
    pub span: cfgrammar::Span,
    pub value: f64,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct IntLiteral {
    pub span: cfgrammar::Span,
    pub value: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StringLiteral<S> {
    pub span: cfgrammar::Span,
    pub value: S,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct BoolLiteral {
    pub span: cfgrammar::Span,
    pub value: bool,
}

#[derive_where(Debug, Clone, Serialize, Deserialize; S)]
pub struct EnumDecl<S, T: AstMetadata> {
    pub name: Ident<S, T>,
    pub variants: Vec<Ident<S, T>>,
    pub metadata: T::EnumDecl,
}

#[derive_where(Debug, Clone, Serialize, Deserialize; S)]
pub struct StructDecl<S, T: AstMetadata> {
    pub name: Ident<S, T>,
    pub fields: Vec<StructField<S, T>>,
    pub span: cfgrammar::Span,
    pub metadata: T::StructDecl,
}

#[derive_where(Debug, Clone, Serialize, Deserialize; S)]
pub struct StructField<S, T: AstMetadata> {
    pub name: Ident<S, T>,
    pub ty: Ident<S, T>,
    pub span: cfgrammar::Span,
    pub metadata: T::StructField,
}

#[derive_where(Debug, Clone, Serialize, Deserialize; S)]
pub struct CellDecl<S, T: AstMetadata> {
    pub name: Ident<S, T>,
    pub args: Vec<ArgDecl<S, T>>,
    pub scope: Scope<S, T>,
    pub span: cfgrammar::Span,
    pub metadata: T::CellDecl,
}

#[derive_where(Debug, Clone, Serialize, Deserialize; S)]
pub enum TySpecKind<S, T: AstMetadata> {
    Ident(Ident<S, T>),
    Seq(Box<TySpec<S, T>>),
}

#[derive_where(Debug, Clone, Serialize, Deserialize; S)]
pub struct TySpec<S, T: AstMetadata> {
    pub kind: TySpecKind<S, T>,
    pub span: cfgrammar::Span,
}

#[derive_where(Debug, Clone, Serialize, Deserialize; S)]
pub struct FnDecl<S, T: AstMetadata> {
    pub name: Ident<S, T>,
    pub args: Vec<ArgDecl<S, T>>,
    pub return_ty: Option<TySpec<S, T>>,
    pub scope: Scope<S, T>,
    pub span: cfgrammar::Span,
    pub metadata: T::FnDecl,
}

#[derive_where(Debug, Clone, Serialize, Deserialize; S)]
pub struct ConstantDecl<S, T: AstMetadata> {
    pub name: Ident<S, T>,
    pub ty: Ident<S, T>,
    pub value: Expr<S, T>,
    pub metadata: T::ConstantDecl,
}

#[derive_where(Debug, Clone, Serialize, Deserialize; S)]
pub struct Scope<S, T: AstMetadata> {
    pub scope_annotation: Option<Ident<S, T>>,
    pub span: cfgrammar::Span,
    pub stmts: Vec<Statement<S, T>>,
    pub tail: Option<Expr<S, T>>,
    pub metadata: T::Scope,
}

#[derive_where(Debug, Clone, Serialize, Deserialize; S)]
pub enum Statement<S, T: AstMetadata> {
    Expr { value: Expr<S, T>, semicolon: bool },
    LetBinding(LetBinding<S, T>),
}

#[derive_where(Debug, Clone, Serialize, Deserialize; S)]
pub struct LetBinding<S, T: AstMetadata> {
    pub name: Ident<S, T>,
    pub value: Expr<S, T>,
    pub metadata: T::LetBinding,
    pub span: cfgrammar::Span,
}

#[derive(Debug, Copy, Clone, Eq, PartialEq, Serialize, Deserialize)]
pub enum BinOp {
    Add,
    Sub,
    Mul,
    Div,
}

#[derive(Debug, Copy, Clone, Eq, PartialEq, Serialize, Deserialize)]
pub enum UnaryOp {
    Not,
    Neg,
}

#[derive(Debug, Copy, Clone, Eq, PartialEq, Serialize, Deserialize)]
pub enum ComparisonOp {
    Eq,
    Ne,
    Geq,
    Gt,
    Leq,
    Lt,
}

#[derive_where(Debug, Clone, Serialize, Deserialize; S)]
pub enum Expr<S, T: AstMetadata> {
    If(Box<IfExpr<S, T>>),
    Match(Box<MatchExpr<S, T>>),
    Comparison(Box<ComparisonExpr<S, T>>),
    BinOp(Box<BinOpExpr<S, T>>),
    UnaryOp(Box<UnaryOpExpr<S, T>>),
    Call(CallExpr<S, T>),
    Emit(Box<EmitExpr<S, T>>),
    FieldAccess(Box<FieldAccessExpr<S, T>>),
    IdentPath(IdentPath<S, T>),
    Nil(NilLiteral),
    SeqNil(SeqNilLiteral),
    FloatLiteral(FloatLiteral),
    IntLiteral(IntLiteral),
    StringLiteral(StringLiteral<S>),
    BoolLiteral(BoolLiteral),
    Scope(Box<Scope<S, T>>),
    Cast(Box<CastExpr<S, T>>),
}

#[derive_where(Debug, Clone, Serialize, Deserialize; S)]
pub struct IfExpr<S, T: AstMetadata> {
    pub scope_annotation: Option<Ident<S, T>>,
    pub cond: Expr<S, T>,
    pub then: Scope<S, T>,
    pub else_: Scope<S, T>,
    pub span: cfgrammar::Span,
    pub metadata: T::IfExpr,
}

#[derive_where(Debug, Clone, Serialize, Deserialize; S)]
pub struct MatchExpr<S, T: AstMetadata> {
    pub scrutinee: Expr<S, T>,
    pub arms: Vec<MatchArm<S, T>>,
    pub span: cfgrammar::Span,
    pub metadata: T::MatchExpr,
}

#[derive_where(Debug, Clone, Serialize, Deserialize; S)]
pub struct MatchArm<S, T: AstMetadata> {
    pub pattern: IdentPath<S, T>,
    pub expr: Expr<S, T>,
    pub span: cfgrammar::Span,
}

#[derive_where(Debug, Clone, Serialize, Deserialize; S)]
pub struct BinOpExpr<S, T: AstMetadata> {
    pub op: BinOp,
    pub left: Expr<S, T>,
    pub right: Expr<S, T>,
    pub span: cfgrammar::Span,
    pub metadata: T::BinOpExpr,
}

#[derive_where(Debug, Clone, Serialize, Deserialize; S)]
pub struct UnaryOpExpr<S, T: AstMetadata> {
    pub op: UnaryOp,
    pub operand: Expr<S, T>,
    pub span: cfgrammar::Span,
    pub metadata: T::UnaryOpExpr,
}

#[derive_where(Debug, Clone, Serialize, Deserialize; S)]
pub struct ComparisonExpr<S, T: AstMetadata> {
    pub op: ComparisonOp,
    pub left: Expr<S, T>,
    pub right: Expr<S, T>,
    pub span: cfgrammar::Span,
    pub metadata: T::ComparisonExpr,
}

#[derive_where(Debug, Clone, Serialize, Deserialize; S)]
pub struct FieldAccessExpr<S, T: AstMetadata> {
    pub base: Expr<S, T>,
    pub field: Ident<S, T>,
    pub span: cfgrammar::Span,
    pub metadata: T::FieldAccessExpr,
}

#[derive_where(Debug, Clone, Serialize, Deserialize; S)]
pub struct CallExpr<S, T: AstMetadata> {
    pub scope_annotation: Option<Ident<S, T>>,
    pub func: IdentPath<S, T>,
    pub args: Args<S, T>,
    pub span: cfgrammar::Span,
    pub metadata: T::CallExpr,
}

#[derive_where(Debug, Clone, Serialize, Deserialize; S)]
pub struct EmitExpr<S, T: AstMetadata> {
    pub value: Expr<S, T>,
    pub span: cfgrammar::Span,
    pub metadata: T::EmitExpr,
}

#[derive_where(Debug, Clone, Serialize, Deserialize; S)]
pub struct Args<S, T: AstMetadata> {
    pub posargs: Vec<Expr<S, T>>,
    pub kwargs: Vec<KwArgValue<S, T>>,
    pub span: cfgrammar::Span,
    pub metadata: T::Args,
}

#[derive_where(Debug, Clone, Serialize, Deserialize; S)]
pub struct KwArgValue<S, T: AstMetadata> {
    pub name: Ident<S, T>,
    pub value: Expr<S, T>,
    pub span: cfgrammar::Span,
    pub metadata: T::KwArgValue,
}

#[derive_where(Debug, Clone, Serialize, Deserialize; S)]
pub struct ArgDecl<S, T: AstMetadata> {
    pub name: Ident<S, T>,
    pub ty: TySpec<S, T>,
    pub metadata: T::ArgDecl,
}

#[derive_where(Debug, Clone, Serialize, Deserialize; S)]
pub struct CastExpr<S, T: AstMetadata> {
    pub value: Expr<S, T>,
    pub ty: TySpec<S, T>,
    pub span: cfgrammar::Span,
    pub metadata: T::CastExpr,
}

pub(crate) fn parse_float(s: &str) -> Result<f64, ()> {
    s.parse::<f64>().map_err(|_| ())
}

pub(crate) fn parse_int(s: &str) -> Result<i64, ()> {
    s.parse::<i64>().map_err(|_| ())
}

pub(crate) fn flatten<T>(lhs: Result<Vec<T>, ()>, rhs: Result<T, ()>) -> Result<Vec<T>, ()> {
    let mut flt = lhs?;
    flt.push(rhs?);
    Ok(flt)
}

impl<S, T: AstMetadata> Expr<S, T> {
    pub fn span(&self) -> cfgrammar::Span {
        match self {
            Self::If(x) => x.span,
            Self::Match(x) => x.span,
            Self::Comparison(x) => x.span,
            Self::BinOp(x) => x.span,
            Self::UnaryOp(x) => x.span,
            Self::Call(x) => x.span,
            Self::Emit(x) => x.span,
            Self::IdentPath(x) => x.span,
            Self::FieldAccess(x) => x.span,
            Self::Nil(x) => x.span,
            Self::SeqNil(x) => x.span,
            Self::FloatLiteral(x) => x.span,
            Self::IntLiteral(x) => x.span,
            Self::StringLiteral(x) => x.span,
            Self::BoolLiteral(x) => x.span,
            Self::Scope(x) => x.span,
            Self::Cast(x) => x.span,
        }
    }
}

pub trait AstMetadata {
    type Ident: Debug + Clone + Serialize + DeserializeOwned;
    type IdentPath: Debug + Clone + Serialize + DeserializeOwned;
    type EnumDecl: Debug + Clone + Serialize + DeserializeOwned;
    type StructDecl: Debug + Clone + Serialize + DeserializeOwned;
    type StructField: Debug + Clone + Serialize + DeserializeOwned;
    type CellDecl: Debug + Clone + Serialize + DeserializeOwned;
    type FnDecl: Debug + Clone + Serialize + DeserializeOwned;
    type ConstantDecl: Debug + Clone + Serialize + DeserializeOwned;
    type LetBinding: Debug + Clone + Serialize + DeserializeOwned;
    type IfExpr: Debug + Clone + Serialize + DeserializeOwned;
    type MatchExpr: Debug + Clone + Serialize + DeserializeOwned;
    type BinOpExpr: Debug + Clone + Serialize + DeserializeOwned;
    type UnaryOpExpr: Debug + Clone + Serialize + DeserializeOwned;
    type ComparisonExpr: Debug + Clone + Serialize + DeserializeOwned;
    type FieldAccessExpr: Debug + Clone + Serialize + DeserializeOwned;
    type CallExpr: Debug + Clone + Serialize + DeserializeOwned;
    type EmitExpr: Debug + Clone + Serialize + DeserializeOwned;
    type Args: Debug + Clone + Serialize + DeserializeOwned;
    type KwArgValue: Debug + Clone + Serialize + DeserializeOwned;
    type ArgDecl: Debug + Clone + Serialize + DeserializeOwned;
    type Scope: Debug + Clone + Serialize + DeserializeOwned;
    type Typ: Debug + Clone + Serialize + DeserializeOwned;
    type CastExpr: Debug + Clone + Serialize + DeserializeOwned;
}

pub trait AstTransformer {
    type InputMetadata: AstMetadata;
    type OutputMetadata: AstMetadata;
    type InputS;
    type OutputS;

    fn dispatch_ident(
        &mut self,
        input: &Ident<Self::InputS, Self::InputMetadata>,
    ) -> <Self::OutputMetadata as AstMetadata>::Ident;
    fn dispatch_ident_path(
        &mut self,
        input: &IdentPath<Self::InputS, Self::InputMetadata>,
    ) -> <Self::OutputMetadata as AstMetadata>::IdentPath;
    fn dispatch_enum_decl(
        &mut self,
        input: &EnumDecl<Self::InputS, Self::InputMetadata>,
        name: &Ident<Self::OutputS, Self::OutputMetadata>,
        variants: &[Ident<Self::OutputS, Self::OutputMetadata>],
    ) -> <Self::OutputMetadata as AstMetadata>::EnumDecl;
    fn dispatch_cell_decl(
        &mut self,
        input: &CellDecl<Self::InputS, Self::InputMetadata>,
        name: &Ident<Self::OutputS, Self::OutputMetadata>,
        args: &[ArgDecl<Self::OutputS, Self::OutputMetadata>],
        scope: &Scope<Self::OutputS, Self::OutputMetadata>,
    ) -> <Self::OutputMetadata as AstMetadata>::CellDecl;
    fn dispatch_fn_decl(
        &mut self,
        input: &FnDecl<Self::InputS, Self::InputMetadata>,
        name: &Ident<Self::OutputS, Self::OutputMetadata>,
        args: &[ArgDecl<Self::OutputS, Self::OutputMetadata>],
        return_ty: &Option<TySpec<Self::OutputS, Self::OutputMetadata>>,
        scope: &Scope<Self::OutputS, Self::OutputMetadata>,
    ) -> <Self::OutputMetadata as AstMetadata>::FnDecl;
    fn dispatch_constant_decl(
        &mut self,
        input: &ConstantDecl<Self::InputS, Self::InputMetadata>,
        name: &Ident<Self::OutputS, Self::OutputMetadata>,
        ty: &Ident<Self::OutputS, Self::OutputMetadata>,
        value: &Expr<Self::OutputS, Self::OutputMetadata>,
    ) -> <Self::OutputMetadata as AstMetadata>::ConstantDecl;
    fn dispatch_let_binding(
        &mut self,
        input: &LetBinding<Self::InputS, Self::InputMetadata>,
        name: &Ident<Self::OutputS, Self::OutputMetadata>,
        value: &Expr<Self::OutputS, Self::OutputMetadata>,
    ) -> <Self::OutputMetadata as AstMetadata>::LetBinding;
    fn dispatch_if_expr(
        &mut self,
        input: &IfExpr<Self::InputS, Self::InputMetadata>,
        cond: &Expr<Self::OutputS, Self::OutputMetadata>,
        then: &Scope<Self::OutputS, Self::OutputMetadata>,
        else_: &Scope<Self::OutputS, Self::OutputMetadata>,
    ) -> <Self::OutputMetadata as AstMetadata>::IfExpr;
    fn dispatch_match_expr(
        &mut self,
        input: &MatchExpr<Self::InputS, Self::InputMetadata>,
        scrutinee: &Expr<Self::OutputS, Self::OutputMetadata>,
        arms: &[MatchArm<Self::OutputS, Self::OutputMetadata>],
    ) -> <Self::OutputMetadata as AstMetadata>::MatchExpr;
    fn dispatch_bin_op_expr(
        &mut self,
        input: &BinOpExpr<Self::InputS, Self::InputMetadata>,
        left: &Expr<Self::OutputS, Self::OutputMetadata>,
        right: &Expr<Self::OutputS, Self::OutputMetadata>,
    ) -> <Self::OutputMetadata as AstMetadata>::BinOpExpr;
    fn dispatch_unary_op_expr(
        &mut self,
        input: &UnaryOpExpr<Self::InputS, Self::InputMetadata>,
        operand: &Expr<Self::OutputS, Self::OutputMetadata>,
    ) -> <Self::OutputMetadata as AstMetadata>::UnaryOpExpr;
    fn dispatch_comparison_expr(
        &mut self,
        input: &ComparisonExpr<Self::InputS, Self::InputMetadata>,
        left: &Expr<Self::OutputS, Self::OutputMetadata>,
        right: &Expr<Self::OutputS, Self::OutputMetadata>,
    ) -> <Self::OutputMetadata as AstMetadata>::ComparisonExpr;
    fn dispatch_cast(
        &mut self,
        input: &CastExpr<Self::InputS, Self::InputMetadata>,
        value: &Expr<Self::OutputS, Self::OutputMetadata>,
        ty: &TySpec<Self::OutputS, Self::OutputMetadata>,
    ) -> <Self::OutputMetadata as AstMetadata>::CastExpr;
    fn dispatch_field_access_expr(
        &mut self,
        input: &FieldAccessExpr<Self::InputS, Self::InputMetadata>,
        base: &Expr<Self::OutputS, Self::OutputMetadata>,
        field: &Ident<Self::OutputS, Self::OutputMetadata>,
    ) -> <Self::OutputMetadata as AstMetadata>::FieldAccessExpr;
    fn dispatch_call_expr(
        &mut self,
        input: &CallExpr<Self::InputS, Self::InputMetadata>,
        func: &IdentPath<Self::OutputS, Self::OutputMetadata>,
        args: &Args<Self::OutputS, Self::OutputMetadata>,
    ) -> <Self::OutputMetadata as AstMetadata>::CallExpr;
    fn dispatch_emit_expr(
        &mut self,
        input: &EmitExpr<Self::InputS, Self::InputMetadata>,
        value: &Expr<Self::OutputS, Self::OutputMetadata>,
    ) -> <Self::OutputMetadata as AstMetadata>::EmitExpr;
    fn dispatch_args(
        &mut self,
        input: &Args<Self::InputS, Self::InputMetadata>,
        posargs: &[Expr<Self::OutputS, Self::OutputMetadata>],
        kwargs: &[KwArgValue<Self::OutputS, Self::OutputMetadata>],
    ) -> <Self::OutputMetadata as AstMetadata>::Args;
    fn dispatch_kw_arg_value(
        &mut self,
        input: &KwArgValue<Self::InputS, Self::InputMetadata>,
        name: &Ident<Self::OutputS, Self::OutputMetadata>,
        value: &Expr<Self::OutputS, Self::OutputMetadata>,
    ) -> <Self::OutputMetadata as AstMetadata>::KwArgValue;
    fn dispatch_arg_decl(
        &mut self,
        input: &ArgDecl<Self::InputS, Self::InputMetadata>,
        name: &Ident<Self::OutputS, Self::OutputMetadata>,
        ty: &TySpec<Self::OutputS, Self::OutputMetadata>,
    ) -> <Self::OutputMetadata as AstMetadata>::ArgDecl;
    fn dispatch_scope(
        &mut self,
        input: &Scope<Self::InputS, Self::InputMetadata>,
        stmts: &[Statement<Self::OutputS, Self::OutputMetadata>],
        tail: &Option<Expr<Self::OutputS, Self::OutputMetadata>>,
    ) -> <Self::OutputMetadata as AstMetadata>::Scope;
    fn enter_scope(&mut self, _input: &Scope<Self::InputS, Self::InputMetadata>) {}
    fn exit_scope(
        &mut self,
        _input: &Scope<Self::InputS, Self::InputMetadata>,
        _output: &Scope<Self::OutputS, Self::OutputMetadata>,
    ) {
    }

    fn transform_s(&mut self, s: &Self::InputS) -> Self::OutputS;

    fn transform_ident(
        &mut self,
        input: &Ident<Self::InputS, Self::InputMetadata>,
    ) -> Ident<Self::OutputS, Self::OutputMetadata> {
        let name = self.transform_s(&input.name);
        let metadata = self.dispatch_ident(input);
        Ident {
            span: input.span,
            name,
            metadata,
        }
    }

    fn transform_ident_path(
        &mut self,
        input: &IdentPath<Self::InputS, Self::InputMetadata>,
    ) -> IdentPath<Self::OutputS, Self::OutputMetadata> {
        let metadata = self.dispatch_ident_path(input);
        IdentPath {
            path: input
                .path
                .iter()
                .map(|ident| self.transform_ident(ident))
                .collect(),
            metadata,
            span: input.span,
        }
    }

    fn transform_ty_spec(
        &mut self,
        input: &TySpec<Self::InputS, Self::InputMetadata>,
    ) -> TySpec<Self::OutputS, Self::OutputMetadata> {
        let kind = match &input.kind {
            TySpecKind::Ident(inner) => TySpecKind::Ident(self.transform_ident(inner)),
            TySpecKind::Seq(inner) => TySpecKind::Seq(Box::new(self.transform_ty_spec(inner))),
        };
        TySpec {
            kind,
            span: input.span,
        }
    }

    fn transform_enum_decl(
        &mut self,
        input: &EnumDecl<Self::InputS, Self::InputMetadata>,
    ) -> EnumDecl<Self::OutputS, Self::OutputMetadata> {
        let name = self.transform_ident(&input.name);
        let variants = input
            .variants
            .iter()
            .map(|variant| self.transform_ident(variant))
            .collect_vec();
        let metadata = self.dispatch_enum_decl(input, &name, &variants);
        EnumDecl {
            name,
            variants,
            metadata,
        }
    }
    fn transform_cell_decl(
        &mut self,
        input: &CellDecl<Self::InputS, Self::InputMetadata>,
    ) -> CellDecl<Self::OutputS, Self::OutputMetadata> {
        let name = self.transform_ident(&input.name);
        let args = input
            .args
            .iter()
            .map(|arg| self.transform_arg_decl(arg))
            .collect_vec();
        let scope = self.transform_scope(&input.scope);
        let metadata = self.dispatch_cell_decl(input, &name, &args, &scope);
        CellDecl {
            name,
            args,
            scope,
            span: input.span,
            metadata,
        }
    }
    fn transform_fn_decl(
        &mut self,
        input: &FnDecl<Self::InputS, Self::InputMetadata>,
    ) -> FnDecl<Self::OutputS, Self::OutputMetadata> {
        let name = self.transform_ident(&input.name);
        let args = input
            .args
            .iter()
            .map(|arg| self.transform_arg_decl(arg))
            .collect_vec();
        let return_ty = input
            .return_ty
            .as_ref()
            .map(|spec| self.transform_ty_spec(spec));
        let scope = self.transform_scope(&input.scope);
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
    fn transform_mod_decl(
        &mut self,
        input: &ModDecl<Self::InputS, Self::InputMetadata>,
    ) -> ModDecl<Self::OutputS, Self::OutputMetadata> {
        let ident = self.transform_ident(&input.ident);
        ModDecl {
            ident,
            span: input.span,
        }
    }
    fn transform_constant_decl(
        &mut self,
        input: &ConstantDecl<Self::InputS, Self::InputMetadata>,
    ) -> ConstantDecl<Self::OutputS, Self::OutputMetadata> {
        let name = self.transform_ident(&input.name);
        let ty = self.transform_ident(&input.ty);
        let value = self.transform_expr(&input.value);
        let metadata = self.dispatch_constant_decl(input, &name, &ty, &value);
        ConstantDecl {
            name,
            ty,
            value,
            metadata,
        }
    }
    fn transform_statement(
        &mut self,
        input: &Statement<Self::InputS, Self::InputMetadata>,
    ) -> Statement<Self::OutputS, Self::OutputMetadata> {
        match input {
            Statement::Expr { value, semicolon } => Statement::Expr {
                value: self.transform_expr(value),
                semicolon: *semicolon,
            },
            Statement::LetBinding(l) => Statement::LetBinding(self.transform_let_binding(l)),
        }
    }
    fn transform_let_binding(
        &mut self,
        input: &LetBinding<Self::InputS, Self::InputMetadata>,
    ) -> LetBinding<Self::OutputS, Self::OutputMetadata> {
        let name = self.transform_ident(&input.name);
        let value = self.transform_expr(&input.value);
        let metadata = self.dispatch_let_binding(input, &name, &value);
        LetBinding {
            name,
            value,
            metadata,
            span: input.span,
        }
    }
    fn transform_if_expr(
        &mut self,
        input: &IfExpr<Self::InputS, Self::InputMetadata>,
    ) -> IfExpr<Self::OutputS, Self::OutputMetadata> {
        let scope_annotation = input
            .scope_annotation
            .as_ref()
            .map(|ident| self.transform_ident(ident));
        let cond = self.transform_expr(&input.cond);
        let then = self.transform_scope(&input.then);
        let else_ = self.transform_scope(&input.else_);
        let metadata = self.dispatch_if_expr(input, &cond, &then, &else_);
        IfExpr {
            scope_annotation,
            span: input.span,
            metadata,
            cond,
            then,
            else_,
        }
    }
    fn transform_match_expr(
        &mut self,
        input: &MatchExpr<Self::InputS, Self::InputMetadata>,
    ) -> MatchExpr<Self::OutputS, Self::OutputMetadata> {
        let scrutinee = self.transform_expr(&input.scrutinee);
        let arms = input
            .arms
            .iter()
            .map(|arm| MatchArm {
                pattern: self.transform_ident_path(&arm.pattern),
                expr: self.transform_expr(&arm.expr),
                span: arm.span,
            })
            .collect::<Vec<_>>();
        let metadata = self.dispatch_match_expr(input, &scrutinee, &arms);
        MatchExpr {
            scrutinee,
            arms,
            span: input.span,
            metadata,
        }
    }
    fn transform_bin_op_expr(
        &mut self,
        input: &BinOpExpr<Self::InputS, Self::InputMetadata>,
    ) -> BinOpExpr<Self::OutputS, Self::OutputMetadata> {
        let left = self.transform_expr(&input.left);
        let right = self.transform_expr(&input.right);
        let metadata = self.dispatch_bin_op_expr(input, &left, &right);
        BinOpExpr {
            op: input.op,
            span: input.span,
            metadata,
            left,
            right,
        }
    }
    fn transform_unary_op_expr(
        &mut self,
        input: &UnaryOpExpr<Self::InputS, Self::InputMetadata>,
    ) -> UnaryOpExpr<Self::OutputS, Self::OutputMetadata> {
        let operand = self.transform_expr(&input.operand);
        let metadata = self.dispatch_unary_op_expr(input, &operand);
        UnaryOpExpr {
            op: input.op,
            span: input.span,
            metadata,
            operand,
        }
    }
    fn transform_comparison_expr(
        &mut self,
        input: &ComparisonExpr<Self::InputS, Self::InputMetadata>,
    ) -> ComparisonExpr<Self::OutputS, Self::OutputMetadata> {
        let left = self.transform_expr(&input.left);
        let right = self.transform_expr(&input.right);
        let metadata = self.dispatch_comparison_expr(input, &left, &right);
        ComparisonExpr {
            op: input.op,
            span: input.span,
            metadata,
            left,
            right,
        }
    }
    fn transform_field_access_expr(
        &mut self,
        input: &FieldAccessExpr<Self::InputS, Self::InputMetadata>,
    ) -> FieldAccessExpr<Self::OutputS, Self::OutputMetadata> {
        let base = self.transform_expr(&input.base);
        let field = self.transform_ident(&input.field);
        let metadata = self.dispatch_field_access_expr(input, &base, &field);
        FieldAccessExpr {
            base,
            field,
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
            .map(|ident| self.transform_ident(ident));
        let func = self.transform_ident_path(&input.func);
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

    fn transform_emit_expr(
        &mut self,
        input: &EmitExpr<Self::InputS, Self::InputMetadata>,
    ) -> EmitExpr<Self::OutputS, Self::OutputMetadata> {
        let value = self.transform_expr(&input.value);
        let metadata = self.dispatch_emit_expr(input, &value);
        EmitExpr {
            value,
            span: input.span,
            metadata,
        }
    }
    fn transform_args(
        &mut self,
        input: &Args<Self::InputS, Self::InputMetadata>,
    ) -> Args<Self::OutputS, Self::OutputMetadata> {
        let posargs = input
            .posargs
            .iter()
            .map(|arg| self.transform_expr(arg))
            .collect_vec();
        let kwargs = input
            .kwargs
            .iter()
            .map(|arg| self.transform_kw_arg_value(arg))
            .collect_vec();
        let metadata = self.dispatch_args(input, &posargs, &kwargs);
        Args {
            posargs,
            kwargs,
            metadata,
            span: input.span,
        }
    }
    fn transform_kw_arg_value(
        &mut self,
        input: &KwArgValue<Self::InputS, Self::InputMetadata>,
    ) -> KwArgValue<Self::OutputS, Self::OutputMetadata> {
        let name = self.transform_ident(&input.name);
        let value = self.transform_expr(&input.value);
        let metadata = self.dispatch_kw_arg_value(input, &name, &value);
        KwArgValue {
            name,
            value,
            span: input.span,
            metadata,
        }
    }

    fn transform_arg_decl(
        &mut self,
        input: &ArgDecl<Self::InputS, Self::InputMetadata>,
    ) -> ArgDecl<Self::OutputS, Self::OutputMetadata> {
        let name = self.transform_ident(&input.name);
        let ty = self.transform_ty_spec(&input.ty);
        let metadata = self.dispatch_arg_decl(input, &name, &ty);
        ArgDecl { name, ty, metadata }
    }

    fn transform_scope(
        &mut self,
        input: &Scope<Self::InputS, Self::InputMetadata>,
    ) -> Scope<Self::OutputS, Self::OutputMetadata> {
        self.enter_scope(input);
        let scope_annotation = input
            .scope_annotation
            .as_ref()
            .map(|ident| self.transform_ident(ident));
        let stmts = input
            .stmts
            .iter()
            .map(|stmt| self.transform_statement(stmt))
            .collect_vec();
        let tail = input.tail.as_ref().map(|stmt| self.transform_expr(stmt));
        let metadata = self.dispatch_scope(input, &stmts, &tail);
        let output = Scope {
            scope_annotation,
            span: input.span,
            stmts,
            tail,
            metadata,
        };
        self.exit_scope(input, &output);
        output
    }

    fn transform_cast(
        &mut self,
        input: &CastExpr<Self::InputS, Self::InputMetadata>,
    ) -> CastExpr<Self::OutputS, Self::OutputMetadata> {
        let value = self.transform_expr(&input.value);
        let ty = self.transform_ty_spec(&input.ty);
        let metadata = self.dispatch_cast(input, &value, &ty);
        CastExpr {
            span: input.span,
            value,
            ty,
            metadata,
        }
    }

    fn transform_string_literal(
        &mut self,
        input: &StringLiteral<Self::InputS>,
    ) -> StringLiteral<Self::OutputS> {
        let value = self.transform_s(&input.value);
        StringLiteral {
            span: input.span,
            value,
        }
    }

    fn transform_expr(
        &mut self,
        input: &Expr<Self::InputS, Self::InputMetadata>,
    ) -> Expr<Self::OutputS, Self::OutputMetadata> {
        match input {
            Expr::If(if_expr) => Expr::If(Box::new(self.transform_if_expr(if_expr))),
            Expr::Match(match_expr) => Expr::Match(Box::new(self.transform_match_expr(match_expr))),
            Expr::BinOp(bin_op_expr) => {
                Expr::BinOp(Box::new(self.transform_bin_op_expr(bin_op_expr)))
            }
            Expr::UnaryOp(unary_op_expr) => {
                Expr::UnaryOp(Box::new(self.transform_unary_op_expr(unary_op_expr)))
            }
            Expr::Comparison(comparison_expr) => {
                Expr::Comparison(Box::new(self.transform_comparison_expr(comparison_expr)))
            }
            Expr::Call(call_expr) => Expr::Call(self.transform_call_expr(call_expr)),
            Expr::Emit(emit_expr) => Expr::Emit(Box::new(self.transform_emit_expr(emit_expr))),
            Expr::FieldAccess(field_access_expr) => Expr::FieldAccess(Box::new(
                self.transform_field_access_expr(field_access_expr),
            )),
            Expr::IdentPath(ident_path) => Expr::IdentPath(self.transform_ident_path(ident_path)),
            Expr::Nil(nil) => Expr::Nil(*nil),
            Expr::SeqNil(nil) => Expr::SeqNil(*nil),
            Expr::FloatLiteral(float_literal) => Expr::FloatLiteral(*float_literal),
            Expr::IntLiteral(int_literal) => Expr::IntLiteral(*int_literal),
            Expr::BoolLiteral(bool_literal) => Expr::BoolLiteral(*bool_literal),
            Expr::StringLiteral(string_literal) => {
                Expr::StringLiteral(self.transform_string_literal(string_literal))
            }
            Expr::Scope(scope) => Expr::Scope(Box::new(self.transform_scope(scope))),
            Expr::Cast(cast) => Expr::Cast(Box::new(self.transform_cast(cast))),
        }
    }
}
