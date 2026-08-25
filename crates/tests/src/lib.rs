//! Shared infrastructure for Argon's cross-component integration tests.

#[cfg(test)]
mod full_stack;
#[cfg(test)]
mod nvim;

#[cfg(test)]
use std::{path::PathBuf, process::Stdio, time::Duration};

#[cfg(test)]
use tokio::{process::Command, time};

#[cfg(test)]
const TEST_TIMEOUT: Duration = Duration::from_secs(30);

#[cfg(test)]
fn repository_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// A consistently isolated headless Neovim process for integration tests.
#[cfg(test)]
fn nvim_command() -> Command {
    let mut command = Command::new("nvim");
    command
        .kill_on_drop(true)
        .env("ARGON_REPOSITORY_ROOT", repository_root())
        .env(
            "NVIM_LOG_FILE",
            std::env::temp_dir().join(format!("argon-nvim-test-{}.log", std::process::id())),
        )
        .arg("--headless")
        .arg("-u")
        .arg("NONE")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    command
}

#[cfg(test)]
async fn finish_nvim(child: tokio::process::Child) {
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
