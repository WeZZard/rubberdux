use zed_extension_api::{self as zed, Command, LanguageServerId, Worktree};

struct MdTestingExtension;

impl zed::Extension for MdTestingExtension {
    fn new() -> Self {
        Self
    }

    fn language_server_command(
        &mut self,
        _language_server_id: &LanguageServerId,
        worktree: &Worktree,
    ) -> zed::Result<Command> {
        // Build path to the LSP binary from the workspace root
        let lsp_path = format!(
            "{}/target/release/md-testing-lsp",
            worktree.root_path()
        );

        Ok(Command {
            command: lsp_path,
            args: vec![],
            env: vec![],
        })
    }
}

zed::register_extension!(MdTestingExtension);
