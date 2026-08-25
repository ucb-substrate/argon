#[tokio::test]
async fn diagnostics_panel_renders_and_navigates_all_entries() {
    let mut command = crate::nvim_command();
    command
        .arg("-l")
        .arg(crate::repository_root().join("crates/tests/fixtures/nvim/diagnostics.lua"));
    let child = command.spawn().expect("start headless Neovim");
    crate::finish_nvim(child).await;
}
