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

#[allow(
    clippy::all,
    dead_code,
    non_snake_case,
    non_camel_case_types,
    non_upper_case_globals,
    unused_parens
)]
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

    let _ = parser.ast();
    errors.borrow().clone()
}

pub fn parse_ast(input: ArcStr, path: PathBuf) -> Result<AnnotatedParseAst, Vec<AntlrParseError>> {
    let input_for_ast = input.clone();
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

    let tree = match parser.ast() {
        Ok(tree) => tree,
        Err(err) => {
            let collected = errors.borrow().clone();
            return Err(if collected.is_empty() {
                vec![AntlrParseError {
                    span: Span::new(offset_base, input.len()),
                    message: err.to_string(),
                }]
            } else {
                collected
            });
        }
    };
    let collected = errors.borrow().clone();
    if !collected.is_empty() {
        return Err(collected);
    }

    let mut builder = AstBuilder {
        input: normalized_input,
        offset_base,
    };
    let ast = builder.build_ast(tree.as_ref());
    Ok(AnnotatedAst::new(input_for_ast, &ast, path))
}

struct AstBuilder<'input> {
    input: &'input str,
    offset_base: usize,
}

impl<'input> AstBuilder<'input> {
    fn build_ast(&mut self, ctx: &AstContext<'input>) -> Ast<&'input str, ParseMetadata> {
        let mut decls = Vec::new();
        for decl_ctx in ctx.children_of_type::<DeclContext<'input>>() {
            if let Some(decl) = self.build_decl(decl_ctx.as_ref()) {
                decls.push(decl);
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
        } else if let Some(ctx) = ctx.constantDecl() {
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
            name: self.build_ident(ctx.ident().unwrap().as_ref()),
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
        ctx.children_of_type::<IdentContext<'input>>()
            .into_iter()
            .map(|ident| self.build_ident(ident.as_ref()))
            .collect()
    }

    fn build_struct_decl(
        &mut self,
        ctx: &StructDeclContext<'input>,
    ) -> StructDecl<&'input str, ParseMetadata> {
        StructDecl {
            name: self.build_ident(ctx.ident().unwrap().as_ref()),
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
        ctx.children_of_type::<StructFieldContext<'input>>()
            .into_iter()
            .map(|field| self.build_struct_field(field.as_ref()))
            .collect()
    }

    fn build_struct_field(
        &mut self,
        ctx: &StructFieldContext<'input>,
    ) -> StructField<&'input str, ParseMetadata> {
        StructField {
            name: self.build_ident(ctx.ident(0).unwrap().as_ref()),
            ty: self.build_ident(ctx.ident(1).unwrap().as_ref()),
            span: self.span_of(ctx),
            metadata: (),
        }
    }

    fn build_cell_decl(
        &mut self,
        ctx: &CellDeclContext<'input>,
    ) -> CellDecl<&'input str, ParseMetadata> {
        CellDecl {
            name: self.build_ident(ctx.ident().unwrap().as_ref()),
            args: self.build_arg_decls(ctx.argDecls().unwrap().as_ref()),
            scope: self.build_scope(ctx.scope().unwrap().as_ref()),
            span: self.span_of(ctx),
            metadata: (),
        }
    }

    fn build_fn_decl(&mut self, ctx: &FnDeclContext<'input>) -> FnDecl<&'input str, ParseMetadata> {
        FnDecl {
            name: self.build_ident(ctx.ident().unwrap().as_ref()),
            args: self.build_arg_decls(ctx.argDecls().unwrap().as_ref()),
            return_ty: ctx.tySpec().map(|ty| self.build_ty_spec(ty.as_ref())),
            scope: self.build_scope(ctx.scope().unwrap().as_ref()),
            span: self.span_of(ctx),
            metadata: (),
        }
    }

    fn build_const_decl(
        &mut self,
        ctx: &ConstantDeclContext<'input>,
    ) -> ConstantDecl<&'input str, ParseMetadata> {
        ConstantDecl {
            name: self.build_ident(ctx.ident(0).unwrap().as_ref()),
            ty: self.build_ident(ctx.ident(1).unwrap().as_ref()),
            value: self.build_expr(ctx.expr().unwrap().as_ref()),
            metadata: (),
        }
    }

    fn build_mod_decl(
        &mut self,
        ctx: &ModDeclContext<'input>,
    ) -> ModDecl<&'input str, ParseMetadata> {
        ModDecl {
            ident: self.build_ident(ctx.ident().unwrap().as_ref()),
            span: self.span_of(ctx),
        }
    }

    fn build_arg_decls(
        &mut self,
        ctx: &ArgDeclsContext<'input>,
    ) -> Vec<ArgDecl<&'input str, ParseMetadata>> {
        ctx.children_of_type::<ArgDeclContext<'input>>()
            .into_iter()
            .map(|arg| self.build_arg_decl(arg.as_ref()))
            .collect()
    }

    fn build_arg_decl(
        &mut self,
        ctx: &ArgDeclContext<'input>,
    ) -> ArgDecl<&'input str, ParseMetadata> {
        ArgDecl {
            name: self.build_ident(ctx.ident().unwrap().as_ref()),
            ty: self.build_ty_spec(ctx.tySpec().unwrap().as_ref()),
            metadata: (),
        }
    }

    fn build_scope(&mut self, ctx: &ScopeContext<'input>) -> Scope<&'input str, ParseMetadata> {
        let mut scope = self.build_unannotated_scope(ctx.unannotatedScope().unwrap().as_ref());
        scope.scope_annotation = ctx
            .scopeAnnotation()
            .map(|annotation| self.build_scope_annotation(annotation.as_ref()));
        scope
    }

    fn build_unannotated_scope(
        &mut self,
        ctx: &UnannotatedScopeContext<'input>,
    ) -> Scope<&'input str, ParseMetadata> {
        let mut stmts = ctx
            .statements()
            .map(|stmts| self.build_statements(stmts.as_ref()))
            .unwrap_or_default();
        let mut tail = ctx
            .nonBlockExpr()
            .map(|expr| self.build_non_block_expr(expr.as_ref()));
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

    fn build_statements(
        &mut self,
        ctx: &StatementsContext<'input>,
    ) -> Vec<Statement<&'input str, ParseMetadata>> {
        ctx.children_of_type::<StatementContext<'input>>()
            .into_iter()
            .filter_map(|stmt| self.build_statement(stmt.as_ref()))
            .collect()
    }

    fn build_statement(
        &mut self,
        ctx: &StatementContext<'input>,
    ) -> Option<Statement<&'input str, ParseMetadata>> {
        if let Some(let_binding) = ctx.letBinding() {
            Some(Statement::LetBinding(LetBinding {
                name: self.build_ident(let_binding.ident().unwrap().as_ref()),
                value: self.build_expr(let_binding.expr().unwrap().as_ref()),
                span: self.span_of(let_binding.as_ref()),
                metadata: (),
            }))
        } else if let Some(for_loop) = ctx.forLoop() {
            Some(Statement::ForLoop(self.build_for_loop(for_loop.as_ref())))
        } else if let Some(if_expr) = ctx.ifExpr() {
            Some(Statement::Expr {
                value: Expr::If(Box::new(self.build_if_expr(if_expr.as_ref()))),
                semicolon: false,
            })
        } else if let Some(match_expr) = ctx.matchExpr() {
            Some(Statement::Expr {
                value: Expr::Match(Box::new(self.build_match_expr(match_expr.as_ref()))),
                semicolon: false,
            })
        } else if let Some(scope) = ctx.scope() {
            Some(Statement::Expr {
                value: Expr::Scope(Box::new(self.build_scope(scope.as_ref()))),
                semicolon: false,
            })
        } else {
            ctx.expr().map(|expr| Statement::Expr {
                value: self.build_expr(expr.as_ref()),
                semicolon: true,
            })
        }
    }

    fn build_for_loop(
        &mut self,
        ctx: &ForLoopContext<'input>,
    ) -> ForLoop<&'input str, ParseMetadata> {
        ForLoop {
            var: self.build_ident(ctx.ident().unwrap().as_ref()),
            seq: self.build_expr(ctx.expr().unwrap().as_ref()),
            body: self.build_scope(ctx.scope().unwrap().as_ref()),
            span: self.span_of(ctx),
            metadata: (),
        }
    }

    fn build_expr(&mut self, ctx: &ExprContext<'input>) -> Expr<&'input str, ParseMetadata> {
        if let Some(if_expr) = ctx.ifExpr() {
            Expr::If(Box::new(self.build_if_expr(if_expr.as_ref())))
        } else if let Some(match_expr) = ctx.matchExpr() {
            Expr::Match(Box::new(self.build_match_expr(match_expr.as_ref())))
        } else if let Some(scope) = ctx.scope() {
            Expr::Scope(Box::new(self.build_scope(scope.as_ref())))
        } else {
            self.build_non_block_expr(ctx.nonBlockExpr().unwrap().as_ref())
        }
    }

    fn build_if_expr(&mut self, ctx: &IfExprContext<'input>) -> IfExpr<&'input str, ParseMetadata> {
        IfExpr {
            scope_annotation: ctx
                .scopeAnnotation()
                .map(|annotation| self.build_scope_annotation(annotation.as_ref())),
            cond: self.build_expr(ctx.expr().unwrap().as_ref()),
            then: self.build_scope(ctx.scope(0).unwrap().as_ref()),
            else_: self.build_scope(ctx.scope(1).unwrap().as_ref()),
            span: self.span_of(ctx),
            metadata: (),
        }
    }

    fn build_match_expr(
        &mut self,
        ctx: &MatchExprContext<'input>,
    ) -> MatchExpr<&'input str, ParseMetadata> {
        MatchExpr {
            scrutinee: self.build_expr(ctx.expr().unwrap().as_ref()),
            arms: self.build_match_arms(ctx.matchArms().unwrap().as_ref()),
            span: self.span_of(ctx),
            metadata: (),
        }
    }

    fn build_match_arms(
        &mut self,
        ctx: &MatchArmsContext<'input>,
    ) -> Vec<MatchArm<&'input str, ParseMetadata>> {
        ctx.children_of_type::<MatchArmContext<'input>>()
            .into_iter()
            .map(|arm| self.build_match_arm(arm.as_ref()))
            .collect()
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

    fn build_non_block_expr(
        &mut self,
        ctx: &NonBlockExprContext<'input>,
    ) -> Expr<&'input str, ParseMetadata> {
        if ctx.start().get_token_type() == BANG {
            let operand = self.build_non_block_expr(ctx.nonBlockExpr(0).unwrap().as_ref());
            Expr::UnaryOp(Box::new(UnaryOpExpr {
                op: UnaryOp::Not,
                operand,
                span: self.span_of(ctx),
                metadata: (),
            }))
        } else if ctx.start().get_token_type() == MINUS {
            let operand = self.build_non_block_expr(ctx.nonBlockExpr(0).unwrap().as_ref());
            Expr::UnaryOp(Box::new(UnaryOpExpr {
                op: UnaryOp::Neg,
                operand,
                span: self.span_of(ctx),
                metadata: (),
            }))
        } else if let Some(rhs) = ctx.nonBlockExpr(1) {
            let left = self.build_non_block_expr(ctx.nonBlockExpr(0).unwrap().as_ref());
            let right = self.build_non_block_expr(rhs.as_ref());
            let span = Span::new(left.span().start(), right.span().end());
            if let Some(op) = self.single_comparison_op(ctx) {
                Expr::Comparison(Box::new(ComparisonExpr {
                    op,
                    left,
                    right,
                    span,
                    metadata: (),
                }))
            } else {
                Expr::BinOp(Box::new(BinOpExpr {
                    op: self.single_bin_op(ctx),
                    left,
                    right,
                    span,
                    metadata: (),
                }))
            }
        } else if let Some(ty) = ctx.tySpec() {
            let value = self.build_non_block_expr(ctx.nonBlockExpr(0).unwrap().as_ref());
            let ty = self.build_ty_spec(ty.as_ref());
            let span = Span::new(value.span().start(), ty.span.end());
            Expr::Cast(Box::new(CastExpr {
                value,
                ty,
                span,
                metadata: (),
            }))
        } else if let Some(ident) = ctx.ident() {
            let base = self.build_non_block_expr(ctx.nonBlockExpr(0).unwrap().as_ref());
            Expr::FieldAccess(Box::new(FieldAccessExpr {
                base,
                field: self.build_ident(ident.as_ref()),
                span: self.span_of(ctx),
                metadata: (),
            }))
        } else if let Some(intlit) = ctx.intLiteral() {
            let base = self.build_non_block_expr(ctx.nonBlockExpr(0).unwrap().as_ref());
            Expr::IndexFieldAccess(Box::new(IndexFieldAccessExpr {
                base,
                field: self.build_int_literal(intlit.as_ref()),
                span: self.span_of(ctx),
                metadata: (),
            }))
        } else if let Some(expr) = ctx.expr() {
            if let Some(base) = ctx.nonBlockExpr(0) {
                Expr::Index(Box::new(IndexExpr {
                    base: self.build_non_block_expr(base.as_ref()),
                    index: self.build_expr(expr.as_ref()),
                    span: self.span_of(ctx),
                    metadata: (),
                }))
            } else {
                self.build_expr(expr.as_ref())
            }
        } else if let Some(nil) = ctx.nilLiteral() {
            Expr::Nil(NilLiteral {
                span: self.span_of(nil.as_ref()),
            })
        } else if let Some(seq_nil) = ctx.seqNilLiteral() {
            Expr::SeqNil(SeqNilLiteral {
                span: self.span_of(seq_nil.as_ref()),
            })
        } else if let Some(tuple) = ctx.tupleExpr() {
            Expr::Tuple(self.build_tuple_expr(tuple.as_ref()))
        } else if let Some(call) = ctx.callExpr() {
            Expr::Call(self.build_call_expr(call.as_ref()))
        } else if let Some(path) = ctx.identPath() {
            Expr::IdentPath(self.build_ident_path(path.as_ref()))
        } else if let Some(literal) = ctx.literal() {
            self.build_literal(literal.as_ref())
        } else {
            Expr::Emit(Box::new(EmitExpr {
                value: self.build_non_block_expr(ctx.nonBlockExpr(0).unwrap().as_ref()),
                span: self.span_of(ctx),
                metadata: (),
            }))
        }
    }

    fn build_tuple_expr(
        &mut self,
        ctx: &TupleExprContext<'input>,
    ) -> TupleExpr<&'input str, ParseMetadata> {
        let items = ctx
            .tupleExprList()
            .unwrap_or_else(|| unreachable!("tupleExpr always contains tupleExprList"))
            .children_of_type::<ExprContext<'input>>()
            .into_iter()
            .map(|expr| self.build_expr(expr.as_ref()))
            .collect();
        TupleExpr {
            items,
            span: self.span_of(ctx),
            metadata: (),
        }
    }

    fn build_call_expr(
        &mut self,
        ctx: &CallExprContext<'input>,
    ) -> CallExpr<&'input str, ParseMetadata> {
        CallExpr {
            scope_annotation: ctx
                .scopeAnnotation()
                .map(|annotation| self.build_scope_annotation(annotation.as_ref())),
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
        crate::ast::Args {
            posargs: ctx
                .posArgList()
                .map(|posargs| self.build_pos_arg_list(posargs.as_ref()))
                .unwrap_or_default(),
            kwargs: ctx
                .kwArgList()
                .map(|kwargs| self.build_kw_arg_list(kwargs.as_ref()))
                .unwrap_or_default(),
            span: self.span_of(ctx),
            metadata: (),
        }
    }

    fn build_kw_arg_value(
        &mut self,
        ctx: &KwArgValueContext<'input>,
    ) -> KwArgValue<&'input str, ParseMetadata> {
        KwArgValue {
            name: self.build_ident(ctx.ident().unwrap().as_ref()),
            value: self.build_expr(ctx.expr().unwrap().as_ref()),
            span: self.span_of(ctx),
            metadata: (),
        }
    }

    fn build_kw_arg_list(
        &mut self,
        ctx: &KwArgListContext<'input>,
    ) -> Vec<KwArgValue<&'input str, ParseMetadata>> {
        ctx.children_of_type::<KwArgValueContext<'input>>()
            .into_iter()
            .map(|kwarg| self.build_kw_arg_value(kwarg.as_ref()))
            .collect()
    }

    fn build_pos_arg_list(
        &mut self,
        ctx: &PosArgListContext<'input>,
    ) -> Vec<Expr<&'input str, ParseMetadata>> {
        ctx.children_of_type::<ExprContext<'input>>()
            .into_iter()
            .map(|expr| self.build_expr(expr.as_ref()))
            .collect()
    }

    fn build_ident_path(
        &mut self,
        ctx: &IdentPathContext<'input>,
    ) -> IdentPath<&'input str, ParseMetadata> {
        let path = ctx
            .children_of_type::<IdentContext<'input>>()
            .into_iter()
            .map(|ident| self.build_ident(ident.as_ref()))
            .collect();
        IdentPath {
            path,
            metadata: (),
            span: self.span_of(ctx),
        }
    }

    fn build_ty_spec(&mut self, ctx: &TySpecContext<'input>) -> TySpec<&'input str, ParseMetadata> {
        let kind = if let Some(ident) = ctx.ident() {
            TySpecKind::Ident(self.build_ident(ident.as_ref()))
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
        } else if let Some(int_ctx) = ctx.intLiteral() {
            Expr::IntLiteral(self.build_int_literal(int_ctx.as_ref()))
        } else if let Some(string_ctx) = ctx.stringLiteral() {
            let token = string_ctx.STRLIT().unwrap();
            let span = self.span_of_token(&token);
            Expr::StringLiteral(StringLiteral {
                span,
                value: self.slice(span).trim_matches('"'),
            })
        } else {
            Expr::BoolLiteral(self.build_bool_literal(ctx.boolLiteral().unwrap().as_ref()))
        }
    }

    fn build_float_literal(&mut self, ctx: &FloatLiteralContext<'input>) -> FloatLiteral {
        let span = self.span_of(ctx);
        FloatLiteral {
            span,
            value: self.slice(span).parse::<f64>().unwrap_or_default(),
        }
    }

    fn build_bool_literal(&self, ctx: &BoolLiteralContext<'input>) -> BoolLiteral {
        BoolLiteral {
            span: self.span_of(ctx),
            value: ctx.TRUE().is_some(),
        }
    }

    fn build_scope_annotation(
        &mut self,
        ctx: &ScopeAnnotationContext<'input>,
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

    fn build_ident(&self, ctx: &IdentContext<'input>) -> Ident<&'input str, ParseMetadata> {
        self.ident_from_token(&ctx.IDENT().unwrap())
    }

    fn build_int_literal(&self, ctx: &IntLiteralContext<'input>) -> IntLiteral {
        self.int_literal_from_token(&ctx.INTLIT().unwrap())
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

    fn single_comparison_op(&self, ctx: &NonBlockExprContext<'input>) -> Option<ComparisonOp> {
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
            .next()
    }

    fn single_bin_op(&self, ctx: &NonBlockExprContext<'input>) -> BinOp {
        self.terminal_types(ctx)
            .into_iter()
            .find_map(|ttype| match ttype {
                PLUS => Some(BinOp::Add),
                MINUS => Some(BinOp::Sub),
                STAR => Some(BinOp::Mul),
                SLASH => Some(BinOp::Div),
                PERCENT => Some(BinOp::Rem),
                _ => None,
            })
            .unwrap_or_else(|| {
                unreachable!("binary nonBlockExpr must contain an arithmetic operator")
            })
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
