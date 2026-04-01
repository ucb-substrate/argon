use std::ops::{Deref, DerefMut, Mul};

use compiler::solver::{LinearExpr, Var};
use indexmap::IndexMap;

#[derive(Clone, Debug)]
pub struct SparseVec(pub(crate) IndexMap<Var, f64>);

impl Deref for SparseVec {
    type Target = IndexMap<Var, f64>;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl DerefMut for SparseVec {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

/// Removes all components in the directions of `vecs` from `u`.
///
/// Assumes `vecs` is an orthonormal set of vectors.
pub(crate) fn remove_component(u: &SparseVec, vecs: &[SparseVec]) -> SparseVec {
    let mut out = u.clone();
    for v in vecs {
        let dot = dot(u, v);
        v.iter()
            .for_each(|(var, &c)| *out.entry(*var).or_default() -= dot * c);
    }
    out
}

pub(crate) fn dot(a: &SparseVec, b: &SparseVec) -> f64 {
    a.iter()
        .map(|(var, &c)| c * *b.get(var).unwrap_or(&0.))
        .sum()
}

impl From<&Vec<(f64, Var)>> for SparseVec {
    fn from(value: &Vec<(f64, Var)>) -> Self {
        SparseVec(value.iter().map(|(c, v)| (*v, *c)).collect())
    }
}

impl From<&LinearExpr> for SparseVec {
    fn from(value: &LinearExpr) -> Self {
        Self::from(&value.coeffs)
    }
}

impl Mul<f64> for SparseVec {
    type Output = Self;
    fn mul(mut self, rhs: f64) -> Self::Output {
        self.iter_mut().for_each(|(_, v)| *v = *v * rhs);
        self
    }
}
