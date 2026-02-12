#include <Eigen/Sparse>
#include <Eigen/SparseQR>
#include <vector>
#include <iostream>

using namespace Eigen;
using namespace std;

struct EigenQR {
    SparseQR<SparseMatrix<double>, COLAMDOrdering<int>> qr_a;
    SparseQR<SparseMatrix<double>, COLAMDOrdering<int>> qr_at;

    int m;
    int n;

    EigenQR(int num_rows, int num_cols) : m(num_rows), n(num_cols) {}
};

extern "C" {
    void* eigen_create(int m, int n) {
        return new EigenQR(m, n);
    }

    void* eigen_free(void* ptr) {
        if (ptr) {
            delete (EigenQR*) ptr;
        }
    }

    int eigen_compute_qr(void* ptr, int* rows, int* cols, double* vals, int nnz) {
        EigenQR* qr = (EigenQR*) ptr;

        std::vector<Triplet<double>> triplets(nnz);

        for (int i = 0; i < nnz; i++) {
            triplets[i] = Triplet<double>(rows[i], cols[i], vals[i]);
        }

        SparseMatrix<double> A(qr->m, qr->n);
        A.setFromTriplets(triplets.begin(), triplets.end());
        A.makeCompressed();

        qr->qr_a.compute(A);
        if (qr->qr_a.info() != Eigen::Success) {
            return 0;
        }
        qr->qr_at.compute(A.transpose());
        if (qr->qr_at.info() != Eigen::Success) {
            return 0;
        }
        return 1;
    }

    int eigen_get_rank(void* ptr) {
        if (ptr) {
            EigenQR* qr = (EigenQR*) ptr;
            return qr->qr_a.rank();
        }
        return 0;
    }

    void eigen_get_q_dense(void* ptr, double* output, int mode) {
        EigenQR* qr = (EigenQR*) ptr;
        auto& qr_mat = (mode == 0) ? qr->qr_a : qr->qr_at;
        int size = (mode == 0) ? qr->m : qr-> n;

        MatrixXd I = MatrixXd::Identity(size, size);
        MatrixXd Q_dense = qr_mat.matrixQ() * I;

        Map<MatrixXd> ret(output, size, size);
        ret = Q_dense;
    }

    void eigen_get_r_dense(void* ptr, double* output, int mode) {
        EigenQR* qr = (EigenQR*) ptr;
        auto& qr_mat = (mode == 0) ? qr->qr_a : qr->qr_at;
        int r_rows = (mode == 0) ? qr->m : qr->n;
        int r_cols = (mode == 0) ? qr->n : qr->m;

        Map<MatrixXd> ret(output, r_rows, r_cols);
        ret.setZero();

        MatrixXd R_dense = MatrixXd(qr_mat.matrixR());
        int h = std::min((int) R_dense.rows(), r_rows);
        int w = std::min((int) R_dense.cols(), r_cols);
        ret.block(0, 0, h, w) = R_dense.block(0, 0, h, w);
    }

    void eigen_get_permutation(void* ptr, int* output, int mode) {
        EigenQR* qr = (EigenQR*) ptr;
        auto& qr_mat = (mode == 0) ? qr->qr_a : qr->qr_at;
        int size = (mode == 0) ? qr->n : qr->m;

        auto& p = qr_mat.colsPermutation();
        for (int i = 0; i < size; i++) {
            output[i] = p.indices()(i);
        }
    }

    //gives the summed absolute value of each null space basis vector along an index
    void eigen_get_free_indices(void* ptr, double* output) {
        EigenQR* qr = (EigenQR*) ptr;

        Map<VectorXd> ret(output, qr->n);
        ret.setZero();

        if (qr->n - qr->qr_a.rank() <= 0) {
            return;
        }

        VectorXd temp = VectorXd::Zero(qr->n);

        for (int i = qr->qr_a.rank(); i < qr->n; i++) {
            temp.setZero();
            temp(i) = 1.0;

            VectorXd col =  qr->qr_at.matrixQ() * temp;
            ret = ret + col.cwiseAbs();
        }
    }

    void eigen_apply_q(void* ptr, double* input, double* output, int mode_mat_factor, int mode_transpose) {
        EigenQR* qr = (EigenQR*) ptr;
        auto& qr_mat = (mode_mat_factor == 0) ? qr->qr_a : qr->qr_at;
        int size = (mode_mat_factor == 0) ? qr->m : qr->n;

        Map<VectorXd> in_vec(input, size);
        Map<VectorXd> out_vec(output, size);

        VectorXd temp;

        if (mode_transpose == 0) {
            temp = qr_mat.matrixQ().transpose() * in_vec;
        } else {
            temp = qr_mat.matrixQ() * in_vec;
        }

        out_vec = temp;
    }
}
