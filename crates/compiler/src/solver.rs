use approx::{relative_eq, relative_ne};
use indexmap::{IndexMap, IndexSet};
use itertools::{Either, Itertools, multiunzip};
use nalgebra::{CsMatrix, DMatrix, DVector};
use serde::{Deserialize, Serialize};
use sparse_linear_solver::{analyze as analyze_sparse_system, nullspace as sparse_nullspace};
use std::collections::VecDeque;
use std::hash::BuildHasherDefault;

use rustc_hash::FxHasher;
use smallvec::SmallVec;

type FxIndexMap<K, V> = IndexMap<K, V, BuildHasherDefault<FxHasher>>;
type FxIndexSet<T> = IndexSet<T, BuildHasherDefault<FxHasher>>;

/// Cumulative `solve()` accounting, for attributing compile time between
/// evaluation and the linear solver.
///
/// A thread-local rather than a field on [`Solver`], because each cell owns its
/// own solver and the question being asked is about a whole compile.
#[cfg(test)]
pub mod solve_stats {
    use std::cell::Cell;

    thread_local! {
        static CALLS: Cell<u64> = const { Cell::new(0) };
        static BODY_CALLS: Cell<u64> = const { Cell::new(0) };
        static NANOS: Cell<u128> = const { Cell::new(0) };
    }

    pub fn reset() {
        CALLS.set(0);
        BODY_CALLS.set(0);
        NANOS.set(0);
    }

    /// `(calls, calls that got past the early return, nanoseconds)`.
    pub fn read() -> (u64, u64, u128) {
        (CALLS.get(), BODY_CALLS.get(), NANOS.get())
    }

    pub(super) fn record(entered_body: bool, nanos: u128) {
        CALLS.set(CALLS.get() + 1);
        if entered_body {
            BODY_CALLS.set(BODY_CALLS.get() + 1);
        }
        NANOS.set(NANOS.get() + nanos);
    }
}

/// Records one `solve()` call on drop, so an early return is still counted.
#[cfg(test)]
struct SolveAccounting {
    started: std::time::Instant,
    entered_body: bool,
}

#[cfg(test)]
impl Drop for SolveAccounting {
    fn drop(&mut self) {
        solve_stats::record(self.entered_body, self.started.elapsed().as_nanos());
    }
}

const EPSILON: f64 = 1e-8;
const DEFAULT_GRID: f64 = 0.1;

fn is_off_grid(value: f64, snapped: f64, grid: f64) -> bool {
    // Keep solver and floating-point noise from becoming a diagnostic. The
    // meaningful tolerance is a fraction of the grid, not of the coordinate.
    let tolerance = EPSILON * grid.abs() + f64::EPSILON * value.abs() * 8.;
    (value - snapped).abs() > tolerance
}

#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq, Serialize, Deserialize, Ord, PartialOrd)]
pub struct Var(u64);

/// The values of solved variables. A solver numbers its variables densely and
/// solves almost all of them, so this is indexed by variable.
#[derive(Clone, Default)]
struct SolvedValues(Vec<Option<f64>>);

impl SolvedValues {
    fn get(&self, var: &Var) -> Option<&f64> {
        self.0.get(var.0 as usize)?.as_ref()
    }

    fn contains_key(&self, var: &Var) -> bool {
        self.get(var).is_some()
    }

    fn insert(&mut self, var: Var, value: f64) -> Option<f64> {
        let index = var.0 as usize;
        if index >= self.0.len() {
            self.0.resize(index + 1, None);
        }
        self.0[index].replace(value)
    }
}

/// A set of variables in insertion order, where removing one moves the last
/// into its place, as `IndexSet::swap_remove` does. Indexed by variable.
#[derive(Clone, Default)]
pub struct VarSet {
    vars: Vec<Var>,
    /// One more than each variable's position in `vars`, or 0 when absent.
    positions: Vec<u32>,
}

impl VarSet {
    fn position(&self, var: &Var) -> Option<usize> {
        match self.positions.get(var.0 as usize) {
            Some(&p) if p > 0 => Some(p as usize - 1),
            _ => None,
        }
    }

    pub fn contains(&self, var: &Var) -> bool {
        self.position(var).is_some()
    }

    pub fn insert(&mut self, var: Var) -> bool {
        if self.contains(&var) {
            return false;
        }
        let index = var.0 as usize;
        if index >= self.positions.len() {
            self.positions.resize(index + 1, 0);
        }
        self.vars.push(var);
        self.positions[index] = self.vars.len() as u32;
        true
    }

    pub fn swap_remove(&mut self, var: &Var) -> bool {
        let Some(position) = self.position(var) else {
            return false;
        };
        self.positions[var.0 as usize] = 0;
        self.vars.swap_remove(position);
        if let Some(moved) = self.vars.get(position) {
            self.positions[moved.0 as usize] = position as u32 + 1;
        }
        true
    }

    pub fn len(&self) -> usize {
        self.vars.len()
    }

    pub fn is_empty(&self) -> bool {
        self.vars.is_empty()
    }

    pub fn iter(&self) -> std::slice::Iter<'_, Var> {
        self.vars.iter()
    }
}

impl<'a> IntoIterator for &'a VarSet {
    type Item = &'a Var;
    type IntoIter = std::slice::Iter<'a, Var>;

    fn into_iter(self) -> Self::IntoIter {
        self.vars.iter()
    }
}

/// What [`Solver::constrain_eq`] did with a constraint.
pub enum Constrained {
    /// Solved its one unknown at once, without recording it; whether the
    /// value was off the grid.
    AtOnce { off_grid: bool },
    /// Recorded it, under this ID.
    Recorded(ConstraintId),
}

#[derive(Clone)]
pub struct Solver {
    grid: f64,
    next_var: u64,
    next_constraint: ConstraintId,
    constraints: FxIndexMap<ConstraintId, LinearExpr>,
    var_to_constraints: FxIndexMap<Var, ConstraintSet>,
    // Solved and unsolved vars are separate to reduce overhead of many solved variables.
    solved_vars: SolvedValues,
    unsolved_vars: VarSet,
    /// Variables solved since the last [`Self::clear_updated_vars`], in the
    /// order they were solved. A variable is solved at most once.
    updated_vars: Vec<Var>,
    back_substitute_stack: Vec<ConstraintId>,
    inconsistent_constraints: FxIndexSet<ConstraintId>,
    /// Variables whose solved value missed the grid, with the value the
    /// solver computed. Rounding each variable in isolation can break a
    /// constraint that couples several of them, so the diagnostic needs the
    /// number, not just the fact that a miss happened.
    off_grid_vars: FxIndexMap<Var, f64>,
    // Per-`solve()` scratch for the sparse elimination pre-pass (`eliminate_definitional`).
    // `elim_worklist` holds constraints to (re)examine for a small pivot; `substitutions`
    // records `var = expr` definitions for variables eliminated via a 2-variable
    // constraint, resolved into numbers afterwards by `resolve_substitutions`. Both are
    // cleared at the start of each elimination pass, so they hold no state between solves.
    elim_worklist: VecDeque<ConstraintId>,
    substitutions: Vec<(ConstraintId, Var, LinearExpr)>,
    /// Null-space vectors produced while analyzing the current sparse
    /// components. Kept only when no elimination substitution changed the
    /// coordinate space, so SSE can reuse the factorization result.
    sparse_nullspace_cache: Option<Vec<Vec<(f64, Var)>>>,
}

impl Default for Solver {
    fn default() -> Self {
        Self {
            grid: DEFAULT_GRID,
            next_var: 0,
            next_constraint: 0,
            constraints: FxIndexMap::default(),
            var_to_constraints: FxIndexMap::default(),
            solved_vars: SolvedValues::default(),
            unsolved_vars: VarSet::default(),
            updated_vars: Vec::new(),
            back_substitute_stack: Vec::new(),
            inconsistent_constraints: FxIndexSet::default(),
            off_grid_vars: FxIndexMap::default(),
            elim_worklist: VecDeque::new(),
            substitutions: Vec::new(),
            sparse_nullspace_cache: None,
        }
    }
}

impl Solver {
    pub fn new() -> Self {
        Default::default()
    }

    pub fn with_grid(grid: f64) -> Self {
        assert!(
            grid.is_finite() && grid > 0.,
            "solver grid must be positive and finite"
        );
        Self {
            grid,
            ..Self::default()
        }
    }

    pub fn new_var(&mut self) -> Var {
        let var = Var(self.next_var);
        self.unsolved_vars.insert(var);
        self.next_var += 1;
        var
    }

    /// Returns true if all variables have been solved.
    pub fn fully_solved(&self) -> bool {
        self.unsolved_vars.is_empty()
    }

    /// Labels every unsolved variable with a representative of its connected
    /// component in the *live* constraint graph.
    ///
    /// Two variables share a label exactly when a chain of live constraints
    /// connects them, so pinning one can only ever change the solved-ness of
    /// variables carrying the same label; a variable in no live constraint is
    /// its own label. That is what lets a caller apply several pending
    /// constraints in one round, as long as their variables carry disjoint
    /// labels.
    pub fn unsolved_var_components(&self) -> FxIndexMap<Var, Var> {
        let mut labels = FxIndexMap::with_capacity_and_hasher(
            self.unsolved_vars.len(),
            BuildHasherDefault::default(),
        );
        let mut queue = VecDeque::new();
        for &root in &self.unsolved_vars {
            if labels.contains_key(&root) {
                continue;
            }
            labels.insert(root, root);
            queue.clear();
            queue.push_back(root);
            while let Some(var) = queue.pop_front() {
                let Some(constraints) = self.var_to_constraints.get(&var) else {
                    continue;
                };
                for constraint_id in constraints {
                    // `var_to_constraints` is not unlinked when a constraint is
                    // consumed by back-substitution, so it can name ids that
                    // are no longer live. Only live constraints connect.
                    let Some(constraint) = self.constraints.get(constraint_id) else {
                        continue;
                    };
                    for &(_, next) in &constraint.coeffs {
                        if self.unsolved_vars.contains(&next) && labels.insert(next, root).is_none()
                        {
                            queue.push_back(next);
                        }
                    }
                }
            }
        }
        labels
    }

    /// The component labels of `expr`'s unsolved variables, using the same
    /// coefficient threshold as [`Self::has_unsolved_var`] so that the two
    /// always agree on which variables an expression actually determines.
    pub fn unsolved_component_labels(
        &self,
        expr: &LinearExpr,
        labels: &FxIndexMap<Var, Var>,
    ) -> Vec<Var> {
        expr.coeffs
            .iter()
            .filter(|(coeff, var)| coeff.abs() > 1e-6 && !self.is_solved(*var))
            .map(|(_, var)| labels.get(var).copied().unwrap_or(*var))
            .collect()
    }

    /// Pins whatever the source left free, so that a cell nobody finished
    /// constraining still produces a layout.
    ///
    /// Pins one variable per *connected component* per round, so a
    /// whole-system [`Self::solve`] runs once per round rather than once per
    /// pinned variable. Components are independent by construction -- see
    /// [`Self::unsolved_var_components`] -- so the fixed point reached is the
    /// same one pinning them singly reaches.
    pub fn force_solution(&mut self) {
        while !self.fully_solved() {
            let labels = self.unsolved_var_components();
            let mut claimed = FxIndexSet::default();
            let mut pinned = false;
            // Collected rather than cloning the `IndexSet`, which would
            // rebuild its hash table for a list this only iterates.
            let candidates = self.unsolved_vars.iter().copied().collect::<Vec<_>>();
            for var in candidates {
                // `constrain_eq0` back-substitutes eagerly, so pinning one
                // variable can solve others in the same component before this
                // loop reaches them.
                if self.is_solved(var) {
                    continue;
                }
                let label = labels.get(&var).copied().unwrap_or(var);
                if !claimed.insert(label) {
                    continue;
                }
                self.constrain_eq0(LinearExpr::from(var));
                pinned = true;
            }
            // Every remaining variable belonged to a component already pinned
            // this round; without this the loop could not make progress and
            // would spin.
            if !pinned {
                break;
            }
            self.solve();
        }
    }

    #[inline]
    pub fn inconsistent_constraints(&self) -> &FxIndexSet<ConstraintId> {
        &self.inconsistent_constraints
    }

    #[inline]
    pub fn updated_vars(&self) -> &[Var] {
        &self.updated_vars
    }

    #[inline]
    pub fn clear_updated_vars(&mut self) {
        self.updated_vars.clear()
    }

    #[inline]
    pub fn off_grid_vars(&self) -> &FxIndexMap<Var, f64> {
        &self.off_grid_vars
    }

    /// The snap-grid spacing this solver rounds solved values to, in source
    /// coordinate units.
    #[inline]
    pub fn grid(&self) -> f64 {
        self.grid
    }

    pub fn unsolved_vars(&self) -> &VarSet {
        &self.unsolved_vars
    }

    pub fn solve_var(&mut self, var: Var, val: f64) {
        let old = self.solved_vars.insert(var, val);
        if old.is_none() {
            self.updated_vars.push(var);
        }
        self.unsolved_vars.swap_remove(&var);
    }

    /// Constrains the value of `expr` to 0.
    /// TODO: Check if added constraints conflict with existing solution.
    pub fn constrain_eq0(&mut self, expr: LinearExpr) -> ConstraintId {
        let id = self.next_constraint;
        self.next_constraint += 1;
        if self.solve_at_once(&expr, &LinearExpr::from(0.)).is_none() {
            self.record_eq0(id, expr);
        }
        id
    }

    /// Constrains `lhs - rhs` to 0, as [`Self::constrain_eq0`] would, but
    /// without building the difference when the constraint is solved at once.
    pub fn constrain_eq(&mut self, lhs: &LinearExpr, rhs: &LinearExpr) -> Constrained {
        let id = self.next_constraint;
        self.next_constraint += 1;
        match self.solve_at_once(lhs, rhs) {
            Some(off_grid) => Constrained::AtOnce { off_grid },
            None => {
                self.record_eq0(id, LinearExpr::difference(lhs, rhs));
                Constrained::Recorded(id)
            }
        }
    }

    /// When back substitution would leave the constraint `lhs - rhs = 0` with
    /// one term, solves that term's variable as it would, and goes on to the
    /// constraints the variable is in; such a constraint is then dropped, so it
    /// is never recorded. Returns whether the value was off the grid, or `None`
    /// if the constraint has to be recorded.
    fn solve_at_once(&mut self, lhs: &LinearExpr, rhs: &LinearExpr) -> Option<bool> {
        // Each term as `LinearExpr::simplify` sees it: a value it removes, or
        // the one variable it keeps.
        let mut terms = SmallVec::<[Result<f64, (f64, Var)>; 4]>::new();
        let mut unknown = None;
        for (coeff, var) in lhs
            .coeffs
            .iter()
            .copied()
            .chain(rhs.coeffs.iter().map(|(c, v)| (-c, *v)))
        {
            let term = if relative_eq!(coeff, 0., epsilon = EPSILON) {
                Ok(0.)
            } else {
                match self.solved_vars.get(&var) {
                    Some(value) => Ok(coeff * value),
                    None if unknown.is_none() => {
                        unknown = Some((coeff, var));
                        Err((coeff, var))
                    }
                    None => return None,
                }
            };
            terms.push(term);
        }
        let (coeff, var) = unknown?;
        // The removed terms, summed left to right, are added to the constant.
        let mut removed: Option<f64> = None;
        for term in terms.into_iter().flatten() {
            removed = Some(removed.map_or(term, |sum| sum + term));
        }
        let constant = (lhs.constant - rhs.constant) + removed.unwrap_or(0.);
        Some(self.solve_term(var, coeff, constant))
    }

    /// Solves `var`, which is not solved yet, from `var = rhs` as
    /// [`Self::constrain_eq`] would, but without looking `var` up. Returns
    /// whether the value was off the grid, or `None` when `rhs` is not known
    /// yet, in which case nothing changes.
    pub fn solve_fresh(&mut self, var: Var, rhs: &LinearExpr) -> Option<bool> {
        let mut removed: Option<f64> = None;
        for (coeff, solved) in rhs.coeffs.iter() {
            let coeff = -coeff;
            let term = if relative_eq!(coeff, 0., epsilon = EPSILON) {
                0.
            } else {
                coeff * self.solved_vars.get(solved)?
            };
            removed = Some(removed.map_or(term, |sum| sum + term));
        }
        self.next_constraint += 1;
        let constant = (0. - rhs.constant) + removed.unwrap_or(0.);
        Some(self.solve_term(var, 1., constant))
    }

    /// Solves `var` from the constraint `coeff * var + constant = 0` as back
    /// substitution would, and goes on to the constraints `var` is in.
    /// Returns whether the value was off the grid.
    fn solve_term(&mut self, var: Var, coeff: f64, constant: f64) -> bool {
        let val = -constant / coeff;
        let rounded_val = crate::tech::snap(val, self.grid);
        let off_grid = is_off_grid(val, rounded_val, self.grid);
        if off_grid {
            self.off_grid_vars.insert(var, val);
        }
        self.solve_var(var, rounded_val);
        if let Some(constraints) = self.var_to_constraints.get(&var) {
            self.back_substitute_stack
                .extend(constraints.iter().copied());
            while !self.back_substitute_stack.is_empty() {
                self.try_back_substitute();
            }
        }
        off_grid
    }

    /// Records the constraint `id`, `expr = 0`, and back-substitutes from it.
    fn record_eq0(&mut self, id: ConstraintId, expr: LinearExpr) {
        for (_, var) in &expr.coeffs {
            self.var_to_constraints.entry(*var).or_default().insert(id);
        }
        self.constraints.insert(id, expr);
        // Use explicit stack in heap-allocated vector to avoid stack overflow.
        self.back_substitute_stack.push(id);
        while !self.back_substitute_stack.is_empty() {
            self.try_back_substitute();
        }
    }

    /// Whether the constraint `id` is still in the system.
    pub fn is_live(&self, id: ConstraintId) -> bool {
        self.constraints.contains_key(&id)
    }

    // Tries to back substitute using the given [`ConstraintId`].
    pub fn try_back_substitute(&mut self) {
        // If coefficient length is not 1, do nothing.
        if let Some(id) = self.back_substitute_stack.pop()
            && let Some(constraint) = self.constraints.get_mut(&id)
        {
            constraint.simplify(&self.solved_vars);
            if constraint.coeffs.is_empty()
                && !relative_eq!(constraint.constant, 0., epsilon = EPSILON)
            {
                self.inconsistent_constraints.insert(id);
                self.constraints.swap_remove(&id);
                return;
            }
            if constraint.coeffs.len() != 1 {
                return;
            }
            // If constraint solves a variable, insert it into the solved vars and traverse all
            // constraints involving the variable.
            let (coeff, var) = constraint.coeffs[0];
            let val = -constraint.constant / coeff;
            if let Some(old_val) = self.solved_vars.get(&var) {
                if relative_ne!(*old_val, val, epsilon = EPSILON) {
                    self.inconsistent_constraints.insert(id);
                }
            } else {
                let rounded_val = crate::tech::snap(val, self.grid);
                if is_off_grid(val, rounded_val, self.grid) {
                    self.off_grid_vars.insert(var, val);
                }
                self.solve_var(var, rounded_val);
            }
            self.constraints.swap_remove(&id);
            if let Some(constraints) = self.var_to_constraints.get(&var) {
                self.back_substitute_stack
                    .extend(constraints.iter().copied());
            }
        }
    }

    /// Solves for as many variables as possible and substitutes their values into existing constraints.
    /// Deletes constraints that no longer contain unsolved variables.
    ///
    /// Constraints should be simplified before this function is invoked.
    pub fn solve(&mut self) {
        #[cfg(test)]
        let started = std::time::Instant::now();
        let entered_body = !(self.unsolved_vars.is_empty() || self.constraints.is_empty());
        #[cfg(test)]
        let _guard = SolveAccounting {
            started,
            entered_body,
        };
        if !entered_body {
            return;
        }
        // Sparsity-exploiting pre-pass: peel off variables that are uniquely defined
        // by a constraint of size <= 2 (generalizing 1-variable back-substitution),
        // shrinking the system before sparse analysis or the dense fallback. Variables eliminated via a
        // 2-variable constraint are expressed in terms of another variable and recorded
        // in `self.substitutions`; their numeric values are recovered by
        // `resolve_substitutions` once the remaining (irreducible) core has been solved.
        // For systems whose constraints are all <= 2 variables (e.g. the coupled ring in
        // `bench_constraints`) this resolves everything in O(n) and the SVD never runs;
        // for a genuinely dense block it is a no-op and behaviour is identical to before.
        self.eliminate_definitional();
        let substitutions_changed_coordinates = !self.substitutions.is_empty();
        self.sparse_nullspace_cache = Some(Vec::new());

        for component in self.constraint_components() {
            self.solve_component(&component.vars, &component.constraints);
        }
        for (id, constraint) in self.constraints.iter_mut() {
            constraint.simplify(&self.solved_vars);
            if constraint.coeffs.is_empty()
                && approx::relative_ne!(constraint.constant, 0., epsilon = EPSILON)
            {
                self.inconsistent_constraints.insert(*id);
            }
        }
        self.constraints
            .retain(|_, constraint| !constraint.coeffs.is_empty());

        self.resolve_substitutions();
        if substitutions_changed_coordinates {
            self.sparse_nullspace_cache = None;
        }
    }

    /// Sparse elimination pre-pass. Repeatedly examines constraints with at most two
    /// variables and uses each to eliminate one of its variables, substituting it out
    /// of the (few) other constraints that mention it. Because a size-2 constraint
    /// expresses a variable as (one variable + constant), substitution replaces one
    /// term with one term and so never increases any constraint's variable count: the
    /// system only shrinks, and the pass runs in O(nnz). Constraints with > 2 variables
    /// are left untouched for the dense path in `solve_component`.
    fn eliminate_definitional(&mut self) {
        self.substitutions.clear();
        self.elim_worklist.clear();
        self.elim_worklist.extend(self.constraints.keys().copied());
        while let Some(id) = self.elim_worklist.pop_front() {
            let Some(constraint) = self.constraints.get_mut(&id) else {
                continue;
            };
            constraint.simplify(&self.solved_vars);
            coalesce_terms(constraint);
            let len = constraint.coeffs.len();
            let constant = constraint.constant;
            match len {
                0 => {
                    if relative_ne!(constant, 0., epsilon = EPSILON) {
                        self.inconsistent_constraints.insert(id);
                    }
                    self.remove_constraint(id);
                }
                1 => self.eliminate_unary(id),
                2 => self.eliminate_binary(id),
                _ => {}
            }
        }
    }

    /// Removes a constraint from the system and unlinks it from `var_to_constraints`.
    fn remove_constraint(&mut self, id: ConstraintId) {
        if let Some(constraint) = self.constraints.swap_remove(&id) {
            for (_, var) in &constraint.coeffs {
                if let Some(set) = self.var_to_constraints.get_mut(var) {
                    set.swap_remove(&id);
                }
            }
        }
    }

    /// Re-queues every still-live constraint mentioning `var`; they may have just
    /// shrunk to a size the pre-pass can act on.
    fn requeue_neighbors(&mut self, var: Var) {
        if let Some(neighbors) = self.var_to_constraints.get(&var) {
            self.elim_worklist.extend(neighbors.iter().copied());
        }
    }

    /// Grounds the single variable of a 1-variable constraint (post-simplify). The
    /// 1-variable analogue of `eliminate_binary`; mirrors `try_back_substitute`.
    fn eliminate_unary(&mut self, id: ConstraintId) {
        let (coeff, var) = self.constraints[&id].coeffs[0];
        let val = -self.constraints[&id].constant / coeff;
        self.remove_constraint(id);
        self.requeue_neighbors(var);
        self.assign_var(var, val);
    }

    /// Eliminates one variable of a 2-variable constraint (post-simplify) by expressing
    /// it in terms of the other and substituting it out of every other constraint that
    /// mentions it. Records the definition in `substitutions` for later resolution.
    fn eliminate_binary(&mut self, id: ConstraintId) {
        let (c0, v0) = self.constraints[&id].coeffs[0];
        let (c1, v1) = self.constraints[&id].coeffs[1];
        let constant = self.constraints[&id].constant;
        // Pivot on the larger-magnitude coefficient (partial-pivoting analogue).
        let ((a, v), (cw, w)) = if c0.abs() >= c1.abs() {
            ((c0, v0), (c1, v1))
        } else {
            ((c1, v1), (c0, v0))
        };
        if a.abs() <= EPSILON || v == w {
            return;
        }
        // From `a*v + cw*w + constant = 0`: v = (-cw/a) * w + (-constant/a).
        let v_expr = LinearExpr {
            coeffs: Terms::one(-cw / a, w),
            constant: -constant / a,
        };
        self.remove_constraint(id);
        let neighbors: SmallVec<[ConstraintId; 8]> = self
            .var_to_constraints
            .get(&v)
            .into_iter()
            .flatten()
            .copied()
            .collect();
        for nid in neighbors {
            let Some(constraint) = self.constraints.get_mut(&nid) else {
                continue;
            };
            substitute_var(constraint, v, &v_expr);
            self.var_to_constraints.entry(w).or_default().insert(nid);
            self.elim_worklist.push_back(nid);
        }
        self.var_to_constraints.swap_remove(&v);
        self.unsolved_vars.swap_remove(&v);
        self.substitutions.push((id, v, v_expr));
    }

    /// Recovers numeric values for variables eliminated by `eliminate_binary`. Walks
    /// `substitutions` in reverse (reverse-topological) order: by the time each entry is
    /// reached, every variable its expression depends on is either solved by the core
    /// SVD / back-substitution or resolved earlier in this walk. A variable whose
    /// expression does not become ground belongs to an under-determined component; its
    /// defining constraint is restored (a row-equivalent of the pivot row that was
    /// removed) so the under-constrained diagnostics in `rowspace_vecs` are unchanged.
    fn resolve_substitutions(&mut self) {
        while let Some((id, var, mut expr)) = self.substitutions.pop() {
            expr.simplify(&self.solved_vars);
            if expr.coeffs.is_empty() {
                self.assign_var(var, expr.constant);
            } else {
                self.unsolved_vars.insert(var);
                let mut coeffs = Terms::with_capacity(expr.coeffs.len() + 1);
                coeffs.push((1., var));
                for (c, v) in expr.coeffs {
                    coeffs.push((-c, v));
                }
                let constraint = LinearExpr {
                    coeffs,
                    constant: -expr.constant,
                };
                for (_, v) in &constraint.coeffs {
                    self.var_to_constraints.entry(*v).or_default().insert(id);
                }
                self.constraints.insert(id, constraint);
            }
        }
    }

    /// Rounds `val` to the solver grid, flags off-grid values, and records the solution.
    /// Shared by the elimination pre-pass; matches the rounding contract used by
    /// `try_back_substitute` and `solve_component`.
    fn assign_var(&mut self, var: Var, val: f64) {
        if self.solved_vars.contains_key(&var) {
            return;
        }
        let rounded = crate::tech::snap(val, self.grid);
        if is_off_grid(val, rounded, self.grid) {
            self.off_grid_vars.insert(var, val);
        }
        self.solve_var(var, rounded);
    }

    pub fn rowspace_vecs(&mut self) -> Vec<Vec<(f64, Var)>> {
        if self.unsolved_vars.is_empty() || self.constraints.is_empty() {
            return Vec::new();
        }
        self.constraint_components()
            .into_iter()
            .flat_map(|component| {
                self.rowspace_component_vecs(&component.vars, &component.constraints)
            })
            .collect()
    }

    /// Computes an orthonormal null-space basis directly from sparse QR. A
    /// `None` result lets callers retain the dense row-space fallback for inputs
    /// outside the sparse solver's scope.
    pub fn sparse_nullspace_vecs(&self) -> Option<Vec<Vec<(f64, Var)>>> {
        if let Some(cached) = &self.sparse_nullspace_cache {
            let mut output = cached.clone();
            self.append_unconstrained_nullspace_vectors(&mut output);
            return Some(output);
        }
        if self.unsolved_vars.is_empty() {
            return Some(Vec::new());
        }
        let mut output = Vec::new();
        for component in self.constraint_components() {
            let var_indices: FxIndexMap<Var, usize> = FxIndexMap::from_iter(
                component
                    .vars
                    .iter()
                    .enumerate()
                    .map(|(index, var)| (*var, index)),
            );
            let rows: Vec<Vec<(usize, f64)>> = component
                .constraints
                .iter()
                .map(|id| {
                    self.constraints[id]
                        .coeffs
                        .iter()
                        .filter_map(|(value, var)| {
                            var_indices.get(var).map(|&column| (column, *value))
                        })
                        .collect()
                })
                .collect();
            let basis = sparse_nullspace(component.vars.len(), &rows)?;
            output.extend(basis.into_iter().map(|vector| {
                vector
                    .into_iter()
                    .map(|(column, value)| (value, component.vars[column]))
                    .collect()
            }));
        }
        self.append_unconstrained_nullspace_vectors(&mut output);
        Some(output)
    }

    /// Variables absent from every active constraint are independent null-space
    /// directions and therefore need explicit unit vectors for SSE dragging.
    fn append_unconstrained_nullspace_vectors(&self, output: &mut Vec<Vec<(f64, Var)>>) {
        output.extend(self.unsolved_vars.iter().filter_map(|var| {
            let constrained = self
                .var_to_constraints
                .get(var)
                .is_some_and(|ids| ids.iter().any(|id| self.constraints.contains_key(id)));
            (!constrained).then_some(vec![(1., *var)])
        }));
    }

    pub fn value_of(&self, var: Var) -> Option<f64> {
        self.solved_vars.get(&var).copied()
    }

    pub fn is_solved(&self, var: Var) -> bool {
        self.solved_vars.contains_key(&var)
    }

    /// Whether `expr` still mentions a variable this solver has not determined.
    ///
    /// Both kinds of deferred constraint -- author-written initial conditions
    /// and compiler defaults -- use this to decide whether applying themselves
    /// would still change anything.
    pub fn has_unsolved_var(&self, expr: &LinearExpr) -> bool {
        expr.coeffs
            .iter()
            .any(|(coeff, var)| coeff.abs() > 1e-6 && !self.is_solved(*var))
    }

    /// Whether every constraint in a component is free of non-finite values.
    fn component_is_finite(&self, constraints: &[ConstraintId]) -> bool {
        constraints
            .iter()
            .all(|id| self.constraints[id].is_finite())
    }

    /// Evaluates `expr` against the solved variables, without snapping.
    ///
    /// Use this wherever the result is a plain scalar rather than a coordinate
    /// that will be emitted -- notably the operands of `*` and `/`. The
    /// manufacturing grid quantizes *positions*, so applying it to a scalar is
    /// a category error: on a 5 DBU grid the literal `2.` snaps to `0.`, which
    /// silently turns `(a + b)/2.` into a division by zero and poisons the
    /// constraint with `inf` coefficients.
    pub fn eval_expr_exact(&self, expr: &LinearExpr) -> Option<f64> {
        Some(
            expr.coeffs
                .iter()
                .map(|(coeff, var)| self.value_of(*var).map(|val| val * coeff))
                .fold_options(0., |a, b| a + b)?
                + expr.constant,
        )
    }

    /// Evaluates `expr` as a coordinate, snapped to the manufacturing grid.
    pub fn eval_expr(&self, expr: &LinearExpr) -> Option<f64> {
        Some(crate::tech::snap(self.eval_expr_exact(expr)?, self.grid))
    }

    fn solve_component(&mut self, vars: &FxIndexSet<Var>, constraints: &[ConstraintId]) {
        let n_vars = vars.len();
        if n_vars == 0 || constraints.is_empty() {
            return;
        }
        // The sparse solver rejects non-finite input, which is exactly what
        // routes a component here; the dense fallback has no such guard and
        // would never converge. Leave the component unsolved instead, so it
        // surfaces as an ordinary diagnostic.
        if !self.component_is_finite(constraints) {
            return;
        }
        if self.try_solve_sparse_component(vars, constraints) {
            return;
        }
        self.sparse_nullspace_cache = None;
        let var_indices: FxIndexMap<Var, usize> =
            FxIndexMap::from_iter(vars.iter().enumerate().map(|(i, var)| (*var, i)));
        let (i, j, val): (Vec<_>, Vec<_>, Vec<_>) =
            multiunzip(constraints.iter().enumerate().flat_map(|(row, id)| {
                self.constraints[id].coeffs.iter().map({
                    let var_indices = &var_indices;
                    move |(coeff, var)| (row, var_indices[var], *coeff)
                })
            }));
        let a = DMatrix::from(CsMatrix::from_triplet(
            constraints.len(),
            n_vars,
            &i,
            &j,
            &val,
        ));
        let b = DVector::from_iterator(
            constraints.len(),
            constraints.iter().map(|id| -self.constraints[id].constant),
        );
        let svd = a.svd(true, true);
        let vt = svd.v_t.as_ref().expect("No V^T matrix");
        let r = svd.rank(EPSILON);
        if r == 0 {
            return;
        }
        let sol = svd.solve(&b, EPSILON).unwrap();

        for (i, var) in vars.iter().enumerate() {
            let recons = (0..r)
                .map(|row| {
                    let coeff = vt[(row, i)];
                    coeff * coeff
                })
                .sum::<f64>();
            if relative_eq!(recons, 1., epsilon = EPSILON) {
                let val = sol[(i, 0)];
                let rounded_val = crate::tech::snap(val, self.grid);
                if is_off_grid(val, rounded_val, self.grid) {
                    self.off_grid_vars.insert(*var, val);
                }
                self.solve_var(*var, rounded_val);
            }
        }
    }

    /// Attempts to solve a sparse full-column-rank component without ever
    /// materializing a dense matrix, using fill-reducing sparse QR. For a
    /// rank-deficient component, its sparse null-space basis identifies which
    /// variables are uniquely determined by the CGLS particular solution.
    fn try_solve_sparse_component(
        &mut self,
        vars: &FxIndexSet<Var>,
        constraints: &[ConstraintId],
    ) -> bool {
        let var_indices: FxIndexMap<Var, usize> =
            FxIndexMap::from_iter(vars.iter().enumerate().map(|(i, var)| (*var, i)));
        let rows: Vec<Vec<(usize, f64)>> = constraints
            .iter()
            .map(|id| {
                self.constraints[id]
                    .coeffs
                    .iter()
                    .filter_map(|(value, var)| var_indices.get(var).map(|&column| (column, *value)))
                    .collect()
            })
            .collect();
        let rhs: Vec<f64> = constraints
            .iter()
            .map(|id| -self.constraints[id].constant)
            .collect();
        let Some(analysis) = analyze_sparse_system(vars.len(), &rows, &rhs) else {
            return false;
        };

        if let Some(cache) = &mut self.sparse_nullspace_cache {
            cache.extend(analysis.nullspace.iter().map(|vector| {
                vector
                    .iter()
                    .map(|&(column, value)| (value, vars[column]))
                    .collect()
            }));
        }

        for (column, (&var, &value)) in vars.iter().zip(&analysis.solution).enumerate() {
            let determined = analysis
                .nullspace
                .iter()
                .all(|vector| vector.iter().all(|&(index, _)| index != column));
            if determined {
                self.assign_var(var, value);
            }
        }
        true
    }

    fn rowspace_component_vecs(
        &self,
        vars: &FxIndexSet<Var>,
        constraints: &[ConstraintId],
    ) -> Vec<Vec<(f64, Var)>> {
        let n_vars = vars.len();
        if n_vars == 0 || constraints.is_empty() {
            return Vec::new();
        }
        // See `solve_component`: a non-finite entry hangs the dense SVD.
        if !self.component_is_finite(constraints) {
            return Vec::new();
        }
        let var_indices: FxIndexMap<Var, usize> =
            FxIndexMap::from_iter(vars.iter().enumerate().map(|(i, var)| (*var, i)));
        let (i, j, val): (Vec<_>, Vec<_>, Vec<_>) =
            multiunzip(constraints.iter().enumerate().flat_map(|(row, id)| {
                self.constraints[id].coeffs.iter().map({
                    let var_indices = &var_indices;
                    move |(coeff, var)| (row, var_indices[var], *coeff)
                })
            }));
        let a = DMatrix::from(CsMatrix::from_triplet(
            constraints.len(),
            n_vars,
            &i,
            &j,
            &val,
        ));
        let svd = a.svd(false, true);
        let vt = svd.v_t.as_ref().expect("No V^T matrix");
        let r = svd.rank(EPSILON);

        (0..r)
            .map(|i| {
                vars.iter()
                    .enumerate()
                    .filter_map(|(j, v)| {
                        let coeff = vt[(i, j)];
                        if relative_ne!(coeff, 0., epsilon = EPSILON) {
                            Some((coeff, *v))
                        } else {
                            None
                        }
                    })
                    .collect::<Vec<_>>()
            })
            .collect()
    }

    fn constraint_components(&self) -> Vec<ConstraintComponent> {
        let mut visited_vars = FxIndexSet::default();
        let mut visited_constraints = FxIndexSet::default();
        let mut components = Vec::new();
        let mut queue = VecDeque::new();

        for &root_var in &self.unsolved_vars {
            if !visited_vars.insert(root_var) {
                continue;
            }
            // A variable in no live, unvisited constraint is a component
            // without constraints, which is not reported.
            let starts_component = self.var_to_constraints.get(&root_var).is_some_and(|ids| {
                ids.iter().any(|id| {
                    self.constraints.contains_key(id) && !visited_constraints.contains(id)
                })
            });
            if !starts_component {
                continue;
            }
            queue.clear();
            queue.push_back(root_var);
            let mut vars = FxIndexSet::from_iter([root_var]);
            let mut constraints = Vec::new();

            while let Some(var) = queue.pop_front() {
                if let Some(var_constraints) = self.var_to_constraints.get(&var) {
                    for &constraint_id in var_constraints {
                        if !self.constraints.contains_key(&constraint_id)
                            || !visited_constraints.insert(constraint_id)
                        {
                            continue;
                        }
                        constraints.push(constraint_id);
                        for &(_, next_var) in &self.constraints[&constraint_id].coeffs {
                            if self.unsolved_vars.contains(&next_var)
                                && visited_vars.insert(next_var)
                            {
                                vars.insert(next_var);
                                queue.push_back(next_var);
                            }
                        }
                    }
                }
            }

            if !constraints.is_empty() {
                components.push(ConstraintComponent { vars, constraints });
            }
        }

        components
    }
}

/// Replaces variable `v` in `expr` with `v_expr` (an expression equal to `v`),
/// coalescing any resulting duplicate terms. Used by the elimination pre-pass.
fn substitute_var(expr: &mut LinearExpr, v: Var, v_expr: &LinearExpr) {
    let Some(pos) = expr.coeffs.iter().position(|(_, var)| *var == v) else {
        return;
    };
    let (cv, _) = expr.coeffs.remove(pos);
    for &(c, var) in &v_expr.coeffs {
        if let Some(term) = expr.coeffs.iter_mut().find(|(_, ev)| *ev == var) {
            term.0 += cv * c;
        } else {
            expr.coeffs.push((cv * c, var));
        }
    }
    expr.constant += cv * v_expr.constant;
}

/// Merges duplicate-variable terms and drops near-zero coefficients in place, so a
/// constraint's `coeffs.len()` faithfully reflects its number of distinct variables
/// (e.g. a chain closing onto itself collapses `x + x` to a single `2x` term, and a
/// cancelling substitution collapses to an empty/contradiction constraint).
fn coalesce_terms(expr: &mut LinearExpr) {
    let distinct = expr
        .coeffs
        .iter()
        .enumerate()
        .all(|(i, (_, v))| expr.coeffs[..i].iter().all(|(_, earlier)| earlier != v));
    if distinct {
        expr.coeffs
            .retain(|(c, _)| relative_ne!(*c, 0., epsilon = EPSILON));
        return;
    }
    let mut merged: Vec<(f64, Var)> = Vec::with_capacity(expr.coeffs.len());
    for &(c, v) in &expr.coeffs {
        if let Some(term) = merged.iter_mut().find(|(_, mv)| *mv == v) {
            term.0 += c;
        } else {
            merged.push((c, v));
        }
    }
    merged.retain(|(c, _)| relative_ne!(*c, 0., epsilon = EPSILON));
    expr.coeffs = merged.into();
}

struct ConstraintComponent {
    vars: FxIndexSet<Var>,
    constraints: Vec<ConstraintId>,
}

pub type ConstraintId = u64;

/// The constraints that mention one variable, in insertion order. Most
/// variables are in a few constraints, which a short list holds without the
/// allocations of a hash set; a longer list moves to one, keeping its order.
#[derive(Clone, Debug)]
enum ConstraintSet {
    Few(SmallVec<[ConstraintId; 4]>),
    Many(FxIndexSet<ConstraintId>),
}

impl Default for ConstraintSet {
    fn default() -> Self {
        ConstraintSet::Few(SmallVec::new())
    }
}

impl ConstraintSet {
    const FEW: usize = 16;

    fn insert(&mut self, id: ConstraintId) -> bool {
        match self {
            ConstraintSet::Few(ids) => {
                if ids.contains(&id) {
                    return false;
                }
                if ids.len() < Self::FEW {
                    ids.push(id);
                } else {
                    let mut set: FxIndexSet<ConstraintId> = ids.drain(..).collect();
                    set.insert(id);
                    *self = ConstraintSet::Many(set);
                }
                true
            }
            ConstraintSet::Many(set) => set.insert(id),
        }
    }

    /// Removes `id`, moving the last constraint into its place.
    fn swap_remove(&mut self, id: &ConstraintId) -> bool {
        match self {
            ConstraintSet::Few(ids) => match ids.iter().position(|other| other == id) {
                Some(index) => {
                    ids.swap_remove(index);
                    true
                }
                None => false,
            },
            ConstraintSet::Many(set) => set.swap_remove(id),
        }
    }

    fn iter(&self) -> ConstraintSetIter<'_> {
        match self {
            ConstraintSet::Few(ids) => Either::Left(ids.iter()),
            ConstraintSet::Many(set) => Either::Right(set.iter()),
        }
    }
}

type ConstraintSetIter<'a> =
    Either<std::slice::Iter<'a, ConstraintId>, indexmap::set::Iter<'a, ConstraintId>>;

impl<'a> IntoIterator for &'a ConstraintSet {
    type Item = &'a ConstraintId;
    type IntoIter = ConstraintSetIter<'a>;

    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

#[derive(Debug, Default, Clone, Serialize, Deserialize, PartialOrd, PartialEq)]
pub struct LinearExpr {
    pub coeffs: Terms,
    pub constant: f64,
}

/// The terms of a [`LinearExpr`], as `(coefficient, variable)` pairs. A single
/// term is stored inline, since most expressions are one variable plus a
/// constant.
#[derive(Clone, Default)]
pub struct Terms(TermsRepr);

#[derive(Clone)]
enum TermsRepr {
    One((f64, Var)),
    Many(Vec<(f64, Var)>),
}

impl Default for TermsRepr {
    fn default() -> Self {
        TermsRepr::Many(Vec::new())
    }
}

impl Terms {
    pub const fn new() -> Self {
        Terms(TermsRepr::Many(Vec::new()))
    }

    pub fn one(coeff: f64, var: Var) -> Self {
        Terms(TermsRepr::One((coeff, var)))
    }

    pub fn with_capacity(capacity: usize) -> Self {
        if capacity <= 1 {
            Self::new()
        } else {
            Terms(TermsRepr::Many(Vec::with_capacity(capacity)))
        }
    }

    pub fn push(&mut self, term: (f64, Var)) {
        match &mut self.0 {
            TermsRepr::Many(terms) if terms.capacity() == 0 => self.0 = TermsRepr::One(term),
            TermsRepr::Many(terms) => terms.push(term),
            TermsRepr::One(first) => {
                let mut terms = Vec::with_capacity(4);
                terms.push(*first);
                terms.push(term);
                self.0 = TermsRepr::Many(terms);
            }
        }
    }

    pub fn remove(&mut self, index: usize) -> (f64, Var) {
        match &mut self.0 {
            TermsRepr::One(term) => {
                assert_eq!(index, 0, "term index out of bounds");
                let term = *term;
                self.0 = TermsRepr::Many(Vec::new());
                term
            }
            TermsRepr::Many(terms) => terms.remove(index),
        }
    }

    pub fn retain(&mut self, mut keep: impl FnMut(&(f64, Var)) -> bool) {
        match &mut self.0 {
            TermsRepr::One(term) => {
                if !keep(term) {
                    self.0 = TermsRepr::Many(Vec::new());
                }
            }
            TermsRepr::Many(terms) => terms.retain(keep),
        }
    }
}

impl std::ops::Deref for Terms {
    type Target = [(f64, Var)];

    fn deref(&self) -> &Self::Target {
        match &self.0 {
            TermsRepr::One(term) => std::slice::from_ref(term),
            TermsRepr::Many(terms) => terms,
        }
    }
}

impl std::ops::DerefMut for Terms {
    fn deref_mut(&mut self) -> &mut Self::Target {
        match &mut self.0 {
            TermsRepr::One(term) => std::slice::from_mut(term),
            TermsRepr::Many(terms) => terms,
        }
    }
}

impl std::fmt::Debug for Terms {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_list().entries(self.iter()).finish()
    }
}

impl PartialEq for Terms {
    fn eq(&self, other: &Self) -> bool {
        **self == **other
    }
}

impl PartialOrd for Terms {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        (**self).partial_cmp(&**other)
    }
}

impl From<Vec<(f64, Var)>> for Terms {
    fn from(terms: Vec<(f64, Var)>) -> Self {
        match terms[..] {
            [term] => Terms(TermsRepr::One(term)),
            _ => Terms(TermsRepr::Many(terms)),
        }
    }
}

impl Extend<(f64, Var)> for Terms {
    fn extend<I: IntoIterator<Item = (f64, Var)>>(&mut self, iter: I) {
        let iter = iter.into_iter();
        if let TermsRepr::Many(terms) = &mut self.0
            && (terms.capacity() > 0 || iter.size_hint().0 > 1)
        {
            terms.extend(iter);
            return;
        }
        for term in iter {
            self.push(term);
        }
    }
}

impl FromIterator<(f64, Var)> for Terms {
    fn from_iter<I: IntoIterator<Item = (f64, Var)>>(iter: I) -> Self {
        let mut terms = Terms::new();
        terms.extend(iter);
        terms
    }
}

impl IntoIterator for Terms {
    type Item = (f64, Var);
    type IntoIter = TermsIntoIter;

    fn into_iter(self) -> Self::IntoIter {
        match self.0 {
            TermsRepr::One(term) => TermsIntoIter::One(Some(term)),
            TermsRepr::Many(terms) => TermsIntoIter::Many(terms.into_iter()),
        }
    }
}

impl<'a> IntoIterator for &'a Terms {
    type Item = &'a (f64, Var);
    type IntoIter = std::slice::Iter<'a, (f64, Var)>;

    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

/// The owning iterator of [`Terms`].
pub enum TermsIntoIter {
    One(Option<(f64, Var)>),
    Many(std::vec::IntoIter<(f64, Var)>),
}

impl Iterator for TermsIntoIter {
    type Item = (f64, Var);

    fn next(&mut self) -> Option<Self::Item> {
        match self {
            TermsIntoIter::One(term) => term.take(),
            TermsIntoIter::Many(terms) => terms.next(),
        }
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        match self {
            TermsIntoIter::One(term) => {
                let len = usize::from(term.is_some());
                (len, Some(len))
            }
            TermsIntoIter::Many(terms) => terms.size_hint(),
        }
    }
}

impl Serialize for Terms {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_seq(self.iter())
    }
}

impl<'de> Deserialize<'de> for Terms {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Vec::<(f64, Var)>::deserialize(deserializer).map(Terms::from)
    }
}

impl LinearExpr {
    pub fn add(lhs: impl Into<LinearExpr>, rhs: impl Into<LinearExpr>) -> Self {
        lhs.into() + rhs.into()
    }

    /// Whether every coefficient and the constant term are finite.
    ///
    /// A NaN or infinite coefficient reaching the dense `nalgebra` fallback
    /// makes its Golub-Kahan iteration run forever: `Matrix::svd` passes
    /// `max_niter = 0`, which that loop treats as *no* limit rather than as an
    /// immediate stop. A hang is not catchable, so non-finite values must be
    /// kept out of the linear algebra entirely.
    pub fn is_finite(&self) -> bool {
        self.constant.is_finite() && self.coeffs.iter().all(|(coeff, _)| coeff.is_finite())
    }

    /// Substitutes variables in `table` and removes entries with coefficient 0.
    fn simplify(&mut self, table: &SolvedValues) {
        // Removed terms are summed left to right, then added to the constant.
        let mut removed: Option<f64> = None;
        self.coeffs.retain(|(coeff, var)| {
            let term = if relative_eq!(*coeff, 0., epsilon = EPSILON) {
                Some(0.)
            } else {
                table.get(var).map(|s| coeff * s)
            };
            match term {
                Some(term) => {
                    removed = Some(removed.map_or(term, |sum| sum + term));
                    false
                }
                None => true,
            }
        });
        self.constant += removed.unwrap_or(0.);
    }

    /// `lhs + rhs`, without cloning either operand first.
    pub fn sum(lhs: &LinearExpr, rhs: &LinearExpr) -> LinearExpr {
        let mut coeffs = Terms::with_capacity(lhs.coeffs.len() + rhs.coeffs.len());
        coeffs.extend(lhs.coeffs.iter().copied());
        coeffs.extend(rhs.coeffs.iter().copied());
        LinearExpr {
            coeffs,
            constant: lhs.constant + rhs.constant,
        }
    }

    /// `lhs - rhs`, without cloning either operand first.
    pub fn difference(lhs: &LinearExpr, rhs: &LinearExpr) -> LinearExpr {
        let mut coeffs = Terms::with_capacity(lhs.coeffs.len() + rhs.coeffs.len());
        coeffs.extend(lhs.coeffs.iter().copied());
        coeffs.extend(rhs.coeffs.iter().map(|(c, v)| (-c, *v)));
        LinearExpr {
            coeffs,
            constant: lhs.constant - rhs.constant,
        }
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
        let coeffs = if self.coeffs.is_empty() {
            rhs.coeffs
        } else {
            let mut coeffs = self.coeffs;
            coeffs.extend(rhs.coeffs);
            coeffs
        };
        Self {
            coeffs,
            constant: self.constant + rhs.constant,
        }
    }
}

impl std::ops::Sub<LinearExpr> for LinearExpr {
    type Output = Self;
    fn sub(self, rhs: LinearExpr) -> Self::Output {
        let coeffs = if self.coeffs.is_empty() {
            rhs.coeffs.into_iter().map(|(c, v)| (-c, v)).collect()
        } else {
            let mut coeffs = self.coeffs;
            coeffs.extend(rhs.coeffs.into_iter().map(|(c, v)| (-c, v)));
            coeffs
        };
        Self {
            coeffs,
            constant: self.constant - rhs.constant,
        }
    }
}

impl std::ops::Sub<&LinearExpr> for LinearExpr {
    type Output = Self;
    fn sub(self, rhs: &LinearExpr) -> Self::Output {
        let mut coeffs = self.coeffs;
        coeffs.extend(rhs.coeffs.iter().map(|(c, v)| (-c, *v)));
        Self {
            coeffs,
            constant: self.constant - rhs.constant,
        }
    }
}

impl std::ops::Mul<f64> for LinearExpr {
    type Output = Self;
    fn mul(self, rhs: f64) -> Self::Output {
        let mut coeffs = self.coeffs;
        for (c, _) in coeffs.iter_mut() {
            *c *= rhs;
        }
        Self {
            coeffs,
            constant: self.constant * rhs,
        }
    }
}

impl std::ops::Div<f64> for LinearExpr {
    type Output = Self;
    fn div(self, rhs: f64) -> Self::Output {
        let mut coeffs = self.coeffs;
        for (c, _) in coeffs.iter_mut() {
            *c /= rhs;
        }
        Self {
            coeffs,
            constant: self.constant / rhs,
        }
    }
}

impl From<Var> for LinearExpr {
    fn from(value: Var) -> Self {
        Self {
            coeffs: Terms::one(1., value),
            constant: 0.,
        }
    }
}

impl From<f64> for LinearExpr {
    fn from(value: f64) -> Self {
        Self {
            coeffs: Terms::new(),
            constant: value,
        }
    }
}

#[cfg(test)]
mod tests {
    /// `unsolved_var_components` must join exactly the variables a chain of
    /// *live* constraints reaches, and leave everything else in its own
    /// singleton.
    #[test]
    fn unsolved_var_components_partitions_by_live_constraints() {
        let mut solver = Solver::new();
        let (a, b, c, d, lone) = (
            solver.new_var(),
            solver.new_var(),
            solver.new_var(),
            solver.new_var(),
            solver.new_var(),
        );
        // Two disjoint chains, plus a variable in no constraint at all.
        solver.constrain_eq0(LinearExpr::from(a) - LinearExpr::from(b));
        solver.constrain_eq0(LinearExpr::from(c) - LinearExpr::from(d));

        let labels = solver.unsolved_var_components();
        assert_eq!(labels[&a], labels[&b], "a chain joins its variables");
        assert_eq!(labels[&c], labels[&d]);
        assert_ne!(labels[&a], labels[&c], "disjoint chains stay disjoint");
        assert_eq!(labels[&lone], lone, "an unconstrained variable is its own");

        // A one-variable constraint is consumed by back-substitution at
        // insertion, so it never joins anything: `a` and `b` are solved and
        // drop out of the partition entirely.
        solver.constrain_eq0(LinearExpr::from(a) - 3.);
        solver.solve();
        let labels = solver.unsolved_var_components();
        assert!(!labels.contains_key(&a), "solved variables are excluded");
        assert!(!labels.contains_key(&b));
        assert_eq!(labels[&c], labels[&d], "the other chain is untouched");
    }

    use super::*;
    use approx::assert_relative_eq;

    #[test]
    fn linear_constraints_solved_correctly() {
        let mut solver = Solver::new();
        let x = solver.new_var();
        let y = solver.new_var();
        let z = solver.new_var();
        solver.constrain_eq0(LinearExpr {
            coeffs: vec![(1., x)].into(),
            constant: -5.,
        });
        solver.constrain_eq0(LinearExpr {
            coeffs: vec![(1., y), (-1., x)].into(),
            constant: 0.,
        });
        solver.solve();
        assert_relative_eq!(*solver.solved_vars.get(&x).unwrap(), 5., epsilon = EPSILON);
        assert_relative_eq!(*solver.solved_vars.get(&y).unwrap(), 5., epsilon = EPSILON);
        assert!(!solver.solved_vars.contains_key(&z));
        assert!(!solver.unsolved_vars.contains(&x));
        assert!(!solver.unsolved_vars.contains(&y));
        assert!(solver.unsolved_vars.contains(&z));
    }

    fn c(coeffs: Vec<(f64, Var)>, constant: f64) -> LinearExpr {
        LinearExpr {
            coeffs: coeffs.into(),
            constant,
        }
    }

    /// A consistent ring of 2-variable constraints with no 1-variable starting point
    /// (the minimal `bench_constraints` shape). The pre-pass must break the cycle by
    /// substitution, then telescope to a 1-variable closure that grounds the chain.
    #[test]
    fn three_cycle_determined() {
        let mut s = Solver::new();
        let a = s.new_var();
        let b = s.new_var();
        let d = s.new_var();
        s.constrain_eq0(c(vec![(1., a), (-1., b)], -5.)); // a - b = 5
        s.constrain_eq0(c(vec![(1., b), (-1., d)], -5.)); // b - c = 5
        s.constrain_eq0(c(vec![(1., a), (1., d)], -100.)); // a + c = 100
        s.solve();
        assert_relative_eq!(s.value_of(a).unwrap(), 55., epsilon = EPSILON);
        assert_relative_eq!(s.value_of(b).unwrap(), 50., epsilon = EPSILON);
        assert_relative_eq!(s.value_of(d).unwrap(), 45., epsilon = EPSILON);
        assert!(s.inconsistent_constraints().is_empty());
        assert!(s.off_grid_vars().is_empty());
    }

    /// A chain pinned at one end resolves transitively (reverse-topological order).
    #[test]
    fn chain_telescopes() {
        let mut s = Solver::new();
        let a = s.new_var();
        let b = s.new_var();
        let d = s.new_var();
        let e = s.new_var();
        s.constrain_eq0(c(vec![(1., a), (-1., b)], -1.)); // a - b = 1
        s.constrain_eq0(c(vec![(1., b), (-1., d)], -1.)); // b - c = 1
        s.constrain_eq0(c(vec![(1., d), (-1., e)], -1.)); // c - d = 1
        s.constrain_eq0(c(vec![(1., a)], -10.)); // a = 10
        s.solve();
        assert_relative_eq!(s.value_of(a).unwrap(), 10., epsilon = EPSILON);
        assert_relative_eq!(s.value_of(b).unwrap(), 9., epsilon = EPSILON);
        assert_relative_eq!(s.value_of(d).unwrap(), 8., epsilon = EPSILON);
        assert_relative_eq!(s.value_of(e).unwrap(), 7., epsilon = EPSILON);
        assert!(s.inconsistent_constraints().is_empty());
    }

    /// An under-determined pair: neither variable is pinned, and the row space still
    /// reports one constrained direction (the pre-pass must re-materialize its
    /// constraint after failing to ground the eliminated variable).
    #[test]
    fn underdetermined_pair_unsolved() {
        let mut s = Solver::new();
        let a = s.new_var();
        let b = s.new_var();
        let constraint = s.constrain_eq0(c(vec![(1., a), (-1., b)], 0.)); // a - b = 0
        s.solve();
        assert!(s.value_of(a).is_none());
        assert!(s.value_of(b).is_none());
        assert!(s.unsolved_vars().contains(&a));
        assert!(s.unsolved_vars().contains(&b));
        assert!(s.constraints.contains_key(&constraint));
        assert_eq!(s.rowspace_vecs().len(), 1);
    }

    /// Two contradictory 2-variable constraints: substitution drives one to `0 = -5`.
    #[test]
    fn inconsistent_pair() {
        let mut s = Solver::new();
        let a = s.new_var();
        let b = s.new_var();
        s.constrain_eq0(c(vec![(1., a), (-1., b)], 0.)); // a - b = 0
        s.constrain_eq0(c(vec![(1., a), (-1., b)], -5.)); // a - b = 5
        s.solve();
        assert!(!s.inconsistent_constraints().is_empty());
    }

    /// An over-constrained cycle whose differences sum to a nonzero constant.
    #[test]
    fn inconsistent_cycle() {
        let mut s = Solver::new();
        let a = s.new_var();
        let b = s.new_var();
        let d = s.new_var();
        s.constrain_eq0(c(vec![(1., a), (-1., b)], -5.)); // a - b = 5
        s.constrain_eq0(c(vec![(1., b), (-1., d)], -5.)); // b - c = 5
        s.constrain_eq0(c(vec![(1., d), (-1., a)], -5.)); // c - a = 5  (loop sum = 15)
        s.solve();
        assert!(!s.inconsistent_constraints().is_empty());
    }

    /// A duplicate constraint inside a coupled core is dropped as redundant (not flagged
    /// inconsistent), and the remaining system still solves.
    #[test]
    fn redundant_in_core() {
        let mut s = Solver::new();
        let a = s.new_var();
        let b = s.new_var();
        s.constrain_eq0(c(vec![(1., a), (-1., b)], 0.)); // a - b = 0
        s.constrain_eq0(c(vec![(1., a), (-1., b)], 0.)); // a - b = 0 (duplicate)
        s.constrain_eq0(c(vec![(1., a), (1., b)], -10.)); // a + b = 10
        s.solve();
        assert_relative_eq!(s.value_of(a).unwrap(), 5., epsilon = EPSILON);
        assert_relative_eq!(s.value_of(b).unwrap(), 5., epsilon = EPSILON);
        assert!(s.inconsistent_constraints().is_empty());
    }

    /// A value reached only through elimination + resolution that lands off the 0.1
    /// grid is flagged in `off_grid_vars`.
    #[test]
    fn off_grid_cycle() {
        let mut s = Solver::new();
        let a = s.new_var();
        let b = s.new_var();
        let d = s.new_var();
        s.constrain_eq0(c(vec![(1., a), (-1., b)], 0.)); // a - b = 0
        s.constrain_eq0(c(vec![(1., b), (-1., d)], 0.)); // b - c = 0
        s.constrain_eq0(c(vec![(1., a), (1., b), (1., d)], -1.)); // a + b + c = 1  => 1/3 each
        s.solve();
        assert!(!s.off_grid_vars().is_empty());
    }

    /// `VarSet` keeps the same order as an `IndexSet` under the same inserts
    /// and swap-removes.
    #[test]
    fn var_set_matches_index_set() {
        let mut set = VarSet::default();
        let mut reference = FxIndexSet::default();
        let mut state = 7u64;
        for _ in 0..20_000 {
            state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            let var = Var((state >> 33) % 64);
            if (state >> 20).is_multiple_of(3) {
                assert_eq!(set.swap_remove(&var), reference.swap_remove(&var));
            } else {
                assert_eq!(set.insert(var), reference.insert(var));
            }
            assert_eq!(set.contains(&var), reference.contains(&var));
            assert!(set.iter().eq(reference.iter()));
        }
    }

    /// A constraint solved at once leaves the solver as recording it and back
    /// substituting would: the same values to the bit, the same off-grid and
    /// inconsistent records, the same live constraints, and the same next ID.
    #[test]
    fn constraints_solved_at_once_match_recorded_ones() {
        // Right-hand sides of `var = rhs`, then whole constraints.
        let rhss = |solved: [Var; 2]| {
            vec![
                LinearExpr::from(120.),
                LinearExpr::from(-0.),
                LinearExpr::from(1.2),
                c(vec![(1., solved[0])], 35.),
                c(vec![(1., solved[0]), (-1., solved[1])], -0.5),
                c(vec![(0.5, solved[0]), (0.5, solved[1])], 0.),
                c(vec![(1. / 3., solved[1]), (1e-12, solved[0])], 7.),
            ]
        };
        let constraints = |var: Var, solved: [Var; 2], other: Var| {
            let mut constraints: Vec<LinearExpr> = rhss(solved)
                .iter()
                .map(|rhs| LinearExpr::difference(&LinearExpr::from(var), rhs))
                .collect();
            constraints.extend([
                c(vec![(3., var), (-2.5, solved[1]), (1e-12, other)], 1e-9),
                c(vec![(1., var), (1., var)], -10.),
                c(vec![(1., var), (-1., other)], 0.),
                c(vec![(1., solved[0])], -101.),
                c(vec![(1., solved[0])], -100.),
            ]);
            constraints
        };
        for index in 0..12 {
            let mut results = Vec::new();
            for way in 0..4 {
                let mut solver = Solver::with_grid(5.);
                let solved = [solver.new_var(), solver.new_var()];
                solver.constrain_eq0(c(vec![(1., solved[0])], -101.));
                solver.constrain_eq0(c(vec![(1., solved[1])], -2003.));
                let (var, other) = (solver.new_var(), solver.new_var());
                // A constraint `var` is already in is solved along with it.
                let later = solver.new_var();
                solver.constrain_eq0(c(vec![(1., later), (-1., var)], -10.));
                let expr = constraints(var, solved, other).swap_remove(index);
                let id = solver.next_constraint;
                match way {
                    0 => {
                        solver.next_constraint += 1;
                        solver.record_eq0(id, expr);
                    }
                    1 => {
                        solver.constrain_eq0(expr);
                    }
                    2 => {
                        solver.constrain_eq(&expr, &LinearExpr::from(0.));
                    }
                    _ => {
                        // Only the `var = rhs` forms apply.
                        if index >= 7 {
                            continue;
                        }
                        let rhs = rhss(solved).swap_remove(index);
                        solver.solve_fresh(var, &rhs).expect("rhs is known");
                    }
                }
                let next = solver.constrain_eq0(LinearExpr::from(0.));
                results.push((
                    [var, other, later].map(|v| solver.value_of(v).map(f64::to_bits)),
                    solver.off_grid_vars().clone(),
                    solver.inconsistent_constraints().clone(),
                    solver.is_live(id),
                    solver.constraints.len(),
                    next,
                ));
            }
            for result in &results[1..] {
                assert_eq!(&results[0], result, "expression {index}");
            }
        }
    }

    #[test]
    fn configured_grid_controls_rounding_and_off_grid_detection() {
        let mut s = Solver::with_grid(0.25);
        let on_grid = s.new_var();
        let off_grid = s.new_var();
        s.constrain_eq0(c(vec![(1., on_grid)], -1.25));
        s.constrain_eq0(c(vec![(1., off_grid)], -1.2));

        assert_relative_eq!(s.value_of(on_grid).unwrap(), 1.25, epsilon = EPSILON);
        assert_relative_eq!(s.value_of(off_grid).unwrap(), 1.25, epsilon = EPSILON);
        assert!(!s.off_grid_vars().contains_key(&on_grid));
        assert!(s.off_grid_vars().contains_key(&off_grid));
    }

    /// Variables eliminated in an earlier `solve()` (before the closing constraint
    /// exists) are resolved once a later constraint pins the chain.
    #[test]
    fn incremental_resolution() {
        let mut s = Solver::new();
        let a = s.new_var();
        let b = s.new_var();
        let d = s.new_var();
        s.constrain_eq0(c(vec![(1., a), (-1., b)], -1.)); // a - b = 1
        s.constrain_eq0(c(vec![(1., b), (-1., d)], -1.)); // b - c = 1
        s.solve(); // under-determined so far
        assert!(s.value_of(a).is_none());
        s.constrain_eq0(c(vec![(1., a)], -10.)); // a = 10
        s.solve();
        assert_relative_eq!(s.value_of(a).unwrap(), 10., epsilon = EPSILON);
        assert_relative_eq!(s.value_of(b).unwrap(), 9., epsilon = EPSILON);
        assert_relative_eq!(s.value_of(d).unwrap(), 8., epsilon = EPSILON);
    }

    fn coupled_ring_system(
        n: usize,
        left: f64,
        diagonal: f64,
        right: f64,
    ) -> (Solver, Vec<Var>, Vec<f64>) {
        let mut solver = Solver::new();
        let vars: Vec<_> = (0..n).map(|_| solver.new_var()).collect();
        let expected: Vec<_> = (0..n).map(|i| ((i % 11) as f64 - 5.) * 0.1).collect();
        for i in 0..n {
            let previous = (i + n - 1) % n;
            let next = (i + 1) % n;
            let rhs = left * expected[previous] + diagonal * expected[i] + right * expected[next];
            solver.constrain_eq0(c(
                vec![
                    (left, vars[previous]),
                    (diagonal, vars[i]),
                    (right, vars[next]),
                ],
                -rhs,
            ));
        }
        (solver, vars, expected)
    }

    /// Every row has three unknowns, so neither insertion-time back-substitution
    /// nor the size-2 elimination pass can make progress. The symmetric sparse
    /// component is solved by sparse QR and never materializes the dense SVD matrix.
    #[test]
    fn sparse_symmetric_ring_uses_sparse_qr() {
        let (mut solver, vars, expected) = coupled_ring_system(128, -1., 1.9, -1.);
        assert!(vars.iter().all(|&var| !solver.is_solved(var)));

        let mut components = solver.constraint_components();
        assert_eq!(components.len(), 1);
        let component = components.pop().unwrap();
        assert!(solver.try_solve_sparse_component(&component.vars, &component.constraints));
        assert!(solver.fully_solved());
        for (&var, &value) in vars.iter().zip(&expected) {
            assert_relative_eq!(solver.value_of(var).unwrap(), value, epsilon = EPSILON);
        }
        assert!(solver.inconsistent_constraints().is_empty());
        assert!(solver.off_grid_vars().is_empty());
    }

    /// The same irreducible shape with asymmetric neighbor coefficients exercises
    /// general nonsymmetric sparse QR.
    #[test]
    fn sparse_nonsymmetric_ring_uses_sparse_qr() {
        let (mut solver, vars, expected) = coupled_ring_system(127, -1., 2., -1.1);
        assert!(vars.iter().all(|&var| !solver.is_solved(var)));

        solver.solve();
        assert!(solver.fully_solved());
        for (&var, &value) in vars.iter().zip(&expected) {
            assert_relative_eq!(solver.value_of(var).unwrap(), value, epsilon = EPSILON);
        }
        assert!(solver.inconsistent_constraints().is_empty());
        assert!(solver.off_grid_vars().is_empty());
    }

    /// A fully-coupled full-rank block has no size-<=2 pivot, so sparse QR solves it.
    #[test]
    fn full_rank_block_uses_sparse_qr() {
        let mut s = Solver::new();
        let a = s.new_var();
        let b = s.new_var();
        let d = s.new_var();
        s.constrain_eq0(c(vec![(1., a), (1., b), (1., d)], -6.)); // a + b + c = 6
        s.constrain_eq0(c(vec![(1., a), (2., b), (3., d)], -14.)); // a + 2b + 3c = 14
        s.constrain_eq0(c(vec![(1., a), (3., b), (6., d)], -25.)); // a + 3b + 6c = 25
        s.solve();
        assert_relative_eq!(s.value_of(a).unwrap(), 1., epsilon = EPSILON);
        assert_relative_eq!(s.value_of(b).unwrap(), 2., epsilon = EPSILON);
        assert_relative_eq!(s.value_of(d).unwrap(), 3., epsilon = EPSILON);
        assert!(s.inconsistent_constraints().is_empty());
    }

    #[test]
    fn rank_deficient_sparse_component_preserves_nullspace_for_sse() {
        let mut solver = Solver::new();
        let x = solver.new_var();
        let y = solver.new_var();
        let z = solver.new_var();
        solver.constrain_eq0(c(vec![(1., x), (1., y), (1., z)], -5.));
        solver.constrain_eq0(c(vec![(1., x), (1., y), (-1., z)], -1.));

        let mut components = solver.constraint_components();
        let component = components.pop().unwrap();
        assert!(solver.try_solve_sparse_component(&component.vars, &component.constraints));
        assert_relative_eq!(solver.value_of(z).unwrap(), 2., epsilon = EPSILON);
        assert!(solver.value_of(x).is_none());
        assert!(solver.value_of(y).is_none());

        let basis = solver.sparse_nullspace_vecs().unwrap();
        assert_eq!(basis.len(), 1);
        assert!(basis[0].iter().all(|&(_, var)| var != z));
    }

    #[test]
    fn unconstrained_variables_are_nullspace_directions_for_sse() {
        let mut solver = Solver::new();
        let x = solver.new_var();
        let y = solver.new_var();

        solver.solve();

        let basis = solver.sparse_nullspace_vecs().unwrap();
        assert_eq!(basis, vec![vec![(1., x)], vec![(1., y)]]);
    }
}
