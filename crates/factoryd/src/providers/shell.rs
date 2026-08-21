//! Deterministic non-interactive provider used by lifecycle tests.
//!
//! A configured command receives the run's startup input on stdin. With no
//! configured command, `sh -s` treats that input as the one shell program to
//! execute; it never opens an interactive shell.

use std::path::PathBuf;

use factory_core::ExecutionMode;

use crate::providers::{Provider, ProviderError, ProviderLaunch, SpawnContext};

#[derive(Clone, Copy, Debug, Default)]
pub struct ShellProvider;

impl Provider for ShellProvider {
    fn spawn_spec(&self, ctx: &SpawnContext) -> Result<ProviderLaunch, ProviderError> {
        if ctx.execution_mode != ExecutionMode::Unrestricted {
            return Err(ProviderError::UnsupportedExecutionMode {
                provider: factory_core::Provider::Shell,
                mode: ctx.execution_mode,
            });
        }
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
}

#[cfg(test)]
mod tests {
    use factory_core::RunId;

    use super::*;

    fn context(directory: &std::path::Path) -> SpawnContext {
        SpawnContext {
            run_id: RunId::try_from("2f5a1e2e-2222-4444-8888-0123456789ab").unwrap(),
            source_root: directory.join("source"),
            startup_input: b"printf ready".to_vec(),
            model: None,
            reasoning_effort: None,
            execution_mode: ExecutionMode::Unrestricted,
            hook_token_path: directory.join("runtime/hook.token"),
            factoryctl_path: PathBuf::from("/abs/factoryctl"),
            socket_path: PathBuf::from("/abs/factory.sock"),
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

    #[test]
    fn shell_refuses_modes_it_cannot_enforce() {
        let directory = tempfile::tempdir().unwrap();
        let mut ctx = context(directory.path());
        ctx.execution_mode = ExecutionMode::WorkspaceWrite;
        assert!(matches!(
            ShellProvider.spawn_spec(&ctx),
            Err(ProviderError::UnsupportedExecutionMode {
                provider: factory_core::Provider::Shell,
                mode: ExecutionMode::WorkspaceWrite,
            })
        ));
    }
}
