//! Power straps on the free segments of a router's tracks.
//!
//! Even tracks carry VSS and odd tracks VDD. A segment becomes a strap when it
//! lands fully on a target of its supply on an adjacent layer, or crosses a
//! strap of the same supply on the next strap layer.

use std::collections::HashMap;

use crate::geom::{Point, Rect, snap_to_grid};
use crate::router::{GreedyRouter, Layer, Segment, ViaRequest};

/// A supply net.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Supply {
    Vss,
    Vdd,
}

fn index(net: Supply) -> usize {
    match net {
        Supply::Vss => 0,
        Supply::Vdd => 1,
    }
}

fn net_from_idx(idx: usize) -> Supply {
    if idx.is_multiple_of(2) {
        Supply::Vss
    } else {
        Supply::Vdd
    }
}

/// Supply geometry that straps may connect to.
#[derive(Debug, Clone, Copy)]
pub struct Target {
    pub rect: Rect,
    pub net: Supply,
}

/// A placed strap.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Strap {
    pub layer: Layer,
    pub rect: Rect,
    pub net: Supply,
    /// Whether the strap starts at the lower or left edge of the routing area.
    pub lower_boundary: bool,
    /// Whether the strap ends at the upper or right edge of the routing area.
    pub upper_boundary: bool,
}

/// The straps and the vias that connect them.
#[derive(Debug, Clone, Default)]
pub struct PlacedStraps {
    pub straps: Vec<Strap>,
    pub vias: Vec<ViaRequest>,
    /// Ranges of `vias` between straps; each via in a range is a translated
    /// copy of the range's first.
    pub crossings: Vec<std::ops::Range<usize>>,
}

fn intersection(a: Rect, b: Rect) -> Option<Rect> {
    let p0 = Point::new(a.left().max(b.left()), a.bottom().max(b.bottom()));
    let p1 = Point::new(a.right().min(b.right()), a.top().min(b.top()));
    (p0.x <= p1.x && p0.y <= p1.y).then(|| Rect::new(p0, p1))
}

fn center(r: Rect) -> Point {
    Point::new((r.left() + r.right()) / 2, (r.bottom() + r.top()) / 2)
}

/// Places straps on `strap_layers`, lowest first, connecting them to
/// `targets` on the layers adjacent to each. `grid` is the manufacturing grid
/// that via centers snap to.
pub fn fill(
    router: &GreedyRouter,
    strap_layers: &[Layer],
    targets: &HashMap<Layer, Vec<Target>>,
    grid: i64,
) -> PlacedStraps {
    assert!(strap_layers.len() >= 2);
    let mut out = PlacedStraps::default();
    let mut segment_map: HashMap<Layer, Vec<Segment>> = HashMap::new();

    for &layer in strap_layers {
        segment_map.insert(layer, router.segments(layer));
        let segments = segment_map.get_mut(&layer).unwrap();
        let track_info = router.track_info(layer);

        let mut valid_target_layers = Vec::new();
        if layer > 0 {
            valid_target_layers.push((layer - 1, (layer - 1, layer)));
        }
        if layer + 1 < router.num_layers() {
            valid_target_layers.push((layer + 1, (layer, layer + 1)));
        }
        for segment in segments.iter_mut() {
            for (target_layer, (bot, top)) in valid_target_layers.iter() {
                let Some(targets) = targets.get(target_layer) else {
                    continue;
                };
                for t in targets {
                    if index(t.net) != segment.track_id % 2 {
                        continue;
                    }
                    let Some(overlap) = intersection(t.rect, segment.rect) else {
                        continue;
                    };
                    if overlap.length(track_info.dir()) == t.rect.length(track_info.dir())
                        && overlap.length(!track_info.dir()) == track_info.tracks().line
                    {
                        out.vias.push(ViaRequest {
                            bot: *bot,
                            bot_rect: t.rect,
                            top: *top,
                            top_rect: segment.rect,
                        });
                        segment.hit = true;
                    }
                }
            }
        }
    }

    for pair in strap_layers.windows(2) {
        let (bot, top) = (pair[0], pair[1]);
        let top_segments = router.segments(top);
        let bot_segments = router.segments(bot);

        // Every via between straps is a copy of the first, centered on its
        // crossing.
        let start = out.vias.len();
        let mut first: Option<(ViaRequest, Point)> = None;
        for (i, t) in top_segments.iter().enumerate() {
            for (j, b) in bot_segments.iter().enumerate() {
                if t.track_id % 2 != b.track_id % 2 {
                    continue;
                }
                let Some(overlap) = intersection(t.rect, b.rect) else {
                    continue;
                };
                let via = match first {
                    Some((via, at)) => {
                        let c = center(overlap);
                        let c = Point::new(snap_to_grid(c.x, grid), snap_to_grid(c.y, grid));
                        let d = Point::new(c.x - at.x, c.y - at.y);
                        ViaRequest {
                            bot_rect: translate(via.bot_rect, d),
                            top_rect: translate(via.top_rect, d),
                            ..via
                        }
                    }
                    None => {
                        let via = ViaRequest {
                            bot,
                            bot_rect: b.rect,
                            top,
                            top_rect: t.rect,
                        };
                        let c = center(overlap);
                        let c = Point::new(snap_to_grid(c.x, grid), snap_to_grid(c.y, grid));
                        first = Some((via, c));
                        via
                    }
                };
                out.vias.push(via);
                segment_map.get_mut(&top).unwrap()[i].hit = true;
                segment_map.get_mut(&bot).unwrap()[j].hit = true;
            }
        }
        if out.vias.len() > start {
            out.crossings.push(start..out.vias.len());
        }
    }

    for &layer in strap_layers {
        for segment in &segment_map[&layer] {
            if segment.hit {
                out.straps.push(Strap {
                    layer,
                    rect: segment.rect,
                    net: net_from_idx(segment.track_id),
                    lower_boundary: segment.lower_boundary,
                    upper_boundary: segment.upper_boundary,
                });
            }
        }
    }

    out
}

fn translate(r: Rect, d: Point) -> Rect {
    Rect::new(
        Point::new(r.left() + d.x, r.bottom() + d.y),
        Point::new(r.right() + d.x, r.top() + d.y),
    )
}
