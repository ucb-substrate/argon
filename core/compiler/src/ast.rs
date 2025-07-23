use std::fmt::Debug;

use anyhow::Context;
use cfgrammar::Span;
use derive_where::derive_where;

#[derive_where(Default, Debug, Clone)]
pub struct Ast<'a, T: AstMetadata> {
    pub decls: Vec<Decl<'a, T>>,
}

#[derive_where(Debug, Clone)]
pub enum Decl<'a, T: AstMetadata> {
    Enum(EnumDecl<'a, T>),
    Constant(ConstantDecl<'a, T>),
    Cell(CellDecl<'a, T>),
    Fn(FnDecl<'a, T>),
}

#[derive_where(Debug, Clone)]
pub struct Ident<'a, T: AstMetadata> {
    pub span: Span,
    pub name: &'a str,
    pub metadata: T::Ident,
}

#[derive(Debug, Clone, Copy)]
pub struct FloatLiteral {
    pub span: Span,
    pub value: f64,
}

#[derive(Debug, Clone, Copy)]
pub struct IntLiteral {
    pub span: Span,
    pub value: i64,
}

#[derive_where(Debug, Clone)]
pub struct EnumDecl<'a, T: AstMetadata> {
    pub name: Ident<'a, T>,
    pub variants: Vec<Ident<'a, T>>,
    pub metadata: T::EnumDecl,
}

#[derive_where(Debug, Clone)]
pub struct CellDecl<'a, T: AstMetadata> {
    pub name: Ident<'a, T>,
    pub args: Vec<ArgDecl<'a, T>>,
    pub stmts: Vec<Statement<'a, T>>,
    pub metadata: T::CellDecl,
}

#[derive_where(Debug, Clone)]
pub struct FnDecl<'a, T: AstMetadata> {
    pub name: Ident<'a, T>,
    pub args: Vec<ArgDecl<'a, T>>,
    pub return_ty: Ident<'a, T>,
    pub scope: Scope<'a, T>,
    pub metadata: T::FnDecl,
}

#[derive_where(Debug, Clone)]
pub struct ConstantDecl<'a, T: AstMetadata> {
    pub name: Ident<'a, T>,
    pub ty: Ident<'a, T>,
    pub value: Expr<'a, T>,
    pub metadata: T::ConstantDecl,
}

#[derive_where(Debug, Clone)]
pub struct Scope<'a, T: AstMetadata> {
    pub span: Span,
    pub stmts: Vec<Statement<'a, T>>,
    pub tail: Option<Expr<'a, T>>,
    pub metadata: T::Scope,
}

#[derive_where(Debug, Clone)]
pub enum Statement<'a, T: AstMetadata> {
    Expr { value: Expr<'a, T>, semicolon: bool },
    LetBinding(LetBinding<'a, T>),
}

#[derive_where(Debug, Clone)]
pub struct LetBinding<'a, T: AstMetadata> {
    pub name: Ident<'a, T>,
    pub value: Expr<'a, T>,
    pub metadata: T::LetBinding,
    pub span: Span,
}

#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub enum BinOp {
    Add,
    Sub,
    Mul,
    Div,
}

#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub enum UnaryOp {
    Not,
    Neg,
}

#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub enum ComparisonOp {
    Eq,
    Ne,
    Geq,
    Gt,
    Leq,
    Lt,
}

#[derive_where(Debug, Clone)]
pub enum Expr<'a, T: AstMetadata> {
    If(Box<IfExpr<'a, T>>),
    Comparison(Box<ComparisonExpr<'a, T>>),
    BinOp(Box<BinOpExpr<'a, T>>),
    UnaryOp(Box<UnaryOpExpr<'a, T>>),
    Call(CallExpr<'a, T>),
    Emit(Box<EmitExpr<'a, T>>),
    EnumValue(EnumValue<'a, T>),
    FieldAccess(Box<FieldAccessExpr<'a, T>>),
    Var(VarExpr<'a, T>),
    FloatLiteral(FloatLiteral),
    IntLiteral(IntLiteral),
    Scope(Box<Scope<'a, T>>),
    Cast(Box<CastExpr<'a, T>>),
}

#[derive_where(Debug, Clone)]
pub struct VarExpr<'a, T: AstMetadata> {
    pub name: Ident<'a, T>,
    pub metadata: T::VarExpr,
}

#[derive_where(Debug, Clone)]
pub struct IfExpr<'a, T: AstMetadata> {
    pub cond: Expr<'a, T>,
    pub then: Expr<'a, T>,
    pub else_: Expr<'a, T>,
    pub span: Span,
    pub metadata: T::IfExpr,
}

#[derive_where(Debug, Clone)]
pub struct BinOpExpr<'a, T: AstMetadata> {
    pub op: BinOp,
    pub left: Expr<'a, T>,
    pub right: Expr<'a, T>,
    pub span: Span,
    pub metadata: T::BinOpExpr,
}

#[derive_where(Debug, Clone)]
pub struct UnaryOpExpr<'a, T: AstMetadata> {
    pub op: UnaryOp,
    pub operand: Expr<'a, T>,
    pub span: Span,
    pub metadata: T::UnaryOpExpr,
}

#[derive_where(Debug, Clone)]
pub struct ComparisonExpr<'a, T: AstMetadata> {
    pub op: ComparisonOp,
    pub left: Expr<'a, T>,
    pub right: Expr<'a, T>,
    pub span: Span,
    pub metadata: T::ComparisonExpr,
}

#[derive_where(Debug, Clone)]
pub struct FieldAccessExpr<'a, T: AstMetadata> {
    pub base: Expr<'a, T>,
    pub field: Ident<'a, T>,
    pub span: Span,
    pub metadata: T::FieldAccessExpr,
}

#[derive_where(Debug, Clone)]
pub struct EnumValue<'a, T: AstMetadata> {
    pub name: Ident<'a, T>,
    pub variant: Ident<'a, T>,
    pub span: Span,
    pub metadata: T::EnumValue,
}

#[derive_where(Debug, Clone)]
pub struct CallExpr<'a, T: AstMetadata> {
    pub func: Ident<'a, T>,
    pub args: Args<'a, T>,
    pub span: Span,
    pub metadata: T::CallExpr,
}

#[derive_where(Debug, Clone)]
pub struct EmitExpr<'a, T: AstMetadata> {
    pub value: Expr<'a, T>,
    pub span: Span,
    pub metadata: T::EmitExpr,
}

#[derive_where(Debug, Clone)]
pub struct Args<'a, T: AstMetadata> {
    pub posargs: Vec<Expr<'a, T>>,
    pub kwargs: Vec<KwArgValue<'a, T>>,
    pub metadata: T::Args,
}

#[derive_where(Debug, Clone)]
pub struct KwArgValue<'a, T: AstMetadata> {
    pub name: Ident<'a, T>,
    pub value: Expr<'a, T>,
    pub span: Span,
    pub metadata: T::KwArgValue,
}

#[derive_where(Debug, Clone)]
pub struct ArgDecl<'a, T: AstMetadata> {
    pub name: Ident<'a, T>,
    pub ty: Ident<'a, T>,
    pub metadata: T::ArgDecl,
}

#[derive_where(Debug, Clone)]
pub struct CastExpr<'a, T: AstMetadata> {
    pub value: Expr<'a, T>,
    pub ty: Ident<'a, T>,
    pub span: Span,
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

impl<'a, T: AstMetadata> Expr<'a, T> {
    pub fn span(&self) -> Span {
        match self {
            Self::If(x) => x.span,
            Self::Comparison(x) => x.span,
            Self::BinOp(x) => x.span,
            Self::UnaryOp(x) => x.span,
            Self::Call(x) => x.span,
            Self::Emit(x) => x.span,
            Self::EnumValue(x) => x.span,
            Self::FieldAccess(x) => x.span,
            Self::Var(x) => x.name.span,
            Self::FloatLiteral(x) => x.span,
            Self::IntLiteral(x) => x.span,
            Self::Scope(x) => x.span,
            Self::Cast(x) => x.span,
        }
    }
}

pub trait AstMetadata {
    type Ident: Debug + Clone;
    type VarExpr: Debug + Clone;
    type EnumDecl: Debug + Clone;
    type CellDecl: Debug + Clone;
    type FnDecl: Debug + Clone;
    type ConstantDecl: Debug + Clone;
    type LetBinding: Debug + Clone;
    type IfExpr: Debug + Clone;
    type BinOpExpr: Debug + Clone;
    type UnaryOpExpr: Debug + Clone;
    type ComparisonExpr: Debug + Clone;
    type FieldAccessExpr: Debug + Clone;
    type EnumValue: Debug + Clone;
    type CallExpr: Debug + Clone;
    type EmitExpr: Debug + Clone;
    type Args: Debug + Clone;
    type KwArgValue: Debug + Clone;
    type ArgDecl: Debug + Clone;
    type Scope: Debug + Clone;
    type Typ: Debug + Clone;
    type CastExpr: Debug + Clone;
}

pub trait AstTransformer<'a> {
    type Input: AstMetadata;
    type Output: AstMetadata;

    fn dispatch_ident(
        &mut self,
        input: &Ident<'a, Self::Input>,
    ) -> <Self::Output as AstMetadata>::Ident;
    fn dispatch_var_expr(
        &mut self,
        input: &VarExpr<'a, Self::Input>,
        name: &Ident<'a, Self::Output>,
    ) -> <Self::Output as AstMetadata>::VarExpr;
    fn dispatch_enum_decl(
        &mut self,
        input: &EnumDecl<'a, Self::Input>,
        name: &Ident<'a, Self::Output>,
        variants: &Vec<Ident<'a, Self::Output>>,
    ) -> <Self::Output as AstMetadata>::EnumDecl;
    fn dispatch_cell_decl(
        &mut self,
        input: &CellDecl<'a, Self::Input>,
        name: &Ident<'a, Self::Output>,
        args: &Vec<ArgDecl<'a, Self::Output>>,
        stmts: &Vec<Statement<'a, Self::Output>>,
    ) -> <Self::Output as AstMetadata>::CellDecl;
    fn dispatch_fn_decl(
        &mut self,
        input: &FnDecl<'a, Self::Input>,
        name: &Ident<'a, Self::Output>,
        args: &Vec<ArgDecl<'a, Self::Output>>,
        return_ty: &Ident<'a, Self::Output>,
        scope: &Scope<'a, Self::Output>,
    ) -> <Self::Output as AstMetadata>::FnDecl;
    fn dispatch_constant_decl(
        &mut self,
        input: &ConstantDecl<'a, Self::Input>,
        name: &Ident<'a, Self::Output>,
        ty: &Ident<'a, Self::Output>,
        value: &Expr<'a, Self::Output>,
    ) -> <Self::Output as AstMetadata>::ConstantDecl;
    fn dispatch_let_binding(
        &mut self,
        input: &LetBinding<'a, Self::Input>,
        name: &Ident<'a, Self::Output>,
        value: &Expr<'a, Self::Output>,
    ) -> <Self::Output as AstMetadata>::LetBinding;
    fn dispatch_if_expr(
        &mut self,
        input: &IfExpr<'a, Self::Input>,
        cond: &Expr<'a, Self::Output>,
        then: &Expr<'a, Self::Output>,
        else_: &Expr<'a, Self::Output>,
    ) -> <Self::Output as AstMetadata>::IfExpr;
    fn dispatch_bin_op_expr(
        &mut self,
        input: &BinOpExpr<'a, Self::Input>,
        left: &Expr<'a, Self::Output>,
        right: &Expr<'a, Self::Output>,
    ) -> <Self::Output as AstMetadata>::BinOpExpr;
    fn dispatch_unary_op_expr(
        &mut self,
        input: &UnaryOpExpr<'a, Self::Input>,
        operand: &Expr<'a, Self::Output>,
    ) -> <Self::Output as AstMetadata>::UnaryOpExpr;
    fn dispatch_comparison_expr(
        &mut self,
        input: &ComparisonExpr<'a, Self::Input>,
        left: &Expr<'a, Self::Output>,
        right: &Expr<'a, Self::Output>,
    ) -> <Self::Output as AstMetadata>::ComparisonExpr;
    fn dispatch_cast(
        &mut self,
        input: &CastExpr<'a, Self::Input>,
        value: &Expr<'a, Self::Output>,
        ty: &Ident<'a, Self::Output>,
    ) -> <Self::Output as AstMetadata>::CastExpr;
    fn dispatch_field_access_expr(
        &mut self,
        input: &FieldAccessExpr<'a, Self::Input>,
        base: &Expr<'a, Self::Output>,
        field: &Ident<'a, Self::Output>,
    ) -> <Self::Output as AstMetadata>::FieldAccessExpr;
    fn dispatch_enum_value(
        &mut self,
        input: &EnumValue<'a, Self::Input>,
        name: &Ident<'a, Self::Output>,
        variant: &Ident<'a, Self::Output>,
    ) -> <Self::Output as AstMetadata>::EnumValue;
    fn dispatch_call_expr(
        &mut self,
        input: &CallExpr<'a, Self::Input>,
        func: &Ident<'a, Self::Output>,
        args: &Args<'a, Self::Output>,
    ) -> <Self::Output as AstMetadata>::CallExpr;
    fn dispatch_emit_expr(
        &mut self,
        input: &EmitExpr<'a, Self::Input>,
        value: &Expr<'a, Self::Output>,
    ) -> <Self::Output as AstMetadata>::EmitExpr;
    fn dispatch_args(
        &mut self,
        input: &Args<'a, Self::Input>,
        posargs: &Vec<Expr<'a, Self::Output>>,
        kwargs: &Vec<KwArgValue<'a, Self::Output>>,
    ) -> <Self::Output as AstMetadata>::Args;
    fn dispatch_kw_arg_value(
        &mut self,
        input: &KwArgValue<'a, Self::Input>,
        name: &Ident<'a, Self::Output>,
        value: &Expr<'a, Self::Output>,
    ) -> <Self::Output as AstMetadata>::KwArgValue;
    fn dispatch_arg_decl(
        &mut self,
        input: &ArgDecl<'a, Self::Input>,
        name: &Ident<'a, Self::Output>,
        ty: &Ident<'a, Self::Output>,
    ) -> <Self::Output as AstMetadata>::ArgDecl;
    fn dispatch_scope(
        &mut self,
        input: &Scope<'a, Self::Input>,
        stmts: &Vec<Statement<'a, Self::Output>>,
        tail: &Option<Expr<'a, Self::Output>>,
    ) -> <Self::Output as AstMetadata>::Scope;
    fn enter_scope(&mut self, input: &Scope<'a, Self::Input>);
    fn exit_scope(&mut self, input: &Scope<'a, Self::Input>, output: &Scope<'a, Self::Output>);

    fn transform_ident(&mut self, input: &Ident<'a, Self::Input>) -> Ident<'a, Self::Output> {
        let metadata = self.dispatch_ident(input);
        Ident {
            span: input.span,
            name: input.name,
            metadata,
        }
    }
    fn transform_var_expr(
        &mut self,
        input: &VarExpr<'a, Self::Input>,
    ) -> VarExpr<'a, Self::Output> {
        let name = self.transform_ident(&input.name);
        let metadata = self.dispatch_var_expr(input, &name);
        VarExpr { name, metadata }
    }
    fn transform_enum_decl(
        &mut self,
        input: &EnumDecl<'a, Self::Input>,
    ) -> EnumDecl<'a, Self::Output> {
        let name = self.transform_ident(&input.name);
        let variants = input
            .variants
            .iter()
            .map(|variant| self.transform_ident(variant))
            .collect();
        let metadata = self.dispatch_enum_decl(input, &name, &variants);
        EnumDecl {
            name,
            variants,
            metadata,
        }
    }
    fn transform_cell_decl(
        &mut self,
        input: &CellDecl<'a, Self::Input>,
    ) -> CellDecl<'a, Self::Output> {
        let name = self.transform_ident(&input.name);
        let args = input
            .args
            .iter()
            .map(|arg| self.transform_arg_decl(arg))
            .collect();
        let stmts = input
            .stmts
            .iter()
            .map(|stmt| self.transform_statement(stmt))
            .collect();
        let metadata = self.dispatch_cell_decl(input, &name, &args, &stmts);
        CellDecl {
            name,
            args,
            stmts,
            metadata,
        }
    }
    fn transform_fn_decl(&mut self, input: &FnDecl<'a, Self::Input>) -> FnDecl<'a, Self::Output> {
        let name = self.transform_ident(&input.name);
        let args = input
            .args
            .iter()
            .map(|arg| self.transform_arg_decl(arg))
            .collect();
        let return_ty = self.transform_ident(&input.return_ty);
        let scope = self.transform_scope(&input.scope);
        let metadata = self.dispatch_fn_decl(input, &name, &args, &return_ty, &scope);
        FnDecl {
            name,
            args,
            return_ty,
            scope,
            metadata,
        }
    }
    fn transform_constant_decl(
        &mut self,
        input: &ConstantDecl<'a, Self::Input>,
    ) -> ConstantDecl<'a, Self::Output> {
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
        input: &Statement<'a, Self::Input>,
    ) -> Statement<'a, Self::Output> {
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
        input: &LetBinding<'a, Self::Input>,
    ) -> LetBinding<'a, Self::Output> {
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
    fn transform_if_expr(&mut self, input: &IfExpr<'a, Self::Input>) -> IfExpr<'a, Self::Output> {
        let cond = self.transform_expr(&input.cond);
        let then = self.transform_expr(&input.then);
        let else_ = self.transform_expr(&input.else_);
        let metadata = self.dispatch_if_expr(&input, &cond, &then, &else_);
        IfExpr {
            span: input.span,
            metadata,
            cond,
            then,
            else_,
        }
    }
    fn transform_bin_op_expr(
        &mut self,
        input: &BinOpExpr<'a, Self::Input>,
    ) -> BinOpExpr<'a, Self::Output> {
        let left = self.transform_expr(&input.left);
        let right = self.transform_expr(&input.right);
        let metadata = self.dispatch_bin_op_expr(&input, &left, &right);
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
        input: &UnaryOpExpr<'a, Self::Input>,
    ) -> UnaryOpExpr<'a, Self::Output> {
        let operand = self.transform_expr(&input.operand);
        let metadata = self.dispatch_unary_op_expr(&input, &operand);
        UnaryOpExpr {
            op: input.op,
            span: input.span,
            metadata,
            operand,
        }
    }
    fn transform_comparison_expr(
        &mut self,
        input: &ComparisonExpr<'a, Self::Input>,
    ) -> ComparisonExpr<'a, Self::Output> {
        let left = self.transform_expr(&input.left);
        let right = self.transform_expr(&input.right);
        let metadata = self.dispatch_comparison_expr(&input, &left, &right);
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
        input: &FieldAccessExpr<'a, Self::Input>,
    ) -> FieldAccessExpr<'a, Self::Output> {
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

    fn transform_enum_value(
        &mut self,
        input: &EnumValue<'a, Self::Input>,
    ) -> EnumValue<'a, Self::Output> {
        let name = self.transform_ident(&input.name);
        let variant = self.transform_ident(&input.variant);
        let metadata = self.dispatch_enum_value(input, &name, &variant);
        EnumValue {
            name,
            variant,
            span: input.span,
            metadata,
        }
    }

    fn transform_call_expr(
        &mut self,
        input: &CallExpr<'a, Self::Input>,
    ) -> CallExpr<'a, Self::Output> {
        let func = self.transform_ident(&input.func);
        let args = self.transform_args(&input.args);
        let metadata = self.dispatch_call_expr(input, &func, &args);
        CallExpr {
            func,
            args,
            span: input.span,
            metadata,
        }
    }
    fn transform_emit_expr(
        &mut self,
        input: &EmitExpr<'a, Self::Input>,
    ) -> EmitExpr<'a, Self::Output> {
        let value = self.transform_expr(&input.value);
        let metadata = self.dispatch_emit_expr(input, &value);
        EmitExpr {
            value,
            span: input.span,
            metadata,
        }
    }
    fn transform_args(&mut self, input: &Args<'a, Self::Input>) -> Args<'a, Self::Output> {
        let posargs = input
            .posargs
            .iter()
            .map(|arg| self.transform_expr(arg))
            .collect();
        let kwargs = input
            .kwargs
            .iter()
            .map(|arg| self.transform_kw_arg_value(arg))
            .collect();
        let metadata = self.dispatch_args(input, &posargs, &kwargs);
        Args {
            posargs,
            kwargs,
            metadata,
        }
    }
    fn transform_kw_arg_value(
        &mut self,
        input: &KwArgValue<'a, Self::Input>,
    ) -> KwArgValue<'a, Self::Output> {
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
        input: &ArgDecl<'a, Self::Input>,
    ) -> ArgDecl<'a, Self::Output> {
        let name = self.transform_ident(&input.name);
        let ty = self.transform_ident(&input.ty);
        let metadata = self.dispatch_arg_decl(input, &name, &ty);
        ArgDecl { name, ty, metadata }
    }

    fn transform_scope(&mut self, input: &Scope<'a, Self::Input>) -> Scope<'a, Self::Output> {
        self.enter_scope(input);
        let stmts = input
            .stmts
            .iter()
            .map(|stmt| self.transform_statement(stmt))
            .collect();
        let tail = input.tail.as_ref().map(|stmt| self.transform_expr(stmt));
        let metadata = self.dispatch_scope(input, &stmts, &tail);
        let output = Scope {
            span: input.span,
            stmts,
            tail,
            metadata,
        };
        self.exit_scope(input, &output);
        output
    }

    fn transform_cast(&mut self, input: &CastExpr<'a, Self::Input>) -> CastExpr<'a, Self::Output> {
        let value = self.transform_expr(&input.value);
        let ty = self.transform_ident(&input.ty);
        let metadata = self.dispatch_cast(&input, &value, &ty);
        CastExpr {
            span: input.span,
            value,
            ty,
            metadata,
        }
    }

    fn transform_expr(&mut self, input: &Expr<'a, Self::Input>) -> Expr<'a, Self::Output> {
        match input {
            Expr::If(if_expr) => Expr::If(Box::new(self.transform_if_expr(if_expr))),
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
            Expr::EnumValue(enum_value) => Expr::EnumValue(self.transform_enum_value(enum_value)),
            Expr::FieldAccess(field_access_expr) => Expr::FieldAccess(Box::new(
                self.transform_field_access_expr(field_access_expr),
            )),
            Expr::Var(var_expr) => Expr::Var(self.transform_var_expr(var_expr)),
            Expr::FloatLiteral(float_literal) => Expr::FloatLiteral(*float_literal),
            Expr::IntLiteral(int_literal) => Expr::IntLiteral(*int_literal),
            Expr::Scope(scope) => Expr::Scope(Box::new(self.transform_scope(scope))),
            Expr::Cast(cast) => Expr::Cast(Box::new(self.transform_cast(cast))),
        }
    }
}
