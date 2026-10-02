//! Per-user records of running analyzers, so local clients can find them.

use std::{
    fs, io,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};

use crate::transport::SessionToken;

/// Where one analyzer can be reached, readable only by the user that ran it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionInfo {
    pub pid: u32,
    pub root: PathBuf,
    pub port: u16,
    pub token: String,
}

impl SessionInfo {
    pub fn token(&self) -> Option<SessionToken> {
        SessionToken::from_hex(&self.token)
    }
}

pub fn sessions_dir() -> Option<PathBuf> {
    crate::argon_state_dir().map(|directory| directory.join("sessions"))
}

/// A published session record, removed when dropped.
#[derive(Debug)]
pub struct SessionFile {
    path: PathBuf,
}

impl Drop for SessionFile {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

fn create_private_dir(directory: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(directory)?;
        fs::set_permissions(directory, fs::Permissions::from_mode(0o700))
    }
    #[cfg(not(unix))]
    {
        fs::create_dir_all(directory)
    }
}

fn write_private_file(path: &Path, contents: &[u8]) -> io::Result<()> {
    use std::io::Write;
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    file.write_all(contents)?;
    file.sync_all()
}

/// Publishes `info` in `directory` under the analyzer's process ID.
pub fn publish_in(directory: &Path, info: &SessionInfo) -> io::Result<SessionFile> {
    create_private_dir(directory)?;
    let path = directory.join(format!("{}.json", info.pid));
    let staging = directory.join(format!("{}.json.tmp", info.pid));
    let _ = fs::remove_file(&staging);
    let contents = serde_json::to_vec_pretty(info).map_err(io::Error::other)?;
    write_private_file(&staging, &contents)?;
    fs::rename(&staging, &path)?;
    Ok(SessionFile { path })
}

pub fn publish(info: &SessionInfo) -> io::Result<SessionFile> {
    let directory = sessions_dir()
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no state directory"))?;
    publish_in(&directory, info)
}

/// Resolves symlinks in the longest prefix of `path` that exists.
fn canonical(path: &Path) -> PathBuf {
    for ancestor in path.ancestors() {
        if let Ok(resolved) = ancestor.canonicalize() {
            let rest = path.strip_prefix(ancestor).unwrap_or(Path::new(""));
            return resolved.join(rest);
        }
    }
    path.to_path_buf()
}

/// Every readable session record in `directory`.
pub fn list_in(directory: &Path) -> Vec<(PathBuf, SessionInfo)> {
    let Ok(entries) = fs::read_dir(directory) else {
        return Vec::new();
    };
    entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "json")
        })
        .filter_map(|path| {
            let info = serde_json::from_slice(&fs::read(&path).ok()?).ok()?;
            Some((path, info))
        })
        .collect()
}

/// Sessions whose workspace contains `directory`, innermost workspace first.
pub fn discover_in(sessions: &Path, directory: &Path) -> Vec<(PathBuf, SessionInfo)> {
    let directory = canonical(directory);
    let mut matches = list_in(sessions)
        .into_iter()
        .filter_map(|(path, info)| {
            let root = canonical(&info.root);
            directory
                .starts_with(&root)
                .then(|| (root.components().count(), path, info))
        })
        .collect::<Vec<_>>();
    matches.sort_by(|(a_depth, a_path, _), (b_depth, b_path, _)| {
        let modified = |path: &Path| fs::metadata(path).and_then(|m| m.modified()).ok();
        b_depth
            .cmp(a_depth)
            .then_with(|| modified(b_path).cmp(&modified(a_path)))
    });
    matches
        .into_iter()
        .map(|(_, path, info)| (path, info))
        .collect()
}

pub fn discover(directory: &Path) -> Vec<(PathBuf, SessionInfo)> {
    sessions_dir()
        .map(|sessions| discover_in(&sessions, directory))
        .unwrap_or_default()
}

/// The session listening on `port`, for clients given only an address.
pub fn find_by_port(port: u16) -> Option<SessionInfo> {
    list_in(&sessions_dir()?)
        .into_iter()
        .map(|(_, info)| info)
        .find(|info| info.port == port)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(pid: u32, root: &Path, port: u16) -> SessionInfo {
        SessionInfo {
            pid,
            root: root.to_path_buf(),
            port,
            token: SessionToken::generate().unwrap().to_hex(),
        }
    }

    #[test]
    fn published_sessions_are_private_and_removed_on_drop() {
        let state = tempfile::tempdir().unwrap();
        let sessions = state.path().join("sessions");
        let file = publish_in(&sessions, &info(7, state.path(), 1234)).unwrap();
        let path = sessions.join("7.json");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&sessions).unwrap().permissions().mode() & 0o777,
                0o700
            );
            assert_eq!(
                fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        assert_eq!(list_in(&sessions).len(), 1);
        drop(file);
        assert!(!path.exists());
    }

    #[test]
    fn discovery_prefers_the_innermost_workspace() {
        let state = tempfile::tempdir().unwrap();
        let sessions = state.path().join("sessions");
        let outer = state.path().join("outer");
        let inner = outer.join("inner");
        let elsewhere = state.path().join("elsewhere");
        for directory in [&inner, &elsewhere] {
            fs::create_dir_all(directory).unwrap();
        }
        let _outer = publish_in(&sessions, &info(1, &outer, 1)).unwrap();
        let _inner = publish_in(&sessions, &info(2, &inner, 2)).unwrap();
        let _elsewhere = publish_in(&sessions, &info(3, &elsewhere, 3)).unwrap();

        let ports = |directory: &Path| {
            discover_in(&sessions, directory)
                .into_iter()
                .map(|(_, info)| info.port)
                .collect::<Vec<_>>()
        };
        assert_eq!(ports(&inner.join("src")), vec![2, 1]);
        assert_eq!(ports(&outer), vec![1]);
        assert_eq!(ports(state.path()), Vec::<u16>::new());
    }
}
