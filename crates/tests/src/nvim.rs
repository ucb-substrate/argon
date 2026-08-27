#[cfg(test)]
mod tests {
    #[tokio::test]
    async fn diagnostics_panel_renders_and_navigates_all_entries() {
        let mut command = crate::nvim_command();
        command
            .arg("-l")
            .arg(crate::repository_root().join("crates/tests/fixtures/nvim/diagnostics.lua"));
        let child = command.spawn().expect("start headless Neovim");
        crate::finish_nvim(child).await;
    }

    #[tokio::test]
    async fn save_writes_only_modified_attached_argon_buffers() {
        let mut command = crate::nvim_command();
        command
            .arg("-l")
            .arg(crate::repository_root().join("crates/tests/fixtures/nvim/save.lua"));
        let child = command.spawn().expect("start headless Neovim");
        crate::finish_nvim(child).await;
    }

    #[tokio::test]
    async fn cancelled_gui_command_returns_focus_to_gui() {
        let mut command = crate::nvim_command();
        command
            .arg("-l")
            .arg(crate::repository_root().join("crates/tests/fixtures/nvim/focus.lua"));
        let child = command.spawn().expect("start headless Neovim");
        crate::finish_nvim(child).await;
    }
}
