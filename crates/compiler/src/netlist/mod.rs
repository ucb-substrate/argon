//! Netlist export. A shared core walks the schematic hierarchy under the top
//! cell, merges identical subcircuits, and names them; the SPICE and Spectre
//! writers only render lines.

mod spectre;
mod spice;

use std::collections::{HashMap, HashSet};
use std::path::Path;

use anyhow::Result;
use indexmap::IndexMap;
use indexmap::map::Entry;

use crate::compile::{
    self, CellId, CompileOutput, CompiledData, DeviceKind, ExecErrorCompileOutput, Namer, NetIdx,
    ParamValue, Schematic,
};

/// A netlist format.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NetlistFormat {
    /// ngspice-compatible SPICE.
    Spice,
    Spectre,
}

/// Net names no writer emits, compared case-insensitively.
const RESERVED_NETS: &[&str] = &["0", "gnd"];

/// Names Spectre reads as netlist keywords, compared case-insensitively.
const SPECTRE_KEYWORDS: &[&str] = &[
    "model",
    "parameters",
    "subckt",
    "ends",
    "simulator",
    "include",
    "if",
    "else",
    "inline",
];

/// The Spectre master of a resistor without a model.
const SPECTRE_RESISTOR: &str = "resistor";

/// The Spectre master of a capacitor without a model.
const SPECTRE_CAPACITOR: &str = "capacitor";

impl NetlistFormat {
    /// The names this format never emits for a user net.
    pub fn reserved_nets(self) -> Vec<&'static str> {
        let extra = match self {
            NetlistFormat::Spice => &[][..],
            NetlistFormat::Spectre => SPECTRE_KEYWORDS,
        };
        RESERVED_NETS.iter().chain(extra).copied().collect()
    }

    /// The names this format never emits for a subcircuit.
    pub fn reserved_subckts(self) -> Vec<&'static str> {
        match self {
            NetlistFormat::Spice => Vec::new(),
            NetlistFormat::Spectre => [SPECTRE_RESISTOR, SPECTRE_CAPACITOR]
                .into_iter()
                .chain(SPECTRE_KEYWORDS.iter().copied())
                .collect(),
        }
    }
}

/// Options for writing a netlist.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NetlistOptions {
    /// Wrap lines longer than this many characters; 0 disables wrapping.
    pub width: usize,
}

impl Default for NetlistOptions {
    fn default() -> Self {
        Self { width: 80 }
    }
}

/// The netlist of the schematic hierarchy under `data`'s top cell.
pub fn netlist(data: &CompiledData, format: NetlistFormat, options: &NetlistOptions) -> String {
    let netlist = Netlist::build(data, &format.reserved_nets(), &format.reserved_subckts());
    match format {
        NetlistFormat::Spice => spice::render(&netlist, options.width),
        NetlistFormat::Spectre => spectre::render(&netlist, options.width),
    }
}

impl CompileOutput {
    /// Writes the schematic hierarchy under the top cell as a netlist. An
    /// output without compiled cells writes nothing.
    pub fn to_netlist(
        &self,
        path: impl AsRef<Path>,
        format: NetlistFormat,
        options: &NetlistOptions,
    ) -> Result<()> {
        if let CompileOutput::Valid(data)
        | CompileOutput::ExecErrors(ExecErrorCompileOutput {
            errors: _,
            output: Some(data),
        }) = self
        {
            let text = netlist(data, format, options);
            crate::write_atomically(path.as_ref(), ".argon-netlist", |temp| {
                Ok(std::fs::write(temp, text)?)
            })?;
        }
        Ok(())
    }
}

/// The index of a subcircuit in a [`Netlist`].
type SubcktId = usize;

/// The subcircuits of a netlist, in no particular format.
#[derive(Debug)]
struct Netlist {
    /// Each distinct subcircuit, keyed by its cell's base name and its
    /// contents, with its unique name. Children precede their parents, so the
    /// top cell is last.
    subckts: IndexMap<(String, Subckt), String>,
}

/// A cell's schematic, with its nets named for the target format.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct Subckt {
    ports: Vec<String>,
    elements: Vec<Element>,
}

/// An instance or device of a [`Subckt`], in creation order.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum Element {
    Instance {
        name: String,
        /// The net on each port of `child`.
        nets: Vec<String>,
        child: SubcktId,
    },
    Device {
        name: String,
        kind: DeviceKind,
        nets: Vec<String>,
        /// The formatted `value` of a resistor or capacitor.
        value: Option<String>,
        model: Option<String>,
        /// The other parameters with formatted values, in source order.
        params: Vec<(String, String)>,
    },
}

impl Netlist {
    /// Collects the subcircuits under `data.top`, naming nets and subcircuits
    /// so that they avoid `reserved_nets` and `reserved_subckts`.
    fn build(data: &CompiledData, reserved_nets: &[&str], reserved_subckts: &[&str]) -> Self {
        let mut builder = Builder {
            data,
            reserved_nets,
            subckts: IndexMap::new(),
            visited: HashMap::new(),
            names: Namer::with_reserved(reserved_subckts.iter().copied()),
        };
        builder.subckt(data.top);
        Self {
            subckts: builder.subckts,
        }
    }

    /// Each subcircuit's name and contents, the top cell last.
    fn subckts(&self) -> impl Iterator<Item = (&str, &Subckt)> {
        self.subckts
            .iter()
            .map(|((_, subckt), name)| (name.as_str(), subckt))
    }

    /// The name of subcircuit `id`.
    fn name(&self, id: SubcktId) -> &str {
        &self.subckts[id]
    }

    /// The name of the top cell's subcircuit.
    fn top(&self) -> &str {
        &self.subckts[self.subckts.len() - 1]
    }
}

struct Builder<'a> {
    data: &'a CompiledData,
    reserved_nets: &'a [&'a str],
    subckts: IndexMap<(String, Subckt), String>,
    /// The subcircuit of each cell already converted.
    visited: HashMap<CellId, SubcktId>,
    /// Subcircuit names, unique across the netlist.
    names: Namer,
}

impl Builder<'_> {
    /// The subcircuit of `cell`, added after its children unless an identical
    /// one already exists.
    fn subckt(&mut self, cell: CellId) -> SubcktId {
        if let Some(id) = self.visited.get(&cell) {
            return *id;
        }
        let data = self.data;
        let compiled = &data.cells[&cell];
        let schematic = &compiled.schematic;
        let children = schematic
            .instances
            .iter()
            .map(|instance| self.subckt(instance.cell))
            .collect::<Vec<_>>();
        let nets = net_names(schematic, self.reserved_nets);
        let nets_of = |terminals: &[NetIdx]| {
            terminals
                .iter()
                .map(|net| nets[*net as usize].clone())
                .collect::<Vec<_>>()
        };
        let elements = schematic
            .elements
            .iter()
            .map(|element| match *element {
                compile::Element::Instance(index) => {
                    let instance = &schematic.instances[index];
                    Element::Instance {
                        name: instance.name.clone(),
                        nets: nets_of(&instance.terminals),
                        child: children[index],
                    }
                }
                compile::Element::Device(index) => {
                    let device = &schematic.devices[index];
                    Element::Device {
                        name: device.name.clone(),
                        kind: device.kind,
                        nets: nets_of(&device.terminals),
                        value: device.value.as_ref().map(format_param),
                        model: device.model.clone(),
                        params: device
                            .params
                            .iter()
                            .map(|(name, value)| (name.clone(), format_param(value)))
                            .collect(),
                    }
                }
            })
            .collect();
        let subckt = Subckt {
            ports: nets_of(&schematic.ports),
            elements,
        };
        let entry = self.subckts.entry((base_name(&compiled.name), subckt));
        let id = entry.index();
        if let Entry::Vacant(entry) = entry {
            let name = self.names.claim(&entry.key().0);
            entry.insert(name);
        }
        self.visited.insert(cell, id);
        id
    }
}

/// The names of `schematic`'s nets, made unique against `reserved` and each
/// other, ports first.
fn net_names(schematic: &Schematic, reserved: &[&str]) -> Vec<String> {
    let mut namer = Namer::with_reserved(reserved.iter().copied());
    let ports = schematic.ports.iter().copied().collect::<HashSet<_>>();
    let others = (0..schematic.nets.len() as NetIdx).filter(|net| !ports.contains(net));
    let mut names = vec![String::new(); schematic.nets.len()];
    for net in schematic.ports.iter().copied().chain(others) {
        names[net as usize] = namer.claim(&schematic.nets[net as usize].name);
    }
    names
}

/// `name` with every character outside `[A-Za-z0-9_]` replaced by `_`.
fn base_name(name: &str) -> String {
    let base = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect::<String>();
    if base.is_empty() {
        "_".to_owned()
    } else {
        base
    }
}

/// `value` as both formats write it.
fn format_param(value: &ParamValue) -> String {
    match value {
        ParamValue::Float(value) => format_float(*value),
        ParamValue::Int(value) => value.to_string(),
        ParamValue::String(value) => value.clone(),
    }
}

/// `value` without an exponent, unless the exponent form is at least 3
/// characters shorter.
fn format_float(value: f64) -> String {
    let plain = value.to_string();
    let exponent = format!("{value:e}");
    if exponent.len() + 3 <= plain.len() {
        exponent
    } else {
        plain
    }
}

/// How a format breaks a long line.
struct LineStyle {
    /// Starts the first line.
    indent: &'static str,
    /// Starts each continuation line.
    continuation: &'static str,
    /// Ends each line that continues on the next.
    suffix: &'static str,
}

/// Appends `tokens` to `out` as one logical line, breaking between tokens so
/// that each line fits in `width` characters if its tokens allow. A width of
/// 0 never breaks.
fn write_line(out: &mut String, tokens: &[String], style: &LineStyle, width: usize) {
    let suffix = style.suffix.chars().count();
    out.push_str(style.indent);
    let mut len = style.indent.chars().count();
    let mut empty = true;
    for (index, token) in tokens.iter().enumerate() {
        let chars = token.chars().count();
        // A token followed by more must leave room to break after it.
        let reserve = if index + 1 < tokens.len() { suffix } else { 0 };
        if !empty && width > 0 && len + 1 + chars + reserve > width {
            out.push_str(style.suffix);
            out.push('\n');
            out.push_str(style.continuation);
            len = style.continuation.chars().count();
            empty = true;
        }
        if !empty {
            out.push(' ');
            len += 1;
        }
        out.push_str(token);
        len += chars;
        empty = false;
    }
    out.push('\n');
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::WorkspaceConfig;
    use crate::compile::{CellArg, CompileInput, compile};
    use crate::parse::{WorkspaceParseAst, parse_source_text, parse_workspace_with_std, with_std};

    const EXAMPLES_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../examples");

    /// The imports every test source starts with.
    const PRELUDE: &str = "use std::schematic::{DeviceKind, Signal, connect, device};\n\
                           use std::schematic::inst as sinst;\n";

    fn compile_cell(ast: &WorkspaceParseAst, cell: &str, args: Vec<CellArg>) -> CompiledData {
        let tech = PathBuf::from(EXAMPLES_DIR).join("tech/basic.tech.toml");
        let output = compile(
            ast,
            CompileInput {
                cell: &[cell],
                args,
            },
            &WorkspaceConfig::default().with_tech(Some(tech)),
        );
        let CompileOutput::Valid(data) = output else {
            panic!("`{cell}` should compile: {output:#?}");
        };
        data
    }

    /// `top()` of `body`, after the schematic imports.
    fn compile_top(body: &str) -> CompiledData {
        let root = parse_source_text(format!("{PRELUDE}{body}"), PathBuf::from("/virtual/lib.ar"))
            .expect("source should parse");
        compile_cell(&with_std(root), "top", Vec::new())
    }

    /// `inv(2., 1., 2)` from the `schematic_inverter` example.
    fn inverter() -> CompiledData {
        let parsed = parse_workspace_with_std(format!("{EXAMPLES_DIR}/schematic_inverter/lib.ar"));
        assert!(
            parsed.static_errors().is_empty(),
            "{:?}",
            parsed.static_errors()
        );
        compile_cell(
            &parsed.ast(),
            "inv",
            vec![CellArg::Float(2.), CellArg::Float(1.), CellArg::Int(2)],
        )
    }

    fn spice(data: &CompiledData) -> String {
        netlist(data, NetlistFormat::Spice, &NetlistOptions::default())
    }

    fn spectre(data: &CompiledData) -> String {
        netlist(data, NetlistFormat::Spectre, &NetlistOptions::default())
    }

    /// The netlist in `format` at `width`.
    fn wrapped(data: &CompiledData, format: NetlistFormat, width: usize) -> String {
        netlist(data, format, &NetlistOptions { width })
    }

    /// `text` without its header comments, which every test shares.
    fn body(text: &str) -> &str {
        let start = text
            .find("\n\n")
            .expect("the header ends with a blank line");
        &text[start + 2..]
    }

    /// The tokens of each logical line of `text`, with continuations joined.
    fn logical_lines(text: &str, format: NetlistFormat) -> Vec<Vec<String>> {
        let mut lines: Vec<Vec<String>> = Vec::new();
        let mut spectre_continues = false;
        for line in text.lines() {
            let (line, continued) = match format {
                NetlistFormat::Spice => match line.strip_prefix("+ ") {
                    Some(line) => (line, true),
                    None => (line, false),
                },
                NetlistFormat::Spectre => {
                    let continued = spectre_continues;
                    spectre_continues = line.ends_with(" \\");
                    (line.strip_suffix(" \\").unwrap_or(line), continued)
                }
            };
            let tokens = line.split_whitespace().map(str::to_owned);
            match lines.last_mut() {
                Some(last) if continued => last.extend(tokens),
                _ => lines.push(tokens.collect()),
            }
        }
        lines
    }

    #[test]
    fn the_inverter_matches_the_spice_golden() {
        assert_eq!(
            spice(&inverter()),
            "* SPICE netlist generated by Argon.
* Top cell: inv

.subckt mos d g s b
X0 d g s b sky130_fd_pr__nfet_01v8 l=0.15 w=1 nf=2
.ends mos

.subckt mos_1 d g s b
X0 d g s b sky130_fd_pr__pfet_01v8 l=0.15 w=2 nf=2
.ends mos_1

.subckt inv a out vdd vss
Xnmos out a vss vss mos
Xpmos out a vdd vdd mos_1
.ends inv
"
        );
    }

    #[test]
    fn the_inverter_matches_the_spectre_golden() {
        assert_eq!(
            spectre(&inverter()),
            "// Spectre netlist generated by Argon.
// Top cell: inv
simulator lang=spectre

subckt mos d g s b
    X0 (d g s b) sky130_fd_pr__nfet_01v8 l=0.15 w=1 nf=2
ends mos

subckt mos_1 d g s b
    X0 (d g s b) sky130_fd_pr__pfet_01v8 l=0.15 w=2 nf=2
ends mos_1

subckt inv a out vdd vss
    Xnmos (out a vss vss) mos
    Xpmos (out a vdd vdd) mos_1
ends inv
"
        );
    }

    #[test]
    fn identical_subcircuits_are_merged() {
        let data = compile_top(
            "cell res(v: Float) {
                 pub let p = Signal();
                 pub let n = Signal();
                 device(DeviceKind::Res, [p, n], \"\", value=v);
             }
             cell top() {
                 pub let a = Signal();
                 pub let b = Signal();
                 let r1 = sinst(res(1000.));
                 let r2 = sinst(res(1000.));
                 let r3 = sinst(res(2000.));
                 connect(a, r1.p);
                 connect(b, r1.n);
                 connect(a, r2.p);
                 connect(b, r2.n);
                 connect(a, r3.p);
                 connect(b, r3.n);
             }",
        );
        let instances = &data.cells[&data.top].schematic.instances;
        assert_ne!(
            instances[0].cell, instances[1].cell,
            "the two placements should be distinct cells for the merge to matter"
        );
        assert_eq!(
            body(&spice(&data)),
            ".subckt res p n
R0 p n 1000
.ends res

.subckt res_1 p n
R0 p n 2000
.ends res_1

.subckt top a b
Xr1 a b res
Xr2 a b res
Xr3 a b res_1
.ends top
"
        );
        assert_eq!(
            body(&spectre(&data)),
            "subckt res p n
    R0 (p n) resistor r=1000
ends res

subckt res_1 p n
    R0 (p n) resistor r=2000
ends res_1

subckt top a b
    Xr1 (a b) res
    Xr2 (a b) res
    Xr3 (a b) res_1
ends top
"
        );
    }

    #[test]
    fn module_qualified_cells_are_named_by_their_path() {
        let root = parse_source_text(
            format!(
                "{PRELUDE}mod sub;\n\
                 cell inner() {{ pub let a = Signal(); }}\n\
                 cell top() {{\n\
                     pub let a = Signal();\n\
                     let i = sinst(sub::inner());\n\
                     let j = sinst(inner());\n\
                     connect(a, i.a);\n\
                     connect(a, j.a);\n\
                 }}\n"
            ),
            PathBuf::from("/virtual/lib.ar"),
        )
        .unwrap();
        let sub = parse_source_text(
            "use std::schematic::Signal;\ncell inner() { pub let a = Signal(); }\n",
            PathBuf::from("/virtual/sub.ar"),
        )
        .unwrap();
        let mut ast = with_std(root);
        ast.insert(vec!["sub".to_owned()], sub);
        let data = compile_cell(&ast, "top", Vec::new());
        assert_eq!(
            body(&spice(&data)),
            ".subckt sub__inner a
.ends sub__inner

.subckt inner a
.ends inner

.subckt top a
Xi a sub__inner
Xj a inner
.ends top
"
        );
        assert_eq!(
            body(&spectre(&data)),
            "subckt sub__inner a
ends sub__inner

subckt inner a
ends inner

subckt top a
    Xi (a) sub__inner
    Xj (a) inner
ends top
"
        );
    }

    #[test]
    fn base_names_replace_other_characters() {
        assert_eq!(base_name("inv"), "inv");
        assert_eq!(base_name("a::b::c"), "a__b__c");
        assert_eq!(base_name("my cell-2"), "my_cell_2");
        assert_eq!(base_name(""), "_");
    }

    #[test]
    fn a_cell_without_schematic_content_is_an_empty_subcircuit() {
        let data = compile_top(
            "cell pad() {
                 let r = std::layout::rect(\"met1\", x0=0., y0=0., x1=10., y1=10.);
             }
             cell top() {
                 pub let a = Signal();
                 let p = sinst(pad());
             }",
        );
        assert_eq!(
            body(&spice(&data)),
            ".subckt pad
.ends pad

.subckt top a
Xp pad
.ends top
"
        );
        assert_eq!(
            body(&spectre(&data)),
            "subckt pad
ends pad

subckt top a
    Xp () pad
ends top
"
        );
    }

    #[test]
    fn a_top_cell_without_schematic_content_is_an_empty_subcircuit() {
        let data = compile_top("cell top() {}");
        assert_eq!(body(&spice(&data)), ".subckt top\n.ends top\n");
        assert_eq!(body(&spectre(&data)), "subckt top\nends top\n");
    }

    #[test]
    fn devices_are_written_by_kind() {
        let data = compile_top(
            "cell top() {
                 pub let a = Signal();
                 pub let b = Signal();
                 pub let c = Signal();
                 pub let d = Signal();
                 device(DeviceKind::Res, [a, b], \"\", value=1000.);
                 device(DeviceKind::Res, [a, b], \"rpoly\", value=2000., w=1.);
                 device(DeviceKind::Res, [a, b], \"rpoly\", w=1.);
                 device(DeviceKind::Cap, [a, b], \"\", value=1e-12);
                 device(DeviceKind::Cap, [a, b], \"cmim\", w=2., l=2.);
                 device(DeviceKind::Mos, [a, b, c, d], \"nch\", w=2., l=0.15);
                 device(DeviceKind::Diode, [a, b], \"dio\");
                 device(DeviceKind::Bjt, [a, b, c], \"npn\", area=2);
                 device(DeviceKind::Subckt, [a, b, c], \"sub\", mode=\"fast\");
             }",
        );
        assert_eq!(
            body(&spice(&data)),
            ".subckt top a b c d
R0 a b 1000
R1 a b 2000 rpoly w=1
R2 a b rpoly w=1
C3 a b 1e-12
C4 a b cmim w=2 l=2
M5 a b c d nch w=2 l=0.15
D6 a b dio
Q7 a b c npn area=2
X8 a b c sub mode=fast
.ends top
"
        );
        assert_eq!(
            body(&spectre(&data)),
            "subckt top a b c d
    R0 (a b) resistor r=1000
    R1 (a b) rpoly r=2000 w=1
    R2 (a b) rpoly w=1
    C3 (a b) capacitor c=1e-12
    C4 (a b) cmim w=2 l=2
    M5 (a b c d) nch w=2 l=0.15
    D6 (a b) dio
    Q7 (a b c) npn area=2
    X8 (a b c) sub mode=fast
ends top
"
        );
    }

    #[test]
    fn instances_and_devices_interleave_in_creation_order() {
        let data = compile_top(
            "cell r() { pub let a = Signal(); }
             cell top() {
                 pub let a = Signal();
                 device(DeviceKind::Res, [a, a], \"\", value=1.);
                 let m = sinst(r());
                 device(DeviceKind::Cap, [a, a], \"\", value=1.);
                 connect(a, m.a);
             }",
        );
        assert_eq!(
            body(&spice(&data)),
            ".subckt r a
.ends r

.subckt top a
R0 a a 1
Xm a r
C1 a a 1
.ends top
"
        );
    }

    #[test]
    fn ints_and_strings_are_written_verbatim() {
        let data = compile_top(
            "cell top() {
                 pub let a = Signal();
                 device(DeviceKind::Subckt, [a], \"sub\", n=-3, big=1000000, expr=\"'w*2'\");
             }",
        );
        assert!(
            spice(&data).contains("\nX0 a sub n=-3 big=1000000 expr='w*2'\n"),
            "{}",
            spice(&data)
        );
        assert!(
            spectre(&data).contains("\n    X0 (a) sub n=-3 big=1000000 expr='w*2'\n"),
            "{}",
            spectre(&data)
        );
    }

    #[test]
    fn floats_use_an_exponent_only_when_much_shorter() {
        for (value, written) in [
            (0.15, "0.15"),
            (2., "2"),
            (1000., "1000"),
            (10000., "10000"),
            (0.0001, "0.0001"),
            (1e-12, "1e-12"),
            (0.00001, "1e-5"),
            (1000000., "1e6"),
            (2.5e3, "2500"),
            (4.7e-15, "4.7e-15"),
            (0., "0"),
            (-0.15, "-0.15"),
            (-0.0001, "-0.0001"),
            (-0.00001, "-1e-5"),
            (-1e-12, "-1e-12"),
            (-1000000., "-1e6"),
        ] {
            assert_eq!(format_float(value), written, "{value}");
        }
    }

    #[test]
    fn float_parameters_are_formatted_in_both_formats() {
        let data = compile_top(
            "cell top() {
                 pub let a = Signal();
                 pub let b = Signal();
                 device(DeviceKind::Cap, [a, b], \"\", value=0.000000000001, w=0.0001, l=1000000.);
             }",
        );
        assert!(spice(&data).contains("\nC0 a b 1e-12 w=0.0001 l=1e6\n"));
        assert!(spectre(&data).contains("\n    C0 (a b) capacitor c=1e-12 w=0.0001 l=1e6\n"));
    }

    #[test]
    fn reserved_net_names_are_renamed() {
        let data = compile_top(
            "cell port() { pub let gnd = Signal(); }
             cell upper() { pub let GND = Signal(); }
             cell internal() {
                 pub let a = Signal();
                 let gnd = Signal();
                 device(DeviceKind::Res, [a, gnd], \"\", value=1.);
             }
             cell taken() {
                 pub let gnd_1 = Signal();
                 pub let gnd = Signal();
             }
             cell top() {
                 pub let a = Signal();
                 let p = sinst(port());
                 let u = sinst(upper());
                 let i = sinst(internal());
                 let t = sinst(taken());
                 connect(a, p.gnd);
                 connect(a, u.GND);
                 connect(a, i.a);
                 connect(a, t.gnd);
             }",
        );
        let expected = ".subckt port gnd_1
.ends port

.subckt upper GND_1
.ends upper

.subckt internal a
R0 a gnd_1 1
.ends internal

.subckt taken gnd_1 gnd_2
.ends taken

.subckt top a
Xp a port
Xu a upper
Xi a internal
Xt t_gnd_1 a taken
.ends top
";
        assert_eq!(body(&spice(&data)), expected);
        let spectre = spectre(&data);
        for line in [
            "subckt port gnd_1",
            "    R0 (a gnd_1) resistor r=1",
            "subckt taken gnd_1 gnd_2",
        ] {
            assert!(spectre.contains(&format!("\n{line}\n")), "{spectre}");
        }
    }

    #[test]
    fn spectre_reserves_its_primitive_masters() {
        let data = compile_top(
            "cell resistor() {
                 pub let p = Signal();
                 pub let n = Signal();
                 device(DeviceKind::Res, [p, n], \"\", value=1.);
             }
             cell top() {
                 pub let a = Signal();
                 let r = sinst(resistor());
                 connect(a, r.p);
             }",
        );
        assert_eq!(
            body(&spectre(&data)),
            "subckt resistor_1 p n
    R0 (p n) resistor r=1
ends resistor_1

subckt top a
    Xr (a r_n) resistor_1
ends top
"
        );
        assert!(spice(&data).contains("\n.subckt resistor p n\n"));
        assert!(spice(&data).contains("\nXr a r_n resistor\n"));
    }

    #[test]
    fn spectre_reserves_its_keywords() {
        let data = compile_top(
            "cell subckt() { pub let model = Signal(); }
             cell top() {
                 pub let a = Signal();
                 let s = sinst(subckt());
                 connect(a, s.model);
             }",
        );
        assert_eq!(
            body(&spectre(&data)),
            "subckt subckt_1 model_1
ends subckt_1

subckt top a
    Xs (a) subckt_1
ends top
"
        );
        assert_eq!(
            body(&spice(&data)),
            ".subckt subckt model
.ends subckt

.subckt top a
Xs a subckt
.ends top
"
        );
    }

    #[test]
    fn reserved_names_are_read_from_the_format_lists() {
        let data = compile_top("cell top() { pub let vdd = Signal(); }");
        let mut nets = NetlistFormat::Spice.reserved_nets();
        let mut subckts = NetlistFormat::Spice.reserved_subckts();
        let names = |nets: &[&str], subckts: &[&str]| {
            let netlist = Netlist::build(&data, nets, subckts);
            let (name, subckt) = netlist.subckts().last().expect("the top cell is last");
            (name.to_owned(), subckt.ports.clone())
        };
        assert_eq!(
            names(&nets, &subckts),
            ("top".to_owned(), vec!["vdd".to_owned()])
        );
        nets.push("VDD");
        subckts.push("top");
        assert_eq!(
            names(&nets, &subckts),
            ("top_1".to_owned(), vec!["vdd_1".to_owned()])
        );
    }

    /// A schematic with a long header, a long instance line, and a long
    /// device line.
    fn long_lines() -> CompiledData {
        let signals = vec!["Signal()"; 16].join(", ");
        compile_top(&format!(
            "cell bus() {{
                 pub let data = [{signals}];
                 device(DeviceKind::Subckt, [data[0], data[1], data[2], data[3]],
                        \"sky130_fd_pr__nfet_01v8\", l=0.15, w=1., nf=2, ad=0.29, sa=0.29,
                        pd=2.58, ps=2.58);
             }}
             cell top() {{
                 pub let a = Signal();
                 let b = sinst(bus());
                 connect(a, b.data[0]);
             }}"
        ))
    }

    #[test]
    fn long_device_lines_wrap_between_tokens() {
        let data = long_lines();
        assert!(
            spice(&data).contains(
                "\nX0 data_0 data_1 data_2 data_3 sky130_fd_pr__nfet_01v8 l=0.15 w=1 nf=2 ad=0.29\n\
                 + sa=0.29 pd=2.58 ps=2.58\n"
            ),
            "{}",
            spice(&data)
        );
        assert!(
            spectre(&data).contains(
                "\n    X0 (data_0 data_1 data_2 data_3) sky130_fd_pr__nfet_01v8 l=0.15 w=1 nf=2 \\\n        \
                 ad=0.29 sa=0.29 pd=2.58 ps=2.58\n"
            ),
            "{}",
            spectre(&data)
        );
    }

    #[test]
    fn long_subcircuit_headers_wrap() {
        let data = long_lines();
        let spice = spice(&data);
        assert!(
            spice.contains(
                "\n.subckt bus data_0 data_1 data_2 data_3 data_4 data_5 data_6 data_7 data_8\n\
                 + data_9 data_10 data_11 data_12 data_13 data_14 data_15\n"
            ),
            "{spice}"
        );
        let spectre = spectre(&data);
        assert!(
            spectre.contains(
                "\nsubckt bus data_0 data_1 data_2 data_3 data_4 data_5 data_6 data_7 data_8 \\\n        \
                 data_9 data_10 data_11 data_12 data_13 data_14 data_15\n"
            ),
            "{spectre}"
        );
    }

    #[test]
    fn spectre_nets_wrap_inside_their_parentheses() {
        let data = long_lines();
        let spectre = spectre(&data);
        let instance = spectre
            .split_once("    Xb ")
            .map(|(_, rest)| rest)
            .and_then(|rest| rest.split_once("\nends top"))
            .map(|(instance, _)| instance)
            .unwrap_or_else(|| panic!("{spectre}"));
        let lines = instance.lines().collect::<Vec<_>>();
        assert!(lines.len() > 1, "{spectre}");
        assert!(lines[0].starts_with("(a b_data_1 "), "{spectre}");
        assert!(lines[0].ends_with(" \\"), "{spectre}");
        assert!(
            lines[lines.len() - 1].ends_with(" b_data_15) bus"),
            "{spectre}"
        );
    }

    #[test]
    fn wrapped_lines_fit_and_keep_their_tokens() {
        let data = long_lines();
        for format in [NetlistFormat::Spice, NetlistFormat::Spectre] {
            let unwrapped = wrapped(&data, format, 0);
            for width in [12, 20, 40, 80] {
                let text = wrapped(&data, format, width);
                for line in text.lines() {
                    let fixed = ["*", "//", "simulator ", ".ends ", "ends "]
                        .iter()
                        .any(|prefix| line.starts_with(prefix));
                    // Only a token too long for any line may overflow, alone.
                    let alone = line
                        .split_whitespace()
                        .filter(|token| !matches!(*token, "+" | "\\"))
                        .count()
                        == 1;
                    assert!(
                        fixed || alone || line.chars().count() <= width,
                        "{format:?} at {width}: `{line}`\n{text}"
                    );
                }
                assert_eq!(
                    logical_lines(&text, format),
                    logical_lines(&unwrapped, format),
                    "{format:?} at {width}\n{text}"
                );
            }
        }
    }

    #[test]
    fn a_zero_width_never_wraps() {
        let data = long_lines();
        for format in [NetlistFormat::Spice, NetlistFormat::Spectre] {
            let text = wrapped(&data, format, 0);
            assert!(!text.lines().any(|line| line.starts_with("+ ")), "{text}");
            assert!(!text.lines().any(|line| line.ends_with(" \\")), "{text}");
            assert!(
                text.lines().any(|line| line.chars().count() > 100),
                "{text}"
            );
        }
    }

    #[test]
    fn comments_are_never_wrapped() {
        let data = compile_top(
            "cell a_cell_with_a_rather_long_name() {}\ncell top() { let m = sinst(a_cell_with_a_rather_long_name()); }",
        );
        let data = CompiledData {
            top: data
                .cells
                .iter()
                .find(|(_, cell)| cell.name == "a_cell_with_a_rather_long_name")
                .map(|(id, _)| *id)
                .expect("the cell is compiled"),
            ..data
        };
        for format in [NetlistFormat::Spice, NetlistFormat::Spectre] {
            let text = wrapped(&data, format, 12);
            assert!(
                text.lines()
                    .any(|line| line.ends_with("Top cell: a_cell_with_a_rather_long_name")),
                "{text}"
            );
        }
    }

    fn tokens(tokens: &[&str]) -> Vec<String> {
        tokens.iter().map(|token| (*token).to_owned()).collect()
    }

    #[test]
    fn an_oversized_token_sits_alone() {
        const SPICE: LineStyle = LineStyle {
            indent: "",
            continuation: "+ ",
            suffix: "",
        };
        const SPECTRE: LineStyle = LineStyle {
            indent: "    ",
            continuation: "        ",
            suffix: " \\",
        };
        let line = tokens(&["X0", "a", "a_very_long_model_name", "w=1"]);
        let mut out = String::new();
        write_line(&mut out, &line, &SPICE, 12);
        assert_eq!(out, "X0 a\n+ a_very_long_model_name\n+ w=1\n");
        let mut out = String::new();
        write_line(&mut out, &line, &SPECTRE, 12);
        assert_eq!(
            out,
            "    X0 a \\\n        a_very_long_model_name \\\n        w=1\n"
        );
        // An oversized first token still starts the first line.
        let mut out = String::new();
        write_line(&mut out, &tokens(&["a_very_long_name", "b"]), &SPICE, 8);
        assert_eq!(out, "a_very_long_name\n+ b\n");
    }

    #[test]
    fn a_broken_line_leaves_room_for_its_continuation_mark() {
        const STYLE: LineStyle = LineStyle {
            indent: "",
            continuation: "  ",
            suffix: " \\",
        };
        // `a bb` fits in 4 columns, but `a bb \` doesn't, so `bb` moves down
        // unless it is the last token.
        let mut out = String::new();
        write_line(&mut out, &tokens(&["a", "bb"]), &STYLE, 4);
        assert_eq!(out, "a bb\n");
        let mut out = String::new();
        write_line(&mut out, &tokens(&["a", "bb", "c"]), &STYLE, 4);
        assert_eq!(out, "a \\\n  bb \\\n  c\n");
    }

    #[test]
    fn to_netlist_writes_the_file() {
        let directory = tempfile::tempdir().expect("temporary directory should be created");
        let output = CompileOutput::Valid(inverter());
        let path = directory.path().join("nested/inv.spice");
        output
            .to_netlist(&path, NetlistFormat::Spice, &NetlistOptions::default())
            .expect("the netlist should be written");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), spice(&inverter()));
        let leftovers = std::fs::read_dir(path.parent().unwrap())
            .unwrap()
            .filter(|entry| entry.as_ref().unwrap().path() != path)
            .count();
        assert_eq!(leftovers, 0, "no temporary file should remain");
    }

    /// Simulates the inverter with stand-in transistor models, when ngspice
    /// is installed.
    #[test]
    fn ngspice_simulates_the_inverter() {
        if std::process::Command::new("ngspice")
            .arg("--version")
            .output()
            .is_err()
        {
            eprintln!("skipping: ngspice is not on PATH");
            return;
        }
        let directory = tempfile::tempdir().expect("temporary directory should be created");
        let netlist = directory.path().join("inv.spice");
        CompileOutput::Valid(inverter())
            .to_netlist(
                &netlist,
                NetlistFormat::Spice,
                &NetlistOptions { width: 20 },
            )
            .unwrap();
        let testbench = directory.path().join("tb.spice");
        std::fs::write(
            &testbench,
            format!(
                "* inverter smoke test
.include {}
.subckt sky130_fd_pr__nfet_01v8 d g s b l=1 w=1 nf=1
M0 d g s b nmos_stub l={{l*1e-6}} w={{w*nf*1e-6}}
.ends
.subckt sky130_fd_pr__pfet_01v8 d g s b l=1 w=1 nf=1
M0 d g s b pmos_stub l={{l*1e-6}} w={{w*nf*1e-6}}
.ends
.model nmos_stub nmos level=1
.model pmos_stub pmos level=1
Xdut in out vdd 0 inv
Vdd vdd 0 1.8
Vin in 0 0
.control
op
print v(out)
alter Vin dc=1.8
op
print v(out)
.endc
.end
",
                netlist.display()
            ),
        )
        .unwrap();
        let run = std::process::Command::new("ngspice")
            .arg("-b")
            .arg(&testbench)
            .current_dir(directory.path())
            .output()
            .expect("ngspice should run");
        let stdout = String::from_utf8_lossy(&run.stdout);
        assert!(run.status.success(), "{stdout}");
        let outputs = stdout
            .lines()
            .filter_map(|line| line.trim().strip_prefix("v(out) = "))
            .map(|value| value.trim().parse::<f64>().expect("a voltage"))
            .collect::<Vec<_>>();
        let [high, low] = outputs.as_slice() else {
            panic!("expected two operating points: {stdout}");
        };
        assert!((high - 1.8).abs() < 1e-3, "{stdout}");
        assert!(low.abs() < 1e-3, "{stdout}");
    }
}
