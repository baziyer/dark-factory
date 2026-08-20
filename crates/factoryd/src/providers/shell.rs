//! Deterministic non-interactive provider used by lifecycle tests.
//!
//! A configured command receives the run's startup input on stdin. With no
//! configured command, `sh -s` treats that input as the one shell program to
//! execute; it never opens an interactive shell.

use std::path::PathBuf;

use crate::providers::{Capabilities, Provider, ProviderError, ProviderLaunch, SpawnContext};

pub const PERMISSION_MODES: [&str; 0] = [];

#[derive(Clone, Copy, Debug, Default)]
pub struct ShellProvider;

impl Provider for ShellProvider {
    fn spawn_spec(&self, ctx: &SpawnContext) -> Result<ProviderLaunch, ProviderError> {
        let args = ctx.model.as_ref().map_or_else(
            || vec!["-s".to_owned()],
            |command| vec!["-lc".to_owned(), command.clone()],
        );
        Ok(ProviderLaunch {
            program: PathBuf::from("sh"),
            args,
            env: vec![(
                "DARK_FACTORY_FACTORYCTL".to_owned(),
                ctx.factoryctl_path.to_string_lossy().into_owned(),
            )],
            startup_input: ctx.startup_input.clone(),
        })
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities::for_provider(factory_core::Provider::Shell, &PERMISSION_MODES)
    }
}

#[cfg(test)]
mod tests {
    use factory_core::RunId;

    use super::*;

    fn context(directory: &std::path::Path) -> SpawnContext {
        SpawnContext {
            run_id: RunId::try_from("2f5a1e2e-2222-4444-8888-0123456789ab").unwrap(),
            worktree: directory.join("worktree"),
            startup_input: b"printf ready".to_vec(),
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
    fn no_command_executes_startup_input_noninteractively() {
        let directory = tempfile::tempdir().unwrap();
        let launch = ShellProvider
            .spawn_spec(&context(directory.path()))
            .unwrap();
        assert_eq!(launch.program, PathBuf::from("sh"));
        assert_eq!(launch.args, ["-s"]);
        assert_eq!(launch.startup_input, b"printf ready");
    }

    #[test]
    fn configured_command_receives_the_same_startup_input() {
        let directory = tempfile::tempdir().unwrap();
        let mut ctx = context(directory.path());
        ctx.model = Some("/abs/fixtures/shell-agent.sh".to_owned());
        let launch = ShellProvider.spawn_spec(&ctx).unwrap();
        assert_eq!(launch.args, ["-lc", "/abs/fixtures/shell-agent.sh"]);
        assert_eq!(launch.startup_input, b"printf ready");
    }
}
