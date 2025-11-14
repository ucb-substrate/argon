use std::collections::HashMap;

use approx::relative_eq;
use indexmap::{IndexMap, IndexSet};
use itertools::{Either, Itertools};
use nalgebra::{DMatrix, DVector};
use serde::{Deserialize, Serialize};

//use crate::solver::IntoFaer;
const EPSILON: f64 = 1e-10;
const ROUND_STEP: f64 = 1e-3;
const INV_ROUND_STEP: f64 = 1. / ROUND_STEP;

#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq, Serialize, Deserialize, Ord, PartialOrd)]
pub struct Var(u64);
use crate::spqr_wrapper::SpqrFactorization;

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

    pub fn solve(&mut self) {
        let method: u64 = 2;
        use std::time::Instant;
        let start_time = Instant::now();
        if method == 0 {
            self.solve_svd();
        } else if method == 1 {
            self.solve_qr();
        } else if method == 2 {
            self.solve_qr_sparse();
        }
        let elapsed_time = start_time.elapsed();

        use std::fs::OpenOptions;
        use std::io::Write;

        let time_str = format!("time taken: {:?}\n", elapsed_time);
        let mut file = OpenOptions::new()
            .append(true)
            .create(true)
            .open("time_count.txt")
            .unwrap();
        file.write_all(time_str.as_bytes()).unwrap();
    }

    pub fn solve_qr_sparse(&mut self) {
        use std::time::Instant;
        let start_time = Instant::now();
        use nalgebra::{DMatrix, DVector};
        use nalgebra_sparse::{CooMatrix, CsrMatrix};
        use rayon::prelude::*;

        let _tolerance = 0.03;
        let n_vars = self.next_var as usize;
        if n_vars == 0 || self.constraints.is_empty() {
            return;
        }
        
        let triplets: Vec<(usize, usize, f64)> = self
            .constraints
            .par_iter()
            .enumerate()
            .map(|(c_index, c)| {
                c.expr.coeffs.iter().map(move |(coeff, var)| {
                    (c_index, var.0 as usize, *coeff)
                })
                .collect::<Vec<_>>()
            })
            .flatten()
            .collect();

        let m = self.constraints.iter().count();
        let n = n_vars;
        
        let temp_b: Vec<f64> = self
            .constraints
            .iter()
            .map(|c| -c.expr.constant)
            .collect();

        let b = DVector::from_iterator(self.constraints.len(), temp_b);
        
        let a_constraint_ids: Vec<u64> = self.constraints.iter().map(|c| c.id).collect();

        let mut a_coo = CooMatrix::new(m, n);
        for (i, j, v) in triplets.iter() {
            a_coo.push(*i, *j, *v);
        }
        let a_sparse: CsrMatrix<f64> = CsrMatrix::from(&a_coo);

        let qr = SpqrFactorization::new_from_triplets(&triplets, m, n).unwrap();

        let rank = qr.rank();
        let E = qr.permutation();

        let x = if m >= n {
            qr.solve_regular(&b).unwrap()
        } else {
            qr.solve_underconstrained_from_triplets(&triplets, &b, m, n)
                .unwrap()
        };

        let residual = &b - &a_sparse * &x;

        let tolerance = 1e-10;

        for i in 0..residual.nrows() {
            let r = residual[(i, 0)];
            if r.abs() > tolerance {
                self.inconsistent_constraints.insert(a_constraint_ids[i]);
            }
        }

        let forward = E.unwrap();

        let determ_var_idx: Vec<usize> = forward[0..rank].to_vec();
        
        for &r in determ_var_idx.iter() {
            let actual_val = x[(r, 0)];
            self.solved_vars.insert(Var(r as u64), actual_val);
        }
        
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


        // TODO: Consider creating option to time or not
        let elapsed_time = start_time.elapsed();

        use std::fs::OpenOptions;
        use std::io::Write;

        let time_str = format!(
            "time taken on {row}x{col}: {:?}\n",
            elapsed_time,
            row = m,
            col = n
        );
        let mut file = OpenOptions::new()
            .append(true)
            .create(true)
            .open("spqr_time_n_size.txt")
            .unwrap();
        file.write_all(time_str.as_bytes()).unwrap();
    }

    pub fn solve_qr(&mut self) {
        use faer::Mat;

        use nalgebra::{DMatrix, DVector};

        use rayon::prelude::*;

        let tolerance = 0.03;
        let n_vars = self.next_var as usize;
        if n_vars == 0 || self.constraints.is_empty() {
            return;
        }
        
        let temp_a: Vec<f64> = self
            .constraints
            .par_iter()
            .flat_map(|c| c.expr.coeff_vec(n_vars))
            .collect();

        let a: DMatrix<f64> = DMatrix::from_row_iterator(self.constraints.len(), n_vars, temp_a);
        
        let temp_b: Vec<f64> = self
            .constraints
            .iter()
            .map(|c| -c.expr.constant)
            .collect();

        let b = DVector::from_iterator(self.constraints.len(), temp_b);
        
        let a_constraint_ids: Vec<u64> = self.constraints.iter().map(|c| c.id).collect();

        let A = Mat::from_fn(a.nrows(), a.ncols(), |i, j| a[(i, j)]);
        let B = Mat::from_fn(b.nrows(), b.ncols(), |i, j| b[(i, j)]);

        let m = A.nrows();
        let n = A.ncols();

        print!("mmmmmmm {:?}", m);
        print!("nnnnnn {:?}", n);

        use faer::linalg::solvers::ColPivQr;
        //use faer::sparse::linalg::solvers::Qr;

        let qr: ColPivQr<f64> = ColPivQr::new(A.as_ref());
        let R = qr.R();
        let P = qr.P();
        let _Q = qr.compute_Q();
        
        let rank_A = R
            .diagonal()
            .column_vector()
            .iter()
            .filter(|&&val| val.abs() > tolerance)
            .count();

        use faer::prelude::SolveLstsq;

        let x = if m >= n {
            qr.solve_lstsq(&B)
        } else {
            let At = A.transpose();
            let AAt = &A * At;
            let qr_normal = ColPivQr::new(AAt.as_ref());
            let y = qr_normal.solve_lstsq(B.as_ref());
            At * y
        };

        let residual = &B - &A * &x;

        let tolerance = 1e-10;

        for i in 0..residual.nrows() {
            let r = residual[(i, 0)];
            if r.abs() > tolerance {
                self.inconsistent_constraints.insert(a_constraint_ids[i]);
            }
        }
        let (forward, __) = P.arrays();

        let determ_var_idx: Vec<usize> = forward[0..rank_A].to_vec();

        for (_i, &r) in determ_var_idx.iter().enumerate() {
            let actual_val = x[(r, 0)];
            self.solved_vars.insert(Var(r as u64), actual_val);
        }
    }

    /// Solves for as many variables as possible and substitutes their values into existing constraints.
    /// Deletes constraints that no longer contain unsolved variables.
    pub fn solve_svd(&mut self) {
        let n_vars = self.next_var as usize;
        if n_vars == 0 || self.constraints.is_empty() {
            return;
        }
        let a = DMatrix::from_row_iterator(
            self.constraints.len(),
            n_vars,
            self.constraints
                .iter()
                .flat_map(|c| c.expr.coeff_vec(n_vars)),
        );
        let b = DVector::from_iterator(
            self.constraints.len(),
            self.constraints.iter().map(|c| -c.expr.constant),
        );

        let svd = a.clone().svd(true, true);
        let vt = svd.v_t.as_ref().expect("No V^T matrix");
        let r = svd.rank(EPSILON);
        if r == 0 {
            return;
        }
        let vt_recons = vt.rows(0, r);
        let sol = svd.solve(&b, EPSILON).unwrap();

        for i in 0..self.next_var {
            let recons = (vt_recons.transpose() * vt_recons.column(i as usize))[((i as usize), 0)];
            if !self.solved_vars.contains_key(&Var(i))
                && relative_eq!(recons, 1., epsilon = EPSILON)
            {
                let val = round(sol[(i as usize, 0)]);
                self.solved_vars.insert(Var(i), val);
            }
        }
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
        assert_relative_eq!(*solver.solved_vars.get(&y).unwrap(), 5., epsilon = EPSILON);
        assert_relative_eq!(*solver.solved_vars.get(&y).unwrap(), 5., epsilon = EPSILON);
        assert!(!solver.solved_vars.contains_key(&z));
    }
}
