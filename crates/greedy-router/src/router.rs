//! Routing on uniform tracks over rectangular geometry.

use std::collections::HashMap;

use itertools::Itertools;

use crate::IndexOwned;
use crate::abs::{self, AbstractLayerConfig, AbstractRoute, GreedyAbstractRouter, Net, PosSpan};
use crate::error::{Error, Result};
use crate::geom::{Corner, Dir, Rect, Side, Sides, Sign, Span};
use crate::tracks::{TrackLocator, UniformTracks};

/// The tracks of one routing layer.
pub struct TrackInfo {
    tracks: UniformTracks,
    dir: Dir,
}

impl TrackInfo {
    pub fn tracks(&self) -> &UniformTracks {
        &self.tracks
    }

    pub fn dir(&self) -> Dir {
        self.dir
    }
}

/// A routing layer, by its index in [`RouterConfig::layers`].
pub type Layer = usize;

/// The routing area and its layers, lowest first. Directions must alternate.
pub struct RouterConfig {
    pub area: Rect,
    pub layers: Vec<LayerConfig>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LayerConfig {
    pub line: i64,
    pub space: i64,
    pub dir: Dir,
}

impl LayerConfig {
    /// The distance from one track to the next.
    pub fn pitch(&self) -> i64 {
        self.line + self.space
    }
}

/// A free run of grid points on one track.
#[derive(Debug, Eq, PartialEq, Ord, PartialOrd, Copy, Clone)]
pub struct Segment {
    pub track_id: usize,
    pub rect: Rect,
    /// Whether the run starts at the lower or left edge of the routing area.
    pub lower_boundary: bool,
    /// Whether the run ends at the upper or right edge of the routing area.
    pub upper_boundary: bool,
    pub hit: bool,
}

/// A via a route needs between two layers, given by the wire on each.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ViaRequest {
    pub bot: Layer,
    pub bot_rect: Rect,
    pub top: Layer,
    pub top_rect: Rect,
}

/// How [`GreedyRouter::expand_to_grid`] brings a rectangle onto the grid.
#[derive(Debug, Clone, Copy)]
pub enum ExpandToGridStrategy {
    Minimum,
    All,
    Side(Side),
    Corner(Corner),
}

/// Routes nets one at a time, each committing the tracks it takes.
pub struct GreedyRouter {
    inner: GreedyAbstractRouter,
    area: Rect,
    layers: Vec<TrackInfo>,
    grid_vtracks: UniformTracks,
    grid_htracks: UniformTracks,
    wires: Vec<(Layer, Rect)>,
    vias: Vec<ViaRequest>,
    net_map: HashMap<String, Net>,
}

impl GreedyRouter {
    pub fn with_config(config: RouterConfig) -> Self {
        assert!(!config.layers.is_empty());

        let layers = config
            .layers
            .iter()
            .map(|layer_cfg| TrackInfo {
                tracks: UniformTracks {
                    line: layer_cfg.line,
                    space: layer_cfg.space,
                    sign: Sign::Pos,
                    start: config.area.span(!layer_cfg.dir).start() - layer_cfg.line / 2,
                },
                dir: layer_cfg.dir,
            })
            .collect();

        let layer0 = &config.layers[0];

        let grid_vtracks = UniformTracks {
            line: layer0.line,
            space: layer0.space,
            sign: Sign::Pos,
            start: config.area.span(Dir::Horiz).start() - layer0.line / 2,
        };
        let grid_htracks = UniformTracks {
            line: layer0.line,
            space: layer0.space,
            sign: Sign::Pos,
            start: config.area.span(Dir::Vert).start() - layer0.line / 2,
        };
        let nx = grid_vtracks.track_at(config.area.right()) + 2;
        let ny = grid_htracks.track_at(config.area.top()) + 2;

        assert!(nx >= 0 && ny >= 0);

        let inner = GreedyAbstractRouter::new(
            config.layers.iter().enumerate().map(|(i, layer_cfg)| {
                assert_eq!(
                    layer_cfg.pitch() % layer0.pitch(),
                    0,
                    "layer pitch should be a multiple of the minimum pitch"
                );
                if i > 0 {
                    assert_ne!(
                        layer_cfg.dir,
                        config.layers[i - 1].dir,
                        "Layers should alternate directions"
                    );
                }
                AbstractLayerConfig {
                    grid_space: ((layer_cfg.line + layer_cfg.space) / (layer0.line + layer0.space))
                        as usize,
                    dir: layer_cfg.dir,
                }
            }),
            nx as usize,
            ny as usize,
        );

        Self {
            inner,
            area: config.area,
            layers,
            grid_vtracks,
            grid_htracks,
            wires: Vec::new(),
            vias: Vec::new(),
            net_map: HashMap::new(),
        }
    }

    pub fn get_net(&mut self, net: &str) -> Net {
        if let Some(net) = self.net_map.get(net) {
            *net
        } else {
            let new_net = self.inner.get_unused_net();
            self.net_map.insert(net.to_string(), new_net);
            new_net
        }
    }

    /// Routes between the provided geometries on `net`, if a route exists.
    pub fn route_with_net(
        &mut self,
        src_layer: Layer,
        src: Rect,
        dst_layer: Layer,
        dst: Rect,
        net: &str,
    ) -> Result<()> {
        let net = self.get_net(net);
        self.route_inner(src_layer, src, dst_layer, dst, net)
    }

    fn route_inner(
        &mut self,
        src_layer: Layer,
        src: Rect,
        dst_layer: Layer,
        dst: Rect,
        net: Net,
    ) -> Result<()> {
        // src and dst geometry must be contained within the routing area.
        assert!(contains(self.area, src));
        assert!(contains(self.area, dst));
        assert!(src_layer < self.layers.len());
        assert!(dst_layer < self.layers.len());

        let src_span = self.shrink_to_pos_span(src_layer, src);
        let dst_span = self.shrink_to_pos_span(dst_layer, dst);
        self.route_spans(src_span, dst_span, net)
    }

    /// Routes between two spans of grid points on `net`, recording the wires
    /// and vias the route needs.
    pub(crate) fn route_spans(
        &mut self,
        src_span: PosSpan,
        dst_span: PosSpan,
        net: Net,
    ) -> Result<()> {
        let route = self.inner.route_with_net(src_span, dst_span, net)?;

        let mut counter = 0;
        while counter < route.len() {
            let mut subroute = vec![route[counter]];
            counter += 1;
            while counter < route.len() && !route[counter].is_jump() {
                subroute.push(route[counter]);
                counter += 1;
            }
            let runs: Vec<(abs::Layer, AbstractRoute)> = subroute
                .into_iter()
                .chunk_by(|n| n.layer)
                .into_iter()
                .map(|(layer, group)| (layer, group.into_iter().collect::<AbstractRoute>()))
                .collect();
            let mut rects = Vec::new();
            for (i, (layer, run)) in runs.iter().enumerate() {
                let layer = *layer;
                let dir = self.inner.dir(layer);
                let (first, last) = (run[0], run[run.len() - 1]);
                let tid = first.coord(!dir);
                let track = self.track_span(layer, tid);

                let first = if i > 0 && self.inner.dir(runs[i - 1].0) != dir {
                    self.track_span(runs[i - 1].0, first.coord(dir))
                } else {
                    self.grid_track(!dir).index(first.coord(dir))
                };

                let last = if i < runs.len() - 1 && self.inner.dir(runs[i + 1].0) != dir {
                    self.track_span(runs[i + 1].0, last.coord(dir))
                } else {
                    self.grid_track(!dir).index(last.coord(dir))
                };

                let rect = Rect::span_builder()
                    .with(dir, first.union(last))
                    .with(!dir, track)
                    .build();

                rects.push((layer, rect));
            }

            let mut prev = None;
            for (layer, rect) in rects {
                self.wires.push((layer.0, rect));

                if let Some((prev_layer, prev_rect)) = prev
                    && prev_layer != layer
                {
                    let (bot, bot_rect, top, top_rect) = if prev_layer < layer {
                        (prev_layer, prev_rect, layer, rect)
                    } else {
                        (layer, rect, prev_layer, prev_rect)
                    };
                    self.vias.push(ViaRequest {
                        bot: bot.0,
                        bot_rect,
                        top: top.0,
                        top_rect,
                    });
                }
                prev = Some((layer, rect));
            }
        }

        Ok(())
    }

    /// Runs of free grid points on each of `layer`'s tracks.
    pub fn segments(&self, layer: Layer) -> Vec<Segment> {
        let abs_layer = abs::Layer(layer);
        let spans = self.inner.segments(abs_layer);
        let dir = self.inner.dir(abs_layer);
        spans
            .into_iter()
            .map(|segment| {
                let tid = segment.span.span(!dir).0;
                let track = self.track_span(abs_layer, tid);
                let start = self.grid_track(!dir).index(segment.span.span(dir).0);
                let end = self.grid_track(!dir).index(segment.span.span(dir).1);
                Segment {
                    track_id: segment.track_id,
                    rect: Rect::span_builder()
                        .with(dir, start.union(end))
                        .with(!dir, track)
                        .build(),
                    lower_boundary: segment.lower_boundary,
                    upper_boundary: segment.upper_boundary,
                    hit: false,
                }
            })
            .collect()
    }

    /// The number of routing layers.
    pub fn num_layers(&self) -> usize {
        self.layers.len()
    }

    pub fn track_info(&self, layer: Layer) -> &TrackInfo {
        &self.layers[layer]
    }

    pub fn block(&mut self, layer: Layer, rect: Rect) {
        let span = self.expand_to_pos_span(layer, rect);
        self.inner.block_span(span);
    }

    /// Makes `rect`'s grid points unavailable to every net except `net`.
    pub fn block_for_net(&mut self, layer: Layer, rect: Rect, net: &str) {
        let net = self.get_net(net);
        let span = self.expand_to_pos_span(layer, rect);
        self.inner.block_span_for_net(span, net);
    }

    pub fn occupy(&mut self, layer: Layer, rect: Rect, net: &str) -> Result<()> {
        let net = self.get_net(net);
        let span = self.expand_to_pos_span(layer, rect);
        self.inner.block_span_for_net(span, net);
        let span = self.shrink_to_pos_span(layer, rect);
        self.inner.occupy_span(span, net)?;
        Ok(())
    }

    /// Wires drawn by the routes so far.
    pub fn wires(&self) -> &[(Layer, Rect)] {
        &self.wires
    }

    /// Vias the routes so far need.
    pub fn vias(&self) -> &[ViaRequest] {
        &self.vias
    }

    fn track_span(&self, layer: abs::Layer, coord: usize) -> Span {
        let layer_info = self.inner.layer_info(layer);
        self.layers[layer.0]
            .tracks
            .index(coord / layer_info.grid_space)
    }

    pub(crate) fn inner(&self) -> &GreedyAbstractRouter {
        &self.inner
    }

    pub(crate) fn inner_mut(&mut self) -> &mut GreedyAbstractRouter {
        &mut self.inner
    }

    /// The grid tracks that run in `dir`; their spans place the grid points
    /// across it.
    pub(crate) fn grid_track(&self, dir: Dir) -> &UniformTracks {
        match dir {
            Dir::Horiz => &self.grid_htracks,
            Dir::Vert => &self.grid_vtracks,
        }
    }

    fn shrink_to_pos_span(&self, layer: Layer, rect: Rect) -> PosSpan {
        let (x_grid, y_grid) = (&self.grid_vtracks, &self.grid_htracks);

        let tx_min = x_grid
            .track_with_loc(TrackLocator::StartsAfter, rect.left())
            .try_into()
            .unwrap_or(0);
        let tx_max = x_grid
            .track_with_loc(TrackLocator::EndsBefore, rect.right())
            .try_into()
            .unwrap_or(0);
        let ty_min = y_grid
            .track_with_loc(TrackLocator::StartsAfter, rect.bottom())
            .try_into()
            .unwrap_or(0);
        let ty_max = y_grid
            .track_with_loc(TrackLocator::EndsBefore, rect.top())
            .try_into()
            .unwrap_or(0);

        PosSpan {
            layer: abs::Layer(layer),
            tx_min,
            tx_max,
            ty_min,
            ty_max,
        }
    }

    fn expand_to_pos_span(&self, layer: Layer, rect: Rect) -> PosSpan {
        let (x_grid, y_grid) = (&self.grid_vtracks, &self.grid_htracks);

        let tx_min = x_grid
            .track_with_loc(TrackLocator::StartsBefore, rect.left())
            .try_into()
            .unwrap_or(0);
        let tx_max = x_grid
            .track_with_loc(TrackLocator::EndsAfter, rect.right())
            .try_into()
            .unwrap_or(0);
        let ty_min = y_grid
            .track_with_loc(TrackLocator::StartsBefore, rect.bottom())
            .try_into()
            .unwrap_or(0);
        let ty_max = y_grid
            .track_with_loc(TrackLocator::EndsAfter, rect.top())
            .try_into()
            .unwrap_or(0);

        PosSpan {
            layer: abs::Layer(layer),
            tx_min,
            tx_max,
            ty_min,
            ty_max,
        }
    }

    /// Expands `rect` to cover grid tracks, as chosen by `strategy`.
    pub fn expand_to_grid(&self, rect: Rect, strategy: ExpandToGridStrategy) -> Rect {
        let (x_grid, y_grid) = (&self.grid_vtracks, &self.grid_htracks);
        match strategy {
            ExpandToGridStrategy::All => {
                let tx_min = x_grid.track_with_loc(TrackLocator::StartsBefore, rect.left());
                let tx_max = x_grid.track_with_loc(TrackLocator::EndsAfter, rect.right());
                let ty_min = y_grid.track_with_loc(TrackLocator::StartsBefore, rect.bottom());
                let ty_max = y_grid.track_with_loc(TrackLocator::EndsAfter, rect.top());
                Rect::from_spans(
                    x_grid.index(tx_min).union(x_grid.index(tx_max)),
                    y_grid.index(ty_min).union(y_grid.index(ty_max)),
                )
            }
            _ => {
                // Tracks that may straddle both sides of the source rectangle.
                let tx_min = x_grid.track_with_loc(TrackLocator::EndsAfter, rect.left());
                let tx_max = x_grid.track_with_loc(TrackLocator::StartsBefore, rect.right());
                let ty_min = y_grid.track_with_loc(TrackLocator::EndsAfter, rect.bottom());
                let ty_max = y_grid.track_with_loc(TrackLocator::StartsBefore, rect.top());

                // Tracks that do not straddle both sides of the source rectangle.
                let track_right = x_grid.track_with_loc(TrackLocator::StartsAfter, rect.left());
                let track_left = x_grid.track_with_loc(TrackLocator::EndsBefore, rect.right());
                let track_top = y_grid.track_with_loc(TrackLocator::StartsAfter, rect.bottom());
                let track_bot = y_grid.track_with_loc(TrackLocator::EndsBefore, rect.top());

                // Non-straddling tracks are better only when the source fully
                // encloses a track.
                let (tx_min, tx_max) = if track_right <= track_left {
                    (track_right, track_left)
                } else {
                    (tx_min, tx_max)
                };
                let (ty_min, ty_max) = if track_top <= track_bot {
                    (track_top, track_bot)
                } else {
                    (ty_min, ty_max)
                };

                // When a track is fully enclosed, `track_left` and `track_bot`
                // must name the leftmost and bottommost tracks.
                let (track_right, track_left) = if track_right < track_left {
                    (track_left, track_right)
                } else {
                    (track_right, track_left)
                };

                let (track_top, track_bot) = if track_top < track_bot {
                    (track_bot, track_top)
                } else {
                    (track_top, track_bot)
                };

                let mut tracks = Sides::new(
                    y_grid.index(ty_max),
                    x_grid.index(tx_max),
                    y_grid.index(ty_min),
                    x_grid.index(tx_min),
                );

                let tracks_constrained = Sides::new(
                    y_grid.index(track_top),
                    x_grid.index(track_right),
                    y_grid.index(track_bot),
                    x_grid.index(track_left),
                );

                let (first_dirs, second_dirs) = match strategy {
                    ExpandToGridStrategy::All => unreachable!(),
                    ExpandToGridStrategy::Side(first_dir) => {
                        tracks[first_dir] = tracks_constrained[first_dir];
                        (
                            vec![first_dir],
                            vec![Side::with_dir(first_dir.edge_dir()).collect()],
                        )
                    }
                    ExpandToGridStrategy::Corner(corner) => {
                        tracks[corner.side(Dir::Horiz)] =
                            tracks_constrained[corner.side(Dir::Horiz)];
                        tracks[corner.side(Dir::Vert)] = tracks_constrained[corner.side(Dir::Vert)];
                        (
                            vec![corner.side(Dir::Horiz)],
                            vec![vec![corner.side(Dir::Vert)]],
                        )
                    }
                    ExpandToGridStrategy::Minimum => {
                        let horiz_dirs: Vec<Side> = Side::with_dir(Dir::Horiz).collect();
                        let vert_dirs: Vec<Side> = Side::with_dir(Dir::Vert).collect();
                        (
                            vec![Side::Top, Side::Bot, Side::Left, Side::Right],
                            vec![horiz_dirs.clone(), horiz_dirs, vert_dirs.clone(), vert_dirs],
                        )
                    }
                };
                let mut best_rect = None;
                let mut best_area = i64::MAX;
                for (first_dir, second_dirs) in first_dirs.iter().zip(second_dirs.iter()) {
                    for second_dir in second_dirs {
                        let rect = Rect::span_builder()
                            .with(
                                first_dir.coord_dir(),
                                tracks[*first_dir].union(rect.span(first_dir.coord_dir())),
                            )
                            .with(
                                second_dir.coord_dir(),
                                tracks[*second_dir].union(rect.span(second_dir.coord_dir())),
                            )
                            .build();
                        if rect.area() < best_area {
                            best_rect = Some(rect);
                            best_area = rect.area();
                        }
                    }
                }

                best_rect.unwrap()
            }
        }
    }
}

fn contains(outer: Rect, inner: Rect) -> bool {
    outer.left() <= inner.left()
        && inner.right() <= outer.right()
        && outer.bottom() <= inner.bottom()
        && inner.top() <= outer.top()
}

impl From<Error> for String {
    fn from(error: Error) -> Self {
        error.to_string()
    }
}
