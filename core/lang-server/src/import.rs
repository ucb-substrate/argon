use arcstr::Substr;
use compiler::{
    ast::{
        ArgDecl, Args, AstMetadata, AstTransformer, BinOpExpr, CallExpr, CellDecl, ComparisonExpr,
        ConstantDecl, Decl, EnumDecl, Expr, FieldAccessExpr, FnDecl, Ident, IdentPath, IfExpr,
        Scope, TySpec, UnaryOpExpr, annotated::AnnotatedAst,
    },
    compile::BUILTINS,
    parse::ParseMetadata,
};
use indexmap::IndexSet;
use tower_lsp_server::lsp_types::{Range, TextEdit};

use crate::document::Document;

pub(crate) struct ScopeAnnotationPass<'a> {
    ast: &'a AnnotatedAst<ParseMetadata>,
    content: Document,
    annotations: Vec<Vec<(Option<Substr>, Range)>>,
    edits: Vec<TextEdit>,
}

impl<'a> ScopeAnnotationPass<'a> {
    pub(crate) fn new(ast: &'a AnnotatedAst<ParseMetadata>) -> Self {
        Self {
            ast,
            content: Document::new(&ast.text, 0),
            annotations: vec![Default::default()],
            edits: vec![],
        }
    }

    pub(crate) fn execute(mut self) -> Vec<TextEdit> {
        for decl in &self.ast.ast.decls {
            match decl {
                Decl::Fn(f) => {
                    self.transform_fn_decl(f);
                }
                Decl::Cell(c) => {
                    self.transform_cell_decl(c);
                }
                Decl::Mod(c) => {
                    self.transform_mod_decl(c);
                }
                Decl::Enum(_) => {}
                _ => todo!(),
            }
        }

        self.edits
    }
}

fn increment_or_add_zero(input: &str) -> String {
    // Find the index where the trailing digits start
    let split_idx = input
        .char_indices()
        .rev()
        .take_while(|(_, c)| c.is_ascii_digit())
        .last()
        .map(|(i, _)| i);

    match split_idx {
        Some(idx) => {
            let (prefix, suffix) = input.split_at(idx);

            // Parse the number
            if let Ok(num) = suffix.parse::<u64>() {
                // Increment and maintain the original width for leading zeros
                let incremented = num + 1;
                format!("{}{:0width$}", prefix, incremented, width = suffix.len())
            } else {
                // Handle cases where the number is too large for u64
                format!("{}0", input)
            }
        }
        None => {
            // No digits at the end, just add '0'
            format!("{}0", input)
        }
    }
}

impl<'a> AstTransformer for ScopeAnnotationPass<'a> {
    type InputMetadata = ParseMetadata;
    type OutputMetadata = ParseMetadata;
    type InputS = Substr;
    type OutputS = Substr;

    fn dispatch_ident(
        &mut self,
        _input: &Ident<Substr, Self::InputMetadata>,
    ) -> <Self::OutputMetadata as AstMetadata>::Ident {
    }

    fn dispatch_ident_path(
        &mut self,
        _input: &IdentPath<Self::InputS, Self::InputMetadata>,
    ) -> <Self::OutputMetadata as AstMetadata>::IdentPath {
    }

    fn dispatch_enum_decl(
        &mut self,
        _input: &EnumDecl<Substr, Self::InputMetadata>,
        _name: &Ident<Substr, Self::OutputMetadata>,
        _variants: &[Ident<Substr, Self::OutputMetadata>],
    ) -> <Self::OutputMetadata as AstMetadata>::EnumDecl {
    }

    fn dispatch_cell_decl(
        &mut self,
        _input: &CellDecl<Substr, Self::InputMetadata>,
        _name: &Ident<Substr, Self::OutputMetadata>,
        _args: &[ArgDecl<Substr, Self::OutputMetadata>],
        _scope: &Scope<Substr, Self::OutputMetadata>,
    ) -> <Self::OutputMetadata as AstMetadata>::CellDecl {
    }

    fn dispatch_fn_decl(
        &mut self,
        _input: &FnDecl<Substr, Self::InputMetadata>,
        _name: &Ident<Substr, Self::OutputMetadata>,
        _args: &[ArgDecl<Substr, Self::OutputMetadata>],
        _return_ty: &Option<TySpec<Substr, Self::OutputMetadata>>,
        _scope: &Scope<Substr, Self::OutputMetadata>,
    ) -> <Self::OutputMetadata as AstMetadata>::FnDecl {
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
        _cond: &Expr<Substr, Self::OutputMetadata>,
        _then: &Scope<Substr, Self::OutputMetadata>,
        _else_: &Scope<Substr, Self::OutputMetadata>,
    ) -> <Self::OutputMetadata as AstMetadata>::IfExpr {
        if let Some(scope_annotation) = &input.scope_annotation {
            self.annotations.last_mut().unwrap().push((
                Some(scope_annotation.name.clone()),
                self.content.span_to_range(scope_annotation.span),
            ));
        } else {
            let start = self.content.offset_to_pos(input.span.start());
            self.annotations
                .last_mut()
                .unwrap()
                .push((None, Range::new(start, start)));
        }
    }

    fn dispatch_match_expr(
        &mut self,
        _input: &compiler::ast::MatchExpr<Self::InputS, Self::InputMetadata>,
        _scrutinee: &Expr<Self::OutputS, Self::OutputMetadata>,
        _arms: &[compiler::ast::MatchArm<Self::OutputS, Self::OutputMetadata>],
    ) -> <Self::OutputMetadata as AstMetadata>::MatchExpr {
    }

    fn dispatch_bin_op_expr(
        &mut self,
        _input: &BinOpExpr<Substr, Self::InputMetadata>,
        _left: &Expr<Substr, Self::OutputMetadata>,
        _right: &Expr<Substr, Self::OutputMetadata>,
    ) -> <Self::OutputMetadata as AstMetadata>::BinOpExpr {
    }

    fn dispatch_unary_op_expr(
        &mut self,
        _input: &UnaryOpExpr<Substr, Self::InputMetadata>,
        _operand: &Expr<Substr, Self::OutputMetadata>,
    ) -> <Self::OutputMetadata as AstMetadata>::UnaryOpExpr {
    }

    fn dispatch_comparison_expr(
        &mut self,
        _input: &ComparisonExpr<Substr, Self::InputMetadata>,
        _left: &Expr<Substr, Self::OutputMetadata>,
        _right: &Expr<Substr, Self::OutputMetadata>,
    ) -> <Self::OutputMetadata as AstMetadata>::ComparisonExpr {
    }

    fn dispatch_field_access_expr(
        &mut self,
        _input: &FieldAccessExpr<Substr, Self::InputMetadata>,
        _base: &Expr<Substr, Self::OutputMetadata>,
        _field: &Ident<Substr, Self::OutputMetadata>,
    ) -> <Self::OutputMetadata as AstMetadata>::FieldAccessExpr {
    }

    fn dispatch_index_expr(
        &mut self,
        _input: &compiler::ast::IndexExpr<Self::InputS, Self::InputMetadata>,
        _base: &Expr<Self::OutputS, Self::OutputMetadata>,
        _index: &Expr<Self::OutputS, Self::OutputMetadata>,
    ) -> <Self::OutputMetadata as AstMetadata>::IndexExpr {
    }

    fn dispatch_call_expr(
        &mut self,
        input: &CallExpr<Substr, Self::InputMetadata>,
        _func: &IdentPath<Substr, Self::OutputMetadata>,
        _args: &Args<Substr, Self::OutputMetadata>,
    ) -> <Self::OutputMetadata as AstMetadata>::CallExpr {
        if BUILTINS.contains(&input.func.path.last().unwrap().name.as_str()) {
            return;
        }
        if let Some(scope_annotation) = &input.scope_annotation {
            self.annotations.last_mut().unwrap().push((
                Some(scope_annotation.name.clone()),
                self.content.span_to_range(scope_annotation.span),
            ));
        } else {
            let start = self.content.offset_to_pos(input.span.start());
            self.annotations
                .last_mut()
                .unwrap()
                .push((None, Range::new(start, start)));
        }
    }

    fn enter_scope(&mut self, _input: &Scope<Substr, Self::InputMetadata>) {
        self.annotations.push(Default::default());
    }

    fn exit_scope(
        &mut self,
        _input: &Scope<Substr, Self::InputMetadata>,
        _output: &Scope<Substr, Self::OutputMetadata>,
    ) {
        if let Some(mut annotations) = self.annotations.pop() {
            let mut assigned_names = IndexSet::new();
            for (annotation, range) in &annotations {
                if let Some(annotation) = annotation {
                    if assigned_names.contains(annotation) {
                        let mut new_name = increment_or_add_zero(annotation);
                        while assigned_names.contains(new_name.as_str()) {
                            new_name = increment_or_add_zero(&new_name);
                        }
                        self.edits.push(TextEdit {
                            range: *range,
                            new_text: new_name.clone(),
                        });
                        assigned_names.insert(Substr::from(new_name));
                    } else {
                        assigned_names.insert(annotation.clone());
                    }
                }
            }
            annotations.reverse();
            let mut id = 0;
            for (annotation, range) in annotations {
                if annotation.is_none() {
                    let name = loop {
                        let name = Substr::from(format!("scope{}", id));
                        id += 1;
                        if !assigned_names.contains(&name) {
                            assigned_names.insert(name.clone());
                            break name;
                        }
                    };
                    self.edits.push(TextEdit {
                        range,
                        new_text: format!("#{name} "),
                    });
                }
            }
        }
    }

    fn dispatch_let_binding(
        &mut self,
        _input: &compiler::ast::LetBinding<Substr, Self::InputMetadata>,
        _name: &Ident<Substr, Self::OutputMetadata>,
        _value: &Expr<Substr, Self::OutputMetadata>,
    ) -> <Self::OutputMetadata as AstMetadata>::LetBinding {
    }

    fn dispatch_cast(
        &mut self,
        _input: &compiler::ast::CastExpr<Substr, Self::InputMetadata>,
        _value: &Expr<Substr, Self::OutputMetadata>,
        _ty: &TySpec<Substr, Self::OutputMetadata>,
    ) -> <Self::OutputMetadata as AstMetadata>::CastExpr {
    }

    fn dispatch_emit_expr(
        &mut self,
        _input: &compiler::ast::EmitExpr<Substr, Self::InputMetadata>,
        _value: &Expr<Substr, Self::OutputMetadata>,
    ) -> <Self::OutputMetadata as AstMetadata>::EmitExpr {
    }

    fn dispatch_args(
        &mut self,
        _input: &Args<Substr, Self::InputMetadata>,
        _posargs: &[Expr<Substr, Self::OutputMetadata>],
        _kwargs: &[compiler::ast::KwArgValue<Substr, Self::OutputMetadata>],
    ) -> <Self::OutputMetadata as AstMetadata>::Args {
    }

    fn dispatch_kw_arg_value(
        &mut self,
        _input: &compiler::ast::KwArgValue<Substr, Self::InputMetadata>,
        _name: &Ident<Substr, Self::OutputMetadata>,
        _value: &Expr<Substr, Self::OutputMetadata>,
    ) -> <Self::OutputMetadata as AstMetadata>::KwArgValue {
    }

    fn dispatch_arg_decl(
        &mut self,
        _input: &ArgDecl<Substr, Self::InputMetadata>,
        _name: &Ident<Substr, Self::OutputMetadata>,
        _ty: &TySpec<Substr, Self::OutputMetadata>,
    ) -> <Self::OutputMetadata as AstMetadata>::ArgDecl {
    }

    fn dispatch_scope(
        &mut self,
        _input: &Scope<Substr, Self::InputMetadata>,
        _stmts: &[compiler::ast::Statement<Substr, Self::OutputMetadata>],
        _tail: &Option<Expr<Substr, Self::OutputMetadata>>,
    ) -> <Self::OutputMetadata as AstMetadata>::Scope {
    }

    fn transform_expr(
        &mut self,
        input: &Expr<Substr, Self::InputMetadata>,
    ) -> Expr<Substr, Self::OutputMetadata> {
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
            Expr::Index(index_expr) => Expr::Index(Box::new(self.transform_index_expr(index_expr))),
            Expr::IdentPath(ident_path) => Expr::IdentPath(self.transform_ident_path(ident_path)),
            Expr::Nil(nil) => Expr::Nil(*nil),
            Expr::SeqNil(nil) => Expr::SeqNil(*nil),
            Expr::FloatLiteral(float_literal) => Expr::FloatLiteral(*float_literal),
            Expr::IntLiteral(int_literal) => Expr::IntLiteral(*int_literal),
            Expr::BoolLiteral(bool_literal) => Expr::BoolLiteral(*bool_literal),
            Expr::StringLiteral(string_literal) => Expr::StringLiteral(string_literal.clone()),
            Expr::Scope(scope) => {
                if let Some(scope_annotation) = &scope.scope_annotation {
                    self.annotations.last_mut().unwrap().push((
                        Some(scope_annotation.name.clone()),
                        self.content.span_to_range(scope_annotation.span),
                    ));
                } else {
                    let start = self.content.offset_to_pos(scope.span.start());
                    self.annotations
                        .last_mut()
                        .unwrap()
                        .push((None, Range::new(start, start)));
                }
                Expr::Scope(Box::new(self.transform_scope(scope)))
            }
            Expr::Cast(cast) => Expr::Cast(Box::new(self.transform_cast(cast))),
        }
    }

    fn transform_s(&mut self, s: &Self::InputS) -> Self::OutputS {
        s.clone()
    }
}
