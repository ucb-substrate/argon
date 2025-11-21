use std::path::PathBuf;

#[cfg(windows)]
fn home_dir() -> Option<PathBuf> {
    env::var_os("USERPROFILE")
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .or_else(home_dir_crt)
}
#[cfg(unix)]
fn home_dir() -> Option<PathBuf> {
    std::env::home_dir()
}

// TODO: Allow configuration via ARGON_HOME environment variable.
pub fn default_argon_home() -> Option<PathBuf> {
    Some(home_dir()?.join(".local/state/argon"))
}
