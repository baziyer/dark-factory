use std::{env, error::Error, ffi::OsStr, ffi::OsString, io, path::PathBuf, sync::Arc};

use factory_core::local::RequestCredential;
use factoryd::{
    execution,
    lifecycle::{DaemonInstance, ShutdownSignals},
    local_api::{ApiState, serve},
    providers::hooks,
    store::Store,
    webhook_http::{WebhookHttpMetrics, WebhookServer, bind_webhooks, load_webhook_config},
};
use tokio::{net::UnixListener, sync::watch, task::JoinHandle};

const DEFAULT_MAX_ACTIVE_RUNS: usize = 4;

#[derive(Eq, PartialEq)]
struct Config {
    factoryd: PathBuf,
    database: PathBuf,
    socket: PathBuf,
    runner: PathBuf,
    factoryctl: PathBuf,
    git: PathBuf,
    runtime_root: PathBuf,
    changes_root: PathBuf,
    /// `$DARK_FACTORY_HOME`: root of the project/agent guidance tree (see
    /// `factory_core::paths`).
    guidance_root: PathBuf,
    max_active_runs: usize,
    webhook_config: Option<PathBuf>,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
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

    let webhook_metrics = config
        .webhook_config
        .as_ref()
        .map(|_| Arc::new(WebhookHttpMetrics::default()));
    let webhooks = match (config.webhook_config, webhook_metrics.as_ref()) {
        (Some(path), Some(metrics)) => Some(
            bind_webhooks(
                state.clone(),
                load_webhook_config(&path)?,
                Arc::clone(metrics),
            )
            .await?,
        ),
        (None, None) => None,
        _ => return Err("invalid webhook configuration state".into()),
    };
    let webhooks_enabled = webhooks.is_some();
    let webhooks_bind = webhooks.as_ref().map(WebhookServer::local_addr);
    let guidance_root = config.guidance_root.clone();
    let (execution, mut execution_join) = execution::spawn(
        execution::Config {
            factoryd_program: config.factoryd,
            runner_program: config.runner,
            factoryctl_path: config.factoryctl,
            git_program: config.git,
            runtime_root: config.runtime_root,
            changes_root: config.changes_root,
            guidance_root: config.guidance_root,
            socket_path: instance.socket_path().to_path_buf(),
            max_active_runs: config.max_active_runs,
        },
        state.clone(),
    )?;
    tracing::info!(
        database = %instance.database_path().display(),
        socket = %instance.socket_path().display(),
        webhooks_enabled,
        webhooks_bind = webhooks_bind.map(|bind| bind.to_string()),
        "factory daemon ready"
    );
    if let Some(bind) = webhooks_bind {
        tracing::info!(target: "factoryd.webhook", %bind, "webhooks enabled");
    } else {
        tracing::info!(
            target: "factoryd.webhook",
            "webhooks disabled: no --webhook-config and no $DARK_FACTORY_HOME/webhooks.json"
        );
    }

    let control_planes = serve_control_planes(
        listener,
        state,
        execution.clone(),
        guidance_root,
        operator_credential,
        webhooks,
        shutdown,
    );
    tokio::pin!(control_planes);
    let result = tokio::select! {
        result = &mut control_planes => {
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
    if let Some(metrics) = webhook_metrics {
        let metrics = metrics.snapshot();
        tracing::info!(
            target: "factoryd.webhook",
            event = "listener_stopped",
            authenticated_requests = metrics.authenticated_requests,
            rate_limited_requests = metrics.rate_limited_requests,
            bounded_rejections = metrics.bounded_rejections
        );
    }
    result?;
    Ok(())
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

async fn serve_control_planes(
    listener: UnixListener,
    state: ApiState,
    execution: execution::Handle,
    guidance_root: PathBuf,
    operator_credential: RequestCredential,
    webhooks: Option<WebhookServer>,
    shutdown: ShutdownSignals,
) -> io::Result<()> {
    let (stop_tx, stop_rx) = watch::channel(false);
    let local = serve(
        listener,
        state,
        execution,
        guidance_root,
        operator_credential,
        wait_for_stop(stop_rx.clone()),
    );
    let web = serve_optional_webhooks(webhooks, stop_rx);
    tokio::pin!(local);
    tokio::pin!(web);

    enum Completed {
        Shutdown,
        Local(io::Result<()>),
        Webhooks(io::Result<()>),
    }

    let completed = tokio::select! {
        () = shutdown.recv() => Completed::Shutdown,
        result = &mut local => Completed::Local(result),
        result = &mut web => Completed::Webhooks(result),
    };
    let _ = stop_tx.send(true);
    match completed {
        Completed::Shutdown => {
            let (local, web) = tokio::join!(local, web);
            local?;
            web?;
            tracing::info!("shutdown requested");
            Ok(())
        }
        Completed::Local(result) => {
            let web = web.await;
            result?;
            web
        }
        Completed::Webhooks(result) => {
            let local = local.await;
            result?;
            local
        }
    }
}

async fn serve_optional_webhooks(
    webhooks: Option<WebhookServer>,
    shutdown: watch::Receiver<bool>,
) -> io::Result<()> {
    match webhooks {
        Some(server) => server
            .serve(wait_for_stop(shutdown))
            .await
            .map_err(|_| io::Error::other("webhook HTTP listener failed")),
        None => {
            wait_for_stop(shutdown).await;
            Ok(())
        }
    }
}

async fn wait_for_stop(mut shutdown: watch::Receiver<bool>) {
    if *shutdown.borrow() {
        return;
    }
    while shutdown.changed().await.is_ok() {
        if *shutdown.borrow() {
            return;
        }
    }
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
    let default_webhook_config = home.join("webhooks.json");
    let config = Config {
        factoryd: current_executable,
        database: home.join("factory.db"),
        socket: env::var_os("DARK_FACTORY_SOCKET")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join("f.sock")),
        runner,
        factoryctl,
        git: resolve_executable_on_path("git")?,
        runtime_root: home.join("runs"),
        changes_root: home.join("changes"),
        guidance_root: home,
        max_active_runs: DEFAULT_MAX_ACTIVE_RUNS,
        webhook_config: None,
    };
    let mut config = parse_arguments(config, env::args_os().skip(1))?;
    // Webhooks are on by default: if `$DARK_FACTORY_HOME/webhooks.json`
    // exists, load it without requiring `--webhook-config`. An explicit
    // `--webhook-config PATH` overrides this default.
    config.webhook_config = resolve_webhook_config(
        config.webhook_config,
        default_webhook_config.clone(),
        default_webhook_config.is_file(),
    );
    Ok(config)
}

fn resolve_executable_on_path(name: &str) -> Result<PathBuf, Box<dyn Error>> {
    let path = env::var_os("PATH").ok_or("PATH is not set")?;
    resolve_executable_in_path(name, &path).map_err(Into::into)
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

fn resolve_webhook_config(
    explicit: Option<PathBuf>,
    default_path: PathBuf,
    default_exists: bool,
) -> Option<PathBuf> {
    explicit.or_else(|| default_exists.then_some(default_path))
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
            Some("--webhook-config") => {
                if config.webhook_config.is_some() {
                    return Err("--webhook-config may only be provided once".into());
                }
                config.webhook_config = Some(next_path(&mut arguments, "--webhook-config")?);
            }
            Some("-h" | "--help") => {
                println!(
                    "factoryd [--database PATH] [--socket PATH] [--runner PATH] [--factoryctl PATH] [--runtime-root PATH] [--max-active-runs N] [--webhook-config PATH]"
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

    use super::{Config, parse_arguments, resolve_executable_in_path, resolve_webhook_config};

    fn config() -> Config {
        Config {
            factoryd: PathBuf::from("/bin/factoryd"),
            database: PathBuf::from("/state/factory.db"),
            socket: PathBuf::from("/state/f.sock"),
            runner: PathBuf::from("/bin/factory-runner"),
            factoryctl: PathBuf::from("/bin/factoryctl"),
            git: PathBuf::from("/usr/bin/git"),
            runtime_root: PathBuf::from("/state/runs"),
            changes_root: PathBuf::from("/state/changes"),
            guidance_root: PathBuf::from("/state"),
            max_active_runs: 4,
            webhook_config: None,
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
        assert_eq!(parsed.max_active_runs, 2);
        assert!(parsed.webhook_config.is_none());

        assert_eq!(
            parse_arguments(config(), args(&["--max-active-runs", "0"]))
                .err()
                .unwrap(),
            "--max-active-runs requires a positive integer"
        );
    }

    #[test]
    fn webhook_config_is_one_explicit_path() {
        let parsed = parse_arguments(
            config(),
            args(&["--webhook-config", "/state/webhooks.json"]),
        )
        .unwrap();
        assert_eq!(
            parsed.webhook_config,
            Some(PathBuf::from("/state/webhooks.json"))
        );

        let duplicate = parse_arguments(
            config(),
            args(&[
                "--webhook-config",
                "/state/one.json",
                "--webhook-config",
                "/state/two.json",
            ]),
        )
        .err()
        .unwrap();
        assert_eq!(duplicate, "--webhook-config may only be provided once");
    }

    #[test]
    fn webhooks_default_on_when_the_home_config_file_exists_but_defer_to_an_explicit_path() {
        let default_path = PathBuf::from("/state/webhooks.json");
        assert_eq!(
            resolve_webhook_config(None, default_path.clone(), true),
            Some(default_path.clone())
        );
        assert_eq!(
            resolve_webhook_config(None, default_path.clone(), false),
            None
        );
        assert_eq!(
            resolve_webhook_config(
                Some(PathBuf::from("/explicit/webhooks.json")),
                default_path,
                true
            ),
            Some(PathBuf::from("/explicit/webhooks.json"))
        );
    }
}
