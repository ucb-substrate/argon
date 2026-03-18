use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;

use antlr_rust::TidExt;
use antlr_rust::common_token_stream::CommonTokenStream;
use antlr_rust::error_listener::ErrorListener;
use antlr_rust::parser_rule_context::ParserRuleContext;
use antlr_rust::recognizer::Recognizer;
use antlr_rust::token::Token;
use antlr_rust::tree::TerminalNode;
use antlr_rust::{InputStream, Parser};
use arcstr::ArcStr;
use cfgrammar::Span;

use crate::ast::annotated::AnnotatedAst;
use crate::ast::{
    ArgDecl, Ast, BinOp, BinOpExpr, BoolLiteral, CallExpr, CastExpr, CellDecl, ComparisonExpr,
    ComparisonOp, ConstantDecl, Decl, EmitExpr, EnumDecl, Expr, FieldAccessExpr, FloatLiteral,
    FnDecl, ForLoop, Ident, IdentPath, IfExpr, IndexExpr, IndexFieldAccessExpr, IntLiteral,
    KwArgValue, LetBinding, MatchArm, MatchExpr, ModDecl, NilLiteral, Scope, SeqNilLiteral,
    Statement, StringLiteral, StructDecl, StructField, TupleExpr, TySpec, TySpecKind, UnaryOp,
    UnaryOpExpr,
};
use crate::parse::{AnnotatedParseAst, ParseMetadata};

mod generated {
    include!(concat!(env!("OUT_DIR"), "/antlr/mod.rs"));
}

use generated::argonlexer::ArgonLexer;
use generated::argonparser::*;

#[derive(Debug, Clone)]
pub struct AntlrParseError {
    pub span: Span,
    pub message: String,
}

#[derive(Debug)]
struct CollectingErrorListener {
    input: Rc<str>,
    offset_base: usize,
    errors: Rc<RefCell<Vec<AntlrParseError>>>,
}

impl CollectingErrorListener {
    fn push(&self, line: isize, column: isize, msg: &str) {
        let start = self.offset_base + line_column_to_offset(&self.input, line, column);
        let end = next_char_boundary(&self.input, start);
        self.errors.borrow_mut().push(AntlrParseError {
            span: Span::new(start, end),
            message: msg.to_string(),
        });
    }
}

impl<'input, T> ErrorListener<'input, T> for CollectingErrorListener
where
    T: Recognizer<'input>,
{
    fn syntax_error(
        &self,
        _recognizer: &T,
        _offending_symbol: Option<
            &<T::TF as antlr_rust::token_factory::TokenFactory<'input>>::Inner,
        >,
        line: isize,
        column: isize,
        msg: &str,
        _error: Option<&antlr_rust::errors::ANTLRError>,
    ) {
        self.push(line, column, msg);
    }
}

pub fn parse_errors(input: &str) -> Vec<AntlrParseError> {
    let normalized_input = input.trim_start_matches(char::is_whitespace);
    let offset_base = input.len() - normalized_input.len();
    let input_rc: Rc<str> = Rc::from(normalized_input);
    let errors = Rc::new(RefCell::new(Vec::new()));

    let mut lexer = ArgonLexer::new(InputStream::new(normalized_input));
    lexer.remove_error_listeners();
    lexer.add_error_listener(Box::new(CollectingErrorListener {
        input: Rc::clone(&input_rc),
        offset_base,
        errors: Rc::clone(&errors),
    }));

    let tokens = CommonTokenStream::new(lexer);
    let mut parser = ArgonParser::new(tokens);
    parser.remove_error_listeners();
    parser.add_error_listener(Box::new(CollectingErrorListener {
        input: Rc::clone(&input_rc),
        offset_base,
        errors: Rc::clone(&errors),
    }));

    let _ = parser.compilationUnit();
    errors.borrow().clone()
}

pub fn parse_ast(input: ArcStr, path: PathBuf) -> Result<AnnotatedParseAst, Vec<AntlrParseError>> {
    let input_for_ast = input.clone();
    let normalized_input = input.trim_start_matches(char::is_whitespace);
    let offset_base = input.len() - normalized_input.len();

    let mut lexer = ArgonLexer::new(InputStream::new(normalized_input));
    lexer.remove_error_listeners();
    let tokens = CommonTokenStream::new(lexer);
    let mut parser = ArgonParser::new(tokens);
    parser.remove_error_listeners();

    let tree = match parser.compilationUnit() {
        Ok(tree) => tree,
        Err(err) => {
            return Err(vec![AntlrParseError {
                span: Span::new(0, input.len()),
                message: err.to_string(),
            }]);
        }
    };

    let mut builder = AstBuilder {
        input: normalized_input,
        offset_base,
        errors: Vec::new(),
    };
    let ast = builder.build_compilation_unit(tree.as_ref());
    if builder.errors.is_empty() {
        Ok(AnnotatedAst::new(input_for_ast, &ast, path))
    } else {
        Err(builder.errors)
    }
}

struct AstBuilder<'input> {
    input: &'input str,
    offset_base: usize,
    errors: Vec<AntlrParseError>,
}

impl<'input> AstBuilder<'input> {
    fn build_compilation_unit(
        &mut self,
        ctx: &CompilationUnitContext<'input>,
    ) -> Ast<&'input str, ParseMetadata> {
        let mut decls = Vec::new();
        for item in ctx.children_of_type::<SourceItemContext<'input>>() {
            if let Some(decl_ctx) = item.decl() {
                if let Some(decl) = self.build_decl(decl_ctx.as_ref()) {
                    decls.push(decl);
                }
            } else if let Some(stmt) = item.topLevelStatement() {
                self.unsupported(
                    self.span_of(stmt.as_ref()),
                    "top-level statements are not supported",
                );
            }
        }
        Ast {
            decls,
            span: self.span_of(ctx),
        }
    }

    fn build_decl(
        &mut self,
        ctx: &DeclContext<'input>,
    ) -> Option<Decl<&'input str, ParseMetadata>> {
        if let Some(ctx) = ctx.enumDecl() {
            Some(Decl::Enum(self.build_enum_decl(ctx.as_ref())))
        } else if let Some(ctx) = ctx.structDecl() {
            Some(Decl::Struct(self.build_struct_decl(ctx.as_ref())))
        } else if let Some(ctx) = ctx.cellDecl() {
            Some(Decl::Cell(self.build_cell_decl(ctx.as_ref())))
        } else if let Some(ctx) = ctx.fnDecl() {
            Some(Decl::Fn(self.build_fn_decl(ctx.as_ref())))
        } else if let Some(ctx) = ctx.constDecl() {
            Some(Decl::Constant(self.build_const_decl(ctx.as_ref())))
        } else {
            ctx.modDecl()
                .map(|ctx| Decl::Mod(self.build_mod_decl(ctx.as_ref())))
        }
    }

    fn build_enum_decl(
        &mut self,
        ctx: &EnumDeclContext<'input>,
    ) -> EnumDecl<&'input str, ParseMetadata> {
        EnumDecl {
            name: self.ident_from_token(&ctx.IDENT().unwrap()),
            variants: ctx
                .enumVariants()
                .map(|ctx| self.build_enum_variants(ctx.as_ref()))
                .unwrap_or_default(),
            metadata: (),
        }
    }

    fn build_enum_variants(
        &mut self,
        ctx: &EnumVariantsContext<'input>,
    ) -> Vec<Ident<&'input str, ParseMetadata>> {
        let mut variants = ctx
            .children_of_type::<EnumVariantsContext<'input>>()
            .into_iter()
            .flat_map(|prev| self.build_enum_variants(prev.as_ref()))
            .collect::<Vec<_>>();
        if let Some(ident) = ctx.IDENT() {
            variants.push(self.ident_from_token(&ident));
        }
        variants
    }

    fn build_struct_decl(
        &mut self,
        ctx: &StructDeclContext<'input>,
    ) -> StructDecl<&'input str, ParseMetadata> {
        StructDecl {
            name: self.ident_from_token(&ctx.IDENT().unwrap()),
            fields: ctx
                .structFields()
                .map(|ctx| self.build_struct_fields(ctx.as_ref()))
                .unwrap_or_default(),
            span: self.span_of(ctx),
            metadata: (),
        }
    }

    fn build_struct_fields(
        &mut self,
        ctx: &StructFieldsContext<'input>,
    ) -> Vec<StructField<&'input str, ParseMetadata>> {
        let mut fields = ctx
            .children_of_type::<StructFieldsContext<'input>>()
            .into_iter()
            .flat_map(|prev| self.build_struct_fields(prev.as_ref()))
            .collect::<Vec<_>>();
        if let Some(field) = ctx.structField() {
            fields.push(self.build_struct_field(field.as_ref()));
        }
        fields
    }

    fn build_struct_field(
        &mut self,
        ctx: &StructFieldContext<'input>,
    ) -> StructField<&'input str, ParseMetadata> {
        let ty_spec = self.build_ty_spec(ctx.tySpec().unwrap().as_ref());
        let ty = match ty_spec.kind {
            TySpecKind::Ident(ident) => ident,
            _ => {
                self.unsupported(self.span_of(ctx), "struct field types must be identifiers");
                self.empty_ident(self.span_of(ctx))
            }
        };
        StructField {
            name: self.ident_from_token(&ctx.IDENT().unwrap()),
            ty,
            span: self.span_of(ctx),
            metadata: (),
        }
    }

    fn build_cell_decl(
        &mut self,
        ctx: &CellDeclContext<'input>,
    ) -> CellDecl<&'input str, ParseMetadata> {
        CellDecl {
            name: self.ident_from_token(&ctx.IDENT().unwrap()),
            args: self.build_arg_decls(ctx.argDecls().unwrap().as_ref()),
            scope: self.build_scope(ctx.scope().unwrap().as_ref()),
            span: self.span_of(ctx),
            metadata: (),
        }
    }

    fn build_fn_decl(&mut self, ctx: &FnDeclContext<'input>) -> FnDecl<&'input str, ParseMetadata> {
        FnDecl {
            name: self.ident_from_token(&ctx.IDENT().unwrap()),
            args: self.build_arg_decls(ctx.argDecls().unwrap().as_ref()),
            return_ty: ctx.tySpec().map(|ty| self.build_ty_spec(ty.as_ref())),
            scope: self.build_scope(ctx.scope().unwrap().as_ref()),
            span: self.span_of(ctx),
            metadata: (),
        }
    }

    fn build_const_decl(
        &mut self,
        ctx: &ConstDeclContext<'input>,
    ) -> ConstantDecl<&'input str, ParseMetadata> {
        let ty_spec = self.build_ty_spec(ctx.tySpec().unwrap().as_ref());
        let ty = match ty_spec.kind {
            TySpecKind::Ident(ident) => ident,
            _ => {
                self.unsupported(self.span_of(ctx), "const types must be identifiers");
                self.empty_ident(self.span_of(ctx))
            }
        };
        ConstantDecl {
            name: self.ident_from_token(&ctx.IDENT().unwrap()),
            ty,
            value: self.build_expr(ctx.expr().unwrap().as_ref()),
            metadata: (),
        }
    }

    fn build_mod_decl(
        &mut self,
        ctx: &ModDeclContext<'input>,
    ) -> ModDecl<&'input str, ParseMetadata> {
        ModDecl {
            ident: self.ident_from_token(&ctx.IDENT().unwrap()),
            span: self.span_of(ctx),
        }
    }

    fn build_arg_decls(
        &mut self,
        ctx: &ArgDeclsContext<'input>,
    ) -> Vec<ArgDecl<&'input str, ParseMetadata>> {
        ctx.argDeclList()
            .map(|list| self.build_arg_decl_list(list.as_ref()))
            .unwrap_or_default()
    }

    fn build_arg_decl_list(
        &mut self,
        ctx: &ArgDeclListContext<'input>,
    ) -> Vec<ArgDecl<&'input str, ParseMetadata>> {
        let mut decls = ctx
            .children_of_type::<ArgDeclListContext<'input>>()
            .into_iter()
            .flat_map(|prev| self.build_arg_decl_list(prev.as_ref()))
            .collect::<Vec<_>>();
        if let Some(arg) = ctx.argDecl() {
            decls.push(self.build_arg_decl(arg.as_ref()));
        }
        decls
    }

    fn build_arg_decl(
        &mut self,
        ctx: &ArgDeclContext<'input>,
    ) -> ArgDecl<&'input str, ParseMetadata> {
        let ty = ctx
            .tySpec()
            .map(|ctx| self.build_ty_spec(ctx.as_ref()))
            .unwrap_or_else(|| {
                self.unsupported(
                    self.span_of(ctx),
                    "function arguments require explicit types",
                );
                TySpec {
                    kind: TySpecKind::Ident(self.empty_ident(self.span_of(ctx))),
                    span: self.span_of(ctx),
                }
            });
        ArgDecl {
            name: self.ident_from_token(&ctx.IDENT().unwrap()),
            ty,
            metadata: (),
        }
    }

    fn build_scope(&mut self, ctx: &ScopeContext<'input>) -> Scope<&'input str, ParseMetadata> {
        let mut scope = self.build_unannotated_scope(ctx.unannotatedScope().unwrap().as_ref());
        scope.scope_annotation = ctx
            .annotation()
            .map(|annotation| self.build_annotation(annotation.as_ref()));
        scope
    }

    fn build_unannotated_scope(
        &mut self,
        ctx: &UnannotatedScopeContext<'input>,
    ) -> Scope<&'input str, ParseMetadata> {
        let mut stmts = ctx
            .children_of_type::<StatementContext<'input>>()
            .into_iter()
            .filter_map(|stmt| self.build_statement(stmt.as_ref()))
            .collect::<Vec<_>>();
        let mut tail = ctx.expr().map(|expr| self.build_expr(expr.as_ref()));
        if tail.is_none()
            && let Some(Statement::Expr {
                value,
                semicolon: false,
            }) = stmts.last().cloned()
        {
            stmts.pop();
            tail = Some(value);
        }
        Scope {
            scope_annotation: None,
            span: self.span_of(ctx),
            stmts,
            tail,
            metadata: (),
        }
    }

    fn build_statement(
        &mut self,
        ctx: &StatementContext<'input>,
    ) -> Option<Statement<&'input str, ParseMetadata>> {
        if let Some(let_stmt) = ctx.letStmt() {
            let names = self.build_ident_list(let_stmt.identList().unwrap().as_ref());
            if names.len() != 1 || let_stmt.expr().is_none() {
                self.unsupported(
                    self.span_of(let_stmt.as_ref()),
                    "only single-name let bindings with values are supported",
                );
                None
            } else {
                Some(Statement::LetBinding(LetBinding {
                    name: names.into_iter().next().unwrap(),
                    value: self.build_expr(let_stmt.expr().unwrap().as_ref()),
                    span: self.span_of(let_stmt.as_ref()),
                    metadata: (),
                }))
            }
        } else if let Some(for_stmt) = ctx.forStmt() {
            Some(Statement::ForLoop(self.build_for_stmt(for_stmt.as_ref())))
        } else if let Some(block_expr) = ctx.blockExpr() {
            Some(Statement::Expr {
                value: self.build_block_expr(block_expr.as_ref()),
                semicolon: false,
            })
        } else {
            ctx.expr().map(|expr| Statement::Expr {
                value: self.build_expr(expr.as_ref()),
                semicolon: true,
            })
        }
    }

    fn build_ident_list(
        &mut self,
        ctx: &IdentListContext<'input>,
    ) -> Vec<Ident<&'input str, ParseMetadata>> {
        let mut names = Vec::new();
        let mut i = 0;
        while let Some(ident) = ctx.IDENT(i) {
            names.push(self.ident_from_token(&ident));
            i += 1;
        }
        names
    }

    fn build_for_stmt(
        &mut self,
        ctx: &ForStmtContext<'input>,
    ) -> ForLoop<&'input str, ParseMetadata> {
        ForLoop {
            var: self.ident_from_token(&ctx.IDENT().unwrap()),
            seq: self.build_expr(ctx.expr().unwrap().as_ref()),
            body: self.build_scope(ctx.scope().unwrap().as_ref()),
            span: self.span_of(ctx),
            metadata: (),
        }
    }

    fn build_expr(&mut self, ctx: &ExprContext<'input>) -> Expr<&'input str, ParseMetadata> {
        if let Some(block) = ctx.blockExpr() {
            self.build_block_expr(block.as_ref())
        } else {
            self.build_comparison_expr(ctx.comparisonExpr().unwrap().as_ref())
        }
    }

    fn build_block_expr(
        &mut self,
        ctx: &BlockExprContext<'input>,
    ) -> Expr<&'input str, ParseMetadata> {
        let scopes = ctx.children_of_type::<ScopeContext<'input>>();
        let arms = ctx.children_of_type::<MatchArmContext<'input>>();
        if !arms.is_empty() {
            Expr::Match(Box::new(MatchExpr {
                scrutinee: self.build_expr(ctx.expr().unwrap().as_ref()),
                arms: arms
                    .into_iter()
                    .map(|arm| self.build_match_arm(arm.as_ref()))
                    .collect(),
                span: self.span_of(ctx),
                metadata: (),
            }))
        } else if scopes.len() == 1 {
            Expr::Scope(Box::new(self.build_scope(scopes[0].as_ref())))
        } else {
            Expr::If(Box::new(IfExpr {
                scope_annotation: ctx
                    .annotation()
                    .map(|annotation| self.build_annotation(annotation.as_ref())),
                cond: self.build_expr(ctx.expr().unwrap().as_ref()),
                then: self.build_scope(scopes[0].as_ref()),
                else_: self.build_scope(scopes[1].as_ref()),
                span: self.span_of(ctx),
                metadata: (),
            }))
        }
    }

    fn build_match_arm(
        &mut self,
        ctx: &MatchArmContext<'input>,
    ) -> MatchArm<&'input str, ParseMetadata> {
        MatchArm {
            pattern: self.build_ident_path(ctx.identPath().unwrap().as_ref()),
            expr: self.build_expr(ctx.expr().unwrap().as_ref()),
            span: self.span_of(ctx),
        }
    }

    fn build_comparison_expr(
        &mut self,
        ctx: &ComparisonExprContext<'input>,
    ) -> Expr<&'input str, ParseMetadata> {
        let mut expr = self.build_additive_expr(ctx.additiveExpr(0).unwrap().as_ref());
        for (i, op) in self.comparison_ops(ctx).into_iter().enumerate() {
            let rhs = self.build_additive_expr(ctx.additiveExpr(i + 1).unwrap().as_ref());
            let span = Span::new(expr.span().start(), rhs.span().end());
            expr = Expr::Comparison(Box::new(ComparisonExpr {
                op,
                left: expr,
                right: rhs,
                span,
                metadata: (),
            }));
        }
        expr
    }

    fn build_additive_expr(
        &mut self,
        ctx: &AdditiveExprContext<'input>,
    ) -> Expr<&'input str, ParseMetadata> {
        let mut expr = self.build_multiplicative_expr(ctx.multiplicativeExpr(0).unwrap().as_ref());
        for (i, op) in self.additive_ops(ctx).into_iter().enumerate() {
            let rhs =
                self.build_multiplicative_expr(ctx.multiplicativeExpr(i + 1).unwrap().as_ref());
            let span = Span::new(expr.span().start(), rhs.span().end());
            expr = Expr::BinOp(Box::new(BinOpExpr {
                op,
                left: expr,
                right: rhs,
                span,
                metadata: (),
            }));
        }
        expr
    }

    fn build_multiplicative_expr(
        &mut self,
        ctx: &MultiplicativeExprContext<'input>,
    ) -> Expr<&'input str, ParseMetadata> {
        let mut expr = self.build_unary_expr(ctx.unaryExpr(0).unwrap().as_ref());
        for (i, op) in self.multiplicative_ops(ctx).into_iter().enumerate() {
            let rhs = self.build_unary_expr(ctx.unaryExpr(i + 1).unwrap().as_ref());
            let span = Span::new(expr.span().start(), rhs.span().end());
            expr = Expr::BinOp(Box::new(BinOpExpr {
                op,
                left: expr,
                right: rhs,
                span,
                metadata: (),
            }));
        }
        expr
    }

    fn build_unary_expr(
        &mut self,
        ctx: &UnaryExprContext<'input>,
    ) -> Expr<&'input str, ParseMetadata> {
        if ctx.BANG().is_some() {
            let operand = self.build_unary_expr(ctx.unaryExpr().unwrap().as_ref());
            Expr::UnaryOp(Box::new(UnaryOpExpr {
                op: UnaryOp::Not,
                operand,
                span: self.span_of(ctx),
                metadata: (),
            }))
        } else if ctx.MINUS().is_some() {
            let operand = self.build_unary_expr(ctx.unaryExpr().unwrap().as_ref());
            Expr::UnaryOp(Box::new(UnaryOpExpr {
                op: UnaryOp::Neg,
                operand,
                span: self.span_of(ctx),
                metadata: (),
            }))
        } else if let Some(cast) = ctx.castExpr() {
            self.build_cast_expr(cast.as_ref())
        } else {
            self.unsupported(
                self.span_of(ctx),
                "parenthesized prefix casts are not supported",
            );
            self.build_unary_expr(ctx.unaryExpr().unwrap().as_ref())
        }
    }

    fn build_cast_expr(
        &mut self,
        ctx: &CastExprContext<'input>,
    ) -> Expr<&'input str, ParseMetadata> {
        let mut expr = self.build_postfix_expr(ctx.postfixExpr().unwrap().as_ref());
        for ty in ctx.children_of_type::<TySpecContext<'input>>() {
            let ty = self.build_ty_spec(ty.as_ref());
            let span = Span::new(expr.span().start(), ty.span.end());
            expr = Expr::Cast(Box::new(CastExpr {
                value: expr,
                ty,
                span,
                metadata: (),
            }));
        }
        expr
    }

    fn build_postfix_expr(
        &mut self,
        ctx: &PostfixExprContext<'input>,
    ) -> Expr<&'input str, ParseMetadata> {
        let mut expr = self.build_primary_expr(ctx.primaryExpr().unwrap().as_ref());
        for op in ctx.children_of_type::<PostfixOpContext<'input>>() {
            expr = self.apply_postfix(expr, op.as_ref());
        }
        expr
    }

    fn apply_postfix(
        &mut self,
        base: Expr<&'input str, ParseMetadata>,
        ctx: &PostfixOpContext<'input>,
    ) -> Expr<&'input str, ParseMetadata> {
        if let Some(ident) = ctx.IDENT() {
            Expr::FieldAccess(Box::new(FieldAccessExpr {
                base,
                field: self.ident_from_token(&ident),
                span: self.span_of(ctx),
                metadata: (),
            }))
        } else if let Some(intlit) = ctx.INTLIT() {
            Expr::IndexFieldAccess(Box::new(IndexFieldAccessExpr {
                base,
                field: self.int_literal_from_token(&intlit),
                span: self.span_of(ctx),
                metadata: (),
            }))
        } else if let Some(expr) = ctx.expr() {
            Expr::Index(Box::new(IndexExpr {
                base,
                index: self.build_expr(expr.as_ref()),
                span: self.span_of(ctx),
                metadata: (),
            }))
        } else {
            Expr::Emit(Box::new(EmitExpr {
                value: base,
                span: self.span_of(ctx),
                metadata: (),
            }))
        }
    }

    fn build_primary_expr(
        &mut self,
        ctx: &PrimaryExprContext<'input>,
    ) -> Expr<&'input str, ParseMetadata> {
        if let Some(tuple) = ctx.tupleExpr() {
            Expr::Tuple(self.build_tuple_expr(tuple.as_ref()))
        } else if let Some(expr) = ctx.expr() {
            self.build_expr(expr.as_ref())
        } else if let Some(block) = ctx.blockExpr() {
            self.build_block_expr(block.as_ref())
        } else if let Some(call) = ctx.scopedCallExpr() {
            Expr::Call(self.build_call_expr(call.as_ref()))
        } else if let Some(path) = ctx.identPath() {
            Expr::IdentPath(self.build_ident_path(path.as_ref()))
        } else if let Some(literal) = ctx.literal() {
            self.build_literal(literal.as_ref())
        } else if ctx.LBRACK().is_some() {
            Expr::SeqNil(SeqNilLiteral {
                span: self.span_of(ctx),
            })
        } else if ctx.structLiteralExpr().is_some() {
            self.unsupported(
                self.span_of(ctx),
                "struct literal expressions are not supported",
            );
            Expr::Nil(NilLiteral {
                span: self.span_of(ctx),
            })
        } else if ctx.deferExpr().is_some() {
            self.unsupported(self.span_of(ctx), "defer expressions are not supported");
            Expr::Nil(NilLiteral {
                span: self.span_of(ctx),
            })
        } else {
            Expr::Nil(NilLiteral {
                span: self.span_of(ctx),
            })
        }
    }

    fn build_tuple_expr(
        &mut self,
        ctx: &TupleExprContext<'input>,
    ) -> TupleExpr<&'input str, ParseMetadata> {
        let mut items = vec![self.build_expr(ctx.expr().unwrap().as_ref())];
        if let Some(rest) = ctx.tupleExprRest() {
            items.extend(
                rest.children_of_type::<ExprContext<'input>>()
                    .into_iter()
                    .map(|expr| self.build_expr(expr.as_ref())),
            );
        }
        TupleExpr {
            items,
            span: self.span_of(ctx),
            metadata: (),
        }
    }

    fn build_call_expr(
        &mut self,
        ctx: &ScopedCallExprContext<'input>,
    ) -> CallExpr<&'input str, ParseMetadata> {
        CallExpr {
            scope_annotation: ctx
                .annotation()
                .map(|annotation| self.build_annotation(annotation.as_ref())),
            func: self.build_ident_path(ctx.identPath().unwrap().as_ref()),
            args: self.build_args(ctx.args().unwrap().as_ref()),
            span: self.span_of(ctx),
            metadata: (),
        }
    }

    fn build_args(
        &mut self,
        ctx: &ArgsContext<'input>,
    ) -> crate::ast::Args<&'input str, ParseMetadata> {
        if let Some(argument_list) = ctx.argumentList() {
            self.build_argument_list(argument_list.as_ref())
        } else {
            crate::ast::Args {
                posargs: Vec::new(),
                kwargs: Vec::new(),
                span: self.span_of(ctx),
                metadata: (),
            }
        }
    }

    fn build_argument_list(
        &mut self,
        ctx: &ArgumentListContext<'input>,
    ) -> crate::ast::Args<&'input str, ParseMetadata> {
        let mut posargs = Vec::new();
        let mut kwargs = Vec::new();
        for argument in ctx.children_of_type::<ArgumentContext<'input>>() {
            if let Some(ident) = argument.IDENT() {
                kwargs.push(KwArgValue {
                    name: self.ident_from_token(&ident),
                    value: self.build_expr(argument.expr().unwrap().as_ref()),
                    span: self.span_of(argument.as_ref()),
                    metadata: (),
                });
            } else if kwargs.is_empty() {
                posargs.push(self.build_expr(argument.expr().unwrap().as_ref()));
            } else {
                self.unsupported(
                    self.span_of(argument.as_ref()),
                    "positional arguments cannot follow keyword arguments",
                );
            }
        }
        crate::ast::Args {
            posargs,
            kwargs,
            span: self.span_of(ctx),
            metadata: (),
        }
    }

    fn build_ident_path(
        &mut self,
        ctx: &IdentPathContext<'input>,
    ) -> IdentPath<&'input str, ParseMetadata> {
        let mut path = Vec::new();
        let mut i = 0;
        while let Some(ident) = ctx.IDENT(i) {
            path.push(self.ident_from_token(&ident));
            i += 1;
        }
        IdentPath {
            path,
            metadata: (),
            span: self.span_of(ctx),
        }
    }

    fn build_ty_spec(&mut self, ctx: &TySpecContext<'input>) -> TySpec<&'input str, ParseMetadata> {
        let kind = if let Some(ident) = ctx.IDENT() {
            TySpecKind::Ident(self.ident_from_token(&ident))
        } else if let Some(inner) = ctx.tySpec() {
            TySpecKind::Seq(Box::new(self.build_ty_spec(inner.as_ref())))
        } else {
            TySpecKind::Tuple(
                ctx.tySpecList()
                    .map(|list| self.build_ty_spec_list(list.as_ref()))
                    .unwrap_or_default(),
            )
        };
        TySpec {
            kind,
            span: self.span_of(ctx),
        }
    }

    fn build_ty_spec_list(
        &mut self,
        ctx: &TySpecListContext<'input>,
    ) -> Vec<TySpec<&'input str, ParseMetadata>> {
        ctx.children_of_type::<TySpecContext<'input>>()
            .into_iter()
            .map(|ty| self.build_ty_spec(ty.as_ref()))
            .collect()
    }

    fn build_literal(&mut self, ctx: &LiteralContext<'input>) -> Expr<&'input str, ParseMetadata> {
        if let Some(float_ctx) = ctx.floatLiteral() {
            Expr::FloatLiteral(self.build_float_literal(float_ctx.as_ref()))
        } else if let Some(token) = ctx.INTLIT() {
            Expr::IntLiteral(self.int_literal_from_token(&token))
        } else if let Some(token) = ctx.STRLIT() {
            let span = self.span_of_token(&token);
            Expr::StringLiteral(StringLiteral {
                span,
                value: self.slice(span).trim_matches('"'),
            })
        } else if ctx.TRUE().is_some() {
            Expr::BoolLiteral(BoolLiteral {
                span: self.span_of(ctx),
                value: true,
            })
        } else {
            Expr::BoolLiteral(BoolLiteral {
                span: self.span_of(ctx),
                value: false,
            })
        }
    }

    fn build_float_literal(&mut self, ctx: &FloatLiteralContext<'input>) -> FloatLiteral {
        let span = self.span_of(ctx);
        FloatLiteral {
            span,
            value: self.slice(span).parse::<f64>().unwrap_or_default(),
        }
    }

    fn build_annotation(
        &mut self,
        ctx: &AnnotationContext<'input>,
    ) -> Ident<&'input str, ParseMetadata> {
        let token = ctx.get_token(ANNOTATION, 0).unwrap();
        let span = self.span_of_token(&token);
        let start = span.start() + 1;
        let end = span.end();
        let span = Span::new(start, end);
        Ident {
            span,
            name: self.slice(span),
            metadata: (),
        }
    }

    fn ident_from_token(
        &self,
        token: &TerminalNode<'input, ArgonParserContextType>,
    ) -> Ident<&'input str, ParseMetadata> {
        let span = self.span_of_token(token);
        Ident {
            span,
            name: self.slice(span),
            metadata: (),
        }
    }

    fn int_literal_from_token(
        &self,
        token: &TerminalNode<'input, ArgonParserContextType>,
    ) -> IntLiteral {
        let span = self.span_of_token(token);
        IntLiteral {
            span,
            value: self.slice(span).parse::<i64>().unwrap_or_default(),
        }
    }

    fn empty_ident(&self, span: Span) -> Ident<&'input str, ParseMetadata> {
        Ident {
            span,
            name: "",
            metadata: (),
        }
    }

    fn comparison_ops(&self, ctx: &ComparisonExprContext<'input>) -> Vec<ComparisonOp> {
        self.terminal_types(ctx)
            .into_iter()
            .filter_map(|ttype| match ttype {
                EQEQ => Some(ComparisonOp::Eq),
                NEQ => Some(ComparisonOp::Ne),
                GEQ => Some(ComparisonOp::Geq),
                GT => Some(ComparisonOp::Gt),
                LEQ => Some(ComparisonOp::Leq),
                LT => Some(ComparisonOp::Lt),
                _ => None,
            })
            .collect()
    }

    fn additive_ops(&self, ctx: &AdditiveExprContext<'input>) -> Vec<BinOp> {
        self.terminal_types(ctx)
            .into_iter()
            .filter_map(|ttype| match ttype {
                PLUS => Some(BinOp::Add),
                MINUS => Some(BinOp::Sub),
                _ => None,
            })
            .collect()
    }

    fn multiplicative_ops(&self, ctx: &MultiplicativeExprContext<'input>) -> Vec<BinOp> {
        self.terminal_types(ctx)
            .into_iter()
            .filter_map(|ttype| match ttype {
                STAR => Some(BinOp::Mul),
                SLASH => Some(BinOp::Div),
                PERCENT => Some(BinOp::Rem),
                _ => None,
            })
            .collect()
    }

    fn terminal_types<T>(&self, ctx: &T) -> Vec<isize>
    where
        T: ParserRuleContext<'input, Ctx = ArgonParserContextType>,
    {
        let mut tokens = Vec::new();
        for child in ctx.get_children() {
            if let Ok(tok) = child.downcast_rc::<TerminalNode<'input, ArgonParserContextType>>() {
                tokens.push(tok.symbol.get_token_type());
            }
        }
        tokens
    }

    fn span_of<T>(&self, ctx: &T) -> Span
    where
        T: ParserRuleContext<'input, Ctx = ArgonParserContextType>,
    {
        let start = ctx.start().get_start().max(0) as usize + self.offset_base;
        let stop = ctx.stop().get_stop().max(-1);
        let end = if stop < 0 {
            start
        } else {
            stop as usize + 1 + self.offset_base
        };
        Span::new(start, end)
    }

    fn span_of_token(&self, token: &TerminalNode<'input, ArgonParserContextType>) -> Span {
        let start = token.symbol.get_start().max(0) as usize + self.offset_base;
        let stop = token.symbol.get_stop().max(-1);
        let end = if stop < 0 {
            start
        } else {
            stop as usize + 1 + self.offset_base
        };
        Span::new(start, end)
    }

    fn slice(&self, span: Span) -> &'input str {
        &self.input[span.start() - self.offset_base..span.end() - self.offset_base]
    }

    fn unsupported(&mut self, span: Span, message: impl Into<String>) {
        self.errors.push(AntlrParseError {
            span,
            message: message.into(),
        });
    }
}

fn line_column_to_offset(input: &str, line: isize, column: isize) -> usize {
    let target_line = line.max(1) as usize;
    let target_column = column.max(0) as usize;

    let mut line_start = 0usize;
    let mut current_line = 1usize;
    for (idx, ch) in input.char_indices() {
        if current_line == target_line {
            line_start = idx;
            break;
        }
        if ch == '\n' {
            current_line += 1;
            line_start = idx + ch.len_utf8();
        }
    }

    if current_line < target_line {
        return input.len();
    }

    input[line_start..]
        .char_indices()
        .nth(target_column)
        .map(|(idx, _)| line_start + idx)
        .unwrap_or(input.len())
}

fn next_char_boundary(input: &str, start: usize) -> usize {
    if start >= input.len() {
        return input.len();
    }
    input[start..]
        .chars()
        .next()
        .map(|ch| start + ch.len_utf8())
        .unwrap_or(start)
}
