//! Source edits for schematic gestures: the statements that add elements,
//! signals, and connections, the rewrites of a call's keywords and
//! arguments, and the deletion of a statement with the references it leaves.

use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;
use std::ops::Range;
use std::path::PathBuf;

use arcstr::Substr;
use argonc::{
    ast::{CallExpr, CellDecl, Decl, Expr, Scope, Span, Statement},
    compile::{
        Align, CompiledCell, CompiledData, NodeSource, Schematic, SymbolItemKind, SymbolText,
        generated_box,
    },
    nav::{NavIndex, Target},
    parse::{self, AnnotatedParseAst, ParseMetadata, WorkspaceParseAst},
};

use crate::cell_edit::validate_identifier;
use crate::rpc::{
    BlockingReference, CallArgument, CallArguments, ClipboardItem, ClipboardKind, ElementPlacement,
    KeywordEdit, NewSymbolItem,
};

/// The pseudo-keyword of a `place` edit that rewrites the points of a line
/// or polygon, which are its first argument.
pub(crate) const POINTS: &str = "points";

type ParsedCall = CallExpr<Substr, ParseMetadata>;
type ParsedCell = CellDecl<Substr, ParseMetadata>;
type ParsedScope = Scope<Substr, ParseMetadata>;
type ParsedStatement = Statement<Substr, ParseMetadata>;
type ParsedExpr = Expr<Substr, ParseMetadata>;

/// Replacements per file, measured against the published source.
pub(crate) type FileReplacements = HashMap<PathBuf, Vec<Replacement>>;

const CHECK_PATH: &str = "/virtual/argon-check.ar";

/// What a call the GUI edits created.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SiteKind {
    Signal,
    Connect,
    Instance,
    Device,
    SymbolLine,
    SymbolPolygon,
    SymbolRect,
    SymbolCircle,
    SymbolArc,
    SymbolText,
    SymbolPin,
    /// The call in the cell body that a function drew a group of items
    /// under.
    SymbolGroup,
}

impl SiteKind {
    /// The keywords the GUI may rewrite on a call of this kind.
    pub(crate) fn keywords(self) -> &'static [&'static str] {
        match self {
            SiteKind::Signal => &["x", "y", "angle"],
            SiteKind::Connect => &["route"],
            SiteKind::Instance | SiteKind::Device => &["x", "y", "angle", "reflect"],
            SiteKind::SymbolLine => &[POINTS],
            SiteKind::SymbolPolygon => &[POINTS, "fill"],
            SiteKind::SymbolRect => &["x0", "y0", "x1", "y1", "fill"],
            SiteKind::SymbolCircle => &["x", "y", "r", "fill"],
            SiteKind::SymbolArc => &["x", "y", "r", "start", "end"],
            SiteKind::SymbolText => &["x", "y", "size", "align"],
            SiteKind::SymbolPin => &["x", "y", "label"],
            SiteKind::SymbolGroup => &[],
        }
    }

    pub(crate) fn describe(self) -> &'static str {
        match self {
            SiteKind::Signal => "signal",
            SiteKind::Connect => "connection",
            SiteKind::Instance => "instance",
            SiteKind::Device => "device",
            SiteKind::SymbolLine => "line",
            SiteKind::SymbolPolygon => "polygon",
            SiteKind::SymbolRect => "rectangle",
            SiteKind::SymbolCircle => "circle",
            SiteKind::SymbolArc => "arc",
            SiteKind::SymbolText => "text",
            SiteKind::SymbolPin => "pin",
            SiteKind::SymbolGroup => "group of symbol items",
        }
    }
}

/// The schematic or symbol site in `data` whose call is exactly `span`,
/// and whether the GUI may edit it.
pub(crate) fn site_of(data: &CompiledData, span: &Span) -> Option<(SiteKind, bool)> {
    data.cells.values().find_map(|cell| {
        let schematic = &cell.schematic;
        schematic
            .nodes
            .iter()
            .find_map(|node| match &node.source {
                NodeSource::Signal {
                    span: site,
                    editable,
                    ..
                } if site == span => Some((SiteKind::Signal, *editable)),
                _ => None,
            })
            .or_else(|| {
                schematic
                    .connections
                    .iter()
                    .find(|connection| connection.span == *span)
                    .map(|connection| (SiteKind::Connect, connection.editable))
            })
            .or_else(|| {
                schematic
                    .instances
                    .iter()
                    .find(|instance| instance.span.as_ref() == Some(span))
                    .map(|instance| (SiteKind::Instance, instance.editable))
            })
            .or_else(|| {
                schematic
                    .devices
                    .iter()
                    .find(|device| device.span.as_ref() == Some(span))
                    .map(|device| (SiteKind::Device, device.editable))
            })
            .or_else(|| symbol_site_of(cell, span))
    })
}

/// The symbol item, pin, or group origin of `cell` at `span`.
fn symbol_site_of(cell: &CompiledCell, span: &Span) -> Option<(SiteKind, bool)> {
    let symbol = cell.symbol.as_ref()?;
    let item = symbol.items.iter().find_map(|item| {
        let site = item.site.as_ref()?;
        (site.span == *span).then(|| {
            let kind = match item.kind {
                SymbolItemKind::Line { .. } => SiteKind::SymbolLine,
                SymbolItemKind::Polygon { .. } => SiteKind::SymbolPolygon,
                SymbolItemKind::Rect { .. } => SiteKind::SymbolRect,
                SymbolItemKind::Circle { .. } => SiteKind::SymbolCircle,
                SymbolItemKind::Arc { .. } => SiteKind::SymbolArc,
                SymbolItemKind::Text { .. } => SiteKind::SymbolText,
            };
            (kind, site.editable)
        })
    });
    item.or_else(|| {
        symbol.pins.iter().flatten().find_map(|pin| {
            let site = pin.site.as_ref()?;
            (site.span == *span).then_some((SiteKind::SymbolPin, site.editable))
        })
    })
    .or_else(|| {
        let origins = symbol
            .items
            .iter()
            .filter_map(|item| item.site.as_ref())
            .chain(
                symbol
                    .pins
                    .iter()
                    .flatten()
                    .filter_map(|pin| pin.site.as_ref()),
            )
            .filter_map(|site| site.origin.as_ref());
        origins
            .into_iter()
            .any(|origin| origin == span)
            .then_some((SiteKind::SymbolGroup, true))
    })
}

/// A replacement of the text in a byte range.
pub(crate) type Replacement = (Range<usize>, String);

/// The text that adds `values` as keywords to the call whose span is
/// `call_span`, before its closing parenthesis, following the call's layout.
pub(crate) fn ensure_keywords(
    source: &str,
    call_span: cfgrammar::Span,
    values: &[(String, String)],
) -> Option<Replacement> {
    let insertion = call_span.end().checked_sub(1)?;
    if source.as_bytes().get(insertion) != Some(&b')') {
        return None;
    }
    let previous = source[..insertion]
        .chars()
        .rev()
        .find(|character| !character.is_whitespace());
    let line_start = source[..insertion]
        .rfind('\n')
        .map_or(0, |newline| newline + 1);
    let closing_indent = &source[line_start..insertion];
    let closing_on_own_line = closing_indent.chars().all(char::is_whitespace);
    let separator = match (previous, closing_on_own_line) {
        (Some('('), _) | (Some(','), true) => "",
        (Some(','), false) => " ",
        _ => ", ",
    };
    let keywords = values
        .iter()
        .map(|(name, value)| format!("{name}={value}"))
        .collect::<Vec<_>>()
        .join(", ");
    let text = if closing_on_own_line {
        format!("    {separator}{keywords},\n{closing_indent}")
    } else {
        format!("{separator}{keywords}")
    };
    Some((insertion..insertion, text))
}

/// The range that removes keyword `index` of `call` together with the comma
/// that separates it from its neighbors.
fn keyword_removal(call: &CallExpr<Substr, ParseMetadata>, index: usize) -> Range<usize> {
    let kwargs = &call.args.kwargs;
    let kwarg = &kwargs[index];
    if let Some(next) = kwargs.get(index + 1) {
        return kwarg.span.start()..next.span.start();
    }
    let previous_end = if index > 0 {
        Some(kwargs[index - 1].span.end())
    } else {
        call.args.posargs.last().map(|arg| arg.span().end())
    };
    match previous_end {
        Some(end) => end..kwarg.span.end(),
        None => kwarg.span.start()..kwarg.span.end(),
    }
}

/// The replacements, in descending order of position, that give `call` the
/// keywords `edits`: a keyword with a value is rewritten or added, and one
/// without is removed.
pub(crate) fn keyword_replacements(
    source: &str,
    call: &CallExpr<Substr, ParseMetadata>,
    edits: &[KeywordEdit],
) -> Vec<Replacement> {
    let mut replacements = Vec::new();
    let mut missing = Vec::new();
    for edit in edits {
        let existing = call
            .args
            .kwargs
            .iter()
            .position(|kwarg| kwarg.name.name == edit.name);
        match (existing, &edit.value) {
            (Some(index), Some(value)) => {
                let value_span = call.args.kwargs[index].value.span();
                replacements.push((value_span.start()..value_span.end(), value.clone()));
            }
            (Some(index), None) => replacements.push((keyword_removal(call, index), String::new())),
            (None, Some(value)) => missing.push((edit.name.clone(), value.clone())),
            (None, None) => {}
        }
    }
    if !missing.is_empty()
        && let Some(insertion) = ensure_keywords(source, call.span, &missing)
    {
        replacements.push(insertion);
    }
    merge_removals(&mut replacements);
    replacements.sort_by_key(|replacement| std::cmp::Reverse(replacement.0.start));
    replacements
}

/// Joins removals that overlap or touch, which neighboring keywords produce
/// when both go, so that each range is applied to the text it was measured
/// on.
fn merge_removals(replacements: &mut Vec<Replacement>) {
    replacements.sort_by_key(|(range, _)| range.start);
    let mut merged: Vec<Replacement> = Vec::with_capacity(replacements.len());
    for (range, text) in replacements.drain(..) {
        if let Some((last, last_text)) = merged.last_mut()
            && text.is_empty()
            && last_text.is_empty()
            && range.start <= last.end
        {
            last.end = last.end.max(range.end);
            continue;
        }
        merged.push((range, text));
    }
    *replacements = merged;
}

/// `span` after `replacements` to its file: its start moves with the edits
/// before it, and its end with those before its end.
pub(crate) fn span_after(span: cfgrammar::Span, replacements: &[Replacement]) -> cfgrammar::Span {
    let shift = |offset: usize| -> usize {
        let mut shifted = offset as i64;
        for (range, text) in replacements {
            if range.start >= offset {
                continue;
            }
            if range.end <= offset {
                shifted += text.len() as i64 - range.len() as i64;
            } else {
                // The edit straddles the offset; the offset lands at its end.
                shifted = (range.start + text.len()) as i64;
            }
        }
        shifted.max(0) as usize
    };
    cfgrammar::Span::new(shift(span.start()), shift(span.end()))
}

// Statements.

/// The placement keywords of a new element.
fn placement_keywords(placement: &ElementPlacement) -> String {
    let (x, y) = placement.at;
    let mut text = format!("x={x}, y={y}");
    let angle = placement.angle.rem_euclid(360);
    if angle != 0 {
        write!(text, ", angle={angle}").expect("writing to a string");
    }
    if placement.reflect {
        text.push_str(", reflect=true");
    }
    text
}

/// A route as source text, such as `[(1, 2), (3, 4)]`.
pub(crate) fn route_text(route: &[(i64, i64)]) -> String {
    let corners = route
        .iter()
        .map(|(x, y)| format!("({x}, {y})"))
        .collect::<Vec<_>>()
        .join(", ");
    format!("[{corners}]")
}

/// The statement that declares a port or a net label at `at`.
pub(crate) fn signal_statement(name: &str, port: bool, (x, y): (i64, i64)) -> String {
    let prefix = if port { "pub let" } else { "let" };
    format!("{prefix} {name} = std::schematic::Signal(x={x}, y={y});")
}

/// The statement that connects `a` and `b`, with `route` when a wire draws it.
pub(crate) fn connect_statement(a: &str, b: &str, route: Option<&[(i64, i64)]>) -> String {
    match route {
        Some(route) => format!(
            "std::schematic::connect({a}, {b}, route={});",
            route_text(route)
        ),
        None => format!("std::schematic::connect({a}, {b});"),
    }
}

/// The statement that places an instance of `invocation`.
pub(crate) fn instance_statement(
    name: &str,
    invocation: &str,
    placement: &ElementPlacement,
) -> String {
    format!(
        "pub let {name} = std::schematic::inst({invocation}, {});",
        placement_keywords(placement)
    )
}

/// The model and parameters of a device as argument text, with an empty
/// model added when `args` names no model.
pub(crate) fn device_args(args: &str) -> String {
    let args = args.trim().trim_end_matches(',').trim();
    let keyword_first = args.split_once('=').is_some_and(|(name, _)| {
        let name = name.trim();
        !name.is_empty()
            && name
                .chars()
                .all(|character| character.is_ascii_alphanumeric() || character == '_')
    });
    if args.is_empty() {
        "\"\"".to_owned()
    } else if keyword_first {
        format!("\"\", {args}")
    } else {
        args.to_owned()
    }
}

/// The statement that places a device of `kind` with fresh terminals and
/// the model and parameters in `args`.
pub(crate) fn device_statement(
    name: &str,
    kind: &str,
    terminals: usize,
    args: &str,
    placement: &ElementPlacement,
) -> String {
    format!(
        "pub let {name} = std::schematic::device(std::schematic::DeviceKind::{kind}, \
         std::schematic::signals({terminals}), {}, {});",
        device_args(args),
        placement_keywords(placement)
    )
}

/// A `Float` literal with the fewest digits that read back as `value`,
/// always with a `.`.
pub(crate) fn float_literal(value: f64) -> String {
    let text = format!("{value}");
    if text.contains('.') || text.contains('e') || text.contains("inf") || text.contains("NaN") {
        text
    } else {
        format!("{text}.")
    }
}

/// A list of points as `[(x, y), …]` of `Float` literals.
pub(crate) fn points_text(points: &[(f64, f64)]) -> String {
    let points = points
        .iter()
        .map(|(x, y)| format!("({}, {})", float_literal(*x), float_literal(*y)))
        .collect::<Vec<_>>()
        .join(", ");
    format!("[{points}]")
}

/// The `size` and `align` keywords of a text item, left out at their
/// defaults.
fn text_keywords(x: f64, y: f64, size: f64, align: Align) -> String {
    let mut text = format!("x={}, y={}", float_literal(x), float_literal(y));
    if size != 1. {
        write!(text, ", size={}", float_literal(size)).expect("writing to a string");
    }
    if align != Align::Left {
        write!(text, ", align=std::symbol::Align::{}", align.name()).expect("writing to a string");
    }
    text
}

/// The statement that draws `item` in the cell `cell`, whose compiled
/// schematic is `schematic`.
pub(crate) fn symbol_statement(
    item: &NewSymbolItem,
    cell: &ParsedCell,
    schematic: &Schematic,
) -> Result<String, String> {
    let fill = |fill: bool| if fill { ", fill=true" } else { "" };
    Ok(match item {
        NewSymbolItem::Line { points } => {
            if points.len() < 2 {
                return Err("A line needs at least two points.".to_owned());
            }
            format!("std::symbol::line({});", points_text(points))
        }
        NewSymbolItem::Polygon { points, fill: f } => {
            if points.len() < 3 {
                return Err("A polygon needs at least three points.".to_owned());
            }
            format!("std::symbol::polygon({}{});", points_text(points), fill(*f))
        }
        NewSymbolItem::Rect {
            x0,
            y0,
            x1,
            y1,
            fill: f,
        } => format!(
            "std::symbol::rect(x0={}, y0={}, x1={}, y1={}{});",
            float_literal(*x0),
            float_literal(*y0),
            float_literal(*x1),
            float_literal(*y1),
            fill(*f)
        ),
        NewSymbolItem::Circle { x, y, r, fill: f } => format!(
            "std::symbol::circle(x={}, y={}, r={}{});",
            float_literal(*x),
            float_literal(*y),
            float_literal(*r),
            fill(*f)
        ),
        NewSymbolItem::Arc {
            x,
            y,
            r,
            start,
            end,
        } => format!(
            "std::symbol::arc(x={}, y={}, r={}, start={}, end={});",
            float_literal(*x),
            float_literal(*y),
            float_literal(*r),
            float_literal(*start),
            float_literal(*end)
        ),
        NewSymbolItem::Text {
            text,
            x,
            y,
            size,
            align,
        } => format!(
            "std::symbol::text({text:?}, {});",
            text_keywords(*x, *y, *size, *align)
        ),
        NewSymbolItem::InstName { x, y, size, align } => format!(
            "std::symbol::inst_name({});",
            text_keywords(*x, *y, *size, *align)
        ),
        NewSymbolItem::Param {
            name,
            x,
            y,
            size,
            align,
        } => {
            let parameter = cell.args.iter().any(|arg| arg.name.name == *name);
            let shadowed = cell
                .scope
                .stmts
                .iter()
                .flat_map(|statement| statement.let_names())
                .any(|binding| binding.name == *name);
            if !parameter || shadowed {
                return Err(format!(
                    "`{name}` is not a parameter of `{}` at the end of its body.",
                    cell.name.name
                ));
            }
            format!(
                "std::symbol::param({name:?}, {name}, {});",
                text_keywords(*x, *y, *size, *align)
            )
        }
        NewSymbolItem::Pin { port, x, y, label } => {
            let path = schematic
                .port_paths
                .get(*port)
                .copied()
                .flatten()
                .map(|path| schematic.path_text(path))
                .ok_or_else(|| {
                    let name = schematic
                        .ports
                        .get(*port)
                        .map(|_| schematic.port_name(*port))
                        .unwrap_or("?");
                    format!(
                        "Port `{name}` cannot be named from the cell body, so it cannot get a pin."
                    )
                })?;
            format!(
                "std::symbol::pin({path}, x={x}, y={y}{});",
                if *label { ", label=true" } else { "" }
            )
        }
    })
}

/// The statements that draw `cell`'s generated box as its own symbol, with
/// labeled pins in place of the box's pin names, so parents' wires stay
/// where they are.
pub(crate) fn create_symbol_statements(cell: &CompiledCell) -> Result<Vec<String>, String> {
    if cell.symbol.is_some() {
        return Err(format!("`{}` already draws a symbol.", cell.name));
    }
    let schematic = &cell.schematic;
    let pathless = (0..schematic.ports.len())
        .filter(|port| schematic.port_paths.get(*port).copied().flatten().is_none())
        .map(|port| format!("`{}`", schematic.port_name(port)))
        .collect::<Vec<_>>();
    if !pathless.is_empty() {
        return Err(format!(
            "Create Symbol needs every port to be nameable from the cell body; {} {} not.",
            pathless.join(", "),
            if pathless.len() == 1 { "is" } else { "are" }
        ));
    }
    let symbol = generated_box(schematic, &cell.name);
    let mut statements = Vec::new();
    let mut named = false;
    for item in &symbol.items {
        let statement = match &item.kind {
            SymbolItemKind::Rect {
                x0,
                y0,
                x1,
                y1,
                fill,
            } => NewSymbolItem::Rect {
                x0: *x0,
                y0: *y0,
                x1: *x1,
                y1: *y1,
                fill: *fill,
            },
            SymbolItemKind::Text {
                text: SymbolText::InstName,
                x,
                y,
                size,
                align,
            } => NewSymbolItem::InstName {
                x: *x,
                y: *y,
                size: *size,
                align: *align,
            },
            // The cell's name comes first among the literal texts; the pin
            // names after it become labels.
            SymbolItemKind::Text {
                text: SymbolText::Literal(text),
                x,
                y,
                size,
                align,
            } if !named && *text == cell.name => {
                named = true;
                NewSymbolItem::Text {
                    text: text.clone(),
                    x: *x,
                    y: *y,
                    size: *size,
                    align: *align,
                }
            }
            _ => continue,
        };
        statements.push(statement);
    }
    for (port, pin) in symbol.pins.iter().enumerate() {
        if let Some(pin) = pin {
            statements.push(NewSymbolItem::Pin {
                port,
                x: pin.x,
                y: pin.y,
                label: true,
            });
        }
    }
    // The generated box names nothing a cell declaration would check.
    let decl = CellDecl {
        name: argonc::ast::Ident {
            span: cfgrammar::Span::new(0, 0),
            name: Substr::from(cell.name.as_str()),
            metadata: (),
        },
        params: Vec::new(),
        args: Vec::new(),
        scope: Scope {
            scope_order: 0,
            span: cfgrammar::Span::new(0, 0),
            stmts: Vec::new(),
            tail: None,
            metadata: (),
        },
        span: cfgrammar::Span::new(0, 0),
        metadata: (),
    };
    statements
        .iter()
        .map(|item| symbol_statement(item, &decl, schematic))
        .collect()
}

/// The range of the call in a statement one of the builders wrote.
pub(crate) fn call_range(statement: &str) -> Range<usize> {
    let start = if statement.starts_with("let ") || statement.starts_with("pub let ") {
        statement.find(" = ").map_or(0, |index| index + " = ".len())
    } else {
        0
    };
    start..statement.len() - usize::from(statement.ends_with(';'))
}

// Names.

/// The names bound at the top level of `cell`: its parameters and its
/// `let` bindings.
pub(crate) fn cell_names(cell: &ParsedCell) -> HashSet<String> {
    cell.args
        .iter()
        .map(|arg| arg.name.name.to_string())
        .chain(
            cell.scope
                .stmts
                .iter()
                .flat_map(|statement| statement.let_names())
                .map(|name| name.name.to_string()),
        )
        .collect()
}

/// `base` followed by the smallest number that makes it unused.
pub(crate) fn fresh_name(base: &str, taken: &HashSet<String>) -> String {
    (0..)
        .map(|index| format!("{base}{index}"))
        .find(|name| !taken.contains(name))
        .expect("an unused name always exists")
}

/// Checks a name the user typed for a new binding among `taken`.
pub(crate) fn check_new_name(
    name: &str,
    what: &str,
    taken: &HashSet<String>,
) -> Result<(), String> {
    validate_identifier(name, what)?;
    if taken.contains(name) {
        return Err(format!("`{name}` is already defined in this cell."));
    }
    Ok(())
}

/// The name of the cell `invocation` calls, which names its instances.
pub(crate) fn invoked_cell_name(invocation: &str) -> Result<String, String> {
    let call = parse::parse_cell(invocation)
        .map_err(|error| format!("`{invocation}` is not a cell invocation: {error}"))?;
    call.func
        .path
        .last()
        .map(|ident| ident.name.to_owned())
        .ok_or_else(|| format!("`{invocation}` names no cell."))
}

// Expressions the GUI sends.

/// Parses `text` as the value of a `let` in a cell body.
fn parse_value(text: &str) -> Result<ParsedExpr, String> {
    let source = format!("cell __argon_check__() {{ let __argon_value__ = {text}; }}");
    let ast = parse::parse_source_text(source, PathBuf::from(CHECK_PATH))
        .map_err(|error| format!("`{text}` is not an expression: {error}"))?;
    let value = ast.ast.decls.iter().find_map(|decl| match decl {
        Decl::Cell(cell) => match cell.scope.stmts.first() {
            Some(Statement::LetBinding(binding)) if cell.scope.stmts.len() == 1 => {
                Some(binding.value.clone())
            }
            _ => None,
        },
        _ => None,
    });
    value.ok_or_else(|| format!("`{text}` is not a single expression."))
}

/// Checks that `text` is one expression.
pub(crate) fn check_expression(text: &str) -> Result<(), String> {
    parse_value(text).map(|_| ())
}

/// Whether `expr` reads a binding, possibly through fields, tuple elements,
/// and indices.
fn is_path(expr: &ParsedExpr) -> bool {
    match expr {
        Expr::IdentPath(path) => path.path.len() == 1 && path.generic_args.is_none(),
        Expr::FieldAccess(access) => is_path(&access.base),
        Expr::IndexFieldAccess(access) => is_path(&access.base),
        Expr::Index(index) => is_path(&index.base) && matches!(index.index, Expr::IntLiteral(_)),
        _ => false,
    }
}

/// Checks that `text` is a path to a signal or terminal, such as `a` or
/// `m0.g`.
pub(crate) fn check_path(text: &str) -> Result<(), String> {
    if is_path(&parse_value(text)?) {
        Ok(())
    } else {
        Err(format!("`{text}` is not a path to a signal or terminal."))
    }
}

// Insertion.

/// New statements at the end of a cell body.
pub(crate) struct Insertion {
    pub(crate) replacement: Replacement,
    /// The range of each statement within the inserted text.
    pub(crate) statements: Vec<Range<usize>>,
}

/// The insertion that adds `statements` to the end of `scope`, before its
/// tail, following the body's indentation.
pub(crate) fn insert_statements(
    source: &str,
    scope: &ParsedScope,
    statements: &[String],
) -> Insertion {
    let (offset, prefix, indent, suffix) = match &scope.tail {
        Some(tail) => {
            let offset = tail.span().start();
            let line_start = source[..offset].rfind('\n').map_or(0, |index| index + 1);
            let before = &source[line_start..offset];
            let indent = if before.trim().is_empty() {
                before.to_owned()
            } else {
                " ".repeat(before.chars().count())
            };
            (offset, String::new(), indent.clone(), format!("\n{indent}"))
        }
        None => {
            let closing = scope.span.end() - 1;
            let line_start = source[..closing].rfind('\n').map_or(0, |index| index + 1);
            let before = &source[line_start..closing];
            let closing_indent = before
                .chars()
                .take_while(|character| *character == ' ' || *character == '\t')
                .collect::<String>();
            let indent = format!("{closing_indent}    ");
            let prefix = if before.trim().is_empty() {
                "    ".to_owned()
            } else {
                format!("\n{indent}")
            };
            (closing, prefix, indent, format!("\n{closing_indent}"))
        }
    };
    let mut text = prefix;
    let mut ranges = Vec::with_capacity(statements.len());
    for (index, statement) in statements.iter().enumerate() {
        if index > 0 {
            text.push('\n');
            text.push_str(&indent);
        }
        let start = text.len();
        text.push_str(statement);
        ranges.push(start..text.len());
    }
    text.push_str(&suffix);
    Insertion {
        replacement: (offset..offset, text),
        statements: ranges,
    }
}

/// The span, after `replacements`, of `range` within the text that
/// `insertion` adds.
pub(crate) fn inserted_span(
    insertion: &Replacement,
    range: &Range<usize>,
    replacements: &[Replacement],
) -> cfgrammar::Span {
    let offset = insertion.0.start;
    let base = span_after(cfgrammar::Span::new(offset, offset), replacements).start();
    cfgrammar::Span::new(base + range.start, base + range.end)
}

// Cells.

/// The module of `path`.
pub(crate) fn module_at<'a>(
    workspace: &'a WorkspaceParseAst,
    path: &std::path::Path,
) -> Option<&'a AnnotatedParseAst> {
    workspace.values().find(|ast| ast.path == path)
}

/// The cell declaration whose body is `span`.
pub(crate) fn cell_with_body(
    ast: &AnnotatedParseAst,
    span: cfgrammar::Span,
) -> Option<&ParsedCell> {
    ast.ast.decls.iter().find_map(|decl| match decl {
        Decl::Cell(cell) if cell.scope.span == span => Some(cell),
        _ => None,
    })
}

/// The source range of a statement, without its `;`.
fn statement_span(statement: &ParsedStatement) -> cfgrammar::Span {
    match statement {
        Statement::Expr { value, .. } => value.span(),
        Statement::LetBinding(binding) => binding.span,
        Statement::LetPattern(binding) => binding.span,
        Statement::ForLoop(for_loop) => for_loop.span,
    }
}

/// The call a statement of the form `let x = <call>;` or `<call>;` makes.
fn statement_call(statement: &ParsedStatement) -> Option<&ParsedCall> {
    match statement {
        Statement::Expr {
            value: Expr::Call(call),
            ..
        } => Some(call),
        Statement::LetBinding(binding) => match &binding.value {
            Expr::Call(call) => Some(call),
            _ => None,
        },
        _ => None,
    }
}

/// The cell and top-level statement that contain `offset`.
fn root_statement_at(
    ast: &AnnotatedParseAst,
    offset: usize,
) -> Option<(&ParsedCell, &ParsedStatement)> {
    ast.ast.decls.iter().find_map(|decl| {
        let Decl::Cell(cell) = decl else {
            return None;
        };
        cell.scope
            .stmts
            .iter()
            .find(|statement| {
                let span = statement_span(statement);
                span.start() <= offset && offset < span.end()
            })
            .map(|statement| (cell, statement))
    })
}

// Deletion.

/// The edits that delete the targets, or the references that stop them.
pub(crate) struct DeletePlan {
    pub(crate) replacements: FileReplacements,
    pub(crate) blocking: Vec<BlockingReference>,
}

/// The replacement that removes the statement at `span`, with its `;` and,
/// when nothing else shares its line, the line itself.
fn statement_removal(source: &str, span: cfgrammar::Span) -> Replacement {
    let mut range = span.start()..span.end();
    let rest = &source[range.end..];
    let spaces = rest.len() - rest.trim_start_matches([' ', '\t']).len();
    if rest[spaces..].starts_with(';') {
        range.end += spaces + 1;
    }
    let line_start = source[..range.start]
        .rfind('\n')
        .map_or(0, |index| index + 1);
    let line_end = source[range.end..]
        .find('\n')
        .map_or(source.len(), |index| range.end + index);
    let before = &source[line_start..range.start];
    let after = &source[range.end..line_end];
    if before.trim().is_empty() && after.trim().is_empty() {
        let end = if line_end < source.len() {
            line_end + 1
        } else {
            line_end
        };
        return (line_start..end, String::new());
    }
    if !after.trim().is_empty() {
        let trailing = after.len() - after.trim_start().len();
        return (range.start..range.end + trailing, String::new());
    }
    let leading = before.len() - before.trim_end().len();
    (range.start - leading..range.end, String::new())
}

/// The identifier a call argument reads, when the argument is a path.
fn argument_root(expr: &ParsedExpr) -> Option<cfgrammar::Span> {
    match expr {
        Expr::IdentPath(path) if path.path.len() == 1 => Some(path.span),
        Expr::FieldAccess(access) => argument_root(&access.base),
        Expr::IndexFieldAccess(access) => argument_root(&access.base),
        Expr::Index(index) => argument_root(&index.base),
        _ => None,
    }
}

/// Whether the compiled schematics of the cell with body `cell` include a
/// `connect` at `span`.
fn is_connect(data: &CompiledData, cell: &Span, span: cfgrammar::Span) -> bool {
    compiled_cells(data, cell).any(|compiled| {
        compiled
            .schematic
            .connections
            .iter()
            .any(|connection| connection.span.path == cell.path && connection.span.span == span)
    })
}

/// Whether a device of the cell with body `cell` lists the signal at `span`
/// as a terminal.
fn is_terminal(data: &CompiledData, cell: &Span, span: cfgrammar::Span) -> bool {
    compiled_cells(data, cell).any(|compiled| {
        compiled.schematic.devices.iter().any(|device| {
            device
                .terminal_spans
                .iter()
                .flatten()
                .any(|terminal| terminal.path == cell.path && terminal.span == span)
        })
    })
}

/// The compiled cells whose body is `cell`: one per set of arguments.
fn compiled_cells<'a>(
    data: &'a CompiledData,
    cell: &'a Span,
) -> impl Iterator<Item = &'a argonc::compile::CompiledCell> + 'a {
    data.cells
        .values()
        .map(|compiled| &**compiled)
        .filter(move |compiled| {
            compiled
                .scopes
                .get(&compiled.root)
                .is_some_and(|root| root.span == *cell)
        })
}

/// A blocking reference, described by its source line.
fn blocking_reference(ast: &AnnotatedParseAst, span: &Span) -> BlockingReference {
    let source: &str = &ast.source_text;
    let start = span.span.start().min(source.len());
    let line_start = source[..start].rfind('\n').map_or(0, |index| index + 1);
    let line_end = source[start..]
        .find('\n')
        .map_or(source.len(), |index| start + index);
    BlockingReference {
        span: span.clone(),
        line: source[..line_start].matches('\n').count() as u32,
        text: source[line_start..line_end].trim().to_owned(),
    }
}

/// Plans the deletion of the statements that made `targets`, cleaning up
/// the `connect` statements and terminal lists that name a deleted binding
/// and reporting every other reference.
pub(crate) fn plan_deletion(
    workspace: &WorkspaceParseAst,
    data: &CompiledData,
    index: Option<&NavIndex>,
    targets: &[Span],
) -> Result<DeletePlan, String> {
    let mut removals: HashMap<PathBuf, Vec<cfgrammar::Span>> = HashMap::new();
    let mut replacements: FileReplacements = HashMap::new();
    let mut blocking = Vec::new();
    for target in targets {
        let ast = module_at(workspace, &target.path)
            .ok_or_else(|| crate::rpc::OUT_OF_SYNC_MESSAGE.to_owned())?;
        if site_of(data, target).is_none() {
            return Err(crate::rpc::STALE_SNAPSHOT_MESSAGE.to_owned());
        }
        let (cell, statement) = root_statement_at(ast, target.span.start())
            .filter(|(_, statement)| {
                statement_call(statement).is_some_and(|call| call.span == target.span)
            })
            .ok_or_else(|| {
                "Only a statement of the form `let x = <call>;` or `<call>;` can be deleted \
                 from the GUI."
                    .to_owned()
            })?;
        let body = Span {
            path: target.path.clone(),
            span: cell.scope.span,
        };
        if let Statement::LetBinding(binding) = statement {
            let index = index
                .filter(|index| {
                    index
                        .source(&target.path)
                        .is_some_and(|source| **source == *ast.source_text)
                })
                .ok_or_else(|| {
                    "The navigation index is not up to date; try again in a moment.".to_owned()
                })?;
            let Some((_, Target::Def(key))) =
                index.target_at(&target.path, binding.name.span.start())
            else {
                return Err(format!("Could not resolve `{}`.", binding.name.name));
            };
            for reference in index.references(key, false) {
                let Some(module) = module_at(workspace, &reference.path) else {
                    return Err(crate::rpc::OUT_OF_SYNC_MESSAGE.to_owned());
                };
                let same_cell = reference.path == target.path;
                let enclosing = root_statement_at(module, reference.span.start())
                    .filter(|(enclosing, _)| same_cell && enclosing.scope.span == cell.scope.span);
                match enclosing {
                    Some((_, enclosing)) if matches!(enclosing, Statement::Expr { .. }) => {
                        let call = statement_call(enclosing);
                        let names_it = call.is_some_and(|call| {
                            is_connect(data, &body, call.span)
                                && call
                                    .args
                                    .posargs
                                    .iter()
                                    .any(|arg| argument_root(arg) == Some(reference.span))
                        });
                        if names_it {
                            removals
                                .entry(reference.path.clone())
                                .or_default()
                                .push(statement_span(enclosing));
                            continue;
                        }
                    }
                    Some(_) if is_terminal(data, &body, reference.span) => {
                        replacements
                            .entry(reference.path.clone())
                            .or_default()
                            .push((
                                reference.span.start()..reference.span.end(),
                                "std::schematic::Signal()".to_owned(),
                            ));
                        continue;
                    }
                    _ => {}
                }
                blocking.push(blocking_reference(module, reference));
            }
        }
        removals
            .entry(target.path.clone())
            .or_default()
            .push(statement_span(statement));
    }
    if !blocking.is_empty() {
        blocking.sort_by(|a, b| {
            (&a.span.path, a.span.span.start()).cmp(&(&b.span.path, b.span.span.start()))
        });
        blocking.dedup();
        return Ok(DeletePlan {
            replacements: HashMap::new(),
            blocking,
        });
    }
    for (path, spans) in removals {
        let Some(ast) = module_at(workspace, &path) else {
            continue;
        };
        let mut spans = spans;
        spans.sort_by_key(|span| span.start());
        spans.dedup();
        let removed = spans
            .iter()
            .map(|span| statement_removal(&ast.source_text, *span))
            .collect::<Vec<_>>();
        let file = replacements.entry(path).or_default();
        // A rewrite inside a removed statement goes with it.
        file.retain(|(range, _)| {
            !removed
                .iter()
                .any(|(removal, _)| removal.start <= range.start && range.end <= removal.end)
        });
        file.extend(removed);
    }
    for file in replacements.values_mut() {
        merge_removals(file);
        file.sort_by_key(|replacement| std::cmp::Reverse(replacement.0.start));
    }
    Ok(DeletePlan {
        replacements,
        blocking,
    })
}

// Renaming.

/// The replacements that rename the binding whose value is the call at
/// `site` to `name`, with every reference to it, in every file.
pub(crate) fn rename_replacements(
    workspace: &WorkspaceParseAst,
    data: &CompiledData,
    index: Option<&NavIndex>,
    site: &Span,
    name: &str,
) -> Result<FileReplacements, String> {
    let ast = module_at(workspace, &site.path)
        .ok_or_else(|| crate::rpc::OUT_OF_SYNC_MESSAGE.to_owned())?;
    if site_of(data, site).is_none() {
        return Err(crate::rpc::STALE_SNAPSHOT_MESSAGE.to_owned());
    }
    let (cell, statement) = root_statement_at(ast, site.span.start())
        .filter(|(_, statement)| {
            statement_call(statement).is_some_and(|call| call.span == site.span)
        })
        .ok_or_else(|| {
            "Only a `let` binding at the top level of the cell can be renamed.".to_owned()
        })?;
    let Statement::LetBinding(binding) = statement else {
        return Err("This statement binds no name to rename.".to_owned());
    };
    let old = binding.name.name.to_string();
    if old == name {
        return Ok(HashMap::new());
    }
    let mut taken = cell_names(cell);
    taken.remove(&old);
    check_new_name(name, "a binding", &taken)?;
    let index = index.ok_or_else(|| {
        "The navigation index is not up to date; try again in a moment.".to_owned()
    })?;
    let Some((_, Target::Def(key))) = index.target_at(&site.path, binding.name.span.start()) else {
        return Err(format!("Could not resolve `{old}`."));
    };
    let mut replacements: FileReplacements = HashMap::new();
    for reference in index.references(key, true) {
        let module = module_at(workspace, &reference.path)
            .ok_or_else(|| crate::rpc::OUT_OF_SYNC_MESSAGE.to_owned())?;
        let current = index
            .source(&reference.path)
            .is_some_and(|source| **source == *module.source_text);
        if !current {
            return Err(
                "The navigation index is not up to date; try again in a moment.".to_owned(),
            );
        }
        replacements
            .entry(reference.path.clone())
            .or_default()
            .push((
                reference.span.start()..reference.span.end(),
                name.to_owned(),
            ));
    }
    for file in replacements.values_mut() {
        file.sort_by_key(|replacement| std::cmp::Reverse(replacement.0.start));
        file.dedup();
    }
    Ok(replacements)
}

// Copying and pasting.

/// The statements that made `targets`, as the clipboard holds them.
pub(crate) fn copy_statements(
    workspace: &WorkspaceParseAst,
    data: &CompiledData,
    targets: &[Span],
) -> Result<Vec<ClipboardItem>, String> {
    let mut items = Vec::new();
    for target in targets {
        let ast = module_at(workspace, &target.path)
            .ok_or_else(|| crate::rpc::OUT_OF_SYNC_MESSAGE.to_owned())?;
        let kind = match site_of(data, target) {
            Some((SiteKind::Signal, _)) => ClipboardKind::Signal,
            Some((SiteKind::Instance, _)) => ClipboardKind::Instance,
            Some((SiteKind::Device, _)) => ClipboardKind::Device,
            Some((SiteKind::Connect, _)) => ClipboardKind::Connect,
            Some((SiteKind::SymbolGroup, _)) => {
                return Err("The items a function drew cannot be copied one by one.".to_owned());
            }
            Some(_) => ClipboardKind::Symbol,
            None => return Err(crate::rpc::STALE_SNAPSHOT_MESSAGE.to_owned()),
        };
        let (_, statement) = root_statement_at(ast, target.span.start())
            .filter(|(_, statement)| {
                statement_call(statement).is_some_and(|call| call.span == target.span)
            })
            .ok_or_else(|| {
                "Only a statement of the form `let x = <call>;` or `<call>;` can be copied."
                    .to_owned()
            })?;
        let (name, public) = match statement {
            Statement::LetBinding(binding) => (
                Some(binding.name.name.to_string()),
                binding.public.is_some(),
            ),
            _ => (None, false),
        };
        items.push(ClipboardItem {
            kind,
            name,
            public,
            call: ast.source_text[target.span.start()..target.span.end()].to_owned(),
        });
    }
    Ok(items)
}

/// The name a copy of `name` gets: the name itself when free, or it with
/// a fresh number.
fn copied_name(name: &str, taken: &HashSet<String>) -> String {
    if !taken.contains(name) {
        return name.to_owned();
    }
    let base = name.trim_end_matches(|character: char| character.is_ascii_digit());
    let base = if base.is_empty() { name } else { base };
    fresh_name(base, taken)
}

/// Parses the call text of a clipboard item, returning the call and the
/// offset of the text within the source it was parsed in.
fn parse_call_text(text: &str) -> Result<(ParsedCall, usize), String> {
    const PREFIX: &str = "cell __argon_check__() { let __argon_value__ = ";
    match parse_value(text)? {
        Expr::Call(call) => Ok((call, PREFIX.len())),
        _ => Err(format!("`{text}` is not a call.")),
    }
}

/// Reads a tuple of two numbers from the text of a tuple expression, whose
/// spans are `prefix` bytes into `source`.
fn tuple_text<T: std::str::FromStr>(
    source: &str,
    prefix: usize,
    expr: &ParsedExpr,
) -> Option<(T, T)> {
    let Expr::Tuple(tuple) = expr else {
        return None;
    };
    let [x, y] = tuple.items.as_slice() else {
        return None;
    };
    let text = |expr: &ParsedExpr| {
        source[expr.span().start() - prefix..expr.span().end() - prefix]
            .trim()
            .parse()
            .ok()
    };
    Some((text(x)?, text(y)?))
}

/// The statements that paste `items` into a cell whose top-level names are
/// `taken`: bindings get fresh names, placements and routes move by
/// `offset`, symbol items by `symbol_offset`, and the copied connections
/// and terminal lists name the copies.
pub(crate) fn paste_statements(
    items: &[ClipboardItem],
    offset: (i64, i64),
    symbol_offset: (f64, f64),
    taken: &HashSet<String>,
) -> Result<Vec<String>, String> {
    let mut taken = taken.clone();
    let mut renames: HashMap<String, String> = HashMap::new();
    for item in items {
        if let Some(name) = &item.name {
            let new = copied_name(name, &taken);
            taken.insert(new.clone());
            renames.insert(name.clone(), new);
        }
    }
    let mut statements = Vec::new();
    for item in items {
        let (call, prefix) = parse_call_text(&item.call)?;
        let text = |span: cfgrammar::Span| &item.call[span.start() - prefix..span.end() - prefix];
        let range = |span: cfgrammar::Span| span.start() - prefix..span.end() - prefix;
        let mut replacements: Vec<Replacement> = Vec::new();
        let shift_int = |replacements: &mut Vec<Replacement>, name: &str, by: i64| {
            if let Some(kwarg) = call
                .args
                .kwargs
                .iter()
                .find(|kwarg| kwarg.name.name == name)
                && let Ok(value) = text(kwarg.value.span()).trim().parse::<i64>()
            {
                replacements.push((range(kwarg.value.span()), (value + by).to_string()));
            }
        };
        let shift_float = |replacements: &mut Vec<Replacement>, name: &str, by: f64| {
            if let Some(kwarg) = call
                .args
                .kwargs
                .iter()
                .find(|kwarg| kwarg.name.name == name)
                && let Ok(value) = text(kwarg.value.span()).trim().parse::<f64>()
            {
                replacements.push((range(kwarg.value.span()), float_literal(value + by)));
            }
        };
        let rename_root = |replacements: &mut Vec<Replacement>, expr: &ParsedExpr| {
            if let Some(span) = argument_root(expr)
                && let Some(new) = renames.get(text(span))
            {
                replacements.push((range(span), new.clone()));
            }
        };
        match item.kind {
            ClipboardKind::Signal | ClipboardKind::Instance | ClipboardKind::Device => {
                shift_int(&mut replacements, "x", offset.0);
                shift_int(&mut replacements, "y", offset.1);
                if item.kind == ClipboardKind::Device
                    && let Some(Expr::Seq(terminals)) = call.args.posargs.get(1)
                {
                    for terminal in &terminals.items {
                        rename_root(&mut replacements, terminal);
                    }
                }
            }
            ClipboardKind::Connect => {
                for end in call.args.posargs.iter().take(2) {
                    rename_root(&mut replacements, end);
                }
                if let Some(kwarg) = call
                    .args
                    .kwargs
                    .iter()
                    .find(|kwarg| kwarg.name.name == "route")
                    && let Expr::Seq(route) = &kwarg.value
                {
                    let corners = route
                        .items
                        .iter()
                        .map(|corner| {
                            tuple_text::<i64>(&item.call, prefix, corner)
                                .map(|(x, y)| (x + offset.0, y + offset.1))
                        })
                        .collect::<Option<Vec<_>>>();
                    if let Some(corners) = corners {
                        replacements.push((range(kwarg.value.span()), route_text(&corners)));
                    }
                }
            }
            ClipboardKind::Symbol => {
                if call
                    .func
                    .path
                    .last()
                    .is_some_and(|ident| ident.name == "pin")
                {
                    // A port has one pin, so a copied pin is left out.
                    continue;
                }
                for name in ["x", "x0", "x1"] {
                    shift_float(&mut replacements, name, symbol_offset.0);
                }
                for name in ["y", "y0", "y1"] {
                    shift_float(&mut replacements, name, symbol_offset.1);
                }
                if let Some(Expr::Seq(points)) = call.args.posargs.first() {
                    let shifted = points
                        .items
                        .iter()
                        .map(|point| {
                            tuple_text::<f64>(&item.call, prefix, point)
                                .map(|(x, y)| (x + symbol_offset.0, y + symbol_offset.1))
                        })
                        .collect::<Option<Vec<_>>>();
                    if let Some(shifted) = shifted {
                        replacements.push((range(points.span), points_text(&shifted)));
                    }
                }
            }
        }
        replacements.sort_by_key(|replacement| std::cmp::Reverse(replacement.0.start));
        let mut call_text = item.call.clone();
        for (range, replacement) in replacements {
            call_text.replace_range(range, &replacement);
        }
        statements.push(match &item.name {
            Some(name) => format!(
                "{}let {} = {call_text};",
                if item.public { "pub " } else { "" },
                renames[name]
            ),
            None => format!("{call_text};"),
        });
    }
    Ok(statements)
}

// Arguments.

/// The replacements that set `argument` of `call` to `value`: a present
/// argument is rewritten, and a missing one is added.
pub(crate) fn argument_replacements(
    source: &str,
    call: &ParsedCall,
    argument: &CallArgument,
    value: &str,
) -> Result<Vec<Replacement>, String> {
    match argument {
        CallArgument::Keyword(name) => Ok(keyword_replacements(
            source,
            call,
            &[KeywordEdit {
                name: name.clone(),
                value: Some(value.to_owned()),
            }],
        )),
        CallArgument::Positional(index) => {
            let posargs = &call.args.posargs;
            if let Some(arg) = posargs.get(*index) {
                let span = arg.span();
                return Ok(vec![(span.start()..span.end(), value.to_owned())]);
            }
            if *index != posargs.len() {
                return Err(format!(
                    "The call has {} positional arguments.",
                    posargs.len()
                ));
            }
            if let Some(last) = posargs.last() {
                let end = last.span().end();
                return Ok(vec![(end..end, format!(", {value}"))]);
            }
            if let Some(first) = call.args.kwargs.first() {
                let start = first.span.start();
                return Ok(vec![(start..start, format!("{value}, "))]);
            }
            let insertion = call.span.end() - 1;
            Ok(vec![(insertion..insertion, value.to_owned())])
        }
    }
}

/// The source text of each argument of `call`.
pub(crate) fn call_arguments(source: &str, call: &ParsedCall) -> CallArguments {
    let text = |span: cfgrammar::Span| source[span.start()..span.end()].to_owned();
    CallArguments {
        positional: call
            .args
            .posargs
            .iter()
            .map(|arg| text(arg.span()))
            .collect(),
        keywords: call
            .args
            .kwargs
            .iter()
            .map(|kwarg| (kwarg.name.name.to_string(), text(kwarg.value.span())))
            .collect(),
    }
}

/// Applies `replacements`, which are in descending order, to `source`.
#[cfg(test)]
pub(crate) fn apply_replacements(source: &str, replacements: &[Replacement]) -> String {
    let mut text = source.to_owned();
    for (range, replacement) in replacements {
        text.replace_range(range.clone(), replacement);
    }
    text
}

/// A workspace compiled with `top()` as its entry, for tests of the edits.
#[cfg(test)]
pub(crate) mod testing {
    use std::path::PathBuf;

    use argonc::{nav::NavIndex, parse};

    use super::*;

    /// The published AST, the compiled data, and the navigation index.
    pub(crate) struct Compiled {
        pub(crate) workspace: WorkspaceParseAst,
        pub(crate) data: CompiledData,
        pub(crate) index: NavIndex,
        pub(crate) path: PathBuf,
    }

    pub(crate) fn compile(source: &str) -> Compiled {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("lib.ar");
        std::fs::write(&path, source).unwrap();
        let mut workspace = parse::parse_workspace_with_std(&path).ast();
        let invocation = parse::splice_cell_invocation(&mut workspace, "top()").unwrap();
        let (typed, errors) = argonc::compile::static_compile(&workspace).unwrap();
        assert!(errors.errors.is_empty(), "{:?}", errors.errors);
        let tech =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../examples/tech/basic.tech.toml");
        let config = argonc::WorkspaceConfig::new(&path).with_tech(Some(tech));
        let data = match argonc::compile::execute_cell_invocation(&typed, &invocation, &config) {
            argonc::compile::CompileOutput::Valid(data) => data,
            other => panic!("{other:#?}"),
        };
        let index = NavIndex::build(&typed);
        // The directory outlives the test through the paths it holds.
        std::mem::forget(directory);
        Compiled {
            workspace,
            data,
            index,
            path,
        }
    }

    impl Compiled {
        pub(crate) fn source(&self) -> &str {
            &module_at(&self.workspace, &self.path).unwrap().source_text
        }

        pub(crate) fn module(&self) -> &AnnotatedParseAst {
            module_at(&self.workspace, &self.path).unwrap()
        }

        /// The span of the whole call that starts with `text`.
        pub(crate) fn site(&self, text: &str) -> Span {
            let start = self.source().find(text).unwrap();
            self.module()
                .span2call
                .keys()
                .find(|span| span.span.start() == start)
                .unwrap_or_else(|| panic!("no call at `{text}`"))
                .clone()
        }

        /// The compiled cell named `name`.
        pub(crate) fn cell(&self, name: &str) -> &CompiledCell {
            self.data
                .cells
                .values()
                .find(|cell| cell.name == name)
                .unwrap_or_else(|| panic!("no cell `{name}`"))
        }

        /// The body of the cell named `name`.
        pub(crate) fn body(&self, name: &str) -> Span {
            let cell = self.cell(name);
            cell.scopes[&cell.root].span.clone()
        }

        pub(crate) fn delete(&self, targets: &[Span]) -> Result<DeletePlan, String> {
            plan_deletion(&self.workspace, &self.data, Some(&self.index), targets)
        }

        pub(crate) fn deleted(&self, targets: &[Span]) -> String {
            let plan = self.delete(targets).unwrap();
            assert!(plan.blocking.is_empty(), "{:?}", plan.blocking);
            apply_replacements(self.source(), &plan.replacements[&self.path])
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use argonc::parse;

    use super::testing::compile;
    use super::*;
    use crate::rpc::{CallArgument, ClipboardKind};

    const SOURCE: &str = "use std::schematic::{Signal, connect, device, DeviceKind};\n\
                          cell top() {\n    \
                              pub let a = Signal(x=-6, y=0);\n    \
                              pub let b = Signal();\n    \
                              let m = device(DeviceKind::Res, [a, b], \"\", value=1., x=4, y=0, angle=90);\n    \
                              connect(a, m.p, route=[(0, 0)]);\n    \
                              connect(a, m.n);\n    \
                              let n = device(\n        DeviceKind::Res,\n        [a, b],\n        \"\",\n        value=2.,\n    );\n\
                          }\n";

    fn call_at<'a>(
        ast: &'a parse::AnnotatedParseAst,
        text: &str,
    ) -> &'a CallExpr<Substr, ParseMetadata> {
        let start = SOURCE.find(text).unwrap();
        ast.span2call
            .iter()
            .find(|(span, _)| span.span.start() == start)
            .map(|(_, call)| call)
            .unwrap_or_else(|| panic!("no call at `{text}`"))
    }

    fn edit(name: &str, value: Option<&str>) -> KeywordEdit {
        KeywordEdit {
            name: name.to_owned(),
            value: value.map(str::to_owned),
        }
    }

    fn rewritten(text: &str, edits: &[KeywordEdit]) -> String {
        let ast = parse::parse_source_text(SOURCE, PathBuf::from("/virtual/lib.ar")).unwrap();
        let call = call_at(&ast, text);
        let replacements = keyword_replacements(SOURCE, call, edits);
        let edited = apply_replacements(SOURCE, &replacements);
        let span = span_after(call.span, &replacements);
        edited[span.start()..span.end()].to_owned()
    }

    /// Compiles `source` statically with the standard library alone.
    fn assert_static(source: &str) {
        let root = parse::parse_source_text(source, PathBuf::from("/virtual/lib.ar")).unwrap();
        let (_, errors) = argonc::compile::static_compile(&parse::with_std(root)).unwrap();
        assert!(errors.errors.is_empty(), "{source}\n{:?}", errors.errors);
    }

    #[test]
    fn statements_name_natives_by_full_path_and_compile_without_imports() {
        let placement = ElementPlacement {
            at: (8, -4),
            angle: 90,
            reflect: true,
        };
        let port = signal_statement("a", true, (-6, 0));
        let label = signal_statement("n1", false, (3, 2));
        let instance = instance_statement("child0", "child(1.)", &placement);
        let device = device_statement(
            "nmos0",
            "Nmos",
            4,
            "\"nch\", w=1., l=0.15,",
            &ElementPlacement {
                at: (0, -4),
                ..Default::default()
            },
        );
        let bare = device_statement("res0", "Res", 2, "value=1.", &ElementPlacement::default());
        let routed = connect_statement("a", "nmos0.g", Some(&[(-6, -4)]));
        let touching = connect_statement("n1", "child0.p", None);
        assert_eq!(port, "pub let a = std::schematic::Signal(x=-6, y=0);");
        assert_eq!(label, "let n1 = std::schematic::Signal(x=3, y=2);");
        assert_eq!(
            instance,
            "pub let child0 = std::schematic::inst(child(1.), x=8, y=-4, angle=90, reflect=true);"
        );
        assert_eq!(
            device,
            "pub let nmos0 = std::schematic::device(std::schematic::DeviceKind::Nmos, \
             std::schematic::signals(4), \"nch\", w=1., l=0.15, x=0, y=-4);"
        );
        assert_eq!(
            bare,
            "pub let res0 = std::schematic::device(std::schematic::DeviceKind::Res, \
             std::schematic::signals(2), \"\", value=1., x=0, y=0);"
        );
        assert_eq!(device_args(""), "\"\"");
        assert_eq!(device_args("\"nch\", w=1."), "\"nch\", w=1.");
        assert_eq!(device_args(" value=1., "), "\"\", value=1.");
        assert_eq!(
            routed,
            "std::schematic::connect(a, nmos0.g, route=[(-6, -4)]);"
        );
        assert_eq!(touching, "std::schematic::connect(n1, child0.p);");
        assert_eq!(
            &routed[call_range(&routed)],
            "std::schematic::connect(a, nmos0.g, route=[(-6, -4)])"
        );
        assert_eq!(
            &port[call_range(&port)],
            "std::schematic::Signal(x=-6, y=0)"
        );
        assert_static(&format!(
            "cell child(w: Float) {{ pub let p = std::schematic::Signal(); }}\n\
             cell top() {{\n    {port}\n    {label}\n    {instance}\n    {device}\n    {bare}\n    \
             {routed}\n    {touching}\n}}\n"
        ));
    }

    #[test]
    fn fresh_and_typed_names_avoid_the_cells_bindings() {
        let taken = HashSet::from(["mos0".to_owned(), "w".to_owned()]);
        assert_eq!(fresh_name("mos", &taken), "mos1");
        assert_eq!(fresh_name("nmos", &taken), "nmos0");
        assert!(check_new_name("out", "a port", &taken).is_ok());
        assert!(check_new_name("w", "a port", &taken).is_err());
        assert!(check_new_name("let", "a port", &taken).is_err());
        assert!(check_new_name("2x", "a port", &taken).is_err());
        assert!(check_new_name("eq", "a net label", &taken).is_err());
        assert_eq!(invoked_cell_name("lib::mos(true, 1., 2)").unwrap(), "mos");
        assert!(invoked_cell_name("mos").is_err());
    }

    #[test]
    fn only_paths_name_connection_ends() {
        for path in ["a", "m0.g", "data[1]", "t.0", "x.inner.pins[2].p"] {
            check_path(path).unwrap_or_else(|error| panic!("{path}: {error}"));
        }
        for text in [
            "f(x)",
            "1 + 2",
            "a b",
            "std::schematic::Signal()",
            "a[i]",
            "",
        ] {
            assert!(check_path(text).is_err(), "{text}");
        }
        assert!(check_expression("1. + w").is_ok());
        assert!(check_expression("1 +").is_err());
    }

    #[test]
    fn statements_are_inserted_at_the_end_of_the_body() {
        fn insert(source: &str, statements: &[&str]) -> (String, Vec<String>) {
            let ast = parse::parse_source_text(source, PathBuf::from("/virtual/lib.ar")).unwrap();
            let cell = cell_with_body(
                &ast,
                source
                    .find('{')
                    .map(|start| {
                        let end = source.rfind('}').unwrap() + 1;
                        cfgrammar::Span::new(start, end)
                    })
                    .unwrap(),
            )
            .unwrap();
            let statements = statements
                .iter()
                .map(|s| (*s).to_owned())
                .collect::<Vec<_>>();
            let insertion = insert_statements(source, &cell.scope, &statements);
            let edited = apply_replacements(source, std::slice::from_ref(&insertion.replacement));
            let inserted = insertion
                .statements
                .iter()
                .map(|range| insertion.replacement.1[range.clone()].to_owned())
                .collect();
            (edited, inserted)
        }
        let (edited, inserted) = insert("cell top() {\n}\n", &["a;", "b;"]);
        assert_eq!(edited, "cell top() {\n    a;\n    b;\n}\n");
        assert_eq!(inserted, ["a;", "b;"]);
        let (edited, _) = insert("cell top() {\n    let x = 1;\n}\n", &["a;"]);
        assert_eq!(edited, "cell top() {\n    let x = 1;\n    a;\n}\n");
        let (edited, _) = insert("cell top() {\n    let x = 1;\n    x\n}\n", &["a;", "b;"]);
        assert_eq!(
            edited,
            "cell top() {\n    let x = 1;\n    a;\n    b;\n    x\n}\n"
        );
        let (edited, _) = insert("cell top() {}\n", &["a;"]);
        assert_eq!(edited, "cell top() {\n    a;\n}\n");
    }

    const DELETION: &str = "\
use std::schematic::{DeviceKind, Signal, connect, device, signals};
cell child() {
    pub let p = Signal(x=0, y=0);
    pub let q = Signal(x=0, y=2);
}
cell top() {
    pub let a = Signal(x=-6, y=0);
    pub let b = Signal(x=4, y=0); let c = Signal(x=4, y=2);
    let m = device(DeviceKind::Res, [a, b], \"\", value=1., x=0, y=4);
    let n = device(DeviceKind::Res, signals(2), \"\", value=2., x=0, y=-4);
    connect(a, n.p, route=[(-6, -4)]);
    connect(b, n.n);
    connect(c, m.n);
    let u = std::schematic::inst(child(), x=8, y=0);
    connect(u.p, c);
}
";

    #[test]
    fn deleting_a_signal_removes_its_connects_and_frees_its_terminal() {
        let compiled = compile(DELETION);
        let edited = compiled.deleted(&[compiled.site("Signal(x=-6")]);
        assert!(!edited.contains("let a ="), "{edited}");
        assert!(!edited.contains("connect(a, n.p"), "{edited}");
        assert!(
            edited.contains("device(DeviceKind::Res, [std::schematic::Signal(), b], \"\""),
            "{edited}"
        );
        assert!(
            edited.contains("connect(b, n.n);"),
            "the other connect stays"
        );
    }

    #[test]
    fn deleting_a_device_removes_the_connects_that_name_its_terminals() {
        let compiled = compile(DELETION);
        let edited = compiled.deleted(&[compiled.site("device(DeviceKind::Res, signals(2)")]);
        assert!(!edited.contains("let n ="), "{edited}");
        assert!(!edited.contains("n.p"), "{edited}");
        assert!(!edited.contains("n.n"), "{edited}");
        assert!(edited.contains("connect(c, m.n);"), "{edited}");
        assert_eq!(
            edited.matches('\n').count(),
            DELETION.matches('\n').count() - 3,
            "three whole lines go: {edited}"
        );
    }

    #[test]
    fn deleting_a_wire_removes_only_its_statement() {
        let compiled = compile(DELETION);
        let edited = compiled.deleted(&[compiled.site("connect(a, n.p")]);
        assert!(!edited.contains("connect(a, n.p"), "{edited}");
        assert!(edited.contains("let a = Signal(x=-6, y=0);"), "{edited}");
        assert!(edited.contains("let n = device"), "{edited}");
    }

    #[test]
    fn a_statement_that_shares_its_line_leaves_the_rest_of_the_line() {
        let compiled = compile(DELETION);
        // `b` is first on its line, and `c` is last.
        let edited = compiled.deleted(&[compiled.site("Signal(x=4, y=0)")]);
        assert!(
            edited.contains("\n    let c = Signal(x=4, y=2);\n"),
            "{edited}"
        );
        assert!(!edited.contains("connect(b, n.n)"), "{edited}");
        let edited = compiled.deleted(&[compiled.site("Signal(x=4, y=2)")]);
        assert!(
            edited.contains("\n    pub let b = Signal(x=4, y=0);\n"),
            "{edited}"
        );
        assert!(!edited.contains("connect(c, m.n)"), "{edited}");
        assert!(!edited.contains("connect(u.p, c)"), "{edited}");
    }

    #[test]
    fn deleting_several_targets_at_once_does_not_edit_the_same_text_twice() {
        let compiled = compile(DELETION);
        // The signal, a device that lists it, and a wire that names it.
        let edited = compiled.deleted(&[
            compiled.site("Signal(x=-6"),
            compiled.site("device(DeviceKind::Res, [a, b]"),
            compiled.site("connect(a, n.p"),
        ]);
        assert!(!edited.contains("let a ="), "{edited}");
        assert!(!edited.contains("let m ="), "{edited}");
        assert!(!edited.contains("connect(a, n.p"), "{edited}");
        assert!(!edited.contains("connect(c, m.n"), "{edited}");
        assert!(edited.contains("connect(b, n.n);"), "{edited}");
    }

    #[test]
    fn other_references_block_a_deletion() {
        let source = "\
use std::schematic::{DeviceKind, Signal, connect, device, signals};
struct Pair { x: Signal, y: Signal }
cell child() {
    pub let p = Signal(x=0, y=0);
}
cell top() {
    pub let a = Signal(x=-6, y=0);
    let alias = a;
    let pair = Pair { x: a, y: Signal() };
    for i in std::range(2) {
        connect(a, pair.y);
    }
    let u = std::schematic::inst(child(), x=8, y=0);
    connect(u.p, a);
    pub let keep = Signal(x=2, y=2);
}
";
        let compiled = compile(source);
        let plan = compiled.delete(&[compiled.site("Signal(x=-6")]).unwrap();
        assert!(plan.replacements.is_empty());
        let texts = plan
            .blocking
            .iter()
            .map(|reference| reference.text.as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            texts,
            [
                "let alias = a;",
                "let pair = Pair { x: a, y: Signal() };",
                "connect(a, pair.y);"
            ]
        );
        assert_eq!(plan.blocking[0].line, 7);
        // The child's port is read by the parent, in the same file here.
        let plan = compiled
            .delete(&[compiled.site("Signal(x=0, y=0)")])
            .unwrap();
        assert_eq!(plan.blocking.len(), 1, "{:?}", plan.blocking);
        assert_eq!(plan.blocking[0].text, "connect(u.p, a);");
        // A statement that is not `let x = <call>;` cannot be deleted.
        assert!(compiled.delete(&[compiled.site("Signal()")]).is_err());
        // A stale index refuses rather than guessing.
        assert!(
            plan_deletion(
                &compiled.workspace,
                &compiled.data,
                None,
                &[compiled.site("Signal(x=2")]
            )
            .is_err()
        );
    }

    #[test]
    fn float_literals_read_back_and_always_carry_a_point() {
        assert_eq!(float_literal(2.), "2.");
        assert_eq!(float_literal(-0.25), "-0.25");
        assert_eq!(float_literal(0.1 + 0.2), "0.30000000000000004");
        assert_eq!(float_literal(1e-7), "0.0000001");
        assert_eq!(
            points_text(&[(-4., 0.), (-2., 0.5)]),
            "[(-4., 0.), (-2., 0.5)]"
        );
    }

    const SYMBOL_CELL: &str = "\
use std::schematic::{Signal, connect};
cell inv(w: Float) {
    pub let a = Signal(x=-6, y=0);
    pub let out = Signal(x=4, y=0);
    pub let vdd = Signal(x=0, y=8);
    let size = 2.;
}
cell top() {
    let u = std::schematic::inst(inv(1.), x=0, y=0);
}
";

    #[test]
    fn symbol_statements_leave_defaults_out_and_name_ports_by_path() {
        let compiled = compile(SYMBOL_CELL);
        let decl = cell_with_body(compiled.module(), compiled.body("inv").span).unwrap();
        let schematic = &compiled.cell("inv").schematic;
        let statement = |item: NewSymbolItem| symbol_statement(&item, decl, schematic);
        assert_eq!(
            statement(NewSymbolItem::Line {
                points: vec![(-4., 0.), (-2., 0.)]
            })
            .unwrap(),
            "std::symbol::line([(-4., 0.), (-2., 0.)]);"
        );
        assert_eq!(
            statement(NewSymbolItem::Polygon {
                points: vec![(-2., 2.), (-2., -2.), (2., 0.)],
                fill: true
            })
            .unwrap(),
            "std::symbol::polygon([(-2., 2.), (-2., -2.), (2., 0.)], fill=true);"
        );
        assert_eq!(
            statement(NewSymbolItem::Rect {
                x0: 0.,
                y0: -4.,
                x1: 4.,
                y1: 0.,
                fill: false
            })
            .unwrap(),
            "std::symbol::rect(x0=0., y0=-4., x1=4., y1=0.);"
        );
        assert_eq!(
            statement(NewSymbolItem::Circle {
                x: 2.25,
                y: 0.,
                r: 0.25,
                fill: false
            })
            .unwrap(),
            "std::symbol::circle(x=2.25, y=0., r=0.25);"
        );
        assert_eq!(
            statement(NewSymbolItem::Arc {
                x: 0.,
                y: 0.,
                r: 1.,
                start: 90.,
                end: 270.
            })
            .unwrap(),
            "std::symbol::arc(x=0., y=0., r=1., start=90., end=270.);"
        );
        assert_eq!(
            statement(NewSymbolItem::Text {
                text: "VDD".into(),
                x: 1.,
                y: 3.,
                size: 1.,
                align: Align::Left
            })
            .unwrap(),
            "std::symbol::text(\"VDD\", x=1., y=3.);"
        );
        assert_eq!(
            statement(NewSymbolItem::Text {
                text: "a\"b".into(),
                x: 1.,
                y: 3.,
                size: 0.5,
                align: Align::Right
            })
            .unwrap(),
            "std::symbol::text(\"a\\\"b\", x=1., y=3., size=0.5, align=std::symbol::Align::Right);"
        );
        assert_eq!(
            statement(NewSymbolItem::InstName {
                x: 2.,
                y: 2.,
                size: 1.,
                align: Align::Center
            })
            .unwrap(),
            "std::symbol::inst_name(x=2., y=2., align=std::symbol::Align::Center);"
        );
        assert_eq!(
            statement(NewSymbolItem::Param {
                name: "w".into(),
                x: 2.,
                y: -2.,
                size: 1.,
                align: Align::Left
            })
            .unwrap(),
            "std::symbol::param(\"w\", w, x=2., y=-2.);"
        );
        // `size` is a binding, not a parameter, and `h` is nothing.
        assert!(
            statement(NewSymbolItem::Param {
                name: "size".into(),
                x: 0.,
                y: 0.,
                size: 1.,
                align: Align::Left
            })
            .is_err()
        );
        assert!(
            statement(NewSymbolItem::Param {
                name: "h".into(),
                x: 0.,
                y: 0.,
                size: 1.,
                align: Align::Left
            })
            .is_err()
        );
        assert_eq!(
            statement(NewSymbolItem::Pin {
                port: 1,
                x: 4,
                y: 0,
                label: true
            })
            .unwrap(),
            "std::symbol::pin(out, x=4, y=0, label=true);"
        );
        assert_eq!(
            statement(NewSymbolItem::Pin {
                port: 0,
                x: -4,
                y: 0,
                label: false
            })
            .unwrap(),
            "std::symbol::pin(a, x=-4, y=0);"
        );
        assert!(
            statement(NewSymbolItem::Line {
                points: vec![(0., 0.)]
            })
            .is_err()
        );
        assert_static(&format!(
            "cell inv(w: Float) {{\n    pub let a = std::schematic::Signal();\n    \
             pub let out = std::schematic::Signal();\n    {}\n    {}\n    {}\n    {}\n}}\n",
            statement(NewSymbolItem::Polygon {
                points: vec![(-2., 2.), (-2., -2.), (2., 0.)],
                fill: true
            })
            .unwrap(),
            statement(NewSymbolItem::Text {
                text: "x".into(),
                x: 1.,
                y: 3.,
                size: 0.5,
                align: Align::Right
            })
            .unwrap(),
            statement(NewSymbolItem::Param {
                name: "w".into(),
                x: 2.,
                y: -2.,
                size: 1.,
                align: Align::Left
            })
            .unwrap(),
            statement(NewSymbolItem::Pin {
                port: 1,
                x: 4,
                y: 0,
                label: true
            })
            .unwrap(),
        ));
    }

    #[test]
    fn create_symbol_writes_the_generated_box_with_the_same_pins() {
        let compiled = compile(SYMBOL_CELL);
        let inv = compiled.cell("inv");
        let statements = create_symbol_statements(inv).unwrap();
        assert_eq!(
            statements,
            [
                "std::symbol::rect(x0=0., y0=-4., x1=4., y1=0.);",
                "std::symbol::inst_name(x=0., y=1.);",
                "std::symbol::text(\"inv\", x=0., y=-5.);",
                "std::symbol::pin(a, x=0, y=-2, label=true);",
                "std::symbol::pin(out, x=4, y=-2, label=true);",
                "std::symbol::pin(vdd, x=2, y=0, label=true);",
            ]
        );
        // Compiled, the pins are where the generated box put them.
        let source = SYMBOL_CELL.replacen(
            "    let size = 2.;\n",
            &format!("    let size = 2.;\n    {}\n", statements.join("\n    ")),
            1,
        );
        let drawn = compile(&source);
        let symbol = drawn
            .cell("inv")
            .symbol
            .as_ref()
            .expect("the cell now draws a symbol");
        let generated = generated_box(&inv.schematic, &inv.name);
        let positions = |symbol: &argonc::compile::Symbol| {
            symbol
                .pins
                .iter()
                .map(|pin| pin.as_ref().map(|pin| (pin.x, pin.y)))
                .collect::<Vec<_>>()
        };
        assert_eq!(positions(symbol), positions(&generated));
        assert!(symbol.pins.iter().flatten().all(|pin| pin.label.is_some()));
        assert!(
            create_symbol_statements(drawn.cell("inv")).is_err(),
            "already drawn"
        );
        // A port without a path cannot get a pin.
        let looped = compile(
            "use std::schematic::Signal;
             cell inv() {
                 pub let ports = [Signal(), Signal()];
                 for i in std::range(2) {
                     std::schematic::connect(ports[i], ports[i]);
                 }
             }
             cell top() {
                 let u = std::schematic::inst(inv(), x=0, y=0);
             }",
        );
        assert!(
            create_symbol_statements(looped.cell("inv")).is_ok(),
            "sequence elements have paths"
        );
        // A cell without ports gets a bare box.
        assert_eq!(
            create_symbol_statements(compiled.cell("top"))
                .unwrap()
                .len(),
            3
        );
    }

    const DRAWN: &str = "\
use std::schematic::Signal;
use std::symbol::{line, pin, rect};
fn stub(x: Float) {
    line([(x, 0.), (x + 1., 0.)]);
    rect(x0=x, y0=-1., x1=x + 1., y1=1.);
}
cell inv() {
    pub let a = Signal(x=-6, y=0);
    pub let out = Signal(x=4, y=0);
    rect(x0=-2., y0=-2., x1=2., y1=2.);
    stub(2.);
    pin(a, x=-4, y=0);
    pin(out, x=4, y=0, label=true);
}
cell top() {
    let u = std::schematic::inst(inv(), x=0, y=0);
}
";

    #[test]
    fn symbol_items_groups_and_pins_are_deleted_by_their_statements() {
        let compiled = compile(DRAWN);
        let edited = compiled.deleted(&[compiled.site("rect(x0=-2.")]);
        assert!(!edited.contains("rect(x0=-2."), "{edited}");
        assert!(edited.contains("stub(2.);"), "{edited}");
        // A group goes by its origin, the call in the cell body.
        assert!(matches!(
            site_of(&compiled.data, &compiled.site("stub(2.)")),
            Some((SiteKind::SymbolGroup, true))
        ));
        let edited = compiled.deleted(&[compiled.site("stub(2.)")]);
        assert!(!edited.contains("stub(2.);"), "{edited}");
        assert!(edited.contains("fn stub"), "the function stays");
        let edited = compiled.deleted(&[compiled.site("pin(a,")]);
        assert!(!edited.contains("pin(a,"), "{edited}");
        assert!(edited.contains("pin(out,"), "{edited}");
        // The items a function drew are not deleted one by one.
        let inner = compiled.cell("inv").symbol.as_ref().unwrap().items[1]
            .site
            .clone()
            .unwrap();
        assert!(inner.origin.is_some());
        assert!(compiled.delete(&[inner.span]).is_err());
    }

    #[test]
    fn renaming_a_binding_rewrites_every_reference_in_every_cell() {
        let compiled = compile(DELETION);
        let replacements = rename_replacements(
            &compiled.workspace,
            &compiled.data,
            Some(&compiled.index),
            &compiled.site("Signal(x=-6"),
            "input",
        )
        .unwrap();
        let edited = apply_replacements(compiled.source(), &replacements[&compiled.path]);
        assert!(
            edited.contains("pub let input = Signal(x=-6, y=0);"),
            "{edited}"
        );
        assert!(
            edited.contains("device(DeviceKind::Res, [input, b]"),
            "{edited}"
        );
        assert!(
            edited.contains("connect(input, n.p, route=[(-6, -4)]);"),
            "{edited}"
        );
        assert!(!edited.contains("let a ="), "{edited}");
        // The child's port is read by the parent through its instance.
        let replacements = rename_replacements(
            &compiled.workspace,
            &compiled.data,
            Some(&compiled.index),
            &compiled.site("Signal(x=0, y=0)"),
            "pin",
        )
        .unwrap();
        let edited = apply_replacements(compiled.source(), &replacements[&compiled.path]);
        assert!(
            edited.contains("pub let pin = Signal(x=0, y=0);"),
            "{edited}"
        );
        assert!(edited.contains("connect(u.pin, c);"), "{edited}");
        // A taken or reserved name, and a statement without a binding.
        let taken = rename_replacements(
            &compiled.workspace,
            &compiled.data,
            Some(&compiled.index),
            &compiled.site("Signal(x=-6"),
            "b",
        );
        assert!(taken.is_err());
        assert!(
            rename_replacements(
                &compiled.workspace,
                &compiled.data,
                Some(&compiled.index),
                &compiled.site("connect(a, n.p"),
                "w",
            )
            .is_err()
        );
    }

    #[test]
    fn copies_are_pasted_with_fresh_names_shifted_and_connected_to_each_other() {
        let compiled = compile(DELETION);
        let items = copy_statements(
            &compiled.workspace,
            &compiled.data,
            &[
                compiled.site("Signal(x=-6"),
                compiled.site("device(DeviceKind::Res, [a, b]"),
                compiled.site("device(DeviceKind::Res, signals(2)"),
                compiled.site("connect(a, n.p"),
            ],
        )
        .unwrap();
        assert_eq!(items.len(), 4);
        assert_eq!(items[0].name.as_deref(), Some("a"));
        assert!(items[0].public);
        assert_eq!(items[3].kind, ClipboardKind::Connect);
        let taken = compiled
            .cell("top")
            .schematic
            .nets
            .iter()
            .map(|net| net.name.clone())
            .chain(["m".to_owned(), "n".to_owned()])
            .collect::<HashSet<_>>();
        let statements = paste_statements(&items, (10, 2), (0., 0.), &taken).unwrap();
        assert_eq!(
            statements,
            [
                "pub let a0 = Signal(x=4, y=2);",
                "let m0 = device(DeviceKind::Res, [a0, b], \"\", value=1., x=10, y=6);",
                "let n0 = device(DeviceKind::Res, signals(2), \"\", value=2., x=10, y=-2);",
                "connect(a0, n0.p, route=[(4, -2)]);",
            ]
        );
        // Names that are free are kept.
        let statements = paste_statements(&items[..1], (0, 0), (0., 0.), &HashSet::new()).unwrap();
        assert_eq!(statements, ["pub let a = Signal(x=-6, y=0);"]);
    }

    #[test]
    fn copied_symbol_items_move_and_pins_are_left_out() {
        let compiled = compile(DRAWN);
        let items = copy_statements(
            &compiled.workspace,
            &compiled.data,
            &[compiled.site("rect(x0=-2."), compiled.site("pin(a,")],
        )
        .unwrap();
        assert!(items.iter().all(|item| item.kind == ClipboardKind::Symbol));
        let statements = paste_statements(&items, (0, 0), (1.5, -0.25), &HashSet::new()).unwrap();
        assert_eq!(statements, ["rect(x0=-0.5, y0=-2.25, x1=3.5, y1=1.75);"]);
        let line = ClipboardItem {
            kind: ClipboardKind::Symbol,
            name: None,
            public: false,
            call: "std::symbol::line([(-4., 0.), (-2., 0.5)])".into(),
        };
        let statements = paste_statements(&[line], (0, 0), (1., 1.), &HashSet::new()).unwrap();
        assert_eq!(statements, ["std::symbol::line([(-3., 1.), (-1., 1.5)]);"]);
        assert!(
            copy_statements(
                &compiled.workspace,
                &compiled.data,
                &[compiled.site("stub(2.)")]
            )
            .is_err(),
            "a group is not copied"
        );
    }

    #[test]
    fn arguments_are_rewritten_or_added() {
        let source = "cell top() {\n    let d = f(1, 2, w=3., l=4.);\n    let e = g();\n    let h = g(k=1);\n}\n";
        let ast = parse::parse_source_text(source, PathBuf::from("/virtual/lib.ar")).unwrap();
        let call_at = |text: &str| {
            let start = source.find(text).unwrap();
            ast.span2call
                .iter()
                .find(|(span, _)| span.span.start() == start)
                .map(|(_, call)| call)
                .unwrap()
        };
        let apply = |text: &str, argument: CallArgument, value: &str| {
            let call = call_at(text);
            let replacements = argument_replacements(source, call, &argument, value).unwrap();
            let edited = apply_replacements(source, &replacements);
            let span = span_after(call.span, &replacements);
            edited[span.start()..span.end()].to_owned()
        };
        assert_eq!(
            apply("f(1", CallArgument::Positional(1), "9"),
            "f(1, 9, w=3., l=4.)"
        );
        assert_eq!(
            apply("f(1", CallArgument::Positional(2), "9"),
            "f(1, 2, 9, w=3., l=4.)"
        );
        assert_eq!(
            apply("f(1", CallArgument::Keyword("w".into()), "5."),
            "f(1, 2, w=5., l=4.)"
        );
        assert_eq!(
            apply("f(1", CallArgument::Keyword("m".into()), "2"),
            "f(1, 2, w=3., l=4., m=2)"
        );
        assert_eq!(
            apply("g()", CallArgument::Positional(0), "\"x\""),
            "g(\"x\")"
        );
        assert_eq!(
            apply("g(k=1)", CallArgument::Positional(0), "\"x\""),
            "g(\"x\", k=1)"
        );
        assert!(
            argument_replacements(source, call_at("f(1"), &CallArgument::Positional(4), "1")
                .is_err()
        );
        let arguments = call_arguments(source, call_at("f(1"));
        assert_eq!(arguments.positional, ["1", "2"]);
        assert_eq!(
            arguments.keywords,
            [
                ("w".to_owned(), "3.".to_owned()),
                ("l".to_owned(), "4.".to_owned())
            ]
        );
    }

    #[test]
    fn existing_keywords_are_rewritten_in_place() {
        assert_eq!(
            rewritten(
                "Signal(x=-6",
                &[edit("x", Some("2")), edit("y", Some("-4"))]
            ),
            "Signal(x=2, y=-4)"
        );
        assert_eq!(
            rewritten("connect(a, m.p", &[edit("route", Some("[(1, 1), (2, 2)]"))]),
            "connect(a, m.p, route=[(1, 1), (2, 2)])"
        );
    }

    #[test]
    fn missing_keywords_are_added_before_the_closing_parenthesis() {
        assert_eq!(
            rewritten("Signal()", &[edit("x", Some("1")), edit("y", Some("2"))]),
            "Signal(x=1, y=2)"
        );
        assert_eq!(
            rewritten("connect(a, m.n)", &[edit("route", Some("[]"))]),
            "connect(a, m.n, route=[])"
        );
        assert_eq!(
            rewritten("device(\n", &[edit("x", Some("3")), edit("y", Some("4"))]),
            "device(\n        DeviceKind::Res,\n        [a, b],\n        \"\",\n        value=2.,\n        x=3, y=4,\n    )"
        );
    }

    #[test]
    fn removed_keywords_take_their_separator_along() {
        assert_eq!(
            rewritten("connect(a, m.p", &[edit("route", None)]),
            "connect(a, m.p)"
        );
        assert_eq!(
            rewritten("device(DeviceKind::Res, [a, b]", &[edit("angle", None)]),
            "device(DeviceKind::Res, [a, b], \"\", value=1., x=4, y=0)"
        );
        assert_eq!(
            rewritten(
                "device(DeviceKind::Res, [a, b]",
                &[edit("x", None), edit("y", None)]
            ),
            "device(DeviceKind::Res, [a, b], \"\", value=1., angle=90)"
        );
        assert_eq!(
            rewritten("Signal(x=-6", &[edit("x", None), edit("y", None)]),
            "Signal()"
        );
        // Removing a keyword that is absent changes nothing.
        assert_eq!(rewritten("Signal()", &[edit("angle", None)]), "Signal()");
    }

    #[test]
    fn mixed_edits_apply_back_to_front() {
        assert_eq!(
            rewritten(
                "device(DeviceKind::Res, [a, b]",
                &[
                    edit("x", Some("10")),
                    edit("angle", None),
                    edit("reflect", Some("true"))
                ]
            ),
            "device(DeviceKind::Res, [a, b], \"\", value=1., x=10, y=0, reflect=true)"
        );
    }
}
