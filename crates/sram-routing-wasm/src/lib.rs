//! The greedy router as a WebAssembly module, which `examples/sram` calls as
//! `routing::route`, `routing::expand_to_grid`, `routing::straps`, and
//! `routing::merge_ports`. Build it with
//! `cargo build --release -p argon-sram-routing-wasm --target wasm32-unknown-unknown`.

use argon_greedy_router::geom::{Corner, Dir, Point, Rect, Side};
use argon_greedy_router::straps::{self, Supply, Target};
use argon_greedy_router::tracks::TrackLocator;
use argon_greedy_router::{
    ExpandToGridStrategy, GreedyRouter, IndexOwned, Layer, LayerConfig, RouterConfig, ViaRequest,
};
use serde::{Deserialize, Serialize};

/// A rectangle in nanometers.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
struct Box {
    x0: f64,
    y0: f64,
    x1: f64,
    y1: f64,
}

/// One routing layer: its wire width, spacing, and direction.
#[derive(Debug, Clone, Deserialize)]
struct GridLayer {
    line: i64,
    space: i64,
    horizontal: bool,
}

/// The routing area and its layers, lowest first.
#[derive(Debug, Clone, Deserialize)]
struct Grid {
    area: Box,
    layers: Vec<GridLayer>,
}

/// Blocks a rectangle's grid points for every net.
const BLOCK: i64 = 0;
/// Gives a rectangle's grid points to a net.
const OCCUPY: i64 = 1;
/// Blocks the tracks within one space of a rectangle that it does not cover.
const BLOCK_UNCLEAN: i64 = 2;
/// Routes a rectangle to `dst` on the same net.
const ROUTE: i64 = 3;

/// One router operation. `net`, `dst_layer`, and `dst` are used only by the
/// kinds that need them.
#[derive(Debug, Clone, Deserialize)]
struct Op {
    kind: i64,
    layer: i64,
    rect: Box,
    net: String,
    dst_layer: i64,
    dst: Box,
}

/// Operations to run in order. Routes are retried in the orders of
/// [`ORDERS`] until all of them succeed.
#[derive(Debug, Clone, Deserialize)]
struct Job {
    grid: Grid,
    ops: Vec<Op>,
    /// The layer whose pins are fixed on the block edge.
    pin_layer: i64,
    /// Nets that end on a fixed pin of another layer on the block edge.
    edge_nets: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
struct Wire {
    layer: i64,
    rect: Box,
}

#[derive(Debug, Clone, Serialize)]
struct Via {
    bot_layer: i64,
    bot: Box,
    top_layer: i64,
    top: Box,
}

#[derive(Debug, Clone, Serialize)]
struct Routed {
    wires: Vec<Wire>,
    vias: Vec<Via>,
    /// The index in [`ORDERS`] of the route order that succeeded.
    order: i64,
}

/// Supply geometry for straps to land on. `vdd` selects the supply.
#[derive(Debug, Clone, Deserialize)]
struct StrapTarget {
    layer: i64,
    vdd: bool,
    rect: Box,
}

/// Blocking operations, then straps on `strap_layers` (lowest first) that
/// connect to `targets`. Via centers snap to `grid`. `blocks[l]` are blocked
/// on layer `l` before `ops` run.
#[derive(Debug, Clone, Deserialize)]
struct StrapJob {
    grid: Grid,
    blocks: Vec<Vec<Box>>,
    ops: Vec<Op>,
    targets: Vec<StrapTarget>,
    strap_layers: Vec<i64>,
    manufacturing_grid: i64,
}

#[derive(Debug, Clone, Serialize)]
struct PlacedStrap {
    layer: i64,
    rect: Box,
    vdd: bool,
    lower_boundary: bool,
    upper_boundary: bool,
}

/// Vias that are translated copies of `via`, one per offset (the first is
/// `(0, 0)`).
#[derive(Debug, Clone, Serialize)]
struct Crossing {
    via: Via,
    offsets: Vec<(f64, f64)>,
}

/// Straps, the vias to their targets, and the vias where straps cross.
#[derive(Debug, Clone, Serialize)]
struct Straps {
    straps: Vec<PlacedStrap>,
    vias: Vec<Via>,
    crossings: Vec<Crossing>,
}

/// Route orders, in the sequence they are tried.
#[derive(Debug, Clone, Copy)]
enum Order {
    AsWritten,
    PinLayerFirst,
    EdgePinsFirst,
    Reversed,
}

const ORDERS: [Order; 4] = [
    Order::AsWritten,
    Order::PinLayerFirst,
    Order::EdgePinsFirst,
    Order::Reversed,
];

fn coord(v: f64) -> Result<i64, String> {
    let rounded = v.round();
    if (v - rounded).abs() > 1e-6 {
        return Err(format!(
            "coordinate {v} is not a whole number of nanometers"
        ));
    }
    Ok(rounded as i64)
}

fn rect(b: Box) -> Result<Rect, String> {
    Ok(Rect::new(
        Point::new(coord(b.x0)?, coord(b.y0)?),
        Point::new(coord(b.x1)?, coord(b.y1)?),
    ))
}

fn strap_via(via: &ViaRequest) -> Via {
    Via {
        bot_layer: via.bot as i64,
        bot: to_box(via.bot_rect),
        top_layer: via.top as i64,
        top: to_box(via.top_rect),
    }
}

fn to_box(r: Rect) -> Box {
    Box {
        x0: r.left() as f64,
        y0: r.bottom() as f64,
        x1: r.right() as f64,
        y1: r.top() as f64,
    }
}

fn layer(index: i64, grid: &Grid) -> Result<Layer, String> {
    usize::try_from(index)
        .ok()
        .filter(|&i| i < grid.layers.len())
        .ok_or_else(|| format!("layer {index} is not a routing layer"))
}

fn router(grid: &Grid) -> Result<GreedyRouter, String> {
    Ok(GreedyRouter::with_config(RouterConfig {
        area: rect(grid.area)?,
        layers: grid
            .layers
            .iter()
            .map(|l| LayerConfig {
                line: l.line,
                space: l.space,
                dir: if l.horizontal { Dir::Horiz } else { Dir::Vert },
            })
            .collect(),
    }))
}

/// Blocks the tracks within one space of `rect` that it does not fully
/// cover, so no net's wire can sit closer to it than the spacing rule allows.
fn block_unclean_tracks(router: &mut GreedyRouter, layer: Layer, rect: Rect) {
    let info = router.track_info(layer);
    let dir = info.dir();
    let tracks = info.tracks().clone();
    let space = tracks.space;
    let perp = rect.span(!dir);
    let along = rect.span(dir);
    let lo = tracks.track_with_loc(TrackLocator::StartsBefore, perp.start() - space);
    let hi = tracks.track_with_loc(TrackLocator::EndsAfter, perp.stop() + space);
    for i in lo..=hi {
        let t = tracks.index(i);
        if perp.start() <= t.start() && t.stop() <= perp.stop() {
            continue;
        }
        let gap = if t.stop() <= perp.start() {
            perp.start() - t.stop()
        } else if perp.stop() <= t.start() {
            t.start() - perp.stop()
        } else {
            0
        };
        if gap < space {
            router.block(
                layer,
                Rect::span_builder().with(dir, along).with(!dir, t).build(),
            );
        }
    }
}

/// Expands `rect` onto the grid's tracks. `strategy` is 0 for the smallest
/// expansion, 1 to cover every track it touches, 2 through 5 to keep the
/// top, right, bottom, or left side's track, and 6 through 9 to keep the
/// lower-left, lower-right, upper-left, or upper-right corner's tracks.
fn expand_to_grid(grid: Grid, rect_in: Box, strategy: i64) -> Result<Box, String> {
    let strategy = match strategy {
        0 => ExpandToGridStrategy::Minimum,
        1 => ExpandToGridStrategy::All,
        2 => ExpandToGridStrategy::Side(Side::Top),
        3 => ExpandToGridStrategy::Side(Side::Right),
        4 => ExpandToGridStrategy::Side(Side::Bot),
        5 => ExpandToGridStrategy::Side(Side::Left),
        6 => ExpandToGridStrategy::Corner(Corner::LowerLeft),
        7 => ExpandToGridStrategy::Corner(Corner::LowerRight),
        8 => ExpandToGridStrategy::Corner(Corner::UpperLeft),
        9 => ExpandToGridStrategy::Corner(Corner::UpperRight),
        _ => return Err(format!("unknown grid expansion strategy {strategy}")),
    };
    let router = router(&grid)?;
    Ok(to_box(router.expand_to_grid(rect(rect_in)?, strategy)))
}

/// Runs `job`'s operations, retrying its routes in each order until all succeed.
fn route(job: Job) -> Result<Routed, String> {
    let (routes, setup): (Vec<&Op>, Vec<&Op>) = job.ops.iter().partition(|op| op.kind == ROUTE);
    let mut last_error = String::from("there are no route orders");
    for (index, order) in ORDERS.into_iter().enumerate() {
        let mut router = router(&job.grid)?;
        for op in &setup {
            let l = layer(op.layer, &job.grid)?;
            let r = rect(op.rect)?;
            match op.kind {
                BLOCK => router.block(l, r),
                OCCUPY => router.occupy(l, r, &op.net).map_err(|e| e.to_string())?,
                BLOCK_UNCLEAN => block_unclean_tracks(&mut router, l, r),
                kind => return Err(format!("unknown router operation {kind}")),
            }
        }
        let mut routes = routes.clone();
        let on_pin_layer = |op: &&Op| op.layer == job.pin_layer || op.dst_layer == job.pin_layer;
        match order {
            Order::AsWritten => {}
            Order::PinLayerFirst => routes.sort_by_key(|op| !on_pin_layer(op)),
            Order::EdgePinsFirst => {
                routes.sort_by_key(|op| !(on_pin_layer(op) || job.edge_nets.contains(&op.net)))
            }
            Order::Reversed => routes.reverse(),
        }
        let routed = routes.iter().try_for_each(|op| {
            router
                .route_with_net(
                    layer(op.layer, &job.grid)?,
                    rect(op.rect)?,
                    layer(op.dst_layer, &job.grid)?,
                    rect(op.dst)?,
                    &op.net,
                )
                .map_err(|e| e.to_string())
        });
        match routed {
            Ok(()) => {
                return Ok(Routed {
                    wires: router
                        .wires()
                        .iter()
                        .map(|&(layer, rect)| Wire {
                            layer: layer as i64,
                            rect: to_box(rect),
                        })
                        .collect(),
                    vias: router
                        .vias()
                        .iter()
                        .map(|via| Via {
                            bot_layer: via.bot as i64,
                            bot: to_box(via.bot_rect),
                            top_layer: via.top as i64,
                            top: to_box(via.top_rect),
                        })
                        .collect(),
                    order: index as i64,
                });
            }
            Err(error) => last_error = error,
        }
    }
    Err(format!("no route order routes every net: {last_error}"))
}

/// Applies `job`'s blocking operations, then places power straps on the free
/// track segments that reach a target or cross another strap.
fn straps(job: StrapJob) -> Result<Straps, String> {
    let mut router = router(&job.grid)?;
    for (index, boxes) in job.blocks.iter().enumerate() {
        let l = layer(index as i64, &job.grid)?;
        for b in boxes {
            router.block(l, rect(*b)?);
        }
    }
    for op in &job.ops {
        let l = layer(op.layer, &job.grid)?;
        let r = rect(op.rect)?;
        match op.kind {
            BLOCK => router.block(l, r),
            OCCUPY => router.occupy(l, r, &op.net).map_err(|e| e.to_string())?,
            BLOCK_UNCLEAN => block_unclean_tracks(&mut router, l, r),
            kind => {
                return Err(format!(
                    "router operation {kind} is not allowed before straps"
                ));
            }
        }
    }
    let mut targets = std::collections::HashMap::new();
    for t in &job.targets {
        targets
            .entry(layer(t.layer, &job.grid)?)
            .or_insert_with(Vec::new)
            .push(Target {
                rect: rect(t.rect)?,
                net: if t.vdd { Supply::Vdd } else { Supply::Vss },
            });
    }
    let strap_layers = job
        .strap_layers
        .iter()
        .map(|&l| layer(l, &job.grid))
        .collect::<Result<Vec<_>, _>>()?;
    let placed = straps::fill(&router, &strap_layers, &targets, job.manufacturing_grid);
    Ok(Straps {
        straps: placed
            .straps
            .iter()
            .map(|s| PlacedStrap {
                layer: s.layer as i64,
                rect: to_box(s.rect),
                vdd: s.net == Supply::Vdd,
                lower_boundary: s.lower_boundary,
                upper_boundary: s.upper_boundary,
            })
            .collect(),
        vias: placed
            .vias
            .iter()
            .enumerate()
            .filter(|(i, _)| !placed.crossings.iter().any(|c| c.contains(i)))
            .map(|(_, via)| strap_via(via))
            .collect(),
        crossings: placed
            .crossings
            .iter()
            .map(|range| {
                let first = &placed.vias[range.start];
                Crossing {
                    via: strap_via(first),
                    offsets: placed.vias[range.clone()]
                        .iter()
                        .map(|via| {
                            (
                                (via.bot_rect.left() - first.bot_rect.left()) as f64,
                                (via.bot_rect.bottom() - first.bot_rect.bottom()) as f64,
                            )
                        })
                        .collect(),
                }
            })
            .collect(),
    })
}

/// Joins port shapes the way merging ports does: the first group is taken as
/// is, and each later group is appended and the list coalesced. Rectangle `i`
/// absorbs each later one with the same span in one direction and an
/// intersecting span in the other; after an absorption its scan resumes two
/// past it, so the result depends on the order of the groups.
fn merge_ports(groups: Vec<Vec<Box>>) -> Result<Vec<Box>, String> {
    let mut rects: Vec<Rect> = Vec::new();
    for (k, group) in groups.into_iter().enumerate() {
        for b in group {
            rects.push(rect(b)?);
        }
        if k > 0 {
            coalesce(&mut rects);
        }
    }
    Ok(rects.into_iter().map(to_box).collect())
}

fn coalesce(rects: &mut Vec<Rect>) {
    let mut i = 0;
    while i < rects.len() {
        let mut j = i + 1;
        while j < rects.len() {
            let (a, b) = (rects[i], rects[j]);
            for dir in [Dir::Horiz, Dir::Vert] {
                let (sa, sb) = (a.span(!dir), b.span(!dir));
                if a.span(dir) == b.span(dir) && !(sb.stop() < sa.start() || sa.stop() < sb.start())
                {
                    rects[i] = Rect::new(
                        Point::new(a.left().min(b.left()), a.bottom().min(b.bottom())),
                        Point::new(a.right().max(b.right()), a.top().max(b.top())),
                    );
                    rects.swap_remove(j);
                    j = i + 1;
                    break;
                }
            }
            j += 1;
        }
        i += 1;
    }
}

argon_guest::export!(route, expand_to_grid, straps, merge_ports);
