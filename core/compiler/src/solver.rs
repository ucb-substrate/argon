use std::collections::HashMap;

use indexmap::{IndexMap, IndexSet};
use itertools::{Either, Itertools};
use nalgebra::DVector;
use nalgebra_sparse::{CooMatrix, CsrMatrix};
use rayon::iter::IntoParallelRefIterator;
use rayon::prelude::*;
use serde::{Deserialize, Serialize};

const EPSILON: f64 = 1e-8;
const ROUND_STEP: f64 = 0.1;
const INV_ROUND_STEP: f64 = 1. / ROUND_STEP;

#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq, Serialize, Deserialize, Ord, PartialOrd)]
pub struct Var(u64);
use crate::spqr_eigen::SpqrFactorization;

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
        // Snapshot unsolved variables before solving.
        let unsolved_vars = self.unsolved_vars.clone();
        let n_vars = unsolved_vars.len();
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

        let x = qr.solve(&b).unwrap();

        let residual = &b - &a_sparse * &x;

        let tolerance = 1e-10;

        for i in 0..residual.nrows() {
            let r = residual[(i, 0)];
            if r.abs() > tolerance {
                self.inconsistent_constraints.insert(a_constraint_ids[i]);
            }
        }

        let null_space_mags = qr.get_free_indices().unwrap();

        let par_solved_vars: HashMap<Var, f64> = (0..n)
            .into_par_iter()
            .filter(|&i| null_space_mags[i] < tolerance)
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
                self.inconsistent_constraints.insert(*id);
            }
        }
        self.constraints
            .retain(|_, constraint| !constraint.coeffs.is_empty());
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

    #[test]
    fn linear_constraints_solved_correctly_two() {
        let mut solver = Solver::new();
        let x = solver.new_var();
        let y = solver.new_var();
        solver.constrain_eq0(LinearExpr {
            coeffs: vec![(1., x)],
            constant: -3.,
        });
        solver.constrain_eq0(LinearExpr {
            coeffs: vec![(1., y)],
            constant: -5.,
        });
        solver.solve();
        assert_relative_eq!(*solver.solved_vars.get(&x).unwrap(), 3., epsilon = EPSILON);
        assert_relative_eq!(*solver.solved_vars.get(&y).unwrap(), 5., epsilon = EPSILON);
    }

    #[test]
    fn linear_constraints_solved_correctly_three() {
        let mut solver = Solver::new();
        let x = solver.new_var();
        let y = solver.new_var();
        solver.constrain_eq0(LinearExpr {
            coeffs: vec![(1., x), (1., y)],
            constant: -3.,
        });
        solver.constrain_eq0(LinearExpr {
            coeffs: vec![(1., y)],
            constant: -5.,
        });
        solver.solve();
        assert_relative_eq!(*solver.solved_vars.get(&x).unwrap(), -2., epsilon = EPSILON);
        assert_relative_eq!(*solver.solved_vars.get(&y).unwrap(), 5., epsilon = EPSILON);
    }

    #[test]
    fn linear_constraints_solved_correctly_four() {
        let mut solver = Solver::new();
        let x = solver.new_var();
        let y = solver.new_var();
        let z = solver.new_var();
        solver.constrain_eq0(LinearExpr {
            coeffs: vec![(2., x), (1., y), (1., z)],
            constant: -3.,
        });
        solver.constrain_eq0(LinearExpr {
            coeffs: vec![(1., x), (2., y), (1., z)],
            constant: -5.,
        });
        solver.constrain_eq0(LinearExpr {
            coeffs: vec![(1., x), (1., y), (2., z)],
            constant: -8.,
        });
        solver.solve();
        assert_relative_eq!(*solver.solved_vars.get(&x).unwrap(), -1., epsilon = EPSILON);
        assert_relative_eq!(*solver.solved_vars.get(&y).unwrap(), 1., epsilon = EPSILON);
        assert_relative_eq!(*solver.solved_vars.get(&z).unwrap(), 4., epsilon = EPSILON);
    }

    //big matrix
    #[test]
    fn big_num_stab_test() {
        //make a graph laplacian for grid, want to see if QR = A
        fn generate_bad_value() -> f64 {
            use rand::Rng;
            use rand::seq::SliceRandom;
            use rand::thread_rng;
            let mut rng = thread_rng();
            let exponents = [-8.0, 0.0, 8.0];
            let exp = exponents.choose(&mut rng).unwrap();
            let magnitude = 10.0_f64.powf(*exp);
            let sign = if rng.gen_bool(0.5) { 1.0 } else { -1.0 };
            let noise = rng.gen_range(0.5..1.5);
            magnitude * sign * noise
        }
        let size = 25;

        use nalgebra::DMatrix;
        let mut a_dense = DMatrix::zeros(size * size, size * size);
        let mut triplets: Vec<(usize, usize, f64)> = Vec::new();

        for i in 0..(size) {
            for j in 0..(size) {
                let curr_node = i * size + j;
                let mut bad_val = generate_bad_value();
                a_dense[(curr_node, curr_node)] = bad_val;
                triplets.push((curr_node, curr_node, bad_val));
                if (j + 1) < size {
                    bad_val = generate_bad_value();
                    a_dense[(curr_node, curr_node + 1)] = bad_val;
                    triplets.push((curr_node, curr_node + 1, bad_val));
                    bad_val = generate_bad_value();
                    a_dense[(curr_node + 1, curr_node)] = bad_val;
                    triplets.push((curr_node + 1, curr_node, bad_val));
                }
                if (i + 1) < size {
                    let next_row_node = size * (i + 1) + j;
                    bad_val = generate_bad_value();
                    a_dense[(curr_node, next_row_node)] = bad_val;
                    triplets.push((curr_node, next_row_node, bad_val));
                    bad_val = generate_bad_value();
                    a_dense[(next_row_node, curr_node)] = bad_val;
                    triplets.push((next_row_node, curr_node, bad_val));
                }
            }
        }
        let qr = SpqrFactorization::from_triplets(&triplets, size * size, size * size).unwrap();
        let q = qr.qa_matrix().unwrap();
        let r = qr.ra_matrix().unwrap();

        let p_indices = qr.permutation_a().unwrap();

        let mut ap_dense = DMatrix::zeros(size * size, size * size);
        for (new_col_idx, &old_col_idx) in p_indices.iter().enumerate() {
            let col = a_dense.column(old_col_idx);
            ap_dense.set_column(new_col_idx, &col);
        }
        let a_norm = ap_dense.norm();
        let resid = ap_dense - q * r;
        let err = resid.norm();
        let relative_err = err / a_norm;
        assert!(relative_err < 1e-12, "not num stab");
    }
}
