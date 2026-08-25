//! Shared infrastructure for Argon's cross-component integration tests.

pub mod full_stack;

use std::{path::PathBuf, process::Stdio, time::Duration};

use tokio::{process::Command, time};

pub const TEST_TIMEOUT: Duration = Duration::from_secs(30);

pub fn repository_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// A consistently isolated headless Neovim process for integration tests.
pub fn nvim_command() -> Command {
    let mut command = Command::new("nvim");
    command
        .kill_on_drop(true)
        .env("ARGON_REPOSITORY_ROOT", repository_root())
        .arg("--headless")
        .arg("-u")
        .arg("NONE")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    command
}

pub async fn finish_nvim(child: tokio::process::Child) {
    let output = time::timeout(TEST_TIMEOUT, child.wait_with_output())
        .await
        .expect("headless Neovim timed out")
        .expect("wait for headless Neovim");
    assert!(
        output.status.success(),
        "headless Neovim failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
}
