//! [`ClaudeProvider`]: the interactive-session [`Provider`] impl for Claude
//! Code, plus the generated `claude-settings.json` hooks configuration it
//! writes per session. See `docs/providers.md`.
//!
//! Track 5's pivot from a non-interactive `claude -p --output-format
//! stream-json` pipe-mode adapter (session identity confirmed by decoding
//! Claude's own JSON event stream) to a resident interactive `claude`
//! process under a PTY (session identity assigned up front by the daemon,
//! state driven by hooks) deleted that whole decoder here: `Decoder`,
//! `Observation`, `ToolState`/`InitState`/`ResultState`, `Outcome`,
//! `FailureReason`, the non-interactive `prepare()`, and their fixtures/tests
//! (`crates/factoryd/tests/claude.rs`) are gone (~750 LOC, see
//! `TRACK5-DESIGN.md` §7). `validate_uuid` is the one piece that survived
//! unchanged: both the old decoder and the new [`ClaudeProvider`] need to
//! confirm a Claude session identity is a canonical UUID.

use std::path::PathBuf;

use factory_core::ProviderHookEvent;
use serde_json::Value;

use crate::providers::{
    Capabilities, InteractiveLaunch, Provider, ProviderError, SpawnContext, hooks,
};

fn validate_uuid(value: &str) -> Result<(), ()> {
    let parsed = uuid::Uuid::parse_str(value).map_err(|_| ())?;
    if parsed.hyphenated().to_string() == value {
        Ok(())
    } else {
        Err(())
    }
}

pub const PERMISSION_MODES: [&str; 3] = ["default", "acceptEdits", "plan"];

/// Interactive-session [`Provider`] for Claude Code. Launches
/// `claude --settings <agent-dir>/claude-settings.json (--session-id <uuid>
/// | --resume <id>) [--model M] [--permission-mode M]` for the session
/// runner to run under a PTY (the PTY itself is the runner's concern, not
/// this type's). Deliberately omits `-p`, `--input-format`,
/// `--output-format`, `--verbose`, `--safe-mode` (disables hooks —
/// unusable here), `--max-turns`, and `--max-budget-usd` (print-mode only):
/// see `docs/providers.md`.
#[derive(Clone, Copy, Debug, Default)]
pub struct ClaudeProvider;

impl Provider for ClaudeProvider {
    fn spawn_spec(&self, ctx: &SpawnContext) -> Result<InteractiveLaunch, ProviderError> {
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

        let mut args = vec![
            "--settings".to_owned(),
            settings_path.to_string_lossy().into_owned(),
        ];
        match &ctx.resume {
            Some(provider_session_id) => {
                validate_uuid(provider_session_id).map_err(|_| ProviderError::ResumeIdNotUuid {
                    value: provider_session_id.clone(),
                })?;
                args.push("--resume".to_owned());
                args.push(provider_session_id.clone());
            }
            None => {
                // A fresh Claude launch reuses this session's own daemon
                // identity as the Claude session UUID: the daemon assigns
                // it up front instead of learning it back from a hook
                // payload (`TRACK5-DESIGN.md` §1). Daemon-generated
                // `SessionId`s are UUIDs by construction; this is validated
                // defensively rather than assumed.
                let session_id = ctx.session_id.as_str();
                validate_uuid(session_id).map_err(|_| ProviderError::SessionIdNotUuid {
                    session_id: session_id.to_owned(),
                })?;
                args.push("--session-id".to_owned());
                args.push(session_id.to_owned());
            }
        }
        if let Some(model) = &ctx.model {
            args.push("--model".to_owned());
            args.push(model.clone());
        }
        if let Some(permission_mode) = &ctx.permission_mode {
            args.push("--permission-mode".to_owned());
            args.push(permission_mode.clone());
        }

        Ok(InteractiveLaunch {
            program: PathBuf::from("claude"),
            args,
            env: Vec::new(),
            generated_files: vec![settings_path],
        })
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            hooks: true,
            resume: true,
            permission_modes: &PERMISSION_MODES,
        }
    }
}

/// Builds the exact `claude-settings.json` contents: one hook per event in
/// [`hooks::HOOK_EVENTS`], `PreToolUse`/`PostToolUse` matching every tool
/// (`"matcher": "*"`), each pointing at `factoryctl hook --token-file
/// <hook_token_path> <Event>`.
fn claude_settings_json(
    factoryctl_path: &std::path::Path,
    hook_token_path: &std::path::Path,
) -> Value {
    let mut hooks_object = serde_json::Map::new();
    for event in hooks::HOOK_EVENTS {
        let command = hooks::hook_command(factoryctl_path, hook_token_path, event);
        let mut entry = serde_json::Map::new();
        if matches!(
            event,
            ProviderHookEvent::PreToolUse | ProviderHookEvent::PostToolUse
        ) {
            entry.insert("matcher".to_owned(), Value::String("*".to_owned()));
        }
        entry.insert(
            "hooks".to_owned(),
            serde_json::json!([{ "type": "command", "command": command }]),
        );
        hooks_object.insert(
            event.provider_event_name().to_owned(),
            Value::Array(vec![Value::Object(entry)]),
        );
    }
    serde_json::json!({ "hooks": Value::Object(hooks_object) })
}

#[cfg(test)]
mod provider_tests {
    use std::os::unix::fs::PermissionsExt;

    use factory_core::{AgentId, ProjectId, SessionId};

    use super::*;

    fn context(directory: &std::path::Path) -> SpawnContext {
        SpawnContext {
            agent_id: AgentId::try_from("worker-1").unwrap(),
            project_id: ProjectId::try_from("factory").unwrap(),
            session_id: SessionId::try_from("2f5a1e2e-2222-4444-8888-0123456789ab").unwrap(),
            worktree: directory.join("worktree"),
            model: None,
            permission_mode: None,
            resume: None,
            hook_token_path: directory.join("runtime").join("hook.token"),
            factoryctl_path: PathBuf::from("/abs/factoryctl"),
            agent_dir: directory.join("agent-dir"),
            socket_path: directory.join("f.sock"),
        }
    }

    #[test]
    fn fresh_launch_uses_the_session_id_as_the_claude_session_uuid() {
        let directory = tempfile::tempdir().unwrap();
        let ctx = context(directory.path());
        let launch = ClaudeProvider.spawn_spec(&ctx).unwrap();

        assert_eq!(launch.program, PathBuf::from("claude"));
        assert_eq!(
            launch.args,
            vec![
                "--settings".to_owned(),
                directory
                    .path()
                    .join("agent-dir")
                    .join("claude-settings.json")
                    .to_string_lossy()
                    .into_owned(),
                "--session-id".to_owned(),
                "2f5a1e2e-2222-4444-8888-0123456789ab".to_owned(),
            ]
        );
        assert!(launch.env.is_empty());
        assert_eq!(launch.generated_files.len(), 1);
    }

    #[test]
    fn resume_launch_passes_the_prior_provider_session_id_and_model_and_permission_mode() {
        let directory = tempfile::tempdir().unwrap();
        let mut ctx = context(directory.path());
        ctx.resume = Some("9b1deb4d-3b7d-4bad-9bdd-2b0d7b3dcb6d".to_owned());
        ctx.model = Some("claude-sonnet-5".to_owned());
        ctx.permission_mode = Some("acceptEdits".to_owned());
        let launch = ClaudeProvider.spawn_spec(&ctx).unwrap();

        assert_eq!(
            launch.args,
            vec![
                "--settings".to_owned(),
                directory
                    .path()
                    .join("agent-dir")
                    .join("claude-settings.json")
                    .to_string_lossy()
                    .into_owned(),
                "--resume".to_owned(),
                "9b1deb4d-3b7d-4bad-9bdd-2b0d7b3dcb6d".to_owned(),
                "--model".to_owned(),
                "claude-sonnet-5".to_owned(),
                "--permission-mode".to_owned(),
                "acceptEdits".to_owned(),
            ]
        );
    }

    #[test]
    fn fresh_launch_rejects_a_non_uuid_session_id() {
        let directory = tempfile::tempdir().unwrap();
        let mut ctx = context(directory.path());
        ctx.session_id = SessionId::try_from("not-a-uuid").unwrap();
        assert!(matches!(
            ClaudeProvider.spawn_spec(&ctx),
            Err(ProviderError::SessionIdNotUuid { .. })
        ));
    }

    #[test]
    fn resume_rejects_a_non_uuid_resume_target() {
        let directory = tempfile::tempdir().unwrap();
        let mut ctx = context(directory.path());
        ctx.resume = Some("not-a-uuid".to_owned());
        assert!(matches!(
            ClaudeProvider.spawn_spec(&ctx),
            Err(ProviderError::ResumeIdNotUuid { .. })
        ));
    }

    #[test]
    fn settings_json_has_every_event_pre_and_post_tool_use_matching_every_tool() {
        let value = claude_settings_json(
            std::path::Path::new("/abs/factoryctl"),
            std::path::Path::new("/abs/runs/session-1/hook.token"),
        );
        let expected = serde_json::json!({
            "hooks": {
                "SessionStart": [{"hooks": [{"type": "command",
                    "command": "'/abs/factoryctl' hook --token-file '/abs/runs/session-1/hook.token' SessionStart"}]}],
                "UserPromptSubmit": [{"hooks": [{"type": "command",
                    "command": "'/abs/factoryctl' hook --token-file '/abs/runs/session-1/hook.token' UserPromptSubmit"}]}],
                "PreToolUse": [{"matcher": "*", "hooks": [{"type": "command",
                    "command": "'/abs/factoryctl' hook --token-file '/abs/runs/session-1/hook.token' PreToolUse"}]}],
                "PostToolUse": [{"matcher": "*", "hooks": [{"type": "command",
                    "command": "'/abs/factoryctl' hook --token-file '/abs/runs/session-1/hook.token' PostToolUse"}]}],
                "Notification": [{"hooks": [{"type": "command",
                    "command": "'/abs/factoryctl' hook --token-file '/abs/runs/session-1/hook.token' Notification"}]}],
                "Stop": [{"hooks": [{"type": "command",
                    "command": "'/abs/factoryctl' hook --token-file '/abs/runs/session-1/hook.token' Stop"}]}],
                "SubagentStop": [{"hooks": [{"type": "command",
                    "command": "'/abs/factoryctl' hook --token-file '/abs/runs/session-1/hook.token' SubagentStop"}]}],
                "SessionEnd": [{"hooks": [{"type": "command",
                    "command": "'/abs/factoryctl' hook --token-file '/abs/runs/session-1/hook.token' SessionEnd"}]}]
            }
        });
        assert_eq!(value, expected);
    }

    #[test]
    fn spawn_spec_writes_settings_json_atomically_at_mode_0600() {
        let directory = tempfile::tempdir().unwrap();
        let ctx = context(directory.path());
        let launch = ClaudeProvider.spawn_spec(&ctx).unwrap();
        let settings_path = &launch.generated_files[0];
        let metadata = std::fs::metadata(settings_path).unwrap();
        assert_eq!(metadata.permissions().mode() & 0o777, 0o600);
        let parsed: Value =
            serde_json::from_str(&std::fs::read_to_string(settings_path).unwrap()).unwrap();
        assert!(parsed.get("hooks").is_some());
    }

    #[test]
    fn capabilities_declare_hooks_resume_and_the_supported_permission_modes() {
        let capabilities = ClaudeProvider.capabilities();
        assert!(capabilities.hooks);
        assert!(capabilities.resume);
        assert_eq!(capabilities.permission_modes, PERMISSION_MODES);
    }
}
