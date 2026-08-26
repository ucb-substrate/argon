//! Pure-Rust sparse linear analysis with QR, CGLS, and null-space extraction.

use dyn_stack::{MemBuffer, MemStack};
use faer::{
    Conj, Mat, Par,
    sparse::linalg::{
        SupernodalThreshold,
        qr::{QrRef, QrSymbolicParams, SymbolicQr, factorize_symbolic_qr},
    },
    sparse::{SparseColMat, SparseColMatRef, Triplet},
};

const ZERO_TOLERANCE: f64 = 1e-8;
const MAX_NONZEROS_PER_DIMENSION: usize = 64;

/// Least-squares solution and an orthonormal basis of the matrix null space.
pub struct Analysis {
    pub solution: Vec<f64>,
    pub nullspace: Vec<Vec<(usize, f64)>>,
}

struct Factorization {
    symbolic: SymbolicQr<usize>,
    indices: Vec<usize>,
    values: Vec<f64>,
}

struct SimplicialData<'a> {
    r_column_ptrs: &'a [usize],
    r_row_indices: &'a [usize],
    r_values: &'a [f64],
    householder_column_ptrs: &'a [usize],
    householder_row_indices: &'a [usize],
    householder_values: &'a [f64],
    tau: &'a [f64],
}

/// Solves a sparse system without materializing a dense matrix.
///
/// Systems with at least as many rows as columns use a fill-reducing sparse QR.
/// The result is returned only when every column is numerically independent;
/// underdetermined and rank-deficient systems return `None` so a caller can use
/// a rank-revealing fallback that solves only uniquely determined variables.
pub fn solve(ncols: usize, rows: &[Vec<(usize, f64)>], rhs: &[f64]) -> Option<Vec<f64>> {
    let nrows = rows.len();
    if ncols == 0 || nrows < ncols || rhs.len() != nrows || !rows_are_safely_scaled(rows) {
        return None;
    }

    let nnz = rows.iter().try_fold(0usize, |total, row| {
        total.checked_add(row.iter().filter(|(_, value)| *value != 0.).count())
    })?;
    if nnz > nrows.max(ncols).saturating_mul(MAX_NONZEROS_PER_DIMENSION) {
        return None;
    }

    let mut triplets = Vec::with_capacity(nnz);
    for (row_index, row) in rows.iter().enumerate() {
        for &(column, value) in row {
            if column >= ncols || !value.is_finite() {
                return None;
            }
            if value != 0. {
                triplets.push(Triplet::new(row_index, column, value));
            }
        }
    }
    if rhs.iter().any(|value| !value.is_finite()) {
        return None;
    }

    let matrix = SparseColMat::<usize, f64>::try_new_from_triplets(nrows, ncols, &triplets).ok()?;
    let factor = factorize(matrix.as_ref())?;

    if numerical_rank(&factor)? != ncols {
        return None;
    }

    // SAFETY: `factorize_numeric_qr` filled both arrays for this exact symbolic
    // factorization immediately above, and neither array has been resized.
    let qr = unsafe { QrRef::new_unchecked(&factor.symbolic, &factor.indices, &factor.values) };
    let mut solution = Mat::<f64>::zeros(nrows, 1);
    for (row, &value) in rhs.iter().enumerate() {
        solution[(row, 0)] = value;
    }
    let par = Par::Seq;
    let mut solve_mem =
        MemBuffer::try_new(factor.symbolic.solve_in_place_scratch::<f64>(1, par)).ok()?;
    qr.solve_in_place_with_conj(
        Conj::No,
        solution.as_mut(),
        par,
        MemStack::new(&mut solve_mem),
    );
    let solution: Vec<_> = (0..ncols).map(|row| solution[(row, 0)]).collect();
    has_small_normal_equation_error(rows, rhs, &solution).then_some(solution)
}

/// Analyzes a general sparse system. Full-column-rank systems use direct sparse
/// QR; rank-deficient systems use CGLS for a particular least-squares solution
/// and sparse Householder QR for an orthonormal null-space basis.
pub fn analyze(ncols: usize, rows: &[Vec<(usize, f64)>], rhs: &[f64]) -> Option<Analysis> {
    if !rows_are_safely_scaled(rows) {
        return None;
    }
    if let Some(solution) = solve(ncols, rows, rhs) {
        return Some(Analysis {
            solution,
            nullspace: Vec::new(),
        });
    }
    let nullspace = nullspace(ncols, rows)?;
    if nullspace.is_empty() {
        return None;
    }
    let solution = solve_cgls(ncols, rows, rhs)?;
    Some(Analysis {
        solution,
        nullspace,
    })
}

/// Returns an orthonormal basis of `null(A)` without constructing a dense input
/// matrix. The output itself can be dense when the mathematical null space is.
pub fn nullspace(ncols: usize, rows: &[Vec<(usize, f64)>]) -> Option<Vec<Vec<(usize, f64)>>> {
    if !rows_are_safely_scaled(rows) {
        return None;
    }
    if ncols == 0 {
        return Some(Vec::new());
    }
    if rows.is_empty() {
        return Some((0..ncols).map(|column| vec![(column, 1.)]).collect());
    }
    let matrix = sparse_matrix(ncols, rows)?;

    // We need an orthonormal basis for col(A^T), whose orthogonal complement
    // is null(A). Faer requires a QR input with rows >= columns. For the usual
    // underconstrained case, factor A^T directly. If there are more constraints
    // than variables, first use A = Q R P^T and factor P R^T, which has exactly
    // the same column space as A^T but is square.
    let rowspace_matrix = if rows.len() <= ncols {
        let triplets: Vec<_> = rows
            .iter()
            .enumerate()
            .flat_map(|(row, entries)| {
                entries.iter().filter_map(move |&(column, value)| {
                    (value != 0.).then_some(Triplet::new(column, row, value))
                })
            })
            .collect();
        SparseColMat::<usize, f64>::try_new_from_triplets(ncols, rows.len(), &triplets).ok()?
    } else {
        let first = factorize(matrix.as_ref())?;
        let data = simplicial_data(&first)?;
        let forward = first.symbolic.col_perm().arrays().0;
        let mut triplets = Vec::with_capacity(data.r_values.len());
        for (permuted_column, &original_column) in forward.iter().enumerate() {
            let start = data.r_column_ptrs[permuted_column];
            let end = data.r_column_ptrs[permuted_column + 1];
            for position in start..end {
                let value = data.r_values[position];
                if value != 0. {
                    // A P = Q R, so A^T and P R^T have the same column space.
                    triplets.push(Triplet::new(
                        original_column,
                        data.r_row_indices[position],
                        value,
                    ));
                }
            }
        }
        SparseColMat::<usize, f64>::try_new_from_triplets(ncols, ncols, &triplets).ok()?
    };

    let factor = factorize(rowspace_matrix.as_ref())?;
    let data = simplicial_data(&factor)?;
    let (rank, coordinate_basis) = left_nullspace_coordinates(&factor, &data)?;
    if rank == ncols {
        return Some(Vec::new());
    }
    let mut used_heads = std::collections::HashSet::new();
    let mut heads = vec![None; factor.symbolic.ncols()];

    // Faer's sparse reflectors do not necessarily start on their column
    // number. Q^T maps each reflector's head row to that column, so Q maps a
    // coordinate vector at the head row back into the corresponding vector.
    for (column, head_slot) in heads.iter_mut().enumerate() {
        let start = data.householder_column_ptrs[column];
        let end = data.householder_column_ptrs[column + 1];
        if let Some(&head) = data.householder_row_indices.get(start..end)?.first() {
            used_heads.insert(head);
            *head_slot = Some(head);
        }
    }

    let mut dense_basis = Vec::with_capacity(ncols - rank);
    for coordinates in coordinate_basis {
        let mut vector = vec![0.; ncols];
        for (column, value) in coordinates.into_iter().enumerate() {
            if let Some(head) = heads[column] {
                vector[head] = value;
            }
        }
        if dot(&vector, &vector) > ZERO_TOLERANCE * ZERO_TOLERANCE {
            dense_basis.push(vector);
        }
    }
    for row in 0..ncols {
        if !used_heads.contains(&row) {
            let mut vector = vec![0.; ncols];
            vector[row] = 1.;
            dense_basis.push(vector);
        }
    }
    if dense_basis.len() != ncols - rank {
        return None;
    }
    orthonormalize(&mut dense_basis)?;
    apply_q(&factor, &mut dense_basis)?;
    validate_nullspace(rows, &dense_basis)?;

    Some(
        dense_basis
            .into_iter()
            .map(|vector| {
                vector
                    .into_iter()
                    .enumerate()
                    .filter_map(|(column, value)| {
                        (value.abs() > ZERO_TOLERANCE).then_some((column, value))
                    })
                    .collect()
            })
            .collect(),
    )
}

/// Rejects systems whose equation scales exceed the numerical rank tolerance.
/// The comparison is relative, so multiplying the whole system by any finite
/// nonzero constant does not change the routing decision. Zero rows are ignored.
fn rows_are_safely_scaled(rows: &[Vec<(usize, f64)>]) -> bool {
    let mut smallest_row_scale = f64::INFINITY;
    let mut largest_row_scale = 0f64;

    for row in rows {
        let mut row_scale = 0f64;
        for &(_, coefficient) in row {
            if !coefficient.is_finite() {
                return false;
            }
            row_scale = row_scale.max(coefficient.abs());
        }
        if row_scale > 0. {
            smallest_row_scale = smallest_row_scale.min(row_scale);
            largest_row_scale = largest_row_scale.max(row_scale);
        }
    }

    smallest_row_scale == f64::INFINITY || smallest_row_scale >= largest_row_scale * ZERO_TOLERANCE
}

fn sparse_matrix(ncols: usize, rows: &[Vec<(usize, f64)>]) -> Option<SparseColMat<usize, f64>> {
    let nnz = rows.iter().try_fold(0usize, |total, row| {
        total.checked_add(row.iter().filter(|(_, value)| *value != 0.).count())
    })?;
    if nnz
        > rows
            .len()
            .max(ncols)
            .saturating_mul(MAX_NONZEROS_PER_DIMENSION)
    {
        return None;
    }
    let mut triplets = Vec::with_capacity(nnz);
    for (row_index, row) in rows.iter().enumerate() {
        for &(column, value) in row {
            if column >= ncols || !value.is_finite() {
                return None;
            }
            if value != 0. {
                triplets.push(Triplet::new(row_index, column, value));
            }
        }
    }
    SparseColMat::<usize, f64>::try_new_from_triplets(rows.len(), ncols, &triplets).ok()
}

/// Applies the implicit Q (not Q^T) by replaying the real Householder
/// reflections in reverse order.
fn apply_q(factor: &Factorization, vectors: &mut [Vec<f64>]) -> Option<()> {
    let data = simplicial_data(factor)?;
    for reflector in (0..factor.symbolic.ncols()).rev() {
        let start = data.householder_column_ptrs[reflector];
        let end = data.householder_column_ptrs[reflector + 1];
        let rows = data.householder_row_indices.get(start..end)?;
        let values = data.householder_values.get(start..end)?;
        if rows.is_empty() {
            continue;
        }
        let tau_inverse = data.tau.get(reflector)?.recip();
        if !tau_inverse.is_finite() && tau_inverse != 0. {
            return None;
        }
        for vector in vectors.iter_mut() {
            let dot: f64 = rows
                .iter()
                .zip(values)
                .map(|(&row, &value)| value * vector[row])
                .sum();
            let scale = dot * tau_inverse;
            for (&row, &value) in rows.iter().zip(values) {
                vector[row] -= scale * value;
            }
        }
    }
    Some(())
}

fn validate_nullspace(rows: &[Vec<(usize, f64)>], basis: &[Vec<f64>]) -> Option<()> {
    for (i, vector) in basis.iter().enumerate() {
        let norm = vector.iter().map(|value| value * value).sum::<f64>();
        if (norm - 1.).abs() > 10. * ZERO_TOLERANCE {
            return None;
        }
        for other in &basis[..i] {
            let dot: f64 = vector.iter().zip(other).map(|(a, b)| a * b).sum();
            if dot.abs() > 10. * ZERO_TOLERANCE {
                return None;
            }
        }
        for row in rows {
            let dot: f64 = row
                .iter()
                .map(|&(column, value)| value * vector[column])
                .sum();
            let scale: f64 = row.iter().map(|(_, value)| value.abs()).sum();
            if dot.abs() > 10. * ZERO_TOLERANCE * scale.max(1.) {
                return None;
            }
        }
    }
    Some(())
}

fn solve_cgls(ncols: usize, rows: &[Vec<(usize, f64)>], rhs: &[f64]) -> Option<Vec<f64>> {
    if ncols == 0 || rhs.len() != rows.len() || rhs.iter().any(|value| !value.is_finite()) {
        return None;
    }
    let mut solution = vec![0.; ncols];
    let mut residual = rhs.to_vec();
    let mut gradient = vec![0.; ncols];
    multiply_transpose(rows, &residual, &mut gradient)?;
    let initial_gradient_norm = dot(&gradient, &gradient);
    if initial_gradient_norm == 0. {
        return Some(solution);
    }
    let target = 1e-20 * initial_gradient_norm.max(1.);
    let mut direction = gradient.clone();
    let mut gradient_norm = initial_gradient_norm;
    let mut product = vec![0.; rows.len()];

    for _ in 0..ncols.max(rows.len()).saturating_mul(4).clamp(64, 100_000) {
        multiply(rows, &direction, &mut product)?;
        let product_norm = dot(&product, &product);
        if !product_norm.is_finite() || product_norm <= f64::EPSILON {
            break;
        }
        let alpha = gradient_norm / product_norm;
        for i in 0..ncols {
            solution[i] += alpha * direction[i];
        }
        for i in 0..rows.len() {
            residual[i] -= alpha * product[i];
        }
        multiply_transpose(rows, &residual, &mut gradient)?;
        let next_gradient_norm = dot(&gradient, &gradient);
        if next_gradient_norm <= target {
            return has_small_normal_equation_error(rows, rhs, &solution).then_some(solution);
        }
        if !next_gradient_norm.is_finite() || gradient_norm == 0. {
            return None;
        }
        let beta = next_gradient_norm / gradient_norm;
        for i in 0..ncols {
            direction[i] = gradient[i] + beta * direction[i];
        }
        gradient_norm = next_gradient_norm;
    }
    has_small_normal_equation_error(rows, rhs, &solution).then_some(solution)
}

fn multiply(rows: &[Vec<(usize, f64)>], input: &[f64], output: &mut [f64]) -> Option<()> {
    for (row, out) in rows.iter().zip(output) {
        let mut value = 0.;
        for &(column, coefficient) in row {
            value += coefficient * *input.get(column)?;
        }
        *out = value;
    }
    Some(())
}

fn multiply_transpose(rows: &[Vec<(usize, f64)>], input: &[f64], output: &mut [f64]) -> Option<()> {
    output.fill(0.);
    for (row, &value) in rows.iter().zip(input) {
        for &(column, coefficient) in row {
            *output.get_mut(column)? += coefficient * value;
        }
    }
    Some(())
}

fn dot(lhs: &[f64], rhs: &[f64]) -> f64 {
    lhs.iter().zip(rhs).map(|(a, b)| a * b).sum()
}

fn factorize(matrix: SparseColMatRef<'_, usize, f64>) -> Option<Factorization> {
    let symbolic = factorize_symbolic_qr(
        matrix.symbolic(),
        QrSymbolicParams {
            // The stable simplicial layout lets us inspect R for numerical rank
            // and apply the stored Householder reflectors to basis vectors.
            supernodal_flop_ratio_threshold: SupernodalThreshold::FORCE_SIMPLICIAL,
            ..Default::default()
        },
    )
    .ok()?;
    let mut indices = vec![0usize; symbolic.len_idx()];
    // Faer reserves its symbolic upper bounds for R and H in one flat array,
    // while the numeric column pointers expose only the entries actually
    // written. NaN sentinels in unused numeric storage let `simplicial_data`
    // recover the boundary between those two reserved sections after the
    // factorization. They are never part of the input or a live factor entry.
    let mut values = vec![f64::NAN; symbolic.len_val()];
    let par = Par::Seq;
    let mut factor_mem =
        MemBuffer::try_new(symbolic.factorize_numeric_qr_scratch::<f64>(par, Default::default()))
            .ok()?;
    symbolic.factorize_numeric_qr(
        &mut indices,
        &mut values,
        matrix,
        par,
        MemStack::new(&mut factor_mem),
        Default::default(),
    );
    Some(Factorization {
        symbolic,
        indices,
        values,
    })
}

/// Parses the documented simplicial storage sections produced by the forced
/// simplicial factorization above.
fn simplicial_data(factor: &Factorization) -> Option<SimplicialData<'_>> {
    let ncols = factor.symbolic.ncols();
    let column_ptrs = factor.indices.get(..ncols + 1)?;
    let &r_nnz = column_ptrs.last()?;
    let r_rows_begin = ncols + 1;
    let r_rows_used_end = r_rows_begin.checked_add(r_nnz)?;
    let total_storage = factor.indices.len().checked_sub(2 * (ncols + 1))?;
    let tau_begin = factor.values.len().checked_sub(ncols)?;
    if tau_begin != total_storage || r_nnz > total_storage {
        return None;
    }

    let r_storage_len = (r_nnz..=total_storage).find(|&r_storage_len| {
        let h_storage_len = total_storage - r_storage_len;
        let h_ptrs_begin = r_rows_begin + r_storage_len;
        let Some(h_ptrs_end) = h_ptrs_begin.checked_add(ncols + 1) else {
            return false;
        };
        let Some(h_ptrs) = factor.indices.get(h_ptrs_begin..h_ptrs_end) else {
            return false;
        };
        let Some((&first, &h_nnz)) = h_ptrs.first().zip(h_ptrs.last()) else {
            return false;
        };
        if first != 0
            || h_nnz > h_storage_len
            || !h_ptrs.windows(2).all(|pair| pair[0] <= pair[1])
            || !factor.values[r_nnz..r_storage_len]
                .iter()
                .all(|value| value.is_nan())
            || !factor.values[r_storage_len..r_storage_len + h_nnz]
                .iter()
                .all(|value| !value.is_nan())
            || !factor.values[r_storage_len + h_nnz..tau_begin]
                .iter()
                .all(|value| value.is_nan())
        {
            return false;
        }
        factor
            .indices
            .get(h_ptrs_end..h_ptrs_end + h_nnz)
            .is_some_and(|rows| rows.iter().all(|&row| row < factor.symbolic.nrows()))
    })?;

    let h_ptrs_begin = r_rows_begin.checked_add(r_storage_len)?;
    let r_row_indices = factor.indices.get(r_rows_begin..r_rows_used_end)?;
    let h_ptrs_end = h_ptrs_begin.checked_add(ncols + 1)?;
    let householder_column_ptrs = factor.indices.get(h_ptrs_begin..h_ptrs_end)?;
    let &h_nnz = householder_column_ptrs.last()?;
    let h_storage_len = total_storage.checked_sub(r_storage_len)?;
    if r_nnz > r_storage_len || h_nnz > h_storage_len {
        return None;
    }
    let h_rows_used_end = h_ptrs_end.checked_add(h_nnz)?;
    let householder_row_indices = factor.indices.get(h_ptrs_end..h_rows_used_end)?;

    let r_values = factor.values.get(..r_nnz)?;
    let h_values_begin = r_storage_len;
    let h_values_end = h_values_begin.checked_add(h_nnz)?;
    let householder_values = factor.values.get(h_values_begin..h_values_end)?;
    let tau_begin = h_values_begin.checked_add(h_storage_len)?;
    let tau = factor.values.get(tau_begin..tau_begin + ncols)?;
    Some(SimplicialData {
        r_column_ptrs: column_ptrs,
        r_row_indices,
        r_values,
        householder_column_ptrs,
        householder_row_indices,
        householder_values,
        tau,
    })
}

/// Computes `null(R^T)` using sparse elimination. A zero diagonal alone is not
/// enough to identify a dependent QR direction: a later independent column
/// can still pivot in an earlier free row. Reducing the already-triangular R
/// handles that ordering without materializing the original matrix densely.
fn left_nullspace_coordinates(
    factor: &Factorization,
    data: &SimplicialData<'_>,
) -> Option<(usize, Vec<Vec<f64>>)> {
    let dimension = factor.symbolic.ncols();
    // A triangular R with every numerical diagonal present is already a rank
    // certificate. This is the common SSE case (independent constraints and a
    // small Q complement), and needs no secondary elimination.
    if numerical_pivots(factor)?.len() == dimension {
        return Some((dimension, Vec::new()));
    }
    let scale = data
        .r_values
        .iter()
        .copied()
        .map(f64::abs)
        .fold(0., f64::max);
    if !scale.is_finite() {
        return None;
    }
    let threshold = ZERO_TOLERANCE * scale.max(1.);
    let mut pivots =
        std::collections::BTreeMap::<usize, std::collections::BTreeMap<usize, f64>>::new();

    // Column j of R is row j of R^T. Its indices are at most j, so choosing
    // the greatest remaining index preserves a forward-solvable echelon form.
    for row in 0..dimension {
        let start = *data.r_column_ptrs.get(row)?;
        let end = *data.r_column_ptrs.get(row + 1)?;
        let mut equation = std::collections::BTreeMap::new();
        for (&column, &value) in data
            .r_row_indices
            .get(start..end)?
            .iter()
            .zip(data.r_values.get(start..end)?)
        {
            if value.is_finite() && value.abs() > threshold {
                *equation.entry(column).or_insert(0.) += value;
            }
        }
        equation.retain(|_, value| value.abs() > threshold);

        while let Some((&pivot, &coefficient)) = equation.last_key_value() {
            if let Some(existing) = pivots.get(&pivot) {
                for (&column, &value) in existing {
                    *equation.entry(column).or_insert(0.) -= coefficient * value;
                }
                equation.retain(|_, value| value.abs() > threshold);
            } else {
                if !coefficient.is_finite() || coefficient.abs() <= threshold {
                    return None;
                }
                for value in equation.values_mut() {
                    *value /= coefficient;
                }
                pivots.insert(pivot, equation);
                break;
            }
        }
    }

    let free: Vec<_> = (0..dimension)
        .filter(|column| !pivots.contains_key(column))
        .collect();
    let mut basis = Vec::with_capacity(free.len());
    for free_column in free {
        let mut vector = vec![0.; dimension];
        vector[free_column] = 1.;
        for (&pivot, equation) in &pivots {
            let sum = equation
                .iter()
                .filter(|&(&column, _)| column != pivot)
                .map(|(&column, &value)| value * vector[column])
                .sum::<f64>();
            vector[pivot] = -sum;
        }
        basis.push(vector);
    }
    Some((pivots.len(), basis))
}

fn orthonormalize(vectors: &mut [Vec<f64>]) -> Option<()> {
    for index in 0..vectors.len() {
        let (previous, current_and_later) = vectors.split_at_mut(index);
        let current = &mut current_and_later[0];
        // A second modified Gram-Schmidt pass keeps the SSE projection stable
        // when dependent constraints have very different scales.
        for _ in 0..2 {
            for other in previous.iter() {
                let projection = dot(current, other);
                for (value, &basis_value) in current.iter_mut().zip(other) {
                    *value -= projection * basis_value;
                }
            }
        }
        let norm = dot(current, current).sqrt();
        if !norm.is_finite() || norm <= ZERO_TOLERANCE {
            return None;
        }
        for value in current {
            *value /= norm;
        }
    }
    Some(())
}

fn numerical_pivots(factor: &Factorization) -> Option<Vec<usize>> {
    let ncols = factor.symbolic.ncols();
    let data = simplicial_data(factor)?;

    let mut diagonal = Vec::with_capacity(ncols);
    for column in 0..ncols {
        let (&start, &end) = data
            .r_column_ptrs
            .get(column)
            .zip(data.r_column_ptrs.get(column + 1))?;
        if start > end || end > data.r_values.len() {
            return None;
        }
        let position = data.r_row_indices[start..end]
            .iter()
            .position(|&row| row == column)
            .map(|offset| start + offset)?;
        diagonal.push(data.r_values[position].abs());
    }
    let scale = diagonal.iter().copied().fold(0., f64::max);
    let threshold = ZERO_TOLERANCE * scale.max(1.);
    scale.is_finite().then(|| {
        diagonal
            .into_iter()
            .enumerate()
            .filter_map(|(column, value)| {
                (value.is_finite() && value > threshold).then_some(column)
            })
            .collect()
    })
}

fn numerical_rank(factor: &Factorization) -> Option<usize> {
    Some(numerical_pivots(factor)?.len())
}

/// A least-squares solution may intentionally have a nonzero residual. Check
/// the scale-independent stationarity condition A^T(Ax-b) ~= 0 instead.
fn has_small_normal_equation_error(
    rows: &[Vec<(usize, f64)>],
    rhs: &[f64],
    solution: &[f64],
) -> bool {
    if solution.iter().any(|value| !value.is_finite()) {
        return false;
    }
    let mut gradient = vec![0.; solution.len()];
    let mut max_column_sum = vec![0.; solution.len()];
    let mut matrix_norm = 0f64;
    for (row, &expected) in rows.iter().zip(rhs) {
        let actual: f64 = row
            .iter()
            .map(|&(column, value)| value * solution[column])
            .sum();
        let residual = actual - expected;
        let mut row_sum = 0.;
        for &(column, value) in row {
            gradient[column] += value * residual;
            max_column_sum[column] += value.abs();
            row_sum += value.abs();
        }
        matrix_norm = matrix_norm.max(row_sum);
    }
    let gradient_norm = gradient.into_iter().map(f64::abs).fold(0., f64::max);
    let transpose_norm = max_column_sum.into_iter().fold(0., f64::max);
    let solution_norm = solution.iter().copied().map(f64::abs).fold(0., f64::max);
    let rhs_norm = rhs.iter().copied().map(f64::abs).fold(0., f64::max);
    gradient_norm
        <= ZERO_TOLERANCE * transpose_norm * (matrix_norm * solution_norm + rhs_norm).max(1.)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sparse_qr_solves_a_non_dominant_system() {
        let rows = vec![
            vec![(0, 1.), (1, 1.), (2, 1.)],
            vec![(0, 1.), (1, -1.), (2, 1.)],
            vec![(0, 1.), (1, 1.), (2, -1.)],
        ];
        let actual = solve(3, &rows, &[6., 2., 0.]).unwrap();
        for (actual, expected) in actual.iter().zip([1., 2., 3.]) {
            assert!((actual - expected).abs() < 1e-8);
        }
    }

    #[test]
    fn sparse_qr_solves_overdetermined_least_squares() {
        // The inconsistent third row makes the residual nonzero, so this also
        // verifies that validation checks A^T(Ax-b), not Ax-b.
        let rows = vec![
            vec![(0, 1.), (1, 1.)],
            vec![(0, 1.), (1, -1.)],
            vec![(0, 2.), (1, 1.)],
        ];
        let actual = solve(2, &rows, &[3., -1., 5.5]).unwrap();

        // Exact least-squares solution of the system above.
        assert!((actual[0] - 10. / 7.).abs() < 1e-8);
        assert!((actual[1] - 31. / 14.).abs() < 1e-8);
    }

    #[test]
    fn sparse_qr_defers_rank_deficient_and_underdetermined_systems() {
        let dependent = vec![vec![(0, 1.), (1, 1.)], vec![(0, 2.), (1, 2.)]];
        assert!(solve(2, &dependent, &[1., 2.]).is_none());

        let underdetermined = vec![vec![(0, 1.), (1, 1.)]];
        assert!(solve(2, &underdetermined, &[1.]).is_none());
    }

    #[test]
    fn scale_preflight_is_relative_and_routes_extreme_ranges() {
        let ordinary = vec![vec![(0, 1.)], vec![(1, 1e-6)]];
        let uniformly_tiny = vec![vec![(0, 1e-100)], vec![(1, 1e-106)]];
        assert!(rows_are_safely_scaled(&ordinary));
        assert!(rows_are_safely_scaled(&uniformly_tiny));
        assert!(rows_are_safely_scaled(&[Vec::new(), vec![(0, 1.)]]));

        let ill_scaled = vec![vec![(0, 1e-12)], vec![(1, 1e12)]];
        assert!(!rows_are_safely_scaled(&ill_scaled));
        assert!(analyze(2, &ill_scaled, &[1e-12, 1e12]).is_none());
    }

    fn dense(vector: &[(usize, f64)], ncols: usize) -> Vec<f64> {
        let mut dense = vec![0.; ncols];
        for &(column, value) in vector {
            dense[column] = value;
        }
        dense
    }

    #[test]
    fn extracts_nullspace_from_an_underdetermined_matrix() {
        let rows = vec![vec![(0, 1.), (1, 1.), (2, 1.)]];
        let basis = nullspace(3, &rows).unwrap();
        assert_eq!(basis.len(), 2);
        let basis: Vec<_> = basis.iter().map(|vector| dense(vector, 3)).collect();
        validate_nullspace(&rows, &basis).unwrap();
    }

    #[test]
    fn extracts_nullspace_with_more_dependent_rows_than_columns() {
        let rows = vec![
            vec![(0, 1.), (1, 1.), (2, 1.)],
            vec![(0, 2.), (1, 2.), (2, 2.)],
            vec![(0, -1.), (1, -1.), (2, -1.)],
            vec![(0, 3.), (1, 3.), (2, 3.)],
        ];
        let basis = nullspace(3, &rows).unwrap();
        assert_eq!(basis.len(), 2);
        let basis: Vec<_> = basis.iter().map(|vector| dense(vector, 3)).collect();
        validate_nullspace(&rows, &basis).unwrap();
    }

    #[test]
    fn rank_deficient_analysis_finds_unique_variables() {
        // z is fixed by combining the two constraints; x and y retain one
        // degree of freedom.
        let rows = vec![
            vec![(0, 1.), (1, 1.), (2, 1.)],
            vec![(0, 1.), (1, 1.), (2, -1.)],
        ];
        let analysis = analyze(3, &rows, &[5., 1.]).unwrap();
        assert_eq!(analysis.nullspace.len(), 1);
        let null = dense(&analysis.nullspace[0], 3);
        assert!(null[2].abs() < 1e-8);
        assert!((analysis.solution[2] - 2.).abs() < 1e-8);
        assert!(has_small_normal_equation_error(
            &rows,
            &[5., 1.],
            &analysis.solution
        ));
    }

    fn dense_rank(rows: &[Vec<(usize, f64)>], ncols: usize) -> usize {
        let mut matrix = vec![vec![0.; ncols]; rows.len()];
        for (row, entries) in matrix.iter_mut().zip(rows) {
            for &(column, value) in entries {
                row[column] += value;
            }
        }
        let mut rank = 0;
        for column in 0..ncols {
            let Some(pivot) = (rank..matrix.len())
                .max_by(|&a, &b| matrix[a][column].abs().total_cmp(&matrix[b][column].abs()))
            else {
                break;
            };
            if matrix[pivot][column].abs() < 1e-8 {
                continue;
            }
            matrix.swap(rank, pivot);
            let pivot_row = matrix[rank].clone();
            for row in matrix.iter_mut().skip(rank + 1) {
                let scale = row[column] / pivot_row[column];
                for entry in column..ncols {
                    row[entry] -= scale * pivot_row[entry];
                }
            }
            rank += 1;
            if rank == matrix.len() {
                break;
            }
        }
        rank
    }

    #[test]
    fn sparse_nullspace_dimension_matches_dense_elimination() {
        let mut state = 0x1234_5678_u64;
        for ncols in 2..=12 {
            for nrows in 1..=15 {
                let mut rows = Vec::with_capacity(nrows);
                for _ in 0..nrows {
                    let mut row = Vec::new();
                    for column in 0..ncols {
                        state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
                        if state >> 61 != 0 {
                            let value = ((state >> 32) % 7) as f64 - 3.;
                            if value != 0. {
                                row.push((column, value));
                            }
                        }
                    }
                    rows.push(row);
                }
                if nrows >= 3 {
                    rows[nrows - 1] = rows[0].clone();
                }
                let rank = dense_rank(&rows, ncols);
                let basis = nullspace(ncols, &rows).unwrap_or_else(|| {
                    panic!("nullspace failed for shape {nrows}x{ncols}: {rows:?}")
                });
                assert_eq!(basis.len(), ncols - rank, "shape {nrows}x{ncols}");
                let basis: Vec<_> = basis.iter().map(|vector| dense(vector, ncols)).collect();
                validate_nullspace(&rows, &basis).unwrap();
            }
        }
    }
}
