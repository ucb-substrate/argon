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
    }

    pub fn rowspace_vecs(&mut self) -> Vec<Vec<(f64, Var)>> {
        if self.unsolved_vars.is_empty() || self.constraints.is_empty() {
            return Vec::new();
        }
        self.constraint_components()
            .into_iter()
            .flat_map(|component| self.rowspace_component_vecs(&component.vars, &component.constraints))
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
        let a = DMatrix::from(CsMatrix::from_triplet(constraints.len(), n_vars, &i, &j, &val));
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
        let a = DMatrix::from(CsMatrix::from_triplet(constraints.len(), n_vars, &i, &j, &val));
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
                            if self.unsolved_vars.contains(&next_var) && visited_vars.insert(next_var)
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
}
