//! Hand-written, zero-copy parser for the Argon language.
//!
//! Replaces the ANTLR-generated parser: a streaming byte lexer ([`lexer`]) feeds
//! a single-pass recursive-descent + Pratt parser ([`grammar`]) that builds the
//! AST directly, borrowing all identifier/string text from the source. The two
//! public entry points match the contract the rest of the compiler expects.

mod grammar;
mod lexer;
mod token;

use std::path::PathBuf;

use arcstr::ArcStr;
use cfgrammar::Span;

use crate::ast::CallExpr;
use crate::ast::annotated::AnnotatedAst;
use crate::parse::{AnnotatedParseAst, ParseMetadata};

/// A syntax error with the byte span (into the original input) it occurred at.
/// Shape-compatible with the old `antlr::AntlrParseError`.
#[derive(Debug, Clone)]
pub struct ParseError {
    pub span: Span,
    pub message: String,
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
    Ok(AnnotatedAst::new(input_for_ast, &ast, path))
}

/// Parse a single cell invocation (a `callExpr`) from raw input, as used by the
/// language server. Returns the borrowed-`&str` AST directly (no annotation
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

    #[test]
    fn accepts_valid_constructs() {
        let valid = [
            "let x = -a.b;",
            "let x = -a!;",
            "let x = !a!;",
            "let x = a as Float!;",
            "let x = 1.0.2;",
            "let x = 100.;",
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
            "#scope0 foo();",
            "#scope0 if a < b {} else {}",
            "#scope0 { eq(a, b); }",
            "let r = rect(\"met1\", x0=0., y0=0., x1=400.)!;",
            "for i in range(3) { eq(i, i); }",
            "match k { A => 1, B => 2, }",
        ];
        for body in valid {
            assert!(snippet_ok(body), "should parse: `{body}`");
        }
    }

    #[test]
    fn rejects_invalid_constructs() {
        let invalid = [
            "let x = (a, b);",      // tuple requires a trailing comma per element
            "let x = #y foo;",      // annotation on a bare path
            "let x = foo(x=1, 2);", // positional after keyword
            "let x = ;",            // missing expression
            "let x = (a, b;",       // unterminated tuple
        ];
        for body in invalid {
            assert!(!snippet_ok(body), "should be rejected: `{body}`");
        }
    }

    #[test]
    fn leading_comment_is_allowed() {
        // The lexer skips `//` comments as trivia everywhere, so a comment
        // before the first declaration parses fine (ANTLR rejected this).
        assert!(parse("// header\ncell c() {}\n").is_ok());
        assert!(parse("  \n// c1\n// c2\nfn f() -> Float { 1. }\n").is_ok());
    }

    #[test]
    fn literal_values_and_spans() {
        // Float value and span (covers the `100.` form), string trimming, and
        // that an annotation strips the leading `#`.
        let ast =
            parse("cell c() {\n  let f = 100.;\n  let s = rect(\"met1\");\n}\n").expect("parses");
        let dump = format!("{:#?}", ast.ast);
        assert!(dump.contains("value: 100.0"), "float value 100.0:\n{dump}");
        assert!(
            dump.contains("\"met1\""),
            "string value met1 (quotes trimmed)"
        );

        let ann = parse("cell c() {\n  #scope0 foo();\n}\n").expect("parses");
        let dump = format!("{:#?}", ann.ast);
        // The scope-annotation ident is `scope0` (no `#`).
        assert!(
            dump.contains("name: \"scope0\""),
            "annotation strips '#':\n{dump}"
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
            "fn helper(a: Float, b: Float) -> Float {\n  #scope0 if a < b { a } else { b }\n}\n",
        );
        for i in 0..n_cells {
            s.push_str(&format!(
                "cell cell_{i}(x: Float, y: Float) {{\n    \
                 let r = rect(\"met1\", x0=0., y0=0., x1=x, y1=y)!;\n    \
                 let a = (x + y) * 2. - 3. / 4.;\n    \
                 let b = helper(a, x);\n    \
                 let c = head(tail(cons(1., cons(2., []))));\n    \
                 eq(r.x1, a + b);\n    \
                 #scope0 if x < y {{ eq(r.y1, x); }} else {{ eq(r.y1, y); }}\n    \
                 let t = (x, y, a,);\n    \
                 eq(t.0, t.1);\n\
                 }}\n"
            ));
        }
        s
    }

    /// Reports parser throughput (lex + parse to AST, excluding the annotation
    /// pass). Ignored by default; run with:
    /// `cargo test -p compiler --release -- --ignored --nocapture parser_throughput`.
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
