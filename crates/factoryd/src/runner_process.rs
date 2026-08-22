//! Safe process boundary between the daemon and the stable runner.

use std::{
    env,
    ffi::{OsStr, OsString},
    fs,
    io::{Seek as _, Write as _},
    os::unix::{ffi::OsStrExt, fs::MetadataExt, fs::OpenOptionsExt, fs::PermissionsExt},
    path::{Path, PathBuf},
    process::Stdio,
};

use factory_core::{
    RunId, RunnerInstanceId,
    runner::{MAX_STARTUP_STDIN_BYTES, RUNNER_STARTUP_LEASE_FILE},
};
use rustix::fs::FlockOperation;
use tokio::process::{Child, Command};

const SAFE_ENVIRONMENT_NAMES: [&str; 9] = [
    "HOME", "USER", "LOGNAME", "SHELL", "PATH", "TMPDIR", "LANG", "LC_ALL", "LC_CTYPE",
];

/// The only daemon-owned environment names a provider launch may add. This
/// closed list carries exact attempt identity and stable local paths without
/// allowing a provider implementation to smuggle arbitrary ambient values.
const ATTEMPT_ENVIRONMENT_NAMES: [&str; 5] = [
    "DARK_FACTORY_AGENT",
    "DARK_FACTORY_PROJECT",
    "DARK_FACTORY_SOCKET",
    "DARK_FACTORY_ATTEMPT_TOKEN_FILE",
    "DARK_FACTORY_FACTORYCTL",
];

/// Everything needed to launch one provider command under `factory-runner`.
///
/// This deliberately has no `Debug` or `Clone` implementation because it owns
/// the task bytes.
pub struct LaunchSpec {
    /// Trusted absolute path to the installed stable runner executable.
    pub runner_program: PathBuf,
    /// Trusted absolute path to `factoryctl`. Its directory is prepended to
    /// the provider process's `PATH` so the attempt's bare `factoryctl task
    /// done`/`task blocked` invocation resolves independently of the
    /// operator's shell configuration.
    pub factoryctl_path: PathBuf,
    /// Provider executable path or name to resolve from the captured `PATH`.
    pub provider_program: PathBuf,
    /// Non-secret provider flags. These are observable in process metadata and
    /// must never contain task content, credentials, or tokens.
    pub provider_arguments: Vec<OsString>,
    /// Closed provider-specific environment additions. Ambient values are
    /// never forwarded implicitly.
    pub provider_environment: ProviderEnvironment,
    /// Fixed-name daemon-set additions on top of [`SAFE_ENVIRONMENT_NAMES`]
    /// and `provider_environment`; every name must appear in
    /// [`ATTEMPT_ENVIRONMENT_NAMES`] or [`spawn_runner`] rejects the launch
    /// before spawning anything.
    pub attempt_environment: Vec<(String, String)>,
    pub run_id: RunId,
    pub runner_instance_id: RunnerInstanceId,
    pub runtime_dir: PathBuf,
    /// Initial cwd for the trusted wrapper or direct provider process.
    pub cwd: PathBuf,
    /// Exact daemon-owned source boundary inherited by the eventual provider,
    /// even when a trusted provisioning wrapper starts from a private cwd.
    pub source_root: PathBuf,
    pub startup_input: Vec<u8>,
}

/// The only provider-specific environment additions allowed across the
/// daemon-to-runner boundary.
///
/// This deliberately has no `Debug` or `Clone` implementation because an
/// explicit provider home is private process metadata.
pub enum ProviderEnvironment {
    /// Use only the daemon's small, fixed environment allowlist.
    Inherited,
    /// Use one explicitly selected Codex home for authentication and attempt
    /// storage. The path must already identify a canonical directory owned by
    /// the effective user and not writable by group or other users.
    CodexHome(PathBuf),
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("startup input is {actual_bytes} bytes; maximum is {maximum_bytes}")]
    StartupInputTooLarge {
        actual_bytes: usize,
        maximum_bytes: usize,
    },
    #[error("runner executable path {program:?} must be absolute")]
    RunnerPathNotAbsolute { program: PathBuf },
    #[error("{role} executable {program:?} was not found")]
    ExecutableNotFound {
        role: &'static str,
        program: PathBuf,
    },
    #[error("{role} executable {program:?} is not an executable regular file")]
    NotExecutable {
        role: &'static str,
        program: PathBuf,
    },
    #[error("could not spawn factory-runner: {0}")]
    Spawn(std::io::Error),
    #[error("could not prepare factory-runner startup input: {0}")]
    StartupInput(std::io::Error),
    #[error("factory-runner exec gate path already exists")]
    ActivationPathExists,
    #[error("could not activate factory-runner: {0}")]
    Activate(std::io::Error),
    #[error("spawned factory-runner exec gate has no process ID")]
    MissingProcessId,
    #[error("provider environment is invalid")]
    InvalidProviderEnvironment,
    #[error("attempt environment variable {name:?} is not in the allowed set")]
    InvalidAttemptEnvironment { name: String },
}

/// A validated runner command whose locked startup input is durably
/// registerable before any process can exist.
pub struct PreparedRunnerSetup {
    command: Command,
    activation_path: PathBuf,
    setup_path: PathBuf,
    setup_device: u64,
    setup_inode: u64,
}

impl PreparedRunnerSetup {
    #[must_use]
    pub fn setup_path(&self) -> &Path {
        &self.setup_path
    }

    #[must_use]
    pub const fn setup_device(&self) -> u64 {
        self.setup_device
    }

    #[must_use]
    pub const fn setup_inode(&self) -> u64 {
        self.setup_inode
    }

    /// Spawns the inert outer gate only after the caller has durably bound
    /// this setup identity. The locked file is mapped to stdin, so the child
    /// inherits the lease without a separate non-CLOEXEC descriptor race.
    pub fn spawn(mut self) -> Result<PreparedRunnerProcess, Error> {
        let child = self.command.spawn().map_err(Error::Spawn)?;
        if child.id().is_none() {
            return Err(Error::MissingProcessId);
        }
        Ok(PreparedRunnerProcess(PreparedProcess::new(
            child,
            self.activation_path,
        )))
    }
}

/// A stable factory-runner PID that has not executed runner code yet.
///
/// The caller must durably register [`Self::child_pid`] before calling
/// [`Self::activate`]. Dropping this value kills the inert exec gate, so there
/// is no unregistered runner to outlive the daemon.
struct PreparedProcess {
    child: Option<Child>,
    activation_path: PathBuf,
}

impl PreparedProcess {
    fn new(child: Child, activation_path: PathBuf) -> Self {
        Self {
            child: Some(child),
            activation_path,
        }
    }

    /// The exact PID retained when the gate execs the real runner.
    #[must_use]
    pub fn child_pid(&self) -> u32 {
        self.child
            .as_ref()
            .and_then(Child::id)
            .expect("prepared runner process has a PID")
    }

    /// Releases the private activation gate after durable registration.
    ///
    /// The returned handle identifies the same process. It intentionally uses
    /// Tokio's default no-kill-on-drop behavior so the registered runner can
    /// outlive a restarting daemon.
    pub async fn activate(mut self) -> Result<Child, Error> {
        if let Err(error) = release_exec_gate(&self.activation_path) {
            self.kill_and_reap().await;
            return Err(error);
        }
        Ok(self
            .child
            .take()
            .expect("prepared runner process is present"))
    }

    async fn kill_and_reap(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill().await;
            let _ = child.wait().await;
        }
    }
}

impl Drop for PreparedProcess {
    fn drop(&mut self) {
        if let Some(child) = self.child.as_mut() {
            let _ = child.start_kill();
        }
    }
}

/// One durably registerable runner process held behind the shared exec gate.
pub struct PreparedRunnerProcess(PreparedProcess);

impl PreparedRunnerProcess {
    #[must_use]
    pub fn child_pid(&self) -> u32 {
        self.0.child_pid()
    }

    pub async fn activate(self) -> Result<Child, Error> {
        self.0.activate().await
    }

    /// Stops and reaps the inert gate when durable state revoked launch after
    /// setup. Correctness does not depend on this fast path: restart recovery
    /// still waits for the inherited setup lease to become available.
    pub async fn terminate(mut self) {
        self.0.kill_and_reap().await;
    }

    #[cfg(test)]
    pub(crate) fn into_unactivated_child(mut self) -> Child {
        self.0
            .child
            .take()
            .expect("prepared runner process is present")
    }
}

/// One daemon effect process held behind the same register-before-exec gate.
pub struct PreparedEffectProcess(PreparedProcess);

impl PreparedEffectProcess {
    #[must_use]
    pub fn child_pid(&self) -> u32 {
        self.0.child_pid()
    }

    pub async fn activate(self) -> Result<Child, Error> {
        self.0.activate().await
    }
}

/// Closed launch description for a daemon-owned effect subprocess.
pub struct EffectLaunchSpec {
    pub gate_program: PathBuf,
    pub program: PathBuf,
    pub arguments: Vec<OsString>,
    pub environment: Vec<(OsString, OsString)>,
    pub cwd: PathBuf,
    pub activation_path: PathBuf,
}

struct CapturedEnvironment {
    values: Vec<(&'static str, OsString)>,
}

impl CapturedEnvironment {
    fn capture() -> Self {
        Self::from_lookup(|name| env::var_os(name))
    }

    fn from_lookup(mut lookup: impl FnMut(&str) -> Option<OsString>) -> Self {
        let values = SAFE_ENVIRONMENT_NAMES
            .into_iter()
            .filter_map(|name| lookup(name).map(|value| (name, value)))
            .collect();
        Self { values }
    }

    fn value(&self, name: &str) -> Option<&OsStr> {
        self.values
            .iter()
            .find_map(|(candidate, value)| (*candidate == name).then_some(value.as_os_str()))
    }
}

/// Prepares one stable runner PID after checking its trusted absolute path and
/// resolving the provider to a canonical executable file.
///
/// Ambient environment is restricted to `HOME`, `USER`, `LOGNAME`, `SHELL`,
/// `PATH`, `TMPDIR`, `LANG`, `LC_ALL`, and `LC_CTYPE` when present. A validated
/// explicit `CODEX_HOME` is the only provider-specific addition. The child
/// also gets fixed non-interactive values for `NO_COLOR`, `TERM`, and
/// `GIT_TERMINAL_PROMPT`. Task bytes are bounded by [`MAX_STARTUP_STDIN_BYTES`],
/// spooled to a locked owner-only file inside the private runtime before
/// spawn, avoiding both argv/environment disclosure and a pipe-capacity
/// deadlock while the runner is gated.
///
/// The returned setup identity must be durably registered before [`PreparedRunnerSetup::spawn`].
/// Its lock is inherited as the gate's stdin, so restart recovery can prove
/// the absence of a gate that spawned before its PID was registered.
///
/// # Errors
///
/// Returns an error when task bytes are oversized, either executable is
/// missing or unusable, the explicit provider environment is invalid, or the
/// locked startup input cannot be prepared. This function does not spawn.
pub async fn prepare_runner(spec: LaunchSpec) -> Result<PreparedRunnerSetup, Error> {
    prepare_runner_with_environment(spec, CapturedEnvironment::capture()).await
}

/// Resolves one provider program through the same captured, bounded PATH and
/// executable checks used by [`prepare_runner`]. The materializer wrapper
/// receives only this absolute path, never a second ambient lookup.
pub fn resolve_provider_executable(program: &Path) -> Result<PathBuf, Error> {
    resolve_executable(program, &CapturedEnvironment::capture(), "provider")
}

async fn prepare_runner_with_environment(
    spec: LaunchSpec,
    environment: CapturedEnvironment,
) -> Result<PreparedRunnerSetup, Error> {
    if spec.startup_input.len() > MAX_STARTUP_STDIN_BYTES {
        return Err(Error::StartupInputTooLarge {
            actual_bytes: spec.startup_input.len(),
            maximum_bytes: MAX_STARTUP_STDIN_BYTES,
        });
    }
    for (name, _) in &spec.attempt_environment {
        if !ATTEMPT_ENVIRONMENT_NAMES.contains(&name.as_str()) {
            return Err(Error::InvalidAttemptEnvironment { name: name.clone() });
        }
    }

    if !spec.runner_program.is_absolute() {
        return Err(Error::RunnerPathNotAbsolute {
            program: spec.runner_program,
        });
    }
    let runner = checked_executable(&spec.runner_program, "runner")?;
    let provider = resolve_executable(&spec.provider_program, &environment, "provider")?;
    let provider_environment = resolve_provider_environment(&spec.provider_environment)?;
    let activation_path = spec.runtime_dir.join("runner-exec.activate");
    ensure_exec_gate_absent(&activation_path)?;
    let setup_path = spec.runtime_dir.join(RUNNER_STARTUP_LEASE_FILE);
    let startup_input = prepare_startup_input(&setup_path, &spec.startup_input)?;
    let setup_metadata = startup_input.metadata().map_err(Error::StartupInput)?;
    let cwd = spec.cwd;
    let source_root = spec.source_root;
    let expected_parent = rustix::process::getpid();
    let mut command = Command::new(&runner);
    // The runner is its own process-group leader: an attempt must outlive
    // whatever started the daemon. Without this, launchd's default
    // behaviour on `bootout`/`kickstart -k` (and a terminal's Ctrl-C) kills
    // every process in the daemon's group -- every runner and, with it,
    // every attempt -- which `ARCHITECTURE.md`'s durable-resource invariant forbids. Verified
    // on macOS: a same-group child dies with the job, a group leader of its
    // own survives it (see docs/development/WORKFLOW.md, "Release and
    // update" process fixture).
    command.process_group(0);
    command
        .arg("--exec-gate")
        .arg(&activation_path)
        .arg("--expected-parent-pid")
        .arg(expected_parent.as_raw_nonzero().get().to_string())
        .arg("--")
        .arg(&runner)
        .arg("--run-id")
        .arg(spec.run_id.as_str())
        .arg("--runner-instance-id")
        .arg(spec.runner_instance_id.as_str())
        .arg("--runtime-dir")
        .arg(spec.runtime_dir)
        .arg("--cwd")
        .arg(&cwd);
    command
        .arg("--stdin-bytes")
        .arg(spec.startup_input.len().to_string());
    command
        .arg("--")
        .arg(provider)
        .args(spec.provider_arguments)
        .stdin(startup_input);
    apply_runner_environment(
        &mut command,
        &environment,
        &spec.factoryctl_path,
        &source_root,
    );
    apply_provider_environment(&mut command, provider_environment.as_deref());
    for (name, value) in &spec.attempt_environment {
        command.env(name, value);
    }

    Ok(PreparedRunnerSetup {
        command,
        activation_path,
        setup_path,
        setup_device: setup_metadata.dev(),
        setup_inode: setup_metadata.ino(),
    })
}

/// Prepares a daemon-owned effect without allowing its program to execute.
///
/// The caller supplies the complete, closed environment and must durably
/// register [`PreparedEffectProcess::child_pid`] before activation. The gate
/// is its own process-group leader, so all descendants remain under one exact
/// finalizer-owned group.
pub async fn prepare_effect(spec: EffectLaunchSpec) -> Result<PreparedEffectProcess, Error> {
    if !spec.gate_program.is_absolute() {
        return Err(Error::RunnerPathNotAbsolute {
            program: spec.gate_program,
        });
    }
    let gate = checked_executable(&spec.gate_program, "effect gate")?;
    let program = checked_executable(&spec.program, "effect")?;
    ensure_exec_gate_absent(&spec.activation_path)?;
    let expected_parent = rustix::process::getpid();
    let mut command = Command::new(&gate);
    command.process_group(0);
    command
        .arg("--exec-gate")
        .arg(&spec.activation_path)
        .arg("--expected-parent-pid")
        .arg(expected_parent.as_raw_nonzero().get().to_string())
        .arg("--")
        .arg(program)
        .args(spec.arguments)
        .current_dir(spec.cwd)
        .env_clear()
        .envs(spec.environment)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let child = command.spawn().map_err(Error::Spawn)?;
    if child.id().is_none() {
        return Err(Error::MissingProcessId);
    }
    Ok(PreparedEffectProcess(PreparedProcess::new(
        child,
        spec.activation_path,
    )))
}

#[cfg(test)]
async fn spawn_runner_with_environment(
    spec: LaunchSpec,
    environment: CapturedEnvironment,
) -> Result<Child, Error> {
    prepare_runner_with_environment(spec, environment)
        .await?
        .spawn()?
        .activate()
        .await
}

fn ensure_exec_gate_absent(path: &Path) -> Result<(), Error> {
    match fs::symlink_metadata(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Ok(_) => Err(Error::ActivationPathExists),
        Err(error) => Err(Error::Activate(error)),
    }
}

fn release_exec_gate(path: &Path) -> Result<(), Error> {
    use std::fs::OpenOptions;

    let file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .map_err(Error::Activate)?;
    file.sync_all().map_err(Error::Activate)
}

fn prepare_startup_input(path: &Path, input: &[u8]) -> Result<fs::File, Error> {
    let mut file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .map_err(Error::StartupInput)?;
    file.set_permissions(fs::Permissions::from_mode(0o600))
        .map_err(Error::StartupInput)?;
    rustix::fs::flock(&file, FlockOperation::LockExclusive)
        .map_err(|error| Error::StartupInput(error.into()))?;
    file.write_all(input).map_err(Error::StartupInput)?;
    file.sync_all().map_err(Error::StartupInput)?;
    file.seek(std::io::SeekFrom::Start(0))
        .map_err(Error::StartupInput)?;
    Ok(file)
}

fn apply_runner_environment(
    command: &mut Command,
    environment: &CapturedEnvironment,
    factoryctl_path: &Path,
    source_root: &Path,
) {
    command.env_clear();
    for (name, value) in &environment.values {
        command.env(name, value);
    }
    if let Some(directory) = factoryctl_path
        .parent()
        .filter(|directory| !directory.as_os_str().is_empty())
    {
        command.env(
            "PATH",
            prepend_to_path(directory, environment.value("PATH")),
        );
    }
    command.env("NO_COLOR", "1").env("TERM", "dumb");
    // Attempts may inspect and edit only their daemon-owned source. Stop
    // ordinary repository discovery at that exact `.git`-free root, reset
    // credential helpers, disable SSH, hide gh configuration, and forbid
    // prompts. Ambient Git locator variables are already removed by
    // `env_clear` above.
    command
        .env("GIT_CEILING_DIRECTORIES", source_root)
        .env("GIT_DISCOVERY_ACROSS_FILESYSTEM", "0")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_ASKPASS", "/usr/bin/false")
        .env("GIT_SSH_COMMAND", "/usr/bin/false")
        .env("GIT_CONFIG_COUNT", "2")
        .env("GIT_CONFIG_KEY_0", "credential.helper")
        .env("GIT_CONFIG_VALUE_0", "")
        .env("GIT_CONFIG_KEY_1", "core.sshCommand")
        .env("GIT_CONFIG_VALUE_1", "/usr/bin/false")
        .env("GH_CONFIG_DIR", "/dev/null");
}

/// Prepends `directory` to a `PATH` value, keeping any existing entries
/// after it so ambient tools (`git`, `sh`, ...) stay resolvable. Falls back
/// to `directory` alone if joining somehow fails (an entry containing the
/// platform's path-list separator, practically unreachable for a
/// daemon-controlled directory).
fn prepend_to_path(directory: &Path, existing: Option<&OsStr>) -> OsString {
    let mut entries = vec![directory.to_path_buf()];
    if let Some(existing) = existing {
        entries.extend(env::split_paths(existing));
    }
    env::join_paths(entries).unwrap_or_else(|_| directory.as_os_str().to_owned())
}

fn apply_provider_environment(command: &mut Command, codex_home: Option<&Path>) {
    if let Some(codex_home) = codex_home {
        command.env("CODEX_HOME", codex_home);
    }
}

fn resolve_provider_environment(
    environment: &ProviderEnvironment,
) -> Result<Option<PathBuf>, Error> {
    match environment {
        ProviderEnvironment::Inherited => Ok(None),
        ProviderEnvironment::CodexHome(home) => {
            // Reject `.`/`..` navigation outright rather than trying to
            // prove any particular occurrence is harmless (defense in
            // depth against a path shaped to look like one directory while
            // actually landing somewhere else): `home` must already be in
            // lexically normal form. This is deliberately *not* the same
            // check as "already equals its own canonicalized form" --
            // found manually against a real attempt (this track's item 6
            // check): a `home` under `$DARK_FACTORY_HOME=/tmp/...` legally
            // and lexically normal, yet never byte-equal to its
            // canonicalized form, because `/tmp` itself is a symlink to
            // `/private/tmp` on macOS. That is an ancestor directory the
            // operating system manages, not anything a provider or an
            // attacker chose; rejecting it here made every Codex attempt
            // spawn fail whenever the daemon's own state lived under such
            // a path.
            if has_dot_or_dot_dot_component(home) {
                return Err(Error::InvalidProviderEnvironment);
            }
            let metadata =
                fs::symlink_metadata(home).map_err(|_| Error::InvalidProviderEnvironment)?;
            let canonical =
                fs::canonicalize(home).map_err(|_| Error::InvalidProviderEnvironment)?;
            if metadata.file_type().is_symlink()
                || !is_owned_directory(&metadata, rustix::process::geteuid().as_raw())
            {
                return Err(Error::InvalidProviderEnvironment);
            }
            Ok(Some(canonical))
        }
    }
}

/// Whether `path` has a literal `.` or `..` path component -- checked
/// lexically, not by resolving anything on disk.
fn has_dot_or_dot_dot_component(path: &Path) -> bool {
    path.components().any(|component| {
        matches!(
            component,
            std::path::Component::CurDir | std::path::Component::ParentDir
        )
    })
}

fn is_owned_directory(metadata: &fs::Metadata, expected_uid: u32) -> bool {
    metadata.is_dir()
        && metadata.uid() == expected_uid
        && metadata.permissions().mode() & 0o022 == 0
}

fn resolve_executable(
    program: &Path,
    environment: &CapturedEnvironment,
    role: &'static str,
) -> Result<PathBuf, Error> {
    if program.as_os_str().as_bytes().contains(&b'/') {
        return checked_executable(program, role);
    }

    let Some(path) = environment.value("PATH") else {
        return Err(Error::ExecutableNotFound {
            role,
            program: program.to_owned(),
        });
    };
    let mut unusable = None;
    for directory in env::split_paths(path) {
        let candidate = directory.join(program);
        match checked_executable(&candidate, role) {
            Ok(executable) => return Ok(executable),
            Err(Error::NotExecutable { .. }) => unusable = Some(candidate),
            Err(Error::ExecutableNotFound { .. }) => {}
            Err(error) => return Err(error),
        }
    }
    if let Some(program) = unusable {
        Err(Error::NotExecutable { role, program })
    } else {
        Err(Error::ExecutableNotFound {
            role,
            program: program.to_owned(),
        })
    }
}

/// Confirms `program` (a trusted absolute path -- no `PATH` search, unlike
/// [`resolve_executable`]) resolves to an executable regular file. `pub`
/// (not just crate-visible) so `factoryd`'s own startup preflight
/// (`main.rs`) can run the exact same check against `factory-runner`/
/// `factoryctl` before claiming any daemon state, turning a would-be
/// silent spawn failure at first task delivery into a refusal to start
/// (this track's item 2).
pub fn checked_executable(program: &Path, role: &'static str) -> Result<PathBuf, Error> {
    let canonical = match fs::canonicalize(program) {
        Ok(path) => path,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(Error::ExecutableNotFound {
                role,
                program: program.to_owned(),
            });
        }
        Err(_) => {
            return Err(Error::NotExecutable {
                role,
                program: program.to_owned(),
            });
        }
    };
    let metadata = fs::metadata(&canonical).map_err(|_| Error::NotExecutable {
        role,
        program: program.to_owned(),
    })?;
    if !metadata.is_file() || metadata.permissions().mode() & 0o111 == 0 {
        return Err(Error::NotExecutable {
            role,
            program: program.to_owned(),
        });
    }
    Ok(canonical)
}

#[cfg(test)]
mod tests {
    use std::{
        env,
        ffi::OsString,
        fs,
        os::unix::fs::PermissionsExt,
        path::{Path, PathBuf},
        process::Stdio,
        time::{Duration, Instant},
    };

    use factory_core::{RunId, RunnerInstanceId, runner::MAX_STARTUP_STDIN_BYTES};
    use rustix::fs::FlockOperation;
    use rustix::process::{Pid, test_kill_process};
    use tokio::process::Command;

    use super::{
        CapturedEnvironment, EffectLaunchSpec, Error, LaunchSpec, ProviderEnvironment,
        SAFE_ENVIRONMENT_NAMES, apply_runner_environment, has_dot_or_dot_dot_component,
        is_owned_directory, prepare_effect, prepare_runner_with_environment, resolve_executable,
        spawn_runner_with_environment,
    };

    fn id<T>(value: &str) -> T
    where
        T: TryFrom<String>,
        T::Error: std::fmt::Debug,
    {
        T::try_from(value.to_owned()).unwrap()
    }

    fn environment(path: OsString, temporary: &Path) -> CapturedEnvironment {
        CapturedEnvironment::from_lookup(|name| match name {
            "HOME" => Some(OsString::from("/safe/home")),
            "USER" => Some(OsString::from("safe-user")),
            "SHELL" => Some(OsString::from("/bin/sh")),
            "PATH" => Some(path.clone()),
            "TMPDIR" => Some(temporary.as_os_str().to_owned()),
            "LANG" => Some(OsString::from("C")),
            "LC_ALL" => Some(OsString::from("C")),
            "LC_CTYPE" => Some(OsString::from("C")),
            "OPENAI_API_KEY" => Some(OsString::from("openai-secret-sentinel")),
            "ANTHROPIC_API_KEY" => Some(OsString::from("anthropic-secret-sentinel")),
            "CODEX_ACCESS_TOKEN" => Some(OsString::from("codex-secret-sentinel")),
            "CODEX_HOME" => Some(OsString::from("/ambient/codex-home-secret")),
            "CLAUDE_CODE_OAUTH_TOKEN" => Some(OsString::from("claude-secret-sentinel")),
            "GOOGLE_API_KEY" => Some(OsString::from("google-secret-sentinel")),
            "VERCEL_TOKEN" => Some(OsString::from("vercel-secret-sentinel")),
            "DARK_FACTORY_TASK" => Some(OsString::from("private task sentinel")),
            "SSH_AUTH_SOCK" => Some(OsString::from("/secret/agent.sock")),
            "HTTPS_PROXY" => Some(OsString::from("https://secret-proxy.example")),
            "DYLD_INSERT_LIBRARIES" => Some(OsString::from("/secret/library.dylib")),
            "LD_PRELOAD" => Some(OsString::from("/secret/library.so")),
            _ => None,
        })
    }

    fn probe_environment(bin: &Path, temporary: &Path) -> CapturedEnvironment {
        let path = env::join_paths([bin, Path::new("/usr/bin"), Path::new("/bin")]).unwrap();
        environment(path, temporary)
    }

    fn executable(path: &Path, source: &str) {
        fs::write(path, source).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
    }

    fn scripts(directory: &Path) {
        executable(
            &directory.join("runner-probe"),
            r#"#!/bin/sh
set -eu
if [ "${1:-}" = "--exec-gate" ]; then
    gate_path=$2
    shift 2
    [ "${1:-}" = "--expected-parent-pid" ] || exit 125
    expected_parent=$2
    shift 2
    [ "${1:-}" = "--" ] || exit 125
    shift
    [ "$PPID" = "$expected_parent" ] || exit 0
    printf '%s\n' "$$" > "$TMPDIR/outer-gate-pid"
    while [ ! -e "$gate_path" ]; do
        [ "$PPID" = "$expected_parent" ] || exit 0
        sleep 0.01
    done
    rm "$gate_path"
    exec "$@"
fi
: > "$TMPDIR/runner-argv"
for item in "$@"; do
    printf '%s\n' "$item" >> "$TMPDIR/runner-argv"
done
while [ "$#" -gt 0 ]; do
    case "$1" in
        --run-id|--runner-instance-id|--runtime-dir|--cwd|--stdin-bytes) shift 2 ;;
        --) shift; break ;;
        *) exit 64 ;;
    esac
done
exec "$@"
"#,
        );
        executable(
            &directory.join("provider-probe"),
            r#"#!/bin/sh
set -eu
cat > "$TMPDIR/provider-stdin"
env > "$TMPDIR/provider-env"
printf '%s\n' "$@" > "$TMPDIR/provider-argv"
"#,
        );
    }

    fn spec(directory: &Path, task: Vec<u8>) -> LaunchSpec {
        fs::create_dir_all(directory.join("runtime")).unwrap();
        LaunchSpec {
            runner_program: directory.join("runner-probe"),
            factoryctl_path: directory.join("factoryctl-probe"),
            provider_program: PathBuf::from("provider-probe"),
            provider_arguments: vec![OsString::from("--safe-provider-flag")],
            provider_environment: ProviderEnvironment::Inherited,
            attempt_environment: Vec::new(),
            run_id: id::<RunId>("run-safe-launch"),
            runner_instance_id: id::<RunnerInstanceId>("runner-safe-launch"),
            runtime_dir: directory.join("runtime"),
            cwd: directory.to_owned(),
            source_root: directory.to_owned(),
            startup_input: task,
        }
    }

    #[tokio::test]
    async fn environment_is_default_deny_with_fixed_overrides() {
        assert_eq!(
            SAFE_ENVIRONMENT_NAMES,
            [
                "HOME", "USER", "LOGNAME", "SHELL", "PATH", "TMPDIR", "LANG", "LC_ALL", "LC_CTYPE",
            ]
        );
        let directory = tempfile::tempdir().unwrap();
        let captured = environment(OsString::from("/usr/bin:/bin"), directory.path());
        let env_program = resolve_executable(Path::new("env"), &captured, "test").unwrap();
        let mut command = Command::new(env_program);
        command
            .env("OPENAI_API_KEY", "openai-secret-sentinel")
            .env("ANTHROPIC_API_KEY", "anthropic-secret-sentinel")
            .env("CODEX_ACCESS_TOKEN", "codex-secret-sentinel")
            .env("CODEX_HOME", "/ambient/codex-home-secret")
            .env("CLAUDE_CODE_OAUTH_TOKEN", "claude-secret-sentinel")
            .env("GOOGLE_API_KEY", "google-secret-sentinel")
            .env("VERCEL_TOKEN", "vercel-secret-sentinel")
            .env("DARK_FACTORY_TASK", "private task sentinel")
            .env("SSH_AUTH_SOCK", "/secret/agent.sock")
            .env("HTTPS_PROXY", "https://secret-proxy.example")
            .env("DYLD_INSERT_LIBRARIES", "/secret/library.dylib")
            .env("LD_PRELOAD", "/secret/library.so")
            .stdout(Stdio::piped());

        apply_runner_environment(
            &mut command,
            &captured,
            Path::new("/opt/dark-factory/bin/factoryctl"),
            Path::new("/opt/dark-factory/changes/change-1"),
        );
        let output = command.output().await.unwrap();
        assert!(output.status.success());
        let output = String::from_utf8(output.stdout).unwrap();

        assert!(output.lines().any(|line| line == "HOME=/safe/home"));
        assert!(output.lines().any(|line| line == "USER=safe-user"));
        assert!(output.lines().any(|line| line == "SHELL=/bin/sh"));
        assert!(
            output
                .lines()
                .any(|line| line == "GIT_CEILING_DIRECTORIES=/opt/dark-factory/changes/change-1")
        );
        assert!(
            output
                .lines()
                .any(|line| line == "PATH=/opt/dark-factory/bin:/usr/bin:/bin")
        );
        assert!(
            output
                .lines()
                .any(|line| line == format!("TMPDIR={}", directory.path().display()))
        );
        assert!(output.lines().any(|line| line == "LANG=C"));
        assert!(output.lines().any(|line| line == "LC_ALL=C"));
        assert!(output.lines().any(|line| line == "LC_CTYPE=C"));
        assert!(output.lines().any(|line| line == "NO_COLOR=1"));
        assert!(output.lines().any(|line| line == "TERM=dumb"));
        assert!(output.lines().any(|line| line == "GIT_TERMINAL_PROMPT=0"));
        assert!(
            output
                .lines()
                .any(|line| line == "GIT_ASKPASS=/usr/bin/false")
        );
        assert!(
            output
                .lines()
                .any(|line| line == "GIT_SSH_COMMAND=/usr/bin/false")
        );
        assert!(
            output
                .lines()
                .any(|line| line == "GIT_CONFIG_KEY_0=credential.helper")
        );
        assert!(output.lines().any(|line| line == "GIT_CONFIG_VALUE_0="));
        assert!(output.lines().any(|line| line == "GH_CONFIG_DIR=/dev/null"));
        assert!(!output.contains("GOOGLE_API_KEY"));
        assert!(!output.contains("OPENAI_API_KEY"));
        assert!(!output.contains("ANTHROPIC_API_KEY"));
        assert!(!output.contains("CODEX_ACCESS_TOKEN"));
        assert!(!output.contains("CODEX_HOME"));
        assert!(!output.contains("CLAUDE_CODE_OAUTH_TOKEN"));
        assert!(!output.contains("openai-secret-sentinel"));
        assert!(!output.contains("anthropic-secret-sentinel"));
        assert!(!output.contains("codex-secret-sentinel"));
        assert!(!output.contains("claude-secret-sentinel"));
        assert!(!output.contains("VERCEL_TOKEN"));
        assert!(!output.contains("DARK_FACTORY_TASK"));
        assert!(!output.contains("private task sentinel"));
        assert!(!output.contains("SSH_AUTH_SOCK"));
        assert!(!output.contains("HTTPS_PROXY"));
        assert!(!output.contains("DYLD_INSERT_LIBRARIES"));
        assert!(!output.contains("LD_PRELOAD"));
        assert!(
            !output.contains("LOGNAME="),
            "missing safe names stay absent"
        );
    }

    #[tokio::test]
    async fn explicit_codex_home_is_canonical_owned_and_replaces_ambient_value() {
        let directory = tempfile::tempdir().unwrap();
        scripts(directory.path());
        let home = directory.path().join("codex-home");
        fs::create_dir(&home).unwrap();
        fs::set_permissions(&home, fs::Permissions::from_mode(0o755)).unwrap();
        let home = fs::canonicalize(home).unwrap();
        let captured = probe_environment(directory.path(), directory.path());
        let mut launch = spec(directory.path(), Vec::new());
        launch.provider_environment = ProviderEnvironment::CodexHome(home.clone());

        let mut child = spawn_runner_with_environment(launch, captured)
            .await
            .unwrap();
        assert!(child.wait().await.unwrap().success());

        let provider_env = fs::read_to_string(directory.path().join("provider-env")).unwrap();
        let expected = format!("CODEX_HOME={}", home.display());
        assert!(provider_env.lines().any(|line| line == expected));
        assert!(!provider_env.contains("/ambient/codex-home-secret"));
    }

    /// Regression test (found manually against a real attempt, this
    /// track's item 6 check): a `CodexHome` under an *ancestor* symlink
    /// (not a symlink itself) must still be accepted, using its
    /// canonicalized form -- `$DARK_FACTORY_HOME=/tmp/...` is exactly this
    /// shape on macOS, where `/tmp` is itself a symlink to `/private/tmp`.
    /// Every real Codex attempt spawn failed closed with "provider
    /// environment is invalid" before this was fixed, because the home
    /// path (though never touched by an attacker, always daemon-
    /// constructed, and never itself a symlink) could never equal its own
    /// canonicalized form while any ancestor was a symlink.
    #[tokio::test]
    async fn codex_home_under_a_symlinked_ancestor_directory_is_accepted_and_canonicalized() {
        let directory = tempfile::tempdir().unwrap();
        scripts(directory.path());
        let real_root = directory.path().join("real-root");
        fs::create_dir(&real_root).unwrap();
        let symlinked_root = directory.path().join("symlinked-root");
        std::os::unix::fs::symlink(&real_root, &symlinked_root).unwrap();
        let home_under_symlink = symlinked_root.join("codex-home");
        fs::create_dir(&home_under_symlink).unwrap();
        fs::set_permissions(&home_under_symlink, fs::Permissions::from_mode(0o755)).unwrap();
        let canonical_home = fs::canonicalize(&home_under_symlink).unwrap();
        assert_ne!(
            canonical_home, home_under_symlink,
            "the test setup itself must exercise a genuine ancestor-symlink mismatch"
        );

        let captured = probe_environment(directory.path(), directory.path());
        let mut launch = spec(directory.path(), Vec::new());
        launch.provider_environment = ProviderEnvironment::CodexHome(home_under_symlink);

        let mut child = spawn_runner_with_environment(launch, captured)
            .await
            .unwrap();
        assert!(child.wait().await.unwrap().success());

        let provider_env = fs::read_to_string(directory.path().join("provider-env")).unwrap();
        let expected = format!("CODEX_HOME={}", canonical_home.display());
        assert!(provider_env.lines().any(|line| line == expected));
    }

    #[test]
    fn dot_dot_components_are_detected_lexically() {
        assert!(!has_dot_or_dot_dot_component(Path::new("/a/b/c")));
        assert!(has_dot_or_dot_dot_component(Path::new("/a/../b")));
        assert!(has_dot_or_dot_dot_component(Path::new("/a/b/..")));
        // `Path::components()` normalizes away a mid-path `.` entirely
        // (it is never observable as a `CurDir` component for an absolute
        // path) -- inert either way, since `/a/./b` and `/a/b` already
        // name the same directory with no symlink resolution involved.
        assert!(!has_dot_or_dot_dot_component(Path::new("/a/./b")));
    }

    #[tokio::test]
    async fn invalid_codex_home_fails_closed_before_spawn_and_is_redacted() {
        let directory = tempfile::tempdir().unwrap();
        scripts(directory.path());
        let missing = directory.path().join("PRIVATE_MISSING_HOME_SECRET");
        let home = directory.path().join("PRIVATE_REAL_HOME_SECRET");
        fs::create_dir(&home).unwrap();
        let alias = directory.path().join("PRIVATE_SYMLINK_HOME_SECRET");
        std::os::unix::fs::symlink(&home, &alias).unwrap();
        let noncanonical = home.join("..").join("PRIVATE_REAL_HOME_SECRET");
        let writable = directory.path().join("PRIVATE_WRITABLE_HOME_SECRET");
        fs::create_dir(&writable).unwrap();
        fs::set_permissions(&writable, fs::Permissions::from_mode(0o777)).unwrap();
        let not_directory = directory.path().join("PRIVATE_FILE_HOME_SECRET");
        fs::write(&not_directory, b"not a home").unwrap();

        for rejected in [missing, alias, noncanonical, writable, not_directory] {
            let captured = probe_environment(directory.path(), directory.path());
            let mut launch = spec(directory.path(), b"PRIVATE_TASK_SECRET".to_vec());
            launch.provider_environment = ProviderEnvironment::CodexHome(rejected.clone());
            let error = spawn_runner_with_environment(launch, captured)
                .await
                .unwrap_err();
            let message = error.to_string();
            assert!(message.contains("provider environment"));
            assert!(!message.contains(rejected.to_string_lossy().as_ref()));
            assert!(!message.contains("PRIVATE_"));
            assert!(!directory.path().join("runner-argv").exists());
        }
    }

    #[tokio::test]
    async fn attempt_environment_is_applied_on_top_of_the_safe_allowlist() {
        let directory = tempfile::tempdir().unwrap();
        scripts(directory.path());
        let captured = probe_environment(directory.path(), directory.path());
        let mut launch = spec(directory.path(), Vec::new());
        launch.attempt_environment = vec![
            ("DARK_FACTORY_AGENT".to_owned(), "curie".to_owned()),
            (
                "DARK_FACTORY_ATTEMPT_TOKEN_FILE".to_owned(),
                "/private/runs/session-1/hook.token".to_owned(),
            ),
        ];

        let mut child = spawn_runner_with_environment(launch, captured)
            .await
            .unwrap();
        assert!(child.wait().await.unwrap().success());

        let provider_env = fs::read_to_string(directory.path().join("provider-env")).unwrap();
        assert!(
            provider_env
                .lines()
                .any(|line| line == "DARK_FACTORY_AGENT=curie")
        );
        assert!(provider_env.lines().any(|line| {
            line == "DARK_FACTORY_ATTEMPT_TOKEN_FILE=/private/runs/session-1/hook.token"
        }));
        assert!(
            !provider_env
                .lines()
                .any(|line| line.starts_with("DARK_FACTORY_AGENT_DIR="))
        );
    }

    #[tokio::test]
    async fn attempt_environment_rejects_a_name_outside_the_fixed_allowlist() {
        let directory = tempfile::tempdir().unwrap();
        scripts(directory.path());
        let captured = probe_environment(directory.path(), directory.path());
        let mut launch = spec(directory.path(), Vec::new());
        launch.attempt_environment = vec![("PATH".to_owned(), "/evil/bin".to_owned())];

        let error = spawn_runner_with_environment(launch, captured)
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            Error::InvalidAttemptEnvironment { name } if name == "PATH"
        ));
        assert!(!directory.path().join("runner-argv").exists());
    }

    #[tokio::test]
    async fn prepends_factoryctls_directory_to_path() {
        let directory = tempfile::tempdir().unwrap();
        let captured = environment(OsString::from("/usr/bin:/bin"), directory.path());
        let env_program = resolve_executable(Path::new("env"), &captured, "test").unwrap();
        let mut command = Command::new(env_program);
        command.stdout(Stdio::piped());

        apply_runner_environment(
            &mut command,
            &captured,
            Path::new("/opt/dark-factory/bin/factoryctl"),
            Path::new("/opt/dark-factory/changes/change-1"),
        );
        let output = command.output().await.unwrap();
        assert!(output.status.success());
        let output = String::from_utf8(output.stdout).unwrap();
        assert!(
            output
                .lines()
                .any(|line| line == "PATH=/opt/dark-factory/bin:/usr/bin:/bin"),
            "{output}"
        );
        assert!(output.lines().any(|line| line == "TERM=dumb"));
    }

    #[test]
    fn codex_home_must_be_owned_by_the_effective_user() {
        let directory = tempfile::tempdir().unwrap();
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o755)).unwrap();
        let metadata = fs::metadata(directory.path()).unwrap();
        let effective_uid = rustix::process::geteuid().as_raw();

        assert!(is_owned_directory(&metadata, effective_uid));
        assert!(!is_owned_directory(
            &metadata,
            effective_uid.wrapping_add(1)
        ));
    }

    fn macos_var_alias_tempdir() -> Option<tempfile::TempDir> {
        if !cfg!(target_os = "macos") {
            return None;
        }
        tempfile::Builder::new()
            .prefix("dark-factory-runner-alias-")
            .tempdir_in("/var/tmp")
            .ok()
    }

    fn linux_tmp_tempdir() -> tempfile::TempDir {
        tempfile::Builder::new()
            .prefix("dark-factory-runner-tmp-")
            .tempdir_in("/tmp")
            .expect("the Linux-style /tmp runner fixture must be writable")
    }

    async fn assert_prompt_is_exact_stdin_and_never_runner_argv_or_environment(
        directory: tempfile::TempDir,
        require_macos_alias: bool,
    ) {
        scripts(directory.path());
        let captured = probe_environment(directory.path(), directory.path());
        let task = b"private task \xf0\x9f\x8f\xad\nwith spaces\0and bytes".to_vec();

        let mut child =
            spawn_runner_with_environment(spec(directory.path(), task.clone()), captured)
                .await
                .unwrap();
        assert!(child.wait().await.unwrap().success());

        assert_eq!(
            fs::read(directory.path().join("provider-stdin")).unwrap(),
            task
        );
        let runner_argv = fs::read(directory.path().join("runner-argv")).unwrap();
        let provider_argv = fs::read(directory.path().join("provider-argv")).unwrap();
        let provider_env = fs::read(directory.path().join("provider-env")).unwrap();
        for observed in [&runner_argv, &provider_argv, &provider_env] {
            assert!(!observed.windows(12).any(|bytes| bytes == b"private task"));
        }
        assert!(!provider_env.windows(10).any(|bytes| bytes == b"CODEX_HOME"));
        assert!(
            !provider_env
                .windows("DARK_FACTORY_TASK".len())
                .any(|bytes| bytes == b"DARK_FACTORY_TASK")
        );
        let runner_argv = String::from_utf8(runner_argv).unwrap();
        assert!(runner_argv.contains("--stdin-bytes\n39\n"));
        assert!(runner_argv.contains("--run-id\nrun-safe-launch\n"));
        assert!(runner_argv.contains("--runner-instance-id\nrunner-safe-launch\n"));
        assert!(runner_argv.contains("--safe-provider-flag"));
        let lexical_provider = directory.path().join("provider-probe");
        let expected_provider = fs::canonicalize(&lexical_provider).unwrap();
        let runner_tokens: Vec<_> = runner_argv.lines().collect();
        assert!(runner_tokens.contains(&expected_provider.to_str().unwrap()));
        assert_eq!(provider_argv, b"--safe-provider-flag\n");
        if require_macos_alias {
            assert!(lexical_provider.starts_with("/var/"));
            assert!(expected_provider.starts_with("/private/var/"));
            assert_ne!(lexical_provider, expected_provider);
            assert!(!runner_tokens.contains(&lexical_provider.to_str().unwrap()));
        }
    }

    #[tokio::test]
    async fn prompt_is_exact_stdin_and_never_runner_argv_or_environment_under_linux_tmp() {
        assert_prompt_is_exact_stdin_and_never_runner_argv_or_environment(
            linux_tmp_tempdir(),
            false,
        )
        .await;
    }

    #[tokio::test]
    async fn prompt_is_exact_stdin_and_never_runner_argv_or_environment_under_macos_var_alias() {
        let Some(directory) = macos_var_alias_tempdir() else {
            assert!(
                !cfg!(target_os = "macos"),
                "macOS /var alias fixture must be writable when running on macOS"
            );
            return;
        };
        assert_prompt_is_exact_stdin_and_never_runner_argv_or_environment(directory, true).await;
    }

    #[tokio::test]
    async fn oversized_multibyte_prompt_is_rejected_before_spawn_without_echoing_it() {
        let directory = tempfile::tempdir().unwrap();
        scripts(directory.path());
        let captured = probe_environment(directory.path(), directory.path());
        let mut task = "£".repeat(MAX_STARTUP_STDIN_BYTES / 2);
        task.push('£');

        let error = spawn_runner_with_environment(
            spec(directory.path(), task.as_bytes().to_vec()),
            captured,
        )
        .await
        .unwrap_err();

        let message = error.to_string();
        assert!(message.contains("1048576"));
        assert!(!message.contains('£'));
        assert!(!directory.path().join("runner-argv").exists());
    }

    #[test]
    fn path_resolution_canonicalizes_and_rejects_unusable_files() {
        let directory = tempfile::tempdir().unwrap();
        let bin = directory.path().join("bin with spaces");
        fs::create_dir(&bin).unwrap();
        let executable_path = bin.join("works target");
        executable(&executable_path, "#!/bin/sh\nexit 0\n");
        let executable_link = bin.join("works");
        std::os::unix::fs::symlink(&executable_path, &executable_link).unwrap();
        let non_executable = bin.join("not-executable");
        fs::write(&non_executable, "not executable").unwrap();
        fs::set_permissions(&non_executable, fs::Permissions::from_mode(0o600)).unwrap();
        let captured = environment(OsString::from(bin.as_os_str()), directory.path());

        assert_eq!(
            resolve_executable(Path::new("works"), &captured, "provider").unwrap(),
            fs::canonicalize(executable_path).unwrap()
        );
        assert!(
            resolve_executable(Path::new("not-executable"), &captured, "provider")
                .unwrap_err()
                .to_string()
                .contains("executable regular file")
        );
        assert!(
            resolve_executable(Path::new("missing"), &captured, "provider")
                .unwrap_err()
                .to_string()
                .contains("not found")
        );
    }

    #[tokio::test]
    async fn both_executables_resolve_before_the_runner_is_spawned() {
        let directory = tempfile::tempdir().unwrap();
        executable(
            &directory.path().join("runner-probe"),
            "#!/bin/sh\nprintf spawned > \"$TMPDIR/spawned\"\n",
        );
        let captured = probe_environment(directory.path(), directory.path());
        let mut launch = spec(directory.path(), b"secret prompt sentinel".to_vec());
        launch.provider_program = PathBuf::from("missing-provider");

        let error = spawn_runner_with_environment(launch, captured)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("not found"));
        assert!(!error.to_string().contains("secret prompt sentinel"));
        assert!(!directory.path().join("spawned").exists());
    }

    #[tokio::test]
    async fn runner_path_must_be_absolute_and_is_never_resolved_from_path() {
        let directory = tempfile::tempdir().unwrap();
        executable(
            &directory.path().join("runner-probe"),
            "#!/bin/sh\nprintf counterfeit > \"$TMPDIR/counterfeit\"\n",
        );
        executable(
            &directory.path().join("provider-probe"),
            "#!/bin/sh\nexit 0\n",
        );

        for runner in ["runner-probe", "./runner-probe"] {
            let captured = probe_environment(directory.path(), directory.path());
            let mut launch = spec(directory.path(), b"runner path secret".to_vec());
            launch.runner_program = PathBuf::from(runner);
            let error = spawn_runner_with_environment(launch, captured)
                .await
                .unwrap_err();
            assert!(error.to_string().contains("absolute"));
            assert!(!error.to_string().contains("runner path secret"));
        }
        assert!(!directory.path().join("counterfeit").exists());
    }

    #[tokio::test]
    async fn exact_startup_limit_is_accepted() {
        let directory = tempfile::tempdir().unwrap();
        scripts(directory.path());
        let captured = probe_environment(directory.path(), directory.path());
        let task = vec![b'x'; MAX_STARTUP_STDIN_BYTES];

        let mut child = spawn_runner_with_environment(spec(directory.path(), task), captured)
            .await
            .unwrap();
        assert!(child.wait().await.unwrap().success());
        assert_eq!(
            fs::metadata(directory.path().join("provider-stdin"))
                .unwrap()
                .len(),
            u64::try_from(MAX_STARTUP_STDIN_BYTES).unwrap()
        );
    }

    #[tokio::test]
    async fn maximum_startup_input_is_spooled_without_waiting_for_activation() {
        let directory = tempfile::tempdir().unwrap();
        scripts(directory.path());
        let captured = probe_environment(directory.path(), directory.path());
        let prepared = tokio::time::timeout(
            Duration::from_secs(2),
            prepare_runner_with_environment(
                spec(directory.path(), vec![b'x'; MAX_STARTUP_STDIN_BYTES]),
                captured,
            ),
        )
        .await
        .expect("preparation must not block on a gated stdin pipe")
        .unwrap();
        assert!(!directory.path().join("runner-argv").exists());
        assert!(!directory.path().join("provider-stdin").exists());

        let mut child = prepared.spawn().unwrap().activate().await.unwrap();
        assert!(child.wait().await.unwrap().success());
        assert_eq!(
            fs::metadata(directory.path().join("provider-stdin"))
                .unwrap()
                .len(),
            u64::try_from(MAX_STARTUP_STDIN_BYTES).unwrap()
        );
    }

    #[tokio::test]
    async fn dropping_prepared_runner_kills_gate_without_launching_runner() {
        let directory = tempfile::tempdir().unwrap();
        scripts(directory.path());
        let captured = probe_environment(directory.path(), directory.path());
        let prepared = prepare_runner_with_environment(
            spec(directory.path(), b"private task".to_vec()),
            captured,
        )
        .await
        .unwrap()
        .spawn()
        .unwrap();
        let child_pid = prepared.child_pid();
        let pid = Pid::from_raw(i32::try_from(child_pid).unwrap()).unwrap();
        let marker = directory.path().join("outer-gate-pid");
        let deadline = Instant::now() + Duration::from_secs(2);
        while !marker.exists() && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert_eq!(
            fs::read_to_string(marker)
                .unwrap()
                .trim()
                .parse::<u32>()
                .unwrap(),
            child_pid
        );

        drop(prepared);
        let deadline = Instant::now() + Duration::from_secs(2);
        while test_kill_process(pid).is_ok() && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert!(test_kill_process(pid).is_err());
        assert!(!directory.path().join("runner-argv").exists());
        assert!(!directory.path().join("provider-stdin").exists());
    }

    #[tokio::test]
    async fn startup_lease_is_inherited_by_the_spawned_exec_gate() {
        let directory = tempfile::tempdir().unwrap();
        scripts(directory.path());
        let captured = probe_environment(directory.path(), directory.path());
        let setup = prepare_runner_with_environment(
            spec(directory.path(), b"private task".to_vec()),
            captured,
        )
        .await
        .unwrap();
        let lease_path = setup.setup_path().to_owned();
        let contender = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&lease_path)
            .unwrap();
        assert_eq!(
            rustix::fs::flock(&contender, FlockOperation::NonBlockingLockExclusive),
            Err(rustix::io::Errno::AGAIN),
            "a separately opened descriptor must not share the setup lock"
        );

        let prepared = setup.spawn().unwrap();
        assert_eq!(
            rustix::fs::flock(&contender, FlockOperation::NonBlockingLockExclusive),
            Err(rustix::io::Errno::AGAIN),
            "the child must inherit the lock after the parent command is consumed"
        );
        prepared.terminate().await;
        rustix::fs::flock(&contender, FlockOperation::NonBlockingLockExclusive)
            .expect("the setup lock must become available only after the gate is reaped");
    }

    #[tokio::test]
    async fn activation_retains_the_exact_prepared_pid() {
        let directory = tempfile::tempdir().unwrap();
        scripts(directory.path());
        let captured = probe_environment(directory.path(), directory.path());
        let prepared =
            prepare_runner_with_environment(spec(directory.path(), Vec::new()), captured)
                .await
                .unwrap()
                .spawn()
                .unwrap();
        let prepared_pid = prepared.child_pid();

        let mut child = prepared.activate().await.unwrap();
        assert_eq!(child.id(), Some(prepared_pid));
        assert!(child.wait().await.unwrap().success());
        assert!(directory.path().join("runner-argv").exists());
    }

    #[tokio::test]
    async fn effect_process_cannot_run_before_its_exact_activation() {
        let directory = tempfile::tempdir().unwrap();
        scripts(directory.path());
        let activation = directory.path().join("effect.activate");
        let marker = directory.path().join("effect-ran");
        let prepared = prepare_effect(EffectLaunchSpec {
            gate_program: directory.path().join("runner-probe"),
            program: PathBuf::from("/bin/sh"),
            arguments: vec![
                OsString::from("-c"),
                OsString::from(format!("printf ran > {}", marker.display())),
            ],
            environment: vec![(
                OsString::from("TMPDIR"),
                directory.path().as_os_str().to_owned(),
            )],
            cwd: directory.path().to_owned(),
            activation_path: activation,
        })
        .await
        .unwrap();
        let prepared_pid = prepared.child_pid();
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(!marker.exists());

        let mut child = prepared.activate().await.unwrap();
        assert_eq!(child.id(), Some(prepared_pid));
        assert!(child.wait().await.unwrap().success());
        assert_eq!(fs::read_to_string(marker).unwrap(), "ran");
    }

    #[tokio::test]
    async fn dropping_returned_child_does_not_kill_it() {
        let directory = tempfile::tempdir().unwrap();
        scripts(directory.path());
        executable(
            &directory.path().join("provider-probe"),
            "#!/bin/sh\nprintf ready > \"$TMPDIR/ready\"\nwhile [ ! -e \"$TMPDIR/release\" ]; do sleep 0.01; done\nprintf survived > \"$TMPDIR/survived.tmp\"\nmv \"$TMPDIR/survived.tmp\" \"$TMPDIR/survived\"\n",
        );
        let captured = probe_environment(directory.path(), directory.path());
        let mut child = spawn_runner_with_environment(spec(directory.path(), Vec::new()), captured)
            .await
            .unwrap();

        let ready = directory.path().join("ready");
        let deadline = Instant::now() + Duration::from_secs(5);
        while !ready.exists() && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        if !ready.exists() {
            let _ = child.kill().await;
            let _ = child.wait().await;
            panic!("provider did not reach the pre-drop barrier");
        }
        drop(child);
        tokio::time::sleep(Duration::from_millis(100)).await;
        fs::write(directory.path().join("release"), b"").unwrap();

        let marker = directory.path().join("survived");
        let deadline = Instant::now() + Duration::from_secs(5);
        while !marker.exists() && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(fs::read_to_string(marker).unwrap(), "survived");
    }
}
