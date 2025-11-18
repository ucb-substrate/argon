#![allow(non_upper_case_globals)]
#![allow(non_camel_case_types)]
#![allow(non_snake_case)]
#![allow(dead_code)]
#![allow(improper_ctypes)]

mod ffi {
    include!(concat!(env!("OUT_DIR"), "/spqr_bindings.rs"));
}

use nalgebra::DMatrix;
use nalgebra::DVector;
use rayon::prelude::*;
use std::ptr;
use std::ptr::NonNull;

///SpqrFactorization struct
///Used for SuiteSparse,
pub struct SpqrFactorization {
    q_a: *mut ffi::cholmod_sparse,   //Q matrix for AP = QR
    q_at: *mut ffi::cholmod_sparse,  //Q' matrix for AT P' = Q'R'
    r_a: *mut ffi::cholmod_sparse,   //R matrix for AP = QR
    r_at: *mut ffi::cholmod_sparse,  //R' matrix for AT P' = Q'R'
    e_a: *mut i64,                   //permutation vector for AP = QR
    e_at: *mut i64,                  //permutation vector for AT P' = Q'R'
    rank: usize,                     //rank of A (and hence AT)
    cc_a: *mut ffi::cholmod_common,  //cholmod struct for AP = QR
    cc_at: *mut ffi::cholmod_common, //cholmod struct for AT P' = Q'R'
    m: usize,                        //number of rows of A (or columns of AT)
    n: usize,                        //number of columns of A (or rows of AT)
}

unsafe impl Send for SpqrFactorization {}
unsafe impl Sync for SpqrFactorization {}

//use a struct as a wrapper for a pointer that's shared across threads
pub struct pointer_wrapper<T> {
    pointer: NonNull<T>,
}

impl<T> pointer_wrapper<T> {
    fn as_ptr(&self) -> *mut T {
        self.pointer.as_ptr()
    }
}

unsafe impl<T> Send for pointer_wrapper<T> {}
unsafe impl<T> Sync for pointer_wrapper<T> {}

impl SpqrFactorization {
    ///Creates new SpqrFactorization struct
    /// takes in as input
    /// triplet : a vector of triplets (row_idx, col_idx, value)
    /// m : number of rows of the matrix A
    /// n : number of columns of the matrix A
    pub fn from_triplets(
        triplet: &Vec<(usize, usize, f64)>,
        m: usize,
        n: usize,
    ) -> Result<Self, String> {
        unsafe {
            let mut cc_a = Box::new(std::mem::zeroed::<ffi::cholmod_common>());
            let mut cc_at = Box::new(std::mem::zeroed::<ffi::cholmod_common>());

            ffi::cholmod_l_start(cc_a.as_mut());
            ffi::cholmod_l_start(cc_at.as_mut());

            cc_a.nthreads_max = 0;
            cc_at.nthreads_max = 0;

            let (A, AT) =
                Self::triplet_to_cholmod_sparse(triplet, m, n, cc_a.as_mut(), cc_at.as_mut())
                    .unwrap();

            let mut q_a: *mut ffi::cholmod_sparse = ptr::null_mut();
            let mut q_at: *mut ffi::cholmod_sparse = ptr::null_mut();

            let mut r_a: *mut ffi::cholmod_sparse = ptr::null_mut();
            let mut r_at: *mut ffi::cholmod_sparse = ptr::null_mut();

            let mut e_a: *mut i64 = ptr::null_mut();
            let mut e_at: *mut i64 = ptr::null_mut();

            let a_econ: i64 = 0;

            let rank_a = ffi::SuiteSparseQR_C_QR(
                ffi::SPQR_ORDERING_DEFAULT as i32,
                ffi::SPQR_DEFAULT_TOL as f64,
                a_econ,
                A,
                &mut q_a,
                &mut r_a,
                &mut e_a,
                cc_a.as_mut(),
            );

            //rank_at = rank_a

            let at_econ: i64 = 1 + (n as i64);

            let rank_at = ffi::SuiteSparseQR_C_QR(
                ffi::SPQR_ORDERING_DEFAULT as i32,
                ffi::SPQR_DEFAULT_TOL as f64,
                at_econ, //want full version of QR for AT
                AT,
                &mut q_at,
                &mut r_at,
                &mut e_at,
                cc_at.as_mut(),
            );

            ffi::cholmod_l_free_sparse(&mut (A as *mut _), cc_a.as_mut());
            ffi::cholmod_l_free_sparse(&mut (AT as *mut _), cc_at.as_mut());

            if rank_a == -1 {
                //failed
                ffi::cholmod_l_finish(cc_a.as_mut());
                ffi::cholmod_l_finish(cc_at.as_mut());
                return Err("failed".to_string());
            }

            Ok(SpqrFactorization {
                q_a: q_a,
                q_at: q_at,
                r_a: r_a,
                r_at: r_at,
                e_a: e_a,
                e_at: e_at,
                rank: rank_a as usize,
                cc_a: Box::into_raw(cc_a),
                cc_at: Box::into_raw(cc_at),
                m,
                n,
            })
        }
    }

    ///returns the R matrix from the QR of A
    pub fn ra_matrix(&self) -> Result<DMatrix<f64>, String> {
        unsafe { self.cholmod_sparse_to_dense(self.r_a, self.cc_a) }
    }

    ///Returns the Q matrix from the QR of A
    pub fn qa_matrix(&self) -> Result<DMatrix<f64>, String> {
        unsafe { self.cholmod_sparse_to_dense(self.q_a, self.cc_a) }
    }

    ///Returns the R (FULL) matrix from the QR of AT
    pub fn rat_matrix(&self) -> Result<DMatrix<f64>, String> {
        unsafe { self.cholmod_sparse_to_dense(self.r_at, self.cc_at) }
    }

    ///Returns the Q (FULL) matrix from the QR of AT
    pub fn qat_matrix(&self) -> Result<DMatrix<f64>, String> {
        unsafe { self.cholmod_sparse_to_dense(self.q_at, self.cc_at) }
    }

    ///Returns the permutation vector from QR of A
    pub fn permutation_a(&self) -> Result<Vec<usize>, String> {
        unsafe {
            // if e is null, permutation is I
            if self.e_a.is_null() {
                return Ok((0..self.n).collect());
            }

            let perm_pointer = self.e_a as *const i64;

            let mut perm = Vec::with_capacity(self.n);
            for i in 0..self.n {
                perm.push(*perm_pointer.add(i) as usize);
            }
            Ok(perm)
        }
    }
    ///Returns the permutation vector from QR of AT
    pub fn permutation_at(&self) -> Result<Vec<usize>, String> {
        unsafe {
            // if e is null, permutation is I
            if self.e_at.is_null() {
                return Ok((0..self.m).collect());
            }

            let perm_pointer = self.e_at as *const i64;

            let mut perm = Vec::with_capacity(self.m);
            for i in 0..self.m {
                perm.push(*perm_pointer.add(i) as usize);
            }
            Ok(perm)
        }
    }
    ///Returns the rank of the matrix A/AT obtained from QR
    ///Warning: not always the actual rank, see Kahan matrices
    pub fn rank(&self) -> usize {
        self.rank
    }

    ///Converts a vector of triplets to two cholmod sparse matrices A and AT.
    ///takes as input
    ///triplet : vector of (row_idx, col_idx, value)
    ///m : number of rows in A
    ///n : number of columns in A
    pub unsafe fn triplet_to_cholmod_sparse(
        triplet: &Vec<(usize, usize, f64)>,
        m: usize,
        n: usize,
        cc_a: *mut ffi::cholmod_common,
        cc_at: *mut ffi::cholmod_common,
    ) -> Result<(*mut ffi::cholmod_sparse, *mut ffi::cholmod_sparse), String> {
        unsafe {
            let nnz = triplet.len();

            let cholmod_triplet_a =
                ffi::cholmod_l_allocate_triplet(m, n, nnz, 0, ffi::CHOLMOD_REAL as i32, cc_a);
            let cholmod_triplet_at =
                ffi::cholmod_l_allocate_triplet(n, m, nnz, 0, ffi::CHOLMOD_REAL as i32, cc_at);

            let cholmod_triplet_a_ref = &mut *cholmod_triplet_a;
            let cholmod_triplet_at_ref = &mut *cholmod_triplet_at;

            let j_a = cholmod_triplet_a_ref.j as *mut i64;
            let i_a = cholmod_triplet_a_ref.i as *mut i64;
            let x_a = cholmod_triplet_a_ref.x as *mut f64;

            let j_at = cholmod_triplet_at_ref.j as *mut i64;
            let i_at = cholmod_triplet_at_ref.i as *mut i64;
            let x_at = cholmod_triplet_at_ref.x as *mut f64;

            let j_a_wrapper = pointer_wrapper {
                pointer: NonNull::new(j_a).unwrap(),
            };
            let i_a_wrapper = pointer_wrapper {
                pointer: NonNull::new(i_a).unwrap(),
            };
            let x_a_wrapper = pointer_wrapper {
                pointer: NonNull::new(x_a).unwrap(),
            };

            let j_at_wrapper = pointer_wrapper {
                pointer: NonNull::new(j_at).unwrap(),
            };
            let i_at_wrapper = pointer_wrapper {
                pointer: NonNull::new(i_at).unwrap(),
            };
            let x_at_wrapper = pointer_wrapper {
                pointer: NonNull::new(x_at).unwrap(),
            };

            triplet
                .par_iter()
                .enumerate()
                .for_each(|(idx, (i, j, val))| {
                    let i_a_pointer = i_a_wrapper.as_ptr();
                    let j_a_pointer = j_a_wrapper.as_ptr();
                    let x_a_pointer = x_a_wrapper.as_ptr();

                    let i_at_pointer = i_at_wrapper.as_ptr();
                    let j_at_pointer = j_at_wrapper.as_ptr();
                    let x_at_pointer = x_at_wrapper.as_ptr();

                    *i_a_pointer.add(idx) = *i as i64;
                    *j_a_pointer.add(idx) = *j as i64;
                    *x_a_pointer.add(idx) = *val;

                    //for at, swap (i, j) -> (j, i)
                    *i_at_pointer.add(idx) = *j as i64;
                    *j_at_pointer.add(idx) = *i as i64;
                    *x_at_pointer.add(idx) = *val;
                });

            cholmod_triplet_a_ref.nnz = nnz;
            cholmod_triplet_at_ref.nnz = nnz;

            let a_sparse = ffi::cholmod_l_triplet_to_sparse(cholmod_triplet_a, nnz, cc_a);
            let at_sparse = ffi::cholmod_l_triplet_to_sparse(cholmod_triplet_at, nnz, cc_at);

            ffi::cholmod_l_free_triplet(&mut (cholmod_triplet_a as *mut _), cc_a);
            ffi::cholmod_l_free_triplet(&mut (cholmod_triplet_at as *mut _), cc_at);

            Ok((a_sparse, at_sparse))
        }
    }

    ///Takes in as input sparse matrix and cholmod struct, returns the dense DMatrix version of the data
    unsafe fn cholmod_sparse_to_dense(
        &self,
        sparse: *const ffi::cholmod_sparse,
        cc: *mut ffi::cholmod_common,
    ) -> Result<DMatrix<f64>, String> {
        unsafe {
            let dense = ffi::cholmod_l_sparse_to_dense(sparse as *mut _, &mut *cc);

            let result = self.cholmod_dense_to_nalgebra(dense).unwrap();
            ffi::cholmod_l_free_dense(&mut (dense as *mut _), &mut *cc);

            Ok(result)
        }
    }

    ///Takes in as input a cholmod dense and converts it into a dense DMatrix
    unsafe fn cholmod_dense_to_nalgebra(
        &self,
        dense: *const ffi::cholmod_dense,
    ) -> Result<DMatrix<f64>, String> {
        unsafe {
            let dense_ref = &*dense;
            let m = dense_ref.nrow;
            let n = dense_ref.ncol;
            let data_pointer = dense_ref.x as *mut f64;
            let acc_data_pointer = pointer_wrapper {
                pointer: NonNull::new(data_pointer).unwrap(),
            };

            let mut matrix = DMatrix::zeros(m, n);

            matrix
                .par_column_iter_mut()
                .enumerate()
                .for_each(|(j, mut col_slice)| unsafe {
                    let col_pointer = acc_data_pointer.as_ptr().add(j * m);
                    for i in 0..m {
                        col_slice[i] = *col_pointer.add(i);
                    }
                });

            Ok(matrix)
        }
    }

    ///Complete solve function; determines what path of action to take depending on dimensions of matrix A
    pub fn solve(&self, b: &DVector<f64>) -> Result<DVector<f64>, String> {
        if self.m >= self.n {
            return self.solve_regular(b);
        } else {
            return self.solve_underconstrained(b);
        }
    }

    ///Solves the system and returns the least squares solution in the case where the matrix has m >= n
    pub fn solve_regular(&self, b: &DVector<f64>) -> Result<DVector<f64>, String> {
        let q = self.qa_matrix().unwrap();
        let r = self.ra_matrix().unwrap();
        let perm_vec = self.permutation_a().unwrap();

        let c = q.transpose() * b;
        let mut y = DVector::zeros(self.n);

        let r_acc = r.columns(0, self.rank);

        match r_acc.solve_upper_triangular(&c) {
            Some(y_main) => {
                y.rows_mut(0, self.rank).copy_from(&y_main);
            }
            None => return Err("failed R solving".to_string()),
        }

        let mut x = DVector::zeros(self.n);

        for i in 0..self.n {
            x[perm_vec[i]] = y[i];
        }

        Ok(x)
    }

    ///Solves the system for the underconstrained case when m < n; uses the precomputed QR of AT
    pub fn solve_underconstrained(&self, b: &DVector<f64>) -> Result<DVector<f64>, String> {
        let rank = self.rank();
        let q = self.qat_matrix().unwrap();
        let q_thin = q.columns(0, rank);
        let r = self.rat_matrix().unwrap();
        let perm_vec = self.permutation_at().unwrap();

        let mut c = DVector::zeros(self.m);
        for i in 0..self.m {
            c[i] = b[perm_vec[i]];
        }

        let r_acc = r.view((0, 0), (rank, rank));
        let c_main = c.rows(0, rank);

        let y = r_acc.transpose().solve_lower_triangular(&c_main).unwrap();

        let x = q_thin * y;

        Ok(x)
    }

    ///Returns the nullspace vectors of A. Uses the last n - r rows of Q from AT
    pub fn get_nullspace(&self) -> Result<DMatrix<f64>, String> {
        let q = &self.qat_matrix().unwrap();
        let null_space_vectors = q.columns(self.rank, self.n - self.rank).clone_owned(); //might have to change depending on memmove time
        return Ok(null_space_vectors);
    }
}

impl Drop for SpqrFactorization {
    fn drop(&mut self) {
        unsafe {
            if !self.q_a.is_null() {
                ffi::cholmod_l_free_sparse(&mut self.q_a, &mut *self.cc_a);
            }
            if !self.q_at.is_null() {
                ffi::cholmod_l_free_sparse(&mut self.q_at, &mut *self.cc_at);
            }
            if !self.r_a.is_null() {
                ffi::cholmod_l_free_sparse(&mut self.r_a, &mut *self.cc_a);
            }
            if !self.r_at.is_null() {
                ffi::cholmod_l_free_sparse(&mut self.r_at, &mut *self.cc_at);
            }

            if !self.e_a.is_null() {
                ffi::cholmod_l_free(
                    self.n,
                    std::mem::size_of::<i64>(),
                    self.e_a as *mut _,
                    &mut *self.cc_a,
                );
            }
            if !self.e_at.is_null() {
                ffi::cholmod_l_free(
                    self.m,
                    std::mem::size_of::<i64>(),
                    self.e_at as *mut _,
                    &mut *self.cc_at,
                );
            }

            if !self.cc_a.is_null() {
                ffi::cholmod_l_finish(&mut *self.cc_a);
                drop(Box::from_raw(self.cc_a));
            }
            if !self.cc_at.is_null() {
                ffi::cholmod_l_finish(&mut *self.cc_at);
                drop(Box::from_raw(self.cc_at));
            }
        }
    }
}
