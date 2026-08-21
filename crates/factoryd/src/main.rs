use std::{
    env,
    error::Error,
    ffi::OsStr,
    ffi::OsString,
    fs, io,
    os::unix::fs::{MetadataExt as _, PermissionsExt as _},
    path::{Path, PathBuf},
    thread,
    time::Duration,
};

use factory_core::local::RequestCredential;
use factoryd::{
    execution,
    lifecycle::{DaemonInstance, ShutdownSignals},
    local_api::{ApiState, serve},
    providers::hooks,
    store::Store,
};
use tokio::{net::UnixListener, task::JoinHandle};

const DEFAULT_MAX_ACTIVE_RUNS: usize = 4;

#[derive(Eq, PartialEq)]
struct Config {
    factoryd: PathBuf,
    database: PathBuf,
    socket: PathBuf,
    runner: PathBuf,
    factoryctl: PathBuf,
    git: PathBuf,
    cargo: Option<PathBuf>,
    runtime_root: PathBuf,
    changes_root: PathBuf,
    artifacts_root: PathBuf,
    /// `$DARK_FACTORY_HOME`: root of the project/agent guidance tree (see
    /// `factory_core::paths`).
    guidance_root: PathBuf,
    max_active_runs: usize,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    if let Some(worker) = rust_worker_invocation()? {
        return run_rust_worker(worker);
    }
    if let Some(path) = materializer_invocation_path()? {
        return match factoryd::run_change_materializer(&path) {
            Ok(never) => match never {},
            Err(error) => Err(error.into()),
        };
    }
    tracing_subscriber::fmt()
        .with_target(false)
        .compact()
        .init();
    let config = parse_config()?;
    preflight_sibling_binaries(&config)?;
    let instance = DaemonInstance::claim(&config.database, &config.socket)?;
    let store = Store::open(instance.database_path())?;
    let state = ApiState::new(store);
    let operator_credential = RequestCredential::new(hooks::read_or_create_operator_token(
        &config.guidance_root.join("operator.token"),
    )?)?;
    let shutdown = ShutdownSignals::install()?;
    let (listener, socket_cleanup) = instance.bind_socket()?;
    listener.set_nonblocking(true)?;
    let listener = UnixListener::from_std(listener)?;

    let guidance_root = config.guidance_root.clone();
    let (execution, mut execution_join) = execution::spawn(
        execution::Config {
            factoryd_program: config.factoryd,
            runner_program: config.runner,
            factoryctl_path: config.factoryctl,
            git_program: config.git,
            cargo_program: config.cargo,
            runtime_root: config.runtime_root,
            changes_root: config.changes_root,
            artifacts_root: config.artifacts_root,
            guidance_root: config.guidance_root,
            socket_path: instance.socket_path().to_path_buf(),
            max_active_runs: config.max_active_runs,
        },
        state.clone(),
    )?;
    tracing::info!(
        database = %instance.database_path().display(),
        socket = %instance.socket_path().display(),
        "factory daemon ready"
    );

    let control_plane = serve(
        listener,
        state,
        execution.clone(),
        guidance_root,
        operator_credential,
        shutdown.recv(),
    );
    tokio::pin!(control_plane);
    let result = tokio::select! {
        result = &mut control_plane => {
            let stopped = stop_execution(execution, execution_join).await;
            result?;
            stopped
        }
        result = &mut execution_join => {
            match result {
                Ok(Ok(())) => Err(io::Error::other("execution manager stopped unexpectedly")),
                Ok(Err(_)) | Err(_) => Err(io::Error::other("execution manager failed")),
            }
        }
    };
    if let Err(error) = socket_cleanup.remove() {
        tracing::warn!(%error, socket = %instance.socket_path().display(), "could not remove socket");
    }
    result?;
    Ok(())
}

struct RustWorkerInvocation {
    invocation: PathBuf,
    result: PathBuf,
    finish: PathBuf,
    expected_parent_pid: u32,
}

fn rust_worker_invocation() -> Result<Option<RustWorkerInvocation>, Box<dyn Error>> {
    let mut arguments = env::args_os().skip(1);
    let Some(first) = arguments.next() else {
        return Ok(None);
    };
    if first != "--rust-verify-worker" {
        return Ok(None);
    }
    let invocation = next_hidden_path(&mut arguments, "invocation")?;
    let result = next_hidden_path(&mut arguments, "result")?;
    let finish = next_hidden_path(&mut arguments, "finish")?;
    let expected_parent_pid = arguments
        .next()
        .and_then(|value| value.to_str().and_then(|value| value.parse::<u32>().ok()))
        .filter(|value| *value > 0)
        .ok_or("--rust-verify-worker requires an expected parent PID")?;
    if arguments.next().is_some() {
        return Err("--rust-verify-worker accepts exactly four values".into());
    }
    Ok(Some(RustWorkerInvocation {
        invocation,
        result,
        finish,
        expected_parent_pid,
    }))
}

fn next_hidden_path(
    arguments: &mut impl Iterator<Item = OsString>,
    name: &str,
) -> Result<PathBuf, Box<dyn Error>> {
    arguments
        .next()
        .map(PathBuf::from)
        .filter(|path| !path.as_os_str().is_empty())
        .ok_or_else(|| format!("--rust-verify-worker requires a {name} path").into())
}

fn run_rust_worker(worker: RustWorkerInvocation) -> Result<(), Box<dyn Error>> {
    let expected_parent = rustix::process::Pid::from_raw(worker.expected_parent_pid as i32)
        .ok_or("invalid Rust worker parent PID")?;
    if rustix::process::getppid() != Some(expected_parent) {
        return Err("Rust worker parent identity changed before execution".into());
    }
    let finish_watch = worker.finish.clone();
    thread::spawn(move || {
        loop {
            if rust_worker_must_terminate(&finish_watch, expected_parent) {
                terminate_own_process_group();
            }
            thread::sleep(Duration::from_millis(50));
        }
    });
    if factoryd::run_rust_verifier_worker(&worker.invocation, &worker.result).is_err() {
        terminate_own_process_group();
    }
    loop {
        if rust_worker_must_terminate(&worker.finish, expected_parent) {
            terminate_own_process_group();
        }
        thread::sleep(Duration::from_millis(50));
    }
}

fn rust_worker_must_terminate(finish: &Path, expected_parent: rustix::process::Pid) -> bool {
    finish.exists() || rustix::process::getppid() != Some(expected_parent)
}

fn terminate_own_process_group() -> ! {
    let _ = rustix::process::kill_process_group(
        rustix::process::getpid(),
        rustix::process::Signal::KILL,
    );
    std::process::abort()
}

fn materializer_invocation_path() -> Result<Option<PathBuf>, Box<dyn Error>> {
    let mut arguments = env::args_os().skip(1);
    let Some(first) = arguments.next() else {
        return Ok(None);
    };
    if first != "--materialize-change" {
        return Ok(None);
    }
    let path = arguments
        .next()
        .map(PathBuf::from)
        .filter(|path| !path.as_os_str().is_empty())
        .ok_or("--materialize-change requires one invocation path")?;
    if arguments.next().is_some() {
        return Err("--materialize-change accepts exactly one invocation path".into());
    }
    Ok(Some(path))
}

async fn stop_execution(
    execution: execution::Handle,
    join: JoinHandle<Result<(), execution::Error>>,
) -> io::Result<()> {
    execution
        .shutdown()
        .await
        .map_err(|_| io::Error::other("execution manager could not shut down"))?;
    join.await
        .map_err(|_| io::Error::other("execution manager task failed"))?
        .map_err(|_| io::Error::other("execution manager failed during shutdown"))
}

fn parse_config() -> Result<Config, Box<dyn Error>> {
    let home = factory_home()?;
    let current_executable = env::current_exe()?;
    let sibling_dir = current_executable
        .parent()
        .ok_or("factoryd executable has no parent directory")?;
    let runner = sibling_dir.join("factory-runner");
    let factoryctl = sibling_dir.join("factoryctl");
    let config = Config {
        factoryd: current_executable,
        database: home.join("factory.db"),
        socket: env::var_os("DARK_FACTORY_SOCKET")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join("f.sock")),
        runner,
        factoryctl,
        git: resolve_executable_on_path("git")?,
        cargo: resolve_cargo_on_path().ok(),
        runtime_root: home.join("runs"),
        changes_root: home.join("changes"),
        artifacts_root: home.join("artifacts"),
        guidance_root: home,
        max_active_runs: DEFAULT_MAX_ACTIVE_RUNS,
    };
    parse_arguments(config, env::args_os().skip(1)).map_err(Into::into)
}

fn resolve_executable_on_path(name: &str) -> Result<PathBuf, Box<dyn Error>> {
    let path = env::var_os("PATH").ok_or("PATH is not set")?;
    resolve_executable_in_path(name, &path).map_err(Into::into)
}

fn resolve_cargo_on_path() -> Result<PathBuf, Box<dyn Error>> {
    let path = env::var_os("PATH").ok_or("PATH is not set")?;
    let rustup_home = env::var_os("RUSTUP_HOME")
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".rustup")));
    let requested_toolchain = env::var("RUSTUP_TOOLCHAIN").ok();
    resolve_cargo_in_path(
        &path,
        rustup_home.as_deref(),
        requested_toolchain.as_deref(),
    )
    .map_err(Into::into)
}

/// Resolves the real fixed Cargo toolchain without executing rustup during
/// daemon startup. A standard `cargo -> rustup` PATH entry is mapped through
/// rustup's bounded settings file to its real Cargo and sibling rustc.
fn resolve_cargo_in_path(
    path: &OsStr,
    rustup_home: Option<&Path>,
    requested_toolchain: Option<&str>,
) -> Result<PathBuf, String> {
    for directory in env::split_paths(path) {
        let candidate = directory.join("cargo");
        let Ok(executable) =
            factoryd::runner_process::checked_executable(&candidate, "Cargo verifier")
        else {
            continue;
        };
        let cargo = if executable.file_name() == Some(OsStr::new("rustup")) {
            let home = rustup_home.ok_or("rustup Cargo requires RUSTUP_HOME or HOME")?;
            let home = fs::canonicalize(home)
                .map_err(|_| "rustup home is missing or cannot be resolved")?;
            verify_owned_toolchain_directory(&home)?;
            let toolchain = match requested_toolchain {
                Some(value) => value.to_owned(),
                None => default_rustup_toolchain(&home.join("settings.toml"))?,
            };
            if !safe_toolchain_name(&toolchain) {
                return Err("rustup toolchain name is invalid".into());
            }
            factoryd::runner_process::checked_executable(
                &home.join("toolchains").join(toolchain).join("bin/cargo"),
                "Cargo verifier",
            )
            .map_err(|error| error.to_string())?
        } else {
            executable
        };
        let parent = cargo
            .parent()
            .ok_or("Cargo verifier has no toolchain directory")?;
        factoryd::runner_process::checked_executable(&parent.join("rustc"), "Rust compiler")
            .map_err(|error| error.to_string())?;
        return Ok(cargo);
    }
    Err("cargo was not found as a usable fixed toolchain on PATH".into())
}

fn verify_owned_toolchain_directory(path: &Path) -> Result<(), String> {
    let metadata = fs::symlink_metadata(path).map_err(|_| "rustup home is unreadable")?;
    if metadata.file_type().is_symlink()
        || !metadata.is_dir()
        || metadata.uid() != rustix::process::geteuid().as_raw()
        || metadata.permissions().mode() & 0o022 != 0
    {
        return Err("rustup home must be an owned, non-writable-by-others directory".into());
    }
    Ok(())
}

fn default_rustup_toolchain(settings: &Path) -> Result<String, String> {
    let metadata =
        fs::symlink_metadata(settings).map_err(|_| "rustup settings are missing or unreadable")?;
    if metadata.file_type().is_symlink() || !metadata.is_file() || metadata.len() > 64 * 1024 {
        return Err("rustup settings are not a bounded regular file".into());
    }
    let content =
        fs::read_to_string(settings).map_err(|_| "rustup settings are not valid UTF-8")?;
    content
        .lines()
        .find_map(|line| {
            line.trim()
                .strip_prefix("default_toolchain")
                .and_then(|value| value.trim_start().strip_prefix('='))
                .map(str::trim)
                .and_then(|value| value.strip_prefix('"'))
                .and_then(|value| value.strip_suffix('"'))
                .map(str::to_owned)
        })
        .filter(|value| safe_toolchain_name(value))
        .ok_or_else(|| "rustup settings have no valid default toolchain".into())
}

fn safe_toolchain_name(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 255
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
        && value != "."
        && value != ".."
}

fn resolve_executable_in_path(name: &str, path: &OsStr) -> Result<PathBuf, String> {
    for directory in env::split_paths(path) {
        let candidate = directory.join(name);
        if let Ok(executable) =
            factoryd::runner_process::checked_executable(&candidate, "source materializer")
        {
            return Ok(executable);
        }
    }
    Err(format!("{name} was not found as an executable on PATH"))
}

fn parse_arguments(
    mut config: Config,
    mut arguments: impl Iterator<Item = OsString>,
) -> Result<Config, String> {
    while let Some(argument) = arguments.next() {
        match argument.to_str() {
            Some("--database") => {
                config.database = next_path(&mut arguments, "--database")?;
            }
            Some("--socket") => {
                config.socket = next_path(&mut arguments, "--socket")?;
            }
            Some("--runner") => {
                config.runner = next_path(&mut arguments, "--runner")?;
            }
            Some("--factoryctl") => {
                config.factoryctl = next_path(&mut arguments, "--factoryctl")?;
            }
            Some("--runtime-root") => {
                config.runtime_root = next_path(&mut arguments, "--runtime-root")?;
            }
            Some("--artifacts-root") => {
                config.artifacts_root = next_path(&mut arguments, "--artifacts-root")?;
            }
            Some("--max-active-runs") => {
                let value = arguments
                    .next()
                    .ok_or("--max-active-runs requires a positive integer")?;
                config.max_active_runs = value
                    .to_str()
                    .and_then(|value| value.parse().ok())
                    .filter(|value| *value > 0)
                    .ok_or("--max-active-runs requires a positive integer")?;
            }
            Some("-h" | "--help") => {
                println!(
                    "factoryd [--database PATH] [--socket PATH] [--runner PATH] [--factoryctl PATH] [--runtime-root PATH] [--artifacts-root PATH] [--max-active-runs N]"
                );
                std::process::exit(0);
            }
            Some("--version" | "-V") => {
                println!("factoryd {}", env!("CARGO_PKG_VERSION"));
                std::process::exit(0);
            }
            _ => return Err(format!("unknown argument: {}", argument.to_string_lossy())),
        }
    }
    Ok(config)
}

fn next_path(
    arguments: &mut impl Iterator<Item = OsString>,
    option: &str,
) -> Result<PathBuf, String> {
    arguments
        .next()
        .map(PathBuf::from)
        .filter(|path| !path.as_os_str().is_empty())
        .ok_or_else(|| format!("{option} requires a path"))
}

fn factory_home() -> Result<PathBuf, Box<dyn Error>> {
    Ok(factory_core::paths::dark_factory_home()?)
}

/// Refuses to start with a one-line actionable error if `factory-runner`
/// or `factoryctl` do not resolve to an executable regular file (this
/// track's item 2): `cargo run -p factoryd` only builds `factoryd`
/// itself, not its sibling binaries (`README.md`'s "get an agent working"
/// walkthrough), so the previous behavior -- start fine, then fail every
/// attempt spawn silently, forever, with no trace outside the daemon's own
/// log -- was exactly the operator footgun this track's item 1 also had to
/// repair defenses around. This is a stricter, cheaper, startup-time
/// version of the same check `runner_process::spawn_runner` runs on every
/// individual spawn attempt (`checked_executable`, shared here rather than
/// duplicated).
fn preflight_sibling_binaries(config: &Config) -> Result<(), String> {
    for (role, path) in [
        ("daemon materializer", &config.factoryd),
        ("runner", &config.runner),
        ("factoryctl", &config.factoryctl),
        ("git source reader", &config.git),
    ] {
        if let Err(error) = factoryd::runner_process::checked_executable(path, role) {
            return Err(format!(
                "{error}; build the workspace: cargo build --workspace"
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::{
        ffi::OsString,
        fs,
        os::unix::fs::{PermissionsExt, symlink},
        path::PathBuf,
    };

    use super::{
        Config, parse_arguments, resolve_cargo_in_path, resolve_executable_in_path,
        rust_worker_must_terminate,
    };

    fn config() -> Config {
        Config {
            factoryd: PathBuf::from("/bin/factoryd"),
            database: PathBuf::from("/state/factory.db"),
            socket: PathBuf::from("/state/f.sock"),
            runner: PathBuf::from("/bin/factory-runner"),
            factoryctl: PathBuf::from("/bin/factoryctl"),
            git: PathBuf::from("/usr/bin/git"),
            cargo: Some(PathBuf::from("/usr/bin/cargo")),
            runtime_root: PathBuf::from("/state/runs"),
            changes_root: PathBuf::from("/state/changes"),
            artifacts_root: PathBuf::from("/state/artifacts"),
            guidance_root: PathBuf::from("/state"),
            max_active_runs: 4,
        }
    }

    fn args(values: &[&str]) -> impl Iterator<Item = OsString> {
        values
            .iter()
            .map(|value| OsString::from(*value))
            .collect::<Vec<_>>()
            .into_iter()
    }

    #[test]
    fn path_resolution_preserves_the_checked_canonical_executable() {
        let current = std::env::current_dir().unwrap();
        let root = tempfile::tempdir_in(&current).unwrap();
        let bin = root.path().join("bin");
        let cellar = root.path().join("cellar");
        fs::create_dir(&bin).unwrap();
        fs::create_dir(&cellar).unwrap();
        let executable = cellar.join("git");
        fs::write(&executable, "#!/bin/sh\n").unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o755)).unwrap();
        symlink("../cellar/git", bin.join("git")).unwrap();
        let relative_bin = bin.strip_prefix(&current).unwrap();
        let path = std::env::join_paths([relative_bin]).unwrap();

        assert_eq!(
            resolve_executable_in_path("git", &path).unwrap(),
            executable.canonicalize().unwrap()
        );
    }

    #[test]
    fn rustup_proxy_resolves_to_one_real_cargo_and_rustc_toolchain() {
        let current = std::env::current_dir().unwrap();
        let root = tempfile::tempdir_in(&current).unwrap();
        let bin = root.path().join("bin");
        let rustup_home = root.path().join("rustup-home");
        let toolchain_bin = rustup_home.join("toolchains/stable-test/bin");
        fs::create_dir(&bin).unwrap();
        fs::create_dir_all(&toolchain_bin).unwrap();
        let rustup = bin.join("rustup");
        fs::write(&rustup, "#!/bin/sh\n").unwrap();
        fs::set_permissions(&rustup, fs::Permissions::from_mode(0o755)).unwrap();
        symlink("rustup", bin.join("cargo")).unwrap();
        for name in ["cargo", "rustc"] {
            let executable = toolchain_bin.join(name);
            fs::write(&executable, "#!/bin/sh\n").unwrap();
            fs::set_permissions(&executable, fs::Permissions::from_mode(0o755)).unwrap();
        }
        fs::write(
            rustup_home.join("settings.toml"),
            "version = \"12\"\ndefault_toolchain = \"stable-test\"\n",
        )
        .unwrap();
        let path = std::env::join_paths([bin]).unwrap();

        assert_eq!(
            resolve_cargo_in_path(&path, Some(&rustup_home), None).unwrap(),
            toolchain_bin.join("cargo").canonicalize().unwrap()
        );
    }

    #[test]
    fn execution_paths_and_capacity_are_explicit_and_bounded() {
        let parsed = parse_arguments(
            config(),
            args(&[
                "--runner",
                "/opt/dark-factory/factory-runner",
                "--factoryctl",
                "/opt/dark-factory/factoryctl",
                "--runtime-root",
                "/private/runs",
                "--artifacts-root",
                "/private/artifacts",
                "--max-active-runs",
                "2",
            ]),
        )
        .unwrap();
        assert_eq!(
            parsed.runner,
            PathBuf::from("/opt/dark-factory/factory-runner")
        );
        assert_eq!(
            parsed.factoryctl,
            PathBuf::from("/opt/dark-factory/factoryctl")
        );
        assert_eq!(parsed.runtime_root, PathBuf::from("/private/runs"));
        assert_eq!(parsed.artifacts_root, PathBuf::from("/private/artifacts"));
        assert_eq!(parsed.max_active_runs, 2);

        assert_eq!(
            parse_arguments(config(), args(&["--max-active-runs", "0"]))
                .err()
                .unwrap(),
            "--max-active-runs requires a positive integer"
        );
    }

    #[test]
    fn rust_worker_finish_signal_requires_group_termination() {
        let current = std::env::current_dir().unwrap();
        let root = tempfile::tempdir_in(&current).unwrap();
        let finish = root.path().join("finish");
        let parent = rustix::process::getppid().unwrap();
        assert!(!rust_worker_must_terminate(&finish, parent));
        fs::write(&finish, b"").unwrap();
        assert!(rust_worker_must_terminate(&finish, parent));
    }
}
