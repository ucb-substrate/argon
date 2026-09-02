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
    async fn gui_commands_respect_return_focus_configuration() {
        let mut command = crate::nvim_command();
        command
            .arg("-l")
            .arg(crate::repository_root().join("crates/tests/fixtures/nvim/focus.lua"));
        let child = command.spawn().expect("start headless Neovim");
        crate::finish_nvim(child).await;
    }

    #[tokio::test]
    async fn compilation_status_renders_and_cleans_up_progress() {
        let mut command = crate::nvim_command();
        command
            .arg("-l")
            .arg(crate::repository_root().join("crates/tests/fixtures/nvim/server_status.lua"));
        let child = command.spawn().expect("start headless Neovim");
        crate::finish_nvim(child).await;
    }
}
