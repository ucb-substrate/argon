//! Hand-written, zero-copy parser for the Argon language.
//!
//! A streaming byte lexer ([`lexer`]) feeds a single-pass recursive-descent +
//! Pratt parser ([`grammar`]) that builds the AST directly, borrowing all
//! identifier/string text from the source. The two public entry points match
//! the contract the rest of the compiler expects.

mod grammar;
mod lexer;
mod token;

use std::path::PathBuf;

use arcstr::ArcStr;
use cfgrammar::Span;

use crate::ast::annotated::AnnotatedAst;
use crate::ast::{CallExpr, Decl};
use crate::parse::{AnnotatedParseAst, ParseMetadata};

/// The syntactic role of the identifier, keyword, or expression being entered
/// at an editor cursor.
///
/// Completion uses this independently of type checking, so it remains
/// available while the source is incomplete.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompletionSite {
    /// Recovery could not identify a narrower grammar position.
    Unknown,
    /// Between declarations in a source file.
    TopLevel,
    /// At the start of a statement or tail expression in a scope.
    Statement,
    /// Somewhere an expression is expected.
    Expression,
    /// Somewhere a type specification is expected.
    Type,
    /// A path in a `use` declaration.
    ImportPath,
    /// A name being introduced rather than referenced.
    NewIdentifier,
    /// One particular grammar keyword is required here.
    Keyword(&'static str),
    /// Inside a comment or string literal.
    Suppressed,
}

impl CompletionSite {
    pub(super) fn priority(self) -> u8 {
        match self {
            Self::Unknown => 0,
            Self::TopLevel => 1,
            // These intentionally tie: the parser records Statement before
            // descending into a direct expression. First-wins preserves that
            // distinction, while a nested expression recorded later is not
            // overwritten when the scope closes at the same cursor.
            Self::Statement | Self::Expression => 2,
            Self::Type | Self::ImportPath | Self::Keyword(_) => 3,
            Self::NewIdentifier => 4,
            Self::Suppressed => 5,
        }
    }
}

/// A syntax error with the byte span (into the original input) it occurred at.
#[derive(Debug, Clone)]
pub struct ParseError {
    pub span: Span,
    pub message: String,
}

/// Classify the grammar position at `cursor` for editor completion.
///
/// The regular recovery parser does the classification, so half-written code
/// follows the same grammar as a complete source file. Strings and comments
/// are suppressed before parsing because the lexer deliberately skips
/// comments and would otherwise classify the token following one.
pub fn completion_site(input: &str, cursor: usize) -> CompletionSite {
    let cursor = cursor.min(input.len());
    if cursor_is_suppressed(input, cursor) {
        return CompletionSite::Suppressed;
    }
    let normalized = input.trim_start_matches(char::is_whitespace);
    let offset_base = input.len() - normalized.len();
    let mut parser = grammar::Parser::for_completion(normalized, offset_base, cursor);
    parser.parse_root();
    parser.completion_site().unwrap_or(CompletionSite::Unknown)
}

/// Whether the cursor is after the opening delimiter of an unfinished string,
/// line comment, or nested block comment. Scanning only to the cursor also
/// handles a cursor in the middle of an otherwise complete token.
fn cursor_is_suppressed(input: &str, cursor: usize) -> bool {
    let bytes = &input.as_bytes()[..cursor];
    let mut index = 0;
    let mut block_depth = 0usize;
    let mut string = false;
    let mut escaped = false;
    let mut line_comment = false;
    while index < bytes.len() {
        let byte = bytes[index];
        let next = bytes.get(index + 1).copied();
        if line_comment {
            if matches!(byte, b'\n' | b'\r') {
                line_comment = false;
            }
            index += 1;
            continue;
        }
        if block_depth > 0 {
            if byte == b'/' && next == Some(b'*') {
                block_depth += 1;
                index += 2;
            } else if byte == b'*' && next == Some(b'/') {
                block_depth -= 1;
                index += 2;
            } else {
                index += 1;
            }
            continue;
        }
        if string {
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                string = false;
            }
            index += 1;
            continue;
        }
        if byte == b'/' && next == Some(b'/') {
            line_comment = true;
            index += 2;
        } else if byte == b'/' && next == Some(b'*') {
            block_depth = 1;
            index += 2;
        } else {
            if byte == b'"' {
                string = true;
            }
            index += 1;
        }
    }
    line_comment || block_depth > 0 || string
}

/// Parse a whole source file into an [`AnnotatedParseAst`].
///
/// On success the returned AST borrows nothing from `input` directly — the
/// annotation pass re-slices identifier/string text from the shared `ArcStr`
/// by span, so spans must be byte-exact (they index the original, untrimmed
/// input). On any syntax error, returns every collected diagnostic.
pub fn parse_ast(input: ArcStr, path: PathBuf) -> Result<AnnotatedParseAst, Vec<ParseError>> {
    let input_for_ast = input.clone();
    let normalized = input.trim_start_matches(char::is_whitespace);
    let offset_base = input.len() - normalized.len();

    let mut parser = grammar::Parser::new(normalized, offset_base);
    let ast = parser.parse_root();
    if !parser.errors.is_empty() {
        return Err(parser.finish_errors(offset_base, input.len()));
    }
    let unsupported = ast
        .decls
        .iter()
        .filter_map(|decl| match decl {
            Decl::Constant(decl) => Some(ParseError {
                span: decl.name.span,
                message: "constant declarations are not implemented".to_string(),
            }),
            _ => None,
        })
        .collect::<Vec<_>>();
    if !unsupported.is_empty() {
        return Err(unsupported);
    }
    Ok(AnnotatedAst::new(input_for_ast, &ast, path))
}

/// Parse a single cell invocation (a `callExpr`) from raw input, as used by the
/// analyzer. Returns the borrowed-`&str` AST directly (no annotation
/// pass), so its `func`/literal values are read by the caller.
pub fn parse_cell(input: &str) -> Result<CallExpr<&str, ParseMetadata>, Vec<ParseError>> {
    let normalized = input.trim_start_matches(char::is_whitespace);
    let offset_base = input.len() - normalized.len();

    let mut parser = grammar::Parser::new(normalized, offset_base);
    let call = parser.parse_cell_entry();
    if !parser.errors.is_empty() {
        return Err(parser.finish_errors(offset_base, input.len()));
    }
    match call {
        Some(call) => Ok(call),
        None => Err(parser.finish_errors(offset_base, input.len())),
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use arcstr::ArcStr;

    fn parse(src: &str) -> Result<crate::parse::AnnotatedParseAst, Vec<super::ParseError>> {
        super::parse_ast(ArcStr::from(src), PathBuf::from("test.ar"))
    }

    /// Parse `cell __t__() { <body> }` and return whether it succeeded.
    fn snippet_ok(body: &str) -> bool {
        parse(&format!("cell __t__() {{ {body} }}")).is_ok()
    }

    fn site(marked: &str) -> super::CompletionSite {
        let cursor = marked.find('|').expect("test source has a cursor marker");
        let source = marked.replacen('|', "", 1);
        super::completion_site(&source, cursor)
    }

    #[test]
    fn completion_sites_follow_incomplete_grammar_positions() {
        use super::CompletionSite::*;

        for (source, expected) in [
            ("ce|", TopLevel),
            ("cell |", NewIdentifier),
            ("cell top(ar|: Float) {}", NewIdentifier),
            ("cell top(arg: Flo|) {}", Type),
            ("fn helper() -> | {}", Type),
            ("use |", ImportPath),
            ("use lib::shape |;", Keyword("as")),
            ("use lib::shape as |;", NewIdentifier),
            ("cell top() { let | = 1.; }", NewIdentifier),
            ("cell top() { for | in [] {} }", NewIdentifier),
            ("cell top() { let value = re|; }", Expression),
            ("cell top() { re|; }", Statement),
            ("cell top() { rect(|); }", Expression),
            ("cell top() { for item | [] {} }", Keyword("in")),
            ("cell top() { if true {} | {} }", Keyword("else")),
            ("// cell |", Suppressed),
            ("cell top() { let value = \"ce|ll\"; }", Suppressed),
            ("/* outer /* cell | */", Suppressed),
        ] {
            assert_eq!(site(source), expected, "{source}");
        }
    }

    #[test]
    fn accepts_valid_constructs() {
        let valid = [
            "let x = -a.b;",
            "let x = -a!;",
            "let x = !a!;",
            "let x = a as Float!;",
            "let x = 1.0.2;",
            "let x = 100.;",
            "let x = 1.foo;", // `<int>.field` is field access, not a float
            "let x = 1.0.bar;",
            "let x = f();",
            "let x = (a,);",
            "let x = (a, b,);",
            "let x = a[i].b;",
            "let x = 1 + 2 * 3;",
            "let x = -a * b + c < d;",
            "let x = a.b.c;",
            "let x = head(tail(arr));",
            "let x = foo(1, 2, x=3, y=4);",
            "let x = foo(x=1,);",
            "let x = (500 % 300) as Float;",
            "let x = r.1[0];",
            "x.0.1;",
            "if c {} else {};",
            "if c {} else {}",
            "let v = (t.0, t.1,);",
            "foo();",
            "if a < b {} else {}",
            "{ eq(a, b); }",
            "let r = rect(\"met1\", x0=0., y0=0., x1=400.)!;",
            "for i in range(3) { eq(i, i); }",
            "match k { A => 1, B => 2, }",
            "let x = a && b;",
            "let x = a || b;",
            "let x = !a && !b;",
            "let x = a && b!;",
            "let x = a < b && c >= d || !e;",
            "if a && b {} else {}",
            // Struct literals: named fields in any order, shorthand fields,
            // and a `..base` that must come last after a comma.
            "let p = Point { x: 1., y: 2. };",
            "let p = Point { x: 1., y: 2., };",
            "let p = geom::Point { x: 1., y: 2. };",
            "let p = lib::geom::Point { x: 1., y: 2. };",
            "let p = Point { x, y };",
            "let p = Point { x, y: 2. };",
            "let p = Point { ..base };",
            "let p = Point { x: 1., ..base };",
            "let p = Point { x, ..base };",
            "let p = Unit {};",
            "let x = Point { x: 1., y: 2. }.x;",
            "let p = Outer { inner: Inner { a: 1 }, list: cons(Inner { a: 2 }, []) };",
            "let p = Point { x: if c { 1. } else { 2. }, y: 2. };",
            "foo(Point { x: 1., y: 2. }, p=Point { ..base });",
            "let x = arr[Point { i: 0 }.i];",
            // A literal in an `if`/`match`/`for` head needs parentheses, but
            // is fine inside the construct's scopes and arms.
            "if (p == Point { x: 1., y: 2. }) {} else {}",
            "if c { Point { x: 1., y: 2. } } else { q }",
            "match k { A => Point { x: 1., y: 2. }, }",
            "for i in seq { let p = Point { x: i, y: i }; }",
            // A `match` in an `if` head still allows literals in its arms.
            "if match k { A => Point { x: 1. }, }.x == 1. {} else {}",
        ];
        for body in valid {
            assert!(snippet_ok(body), "should parse: `{body}`");
        }
    }

    #[test]
    fn rejects_invalid_constructs() {
        let invalid = [
            "let x = (a, b);",                 // tuple requires a trailing comma per element
            "let x = #old foo();",             // scope annotations are no longer syntax
            "let x = foo(x=1, 2);",            // positional after keyword
            "let x = ;",                       // missing expression
            "let x = (a, b;",                  // unterminated tuple
            "let x = match k {};",             // empty match: `matchArms` requires >= 1 arm
            "let x = 99999999999999999999;",   // out of range for Int
            "let x = 1 . 5;",                  // a float may not be split by trivia
            "let x = t.99999999999999999999;", // out of range tuple index
            // `name {` in an `if`/`match`/`for` head is the construct's scope.
            "if p == Point { x: 1. } {} else {}",
            "match Point { x: 1. } { A => 1, }",
            "for p in Point { xs: [] }.xs {}",
            "let p = Point { x: };",         // missing value
            "let p = Point { x: 1. ..b };",  // a comma is required before `..`
            "let p = Point { ..b, };",       // no comma after the base
            "let p = Point { ..b, x: 1. };", // the base must come last
            "let p = Point { x.y };",        // a field is a bare identifier
            "let p = Point { x: 1. y: 2. };",
        ];
        for body in invalid {
            assert!(!snippet_ok(body), "should be rejected: `{body}`");
        }
    }

    #[test]
    fn struct_declarations_take_type_specs() {
        assert!(parse("struct S { a: [Float], b: (Int, Int), c: Other, }").is_ok());
        assert!(parse("struct Unit {}").is_ok());
        assert!(parse("struct S { a }").is_err());
        assert!(parse("struct S { a: }").is_err());
        assert!(parse("struct S { a: Float b: Float }").is_err());
    }

    /// A shorthand field desugars to `name: name` with the value at the name's
    /// own span, so later passes see an ordinary field.
    #[test]
    fn shorthand_struct_fields_desugar_to_a_path_at_the_name() {
        use crate::ast::{Decl, Expr, Statement};

        let ast = parse("cell __t__() { let p = Point { x, y: 1., ..q }; }").unwrap();
        let Decl::Cell(cell) = &ast.ast.decls[0] else {
            panic!("expected a cell");
        };
        let Statement::LetBinding(binding) = &cell.scope.stmts[0] else {
            panic!("expected a let binding");
        };
        let Expr::StructLit(lit) = &binding.value else {
            panic!("expected a struct literal");
        };
        assert_eq!(lit.path.path.len(), 1);
        assert_eq!(lit.fields.len(), 2);
        assert!(lit.fields[0].shorthand);
        assert_eq!(lit.fields[0].value.span(), lit.fields[0].name.span);
        assert!(matches!(
            &lit.fields[0].value,
            Expr::IdentPath(path) if path.path.len() == 1 && path.path[0].name == "x"
        ));
        assert!(!lit.fields[1].shorthand);
        assert!(matches!(lit.base, Some(Expr::IdentPath(_))));
        // The literal spans from its path to the closing brace.
        assert_eq!(
            &ast.text[lit.span.start()..lit.span.end()],
            "Point { x, y: 1., ..q }"
        );
    }

    /// Renders an expression fully parenthesized.
    fn shape<S: std::fmt::Display, T: crate::ast::AstMetadata>(
        expr: &crate::ast::Expr<S, T>,
    ) -> String {
        use crate::ast::{ArithOp, BinOp, BoolOp, ComparisonOp, Expr, UnaryOp};
        match expr {
            Expr::BinOp(e) => {
                let op = match e.op {
                    BinOp::Bool(BoolOp::Or) => "||",
                    BinOp::Bool(BoolOp::And) => "&&",
                    BinOp::Cmp(ComparisonOp::Eq) => "==",
                    BinOp::Cmp(ComparisonOp::Ne) => "!=",
                    BinOp::Cmp(ComparisonOp::Geq) => ">=",
                    BinOp::Cmp(ComparisonOp::Gt) => ">",
                    BinOp::Cmp(ComparisonOp::Leq) => "<=",
                    BinOp::Cmp(ComparisonOp::Lt) => "<",
                    BinOp::Arith(ArithOp::Add) => "+",
                    BinOp::Arith(ArithOp::Sub) => "-",
                    BinOp::Arith(ArithOp::Mul) => "*",
                    BinOp::Arith(ArithOp::Div) => "/",
                    BinOp::Arith(ArithOp::Rem) => "%",
                };
                format!("({} {op} {})", shape(&e.left), shape(&e.right))
            }
            Expr::UnaryOp(e) => {
                let op = match e.op {
                    UnaryOp::Not => "!",
                    UnaryOp::Neg => "-",
                };
                format!("({op}{})", shape(&e.operand))
            }
            Expr::Emit(e) => format!("({}!)", shape(&e.value)),
            Expr::IdentPath(p) => p
                .path
                .iter()
                .map(|ident| ident.name.to_string())
                .collect::<Vec<_>>()
                .join("::"),
            Expr::BoolLiteral(b) => b.value.to_string(),
            Expr::IntLiteral(i) => i.value.to_string(),
            other => panic!(
                "shape() does not render this expression kind (span {:?})",
                other.span()
            ),
        }
    }

    /// Parses `let x = <expr>;` in a cell body and renders the expression.
    fn expr_shape(expr: &str) -> String {
        use crate::ast::{Decl, Statement};

        let src = format!("cell __t__() {{ let x = {expr}; }}");
        let mut parser = super::grammar::Parser::new(&src, 0);
        let ast = parser.parse_root();
        assert!(parser.errors.is_empty(), "`{expr}`: {:?}", parser.errors);
        let Decl::Cell(cell) = &ast.decls[0] else {
            panic!("expected a cell decl");
        };
        let Statement::LetBinding(binding) = &cell.scope.stmts[0] else {
            panic!("expected a let binding");
        };
        shape(&binding.value)
    }

    #[test]
    fn boolean_operator_precedence_and_associativity() {
        // `||` binds loosest, then `&&`, then the comparisons, then the
        // arithmetic operators -- as in Rust. All are left-associative.
        for (expr, expected) in [
            ("a || b && c", "(a || (b && c))"),
            ("a && b || c", "((a && b) || c)"),
            ("a && b && c", "((a && b) && c)"),
            ("a || b || c", "((a || b) || c)"),
            ("a == b && c != d", "((a == b) && (c != d))"),
            ("a < b || c >= d", "((a < b) || (c >= d))"),
            ("a + b < c && d", "(((a + b) < c) && d)"),
            // Prefix `!` takes only its operand, so it binds tighter than
            // every infix operator but does not absorb a suffix.
            ("!a && b", "((!a) && b)"),
            ("!(a && b)", "(!(a && b))"),
            ("!a || !b && !c", "((!a) || ((!b) && (!c)))"),
            ("a && b!", "(a && (b!))"),
            ("!a!", "((!a)!)"),
        ] {
            assert_eq!(expr_shape(expr), expected, "parsing `{expr}`");
        }
    }

    #[test]
    fn unparsable_numeric_literals_are_reported() {
        // These used to silently evaluate to 0, compiling to wrong geometry.
        for (body, message) in [
            (
                "let x = 99999999999999999999;",
                "invalid integer literal `99999999999999999999`",
            ),
            ("let x = 1 . 5;", "invalid float literal `1 . 5`"),
        ] {
            let errors = parse(&format!("cell c() {{ {body} }}"))
                .expect_err("an unparsable literal should be rejected");
            assert!(
                errors.iter().any(|error| error.message == message),
                "`{body}` should report `{message}`, got {errors:?}"
            );
        }
    }

    #[test]
    fn leading_comment_is_allowed() {
        // The lexer skips `//` comments as trivia everywhere, so a comment
        // before the first declaration parses fine.
        assert!(parse("// header\ncell c() {}\n").is_ok());
        assert!(parse("  \n// c1\n// c2\nfn f() -> Float { 1. }\n").is_ok());
    }

    #[test]
    fn use_declarations_parse_paths_and_aliases() {
        use crate::ast::Decl;

        let src = "use geometry::width;\nuse lib::math::double as twice;\nfn f() {}";
        let ast = parse(src).expect("use declarations should parse");
        let Decl::Use(width) = &ast.ast.decls[0] else {
            panic!("expected a use declaration");
        };
        assert_eq!(
            width
                .path
                .iter()
                .map(|part| part.name.as_str())
                .collect::<Vec<_>>(),
            ["geometry", "width"]
        );
        assert!(width.alias.is_none());
        assert_eq!(
            &src[width.span.start()..width.span.end()],
            "use geometry::width;"
        );

        let Decl::Use(double) = &ast.ast.decls[1] else {
            panic!("expected an aliased use declaration");
        };
        assert_eq!(double.alias.as_ref().unwrap().name.as_str(), "twice");
        assert_eq!(
            &src[double.span.start()..double.span.end()],
            "use lib::math::double as twice;"
        );

        let error = parse("use width;").expect_err("a use must include a module path");
        assert!(
            error
                .iter()
                .any(|error| error.message.contains("must name an item in a module"))
        );
    }

    #[test]
    fn literal_values_and_spans() {
        use crate::ast::{Decl, Expr, Statement};

        // Assert the concrete AST variant *and* that each node's span re-slices
        // to exactly the source text it covers, rather than fuzzy-matching a
        // `{:#?}` dump.
        let src = "cell c() {\n  let f = 100.;\n  let s = rect(\"met1\");\n}\n";
        let mut parser = super::grammar::Parser::new(src, 0);
        let ast = parser.parse_root();
        assert!(parser.errors.is_empty(), "{:?}", parser.errors);

        let Decl::Cell(cell) = &ast.decls[0] else {
            panic!("expected a cell decl, got {:?}", ast.decls[0]);
        };

        // `let f = 100.;` — a FloatLiteral whose span is exactly `100.`.
        let Statement::LetBinding(let_f) = &cell.scope.stmts[0] else {
            panic!("expected a let binding, got {:?}", cell.scope.stmts[0]);
        };
        let Expr::FloatLiteral(f) = &let_f.value else {
            panic!("expected a FloatLiteral, got {:?}", let_f.value);
        };
        assert_eq!(f.value, 100.0);
        assert_eq!(&src[f.span.start()..f.span.end()], "100.");

        // `let s = rect("met1");` — the StringLiteral value trims the quotes,
        // but its span still covers them.
        let Statement::LetBinding(let_s) = &cell.scope.stmts[1] else {
            panic!("expected a let binding, got {:?}", cell.scope.stmts[1]);
        };
        let Expr::Call(call) = &let_s.value else {
            panic!("expected a call, got {:?}", let_s.value);
        };
        let Expr::StringLiteral(s) = &call.args.posargs[0] else {
            panic!("expected a StringLiteral, got {:?}", call.args.posargs[0]);
        };
        assert_eq!(s.value, "met1");
        assert_eq!(&src[s.span.start()..s.span.end()], "\"met1\"");
        assert_eq!(call.scope_order, 0);
    }

    #[test]
    fn scope_orders_are_lexical_and_reset_in_nested_scopes() {
        use crate::ast::{Decl, Expr, Statement};

        let src = "cell c() { rect(); alpha(); if true { beta(); } else {}; for i in range_full(0, 2) { gamma(); } { delta(); }; }";
        let mut parser = super::grammar::Parser::new(src, 0);
        let ast = parser.parse_root();
        assert!(parser.errors.is_empty(), "{:?}", parser.errors);
        let Decl::Cell(cell) = &ast.decls[0] else {
            panic!("expected cell");
        };

        let Statement::Expr {
            value: Expr::Call(rect),
            ..
        } = &cell.scope.stmts[0]
        else {
            panic!("expected rect call");
        };
        let Statement::Expr {
            value: Expr::Call(alpha),
            ..
        } = &cell.scope.stmts[1]
        else {
            panic!("expected alpha call");
        };
        let Statement::Expr {
            value: Expr::If(if_),
            ..
        } = &cell.scope.stmts[2]
        else {
            panic!("expected if");
        };
        let Statement::ForLoop(for_) = &cell.scope.stmts[3] else {
            panic!("expected for loop");
        };
        let Statement::Expr {
            value: Expr::Scope(block),
            ..
        } = &cell.scope.stmts[4]
        else {
            panic!("expected block");
        };

        // Builtins do not consume ordinals because they do not produce scopes.
        assert_eq!(rect.scope_order, 0);
        assert_eq!(alpha.scope_order, 0);
        assert_eq!(if_.scope_order, 1);
        assert_eq!(for_.scope_order, 2);
        assert_eq!(block.scope_order, 3);

        let Statement::Expr {
            value: Expr::Call(beta),
            ..
        } = &if_.then.stmts[0]
        else {
            panic!("expected nested call");
        };
        let Statement::Expr {
            value: Expr::Call(gamma),
            ..
        } = &for_.body.stmts[0]
        else {
            panic!("expected loop call");
        };
        let Statement::Expr {
            value: Expr::Call(delta),
            ..
        } = &block.stmts[0]
        else {
            panic!("expected block call");
        };
        assert_eq!(beta.scope_order, 0);
        assert_eq!(gamma.scope_order, 0);
        assert_eq!(delta.scope_order, 0);
    }

    #[test]
    fn int_dot_field_access_parses() {
        use crate::ast::{Decl, Expr, Statement};

        // `1.foo` is field access on an integer (`(1).foo`), not a malformed
        // float: the `.` before an identifier is a suffix, not a fractional
        // part. The greedy float assembly used to eat `1.` and strand `foo` (F4).
        let src = "cell c() { let x = 1.foo; }";
        let mut parser = super::grammar::Parser::new(src, 0);
        let ast = parser.parse_root();
        assert!(
            parser.errors.is_empty(),
            "`1.foo` should parse: {:?}",
            parser.errors
        );
        let Decl::Cell(cell) = &ast.decls[0] else {
            panic!()
        };
        let Statement::LetBinding(b) = &cell.scope.stmts[0] else {
            panic!()
        };
        let Expr::FieldAccess(fa) = &b.value else {
            panic!("expected a FieldAccess, got {:?}", b.value);
        };
        assert!(
            matches!(fa.base, Expr::IntLiteral(_)),
            "base should be an int literal, got {:?}",
            fa.base
        );
        assert_eq!(fa.field.name, "foo");
        assert_eq!(&src[fa.field.span.start()..fa.field.span.end()], "foo");
    }

    #[test]
    fn non_ascii_char_is_one_full_width_error_token() {
        use super::lexer::Lexer;
        use super::token::TokenKind;

        // `€` is a 3-byte UTF-8 char that begins no valid token. The lexer must
        // emit a single Error token spanning the whole char (advancing past its
        // continuation bytes to the next char boundary), so the following token
        // starts on a char boundary and slicing never lands mid-char (F12).
        let mut lex = Lexer::new("€x", 0);
        let err = lex.next_token();
        assert_eq!(err.kind, TokenKind::Error);
        assert_eq!(
            (err.start, err.end),
            (0, 3),
            "Error token should span all of `€`"
        );
        let ident = lex.next_token();
        assert_eq!(ident.kind, TokenKind::Ident);
        assert_eq!(
            (ident.start, ident.end),
            (3, 4),
            "`x` lexes cleanly after the bad char"
        );

        // An ASCII byte that begins no token stays one byte wide.
        let mut lex = Lexer::new("@", 0);
        let err = lex.next_token();
        assert_eq!(err.kind, TokenKind::Error);
        assert_eq!((err.start, err.end), (0, 1));
    }

    #[test]
    fn parse_cell_requires_a_single_invocation() {
        // The cell entry must parse to exactly one call expression and reach EOF.
        assert!(super::parse_cell("top()").is_ok());
        assert!(super::parse_cell("top(1., 5)").is_ok());
        // Trailing tokens after the call are no longer silently dropped (F3).
        assert!(super::parse_cell("top() junk").is_err());
        // Suffixed calls parse to a non-`Call` root, so they're rejected too.
        assert!(super::parse_cell("top()!").is_err());
        assert!(super::parse_cell("top().x").is_err());
        assert!(super::parse_cell("top()[0]").is_err());
        // Not a call at all.
        assert!(super::parse_cell("1 + 2").is_err());
    }

    #[test]
    fn tuple_types_parse() {
        // Empty tuple type `()` (the unit type), trailing commas, and nesting all
        // parse. Tuple types appear in `fn` signatures, so use whole programs (F2).
        for src in [
            "fn f() -> () {}",
            "fn f(x: ()) {}",
            "fn f(x: (Float, Int)) {}",
            "fn f(x: (Float, Int,)) {}",
            "fn f(x: [(Float, Int)]) -> (Int,) {}",
        ] {
            assert!(parse(src).is_ok(), "should parse: `{src}`");
        }
    }

    #[test]
    fn default_values_parse() {
        use crate::ast::{Decl, Expr};

        let src = "fn f(a: Float, b: Float = 1., c: [Int] = []) {}\ncell c(n: Int = 2 * 3) {}";
        let mut parser = super::grammar::Parser::new(src, 0);
        let ast = parser.parse_root();
        assert!(parser.errors.is_empty(), "{:?}", parser.errors);
        let [Decl::Fn(f), Decl::Cell(c)] = ast.decls.as_slice() else {
            panic!("expected a fn and a cell, got {:?}", ast.decls);
        };
        assert!(f.args[0].default.is_none());
        let b = f.args[1].default.as_ref().expect("`b` has a default");
        assert!(matches!(b, Expr::FloatLiteral(_)));
        assert_eq!(&src[b.span().start()..b.span().end()], "1.");
        assert!(matches!(f.args[2].default, Some(Expr::SeqNil(_))));
        let n = c.args[0].default.as_ref().expect("`n` has a default");
        assert_eq!(&src[n.span().start()..n.span().end()], "2 * 3");

        for src in [
            "fn f(a: Float = ) {}",
            "cell c(n: Int = 1 {}",
            "fn f(a = 1.) {}",
        ] {
            assert!(parse(src).is_err(), "should be rejected: `{src}`");
        }
    }

    #[test]
    fn distinct_diagnostics_at_same_offset_are_kept() {
        // `foo(` then `}`: the `}` is simultaneously where an expression, a `)`,
        // and a `;` were expected — independent diagnostics at one byte offset.
        // They must not collapse into a single error keyed only on position (F6).
        let errs = parse("cell c() { let x = foo( }").unwrap_err();
        let distinct: std::collections::BTreeSet<_> =
            errs.iter().map(|e| e.message.clone()).collect();
        assert!(
            distinct.len() >= 2,
            "distinct diagnostics collapsed to one: {errs:#?}"
        );
    }

    fn collect_ar(root: &Path, out: &mut Vec<PathBuf>) {
        if !root.exists() {
            return;
        }
        for entry in std::fs::read_dir(root).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                collect_ar(&path, out);
            } else if path.extension().and_then(|e| e.to_str()) == Some("ar") {
                out.push(path);
            }
        }
    }

    /// Every grammar-valid `.ar` file in the repo parses without error.
    #[test]
    fn corpus_parses() {
        let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let repo = manifest.parent().unwrap().parent().unwrap();
        let mut files = Vec::new();
        collect_ar(&repo.join("examples"), &mut files);
        collect_ar(&repo.join("pdks"), &mut files);
        collect_ar(&manifest.join("src").join("std"), &mut files);
        // Scratch fixtures that intentionally use constructs outside the
        // grammar (and are not referenced by any test).
        files.retain(|p| {
            !p.ends_with(Path::new("defer/lib.ar")) && !p.ends_with(Path::new("testing/lib.ar"))
        });
        assert!(!files.is_empty());

        for path in files {
            let src = std::fs::read_to_string(&path).unwrap();
            let r = super::parse_ast(ArcStr::from(src), path.clone());
            assert!(
                r.is_ok(),
                "failed to parse {}: {:?}",
                path.display(),
                r.err()
            );
        }
    }

    /// Synthetic large program for the throughput benchmark.
    fn gen_program(n_cells: usize) -> String {
        let mut s = String::from(
            "fn helper(a: Float, b: Float) -> Float {\n  if a < b { a } else { b }\n}\n",
        );
        for i in 0..n_cells {
            s.push_str(&format!(
                "cell cell_{i}(x: Float, y: Float) {{\n    \
                 let r = rect(\"met1\", x0=0., y0=0., x1=x, y1=y)!;\n    \
                 let a = (x + y) * 2. - 3. / 4.;\n    \
                 let b = helper(a, x);\n    \
                 let c = head(tail(cons(1., cons(2., []))));\n    \
                 eq(r.x1, a + b);\n    \
                 if x < y {{ eq(r.y1, x); }} else {{ eq(r.y1, y); }}\n    \
                 let t = (x, y, a,);\n    \
                 eq(t.0, t.1);\n\
                 }}\n"
            ));
        }
        s
    }

    /// Reports parser throughput (lex + parse to AST, excluding the annotation
    /// pass). Ignored by default; run with:
    /// `cargo test -p argonc --release -- --ignored --nocapture parser_throughput`.
    #[test]
    #[ignore = "perf benchmark"]
    fn parser_throughput() {
        let program = gen_program(400);
        let bytes = program.len();
        let normalized = program.trim_start_matches(char::is_whitespace);
        let offset_base = program.len() - normalized.len();

        let reps = 50;
        let mut best = std::time::Duration::MAX;
        for _ in 0..reps {
            let start = std::time::Instant::now();
            let mut parser = super::grammar::Parser::new(normalized, offset_base);
            let ast = parser.parse_root();
            best = best.min(start.elapsed());
            std::hint::black_box(ast.decls.len());
        }
        eprintln!(
            "\nparser throughput: {bytes} bytes in {best:?} = {:.1} MB/s (best of {reps})\n",
            bytes as f64 / best.as_secs_f64() / 1e6
        );
    }
}
