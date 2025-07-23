use std::{
    collections::{HashMap, HashSet, VecDeque},
    ops::Sub,
};

use anyhow::{Result, anyhow};
use approx::{assert_abs_diff_eq, assert_relative_eq, relative_eq};
use arcstr::ArcStr;
use ena::unify::{InPlaceUnificationTable, UnifyKey};
use itertools::{Either, Itertools};
use nalgebra::{DMatrix, DVector, MatrixMN};
use serde::{Deserialize, Serialize};

type Layer = ArcStr;

const EPSILON: f64 = 1e-10;

#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq)]
pub struct Var(u64);

#[derive(Clone, Default)]
pub struct Solver {
    next_id: u64,
    constraints: Vec<LinearExpr>,
    solved_vars: HashMap<Var, f64>,
}

pub fn substitute_expr(table: &HashMap<Var, f64>, expr: &mut LinearExpr) {
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

impl Solver {
    pub fn new() -> Self {
        Default::default()
    }

    pub fn new_var(&mut self) -> Var {
        let var = Var(self.next_id);
        self.next_id += 1;
        var
    }

    /// Constrains the value of `expr` to 0.
    /// TODO: Check if added constraints conflict with existing solution.
    pub fn constrain_eq0(&mut self, mut expr: LinearExpr) {
        substitute_expr(&self.solved_vars, &mut expr);
        self.constraints.push(expr);
    }

    /// Solves for as many variables as possible and substitutes their values into existing constraints.
    /// Deletes constraints that no longer contain unsolved variables.
    pub fn solve(&mut self) {
        let n_vars = self.next_id as usize;
        if n_vars == 0 || self.constraints.is_empty() {
            return;
        }
        let a = DMatrix::from_row_iterator(
            self.constraints.len(),
            n_vars,
            self.constraints
                .iter()
                .flat_map(|expr| expr.coeff_vec(n_vars)),
        );
        let b = DVector::from_iterator(
            self.constraints.len(),
            self.constraints.iter().map(|expr| -expr.constant),
        );
        let svd = a.clone().svd(true, true);
        let vt = svd.v_t.as_ref().expect("No V^T matrix");
        let s = &svd.singular_values;
        let sol = svd.solve(&b, EPSILON).unwrap();

        let mut row_space = Vec::new();

        for i in 0..self.constraints.len() {
            if s[i as usize] > EPSILON {
                // i-th singular value is small ⇒ null space component
                let vec = vt.row(i as usize); // Row of V^T = column of V
                row_space.push(vec);
            }
        }

        for i in 0..self.next_id {
            if !self.solved_vars.contains_key(&Var(i))
                && !row_space
                    .iter()
                    .all(|row| relative_eq!(row[i as usize], 0., epsilon = EPSILON))
            {
                self.solved_vars.insert(Var(i), sol[(i as usize, 0)]);
            }
        }
        for constraint in self.constraints.iter_mut() {
            substitute_expr(&self.solved_vars, constraint);
        }
        self.constraints
            .retain(|constraint| !constraint.coeffs.is_empty());
    }

    pub fn value_of(&self, var: Var) -> Option<f64> {
        self.solved_vars.get(&var).copied()
    }

    pub fn eval_expr(&self, expr: &LinearExpr) -> Option<f64> {
        Some(
            expr.coeffs
                .iter()
                .map(|(coeff, var)| self.value_of(*var).map(|val| val * coeff))
                .fold_options(0., |a, b| a + b)?
                + expr.constant,
        )
    }
}

#[derive(Debug, Clone)]
pub struct LinearExpr {
    pub coeffs: Vec<(f64, Var)>,
    pub constant: f64,
}

impl LinearExpr {
    pub fn coeff_vec(&self, n_vars: usize) -> Vec<f64> {
        let mut out = vec![0.; n_vars];
        for (val, var) in &self.coeffs {
            out[var.0 as usize] = *val;
        }
        out
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
        assert!(solver.solved_vars.get(&z).is_none());
    }
}
