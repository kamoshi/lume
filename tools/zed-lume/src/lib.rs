use zed_extension_api::{self as zed, LanguageServerId, Result};

struct LumeExtension;

impl zed::Extension for LumeExtension {
    fn new() -> Self {
        Self
    }

    fn language_server_command(
        &mut self,
        _language_server_id: &LanguageServerId,
        worktree: &zed::Worktree,
    ) -> Result<zed::Command> {
        let path = worktree
            .which("lume-lsp")
            .ok_or_else(|| "lume-lsp not found in PATH".to_string())?;

        Ok(zed::Command {
            command: path,
            args: vec![],
            env: vec![],
        })
    }
}

zed::register_extension!(LumeExtension);
