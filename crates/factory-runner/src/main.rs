use std::{env, io::Read, path::PathBuf, process};

use factory_core::{RunId, RunnerInstanceId};
use factory_runner::{Config, Error, MAX_STARTUP_STDIN_BYTES};

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let result = match parse_arguments() {
        Ok(config) => run_config(config).await,
        Err(error) => Err(error),
    };
    if let Err(error) = result {
        eprintln!("factory-runner: {error}");
        process::exit(1);
    }
}

async fn run_config(config: Config) -> Result<(), Error> {
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let mut interrupt = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    let signal_task = tokio::spawn(async move {
        tokio::select! {
            _ = terminate.recv() => {}
            _ = interrupt.recv() => {}
        }
        let _ = shutdown_tx.send(true);
    });
    let result = factory_runner::run_with_shutdown(config, shutdown_rx).await;
    signal_task.abort();
    let _ = signal_task.await;
    result
}

fn parse_arguments() -> Result<Config, Error> {
    let mut arguments = env::args().skip(1);
    let mut run_id = None;
    let mut runner_instance_id = None;
    let mut runtime_dir = None;
    let mut cwd = None;
    let mut stdin_bytes = None;
    let mut program = None;
    let mut agent_arguments = Vec::new();

    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--run-id" => run_id = Some(required(&mut arguments, "--run-id")?),
            "--runner-instance-id" => {
                runner_instance_id = Some(required(&mut arguments, "--runner-instance-id")?);
            }
            "--runtime-dir" => {
                runtime_dir = Some(PathBuf::from(required(&mut arguments, "--runtime-dir")?));
            }
            "--cwd" => cwd = Some(PathBuf::from(required(&mut arguments, "--cwd")?)),
            "--stdin-bytes" => {
                let value = required(&mut arguments, "--stdin-bytes")?;
                stdin_bytes = Some(value.parse::<usize>().map_err(|_| {
                    Error::InvalidArguments("--stdin-bytes must be a non-negative integer".into())
                })?);
            }
            "--" => {
                program = arguments.next().map(PathBuf::from);
                agent_arguments.extend(arguments);
                break;
            }
            _ => {
                return Err(Error::InvalidArguments(format!(
                    "unknown runner argument {argument:?}"
                )));
            }
        }
    }

    let run_id = RunId::try_from(required_value(run_id, "--run-id")?)
        .map_err(|error| Error::InvalidArguments(error.to_string()))?;
    let runner_instance_id =
        RunnerInstanceId::try_from(required_value(runner_instance_id, "--runner-instance-id")?)
            .map_err(|error| Error::InvalidArguments(error.to_string()))?;
    let runtime_dir = required_value(runtime_dir, "--runtime-dir")?;
    let cwd = required_value(cwd, "--cwd")?;
    let program = required_value(program, "agent program after --")?;
    let startup_input = read_startup_input(stdin_bytes)?;
    Ok(Config {
        run_id,
        runner_instance_id,
        runtime_dir,
        cwd,
        startup_input,
        program,
        arguments: agent_arguments,
    })
}

fn read_startup_input(declared_bytes: Option<usize>) -> Result<Option<Vec<u8>>, Error> {
    let Some(declared_bytes) = declared_bytes else {
        return Ok(None);
    };
    if declared_bytes > MAX_STARTUP_STDIN_BYTES {
        return Err(Error::InvalidArguments(format!(
            "--stdin-bytes exceeds the {MAX_STARTUP_STDIN_BYTES}-byte limit"
        )));
    }

    let mut input = vec![0; declared_bytes];
    let mut stdin = std::io::stdin().lock();
    stdin.read_exact(&mut input).map_err(|error| {
        Error::InvalidArguments(format!(
            "stdin ended before the declared {declared_bytes} bytes: {error}"
        ))
    })?;
    let mut extra = [0_u8; 1];
    if stdin.read(&mut extra)? != 0 {
        return Err(Error::InvalidArguments(
            "stdin contained bytes beyond --stdin-bytes".into(),
        ));
    }
    Ok(Some(input))
}

fn required(arguments: &mut impl Iterator<Item = String>, option: &str) -> Result<String, Error> {
    arguments
        .next()
        .ok_or_else(|| Error::InvalidArguments(format!("{option} requires a value")))
}

fn required_value<T>(value: Option<T>, name: &str) -> Result<T, Error> {
    value.ok_or_else(|| Error::InvalidArguments(format!("missing {name}")))
}
