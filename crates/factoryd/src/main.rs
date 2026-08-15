use std::{env, error::Error, path::PathBuf};

use factoryd::{
    lifecycle::{DaemonInstance, shutdown_signal},
    local_api::{ApiState, serve},
    store::Store,
};
use tokio::net::UnixListener;

struct Config {
    database: PathBuf,
    socket: PathBuf,
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
    let (listener, socket_cleanup) = instance.bind_socket()?;
    listener.set_nonblocking(true)?;
    let listener = UnixListener::from_std(listener)?;
    tracing::info!(
        database = %instance.database_path().display(),
        socket = %instance.socket_path().display(),
        "factory daemon ready"
    );

    let result = serve(listener, ApiState::new(store), async {
        match shutdown_signal().await {
            Ok(()) => tracing::info!("shutdown requested"),
            Err(error) => tracing::error!(%error, "could not listen for shutdown"),
        }
    })
    .await;
    if let Err(error) = socket_cleanup.remove() {
        tracing::warn!(%error, socket = %instance.socket_path().display(), "could not remove socket");
    }
    result?;
    Ok(())
}

fn parse_config() -> Result<Config, Box<dyn Error>> {
    let home = factory_home()?;
    let mut database = home.join("factory.db");
    let mut socket = env::var_os("DARK_FACTORY_SOCKET")
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join("f.sock"));
    let mut arguments = env::args_os().skip(1);

    while let Some(argument) = arguments.next() {
        match argument.to_str() {
            Some("--database") => {
                database = PathBuf::from(arguments.next().ok_or("--database requires a path")?);
            }
            Some("--socket") => {
                socket = PathBuf::from(arguments.next().ok_or("--socket requires a path")?);
            }
            Some("-h" | "--help") => {
                println!("factoryd [--database PATH] [--socket PATH]");
                std::process::exit(0);
            }
            _ => return Err(format!("unknown argument: {}", argument.to_string_lossy()).into()),
        }
    }

    Ok(Config { database, socket })
}

fn factory_home() -> Result<PathBuf, Box<dyn Error>> {
    if let Some(path) = env::var_os("DARK_FACTORY_HOME") {
        return Ok(PathBuf::from(path));
    }
    let home = env::var_os("HOME").ok_or("HOME or DARK_FACTORY_HOME must be set")?;
    Ok(PathBuf::from(home).join(".dark-factory"))
}
