use nalgebra::{DMatrix, DVector};
use std::ffi::c_void;

unsafe extern "C" {
    fn eigen_create(m: i32, n: i32) -> *mut c_void;
    fn eigen_free(ptr: *mut c_void);
    fn eigen_compute_qr(
        ptr: *mut c_void,
        rows: *const i32,
        cols: *const i32,
        vals: *const f64,
        nnz: i32,
    ) -> i32;
    fn eigen_get_rank(ptr: *mut c_void) -> i32;
    fn eigen_get_q_dense(ptr: *mut c_void, output: *mut f64, mode: i32);
    fn eigen_get_r_dense(ptr: *mut c_void, output: *mut f64, mode: i32);
    fn eigen_get_permutation(ptr: *mut c_void, output: *mut i32, mode: i32);
    fn eigen_get_free_indices(ptr: *mut c_void, output: *mut f64);
    fn eigen_apply_q(ptr: *mut c_void, input: *const f64, output: *mut f64, mode_mat_factor: i32, mode_transpose: i32);
}

pub struct SpqrFactorization {
    ptr: *mut c_void,
    m: usize,
    n: usize,
    rank: usize,
}

unsafe impl Send for SpqrFactorization {}
unsafe impl Sync for SpqrFactorization {}

impl SpqrFactorization {
    pub fn from_triplets(
        triplet: &Vec<(usize, usize, f64)>,
        m: usize,
        n: usize,
    ) -> Result<Self, String> {
        unsafe {
            if m == 0 || n == 0 {

                let ptr = eigen_create(m as i32, n as i32);
                return Ok(SpqrFactorization {
                    ptr: ptr,
                    m: 0,
                    n: 0,
                    rank: 0 as usize,
                });
            }

            let ptr = eigen_create(m as i32, n as i32);

            let nnz = triplet.len();
            let mut rows = Vec::with_capacity(nnz);
            let mut cols = Vec::with_capacity(nnz);
            let mut vals = Vec::with_capacity(nnz);

            for (r, c, v) in triplet {
                rows.push(*r as i32);
                cols.push(*c as i32);
                vals.push(*v);
            }

            let res =
                eigen_compute_qr(ptr, rows.as_ptr(), cols.as_ptr(), vals.as_ptr(), nnz as i32);

            if res == 0 {
                eigen_free(ptr);
            }

            let rank = eigen_get_rank(ptr);

            Ok(SpqrFactorization {
                ptr: ptr,
                m: m,
                n: n,
                rank: rank as usize,
            })
        }
    }

    pub fn rank(&self) -> usize {
        self.rank
    }

    fn get_dense_matrix(&self, num_rows: usize, num_cols: usize, mode_matrix: i32, mode_factor: i32) -> DMatrix<f64> {
        let mut output = vec![0.0; num_rows * num_cols];
        unsafe {
            if mode_factor == 0 {
                eigen_get_q_dense(self.ptr, output.as_mut_ptr(), mode_matrix);
            } else {
                eigen_get_r_dense(self.ptr, output.as_mut_ptr(), mode_matrix);
            }
        }
        DMatrix::from_vec(num_rows, num_cols, output)
    }

    ///returns the Dense R matrix from the QR of A
    pub fn ra_matrix(&self) -> Result<DMatrix<f64>, String> {
        Ok(self.get_dense_matrix(self.m, self.n, 0, 1))
    }

    ///Returns the Dense Q matrix from the QR of A
    pub fn qa_matrix(&self) -> Result<DMatrix<f64>, String> {
        Ok(self.get_dense_matrix(self.m, self.m, 0, 0))
    }

    ///Returns the Dense R (FULL) matrix from the QR of AT
    pub fn rat_matrix(&self) -> Result<DMatrix<f64>, String> {
        Ok(self.get_dense_matrix(self.n, self.m, 1, 1))
    }

    ///Returns the Dense Q (FULL) matrix from the QR of AT
    pub fn qat_matrix(&self) -> Result<DMatrix<f64>, String> {
        Ok(self.get_dense_matrix(self.n, self.n, 1, 0))
    }


    pub fn permutation_a(&self) -> Result<Vec<usize>, String> {
        let mut indices = vec![0_i32; self.n];
        unsafe {
            eigen_get_permutation(self.ptr, indices.as_mut_ptr(), 0);
        }
        Ok(indices.into_iter().map(|x| x as usize).collect())
    }

    pub fn permutation_at(&self) -> Result<Vec<usize>, String> {
        let mut indices = vec![0_i32; self.m];
        unsafe {
            eigen_get_permutation(self.ptr, indices.as_mut_ptr(), 1);
        }
        Ok(indices.into_iter().map(|x| x as usize).collect())
    }

    fn apply_q_internal(&self, vec: &DVector<f64>, mode_mat_factor: i32, mode_transpose: i32) -> DVector<f64> {
        let size = if mode_mat_factor == 0 { self.m } else { self.n };
        let mut output = vec![0.0_f64; size];

        unsafe {
            eigen_apply_q(
                self.ptr,
                vec.as_ptr(),
                output.as_mut_ptr(),
                mode_mat_factor,
                mode_transpose,
            );
        }

        DVector::from_vec(output)
    }

    pub fn apply_qt(&self, b: &DVector<f64>) -> DVector<f64> {
        self.apply_q_internal(b, 0, 0)
    }


    pub fn apply_q(&self, b: &DVector<f64>) -> DVector<f64> {
        self.apply_q_internal(b, 0, 1)
    }


    ///Complete solve function; determines what path of action to take depending on dimensions of matrix A
    pub fn solve(&self, b: &DVector<f64>) -> Result<DVector<f64>, String> {
        if self.n == 0 {
            return Ok(DVector::zeros(0));
        }
        if self.m >= self.n {
            return self.solve_regular(b);
        } else {
            return self.solve_underconstrained(b);
        }
    }

    ///Solves the system and returns the least squares solution in the case where the matrix has m >= n
    pub fn solve_regular(&self, b: &DVector<f64>) -> Result<DVector<f64>, String> {
        let r = self.ra_matrix().unwrap();

        let r_r = r.view((0, 0), (self.rank, self.rank));
        let perm_vec = self.permutation_a().unwrap();

        let c = self.apply_qt(&b);
        let c_r = c.rows(0, self.rank);

        let y_r = r_r.solve_upper_triangular(&c_r).expect("asdf \n");

        let mut y_prime = DVector::zeros(self.n);

        y_prime.rows_mut(0, self.rank).copy_from(&y_r);

        let mut x_prime = DVector::zeros(self.n);
        for i in 0..self.n {
            x_prime[perm_vec[i]] = y_prime[i];
        }

        return Ok(x_prime);

    }

    ///Solves the system for the underconstrained case when m < n; uses the precomputed QR of AT
    pub fn solve_underconstrained(&self, b: &DVector<f64>) -> Result<DVector<f64>, String> {
        let rank = self.rank();
        let r = self.rat_matrix().unwrap();
        let perm_vec = self.permutation_at().unwrap();

        let mut c = DVector::zeros(self.m);
        for i in 0..self.m {
            c[i] = b[perm_vec[i]];
        }

        let r_acc = r.view((0, 0), (rank, rank));
        let c_main = c.rows(0, rank);

        let y_small = r_acc.transpose().solve_lower_triangular(&c_main).unwrap();

        let mut y = DVector::zeros(self.n);
        y.rows_mut(0, rank).copy_from(&y_small);

        let x = self.apply_q_internal(&y, 1, 1);

        Ok(x)
    }

    ///Returns the nullspace vectors of A. Uses the last n - r rows of Q from AT
    pub fn get_nullspace(&self) -> Result<DMatrix<f64>, String> {
        let q = &self.qat_matrix().unwrap();
        let null_space_vectors = q.columns(self.rank, self.n - self.rank).clone_owned();
        return Ok(null_space_vectors);
    }

    pub fn get_free_indices(&self) -> Result<DVector<f64>, String> {
        if self.n == 0 {
            return Ok(DVector::zeros(0));
        }
        if self.m == 0 {
            return Ok(DVector::zeros(self.n));
        }
        let mut indices_mags = vec![0.0_f64; self.n];
        unsafe {
            eigen_get_free_indices(self.ptr, indices_mags.as_mut_ptr());
        }
        return Ok(DVector::from_vec(indices_mags));
    }
}

impl Drop for SpqrFactorization {
    fn drop(&mut self) {
        unsafe {
            eigen_free(self.ptr);
        }
    }
}
