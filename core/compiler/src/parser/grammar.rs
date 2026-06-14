//! Hand-written recursive-descent + Pratt parser for Argon.
//!
//! Builds `Ast<&'a str, ParseMetadata>` directly from the token stream in a
//! single pass — no intermediate concrete syntax tree. Identifier and string
//! text is borrowed straight from the source (`&'a str`); every node records a
//! byte-offset `cfgrammar::Span` that indexes the original (untrimmed) input,
//! matching the spans the ANTLR integration produced (the annotation pass later
//! re-slices names/values from these spans).
//!
//! Expression precedence/associativity mirrors the ANTLR `expr` rule exactly
//! (validated against the generated `expr_rec`/`precpred`): prefix unary binds
//! tightest for its operand; the suffix cluster (`.field`, `.idx`, `[]`, `!`,
//! `as`) binds tighter than the binary operators; `* / %` > `+ -` > comparisons;
//! all binary operators are left-associative.

use cfgrammar::Span;

use crate::ast::{
    ArgDecl, Args, Ast, BinOp, BinOpExpr, BoolLiteral, CallExpr, CastExpr, CellDecl,
    ComparisonExpr, ComparisonOp, ConstantDecl, Decl, EmitExpr, EnumDecl, Expr, FieldAccessExpr,
    FloatLiteral, FnDecl, ForLoop, Ident, IdentPath, IfExpr, IndexExpr, IndexFieldAccessExpr,
    IntLiteral, KwArgValue, LetBinding, MatchArm, MatchExpr, ModDecl, NilLiteral, Scope,
    SeqNilLiteral, Statement, StringLiteral, StructDecl, StructField, TupleExpr, TySpec,
    TySpecKind, UnaryOp, UnaryOpExpr,
};
use crate::parse::ParseMetadata;

use super::ParseError;
use super::lexer::Lexer;
use super::token::{Token, TokenKind};

type Md = ParseMetadata;

// Binding powers for the Pratt loop. Higher binds tighter. The numbers only
// need to preserve the ANTLR ordering; the absolute values are arbitrary.
//   comparisons: 1/2   additive: 3/4   multiplicative: 5/6
//   suffix cluster: 7   prefix unary operand: 9
const SUFFIX_BP: u8 = 7;
const PREFIX_BP: u8 = 9;

/// Recursion-depth guard for pathological nesting (real programs are shallow).
const MAX_DEPTH: u32 = 256;

/// Left/right binding power of an infix operator token, or `None` if the token
/// is not an infix operator.
#[inline]
fn infix_bp(k: TokenKind) -> Option<(u8, u8)> {
    use TokenKind::*;
    Some(match k {
        EqEq | Neq | Geq | Gt | Leq | Lt => (1, 2),
        Plus | Minus => (3, 4),
        Star | Slash | Percent => (5, 6),
        _ => return None,
    })
}

pub struct Parser<'a> {
    src: &'a str,
    base: usize,
    lexer: Lexer<'a>,
    cur: Token,
    nxt: Token,
    /// End offset (original coords) of the most recently consumed token. Used to
    /// close composite-node spans at the end of the last token they cover.
    prev_end: u32,
    /// Monotonic count of consumed tokens, used by loop progress guards.
    ntok: u64,
    depth: u32,
    pub errors: Vec<ParseError>,
    /// Start offset of the last reported error, to suppress duplicate errors at
    /// the same position (cascades from repeated `expect` failures).
    last_error_pos: Option<u32>,
}

impl<'a> Parser<'a> {
    pub fn new(src: &'a str, offset_base: usize) -> Self {
        let mut lexer = Lexer::new(src, offset_base);
        let cur = lexer.next_token();
        let nxt = lexer.next_token();
        Self {
            src,
            base: offset_base,
            lexer,
            prev_end: cur.start,
            cur,
            nxt,
            ntok: 0,
            depth: 0,
            errors: Vec::new(),
            last_error_pos: None,
        }
    }

    // ------------------------------------------------------------------
    // Token plumbing
    // ------------------------------------------------------------------

    #[inline]
    fn at(&self, k: TokenKind) -> bool {
        self.cur.kind == k
    }

    fn bump(&mut self) -> Token {
        let t = self.cur;
        self.prev_end = t.end;
        self.cur = self.nxt;
        self.nxt = self.lexer.next_token();
        self.ntok += 1;
        t
    }

    fn eat(&mut self, k: TokenKind) -> bool {
        if self.cur.kind == k {
            self.bump();
            true
        } else {
            false
        }
    }

    fn expect(&mut self, k: TokenKind) -> Token {
        if self.cur.kind == k {
            self.bump()
        } else {
            let t = self.cur;
            self.error_at(
                self.span(t),
                format!("expected {}, found {}", k.describe(), t.kind.describe()),
            );
            // Do not consume; return a zero-width synthetic token so callers can
            // still derive a (degraded) span. Loop progress guards prevent stalls.
            Token::new(k, t.start, t.start)
        }
    }

    #[inline]
    fn span(&self, t: Token) -> Span {
        Span::new(t.start as usize, t.end as usize)
    }

    /// Slice the source for token `t` (offsets are in original coords; subtract
    /// `base` to index the trimmed buffer the lexer scanned).
    #[inline]
    fn slice_tok(&self, t: Token) -> &'a str {
        &self.src[t.start as usize - self.base..t.end as usize - self.base]
    }

    #[inline]
    fn slice_span(&self, span: Span) -> &'a str {
        &self.src[span.start() - self.base..span.end() - self.base]
    }

    fn error_at(&mut self, span: Span, message: String) {
        let pos = span.start() as u32;
        if self.last_error_pos == Some(pos) {
            return;
        }
        self.last_error_pos = Some(pos);
        self.errors.push(ParseError { span, message });
    }

    /// Collect the accumulated errors, guaranteeing at least one is present.
    pub fn finish_errors(mut self, offset_base: usize, input_len: usize) -> Vec<ParseError> {
        if self.errors.is_empty() {
            self.errors.push(ParseError {
                span: Span::new(offset_base, input_len),
                message: "syntax error".to_string(),
            });
        }
        self.errors
    }

    // ------------------------------------------------------------------
    // Entry points
    // ------------------------------------------------------------------

    /// `ast : decl* EOF`
    pub fn parse_root(&mut self) -> Ast<&'a str, Md> {
        let lo = self.cur.start as usize;
        let mut decls = Vec::new();
        while !self.at(TokenKind::Eof) {
            let mark = self.ntok;
            self.last_error_pos = None;
            match self.parse_decl() {
                Some(decl) => decls.push(decl),
                None => {
                    self.error_at(
                        self.span(self.cur),
                        format!("expected a declaration, found {}", self.cur.kind.describe()),
                    );
                    self.recover_to_decl();
                }
            }
            if self.ntok == mark {
                self.bump();
            }
        }
        // Like ANTLR's `ast : decl* EOF` context, the root span runs to the EOF
        // token, i.e. the end of the (untrimmed) input — `src` is the trimmed
        // buffer, so `src.len() + base` is the original length.
        let end = self.src.len() + self.base;
        Ast {
            decls,
            span: Span::new(lo, end),
        }
    }

    /// `callExpr` as a standalone entry (used by `parse_cell`). Returns `None`
    /// (with an error recorded) if the input does not start a call.
    pub fn parse_cell_entry(&mut self) -> Option<CallExpr<&'a str, Md>> {
        let expr = self.parse_expr(0);
        match expr {
            Expr::Call(call) => Some(call),
            other => {
                self.error_at(
                    self.span(self.cur),
                    "expected a cell invocation".to_string(),
                );
                let _ = other;
                None
            }
        }
    }

    fn recover_to_decl(&mut self) {
        use TokenKind::*;
        while !self.at(Eof) {
            match self.cur.kind {
                KwEnum | KwStruct | KwCell | KwFn | KwConst | KwMod => break,
                _ => {
                    self.bump();
                }
            }
        }
    }

    // ------------------------------------------------------------------
    // Declarations
    // ------------------------------------------------------------------

    fn parse_decl(&mut self) -> Option<Decl<&'a str, Md>> {
        use TokenKind::*;
        Some(match self.cur.kind {
            KwEnum => Decl::Enum(self.parse_enum_decl()),
            KwStruct => Decl::Struct(self.parse_struct_decl()),
            KwCell => Decl::Cell(self.parse_cell_decl()),
            KwFn => Decl::Fn(self.parse_fn_decl()),
            KwConst => Decl::Constant(self.parse_const_decl()),
            KwMod => Decl::Mod(self.parse_mod_decl()),
            _ => return None,
        })
    }

    /// `enumDecl : ENUM ident LBRACE enumVariants RBRACE`
    fn parse_enum_decl(&mut self) -> EnumDecl<&'a str, Md> {
        self.expect(TokenKind::KwEnum);
        let name = self.ident();
        self.expect(TokenKind::LBrace);
        let variants = self.parse_ident_list();
        self.expect(TokenKind::RBrace);
        EnumDecl {
            name,
            variants,
            metadata: (),
        }
    }

    /// `structDecl : STRUCT ident LBRACE structFields RBRACE`
    fn parse_struct_decl(&mut self) -> StructDecl<&'a str, Md> {
        let lo = self.cur.start;
        self.expect(TokenKind::KwStruct);
        let name = self.ident();
        self.expect(TokenKind::LBrace);
        let mut fields = Vec::new();
        while !self.at(TokenKind::RBrace) && !self.at(TokenKind::Eof) {
            let mark = self.ntok;
            fields.push(self.parse_struct_field());
            if !self.eat(TokenKind::Comma) {
                break;
            }
            if self.ntok == mark {
                self.bump();
            }
        }
        self.expect(TokenKind::RBrace);
        StructDecl {
            name,
            fields,
            span: Span::new(lo as usize, self.prev_end as usize),
            metadata: (),
        }
    }

    /// `structField : ident COLON ident`
    fn parse_struct_field(&mut self) -> StructField<&'a str, Md> {
        let lo = self.cur.start;
        let name = self.ident();
        self.expect(TokenKind::Colon);
        let ty = self.ident();
        StructField {
            name,
            ty,
            span: Span::new(lo as usize, self.prev_end as usize),
            metadata: (),
        }
    }

    /// `constantDecl : CONST ident COLON ident EQ expr SEMI`
    fn parse_const_decl(&mut self) -> ConstantDecl<&'a str, Md> {
        self.expect(TokenKind::KwConst);
        let name = self.ident();
        self.expect(TokenKind::Colon);
        let ty = self.ident();
        self.expect(TokenKind::Eq);
        let value = self.parse_expr(0);
        self.expect(TokenKind::Semi);
        ConstantDecl {
            name,
            ty,
            value,
            metadata: (),
        }
    }

    /// `modDecl : MOD ident SEMI`
    fn parse_mod_decl(&mut self) -> ModDecl<&'a str, Md> {
        let lo = self.cur.start;
        self.expect(TokenKind::KwMod);
        let ident = self.ident();
        self.expect(TokenKind::Semi);
        ModDecl {
            ident,
            span: Span::new(lo as usize, self.prev_end as usize),
        }
    }

    /// `cellDecl : CELL ident LPAREN argDecls RPAREN scope`
    fn parse_cell_decl(&mut self) -> CellDecl<&'a str, Md> {
        let lo = self.cur.start;
        self.expect(TokenKind::KwCell);
        let name = self.ident();
        self.expect(TokenKind::LParen);
        let args = self.parse_arg_decls();
        self.expect(TokenKind::RParen);
        let scope = self.parse_scope();
        CellDecl {
            name,
            args,
            scope,
            span: Span::new(lo as usize, self.prev_end as usize),
            metadata: (),
        }
    }

    /// `fnDecl : FN ident LPAREN argDecls RPAREN (ARROW tySpec)? scope`
    fn parse_fn_decl(&mut self) -> FnDecl<&'a str, Md> {
        let lo = self.cur.start;
        self.expect(TokenKind::KwFn);
        let name = self.ident();
        self.expect(TokenKind::LParen);
        let args = self.parse_arg_decls();
        self.expect(TokenKind::RParen);
        let return_ty = if self.at(TokenKind::Arrow) {
            self.bump();
            Some(self.parse_ty_spec())
        } else {
            None
        };
        let scope = self.parse_scope();
        FnDecl {
            name,
            args,
            return_ty,
            scope,
            span: Span::new(lo as usize, self.prev_end as usize),
            metadata: (),
        }
    }

    /// `argDecls : (argDecl (COMMA argDecl)* COMMA?)?`
    fn parse_arg_decls(&mut self) -> Vec<ArgDecl<&'a str, Md>> {
        let mut v = Vec::new();
        while !self.at(TokenKind::RParen) && !self.at(TokenKind::Eof) {
            let mark = self.ntok;
            v.push(self.parse_arg_decl());
            if !self.eat(TokenKind::Comma) {
                break;
            }
            if self.ntok == mark {
                self.bump();
            }
        }
        v
    }

    /// `argDecl : ident COLON tySpec`
    fn parse_arg_decl(&mut self) -> ArgDecl<&'a str, Md> {
        let name = self.ident();
        self.expect(TokenKind::Colon);
        let ty = self.parse_ty_spec();
        ArgDecl {
            name,
            ty,
            metadata: (),
        }
    }

    /// `enumVariants : (ident (COMMA ident)* COMMA?)?`
    fn parse_ident_list(&mut self) -> Vec<Ident<&'a str, Md>> {
        let mut v = Vec::new();
        while !self.at(TokenKind::RBrace) && !self.at(TokenKind::Eof) {
            let mark = self.ntok;
            v.push(self.ident());
            if !self.eat(TokenKind::Comma) {
                break;
            }
            if self.ntok == mark {
                self.bump();
            }
        }
        v
    }

    /// `tySpec : ident | LBRACK tySpec RBRACK | LPAREN tySpecList RPAREN`
    fn parse_ty_spec(&mut self) -> TySpec<&'a str, Md> {
        let lo = self.cur.start;
        let kind = match self.cur.kind {
            TokenKind::LBrack => {
                self.bump();
                let inner = self.parse_ty_spec();
                self.expect(TokenKind::RBrack);
                TySpecKind::Seq(Box::new(inner))
            }
            TokenKind::LParen => {
                self.bump();
                let mut list = Vec::new();
                if !self.at(TokenKind::RParen) {
                    list.push(self.parse_ty_spec());
                    while self.eat(TokenKind::Comma) {
                        let mark = self.ntok;
                        list.push(self.parse_ty_spec());
                        if self.ntok == mark {
                            break;
                        }
                    }
                }
                self.expect(TokenKind::RParen);
                TySpecKind::Tuple(list)
            }
            TokenKind::Ident => TySpecKind::Ident(self.ident()),
            _ => {
                self.error_at(
                    self.span(self.cur),
                    format!("expected a type, found {}", self.cur.kind.describe()),
                );
                TySpecKind::Tuple(Vec::new())
            }
        };
        TySpec {
            kind,
            span: Span::new(lo as usize, self.prev_end as usize),
        }
    }

    // ------------------------------------------------------------------
    // Scopes & statements
    // ------------------------------------------------------------------

    /// `scope : scopeAnnotation? unannotatedScope`
    fn parse_scope(&mut self) -> Scope<&'a str, Md> {
        let ann = if self.at(TokenKind::Annotation) {
            let t = self.bump();
            Some(self.annotation_ident(t))
        } else {
            None
        };
        self.parse_unannotated_scope(ann)
    }

    /// `unannotatedScope : LBRACE statements (expr)? RBRACE`
    ///
    /// `Scope.span` covers only the braces (the annotation, if any, has its own
    /// span and is excluded), matching the ANTLR `AstBuilder`.
    fn parse_unannotated_scope(&mut self, ann: Option<Ident<&'a str, Md>>) -> Scope<&'a str, Md> {
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            self.depth -= 1;
            self.error_at(self.span(self.cur), "nesting too deep".to_string());
            let lo = self.cur.start;
            return Scope {
                scope_annotation: ann,
                span: Span::new(lo as usize, lo as usize),
                stmts: Vec::new(),
                tail: None,
                metadata: (),
            };
        }
        let lb = self.expect(TokenKind::LBrace);
        let lo = lb.start;
        let mut stmts = Vec::new();
        let mut tail: Option<Expr<&'a str, Md>> = None;

        while !self.at(TokenKind::RBrace) && !self.at(TokenKind::Eof) {
            let mark = self.ntok;
            self.last_error_pos = None;
            match self.cur.kind {
                TokenKind::KwLet => {
                    let lb = self.parse_let_binding();
                    self.expect(TokenKind::Semi);
                    stmts.push(Statement::LetBinding(lb));
                }
                TokenKind::KwFor => {
                    stmts.push(Statement::ForLoop(self.parse_for_loop()));
                }
                _ => {
                    // Everything else is an expression. `if`/`match`/`{...}` are
                    // expression primaries, so the Pratt parser also extends
                    // them with trailing operators (e.g. `if c {a} else {b} + 1`).
                    let e = self.parse_expr(0);
                    let is_block = matches!(e, Expr::If(_) | Expr::Match(_) | Expr::Scope(_));
                    if self.eat(TokenKind::Semi) {
                        stmts.push(Statement::Expr {
                            value: e,
                            semicolon: true,
                        });
                    } else if self.at(TokenKind::RBrace) || self.at(TokenKind::Eof) {
                        tail = Some(e);
                    } else if is_block {
                        // A bare block-expr used as a statement; more follow.
                        stmts.push(Statement::Expr {
                            value: e,
                            semicolon: false,
                        });
                    } else {
                        self.error_at(
                            self.span(self.cur),
                            format!("expected ';', found {}", self.cur.kind.describe()),
                        );
                        stmts.push(Statement::Expr {
                            value: e,
                            semicolon: true,
                        });
                    }
                }
            }
            if tail.is_some() {
                break;
            }
            if self.ntok == mark {
                self.bump();
            }
        }
        self.expect(TokenKind::RBrace);

        // Tail fixup: a trailing un-semicoloned block-expr statement becomes the
        // scope's tail (mirrors AstBuilder::build_unannotated_scope).
        if tail.is_none()
            && let Some(Statement::Expr {
                semicolon: false, ..
            }) = stmts.last()
            && let Some(Statement::Expr { value, .. }) = stmts.pop()
        {
            tail = Some(value);
        }

        self.depth -= 1;
        Scope {
            scope_annotation: ann,
            span: Span::new(lo as usize, self.prev_end as usize),
            stmts,
            tail,
            metadata: (),
        }
    }

    /// `letBinding : LET ident EQ expr` (span excludes the trailing SEMI, which
    /// belongs to the enclosing `statement`).
    fn parse_let_binding(&mut self) -> LetBinding<&'a str, Md> {
        let lo = self.cur.start;
        self.expect(TokenKind::KwLet);
        let name = self.ident();
        self.expect(TokenKind::Eq);
        let value = self.parse_expr(0);
        LetBinding {
            name,
            value,
            metadata: (),
            span: Span::new(lo as usize, self.prev_end as usize),
        }
    }

    /// `forLoop : FOR ident IN expr scope`
    fn parse_for_loop(&mut self) -> ForLoop<&'a str, Md> {
        let lo = self.cur.start;
        self.expect(TokenKind::KwFor);
        let var = self.ident();
        self.expect(TokenKind::KwIn);
        let seq = self.parse_expr(0);
        let body = self.parse_scope();
        ForLoop {
            var,
            seq,
            body,
            metadata: (),
            span: Span::new(lo as usize, self.prev_end as usize),
        }
    }

    /// `ifExpr : scopeAnnotation? IF expr scope ELSE scope`. `lo` is the start
    /// offset of the first token (annotation if present, else `if`).
    fn parse_if(&mut self, ann: Option<Ident<&'a str, Md>>, lo: u32) -> IfExpr<&'a str, Md> {
        self.expect(TokenKind::KwIf);
        let cond = self.parse_expr(0);
        let then = self.parse_scope();
        self.expect(TokenKind::KwElse);
        let else_ = self.parse_scope();
        IfExpr {
            scope_annotation: ann,
            cond,
            then,
            else_,
            span: Span::new(lo as usize, self.prev_end as usize),
            metadata: (),
        }
    }

    /// `matchExpr : MATCH expr LBRACE matchArms RBRACE`
    fn parse_match(&mut self) -> MatchExpr<&'a str, Md> {
        let lo = self.cur.start;
        self.expect(TokenKind::KwMatch);
        let scrutinee = self.parse_expr(0);
        self.expect(TokenKind::LBrace);
        let mut arms = Vec::new();
        while !self.at(TokenKind::RBrace) && !self.at(TokenKind::Eof) {
            let mark = self.ntok;
            arms.push(self.parse_match_arm());
            if self.ntok == mark {
                self.bump();
            }
        }
        self.expect(TokenKind::RBrace);
        MatchExpr {
            scrutinee,
            arms,
            span: Span::new(lo as usize, self.prev_end as usize),
            metadata: (),
        }
    }

    /// `matchArm : identPath FAT_ARROW expr COMMA` (span includes the comma).
    fn parse_match_arm(&mut self) -> MatchArm<&'a str, Md> {
        let lo = self.cur.start;
        let pattern = self.parse_ident_path();
        self.expect(TokenKind::FatArrow);
        let expr = self.parse_expr(0);
        self.expect(TokenKind::Comma);
        MatchArm {
            pattern,
            expr,
            span: Span::new(lo as usize, self.prev_end as usize),
        }
    }

    // ------------------------------------------------------------------
    // Expressions (Pratt)
    // ------------------------------------------------------------------

    fn parse_expr(&mut self, min_bp: u8) -> Expr<&'a str, Md> {
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            self.depth -= 1;
            self.error_at(
                self.span(self.cur),
                "expression nesting too deep".to_string(),
            );
            let p = self.cur.start as usize;
            return Expr::Nil(NilLiteral {
                span: Span::new(p, p),
            });
        }
        // Lexical start of this expression (the first token). Composite-node
        // spans start here, not at `lhs.span().start()`: a parenthesized
        // operand is unwrapped to its inner node (whose span excludes the
        // parens), but ANTLR spans the enclosing operator from the `(`.
        let lhs_start = self.cur.start;
        let mut lhs = self.parse_prefix();

        loop {
            let k = self.cur.kind;
            // Suffix cluster (binds tightest).
            if SUFFIX_BP >= min_bp
                && matches!(
                    k,
                    TokenKind::Dot | TokenKind::LBrack | TokenKind::Bang | TokenKind::KwAs
                )
            {
                lhs = self.parse_suffix(lhs, lhs_start);
                continue;
            }
            // Infix binary / comparison.
            if let Some((l_bp, r_bp)) = infix_bp(k) {
                if l_bp < min_bp {
                    break;
                }
                self.bump();
                let rhs = self.parse_expr(r_bp);
                lhs = self.make_infix(k, lhs, rhs, lhs_start);
                continue;
            }
            break;
        }

        self.depth -= 1;
        lhs
    }

    fn parse_prefix(&mut self) -> Expr<&'a str, Md> {
        match self.cur.kind {
            TokenKind::Bang | TokenKind::Minus => {
                let op_tok = self.bump();
                let op = if op_tok.kind == TokenKind::Bang {
                    UnaryOp::Not
                } else {
                    UnaryOp::Neg
                };
                let operand = self.parse_expr(PREFIX_BP);
                Expr::UnaryOp(Box::new(UnaryOpExpr {
                    op,
                    operand,
                    span: Span::new(op_tok.start as usize, self.prev_end as usize),
                    metadata: (),
                }))
            }
            _ => self.parse_primary(),
        }
    }

    fn make_infix(
        &self,
        k: TokenKind,
        left: Expr<&'a str, Md>,
        right: Expr<&'a str, Md>,
        lhs_start: u32,
    ) -> Expr<&'a str, Md> {
        let span = Span::new(lhs_start as usize, self.prev_end as usize);
        match k {
            TokenKind::Star
            | TokenKind::Slash
            | TokenKind::Percent
            | TokenKind::Plus
            | TokenKind::Minus => {
                let op = match k {
                    TokenKind::Star => BinOp::Mul,
                    TokenKind::Slash => BinOp::Div,
                    TokenKind::Percent => BinOp::Rem,
                    TokenKind::Plus => BinOp::Add,
                    _ => BinOp::Sub,
                };
                Expr::BinOp(Box::new(BinOpExpr {
                    op,
                    left,
                    right,
                    span,
                    metadata: (),
                }))
            }
            _ => {
                let op = match k {
                    TokenKind::EqEq => ComparisonOp::Eq,
                    TokenKind::Neq => ComparisonOp::Ne,
                    TokenKind::Geq => ComparisonOp::Geq,
                    TokenKind::Gt => ComparisonOp::Gt,
                    TokenKind::Leq => ComparisonOp::Leq,
                    _ => ComparisonOp::Lt,
                };
                Expr::Comparison(Box::new(ComparisonExpr {
                    op,
                    left,
                    right,
                    span,
                    metadata: (),
                }))
            }
        }
    }

    /// Apply one suffix (`.field`, `.idx`, `[index]`, postfix `!`, `as ty`).
    /// `lhs_start` is the lexical start of the whole expression (see `parse_expr`).
    fn parse_suffix(&mut self, lhs: Expr<&'a str, Md>, lhs_start: u32) -> Expr<&'a str, Md> {
        let start = lhs_start as usize;
        match self.cur.kind {
            TokenKind::Dot => {
                self.bump();
                match self.cur.kind {
                    TokenKind::Ident => {
                        let field = self.ident();
                        Expr::FieldAccess(Box::new(FieldAccessExpr {
                            base: lhs,
                            field,
                            span: Span::new(start, self.prev_end as usize),
                            metadata: (),
                        }))
                    }
                    TokenKind::IntLit => {
                        let t = self.bump();
                        let field = IntLiteral {
                            span: self.span(t),
                            value: self.slice_tok(t).parse::<i64>().unwrap_or_default(),
                        };
                        Expr::IndexFieldAccess(Box::new(IndexFieldAccessExpr {
                            base: lhs,
                            field,
                            span: Span::new(start, self.prev_end as usize),
                            metadata: (),
                        }))
                    }
                    _ => {
                        self.error_at(
                            self.span(self.cur),
                            format!(
                                "expected field name or index, found {}",
                                self.cur.kind.describe()
                            ),
                        );
                        lhs
                    }
                }
            }
            TokenKind::LBrack => {
                self.bump();
                let index = self.parse_expr(0);
                self.expect(TokenKind::RBrack);
                Expr::Index(Box::new(IndexExpr {
                    base: lhs,
                    index,
                    span: Span::new(start, self.prev_end as usize),
                    metadata: (),
                }))
            }
            TokenKind::Bang => {
                let t = self.bump();
                Expr::Emit(Box::new(EmitExpr {
                    value: lhs,
                    span: Span::new(start, t.end as usize),
                    metadata: (),
                }))
            }
            TokenKind::KwAs => {
                self.bump();
                let ty = self.parse_ty_spec();
                Expr::Cast(Box::new(CastExpr {
                    value: lhs,
                    ty,
                    span: Span::new(start, self.prev_end as usize),
                    metadata: (),
                }))
            }
            _ => unreachable!("parse_suffix called on non-suffix token"),
        }
    }

    fn parse_primary(&mut self) -> Expr<&'a str, Md> {
        match self.cur.kind {
            TokenKind::LParen => self.parse_paren(),
            TokenKind::LBrack => self.parse_seq_nil(),
            TokenKind::KwIf => {
                let lo = self.cur.start;
                Expr::If(Box::new(self.parse_if(None, lo)))
            }
            TokenKind::KwMatch => Expr::Match(Box::new(self.parse_match())),
            TokenKind::LBrace => Expr::Scope(Box::new(self.parse_unannotated_scope(None))),
            TokenKind::Annotation => self.parse_annotated_primary(),
            TokenKind::Ident => {
                let path = self.parse_ident_path();
                if self.at(TokenKind::LParen) {
                    let lo = path.span.start() as u32;
                    Expr::Call(self.finish_call(None, lo, path))
                } else {
                    Expr::IdentPath(path)
                }
            }
            TokenKind::IntLit => self.parse_int_or_float(),
            TokenKind::StrLit => self.parse_string_literal(),
            TokenKind::KwTrue | TokenKind::KwFalse => {
                let t = self.bump();
                Expr::BoolLiteral(BoolLiteral {
                    span: self.span(t),
                    value: t.kind == TokenKind::KwTrue,
                })
            }
            _ => {
                let t = self.cur;
                self.error_at(
                    self.span(t),
                    format!("expected an expression, found {}", t.kind.describe()),
                );
                Expr::Nil(NilLiteral {
                    span: Span::new(t.start as usize, t.start as usize),
                })
            }
        }
    }

    /// An expression starting with `#name`: either an annotated `if`, an
    /// annotated scope `{...}`, or an annotated call `path(...)`.
    fn parse_annotated_primary(&mut self) -> Expr<&'a str, Md> {
        let ann_tok = self.bump();
        let lo = ann_tok.start;
        let ann = self.annotation_ident(ann_tok);
        match self.cur.kind {
            TokenKind::KwIf => Expr::If(Box::new(self.parse_if(Some(ann), lo))),
            TokenKind::LBrace => Expr::Scope(Box::new(self.parse_unannotated_scope(Some(ann)))),
            TokenKind::Ident => {
                let path = self.parse_ident_path();
                if self.at(TokenKind::LParen) {
                    Expr::Call(self.finish_call(Some(ann), lo, path))
                } else {
                    self.error_at(
                        self.span(self.cur),
                        "annotation is only allowed before a call, `if`, or `{` block".to_string(),
                    );
                    Expr::IdentPath(path)
                }
            }
            _ => {
                self.error_at(
                    self.span(self.cur),
                    "annotation is only allowed before a call, `if`, or `{` block".to_string(),
                );
                Expr::Nil(NilLiteral {
                    span: Span::new(lo as usize, lo as usize),
                })
            }
        }
    }

    /// `( )` nil, `( expr )` parenthesized group (unwrapped), or
    /// `( expr , (expr ,)* )` tuple (a comma after every element is required).
    fn parse_paren(&mut self) -> Expr<&'a str, Md> {
        let lp = self.bump();
        if self.at(TokenKind::RParen) {
            let rp = self.bump();
            return Expr::Nil(NilLiteral {
                span: Span::new(lp.start as usize, rp.end as usize),
            });
        }
        let first = self.parse_expr(0);
        if self.at(TokenKind::RParen) {
            self.bump();
            // Parenthesized group: no node, keep the inner expr's own span.
            return first;
        }
        // Tuple. `tupleExprList : expr COMMA (expr COMMA)*`.
        self.expect(TokenKind::Comma);
        let mut items = vec![first];
        while !self.at(TokenKind::RParen) && !self.at(TokenKind::Eof) {
            let mark = self.ntok;
            items.push(self.parse_expr(0));
            self.expect(TokenKind::Comma);
            if self.ntok == mark {
                self.bump();
            }
        }
        self.expect(TokenKind::RParen);
        Expr::Tuple(TupleExpr {
            items,
            span: Span::new(lp.start as usize, self.prev_end as usize),
            metadata: (),
        })
    }

    /// `seqNilLiteral : LBRACK RBRACK` (a non-empty `[...]` is not an expression).
    fn parse_seq_nil(&mut self) -> Expr<&'a str, Md> {
        let lb = self.bump();
        let rb = self.expect(TokenKind::RBrack);
        Expr::SeqNil(SeqNilLiteral {
            span: Span::new(lb.start as usize, rb.end as usize),
        })
    }

    /// `identPath : ident (PATHSEP ident)*`
    fn parse_ident_path(&mut self) -> IdentPath<&'a str, Md> {
        let lo = self.cur.start;
        let mut path = vec![self.ident()];
        while self.at(TokenKind::PathSep) {
            self.bump();
            path.push(self.ident());
        }
        IdentPath {
            path,
            metadata: (),
            span: Span::new(lo as usize, self.prev_end as usize),
        }
    }

    /// `callExpr : scopeAnnotation? identPath LPAREN args RPAREN`
    fn finish_call(
        &mut self,
        ann: Option<Ident<&'a str, Md>>,
        lo: u32,
        func: IdentPath<&'a str, Md>,
    ) -> CallExpr<&'a str, Md> {
        self.expect(TokenKind::LParen);
        let args = self.parse_args();
        self.expect(TokenKind::RParen);
        CallExpr {
            scope_annotation: ann,
            func,
            args,
            span: Span::new(lo as usize, self.prev_end as usize),
            metadata: (),
        }
    }

    /// `args : posArgList (COMMA kwArgList)? COMMA? | kwArgList COMMA? | ε`
    fn parse_args(&mut self) -> Args<&'a str, Md> {
        let lparen_end = self.prev_end;
        let lo = self.cur.start;
        let mut posargs = Vec::new();
        let mut kwargs = Vec::new();

        if self.at(TokenKind::RParen) {
            // Empty arg list: zero-width span just past the `(`.
            return Args {
                posargs,
                kwargs,
                span: Span::new(lparen_end as usize, lparen_end as usize),
                metadata: (),
            };
        }

        if self.is_kwarg_start() {
            self.parse_kwargs(&mut kwargs);
        } else {
            loop {
                let mark = self.ntok;
                posargs.push(self.parse_expr(0));
                if !self.eat(TokenKind::Comma) {
                    break;
                }
                if self.at(TokenKind::RParen) || self.at(TokenKind::Eof) {
                    break; // trailing comma
                }
                if self.is_kwarg_start() {
                    self.parse_kwargs(&mut kwargs);
                    break;
                }
                if self.ntok == mark {
                    self.bump();
                }
            }
        }

        Args {
            posargs,
            kwargs,
            span: Span::new(lo as usize, self.prev_end as usize),
            metadata: (),
        }
    }

    #[inline]
    fn is_kwarg_start(&self) -> bool {
        self.cur.kind == TokenKind::Ident && self.nxt.kind == TokenKind::Eq
    }

    fn parse_kwargs(&mut self, kwargs: &mut Vec<KwArgValue<&'a str, Md>>) {
        loop {
            if self.at(TokenKind::RParen) || self.at(TokenKind::Eof) {
                break;
            }
            let mark = self.ntok;
            kwargs.push(self.parse_kw_arg_value());
            if !self.eat(TokenKind::Comma) {
                break;
            }
            if self.ntok == mark {
                self.bump();
            }
        }
    }

    /// `kwArgValue : ident EQ expr`
    fn parse_kw_arg_value(&mut self) -> KwArgValue<&'a str, Md> {
        let lo = self.cur.start;
        let name = self.ident();
        self.expect(TokenKind::Eq);
        let value = self.parse_expr(0);
        KwArgValue {
            name,
            value,
            span: Span::new(lo as usize, self.prev_end as usize),
            metadata: (),
        }
    }

    /// `INTLIT` optionally followed by `. INTLIT?` to form a float. The value is
    /// parsed from the raw source slice (including any interior trivia) and
    /// defaults to 0 on failure, matching the ANTLR `AstBuilder`.
    fn parse_int_or_float(&mut self) -> Expr<&'a str, Md> {
        let i0 = self.bump();
        if self.at(TokenKind::Dot) {
            let dot = self.bump();
            let end = if self.at(TokenKind::IntLit) {
                self.bump().end
            } else {
                dot.end
            };
            let span = Span::new(i0.start as usize, end as usize);
            Expr::FloatLiteral(FloatLiteral {
                span,
                value: self.slice_span(span).parse::<f64>().unwrap_or_default(),
            })
        } else {
            let span = self.span(i0);
            Expr::IntLiteral(IntLiteral {
                span,
                value: self.slice_tok(i0).parse::<i64>().unwrap_or_default(),
            })
        }
    }

    /// `stringLiteral : STRLIT` — span includes the quotes; value trims them.
    fn parse_string_literal(&mut self) -> Expr<&'a str, Md> {
        let t = self.bump();
        let span = self.span(t);
        let value = self.slice_span(span).trim_matches('"');
        Expr::StringLiteral(StringLiteral { span, value })
    }

    // ------------------------------------------------------------------
    // Leaves
    // ------------------------------------------------------------------

    fn ident(&mut self) -> Ident<&'a str, Md> {
        if self.at(TokenKind::Ident) {
            let t = self.bump();
            Ident {
                span: self.span(t),
                name: self.slice_tok(t),
                metadata: (),
            }
        } else {
            let t = self.cur;
            self.error_at(
                self.span(t),
                format!("expected identifier, found {}", t.kind.describe()),
            );
            Ident {
                span: Span::new(t.start as usize, t.start as usize),
                name: "",
                metadata: (),
            }
        }
    }

    /// Build an `Ident` from an `ANNOTATION` token, stripping the leading `#`.
    fn annotation_ident(&self, t: Token) -> Ident<&'a str, Md> {
        let span = Span::new(t.start as usize + 1, t.end as usize);
        Ident {
            span,
            name: self.slice_span(span),
            metadata: (),
        }
    }
}
