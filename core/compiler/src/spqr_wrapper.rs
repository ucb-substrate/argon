// src/spqr_wrapper.rs

#![allow(non_upper_case_globals)]
#![allow(non_camel_case_types)]
#![allow(non_snake_case)]
#![allow(dead_code)]
#![allow(improper_ctypes)]

// Include the generated bindings
mod ffi {
    include!(concat!(env!("OUT_DIR"), "/spqr_bindings.rs"));
}

use nalgebra::DMatrix;
use nalgebra::DVector;
use rayon::prelude::*;
use std::ptr;
use std::ptr::NonNull;

use crate::spqr_wrapper::ffi::CHOLMOD_REAL;
use crate::spqr_wrapper::ffi::cholmod_l_allocate_triplet;

pub struct SpqrFactorization {
    q: *mut ffi::cholmod_sparse,
    r: *mut ffi::cholmod_sparse,
    e: *mut i64,
    rank: usize,
    cc: *mut ffi::cholmod_common,
    m: usize,
    n: usize,
}

pub struct unsafe_pointer_for_threads<T> {
    pointer: NonNull<T>,
}
impl<T> unsafe_pointer_for_threads<T> {
    fn as_ptr(&self) -> *mut T {
        self.pointer.as_ptr()
    }
}
unsafe impl<T> Send for unsafe_pointer_for_threads<T> {}
unsafe impl<T> Sync for unsafe_pointer_for_threads<T> {}

impl SpqrFactorization {
    pub fn new(matrix: &DMatrix<f64>) -> Result<Self, String> {
        unsafe {
            let mut cc = Box::new(std::mem::zeroed::<ffi::cholmod_common>());
            ffi::cholmod_l_start(cc.as_mut());
            cc.nthreads_max = 0;

            let m = matrix.nrows();
            let n = matrix.ncols();

            let cholmod_matrix = Self::dmatrix_to_cholmod_sparse(matrix, cc.as_mut()).unwrap();

            let mut q_s: *mut ffi::cholmod_sparse = ptr::null_mut();
            let mut r_s: *mut ffi::cholmod_sparse = ptr::null_mut();
            let mut e_p: *mut i64 = ptr::null_mut();

            let econ: i64 = 0;

            let rank = ffi::SuiteSparseQR_C_QR(
                ffi::SPQR_ORDERING_DEFAULT as i32,
                ffi::SPQR_DEFAULT_TOL as f64,
                econ,
                cholmod_matrix,
                &mut q_s,
                &mut r_s,
                &mut e_p,
                cc.as_mut(),
            );

            ffi::cholmod_l_free_sparse(&mut (cholmod_matrix as *mut _), cc.as_mut());

            if rank == -1 {
                //failed
                ffi::cholmod_l_finish(cc.as_mut());
                return Err("failed".to_string());
            }

            Ok(SpqrFactorization {
                q: q_s,
                r: r_s,
                e: e_p,
                rank: rank as usize,
                cc: Box::into_raw(cc),
                m,
                n,
            })
        }
    }

    pub fn new_from_triplets(
        triplet: &Vec<(usize, usize, f64)>,
        m: usize,
        n: usize,
    ) -> Result<Self, String> {
        unsafe {
            let mut cc = Box::new(std::mem::zeroed::<ffi::cholmod_common>());
            ffi::cholmod_l_start(cc.as_mut());
            cc.nthreads_max = 0;
            let cholmod_matrix =
                Self::triplet_to_cholmod_sparse(triplet, m, n, cc.as_mut()).unwrap();

            let mut q_s: *mut ffi::cholmod_sparse = ptr::null_mut();
            let mut r_s: *mut ffi::cholmod_sparse = ptr::null_mut();
            let mut e_p: *mut i64 = ptr::null_mut();

            let econ: i64 = 0;

            let rank = ffi::SuiteSparseQR_C_QR(
                ffi::SPQR_ORDERING_DEFAULT as i32,
                ffi::SPQR_DEFAULT_TOL as f64,
                econ,
                cholmod_matrix,
                &mut q_s,
                &mut r_s,
                &mut e_p,
                cc.as_mut(),
            );

            ffi::cholmod_l_free_sparse(&mut (cholmod_matrix as *mut _), cc.as_mut());

            if rank == -1 {
                //failed
                ffi::cholmod_l_finish(cc.as_mut());
                return Err("failed".to_string());
            }

            Ok(SpqrFactorization {
                q: q_s,
                r: r_s,
                e: e_p,
                rank: rank as usize,
                cc: Box::into_raw(cc),
                m,
                n,
            })
        }
    }

    ///triplet construction of A, AT
    /// Need AT to get nullspace vectors a
    /// last m - r columns of Q for A^T are nullspace basis vectors

    ///triplet to cholmod sparse
    pub unsafe fn triplet_to_cholmod_sparse(
        triplet: &Vec<(usize, usize, f64)>,
        m: usize,
        n: usize,
        cc: *mut ffi::cholmod_common,
    ) -> Result<*mut ffi::cholmod_sparse, String> {
        unsafe {
            let nnz = triplet.len();

            let cholmod_triplet =
                ffi::cholmod_l_allocate_triplet(m, n, nnz, 0, ffi::CHOLMOD_REAL as i32, cc);

            let cholmod_triplet_ref = &mut *cholmod_triplet;

            let j_pointer = cholmod_triplet_ref.j as *mut i64;
            let i_pointer = cholmod_triplet_ref.i as *mut i64;
            let x_pointer = cholmod_triplet_ref.x as *mut f64;

            let j_pointer_wrapper = unsafe_pointer_for_threads {
                pointer: NonNull::new(j_pointer).unwrap(),
            };
            let i_pointer_wrapper = unsafe_pointer_for_threads {
                pointer: NonNull::new(i_pointer).unwrap(),
            };
            let x_pointer_wrapper = unsafe_pointer_for_threads {
                pointer: NonNull::new(x_pointer).unwrap(),
            };

            triplet
                .par_iter()
                .enumerate()
                .for_each(|(idx, (i, j, val))| {
                    let acc_i_pointer = i_pointer_wrapper.as_ptr();
                    let acc_j_pointer = j_pointer_wrapper.as_ptr();
                    let acc_x_pointer = x_pointer_wrapper.as_ptr();
                    *acc_i_pointer.add(idx) = *i as i64;
                    *acc_j_pointer.add(idx) = *j as i64;
                    *acc_x_pointer.add(idx) = *val;
                });
            cholmod_triplet_ref.nnz = nnz;

            let a_sparse = ffi::cholmod_l_triplet_to_sparse(cholmod_triplet, nnz, cc);

            ffi::cholmod_l_free_triplet(&mut (cholmod_triplet as *mut _), cc);

            Ok(a_sparse)
        }
    }

    pub fn r_matrix(&self) -> Result<DMatrix<f64>, String> {
        unsafe { self.cholmod_sparse_to_dense(self.r) }
    }

    pub fn q_matrix(&self) -> Result<DMatrix<f64>, String> {
        unsafe { self.cholmod_sparse_to_dense(self.q) }
    }

    pub fn permutation(&self) -> Result<Vec<usize>, String> {
        unsafe {
            // if e is null, permutation is I
            if self.e.is_null() {
                return Ok((0..self.n).collect());
            }

            let perm_pointer = self.e as *const i64;

            let mut perm = Vec::with_capacity(self.n);
            for i in 0..self.n {
                perm.push(*perm_pointer.add(i) as usize);
            }
            Ok(perm)
        }
    }

    pub fn rank(&self) -> usize {
        self.rank
    }

    ///DMatrix to CHOLMOD sparse format (CSC)
    unsafe fn dmatrix_to_cholmod_sparse(
        matrix: &DMatrix<f64>,
        cc: *mut ffi::cholmod_common,
    ) -> Result<*mut ffi::cholmod_sparse, String> {
        unsafe {
            let m = matrix.nrows();
            let n = matrix.ncols();

            let mut nnz = matrix
                .par_column_iter()
                .map(|col| col.into_iter().filter(|x| **x != 0.0).count())
                .sum();

            if nnz < 1 {
                nnz = 1;
            }

            let sparse =
                ffi::cholmod_l_allocate_sparse(m, n, nnz, 1, 1, 0, ffi::CHOLMOD_REAL as i32, cc);

            let sparse_ref = &mut *sparse;

            let col_ptr = sparse_ref.p as *mut i64;
            let row_ind = sparse_ref.i as *mut i64;
            let values = sparse_ref.x as *mut f64;

            let mut idx = 0;
            for j in 0..n {
                *col_ptr.add(j) = idx as i64;
                for i in 0..m {
                    let val = matrix[(i, j)];
                    if val != 0.0 && idx < nnz {
                        *row_ind.add(idx) = i as i64;
                        *values.add(idx) = val;
                        idx += 1;
                    }
                }
            }
            *col_ptr.add(n) = idx as i64;

            if idx == 0 {
                *col_ptr.add(n) = 1;
                *row_ind.add(0) = 0;
                *values.add(0) = 0.0;
                sparse_ref.nzmax = 1;
            } else {
                sparse_ref.nzmax = idx;
            }

            Ok(sparse)
        }
    }

    /// Convert DMatrix to CHOLMOD dense format (alternative approach)
    unsafe fn dmatrix_to_cholmod_dense(
        matrix: &DMatrix<f64>,
        cc: *mut ffi::cholmod_common,
    ) -> Result<*mut ffi::cholmod_dense, String> {
        unsafe {
            let m = matrix.nrows();
            let n = matrix.ncols();

            let dense = ffi::cholmod_l_allocate_dense(m, n, m, ffi::CHOLMOD_REAL as i32, cc);

            let dense_ref = &mut *dense;
            let data_pointer = dense_ref.x as *mut f64;
            let acc_data_pointer = unsafe_pointer_for_threads::<f64> {
                pointer: NonNull::new(data_pointer).unwrap(),
            };

            //column major for cholmod

            (0..n).into_par_iter().for_each(|j| unsafe {
                let col_pointer = acc_data_pointer.as_ptr().add(j * m);
                for i in 0..m {
                    *col_pointer.add(i) = matrix[(i, j)];
                }
            });

            Ok(dense)
        }
    }

    /// Convert CHOLMOD sparse to dense matrix
    unsafe fn cholmod_sparse_to_dense(
        &self,
        sparse: *const ffi::cholmod_sparse,
    ) -> Result<DMatrix<f64>, String> {
        unsafe {
            let dense = ffi::cholmod_l_sparse_to_dense(sparse as *mut _, &mut *self.cc);

            let result = self.cholmod_dense_to_nalgebra(dense).unwrap();
            ffi::cholmod_l_free_dense(&mut (dense as *mut _), &mut *self.cc);

            Ok(result)
        }
    }

    /// Convert CHOLMOD dense to nalgebra DMatrix
    unsafe fn cholmod_dense_to_nalgebra(
        &self,
        dense: *const ffi::cholmod_dense,
    ) -> Result<DMatrix<f64>, String> {
        unsafe {
            let dense_ref = &*dense;
            let m = dense_ref.nrow;
            let n = dense_ref.ncol;
            let data_pointer = dense_ref.x as *mut f64;
            let acc_data_pointer = unsafe_pointer_for_threads {
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

    pub fn solve_regular(&self, b: &DVector<f64>) -> Result<DVector<f64>, String> {
        let q = self.q_matrix().unwrap();
        let r = self.r_matrix().unwrap();
        let perm_vec = self.permutation().unwrap();

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

    pub fn solve_underconstrained(
        &self,
        a: &DMatrix<f64>,
        b: &DVector<f64>,
    ) -> Result<DVector<f64>, String> {
        let qr = SpqrFactorization::new(&a.transpose()).unwrap();

        let q = qr.q_matrix().unwrap();
        let r = qr.r_matrix().unwrap();
        let perm_vec = qr.permutation().unwrap();
        let rank = qr.rank();

        let mut c = DVector::zeros(a.nrows());
        for i in 0..a.nrows() {
            c[i] = b[perm_vec[i]];
        }

        let r_main = r.columns(0, rank);
        let c_main = c.rows(0, rank);

        let y = r_main.transpose().solve_lower_triangular(&c_main).unwrap();

        let x = q * y;

        Ok(x)
    }

    pub fn solve_underconstrained_from_triplets(
        &self,
        triplets: &Vec<(usize, usize, f64)>,
        b: &DVector<f64>,
        m: usize,
        n: usize,
    ) -> Result<DVector<f64>, String> {
        let at_triplets: Vec<(usize, usize, f64)> =
            triplets.par_iter().map(|(i, j, v)| (*j, *i, *v)).collect();

        let qr = SpqrFactorization::new_from_triplets(&at_triplets, n, m).unwrap(); //n, m because transpose

        let q = qr.q_matrix().unwrap();
        let r = qr.r_matrix().unwrap();
        let perm_vec = qr.permutation().unwrap();
        let rank = qr.rank();

        let mut c = DVector::zeros(m);
        for i in 0..m {
            c[i] = b[perm_vec[i]];
        }

        let r_main = r.columns(0, rank);
        let c_main = c.rows(0, rank);

        let y = r_main.transpose().solve_lower_triangular(&c_main).unwrap();

        let x = q * y;

        Ok(x)
    }
}

impl Drop for SpqrFactorization {
    fn drop(&mut self) {
        unsafe {
            if !self.q.is_null() {
                ffi::cholmod_l_free_sparse(&mut self.q, &mut *self.cc);
            }
            if !self.r.is_null() {
                ffi::cholmod_l_free_sparse(&mut self.r, &mut *self.cc);
            }
            if !self.e.is_null() {
                ffi::cholmod_l_free(
                    self.n,
                    std::mem::size_of::<i64>(),
                    self.e as *mut _,
                    &mut *self.cc,
                );
            }

            if !self.cc.is_null() {
                ffi::cholmod_l_finish(&mut *self.cc);
                drop(Box::from_raw(self.cc));
            }
        }
    }
}

// Make it thread-safe
unsafe impl Send for SpqrFactorization {}
unsafe impl Sync for SpqrFactorization {}
