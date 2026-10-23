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
    async fn gui_commands_become_argon_command_lines() {
        use analyzer::rpc::EditorCommand;

        let commands = [
            EditorCommand::Messages,
            EditorCommand::prefill("inst", ""),
            EditorCommand::prefill("openCell", "pad<1>("),
            EditorCommand::run("diagnostics"),
        ];
        let mut command = crate::nvim_command();
        command
            .env(
                "ARGON_EDITOR_COMMANDS",
                serde_json::to_string(&commands).unwrap(),
            )
            .arg("-l")
            .arg(crate::repository_root().join("crates/tests/fixtures/nvim/command_line.lua"));
        let child = command.spawn().expect("start headless Neovim");
        crate::finish_nvim(child).await;
    }

    #[tokio::test]
    async fn embedded_gui_leaves_ctrl_backslash_and_flags_the_analyzer() {
        let mut command = crate::nvim_command();
        command
            .arg("-l")
            .arg(crate::repository_root().join("crates/tests/fixtures/nvim/embedded.lua"));
        let child = command.spawn().expect("start headless Neovim");
        crate::finish_nvim(child).await;
    }

    #[tokio::test]
    async fn command_arguments_complete_against_the_analyzer() {
        let mut command = crate::nvim_command();
        command.arg("-l").arg(
            crate::repository_root().join("crates/tests/fixtures/nvim/command_completion.lua"),
        );
        let child = command.spawn().expect("start headless Neovim");
        crate::finish_nvim(child).await;
    }
}
