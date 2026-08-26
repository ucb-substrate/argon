//! Dependency-free iterative solver for sparse, strictly diagonally dominant
//! linear systems.
//!
//! Rows may arrive in any order. [`System::new`] matches each row to its unique
//! dominant column. When every column is matched, the Levy--Desplanques theorem
//! proves that the matrix is nonsingular. Symmetric systems are solved with
//! Jacobi-preconditioned conjugate gradient; other systems use CGLS.

const ZERO_TOLERANCE: f64 = 1e-8;
const ITERATIVE_TOLERANCE: f64 = 1e-10;
const MAX_TERMS_PER_VARIABLE: usize = 64;

/// A row-oriented sparse matrix with a proven unique solution.
pub struct System {
    rows: Vec<Vec<(usize, f64)>>,
    rhs: Vec<f64>,
    diagonal: Vec<f64>,
}

impl System {
    /// Builds a system after coalescing duplicate terms and matching arbitrarily
    /// ordered rows to their strictly dominant columns. Returns `None` for
    /// rectangular, non-dominant, or insufficiently sparse inputs.
    pub fn new(
        mut unordered_rows: Vec<Vec<(usize, f64)>>,
        unordered_rhs: Vec<f64>,
    ) -> Option<Self> {
        let n = unordered_rows.len();
        if n == 0 || unordered_rhs.len() != n {
            return None;
        }
        let mut row_for_column = vec![None; n];
        let mut nonzeros = 0usize;

        for (row_index, row) in unordered_rows.iter_mut().enumerate() {
            if row.iter().any(|&(column, _)| column >= n) {
                return None;
            }
            row.sort_unstable_by_key(|&(column, _)| column);
            let mut coalesced: Vec<(usize, f64)> = Vec::with_capacity(row.len());
            for &(column, value) in row.iter() {
                if let Some((last_column, last_value)) = coalesced.last_mut()
                    && *last_column == column
                {
                    *last_value += value;
                } else {
                    coalesced.push((column, value));
                }
            }
            coalesced.retain(|&(_, value)| value.abs() > ZERO_TOLERANCE);
            nonzeros = nonzeros.checked_add(coalesced.len())?;
            if nonzeros > n.saturating_mul(MAX_TERMS_PER_VARIABLE) {
                return None;
            }

            let row_norm: f64 = coalesced.iter().map(|(_, value)| value.abs()).sum();
            let dominant = coalesced.iter().find_map(|&(column, value)| {
                let other_terms = row_norm - value.abs();
                (value.abs() > other_terms + ZERO_TOLERANCE * row_norm.max(1.)).then_some(column)
            })?;
            if row_for_column[dominant].replace(row_index).is_some() {
                return None;
            }
            *row = coalesced;
        }

        let mut rows = Vec::with_capacity(n);
        let mut rhs = Vec::with_capacity(n);
        let mut diagonal = Vec::with_capacity(n);
        for (column, source_row) in row_for_column.into_iter().enumerate() {
            let source_row = source_row?;
            let mut row = std::mem::take(&mut unordered_rows[source_row]);
            let mut b = unordered_rhs[source_row];
            let diagonal_value = row
                .iter()
                .find_map(|&(other_column, value)| (other_column == column).then_some(value))?;
            if diagonal_value < 0. {
                for (_, value) in &mut row {
                    *value = -*value;
                }
                b = -b;
            }
            diagonal.push(diagonal_value.abs());
            rows.push(row);
            rhs.push(b);
        }

        Some(Self {
            rows,
            rhs,
            diagonal,
        })
    }

    /// Solves the system and validates a scale-independent backward error before
    /// returning. Failure is non-destructive and can be handled by a fallback.
    pub fn solve(&self) -> Option<Vec<f64>> {
        let solution = if self.is_symmetric() {
            self.solve_pcg()
        } else {
            self.solve_cgls()
        }?;
        self.has_small_backward_error(&solution).then_some(solution)
    }

    fn multiply(&self, x: &[f64], output: &mut [f64]) {
        for (out, row) in output.iter_mut().zip(&self.rows) {
            *out = row.iter().map(|&(column, value)| value * x[column]).sum();
        }
    }

    fn multiply_transpose(&self, x: &[f64], output: &mut [f64]) {
        output.fill(0.);
        for (row_index, row) in self.rows.iter().enumerate() {
            for &(column, value) in row {
                output[column] += value * x[row_index];
            }
        }
    }

    fn is_symmetric(&self) -> bool {
        self.rows.iter().enumerate().all(|(row_index, row)| {
            row.iter().all(|&(column, value)| {
                self.rows[column]
                    .binary_search_by_key(&row_index, |&(other_column, _)| other_column)
                    .ok()
                    .is_some_and(|position| {
                        approximately_equal(self.rows[column][position].1, value)
                    })
            })
        })
    }

    fn solve_pcg(&self) -> Option<Vec<f64>> {
        let n = self.rows.len();
        let mut x = vec![0.; n];
        let mut residual = self.rhs.clone();
        let target = ITERATIVE_TOLERANCE * l2_norm(&self.rhs).max(1.);
        if l2_norm(&residual) <= target {
            return Some(x);
        }
        let mut preconditioned: Vec<_> = residual
            .iter()
            .zip(&self.diagonal)
            .map(|(&value, &diagonal)| value / diagonal)
            .collect();
        let mut direction = preconditioned.clone();
        let mut residual_dot_preconditioned = dot(&residual, &preconditioned);
        let mut product = vec![0.; n];

        for _ in 0..iteration_limit(n) {
            self.multiply(&direction, &mut product);
            let denominator = dot(&direction, &product);
            if !denominator.is_finite() || denominator <= 0. {
                return None;
            }
            let alpha = residual_dot_preconditioned / denominator;
            for i in 0..n {
                x[i] += alpha * direction[i];
                residual[i] -= alpha * product[i];
            }
            if l2_norm(&residual) <= target {
                return Some(x);
            }
            for i in 0..n {
                preconditioned[i] = residual[i] / self.diagonal[i];
            }
            let next_dot = dot(&residual, &preconditioned);
            if !next_dot.is_finite() || residual_dot_preconditioned == 0. {
                return None;
            }
            let beta = next_dot / residual_dot_preconditioned;
            for i in 0..n {
                direction[i] = preconditioned[i] + beta * direction[i];
            }
            residual_dot_preconditioned = next_dot;
        }
        None
    }

    /// Conjugate-gradient least squares. This applies `A` and `A^T` directly;
    /// it never forms the denser normal-equation matrix `A^T A`.
    fn solve_cgls(&self) -> Option<Vec<f64>> {
        let n = self.rows.len();
        let mut x = vec![0.; n];
        let mut residual = self.rhs.clone();
        let target = ITERATIVE_TOLERANCE * l2_norm(&self.rhs).max(1.);
        if l2_norm(&residual) <= target {
            return Some(x);
        }
        let mut gradient = vec![0.; n];
        self.multiply_transpose(&residual, &mut gradient);
        let mut direction = gradient.clone();
        let mut gradient_norm = dot(&gradient, &gradient);
        let mut product = vec![0.; n];

        for _ in 0..iteration_limit(n) {
            self.multiply(&direction, &mut product);
            let product_norm = dot(&product, &product);
            if !product_norm.is_finite() || product_norm <= f64::EPSILON {
                return None;
            }
            let alpha = gradient_norm / product_norm;
            for i in 0..n {
                x[i] += alpha * direction[i];
                residual[i] -= alpha * product[i];
            }
            if l2_norm(&residual) <= target {
                return Some(x);
            }
            self.multiply_transpose(&residual, &mut gradient);
            let next_gradient_norm = dot(&gradient, &gradient);
            if !next_gradient_norm.is_finite() || gradient_norm == 0. {
                return None;
            }
            let beta = next_gradient_norm / gradient_norm;
            for i in 0..n {
                direction[i] = gradient[i] + beta * direction[i];
            }
            gradient_norm = next_gradient_norm;
        }
        None
    }

    fn has_small_backward_error(&self, solution: &[f64]) -> bool {
        if solution.iter().any(|value| !value.is_finite()) {
            return false;
        }
        let mut product = vec![0.; self.rows.len()];
        self.multiply(solution, &mut product);
        let residual = product
            .iter()
            .zip(&self.rhs)
            .map(|(&actual, &expected)| (actual - expected).abs())
            .fold(0., f64::max);
        let matrix_norm = self
            .rows
            .iter()
            .map(|row| row.iter().map(|(_, value)| value.abs()).sum::<f64>())
            .fold(0., f64::max);
        let solution_norm = solution.iter().map(|value| value.abs()).fold(0., f64::max);
        let rhs_norm = self.rhs.iter().map(|value| value.abs()).fold(0., f64::max);
        residual <= ZERO_TOLERANCE * (matrix_norm * solution_norm + rhs_norm).max(1.)
    }
}

fn approximately_equal(a: f64, b: f64) -> bool {
    (a - b).abs() <= ITERATIVE_TOLERANCE * a.abs().max(b.abs()).max(1.)
}

fn dot(lhs: &[f64], rhs: &[f64]) -> f64 {
    lhs.iter().zip(rhs).map(|(&a, &b)| a * b).sum()
}

fn l2_norm(values: &[f64]) -> f64 {
    dot(values, values).sqrt()
}

fn iteration_limit(n: usize) -> usize {
    n.saturating_mul(4).clamp(64, 20_000)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ring(n: usize, left: f64, right: f64) -> (System, Vec<f64>) {
        let expected: Vec<_> = (0..n).map(|i| ((i % 11) as f64 - 5.) * 0.1).collect();
        let diagonal = left.abs() + right.abs() + 2.;
        let mut rows = Vec::with_capacity(n);
        let mut rhs = Vec::with_capacity(n);
        for i in 0..n {
            let previous = (i + n - 1) % n;
            let next = (i + 1) % n;
            rows.push(vec![(previous, left), (i, diagonal), (next, right)]);
            rhs.push(left * expected[previous] + diagonal * expected[i] + right * expected[next]);
        }
        (System::new(rows, rhs).unwrap(), expected)
    }

    #[test]
    fn solves_symmetric_system_with_pcg() {
        let (system, expected) = ring(128, -1., -1.);
        assert!(system.is_symmetric());
        let actual = system.solve().unwrap();
        for (actual, expected) in actual.iter().zip(expected) {
            assert!((actual - expected).abs() < 1e-8);
        }
    }

    #[test]
    fn solves_nonsymmetric_system_with_cgls() {
        let (system, expected) = ring(127, -1., -2.);
        assert!(!system.is_symmetric());
        let actual = system.solve().unwrap();
        for (actual, expected) in actual.iter().zip(expected) {
            assert!((actual - expected).abs() < 1e-8);
        }
    }

    #[test]
    fn rejects_a_singular_system() {
        let rows = vec![vec![(0, 1.), (1, 1.)], vec![(0, 2.), (1, 2.)]];
        assert!(System::new(rows, vec![1., 2.]).is_none());
    }

    #[test]
    fn accepts_shuffled_and_scaled_rows() {
        let rows = vec![vec![(1, -8.), (0, 2.)], vec![(1, -1.), (0, 4.)]];
        let solution = System::new(rows, vec![-14., 2.]).unwrap().solve().unwrap();
        assert!((solution[0] - 1.).abs() < 1e-8);
        assert!((solution[1] - 2.).abs() < 1e-8);
    }
}
