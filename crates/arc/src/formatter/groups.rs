//! Finds the brackets of a parsed source file that the formatter may break,
//! and how each one lays out its contents.

use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::path::PathBuf;

use arcstr::Substr;
use argonc::ast::{
    ArgDecl, Decl, Expr, ForLoop, IdentPath, Pattern, Scope, Statement, TySpec, VariantPayload,
};
use argonc::parse::{ParseMetadata, parse_source_text};

use super::tokens::{Token, TokenKind};

type Md = ParseMetadata;

/// How a broken group lays out its contents.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Kind {
    /// Call arguments.
    Args,
    /// A sequence literal.
    Seq,
    /// A tuple literal.
    Tuple,
    /// Declared parameters, fields, or variants, or the fields of a variant
    /// pattern.
    Decl,
    /// A struct literal or pattern, whose `..` entry comes last and takes no
    /// comma.
    Struct,
    /// The items of a `use` group.
    Use,
    /// The arms of a `match`.
    Arms,
    /// The statements of a block.
    Block,
    /// A list comprehension, broken at the braces of its body.
    Comprehension,
}

impl Kind {
    /// The byte that separates the group's elements.
    pub(super) fn separator(self) -> u8 {
        if self == Self::Block { b';' } else { b',' }
    }

    /// Whether the group is a body that is broken before any list on its line.
    pub(super) fn is_block(self) -> bool {
        matches!(self, Self::Block | Self::Arms)
    }

    /// Whether a group with a single element may keep its brackets on the
    /// lines of that element's own brackets, as in `f([` and `])`.
    pub(super) fn hugs(self) -> bool {
        matches!(self, Self::Args | Self::Seq)
    }

    /// Whether the group may be the single element that another group hugs.
    pub(super) fn is_huggable(self) -> bool {
        matches!(
            self,
            Self::Args | Self::Seq | Self::Tuple | Self::Struct | Self::Comprehension
        )
    }
}

/// The breakable brackets of one source text, by byte offset.
pub(super) struct Groups {
    kinds: HashMap<usize, Kind>,
    /// Sorted, disjoint ranges of type syntax, whose commas separate nothing.
    types: Vec<Range<usize>>,
    /// Offsets just past statements that end without a semicolon.
    statement_ends: HashSet<usize>,
}

impl Groups {
    /// Classifies the brackets of `text`, or `None` if it does not parse.
    pub(super) fn new(text: &str, tokens: &[Token]) -> Option<Self> {
        let ast = parse_source_text(text, PathBuf::from("format.ar")).ok()?;
        let mut collector = Collector {
            text,
            tokens,
            kinds: HashMap::new(),
            types: Vec::new(),
            statement_ends: HashSet::new(),
        };
        for decl in &ast.ast.decls {
            collector.decl(decl);
        }
        collector.use_groups();

        let mut types = collector.types;
        types.sort_by_key(|range| range.start);
        let mut merged: Vec<Range<usize>> = Vec::with_capacity(types.len());
        for range in types {
            match merged.last_mut() {
                Some(last) if range.start <= last.end => last.end = last.end.max(range.end),
                _ => merged.push(range),
            }
        }
        Some(Self {
            kinds: collector.kinds,
            types: merged,
            statement_ends: collector.statement_ends,
        })
    }

    /// The kind of the group opened at `offset`, if it may be broken.
    pub(super) fn kind(&self, offset: usize) -> Option<Kind> {
        self.kinds.get(&offset).copied()
    }

    /// Whether `offset` is inside a type, such as the comma of `Pair<A, B>`.
    pub(super) fn in_type(&self, offset: usize) -> bool {
        let index = self.types.partition_point(|range| range.start <= offset);
        index > 0 && self.types[index - 1].end > offset
    }

    /// Whether a statement without a semicolon ends just before `offset`.
    pub(super) fn ends_statement(&self, offset: usize) -> bool {
        self.statement_ends.contains(&offset)
    }
}

struct Collector<'a> {
    text: &'a str,
    tokens: &'a [Token],
    kinds: HashMap<usize, Kind>,
    types: Vec<Range<usize>>,
    statement_ends: HashSet<usize>,
}

impl Collector<'_> {
    /// Records `kind` for the bracket at `offset`, if one opens there.
    fn at(&mut self, offset: usize, open: u8, kind: Kind) {
        if let Ok(index) = self
            .tokens
            .binary_search_by_key(&offset, |token| token.start)
            && self.tokens[index].kind == TokenKind::Open(open)
        {
            self.kinds.insert(offset, kind);
        }
    }

    /// Records `kind` for the first bracket at or after `offset`, if it is
    /// `open` and no other bracket or separator comes before it.
    fn after(&mut self, offset: usize, open: u8, kind: Kind) {
        let start = self.tokens.partition_point(|token| token.start < offset);
        for token in &self.tokens[start..] {
            match token.kind {
                TokenKind::Open(byte) if byte == open => {
                    self.kinds.insert(token.start, kind);
                    return;
                }
                TokenKind::Other | TokenKind::Comma | TokenKind::Comment => {}
                _ => return,
            }
        }
    }

    /// Records the braces of every `use prefix::{..}` group.
    fn use_groups(&mut self) {
        for pair in self.tokens.windows(2) {
            if pair[0].kind == TokenKind::Other
                && pair[1].kind == TokenKind::Open(b'{')
                && self.text[pair[0].start..pair[0].end].ends_with("::")
            {
                self.kinds.insert(pair[1].start, Kind::Use);
            }
        }
    }

    fn ty(&mut self, ty: &TySpec<Substr, Md>) {
        self.types.push(ty.span.start()..ty.span.end());
    }

    fn path(&mut self, path: &IdentPath<Substr, Md>) {
        if let Some(args) = &path.generic_args {
            self.types.push(args.span.start()..args.span.end());
        }
    }

    fn decl(&mut self, decl: &Decl<Substr, Md>) {
        match decl {
            Decl::Enum(decl) => {
                self.after(decl.name.span.end(), b'{', Kind::Decl);
                for variant in &decl.variants {
                    let end = variant.name.span.end();
                    match &variant.payload {
                        VariantPayload::Tuple(tys) => {
                            if !tys.is_empty() {
                                self.after(end, b'(', Kind::Decl);
                            }
                            tys.iter().for_each(|ty| self.ty(ty));
                        }
                        VariantPayload::Struct(fields) => {
                            self.after(end, b'{', Kind::Decl);
                            fields.iter().for_each(|field| self.ty(&field.ty));
                        }
                    }
                }
            }
            Decl::Struct(decl) => {
                self.after(decl.name.span.end(), b'{', Kind::Decl);
                decl.fields.iter().for_each(|field| self.ty(&field.ty));
            }
            Decl::Constant(decl) => self.expr(&decl.value),
            Decl::Cell(decl) => {
                self.after(decl.name.span.end(), b'(', Kind::Decl);
                self.arg_decls(&decl.args);
                self.scope(&decl.scope);
            }
            Decl::Fn(decl) => {
                self.after(decl.name.span.end(), b'(', Kind::Decl);
                self.arg_decls(&decl.args);
                if let Some(ty) = &decl.return_ty {
                    self.ty(ty);
                }
                if decl.foreign.is_none() {
                    self.scope(&decl.scope);
                }
            }
            Decl::Mod(_) | Decl::Use(_) => {}
        }
    }

    fn arg_decls(&mut self, args: &[ArgDecl<Substr, Md>]) {
        for arg in args {
            self.ty(&arg.ty);
            if let Some(default) = &arg.default {
                self.expr(default);
            }
        }
    }

    fn scope(&mut self, scope: &Scope<Substr, Md>) {
        self.at(scope.span.start(), b'{', Kind::Block);
        for statement in &scope.stmts {
            match statement {
                Statement::Expr { value, semicolon } => {
                    self.expr(value);
                    if !semicolon {
                        self.statement_ends.insert(value.span().end());
                    }
                }
                Statement::LetBinding(binding) => {
                    if let Some(ty) = &binding.ty {
                        self.ty(ty);
                    }
                    self.expr(&binding.value);
                }
                Statement::LetPattern(binding) => {
                    self.pattern(&binding.pattern);
                    self.expr(&binding.value);
                }
                Statement::ForLoop(for_loop) => {
                    self.for_loop(for_loop);
                    self.statement_ends.insert(for_loop.span.end());
                }
            }
        }
        if let Some(tail) = &scope.tail {
            self.expr(tail);
        }
    }

    fn for_loop(&mut self, for_loop: &ForLoop<Substr, Md>) {
        self.expr(&for_loop.seq);
        if let Some(filter) = &for_loop.filter {
            self.expr(filter);
        }
        self.scope(&for_loop.body);
    }

    fn pattern(&mut self, pattern: &Pattern<Substr, Md>) {
        match pattern {
            Pattern::Wildcard { .. } | Pattern::Binding { .. } => {}
            Pattern::Variant { path, fields, .. } => {
                self.path(path);
                if !fields.is_empty() {
                    self.after(path.span.end(), b'(', Kind::Decl);
                }
            }
            Pattern::Struct { path, .. } => {
                self.path(path);
                self.after(path.span.end(), b'{', Kind::Struct);
            }
        }
    }

    fn expr(&mut self, expr: &Expr<Substr, Md>) {
        match expr {
            Expr::If(expr) => {
                self.expr(&expr.cond);
                self.scope(&expr.then);
                if let Some(else_) = &expr.else_ {
                    self.scope(else_);
                }
            }
            Expr::ListComp(expr) => {
                self.at(expr.span.start(), b'[', Kind::Comprehension);
                self.for_loop(&expr.for_loop);
            }
            Expr::Match(expr) => {
                self.expr(&expr.scrutinee);
                self.after(expr.scrutinee.span().end(), b'{', Kind::Arms);
                for arm in &expr.arms {
                    self.pattern(&arm.pattern);
                    self.expr(&arm.expr);
                }
            }
            Expr::BinOp(expr) => {
                self.expr(&expr.left);
                self.expr(&expr.right);
            }
            Expr::UnaryOp(expr) => self.expr(&expr.operand),
            Expr::Call(call) => {
                self.path(&call.func);
                self.after(call.func.span.end(), b'(', Kind::Args);
                call.args.posargs.iter().for_each(|arg| self.expr(arg));
                call.args
                    .kwargs
                    .iter()
                    .for_each(|kwarg| self.expr(&kwarg.value));
            }
            Expr::Emit(expr) => self.expr(&expr.value),
            Expr::FieldAccess(expr) => self.expr(&expr.base),
            Expr::IndexFieldAccess(expr) => self.expr(&expr.base),
            Expr::Index(expr) => {
                self.expr(&expr.base);
                self.expr(&expr.index);
            }
            Expr::IdentPath(path) => self.path(path),
            Expr::Seq(seq) => {
                self.at(seq.span.start(), b'[', Kind::Seq);
                seq.items.iter().for_each(|item| self.expr(item));
            }
            Expr::Scope(scope) => self.scope(scope),
            Expr::Cast(expr) => {
                self.expr(&expr.value);
                self.ty(&expr.ty);
            }
            Expr::Tuple(tuple) => {
                self.at(tuple.span.start(), b'(', Kind::Tuple);
                tuple.items.iter().for_each(|item| self.expr(item));
            }
            Expr::StructLit(expr) => {
                self.path(&expr.path);
                self.after(expr.path.span.end(), b'{', Kind::Struct);
                for field in expr.fields.iter().filter(|field| !field.shorthand) {
                    self.expr(&field.value);
                }
                if let Some(base) = &expr.base {
                    self.expr(base);
                }
            }
            Expr::Nil(_)
            | Expr::FloatLiteral(_)
            | Expr::IntLiteral(_)
            | Expr::StringLiteral(_)
            | Expr::BoolLiteral(_) => {}
        }
    }
}
