//! The provider boundary: how to launch each supported coding-agent CLI as
//! one resident interactive session, and how its own hook events map onto
//! `factory_core::ProviderHookEvent`.
//!
//! A [`Provider`] describes *launch* only. It never owns a PTY, never calls
//! `send_input`/`resize`/`stop`, and never reads process output: those are
//! generic over any provider's PTY-backed session, implemented once by the
//! daemon's session runner (`execution.rs` plus `factory-runner`), because
//! every provider ends up speaking the same normalized hook wire protocol
//! once its own hook subprocess calls `factoryctl hook` (see
//! `crates/factoryctl/src/main.rs`). This mirrors the boundary described in
//! `docs/providers.md`: `spawn` (this trait) is provider-specific;
//! `send_input`/`resize`/`stop`/`events` are not.
//!
//! See `docs/providers.md` for how to add a new provider, and
//! `crates/factory-runner/src/bin/fake-agent.rs` for a minimal example/mock
//! provider used by the deterministic lifecycle tests.

pub mod claude;
pub mod codex;
pub mod hooks;
pub mod shell;

use std::path::PathBuf;

use factory_core::{AgentId, ProjectId, SessionId};

/// Everything a provider needs to describe how to launch one resident
/// interactive session for one agent. Built by the daemon (session
/// spawn/resume logic in `execution.rs`), consumed by exactly one
/// [`Provider::spawn_spec`] call.
pub struct SpawnContext {
    pub agent_id: AgentId,
    pub project_id: ProjectId,
    /// This session's own durable identity. For a fresh Claude launch this
    /// value is also used as the Claude session UUID passed to
    /// `--session-id`, so the daemon never has to learn the provider's
    /// session identity back from hook payloads; see
    /// `claude::ClaudeProvider::spawn_spec`.
    pub session_id: SessionId,
    /// Absolute path to the agent's git worktree; the session's `cwd`.
    pub worktree: PathBuf,
    /// An explicit provider model, or `None` for the provider's own
    /// interactive default.
    pub model: Option<String>,
    /// A provider-scoped permission/approval mode string (Claude:
    /// `acceptEdits`/`plan`/...; Codex: `on-request`/`never`), or `None` to
    /// override the factory-wide auto-mode default.
    pub permission_mode: Option<String>,
    /// Factory-wide bypass default. An explicit `permission_mode` wins.
    pub auto_mode: bool,
    /// The provider's own prior session/thread identity to resume (e.g. a
    /// Claude session UUID or Codex thread UUID previously observed from
    /// this agent's last session), or `None` to launch fresh.
    pub resume: Option<String>,
    /// Absolute path to this session's private hook-token file. The file is
    /// expected to already exist (written by
    /// [`hooks::write_hook_token`]) — a provider only needs its path to
    /// embed in generated hook commands, never its contents.
    pub hook_token_path: PathBuf,
    /// Trusted absolute path to the `factoryctl` binary that generated hook
    /// commands should invoke. A provider's hook subprocess inherits the
    /// runner's sanitized `PATH`, so this must not be a bare name.
    pub factoryctl_path: PathBuf,
    /// This agent's guidance directory (`factory_core::paths::agent_dir`),
    /// where per-agent generated provider configuration lives
    /// (`claude-settings.json`, `codex-home/`) so it survives across this
    /// agent's sessions (needed for Codex `resume`, see
    /// `codex::CodexProvider`).
    pub agent_dir: PathBuf,
    /// Absolute path to the daemon's local control socket. Not used to
    /// generate hook commands (those call `factoryctl hook`, which resolves
    /// its own socket); carried here only so a provider can export it as
    /// `DARK_FACTORY_SOCKET` in its launch environment if it needs to
    /// (`factoryctl` invocations run by the agent itself, e.g. `task
    /// done`, pick it up the same way).
    pub socket_path: PathBuf,
}

/// One resolved launch: an executable, its argument vector, environment
/// additions, and the paths of any configuration files this call just wrote.
///
/// This deliberately mirrors `runner_process::LaunchSpec`'s
/// `provider_program`/`provider_arguments`/`provider_environment` shape but
/// is not that type: [`InteractiveLaunch`] is provider-agnostic output,
/// independent of how the daemon happens to spawn processes today.
pub struct InteractiveLaunch {
    /// Executable path or bare name (resolved against the runner's
    /// sanitized `PATH`, matching `runner_process::resolve_executable`).
    pub program: PathBuf,
    pub args: Vec<String>,
    /// Provider-specific environment additions, e.g. Codex's `CODEX_HOME`.
    /// Ambient environment (`HOME`, `PATH`, ...) is the generic session
    /// runner's concern, not a provider's.
    pub env: Vec<(String, String)>,
    /// Absolute paths of configuration this call just wrote (settings
    /// file, hooks-seeded config, ...), for logging and tests only; the
    /// session runner does not read this list.
    pub generated_files: Vec<PathBuf>,
}

/// What a provider supports, so generic callers (the dispatcher, the TUI)
/// can behave correctly without a provider-specific `match`.
pub struct Capabilities {
    /// Whether this provider's hook events drive session state at all. Both
    /// shipped providers are `true`; a minimal/example provider (e.g. a
    /// mock used only for deterministic tests) may be `false`.
    pub hooks: bool,
    /// Whether [`SpawnContext::resume`] is meaningful for this provider.
    pub resume: bool,
    /// The permission/approval mode strings this provider accepts in
    /// [`SpawnContext::permission_mode`], for validating `agent profile set
    /// --permission-mode` up front rather than failing at spawn time.
    pub permission_modes: &'static [&'static str],
}

#[derive(Debug, thiserror::Error)]
pub enum ProviderError {
    #[error("session id {session_id:?} is not a canonical UUID Claude requires for --session-id")]
    SessionIdNotUuid { session_id: String },
    #[error("resume target {value:?} is not a canonical UUID")]
    ResumeIdNotUuid { value: String },
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
}

/// A provider describes how to launch a resident interactive session for
/// one agent and, implicitly through [`Capabilities`], what that session
/// supports. It never owns a PTY: `send_input`/`resize`/`stop`/`events` are
/// implemented once, generically, by the daemon's session runner.
pub trait Provider {
    /// Resolves one launch: writes any generated configuration files this
    /// provider needs (hooks, seeded home, ...) and returns the resulting
    /// executable, argv, and environment additions.
    ///
    /// # Errors
    ///
    /// Returns a [`ProviderError`] if `ctx` cannot produce a valid launch
    /// (an unusable session/resume identity) or generated configuration
    /// cannot be written.
    fn spawn_spec(&self, ctx: &SpawnContext) -> Result<InteractiveLaunch, ProviderError>;

    fn capabilities(&self) -> Capabilities;
}
