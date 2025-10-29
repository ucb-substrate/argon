use approx::relative_eq;
use faer::{
    Faer, Mat,
    sparse::{Csc, SparseColMat, linalg::qr},
};
use indexmap::{IndexMap, IndexSet};
use itertools::{Either, Itertools};
use nalgebra::{DMatrix, DVector};
use serde::{Deserialize, Serialize};

use faer_ext::IntoFaer;

const EPSILON: f64 = 1e-10;
const ROUND_STEP: f64 = 1e-3;
const INV_ROUND_STEP: f64 = 1. / ROUND_STEP;

#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq, Serialize, Deserialize, Ord, PartialOrd)]
pub struct Var(u64);

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

    ///use QR to find inconcistent constraints (like what is a linear combination of others)
    /// kinda troll cuz u doing qr of AT
    pub fn relationship_residuals(&self, A: Mat<f64>, B: Mat<f64>) {
        let AT = A.transpose();
        let qr = AT.col_piv_qr();

        let Q = qr.q();
        let R = qr.r();
        let P = qr.p().to_mat();

        let tolerance = 1e-3;

        let rank = R
            .diagonal()
            .as_slice()
            .iter()
            .filter(|&&val| val.abs() > tolerance)
            .count();

        if rank == A.nrows() {
            //full-rank case
            //return? idk
        }

        //get the constraints into the right order by multiplying by P transpose, since we found QR of A^T
        let b_perm = P.transpose() * b;
        let A_perm = P.transpose() * A;

        //split up the A, b matrices into blocks depending on rank
        let A_b = A_perm.rows(0..rank); //not really needed
        let A_d = A_perm.rows(rank..A_perm.nrows()); //not really needed
        let b_b = b_perm.rows(0..rank);
        let b_d = b_perm.rows(rank..A_perm.nrows());

        //split up R matrix into block
        let R11 = R.block(0, 0, rank, rank);
        let R12 = R.block(0, rank, rank, R.ncols() - rank);
        let LT = R12.to_owned();

        //find relationship between the A_b and A_d
        solve_upper_triangular_in_place(R11, LT.as_mut(), Parallelism::None);

        //see if relationship matches that of b_b and b_d
        let perm_residuals = b_d - LT.transpose() * b_b;

        //permute residuals to get back to og order
        let residuals = P * perm_residuals;
        return residuals;
    }

    ///Uses QR to solve least squares, and then checks residuals to get bad constraints
    pub fn ls_residuals(&self, A: Mat<f64>, B: Mat<f64>) {
        let m = A.nrows();
        let n = A.ncols();

        let qr = A.col_piv_qr();
        let Q = qr.q();
        let R = qr.r();
        let P = qr.p().to_mat();

        let rank = R
            .diagonal()
            .as_slice()
            .iter()
            .filter(|&&val| val.abs() > tolerance)
            .count();

        let c = Q.transpose() * B;
        let c1 = c.rows(0..rank);
        let R11 = R.block(0, 0, rank, rank);

        let mut y1 = c1.to_owned();
        solve_upper_triangular_in_place_with_conj(
            R11.as_ref(),
            y1.as_mut(),
            Conj::No,
            Parallelism::None,
        );

        let x = P * y1;
        let residuals = (B - A*x).col(1);
        let residuals_norm = residuals.norm_l2();
        
        let mut badConstraints = false;
        
        for r in 0..residuals.nrows() {
            if residuals[(r, 0)].abs() > 1e-3 {
                badConstraints = true;
            } 
        }
        
        
        
        return Vec![b - A * x, x];
    }

    /// Solves for as many variables as possible and substitutes their values into existing constraints.
    /// Deletes constraints that no longer contain unsolved variables.
    pub fn solve(&mut self) {
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

        let A: Mat<f64> = a.to_faer();
        let B: Mat<f64> = b.to_faer();

        //
        let residual_rqr = self.relationship_residuals(A, B);
        let mut bad_constraints: Vec<u32> = Vec::new();

        //if any nonzero residuals, then those are inconsistent (what are constraint id's)
        for r in 0..residual.nrows() {
            if residual[(r, 0)] > tolerance {
                bad_constraints.push(r);
            }
        }

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
        assert_relative_eq!(*solver.solved_vars.get(&x).unwrap(), 5., epsilon = EPSILON);
        assert_relative_eq!(*solver.solved_vars.get(&y).unwrap(), 5., epsilon = EPSILON);
        assert!(!solver.solved_vars.contains_key(&z));
    }
}
