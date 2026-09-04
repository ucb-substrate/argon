//! Hand-written recursive-descent + Pratt parser for Argon.
//!
//! Builds `Ast<&'a str, ParseMetadata>` directly from the token stream in a
//! single pass — no intermediate concrete syntax tree. Identifier and string
//! text is borrowed straight from the source (`&'a str`); every node records a
//! byte-offset `cfgrammar::Span` that indexes the original (untrimmed) input.
//!
//! Expression precedence/associativity: prefix unary binds tightest for its
//! operand; the suffix cluster (`.field`, `.idx`, `[]`, `!`, `as`) binds tighter
//! than the binary operators; `* / %` > `+ -` > comparisons; all binary
//! operators are left-associative. Boolean operations are lower precedence than
//! comparisons, as in Rust: comparisons > `&&` > `||`.

use std::str::FromStr;

use cfgrammar::Span;

use crate::ast::{
    ArgDecl, Args, ArithOp, Ast, BinOp, BinOpExpr, BoolLiteral, BoolOp, CallExpr, CastExpr,
    CellDecl, ComparisonOp, ConstantDecl, Decl, EmitExpr, EnumDecl, Expr, FieldAccessExpr,
    FloatLiteral, FnDecl, ForLoop, Ident, IdentPath, IfExpr, IndexExpr, IndexFieldAccessExpr,
    IntLiteral, KwArgValue, LetBinding, MatchArm, MatchExpr, ModDecl, NilLiteral, Scope,
    SeqNilLiteral, Statement, StringLiteral, StructDecl, StructField, StructLitExpr,
    StructLitField, TupleExpr, TySpec, TySpecKind, UnaryOp, UnaryOpExpr, UseDecl,
};
use crate::compile::BUILTINS;
use crate::parse::ParseMetadata;

use super::ParseError;
use super::lexer::Lexer;
use super::token::{Token, TokenKind};

type Md = ParseMetadata;

// Binding powers for the Pratt loop. Higher binds tighter. The numbers only
// need to preserve the ordering; the absolute values are arbitrary.
//   `||`: 1/2   `&&`: 3/4   comparisons: 5/6   additive: 7/8
//   multiplicative: 9/10   suffix cluster: 11   prefix unary operand: 13
const SUFFIX_BP: u8 = 11;
const PREFIX_BP: u8 = 13;

/// Recursion-depth guard for pathological nesting (real programs are shallow).
const MAX_DEPTH: u32 = 256;

/// The infix operator a token denotes plus its left/right binding power, or
/// `None` if the token is not an infix operator. Single source of truth for the
/// infix set: precedence and the AST op are defined together so they can't drift.
#[inline]
fn infix_op(k: TokenKind) -> Option<(BinOp, u8, u8)> {
    use TokenKind::*;
    Some(match k {
        PipePipe => (BinOp::Bool(BoolOp::Or), 1, 2),
        AmpAmp => (BinOp::Bool(BoolOp::And), 3, 4),
        EqEq => (BinOp::Cmp(ComparisonOp::Eq), 5, 6),
        Neq => (BinOp::Cmp(ComparisonOp::Ne), 5, 6),
        Geq => (BinOp::Cmp(ComparisonOp::Geq), 5, 6),
        Gt => (BinOp::Cmp(ComparisonOp::Gt), 5, 6),
        Leq => (BinOp::Cmp(ComparisonOp::Leq), 5, 6),
        Lt => (BinOp::Cmp(ComparisonOp::Lt), 5, 6),
        Plus => (BinOp::Arith(ArithOp::Add), 7, 8),
        Minus => (BinOp::Arith(ArithOp::Sub), 7, 8),
        Star => (BinOp::Arith(ArithOp::Mul), 9, 10),
        Slash => (BinOp::Arith(ArithOp::Div), 9, 10),
        Percent => (BinOp::Arith(ArithOp::Rem), 9, 10),
        _ => return None,
    })
}

/// A single-pass recursive-descent + Pratt parser over the [`Lexer`] token
/// stream.
///
/// **Two-token lookahead.** The parser keeps a sliding window of `cur` (the
/// token to act on) and `nxt` (one token of lookahead), refilled by
/// [`Parser::bump`]. One token of lookahead is enough for every decision in the
/// grammar — e.g. telling a keyword argument `name = expr` from a positional one
/// requires peeking past the identifier at the `=` (see `is_kwarg_start`).
///
/// **Zero-copy.** Tokens carry only a kind and a byte span; identifier and
/// string text is borrowed straight from the source `&'a str` by slicing that
/// span (`slice_tok`/`slice_span`). No intermediate concrete syntax tree is
/// built — AST nodes are produced directly during the walk.
///
/// **Accumulate-and-recover, never panic.** Diagnostics are pushed onto `errors`
/// instead of aborting: a failed [`Parser::expect`] records an error and returns
/// a zero-width synthetic token, and every list/statement loop carries a
/// *progress guard* — it remembers `ntok` (the monotonic consumed-token count)
/// at the top of the iteration and force-advances past a stuck token if nothing
/// was consumed — so malformed input can never spin forever. A parse therefore
/// reports many diagnostics in one pass and always yields a (possibly degraded)
/// AST, which the analyzer relies on to analyze incomplete files on every
/// keystroke.
///
/// **Spans.** Every node records a byte-offset [`Span`] into the *original*
/// (untrimmed) input; composite-node spans are closed panic-safely by
/// [`Parser::finish_span`]. Later AST passes re-slice names and literal values
/// from these spans, so they must be byte-exact.
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
    /// Next semantic scope ordinal in each enclosing lexical scope.
    scope_orders: Vec<u64>,
    depth: u32,
    /// Whether `name {` must be read as an identifier followed by a scope
    /// rather than as a struct literal. See [`Parser::with_struct_literals`].
    no_struct_literal: bool,
    pub errors: Vec<ParseError>,
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
            scope_orders: vec![0],
            depth: 0,
            no_struct_literal: false,
            errors: Vec::new(),
        }
    }

    /// Runs `f` with struct literals allowed or forbidden, restoring the
    /// previous setting afterwards.
    ///
    /// A struct literal is forbidden at the top level of an `if` condition, a
    /// `match` scrutinee, and a `for` sequence, where `name {` already opens
    /// the construct's own scope; Rust has the same rule, and the same escape
    /// hatch of wrapping the literal in parentheses. Parentheses, brackets,
    /// call arguments, struct literal bodies, match arm bodies, and brace
    /// scopes lift the restriction again.
    fn with_struct_literals<T>(&mut self, allowed: bool, f: impl FnOnce(&mut Self) -> T) -> T {
        let saved = std::mem::replace(&mut self.no_struct_literal, !allowed);
        let result = f(self);
        self.no_struct_literal = saved;
        result
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

    /// Parse a comma-separated list `item (',' item)* ','?` up to `close` (or
    /// EOF): zero or more items with an **optional trailing comma**, returning
    /// the collected items (empty if the cursor is already at `close`).
    ///
    /// This is the single source of truth for comma-list policy. Every
    /// comma-separated construct — arg decls, enum variants, struct fields,
    /// tuple-type elements, keyword args — routes through it, so trailing-comma
    /// handling and the termination guarantee cannot drift between call sites
    /// (that drift is what silently accepted an empty tuple type and dropped
    /// trailing commas on it). Note this is distinct from the *comma-terminated*
    /// lists (tuple expressions, match arms) where a comma after **every**
    /// element is mandatory; those keep their own loops.
    ///
    /// Termination: every iteration that does not `break` consumes at least the
    /// separator, so at most one iteration runs per remaining comma — a
    /// non-consuming `parse_item` cannot spin.
    fn separated_list<T>(
        &mut self,
        close: TokenKind,
        mut parse_item: impl FnMut(&mut Self) -> T,
    ) -> Vec<T> {
        let mut items = Vec::new();
        while !self.at(close) && !self.at(TokenKind::Eof) {
            items.push(parse_item(self));
            if !self.eat(TokenKind::Comma) {
                break;
            }
        }
        items
    }

    #[inline]
    fn span(&self, t: Token) -> Span {
        Span::new(t.start as usize, t.end as usize)
    }

    /// Close a composite-node span that began at `lo` (the start offset of the
    /// node's first token) at the end of the last consumed token. On an error
    /// or recovery path a rule may consume nothing after capturing `lo`, leaving
    /// `prev_end < lo`; clamp so the span is never inverted (`cfgrammar::Span::new`
    /// panics when `end < start`). For well-formed nodes `prev_end >= lo`, so this
    /// is a no-op.
    #[inline]
    fn finish_span(&self, lo: u32) -> Span {
        Span::new(lo as usize, self.prev_end.max(lo) as usize)
    }

    /// Enter a recursive rule, bumping the shared depth guard. Returns `false`
    /// (leaving the depth unchanged) when the nesting limit is exceeded, so the
    /// caller can record an error and return a degraded node. Pair every `true`
    /// with `exit_depth`.
    fn enter_depth(&mut self) -> bool {
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            self.depth -= 1;
            false
        } else {
            true
        }
    }

    fn exit_depth(&mut self) {
        self.depth -= 1;
    }

    fn next_scope_order(&mut self) -> u64 {
        let next = self.scope_orders.last_mut().unwrap();
        let order = *next;
        *next += 1;
        order
    }

    fn current_scope_order(&self) -> u64 {
        *self.scope_orders.last().unwrap()
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
        // Suppress only an exact duplicate of the immediately preceding
        // diagnostic (same start offset *and* same message) — the cascade an
        // `expect` retry produces while the cursor is stuck on a bad token.
        // Distinct diagnostics at the same offset are kept: they describe
        // independent problems (e.g. a token that is simultaneously not an
        // expression and not the expected `)`), so collapsing them by position
        // alone would lose real diagnostics.
        if let Some(last) = self.errors.last()
            && last.span.start() == span.start()
            && last.message == message
        {
            return;
        }
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
        // The root span runs to the EOF token, i.e. the end of the (untrimmed)
        // input — `src` is the trimmed buffer, so `src.len() + base` is the
        // original length.
        let end = self.src.len() + self.base;
        Ast {
            decls,
            span: Span::new(lo, end),
        }
    }

    /// A single call expression followed by EOF, as a standalone entry (used by
    /// `parse_cell`). Returns `None` (with an error recorded) unless the input is
    /// *exactly* one call expression: the whole input must parse to an
    /// `Expr::Call` and reach EOF.
    /// This rejects both trailing garbage (`f() junk`) and suffixed calls
    /// (`f()!`, `f().x`, `f()[0]`, which parse to an `Emit`/`FieldAccess`/`Index`
    /// root rather than a `Call`).
    pub fn parse_cell_entry(&mut self) -> Option<CallExpr<&'a str, Md>> {
        let expr = self.parse_expr(0);
        let Expr::Call(call) = expr else {
            self.error_at(
                self.span(self.cur),
                "expected a cell invocation".to_string(),
            );
            return None;
        };
        if !self.at(TokenKind::Eof) {
            self.error_at(
                self.span(self.cur),
                format!(
                    "expected end of input after cell invocation, found {}",
                    self.cur.kind.describe()
                ),
            );
            return None;
        }
        Some(call)
    }

    fn recover_to_decl(&mut self) {
        use TokenKind::*;
        while !self.at(Eof) {
            match self.cur.kind {
                KwEnum | KwStruct | KwCell | KwFn | KwConst | KwMod | KwUse => break,
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
            KwUse => Decl::Use(self.parse_use_decl()),
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
        let fields = self.separated_list(TokenKind::RBrace, |p| p.parse_struct_field());
        self.expect(TokenKind::RBrace);
        StructDecl {
            name,
            fields,
            span: self.finish_span(lo),
            metadata: (),
        }
    }

    /// `structField : ident COLON tySpec`
    fn parse_struct_field(&mut self) -> StructField<&'a str, Md> {
        let lo = self.cur.start;
        let name = self.ident();
        self.expect(TokenKind::Colon);
        let ty = self.parse_ty_spec();
        StructField {
            name,
            ty,
            span: self.finish_span(lo),
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
            span: self.finish_span(lo),
        }
    }

    /// `useDecl : USE identPath (AS ident)? SEMI`
    fn parse_use_decl(&mut self) -> UseDecl<&'a str, Md> {
        let lo = self.cur.start;
        self.expect(TokenKind::KwUse);
        let path = self.parse_ident_path();
        if path.path.len() < 2 {
            self.error_at(
                path.span,
                "a use path must name an item in a module".to_string(),
            );
        }
        let alias = if self.eat(TokenKind::KwAs) {
            Some(self.ident())
        } else {
            None
        };
        self.expect(TokenKind::Semi);
        UseDecl {
            path: path.path,
            alias,
            span: self.finish_span(lo),
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
            span: self.finish_span(lo),
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
            span: self.finish_span(lo),
            metadata: (),
        }
    }

    /// `argDecls : (argDecl (COMMA argDecl)* COMMA?)?`
    ///
    /// Default values number their scopes from zero, like a brace scope.
    fn parse_arg_decls(&mut self) -> Vec<ArgDecl<&'a str, Md>> {
        self.scope_orders.push(0);
        let args = self.separated_list(TokenKind::RParen, |p| p.parse_arg_decl());
        self.scope_orders.pop();
        args
    }

    /// `argDecl : ident COLON tySpec (EQ expr)?`
    fn parse_arg_decl(&mut self) -> ArgDecl<&'a str, Md> {
        let name = self.ident();
        self.expect(TokenKind::Colon);
        let ty = self.parse_ty_spec();
        let default = self.eat(TokenKind::Eq).then(|| self.parse_expr(0));
        ArgDecl {
            name,
            ty,
            default,
            metadata: (),
        }
    }

    /// `enumVariants : (ident (COMMA ident)* COMMA?)?`
    fn parse_ident_list(&mut self) -> Vec<Ident<&'a str, Md>> {
        self.separated_list(TokenKind::RBrace, |p| p.ident())
    }

    /// `tySpec : ident | LBRACK tySpec RBRACK | LPAREN tySpecList RPAREN`
    fn parse_ty_spec(&mut self) -> TySpec<&'a str, Md> {
        let lo = self.cur.start;
        // `[..]`/`(..)` nest recursively; guard the native stack like parse_expr.
        if !self.enter_depth() {
            self.error_at(self.span(self.cur), "type nesting too deep".to_string());
            return TySpec {
                kind: TySpecKind::Tuple(Vec::new()),
                span: self.finish_span(lo),
            };
        }
        let kind = match self.cur.kind {
            TokenKind::LBrack => {
                self.bump();
                let inner = self.parse_ty_spec();
                self.expect(TokenKind::RBrack);
                TySpecKind::Seq(Box::new(inner))
            }
            TokenKind::LParen => {
                self.bump();
                // `()` yields the empty (unit) tuple type; a trailing comma is
                // allowed like every other comma list. `ty_from_spec` lowers the
                // empty tuple to the unit type `Ty::Nil` (the type of the `()`
                // value), so an empty tuple type is a real, usable type rather
                // than an unhandled edge case.
                let list = self.separated_list(TokenKind::RParen, |p| p.parse_ty_spec());
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
        self.exit_depth();
        TySpec {
            kind,
            span: self.finish_span(lo),
        }
    }

    // ------------------------------------------------------------------
    // Scopes & statements
    // ------------------------------------------------------------------

    /// `scope : LBRACE statements (expr)? RBRACE`
    fn parse_scope(&mut self) -> Scope<&'a str, Md> {
        self.parse_unannotated_scope(0)
    }

    fn parse_unannotated_scope(&mut self, scope_order: u64) -> Scope<&'a str, Md> {
        self.with_struct_literals(true, |p| p.parse_unannotated_scope_inner(scope_order))
    }

    fn parse_unannotated_scope_inner(&mut self, scope_order: u64) -> Scope<&'a str, Md> {
        if !self.enter_depth() {
            self.error_at(self.span(self.cur), "nesting too deep".to_string());
            let lo = self.cur.start;
            return Scope {
                scope_order,
                span: Span::new(lo as usize, lo as usize),
                stmts: Vec::new(),
                tail: None,
                metadata: (),
            };
        }
        let lb = self.expect(TokenKind::LBrace);
        let lo = lb.start;
        self.scope_orders.push(0);
        let mut stmts = Vec::new();
        let mut tail: Option<Expr<&'a str, Md>> = None;

        while !self.at(TokenKind::RBrace) && !self.at(TokenKind::Eof) {
            let mark = self.ntok;
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
        self.scope_orders.pop();

        // No separate tail fixup is needed: the statement loop above already
        // routes a trailing un-semicoloned expression into `tail` (the
        // `at(RBrace) || at(Eof)` arm), and only ever pushes a `semicolon: false`
        // statement when more tokens follow it — so a `semicolon: false`
        // statement is never the last element here.

        self.exit_depth();
        Scope {
            scope_order,
            span: self.finish_span(lo),
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
            span: self.finish_span(lo),
        }
    }

    /// `forLoop : FOR ident IN expr scope`
    fn parse_for_loop(&mut self) -> ForLoop<&'a str, Md> {
        let lo = self.cur.start;
        let scope_order = self.next_scope_order();
        self.expect(TokenKind::KwFor);
        let var = self.ident();
        self.expect(TokenKind::KwIn);
        let seq = self.with_struct_literals(false, |p| p.parse_expr(0));
        let body = self.parse_scope();
        ForLoop {
            var,
            seq,
            body,
            scope_order,
            metadata: (),
            span: self.finish_span(lo),
        }
    }

    fn parse_if(&mut self, scope_order: u64, lo: u32) -> IfExpr<&'a str, Md> {
        self.expect(TokenKind::KwIf);
        let cond = self.with_struct_literals(false, |p| p.parse_expr(0));
        let then = self.parse_scope();
        self.expect(TokenKind::KwElse);
        let else_ = self.parse_scope();
        IfExpr {
            scope_order,
            cond,
            then,
            else_,
            span: self.finish_span(lo),
            metadata: (),
        }
    }

    /// `matchExpr : MATCH expr LBRACE matchArms RBRACE`
    fn parse_match(&mut self) -> MatchExpr<&'a str, Md> {
        let lo = self.cur.start;
        self.expect(TokenKind::KwMatch);
        let scrutinee = self.with_struct_literals(false, |p| p.parse_expr(0));
        let lbrace = self.expect(TokenKind::LBrace);
        let mut arms = Vec::new();
        while !self.at(TokenKind::RBrace) && !self.at(TokenKind::Eof) {
            let mark = self.ntok;
            arms.push(self.parse_match_arm());
            if self.ntok == mark {
                self.bump();
            }
        }
        let rbrace = self.expect(TokenKind::RBrace);
        if arms.is_empty() {
            // `matchArms : matchArm+` requires at least one arm; `match k {}` is
            // a syntax error, not a degenerate empty-arm AST flowing into the
            // type checker. Point the diagnostic at the empty `{}`.
            self.error_at(
                Span::new(lbrace.start as usize, rbrace.end as usize),
                "match requires at least one arm".to_string(),
            );
        }
        MatchExpr {
            scrutinee,
            arms,
            span: self.finish_span(lo),
            metadata: (),
        }
    }

    /// `matchArm : identPath FAT_ARROW expr COMMA` (span includes the comma).
    fn parse_match_arm(&mut self) -> MatchArm<&'a str, Md> {
        let lo = self.cur.start;
        let pattern = self.parse_ident_path();
        self.expect(TokenKind::FatArrow);
        // An arm body is bounded by its comma, not by `{`, so a struct
        // literal is unambiguous here even when the whole `match` sits in
        // an `if`/`match`/`for` head.
        let expr = self.with_struct_literals(true, |p| p.parse_expr(0));
        self.expect(TokenKind::Comma);
        MatchArm {
            pattern,
            expr,
            span: self.finish_span(lo),
        }
    }

    // ------------------------------------------------------------------
    // Expressions (Pratt)
    // ------------------------------------------------------------------

    /// Parse an expression by **precedence climbing** (the Pratt loop).
    ///
    /// `min_bp` is the minimum left binding power an operator must have to bind
    /// at this point. After parsing a prefix operand, the loop keeps folding
    /// trailing suffix/infix operators into `lhs` while their *left* binding
    /// power is `>= min_bp`, recursing with the operator's *right* binding power
    /// for the right operand. Higher power binds tighter; a right power strictly
    /// greater than the left power makes an operator left-associative (so
    /// `a - b - c` parses as `(a - b) - c`). The powers live in one table,
    /// [`infix_op`]; the suffix cluster (`.field`, `.idx`, `[]`, `!`, `as`) sits
    /// at [`SUFFIX_BP`], tighter than any binary operator. A caller wanting a
    /// full expression passes `min_bp == 0`.
    fn parse_expr(&mut self, min_bp: u8) -> Expr<&'a str, Md> {
        if !self.enter_depth() {
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
        // parens), while the enclosing operator's span must start at the `(`.
        let lhs_start = self.cur.start;
        let mut lhs = self.parse_prefix();

        // The loop folds each suffix/infix operator into `lhs`, so it deepens
        // the tree by one level per iteration *without* recursing. Charging
        // each fold to the same depth budget is what makes the guard measure
        // the AST that later passes recurse over, rather than only the
        // parser's own stack: `1+1+1+...` and `a.f.f.f...` are as deep as
        // `f(f(f(...)))`, and the post-parse walks overflow on them alike.
        let mut folds = 0u32;
        loop {
            let k = self.cur.kind;
            let is_suffix = SUFFIX_BP >= min_bp
                && matches!(
                    k,
                    TokenKind::Dot | TokenKind::LBrack | TokenKind::Bang | TokenKind::KwAs
                );
            let infix = infix_op(k).filter(|(_, l_bp, _)| *l_bp >= min_bp);
            if !is_suffix && infix.is_none() {
                break;
            }
            if !self.enter_depth() {
                self.error_at(
                    self.span(self.cur),
                    "expression nesting too deep".to_string(),
                );
                break;
            }
            folds += 1;
            if is_suffix {
                lhs = self.parse_suffix(lhs, lhs_start);
                continue;
            }
            let (op, _, r_bp) = infix.expect("not a suffix, so an infix operator");
            self.bump();
            let rhs = self.parse_expr(r_bp);
            lhs = self.make_infix(op, lhs, rhs, lhs_start);
        }

        for _ in 0..=folds {
            self.exit_depth();
        }
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
                    span: self.finish_span(op_tok.start),
                    metadata: (),
                }))
            }
            _ => self.parse_primary(),
        }
    }

    fn make_infix(
        &self,
        op: BinOp,
        left: Expr<&'a str, Md>,
        right: Expr<&'a str, Md>,
        lhs_start: u32,
    ) -> Expr<&'a str, Md> {
        Expr::BinOp(Box::new(BinOpExpr {
            op,
            left,
            right,
            span: self.finish_span(lhs_start),
            metadata: (),
        }))
    }

    /// Apply one suffix (`.field`, `.idx`, `[index]`, postfix `!`, `as ty`).
    /// `lhs_start` is the lexical start of the whole expression (see `parse_expr`).
    fn parse_suffix(&mut self, lhs: Expr<&'a str, Md>, lhs_start: u32) -> Expr<&'a str, Md> {
        match self.cur.kind {
            TokenKind::Dot => {
                self.bump();
                match self.cur.kind {
                    TokenKind::Ident => {
                        let field = self.ident();
                        Expr::FieldAccess(Box::new(FieldAccessExpr {
                            base: lhs,
                            field,
                            span: self.finish_span(lhs_start),
                            metadata: (),
                        }))
                    }
                    TokenKind::IntLit => {
                        let t = self.bump();
                        let span = self.span(t);
                        let field = IntLiteral {
                            span,
                            value: self.literal_value(span, "integer"),
                        };
                        Expr::IndexFieldAccess(Box::new(IndexFieldAccessExpr {
                            base: lhs,
                            field,
                            span: self.finish_span(lhs_start),
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
                let index = self.with_struct_literals(true, |p| p.parse_expr(0));
                self.expect(TokenKind::RBrack);
                Expr::Index(Box::new(IndexExpr {
                    base: lhs,
                    index,
                    span: self.finish_span(lhs_start),
                    metadata: (),
                }))
            }
            TokenKind::Bang => {
                let t = self.bump();
                Expr::Emit(Box::new(EmitExpr {
                    value: lhs,
                    span: Span::new(lhs_start as usize, t.end as usize),
                    metadata: (),
                }))
            }
            TokenKind::KwAs => {
                self.bump();
                let ty = self.parse_ty_spec();
                Expr::Cast(Box::new(CastExpr {
                    value: lhs,
                    ty,
                    span: self.finish_span(lhs_start),
                    metadata: (),
                }))
            }
            _ => unreachable!("parse_suffix called on non-suffix token"),
        }
    }

    /// Parse a primary expression: the atomic operand at the head of an
    /// expression (the Pratt "null denotation"). Covers literals, identifier
    /// paths and calls, the parenthesized/tuple/`nil` forms, sequence-nil `[]`,
    /// and the block-form primaries `if`/`match`/`{…}` — which are themselves
    /// expressions in Argon, so [`Self::parse_expr`] can still extend them with
    /// trailing operators (e.g. `if c {a} else {b} + 1`). Trailing suffixes and
    /// infix operators are applied by [`Self::parse_expr`], not here.
    fn parse_primary(&mut self) -> Expr<&'a str, Md> {
        match self.cur.kind {
            TokenKind::LParen => self.parse_paren(),
            TokenKind::LBrack => self.parse_seq_nil(),
            TokenKind::KwIf => {
                let lo = self.cur.start;
                let scope_order = self.next_scope_order();
                Expr::If(Box::new(self.parse_if(scope_order, lo)))
            }
            TokenKind::KwMatch => Expr::Match(Box::new(self.parse_match())),
            TokenKind::LBrace => {
                let scope_order = self.next_scope_order();
                Expr::Scope(Box::new(self.parse_unannotated_scope(scope_order)))
            }
            TokenKind::Ident => {
                let path = self.parse_ident_path();
                if self.at(TokenKind::LParen) {
                    let lo = path.span.start() as u32;
                    let scope_order = if BUILTINS.contains(&path.path.last().unwrap().name) {
                        self.current_scope_order()
                    } else {
                        self.next_scope_order()
                    };
                    Expr::Call(self.finish_call(scope_order, lo, path))
                } else if self.at(TokenKind::LBrace) && !self.no_struct_literal {
                    Expr::StructLit(Box::new(self.parse_struct_lit(path)))
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

    /// `structLit : identPath LBRACE structLitBody RBRACE`, where
    /// `structLitBody : (structLitField (COMMA structLitField)* (COMMA structBase | COMMA)?)? | structBase`
    /// and `structBase : DOTDOT expr`.
    ///
    /// The `..base` comes last, after a comma, and may not be followed by one,
    /// which is the shape Rust accepts. Because the body has two terminators
    /// (`}` and `..`) it does not go through `separated_list`; termination
    /// holds for the same reason, since every iteration that does not `break`
    /// consumes the separator.
    fn parse_struct_lit(&mut self, path: IdentPath<&'a str, Md>) -> StructLitExpr<&'a str, Md> {
        let lo = path.span.start() as u32;
        self.expect(TokenKind::LBrace);
        let mut fields = Vec::new();
        let mut base = None;
        self.with_struct_literals(true, |p| {
            while !p.at(TokenKind::RBrace) && !p.at(TokenKind::Eof) {
                if p.eat(TokenKind::DotDot) {
                    base = Some(p.parse_expr(0));
                    break;
                }
                fields.push(p.parse_struct_lit_field());
                if !p.eat(TokenKind::Comma) {
                    break;
                }
            }
        });
        self.expect(TokenKind::RBrace);
        StructLitExpr {
            path,
            fields,
            base,
            span: self.finish_span(lo),
            metadata: (),
        }
    }

    /// `structLitField : ident (COLON expr)?`
    fn parse_struct_lit_field(&mut self) -> StructLitField<&'a str, Md> {
        let lo = self.cur.start;
        let name = self.ident();
        let (value, shorthand) = if self.eat(TokenKind::Colon) {
            (self.parse_expr(0), false)
        } else {
            // Shorthand: `x` stands for `x: x`. The value is a path at the
            // name's own span, so diagnostics and navigation on it point at the
            // one token the user wrote.
            let value = Expr::IdentPath(IdentPath {
                path: vec![name.clone()],
                metadata: (),
                span: name.span,
            });
            (value, true)
        };
        StructLitField {
            name,
            value,
            shorthand,
            span: self.finish_span(lo),
        }
    }

    /// `( )` nil, `( expr )` parenthesized group (unwrapped), or
    /// `( expr , (expr ,)* )` tuple (a comma after every element is required).
    fn parse_paren(&mut self) -> Expr<&'a str, Md> {
        self.with_struct_literals(true, |p| p.parse_paren_inner())
    }

    fn parse_paren_inner(&mut self) -> Expr<&'a str, Md> {
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
            span: self.finish_span(lp.start),
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
            span: self.finish_span(lo),
        }
    }

    /// `callExpr : identPath LPAREN args RPAREN`
    fn finish_call(
        &mut self,
        scope_order: u64,
        lo: u32,
        func: IdentPath<&'a str, Md>,
    ) -> CallExpr<&'a str, Md> {
        self.expect(TokenKind::LParen);
        let args = self.parse_args();
        self.expect(TokenKind::RParen);
        CallExpr {
            scope_order,
            func,
            args,
            span: self.finish_span(lo),
            metadata: (),
        }
    }

    /// `args : posArgList (COMMA kwArgList)? COMMA? | kwArgList COMMA? | ε`
    fn parse_args(&mut self) -> Args<&'a str, Md> {
        self.with_struct_literals(true, |p| p.parse_args_inner())
    }

    fn parse_args_inner(&mut self) -> Args<&'a str, Md> {
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
            kwargs = self.parse_kwargs();
        } else {
            // Positional args until a `)`, a trailing comma, or the first
            // keyword arg (after which only keyword args may follow). Each
            // iteration consumes at least the separator, so the loop terminates.
            loop {
                posargs.push(self.parse_expr(0));
                if !self.eat(TokenKind::Comma) {
                    break;
                }
                if self.at(TokenKind::RParen) || self.at(TokenKind::Eof) {
                    break; // trailing comma
                }
                if self.is_kwarg_start() {
                    kwargs = self.parse_kwargs();
                    break;
                }
            }
        }

        Args {
            posargs,
            kwargs,
            span: self.finish_span(lo),
            metadata: (),
        }
    }

    #[inline]
    fn is_kwarg_start(&self) -> bool {
        self.cur.kind == TokenKind::Ident && self.nxt.kind == TokenKind::Eq
    }

    /// `kwArgList : kwArgValue (COMMA kwArgValue)* COMMA?`
    fn parse_kwargs(&mut self) -> Vec<KwArgValue<&'a str, Md>> {
        self.separated_list(TokenKind::RParen, |p| p.parse_kw_arg_value())
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
            span: self.finish_span(lo),
            metadata: (),
        }
    }

    /// `INTLIT` optionally followed by `. INTLIT?` to form a float.
    ///
    /// See `literal_value` for a slice that does not parse.
    fn parse_int_or_float(&mut self) -> Expr<&'a str, Md> {
        let i0 = self.bump();
        // `INTLIT .` forms a float (`1.`, `1.5`) — except when the `.` is
        // immediately followed by an identifier, which is a field-access suffix
        // on the integer (`1.foo`); leave that `.` for the Pratt suffix loop so
        // an integer can be the base of `.field`/`.idx` like every other
        // primary. A `.` before another `INTLIT`, or before any non-identifier
        // token (e.g. `1.`), still assembles a float, matching prior behavior.
        if self.at(TokenKind::Dot) && self.nxt.kind != TokenKind::Ident {
            let dot = self.bump();
            let end = if self.at(TokenKind::IntLit) {
                self.bump().end
            } else {
                dot.end
            };
            let span = Span::new(i0.start as usize, end as usize);
            Expr::FloatLiteral(FloatLiteral {
                span,
                value: self.literal_value(span, "float"),
            })
        } else {
            let span = self.span(i0);
            Expr::IntLiteral(IntLiteral {
                span,
                value: self.literal_value(span, "integer"),
            })
        }
    }

    /// Parses a numeric literal from its source slice.
    fn literal_value<T: Default + FromStr>(&mut self, span: Span, kind: &str) -> T {
        let slice = self.slice_span(span);
        slice.parse().unwrap_or_else(|_| {
            self.error_at(span, format!("invalid {kind} literal `{slice}`"));
            T::default()
        })
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
}
