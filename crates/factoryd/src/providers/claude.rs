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

use std::{
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
};

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
#[derive(Clone, Debug, Default)]
pub struct ClaudeProvider {
    /// The operator's real home directory, used to locate `~/.claude.json`
    /// for [`pretrust_worktree`]. Defaults to `$HOME`; overridable for
    /// tests via [`ClaudeProvider::with_home`] so exercising `spawn_spec`
    /// in a unit test never touches the real `~/.claude.json` on the
    /// machine running the test suite (the same reason
    /// `CodexProvider::with_source_home` exists).
    home: Option<PathBuf>,
}

impl ClaudeProvider {
    /// Resolves the pre-trust target from `$HOME`, matching Claude Code's
    /// own default lookup for `~/.claude.json`. `None` (no `$HOME`) means
    /// [`Provider::spawn_spec`] skips pre-trusting entirely -- the same
    /// outcome as `pretrust_worktree` finding no readable file.
    #[must_use]
    pub fn new() -> Self {
        Self {
            home: std::env::var_os("HOME").map(PathBuf::from),
        }
    }

    #[cfg(test)]
    fn with_home(home: PathBuf) -> Self {
        Self { home: Some(home) }
    }
}

impl Provider for ClaudeProvider {
    fn spawn_spec(&self, ctx: &SpawnContext) -> Result<InteractiveLaunch, ProviderError> {
        // Best-effort, before anything else: a worktree Dark Factory itself
        // created for this agent (never handed to it by an untrusted
        // source) should never block a fresh, unattended session on
        // Claude's interactive "do you trust this directory?" prompt. See
        // `pretrust_worktree`'s own doc comment for exactly what this does
        // and does not touch.
        if let Some(home) = &self.home {
            pretrust_worktree(&ctx.worktree, home);
        }

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

/// Marks `worktree` as trusted in the operator's real `~/.claude.json` --
/// Claude Code's own trust-dialog state, not anything Dark Factory owns --
/// so a fresh session in a git worktree Dark Factory itself created (never
/// handed to it by an untrusted source, D3) never blocks on Claude's
/// interactive "do you trust the files in this folder?" prompt. The real
/// key, confirmed against this machine's own `~/.claude.json`:
/// `projects[<absolute worktree path>].hasTrustDialogAccepted = true`,
/// alongside dozens of other per-project fields (`allowedTools`,
/// `lastSessionId`, ...) this must not disturb.
///
/// Best-effort and read-mostly, by design: if `home/.claude.json` does not
/// exist, is not readable, or does not parse as a JSON object, this logs a
/// warning and returns without writing anything at all -- it never creates
/// or overwrites a file it cannot first read and understand (a real user
/// file Dark Factory does not own, unlike its own generated
/// `claude-settings.json`/`codex-home`). Every other key and field in the
/// file, including every other project's own trust state, round-trips
/// unchanged; only the one project entry for `worktree` gains (or keeps)
/// `hasTrustDialogAccepted: true`. Written atomically (temp file plus
/// rename, in the same directory as `~/.claude.json` itself) so a crash
/// mid-write can never leave a corrupt file behind, and at the file's own
/// existing permissions (not Dark Factory's usual private `0600`) -- this
/// function only ever edits a file that already exists at whatever mode
/// the operator's own Claude Code installation created it at.
pub fn pretrust_worktree(worktree: &Path, home: &Path) {
    let path = home.join(".claude.json");
    if let Err(reason) = try_pretrust_worktree(worktree, &path) {
        tracing::warn!(
            worktree = %worktree.display(),
            path = %path.display(),
            %reason,
            "could not pre-trust worktree in ~/.claude.json"
        );
    }
}

fn try_pretrust_worktree(worktree: &Path, path: &Path) -> Result<(), String> {
    let mode = std::fs::metadata(path)
        .map_err(|error| format!("cannot read metadata: {error}"))?
        .permissions()
        .mode()
        & 0o777;
    let contents =
        std::fs::read_to_string(path).map_err(|error| format!("cannot read file: {error}"))?;
    let Value::Object(mut document) =
        serde_json::from_str(&contents).map_err(|_| "not valid JSON".to_owned())?
    else {
        return Err("top level is not a JSON object".to_owned());
    };
    let projects = document
        .entry("projects")
        .or_insert_with(|| Value::Object(serde_json::Map::new()));
    let Value::Object(projects) = projects else {
        return Err("\"projects\" is not a JSON object".to_owned());
    };
    let worktree_key = worktree.to_string_lossy().into_owned();
    let project = projects
        .entry(worktree_key)
        .or_insert_with(|| Value::Object(serde_json::Map::new()));
    let Value::Object(project) = project else {
        return Err(format!(
            "projects[{}] is not a JSON object",
            worktree.display()
        ));
    };
    project.insert("hasTrustDialogAccepted".to_owned(), Value::Bool(true));

    let bytes = serde_json::to_vec_pretty(&Value::Object(document))
        .map_err(|error| format!("cannot serialize: {error}"))?;
    hooks::write_file_with_mode(path, &bytes, mode)
        .map_err(|error| format!("cannot write file: {error}"))
}

/// A resident session's own `factoryctl` calls (the composed delivery
/// text's "when finished, run: `factoryctl task done ...`", and every
/// generated hook command) are themselves `Bash` tool calls from Claude's
/// own point of view -- without this, Claude's default interactive
/// permission posture (TRACK5-DESIGN.md §5: no `--permission-mode` is
/// passed, the native prompt is the human-in-the-loop gate) would stop and
/// ask the operator to approve every single one, defeating "task done"
/// ever completing unattended. `Bash(factoryctl *)` is the exact rule shape
/// Claude Code's own `--allowedTools`/`settings.json` documentation and
/// this machine's real `~/.claude/settings.local.json` files use for a
/// command-prefix allow rule (e.g. `"Bash(git fetch *)"`) -- verified
/// against `claude --help`'s own example (`"Bash(git *)`) and several real
/// `permissions.allow` lists on this machine, not assumed. Deliberately
/// narrow: only `factoryctl` is pre-approved, nothing else -- an operator
/// still approves (or configures their own broader allowlist for) every
/// other tool call.
const CLAUDE_ALLOWED_BASH_PREFIXES: [&str; 1] = ["Bash(factoryctl *)"];

/// Builds the exact `claude-settings.json` contents: one hook per event in
/// [`hooks::HOOK_EVENTS`], `PreToolUse`/`PostToolUse` matching every tool
/// (`"matcher": "*"`), each pointing at `factoryctl hook --token-file
/// <hook_token_path> <Event>`, plus a minimal `permissions.allow` so the
/// session's own `factoryctl` calls never block on a Bash approval prompt
/// (see [`CLAUDE_ALLOWED_BASH_PREFIXES`]).
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
    serde_json::json!({
        "hooks": Value::Object(hooks_object),
        "permissions": { "allow": CLAUDE_ALLOWED_BASH_PREFIXES },
    })
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

    /// A `ClaudeProvider` pointed at an isolated, empty tempdir "home" --
    /// never the real `$HOME` of the machine running the test suite (see
    /// `ClaudeProvider::with_home`'s doc comment). `pretrust_worktree`
    /// finds no `.claude.json` there and is a silent no-op, so these tests
    /// exercise `spawn_spec`'s ordinary launch-building behavior only.
    fn provider(directory: &std::path::Path) -> ClaudeProvider {
        ClaudeProvider::with_home(directory.join("home"))
    }

    #[test]
    fn fresh_launch_uses_the_session_id_as_the_claude_session_uuid() {
        let directory = tempfile::tempdir().unwrap();
        let ctx = context(directory.path());
        let launch = provider(directory.path()).spawn_spec(&ctx).unwrap();

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
        let launch = provider(directory.path()).spawn_spec(&ctx).unwrap();

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
            provider(directory.path()).spawn_spec(&ctx),
            Err(ProviderError::SessionIdNotUuid { .. })
        ));
    }

    #[test]
    fn resume_rejects_a_non_uuid_resume_target() {
        let directory = tempfile::tempdir().unwrap();
        let mut ctx = context(directory.path());
        ctx.resume = Some("not-a-uuid".to_owned());
        assert!(matches!(
            provider(directory.path()).spawn_spec(&ctx),
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
            },
            "permissions": { "allow": ["Bash(factoryctl *)"] }
        });
        assert_eq!(value, expected);
    }

    #[test]
    fn settings_json_pre_approves_only_factoryctl_bash_calls() {
        let value = claude_settings_json(
            std::path::Path::new("/abs/factoryctl"),
            std::path::Path::new("/abs/runs/session-1/hook.token"),
        );
        assert_eq!(
            value["permissions"]["allow"],
            serde_json::json!(["Bash(factoryctl *)"])
        );
    }

    #[test]
    fn spawn_spec_writes_settings_json_atomically_at_mode_0600() {
        let directory = tempfile::tempdir().unwrap();
        let ctx = context(directory.path());
        let launch = provider(directory.path()).spawn_spec(&ctx).unwrap();
        let settings_path = &launch.generated_files[0];
        let metadata = std::fs::metadata(settings_path).unwrap();
        assert_eq!(metadata.permissions().mode() & 0o777, 0o600);
        let parsed: Value =
            serde_json::from_str(&std::fs::read_to_string(settings_path).unwrap()).unwrap();
        assert!(parsed.get("hooks").is_some());
    }

    #[test]
    fn capabilities_declare_hooks_resume_and_the_supported_permission_modes() {
        let capabilities = ClaudeProvider::default().capabilities();
        assert!(capabilities.hooks);
        assert!(capabilities.resume);
        assert_eq!(capabilities.permission_modes, PERMISSION_MODES);
    }

    mod pretrust_worktree_tests {
        use super::*;

        fn write_fake_claude_json(home: &std::path::Path, contents: &str) {
            std::fs::write(home.join(".claude.json"), contents).unwrap();
        }

        fn read_claude_json(home: &std::path::Path) -> Value {
            serde_json::from_str(&std::fs::read_to_string(home.join(".claude.json")).unwrap())
                .unwrap()
        }

        #[test]
        fn marks_a_new_project_entry_trusted_and_preserves_every_other_key() {
            let home = tempfile::tempdir().unwrap();
            write_fake_claude_json(
                home.path(),
                r#"{
                    "numStartups": 12,
                    "userID": "sentinel-user-id",
                    "projects": {
                        "/Users/op/other-repo": {
                            "hasTrustDialogAccepted": true,
                            "lastSessionId": "sentinel-other-session"
                        }
                    }
                }"#,
            );
            let worktree = PathBuf::from("/Users/op/dark-factory-worktrees/worker-1");

            pretrust_worktree(&worktree, home.path());

            let document = read_claude_json(home.path());
            assert_eq!(document["numStartups"], 12);
            assert_eq!(document["userID"], "sentinel-user-id");
            assert_eq!(
                document["projects"]["/Users/op/other-repo"]["hasTrustDialogAccepted"],
                true
            );
            assert_eq!(
                document["projects"]["/Users/op/other-repo"]["lastSessionId"],
                "sentinel-other-session"
            );
            assert_eq!(
                document["projects"]["/Users/op/dark-factory-worktrees/worker-1"]["hasTrustDialogAccepted"],
                true
            );
        }

        #[test]
        fn preserves_other_fields_already_on_the_targeted_project_entry() {
            let home = tempfile::tempdir().unwrap();
            let worktree = PathBuf::from("/Users/op/dark-factory-worktrees/worker-1");
            write_fake_claude_json(
                home.path(),
                r#"{
                    "projects": {
                        "/Users/op/dark-factory-worktrees/worker-1": {
                            "hasTrustDialogAccepted": false,
                            "allowedTools": ["Bash(git *)"],
                            "lastSessionId": "sentinel-keep-me"
                        }
                    }
                }"#,
            );

            pretrust_worktree(&worktree, home.path());

            let document = read_claude_json(home.path());
            let project = &document["projects"]["/Users/op/dark-factory-worktrees/worker-1"];
            assert_eq!(project["hasTrustDialogAccepted"], true);
            assert_eq!(project["allowedTools"], serde_json::json!(["Bash(git *)"]));
            assert_eq!(project["lastSessionId"], "sentinel-keep-me");
        }

        #[test]
        fn creates_a_projects_object_when_the_file_has_none_yet() {
            let home = tempfile::tempdir().unwrap();
            write_fake_claude_json(home.path(), r#"{"numStartups": 1}"#);
            let worktree = PathBuf::from("/Users/op/dark-factory-worktrees/worker-1");

            pretrust_worktree(&worktree, home.path());

            let document = read_claude_json(home.path());
            assert_eq!(
                document["projects"]["/Users/op/dark-factory-worktrees/worker-1"]["hasTrustDialogAccepted"],
                true
            );
        }

        #[test]
        fn preserves_the_files_existing_mode() {
            let home = tempfile::tempdir().unwrap();
            let path = home.path().join(".claude.json");
            std::fs::write(&path, r#"{"projects": {}}"#).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();

            pretrust_worktree(&PathBuf::from("/Users/op/worktree"), home.path());

            let metadata = std::fs::metadata(&path).unwrap();
            assert_eq!(metadata.permissions().mode() & 0o777, 0o644);
        }

        #[test]
        fn missing_claude_json_is_skipped_without_creating_one() {
            let home = tempfile::tempdir().unwrap();

            pretrust_worktree(&PathBuf::from("/Users/op/worktree"), home.path());

            assert!(!home.path().join(".claude.json").exists());
        }

        #[test]
        fn invalid_json_is_skipped_and_left_untouched() {
            let home = tempfile::tempdir().unwrap();
            write_fake_claude_json(home.path(), "not valid json {{{");

            pretrust_worktree(&PathBuf::from("/Users/op/worktree"), home.path());

            assert_eq!(
                std::fs::read_to_string(home.path().join(".claude.json")).unwrap(),
                "not valid json {{{"
            );
        }

        #[test]
        fn a_json_array_at_the_top_level_is_skipped_and_left_untouched() {
            let home = tempfile::tempdir().unwrap();
            write_fake_claude_json(home.path(), "[1, 2, 3]");

            pretrust_worktree(&PathBuf::from("/Users/op/worktree"), home.path());

            assert_eq!(
                std::fs::read_to_string(home.path().join(".claude.json")).unwrap(),
                "[1, 2, 3]"
            );
        }

        #[test]
        fn a_non_object_projects_value_is_skipped_and_left_untouched() {
            let home = tempfile::tempdir().unwrap();
            write_fake_claude_json(home.path(), r#"{"projects": "not an object"}"#);

            pretrust_worktree(&PathBuf::from("/Users/op/worktree"), home.path());

            assert_eq!(
                std::fs::read_to_string(home.path().join(".claude.json")).unwrap(),
                r#"{"projects": "not an object"}"#
            );
        }
    }
}
