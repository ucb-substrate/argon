use std::{collections::HashMap, time::Instant};

use approx::relative_eq;
use itertools::{Either, Itertools};
use nalgebra::{DMatrix, DVector};
use ndarray_linalg::SVD;
use serde::{Deserialize, Serialize};

const EPSILON: f64 = 1e-10;

#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq, Serialize, Deserialize)]
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

    /// Returns true if all variables have been solved.
    pub fn fully_solved(&self) -> bool {
        self.solved_vars.len() == self.next_id as usize
    }

    pub fn nullspace_vecs(&self) -> Vec<Vec<f64>> {
        let n_vars = self.next_id as usize;
        let arr_shape = (self.constraints.len() + self.solved_vars.len(), n_vars);
        let arr = self
            .constraints
            .iter()
            .flat_map(|expr| expr.coeff_vec(n_vars))
            .chain(self.solved_vars.iter().flat_map(|(var, _)| {
                let var = var.0 as usize;
                std::iter::repeat_n(0., var)
                    .chain(std::iter::once(1.))
                    .chain(std::iter::repeat_n(0., n_vars - var - 1))
            }))
            .collect::<Vec<_>>();
        let arr = ndarray::Array::from_shape_vec(arr_shape, arr).unwrap();
        let (_, s, vt) = arr.svd(false, true).unwrap();
        let vt = vt.unwrap();
        let shape = vt.shape();
        assert_eq!(shape.len(), 2);
        assert_eq!(shape[0], n_vars);
        assert_eq!(shape[1], n_vars);
        let idx = s
            .iter()
            .position(|x| *x < 1e-10)
            .unwrap_or_else(|| std::cmp::min(arr_shape.0, arr_shape.1));
        (idx..n_vars).map(|i| vt.row(i).to_vec()).collect()
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

        println!("{a:?}");
        println!("{b:?}");
        let start_time = Instant::now();
        let svd = a.clone().svd(true, true);
        let vt = svd.v_t.as_ref().expect("No V^T matrix");
        let s = &svd.singular_values;
        if s[0] < EPSILON {
            return;
        }
        let sol = svd.solve(&b, EPSILON).unwrap();
        println!("elapsed time: {:?}", start_time.elapsed());

        for i in 0..self.next_id {
            if !self.solved_vars.contains_key(&Var(i))
                && relative_eq!(
                    (vt.transpose() * vt.column(i as usize))[((i as usize), 0)],
                    1.,
                    epsilon = EPSILON
                )
            {
                self.solved_vars.insert(Var(i), sol[(i as usize, 0)]);
            }
        }
        for constraint in self.constraints.iter_mut() {
            substitute_expr(&self.solved_vars, constraint);
        }
        // TODO: detect inconsistent constraints
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

#[derive(Debug, Clone, Serialize, Deserialize)]
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
