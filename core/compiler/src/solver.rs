use approx::{relative_eq, relative_ne};
use indexmap::{IndexMap, IndexSet};
use itertools::{Either, Itertools, multiunzip};
use nalgebra::{CsMatrix, DMatrix, DVector};
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;

const EPSILON: f64 = 1e-8;
const ROUND_STEP: f64 = 0.1;
const INV_ROUND_STEP: f64 = 1. / ROUND_STEP;

#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq, Serialize, Deserialize, Ord, PartialOrd)]
pub struct Var(u64);

#[derive(Clone, Default)]
pub struct Solver {
    next_var: u64,
    next_constraint: ConstraintId,
    constraints: IndexMap<ConstraintId, LinearExpr>,
    var_to_constraints: IndexMap<Var, IndexSet<ConstraintId>>,
    // Solved and unsolved vars are separate to reduce overhead of many solved variables.
    solved_vars: IndexMap<Var, f64>,
    unsolved_vars: IndexSet<Var>,
    updated_vars: IndexSet<Var>,
    back_substitute_stack: Vec<ConstraintId>,
    inconsistent_constraints: IndexSet<ConstraintId>,
    invalid_rounding: IndexSet<Var>,
    // Per-`solve()` scratch for the sparse elimination pre-pass (`eliminate_definitional`).
    // `elim_worklist` holds constraints to (re)examine for a small pivot; `substitutions`
    // records `var = expr` definitions for variables eliminated via a 2-variable
    // constraint, resolved into numbers afterwards by `resolve_substitutions`. Both are
    // cleared at the start of each elimination pass, so they hold no state between solves.
    elim_worklist: VecDeque<ConstraintId>,
    substitutions: Vec<(Var, LinearExpr)>,
}

fn round(x: f64) -> f64 {
    (x * INV_ROUND_STEP).round() * ROUND_STEP
}

impl Solver {
    pub fn new() -> Self {
        Default::default()
    }

    pub fn new_var(&mut self) -> Var {
        let var = Var(self.next_var);
        self.unsolved_vars.insert(var);
        self.next_var += 1;
        var
    }

    /// Returns true if all variables have been solved.
    pub fn fully_solved(&self) -> bool {
        self.unsolved_vars.is_empty()
    }

    pub fn force_solution(&mut self) {
        while !self.fully_solved() {
            // Find any unsolved variable and constrain it to equal 0.
            let v = self.unsolved_vars.first().unwrap();
            self.constrain_eq0(LinearExpr::from(*v));
            self.solve();
        }
    }

    #[inline]
    pub fn inconsistent_constraints(&self) -> &IndexSet<ConstraintId> {
        &self.inconsistent_constraints
    }

    #[inline]
    pub fn updated_vars(&self) -> &IndexSet<Var> {
        &self.updated_vars
    }

    #[inline]
    pub fn clear_updated_vars(&mut self) {
        self.updated_vars.clear()
    }

    #[inline]
    pub fn invalid_rounding(&self) -> &IndexSet<Var> {
        &self.invalid_rounding
    }

    pub fn unsolved_vars(&self) -> &IndexSet<Var> {
        &self.unsolved_vars
    }

    pub fn solve_var(&mut self, var: Var, val: f64) {
        let old = self.solved_vars.insert(var, val);
        if old.is_none() {
            self.updated_vars.insert(var);
        }
        self.unsolved_vars.swap_remove(&var);
    }

    /// Constrains the value of `expr` to 0.
    /// TODO: Check if added constraints conflict with existing solution.
    pub fn constrain_eq0(&mut self, expr: LinearExpr) -> ConstraintId {
        let id = self.next_constraint;
        self.next_constraint += 1;
        for (_, var) in &expr.coeffs {
            self.var_to_constraints.entry(*var).or_default().insert(id);
        }
        self.constraints.insert(id, expr);
        // Use explicit stack in heap-allocated vector to avoid stack overflow.
        self.back_substitute_stack.push(id);
        while !self.back_substitute_stack.is_empty() {
            self.try_back_substitute();
        }
        id
    }

    // Tries to back substitute using the given [`ConstraintId`].
    pub fn try_back_substitute(&mut self) {
        // If coefficient length is not 1, do nothing.
        if let Some(id) = self.back_substitute_stack.pop()
            && let Some(constraint) = self.constraints.get_mut(&id)
        {
            constraint.simplify(&self.solved_vars);
            if constraint.coeffs.is_empty()
                && !relative_eq!(constraint.constant, 0., epsilon = EPSILON)
            {
                self.inconsistent_constraints.insert(id);
                self.constraints.swap_remove(&id);
                return;
            }
            if constraint.coeffs.len() != 1 {
                return;
            }
            // If constraint solves a variable, insert it into the solved vars and traverse all
            // constraints involving the variable.
            let (coeff, var) = constraint.coeffs[0];
            let val = -constraint.constant / coeff;
            if let Some(old_val) = self.solved_vars.get(&var) {
                if relative_ne!(*old_val, val, epsilon = EPSILON) {
                    self.inconsistent_constraints.insert(id);
                }
            } else {
                let rounded_val = round(val);
                if relative_ne!(val, rounded_val, epsilon = EPSILON) {
                    self.invalid_rounding.insert(var);
                }
                self.solve_var(var, rounded_val);
            }
            self.constraints.swap_remove(&id);
            for constraint in self
                .var_to_constraints
                .get(&var)
                .into_iter()
                .flatten()
                .copied()
                .collect_vec()
            {
                self.back_substitute_stack.push(constraint);
            }
        }
    }

    /// Solves for as many variables as possible and substitutes their values into existing constraints.
    /// Deletes constraints that no longer contain unsolved variables.
    ///
    /// Constraints should be simplified before this function is invoked.
    pub fn solve(&mut self) {
        if self.unsolved_vars.is_empty() || self.constraints.is_empty() {
            return;
        }
        // Sparsity-exploiting pre-pass: peel off variables that are uniquely defined
        // by a constraint of size <= 2 (generalizing 1-variable back-substitution),
        // shrinking the system before the dense SVD below. Variables eliminated via a
        // 2-variable constraint are expressed in terms of another variable and recorded
        // in `self.substitutions`; their numeric values are recovered by
        // `resolve_substitutions` once the remaining (irreducible) core has been solved.
        // For systems whose constraints are all <= 2 variables (e.g. the coupled ring in
        // `bench_constraints`) this resolves everything in O(n) and the SVD never runs;
        // for a genuinely dense block it is a no-op and behaviour is identical to before.
        self.eliminate_definitional();

        for component in self.constraint_components() {
            self.solve_component(&component.vars, &component.constraints);
        }
        for (id, constraint) in self.constraints.iter_mut() {
            constraint.simplify(&self.solved_vars);
            if constraint.coeffs.is_empty()
                && approx::relative_ne!(constraint.constant, 0., epsilon = EPSILON)
            {
                self.inconsistent_constraints.insert(*id);
            }
        }
        self.constraints
            .retain(|_, constraint| !constraint.coeffs.is_empty());

        self.resolve_substitutions();
    }

    /// Sparse elimination pre-pass. Repeatedly examines constraints with at most two
    /// variables and uses each to eliminate one of its variables, substituting it out
    /// of the (few) other constraints that mention it. Because a size-2 constraint
    /// expresses a variable as (one variable + constant), substitution replaces one
    /// term with one term and so never increases any constraint's variable count: the
    /// system only shrinks, and the pass runs in O(nnz). Constraints with > 2 variables
    /// are left untouched for the dense path in `solve_component`.
    fn eliminate_definitional(&mut self) {
        self.substitutions.clear();
        self.elim_worklist.clear();
        self.elim_worklist.extend(self.constraints.keys().copied());
        while let Some(id) = self.elim_worklist.pop_front() {
            let Some(constraint) = self.constraints.get_mut(&id) else {
                continue;
            };
            constraint.simplify(&self.solved_vars);
            coalesce_terms(constraint);
            let len = constraint.coeffs.len();
            let constant = constraint.constant;
            match len {
                0 => {
                    if relative_ne!(constant, 0., epsilon = EPSILON) {
                        self.inconsistent_constraints.insert(id);
                    }
                    self.remove_constraint(id);
                }
                1 => self.eliminate_unary(id),
                2 => self.eliminate_binary(id),
                _ => {}
            }
        }
    }

    /// Removes a constraint from the system and unlinks it from `var_to_constraints`.
    fn remove_constraint(&mut self, id: ConstraintId) {
        if let Some(constraint) = self.constraints.swap_remove(&id) {
            for (_, var) in &constraint.coeffs {
                if let Some(set) = self.var_to_constraints.get_mut(var) {
                    set.swap_remove(&id);
                }
            }
        }
    }

    /// Re-queues every still-live constraint mentioning `var`; they may have just
    /// shrunk to a size the pre-pass can act on.
    fn requeue_neighbors(&mut self, var: Var) {
        let neighbors: Vec<ConstraintId> = self
            .var_to_constraints
            .get(&var)
            .into_iter()
            .flatten()
            .copied()
            .collect();
        self.elim_worklist.extend(neighbors);
    }

    /// Grounds the single variable of a 1-variable constraint (post-simplify). The
    /// 1-variable analogue of `eliminate_binary`; mirrors `try_back_substitute`.
    fn eliminate_unary(&mut self, id: ConstraintId) {
        let (coeff, var) = self.constraints[&id].coeffs[0];
        let val = -self.constraints[&id].constant / coeff;
        self.remove_constraint(id);
        self.requeue_neighbors(var);
        self.assign_var(var, val);
    }

    /// Eliminates one variable of a 2-variable constraint (post-simplify) by expressing
    /// it in terms of the other and substituting it out of every other constraint that
    /// mentions it. Records the definition in `substitutions` for later resolution.
    fn eliminate_binary(&mut self, id: ConstraintId) {
        let (c0, v0) = self.constraints[&id].coeffs[0];
        let (c1, v1) = self.constraints[&id].coeffs[1];
        let constant = self.constraints[&id].constant;
        // Pivot on the larger-magnitude coefficient (partial-pivoting analogue).
        let ((a, v), (cw, w)) = if c0.abs() >= c1.abs() {
            ((c0, v0), (c1, v1))
        } else {
            ((c1, v1), (c0, v0))
        };
        if a.abs() <= EPSILON || v == w {
            return;
        }
        // From `a*v + cw*w + constant = 0`: v = (-cw/a) * w + (-constant/a).
        let v_expr = LinearExpr {
            coeffs: vec![(-cw / a, w)],
            constant: -constant / a,
        };
        self.remove_constraint(id);
        let neighbors: Vec<ConstraintId> = self
            .var_to_constraints
            .get(&v)
            .into_iter()
            .flatten()
            .copied()
            .collect();
        for nid in neighbors {
            let Some(constraint) = self.constraints.get_mut(&nid) else {
                continue;
            };
            substitute_var(constraint, v, &v_expr);
            self.var_to_constraints.entry(w).or_default().insert(nid);
            self.elim_worklist.push_back(nid);
        }
        self.var_to_constraints.swap_remove(&v);
        self.unsolved_vars.swap_remove(&v);
        self.substitutions.push((v, v_expr));
    }

    /// Recovers numeric values for variables eliminated by `eliminate_binary`. Walks
    /// `substitutions` in reverse (reverse-topological) order: by the time each entry is
    /// reached, every variable its expression depends on is either solved by the core
    /// SVD / back-substitution or resolved earlier in this walk. A variable whose
    /// expression does not become ground belongs to an under-determined component; its
    /// defining constraint is restored (a row-equivalent of the pivot row that was
    /// removed) so the under-constrained diagnostics in `rowspace_vecs` are unchanged.
    fn resolve_substitutions(&mut self) {
        while let Some((var, mut expr)) = self.substitutions.pop() {
            expr.simplify(&self.solved_vars);
            if expr.coeffs.is_empty() {
                self.assign_var(var, expr.constant);
            } else {
                self.unsolved_vars.insert(var);
                let id = self.next_constraint;
                self.next_constraint += 1;
                let mut coeffs = Vec::with_capacity(expr.coeffs.len() + 1);
                coeffs.push((1., var));
                for (c, v) in expr.coeffs {
                    coeffs.push((-c, v));
                }
                let constraint = LinearExpr {
                    coeffs,
                    constant: -expr.constant,
                };
                for (_, v) in &constraint.coeffs {
                    self.var_to_constraints.entry(*v).or_default().insert(id);
                }
                self.constraints.insert(id, constraint);
            }
        }
    }

    /// Rounds `val` to the solver grid, flags off-grid values, and records the solution.
    /// Shared by the elimination pre-pass; matches the rounding contract used by
    /// `try_back_substitute` and `solve_component`.
    fn assign_var(&mut self, var: Var, val: f64) {
        if self.solved_vars.contains_key(&var) {
            return;
        }
        let rounded = round(val);
        if relative_ne!(val, rounded, epsilon = EPSILON) {
            self.invalid_rounding.insert(var);
        }
        self.solve_var(var, rounded);
    }

    pub fn rowspace_vecs(&mut self) -> Vec<Vec<(f64, Var)>> {
        if self.unsolved_vars.is_empty() || self.constraints.is_empty() {
            return Vec::new();
        }
        self.constraint_components()
            .into_iter()
            .flat_map(|component| {
                self.rowspace_component_vecs(&component.vars, &component.constraints)
            })
            .collect()
    }

    pub fn value_of(&self, var: Var) -> Option<f64> {
        self.solved_vars.get(&var).copied()
    }

    pub fn is_solved(&self, var: Var) -> bool {
        self.solved_vars.contains_key(&var)
    }

    pub fn eval_expr(&self, expr: &LinearExpr) -> Option<f64> {
        Some(round(
            expr.coeffs
                .iter()
                .map(|(coeff, var)| self.value_of(*var).map(|val| val * coeff))
                .fold_options(0., |a, b| a + b)?
                + expr.constant,
        ))
    }

    fn solve_component(&mut self, vars: &IndexSet<Var>, constraints: &[ConstraintId]) {
        let n_vars = vars.len();
        if n_vars == 0 || constraints.is_empty() {
            return;
        }
        let var_indices: IndexMap<Var, usize> =
            IndexMap::from_iter(vars.iter().enumerate().map(|(i, var)| (*var, i)));
        let (i, j, val): (Vec<_>, Vec<_>, Vec<_>) =
            multiunzip(constraints.iter().enumerate().flat_map(|(row, id)| {
                self.constraints[id].coeffs.iter().map({
                    let var_indices = &var_indices;
                    move |(coeff, var)| (row, var_indices[var], *coeff)
                })
            }));
        let a = DMatrix::from(CsMatrix::from_triplet(
            constraints.len(),
            n_vars,
            &i,
            &j,
            &val,
        ));
        let b = DVector::from_iterator(
            constraints.len(),
            constraints.iter().map(|id| -self.constraints[id].constant),
        );
        let svd = a.svd(true, true);
        let vt = svd.v_t.as_ref().expect("No V^T matrix");
        let r = svd.rank(EPSILON);
        if r == 0 {
            return;
        }
        let sol = svd.solve(&b, EPSILON).unwrap();

        for (i, var) in vars.iter().enumerate() {
            let recons = (0..r)
                .map(|row| {
                    let coeff = vt[(row, i)];
                    coeff * coeff
                })
                .sum::<f64>();
            if relative_eq!(recons, 1., epsilon = EPSILON) {
                let val = sol[(i, 0)];
                let rounded_val = round(val);
                if relative_ne!(val, rounded_val, epsilon = EPSILON) {
                    self.invalid_rounding.insert(*var);
                }
                self.solve_var(*var, rounded_val);
            }
        }
    }

    fn rowspace_component_vecs(
        &self,
        vars: &IndexSet<Var>,
        constraints: &[ConstraintId],
    ) -> Vec<Vec<(f64, Var)>> {
        let n_vars = vars.len();
        if n_vars == 0 || constraints.is_empty() {
            return Vec::new();
        }
        let var_indices: IndexMap<Var, usize> =
            IndexMap::from_iter(vars.iter().enumerate().map(|(i, var)| (*var, i)));
        let (i, j, val): (Vec<_>, Vec<_>, Vec<_>) =
            multiunzip(constraints.iter().enumerate().flat_map(|(row, id)| {
                self.constraints[id].coeffs.iter().map({
                    let var_indices = &var_indices;
                    move |(coeff, var)| (row, var_indices[var], *coeff)
                })
            }));
        let a = DMatrix::from(CsMatrix::from_triplet(
            constraints.len(),
            n_vars,
            &i,
            &j,
            &val,
        ));
        let svd = a.svd(false, true);
        let vt = svd.v_t.as_ref().expect("No V^T matrix");
        let r = svd.rank(EPSILON);

        (0..r)
            .map(|i| {
                vars.iter()
                    .enumerate()
                    .filter_map(|(j, v)| {
                        let coeff = vt[(i, j)];
                        if relative_ne!(coeff, 0., epsilon = EPSILON) {
                            Some((coeff, *v))
                        } else {
                            None
                        }
                    })
                    .collect::<Vec<_>>()
            })
            .collect()
    }

    fn constraint_components(&self) -> Vec<ConstraintComponent> {
        let mut visited_vars = IndexSet::new();
        let mut visited_constraints = IndexSet::new();
        let mut components = Vec::new();

        for &root_var in &self.unsolved_vars {
            if !visited_vars.insert(root_var) {
                continue;
            }
            let mut queue = VecDeque::from([root_var]);
            let mut vars = IndexSet::from([root_var]);
            let mut constraints = Vec::new();

            while let Some(var) = queue.pop_front() {
                if let Some(var_constraints) = self.var_to_constraints.get(&var) {
                    for &constraint_id in var_constraints {
                        if !self.constraints.contains_key(&constraint_id)
                            || !visited_constraints.insert(constraint_id)
                        {
                            continue;
                        }
                        constraints.push(constraint_id);
                        for &(_, next_var) in &self.constraints[&constraint_id].coeffs {
                            if self.unsolved_vars.contains(&next_var)
                                && visited_vars.insert(next_var)
                            {
                                vars.insert(next_var);
                                queue.push_back(next_var);
                            }
                        }
                    }
                }
            }

            if !constraints.is_empty() {
                components.push(ConstraintComponent { vars, constraints });
            }
        }

        components
    }
}

/// Replaces variable `v` in `expr` with `v_expr` (an expression equal to `v`),
/// coalescing any resulting duplicate terms. Used by the elimination pre-pass.
fn substitute_var(expr: &mut LinearExpr, v: Var, v_expr: &LinearExpr) {
    let Some(pos) = expr.coeffs.iter().position(|(_, var)| *var == v) else {
        return;
    };
    let (cv, _) = expr.coeffs.remove(pos);
    for &(c, var) in &v_expr.coeffs {
        if let Some(term) = expr.coeffs.iter_mut().find(|(_, ev)| *ev == var) {
            term.0 += cv * c;
        } else {
            expr.coeffs.push((cv * c, var));
        }
    }
    expr.constant += cv * v_expr.constant;
}

/// Merges duplicate-variable terms and drops near-zero coefficients in place, so a
/// constraint's `coeffs.len()` faithfully reflects its number of distinct variables
/// (e.g. a chain closing onto itself collapses `x + x` to a single `2x` term, and a
/// cancelling substitution collapses to an empty/contradiction constraint).
fn coalesce_terms(expr: &mut LinearExpr) {
    let mut merged: Vec<(f64, Var)> = Vec::with_capacity(expr.coeffs.len());
    for &(c, v) in &expr.coeffs {
        if let Some(term) = merged.iter_mut().find(|(_, mv)| *mv == v) {
            term.0 += c;
        } else {
            merged.push((c, v));
        }
    }
    merged.retain(|(c, _)| relative_ne!(*c, 0., epsilon = EPSILON));
    expr.coeffs = merged;
}

struct ConstraintComponent {
    vars: IndexSet<Var>,
    constraints: Vec<ConstraintId>,
}

pub type ConstraintId = u64;

#[derive(Debug, Clone, Serialize, Deserialize, PartialOrd, PartialEq)]
pub struct LinearExpr {
    pub coeffs: Vec<(f64, Var)>,
    pub constant: f64,
}

impl LinearExpr {
    pub fn add(lhs: impl Into<LinearExpr>, rhs: impl Into<LinearExpr>) -> Self {
        lhs.into() + rhs.into()
    }

    /// Substitutes variables in `table` and removes entries with coefficient 0.
    pub fn simplify(&mut self, table: &IndexMap<Var, f64>) {
        let (l, r): (Vec<f64>, Vec<_>) = self.coeffs.iter().partition_map(|a @ (coeff, var)| {
            if relative_eq!(*coeff, 0., epsilon = EPSILON) {
                return Either::Left(0.);
            }
            if let Some(s) = table.get(var) {
                Either::Left(coeff * s)
            } else {
                Either::Right(*a)
            }
        });
        self.coeffs = r;
        self.constant += l.into_iter().reduce(|a, b| a + b).unwrap_or(0.);
    }
}

impl std::ops::Add<f64> for LinearExpr {
    type Output = Self;
    fn add(self, rhs: f64) -> Self::Output {
        Self {
            coeffs: self.coeffs,
            constant: self.constant + rhs,
        }
    }
}

impl std::ops::Sub<f64> for LinearExpr {
    type Output = Self;
    fn sub(self, rhs: f64) -> Self::Output {
        Self {
            coeffs: self.coeffs,
            constant: self.constant - rhs,
        }
    }
}

impl std::ops::Add<LinearExpr> for LinearExpr {
    type Output = Self;
    fn add(self, rhs: LinearExpr) -> Self::Output {
        Self {
            coeffs: self.coeffs.into_iter().chain(rhs.coeffs).collect(),
            constant: self.constant + rhs.constant,
        }
    }
}

impl std::ops::Sub<LinearExpr> for LinearExpr {
    type Output = Self;
    fn sub(self, rhs: LinearExpr) -> Self::Output {
        Self {
            coeffs: self
                .coeffs
                .into_iter()
                .chain(rhs.coeffs.into_iter().map(|(c, v)| (-c, v)))
                .collect(),
            constant: self.constant - rhs.constant,
        }
    }
}

impl std::ops::Sub<&LinearExpr> for LinearExpr {
    type Output = Self;
    fn sub(self, rhs: &LinearExpr) -> Self::Output {
        Self {
            coeffs: self
                .coeffs
                .into_iter()
                .chain(rhs.coeffs.iter().map(|(c, v)| (-c, *v)))
                .collect(),
            constant: self.constant - rhs.constant,
        }
    }
}

impl std::ops::Mul<f64> for LinearExpr {
    type Output = Self;
    fn mul(self, rhs: f64) -> Self::Output {
        Self {
            coeffs: self.coeffs.into_iter().map(|(c, v)| (c * rhs, v)).collect(),
            constant: self.constant * rhs,
        }
    }
}

impl std::ops::Div<f64> for LinearExpr {
    type Output = Self;
    fn div(self, rhs: f64) -> Self::Output {
        Self {
            coeffs: self.coeffs.into_iter().map(|(c, v)| (c / rhs, v)).collect(),
            constant: self.constant / rhs,
        }
    }
}

impl From<Var> for LinearExpr {
    fn from(value: Var) -> Self {
        Self {
            coeffs: vec![(1., value)],
            constant: 0.,
        }
    }
}

impl From<f64> for LinearExpr {
    fn from(value: f64) -> Self {
        Self {
            coeffs: vec![],
            constant: value,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use approx::assert_relative_eq;

    #[test]
    fn linear_constraints_solved_correctly() {
        let mut solver = Solver::new();
        let x = solver.new_var();
        let y = solver.new_var();
        let z = solver.new_var();
        solver.constrain_eq0(LinearExpr {
            coeffs: vec![(1., x)],
            constant: -5.,
        });
        solver.constrain_eq0(LinearExpr {
            coeffs: vec![(1., y), (-1., x)],
            constant: 0.,
        });
        solver.solve();
        assert_relative_eq!(*solver.solved_vars.get(&x).unwrap(), 5., epsilon = EPSILON);
        assert_relative_eq!(*solver.solved_vars.get(&y).unwrap(), 5., epsilon = EPSILON);
        assert!(!solver.solved_vars.contains_key(&z));
        assert!(!solver.unsolved_vars.contains(&x));
        assert!(!solver.unsolved_vars.contains(&y));
        assert!(solver.unsolved_vars.contains(&z));
    }

    fn c(coeffs: Vec<(f64, Var)>, constant: f64) -> LinearExpr {
        LinearExpr { coeffs, constant }
    }

    /// A consistent ring of 2-variable constraints with no 1-variable starting point
    /// (the minimal `bench_constraints` shape). The pre-pass must break the cycle by
    /// substitution, then telescope to a 1-variable closure that grounds the chain.
    #[test]
    fn three_cycle_determined() {
        let mut s = Solver::new();
        let a = s.new_var();
        let b = s.new_var();
        let d = s.new_var();
        s.constrain_eq0(c(vec![(1., a), (-1., b)], -5.)); // a - b = 5
        s.constrain_eq0(c(vec![(1., b), (-1., d)], -5.)); // b - c = 5
        s.constrain_eq0(c(vec![(1., a), (1., d)], -100.)); // a + c = 100
        s.solve();
        assert_relative_eq!(s.value_of(a).unwrap(), 55., epsilon = EPSILON);
        assert_relative_eq!(s.value_of(b).unwrap(), 50., epsilon = EPSILON);
        assert_relative_eq!(s.value_of(d).unwrap(), 45., epsilon = EPSILON);
        assert!(s.inconsistent_constraints().is_empty());
        assert!(s.invalid_rounding().is_empty());
    }

    /// A chain pinned at one end resolves transitively (reverse-topological order).
    #[test]
    fn chain_telescopes() {
        let mut s = Solver::new();
        let a = s.new_var();
        let b = s.new_var();
        let d = s.new_var();
        let e = s.new_var();
        s.constrain_eq0(c(vec![(1., a), (-1., b)], -1.)); // a - b = 1
        s.constrain_eq0(c(vec![(1., b), (-1., d)], -1.)); // b - c = 1
        s.constrain_eq0(c(vec![(1., d), (-1., e)], -1.)); // c - d = 1
        s.constrain_eq0(c(vec![(1., a)], -10.)); // a = 10
        s.solve();
        assert_relative_eq!(s.value_of(a).unwrap(), 10., epsilon = EPSILON);
        assert_relative_eq!(s.value_of(b).unwrap(), 9., epsilon = EPSILON);
        assert_relative_eq!(s.value_of(d).unwrap(), 8., epsilon = EPSILON);
        assert_relative_eq!(s.value_of(e).unwrap(), 7., epsilon = EPSILON);
        assert!(s.inconsistent_constraints().is_empty());
    }

    /// An under-determined pair: neither variable is pinned, and the row space still
    /// reports one constrained direction (the pre-pass must re-materialize its
    /// constraint after failing to ground the eliminated variable).
    #[test]
    fn underdetermined_pair_unsolved() {
        let mut s = Solver::new();
        let a = s.new_var();
        let b = s.new_var();
        s.constrain_eq0(c(vec![(1., a), (-1., b)], 0.)); // a - b = 0
        s.solve();
        assert!(s.value_of(a).is_none());
        assert!(s.value_of(b).is_none());
        assert!(s.unsolved_vars().contains(&a));
        assert!(s.unsolved_vars().contains(&b));
        assert_eq!(s.rowspace_vecs().len(), 1);
    }

    /// Two contradictory 2-variable constraints: substitution drives one to `0 = -5`.
    #[test]
    fn inconsistent_pair() {
        let mut s = Solver::new();
        let a = s.new_var();
        let b = s.new_var();
        s.constrain_eq0(c(vec![(1., a), (-1., b)], 0.)); // a - b = 0
        s.constrain_eq0(c(vec![(1., a), (-1., b)], -5.)); // a - b = 5
        s.solve();
        assert!(!s.inconsistent_constraints().is_empty());
    }

    /// An over-constrained cycle whose differences sum to a nonzero constant.
    #[test]
    fn inconsistent_cycle() {
        let mut s = Solver::new();
        let a = s.new_var();
        let b = s.new_var();
        let d = s.new_var();
        s.constrain_eq0(c(vec![(1., a), (-1., b)], -5.)); // a - b = 5
        s.constrain_eq0(c(vec![(1., b), (-1., d)], -5.)); // b - c = 5
        s.constrain_eq0(c(vec![(1., d), (-1., a)], -5.)); // c - a = 5  (loop sum = 15)
        s.solve();
        assert!(!s.inconsistent_constraints().is_empty());
    }

    /// A duplicate constraint inside a coupled core is dropped as redundant (not flagged
    /// inconsistent), and the remaining system still solves.
    #[test]
    fn redundant_in_core() {
        let mut s = Solver::new();
        let a = s.new_var();
        let b = s.new_var();
        s.constrain_eq0(c(vec![(1., a), (-1., b)], 0.)); // a - b = 0
        s.constrain_eq0(c(vec![(1., a), (-1., b)], 0.)); // a - b = 0 (duplicate)
        s.constrain_eq0(c(vec![(1., a), (1., b)], -10.)); // a + b = 10
        s.solve();
        assert_relative_eq!(s.value_of(a).unwrap(), 5., epsilon = EPSILON);
        assert_relative_eq!(s.value_of(b).unwrap(), 5., epsilon = EPSILON);
        assert!(s.inconsistent_constraints().is_empty());
    }

    /// A value reached only through elimination + resolution that lands off the 0.1
    /// grid is flagged in `invalid_rounding`.
    #[test]
    fn off_grid_cycle() {
        let mut s = Solver::new();
        let a = s.new_var();
        let b = s.new_var();
        let d = s.new_var();
        s.constrain_eq0(c(vec![(1., a), (-1., b)], 0.)); // a - b = 0
        s.constrain_eq0(c(vec![(1., b), (-1., d)], 0.)); // b - c = 0
        s.constrain_eq0(c(vec![(1., a), (1., b), (1., d)], -1.)); // a + b + c = 1  => 1/3 each
        s.solve();
        assert!(!s.invalid_rounding().is_empty());
    }

    /// Variables eliminated in an earlier `solve()` (before the closing constraint
    /// exists) are resolved once a later constraint pins the chain.
    #[test]
    fn incremental_resolution() {
        let mut s = Solver::new();
        let a = s.new_var();
        let b = s.new_var();
        let d = s.new_var();
        s.constrain_eq0(c(vec![(1., a), (-1., b)], -1.)); // a - b = 1
        s.constrain_eq0(c(vec![(1., b), (-1., d)], -1.)); // b - c = 1
        s.solve(); // under-determined so far
        assert!(s.value_of(a).is_none());
        s.constrain_eq0(c(vec![(1., a)], -10.)); // a = 10
        s.solve();
        assert_relative_eq!(s.value_of(a).unwrap(), 10., epsilon = EPSILON);
        assert_relative_eq!(s.value_of(b).unwrap(), 9., epsilon = EPSILON);
        assert_relative_eq!(s.value_of(d).unwrap(), 8., epsilon = EPSILON);
    }

    /// A fully-coupled 3x3 block has no size-<=2 pivot: the pre-pass is a no-op and the
    /// dense SVD path solves it, exactly as before.
    #[test]
    fn dense_block_falls_back() {
        let mut s = Solver::new();
        let a = s.new_var();
        let b = s.new_var();
        let d = s.new_var();
        s.constrain_eq0(c(vec![(1., a), (1., b), (1., d)], -6.)); // a + b + c = 6
        s.constrain_eq0(c(vec![(1., a), (2., b), (3., d)], -14.)); // a + 2b + 3c = 14
        s.constrain_eq0(c(vec![(1., a), (3., b), (6., d)], -25.)); // a + 3b + 6c = 25
        s.solve();
        assert_relative_eq!(s.value_of(a).unwrap(), 1., epsilon = EPSILON);
        assert_relative_eq!(s.value_of(b).unwrap(), 2., epsilon = EPSILON);
        assert_relative_eq!(s.value_of(d).unwrap(), 3., epsilon = EPSILON);
        assert!(s.inconsistent_constraints().is_empty());
    }
}
