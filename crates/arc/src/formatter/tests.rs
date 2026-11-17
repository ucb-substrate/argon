use std::fs;
use std::path::{Path, PathBuf};

use argonc::parse::parse_source_text;
use serde_json::Value;

use super::tokens::{TokenKind, tokenize};
use super::{MAX_WIDTH, format_source, format_source_with_width};

/// The parsed AST of `source` as JSON without spans, or `None` if it does not
/// parse.
fn ast_without_spans(source: &str) -> Option<Value> {
    fn strip(value: &mut Value) {
        match value {
            Value::Object(object) => {
                object.remove("span");
                if let Some(public) = object.get_mut("public") {
                    *public = Value::Bool(!public.is_null());
                }
                object.values_mut().for_each(strip);
            }
            Value::Array(items) => items.iter_mut().for_each(strip),
            _ => {}
        }
    }
    let ast = parse_source_text(source, PathBuf::from("format.ar")).ok()?;
    let mut value = serde_json::to_value(&ast.ast).expect("an AST serializes");
    strip(&mut value);
    Some(value)
}

fn comments(source: &str) -> Vec<&str> {
    tokenize(source)
        .into_iter()
        .filter(|token| token.kind == TokenKind::Comment)
        .map(|token| source[token.start..token.end].trim_end())
        .collect()
}

/// Asserts that `formatted` means the same as `source`: the same AST apart
/// from spans, and the same comments in the same order.
fn assert_equivalent(name: &str, source: &str, formatted: &str) {
    let before = ast_without_spans(source);
    if before.is_some() {
        assert!(
            ast_without_spans(formatted) == before,
            "formatting changed the AST of {name}:\n{formatted}"
        );
    }
    assert_eq!(
        comments(source),
        comments(formatted),
        "formatting changed the comments of {name}"
    );
}

/// Formats `source` to `width` columns, checking that the result means the
/// same and formats to itself.
fn format_checked(source: &str, width: usize) -> String {
    let formatted = format_source_with_width(source, width);
    assert_equivalent("the test source", source, &formatted);
    assert_eq!(
        format_source_with_width(&formatted, width),
        formatted,
        "formatting is not idempotent"
    );
    formatted
}

#[test]
fn no_wrap_comment_keeps_long_lines() {
    let source = "// Transcribed geometry.\n// arc-fmt: no-wrap\n\ncell top() {\n      let pins = [tap0.vpwr, reset_inv.vpwr, tap1.vpwr, clk_delay.vdd_port];\n}\n";
    assert_eq!(
        format_checked(source, 60),
        "// Transcribed geometry.\n// arc-fmt: no-wrap\n\ncell top() {\n    let pins = [tap0.vpwr, reset_inv.vpwr, tap1.vpwr, clk_delay.vdd_port];\n}\n"
    );
}

#[test]
fn return_type_on_its_own_line_is_indented() {
    let source = "extern \"m\" fn f(a: Int, b: Int)\n-> Int;\n";
    assert_eq!(
        format_checked(source, 100),
        "extern \"m\" fn f(a: Int, b: Int)\n    -> Int;\n"
    );
}

#[test]
fn no_wrap_comment_after_code_is_ignored() {
    let source = "cell top() {\n    // arc-fmt: no-wrap\n    let pins = [tap0.vpwr, reset_inv.vpwr, tap1.vpwr, clk_delay.vdd_port];\n}\n";
    assert!(format_checked(source, 60).contains("[\n"));
}

#[test]
fn long_list_puts_one_element_per_line() {
    let source = "cell top() {\n    let pins = [tap0.vpwr, reset_inv.vpwr, tap1.vpwr, clk_delay.vdd_port];\n}\n";
    assert_eq!(
        format_checked(source, 60),
        "cell top() {\n    let pins = [\n        tap0.vpwr,\n        reset_inv.vpwr,\n        tap1.vpwr,\n        clk_delay.vdd_port,\n    ];\n}\n"
    );
}

#[test]
fn short_constructs_stay_on_one_line() {
    let source = "cell top() {\n    let r = rect(\"met1\", x0=0., y0=0., x1=100., y1=200.);\n}\n";
    assert_eq!(format_checked(source, MAX_WIDTH), source);
}

#[test]
fn long_call_puts_each_argument_and_keyword_on_a_line() {
    let source = "cell top() {\n    let buf = inst(cells::sky130_fd_sc_hs__buf_16(), x=clk_pulse_buf_x + 10560., y=y0, reflect=true, angle=180);\n}\n";
    assert_eq!(
        format_checked(source, MAX_WIDTH),
        r#"cell top() {
    let buf = inst(
        cells::sky130_fd_sc_hs__buf_16(),
        x=clk_pulse_buf_x + 10560.,
        y=y0,
        reflect=true,
        angle=180,
    );
}
"#
    );
}

#[test]
fn nested_groups_break_until_each_line_fits() {
    let source = "cell top() {\n    let spans = row_spans(concat(inv_chain_gaps(t, clkp_delay_invs(), 1), concat(inv_chain_gaps(clkpd_delay_x, p.wlen_pulse_invs, 1), sr_latch_gaps(wl_ctl_x))), 0., wl_ctl_x + blocks::sr_latch_w() + t);\n}\n";
    assert_eq!(
        format_checked(source, 80),
        r#"cell top() {
    let spans = row_spans(
        concat(
            inv_chain_gaps(t, clkp_delay_invs(), 1),
            concat(
                inv_chain_gaps(clkpd_delay_x, p.wlen_pulse_invs, 1),
                sr_latch_gaps(wl_ctl_x),
            ),
        ),
        0.,
        wl_ctl_x + blocks::sr_latch_w() + t,
    );
}
"#
    );
}

#[test]
fn a_single_list_argument_keeps_the_call_brackets_beside_its_own() {
    let source = "cell top() {\n    pub let vdd = router::merge_rects([router::merge_rects([tap0.vpwr, reset_inv.vpwr, tap1.vpwr]), router::merge_rects([tap8.vpwr, clkp_delay.vdd_port])]);\n}\n";
    assert_eq!(
        format_checked(source, 70),
        r#"cell top() {
    pub let vdd = router::merge_rects([
        router::merge_rects([tap0.vpwr, reset_inv.vpwr, tap1.vpwr]),
        router::merge_rects([tap8.vpwr, clkp_delay.vdd_port]),
    ]);
}
"#
    );
}

#[test]
fn struct_literals_break_and_keep_the_base_last_without_a_comma() {
    let source = "struct P { a: Float, b: Float, c: Float }\n\nfn f(base: P) -> P {\n    P { a: 1000000., b: 2000000., ..base }\n}\n";
    assert_eq!(
        format_checked(source, 30),
        r#"struct P {
    a: Float,
    b: Float,
    c: Float,
}

fn f(base: P) -> P {
    P {
        a: 1000000.,
        b: 2000000.,
        ..base
    }
}
"#
    );
}

#[test]
fn use_groups_pack_their_items() {
    let source = "use lib::logic::router::{access, access_right, all_ops, block, blockages, corner_ll, corner_lr, corner_ul, corner_ur, draw_m1, draw_m2};\n";
    assert_eq!(
        format_checked(source, 60),
        r#"use lib::logic::router::{
    access, access_right, all_ops, block, blockages,
    corner_ll, corner_lr, corner_ul, corner_ur, draw_m1,
    draw_m2,
};
"#
    );
}

#[test]
fn lists_of_short_literals_pack_several_to_a_line() {
    let source = "cell top() {\n    let xs = [0., 1.5, -2., 3e-3, 4., 5., 6., 7., 8., 9., 10., 11., 12., 13., 14., 15.];\n}\n";
    assert_eq!(
        format_checked(source, 50),
        "cell top() {\n    let xs = [\n        0., 1.5, -2., 3e-3, 4., 5., 6., 7., 8.,\n        9., 10., 11., 12., 13., 14., 15.,\n    ];\n}\n"
    );
}

#[test]
fn comments_stay_with_their_lines() {
    let source = r#"cell top() {
    let pins = [
        // The first row.
        merge(tap0.vpwr, reset_inv.vpwr, tap1.vpwr), // Row zero.
        merge(tap8.vpwr, clkp_delay.vdd_port),
    ];
    rail_pins("vdd", y2 + blocks::row_h(), shared_spans(spans1, spans2)); // Joined outside.
}
"#;
    assert_eq!(
        format_checked(source, 50),
        r#"cell top() {
    let pins = [
        // The first row.
        merge(
            tap0.vpwr,
            reset_inv.vpwr,
            tap1.vpwr,
        ), // Row zero.
        merge(tap8.vpwr, clkp_delay.vdd_port),
    ];
    rail_pins(
        "vdd",
        y2 + blocks::row_h(),
        shared_spans(spans1, spans2),
    ); // Joined outside.
}
"#
    );
}

#[test]
fn bodies_break_before_the_lists_on_their_line() {
    let source = "fn pick(a: Float, b: Float) -> Float {\n    let w = if a > b { max_float(a, b) * 2. } else { min_float(a, b) / 2. };\n    w\n}\n";
    assert_eq!(
        format_checked(source, 60),
        r#"fn pick(a: Float, b: Float) -> Float {
    let w = if a > b {
        max_float(a, b) * 2.
    } else {
        min_float(a, b) / 2.
    };
    w
}
"#
    );
}

#[test]
fn match_arms_and_one_line_bodies_break() {
    let source = "fn f(x: Option<Int>) -> Int { match x { Some(v) => first_value(v), None => second_value(), } }\n";
    assert_eq!(
        format_checked(source, 40),
        r#"fn f(x: Option<Int>) -> Int {
    match x {
        Some(v) => first_value(v),
        None => second_value(),
    }
}
"#
    );
}

#[test]
fn one_line_bodies_put_each_statement_on_a_line() {
    let source = "cell top(xs: [Float]) { for x in xs { rect(\"met1\", x0=x, y0=0., w=100., h=100.); } finish(xs); }\n";
    assert_eq!(
        format_checked(source, 60),
        r#"cell top(xs: [Float]) {
    for x in xs {
        rect("met1", x0=x, y0=0., w=100., h=100.);
    }
    finish(xs);
}
"#
    );
}

#[test]
fn a_comma_after_a_single_element_is_kept() {
    let source = "cell top() {\n    let r = merge_rects([first_rectangle, second_rectangle, third_rectangle],);\n}\n";
    assert_eq!(
        format_checked(source, 64),
        r#"cell top() {
    let r = merge_rects(
        [first_rectangle, second_rectangle, third_rectangle],
    );
}
"#
    );
}

#[test]
fn comprehensions_break_at_their_body() {
    let source = "cell top() {\n    let pads = std::flatten([for k in range_full(0, pairs, 1) { boundary_pad(x + (2 * k + 1) as Float * bitcell_width(), 1960., 2055.) }]);\n}\n";
    assert_eq!(
        format_checked(source, MAX_WIDTH),
        r#"cell top() {
    let pads = std::flatten([for k in range_full(0, pairs, 1) {
        boundary_pad(x + (2 * k + 1) as Float * bitcell_width(), 1960., 2055.)
    }]);
}
"#
    );
}

#[test]
fn small_operands_of_long_expressions_stay_whole() {
    let source = "fn f(group: Int, bit: Int) -> Float {\n    rowend_width() + bitcell_width() + group as Float * array_group_width(mux_ratio) + bit as Float * bitcell_width()\n}\n\nfn g(a: Float) -> Float {\n    max_float(clamp_to_grid(first_bound, a), clamp_to_grid(second_bound, a)) - 4. * 6000.\n}\n";
    assert_eq!(
        format_checked(source, 80),
        r#"fn f(group: Int, bit: Int) -> Float {
    rowend_width() + bitcell_width() + group as Float * array_group_width(mux_ratio) + bit as Float * bitcell_width()
}

fn g(a: Float) -> Float {
    max_float(
        clamp_to_grid(first_bound, a),
        clamp_to_grid(second_bound, a),
    ) - 4. * 6000.
}
"#
    );
}

#[test]
fn parameters_break_without_splitting_types() {
    let source = "fn combine(first: Pair<Float, Float>, second: Pair<Float, Float>, scale: Float) -> Float {\n    scale\n}\n";
    assert_eq!(
        format_checked(source, 60),
        r#"fn combine(
    first: Pair<Float, Float>,
    second: Pair<Float, Float>,
    scale: Float,
) -> Float {
    scale
}
"#
    );
}

#[test]
fn one_element_tuples_keep_their_comma() {
    let source =
        "cell top() {\n    let t = (very_long_function_name(argument_one, argument_two),);\n}\n";
    let formatted = format_checked(source, 40);
    assert_eq!(
        formatted,
        r#"cell top() {
    let t = (
        very_long_function_name(
            argument_one,
            argument_two,
        ),
    );
}
"#
    );
}

#[test]
fn lines_that_open_several_brackets_indent_once() {
    let source = "cell top() {\nlet x = f([\na,\nb,\n]);\nlet y = g(h(\nc,\n), [\nd,\n]);\n}\n";
    assert_eq!(
        format_checked(source, MAX_WIDTH),
        "cell top() {\n    let x = f([\n        a,\n        b,\n    ]);\n    let y = g(h(\n        c,\n    ), [\n        d,\n    ]);\n}\n"
    );
}

#[test]
fn source_that_does_not_parse_is_only_reindented() {
    let source = "cell top() {\nlet pins = [tap0.vpwr, reset_inv.vpwr, tap1.vpwr, clk_delay.vdd_port, oops\n}\n";
    assert_eq!(
        format_source_with_width(source, 40),
        "cell top() {\n    let pins = [tap0.vpwr, reset_inv.vpwr, tap1.vpwr, clk_delay.vdd_port, oops\n}\n"
    );
}

fn argon_files(directory: &Path, files: &mut Vec<PathBuf>) {
    let mut entries: Vec<_> = fs::read_dir(directory)
        .expect("directory should be readable")
        .map(|entry| entry.expect("entry should be readable").path())
        .collect();
    entries.sort();
    for path in entries {
        if path.is_dir() {
            if path.file_name().is_some_and(|name| name != "target") {
                argon_files(&path, files);
            }
        } else if path.extension().is_some_and(|extension| extension == "ar") {
            files.push(path);
        }
    }
}

/// Every example and standard library source formats to source with the same
/// meaning and comments, and formatting it again changes nothing.
#[test]
fn examples_and_std_format_without_changing_meaning() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let mut files = Vec::new();
    argon_files(&root.join("examples"), &mut files);
    argon_files(&root.join("pdks"), &mut files);
    argon_files(&root.join("crates/compiler/src/std"), &mut files);
    assert!(files.len() > 50, "found only {} sources", files.len());

    let mut parsed = 0;
    for file in files {
        let name = file.display().to_string();
        let source = fs::read_to_string(&file).expect("source should be readable");
        let formatted = format_source(&source);
        assert_equivalent(&name, &source, &formatted);
        assert_eq!(
            format_source(&formatted),
            formatted,
            "formatting {name} is not idempotent"
        );
        parsed += usize::from(ast_without_spans(&source).is_some());
    }
    // Only sources that parse have their lines broken and their ASTs compared.
    assert!(parsed > 50, "only {parsed} sources parse");
}
