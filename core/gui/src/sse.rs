use std::ops::{Deref, DerefMut, Mul};

use compiler::solver::{LinearExpr, Var};
use indexmap::{IndexMap, IndexSet};

/// Values with magnitude below this are treated as zero when deciding whether
/// a dragged edge still has a free direction to move along.
const EPSILON: f64 = 1e-8;

#[derive(Clone, Debug)]
pub struct SparseVec(pub(crate) IndexMap<Var, f64>);

impl Deref for SparseVec {
    type Target = IndexMap<Var, f64>;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl DerefMut for SparseVec {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

/// Removes all components in the directions of `vecs` from `u`.
///
/// Assumes `vecs` is an orthonormal set of vectors.
pub(crate) fn remove_component(u: &SparseVec, vecs: &[SparseVec]) -> SparseVec {
    let mut out = u.clone();
    for v in vecs {
        let dot = dot(u, v);
        v.iter()
            .for_each(|(var, &c)| *out.entry(*var).or_default() -= dot * c);
    }
    out
}

pub(crate) fn dot(a: &SparseVec, b: &SparseVec) -> f64 {
    a.iter()
        .map(|(var, &c)| c * *b.get(var).unwrap_or(&0.))
        .sum()
}

impl From<&Vec<(f64, Var)>> for SparseVec {
    fn from(value: &Vec<(f64, Var)>) -> Self {
        SparseVec(value.iter().map(|(c, v)| (*v, *c)).collect())
    }
}

impl From<&LinearExpr> for SparseVec {
    fn from(value: &LinearExpr) -> Self {
        Self::from(&value.coeffs)
    }
}

impl Mul<f64> for SparseVec {
    type Output = Self;
    fn mul(mut self, rhs: f64) -> Self::Output {
        self.iter_mut().for_each(|(_, v)| *v *= rhs);
        self
    }
}

/// Converts a pixel-space mouse delta into the signed distance the dragged edge
/// should travel along its normal, expressed in layout units.
///
/// `pixel_delta` is the cumulative mouse movement in pixels since the drag
/// began, `normal` is the dragged edge's unit normal in layout space (`(1, 0)`
/// for the left/right edges, `(0, 1)` for the top/bottom edges), and `scale` is
/// the number of pixels per layout unit.
///
/// The y component is negated because layout space has y pointing up while the
/// screen has y pointing down (see [`super::editor::canvas::LayoutCanvas`]'s
/// `px_to_layout`). This computes the `n̂ᵀd` term of Algorithm 3.
pub(crate) fn edge_drag_distance(pixel_delta: (f32, f32), normal: (f32, f32), scale: f32) -> f32 {
    let layout_dx = pixel_delta.0 / scale;
    let layout_dy = -pixel_delta.1 / scale;
    normal.0 * layout_dx + normal.1 * layout_dy
}

/// Computes the change in the constraint variable vector (Δ𝑥) produced by
/// dragging an edge a signed distance `delta` along its normal while keeping
/// every linear constraint satisfied. This implements Algorithm 3 (Solution
/// Space Exploration) from the manuscript.
///
/// - `edge` is the coefficient vector `c` of the dragged edge's position (the
///   linear combination of variables that determines where the edge sits).
/// - `rowspace` is an orthonormal basis `V` of the rowspace of the constraint
///   matrix `A`, as returned by [`compiler::solver::Solver::rowspace_vecs`].
/// - `unsolved` is the set of variables that are still free to move. Locked
///   variables have a fixed value and must never change, so the dragged edge's
///   coefficient vector is restricted to `unsolved` before projecting.
///
/// Returns `None` when the edge is fully constrained — i.e. its coefficient
/// vector lies entirely in the rowspace of `A` (or references only locked
/// variables) — in which case dragging it cannot move anything.
pub(crate) fn drag_delta(
    edge: &SparseVec,
    rowspace: &[SparseVec],
    unsolved: &IndexSet<Var>,
    delta: f64,
) -> Option<SparseVec> {
    // Restrict the coefficient vector to variables that can actually move.
    let edge = SparseVec(
        edge.iter()
            .filter(|(var, _)| unsolved.contains(*var))
            .map(|(var, coeff)| (*var, *coeff))
            .collect(),
    );
    // r = c − proj_V(c): the component of the edge's coefficient vector lying in
    // the null space of A. Moving the solution along r keeps every constraint
    // satisfied while changing the dragged edge as directly as possible.
    let r = remove_component(&edge, rowspace);
    // cᵀr = ‖r‖², since c = proj_V(c) + r and proj_V(c) ⊥ r.
    let denom = dot(&edge, &r);
    if denom.abs() < EPSILON {
        return None;
    }
    Some(r * (delta / denom))
}

/// Given a used fallback (initial-condition) constraint of the form
/// `expr - value` — so the currently pinned value is `-constraint.constant`
/// when `expr` has no constant term — and the solution-space move `dv` produced
/// by a drag, returns the new value to write into the source, or `None` if the
/// drag does not move this fallback's variables.
///
/// Persisting a drag means rewriting each affected initial condition to this new
/// value, so recompilation reproduces the dragged layout instead of snapping
/// back.
pub(crate) fn updated_initial_condition(constraint: &LinearExpr, dv: &SparseVec) -> Option<f64> {
    let delta = dot(&SparseVec::from(constraint), dv);
    if delta.abs() < EPSILON {
        return None;
    }
    Some(-constraint.constant + delta)
}

/// Formats a layout value as an Argon float literal (always containing a `.`),
/// snapped to the solver's 0.1 grid so the written code stays clean and matches
/// what recompilation produces.
pub(crate) fn format_value(v: f64) -> String {
    // `+ 0.0` collapses a possible `-0.0` to `0.0`.
    let snapped = (v * 10.0).round() / 10.0 + 0.0;
    let s = format!("{snapped}");
    if s.contains('.') { s } else { format!("{s}.") }
}

#[cfg(test)]
mod tests {
    use super::*;
    use approx::assert_relative_eq;
    use compiler::solver::Solver;

    /// Orthonormal rowspace basis of the solver's constraint matrix.
    fn rowspace(solver: &mut Solver) -> Vec<SparseVec> {
        solver.rowspace_vecs().iter().map(SparseVec::from).collect()
    }

    /// Coefficient vector of an edge whose position is exactly a single variable.
    fn coeff(var: Var) -> SparseVec {
        SparseVec::from(&LinearExpr::from(var))
    }

    #[test]
    fn edge_drag_distance_picks_axis_and_flips_y() {
        // Right edge: normal +x. Only horizontal motion matters, no sign flip.
        assert_relative_eq!(edge_drag_distance((10., 7.), (1., 0.), 2.), 5.);
        // Top edge: normal +y. Screen y points down, so dragging the mouse up
        // (negative pixel y) must move the edge up (positive layout y).
        assert_relative_eq!(edge_drag_distance((3., -10.), (0., 1.), 2.), 5.);
        // Dragging the mouse down moves a +y edge down.
        assert_relative_eq!(edge_drag_distance((3., 10.), (0., 1.), 2.), -5.);
    }

    #[test]
    fn free_edge_moves_only_itself() {
        // Two totally unconstrained edge variables (e.g. a rect with no
        // dimensions): dragging one edge should move only that edge.
        let mut solver = Solver::new();
        let x0 = solver.new_var();
        let x1 = solver.new_var();
        solver.solve();
        let rs = rowspace(&mut solver);
        let unsolved = solver.unsolved_vars().clone();

        let dx = drag_delta(&coeff(x1), &rs, &unsolved, 3.).unwrap();
        assert_relative_eq!(dot(&coeff(x1), &dx), 3., epsilon = 1e-9);
        assert_relative_eq!(dot(&coeff(x0), &dx), 0., epsilon = 1e-9);
    }

    #[test]
    fn width_locked_rect_translates() {
        // x1 - x0 = 4 fixes the width but leaves the absolute position free.
        let mut solver = Solver::new();
        let x0 = solver.new_var();
        let x1 = solver.new_var();
        solver.constrain_eq0(LinearExpr {
            coeffs: vec![(1., x1), (-1., x0)],
            constant: -4.,
        });
        solver.solve();
        let rs = rowspace(&mut solver);
        let unsolved = solver.unsolved_vars().clone();

        // Dragging the right edge by 2 slides the whole rect by 2 so the width
        // is preserved: both edges move together.
        let dx = drag_delta(&coeff(x1), &rs, &unsolved, 2.).unwrap();
        assert_relative_eq!(dot(&coeff(x1), &dx), 2., epsilon = 1e-9);
        assert_relative_eq!(dot(&coeff(x0), &dx), 2., epsilon = 1e-9);
        // Width (x1 - x0) is unchanged.
        let width_change = dot(&coeff(x1), &dx) - dot(&coeff(x0), &dx);
        assert_relative_eq!(width_change, 0., epsilon = 1e-9);
    }

    #[test]
    fn aligned_edges_move_together() {
        // Edge `a` of one rect is aligned to edge `b` of another: a = b.
        let mut solver = Solver::new();
        let a = solver.new_var();
        let b = solver.new_var();
        solver.constrain_eq0(LinearExpr {
            coeffs: vec![(1., a), (-1., b)],
            constant: 0.,
        });
        solver.solve();
        let rs = rowspace(&mut solver);
        let unsolved = solver.unsolved_vars().clone();

        // Dragging `a` drags `b` with it so the alignment constraint holds.
        let dx = drag_delta(&coeff(a), &rs, &unsolved, 2.).unwrap();
        assert_relative_eq!(dot(&coeff(a), &dx), 2., epsilon = 1e-9);
        assert_relative_eq!(dot(&coeff(b), &dx), 2., epsilon = 1e-9);
    }

    #[test]
    fn fully_constrained_edge_cannot_move() {
        // Both edges pinned to constants: there is no free direction to drag.
        let mut solver = Solver::new();
        let x0 = solver.new_var();
        let x1 = solver.new_var();
        solver.constrain_eq0(LinearExpr {
            coeffs: vec![(1., x0)],
            constant: -1.,
        });
        solver.constrain_eq0(LinearExpr {
            coeffs: vec![(1., x1)],
            constant: -5.,
        });
        solver.solve();
        let rs = rowspace(&mut solver);
        let unsolved = solver.unsolved_vars().clone();

        assert!(drag_delta(&coeff(x1), &rs, &unsolved, 2.).is_none());
    }

    #[test]
    fn remove_component_extracts_null_space() {
        // Project (1, 0) off the orthonormal basis vector (1, 1)/√2; the
        // remaining null-space component should be (1/2, -1/2).
        let mut solver = Solver::new();
        let x = solver.new_var();
        let y = solver.new_var();
        let s = 1. / 2f64.sqrt();
        let basis = SparseVec([(x, s), (y, s)].into_iter().collect());
        let u = SparseVec([(x, 1.), (y, 0.)].into_iter().collect());
        let r = remove_component(&u, &[basis]);
        assert_relative_eq!(*r.get(&x).unwrap(), 0.5, epsilon = 1e-9);
        assert_relative_eq!(*r.get(&y).unwrap(), -0.5, epsilon = 1e-9);
    }

    #[test]
    fn updated_initial_condition_adds_drag_delta() {
        let mut solver = Solver::new();
        let x1 = solver.new_var();
        // Fallback `x1 - 100` pins x1 = 100; a drag moved x1 by +2.5.
        let constraint = LinearExpr {
            coeffs: vec![(1., x1)],
            constant: -100.,
        };
        let dv = SparseVec([(x1, 2.5)].into_iter().collect());
        assert_relative_eq!(
            updated_initial_condition(&constraint, &dv).unwrap(),
            102.5,
            epsilon = 1e-9
        );
    }

    #[test]
    fn updated_initial_condition_ignores_unaffected_fallback() {
        let mut solver = Solver::new();
        let x0 = solver.new_var();
        let x1 = solver.new_var();
        // Fallback on x0; the drag moved only x1, so x0's initial condition is
        // untouched.
        let constraint = LinearExpr {
            coeffs: vec![(1., x0)],
            constant: 0.,
        };
        let dv = SparseVec([(x1, 5.)].into_iter().collect());
        assert!(updated_initial_condition(&constraint, &dv).is_none());
    }

    #[test]
    fn format_value_snaps_to_grid_and_is_float_literal() {
        assert_eq!(format_value(100.0), "100.");
        assert_eq!(format_value(0.0), "0.");
        assert_eq!(format_value(150.37), "150.4"); // snapped to the 0.1 grid
        assert_eq!(format_value(-0.04), "0."); // snaps to 0, never "-0"
        assert_eq!(format_value(42.5), "42.5");
    }
}
