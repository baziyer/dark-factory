//! [`CodexProvider`]: the interactive-session [`Provider`] impl for Codex,
//! plus the per-agent `CODEX_HOME` it seeds and the hooks block it rewrites
//! into `config.toml` per session. See `docs/providers.md`.
//!
//! Track 5's pivot from a non-interactive `codex exec --json` pipe-mode
//! adapter (session identity confirmed by decoding Codex's own JSONL event
//! stream) to a resident interactive `codex` process under a PTY (session
//! identity learned from the `SessionStart` hook payload, state driven by
//! hooks thereafter) deleted that whole decoder here: `Decoder`,
//! `Observation`, item-tracking state, `Outcome`, `FailureReason`, the
//! non-interactive `prepare()`, and their fixtures/tests
//! (`crates/factoryd/tests/codex.rs`) are gone (~540 LOC, see
//! `TRACK5-DESIGN.md` §7). `validate_thread_id` is the one piece that
//! survived unchanged: both the old decoder and the new [`CodexProvider`]
//! need to confirm a Codex thread identity is a canonical UUID.

use std::{
    fs,
    path::{Path, PathBuf},
};

use uuid::Uuid;

use crate::providers::{
    Capabilities, InteractiveLaunch, Provider, ProviderError, SpawnContext, hooks,
};

fn validate_thread_id(value: &str) -> Result<(), ()> {
    let parsed = Uuid::parse_str(value).map_err(|_| ())?;
    if parsed.hyphenated().to_string() == value {
        Ok(())
    } else {
        Err(())
    }
}

pub const PERMISSION_MODES: [&str; 2] = ["on-request", "never"];

const HOOKS_BEGIN_MARKER: &str = "# --- dark-factory hooks BEGIN ---";
const HOOKS_END_MARKER: &str = "# --- dark-factory hooks END ---";
const MINIMAL_CONFIG_TOML: &str =
    "# Dark Factory generated Codex home (no ~/.codex/config.toml was found to copy).\n";

/// Interactive-session [`Provider`] for Codex. Launches `codex
/// --dangerously-bypass-hook-trust [--model M] [-c
/// approval_policy="<permission_mode>"] [resume <thread-id>]` with
/// `CODEX_HOME` pointed at this agent's own seeded home (per orchestrator
/// amendment A2, `TRACK5-DESIGN.md`: per *agent*, not per session, so
/// `codex resume` can find its own prior rollout file across a stop and
/// restart). `--dangerously-bypass-hook-trust` is unconditional: the hooks
/// this provider writes are 100% daemon-authored into an isolated
/// `CODEX_HOME` the operator never hand-edits, which already is the
/// vetting Codex's normal hook-trust prompt would otherwise ask for. See
/// `docs/providers.md`.
pub struct CodexProvider {
    /// The operator's real Codex home to seed a fresh per-agent
    /// `CODEX_HOME` from (`config.toml`, `auth.json`). Defaults to
    /// `$HOME/.codex`; overridable for tests via
    /// [`CodexProvider::with_source_home`].
    source_home: Option<PathBuf>,
}

impl CodexProvider {
    /// Resolves the seed source from `$HOME/.codex`, matching Codex's own
    /// default `CODEX_HOME`. `None` (no `$HOME`) means a fresh per-agent
    /// home always starts from [`MINIMAL_CONFIG_TOML`] with no `auth.json`
    /// link — Codex will then have no subscription credentials, same as
    /// running `codex` with no prior login.
    #[must_use]
    pub fn new() -> Self {
        Self {
            source_home: std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".codex")),
        }
    }

    #[cfg(test)]
    fn with_source_home(source_home: PathBuf) -> Self {
        Self {
            source_home: Some(source_home),
        }
    }
}

impl Default for CodexProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl Provider for CodexProvider {
    fn spawn_spec(&self, ctx: &SpawnContext) -> Result<InteractiveLaunch, ProviderError> {
        let codex_home = ctx.agent_dir.join("codex-home");
        seed_codex_home_once(&codex_home, self.source_home.as_deref())?;
        rewrite_hooks_block(&codex_home, &ctx.factoryctl_path, &ctx.hook_token_path)?;

        let mut args = vec!["--dangerously-bypass-hook-trust".to_owned()];
        if let Some(model) = &ctx.model {
            args.push("--model".to_owned());
            args.push(model.clone());
        }
        if let Some(permission_mode) = &ctx.permission_mode {
            args.push("-c".to_owned());
            args.push(format!("approval_policy=\"{permission_mode}\""));
        }
        if let Some(thread_id) = &ctx.resume {
            validate_thread_id(thread_id).map_err(|_| ProviderError::ResumeIdNotUuid {
                value: thread_id.clone(),
            })?;
            args.push("resume".to_owned());
            args.push(thread_id.clone());
        }

        Ok(InteractiveLaunch {
            program: PathBuf::from("codex"),
            args,
            env: vec![(
                "CODEX_HOME".to_owned(),
                codex_home.to_string_lossy().into_owned(),
            )],
            generated_files: vec![codex_home.join("config.toml")],
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

/// Idempotently seeds `codex_home` (mode `0700`, created if missing) the
/// first time it is used: copies `source_home/config.toml` if present, else
/// writes [`MINIMAL_CONFIG_TOML`]; symlinks `source_home/auth.json` if
/// present and not already linked. Existing files are never overwritten —
/// this is a one-time seed, not a sync. The hooks block is refreshed
/// separately, every spawn, by [`rewrite_hooks_block`].
fn seed_codex_home_once(
    codex_home: &Path,
    source_home: Option<&Path>,
) -> Result<(), ProviderError> {
    hooks::ensure_private_dir(codex_home).map_err(|source| ProviderError::Seed {
        path: codex_home.to_path_buf(),
        source,
    })?;

    let config_path = codex_home.join("config.toml");
    if !config_path.exists() {
        let contents = source_home
            .map(|home| home.join("config.toml"))
            .and_then(|path| fs::read(path).ok())
            .unwrap_or_else(|| MINIMAL_CONFIG_TOML.as_bytes().to_vec());
        hooks::write_private_file(&config_path, &contents).map_err(|source| {
            ProviderError::Seed {
                path: config_path.clone(),
                source,
            }
        })?;
    }

    if let Some(source_home) = source_home {
        let auth_path = codex_home.join("auth.json");
        let source_auth = source_home.join("auth.json");
        if fs::symlink_metadata(&auth_path).is_err() && source_auth.exists() {
            std::os::unix::fs::symlink(&source_auth, &auth_path).map_err(|source| {
                ProviderError::Seed {
                    path: auth_path.clone(),
                    source,
                }
            })?;
        }
    }
    Ok(())
}

/// Idempotently rewrites the daemon-owned hooks block in `codex_home`'s
/// `config.toml`, replacing everything between the `BEGIN`/`END` markers if
/// present, else appending a fresh block. Called on every spawn: the hook
/// token path changes per session, so this keeps the seeded config current
/// without disturbing whatever the operator's real `config.toml` carried
/// forward from the one-time seed (model, provider, trust settings, ...).
fn rewrite_hooks_block(
    codex_home: &Path,
    factoryctl_path: &Path,
    hook_token_path: &Path,
) -> Result<(), ProviderError> {
    let config_path = codex_home.join("config.toml");
    let existing = fs::read_to_string(&config_path).map_err(|source| ProviderError::Seed {
        path: config_path.clone(),
        source,
    })?;
    let mut rewritten = strip_hooks_block(&existing);
    if !rewritten.is_empty() {
        rewritten.push_str("\n\n");
    }
    rewritten.push_str(&hooks_block_toml(factoryctl_path, hook_token_path));
    hooks::write_private_file(&config_path, rewritten.as_bytes()).map_err(|source| {
        ProviderError::Seed {
            path: config_path.clone(),
            source,
        }
    })
}

/// Removes a previously written daemon hooks block (markers inclusive), if
/// present, leaving the rest of the file untouched (trailing whitespace
/// trimmed). Not a general TOML parser: it operates on the exact marker
/// lines [`hooks_block_toml`] writes.
fn strip_hooks_block(config: &str) -> String {
    let Some(begin) = config.find(HOOKS_BEGIN_MARKER) else {
        return config.trim_end().to_owned();
    };
    let before = &config[..begin];
    let after_marker = &config[begin..];
    let after = after_marker
        .find(HOOKS_END_MARKER)
        .map_or("", |end_offset| {
            &after_marker[end_offset + HOOKS_END_MARKER.len()..]
        });
    format!("{}{}", before.trim_end(), after)
        .trim_end()
        .to_owned()
}

fn hooks_block_toml(factoryctl_path: &Path, hook_token_path: &Path) -> String {
    let mut block = String::new();
    block.push_str(HOOKS_BEGIN_MARKER);
    block.push('\n');
    for event in hooks::HOOK_EVENTS {
        let name = event.provider_event_name();
        let command = hooks::hook_command(factoryctl_path, hook_token_path, event);
        block.push_str(&format!("[[hooks.{name}]]\n"));
        block.push_str(&format!("[[hooks.{name}.hooks]]\n"));
        block.push_str("type = \"command\"\n");
        block.push_str(&format!("command = \"{}\"\n", toml_escape(&command)));
        block.push_str("timeout = 30\n\n");
    }
    block.push_str(HOOKS_END_MARKER);
    block.push('\n');
    block
}

fn toml_escape(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

#[cfg(test)]
mod provider_tests {
    use std::os::unix::fs::PermissionsExt;

    use factory_core::{AgentId, ProjectId, SessionId};
    use serde_json::Value;

    use super::*;

    fn context(directory: &Path) -> SpawnContext {
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
    fn fresh_launch_has_no_resume_argument_and_sets_codex_home() {
        let directory = tempfile::tempdir().unwrap();
        let ctx = context(directory.path());
        let launch = CodexProvider::with_source_home(directory.path().join("no-real-home"))
            .spawn_spec(&ctx)
            .unwrap();

        assert_eq!(launch.program, PathBuf::from("codex"));
        assert_eq!(
            launch.args,
            vec!["--dangerously-bypass-hook-trust".to_owned()]
        );
        let codex_home = directory.path().join("agent-dir").join("codex-home");
        assert_eq!(
            launch.env,
            vec![(
                "CODEX_HOME".to_owned(),
                codex_home.to_string_lossy().into_owned()
            )]
        );
    }

    #[test]
    fn resume_launch_passes_model_approval_policy_and_thread_id_in_order() {
        let directory = tempfile::tempdir().unwrap();
        let mut ctx = context(directory.path());
        ctx.model = Some("gpt-5-codex".to_owned());
        ctx.permission_mode = Some("never".to_owned());
        ctx.resume = Some("9b1deb4d-3b7d-4bad-9bdd-2b0d7b3dcb6d".to_owned());
        let launch = CodexProvider::with_source_home(directory.path().join("no-real-home"))
            .spawn_spec(&ctx)
            .unwrap();

        assert_eq!(
            launch.args,
            vec![
                "--dangerously-bypass-hook-trust".to_owned(),
                "--model".to_owned(),
                "gpt-5-codex".to_owned(),
                "-c".to_owned(),
                "approval_policy=\"never\"".to_owned(),
                "resume".to_owned(),
                "9b1deb4d-3b7d-4bad-9bdd-2b0d7b3dcb6d".to_owned(),
            ]
        );
    }

    #[test]
    fn resume_rejects_a_non_uuid_thread_id() {
        let directory = tempfile::tempdir().unwrap();
        let mut ctx = context(directory.path());
        ctx.resume = Some("not-a-uuid".to_owned());
        let result =
            CodexProvider::with_source_home(directory.path().join("no-real-home")).spawn_spec(&ctx);
        assert!(matches!(result, Err(ProviderError::ResumeIdNotUuid { .. })));
    }

    #[test]
    fn seeds_a_minimal_config_when_no_real_codex_home_exists() {
        let directory = tempfile::tempdir().unwrap();
        let ctx = context(directory.path());
        CodexProvider::with_source_home(directory.path().join("missing"))
            .spawn_spec(&ctx)
            .unwrap();

        let config_path = directory
            .path()
            .join("agent-dir")
            .join("codex-home")
            .join("config.toml");
        let contents = fs::read_to_string(&config_path).unwrap();
        assert!(contents.starts_with(MINIMAL_CONFIG_TOML.trim_end()));
        assert!(contents.contains(HOOKS_BEGIN_MARKER));
        assert!(contents.contains(HOOKS_END_MARKER));
        assert!(
            !directory
                .path()
                .join("agent-dir")
                .join("codex-home")
                .join("auth.json")
                .exists()
        );

        let metadata = fs::metadata(config_path.parent().unwrap()).unwrap();
        assert_eq!(metadata.permissions().mode() & 0o777, 0o700);
    }

    #[test]
    fn copies_the_real_config_and_symlinks_auth_json_on_first_seed_only() {
        let directory = tempfile::tempdir().unwrap();
        let real_home = directory.path().join("real-codex-home");
        fs::create_dir_all(&real_home).unwrap();
        fs::write(real_home.join("config.toml"), "model = \"gpt-5.6\"\n").unwrap();
        fs::write(real_home.join("auth.json"), "{\"token\":\"secret\"}").unwrap();

        let ctx = context(directory.path());
        let provider = CodexProvider::with_source_home(real_home.clone());
        provider.spawn_spec(&ctx).unwrap();

        let codex_home = directory.path().join("agent-dir").join("codex-home");
        let config_contents = fs::read_to_string(codex_home.join("config.toml")).unwrap();
        assert!(config_contents.starts_with("model = \"gpt-5.6\""));
        assert!(config_contents.contains(HOOKS_BEGIN_MARKER));
        let auth_link = fs::read_link(codex_home.join("auth.json")).unwrap();
        assert_eq!(auth_link, real_home.join("auth.json"));

        // A real user edit to the seeded config.toml after the first spawn
        // is preserved by later spawns: only the hooks block is refreshed.
        let seeded = fs::read_to_string(codex_home.join("config.toml")).unwrap();
        let base = strip_hooks_block(&seeded);
        fs::write(
            codex_home.join("config.toml"),
            format!("{base}\nmodel_reasoning_effort = \"xhigh\"\n"),
        )
        .unwrap();
        provider.spawn_spec(&ctx).unwrap();
        let after_second_spawn = fs::read_to_string(codex_home.join("config.toml")).unwrap();
        assert!(after_second_spawn.contains("model_reasoning_effort = \"xhigh\""));
        assert_eq!(after_second_spawn.matches(HOOKS_BEGIN_MARKER).count(), 1);
    }

    #[test]
    fn hooks_block_is_rewritten_idempotently_across_spawns() {
        let directory = tempfile::tempdir().unwrap();
        let ctx = context(directory.path());
        let provider = CodexProvider::with_source_home(directory.path().join("missing"));
        provider.spawn_spec(&ctx).unwrap();
        provider.spawn_spec(&ctx).unwrap();
        provider.spawn_spec(&ctx).unwrap();

        let config_path = directory
            .path()
            .join("agent-dir")
            .join("codex-home")
            .join("config.toml");
        let contents = fs::read_to_string(&config_path).unwrap();
        assert_eq!(contents.matches(HOOKS_BEGIN_MARKER).count(), 1);
        assert_eq!(contents.matches(HOOKS_END_MARKER).count(), 1);
        assert_eq!(contents.matches("[[hooks.Stop]]").count(), 1);
    }

    #[test]
    fn hooks_block_toml_has_the_exact_designed_shape_for_one_event() {
        let block = hooks_block_toml(
            Path::new("/abs/factoryctl"),
            Path::new("/abs/runs/session-1/hook.token"),
        );
        assert!(block.starts_with(HOOKS_BEGIN_MARKER));
        assert!(block.trim_end().ends_with(HOOKS_END_MARKER));
        assert!(block.contains(
            "[[hooks.Stop]]\n[[hooks.Stop.hooks]]\ntype = \"command\"\ncommand = \"'/abs/factoryctl' hook --token-file '/abs/runs/session-1/hook.token' Stop\"\ntimeout = 30\n"
        ));
    }

    #[test]
    fn capabilities_declare_hooks_resume_and_the_supported_permission_modes() {
        let capabilities = CodexProvider::new().capabilities();
        assert!(capabilities.hooks);
        assert!(capabilities.resume);
        assert_eq!(capabilities.permission_modes, PERMISSION_MODES);
    }

    #[test]
    fn config_toml_generated_by_a_fresh_seed_parses_under_codex_doctor() {
        // Guards against a schema regression without spawning a real
        // interactive session: `codex doctor` is a read-only diagnostic
        // that parses `CODEX_HOME/config.toml` and reports whether it
        // loaded, matching the manual verification recorded in this
        // track's report (`config.toml parse: ok` under `--strict-config`
        // for exactly this generated shape).
        if std::process::Command::new("codex")
            .arg("--version")
            .output()
            .is_err()
        {
            eprintln!("skipping: codex is not installed in this environment");
            return;
        }
        let directory = tempfile::tempdir().unwrap();
        let ctx = context(directory.path());
        CodexProvider::with_source_home(directory.path().join("missing"))
            .spawn_spec(&ctx)
            .unwrap();
        let codex_home = directory.path().join("agent-dir").join("codex-home");

        let output = std::process::Command::new("codex")
            .env("CODEX_HOME", &codex_home)
            .args(["--strict-config", "doctor", "--json"])
            .output()
            .unwrap();
        let report: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(
            report["checks"]["config.load"]["details"]["config.toml parse"],
            "ok"
        );
    }
}
