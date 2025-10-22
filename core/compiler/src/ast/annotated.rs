use std::{marker::PhantomData, path::PathBuf};

use arcstr::{ArcStr, Substr};
use derive_where::derive_where;
use indexmap::IndexMap;

use crate::ast::{
    Ast, AstMetadata, AstTransformer, CallExpr, Decl, Ident, Scope, Span, StringLiteral,
};

#[derive_where(Debug, Clone)]
pub struct AnnotatedAst<T: AstMetadata> {
    pub text: ArcStr,
    pub ast: Ast<Substr, T>,
    pub path: PathBuf,
    pub span2scope: IndexMap<Span, Scope<Substr, T>>,
    // TODO: merge span2 IndexMaps
    pub span2call: IndexMap<Span, CallExpr<Substr, T>>,
}

impl<T: AstMetadata> AnnotatedAst<T> {
    pub fn new<S>(text: ArcStr, ast: &Ast<S, T>, path: PathBuf) -> Self {
        let mut pass = AstAnnotationPass {
            text,
            path: path.clone(),
            span2scope: Default::default(),
            span2call: Default::default(),
            phantom: Default::default(),
        };

        let mut decls = Vec::new();
        for decl in &ast.decls {
            match decl {
                Decl::Fn(f) => {
                    decls.push(Decl::Fn(pass.transform_fn_decl(f)));
                }
                Decl::Cell(c) => {
                    decls.push(Decl::Cell(pass.transform_cell_decl(c)));
                }
                Decl::Mod(m) => {
                    decls.push(Decl::Mod(pass.transform_mod_decl(m)));
                }
                Decl::Enum(e) => {
                    decls.push(Decl::Enum(pass.transform_enum_decl(e)));
                }
                _ => todo!(),
            }
        }

        Self {
            text: pass.text,
            ast: Ast {
                decls,
                span: ast.span,
            },
            path,
            span2scope: pass.span2scope,
            span2call: pass.span2call,
        }
    }
}

struct AstAnnotationPass<S, T: AstMetadata> {
    text: ArcStr,
    path: PathBuf,
    span2scope: IndexMap<Span, Scope<Substr, T>>,
    span2call: IndexMap<Span, CallExpr<Substr, T>>,
    phantom: PhantomData<S>,
}

impl<S, T: AstMetadata> AstTransformer for AstAnnotationPass<S, T> {
    type InputMetadata = T;
    type OutputMetadata = T;
    type InputS = S;
    type OutputS = Substr;

    fn dispatch_ident(
        &mut self,
        input: &super::Ident<Self::InputS, Self::InputMetadata>,
    ) -> <Self::OutputMetadata as AstMetadata>::Ident {
        input.metadata.clone()
    }

    fn dispatch_ident_path(
        &mut self,
        input: &super::IdentPath<Self::InputS, Self::InputMetadata>,
    ) -> <Self::OutputMetadata as AstMetadata>::IdentPath {
        input.metadata.clone()
    }

    fn dispatch_enum_decl(
        &mut self,
        input: &super::EnumDecl<Self::InputS, Self::InputMetadata>,
        _name: &super::Ident<Self::OutputS, Self::OutputMetadata>,
        _variants: &[super::Ident<Self::OutputS, Self::OutputMetadata>],
    ) -> <Self::OutputMetadata as AstMetadata>::EnumDecl {
        input.metadata.clone()
    }

    fn dispatch_cell_decl(
        &mut self,
        input: &super::CellDecl<Self::InputS, Self::InputMetadata>,
        _name: &super::Ident<Self::OutputS, Self::OutputMetadata>,
        _args: &[super::ArgDecl<Self::OutputS, Self::OutputMetadata>],
        _scope: &Scope<Self::OutputS, Self::OutputMetadata>,
    ) -> <Self::OutputMetadata as AstMetadata>::CellDecl {
        input.metadata.clone()
    }

    fn dispatch_fn_decl(
        &mut self,
        input: &super::FnDecl<Self::InputS, Self::InputMetadata>,
        _name: &super::Ident<Self::OutputS, Self::OutputMetadata>,
        _args: &[super::ArgDecl<Self::OutputS, Self::OutputMetadata>],
        _return_ty: &Option<super::TySpec<Self::OutputS, Self::OutputMetadata>>,
        _scope: &Scope<Self::OutputS, Self::OutputMetadata>,
    ) -> <Self::OutputMetadata as AstMetadata>::FnDecl {
        input.metadata.clone()
    }

    fn dispatch_constant_decl(
        &mut self,
        input: &super::ConstantDecl<Self::InputS, Self::InputMetadata>,
        _name: &super::Ident<Self::OutputS, Self::OutputMetadata>,
        _ty: &super::Ident<Self::OutputS, Self::OutputMetadata>,
        _value: &super::Expr<Self::OutputS, Self::OutputMetadata>,
    ) -> <Self::OutputMetadata as AstMetadata>::ConstantDecl {
        input.metadata.clone()
    }

    fn dispatch_let_binding(
        &mut self,
        input: &super::LetBinding<Self::InputS, Self::InputMetadata>,
        _name: &super::Ident<Self::OutputS, Self::OutputMetadata>,
        _value: &super::Expr<Self::OutputS, Self::OutputMetadata>,
    ) -> <Self::OutputMetadata as AstMetadata>::LetBinding {
        input.metadata.clone()
    }

    fn dispatch_if_expr(
        &mut self,
        input: &super::IfExpr<Self::InputS, Self::InputMetadata>,
        _cond: &super::Expr<Self::OutputS, Self::OutputMetadata>,
        _then: &Scope<Self::OutputS, Self::OutputMetadata>,
        _else_: &Scope<Self::OutputS, Self::OutputMetadata>,
    ) -> <Self::OutputMetadata as AstMetadata>::IfExpr {
        input.metadata.clone()
    }

    fn dispatch_match_expr(
        &mut self,
        input: &super::MatchExpr<Self::InputS, Self::InputMetadata>,
        _scrutinee: &super::Expr<Self::OutputS, Self::OutputMetadata>,
        _arms: &[super::MatchArm<Self::OutputS, Self::OutputMetadata>],
    ) -> <Self::OutputMetadata as AstMetadata>::MatchExpr {
        input.metadata.clone()
    }

    fn dispatch_bin_op_expr(
        &mut self,
        input: &super::BinOpExpr<Self::InputS, Self::InputMetadata>,
        _left: &super::Expr<Self::OutputS, Self::OutputMetadata>,
        _right: &super::Expr<Self::OutputS, Self::OutputMetadata>,
    ) -> <Self::OutputMetadata as AstMetadata>::BinOpExpr {
        input.metadata.clone()
    }

    fn dispatch_unary_op_expr(
        &mut self,
        input: &super::UnaryOpExpr<Self::InputS, Self::InputMetadata>,
        _operand: &super::Expr<Self::OutputS, Self::OutputMetadata>,
    ) -> <Self::OutputMetadata as AstMetadata>::UnaryOpExpr {
        input.metadata.clone()
    }

    fn dispatch_comparison_expr(
        &mut self,
        input: &super::ComparisonExpr<Self::InputS, Self::InputMetadata>,
        _left: &super::Expr<Self::OutputS, Self::OutputMetadata>,
        _right: &super::Expr<Self::OutputS, Self::OutputMetadata>,
    ) -> <Self::OutputMetadata as AstMetadata>::ComparisonExpr {
        input.metadata.clone()
    }

    fn dispatch_cast(
        &mut self,
        input: &super::CastExpr<Self::InputS, Self::InputMetadata>,
        _value: &super::Expr<Self::OutputS, Self::OutputMetadata>,
        _ty: &super::TySpec<Self::OutputS, Self::OutputMetadata>,
    ) -> <Self::OutputMetadata as AstMetadata>::CastExpr {
        input.metadata.clone()
    }

    fn dispatch_field_access_expr(
        &mut self,
        input: &super::FieldAccessExpr<Self::InputS, Self::InputMetadata>,
        _base: &super::Expr<Self::OutputS, Self::OutputMetadata>,
        _field: &super::Ident<Self::OutputS, Self::OutputMetadata>,
    ) -> <Self::OutputMetadata as AstMetadata>::FieldAccessExpr {
        input.metadata.clone()
    }

    fn dispatch_call_expr(
        &mut self,
        input: &super::CallExpr<Self::InputS, Self::InputMetadata>,
        _func: &super::IdentPath<Self::OutputS, Self::OutputMetadata>,
        _args: &super::Args<Self::OutputS, Self::OutputMetadata>,
    ) -> <Self::OutputMetadata as AstMetadata>::CallExpr {
        input.metadata.clone()
    }

    fn dispatch_emit_expr(
        &mut self,
        input: &super::EmitExpr<Self::InputS, Self::InputMetadata>,
        _value: &super::Expr<Self::OutputS, Self::OutputMetadata>,
    ) -> <Self::OutputMetadata as AstMetadata>::EmitExpr {
        input.metadata.clone()
    }

    fn dispatch_args(
        &mut self,
        input: &super::Args<Self::InputS, Self::InputMetadata>,
        _posargs: &[super::Expr<Self::OutputS, Self::OutputMetadata>],
        _kwargs: &[super::KwArgValue<Self::OutputS, Self::OutputMetadata>],
    ) -> <Self::OutputMetadata as AstMetadata>::Args {
        input.metadata.clone()
    }

    fn dispatch_kw_arg_value(
        &mut self,
        input: &super::KwArgValue<Self::InputS, Self::InputMetadata>,
        _name: &super::Ident<Self::OutputS, Self::OutputMetadata>,
        _value: &super::Expr<Self::OutputS, Self::OutputMetadata>,
    ) -> <Self::OutputMetadata as AstMetadata>::KwArgValue {
        input.metadata.clone()
    }

    fn dispatch_arg_decl(
        &mut self,
        input: &super::ArgDecl<Self::InputS, Self::InputMetadata>,
        _name: &super::Ident<Self::OutputS, Self::OutputMetadata>,
        _ty: &super::TySpec<Self::OutputS, Self::OutputMetadata>,
    ) -> <Self::OutputMetadata as AstMetadata>::ArgDecl {
        input.metadata.clone()
    }

    fn dispatch_scope(
        &mut self,
        input: &Scope<Self::InputS, Self::InputMetadata>,
        _stmts: &[super::Statement<Self::OutputS, Self::OutputMetadata>],
        _tail: &Option<super::Expr<Self::OutputS, Self::OutputMetadata>>,
    ) -> <Self::OutputMetadata as AstMetadata>::Scope {
        input.metadata.clone()
    }

    fn exit_scope(
        &mut self,
        _input: &Scope<Self::InputS, Self::InputMetadata>,
        output: &Scope<Self::OutputS, Self::OutputMetadata>,
    ) {
        self.span2scope.insert(
            Span {
                path: self.path.clone(),
                span: output.span,
            },
            output.clone(),
        );
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
        let o = CallExpr {
            scope_annotation,
            func,
            args,
            span: input.span,
            metadata,
        };
        self.span2call.insert(
            Span {
                path: self.path.clone(),
                span: o.span,
            },
            o.clone(),
        );
        o
    }

    fn transform_s(&mut self, _s: &Self::InputS) -> Self::OutputS {
        unreachable!()
    }
    fn transform_ident(
        &mut self,
        input: &Ident<Self::InputS, Self::InputMetadata>,
    ) -> Ident<Self::OutputS, Self::OutputMetadata> {
        let name = self.text.substr(input.span.start()..input.span.end());
        let metadata = self.dispatch_ident(input);
        Ident {
            span: input.span,
            name,
            metadata,
        }
    }
    fn transform_string_literal(
        &mut self,
        input: &StringLiteral<Self::InputS>,
    ) -> StringLiteral<Self::OutputS> {
        // TODO: recover without reimplementing trimming logic?
        let value = self
            .text
            .substr(input.span.start()..input.span.end())
            .substr_using(|s| s.trim_matches('"'));
        StringLiteral {
            span: input.span,
            value,
        }
    }
}
