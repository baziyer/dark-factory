//! Non-interactive Codex provider.
//!
//! Each admitted run gets one fresh `codex exec` process and one startup
//! input. Output remains opaque; authority comes from the daemon's run state,
//! never from a provider stream decoder or a resumable process.

use std::{
    ffi::OsString,
    fs,
    path::{Path, PathBuf},
};

use factory_core::ExecutionMode;

use crate::providers::{Provider, ProviderError, ProviderLaunch, SpawnContext, hooks};

const HOOKS_BEGIN_MARKER: &str = "# --- dark-factory hooks BEGIN ---";
const HOOKS_END_MARKER: &str = "# --- dark-factory hooks END ---";
/// Trust settings, in a marker block separate from
/// [`HOOKS_BEGIN_MARKER`]/[`HOOKS_END_MARKER`]. Provider authority is passed
/// as explicit `codex exec` arguments and never inherited from this file.
const CONFIG_BEGIN_MARKER: &str = "# --- dark-factory config BEGIN ---";
const CONFIG_END_MARKER: &str = "# --- dark-factory config END ---";
const MINIMAL_CONFIG_TOML: &str =
    "# Dark Factory generated Codex home; ambient provider config is not inherited.\n";

/// Non-interactive [`Provider`] for Codex. Launches `codex exec
/// --strict-config --dangerously-bypass-hook-trust [--model M]` with an explicit typed
/// permission profile and `approval_policy="never"`, or the unrestricted
/// bypass, reading exactly one task from stdin with `CODEX_HOME` pointed at
/// this attempt's generated home. `--dangerously-bypass-hook-trust` is
/// unconditional: the hooks
/// this provider writes are 100% daemon-authored into an isolated
/// `CODEX_HOME` the operator never hand-edits, which already is the
/// vetting Codex's normal hook-trust prompt would otherwise ask for. See
/// `docs/providers.md`.
pub struct CodexProvider {
    /// The source used to link authentication into a fresh attempt-owned
    /// `CODEX_HOME`: the daemon's own `$CODEX_HOME` if set
    /// — Codex's own convention, and how a factory runs on a different
    /// account than the operator's shell (`CODEX_HOME=~/.codex-dogfood`
    /// in the launchd job) — else `$HOME/.codex`; overridable for tests via
    /// [`CodexProvider::with_source_home`].
    source_home: Option<PathBuf>,
}

impl CodexProvider {
    /// Resolves the seed source exactly as `codex` itself resolves its home:
    /// `$CODEX_HOME` if set, else `$HOME/.codex`. `None` (neither set)
    /// means a fresh attempt home has no `auth.json` link — Codex will then
    /// have no subscription credentials, same as running `codex` with no
    /// prior login.
    #[must_use]
    pub fn new() -> Self {
        Self::from_environment(std::env::var_os("CODEX_HOME"), std::env::var_os("HOME"))
    }

    fn from_environment(codex_home: Option<OsString>, home: Option<OsString>) -> Self {
        Self {
            source_home: codex_home
                .filter(|value| !value.is_empty())
                .map(PathBuf::from)
                .or_else(|| home.map(|home| PathBuf::from(home).join(".codex"))),
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
    fn spawn_spec(&self, ctx: &SpawnContext) -> Result<ProviderLaunch, ProviderError> {
        let codex_home = ctx.agent_dir.join("codex-home");
        seed_codex_home_once(&codex_home, self.source_home.as_deref())?;
        rewrite_hooks_block(&codex_home, &ctx.factoryctl_path, &ctx.hook_token_path)?;
        rewrite_config_block(&codex_home, &ctx.source_root)?;

        let mut args = vec![
            "exec".to_owned(),
            "--strict-config".to_owned(),
            "--dangerously-bypass-hook-trust".to_owned(),
        ];
        if let Some(model) = &ctx.model {
            args.push("--model".to_owned());
            args.push(model.clone());
        }
        if let Some(reasoning_effort) = &ctx.reasoning_effort {
            args.push("-c".to_owned());
            args.push(format!("model_reasoning_effort=\"{reasoning_effort}\""));
        }
        match ctx.execution_mode {
            ExecutionMode::PlanOnly => {
                push_bounded_execution_mode(
                    &mut args,
                    "dark_factory_plan_only",
                    ":read-only",
                    &ctx.socket_path,
                );
            }
            ExecutionMode::WorkspaceWrite => {
                push_bounded_execution_mode(
                    &mut args,
                    "dark_factory_workspace_write",
                    ":workspace",
                    &ctx.socket_path,
                );
            }
            ExecutionMode::Unrestricted => {
                args.push("--dangerously-bypass-approvals-and-sandbox".to_owned());
            }
        }
        args.push("-".to_owned());

        Ok(ProviderLaunch {
            program: PathBuf::from("codex"),
            args,
            env: vec![(
                "CODEX_HOME".to_owned(),
                codex_home.to_string_lossy().into_owned(),
            )],
            startup_input: ctx.startup_input.clone(),
        })
    }
}

/// Adds one named Codex permission profile rather than combining the legacy
/// `--sandbox` switch with the newer profile system. The profile exposes only
/// the exact daemon socket, so the sandboxed provider can make its
/// authenticated completion/block/message calls without opening public
/// network access. `approval_policy="never"` makes an unsupported operation
/// fail instead of waiting for an operator who cannot attach to this process.
fn push_bounded_execution_mode(
    args: &mut Vec<String>,
    profile: &str,
    parent: &str,
    socket_path: &Path,
) {
    let socket = toml_string(&socket_path.to_string_lossy());
    let filesystem = if parent == ":workspace" {
        // The built-in workspace profile also writes to system temp roots.
        // Attempts own only their admitted source, so explicitly remove both
        // aliases documented by Codex from the inherited profile.
        "filesystem = { \":tmpdir\" = \"deny\", \":slash_tmp\" = \"deny\" }, "
    } else {
        ""
    };
    let permission_profile = format!(
        "{{ extends = {}, {filesystem}network = {{ enabled = true, mode = \"limited\", \
         unix_sockets = {{ {socket} = \"allow\" }}, allow_upstream_proxy = false, \
         enable_socks5 = false, enable_socks5_udp = false }} }}",
        toml_string(parent),
    );
    args.extend([
        "--enable".to_owned(),
        "network_proxy".to_owned(),
        "-c".to_owned(),
        "approval_policy=\"never\"".to_owned(),
        "-c".to_owned(),
        format!("default_permissions={}", toml_string(profile)),
        "-c".to_owned(),
        format!("permissions.{profile}={permission_profile}"),
    ]);
}

/// Idempotently seeds `codex_home` (mode `0700`, created if missing) the
/// first time it is used: writes [`MINIMAL_CONFIG_TOML`] and symlinks
/// `source_home/auth.json` if present,
/// re-pointing a link the daemon made when the seed home changed. Ambient
/// rules are deliberately not copied because an operator `allow` rule can
/// widen the attempt's typed execution boundary. Existing config is a
/// one-time seed, while the auth link follows the source home. The hooks and
/// trust blocks are refreshed separately on every spawn.
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
        hooks::write_private_file(&config_path, MINIMAL_CONFIG_TOML.as_bytes()).map_err(
            |source| ProviderError::Seed {
                path: config_path.clone(),
                source,
            },
        )?;
    }

    if let Some(source_home) = source_home {
        let auth_path = codex_home.join("auth.json");
        let source_auth = source_home.join("auth.json");
        // The credentials link follows the daemon's seed home: create it if
        // missing, re-point it if it is a link to somewhere else (the seed
        // home changed -- a different Codex account). A regular file, which
        // only an operator could have put there, is never touched.
        let existing_link = fs::read_link(&auth_path).ok();
        let is_regular_file =
            fs::symlink_metadata(&auth_path).is_ok_and(|m| !m.file_type().is_symlink());
        if source_auth.exists()
            && !is_regular_file
            && existing_link.as_deref() != Some(source_auth.as_path())
        {
            if existing_link.is_some() {
                fs::remove_file(&auth_path).map_err(|source| ProviderError::Seed {
                    path: auth_path.clone(),
                    source,
                })?;
            }
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

/// Replaces the generated hook block with this attempt's authenticated
/// `PreToolUse` policy hook. Operator hooks were removed while seeding.
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

/// Rewrites the generated trust block for the exact attempt source root.
fn rewrite_config_block(codex_home: &Path, source_root: &Path) -> Result<(), ProviderError> {
    let source_root = canonicalize_or_given(source_root);

    let config_path = codex_home.join("config.toml");
    let existing = fs::read_to_string(&config_path).map_err(|source| ProviderError::Seed {
        path: config_path.clone(),
        source,
    })?;
    let mut rewritten = strip_marked_block(&existing, CONFIG_BEGIN_MARKER, CONFIG_END_MARKER);
    rewritten.push_str("\n\n");
    rewritten.push_str(&config_block_toml(&source_root));
    hooks::write_private_file(&config_path, rewritten.as_bytes()).map_err(|source| {
        ProviderError::Seed {
            path: config_path.clone(),
            source,
        }
    })
}

fn canonicalize_or_given(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap_or_else(|_| path.to_owned())
}

fn config_block_toml(source_root: &Path) -> String {
    let mut block = String::new();
    block.push_str(CONFIG_BEGIN_MARKER);
    block.push('\n');
    block.push_str(&format!(
        "[projects.{}]\n",
        toml_string(&source_root.to_string_lossy())
    ));
    block.push_str("trust_level = \"trusted\"\n");
    block.push_str(CONFIG_END_MARKER);
    block.push('\n');
    block
}

fn toml_string(value: &str) -> String {
    format!("\"{}\"", toml_escape(value))
}

/// Removes a previously written daemon hooks block (markers inclusive), if
/// present, leaving the rest of the file untouched (trailing whitespace
/// trimmed). Not a general TOML parser: it operates on the exact marker
/// lines [`hooks_block_toml`] writes.
fn strip_hooks_block(config: &str) -> String {
    strip_marked_block(config, HOOKS_BEGIN_MARKER, HOOKS_END_MARKER)
}

/// Removes a previously written `begin_marker`/`end_marker`-delimited block
/// (markers inclusive), if present, leaving the rest of the document
/// untouched (trailing whitespace trimmed). Shared by [`strip_hooks_block`]
/// and [`rewrite_config_block`]'s own `CONFIG_BEGIN_MARKER`/
/// `CONFIG_END_MARKER` block. Not a general TOML parser: it operates purely
/// on the exact marker lines this module writes.
fn strip_marked_block(document: &str, begin_marker: &str, end_marker: &str) -> String {
    let Some(begin) = document.find(begin_marker) else {
        return document.trim_end().to_owned();
    };
    let before = &document[..begin];
    let after_marker = &document[begin..];
    let after = after_marker.find(end_marker).map_or("", |end_offset| {
        &after_marker[end_offset + end_marker.len()..]
    });
    format!("{}{}", before.trim_end(), after)
        .trim_end()
        .to_owned()
}

fn hooks_block_toml(factoryctl_path: &Path, hook_token_path: &Path) -> String {
    let mut block = String::new();
    block.push_str(HOOKS_BEGIN_MARKER);
    block.push('\n');
    let event = factory_core::ProviderHookEvent::PreToolUse;
    let name = event.provider_event_name();
    let command = hooks::hook_command(factoryctl_path, hook_token_path, event);
    block.push_str(&format!("[[hooks.{name}]]\n"));
    block.push_str(&format!("[[hooks.{name}.hooks]]\n"));
    block.push_str("type = \"command\"\n");
    block.push_str(&format!("command = \"{}\"\n", toml_escape(&command)));
    block.push_str("timeout = 30\n\n");
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

    use factory_core::RunId;
    use serde_json::Value;

    use super::*;

    fn context(directory: &Path) -> SpawnContext {
        SpawnContext {
            run_id: RunId::try_from("2f5a1e2e-2222-4444-8888-0123456789ab").unwrap(),
            source_root: directory.join("source"),
            startup_input: b"fix the admitted task".to_vec(),
            model: None,
            reasoning_effort: None,
            execution_mode: ExecutionMode::WorkspaceWrite,
            hook_token_path: directory.join("runtime").join("hook.token"),
            factoryctl_path: PathBuf::from("/abs/factoryctl"),
            socket_path: PathBuf::from("/abs/factory.sock"),
            agent_dir: directory.join("agent-dir"),
        }
    }

    #[test]
    fn workspace_launch_is_noninteractive_and_sets_codex_home() {
        let directory = tempfile::tempdir().unwrap();
        let ctx = context(directory.path());
        let launch = CodexProvider::with_source_home(directory.path().join("no-real-home"))
            .spawn_spec(&ctx)
            .unwrap();

        assert_eq!(launch.program, PathBuf::from("codex"));
        assert_eq!(
            launch.args,
            vec![
                "exec".to_owned(),
                "--strict-config".to_owned(),
                "--dangerously-bypass-hook-trust".to_owned(),
                "--enable".to_owned(),
                "network_proxy".to_owned(),
                "-c".to_owned(),
                "approval_policy=\"never\"".to_owned(),
                "-c".to_owned(),
                "default_permissions=\"dark_factory_workspace_write\"".to_owned(),
                "-c".to_owned(),
                "permissions.dark_factory_workspace_write={ extends = \":workspace\", filesystem = { \":tmpdir\" = \"deny\", \":slash_tmp\" = \"deny\" }, network = { enabled = true, mode = \"limited\", unix_sockets = { \"/abs/factory.sock\" = \"allow\" }, allow_upstream_proxy = false, enable_socks5 = false, enable_socks5_udp = false } }".to_owned(),
                "-".to_owned(),
            ]
        );
        assert_eq!(launch.startup_input, b"fix the admitted task");
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
    fn installed_codex_accepts_the_bounded_permission_profile_without_a_prompt() {
        if !codex_is_installed() {
            eprintln!("skipping permission-profile validation: codex is not installed");
            return;
        }
        let directory = tempfile::tempdir().unwrap();
        let ctx = context(directory.path());
        let launch = CodexProvider::with_source_home(directory.path().join("no-real-home"))
            .spawn_spec(&ctx)
            .unwrap();
        let codex_home = launch
            .env
            .iter()
            .find(|(name, _)| name == "CODEX_HOME")
            .map(|(_, value)| value)
            .unwrap();
        let output = std::process::Command::new("codex")
            .env("CODEX_HOME", codex_home)
            // Validate the exact feature gate and config overrides through a
            // local metadata command. Never start `exec` or send a paid prompt.
            .arg("--strict-config")
            .args(&launch.args[3..launch.args.len() - 1])
            .args(["doctor", "--json"])
            .output()
            .unwrap();
        let report: Value = serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
            panic!(
                "installed Codex did not validate the strict bounded profile: {error}; stderr={}",
                String::from_utf8_lossy(&output.stderr)
            )
        });
        assert_eq!(report["checks"]["config.load"]["status"], "ok");
        assert_eq!(
            report["checks"]["sandbox.helpers"]["details"]["approval policy"],
            "Never"
        );
        assert_eq!(
            report["checks"]["sandbox.helpers"]["details"]["filesystem sandbox"],
            "restricted"
        );
        assert_eq!(
            report["checks"]["sandbox.helpers"]["details"]["network sandbox"],
            "enabled"
        );
    }

    #[test]
    fn plan_only_launch_is_read_only_and_never_prompts() {
        let directory = tempfile::tempdir().unwrap();
        let mut ctx = context(directory.path());
        ctx.execution_mode = ExecutionMode::PlanOnly;
        let launch = CodexProvider::with_source_home(directory.path().join("no-real-home"))
            .spawn_spec(&ctx)
            .unwrap();

        assert_eq!(
            launch.args,
            vec![
                "exec".to_owned(),
                "--strict-config".to_owned(),
                "--dangerously-bypass-hook-trust".to_owned(),
                "--enable".to_owned(),
                "network_proxy".to_owned(),
                "-c".to_owned(),
                "approval_policy=\"never\"".to_owned(),
                "-c".to_owned(),
                "default_permissions=\"dark_factory_plan_only\"".to_owned(),
                "-c".to_owned(),
                "permissions.dark_factory_plan_only={ extends = \":read-only\", network = { enabled = true, mode = \"limited\", unix_sockets = { \"/abs/factory.sock\" = \"allow\" }, allow_upstream_proxy = false, enable_socks5 = false, enable_socks5_udp = false } }".to_owned(),
                "-".to_owned(),
            ]
        );
    }

    #[test]
    fn unrestricted_launch_uses_only_the_explicit_native_bypass() {
        let directory = tempfile::tempdir().unwrap();
        let mut ctx = context(directory.path());
        ctx.execution_mode = ExecutionMode::Unrestricted;
        let launch = CodexProvider::with_source_home(directory.path().join("no-real-home"))
            .spawn_spec(&ctx)
            .unwrap();

        assert_eq!(
            launch.args,
            vec![
                "exec".to_owned(),
                "--strict-config".to_owned(),
                "--dangerously-bypass-hook-trust".to_owned(),
                "--dangerously-bypass-approvals-and-sandbox".to_owned(),
                "-".to_owned(),
            ]
        );
    }

    #[test]
    fn profile_reasoning_effort_is_passed_explicitly() {
        let directory = tempfile::tempdir().unwrap();
        let mut ctx = context(directory.path());
        ctx.model = Some("gpt-5.6-luna".to_owned());
        ctx.reasoning_effort = Some("medium".to_owned());
        let launch = CodexProvider::with_source_home(directory.path().join("no-real-home"))
            .spawn_spec(&ctx)
            .unwrap();

        assert!(launch.args.windows(2).any(|args| {
            args == [
                "-c".to_owned(),
                "model_reasoning_effort=\"medium\"".to_owned(),
            ]
        }));
    }

    #[test]
    fn launch_passes_model_without_changing_frozen_workspace_authority() {
        let directory = tempfile::tempdir().unwrap();
        let mut ctx = context(directory.path());
        ctx.model = Some("gpt-5-codex".to_owned());
        let launch = CodexProvider::with_source_home(directory.path().join("no-real-home"))
            .spawn_spec(&ctx)
            .unwrap();

        assert_eq!(
            launch.args,
            vec![
                "exec".to_owned(),
                "--strict-config".to_owned(),
                "--dangerously-bypass-hook-trust".to_owned(),
                "--model".to_owned(),
                "gpt-5-codex".to_owned(),
                "--enable".to_owned(),
                "network_proxy".to_owned(),
                "-c".to_owned(),
                "approval_policy=\"never\"".to_owned(),
                "-c".to_owned(),
                "default_permissions=\"dark_factory_workspace_write\"".to_owned(),
                "-c".to_owned(),
                "permissions.dark_factory_workspace_write={ extends = \":workspace\", filesystem = { \":tmpdir\" = \"deny\", \":slash_tmp\" = \"deny\" }, network = { enabled = true, mode = \"limited\", unix_sockets = { \"/abs/factory.sock\" = \"allow\" }, allow_upstream_proxy = false, enable_socks5 = false, enable_socks5_udp = false } }".to_owned(),
                "-".to_owned(),
            ]
        );
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
    fn ambient_operator_rules_cannot_widen_the_typed_attempt_boundary() {
        let directory = tempfile::tempdir().unwrap();
        let real_home = directory.path().join("real-codex-home");
        let real_rules_dir = real_home.join("rules");
        fs::create_dir_all(&real_rules_dir).unwrap();
        fs::write(
            real_rules_dir.join("default.rules"),
            "prefix_rule(pattern=[\"git\", \"push\"], decision=\"allow\")\n",
        )
        .unwrap();

        let ctx = context(directory.path());
        CodexProvider::with_source_home(real_home)
            .spawn_spec(&ctx)
            .unwrap();

        let seeded_rules_dir = directory
            .path()
            .join("agent-dir")
            .join("codex-home")
            .join("rules");
        assert!(
            !seeded_rules_dir.exists(),
            "ambient allow rules must not enter an exact typed attempt"
        );
    }

    #[test]
    fn ignores_ambient_config_and_links_only_auth_from_the_seed_home() {
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
        assert!(config_contents.starts_with(MINIMAL_CONFIG_TOML));
        assert!(!config_contents.contains("model = \"gpt-5.6\""));
        assert!(config_contents.contains(HOOKS_BEGIN_MARKER));
        let auth_link = fs::read_link(codex_home.join("auth.json")).unwrap();
        assert_eq!(auth_link, real_home.join("auth.json"));

        // A different seed home (another Codex account) re-points the auth
        // link on the next spawn without introducing ambient config.
        let other_home = directory.path().join("other-codex-home");
        fs::create_dir_all(&other_home).unwrap();
        fs::write(other_home.join("auth.json"), "{\"token\":\"other\"}").unwrap();
        CodexProvider::with_source_home(other_home.clone())
            .spawn_spec(&ctx)
            .unwrap();
        assert_eq!(
            fs::read_link(codex_home.join("auth.json")).unwrap(),
            other_home.join("auth.json")
        );
        assert!(
            fs::read_to_string(codex_home.join("config.toml"))
                .unwrap()
                .starts_with(MINIMAL_CONFIG_TOML)
        );
        // A regular auth.json an operator placed is never touched.
        fs::remove_file(codex_home.join("auth.json")).unwrap();
        fs::write(codex_home.join("auth.json"), "{\"token\":\"mine\"}").unwrap();
        provider.spawn_spec(&ctx).unwrap();
        assert!(
            fs::symlink_metadata(codex_home.join("auth.json"))
                .unwrap()
                .file_type()
                .is_file()
        );
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
        assert_eq!(contents.matches("[[hooks.PreToolUse]]").count(), 1);
        assert!(!contents.contains("[[hooks.Stop]]"));
        assert!(!contents.contains("[[hooks.PermissionRequest]]"));
    }

    #[test]
    fn hooks_block_toml_has_the_exact_designed_shape_for_one_event() {
        let block = hooks_block_toml(
            Path::new("/abs/factoryctl"),
            Path::new("/abs/runs/attempt-1/hook.token"),
        );
        assert!(block.starts_with(HOOKS_BEGIN_MARKER));
        assert!(block.trim_end().ends_with(HOOKS_END_MARKER));
        assert!(block.contains(
            "[[hooks.PreToolUse]]\n[[hooks.PreToolUse.hooks]]\ntype = \"command\"\ncommand = \"'/abs/factoryctl' hook --token-file '/abs/runs/attempt-1/hook.token' PreToolUse\"\ntimeout = 30\n"
        ));
    }

    #[test]
    fn config_toml_generated_by_a_fresh_seed_parses_under_codex_doctor() {
        // Guards against a schema regression without spawning a real
        // provider process: `codex doctor` is a read-only diagnostic
        // that parses `CODEX_HOME/config.toml` and reports whether it
        // loaded under `--strict-config` for this generated shape.
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

    fn codex_is_installed() -> bool {
        std::process::Command::new("codex")
            .arg("--version")
            .output()
            .is_ok()
    }

    #[test]
    fn config_block_toml_has_the_exact_designed_shape() {
        let block = config_block_toml(Path::new("/abs/changes/change-1"));
        assert_eq!(
            block,
            "# --- dark-factory config BEGIN ---\n\
             [projects.\"/abs/changes/change-1\"]\n\
             trust_level = \"trusted\"\n\
             # --- dark-factory config END ---\n"
        );
    }

    #[test]
    fn spawn_spec_writes_only_project_trust_to_config() {
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
        assert!(!contents.contains("sandbox_mode ="));
        assert!(contents.contains(&format!(
            "[projects.{}]",
            toml_string(&canonicalize_or_given(&ctx.source_root).to_string_lossy())
        )));
        assert!(contents.contains("trust_level = \"trusted\""));
    }

    #[test]
    fn config_block_is_rewritten_idempotently_across_spawns() {
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
        assert_eq!(contents.matches(CONFIG_BEGIN_MARKER).count(), 1);
        assert_eq!(contents.matches(CONFIG_END_MARKER).count(), 1);
        assert_eq!(contents.matches("trust_level = \"trusted\"").count(), 1);
    }

    #[test]
    fn a_real_configs_ambient_sandbox_authority_is_removed_before_profile_launch() {
        // A representative operator config: root-level scalars (including
        // the operator's own legacy sandbox authority), then dozens of trailing
        // `[projects."..."]` tables. Ambient config is never seeded, so this
        // fixture also carries a custom provider table, including a
        // command-backed auth helper, which must not survive the seed.
        let directory = tempfile::tempdir().unwrap();
        let real_home = directory.path().join("real-codex-home");
        fs::create_dir_all(&real_home).unwrap();
        fs::write(
            real_home.join("config.toml"),
            "model = \"gpt-5.6\"\n\
             sandbox_mode = \"read-only\"\n\
             default_permissions = \":danger-full-access\"\n\
             approval_policy = \"on-request\"\n\
             profile = \"unsafe\"\n\
             \n\
             [projects.\"/Users/op/other-repo\"]\n\
             trust_level = \"trusted\"\n\
             \n\
             [projects.\"/Users/op/another-repo\"]\n\
             trust_level = \"trusted\"\n\
             \n\
             [sandbox_workspace_write]\n\
             network_access = true\n\
             \n\
             [profiles.unsafe]\n\
             sandbox_mode = \"danger-full-access\"\n\
             \n\
             [model_providers.custom]\n\
             name = \"Custom\"\n\
             [model_providers.custom.auth]\n\
             command = \"/bin/sh\"\n\
             args = [\"-c\", \"touch /outside; printf token\"]\n",
        )
        .unwrap();

        let ctx = context(directory.path());
        let provider = CodexProvider::with_source_home(real_home);
        let launch = provider.spawn_spec(&ctx).unwrap();
        assert!(launch.args.windows(2).any(|pair| {
            pair == [
                "-c".to_owned(),
                "default_permissions=\"dark_factory_workspace_write\"".to_owned(),
            ]
        }));

        let codex_home = directory.path().join("agent-dir").join("codex-home");
        let contents = fs::read_to_string(codex_home.join("config.toml")).unwrap();

        // Legacy and ambient profile authority do not compose with this
        // attempt's exact permission profile, so none survives the seed.
        assert!(!contents.contains("sandbox_mode"));
        assert!(!contents.contains("sandbox_workspace_write"));
        assert!(!contents.contains("default_permissions"));
        assert!(!contents.contains("profile = \"unsafe\""));
        assert!(!contents.contains("[profiles.unsafe]"));
        // No ambient provider configuration survives. Model selection comes
        // only from the explicit admitted profile; custom provider tables can
        // also execute auth helpers outside the permission profile.
        assert!(!contents.contains("model = \"gpt-5.6\""));
        assert!(!contents.contains("approval_policy = \"on-request\""));
        assert!(!contents.contains("model_providers"));
        assert!(!contents.contains("/bin/sh"));
        assert!(!contents.contains("touch /outside"));
        // The operator's own project trust entries do not: an operator's
        // decision to trust *their own* repos has no bearing on this
        // factory attempt.
        assert!(!contents.contains("/Users/op/other-repo"));
        assert!(!contents.contains("/Users/op/another-repo"));
        // This attempt's own daemon-owned source still gains a trust entry, from
        // `rewrite_config_block` -- unrelated to what was (or wasn't)
        // seeded.
        assert!(contents.contains(&format!(
            "[projects.{}]",
            toml_string(&ctx.source_root.to_string_lossy())
        )));

        if !codex_is_installed() {
            eprintln!(
                "skipping real codex doctor check: codex is not installed in this environment"
            );
            return;
        }
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

    /// An operator config carrying the three shapes that must not be
    /// inherited:
    /// `[mcp_servers.*]` (the "Starting MCP servers" stall), `[projects.*]`
    /// (an operator's own repo trust, irrelevant to a factory worker), and
    /// `[hooks.state]` (Codex's own persisted hook-trust bookkeeping) plus
    /// the `[[hooks.<Event>]]`/`[[hooks.<Event>.hooks]]` shape a real
    /// `~/.codex/config.toml` could also carry if the operator has their
    /// own hooks configured -- both variants have top-level key `hooks`.
    /// None of it should survive the seed.
    #[test]
    fn ambient_operator_config_is_not_seeded() {
        let directory = tempfile::tempdir().unwrap();
        let real_home = directory.path().join("real-codex-home");
        fs::create_dir_all(&real_home).unwrap();
        fs::write(
            real_home.join("config.toml"),
            "model = \"gpt-5.6\"\n\
             model_provider = \"openai\"\n\
             approval_policy = \"on-request\"\n\
             \n\
             [mcp_servers.filesystem]\n\
             command = \"npx\"\n\
             args = [\"-y\", \"@modelcontextprotocol/server-filesystem\"]\n\
             \n\
             [mcp_servers.browser]\n\
             command = \"mcp-browser\"\n\
             \n\
             [projects.\"/Users/op/some-repo\"]\n\
             trust_level = \"trusted\"\n\
             \n\
             [hooks.state]\n\
             \"/Users/op/.codex/config.toml:SessionStart:0:0\" = true\n\
             \n\
             [[hooks.SessionStart]]\n\
             [[hooks.SessionStart.hooks]]\n\
             type = \"command\"\n\
             command = \"/Users/op/bin/operators-own-hook.sh\"\n\
             \n\
             [model_providers.custom]\n\
             name = \"Custom\"\n\
             [model_providers.custom.auth]\n\
             command = \"/bin/sh\"\n\
             args = [\"-c\", \"touch /outside; printf token\"]\n",
        )
        .unwrap();

        let ctx = context(directory.path());
        CodexProvider::with_source_home(real_home)
            .spawn_spec(&ctx)
            .unwrap();

        let codex_home = directory.path().join("agent-dir").join("codex-home");
        let contents = fs::read_to_string(codex_home.join("config.toml")).unwrap();

        // Dropped: every inherited table, including each authority-bearing
        // shape in this fixture.
        assert!(!contents.contains("mcp_servers"));
        assert!(!contents.contains("server-filesystem"));
        assert!(!contents.contains("mcp-browser"));
        assert!(!contents.contains("/Users/op/some-repo"));
        assert!(!contents.contains("hooks.state"));
        assert!(!contents.contains("operators-own-hook.sh"));
        // The operator's lifecycle hook is dropped; only the daemon's
        // authenticated authority hook is generated.
        assert!(!contents.contains("[[hooks.SessionStart]]"));
        assert_eq!(contents.matches("[[hooks.PreToolUse]]").count(), 1);
        assert!(contents.contains("factoryctl' hook --token-file"));

        // Model selection and custom provider tables are both absent. The
        // latter is executable authority because its auth block can launch a
        // helper.
        assert!(!contents.contains("model = \"gpt-5.6\""));
        assert!(!contents.contains("model_provider = \"openai\""));
        assert!(!contents.contains("approval_policy = \"on-request\""));
        assert!(!contents.contains("model_providers"));
        assert!(!contents.contains("/bin/sh"));
        assert!(!contents.contains("touch /outside"));

        if !codex_is_installed() {
            eprintln!(
                "skipping real codex doctor check: codex is not installed in this environment"
            );
            return;
        }
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

    #[test]
    fn the_daemons_codex_home_overrides_the_operators_own() {
        let dogfood = CodexProvider::from_environment(
            Some("/Users/me/.codex-dogfood".into()),
            Some("/Users/me".into()),
        );
        assert_eq!(
            dogfood.source_home.as_deref(),
            Some(Path::new("/Users/me/.codex-dogfood"))
        );
        let personal = CodexProvider::from_environment(Some("".into()), Some("/Users/me".into()));
        assert_eq!(
            personal.source_home.as_deref(),
            Some(Path::new("/Users/me/.codex")),
            "an empty override means unset"
        );
        assert_eq!(
            CodexProvider::from_environment(None, None).source_home,
            None
        );
    }
}
