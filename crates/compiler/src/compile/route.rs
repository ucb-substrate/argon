//! The `std::layout::route_tracks` native: routes nets on uniform tracks with
//! the greedy track router, in units of the manufacturing grid.

use argon_greedy_router::LayerConfig;
use argon_greedy_router::autoroute::{self, Job, NetPins, RoutedNet, ViaShape};
use argon_greedy_router::geom::{Dir, Point, Rect as GridRect};

use super::ops::{ErrorAt, OpFailure, OpResult};
use super::*;
use crate::solver::{LinearExpr, Solver};
use crate::via_array::{ViaArrayQuery, max_via_array};

/// Coordinates larger than this many grid units are rejected, which keeps the
/// router's integer arithmetic far from overflow.
const MAX_UNITS: f64 = (1u64 << 50) as f64;

/// The routed nets, in grid units.
pub(super) struct Routed {
    pub(super) nets: Vec<RoutedNet>,
    pub(super) grid: f64,
}

/// Routes the nets of a `route_tracks` call, waiting until every coordinate
/// and length in its arguments is solved.
pub(super) fn route_tracks(solver: &Solver, args: &[&Value]) -> OpResult<Routed> {
    let &[area, layers, vias, obstacles, nets] = args else {
        return Err(invalid_type());
    };
    let mut unsolved = SmallVec::new();
    for arg in args {
        collect_unsolved(solver, arg, &mut unsolved);
    }
    if !unsolved.is_empty() {
        return Err(OpFailure::Wait(unsolved));
    }
    let units = Units {
        solver,
        grid: solver.grid(),
    };

    let area = units.rect(area)?;
    let layers = items(layers)?
        .enumerate()
        .map(|(i, layer)| {
            let [width, space, horizontal] = tuple(layer)?;
            let Value::Bool(horizontal) = horizontal else {
                return Err(invalid_type());
            };
            Ok(LayerConfig {
                line: units.positive(width, || format!("the width of layer {i}"))?,
                space: units.positive(space, || format!("the space of layer {i}"))?,
                dir: if *horizontal { Dir::Horiz } else { Dir::Vert },
            })
        })
        .collect::<OpResult<Vec<_>>>()?;
    if let Some(first) = layers.first() {
        for (i, layer) in layers.iter().enumerate() {
            if layer.pitch() % first.pitch() != 0 {
                return Err(routing(format!(
                    "the pitch of layer {i} ({}) must be a multiple of the pitch of layer 0 ({})",
                    units.length(layer.pitch()),
                    units.length(first.pitch())
                )));
            }
        }
    }
    let vias = items(vias)?
        .enumerate()
        .map(|(i, via)| {
            let [size, space, bot_enc, top_enc] = tuple(via)?;
            let enclosure = |value: &Value, which: &str| -> OpResult<(i64, i64)> {
                let [short, long] = tuple(value)?;
                let what = || format!("the {which} enclosure of via {i}");
                Ok((
                    units.non_negative(short, what)?,
                    units.non_negative(long, what)?,
                ))
            };
            Ok(ViaRule {
                size: units.positive(size, || format!("the cut size of via {i}"))?,
                space: units.positive(space, || format!("the cut space of via {i}"))?,
                bot_enc: enclosure(bot_enc, "bottom")?,
                top_enc: enclosure(top_enc, "top")?,
            })
        })
        .collect::<OpResult<Vec<_>>>()?;
    let obstacles = items(obstacles)?
        .map(|obstacle| units.layer_rect(obstacle))
        .collect::<OpResult<Vec<_>>>()?;
    let nets = items(nets)?
        .map(|net| {
            let [name, pins] = tuple(net)?;
            let Value::String(name) = name else {
                return Err(invalid_type());
            };
            Ok(NetPins {
                name: name.clone(),
                pins: items(pins)?
                    .map(|pin| units.layer_rect(pin))
                    .collect::<OpResult<Vec<_>>>()?,
            })
        })
        .collect::<OpResult<Vec<_>>>()?;

    if !layers.is_empty() && vias.len() + 1 != layers.len() {
        return Err(routing(format!(
            "{} routing layers need {} vias, one between each adjacent pair, found {}",
            layers.len(),
            layers.len() - 1,
            vias.len()
        )));
    }
    let job = Job {
        area,
        vias: vias
            .iter()
            .enumerate()
            .map(|(lower, rule)| via_shape(&layers, lower, rule, units.grid))
            .collect(),
        layers,
        obstacles,
        nets,
    };
    let nets = autoroute::route(&job).map_err(|error| routing(error.to_string()))?;
    Ok(Routed {
        nets,
        grid: units.grid,
    })
}

/// A via's design rules, in grid units. Enclosures are `(short, long)`.
struct ViaRule {
    size: i64,
    space: i64,
    bot_enc: (i64, i64),
    top_enc: (i64, i64),
}

/// The cuts and landings of the via between layer `lower` and the one above
/// it, relative to the crossing of their wires. `max_via_array` picks the
/// cut array for wires that run on through the crossing, with each long
/// enclosure along its layer's wires; the array is centered on the crossing.
fn via_shape(layers: &[LayerConfig], lower: usize, rule: &ViaRule, grid: f64) -> ViaShape {
    let (w, h) = autoroute::crossing_size(layers, lower);
    let margin = 2 * (w + h + rule.size + rule.space)
        + 4 * [rule.bot_enc, rule.top_enc]
            .iter()
            .map(|&(short, long)| short.max(long))
            .max()
            .unwrap_or(0);
    let float = |v: i64| v as f64 * grid;
    let wire = |dir: Dir| match dir {
        Dir::Horiz => [float(-margin), 0., float(w + margin), float(h)],
        Dir::Vert => [0., float(-margin), float(w), float(h + margin)],
    };
    let ext_dir = |dir: Dir| match dir {
        Dir::Horiz => 1,
        Dir::Vert => 2,
    };
    let (bot_dir, top_dir) = (layers[lower].dir, layers[lower + 1].dir);
    let choice = max_via_array(
        &ViaArrayQuery {
            size: float(rule.size),
            space: float(rule.space),
            bot_enc: (float(rule.bot_enc.0), float(rule.bot_enc.1)),
            top_enc: (float(rule.top_enc.0), float(rule.top_enc.1)),
            bot: wire(bot_dir),
            top: wire(top_dir),
            bot_ext_dir: ext_dir(bot_dir),
            top_ext_dir: ext_dir(top_dir),
            longer: false,
            later_wins_ties: false,
        },
        grid,
    );
    let units = |v: f64| (v / grid).round() as i64;
    let (nx, ny) = (choice.0.max(1), choice.1.max(1));
    let pitch = rule.size + rule.space;
    let (aw, ah) = (nx * pitch - rule.space, ny * pitch - rule.space);
    let (ax, ay) = ((w - aw).div_euclid(2), (h - ah).div_euclid(2));
    let rect =
        |x0: i64, y0: i64, x1: i64, y1: i64| GridRect::new(Point::new(x0, y0), Point::new(x1, y1));
    let landing = |ex: i64, ey: i64| rect(ax - ex, ay - ey, ax + aw + ex, ay + ah + ey);
    ViaShape {
        cut_space: rule.space,
        cuts: (0..nx)
            .flat_map(|ix| (0..ny).map(move |iy| (ix, iy)))
            .map(|(ix, iy)| {
                let (x, y) = (ax + ix * pitch, ay + iy * pitch);
                rect(x, y, x + rule.size, y + rule.size)
            })
            .collect(),
        bot: landing(units(choice.2), units(choice.3)),
        top: landing(units(choice.4), units(choice.5)),
    }
}

/// Converts solved values to grid units.
struct Units<'a> {
    solver: &'a Solver,
    grid: f64,
}

impl Units<'_> {
    /// A coordinate, snapped to the grid as drawn geometry is.
    fn coord(&self, expr: &LinearExpr) -> OpResult<i64> {
        let value = self
            .solver
            .eval_expr(expr)
            .expect("arguments are solved before routing");
        let units = (value / self.grid).round();
        if !units.is_finite() || units.abs() > MAX_UNITS {
            return Err(routing(format!("coordinate {value} is too large to route")));
        }
        Ok(units as i64)
    }

    fn rect(&self, value: &Value) -> OpResult<GridRect> {
        let Value::Rect(r) = value else {
            return Err(invalid_type());
        };
        Ok(GridRect::new(
            Point::new(self.coord(&r.x0)?, self.coord(&r.y0)?),
            Point::new(self.coord(&r.x1)?, self.coord(&r.y1)?),
        ))
    }

    /// A `(layer, rect)` tuple.
    fn layer_rect(&self, value: &Value) -> OpResult<(usize, GridRect)> {
        let [layer, rect] = tuple(value)?;
        let Value::Int(layer) = layer else {
            return Err(invalid_type());
        };
        let layer = usize::try_from(*layer)
            .map_err(|_| routing(format!("layer index {layer} is negative")))?;
        Ok((layer, self.rect(rect)?))
    }

    /// A length, which must be a whole number of grid units.
    fn length_units(&self, value: &Value, what: &dyn Fn() -> String) -> OpResult<i64> {
        let Value::Linear(expr) = value else {
            return Err(invalid_type());
        };
        let value = self
            .solver
            .eval_expr_exact(expr)
            .expect("arguments are solved before routing");
        let units = value / self.grid;
        if !units.is_finite() || units.abs() > MAX_UNITS {
            return Err(routing(format!("{} ({value}) is too large", what())));
        }
        if (units - units.round()).abs() > 1e-6 {
            return Err(routing(format!(
                "{} ({value}) is not a multiple of the manufacturing grid ({})",
                what(),
                self.grid
            )));
        }
        Ok(units.round() as i64)
    }

    fn positive(&self, value: &Value, what: impl Fn() -> String) -> OpResult<i64> {
        let units = self.length_units(value, &what)?;
        if units <= 0 {
            return Err(routing(format!(
                "{} must be positive, found {}",
                what(),
                self.length(units)
            )));
        }
        Ok(units)
    }

    fn non_negative(&self, value: &Value, what: impl Fn() -> String) -> OpResult<i64> {
        let units = self.length_units(value, &what)?;
        if units < 0 {
            return Err(routing(format!(
                "{} must not be negative, found {}",
                what(),
                self.length(units)
            )));
        }
        Ok(units)
    }

    /// A length in grid units, in layout coordinates.
    fn length(&self, units: i64) -> f64 {
        crate::tech::snap(units as f64 * self.grid, self.grid)
    }
}

/// Adds the solver variables `value` depends on that are not yet solved.
fn collect_unsolved(solver: &Solver, value: &Value, unsolved: &mut SmallVec<[Var; 4]>) {
    let mut expr = |expr: &LinearExpr| {
        unsolved.extend(
            expr.coeffs
                .iter()
                .map(|(_, var)| *var)
                .filter(|var| !solver.is_solved(*var)),
        )
    };
    match value {
        Value::Linear(e) => expr(e),
        Value::Rect(r) => {
            for e in [&r.x0, &r.y0, &r.x1, &r.y1] {
                expr(e);
            }
        }
        Value::Seq(items) => {
            for item in items.iter() {
                collect_unsolved(solver, item, unsolved);
            }
        }
        Value::Tuple(items) => {
            for item in items.iter() {
                collect_unsolved(solver, item, unsolved);
            }
        }
        _ => {}
    }
}

fn items(value: &Value) -> OpResult<impl Iterator<Item = &Value>> {
    match value {
        Value::Seq(items) => Ok(items.iter()),
        _ => Err(invalid_type()),
    }
}

fn tuple<const N: usize>(value: &Value) -> OpResult<&[Value; N]> {
    match value {
        Value::Tuple(items) => <&[Value; N]>::try_from(&items[..]).map_err(|_| invalid_type()),
        _ => Err(invalid_type()),
    }
}

fn invalid_type() -> OpFailure {
    OpFailure::Error(ExecErrorKind::InvalidType, ErrorAt::Expr)
}

fn routing(message: String) -> OpFailure {
    OpFailure::Error(ExecErrorKind::Routing(message), ErrorAt::Expr)
}
