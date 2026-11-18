//! Rough wiring parasitics of a cell's nets, from their shapes and the
//! technology file's capacitance coefficients.
//!
//! Each net's capacitance is its area and perimeter to the substrate, less
//! the area lower conductors shield, plus its overlap with other nets' shapes
//! on the layers the technology pairs, less the area a layer between them
//! shields, plus its coupling to other nets' facing edges on one layer within
//! [`SIDE_HALO`], less the length a shape between them shields.

use std::collections::HashMap;

use crate::connectivity::{Bounds, Check, Index, overlaps};
use crate::tech::Technology;

/// Coordinates closer than this are equal.
const EPS: f64 = 1e-6;

/// Facing edges farther apart than this, in µm, don't couple.
pub const SIDE_HALO: f64 = 2.;

/// A shape is long when it is at least this fraction of the cell's longer side…
pub const LONG_FRACTION: f64 = 0.5;
/// …and at least this many times as long as it is wide.
pub const LONG_ASPECT: f64 = 20.;

/// One net's wiring on one layer, in µm and µm².
#[derive(Clone, Debug, Default)]
pub struct LayerUse {
    pub layer: String,
    /// The sum of each shape's longer side.
    pub length: f64,
    /// The area its shapes cover together.
    pub area: f64,
    /// The perimeter of that area.
    pub perimeter: f64,
}

/// Where one net's shapes on `top` overlap another's on `bot`, or, when
/// `side`, face another's across a gap on the layer `top` and `bot` both name.
#[derive(Clone, Debug)]
pub struct Coupling {
    pub other: usize,
    pub top: String,
    pub bot: String,
    /// Overlap not shielded by a layer between, in µm²; zero when `side`.
    pub area: f64,
    /// Whether this is coupling between facing edges on one layer.
    pub side: bool,
    /// The facing length not shielded by a shape between, in µm; zero unless
    /// `side`.
    pub length: f64,
    /// In aF; zero when the technology gives the pair no coefficient.
    pub cap: f64,
}

/// A shape long for its cell, by its index in the connectivity's shapes.
#[derive(Clone, Debug)]
pub struct LongWire {
    pub shape: usize,
    pub length: f64,
    pub width: f64,
}

/// One net's wiring.
#[derive(Clone, Debug)]
pub struct NetReport {
    pub net: usize,
    pub name: Option<String>,
    pub layers: Vec<LayerUse>,
    /// Cuts by cut layer.
    pub cuts: Vec<(String, usize)>,
    /// Capacitance to the substrate, in aF.
    pub substrate: f64,
    pub couplings: Vec<Coupling>,
    pub long: Vec<LongWire>,
}

impl NetReport {
    /// Total capacitance in aF.
    pub fn total(&self) -> f64 {
        self.substrate + self.couplings.iter().map(|c| c.cap).sum::<f64>()
    }
}

/// The wiring of every net of `check`, in net order.
pub fn report(check: &Check, tech: &Technology) -> Vec<NetReport> {
    let c = &check.conductors;
    let um = tech.microns_per_unit();
    let um2 = um * um;
    let layer_of = |name: &str| c.layers.iter().position(|l| l == name);
    let mut reports = (0..c.nets)
        .map(|net| NetReport {
            net,
            name: check.net_name(net),
            layers: Vec::new(),
            cuts: Vec::new(),
            substrate: 0.,
            couplings: Vec::new(),
            long: Vec::new(),
        })
        .collect::<Vec<_>>();

    // Each net's shapes by layer, and each layer's shapes of every net.
    let mut by_net: HashMap<(usize, usize), Vec<Bounds>> = HashMap::new();
    let mut by_layer: HashMap<usize, Vec<Bounds>> = HashMap::new();
    let mut extent = [f64::MAX, f64::MAX, f64::MIN, f64::MIN];
    for (i, shape) in c.shapes.iter().enumerate() {
        let b = shape.bounds;
        extent = [
            extent[0].min(b[0]),
            extent[1].min(b[1]),
            extent[2].max(b[2]),
            extent[3].max(b[3]),
        ];
        let net = c.net[i];
        if c.cut[shape.layer] {
            let name = &c.layers[shape.layer];
            let cuts = &mut reports[net].cuts;
            match cuts.iter_mut().find(|(l, _)| l == name) {
                Some((_, n)) => *n += 1,
                None => cuts.push((name.clone(), 1)),
            }
            continue;
        }
        by_net.entry((net, shape.layer)).or_default().push(b);
        by_layer.entry(shape.layer).or_default().push(b);
    }
    let longest_side = (extent[2] - extent[0]).max(extent[3] - extent[1]).max(0.);

    // Long shapes.
    for (i, shape) in c.shapes.iter().enumerate() {
        if c.cut[shape.layer] {
            continue;
        }
        let b = shape.bounds;
        let (w, h) = (b[2] - b[0], b[3] - b[1]);
        let (length, width) = (w.max(h), w.min(h));
        if length >= LONG_FRACTION * longest_side && length >= LONG_ASPECT * width && length > EPS {
            reports[c.net[i]].long.push(LongWire {
                shape: i,
                length: length * um,
                width: width * um,
            });
        }
    }

    // Each net's layers, unioned.
    let mut unions: HashMap<(usize, usize), Union> = HashMap::new();
    let mut keys = by_net.keys().copied().collect::<Vec<_>>();
    keys.sort_unstable();
    for (net, layer) in keys {
        let rects = &by_net[&(net, layer)];
        let u = union(rects);
        reports[net].layers.push(LayerUse {
            layer: c.layers[layer].clone(),
            length: rects
                .iter()
                .map(|r| (r[2] - r[0]).max(r[3] - r[1]))
                .sum::<f64>()
                * um,
            area: u.area * um2,
            perimeter: u.perimeter * um,
        });
        unions.insert((net, layer), u);
    }

    // Each layer's shapes of all nets, and what lies below each, in the
    // capacitance table's bottom-up order.
    let order = tech
        .capacitance
        .iter()
        .filter_map(|cap| layer_of(&cap.layer).map(|l| (l, cap)))
        .collect::<Vec<_>>();
    let all: HashMap<usize, Union> = by_layer
        .iter()
        .map(|(l, rects)| (*l, union(rects)))
        .collect();
    let empty = Union::default();
    let mut below_rects: Vec<Bounds> = Vec::new();
    let mut below: HashMap<usize, Union> = HashMap::new();
    for (layer, _) in &order {
        below.insert(*layer, union(&below_rects));
        if let Some(u) = all.get(layer) {
            below_rects.extend_from_slice(&u.pieces);
        }
    }
    for (layer, cap) in &order {
        let shield = below.get(layer).unwrap_or(&empty);
        let shield_index = Index::new(&shield.pieces);
        for report in reports.iter_mut() {
            if let Some(u) = unions.get(&(report.net, *layer)) {
                let exposed = u.area - intersection_area(&u.pieces, &shield.pieces, &shield_index);
                report.substrate += exposed * um2 * cap.area + u.perimeter * um * cap.perimeter;
            }
        }
    }

    // Overlaps between nets on the layer pairs the technology names, less
    // what a layer between them shields.
    let position = |l: usize| order.iter().position(|(o, _)| *o == l);
    for pair in &tech.overlap_capacitance {
        let (Some(top), Some(bot)) = (layer_of(&pair.top), layer_of(&pair.bot)) else {
            continue;
        };
        let between_rects = match (position(top), position(bot)) {
            (Some(t), Some(b)) if b + 1 < t => order[b + 1..t]
                .iter()
                .filter_map(|(l, _)| all.get(l))
                .flat_map(|u| u.pieces.iter().copied())
                .collect::<Vec<_>>(),
            _ => Vec::new(),
        };
        let between = union(&between_rects);
        let between_index = Index::new(&between.pieces);
        // Every net's pieces on `bot`, indexed together.
        let mut bot_pieces: Vec<(Bounds, usize)> = Vec::new();
        for ((net, layer), u) in &unions {
            if *layer == bot {
                bot_pieces.extend(u.pieces.iter().map(|p| (*p, *net)));
            }
        }
        let bot_bounds = bot_pieces.iter().map(|(b, _)| *b).collect::<Vec<_>>();
        let bot_index = Index::new(&bot_bounds);
        let mut areas: HashMap<(usize, usize), f64> = HashMap::new();
        for ((net, layer), u) in &unions {
            if *layer != top {
                continue;
            }
            for piece in &u.pieces {
                for i in bot_index.query(*piece) {
                    let (other_piece, other) = bot_pieces[i];
                    if other == *net || !overlaps(*piece, other_piece) {
                        continue;
                    }
                    let r = intersect(*piece, other_piece);
                    let shielded = intersection_area(&[r], &between.pieces, &between_index);
                    *areas.entry((*net, other)).or_default() += area(r) - shielded;
                }
            }
        }
        for ((net, other), a) in areas {
            if a <= EPS {
                continue;
            }
            let coupling = |other| Coupling {
                other,
                top: pair.top.clone(),
                bot: pair.bot.clone(),
                area: a * um2,
                side: false,
                length: 0.,
                cap: a * um2 * pair.area,
            };
            reports[net].couplings.push(coupling(other));
            reports[other].couplings.push(coupling(net));
        }
    }
    // Facing edges of different nets on one layer, less what a shape between
    // them shields, each pair once.
    let halo = SIDE_HALO / um;
    for (layer, cap) in &order {
        if cap.sidewall <= 0. {
            continue;
        }
        let mut pieces = unions
            .iter()
            .filter(|((_, l), _)| l == layer)
            .flat_map(|((net, _), u)| u.pieces.iter().map(move |p| (*p, *net)))
            .collect::<Vec<_>>();
        pieces.sort_by(|a, b| a.1.cmp(&b.1).then(a.0.partial_cmp(&b.0).unwrap()));
        let bounds = pieces.iter().map(|(b, _)| *b).collect::<Vec<_>>();
        let index = Index::new(&bounds);
        let mut sides: HashMap<(usize, usize), (f64, f64)> = HashMap::new();
        for (i, (p, net)) in pieces.iter().enumerate() {
            let reach = [p[0] - halo, p[1] - halo, p[2] + halo, p[3] + halo];
            for j in index.query(reach) {
                let (q, other) = pieces[j];
                if j <= i || other == *net {
                    continue;
                }
                let Some(face) = facing(*p, q) else {
                    continue;
                };
                if face.gap <= EPS || face.gap > halo {
                    continue;
                }
                let shadows = index
                    .query(face.between)
                    .into_iter()
                    .filter(|&k| k != i && k != j && overlaps(bounds[k], face.between))
                    .map(|k| {
                        let b = bounds[k];
                        let (lo, hi) = if face.vertical {
                            (b[1], b[3])
                        } else {
                            (b[0], b[2])
                        };
                        (lo.max(face.lo), hi.min(face.hi))
                    })
                    .collect::<Vec<_>>();
                let length = face.hi - face.lo - covered(shadows);
                if length <= EPS {
                    continue;
                }
                let entry = sides
                    .entry(((*net).min(other), (*net).max(other)))
                    .or_default();
                entry.0 += cap.sidewall * length / face.gap;
                entry.1 += length * um;
            }
        }
        let mut sides = sides.into_iter().collect::<Vec<_>>();
        sides.sort_by_key(|(pair, _)| *pair);
        for ((a, b), (aff, length)) in sides {
            let coupling = |other| Coupling {
                other,
                top: c.layers[*layer].clone(),
                bot: c.layers[*layer].clone(),
                area: 0.,
                side: true,
                length,
                cap: aff,
            };
            reports[a].couplings.push(coupling(b));
            reports[b].couplings.push(coupling(a));
        }
    }

    for report in &mut reports {
        report.couplings.sort_by(|a, b| {
            b.cap
                .total_cmp(&a.cap)
                .then(b.area.total_cmp(&a.area))
                .then(a.other.cmp(&b.other))
                .then(a.top.cmp(&b.top))
        });
    }
    reports
}

/// How two shapes face each other across a gap: its width, the span of
/// their facing edges from `lo` to `hi`, the region between them, and whether
/// the edges are vertical.
struct Facing {
    gap: f64,
    lo: f64,
    hi: f64,
    between: Bounds,
    vertical: bool,
}

/// How `a` and `b` face each other, if their edges share a span.
fn facing(a: Bounds, b: Bounds) -> Option<Facing> {
    let (lo, hi) = (a[1].max(b[1]), a[3].min(b[3]));
    if hi - lo > EPS {
        let (left, right) = if a[2] <= b[0] + EPS {
            (a, b)
        } else if b[2] <= a[0] + EPS {
            (b, a)
        } else {
            return None;
        };
        return Some(Facing {
            gap: right[0] - left[2],
            lo,
            hi,
            between: [left[2], lo, right[0], hi],
            vertical: true,
        });
    }
    let (lo, hi) = (a[0].max(b[0]), a[2].min(b[2]));
    if hi - lo > EPS {
        let (low, high) = if a[3] <= b[1] + EPS {
            (a, b)
        } else if b[3] <= a[1] + EPS {
            (b, a)
        } else {
            return None;
        };
        return Some(Facing {
            gap: high[1] - low[3],
            lo,
            hi,
            between: [lo, low[3], hi, high[1]],
            vertical: false,
        });
    }
    None
}

/// The length `intervals` cover together.
fn covered(mut intervals: Vec<(f64, f64)>) -> f64 {
    intervals.retain(|(lo, hi)| hi > lo);
    intervals.sort_by(|a, b| a.0.total_cmp(&b.0));
    let mut total = 0.;
    let mut reach = f64::MIN;
    for (lo, hi) in intervals {
        let start = lo.max(reach);
        if hi > start {
            total += hi - start;
        }
        reach = reach.max(hi);
    }
    total
}

fn area(r: Bounds) -> f64 {
    (r[2] - r[0]).max(0.) * (r[3] - r[1]).max(0.)
}

fn intersect(a: Bounds, b: Bounds) -> Bounds {
    [
        a[0].max(b[0]),
        a[1].max(b[1]),
        a[2].min(b[2]),
        a[3].min(b[3]),
    ]
}

/// The area `rects`, which don't overlap each other, share with `pieces`,
/// which don't either.
fn intersection_area(rects: &[Bounds], pieces: &[Bounds], index: &Index) -> f64 {
    if pieces.is_empty() {
        return 0.;
    }
    let mut total = 0.;
    for r in rects {
        for i in index.query(*r) {
            if overlaps(*r, pieces[i]) {
                total += area(intersect(*r, pieces[i]));
            }
        }
    }
    total
}

/// The area rectangles cover together, as rectangles that don't overlap.
#[derive(Clone, Debug, Default)]
pub struct Union {
    pub pieces: Vec<Bounds>,
    pub area: f64,
    pub perimeter: f64,
}

/// Unions `rects` by vertical slabs between their distinct x coordinates,
/// merging a slab's piece into the one before when they span the same y.
pub fn union(rects: &[Bounds]) -> Union {
    let rects = rects
        .iter()
        .copied()
        .filter(|r| r[2] - r[0] > EPS && r[3] - r[1] > EPS)
        .collect::<Vec<_>>();
    let mut out = Union::default();
    if rects.is_empty() {
        return out;
    }
    let mut xs = rects.iter().flat_map(|r| [r[0], r[2]]).collect::<Vec<_>>();
    xs.sort_by(f64::total_cmp);
    xs.dedup_by(|a, b| (*a - *b).abs() < EPS);
    let mut order = (0..rects.len()).collect::<Vec<_>>();
    order.sort_by(|a, b| rects[*a][0].total_cmp(&rects[*b][0]));
    let mut next = 0;
    let mut active: Vec<usize> = Vec::new();
    let mut previous: Vec<(f64, f64)> = Vec::new();
    // Pieces of the previous slab, by their y interval.
    let mut open: Vec<((f64, f64), usize)> = Vec::new();
    for slab in xs.windows(2) {
        let (xa, xb) = (slab[0], slab[1]);
        while next < order.len() && rects[order[next]][0] <= xa + EPS {
            active.push(order[next]);
            next += 1;
        }
        active.retain(|&i| rects[i][2] > xa + EPS);
        let mut spans = active
            .iter()
            .map(|&i| (rects[i][1], rects[i][3]))
            .collect::<Vec<_>>();
        spans.sort_by(|a, b| a.0.total_cmp(&b.0));
        let mut merged: Vec<(f64, f64)> = Vec::new();
        for (y0, y1) in spans {
            match merged.last_mut() {
                Some(last) if y0 <= last.1 + EPS => last.1 = last.1.max(y1),
                _ => merged.push((y0, y1)),
            }
        }
        out.perimeter += 2. * (xb - xa) * merged.len() as f64;
        out.perimeter += symmetric_difference(&previous, &merged);
        let mut now = Vec::with_capacity(merged.len());
        // Both lists are sorted by their lower ends, so one pass matches them.
        let mut j = 0;
        for span in &merged {
            out.area += (xb - xa) * (span.1 - span.0);
            while j < open.len() && open[j].0.0 < span.0 - EPS {
                j += 1;
            }
            let same = open
                .get(j)
                .filter(|(s, _)| (s.0 - span.0).abs() < EPS && (s.1 - span.1).abs() < EPS);
            let index = match same {
                Some((_, k)) => {
                    out.pieces[*k][2] = xb;
                    *k
                }
                None => {
                    out.pieces.push([xa, span.0, xb, span.1]);
                    out.pieces.len() - 1
                }
            };
            now.push((*span, index));
        }
        open = now;
        previous = merged;
    }
    out.perimeter += previous.iter().map(|s| s.1 - s.0).sum::<f64>();
    out
}

/// The length covered by exactly one of two sorted lists of disjoint intervals.
fn symmetric_difference(a: &[(f64, f64)], b: &[(f64, f64)]) -> f64 {
    let length = |v: &[(f64, f64)]| v.iter().map(|s| s.1 - s.0).sum::<f64>();
    let (mut i, mut j, mut common) = (0, 0, 0.);
    while i < a.len() && j < b.len() {
        let lo = a[i].0.max(b[j].0);
        let hi = a[i].1.min(b[j].1);
        if hi > lo {
            common += hi - lo;
        }
        if a[i].1 < b[j].1 {
            i += 1;
        } else {
            j += 1;
        }
    }
    length(a) + length(b) - 2. * common
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_facing_edges_and_the_length_shapes_between_cover() {
        // Side by side, 1 apart, facing from y = 1 to 2.
        let f = facing([0., 0., 1., 2.], [2., 1., 3., 4.]).unwrap();
        assert_eq!((f.gap, f.lo, f.hi, f.vertical), (1., 1., 2., true));
        assert_eq!(f.between, [1., 1., 2., 2.]);
        // One above the other, in either order.
        let f = facing([0., 3., 2., 4.], [1., 0., 5., 1.]).unwrap();
        assert_eq!((f.gap, f.lo, f.hi, f.vertical), (2., 1., 2., false));
        // Only corners face: no span in common.
        assert!(facing([0., 0., 1., 1.], [2., 2., 3., 3.]).is_none());
        // Overlapping intervals count once.
        assert_eq!(covered(vec![(0., 2.), (1., 3.), (5., 6.), (4., 4.)]), 4.);
    }

    #[test]
    fn unions_overlapping_rectangles_with_their_outline() {
        // An L of two overlapping rectangles: area 3, perimeter 8.
        let u = union(&[[0., 0., 2., 1.], [0., 0., 1., 2.]]);
        assert!((u.area - 3.).abs() < 1e-9);
        assert!((u.perimeter - 8.).abs() < 1e-9);
        // Two touching squares merge into one piece of perimeter 6.
        let u = union(&[[0., 0., 1., 1.], [1., 0., 2., 1.]]);
        assert_eq!(u.pieces, vec![[0., 0., 2., 1.]]);
        assert!((u.perimeter - 6.).abs() < 1e-9);
        // A ring's hole counts toward the perimeter.
        let ring = [
            [0., 0., 3., 1.],
            [0., 2., 3., 3.],
            [0., 0., 1., 3.],
            [2., 0., 3., 3.],
        ];
        let u = union(&ring);
        assert!((u.area - 8.).abs() < 1e-9);
        assert!((u.perimeter - 16.).abs() < 1e-9);
    }
}
