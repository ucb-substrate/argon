use std::path::Path;

fn analyzer_executable() -> &'static Path {
    Path::new(env!("CARGO_BIN_EXE_argon-test-analyzer"))
}

#[tokio::test(flavor = "multi_thread")]
async fn gui_edit_round_trips_through_nvim_and_back_to_gui() {
    argon_tests::full_stack::gui_edit_roundtrip(analyzer_executable()).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn analyzer_diagnostics_recover_in_nvim_and_gui() {
    argon_tests::full_stack::diagnostic_recovery(analyzer_executable()).await;
}

#[tokio::test]
async fn diagnostics_panel_renders_and_navigates_all_entries() {
    let mut command = argon_tests::nvim_command();
    command
        .arg("-l")
        .arg(argon_tests::repository_root().join("crates/tests/fixtures/nvim/diagnostics.lua"));
    let child = command.spawn().expect("start headless Neovim");
    argon_tests::finish_nvim(child).await;
}
