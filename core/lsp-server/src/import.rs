use std::collections::HashSet;

use compiler::{
    ast::{
        ArgDecl, Args, Ast, AstMetadata, AstTransformer, BinOpExpr, CallExpr, CellDecl,
        ComparisonExpr, ConstantDecl, Decl, EnumDecl, Expr, FieldAccessExpr, FnDecl, Ident, IfExpr,
        Scope, UnaryOpExpr, VarExpr,
    },
    parse::ParseMetadata,
};
use tower_lsp::lsp_types::{Range, TextEdit};

use crate::document::Document;

pub(crate) struct ScopeAnnotationPass<'a> {
    ast: &'a Ast<'a, ParseMetadata>,
    content: &'a Document,
    assigned_names: Vec<HashSet<String>>,
    ids: Vec<usize>,
    edits: Vec<TextEdit>,
}

impl<'a> ScopeAnnotationPass<'a> {
    pub(crate) async fn new(content: &'a Document, ast: &'a Ast<'a, ParseMetadata>) -> Self {
        Self {
            ast,
            content,
            assigned_names: vec![Default::default()],
            ids: vec![Default::default()],
            edits: vec![],
        }
    }

    pub(crate) fn execute(mut self) -> Vec<TextEdit> {
        for decl in &self.ast.decls {
            match decl {
                Decl::Fn(f) => {
                    self.transform_fn_decl(f);
                }
                Decl::Cell(c) => {
                    self.transform_cell_decl(c);
                }
                _ => todo!(),
            }
        }

        self.edits
    }
}

impl<'a> AstTransformer<'a> for ScopeAnnotationPass<'a> {
    type Input = ParseMetadata;
    type Output = ParseMetadata;

    fn dispatch_ident(
        &mut self,
        _input: &Ident<'a, Self::Input>,
    ) -> <Self::Output as AstMetadata>::Ident {
    }

    fn dispatch_var_expr(
        &mut self,
        _input: &VarExpr<'a, Self::Input>,
        _name: &Ident<'a, Self::Output>,
    ) -> <Self::Output as AstMetadata>::VarExpr {
    }

    fn dispatch_enum_decl(
        &mut self,
        _input: &EnumDecl<'a, Self::Input>,
        _name: &Ident<'a, Self::Output>,
        _variants: &[Ident<'a, Self::Output>],
    ) -> <Self::Output as AstMetadata>::EnumDecl {
    }

    fn dispatch_cell_decl(
        &mut self,
        _input: &CellDecl<'a, Self::Input>,
        _name: &Ident<'a, Self::Output>,
        _args: &[ArgDecl<'a, Self::Output>],
        _scope: &Scope<'a, Self::Output>,
    ) -> <Self::Output as AstMetadata>::CellDecl {
    }

    fn dispatch_fn_decl(
        &mut self,
        _input: &FnDecl<'a, Self::Input>,
        _name: &Ident<'a, Self::Output>,
        _args: &[ArgDecl<'a, Self::Output>],
        _return_ty: &Option<Ident<'a, Self::Output>>,
        _scope: &Scope<'a, Self::Output>,
    ) -> <Self::Output as AstMetadata>::FnDecl {
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
        _cond: &Expr<'a, Self::Output>,
        _then: &Scope<'a, Self::Output>,
        _else_: &Scope<'a, Self::Output>,
    ) -> <Self::Output as AstMetadata>::IfExpr {
        if let Some(scope_annotation) = &input.scope_annotation {
            self.assigned_names
                .last_mut()
                .unwrap()
                .insert(scope_annotation.name.to_string());
        } else {
            let name = loop {
                let name = format!("scope{}", self.ids.last().unwrap());
                *self.ids.last_mut().unwrap() += 1;
                let names = self.assigned_names.last_mut().unwrap();
                if !names.contains(&name) {
                    names.insert(name.clone());
                    break name;
                }
            };
            let start = self.content.offset_to_pos(input.span.start());
            self.edits.push(TextEdit {
                range: Range::new(start, start),
                new_text: format!("#{name} "),
            });
        }
    }

    fn dispatch_bin_op_expr(
        &mut self,
        _input: &BinOpExpr<'a, Self::Input>,
        _left: &Expr<'a, Self::Output>,
        _right: &Expr<'a, Self::Output>,
    ) -> <Self::Output as AstMetadata>::BinOpExpr {
    }

    fn dispatch_unary_op_expr(
        &mut self,
        _input: &UnaryOpExpr<'a, Self::Input>,
        _operand: &Expr<'a, Self::Output>,
    ) -> <Self::Output as AstMetadata>::UnaryOpExpr {
    }

    fn dispatch_comparison_expr(
        &mut self,
        _input: &ComparisonExpr<'a, Self::Input>,
        _left: &Expr<'a, Self::Output>,
        _right: &Expr<'a, Self::Output>,
    ) -> <Self::Output as AstMetadata>::ComparisonExpr {
    }

    fn dispatch_field_access_expr(
        &mut self,
        _input: &FieldAccessExpr<'a, Self::Input>,
        _base: &Expr<'a, Self::Output>,
        _field: &Ident<'a, Self::Output>,
    ) -> <Self::Output as AstMetadata>::FieldAccessExpr {
    }

    fn dispatch_call_expr(
        &mut self,
        _input: &CallExpr<'a, Self::Input>,
        _func: &Ident<'a, Self::Output>,
        _args: &Args<'a, Self::Output>,
    ) -> <Self::Output as AstMetadata>::CallExpr {
    }

    fn enter_scope(&mut self, _input: &Scope<'a, Self::Input>) {
        self.assigned_names.push(Default::default());
        self.ids.push(Default::default());
    }

    fn exit_scope(&mut self, _input: &Scope<'a, Self::Input>, _output: &Scope<'a, Self::Output>) {
        self.assigned_names.pop();
        self.ids.pop();
    }

    fn dispatch_let_binding(
        &mut self,
        _input: &compiler::ast::LetBinding<'a, Self::Input>,
        _name: &Ident<'a, Self::Output>,
        _value: &Expr<'a, Self::Output>,
    ) -> <Self::Output as AstMetadata>::LetBinding {
    }

    fn dispatch_cast(
        &mut self,
        _input: &compiler::ast::CastExpr<'a, Self::Input>,
        _value: &Expr<'a, Self::Output>,
        _ty: &Ident<'a, Self::Output>,
    ) -> <Self::Output as AstMetadata>::CastExpr {
    }

    fn dispatch_enum_value(
        &mut self,
        _input: &compiler::ast::EnumValue<'a, Self::Input>,
        _name: &Ident<'a, Self::Output>,
        _variant: &Ident<'a, Self::Output>,
    ) -> <Self::Output as AstMetadata>::EnumValue {
    }

    fn dispatch_emit_expr(
        &mut self,
        _input: &compiler::ast::EmitExpr<'a, Self::Input>,
        _value: &Expr<'a, Self::Output>,
    ) -> <Self::Output as AstMetadata>::EmitExpr {
    }

    fn dispatch_args(
        &mut self,
        _input: &Args<'a, Self::Input>,
        _posargs: &[Expr<'a, Self::Output>],
        _kwargs: &[compiler::ast::KwArgValue<'a, Self::Output>],
    ) -> <Self::Output as AstMetadata>::Args {
    }

    fn dispatch_kw_arg_value(
        &mut self,
        _input: &compiler::ast::KwArgValue<'a, Self::Input>,
        _name: &Ident<'a, Self::Output>,
        _value: &Expr<'a, Self::Output>,
    ) -> <Self::Output as AstMetadata>::KwArgValue {
    }

    fn dispatch_arg_decl(
        &mut self,
        _input: &ArgDecl<'a, Self::Input>,
        _name: &Ident<'a, Self::Output>,
        _ty: &Ident<'a, Self::Output>,
    ) -> <Self::Output as AstMetadata>::ArgDecl {
    }

    fn dispatch_scope(
        &mut self,
        _input: &Scope<'a, Self::Input>,
        _stmts: &[compiler::ast::Statement<'a, Self::Output>],
        _tail: &Option<Expr<'a, Self::Output>>,
    ) -> <Self::Output as AstMetadata>::Scope {
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
            Expr::StringLiteral(string_literal) => Expr::StringLiteral(string_literal.clone()),
            Expr::Scope(scope) => {
                if let Some(scope_annotation) = &scope.scope_annotation {
                    self.assigned_names
                        .last_mut()
                        .unwrap()
                        .insert(scope_annotation.name.to_string());
                } else {
                    let name = loop {
                        let name = format!("scope{}", self.ids.last().unwrap());
                        *self.ids.last_mut().unwrap() += 1;
                        let names = self.assigned_names.last_mut().unwrap();
                        if !names.contains(&name) {
                            names.insert(name.clone());
                            break name;
                        }
                    };
                    let start = self.content.offset_to_pos(scope.span.start());
                    self.edits.push(TextEdit {
                        range: Range::new(start, start),
                        new_text: format!("#{name} "),
                    });
                }
                Expr::Scope(Box::new(self.transform_scope(scope)))
            }
            Expr::Cast(cast) => Expr::Cast(Box::new(self.transform_cast(cast))),
        }
    }
}
