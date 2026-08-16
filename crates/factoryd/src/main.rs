use std::{
    env, error::Error, ffi::OsString, io, num::NonZeroU32, path::PathBuf, sync::Arc, time::Duration,
};

use factoryd::{
    execution,
    lifecycle::{DaemonInstance, shutdown_signal},
    local_api::{ApiState, serve},
    store::Store,
    webhook_http::{WebhookHttpMetrics, WebhookServer, bind_webhooks, load_webhook_config},
};
use tokio::{net::UnixListener, sync::watch, task::JoinHandle};

const DEFAULT_MAX_ACTIVE_RUNS: usize = 4;
const CLAUDE_MAX_TURNS: NonZeroU32 = NonZeroU32::new(20).unwrap();
const CLAUDE_MAX_BUDGET_CENTS: NonZeroU32 = NonZeroU32::new(500).unwrap();
const STARTUP_TIMEOUT: Duration = Duration::from_secs(15);
const CONNECT_GRACE: Duration = Duration::from_secs(5);
const BATCH_DELAY: Duration = Duration::from_millis(25);

#[derive(Eq, PartialEq)]
struct Config {
    database: PathBuf,
    socket: PathBuf,
    runner: PathBuf,
    codex: PathBuf,
    claude: PathBuf,
    runtime_root: PathBuf,
    max_active_runs: usize,
    webhook_config: Option<PathBuf>,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    tracing_subscriber::fmt()
        .with_target(false)
        .compact()
        .init();
    let config = parse_config()?;
    let instance = DaemonInstance::claim(&config.database, &config.socket)?;
    let store = Store::open(instance.database_path())?;
    let state = ApiState::new(store);
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
    let (execution, mut execution_join) = execution::spawn(
        execution::Config {
            runner_program: config.runner,
            codex_program: config.codex,
            claude_program: config.claude,
            claude_max_turns: CLAUDE_MAX_TURNS,
            claude_max_budget_cents: CLAUDE_MAX_BUDGET_CENTS,
            runtime_root: config.runtime_root,
            max_active_runs: config.max_active_runs,
            startup_timeout: STARTUP_TIMEOUT,
            connect_grace: CONNECT_GRACE,
            batch_delay: BATCH_DELAY,
        },
        state.clone(),
    )?;
    tracing::info!(
        database = %instance.database_path().display(),
        socket = %instance.socket_path().display(),
        webhooks_enabled,
        "factory daemon ready"
    );

    let control_planes = serve_control_planes(listener, state, execution.clone(), webhooks);
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

async fn serve_control_planes(
    listener: UnixListener,
    state: ApiState,
    execution: execution::Handle,
    webhooks: Option<WebhookServer>,
) -> io::Result<()> {
    let (stop_tx, stop_rx) = watch::channel(false);
    let local = serve(listener, state, execution, wait_for_stop(stop_rx.clone()));
    let web = serve_optional_webhooks(webhooks, stop_rx);
    tokio::pin!(local);
    tokio::pin!(web);

    enum Completed {
        Shutdown(io::Result<()>),
        Local(io::Result<()>),
        Webhooks(io::Result<()>),
    }

    let completed = tokio::select! {
        result = shutdown_signal() => Completed::Shutdown(result),
        result = &mut local => Completed::Local(result),
        result = &mut web => Completed::Webhooks(result),
    };
    let _ = stop_tx.send(true);
    match completed {
        Completed::Shutdown(signal) => {
            let (local, web) = tokio::join!(local, web);
            local?;
            web?;
            signal?;
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
    let runner = current_executable
        .parent()
        .ok_or("factoryd executable has no parent directory")?
        .join("factory-runner");
    let config = Config {
        database: home.join("factory.db"),
        socket: env::var_os("DARK_FACTORY_SOCKET")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join("f.sock")),
        runner,
        codex: PathBuf::from("codex"),
        claude: PathBuf::from("claude"),
        runtime_root: home.join("runs"),
        max_active_runs: DEFAULT_MAX_ACTIVE_RUNS,
        webhook_config: None,
    };
    parse_arguments(config, env::args_os().skip(1)).map_err(Into::into)
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
            Some("--codex") => {
                config.codex = next_path(&mut arguments, "--codex")?;
            }
            Some("--claude") => {
                config.claude = next_path(&mut arguments, "--claude")?;
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
                    "factoryd [--database PATH] [--socket PATH] [--runner PATH] [--codex PATH] [--claude PATH] [--runtime-root PATH] [--max-active-runs N] [--webhook-config PATH]"
                );
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
    if let Some(path) = env::var_os("DARK_FACTORY_HOME") {
        return Ok(PathBuf::from(path));
    }
    let home = env::var_os("HOME").ok_or("HOME or DARK_FACTORY_HOME must be set")?;
    Ok(PathBuf::from(home).join(".dark-factory"))
}

#[cfg(test)]
mod tests {
    use std::{ffi::OsString, path::PathBuf};

    use super::{Config, parse_arguments};

    fn config() -> Config {
        Config {
            database: PathBuf::from("/state/factory.db"),
            socket: PathBuf::from("/state/f.sock"),
            runner: PathBuf::from("/bin/factory-runner"),
            codex: PathBuf::from("codex"),
            claude: PathBuf::from("claude"),
            runtime_root: PathBuf::from("/state/runs"),
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
    fn execution_paths_and_capacity_are_explicit_and_bounded() {
        let parsed = parse_arguments(
            config(),
            args(&[
                "--runner",
                "/opt/dark-factory/factory-runner",
                "--codex",
                "/opt/codex",
                "--claude",
                "/opt/claude",
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
        assert_eq!(parsed.codex, PathBuf::from("/opt/codex"));
        assert_eq!(parsed.claude, PathBuf::from("/opt/claude"));
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
}
