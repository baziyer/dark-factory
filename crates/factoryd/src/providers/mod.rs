//! The provider boundary: how to launch each supported coding-agent CLI as
//! one non-interactive process for one admitted run.
//!
//! A [`Provider`] describes launch only. It never owns process lifecycle,
//! reads output, or extends a run's authority. The daemon supplies the exact
//! [`factory_core::RunId`] and startup input; the process exits at the end of
//! that run.
//!
//! See `docs/providers.md` for how to add a new provider, and
//! `crates/factory-runner/src/bin/fake-agent.rs` for a minimal example/mock
//! provider used by the deterministic lifecycle tests.

pub mod claude;
pub mod codex;
pub mod hooks;
pub mod shell;

use std::path::PathBuf;

use factory_core::{ExecutionMode, RunId};

/// Everything a provider needs to describe how to launch one process for one
/// admitted run. Built by the daemon and consumed by exactly one
/// [`Provider::spawn_spec`] call.
pub struct SpawnContext {
    /// The exact durable attempt that authorizes this process.
    pub run_id: RunId,
    /// Absolute path to the run's daemon-owned `.git`-free Change source; the
    /// provider process `cwd`.
    pub source_root: PathBuf,
    /// The one input delivered when the process starts.
    pub startup_input: Vec<u8>,
    /// An explicit provider model, or `None` for the provider's default.
    pub model: Option<String>,
    /// An explicit provider reasoning tier, when supported.
    pub reasoning_effort: Option<String>,
    /// Typed authority frozen by admission for this exact attempt.
    pub execution_mode: ExecutionMode,
    /// Reserved absolute path for this run's private hook-token file. The
    /// daemon creates it after [`Provider::spawn_spec`] succeeds and before
    /// launch; a provider may embed the path in generated hook commands but
    /// never reads its contents.
    pub hook_token_path: PathBuf,
    /// Trusted absolute path to the `factoryctl` binary that generated hook
    /// commands should invoke. A provider's hook subprocess inherits the
    /// runner's sanitized `PATH`, so this must not be a bare name.
    pub factoryctl_path: PathBuf,
    /// Exact daemon Unix socket the authenticated `factoryctl` control call
    /// must reach from a bounded provider sandbox.
    pub socket_path: PathBuf,
    /// Directory where generated provider configuration lives.
    pub agent_dir: PathBuf,
}

/// One resolved launch: an executable, its argument vector, environment
/// additions, and startup input.
///
/// This deliberately mirrors `runner_process::LaunchSpec`'s
/// `provider_program`/`provider_arguments`/`provider_environment` shape but
/// is not that type: [`ProviderLaunch`] is provider-agnostic output,
/// independent of how the daemon happens to spawn processes today.
pub struct ProviderLaunch {
    /// Executable path or bare name (resolved against the runner's
    /// sanitized `PATH`, matching `runner_process::resolve_executable`).
    pub program: PathBuf,
    pub args: Vec<String>,
    /// Provider-specific environment additions, e.g. Codex's `CODEX_HOME`.
    /// Ambient environment (`HOME`, `PATH`, ...) is the runner's concern,
    /// not a provider's.
    pub env: Vec<(String, String)>,
    /// Bytes written once to the provider's stdin before stdin is closed.
    pub startup_input: Vec<u8>,
}

#[derive(Debug, thiserror::Error)]
pub enum ProviderError {
    #[error("run id {run_id:?} is not a canonical UUID Claude requires for --session-id")]
    RunIdNotUuid { run_id: String },
    #[error("cannot write generated provider config at {path}: {source}")]
    Config {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("cannot seed provider home at {path}: {source}")]
    Seed {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("execution mode {mode:?} is not supported by provider {provider:?}")]
    UnsupportedExecutionMode {
        provider: factory_core::Provider,
        mode: ExecutionMode,
    },
    #[error("provider {provider:?} is unavailable: {reason}")]
    Unavailable {
        provider: factory_core::Provider,
        reason: &'static str,
    },
    #[error("execution mode {mode:?} is not supported by provider {provider:?} on {platform}")]
    UnsupportedPlatform {
        provider: factory_core::Provider,
        mode: ExecutionMode,
        platform: String,
    },
    #[error("Claude Code preflight failed for {program}: {reason}")]
    ClaudePreflight { program: PathBuf, reason: String },
}

/// A provider describes how to launch one non-interactive process for one
/// admitted run. Process ownership remains in the daemon.
pub trait Provider {
    /// Resolves one launch: writes any generated configuration files this
    /// provider needs (hooks, seeded home, ...) and returns the resulting
    /// executable, argv, and environment additions.
    ///
    /// # Errors
    ///
    /// Returns a [`ProviderError`] if `ctx` cannot produce a valid launch
    /// (an unusable run identity) or generated configuration
    /// cannot be written.
    fn spawn_spec(&self, ctx: &SpawnContext) -> Result<ProviderLaunch, ProviderError>;
}
