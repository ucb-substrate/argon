//! Compiled cells imported from GDS, retained across source edits.
//!
//! A GDS import is by far the most expensive thing a compilation can do and
//! the one thing a source edit provably cannot change. Importing a 45 MB
//! library takes over three seconds and yields ten thousand cells; the Argon
//! cell that instantiates it is three lines long. Without this cache, drawing
//! one rectangle beside that instance re-decodes the whole library, so the
//! edit costs seconds instead of milliseconds.
//!
//! Two properties make this the safe part of the cache to build first:
//!
//! * Imported cells carry no Argon source spans. Every span in one names the
//!   `.gds` file at offset `0..0` (see `ExecPass::execute_gds_cell`), so an
//!   entry reinstated after an edit cannot carry a stale offset into a `.ar`
//!   file -- which is the failure mode that would otherwise let the GUI
//!   rewrite the wrong bytes of someone's source.
//! * Their identity does not depend on the program. A structure is named by
//!   the import it came from and its index within that import, so a
//!   content-derived [`CellId`] means the same thing in every run and nothing
//!   has to be renumbered on reuse.
//!
//! Freshness of the `.gds` file itself is not checked here. The session drops
//! the whole cache when its execution environment changes, and that key
//! already covers every import's path and contents.

use std::{
    collections::HashMap,
    hash::{Hash, Hasher},
    path::{Path, PathBuf},
    sync::Arc,
};

use crate::compile::{CellArgKey, CellId, CompiledCell};

/// Marks a [`CellId`] as derived from content rather than allocated by
/// `ExecPass::alloc_id`.
///
/// The allocator is a counter starting at a small integer, so reserving the
/// top bit keeps the two spaces disjoint by construction rather than by the
/// improbability of a collision.
pub(crate) const CONTENT_ID_BIT: u64 = 1 << 63;

/// Whether a [`CellId`] was derived from content rather than allocated.
pub(crate) fn is_content_id(id: CellId) -> bool {
    id & CONTENT_ID_BIT != 0
}

/// Identifies one import: which GDS cell was asked for, from which file, and
/// under which scope name.
///
/// `scope_name` is part of the key because it becomes the imported top cell's
/// root scope name, and therefore its [`crate::compile::ScopeId`]. Only the
/// top cell is affected by it, so the structures beneath are keyed without it
/// and shared across scope names.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct GdsImportKey {
    pub declared_name: String,
    pub path: PathBuf,
    pub scope_name: Option<String>,
}

/// One import's compiled cells, in the order a fresh run inserts them.
///
/// Order is retained rather than recovered from a map because
/// `CompiledData::cells` is an `IndexMap` whose iteration order reaches the
/// GDS exporter and the serialized artifact; reinstating in a different order
/// than a fresh import would change compiled output that is otherwise
/// identical.
#[derive(Clone, Debug)]
pub(crate) struct GdsImportEntry {
    pub top: CellId,
    pub cells: Vec<(CellId, Arc<CompiledCell>)>,
    /// How many ids a fresh import of this entry consumed from
    /// `ExecPass::next_id`.
    ///
    /// A hit allocates a value per cell but no object ids, so without this the
    /// counter would sit lower than after a fresh import and every id handed
    /// out afterwards -- including the ids of the cell the user is editing --
    /// would differ between a cached and an uncached compile. Nothing
    /// *observable* depends on an id's value, but replaying the same
    /// consumption keeps a cached compile byte-identical to a fresh one, which
    /// is what makes the two comparable in a test.
    pub ids_consumed: u64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct GdsCacheStats {
    pub hits: u64,
    pub misses: u64,
    pub entries: u64,
    pub cells: u64,
}

/// Imported GDS hierarchies, keyed by import rather than by source revision.
#[derive(Debug, Default, Clone)]
pub struct GdsCache {
    entries: HashMap<GdsImportKey, GdsImportEntry>,
    stats: GdsCacheStats,
}

impl GdsCache {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn stats(&self) -> GdsCacheStats {
        self.stats
    }

    /// Forgets every import. Called when the session's execution environment
    /// changes, which is what tracks the `.gds` files' contents.
    pub fn clear(&mut self) {
        self.entries.clear();
        self.stats.entries = 0;
        self.stats.cells = 0;
    }

    pub(crate) fn get(&mut self, key: &GdsImportKey) -> Option<GdsImportEntry> {
        match self.entries.get(key) {
            Some(entry) => {
                self.stats.hits += 1;
                Some(entry.clone())
            }
            None => {
                self.stats.misses += 1;
                None
            }
        }
    }

    pub(crate) fn insert(&mut self, key: GdsImportKey, entry: GdsImportEntry) {
        let cells = entry.cells.len() as u64;
        // Counted against what the entry replaced, so re-importing under a key
        // that is already held reports the cells the cache holds rather than
        // the cells it has ever seen.
        match self.entries.insert(key, entry) {
            Some(previous) => self.stats.cells -= previous.cells.len() as u64,
            None => self.stats.entries += 1,
        }
        self.stats.cells += cells;
    }
}

/// The [`CellId`] of a cell executed from source, derived from what it is
/// rather than from when it was allocated.
///
/// The arguments are already normalized by [`CellArgKey`] -- floats by their
/// bits, so that two calls agreeing on every bit agree on the id -- and the
/// scope name is included because it becomes the cell's root scope name.
///
/// Two calls that agree on all three produce the same cell, which is exactly
/// the condition under which one may be reused for the other.
pub(crate) fn source_cell_id(
    fingerprint: u64,
    args: &[CellArgKey],
    scope_name: Option<&str>,
) -> CellId {
    let mut hasher = fnv::FnvHasher::default();
    hasher.write_u64(fingerprint);
    hasher.write_usize(args.len());
    for arg in args {
        hash_cell_arg_key(&mut hasher, arg);
    }
    match scope_name {
        Some(name) => {
            hasher.write_u8(1);
            hasher.write_usize(name.len());
            hasher.write(name.as_bytes());
        }
        None => hasher.write_u8(0),
    }
    CONTENT_ID_BIT | (hasher.finish() & !CONTENT_ID_BIT)
}

/// Discriminant-tagged and length-prefixed, so that no two distinct argument
/// lists can hash alike -- a sequence and its single element included.
fn hash_cell_arg_key(hasher: &mut fnv::FnvHasher, arg: &CellArgKey) {
    match arg {
        CellArgKey::Float(bits) => {
            hasher.write_u8(0);
            hasher.write_u64(*bits);
        }
        CellArgKey::Int(value) => {
            hasher.write_u8(1);
            hasher.write_i64(*value);
        }
        CellArgKey::Bool(value) => {
            hasher.write_u8(2);
            hasher.write_u8(u8::from(*value));
        }
        CellArgKey::String(value) => {
            hasher.write_u8(3);
            hasher.write_usize(value.len());
            hasher.write(value.as_bytes());
        }
        CellArgKey::Enum(value) => {
            hasher.write_u8(4);
            hasher.write_usize(value.len());
            hasher.write(value.as_bytes());
        }
        CellArgKey::Seq(values) => {
            hasher.write_u8(5);
            hasher.write_usize(values.len());
            for value in values {
                hash_cell_arg_key(hasher, value);
            }
        }
    }
}

/// The [`CellId`] of one structure within an import.
///
/// `top_scope_name` is mixed in only for the structure that becomes the
/// import's top cell, so that two scope names share every structure beneath
/// it instead of duplicating the whole hierarchy.
pub(crate) fn gds_cell_id(
    declared_name: &str,
    path: &Path,
    index: usize,
    top_scope_name: Option<&str>,
) -> CellId {
    let mut hasher = fnv::FnvHasher::default();
    // Length-prefixed, so that the boundary between the name and the path is
    // recoverable from the digest and `("ab", "c")` cannot collide with
    // `("a", "bc")`.
    hasher.write_usize(declared_name.len());
    hasher.write(declared_name.as_bytes());
    let path = path.to_string_lossy();
    hasher.write_usize(path.len());
    hasher.write(path.as_bytes());
    hasher.write_usize(index);
    match top_scope_name {
        Some(name) => {
            hasher.write_u8(1);
            hasher.write_usize(name.len());
            hasher.write(name.as_bytes());
        }
        None => hasher.write_u8(0),
    }
    CONTENT_ID_BIT | (hasher.finish() & !CONTENT_ID_BIT)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_ids_are_stable_disjoint_and_distinguishing() {
        let path = Path::new("/a/b.gds");
        let id = gds_cell_id("sram", path, 3, None);
        assert_eq!(
            id,
            gds_cell_id("sram", path, 3, None),
            "stable across calls"
        );
        assert_ne!(id, gds_cell_id("sram", path, 4, None), "index matters");
        assert_ne!(id, gds_cell_id("other", path, 3, None), "name matters");
        assert_ne!(
            id,
            gds_cell_id("sram", Path::new("/a/c.gds"), 3, None),
            "path matters"
        );
        assert_ne!(
            id,
            gds_cell_id("sram", path, 3, Some("cell x")),
            "the top cell's scope name matters"
        );
        // The concatenation of name and path is unambiguous.
        assert_ne!(
            gds_cell_id("ab", Path::new("c"), 0, None),
            gds_cell_id("a", Path::new("bc"), 0, None)
        );
        assert_ne!(id & CONTENT_ID_BIT, 0, "marked as content-derived");
    }
}
