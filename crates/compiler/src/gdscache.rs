//! Compiled cells imported from GDS, retained across source edits.
//!
//! A GDS import is the most expensive thing a compilation can do and the one
//! thing a source edit cannot change: importing a 45 MB library takes seconds
//! and yields ten thousand cells, while the Argon cell that instantiates it is
//! three lines long.
//!
//! An imported cell carries no Argon source spans: every span in one names the
//! `.gds` file at offset `0..0` (see `ExecPass::execute_gds_cell`). Its
//! identity depends only on the import it came from and its index within that
//! import, so a content-derived [`CellId`] means the same thing in every run.
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
/// top bit keeps the two spaces disjoint by construction.
pub(crate) const CONTENT_ID_BIT: u64 = 1 << 63;

/// Whether a [`CellId`] was derived from content rather than allocated.
pub(crate) fn is_content_id(id: CellId) -> bool {
    id & CONTENT_ID_BIT != 0
}

/// Identifies one import: which GDS cell was asked for, from which file, and
/// under which scope name.
///
/// `scope_name` is part of the key because it becomes the imported top cell's
/// root scope name. Only the top cell is affected by it, so the structures
/// beneath are keyed without it and shared across scope names.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct GdsImportKey {
    pub declared_name: String,
    pub path: PathBuf,
    pub scope_name: Option<String>,
}

/// One import's compiled cells, in the order a fresh run inserts them.
///
/// Order is retained because `CompiledData::cells` is an `IndexMap` whose
/// iteration order reaches the GDS exporter and the serialized artifact.
#[derive(Clone, Debug)]
pub(crate) struct GdsImportEntry {
    pub top: CellId,
    pub cells: Vec<(CellId, Arc<CompiledCell>)>,
    /// How many ids a fresh import of this entry consumed from
    /// `ExecPass::next_id`.
    ///
    /// Replayed on a hit, which allocates a value per cell but no object ids,
    /// so that every id handed out afterwards is the one a fresh import would
    /// have handed out.
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

/// The [`CellId`] of a cell executed from source, derived from the cell's
/// fingerprint, its arguments and its scope name rather than from when it was
/// allocated.
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
        CellArgKey::Struct(name, fields) => {
            hasher.write_u8(6);
            hasher.write_usize(name.len());
            hasher.write(name.as_bytes());
            hasher.write_usize(fields.len());
            for (field, value) in fields {
                hasher.write_usize(field.len());
                hasher.write(field.as_bytes());
                hash_cell_arg_key(hasher, value);
            }
        }
        CellArgKey::Rect(layer, drawable, corners) => {
            hasher.write_u8(7);
            // Tagged by presence, so that no layer and an empty layer name
            // differ.
            match layer {
                Some(layer) => {
                    hasher.write_u8(1);
                    hasher.write_usize(layer.len());
                    hasher.write(layer.as_bytes());
                }
                None => hasher.write_u8(0),
            }
            hasher.write_u8(u8::from(*drawable));
            for bits in corners {
                hasher.write_u64(*bits);
            }
        }
        CellArgKey::Polygon(layer, drawable, points) => {
            hasher.write_u8(8);
            hasher.write_usize(layer.len());
            hasher.write(layer.as_bytes());
            hasher.write_u8(u8::from(*drawable));
            hash_points(hasher, points);
        }
        CellArgKey::Path(layer, drawable, lengths, points) => {
            hasher.write_u8(9);
            hasher.write_usize(layer.len());
            hasher.write(layer.as_bytes());
            hasher.write_u8(u8::from(*drawable));
            for bits in lengths {
                hasher.write_u64(*bits);
            }
            hash_points(hasher, points);
        }
        CellArgKey::Point(x, y) => {
            hasher.write_u8(10);
            hasher.write_u64(*x);
            hasher.write_u64(*y);
        }
    }
}

fn hash_points(hasher: &mut fnv::FnvHasher, points: &[(u64, u64)]) {
    hasher.write_usize(points.len());
    for (x, y) in points {
        hasher.write_u64(*x);
        hasher.write_u64(*y);
    }
}

/// The [`CellId`] of one structure within an import.
///
/// `top_scope_name` is mixed in only for the structure that becomes the
/// import's top cell, so that two scope names share every structure beneath
/// it.
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
