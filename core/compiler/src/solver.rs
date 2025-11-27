use std::collections::HashMap;

use indexmap::{IndexMap, IndexSet};
use itertools::{Either, Itertools};
use nalgebra::DVector;
use nalgebra_sparse::{CooMatrix, CsrMatrix};
use rayon::prelude::*;
use serde::{Deserialize, Serialize};

const EPSILON: f64 = 1e-10;
const ROUND_STEP: f64 = 1e-3;
const INV_ROUND_STEP: f64 = 1. / ROUND_STEP;

#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq, Serialize, Deserialize, Ord, PartialOrd)]
pub struct Var(u64);
use crate::spqr::SpqrFactorization;

#[derive(Clone, Default)]
pub struct Solver {
    next_var: u64,
    next_constraint: ConstraintId,
    constraints: Vec<SolverConstraint>,
    solved_vars: IndexMap<Var, f64>,
    inconsistent_constraints: IndexSet<ConstraintId>,
}

pub fn substitute_expr(table: &IndexMap<Var, f64>, expr: &mut LinearExpr) {
    let (l, r): (Vec<f64>, Vec<_>) = expr.coeffs.iter().partition_map(|a @ (coeff, var)| {
        if let Some(s) = table.get(var) {
            Either::Left(coeff * s)
        } else {
            Either::Right(*a)
        }
    });
    expr.coeffs = r;
    expr.constant += l.into_iter().reduce(|a, b| a + b).unwrap_or(0.);
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
        self.next_var += 1;
        var
    }

    /// Returns true if all variables have been solved.
    pub fn fully_solved(&self) -> bool {
        self.solved_vars.len() == self.next_var as usize
    }

    pub fn force_solution(&mut self) {
        while !self.fully_solved() {
            // Find any unsolved variable and constrain it to equal 0.
            let v = (0..self.next_var)
                .find(|&i| !self.solved_vars.contains_key(&Var(i)))
                .unwrap();
            self.constrain_eq0(LinearExpr::from(Var(v)));
            self.solve();
        }
    }

    #[inline]
    pub fn inconsistent_constraints(&self) -> &IndexSet<ConstraintId> {
        &self.inconsistent_constraints
    }

    pub fn unsolved_vars(&self) -> IndexSet<Var> {
        IndexSet::from_iter((0..self.next_var).map(Var).filter(|&v| !self.is_solved(v)))
    }

    /// Constrains the value of `expr` to 0.
    /// TODO: Check if added constraints conflict with existing solution.
    pub fn constrain_eq0(&mut self, expr: LinearExpr) -> ConstraintId {
        let id = self.next_constraint;
        self.next_constraint += 1;
        let mut constraint = SolverConstraint { id, expr };
        substitute_expr(&self.solved_vars, &mut constraint.expr);
        self.constraints.push(constraint);
        id
    }

    /// Solves for as many variables as possible and substitutes their values into existing constraints.
    /// Deletes constraints that no longer contain unsolved variables.
    pub fn solve(&mut self) {
        let n_vars = self.next_var as usize;
        if n_vars == 0 || self.constraints.is_empty() {
            return;
        }

        let old_triplets: Vec<(usize, usize, f64)> = self
            .constraints
            .par_iter()
            .enumerate()
            .flat_map(|(c_index, c)| {
                c.expr
                    .coeff_vec(n_vars)
                    .into_par_iter()
                    .enumerate()
                    .filter_map(move |(v_index, v)| {
                        if v != 0.0 {
                            Some((c_index, v_index, v))
                        } else {
                            None
                        }
                    })
            })
            .collect();

        let mut used = vec![false; n_vars];
        for (_, v_index, _) in &old_triplets {
            used[*v_index] = true;
        }

        let mut var_map = vec![usize::MAX; n_vars];
        let mut rev_var_map = Vec::with_capacity(n_vars);
        let mut new_index = 0;

        for (old_index, &is_used) in used.iter().enumerate() {
            if is_used {
                var_map[old_index] = new_index;
                rev_var_map.push(old_index);
                new_index += 1;
            }
        }

        let n = new_index;
        let m = self.constraints.len();

        let triplets: Vec<(usize, usize, f64)> = old_triplets
            .into_par_iter()
            .map(|(c_index, v_index, val)| {
                let new_index = var_map[v_index];
                (c_index, new_index, val)
            })
            .collect();

        let temp_b: Vec<f64> = self
            .constraints
            .par_iter()
            .map(|c| -c.expr.constant)
            .collect();

        let b = DVector::from_iterator(self.constraints.len(), temp_b);

        let temp_a_constraind_ids: Vec<u64> = self.constraints.par_iter().map(|c| c.id).collect();
        let a_constraint_ids = Vec::from_iter(temp_a_constraind_ids);

        let mut a_coo = CooMatrix::new(m, n);
        for (i, j, v) in triplets.iter() {
            a_coo.push(*i, *j, *v);
        }
        let a_sparse: CsrMatrix<f64> = CsrMatrix::from(&a_coo);

        let qr = SpqrFactorization::from_triplets(&triplets, m, n).unwrap();

        let rank = qr.rank();

        let x = qr.solve(&b).unwrap();

        let residual = &b - &a_sparse * &x;

        let tolerance = 1e-10;

        for i in 0..residual.nrows() {
            let r = residual[(i, 0)];
            if r.abs() > tolerance {
                self.inconsistent_constraints.insert(a_constraint_ids[i]);
            }
        }

        let ones_vector: DVector<f64> = DVector::from_element(n - rank, 1.0);
        let null_space_components = qr.get_nspace_sparse().unwrap() * ones_vector;

        let par_solved_vars: HashMap<Var, f64> = (0..n)
            .into_par_iter()
            .filter(|&i| null_space_components[i] < tolerance)
            .map(|i| {
                let actual_val = x[(i, 0)];
                let actual_var = rev_var_map[i];
                (Var(actual_var as u64), round(actual_val))
            })
            .collect();

        self.solved_vars.extend(par_solved_vars);

        for constraint in self.constraints.iter_mut() {
            substitute_expr(&self.solved_vars, &mut constraint.expr);
            if constraint.expr.coeffs.is_empty()
                && approx::relative_ne!(constraint.expr.constant, 0., epsilon = EPSILON)
            {
                self.inconsistent_constraints.insert(constraint.id);
            }
        }
        self.constraints
            .retain(|constraint| !constraint.expr.coeffs.is_empty());
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
}

pub type ConstraintId = u64;

#[derive(Debug, Clone, Serialize, Deserialize, PartialOrd, PartialEq)]
pub struct SolverConstraint {
    pub id: ConstraintId,
    pub expr: LinearExpr,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialOrd, PartialEq)]
pub struct LinearExpr {
    pub coeffs: Vec<(f64, Var)>,
    pub constant: f64,
}

impl LinearExpr {
    pub fn coeff_vec(&self, n_vars: usize) -> Vec<f64> {
        let mut out = vec![0.; n_vars];
        for (val, var) in &self.coeffs {
            out[var.0 as usize] += *val;
        }
        out
    }

    pub fn add(lhs: impl Into<LinearExpr>, rhs: impl Into<LinearExpr>) -> Self {
        lhs.into() + rhs.into()
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
    }
}
