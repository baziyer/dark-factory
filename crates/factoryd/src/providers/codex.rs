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
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CodexProvider {
    /// The source used to link authentication into a fresh attempt-owned
    /// `CODEX_HOME`: the daemon's own `$CODEX_HOME` if set
    /// — Codex's own convention, and how a factory runs on a different
    /// account than the operator's shell (`CODEX_HOME=~/.codex-dogfood`
    /// in the launchd job) — else `$HOME/.codex`; overridable for tests via
    /// [`CodexProvider::from_environment`].
    source_home: Option<PathBuf>,
}

impl CodexProvider {
    /// Creates a provider with one source home resolved at daemon startup.
    #[must_use]
    pub const fn new(source_home: Option<PathBuf>) -> Self {
        Self { source_home }
    }

    /// Resolves the source exactly once as Codex does: `$CODEX_HOME` when
    /// non-empty, otherwise `$HOME/.codex`.
    #[must_use]
    pub fn from_environment(codex_home: Option<OsString>, home: Option<OsString>) -> Self {
        Self::new(
            codex_home
                .filter(|value| !value.is_empty())
                .map(PathBuf::from)
                .or_else(|| home.map(|home| PathBuf::from(home).join(".codex"))),
        )
    }
}

impl Provider for CodexProvider {
    fn spawn_spec(&self, ctx: &SpawnContext) -> Result<ProviderLaunch, ProviderError> {
        let codex_home = ctx.agent_dir.join("codex-home");
        write_codex_home(
            &codex_home,
            self.source_home.as_deref(),
            &ctx.source_root,
            &ctx.factoryctl_path,
            &ctx.hook_token_path,
        )?;

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

/// Creates one attempt-owned Codex home. The runtime path is fresh for every
/// admission, and crash recovery reconnects to the prepared runner rather than
/// calling provider launch again, so generated configuration has no update or
/// merge path. Only subscription authentication crosses from the daemon's
/// startup-resolved Codex home.
fn write_codex_home(
    codex_home: &Path,
    source_home: Option<&Path>,
    source_root: &Path,
    factoryctl_path: &Path,
    hook_token_path: &Path,
) -> Result<(), ProviderError> {
    hooks::ensure_private_dir(codex_home).map_err(|source| ProviderError::Seed {
        path: codex_home.to_path_buf(),
        source,
    })?;

    if let Some(source_home) = source_home {
        let source_auth = source_home.join("auth.json");
        if source_auth.exists() {
            let auth_path = codex_home.join("auth.json");
            std::os::unix::fs::symlink(source_auth, &auth_path).map_err(|source| {
                ProviderError::Seed {
                    path: auth_path,
                    source,
                }
            })?;
        }
    }
    let config_path = codex_home.join("config.toml");
    let config = generated_config_toml(
        &canonicalize_or_given(source_root),
        factoryctl_path,
        hook_token_path,
    );
    hooks::write_private_file(&config_path, config.as_bytes()).map_err(|source| {
        ProviderError::Seed {
            path: config_path,
            source,
        }
    })
}

fn canonicalize_or_given(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap_or_else(|_| path.to_owned())
}

fn generated_config_toml(
    source_root: &Path,
    factoryctl_path: &Path,
    hook_token_path: &Path,
) -> String {
    let event = factory_core::ProviderHookEvent::PreToolUse;
    let name = event.provider_event_name();
    let command = hooks::hook_command(factoryctl_path, hook_token_path, event);
    format!(
        "{MINIMAL_CONFIG_TOML}\
         [[hooks.{name}]]\n\
         [[hooks.{name}.hooks]]\n\
         type = \"command\"\n\
         command = \"{}\"\n\
         timeout = 30\n\n\
         [projects.{}]\n\
         trust_level = \"trusted\"\n",
        toml_escape(&command),
        toml_string(&source_root.to_string_lossy()),
    )
}

fn toml_string(value: &str) -> String {
    format!("\"{}\"", toml_escape(value))
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

    fn provider(source_home: impl Into<PathBuf>) -> CodexProvider {
        CodexProvider::new(Some(source_home.into()))
    }

    #[test]
    fn workspace_launch_is_noninteractive_and_sets_codex_home() {
        let directory = tempfile::tempdir().unwrap();
        let ctx = context(directory.path());
        let launch = provider(directory.path().join("no-real-home"))
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
        let launch = provider(directory.path().join("no-real-home"))
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
        let launch = provider(directory.path().join("no-real-home"))
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
        let launch = provider(directory.path().join("no-real-home"))
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
    fn launch_passes_model_and_reasoning_without_changing_frozen_authority() {
        let directory = tempfile::tempdir().unwrap();
        let mut ctx = context(directory.path());
        ctx.model = Some("gpt-5-codex".to_owned());
        ctx.reasoning_effort = Some("medium".to_owned());
        let launch = provider(directory.path().join("no-real-home"))
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
                "-c".to_owned(),
                "model_reasoning_effort=\"medium\"".to_owned(),
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
    fn fresh_home_contains_only_generated_config_and_auth() {
        let directory = tempfile::tempdir().unwrap();
        let source_home = directory.path().join("source-home");
        fs::create_dir_all(source_home.join("rules")).unwrap();
        fs::write(
            source_home.join("config.toml"),
            "default_permissions = \":danger-full-access\"\n",
        )
        .unwrap();
        fs::write(
            source_home.join("rules/default.rules"),
            "prefix_rule(pattern=[\"git\", \"push\"], decision=\"allow\")\n",
        )
        .unwrap();
        fs::write(source_home.join("auth.json"), "{\"token\":\"secret\"}").unwrap();
        let mut ctx = context(directory.path());
        ctx.source_root = PathBuf::from("/abs/changes/change-1");
        ctx.hook_token_path = PathBuf::from("/abs/runs/attempt-1/hook.token");
        provider(source_home.clone()).spawn_spec(&ctx).unwrap();

        let codex_home = directory.path().join("agent-dir/codex-home");
        let config_path = codex_home.join("config.toml");
        let contents = fs::read_to_string(&config_path).unwrap();
        assert_eq!(
            contents,
            "# Dark Factory generated Codex home; ambient provider config is not inherited.\n\
             [[hooks.PreToolUse]]\n\
             [[hooks.PreToolUse.hooks]]\n\
             type = \"command\"\n\
             command = \"'/abs/factoryctl' hook --token-file '/abs/runs/attempt-1/hook.token' PreToolUse\"\n\
             timeout = 30\n\n\
             [projects.\"/abs/changes/change-1\"]\n\
             trust_level = \"trusted\"\n"
        );
        assert_eq!(
            fs::read_link(codex_home.join("auth.json")).unwrap(),
            source_home.join("auth.json")
        );
        let mut entries = fs::read_dir(&codex_home)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect::<Vec<_>>();
        entries.sort();
        assert_eq!(entries, ["auth.json", "config.toml"]);
        assert_eq!(
            fs::metadata(&codex_home).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(config_path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[test]
    fn generated_toml_escapes_backslashes_and_quotes() {
        assert_eq!(toml_string(r#"a\b"c"#), r#""a\\b\"c""#);
    }

    fn codex_is_installed() -> bool {
        std::process::Command::new("codex")
            .arg("--version")
            .output()
            .is_ok()
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
