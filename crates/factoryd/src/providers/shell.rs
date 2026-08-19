//! [`ShellProvider`]: the minimal example [`Provider`] — a plain POSIX
//! shell, driven entirely by `factoryctl hook`/`task done`/`task blocked`
//! calls the launched command makes itself, with no native hook
//! configuration, permission modes, or resume support of its own.
//!
//! This exists for two reasons: it is the deterministic fixture behind
//! `crates/factoryd/tests/sessions_e2e.rs` (a real `claude`/`codex` session
//! is nondeterministic and costs tokens; a shell script is neither), and it
//! is the shortest template for a contributor adding a new provider — see
//! `docs/providers.md`.
//!
//! Unlike Claude and Codex, a shell command has no generated hooks
//! configuration file for the daemon to point at `factoryctl`'s trusted
//! absolute path (`docs/providers.md`'s "provider A1" resolution): there is
//! nothing here to embed it in. Instead this provider exports it as
//! `DARK_FACTORY_FACTORYCTL` in the session's environment (a fixed name in
//! `runner_process::SESSION_ENVIRONMENT_NAMES`, alongside the four
//! `DARK_FACTORY_*` identity variables every session gets); the fixture
//! script (`crates/factoryd/tests/fixtures/shell-agent.sh`) reads both that
//! and `DARK_FACTORY_SESSION_TOKEN_FILE` to call
//! `factoryctl hook --token-file "$DARK_FACTORY_SESSION_TOKEN_FILE" <Event>`
//! itself.

use std::path::PathBuf;

use crate::providers::{Capabilities, InteractiveLaunch, Provider, ProviderError, SpawnContext};

pub const PERMISSION_MODES: [&str; 0] = [];

/// Interactive-session [`Provider`] for a plain shell. Launches `sh -lc
/// "<command>"`, where `<command>` is [`SpawnContext::model`] repurposed as
/// the command to run (an agent's `agent_profiles.model`, e.g. an absolute
/// path to a script) or plain `sh` when unset — a bare interactive shell,
/// useful for manually poking at a session but unable to drive itself.
#[derive(Clone, Copy, Debug, Default)]
pub struct ShellProvider;

impl Provider for ShellProvider {
    fn spawn_spec(&self, ctx: &SpawnContext) -> Result<InteractiveLaunch, ProviderError> {
        let command = ctx.model.clone().unwrap_or_else(|| "sh".to_owned());
        Ok(InteractiveLaunch {
            program: PathBuf::from("sh"),
            args: vec!["-lc".to_owned(), command],
            env: vec![(
                "DARK_FACTORY_FACTORYCTL".to_owned(),
                ctx.factoryctl_path.to_string_lossy().into_owned(),
            )],
            generated_files: Vec::new(),
            runtime: Default::default(),
        })
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities::for_provider(
            factory_core::Provider::Shell,
            true,
            false,
            &PERMISSION_MODES,
        )
    }
}

#[cfg(test)]
mod tests {
    use factory_core::{AgentId, ProjectId, SessionId};

    use super::*;

    fn context(directory: &std::path::Path) -> SpawnContext {
        SpawnContext {
            agent_id: AgentId::try_from("worker-1").unwrap(),
            project_id: ProjectId::try_from("factory").unwrap(),
            session_id: SessionId::try_from("2f5a1e2e-2222-4444-8888-0123456789ab").unwrap(),
            worktree: directory.join("worktree"),
            model: None,
            reasoning_effort: None,
            permission_mode: None,
            auto_mode: true,
            resume: None,
            hook_token_path: directory.join("runtime").join("hook.token"),
            factoryctl_path: PathBuf::from("/abs/factoryctl"),
            agent_dir: directory.join("agent-dir"),
            socket_path: directory.join("f.sock"),
        }
    }

    #[test]
    fn no_model_falls_back_to_a_plain_interactive_shell() {
        let directory = tempfile::tempdir().unwrap();
        let launch = ShellProvider
            .spawn_spec(&context(directory.path()))
            .unwrap();
        assert_eq!(launch.program, PathBuf::from("sh"));
        assert_eq!(launch.args, vec!["-lc".to_owned(), "sh".to_owned()]);
        assert_eq!(
            launch.env,
            vec![(
                "DARK_FACTORY_FACTORYCTL".to_owned(),
                "/abs/factoryctl".to_owned()
            )]
        );
        assert!(launch.generated_files.is_empty());
    }

    #[test]
    fn model_becomes_the_command_sh_lc_runs() {
        let directory = tempfile::tempdir().unwrap();
        let mut ctx = context(directory.path());
        ctx.model = Some("/abs/fixtures/shell-agent.sh".to_owned());
        let launch = ShellProvider.spawn_spec(&ctx).unwrap();
        assert_eq!(
            launch.args,
            vec!["-lc".to_owned(), "/abs/fixtures/shell-agent.sh".to_owned()]
        );
    }

    #[test]
    fn capabilities_drive_hooks_but_never_resume_and_accept_no_permission_mode() {
        let capabilities = ShellProvider.capabilities();
        assert!(capabilities.hooks);
        assert!(!capabilities.resume);
        assert!(capabilities.permission_modes.is_empty());
    }
}
