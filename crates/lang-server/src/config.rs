use std::path::PathBuf;

// TODO: Allow configuration via ARGON_HOME environment variable.
pub fn default_argon_home() -> Option<PathBuf> {
    Some(homedir::my_home().ok()??.join(".local/state/argon"))
}
