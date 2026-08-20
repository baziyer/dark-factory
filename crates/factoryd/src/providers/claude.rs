//! Non-interactive Claude Code provider.
//!
//! Each admitted run gets one fresh `claude -p` process and one startup
//! input. Output remains opaque; authority comes from the daemon's run state,
//! never from a provider stream decoder or a resumable process.

use std::path::PathBuf;

use serde_json::Value;

use crate::providers::{
    Capabilities, Provider, ProviderError, ProviderLaunch, SpawnContext, hooks,
};

pub const PERMISSION_MODES: [&str; 3] = ["default", "acceptEdits", "plan"];

fn validate_uuid(value: &str) -> Result<(), ()> {
    let parsed = uuid::Uuid::parse_str(value).map_err(|_| ())?;
    (parsed.hyphenated().to_string() == value)
        .then_some(())
        .ok_or(())
}

/// Launches one fresh `claude -p`; Claude requires the run ID in its
/// upstream `--session-id` argument.
#[derive(Clone, Copy, Debug, Default)]
pub struct ClaudeProvider;

impl Provider for ClaudeProvider {
    fn spawn_spec(&self, ctx: &SpawnContext) -> Result<ProviderLaunch, ProviderError> {
        let settings_path = ctx.agent_dir.join("claude-settings.json");
        let settings = claude_settings_json(&ctx.factoryctl_path, &ctx.hook_token_path);
        let bytes = serde_json::to_vec_pretty(&settings)
            .expect("claude settings json is always representable");
        hooks::write_private_file(&settings_path, &bytes).map_err(|source| {
            ProviderError::Config {
                path: settings_path.clone(),
                source,
            }
        })?;

        let run_id = ctx.run_id.as_str();
        validate_uuid(run_id).map_err(|_| ProviderError::RunIdNotUuid {
            run_id: run_id.to_owned(),
        })?;

        let mut args = vec![
            "-p".to_owned(),
            "--settings".to_owned(),
            settings_path.to_string_lossy().into_owned(),
            "--session-id".to_owned(),
            run_id.to_owned(),
        ];
        if let Some(model) = &ctx.model {
            args.push("--model".to_owned());
            args.push(model.clone());
        }
        if let Some(permission_mode) = &ctx.permission_mode {
            args.push("--permission-mode".to_owned());
            args.push(permission_mode.clone());
        } else if ctx.auto_mode {
            args.push("--permission-mode".to_owned());
            args.push("bypassPermissions".to_owned());
        }

        Ok(ProviderLaunch {
            program: PathBuf::from("claude"),
            args,
            env: Vec::new(),
            startup_input: ctx.startup_input.clone(),
        })
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities::for_provider(factory_core::Provider::ClaudeCode, &PERMISSION_MODES)
    }
}

fn claude_settings_json(
    factoryctl_path: &std::path::Path,
    hook_token_path: &std::path::Path,
) -> Value {
    let event = factory_core::ProviderHookEvent::PreToolUse;
    let command = hooks::hook_command(factoryctl_path, hook_token_path, event);
    serde_json::json!({
        "hooks": {
            "PreToolUse": [{
                "matcher": "*",
                "hooks": [{ "type": "command", "command": command }]
            }]
        }
    })
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use factory_core::RunId;

    use super::*;

    fn context(directory: &std::path::Path) -> SpawnContext {
        SpawnContext {
            run_id: RunId::try_from("2f5a1e2e-2222-4444-8888-0123456789ab").unwrap(),
            worktree: directory.join("worktree"),
            startup_input: b"fix the admitted task".to_vec(),
            model: None,
            reasoning_effort: None,
            permission_mode: None,
            auto_mode: true,
            hook_token_path: directory.join("runtime/hook.token"),
            factoryctl_path: PathBuf::from("/abs/factoryctl"),
            agent_dir: directory.join("agent-dir"),
        }
    }

    #[test]
    fn launch_is_fresh_noninteractive_and_carries_startup_input() {
        let directory = tempfile::tempdir().unwrap();
        let ctx = context(directory.path());
        let launch = ClaudeProvider.spawn_spec(&ctx).unwrap();

        assert_eq!(launch.program, PathBuf::from("claude"));
        assert_eq!(launch.args.first().map(String::as_str), Some("-p"));
        assert!(
            launch
                .args
                .windows(2)
                .any(|pair| pair == ["--session-id", "2f5a1e2e-2222-4444-8888-0123456789ab"])
        );
        assert!(!launch.args.iter().any(|arg| arg == "--resume"));
        assert_eq!(launch.startup_input, b"fix the admitted task");
    }

    #[test]
    fn non_uuid_run_id_is_refused() {
        let directory = tempfile::tempdir().unwrap();
        let mut ctx = context(directory.path());
        ctx.run_id = RunId::try_from("not-a-uuid").unwrap();
        assert!(matches!(
            ClaudeProvider.spawn_spec(&ctx),
            Err(ProviderError::RunIdNotUuid { .. })
        ));
    }

    #[test]
    fn spawn_writes_private_settings_with_only_the_authority_hook() {
        let directory = tempfile::tempdir().unwrap();
        let ctx = context(directory.path());
        ClaudeProvider.spawn_spec(&ctx).unwrap();
        let settings_path = ctx.agent_dir.join("claude-settings.json");
        let metadata = std::fs::metadata(&settings_path).unwrap();
        assert_eq!(metadata.permissions().mode() & 0o777, 0o600);
        let parsed: Value =
            serde_json::from_str(&std::fs::read_to_string(&settings_path).unwrap()).unwrap();
        assert_eq!(
            parsed["hooks"].as_object().map(serde_json::Map::len),
            Some(1)
        );
        assert!(parsed["hooks"]["PreToolUse"].is_array());
    }
}
