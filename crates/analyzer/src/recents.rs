//! The cell invocations recently opened in a library, stored in the library.

use std::{collections::HashSet, fs, io, path::Path};

use argonc::parse;
use serde::{Deserialize, Serialize};
use tracing::{error, warn};

const RECENTS_DIR: &str = ".argon";
const RECENTS_FILE: &str = "recents.json";
const MAX_RECENT_CELLS: usize = 10;

#[derive(Serialize, Deserialize)]
struct RecentsFile {
    cells: Vec<RecentEntry>,
}

#[derive(Serialize, Deserialize)]
struct RecentEntry {
    invocation: String,
}

/// The library's recent invocations, most recent first.
pub(crate) fn load(root: &Path) -> Vec<String> {
    normalize(read(root))
}

/// Moves `invocation` to the front of the library's recent invocations.
pub(crate) fn record(root: &Path, invocation: &str) -> Vec<String> {
    let cells = normalize(std::iter::once(invocation.to_owned()).chain(read(root)));
    save(root, &cells);
    cells
}

/// Replaces each recent invocation that `rename` maps to a new one.
pub(crate) fn rewrite(root: &Path, rename: impl Fn(&str) -> Option<String>) -> Vec<String> {
    let previous = read(root);
    let cells = normalize(
        previous
            .iter()
            .map(|cell| rename(cell).unwrap_or_else(|| cell.clone())),
    );
    if cells != previous {
        save(root, &cells);
    }
    cells
}

/// Trims each invocation, and drops those that do not parse or repeat an
/// earlier one.
fn normalize(cells: impl IntoIterator<Item = String>) -> Vec<String> {
    let mut keys = HashSet::new();
    cells
        .into_iter()
        .map(|cell| cell.trim().to_owned())
        .filter(|cell| parse::invocation_key(cell).is_some_and(|key| keys.insert(key)))
        .take(MAX_RECENT_CELLS)
        .collect()
}

/// The invocations in the recents file. A missing or malformed file has none.
fn read(root: &Path) -> Vec<String> {
    let path = root.join(RECENTS_DIR).join(RECENTS_FILE);
    let contents = match fs::read(&path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Vec::new(),
        Err(error) => {
            warn!("could not read {}: {error}", path.display());
            return Vec::new();
        }
    };
    match serde_json::from_slice::<RecentsFile>(&contents) {
        Ok(file) => file
            .cells
            .into_iter()
            .map(|entry| entry.invocation)
            .collect(),
        Err(error) => {
            warn!("ignoring malformed {}: {error}", path.display());
            Vec::new()
        }
    }
}

fn save(root: &Path, cells: &[String]) {
    if let Err(error) = write(root, cells) {
        error!(
            "could not save recent cells in {}: {error}",
            root.join(RECENTS_DIR).display()
        );
    }
}

/// Writes the recents file. A new `.argon` directory gets a `.gitignore` that
/// ignores the directory's contents.
fn write(root: &Path, cells: &[String]) -> io::Result<()> {
    let directory = root.join(RECENTS_DIR);
    match fs::create_dir(&directory) {
        Ok(()) => fs::write(directory.join(".gitignore"), "*\n")?,
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error),
    }
    let file = RecentsFile {
        cells: cells
            .iter()
            .map(|invocation| RecentEntry {
                invocation: invocation.clone(),
            })
            .collect(),
    };
    let contents = serde_json::to_vec_pretty(&file).map_err(io::Error::other)?;
    let staging = directory.join(format!("{RECENTS_FILE}.tmp"));
    fs::write(&staging, contents)?;
    fs::rename(&staging, directory.join(RECENTS_FILE))
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    fn recents_path(root: &Path) -> std::path::PathBuf {
        root.join(RECENTS_DIR).join(RECENTS_FILE)
    }

    #[test]
    fn recording_moves_an_invocation_to_the_front() {
        let root = tempfile::tempdir().unwrap();
        let root = root.path();
        assert!(load(root).is_empty());
        record(root, "inv(1., 2)");
        record(root, "top()");
        assert_eq!(record(root, " inv( 1.,2 ) "), ["inv( 1.,2 )", "top()"]);
        assert_eq!(load(root), ["inv( 1.,2 )", "top()"]);
        let contents = fs::read_to_string(recents_path(root)).unwrap();
        let file: serde_json::Value = serde_json::from_str(&contents).unwrap();
        assert_eq!(
            file,
            serde_json::json!({
                "cells": [{ "invocation": "inv( 1.,2 )" }, { "invocation": "top()" }]
            })
        );
    }

    #[test]
    fn recording_keeps_the_most_recent_invocations() {
        let root = tempfile::tempdir().unwrap();
        for width in 0..12 {
            record(root.path(), &format!("inv({width})"));
        }
        let expected = (2..12).rev().map(|width| format!("inv({width})"));
        assert_eq!(load(root.path()), expected.collect::<Vec<_>>());
    }

    #[test]
    fn a_malformed_file_loads_as_empty_and_is_replaced() {
        let root = tempfile::tempdir().unwrap();
        let root = root.path();
        fs::create_dir(root.join(RECENTS_DIR)).unwrap();
        fs::write(recents_path(root), "{ not json").unwrap();
        assert!(load(root).is_empty());
        assert_eq!(record(root, "top()"), ["top()"]);
        assert_eq!(load(root), ["top()"]);
    }

    #[test]
    fn the_gitignore_is_written_only_with_a_new_directory() {
        let root = tempfile::tempdir().unwrap();
        let gitignore = root.path().join(RECENTS_DIR).join(".gitignore");
        record(root.path(), "top()");
        assert_eq!(fs::read_to_string(&gitignore).unwrap(), "*\n");
        fs::write(&gitignore, "recents.json\n").unwrap();
        record(root.path(), "inv(2)");
        assert_eq!(fs::read_to_string(&gitignore).unwrap(), "recents.json\n");

        let existing = tempfile::tempdir().unwrap();
        fs::create_dir(existing.path().join(RECENTS_DIR)).unwrap();
        record(existing.path(), "top()");
        assert!(
            !existing
                .path()
                .join(RECENTS_DIR)
                .join(".gitignore")
                .exists()
        );
    }

    #[test]
    fn rewriting_renames_only_matching_invocations() {
        let root = tempfile::tempdir().unwrap();
        let root = root.path();
        let rename = |cell: &str| {
            cell.starts_with("inv(")
                .then(|| cell.replacen("inv", "unit", 1))
        };
        assert!(rewrite(root, rename).is_empty());
        assert!(
            !root.join(RECENTS_DIR).exists(),
            "nothing changed, so nothing is written"
        );

        for cell in ["unit(3)", "inv(2)", "top()", "inv(3)"] {
            record(root, cell);
        }
        let renamed = ["unit(3)", "top()", "unit(2)"];
        assert_eq!(rewrite(root, rename), renamed);
        assert_eq!(load(root), renamed);
    }
}
