//! Compiled cells retained across source edits.
//!
//! A cell's compiled form depends on its own text, on everything it refers to,
//! on its arguments, and on the technology it was compiled against -- and on
//! nothing else. An edit that changes none of those cannot change the cell, so
//! it can be handed back rather than recomputed. That is worth doing because
//! the cost of a design is usually not in the cell being edited: in
//! `examples/multi_stress_shapes`, `multi_shapes()` is two instantiations over
//! two children of ten thousand rectangles each, and drawing one rectangle in
//! it re-executed both children -- essentially the whole compile.
//!
//! What identifies an entry is the content [`CellId`] from
//! [`crate::gdscache::source_cell_id`], which already folds in the fingerprint,
//! the arguments and the scope name. Entries survive edits; they are dropped
//! only when the execution environment changes, which is what tracks the
//! technology file and the imported GDS libraries.

use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};

use crate::{
    compile::{CellId, CompiledCell, ExecError},
    fingerprint::{ItemIndex, SpanRebase},
};

/// One compiled cell, plus what reinstating it needs.
#[derive(Clone, Debug)]
pub(crate) struct CachedCell {
    pub cell: Arc<CompiledCell>,
    /// Cells this one instantiates. Reinstating means reinstating all of them:
    /// `SolvedValue::Instance::cell` and `ExecPass::bbox` both follow the
    /// hierarchy through the compiled map, so a parent whose child was evicted
    /// would name a cell that is not there.
    pub children: Vec<CellId>,
    /// Diagnostics this cell produced.
    ///
    /// `execute_cell_inner` can return `Ok` having reported that a cell is
    /// underconstrained, off-grid, inconsistent, or has flipped rectangle
    /// edges. The intra-run cache already reports each of those once per
    /// distinct instantiation; replaying them is what keeps cross-edit reuse
    /// from quietly deleting a diagnostic the user is looking at.
    pub errors: Vec<ExecError>,
    /// How many ids a fresh execution of this cell consumed, replayed on a hit
    /// so that ids allocated afterwards do not depend on whether the cell was
    /// cached. See [`crate::gdscache::GdsImportEntry::ids_consumed`].
    pub ids_consumed: u64,
    /// Declaration placements the cell's spans were recorded against.
    pub items: Arc<ItemIndex>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CellCacheStats {
    pub hits: u64,
    pub misses: u64,
    /// Entries whose spans had to be shifted because a declaration moved.
    pub rebased: u64,
    /// Entries dropped because a span could not be translated. Always zero; a
    /// nonzero value means a dependency edge is missing from the fingerprint.
    pub rebase_failures: u64,
    pub entries: u64,
}

/// Compiled cells retained across source edits, keyed by content.
#[derive(Debug, Default, Clone)]
pub struct CellCache {
    entries: HashMap<CellId, CachedCell>,
    stats: CellCacheStats,
}

impl CellCache {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn stats(&self) -> CellCacheStats {
        self.stats
    }

    pub fn clear(&mut self) {
        self.entries.clear();
        self.stats.entries = 0;
    }

    /// The entry for `id` and its whole instantiation closure, with every span
    /// translated onto `items`.
    ///
    /// A miss for any reason -- an evicted child, a span that cannot be
    /// translated -- returns `None` and the caller executes the cell. That
    /// keeps eviction policy-free: dropping any entry can only cost time.
    pub(crate) fn reinstate(
        &mut self,
        id: CellId,
        items: &Arc<ItemIndex>,
    ) -> Option<Vec<(CellId, CachedCell)>> {
        let Some(mut closure) = self.closure(id) else {
            self.stats.misses += 1;
            return None;
        };

        // One translation per source revision, not one per cell. Every entry
        // in a closure was almost always recorded against the same
        // `ItemIndex`, and `SpanRebase::new` walks the whole workspace index
        // to build it -- doing that once per cell is quadratic in a design
        // with many cells. Keyed by `Arc` identity, which is sound because
        // `closure` holds a strong reference to every index it names for the
        // whole of this loop, so no address can be reused.
        let mut rebases: HashMap<*const ItemIndex, Option<SpanRebase>> = HashMap::new();
        // Indices of the entries whose `items` moved on, which are the only
        // ones worth writing back.
        let mut translated = Vec::new();
        for (index, (_, entry)) in closure.iter_mut().enumerate() {
            if Arc::ptr_eq(&entry.items, items) {
                continue;
            }
            let revision = Arc::as_ptr(&entry.items);
            let rebase = rebases
                .entry(revision)
                .or_insert_with(|| SpanRebase::new(&entry.items, items));
            let Some(rebase) = rebase.as_ref() else {
                // Nothing moved; the spans are already correct, and recording
                // that avoids recomputing the comparison next time.
                entry.items = items.clone();
                translated.push(index);
                continue;
            };
            let cell = Arc::make_mut(&mut entry.cell);
            if cell.rebase_spans(rebase).is_err() {
                self.stats.rebase_failures += 1;
                self.stats.misses += 1;
                return None;
            }
            for error in &mut entry.errors {
                if rebase.rebase_opt(&mut error.span).is_err() {
                    self.stats.rebase_failures += 1;
                    self.stats.misses += 1;
                    return None;
                }
            }
            entry.items = items.clone();
            translated.push(index);
            self.stats.rebased += 1;
        }

        // Write the translated entries back, so the shift is paid once per
        // move rather than once per compile. Entries that were already current
        // are left alone: re-inserting an identical clone is pure copying.
        for index in translated {
            let (id, entry) = &closure[index];
            self.entries.insert(*id, entry.clone());
        }
        self.stats.hits += 1;
        Some(closure)
    }

    /// The entry for `id` together with every cell it transitively
    /// instantiates, or `None` if any of them is absent.
    ///
    /// Emitted in post-order over each cell's instantiation list, which is the
    /// order a fresh execution leaves `compiled_cells` in: a cell is emitted
    /// only once every cell it instantiates has been. A depth-first *preorder*
    /// reversed is not enough -- with a child shared between a cell and one of
    /// that cell's own ancestors it puts the ancestor first -- and the order
    /// reaches the GDS exporter and the serialized artifact.
    fn closure(&self, id: CellId) -> Option<Vec<(CellId, CachedCell)>> {
        let mut closure = Vec::new();
        let mut emitted = HashSet::new();
        // `(cell, index of the next child to descend into)`.
        let mut stack = vec![(id, 0_usize)];
        // Guards against a cycle in the instantiation graph, which no source
        // can produce today but which must not hang the compiler if one can.
        let mut on_stack = HashSet::from([id]);
        while let Some(&(next, cursor)) = stack.last() {
            let entry = self.entries.get(&next)?;
            if let Some(&child) = entry.children.get(cursor) {
                stack.last_mut().expect("just peeked").1 = cursor + 1;
                if !emitted.contains(&child) && on_stack.insert(child) {
                    stack.push((child, 0));
                }
                continue;
            }
            stack.pop();
            on_stack.remove(&next);
            if emitted.insert(next) {
                closure.push((next, entry.clone()));
            }
        }
        Some(closure)
    }

    pub(crate) fn insert(&mut self, id: CellId, entry: CachedCell) {
        if self.entries.insert(id, entry).is_none() {
            self.stats.entries += 1;
        }
    }

    pub(crate) fn contains(&self, id: CellId) -> bool {
        self.entries.contains_key(&id)
    }
}
