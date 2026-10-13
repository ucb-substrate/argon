//! The symbol view's drawing: the cell's own symbol at its origin, every pin
//! with its port's name, and the ports that have no pin.

use std::collections::HashMap;

use argonc::ast::Span;
use argonc::compile::{CellId, CompiledData, Placement, SymbolItemKind, generated_box};

use super::symbols::{Bounds, PlacedSymbol, place_symbol};

/// Items and pins drawn in place of the source's, by the span of their
/// call: a gesture in progress, or one whose edit is pending.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SymbolOverrides {
    pub items: HashMap<Span, SymbolItemKind>,
    pub pins: HashMap<Span, (i64, i64)>,
}

impl SymbolOverrides {
    pub fn is_empty(&self) -> bool {
        self.items.is_empty() && self.pins.is_empty()
    }
}

/// Everything the symbol view draws for one cell.
#[derive(Debug, Clone, Default)]
pub struct SymbolScene {
    pub placed: PlacedSymbol,
    /// Each shape or text, in item order.
    pub items: Vec<SymbolSceneItem>,
    /// The pin of each port that has one.
    pub pins: Vec<SymbolPin>,
    /// The ports without a pin, by name.
    pub missing: Vec<String>,
    /// The ports without a pin, by position among the ports.
    pub missing_ports: Vec<usize>,
    /// Whether the symbol is the generated box rather than drawn in source.
    pub generated: bool,
    /// The site of each item, in item order; `None` in a generated symbol.
    pub item_spans: Vec<Option<Span>>,
    pub bounds: Option<Bounds>,
}

/// A shape or text of the symbol, with the call that drew it.
#[derive(Debug, Clone)]
pub struct SymbolSceneItem {
    pub kind: SymbolItemKind,
    pub span: Option<Span>,
    /// The call in the cell body a function drew it under.
    pub origin: Option<Span>,
    pub editable: bool,
}

#[derive(Debug, Clone)]
pub struct SymbolPin {
    pub port: usize,
    pub name: String,
    pub position: (i64, i64),
    pub labeled: bool,
    pub editable: bool,
    pub span: Option<Span>,
    pub origin: Option<Span>,
}

/// Builds the symbol scene of `cell`, showing `inst_name` for its
/// instance-name items.
pub fn build_symbol_scene(data: &CompiledData, cell: CellId, inst_name: &str) -> SymbolScene {
    build_symbol_scene_with(data, cell, inst_name, &SymbolOverrides::default())
}

/// Builds the symbol scene of `cell` with `overrides` in place of the
/// source's items and pins.
pub fn build_symbol_scene_with(
    data: &CompiledData,
    cell: CellId,
    inst_name: &str,
    overrides: &SymbolOverrides,
) -> SymbolScene {
    let Some(compiled) = data.cells.get(&cell) else {
        return SymbolScene::default();
    };
    let schematic = &compiled.schematic;
    let port_names = (0..schematic.ports.len())
        .map(|port| schematic.port_name(port).to_owned())
        .collect::<Vec<_>>();
    let (mut symbol, generated) = match &compiled.symbol {
        Some(symbol) => (symbol.clone(), false),
        None => (generated_box(schematic, &compiled.name), true),
    };
    for item in &mut symbol.items {
        if let Some(site) = &item.site
            && let Some(kind) = overrides.items.get(&site.span)
        {
            item.kind = kind.clone();
        }
    }
    for pin in symbol.pins.iter_mut().flatten() {
        if let Some(site) = &pin.site
            && let Some((x, y)) = overrides.pins.get(&site.span)
        {
            pin.x = *x;
            pin.y = *y;
        }
    }
    let placed = place_symbol(&symbol, &Placement::default(), inst_name, &port_names);
    let mut pins = Vec::new();
    let mut missing = Vec::new();
    let mut missing_ports = Vec::new();
    for (port, pin) in symbol.pins.iter().enumerate() {
        match pin {
            Some(pin) => pins.push(SymbolPin {
                port,
                name: port_names[port].clone(),
                position: (pin.x, pin.y),
                labeled: pin.label.is_some(),
                editable: pin.site.as_ref().is_some_and(|site| site.editable),
                span: pin.site.as_ref().map(|site| site.span.clone()),
                origin: pin.site.as_ref().and_then(|site| site.origin.clone()),
            }),
            None => {
                missing.push(port_names[port].clone());
                missing_ports.push(port);
            }
        }
    }
    let bounds = placed.bounds();
    SymbolScene {
        placed,
        items: symbol
            .items
            .iter()
            .map(|item| SymbolSceneItem {
                kind: item.kind.clone(),
                span: item.site.as_ref().map(|site| site.span.clone()),
                origin: item.site.as_ref().and_then(|site| site.origin.clone()),
                editable: item.site.as_ref().is_some_and(|site| site.editable),
            })
            .collect(),
        pins,
        missing,
        missing_ports,
        generated,
        item_spans: symbol
            .items
            .iter()
            .map(|item| item.site.as_ref().map(|site| site.span.clone()))
            .collect(),
        bounds,
    }
}
