use std::ops::{Deref, DerefMut, Mul};

use argonc::solver::{LinearExpr, Var};
use indexmap::{IndexMap, IndexSet};

/// Values with magnitude below this are treated as zero when deciding whether
/// a dragged edge still has a free direction to move along.
pub(crate) const EPSILON: f64 = 1e-8;

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

/// Projects `u` onto an orthonormal basis directly.
pub(crate) fn component_in_basis(u: &SparseVec, vecs: &[SparseVec]) -> SparseVec {
    let mut out = SparseVec(IndexMap::new());
    for vector in vecs {
        let weight = dot(u, vector);
        for (var, coefficient) in vector.iter() {
            *out.entry(*var).or_default() += weight * coefficient;
        }
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
        let mut coefficients = IndexMap::new();
        for (coefficient, variable) in value {
            *coefficients.entry(*variable).or_default() += coefficient;
        }
        SparseVec(coefficients)
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

/// Computes a null-space move that changes several edge expressions by the
/// requested distances simultaneously. With one target this implements the
/// edge-drag form of Algorithm 3; multiple targets support corner handles and
/// whole-rectangle translation.
pub(crate) fn drag_delta_multi(
    edges: &[SparseVec],
    rowspace: &[SparseVec],
    unsolved: &IndexSet<Var>,
    deltas: &[f64],
) -> Option<SparseVec> {
    if edges.is_empty() || edges.len() != deltas.len() {
        return None;
    }

    let edges = edges
        .iter()
        .map(|edge| {
            SparseVec(
                edge.iter()
                    .filter(|(var, _)| unsolved.contains(*var))
                    .map(|(var, coeff)| (*var, *coeff))
                    .collect(),
            )
        })
        .collect::<Vec<_>>();
    // Each residual is the target expression's component in the null space of
    // the constraint matrix. Any linear combination remains constraint-safe.
    let residuals = edges
        .iter()
        .map(|edge| remove_component(edge, rowspace))
        .collect::<Vec<_>>();
    combine_drag(&edges, &residuals, deltas)
}

/// Variant used when the compiler supplied the null-space basis directly from
/// sparse QR, avoiding the dense-SVD row-space representation entirely.
pub(crate) fn drag_delta_multi_nullspace(
    edges: &[SparseVec],
    nullspace: &[SparseVec],
    unsolved: &IndexSet<Var>,
    deltas: &[f64],
) -> Option<SparseVec> {
    if edges.is_empty() || edges.len() != deltas.len() {
        return None;
    }
    let edges = edges
        .iter()
        .map(|edge| {
            SparseVec(
                edge.iter()
                    .filter(|(var, _)| unsolved.contains(*var))
                    .map(|(var, coeff)| (*var, *coeff))
                    .collect(),
            )
        })
        .collect::<Vec<_>>();
    let residuals = edges
        .iter()
        .map(|edge| component_in_basis(edge, nullspace))
        .collect::<Vec<_>>();
    combine_drag(&edges, &residuals, deltas)
}

fn combine_drag(edges: &[SparseVec], residuals: &[SparseVec], deltas: &[f64]) -> Option<SparseVec> {
    let gram = edges
        .iter()
        .map(|edge| {
            residuals
                .iter()
                .map(|residual| dot(edge, residual))
                .collect::<Vec<_>>()
        })
        .collect::<Vec<_>>();
    let weights = solve_linear_system(gram, deltas.to_vec())?;

    let mut result = SparseVec(IndexMap::new());
    for (residual, weight) in residuals.iter().zip(weights) {
        for (var, coefficient) in residual.iter() {
            *result.entry(*var).or_default() += coefficient * weight;
        }
    }

    // Rank-deficient systems are allowed when their duplicate equations agree
    // (for example, fixed width makes the x0 and x1 translation equations the
    // same). Still reject a numerically inconsistent requested drag.
    edges
        .iter()
        .zip(deltas)
        .all(|(edge, delta)| (dot(edge, &result) - delta).abs() < EPSILON * (1. + delta.abs()))
        .then_some(result)
}

fn solve_linear_system(mut matrix: Vec<Vec<f64>>, mut rhs: Vec<f64>) -> Option<Vec<f64>> {
    let rows = matrix.len();
    let columns = matrix.first()?.len();
    if rhs.len() != rows || matrix.iter().any(|row| row.len() != columns) {
        return None;
    }

    let mut pivot_row = 0;
    let mut pivots = Vec::new();
    for column in 0..columns {
        let Some(pivot) = (pivot_row..rows).max_by(|&left, &right| {
            matrix[left][column]
                .abs()
                .total_cmp(&matrix[right][column].abs())
        }) else {
            break;
        };
        if matrix[pivot][column].abs() < EPSILON {
            continue;
        }
        matrix.swap(pivot_row, pivot);
        rhs.swap(pivot_row, pivot);

        let scale = matrix[pivot_row][column];
        for value in &mut matrix[pivot_row][column..] {
            *value /= scale;
        }
        rhs[pivot_row] /= scale;
        let pivot_values = matrix[pivot_row][column..].to_vec();

        for row in 0..rows {
            if row == pivot_row {
                continue;
            }
            let factor = matrix[row][column];
            for (entry, pivot_entry) in matrix[row][column..].iter_mut().zip(&pivot_values) {
                *entry -= factor * pivot_entry;
            }
            rhs[row] -= factor * rhs[pivot_row];
        }
        pivots.push(column);
        pivot_row += 1;
        if pivot_row == rows {
            break;
        }
    }

    for row in pivot_row..rows {
        if matrix[row].iter().all(|value| value.abs() < EPSILON) && rhs[row].abs() >= EPSILON {
            return None;
        }
    }

    // Free variables are set to zero. This is sufficient here because the
    // result is a set of weights over null-space residuals; any solution to the
    // Gram system yields the same minimum-norm drag vector.
    let mut solution = vec![0.; columns];
    for (row, column) in pivots.into_iter().enumerate() {
        solution[column] = rhs[row];
    }
    Some(solution)
}

/// Returns an initial condition's value after a drag and whether the drag
/// actually moved it. It exposes unchanged values too, so callers can compare
/// both ends of a rectangle and swap their source values if one edge crossed
/// the other.
pub(crate) fn initial_condition_after_drag(constraint: &LinearExpr, dv: &SparseVec) -> (f64, bool) {
    let delta = dot(&SparseVec::from(constraint), dv);
    (-constraint.constant + delta, delta.abs() >= EPSILON)
}

/// Formats a layout value as an Argon float literal (always containing a `.`),
/// snapped to the solver's 0.1 grid so the written code stays clean and matches
/// what recompilation produces.
pub(crate) fn format_value(v: f64) -> String {
    argonc::compile::format_initial_condition(v)
}

#[cfg(test)]
mod tests {
    use super::*;
    use approx::assert_relative_eq;
    use argonc::solver::Solver;

    /// Orthonormal rowspace basis of the solver's constraint matrix.
    fn rowspace(solver: &mut Solver) -> Vec<SparseVec> {
        solver.rowspace_vecs().iter().map(SparseVec::from).collect()
    }

    /// Coefficient vector of an edge whose position is exactly a single variable.
    fn coeff(var: Var) -> SparseVec {
        SparseVec::from(&LinearExpr::from(var))
    }

    fn drag_one(
        edge: &SparseVec,
        rowspace: &[SparseVec],
        unsolved: &IndexSet<Var>,
        delta: f64,
    ) -> Option<SparseVec> {
        drag_delta_multi(std::slice::from_ref(edge), rowspace, unsolved, &[delta])
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
    fn repeated_dimension_terms_follow_the_full_drag_distance() {
        let mut solver = Solver::new();
        let x0 = solver.new_var();
        let x1 = solver.new_var();
        // This is the coefficient form of
        // `(rect.x0 + rect.x1 + rect.x0 + rect.x1) / 4.`.
        let coordinate = LinearExpr {
            coeffs: vec![(0.25, x0), (0.25, x1), (0.25, x0), (0.25, x1)],
            constant: 0.,
        };
        let coordinate = SparseVec::from(&coordinate);
        let translation = SparseVec([(x0, 12.), (x1, 12.)].into_iter().collect());

        assert_relative_eq!(coordinate[&x0], 0.5, epsilon = 1e-9);
        assert_relative_eq!(coordinate[&x1], 0.5, epsilon = 1e-9);
        assert_relative_eq!(dot(&coordinate, &translation), 12., epsilon = 1e-9);
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

        let dx = drag_one(&coeff(x1), &rs, &unsolved, 3.).unwrap();
        assert_relative_eq!(dot(&coeff(x1), &dx), 3., epsilon = 1e-9);
        assert_relative_eq!(dot(&coeff(x0), &dx), 0., epsilon = 1e-9);
    }

    #[test]
    fn corner_drag_moves_both_requested_edges() {
        let mut solver = Solver::new();
        let x = solver.new_var();
        let y = solver.new_var();
        solver.solve();
        let rs = rowspace(&mut solver);
        let unsolved = solver.unsolved_vars().clone();

        let dv = drag_delta_multi(&[coeff(x), coeff(y)], &rs, &unsolved, &[3.5, -2.25]).unwrap();
        assert_relative_eq!(dot(&coeff(x), &dv), 3.5, epsilon = 1e-9);
        assert_relative_eq!(dot(&coeff(y), &dv), -2.25, epsilon = 1e-9);
    }

    #[test]
    fn body_drag_translates_all_four_free_edges() {
        let mut solver = Solver::new();
        let x0 = solver.new_var();
        let x1 = solver.new_var();
        let y0 = solver.new_var();
        let y1 = solver.new_var();
        solver.solve();
        let rs = rowspace(&mut solver);
        let unsolved = solver.unsolved_vars().clone();
        let edges = [coeff(x0), coeff(x1), coeff(y0), coeff(y1)];

        let dv = drag_delta_multi(&edges, &rs, &unsolved, &[4., 4., -6., -6.]).unwrap();
        for edge in &edges[..2] {
            assert_relative_eq!(dot(edge, &dv), 4., epsilon = 1e-9);
        }
        for edge in &edges[2..] {
            assert_relative_eq!(dot(edge, &dv), -6., epsilon = 1e-9);
        }
    }

    #[test]
    fn body_drag_translates_a_fixed_size_rectangle() {
        let mut solver = Solver::new();
        let x0 = solver.new_var();
        let x1 = solver.new_var();
        let y0 = solver.new_var();
        let y1 = solver.new_var();
        solver.constrain_eq0(LinearExpr {
            coeffs: vec![(1., x1), (-1., x0)],
            constant: -4.,
        });
        solver.constrain_eq0(LinearExpr {
            coeffs: vec![(1., y1), (-1., y0)],
            constant: -7.,
        });
        solver.solve();
        let rs = rowspace(&mut solver);
        let unsolved = solver.unsolved_vars().clone();
        let edges = [coeff(x0), coeff(x1), coeff(y0), coeff(y1)];

        // The paired x and y equations are redundant because width and height
        // are fixed, but they agree and therefore describe a valid translation.
        let dv = drag_delta_multi(&edges, &rs, &unsolved, &[3., 3., -2., -2.]).unwrap();
        for edge in &edges[..2] {
            assert_relative_eq!(dot(edge, &dv), 3., epsilon = 1e-9);
        }
        for edge in &edges[2..] {
            assert_relative_eq!(dot(edge, &dv), -2., epsilon = 1e-9);
        }
    }

    #[test]
    fn body_drag_rejects_conflicting_dependent_edge_requests() {
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

        assert!(drag_delta_multi(&[coeff(x0), coeff(x1)], &rs, &unsolved, &[1., 2.]).is_none());
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
        let dx = drag_one(&coeff(x1), &rs, &unsolved, 2.).unwrap();
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
        let dx = drag_one(&coeff(a), &rs, &unsolved, 2.).unwrap();
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

        assert!(drag_one(&coeff(x1), &rs, &unsolved, 2.).is_none());
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
    fn direct_nullspace_basis_produces_the_same_drag() {
        let mut solver = Solver::new();
        let x = solver.new_var();
        let y = solver.new_var();
        let s = 1. / 2f64.sqrt();
        let nullspace = vec![SparseVec([(x, s), (y, -s)].into_iter().collect())];
        let unsolved = IndexSet::from([x, y]);

        let delta = drag_delta_multi_nullspace(&[coeff(x)], &nullspace, &unsolved, &[2.]).unwrap();
        assert_relative_eq!(*delta.get(&x).unwrap(), 2., epsilon = 1e-9);
        assert_relative_eq!(*delta.get(&y).unwrap(), -2., epsilon = 1e-9);
    }

    #[test]
    fn initial_condition_after_drag_adds_delta() {
        let mut solver = Solver::new();
        let x1 = solver.new_var();
        // Fallback `x1 - 100` pins x1 = 100; a drag moved x1 by +2.5.
        let constraint = LinearExpr {
            coeffs: vec![(1., x1)],
            constant: -100.,
        };
        let dv = SparseVec([(x1, 2.5)].into_iter().collect());
        let (value, changed) = initial_condition_after_drag(&constraint, &dv);
        assert!(changed);
        assert_relative_eq!(value, 102.5, epsilon = 1e-9);
    }

    #[test]
    fn initial_condition_after_drag_marks_unaffected_fallback_unchanged() {
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
        assert_eq!(initial_condition_after_drag(&constraint, &dv), (0., false));
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
