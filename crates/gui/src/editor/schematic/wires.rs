//! Wires from routes: merged segments, junctions, conflicts between nets, and
//! the flylines that join the islands of each net.

use std::collections::HashMap;

use argonc::compile::{NetIdx, NodeIdx};

use super::scene::{Conflict, Flyline, Junction, NetSegment};
use super::symbols::Pt;

const EPSILON: f64 = 1e-6;

/// Something of a net that can touch something else of the same net.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct NetItem {
    pub net: NetIdx,
    pub kind: NetItemKind,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum NetItemKind {
    /// A terminal pin.
    Pin { at: Pt, node: NodeIdx },
    /// A port pin or a net label, which shows the net's name.
    Named { at: Pt, node: NodeIdx },
    /// Where the flylines of a terminal without a pin end. It touches
    /// nothing.
    Anchor { at: Pt, node: NodeIdx },
    /// A wire segment.
    Segment { a: Pt, b: Pt },
}

impl NetItemKind {
    /// The point of a pin, name, or anchor.
    fn point(&self) -> Option<(Pt, NodeIdx)> {
        match *self {
            NetItemKind::Pin { at, node }
            | NetItemKind::Named { at, node }
            | NetItemKind::Anchor { at, node } => Some((at, node)),
            NetItemKind::Segment { .. } => None,
        }
    }
}

/// A point a flyline can end at, with its node.
type Anchor = (Pt, NodeIdx);

/// What [`analyze_nets`] finds.
#[derive(Debug, Default)]
pub struct NetAnalysis {
    pub segments: Vec<NetSegment>,
    pub junctions: Vec<Junction>,
    pub conflicts: Vec<Conflict>,
    pub flylines: Vec<Flyline>,
}

fn same(a: Pt, b: Pt) -> bool {
    (a.0 - b.0).abs() < EPSILON && (a.1 - b.1).abs() < EPSILON
}

/// Whether `p` lies on the segment from `a` to `b`, ends included.
pub fn on_segment(p: Pt, a: Pt, b: Pt) -> bool {
    let cross = (b.0 - a.0) * (p.1 - a.1) - (b.1 - a.1) * (p.0 - a.0);
    if cross.abs() > EPSILON * (1. + (b.0 - a.0).abs() + (b.1 - a.1).abs()) {
        return false;
    }
    p.0 >= a.0.min(b.0) - EPSILON
        && p.0 <= a.0.max(b.0) + EPSILON
        && p.1 >= a.1.min(b.1) - EPSILON
        && p.1 <= a.1.max(b.1) + EPSILON
}

/// Whether `p` lies strictly inside the segment from `a` to `b`.
fn inside_segment(p: Pt, a: Pt, b: Pt) -> bool {
    on_segment(p, a, b) && !same(p, a) && !same(p, b)
}

/// The distance from `p` to the segment from `a` to `b`.
pub fn segment_distance(p: Pt, a: Pt, b: Pt) -> f64 {
    let (dx, dy) = (b.0 - a.0, b.1 - a.1);
    let length = dx * dx + dy * dy;
    let t = if length < EPSILON {
        0.
    } else {
        (((p.0 - a.0) * dx + (p.1 - a.1) * dy) / length).clamp(0., 1.)
    };
    let (cx, cy) = (a.0 + t * dx, a.1 + t * dy);
    ((p.0 - cx).powi(2) + (p.1 - cy).powi(2)).sqrt()
}

fn distance(a: Pt, b: Pt) -> f64 {
    ((a.0 - b.0).powi(2) + (a.1 - b.1).powi(2)).sqrt()
}

/// Where two segments touch: an end of one on the other, or `None` when they
/// only cross or are apart.
fn segments_touch(a: (Pt, Pt), b: (Pt, Pt)) -> Option<Pt> {
    [a.0, a.1]
        .into_iter()
        .find(|p| on_segment(*p, b.0, b.1))
        .or_else(|| [b.0, b.1].into_iter().find(|p| on_segment(*p, a.0, a.1)))
}

/// A key that groups points closer than the tolerance.
fn key(p: Pt) -> (i64, i64) {
    ((p.0 * 1e4).round() as i64, (p.1 * 1e4).round() as i64)
}

/// Merges overlapping collinear horizontal and vertical segments of one net.
fn merge_segments(net: NetIdx, segments: &[(Pt, Pt)]) -> Vec<NetSegment> {
    let mut merged = Vec::new();
    // (horizontal, fixed coordinate) -> intervals along the other axis.
    let mut runs: HashMap<(bool, i64), Vec<(f64, f64)>> = HashMap::new();
    for (a, b) in segments {
        if (a.1 - b.1).abs() < EPSILON {
            runs.entry((true, key(*a).1))
                .or_default()
                .push((a.0.min(b.0), a.0.max(b.0)));
        } else if (a.0 - b.0).abs() < EPSILON {
            runs.entry((false, key(*a).0))
                .or_default()
                .push((a.1.min(b.1), a.1.max(b.1)));
        } else {
            merged.push(NetSegment { net, a: *a, b: *b });
        }
    }
    let mut keys = runs.keys().copied().collect::<Vec<_>>();
    keys.sort_unstable();
    for run_key in keys {
        let (horizontal, fixed) = run_key;
        let fixed = fixed as f64 / 1e4;
        let mut intervals = runs.remove(&run_key).unwrap_or_default();
        intervals.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let mut current: Option<(f64, f64)> = None;
        let flush = |interval: (f64, f64), merged: &mut Vec<NetSegment>| {
            let (a, b) = if horizontal {
                ((interval.0, fixed), (interval.1, fixed))
            } else {
                ((fixed, interval.0), (fixed, interval.1))
            };
            merged.push(NetSegment { net, a, b });
        };
        for (start, end) in intervals {
            match current {
                Some((s, e)) if start <= e + EPSILON => current = Some((s, e.max(end))),
                Some(previous) => {
                    flush(previous, &mut merged);
                    current = Some((start, end));
                }
                None => current = Some((start, end)),
            }
        }
        if let Some(interval) = current {
            flush(interval, &mut merged);
        }
    }
    merged
}

/// Union-find over item indices.
struct Islands {
    parent: Vec<usize>,
}

impl Islands {
    fn new(count: usize) -> Self {
        Self {
            parent: (0..count).collect(),
        }
    }

    fn find(&mut self, mut index: usize) -> usize {
        while self.parent[index] != index {
            self.parent[index] = self.parent[self.parent[index]];
            index = self.parent[index];
        }
        index
    }

    fn union(&mut self, a: usize, b: usize) {
        let (a, b) = (self.find(a), self.find(b));
        if a != b {
            self.parent[b] = a;
        }
    }
}

/// Whether two items of one net touch.
fn items_touch(a: &NetItemKind, b: &NetItemKind) -> bool {
    use NetItemKind::*;
    match (a, b) {
        (Anchor { .. }, _) | (_, Anchor { .. }) => false,
        (Segment { a: a0, b: a1 }, Segment { a: b0, b: b1 }) => {
            segments_touch((*a0, *a1), (*b0, *b1)).is_some()
        }
        (Segment { a, b }, other) | (other, Segment { a, b }) => {
            other.point().is_some_and(|(p, _)| on_segment(p, *a, *b))
        }
        (x, y) => match (x.point(), y.point()) {
            (Some((p, _)), Some((q, _))) => same(p, q),
            _ => false,
        },
    }
}

/// Where two items of different nets touch, if they do.
fn items_conflict(a: &NetItemKind, b: &NetItemKind) -> Option<Pt> {
    use NetItemKind::*;
    match (a, b) {
        (Anchor { .. }, _) | (_, Anchor { .. }) => None,
        (Segment { a: a0, b: a1 }, Segment { a: b0, b: b1 }) => {
            segments_touch((*a0, *a1), (*b0, *b1))
        }
        (Segment { a, b }, other) | (other, Segment { a, b }) => other
            .point()
            .filter(|(p, _)| on_segment(*p, *a, *b))
            .map(|(p, _)| p),
        (x, y) => match (x.point(), y.point()) {
            (Some((p, _)), Some((q, _))) if same(p, q) => Some(p),
            _ => None,
        },
    }
}

/// Analyzes the items of every net of a schematic with `net_count` nets.
pub fn analyze_nets(items: &[NetItem], net_count: usize) -> NetAnalysis {
    let mut analysis = NetAnalysis::default();

    // Merged segments and junctions, per net.
    let mut per_net: Vec<Vec<(Pt, Pt)>> = vec![Vec::new(); net_count];
    for item in items {
        if let NetItemKind::Segment { a, b } = item.kind {
            per_net[item.net as usize].push((a, b));
        }
    }
    for (net, segments) in per_net.iter().enumerate() {
        if segments.is_empty() {
            continue;
        }
        let merged = merge_segments(net as NetIdx, segments);
        let mut ends: HashMap<(i64, i64), (Pt, usize)> = HashMap::new();
        for segment in &merged {
            for end in [segment.a, segment.b] {
                ends.entry(key(end)).or_insert((end, 0)).1 += 1;
            }
        }
        let mut junctions = Vec::new();
        for (end, count) in ends.values() {
            let inside = merged
                .iter()
                .any(|segment| inside_segment(*end, segment.a, segment.b));
            if *count >= 3 || inside {
                junctions.push(*end);
            }
        }
        junctions.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        analysis
            .junctions
            .extend(junctions.into_iter().map(|at| Junction {
                net: net as NetIdx,
                at,
            }));
        analysis.segments.extend(merged);
    }

    // Conflicts: items of different nets that touch.
    let mut conflicts = Vec::new();
    for (i, a) in items.iter().enumerate() {
        for b in &items[i + 1..] {
            if a.net == b.net {
                continue;
            }
            if let Some(at) = items_conflict(&a.kind, &b.kind) {
                let nets = if a.net < b.net {
                    [a.net, b.net]
                } else {
                    [b.net, a.net]
                };
                conflicts.push(Conflict { nets, at });
            }
        }
    }
    conflicts.sort_by(|a, b| {
        (a.nets, key(a.at))
            .partial_cmp(&(b.nets, key(b.at)))
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    conflicts.dedup_by(|a, b| a.nets == b.nets && key(a.at) == key(b.at));
    analysis.conflicts = conflicts;

    // Flylines: the shortest tree joining the islands of each net.
    for net in 0..net_count {
        let indices = items
            .iter()
            .enumerate()
            .filter(|(_, item)| item.net as usize == net)
            .map(|(index, _)| index)
            .collect::<Vec<_>>();
        if indices.len() < 2 {
            continue;
        }
        let mut islands = Islands::new(indices.len());
        for (i, a) in indices.iter().enumerate() {
            for (j, b) in indices.iter().enumerate().skip(i + 1) {
                if items_touch(&items[*a].kind, &items[*b].kind) {
                    islands.union(i, j);
                }
            }
        }
        // Islands that show the net's name are joined by it.
        let named = indices
            .iter()
            .enumerate()
            .filter(|(_, index)| matches!(items[**index].kind, NetItemKind::Named { .. }))
            .map(|(i, _)| i)
            .collect::<Vec<_>>();
        for pair in named.windows(2) {
            islands.union(pair[0], pair[1]);
        }
        // The points of each island that a flyline may end at.
        let mut points: HashMap<usize, Vec<(Pt, NodeIdx)>> = HashMap::new();
        for (i, index) in indices.iter().enumerate() {
            if let Some(point) = items[*index].kind.point() {
                points.entry(islands.find(i)).or_default().push(point);
            }
        }
        let mut roots = points.keys().copied().collect::<Vec<_>>();
        roots.sort_unstable();
        if roots.len() < 2 {
            continue;
        }
        // Kruskal over island pairs by their nearest points.
        let mut edges = Vec::new();
        for (i, a) in roots.iter().enumerate() {
            for b in &roots[i + 1..] {
                let mut best: Option<(f64, Anchor, Anchor)> = None;
                for p in &points[a] {
                    for q in &points[b] {
                        let d = distance(p.0, q.0);
                        if best.as_ref().is_none_or(|(bd, _, _)| d < *bd) {
                            best = Some((d, *p, *q));
                        }
                    }
                }
                if let Some((d, p, q)) = best {
                    edges.push((d, *a, *b, p, q));
                }
            }
        }
        edges.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
        let mut tree = Islands::new(indices.len());
        for (_, a, b, p, q) in edges {
            if tree.find(a) == tree.find(b) {
                continue;
            }
            tree.union(a, b);
            analysis.flylines.push(Flyline {
                net: net as NetIdx,
                from: p.0,
                to: q.0,
                nodes: [Some(p.1), Some(q.1)],
            });
        }
    }
    analysis
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seg(net: NetIdx, a: Pt, b: Pt) -> NetItem {
        NetItem {
            net,
            kind: NetItemKind::Segment { a, b },
        }
    }

    fn pin(net: NetIdx, at: Pt, node: NodeIdx) -> NetItem {
        NetItem {
            net,
            kind: NetItemKind::Pin { at, node },
        }
    }

    fn named(net: NetIdx, at: Pt, node: NodeIdx) -> NetItem {
        NetItem {
            net,
            kind: NetItemKind::Named { at, node },
        }
    }

    #[test]
    fn collinear_overlaps_of_one_net_are_drawn_once_with_a_junction() {
        // Two wires from a port share the run from (-6, 0) to (-4, 0).
        let items = [
            named(0, (-6., 0.), 0),
            seg(0, (-6., 0.), (-4., 0.)),
            seg(0, (-4., 0.), (-4., 4.)),
            seg(0, (-6., 0.), (-4., 0.)),
            seg(0, (-4., 0.), (-4., -4.)),
            pin(0, (-4., 4.), 1),
            pin(0, (-4., -4.), 2),
        ];
        let analysis = analyze_nets(&items, 1);
        assert_eq!(analysis.segments.len(), 2, "{:?}", analysis.segments);
        assert!(analysis.segments.contains(&NetSegment {
            net: 0,
            a: (-6., 0.),
            b: (-4., 0.)
        }));
        assert!(analysis.segments.contains(&NetSegment {
            net: 0,
            a: (-4., -4.),
            b: (-4., 4.)
        }));
        assert_eq!(
            analysis.junctions,
            [Junction {
                net: 0,
                at: (-4., 0.)
            }]
        );
        assert!(analysis.flylines.is_empty(), "fully wired");
        assert!(analysis.conflicts.is_empty());
    }

    #[test]
    fn a_branch_ending_inside_a_segment_gets_a_dot_and_crossings_do_not() {
        let items = [
            seg(0, (0., 0.), (4., 0.)),
            seg(0, (2., 0.), (2., 3.)),
            seg(1, (1., -1.), (1., 1.)),
        ];
        let analysis = analyze_nets(&items, 2);
        assert_eq!(
            analysis.junctions,
            [Junction {
                net: 0,
                at: (2., 0.)
            }]
        );
        assert!(analysis.conflicts.is_empty(), "a crossing is not a touch");
    }

    #[test]
    fn touching_items_of_different_nets_are_conflicts() {
        let items = [
            seg(0, (0., 0.), (4., 0.)),
            seg(1, (2., 0.), (2., 3.)),
            pin(2, (4., 0.), 7),
            pin(3, (4., 0.), 8),
        ];
        let analysis = analyze_nets(&items, 4);
        let mut conflicts = analysis.conflicts.clone();
        conflicts.sort_by_key(|c| c.nets);
        assert_eq!(
            conflicts,
            [
                Conflict {
                    nets: [0, 1],
                    at: (2., 0.)
                },
                Conflict {
                    nets: [0, 2],
                    at: (4., 0.)
                },
                Conflict {
                    nets: [0, 3],
                    at: (4., 0.)
                },
                Conflict {
                    nets: [2, 3],
                    at: (4., 0.)
                },
            ]
        );
    }

    #[test]
    fn flylines_join_islands_by_their_nearest_points_and_names_join_by_name() {
        // Three pins of one net with no wires: two flylines in a tree.
        let items = [
            pin(0, (0., 0.), 0),
            pin(0, (10., 0.), 1),
            pin(0, (10., 1.), 2),
        ];
        let analysis = analyze_nets(&items, 1);
        assert_eq!(analysis.flylines.len(), 2);
        assert!(analysis.flylines.contains(&Flyline {
            net: 0,
            from: (10., 0.),
            to: (10., 1.),
            nodes: [Some(1), Some(2)]
        }));

        // A wire from a pin to a label, and a second label on another pin:
        // the labels join the two islands by name.
        let items = [
            pin(0, (0., 0.), 0),
            seg(0, (0., 0.), (2., 0.)),
            named(0, (2., 0.), 1),
            named(0, (10., 0.), 2),
            pin(0, (10., 0.), 3),
        ];
        let analysis = analyze_nets(&items, 1);
        assert!(analysis.flylines.is_empty(), "{:?}", analysis.flylines);

        // An anchor stands in for a terminal without a pin.
        let items = [
            pin(0, (0., 0.), 0),
            NetItem {
                net: 0,
                kind: NetItemKind::Anchor {
                    at: (5., 5.),
                    node: 1,
                },
            },
        ];
        let analysis = analyze_nets(&items, 1);
        assert_eq!(analysis.flylines.len(), 1);
        assert_eq!(analysis.flylines[0].to, (5., 5.));
    }

    #[test]
    fn segment_distance_measures_to_the_nearest_point() {
        assert!((segment_distance((2., 1.), (0., 0.), (4., 0.)) - 1.).abs() < 1e-9);
        assert!((segment_distance((6., 0.), (0., 0.), (4., 0.)) - 2.).abs() < 1e-9);
        assert!(on_segment((2., 0.), (0., 0.), (4., 0.)));
        assert!(!on_segment((2., 0.1), (0., 0.), (4., 0.)));
    }
}
