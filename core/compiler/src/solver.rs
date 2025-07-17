use std::{
    collections::{HashMap, HashSet, VecDeque},
    ops::Sub,
};

use anyhow::{anyhow, Result};
use approx::{assert_relative_eq, relative_eq};
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
    pub fn constrain_eq0(&mut self, expr: LinearExpr) {
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
        let p = a.clone().svd(true, true).pseudo_inverse(EPSILON).unwrap();
        assert_relative_eq!(&(&a * &p * &b), &b, epsilon = EPSILON);
        let sol = &p * b;
        println!("{:?}", a);
        println!("{:?}", p);
        let variation = DMatrix::identity(n_vars, n_vars) - &p * a;
        println!("{:?}", variation);
        let zeros = DVector::zeros(n_vars);
        for i in 0..self.next_id {
            if relative_eq!(
                &variation.row(i as usize).transpose(),
                &zeros,
                epsilon = EPSILON
            ) {
                self.solved_vars.insert(Var(i), sol[(i as usize, 0)]);
            }
        }
        for constraint in self.constraints.iter_mut() {
            let (l, r): (Vec<f64>, Vec<_>) =
                constraint.coeffs.iter().partition_map(|a @ (coeff, var)| {
                    if let Some(s) = self.solved_vars.get(var) {
                        Either::Left(coeff * s)
                    } else {
                        Either::Right(*a)
                    }
                });
            constraint.coeffs = r;
            constraint.constant -= l.into_iter().reduce(|a, b| a + b).unwrap_or(0.);
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
            coeffs: vec![(1., x), (1., y)],
            constant: 5.,
        });
        solver.constrain_eq0(LinearExpr {
            coeffs: vec![(1., x), (-1., y)],
            constant: 2.,
        });
        solver.solve();
        assert_relative_eq!(
            *solver.solved_vars.get(&x).unwrap(),
            -3.5,
            epsilon = EPSILON
        );
        assert_relative_eq!(
            *solver.solved_vars.get(&y).unwrap(),
            -1.5,
            epsilon = EPSILON
        );
        assert!(solver.solved_vars.get(&z).is_none());
    }
}
