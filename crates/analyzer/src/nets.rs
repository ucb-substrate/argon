//! What `compile` reports about `net=` tags and pin labels, and the `nets`
//! tool's report of each net's wiring and rough capacitance.

use std::{
    cell::RefCell,
    collections::HashMap,
    fmt::Write as _,
    fs,
    path::{Path, PathBuf},
    sync::Arc,
};

use argonc::{
    compile::{Arrayed, CellId, CompiledData, ObjectId},
    connectivity::{self, Check, Issue, Site},
    parasitics::{self, LONG_ASPECT, LONG_FRACTION, NetReport},
};
use serde_json::{Value, json};

/// Shapes shown on the path joining a short's names before the rest are elided.
const MAX_PATH: usize = 8;

/// Couplings listed for a net before the rest are summed.
const MAX_COUPLINGS: usize = 5;

/// Unnamed nets listed before the rest are summed.
const MAX_UNNAMED: usize = 5;

/// Where source spans point, as `path:line:column`, with paths shown by the
/// caller's convention.
pub(crate) struct Sources<'a> {
    show: &'a dyn Fn(&Path) -> String,
    files: RefCell<HashMap<PathBuf, Option<Arc<str>>>>,
    /// Each cell's binding names, by object.
    names: RefCell<HashMap<CellId, HashMap<ObjectId, String>>>,
}

impl<'a> Sources<'a> {
    pub(crate) fn new(show: &'a dyn Fn(&Path) -> String) -> Self {
        Self {
            show,
            files: RefCell::default(),
            names: RefCell::default(),
        }
    }

    fn at(&self, span: &argonc::ast::Span) -> String {
        let text = self
            .files
            .borrow_mut()
            .entry(span.path.to_path_buf())
            .or_insert_with(|| match argonc::parse::virtual_source(&span.path) {
                Some(source) => Some(Arc::from(source)),
                None => fs::read_to_string(&span.path).ok().map(Arc::from),
            })
            .clone();
        let (line, column) = text.map_or((1, 1), |text| line_column(&text, span.span.start()));
        format!("{}:{line}:{column}", (self.show)(&span.path))
    }

    /// The name a cell's body binds `object` to, or `instance`.
    fn name(&self, data: &CompiledData, cell: CellId, object: ObjectId) -> String {
        let mut names = self.names.borrow_mut();
        let names = names.entry(cell).or_insert_with(|| {
            let compiled = &data.cells[&cell];
            let mut names = HashMap::new();
            let root = compiled.scopes.get(&compiled.root).into_iter();
            let others = compiled
                .scopes
                .iter()
                .filter(|(id, _)| **id != compiled.root)
                .map(|(_, scope)| scope);
            for scope in root.chain(others) {
                for (_, (name, value)) in scope.bindings.iter() {
                    name_objects(name, value, &mut names);
                }
            }
            names
        });
        names
            .get(&object)
            .cloned()
            .unwrap_or_else(|| "instance".to_owned())
    }

    /// A site's layer, bounds, source line, and the instances leading to it.
    fn site(&self, data: &CompiledData, site: &Site) -> String {
        let b = site.bounds;
        let mut text = format!(
            "{} ({}, {}) to ({}, {})",
            site.layer,
            number(b[0]),
            number(b[1]),
            number(b[2]),
            number(b[3])
        );
        if let Some(span) = &site.source.span {
            let _ = write!(text, ": {}", self.at(span));
            // A shape drawn inside a function, such as a via's cuts, also
            // names the call in its cell's body.
            let origin = data.cells.get(&site.source.cell).and_then(|cell| {
                cell.scopes
                    .values()
                    .find_map(|scope| {
                        scope
                            .emit
                            .iter()
                            .find(|(object, _)| *object == site.source.object)
                            .map(|(_, emit)| emit)
                    })
                    .and_then(|emit| emit.origin.as_ref())
            });
            if let Some(origin) = origin {
                let _ = write!(text, ", called from {}", self.at(origin));
            }
        }
        if !site.source.instances.is_empty() {
            let path = site
                .source
                .instances
                .iter()
                .map(|(parent, id)| self.name(data, *parent, *id))
                .collect::<Vec<_>>();
            let _ = write!(text, " in {}", path.join(" > "));
        }
        text
    }
}

fn name_objects(name: &str, value: &Arrayed<ObjectId>, names: &mut HashMap<ObjectId, String>) {
    match value {
        Arrayed::Elem(id) => {
            names.entry(*id).or_insert_with(|| name.to_owned());
        }
        Arrayed::Array(items) => {
            for (index, item) in items.iter().enumerate() {
                name_objects(&format!("{name}[{index}]"), item, names);
            }
        }
    }
}

/// The one-based line and column of byte `offset` in `text`.
fn line_column(text: &str, offset: usize) -> (usize, usize) {
    let mut offset = offset.min(text.len());
    while !text.is_char_boundary(offset) {
        offset -= 1;
    }
    let before = &text[..offset];
    let line_start = before.rfind('\n').map_or(0, |index| index + 1);
    (
        before.matches('\n').count() + 1,
        before[line_start..].chars().count() + 1,
    )
}

/// `value` with at most three decimals and no trailing zeros.
fn number(value: f64) -> String {
    let text = format!("{:.3}", value + 0.);
    let text = text.trim_end_matches('0').trim_end_matches('.');
    if text == "-0" {
        "0".to_owned()
    } else {
        text.to_owned()
    }
}

/// The connectivity check's report for every cell with names, and how many
/// problems it found. A tag the check can't follow is a note, not a problem,
/// and a technology without vias has nothing to check against.
pub(crate) fn connectivity_lines(
    data: &CompiledData,
    sources: &Sources,
    indent: &str,
) -> (String, usize) {
    let cells = connectivity::named_cells(data, data.top);
    if cells.is_empty() || data.tech.vias.is_empty() {
        return (String::new(), 0);
    }
    let mut text = String::new();
    let mut named = 0;
    let mut problems = 0;
    let mut notes = 0;
    for cell in cells {
        let check = connectivity::check(data, cell);
        named += (0..check.conductors.nets)
            .filter(|net| !check.names[*net].is_empty())
            .count();
        let untracked = check
            .issues
            .iter()
            .filter(|issue| matches!(issue, Issue::Untracked { .. }))
            .count();
        notes += untracked;
        problems += check.issues.len() - untracked;
        text.push_str(&issue_lines(data, &check, sources, indent));
    }
    let plural = |n: usize, one: &str, many: &str| if n == 1 { one } else { many }.to_owned();
    let text = if problems > 0 {
        format!(
            "{indent}nets: {problems} connectivity {} (warnings; LVS would fail):\n{text}",
            plural(problems, "problem", "problems")
        )
    } else if notes > 0 {
        format!(
            "{indent}nets: {named} named {} the layout's connectivity, and {notes} {} unchecked:\n{text}",
            plural(named, "net matches", "nets match"),
            plural(notes, "tag is", "tags are")
        )
    } else {
        format!(
            "{indent}nets: {named} named {} the layout's connectivity\n",
            plural(named, "net matches", "nets match")
        )
    };
    (text, problems)
}

fn issue_lines(data: &CompiledData, check: &Check, sources: &Sources, indent: &str) -> String {
    let cell = &data.cells[&check.conductors.cell].name;
    let mut text = String::new();
    for issue in &check.issues {
        match issue {
            Issue::Short { names, path } => {
                let list = names
                    .iter()
                    .map(|n| format!("`{}`", n.name))
                    .collect::<Vec<_>>()
                    .join(", ");
                let _ = writeln!(text, "{indent}  short in {cell}: {list} are one net");
                for naming in names {
                    let _ = writeln!(
                        text,
                        "{indent}    {}: {}",
                        naming.name,
                        sources.site(data, &naming.site)
                    );
                }
                if path.len() > 2 {
                    let _ = writeln!(
                        text,
                        "{indent}    joined by, from `{}` to `{}`:",
                        names[0].name, names[1].name
                    );
                    let shown = if path.len() > MAX_PATH {
                        let mut shown = path[..MAX_PATH / 2].iter().collect::<Vec<_>>();
                        shown.extend(&path[path.len() - MAX_PATH / 2..]);
                        shown
                    } else {
                        path.iter().collect()
                    };
                    for (i, site) in shown.iter().enumerate() {
                        if path.len() > MAX_PATH && i == MAX_PATH / 2 {
                            let _ =
                                writeln!(text, "{indent}      … {} more", path.len() - MAX_PATH);
                        }
                        let _ = writeln!(text, "{indent}      {}", sources.site(data, site));
                    }
                }
            }
            Issue::Open { name, pieces } => {
                let _ = writeln!(
                    text,
                    "{indent}  open in {cell}: `{name}` is on {} separate nets",
                    pieces.len()
                );
                for site in pieces {
                    let _ = writeln!(text, "{indent}    {}", sources.site(data, site));
                }
            }
            Issue::Unattached { label, reason } => {
                let _ = writeln!(
                    text,
                    "{indent}  label `{}` in {cell}: {reason}: {}",
                    label.name,
                    sources.site(data, &label.site)
                );
            }
            Issue::Untracked { name, site } => {
                let _ = writeln!(
                    text,
                    "{indent}  tag `{name}` in {cell} is on a layer no via joins, so it isn't checked: {}",
                    sources.site(data, site)
                );
            }
        }
    }
    text
}

/// The `nets` tool's report of the compiled cell's nets: the named ones in
/// `only`, or all of them, then each pair in `pairs` compared.
pub(crate) fn nets_report(
    data: &CompiledData,
    sources: &Sources,
    only: &[String],
    pairs: &[(String, String)],
) -> String {
    if data.tech.vias.is_empty() {
        return format!(
            "{}: the technology file defines no `[[vias]]`, so there is no connectivity to measure nets by.",
            data.cells[&data.top].name
        );
    }
    let check = connectivity::check(data, data.top);
    let reports = parasitics::report(&check, &data.tech);
    // The distinct names on each net: several when it is shorted.
    let names_of = |net: usize| {
        let mut names = check.names[net]
            .iter()
            .map(|n| n.name.as_str())
            .collect::<Vec<_>>();
        names.sort_unstable();
        names.dedup();
        names
    };
    // Each name's nets: more than one when the name is open.
    let mut by_name: HashMap<String, Vec<&NetReport>> = HashMap::new();
    for report in &reports {
        for name in names_of(report.net) {
            by_name.entry(name.to_owned()).or_default().push(report);
        }
    }
    let label = |net: usize| match &reports[net].name {
        Some(name) => {
            let open = names_of(net)
                .into_iter()
                .find_map(|n| Some(&by_name[n]).filter(|pieces| pieces.len() > 1));
            match open {
                Some(pieces) => {
                    let k = pieces.iter().position(|r| r.net == net).unwrap_or(0) + 1;
                    format!("{name} (piece {k} of {})", pieces.len())
                }
                None => name.clone(),
            }
        }
        None => format!("unnamed net {net}"),
    };
    let mut text = String::new();
    let unnamed_count = reports.iter().filter(|r| r.name.is_none()).count();
    let _ = writeln!(
        text,
        "{}: {} named and {unnamed_count} unnamed nets. Capacitances are rough, in fF: area and perimeter to the substrate, plus overlaps with other nets; lateral coupling isn't counted.",
        data.cells[&data.top].name,
        by_name.len(),
    );
    let issues = issue_lines(data, &check, sources, "");
    if !issues.is_empty() {
        let _ = write!(
            text,
            "Connectivity problems, which make these nets differ from LVS's:\n{issues}"
        );
    }
    let mut named_reports = reports
        .iter()
        .filter(|r| r.name.is_some())
        .collect::<Vec<_>>();
    named_reports.sort_by(|a, b| a.name.cmp(&b.name));
    for name in only {
        if !by_name.contains_key(name) {
            let _ = writeln!(text, "{name}: no net has this name");
        }
    }
    for report in named_reports {
        let names = names_of(report.net);
        if !only.is_empty() && !only.iter().any(|n| names.contains(&n.as_str())) {
            continue;
        }
        text.push_str(&net_lines(report, &check, sources, data, &label));
    }
    if only.is_empty() {
        let mut unnamed = reports
            .iter()
            .filter(|r| r.name.is_none())
            .collect::<Vec<_>>();
        unnamed.sort_by(|a, b| b.total().total_cmp(&a.total()));
        for report in unnamed.iter().take(MAX_UNNAMED) {
            text.push_str(&net_lines(report, &check, sources, data, &label));
        }
        if unnamed.len() > MAX_UNNAMED {
            let rest = &unnamed[MAX_UNNAMED..];
            let _ = writeln!(
                text,
                "{} more unnamed nets, {} fF in all",
                rest.len(),
                femto(rest.iter().map(|r| r.total()).sum())
            );
        }
    }
    for (a, b) in pairs {
        text.push_str(&compare(a, b, &by_name));
    }
    text.trim_end().to_owned()
}

fn femto(attofarads: f64) -> String {
    format!("{:.3}", attofarads / 1000.)
}

/// The short name of a layer: `met1` for `met1.drawing`.
fn short(layer: &str) -> &str {
    layer.split('.').next().unwrap_or(layer)
}

fn net_lines(
    report: &NetReport,
    check: &Check,
    sources: &Sources,
    data: &CompiledData,
    label: &dyn Fn(usize) -> String,
) -> String {
    let mut text = format!(
        "{}: {} fF = substrate {}",
        label(report.net),
        femto(report.total()),
        femto(report.substrate)
    );
    // Each neighbor's couplings, summed over layer pairs, largest first.
    let mut neighbors: Vec<(usize, f64, Vec<String>)> = Vec::new();
    for coupling in &report.couplings {
        let pair = format!(
            "{}/{} {} µm²",
            short(&coupling.top),
            short(&coupling.bot),
            number(coupling.area)
        );
        match neighbors.iter_mut().find(|(n, _, _)| *n == coupling.other) {
            Some((_, cap, pairs)) => {
                *cap += coupling.cap;
                pairs.push(pair);
            }
            None => neighbors.push((coupling.other, coupling.cap, vec![pair])),
        }
    }
    neighbors.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
    for (other, cap, pairs) in neighbors.iter().take(MAX_COUPLINGS) {
        let _ = write!(
            text,
            " + {} {} ({})",
            label(*other),
            femto(*cap),
            pairs.join(", ")
        );
    }
    if neighbors.len() > MAX_COUPLINGS {
        let rest = &neighbors[MAX_COUPLINGS..];
        let _ = write!(
            text,
            " + {} more nets {}",
            rest.len(),
            femto(rest.iter().map(|n| n.1).sum())
        );
    }
    text.push('\n');
    let layers = report
        .layers
        .iter()
        .map(|l| {
            format!(
                "{} {} µm long, {} µm²",
                short(&l.layer),
                number(l.length),
                number(l.area)
            )
        })
        .collect::<Vec<_>>();
    let cuts = report
        .cuts
        .iter()
        .map(|(layer, n)| format!("{n} {}", short(layer)))
        .collect::<Vec<_>>();
    let mut line = layers.join("; ");
    if !cuts.is_empty() {
        let _ = write!(line, "; cuts: {}", cuts.join(", "));
    }
    if line.is_empty() {
        line = "no wiring".to_owned();
    }
    if report.name.is_none()
        && let Some(shape) = check.conductors.net.iter().position(|n| *n == report.net)
    {
        let _ = write!(
            line,
            "; at {}",
            sources.site(data, &check.conductors.site(shape))
        );
    }
    let _ = writeln!(text, "  {line}");
    for wire in &report.long {
        let site = check.conductors.site(wire.shape);
        let _ = writeln!(
            text,
            "  long: {} µm by {} µm, {}",
            number(wire.length),
            number(wire.width),
            sources.site(data, &site)
        );
    }
    text
}

/// Compares two names' nets, which should match, layer by layer, summing
/// over the pieces of a name that is open.
fn compare(a: &str, b: &str, by_name: &HashMap<String, Vec<&NetReport>>) -> String {
    let (Some(ra), Some(rb)) = (by_name.get(a), by_name.get(b)) else {
        let missing = if by_name.contains_key(a) { b } else { a };
        return format!("{a} / {b}: no net is named `{missing}`\n");
    };
    let mismatch = |x: f64, y: f64| {
        let scale = x.abs().max(y.abs());
        if scale > 0. && (x - y).abs() / scale > 0.05 {
            format!(" ({:+.0}%)", (y - x) / scale * 100.)
        } else {
            String::new()
        }
    };
    let mut layers = Vec::new();
    for l in ra.iter().chain(rb).flat_map(|r| &r.layers) {
        if !layers.contains(&l.layer) {
            layers.push(l.layer.clone());
        }
    }
    let use_of = |r: &[&NetReport], layer: &str| {
        r.iter()
            .flat_map(|r| &r.layers)
            .filter(|l| l.layer == layer)
            .fold((0., 0.), |(length, area), l| {
                (length + l.length, area + l.area)
            })
    };
    let cuts = |r: &[&NetReport]| {
        r.iter()
            .flat_map(|r| &r.cuts)
            .map(|(_, n)| *n)
            .sum::<usize>()
    };
    let total = |r: &[&NetReport]| r.iter().map(|r| r.total()).sum::<f64>();
    let mut parts = Vec::new();
    for layer in &layers {
        let (la, aa) = use_of(ra, layer);
        let (lb, ab) = use_of(rb, layer);
        parts.push(format!(
            "{} {} / {} µm{}, {} / {} µm²{}",
            short(layer),
            number(la),
            number(lb),
            mismatch(la, lb),
            number(aa),
            number(ab),
            mismatch(aa, ab)
        ));
    }
    parts.push(format!(
        "cuts {} / {}{}",
        cuts(ra),
        cuts(rb),
        mismatch(cuts(ra) as f64, cuts(rb) as f64)
    ));
    parts.push(format!(
        "C {} / {} fF{}",
        femto(total(ra)),
        femto(total(rb)),
        mismatch(total(ra), total(rb))
    ));
    format!("{a} / {b}: {}\n", parts.join("; "))
}

/// The `nets` tool's definition.
pub(crate) fn tool_definition(cell: &Value, workspace: &Value) -> Value {
    json!({
        "name": "nets",
        "description": format!("Report each net of a cell's layout: its wiring's length and area on each layer, its cuts, and a rough capacitance, to the substrate and to each net it overlaps on another layer, from the technology file's coefficients. Nets are the conductors the technology's vias join; a net is named by the `net=` tags and pin labels on it, and unnamed nets are listed largest first. Shapes at least {}% of the cell's longer side and {} times as long as wide are listed as long. With `pairs`, also compare nets that should match, such as a differential pair's, flagging differences over 5%. Shorts, opens, and unattached labels are listed first.", LONG_FRACTION * 100., LONG_ASPECT),
        "inputSchema": {
            "type": "object",
            "properties": {
                "cell": cell,
                "nets": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Report only these named nets. Defaults to every net."
                },
                "pairs": {
                    "type": "array",
                    "items": {
                        "type": "array",
                        "items": { "type": "string" },
                        "minItems": 2,
                        "maxItems": 2
                    },
                    "description": "Pairs of net names to compare, such as `[[\"outp\", \"outn\"]]`."
                },
                "workspace": workspace
            },
            "required": ["cell"]
        }
    })
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use serde_json::json;

    use crate::headless::Headless;

    const LIB: &str = r#"use std::layout::{rect, text};
use std::schematic::Signal;

cell tagged(short: Bool) {
    pub let a = Signal();
    pub let b = Signal();
    rect("met1.drawing", x0=0., y0=0., x1=1000., y1=320., net=a);
    rect("met2.drawing", x0=0., y0=0., x1=300., y1=320., net=if short { b } else { a });
    rect("via.drawing", x0=75., y0=85., w=150., h=150.);
    // `b` crosses `a` on met2, and has two more pieces.
    rect("met2.drawing", x0=400., y0=-1000., x1=540., y1=1000., net=b);
    rect("met1.drawing", x0=3000., y0=0., x1=3300., y1=320., net=b);
    rect("met1.drawing", x0=5000., y0=0., x1=5300., y1=320., net=b);
    rect("met1.pin", x0=800., y0=100., x1=900., y1=200.);
    text("a", "met1.label", 850., 150.);
    text("c", "met1.label", 7000., 150.);
}
"#;

    /// A library holding `LIB`, using this checkout's SKY130 technology.
    fn library() -> tempfile::TempDir {
        library_with(LIB)
    }

    fn library_with(source: &str) -> tempfile::TempDir {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
        let tech = std::fs::canonicalize(root.join("pdks/sky130/sky130.tech.toml")).unwrap();
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join("lib.ar"), source).unwrap();
        std::fs::write(
            directory.path().join("Argon.toml"),
            format!("name = \"tagged\"\ntech = \"{}\"\n", tech.display()),
        )
        .unwrap();
        directory
    }

    #[tokio::test]
    async fn untracked_tags_are_notes_and_no_vias_means_no_check() {
        const SOURCE: &str = r#"use std::layout::rect;
use std::schematic::Signal;

cell top() {
    pub let a = Signal();
    rect("met1.drawing", x0=0., y0=0., x1=1000., y1=300., net=a);
    rect("nwell.drawing", x0=0., y0=0., x1=1000., y1=1000., net=a);
}
"#;
        let library = library_with(SOURCE);
        let tools = Headless::new(library.path().to_path_buf());
        let text = call(&tools, "compile", json!({"cells": ["top()"]})).await;
        assert!(
            text.contains(
                "nets: 1 named net matches the layout's connectivity, and 1 tag is unchecked"
            ),
            "{text}"
        );
        assert!(!text.contains("LVS would fail"), "{text}");

        // A technology without vias has no connectivity to check names against.
        let basic =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../examples/tech/basic.tech.toml");
        std::fs::write(
            library.path().join("Argon.toml"),
            format!(
                "name = \"tagged\"\ntech = \"{}\"\n",
                std::fs::canonicalize(basic).unwrap().display()
            ),
        )
        .unwrap();
        let source = SOURCE
            .replace(
                "    rect(\"nwell.drawing\", x0=0., y0=0., x1=1000., y1=1000., net=a);\n",
                "",
            )
            .replace(".drawing", "");
        std::fs::write(library.path().join("lib.ar"), source).unwrap();
        let tools = Headless::new(library.path().to_path_buf());
        let text = call(&tools, "compile", json!({"cells": ["top()"]})).await;
        assert!(text.starts_with("top(): "), "{text}");
        assert!(!text.contains("nets:"), "{text}");
    }

    #[tokio::test]
    async fn names_follow_pins_wells_connections_and_diagonals() {
        let library = library_with(
            r#"use std::layout::{polygon, rect, text};
use std::schematic::{Signal, connect};

cell labels() {
    pub let a = Signal();
    pub let vpb = Signal();
    // A well pin isn't on a conductor, so it isn't checked.
    rect("nwell.drawing", x0=0., y0=2000., x1=1000., y1=3000.);
    rect("nwell.pin", x0=100., y0=2100., x1=300., y1=2300.);
    text("vpb", "nwell.label", 200., 2200.);
    // Text on a pin layer names its pin's net, here against the tag.
    rect("met1.drawing", x0=0., y0=0., x1=1000., y1=300., net=a);
    rect("met1.pin", x0=100., y0=100., x1=240., y1=240.);
    text("b", "met1.pin", 170., 170.);
}

cell joined() {
    pub let a = Signal();
    let b = Signal();
    connect(a, b);
    rect("met1.drawing", x0=0., y0=0., x1=1000., y1=300., net=b);
    rect("met1.pin", x0=100., y0=100., x1=240., y1=240.);
    text("a", "met1.label", 170., 170.);
}

cell diagonal() {
    pub let a = Signal();
    pub let b = Signal();
    // A diagonal strip whose bounding box would cover both squares.
    polygon("met1.drawing", 4, x0=0., y0=0., x1=200., y1=0., x2=2000., y2=1800., x3=1800., y3=1800.);
    rect("met1.drawing", x0=0., y0=0., x1=200., y1=200., net=a);
    rect("met1.drawing", x0=0., y0=1600., x1=200., y1=1800., net=b);
}

cell shorted() {
    pub let a = Signal();
    pub let b = Signal();
    rect("met1.drawing", x0=0., y0=0., x1=1000., y1=300., net=a);
    rect("met1.drawing", x0=900., y0=0., x1=2000., y1=300., net=b);
}
"#,
        );
        let tools = Headless::new(library.path().to_path_buf());
        let text = call(&tools, "compile", json!({"cells": ["labels()"]})).await;
        assert!(text.contains("short in labels: `a`, `b`"), "{text}");
        assert!(!text.contains("vpb"), "{text}");
        let text = call(&tools, "compile", json!({"cells": ["joined()"]})).await;
        assert!(text.contains("1 named net matches"), "{text}");
        let text = call(&tools, "compile", json!({"cells": ["diagonal()"]})).await;
        assert!(text.contains("2 named nets match"), "{text}");
        let text = call(
            &tools,
            "nets",
            json!({"cell": "shorted()", "nets": ["a"], "pairs": [["a", "b"]]}),
        )
        .await;
        assert!(!text.contains("no net has this name"), "{text}");
        assert!(!text.contains("no net is named"), "{text}");
        assert!(text.contains("a+b: "), "{text}");
    }

    async fn call(tools: &Headless, name: &str, arguments: serde_json::Value) -> String {
        let response = crate::mcp::handle(
            tools,
            json!({
                "jsonrpc": "2.0", "id": 1, "method": "tools/call",
                "params": { "name": name, "arguments": arguments }
            }),
        )
        .await
        .unwrap();
        let result = &response["result"];
        assert!(!result["isError"].as_bool().unwrap(), "{result}");
        result["content"][0]["text"].as_str().unwrap().to_owned()
    }

    /// A GDS file's bytes, with BGNLIB's and BGNSTR's timestamps zeroed.
    fn gds_without_timestamps(path: &std::path::Path) -> Vec<u8> {
        let mut bytes = std::fs::read(path).unwrap();
        let mut at = 0;
        while at + 4 <= bytes.len() {
            let length = usize::from(u16::from_be_bytes([bytes[at], bytes[at + 1]]));
            if length < 4 {
                break;
            }
            if matches!(bytes[at + 2], 0x01 | 0x05) {
                bytes[at + 4..at + length].fill(0);
            }
            at += length;
        }
        bytes
    }

    #[tokio::test]
    async fn tags_leave_the_exported_layout_and_netlist_unchanged() {
        let untagged = LIB
            .replace(", net=if short { b } else { a }", "")
            .replace(", net=a", "")
            .replace(", net=b", "");
        assert!(!untagged.contains("net="));
        let mut exports = Vec::new();
        for source in [LIB, untagged.as_str()] {
            let library = library_with(source);
            let tools = Headless::new(library.path().to_path_buf());
            call(
                &tools,
                "compile",
                json!({"cells": ["tagged(true)"], "export": "out"}),
            )
            .await;
            let directory = library.path().join("out/tagged-true");
            exports.push((
                gds_without_timestamps(&directory.join("argon.gds")),
                std::fs::read_to_string(directory.join("argon.spice")).unwrap(),
            ));
        }
        assert_eq!(exports[0], exports[1]);
    }

    #[tokio::test]
    async fn gates_split_diffusion_and_top_plate_cuts_skip_the_bottom_plate() {
        let library = library_with(
            r#"use std::layout::rect;
use std::schematic::Signal;

cell devices() {
    pub let s = Signal();
    pub let d = Signal();
    pub let bot = Signal();
    pub let top = Signal();
    // A gate between two contacted sides of one diffusion.
    rect("diff.drawing", x0=0., y0=0., x1=1000., y1=500.);
    rect("poly.drawing", x0=425., y0=-130., x1=575., y1=630.);
    rect("licon1.drawing", x0=100., y0=165., w=170., h=170.);
    rect("licon1.drawing", x0=730., y0=165., w=170., h=170.);
    rect("li1.drawing", x0=100., y0=0., x1=270., y1=500., net=s);
    rect("li1.drawing", x0=730., y0=0., x1=900., y1=500., net=d);
    // A capacitor whose top plate's via3 lands over its bottom plate.
    rect("met3.drawing", x0=0., y0=2000., x1=3000., y1=5000., net=bot);
    rect("capm.drawing", x0=500., y0=2500., x1=2500., y1=4500.);
    rect("via3.drawing", x0=1400., y0=3400., w=200., h=200.);
    rect("met4.drawing", x0=1000., y0=3000., x1=2000., y1=4000., net=top);
}
"#,
        );
        let tools = Headless::new(library.path().to_path_buf());
        let text = call(&tools, "compile", json!({"cells": ["devices()"]})).await;
        assert!(
            text.contains("nets: 4 named nets match the layout's connectivity"),
            "{text}"
        );
    }

    #[tokio::test]
    async fn compile_reports_shorts_opens_and_unattached_labels() {
        let library = library();
        let tools = Headless::new(library.path().to_path_buf());
        let text = call(&tools, "compile", json!({"cells": ["tagged(false)"]})).await;
        assert!(!text.contains("short"), "{text}");
        assert!(
            text.contains("open in tagged: `b` is on 3 separate nets"),
            "{text}"
        );
        assert!(
            text.contains("met1.drawing (3000, 0) to (3300, 320): lib.ar:12:5"),
            "{text}"
        );
        assert!(
            text.contains("label `c` in tagged: no met1.pin shape is under it"),
            "{text}"
        );

        let text = call(&tools, "compile", json!({"cells": ["tagged(true)"]})).await;
        assert!(
            text.contains("short in tagged: `a`, `b` are one net"),
            "{text}"
        );
        // The chain joining the names runs through the via.
        assert!(
            text.contains("via.drawing (75, 85) to (225, 235): lib.ar:9:5"),
            "{text}"
        );
    }

    #[tokio::test]
    async fn nets_reports_wiring_couplings_and_pairs() {
        let library = library();
        let tools = Headless::new(library.path().to_path_buf());
        let text = call(
            &tools,
            "nets",
            json!({"cell": "tagged(false)", "pairs": [["a", "b"]]}),
        )
        .await;
        // `b`'s met2 crosses `a`'s met1 over 140 by 320 nm, at 134 aF/µm².
        assert!(
            text.contains("+ b (piece 1 of 3) 0.006 (met2/met1 0.045 µm²)"),
            "{text}"
        );
        assert!(
            text.contains("met1 1 µm long, 0.32 µm²; met2 0.32 µm long, 0.096 µm²; cuts: 1 via"),
            "{text}"
        );
        assert!(text.contains("a / b: met1 1 / 0.64 µm (-36%)"), "{text}");
        let only = call(
            &tools,
            "nets",
            json!({"cell": "tagged(false)", "nets": ["a", "z"]}),
        )
        .await;
        assert!(only.contains("z: no net has this name"), "{only}");
        assert!(!only.contains("\nb "), "{only}");
    }
}
