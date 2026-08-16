use std::{env, error::Error, ffi::OsString, io, path::PathBuf, time::Duration};

use factoryd::{
    execution,
    lifecycle::{DaemonInstance, shutdown_signal},
    local_api::{ApiState, serve},
    store::Store,
};
use tokio::{net::UnixListener, task::JoinHandle};

const DEFAULT_MAX_ACTIVE_RUNS: usize = 4;
const STARTUP_TIMEOUT: Duration = Duration::from_secs(15);
const CONNECT_GRACE: Duration = Duration::from_secs(5);
const BATCH_DELAY: Duration = Duration::from_millis(25);

#[derive(Debug, Eq, PartialEq)]
struct Config {
    database: PathBuf,
    socket: PathBuf,
    runner: PathBuf,
    codex: PathBuf,
    runtime_root: PathBuf,
    max_active_runs: usize,
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
    let (execution, mut execution_join) = execution::spawn(
        execution::Config {
            runner_program: config.runner,
            codex_program: config.codex,
            runtime_root: config.runtime_root,
            max_active_runs: config.max_active_runs,
            startup_timeout: STARTUP_TIMEOUT,
            connect_grace: CONNECT_GRACE,
            batch_delay: BATCH_DELAY,
        },
        state.clone(),
    )?;
    let (listener, socket_cleanup) = instance.bind_socket()?;
    listener.set_nonblocking(true)?;
    let listener = UnixListener::from_std(listener)?;
    tracing::info!(
        database = %instance.database_path().display(),
        socket = %instance.socket_path().display(),
        "factory daemon ready"
    );

    let api = serve(listener, state, execution.clone(), async {
        match shutdown_signal().await {
            Ok(()) => tracing::info!("shutdown requested"),
            Err(error) => tracing::error!(%error, "could not listen for shutdown"),
        }
    });
    tokio::pin!(api);
    let result = tokio::select! {
        result = &mut api => {
            stop_execution(execution, execution_join).await?;
            result
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
        runtime_root: home.join("runs"),
        max_active_runs: DEFAULT_MAX_ACTIVE_RUNS,
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
            Some("-h" | "--help") => {
                println!(
                    "factoryd [--database PATH] [--socket PATH] [--runner PATH] [--codex PATH] [--runtime-root PATH] [--max-active-runs N]"
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
            runtime_root: PathBuf::from("/state/runs"),
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
    fn execution_paths_and_capacity_are_explicit_and_bounded() {
        let parsed = parse_arguments(
            config(),
            args(&[
                "--runner",
                "/opt/dark-factory/factory-runner",
                "--codex",
                "/opt/codex",
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
        assert_eq!(parsed.runtime_root, PathBuf::from("/private/runs"));
        assert_eq!(parsed.max_active_runs, 2);

        assert_eq!(
            parse_arguments(config(), args(&["--max-active-runs", "0"])).unwrap_err(),
            "--max-active-runs requires a positive integer"
        );
    }
}
