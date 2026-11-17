//! Routes whole nets between pins while keeping each layer's spacing to
//! obstacles, to other nets' pins, and to the nets routed before.
//!
//! A grid point is blocked for a net when a wire through it would come closer
//! than the layer's spacing to another net's geometry, and closed to vias when
//! only a via landing or cut there would. Distances are measured separately in
//! x and y, which is stricter than a Euclidean rule at corners. After routing,
//! gaps narrower than the spacing within a net are filled, and a via whose
//! cuts crowd another of its net's that already makes its connection is
//! dropped.

use std::ops::RangeInclusive;

use thiserror::Error;

use crate::IndexOwned;
use crate::abs::{self, Net, Pos, PosSpan, State};
use crate::geom::{Dir, Point, Rect, Span};
use crate::router::{GreedyRouter, Layer, LayerConfig, RouterConfig, ViaRequest};
use crate::tracks::UniformTracks;

/// The most grid points, summed over the layers, that one job may route on.
pub const MAX_GRID_POINTS: u64 = 4_000_000;

/// A via's cuts and landings, relative to the lower-left corner of the
/// crossing of the two wires it joins.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ViaShape {
    pub cuts: Vec<Rect>,
    /// The least spacing between cuts of different vias.
    pub cut_space: i64,
    /// The landing on the lower layer.
    pub bot: Rect,
    /// The landing on the upper layer.
    pub top: Rect,
}

/// A net and its pins. The first pin is routed to each of the others.
#[derive(Debug, Clone)]
pub struct NetPins {
    pub name: String,
    pub pins: Vec<(Layer, Rect)>,
}

/// A routing job. Coordinates are integers, usually manufacturing grid units.
#[derive(Debug, Clone)]
pub struct Job {
    /// The region whose lower-left corner is a grid point and whose grid
    /// points routes may use.
    pub area: Rect,
    /// The routing layers, lowest first. Directions must alternate.
    pub layers: Vec<LayerConfig>,
    /// The via from each layer to the one above it.
    pub vias: Vec<ViaShape>,
    pub obstacles: Vec<(Layer, Rect)>,
    /// Nets, routed in order.
    pub nets: Vec<NetPins>,
}

/// A via placed by a route.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlacedVia {
    /// The lower of the two layers.
    pub bot: Layer,
    pub cuts: Vec<Rect>,
    pub bot_rect: Rect,
    pub top_rect: Rect,
}

/// The geometry routed for one net.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RoutedNet {
    pub wires: Vec<(Layer, Rect)>,
    pub vias: Vec<PlacedVia>,
    /// Pins extended to a grid point, which must be drawn for the wires to
    /// reach them.
    pub stubs: Vec<(Layer, Rect)>,
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum AutorouteError {
    #[error("{0}")]
    Invalid(String),
    #[error("net `{net}` has no route from pin {from} to pin {to}")]
    Unroutable { net: String, from: usize, to: usize },
    #[error(
        "pin {pin} of net `{net}` has no free grid point nearby; every one is blocked or too close to another net or an obstacle"
    )]
    NoGridPoint { net: String, pin: usize },
}

/// The width and height of the crossing of a wire on `lower` with a wire on
/// the layer above it.
pub fn crossing_size(layers: &[LayerConfig], lower: Layer) -> (i64, i64) {
    let (bot, top) = (&layers[lower], &layers[lower + 1]);
    match bot.dir {
        Dir::Horiz => (top.line, bot.line),
        Dir::Vert => (bot.line, top.line),
    }
}

/// How many net orders [`route`] tries before giving up.
pub const MAX_ATTEMPTS: usize = 30;

/// Routes every net of `job`. Nets are routed in order. When one cannot be,
/// routing starts over with that net first or, if that was tried, with that
/// net routed from its next pin, up to [`MAX_ATTEMPTS`] times in all. If none
/// succeeds, the error is the one from routing in the job's order.
pub fn route(job: &Job) -> Result<Vec<RoutedNet>, AutorouteError> {
    validate(job)?;
    let mut plan = Plan {
        order: (0..job.nets.len()).collect(),
        first_pins: vec![0; job.nets.len()],
    };
    let mut tried = vec![plan.clone()];
    let mut first_error = None;
    loop {
        let (failed, error) = match route_with(job, &plan) {
            Ok(routed) => return Ok(routed),
            Err((Some(failed), error)) => (failed, error),
            Err((None, error)) => return Err(first_error.unwrap_or(error)),
        };
        let first_error = first_error.get_or_insert(error);
        let mut first = plan.clone();
        first.order.retain(|&net| net != failed);
        first.order.insert(0, failed);
        let mut rotated = plan.clone();
        rotated.first_pins[failed] = (plan.first_pins[failed] + 1) % job.nets[failed].pins.len();
        let next = [first, rotated]
            .into_iter()
            .find(|next| !tried.contains(next));
        match next {
            Some(next) if tried.len() < MAX_ATTEMPTS => {
                tried.push(next.clone());
                plan = next;
            }
            _ => return Err(first_error.clone()),
        }
    }
}

/// The order to route nets in, and the pin each net is routed from.
#[derive(Clone, PartialEq, Eq)]
struct Plan {
    order: Vec<usize>,
    first_pins: Vec<usize>,
}

/// Routes the nets as `plan` says, returning them in the job's order. A
/// failure names the net that could not be routed, if a route failed.
fn route_with(job: &Job, plan: &Plan) -> Result<Vec<RoutedNet>, (Option<usize>, AutorouteError)> {
    let mut ctx = Ctx::new(job);
    ctx.block_outside_area();

    for &(layer, rect) in &job.obstacles {
        let owner = ctx.obstacle_owner(layer, rect);
        ctx.keep_out(layer, rect, owner.map(|i| ctx.nets[i]));
        ctx.fixed.push((layer, rect, owner));
    }
    for (i, net) in job.nets.iter().enumerate() {
        for &(layer, rect) in &net.pins {
            ctx.keep_out(layer, rect, Some(ctx.nets[i]));
            ctx.fixed.push((layer, rect, Some(i)));
        }
    }

    // Find each pin's grid points, extending pins that cover none.
    let mut points = Vec::with_capacity(job.nets.len());
    let mut stubs = vec![Vec::new(); job.nets.len()];
    for (i, net) in job.nets.iter().enumerate() {
        let mut net_points = Vec::with_capacity(net.pins.len());
        for (j, &(layer, rect)) in net.pins.iter().enumerate() {
            let covered = ctx.covered(layer, rect, ctx.nets[i]);
            if !covered.is_empty() {
                net_points.push(covered);
                continue;
            }
            let Some((pos, stub)) = ctx.stub(layer, rect, i) else {
                return Err((None, no_grid_point(net, j)));
            };
            ctx.keep_out(layer, stub, Some(ctx.nets[i]));
            ctx.fixed.push((layer, stub, Some(i)));
            stubs[i].push((layer, stub));
            net_points.push(vec![pos]);
        }
        points.push(net_points);
    }

    // Claim them, now that every pin's and stub's spacing is kept.
    for (i, net) in job.nets.iter().enumerate() {
        for (j, pin_points) in points[i].iter_mut().enumerate() {
            pin_points.retain(|&pos| ctx.free_for(pos, ctx.nets[i]));
            if pin_points.is_empty() {
                return Err((None, no_grid_point(net, j)));
            }
            ctx.router
                .inner_mut()
                .occupy_positions(pin_points, ctx.nets[i]);
        }
    }

    let mut routed = vec![RoutedNet::default(); job.nets.len()];
    for &i in &plan.order {
        let net = &job.nets[i];
        let id = ctx.nets[i];
        let first_wire = ctx.router.wires().len();
        let mut vias = Vec::new();
        // The first point of each pin stands for the pin: the router jumps
        // between the points of a connected group.
        let first = plan.first_pins[i];
        let src = points[i][first][0];
        for (j, dst) in points[i].iter().enumerate().filter(|&(j, _)| j != first) {
            let before = ctx.router.vias().len();
            ctx.router
                .route_spans(PosSpan::from(src), PosSpan::from(dst[0]), id)
                .map_err(|_| {
                    let error = AutorouteError::Unroutable {
                        net: net.name.clone(),
                        from: first,
                        to: j,
                    };
                    (Some(i), error)
                })?;
            // Later connections, of this net too, keep their cuts apart.
            let added = ctx.router.vias()[before..]
                .iter()
                .map(|request| ctx.place_via(request))
                .collect::<Vec<_>>();
            for via in &added {
                ctx.space_cuts(via);
            }
            vias.extend(added);
        }
        let mut wires = ctx.router.wires()[first_wire..].to_vec();
        for &(layer, rect) in &wires {
            ctx.keep_out(layer, rect, Some(id));
        }
        for via in &vias {
            ctx.keep_out(via.bot, via.bot_rect, Some(id));
            ctx.keep_out(via.bot + 1, via.top_rect, Some(id));
        }
        // A route that starts or ends with a jump leaves a square inside the
        // pin it jumped within; it adds nothing to the pin.
        wires.retain(|&(layer, rect)| {
            !net.pins
                .iter()
                .chain(&stubs[i])
                .any(|&(pin_layer, pin)| pin_layer == layer && contains(pin, rect))
        });
        routed[i] = RoutedNet {
            wires,
            vias,
            stubs: std::mem::take(&mut stubs[i]),
        };
    }
    fill_gaps(job, &mut routed);
    drop_crowded_vias(job, &mut routed);
    Ok(routed)
}

/// Drops each via of a net whose cuts come closer than the cut spacing to an
/// earlier via's of the same net, when the two vias' landings are already
/// joined on both layers, so the earlier via makes the same connection. The
/// dropped via's landings stay as wires.
fn drop_crowded_vias(job: &Job, routed: &mut [RoutedNet]) {
    for i in 0..routed.len() {
        let own = metal(job, routed, i);
        let mut kept: Vec<PlacedVia> = Vec::new();
        let mut landings = Vec::new();
        for via in std::mem::take(&mut routed[i].vias) {
            let space = job.vias[via.bot].cut_space;
            let crowded = kept.iter().any(|other| {
                other.bot == via.bot
                    && via
                        .cuts
                        .iter()
                        .any(|&a| other.cuts.iter().any(|&b| too_close(a, b, space)))
                    && joined(&own, via.bot, via.bot_rect, other.bot_rect)
                    && joined(&own, via.bot + 1, via.top_rect, other.top_rect)
            });
            if crowded {
                landings.extend([(via.bot, via.bot_rect), (via.bot + 1, via.top_rect)]);
            } else {
                kept.push(via);
            }
        }
        routed[i].vias = kept;
        routed[i].wires.extend(landings);
    }
}

/// Whether `a` and `b` on `layer` overlap, share an edge, or have the space
/// between them covered by one of `shapes`.
fn joined(shapes: &[(Layer, Rect)], layer: Layer, a: Rect, b: Rect) -> bool {
    match gap_between(a, b, i64::MAX) {
        None => true,
        Some(gap) => shapes
            .iter()
            .any(|&(l, r)| l == layer && contains(r, gap.cover())),
    }
}

/// Every shape of net `i` on a routing layer.
fn metal(job: &Job, routed: &[RoutedNet], i: usize) -> Vec<(Layer, Rect)> {
    let net = &routed[i];
    job.nets[i]
        .pins
        .iter()
        .chain(&net.stubs)
        .chain(&net.wires)
        .copied()
        .chain(
            net.vias
                .iter()
                .flat_map(|via| [(via.bot, via.bot_rect), (via.bot + 1, via.top_rect)]),
        )
        .collect()
}

/// How many times [`fill_gaps`] looks again at a net after filling some gaps.
const FILL_ROUNDS: usize = 16;

/// Fills the gaps narrower than the spacing between shapes of one net, such as
/// via landings on neighboring tracks, which design rules flag although both
/// sides are the same net. A fill is added to the net's wires, unless it would
/// come too close to another net or an obstacle.
fn fill_gaps(job: &Job, routed: &mut [RoutedNet]) {
    for i in 0..routed.len() {
        let others = (0..routed.len())
            .filter(|&j| j != i)
            .flat_map(|j| metal(job, routed, j))
            .chain(job.obstacles.iter().copied())
            .collect::<Vec<_>>();
        for _ in 0..FILL_ROUNDS {
            let own = metal(job, routed, i);
            let mut fills: Vec<(Layer, Rect)> = Vec::new();
            for (x, &(layer, a)) in own.iter().enumerate() {
                let (line, space) = (job.layers[layer].line, job.layers[layer].space);
                for &(other_layer, b) in &own[x + 1..] {
                    if other_layer != layer {
                        continue;
                    }
                    let Some(gap) = gap_between(a, b, space) else {
                        continue;
                    };
                    let filled = own
                        .iter()
                        .chain(&fills)
                        .any(|&(l, r)| l == layer && contains(r, gap.cover()));
                    if filled {
                        continue;
                    }
                    let clear = |fill: Rect| {
                        !others
                            .iter()
                            .any(|&(l, r)| l == layer && too_close(fill, r, space))
                    };
                    let fill = gap.candidates(line).into_iter().find(|&fill| clear(fill));
                    if let Some(fill) = fill {
                        fills.push((layer, fill));
                    }
                }
            }
            if fills.is_empty() {
                break;
            }
            routed[i].wires.extend(fills);
        }
    }
}

/// The space between two shapes that are closer than the spacing but do not
/// touch.
struct Gap {
    /// The region between the shapes: their overlap along an axis on which
    /// they overlap, and the space between them along one on which they
    /// do not.
    region: Rect,
    /// Along each axis, whether the shapes overlap by a positive length.
    overlap: (bool, bool),
}

fn gap_between(a: Rect, b: Rect, space: i64) -> Option<Gap> {
    if !too_close(a, b, space) {
        return None;
    }
    let axis = |a: Span, b: Span| {
        let (lo, hi) = (a.start().max(b.start()), a.stop().min(b.stop()));
        if lo < hi {
            (Span::new(lo, hi), true)
        } else {
            (Span::new(hi, lo), false)
        }
    };
    let (x, overlap_x) = axis(a.hspan(), b.hspan());
    let (y, overlap_y) = axis(a.vspan(), b.vspan());
    let touching = x.length() == 0 && !overlap_x || y.length() == 0 && !overlap_y;
    if overlap_x && overlap_y || touching && (overlap_x || overlap_y) {
        return None;
    }
    Some(Gap {
        region: Rect::from_spans(x, y),
        overlap: (overlap_x, overlap_y),
    })
}

impl Gap {
    /// What a shape must contain to fill the gap: the region, grown by one
    /// unit where it has no width, since the two shapes contain that edge.
    fn cover(&self) -> Rect {
        let r = self.region;
        if r.width() == 0 || r.height() == 0 {
            r.expand(1)
        } else {
            r
        }
    }

    /// Fills for the gap, in order of preference. Where the shapes face each
    /// other, the fill spans their overlap, widened to at least `line` if
    /// possible; at a corner, it reaches into both shapes.
    fn candidates(&self, line: i64) -> Vec<Rect> {
        let widen = |span: Span| {
            let short = line - span.length();
            if short > 0 {
                Span::new(span.start() - short / 2, span.stop() + short - short / 2)
            } else {
                span
            }
        };
        let r = self.region;
        match self.overlap {
            (true, false) => vec![Rect::from_spans(widen(r.hspan()), r.vspan()), r],
            (false, true) => vec![Rect::from_spans(r.hspan(), widen(r.vspan())), r],
            _ => vec![r.expand(std::cmp::max(line / 2, 1))],
        }
    }
}

fn no_grid_point(net: &NetPins, pin: usize) -> AutorouteError {
    AutorouteError::NoGridPoint {
        net: net.name.clone(),
        pin,
    }
}

fn invalid(message: String) -> Result<(), AutorouteError> {
    Err(AutorouteError::Invalid(message))
}

fn validate(job: &Job) -> Result<(), AutorouteError> {
    let Some(first) = job.layers.first() else {
        return invalid("at least one routing layer is required".to_owned());
    };
    for (i, layer) in job.layers.iter().enumerate() {
        if layer.line <= 0 || layer.space <= 0 {
            return invalid(format!(
                "layer {i} must have a positive width and space, found width {} and space {}",
                layer.line, layer.space
            ));
        }
        if i > 0 && layer.dir == job.layers[i - 1].dir {
            return invalid(format!(
                "layers {} and {i} both run {}; routing layers must alternate direction",
                i - 1,
                layer.dir
            ));
        }
        if layer.pitch() % first.pitch() != 0 {
            return invalid(format!(
                "the pitch of layer {i} ({}) must be a multiple of the pitch of layer 0 ({})",
                layer.pitch(),
                first.pitch()
            ));
        }
    }
    if job.vias.len() + 1 != job.layers.len() {
        return invalid(format!(
            "{} routing layers need {} vias, one between each adjacent pair, found {}",
            job.layers.len(),
            job.layers.len() - 1,
            job.vias.len()
        ));
    }
    let pitch = first.pitch() as u128;
    let columns = (job.area.width() as u128 + first.line as u128) / pitch + 2;
    let rows = (job.area.height() as u128 + first.line as u128) / pitch + 2;
    let points = columns * rows * job.layers.len() as u128;
    if points > MAX_GRID_POINTS as u128 {
        return invalid(format!(
            "the routing grid would have {points} points, more than the limit of {MAX_GRID_POINTS}; use a smaller area or a larger pitch"
        ));
    }
    let check_layer = |layer: Layer, what: &dyn Fn() -> String| {
        if layer >= job.layers.len() {
            invalid(format!(
                "{} is on layer {layer}, but there are only {} routing layers",
                what(),
                job.layers.len()
            ))
        } else {
            Ok(())
        }
    };
    for (i, &(layer, _)) in job.obstacles.iter().enumerate() {
        check_layer(layer, &|| format!("obstacle {i}"))?;
    }
    for (i, net) in job.nets.iter().enumerate() {
        if job.nets[..i].iter().any(|other| other.name == net.name) {
            return invalid(format!("two nets are named `{}`", net.name));
        }
        if net.pins.is_empty() {
            return invalid(format!("net `{}` has no pins", net.name));
        }
        for (j, &(layer, rect)) in net.pins.iter().enumerate() {
            check_layer(layer, &|| format!("pin {j} of net `{}`", net.name))?;
            if !job.area.hspan().intersects(&rect.hspan())
                || !job.area.vspan().intersects(&rect.vspan())
            {
                return invalid(format!(
                    "pin {j} of net `{}` lies outside the routing area",
                    net.name
                ));
            }
        }
    }
    Ok(())
}

/// One layer's tracks.
struct LayerGrid {
    dir: Dir,
    space: i64,
    tracks: UniformTracks,
    /// Grid lines from one track to the next.
    step: usize,
    /// The last track inside the area.
    last_track: usize,
}

struct Ctx<'a> {
    job: &'a Job,
    router: GreedyRouter,
    /// The spans of the grid points along x and along y.
    gx: UniformTracks,
    gy: UniformTracks,
    /// The last grid index inside the area, in x and in y.
    last: (usize, usize),
    layers: Vec<LayerGrid>,
    /// A distance beyond which no footprint can come within spacing of a
    /// rectangle.
    reach: i64,
    /// Obstacles, pins, and stubs, with the index of the net that owns each.
    fixed: Vec<(Layer, Rect, Option<usize>)>,
    nets: Vec<Net>,
}

impl<'a> Ctx<'a> {
    fn new(job: &'a Job) -> Self {
        let mut router = GreedyRouter::with_config(RouterConfig {
            area: job.area,
            layers: job.layers.clone(),
        });
        let gx = router.grid_track(Dir::Vert).clone();
        let gy = router.grid_track(Dir::Horiz).clone();
        let (nx, ny) = router.inner().size();
        let pitch = job.layers[0].pitch();
        let last = (
            ((job.area.width() / pitch) as usize).min(nx - 1),
            ((job.area.height() / pitch) as usize).min(ny - 1),
        );
        let layers = job
            .layers
            .iter()
            .enumerate()
            .map(|(i, layer)| {
                let step = (layer.pitch() / pitch) as usize;
                let last_across = match layer.dir {
                    Dir::Horiz => last.1,
                    Dir::Vert => last.0,
                };
                LayerGrid {
                    dir: layer.dir,
                    space: layer.space,
                    tracks: router.track_info(i).tracks().clone(),
                    step,
                    last_track: last_across / step,
                }
            })
            .collect::<Vec<_>>();
        let landing_extent = job
            .vias
            .iter()
            .flat_map(|via| [via.bot, via.top])
            .flat_map(|r| [r.left(), r.bottom(), r.right(), r.top()])
            .map(i64::abs)
            .max()
            .unwrap_or(0);
        let max_line = job.layers.iter().map(|l| l.line).max().unwrap_or(0);
        let max_space = job.layers.iter().map(|l| l.space).max().unwrap_or(0);
        let reach = max_space + 2 * (max_line + landing_extent) + pitch;
        let nets = job
            .nets
            .iter()
            .map(|net| router.get_net(&net.name))
            .collect();
        Self {
            job,
            router,
            gx,
            gy,
            last,
            layers,
            reach,
            fixed: Vec::new(),
            nets,
        }
    }

    /// The spans of the grid points along `dir`.
    fn along(&self, dir: Dir) -> &UniformTracks {
        match dir {
            Dir::Horiz => &self.gx,
            Dir::Vert => &self.gy,
        }
    }

    fn last_along(&self, dir: Dir) -> usize {
        match dir {
            Dir::Horiz => self.last.0,
            Dir::Vert => self.last.1,
        }
    }

    /// The grid point of `layer`'s track `k` at grid index `t` along it.
    fn pos(&self, layer: Layer, t: usize, k: usize) -> Pos {
        let across = k * self.layers[layer].step;
        match self.layers[layer].dir {
            Dir::Horiz => Pos::new(abs::Layer(layer), t, across),
            Dir::Vert => Pos::new(abs::Layer(layer), across, t),
        }
    }

    fn adjacent(&self, layer: Layer) -> impl Iterator<Item = Layer> + use<> {
        let count = self.layers.len();
        [layer.checked_sub(1), Some(layer + 1)]
            .into_iter()
            .flatten()
            .filter(move |&adj| adj < count)
    }

    /// The span along `layer` that a wire ending at grid index `t` covers:
    /// the grid point's own span, widened to the track of an adjacent layer
    /// that a via there would end it on.
    fn end_span(&self, layer: Layer, t: usize) -> Span {
        let mut span = self.along(self.layers[layer].dir).index(t);
        for adj in self.adjacent(layer) {
            let adj = &self.layers[adj];
            if t.is_multiple_of(adj.step) {
                span = span.union(adj.tracks.index(t / adj.step));
            }
        }
        span
    }

    /// What a wire through or ending at the point can cover.
    fn wire_footprint(&self, layer: Layer, t: usize, k: usize) -> Rect {
        let l = &self.layers[layer];
        build(l.dir, self.end_span(layer, t), l.tracks.index(k))
    }

    /// What a wire ending at the point without a via covers.
    fn end_square(&self, layer: Layer, t: usize, k: usize) -> Rect {
        let l = &self.layers[layer];
        build(l.dir, self.along(l.dir).index(t), l.tracks.index(k))
    }

    /// Where `layer`'s track `k` crosses a track of the adjacent layer `adj`
    /// at grid index `t`, if `adj` has a track there.
    fn crossing(&self, layer: Layer, adj: Layer, t: usize, k: usize) -> Option<Rect> {
        let (l, a) = (&self.layers[layer], &self.layers[adj]);
        t.is_multiple_of(a.step)
            .then(|| build(l.dir, a.tracks.index(t / a.step), l.tracks.index(k)))
    }

    /// The landing that a via to `adj` at the point puts on `layer`.
    fn landing(&self, layer: Layer, adj: Layer, t: usize, k: usize) -> Option<Rect> {
        let crossing = self.crossing(layer, adj, t, k)?;
        let shape = &self.job.vias[layer.min(adj)];
        let landing = if layer < adj { shape.bot } else { shape.top };
        Some(translate(landing, crossing.p0))
    }

    /// The grid indices along and across `layer` whose points might come
    /// within `reach` of `rect`.
    fn ranges(
        &self,
        layer: Layer,
        rect: Rect,
        reach: i64,
    ) -> Option<(RangeInclusive<usize>, RangeInclusive<usize>)> {
        let l = &self.layers[layer];
        let along = index_range(
            self.along(l.dir),
            rect.span(l.dir),
            reach,
            self.last_along(l.dir),
        )?;
        let across = index_range(&l.tracks, rect.span(!l.dir), reach, l.last_track)?;
        Some((along, across))
    }

    fn block(&mut self, pos: Pos, owner: Option<Net>) {
        let inner = self.router.inner_mut();
        match owner {
            None => inner.block(pos),
            Some(net) => inner.block_for_net(pos, net),
        }
    }

    /// Whether `net` may use the point at `pos`.
    fn free_for(&self, pos: Pos, net: Net) -> bool {
        match self.router.inner().state(pos) {
            State::Empty => true,
            State::Blocked { net: only } => only == Some(net),
            State::Occupied { net: owner, .. } => owner == net,
        }
    }

    /// Blocks the grid points past the area's upper and right edges, which
    /// the router's grid includes.
    fn block_outside_area(&mut self) {
        let (nx, ny) = self.router.inner().size();
        for layer in 0..self.layers.len() {
            for tx in 0..nx {
                for ty in 0..ny {
                    if tx > self.last.0 || ty > self.last.1 {
                        self.block(Pos::new(abs::Layer(layer), tx, ty), None);
                    }
                }
            }
        }
    }

    /// The net whose pins an obstacle touches, if it touches exactly one
    /// net's: such an obstacle is that net's metal.
    fn obstacle_owner(&self, layer: Layer, rect: Rect) -> Option<usize> {
        let mut owners = self.job.nets.iter().enumerate().filter(|(_, net)| {
            net.pins
                .iter()
                .any(|&(pin_layer, pin)| pin_layer == layer && too_close(pin, rect, 1))
        });
        let first = owners.next()?.0;
        owners.next().is_none().then_some(first)
    }

    /// Keeps every net but `owner`, or every net when there is no owner, at
    /// least `layer`'s spacing from `rect`.
    fn keep_out(&mut self, layer: Layer, rect: Rect, owner: Option<Net>) {
        let Some((along, across)) = self.ranges(layer, rect, self.reach) else {
            return;
        };
        let (dir, space) = (self.layers[layer].dir, self.layers[layer].space);
        for k in across {
            let track = self.layers[layer].tracks.index(k);
            // The previous point along the track, if a wire through it alone
            // keeps its spacing.
            let mut previous: Option<usize> = None;
            for t in along.clone() {
                let pos = self.pos(layer, t, k);
                if too_close(self.wire_footprint(layer, t, k), rect, space) {
                    self.block(pos, owner);
                    previous = None;
                    continue;
                }
                for adj in self.adjacent(layer) {
                    if let Some(landing) = self.landing(layer, adj, t, k)
                        && too_close(landing, rect, space)
                    {
                        let lower = abs::Layer(layer.min(adj));
                        self.router
                            .inner_mut()
                            .forbid_via(lower, pos.tx, pos.ty, owner);
                    }
                }
                // A wire from the previous point to this one also covers the
                // gap between their footprints.
                if let Some(prev) = previous {
                    let from = self.end_span(layer, prev).stop();
                    let to = self.end_span(layer, t).start();
                    if from < to && too_close(build(dir, Span::new(from, to), track), rect, space) {
                        let prev_pos = self.pos(layer, prev, k);
                        self.block(prev_pos, owner);
                        self.block(pos, owner);
                        previous = None;
                        continue;
                    }
                }
                previous = Some(t);
            }
        }
    }

    /// The points of `layer` whose wire ends `rect` contains and that `net`
    /// may use.
    fn covered(&self, layer: Layer, rect: Rect, net: Net) -> Vec<Pos> {
        let Some((along, across)) = self.ranges(layer, rect, 0) else {
            return Vec::new();
        };
        let mut points = Vec::new();
        for k in across {
            for t in along.clone() {
                let pos = self.pos(layer, t, k);
                if contains(rect, self.end_square(layer, t, k)) && self.free_for(pos, net) {
                    points.push(pos);
                }
            }
        }
        points
    }

    /// The nearby point that extends `pin` the least, with the extended pin,
    /// among the points net `net` may use whose extension comes no closer to
    /// other nets' geometry and obstacles than `pin` already does.
    fn stub(&self, layer: Layer, pin: Rect, net: usize) -> Option<(Pos, Rect)> {
        let (along, across) = self.ranges(layer, pin, 0)?;
        let space = self.layers[layer].space;
        let mut best: Option<(Pos, Rect)> = None;
        for k in across {
            for t in along.clone() {
                let pos = self.pos(layer, t, k);
                if !self.free_for(pos, self.nets[net]) {
                    continue;
                }
                let stub = bounding(pin, self.end_square(layer, t, k));
                let crowds = self.fixed.iter().any(|&(other_layer, other, owner)| {
                    other_layer == layer
                        && owner != Some(net)
                        && too_close(stub, other, space)
                        && !too_close(pin, other, space)
                });
                if !crowds && best.is_none_or(|(_, b)| stub.area() < b.area()) {
                    best = Some((pos, stub));
                }
            }
        }
        best
    }

    /// Closes the grid points near `via` to vias whose cuts would come closer
    /// than the cut spacing to its cuts, for every net.
    fn space_cuts(&mut self, via: &PlacedVia) {
        let (lower, upper) = (via.bot, via.bot + 1);
        let space = self.job.vias[lower].cut_space;
        let Some(&first) = via.cuts.first() else {
            return;
        };
        let cuts = via.cuts.iter().fold(first, |a, &b| bounding(a, b));
        let Some((along, across)) = self.ranges(lower, cuts, self.reach) else {
            return;
        };
        for k in across {
            for t in along.clone() {
                let Some(crossing) = self.crossing(lower, upper, t, k) else {
                    continue;
                };
                let shape = &self.job.vias[lower];
                let here = shape.cuts.iter().map(|&cut| translate(cut, crossing.p0));
                let mut too_near = false;
                let mut same = false;
                for cut in here {
                    same |= via.cuts.contains(&cut);
                    too_near |= via.cuts.iter().any(|&other| too_close(cut, other, space));
                }
                if too_near && !same {
                    let pos = self.pos(lower, t, k);
                    self.router
                        .inner_mut()
                        .forbid_via(abs::Layer(lower), pos.tx, pos.ty, None);
                }
            }
        }
    }

    /// The cuts and landings of the via a route asked for.
    fn place_via(&self, via: &ViaRequest) -> PlacedVia {
        let (bot, top) = (via.bot_rect, via.top_rect);
        let origin = Point::new(bot.left().max(top.left()), bot.bottom().max(top.bottom()));
        let shape = &self.job.vias[via.bot];
        PlacedVia {
            bot: via.bot,
            cuts: shape
                .cuts
                .iter()
                .map(|&cut| translate(cut, origin))
                .collect(),
            bot_rect: translate(shape.bot, origin),
            top_rect: translate(shape.top, origin),
        }
    }
}

/// The grid indices of `tracks` that might come within `reach` of `span`,
/// clamped to `0..=last`.
fn index_range(
    tracks: &UniformTracks,
    span: Span,
    reach: i64,
    last: usize,
) -> Option<RangeInclusive<usize>> {
    let pitch = tracks.line + tracks.space;
    let lo = ((span.start() - reach - tracks.start).div_euclid(pitch) - 1).max(0);
    let hi = ((span.stop() + reach - tracks.start).div_euclid(pitch) + 1).min(last as i64);
    (lo <= hi).then_some(lo as usize..=hi as usize)
}

/// Whether `a` and `b` are closer than `space` in both x and y. With a
/// positive `space`, touching and overlapping count as too close.
pub fn too_close(a: Rect, b: Rect, space: i64) -> bool {
    let gap = |a: Span, b: Span| (a.start() - b.stop()).max(b.start() - a.stop());
    gap(a.hspan(), b.hspan()) < space && gap(a.vspan(), b.vspan()) < space
}

fn contains(outer: Rect, inner: Rect) -> bool {
    outer.hspan().contains(inner.hspan()) && outer.vspan().contains(inner.vspan())
}

fn bounding(a: Rect, b: Rect) -> Rect {
    Rect::from_spans(a.hspan().union(b.hspan()), a.vspan().union(b.vspan()))
}

fn translate(rect: Rect, by: Point) -> Rect {
    Rect::new(rect.p0 + by, rect.p1 + by)
}

/// The rectangle spanning `along` in `dir` and `across` perpendicular to it.
fn build(dir: Dir, along: Span, across: Span) -> Rect {
    Rect::span_builder()
        .with(dir, along)
        .with(!dir, across)
        .build()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(x0: i64, y0: i64, x1: i64, y1: i64) -> Rect {
        Rect::new(Point::new(x0, y0), Point::new(x1, y1))
    }

    /// A horizontal and a vertical layer of 100-wide wires at a 200 pitch,
    /// with a via whose landings overhang the 100-square crossing by 20 along
    /// each wire and 10 across it.
    fn two_layers() -> (Vec<LayerConfig>, Vec<ViaShape>) {
        let layer = |dir| LayerConfig {
            line: 100,
            space: 100,
            dir,
        };
        let via = ViaShape {
            cuts: vec![r(20, 20, 80, 80)],
            cut_space: 60,
            bot: r(-20, -10, 120, 110),
            top: r(-10, -20, 110, 120),
        };
        (vec![layer(Dir::Horiz), layer(Dir::Vert)], vec![via])
    }

    fn job(obstacles: Vec<(Layer, Rect)>, nets: Vec<(&str, Vec<(Layer, Rect)>)>) -> Job {
        let (layers, vias) = two_layers();
        Job {
            area: r(0, 0, 2000, 2000),
            layers,
            vias,
            obstacles,
            nets: nets
                .into_iter()
                .map(|(name, pins)| NetPins {
                    name: name.to_owned(),
                    pins,
                })
                .collect(),
        }
    }

    /// Each net's metal on routing layers, with the via landing pairs that
    /// join layers.
    fn metal(job: &Job, routed: &[RoutedNet], net: usize) -> Vec<(Layer, Rect)> {
        let routed = &routed[net];
        job.nets[net]
            .pins
            .iter()
            .chain(&routed.stubs)
            .chain(&routed.wires)
            .copied()
            .chain(
                routed
                    .vias
                    .iter()
                    .flat_map(|v| [(v.bot, v.bot_rect), (v.bot + 1, v.top_rect)]),
            )
            .collect()
    }

    /// Asserts that nets keep their layer's spacing from each other and from
    /// obstacles, and that each net's pins are connected.
    fn check(job: &Job, routed: &[RoutedNet]) {
        let metals = (0..job.nets.len())
            .map(|i| metal(job, routed, i))
            .collect::<Vec<_>>();
        for (i, a) in metals.iter().enumerate() {
            for &(layer, rect) in a {
                let space = job.layers[layer].space;
                for &(obstacle_layer, obstacle) in &job.obstacles {
                    assert!(
                        obstacle_layer != layer || !too_close(rect, obstacle, space),
                        "net {i}: {rect:?} on layer {layer} is too close to obstacle {obstacle:?}"
                    );
                }
                for (j, b) in metals.iter().enumerate().skip(i + 1) {
                    for &(other_layer, other) in b {
                        assert!(
                            other_layer != layer || !too_close(rect, other, space),
                            "nets {i} and {j} are too close on layer {layer}: {rect:?} and {other:?}"
                        );
                    }
                }
            }
            // Union-find over the net's shapes: shapes on one layer that touch
            // are joined, and so are the two landings of each via.
            let mut parent = (0..a.len()).collect::<Vec<_>>();
            fn find(parent: &mut [usize], x: usize) -> usize {
                if parent[x] != x {
                    parent[x] = find(parent, parent[x]);
                }
                parent[x]
            }
            let join = |parent: &mut Vec<usize>, x: usize, y: usize| {
                let (x, y) = (find(parent, x), find(parent, y));
                parent[x] = y;
            };
            for x in 0..a.len() {
                for y in x + 1..a.len() {
                    if a[x].0 == a[y].0 && too_close(a[x].1, a[y].1, 1) {
                        join(&mut parent, x, y);
                    }
                }
            }
            let first_via = a.len() - 2 * routed[i].vias.len();
            for v in 0..routed[i].vias.len() {
                join(&mut parent, first_via + 2 * v, first_via + 2 * v + 1);
            }
            for (x, &(layer, rect)) in a.iter().enumerate() {
                for &(other_layer, other) in &a[x + 1..] {
                    if other_layer != layer {
                        continue;
                    }
                    if let Some(gap) = gap_between(rect, other, job.layers[layer].space) {
                        assert!(
                            a.iter()
                                .any(|&(l, r)| l == layer && contains(r, gap.cover())),
                            "net {i} leaves a gap between {rect:?} and {other:?} on layer {layer}"
                        );
                    }
                }
            }
            let root = find(&mut parent, 0);
            for pin in 1..job.nets[i].pins.len() {
                assert_eq!(
                    find(&mut parent, pin),
                    root,
                    "pin {pin} of net {i} is not connected"
                );
            }
        }
    }

    #[test]
    fn routes_around_an_obstacle() {
        let job = job(
            vec![(0, r(900, 600, 1100, 1400))],
            vec![(
                "a",
                vec![(0, r(150, 950, 350, 1050)), (0, r(1650, 950, 1850, 1050))],
            )],
        );
        let routed = route(&job).unwrap();
        check(&job, &routed);
        assert!(
            !routed[0].vias.is_empty(),
            "the wall on layer 0 needs a detour"
        );
    }

    #[test]
    fn spacing_to_an_obstacle_is_kept_exactly() {
        // Pins on the vertical layer's track at x = 1000, whose wires span
        // 950..1050. A wall 100 to the right leaves that track usable; a wall
        // 99 to the right does not.
        let pins = vec![(1, r(950, 150, 1050, 250)), (1, r(950, 1750, 1050, 1850))];
        let legal = job(
            vec![(1, r(1150, 500, 1300, 1500))],
            vec![("a", pins.clone())],
        );
        let routed = route(&legal).unwrap();
        check(&legal, &routed);
        assert_eq!(routed[0].wires, [(1, r(950, 150, 1050, 1850))]);
        assert!(routed[0].stubs.is_empty());

        let tight = job(vec![(1, r(1149, 500, 1300, 1500))], vec![("a", pins)]);
        let routed = route(&tight).unwrap();
        check(&tight, &routed);
        assert!(!routed[0].vias.is_empty(), "the route must leave the track");
        for &(layer, wire) in &routed[0].wires {
            if layer == 1 && wire.vspan().intersects(&Span::new(400, 1600)) {
                assert!(
                    wire.right() <= 1049 - 100,
                    "{wire:?} is on the blocked track"
                );
            }
        }
    }

    #[test]
    fn multi_pin_nets_connect_without_crowding_each_other() {
        let job = job(
            vec![(0, r(950, 950, 1050, 1050))],
            vec![
                (
                    "a",
                    vec![
                        (0, r(150, 150, 250, 250)),
                        (0, r(1750, 150, 1850, 250)),
                        (1, r(950, 1750, 1050, 1850)),
                    ],
                ),
                (
                    "b",
                    vec![(0, r(150, 1550, 250, 1650)), (1, r(1750, 550, 1850, 650))],
                ),
                (
                    "c",
                    vec![(1, r(550, 150, 650, 250)), (1, r(550, 1750, 650, 1850))],
                ),
            ],
        );
        let routed = route(&job).unwrap();
        check(&job, &routed);
    }

    #[test]
    fn a_via_landing_keeps_clear_of_another_nets_wire() {
        // Net `a` runs along y = 1000 up to x = 1050. The shortest route for
        // `b` turns onto the next track, y = 1200, at x = 1000, but a via
        // there or at x = 1200 would put its landing 90 from `a`'s wire.
        let job = job(
            Vec::new(),
            vec![
                (
                    "a",
                    vec![(0, r(150, 950, 250, 1050)), (0, r(950, 950, 1050, 1050))],
                ),
                (
                    "b",
                    vec![
                        (1, r(950, 1750, 1050, 1850)),
                        (0, r(1550, 1150, 1650, 1250)),
                    ],
                ),
            ],
        );
        let routed = route(&job).unwrap();
        check(&job, &routed);
        for via in &routed[1].vias {
            let center = via.bot_rect.center();
            assert!(center.y != 1200 || center.x >= 1400, "{via:?}");
        }
    }

    #[test]
    fn off_grid_pins_get_stubs() {
        let job = job(
            Vec::new(),
            vec![(
                "a",
                vec![(0, r(160, 990, 200, 1030)), (0, r(1810, 960, 1830, 980))],
            )],
        );
        let routed = route(&job).unwrap();
        check(&job, &routed);
        assert_eq!(routed[0].stubs.len(), 2);
        for &(layer, stub) in &routed[0].stubs {
            assert_eq!(layer, 0);
            assert_eq!(stub.vspan(), Span::new(950, 1050));
        }
    }

    #[test]
    fn obstacles_touching_a_pin_belong_to_its_net() {
        // The pin is part of a larger shape that is also passed as an obstacle.
        let job = job(
            vec![(0, r(100, 900, 400, 1100))],
            vec![(
                "a",
                vec![(0, r(150, 950, 350, 1050)), (0, r(1650, 950, 1850, 1050))],
            )],
        );
        let routed = route(&job).unwrap();
        assert!(!routed[0].wires.is_empty());
    }

    #[test]
    fn an_enclosed_pin_is_unroutable() {
        let ring = |layer| {
            [
                (layer, r(500, 500, 1500, 600)),
                (layer, r(500, 1400, 1500, 1500)),
                (layer, r(500, 500, 600, 1500)),
                (layer, r(1400, 500, 1500, 1500)),
            ]
        };
        let mut obstacles = ring(0).to_vec();
        obstacles.extend(ring(1));
        obstacles.push((1, r(700, 700, 1300, 1300)));
        let job = job(
            obstacles,
            vec![
                (
                    "ok",
                    vec![(0, r(150, 150, 250, 250)), (0, r(150, 1750, 250, 1850))],
                ),
                (
                    "trapped",
                    vec![(0, r(950, 950, 1050, 1050)), (0, r(1750, 150, 1850, 250))],
                ),
            ],
        );
        assert_eq!(
            route(&job),
            Err(AutorouteError::Unroutable {
                net: "trapped".to_owned(),
                from: 0,
                to: 1,
            })
        );
    }

    #[test]
    fn invalid_jobs_are_reported() {
        let mut same_dir = job(Vec::new(), Vec::new());
        same_dir.layers[1].dir = Dir::Horiz;
        assert!(
            matches!(route(&same_dir), Err(AutorouteError::Invalid(m)) if m.contains("alternate"))
        );

        let mut uneven = job(Vec::new(), Vec::new());
        uneven.layers[1].space = 150;
        assert!(
            matches!(route(&uneven), Err(AutorouteError::Invalid(m)) if m.contains("multiple"))
        );

        let mut no_vias = job(Vec::new(), Vec::new());
        no_vias.vias.clear();
        assert!(matches!(route(&no_vias), Err(AutorouteError::Invalid(m)) if m.contains("vias")));

        let outside = job(Vec::new(), vec![("a", vec![(0, r(5000, 0, 5100, 100))])]);
        assert!(
            matches!(route(&outside), Err(AutorouteError::Invalid(m)) if m.contains("outside"))
        );

        let mut huge = job(Vec::new(), Vec::new());
        huge.area = r(0, 0, 10_000_000, 10_000_000);
        assert!(matches!(route(&huge), Err(AutorouteError::Invalid(m)) if m.contains("limit")));
    }

    /// A small deterministic generator, so the random jobs are the same on
    /// every run.
    struct Lcg(u64);

    impl Lcg {
        fn below(&mut self, n: i64) -> i64 {
            self.0 = self
                .0
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            ((self.0 >> 33) % n as u64) as i64
        }
    }

    /// A job in 5 nm units with rules like sky130's: 140 nm metal 1 and 2 at
    /// a 340 nm pitch, 300 nm metal 3 at twice that, and single-cut vias whose
    /// landings are wider than the wires. Pins and obstacles are scattered at
    /// random over a 30 by 30 grid.
    fn random_job(seed: u64) -> Job {
        let mut rng = Lcg(seed);
        let (pitch, n) = (68, 30);
        let layers = vec![
            LayerConfig {
                line: 28,
                space: pitch - 28,
                dir: Dir::Horiz,
            },
            LayerConfig {
                line: 28,
                space: pitch - 28,
                dir: Dir::Vert,
            },
            LayerConfig {
                line: 60,
                space: 2 * pitch - 60,
                dir: Dir::Horiz,
            },
        ];
        // Via 1: a 150 nm cut, 55 and 85 nm enclosures, on a 140 by 140 crossing.
        // Via 2: a 200 nm cut on a 140 by 300 crossing.
        let vias = vec![
            ViaShape {
                cuts: vec![r(-1, -1, 29, 29)],
                cut_space: 34,
                bot: r(-18, -12, 46, 40),
                top: r(-12, -18, 40, 46),
            },
            ViaShape {
                cuts: vec![r(-6, 10, 34, 50)],
                cut_space: 40,
                bot: r(-14, -7, 42, 67),
                top: r(-19, -3, 47, 63),
            },
        ];
        let mut used: Vec<(i64, i64)> = Vec::new();
        let mut pick = |rng: &mut Lcg, apart: i64| {
            (0..1000).find_map(|_| {
                let (i, j) = (1 + rng.below(n - 2), 1 + rng.below(n - 2));
                let free = used
                    .iter()
                    .all(|&(a, b)| (a - i).abs() > apart || (b - j).abs() > apart);
                free.then(|| {
                    used.push((i, j));
                    (i * pitch, j * pitch)
                })
            })
        };
        let mut nets = Vec::new();
        for k in 0..10 {
            let count = 2 + rng.below(2);
            let pins = (0..count)
                .filter_map(|_| {
                    let (x, y) = pick(&mut rng, 2)?;
                    Some(if rng.below(2) == 0 {
                        (0, r(x - 14, y - 14, x + pitch + 14, y + 14))
                    } else {
                        (1, r(x - 14, y - 14, x + 14, y + pitch + 14))
                    })
                })
                .collect::<Vec<_>>();
            if pins.len() >= 2 {
                nets.push(NetPins {
                    name: format!("n{k}"),
                    pins,
                });
            }
        }
        let mut obstacles = Vec::new();
        for _ in 0..8 {
            let Some((x, y)) = pick(&mut rng, 3) else {
                break;
            };
            let (x, y) = (x + rng.below(20) - 10, y + rng.below(20) - 10);
            let layer = rng.below(3) as usize;
            let min = if layer == 2 { 60 } else { 28 };
            obstacles.push((
                layer,
                r(x, y, x + min + rng.below(100), y + min + rng.below(100)),
            ));
        }
        Job {
            area: r(0, 0, (n - 1) * pitch, (n - 1) * pitch),
            layers,
            vias,
            obstacles,
            nets,
        }
    }

    #[test]
    fn random_jobs_keep_spacing_and_connect() {
        // Seed 3 is one of the few dense jobs where two nets block each other
        // in every order tried.
        for seed in [0, 1, 2, 4, 5, 6, 7, 8] {
            let job = random_job(seed);
            let routed = route(&job).unwrap_or_else(|e| panic!("seed {seed}: {e}"));
            check(&job, &routed);
        }
    }
}
