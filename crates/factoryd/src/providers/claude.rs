//! Non-interactive Claude Code provider.
//!
//! Each admitted run gets one fresh `claude -p` process and one startup
//! input. Output remains opaque; authority comes from the daemon's run state,
//! never from a provider stream decoder or a resumable process.

use std::{
    ffi::OsStr,
    fs,
    os::unix::fs::MetadataExt as _,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

use factory_core::{ExecutionMode, Provider as ProviderKind};
use serde_json::Value;

use crate::providers::{Provider, ProviderError, ProviderLaunch, SpawnContext, hooks};

fn validate_uuid(value: &str) -> Result<(), ()> {
    let parsed = uuid::Uuid::parse_str(value).map_err(|_| ())?;
    (parsed.hyphenated().to_string() == value)
        .then_some(())
        .ok_or(())
}

/// The exact reviewed Claude Code version. Claude auto-updates by default, so
/// accepting a range would let settings or sandbox semantics drift without a
/// Dark Factory review.
pub const SUPPORTED_CLAUDE_VERSION: &str = "2.1.236 (Claude Code)";

/// Launches one fresh `claude -p`; Claude requires the run ID in its
/// upstream `--session-id` argument.
#[derive(Clone, Debug)]
pub struct ClaudeProvider {
    installation: ClaudeInstallation,
    platform: &'static str,
}

impl ClaudeProvider {
    #[must_use]
    pub fn new(installation: ClaudeInstallation) -> Self {
        Self {
            installation,
            platform: std::env::consts::OS,
        }
    }

    #[cfg(test)]
    fn for_platform(installation: ClaudeInstallation, platform: &'static str) -> Self {
        Self {
            installation,
            platform,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ClaudeInstallation {
    program: PathBuf,
    identity: ExecutableIdentity,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ExecutableIdentity {
    device: u64,
    inode: u64,
    length: u64,
    modified_seconds: i64,
    modified_nanoseconds: i64,
}

impl Provider for ClaudeProvider {
    fn spawn_spec(&self, ctx: &SpawnContext) -> Result<ProviderLaunch, ProviderError> {
        let run_id = ctx.run_id.as_str();
        validate_uuid(run_id).map_err(|_| ProviderError::RunIdNotUuid {
            run_id: run_id.to_owned(),
        })?;
        ensure_mode_supported(ctx.execution_mode, self.platform)?;
        self.installation.verify_unchanged()?;

        let settings_path = ctx.agent_dir.join("claude-settings.json");
        let settings = claude_settings_json(
            &ctx.factoryctl_path,
            &ctx.hook_token_path,
            &ctx.socket_path,
            ctx.execution_mode,
        );
        let bytes = serde_json::to_vec_pretty(&settings)
            .expect("claude settings json is always representable");
        hooks::write_private_file(&settings_path, &bytes).map_err(|source| {
            ProviderError::Config {
                path: settings_path.clone(),
                source,
            }
        })?;
        let mut args = vec![
            "-p".to_owned(),
            "--settings".to_owned(),
            settings_path.to_string_lossy().into_owned(),
            // Do not merge user/project settings that could widen this
            // attempt's frozen mode. Explicit daemon settings and mandatory
            // managed policy still apply.
            "--setting-sources".to_owned(),
            String::new(),
            "--strict-mcp-config".to_owned(),
            "--session-id".to_owned(),
            run_id.to_owned(),
        ];
        if let Some(model) = &ctx.model {
            args.push("--model".to_owned());
            args.push(model.clone());
        }
        args.push("--permission-mode".to_owned());
        match ctx.execution_mode {
            ExecutionMode::PlanOnly => {
                // `dontAsk` auto-denies anything outside the exact read-only
                // tool set, so `claude -p` can never wait on an unanswered
                // native prompt.
                args.push("dontAsk".to_owned());
                args.push("--tools".to_owned());
                args.push("Read,Glob,Grep,Bash".to_owned());
            }
            ExecutionMode::WorkspaceWrite => args.push("dontAsk".to_owned()),
            ExecutionMode::Unrestricted => args.push("bypassPermissions".to_owned()),
        }

        Ok(ProviderLaunch {
            program: self.installation.program.clone(),
            args,
            env: Vec::new(),
            startup_input: ctx.startup_input.clone(),
        })
    }
}

fn claude_settings_json(
    factoryctl_path: &Path,
    hook_token_path: &Path,
    socket_path: &Path,
    execution_mode: ExecutionMode,
) -> Value {
    let event = factory_core::ProviderHookEvent::PreToolUse;
    let command = hooks::hook_command(factoryctl_path, hook_token_path, event);
    let mut settings = serde_json::json!({
        "hooks": {
            "PreToolUse": [{
                "matcher": "*",
                "hooks": [{ "type": "command", "command": command }]
            }]
        }
    });
    match execution_mode {
        ExecutionMode::PlanOnly => {
            settings["permissions"] = serde_json::json!({
                "allow": [
                    "Read",
                    "Glob",
                    "Grep",
                    "Bash(factoryctl task done:*)",
                    "Bash(factoryctl task blocked:*)"
                ],
                "deny": ["Edit"]
            });
        }
        ExecutionMode::WorkspaceWrite => {
            settings["permissions"] = serde_json::json!({
                "allow": [
                    "Read",
                    "Glob",
                    "Grep",
                    // The runner launches Claude with the exact Change as its
                    // cwd. This constant gitignore rule never interpolates
                    // attacker-shaped or non-UTF8 source-path bytes.
                    "Edit(./**)"
                ]
            });
            settings["sandbox"] = serde_json::json!({
                "enabled": true,
                "failIfUnavailable": true,
                "allowUnsandboxedCommands": false,
                "autoAllowBashIfSandboxed": true,
                "network": {
                    // Claude enforces this exact path only on macOS.
                    // WorkspaceWrite is rejected on every other platform.
                    "allowUnixSockets": [socket_path]
                }
            });
        }
        ExecutionMode::Unrestricted => {}
    }
    settings
}

fn ensure_mode_supported(
    execution_mode: ExecutionMode,
    platform: &str,
) -> Result<(), ProviderError> {
    if execution_mode != ExecutionMode::Unrestricted && platform != "macos" {
        return Err(ProviderError::UnsupportedPlatform {
            provider: ProviderKind::ClaudeCode,
            mode: execution_mode,
            platform: platform.to_owned(),
        });
    }
    Ok(())
}

/// Validates one installed Claude executable without sending a model prompt.
/// The version is exact because Claude auto-updates, and each supported
/// generated settings shape is passed to the metadata-only `doctor` command.
pub fn preflight_installation(
    program: &Path,
    platform: &str,
) -> Result<ClaudeInstallation, ProviderError> {
    let before = executable_identity(program)?;
    validate_claude_version(program)?;
    for execution_mode in [
        ExecutionMode::PlanOnly,
        ExecutionMode::WorkspaceWrite,
        ExecutionMode::Unrestricted,
    ] {
        if ensure_mode_supported(execution_mode, platform).is_err() {
            continue;
        }
        let settings = claude_settings_json(
            Path::new("/opt/dark-factory/factoryctl"),
            Path::new("/private/dark-factory/attempt.token"),
            Path::new("/private/dark-factory/f.sock"),
            execution_mode,
        );
        let inline =
            serde_json::to_string(&settings).expect("claude settings json is always representable");
        validate_settings(program, OsStr::new(&inline))?;
    }
    let after = executable_identity(program)?;
    if before != after {
        return Err(claude_preflight_error(
            program,
            "executable changed during startup validation".to_owned(),
        ));
    }
    Ok(ClaudeInstallation {
        program: program.to_path_buf(),
        identity: after,
    })
}

fn validate_claude_version(program: &Path) -> Result<(), ProviderError> {
    let output = Command::new(program)
        .arg("--version")
        .stdin(Stdio::null())
        .output()
        .map_err(|error| claude_preflight_error(program, error.to_string()))?;
    let version = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    if !output.status.success() || version != SUPPORTED_CLAUDE_VERSION {
        return Err(claude_preflight_error(
            program,
            format!("expected exact version {SUPPORTED_CLAUDE_VERSION:?}, found {version:?}"),
        ));
    }
    Ok(())
}

fn validate_settings(program: &Path, settings: &OsStr) -> Result<(), ProviderError> {
    let mut command = Command::new(program);
    command
        .args([OsStr::new("--settings"), settings])
        .arg("--setting-sources")
        .arg("")
        .arg("doctor")
        .env("NO_COLOR", "1")
        .env("TERM", "dumb")
        .env("LANG", "C")
        .env("LC_ALL", "C")
        .stdin(Stdio::null());
    let output = command
        .output()
        .map_err(|error| claude_preflight_error(program, error.to_string()))?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    if !output.status.success()
        || stdout.contains("Invalid settings")
        || stderr.contains("Invalid settings")
    {
        return Err(claude_preflight_error(
            program,
            "metadata-only doctor rejected the generated settings".to_owned(),
        ));
    }
    Ok(())
}

fn claude_preflight_error(program: &Path, reason: String) -> ProviderError {
    ProviderError::ClaudePreflight {
        program: program.to_path_buf(),
        reason,
    }
}

fn executable_identity(program: &Path) -> Result<ExecutableIdentity, ProviderError> {
    let metadata = fs::metadata(program)
        .map_err(|error| claude_preflight_error(program, error.to_string()))?;
    Ok(ExecutableIdentity {
        device: metadata.dev(),
        inode: metadata.ino(),
        length: metadata.len(),
        modified_seconds: metadata.mtime(),
        modified_nanoseconds: metadata.mtime_nsec(),
    })
}

impl ClaudeInstallation {
    fn verify_unchanged(&self) -> Result<(), ProviderError> {
        if executable_identity(&self.program)? != self.identity {
            return Err(claude_preflight_error(
                &self.program,
                "executable changed after daemon startup validation".to_owned(),
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::{
        ffi::OsString,
        fs,
        os::unix::{ffi::OsStringExt, fs::PermissionsExt},
    };

    use factory_core::RunId;

    use super::*;

    fn context(directory: &std::path::Path) -> SpawnContext {
        SpawnContext {
            run_id: RunId::try_from("2f5a1e2e-2222-4444-8888-0123456789ab").unwrap(),
            source_root: directory.join("source"),
            startup_input: b"fix the admitted task".to_vec(),
            model: None,
            reasoning_effort: None,
            execution_mode: ExecutionMode::WorkspaceWrite,
            hook_token_path: directory.join("runtime/hook.token"),
            factoryctl_path: PathBuf::from("/abs/factoryctl"),
            socket_path: PathBuf::from("/abs/factory.sock"),
            agent_dir: directory.join("agent-dir"),
        }
    }

    fn fake_claude(directory: &Path) -> PathBuf {
        fake_claude_with(directory, SUPPORTED_CLAUDE_VERSION, "Claude Code doctor")
    }

    fn fake_claude_with(directory: &Path, version: &str, doctor_output: &str) -> PathBuf {
        let program = directory.join("claude");
        fs::write(
            &program,
            format!(
                "#!/bin/sh\n\
                 if [ \"$1\" = \"--version\" ]; then\n\
                   printf '%s\\n' '{version}'\n\
                 else\n\
                   printf '%s\\n' \"$@\" > \"$0.args\"\n\
                   printf 'NO_COLOR=%s\\nTERM=%s\\nLANG=%s\\nLC_ALL=%s\\n' \
                     \"$NO_COLOR\" \"$TERM\" \"$LANG\" \"$LC_ALL\" >> \"$0.args\"\n\
                   printf '%s\\n' '{doctor_output}'\n\
                 fi\n"
            ),
        )
        .unwrap();
        fs::set_permissions(&program, fs::Permissions::from_mode(0o755)).unwrap();
        program
    }

    fn provider(directory: &Path) -> ClaudeProvider {
        let program = fake_claude(directory);
        let installation = preflight_installation(&program, "macos").unwrap();
        ClaudeProvider::for_platform(installation, "macos")
    }

    #[test]
    fn launch_is_fresh_noninteractive_and_carries_startup_input() {
        let directory = tempfile::tempdir().unwrap();
        let ctx = context(directory.path());
        let provider = provider(directory.path());
        let launch = provider.spawn_spec(&ctx).unwrap();

        assert_eq!(launch.program, directory.path().join("claude"));
        assert_eq!(launch.args.first().map(String::as_str), Some("-p"));
        assert!(
            launch
                .args
                .windows(2)
                .any(|pair| pair == ["--session-id", "2f5a1e2e-2222-4444-8888-0123456789ab"])
        );
        assert!(!launch.args.iter().any(|arg| arg == "--resume"));
        assert!(
            launch
                .args
                .windows(2)
                .any(|pair| { pair == ["--permission-mode".to_owned(), "dontAsk".to_owned()] })
        );
        assert!(
            launch
                .args
                .windows(2)
                .any(|pair| { pair == ["--setting-sources".to_owned(), String::new()] })
        );
        assert!(launch.args.iter().any(|arg| arg == "--strict-mcp-config"));
        assert_eq!(launch.startup_input, b"fix the admitted task");
    }

    #[test]
    fn plan_only_is_noninteractive_read_only_and_can_report_its_outcome() {
        let directory = tempfile::tempdir().unwrap();
        let mut ctx = context(directory.path());
        ctx.execution_mode = ExecutionMode::PlanOnly;
        let launch = provider(directory.path()).spawn_spec(&ctx).unwrap();

        assert!(
            launch
                .args
                .windows(2)
                .any(|pair| { pair == ["--permission-mode".to_owned(), "dontAsk".to_owned()] })
        );
        assert!(
            launch
                .args
                .windows(2)
                .any(|pair| { pair == ["--tools".to_owned(), "Read,Glob,Grep,Bash".to_owned()] })
        );
        let settings: Value = serde_json::from_slice(
            &std::fs::read(ctx.agent_dir.join("claude-settings.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(settings["permissions"]["deny"], serde_json::json!(["Edit"]));
        assert_eq!(
            settings["permissions"]["allow"][3],
            "Bash(factoryctl task done:*)"
        );
        assert_eq!(
            settings["permissions"]["allow"][4],
            "Bash(factoryctl task blocked:*)"
        );
        assert!(settings.get("sandbox").is_none());
    }

    #[test]
    fn workspace_write_is_strictly_sandboxed_and_cannot_retry_unsandboxed() {
        let directory = tempfile::tempdir().unwrap();
        let ctx = context(directory.path());
        provider(directory.path()).spawn_spec(&ctx).unwrap();
        let settings: Value = serde_json::from_slice(
            &std::fs::read(ctx.agent_dir.join("claude-settings.json")).unwrap(),
        )
        .unwrap();

        assert_eq!(settings["sandbox"]["enabled"], true);
        assert_eq!(settings["sandbox"]["failIfUnavailable"], true);
        assert_eq!(settings["sandbox"]["allowUnsandboxedCommands"], false);
        assert_eq!(settings["sandbox"]["autoAllowBashIfSandboxed"], true);
        assert_eq!(
            settings["sandbox"]["network"]["allowUnixSockets"],
            serde_json::json!(["/abs/factory.sock"])
        );
        assert_eq!(
            settings["permissions"]["allow"],
            serde_json::json!(["Read", "Glob", "Grep", "Edit(./**)"])
        );
    }

    #[test]
    fn workspace_settings_are_source_path_independent_and_never_lossy() {
        let directory = tempfile::tempdir().unwrap();
        let provider = provider(directory.path());
        let mut ctx = context(directory.path());
        ctx.source_root = directory.path().join("source-[*?]");
        provider.spawn_spec(&ctx).unwrap();
        let settings_path = ctx.agent_dir.join("claude-settings.json");
        let metachar_settings = fs::read(&settings_path).unwrap();

        ctx.source_root = directory
            .path()
            .join(OsString::from_vec(b"source-\xff-[*?]".to_vec()));
        provider.spawn_spec(&ctx).unwrap();
        let non_utf8_settings = fs::read(&settings_path).unwrap();

        assert_eq!(non_utf8_settings, metachar_settings);
        let settings: Value = serde_json::from_slice(&non_utf8_settings).unwrap();
        assert_eq!(
            settings["permissions"]["allow"],
            serde_json::json!(["Read", "Glob", "Grep", "Edit(./**)"])
        );
    }

    #[test]
    fn unrestricted_uses_the_explicit_bypass_without_a_sandbox_claim() {
        let directory = tempfile::tempdir().unwrap();
        let mut ctx = context(directory.path());
        ctx.execution_mode = ExecutionMode::Unrestricted;
        let launch = provider(directory.path()).spawn_spec(&ctx).unwrap();

        assert!(launch.args.windows(2).any(|pair| {
            pair == [
                "--permission-mode".to_owned(),
                "bypassPermissions".to_owned(),
            ]
        }));
        assert!(!launch.args.iter().any(|arg| arg == "--tools"));
        let settings: Value = serde_json::from_slice(
            &std::fs::read(ctx.agent_dir.join("claude-settings.json")).unwrap(),
        )
        .unwrap();
        assert!(settings.get("permissions").is_none());
        assert!(settings.get("sandbox").is_none());
    }

    #[test]
    fn non_uuid_run_id_is_refused() {
        let directory = tempfile::tempdir().unwrap();
        let mut ctx = context(directory.path());
        ctx.run_id = RunId::try_from("not-a-uuid").unwrap();
        assert!(matches!(
            provider(directory.path()).spawn_spec(&ctx),
            Err(ProviderError::RunIdNotUuid { .. })
        ));
    }

    #[test]
    fn spawn_writes_private_settings_with_one_daemon_authority_hook() {
        let directory = tempfile::tempdir().unwrap();
        let ctx = context(directory.path());
        provider(directory.path()).spawn_spec(&ctx).unwrap();
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

    #[test]
    fn linux_refuses_workspace_write_before_invoking_claude() {
        let directory = tempfile::tempdir().unwrap();
        let program = fake_claude(directory.path());
        let installation = preflight_installation(&program, "linux").unwrap();
        let args_path = PathBuf::from(format!("{}.args", program.display()));
        fs::remove_file(&args_path).unwrap();
        let provider = ClaudeProvider::for_platform(installation, "linux");
        let ctx = context(directory.path());

        assert!(matches!(
            provider.spawn_spec(&ctx),
            Err(ProviderError::UnsupportedPlatform {
                provider: ProviderKind::ClaudeCode,
                mode: ExecutionMode::WorkspaceWrite,
                ref platform,
            }) if platform == "linux"
        ));
        assert!(!args_path.exists());
    }

    #[test]
    fn linux_refuses_plan_only_before_invoking_claude() {
        let directory = tempfile::tempdir().unwrap();
        let program = fake_claude(directory.path());
        let installation = preflight_installation(&program, "linux").unwrap();
        let args_path = PathBuf::from(format!("{}.args", program.display()));
        fs::remove_file(&args_path).unwrap();
        let mut ctx = context(directory.path());
        ctx.execution_mode = ExecutionMode::PlanOnly;

        assert!(matches!(
            ClaudeProvider::for_platform(installation, "linux").spawn_spec(&ctx),
            Err(ProviderError::UnsupportedPlatform {
                provider: ProviderKind::ClaudeCode,
                mode: ExecutionMode::PlanOnly,
                ref platform,
            }) if platform == "linux"
        ));
        assert!(!args_path.exists());
    }

    #[test]
    fn linux_permits_only_explicit_unrestricted_claude() {
        let directory = tempfile::tempdir().unwrap();
        let program = fake_claude(directory.path());
        let installation = preflight_installation(&program, "linux").unwrap();
        let mut ctx = context(directory.path());
        ctx.execution_mode = ExecutionMode::Unrestricted;
        ClaudeProvider::for_platform(installation, "linux")
            .spawn_spec(&ctx)
            .unwrap();
    }

    #[test]
    fn generated_settings_are_checked_by_metadata_only_doctor() {
        let directory = tempfile::tempdir().unwrap();
        let program = fake_claude(directory.path());
        preflight_installation(&program, "macos").unwrap();

        let args = fs::read_to_string(format!("{}.args", program.display())).unwrap();
        assert!(args.lines().any(|arg| arg == "--settings"));
        assert!(args.lines().any(|arg| arg == "--setting-sources"));
        assert!(args.lines().any(|arg| arg == "doctor"));
        assert!(!args.lines().any(|arg| arg == "-p"));
        for expected in ["NO_COLOR=1", "TERM=dumb", "LANG=C", "LC_ALL=C"] {
            assert!(args.lines().any(|arg| arg == expected));
        }
    }

    #[test]
    fn zero_status_doctor_invalid_settings_marker_fails_closed() {
        let directory = tempfile::tempdir().unwrap();
        let program = fake_claude_with(
            directory.path(),
            SUPPORTED_CLAUDE_VERSION,
            "Invalid settings",
        );
        assert!(matches!(
            preflight_installation(&program, "macos"),
            Err(ProviderError::ClaudePreflight { ref reason, .. })
                if reason.contains("rejected the generated settings")
        ));
    }

    #[test]
    fn unreviewed_claude_version_fails_closed() {
        let directory = tempfile::tempdir().unwrap();
        let program = fake_claude_with(directory.path(), "2.1.237 (Claude Code)", "doctor");

        assert!(matches!(
            preflight_installation(&program, "macos"),
            Err(ProviderError::ClaudePreflight { ref reason, .. })
                if reason.contains(SUPPORTED_CLAUDE_VERSION)
        ));
    }

    #[test]
    fn executable_replacement_after_startup_is_refused_without_running_it() {
        let directory = tempfile::tempdir().unwrap();
        let program = fake_claude(directory.path());
        let installation = preflight_installation(&program, "macos").unwrap();
        let replacement = directory.path().join("replacement");
        fs::write(&replacement, "#!/bin/sh\ntouch \"$0.ran\"\n").unwrap();
        fs::set_permissions(&replacement, fs::Permissions::from_mode(0o755)).unwrap();
        fs::rename(replacement, &program).unwrap();
        let ctx = context(directory.path());

        assert!(matches!(
            ClaudeProvider::for_platform(installation, "macos").spawn_spec(&ctx),
            Err(ProviderError::ClaudePreflight { ref reason, .. })
                if reason.contains("changed after daemon startup")
        ));
        assert!(!PathBuf::from(format!("{}.ran", program.display())).exists());
    }
}
