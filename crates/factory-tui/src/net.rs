//! All socket I/O for the board lives here. Everything runs on background threads and reports
//! back to the render loop over an `mpsc` channel (`NetMsg`) — the render loop never blocks on
//! the daemon, and the daemon connection is retried with backoff rather than crashing the UI.
//!
//! Three kinds of thread run over the crate's lifetime:
//! - [`spawn_project_list`]: one-shot (with retry-until-success), fetches the project list so
//!   `main.rs` can auto-select or hand off to `Board`'s project picker.
//! - [`spawn_project_session`]: started once a project is chosen. Loads a consistent initial
//!   snapshot (agents/tasks/runs), then subscribes to the daemon's event stream forever,
//!   reconnecting with backoff on failure. This is the only source of ongoing updates — no
//!   polling.
//! - [`spawn_request`]: fire-and-forget, one per operator action (`StartTask`, `CancelTask`, …).

use std::path::PathBuf;
use std::sync::mpsc::Sender;
use std::thread;
use std::time::Duration;

use factory_core::local::{
    LocalRequest, LocalResponse, MAX_AGENT_PAGE_ITEMS, MAX_PROJECT_PAGE_ITEMS, MAX_RUN_PAGE_ITEMS,
    MAX_TASK_PAGE_ITEMS, ServerFrame,
};
use factory_core::{
    AgentSnapshot, EventEnvelope, ProjectId, ProjectSnapshot, RunSnapshot, TaskDetail,
};

use factoryctl::Client;

const MIN_BACKOFF: Duration = Duration::from_millis(400);
const MAX_BACKOFF: Duration = Duration::from_secs(5);

/// Resolves the control socket path using the same three-step rule as `factoryctl`
/// (`crates/factoryctl/src/main.rs::resolve_socket_path`, not exported from its `lib.rs`, so
/// reimplemented here verbatim): an explicit path wins, then `$DARK_FACTORY_SOCKET`, then
/// `$DARK_FACTORY_HOME/f.sock`, then `$HOME/.dark-factory/f.sock`.
pub fn resolve_socket_path(explicit: Option<&str>) -> Result<PathBuf, String> {
    if let Some(path) = explicit.filter(|path| !path.is_empty()) {
        return Ok(PathBuf::from(path));
    }
    if let Ok(path) = std::env::var("DARK_FACTORY_SOCKET") {
        if !path.is_empty() {
            return Ok(PathBuf::from(path));
        }
    }
    if let Ok(path) = std::env::var("DARK_FACTORY_HOME") {
        if !path.is_empty() {
            return Ok(PathBuf::from(path).join("f.sock"));
        }
    }
    std::env::var("HOME")
        .ok()
        .filter(|path| !path.is_empty())
        .map(|path| PathBuf::from(path).join(".dark-factory/f.sock"))
        .ok_or_else(|| "no socket configured and HOME is unavailable".to_owned())
}

/// Messages the render loop consumes. `main.rs` drains these non-blockingly between draws.
pub enum NetMsg {
    ConnectionRetrying(String),
    ConnectionLive,
    Projects(Vec<ProjectSnapshot>),
    ProjectSnapshot {
        agents: Vec<AgentSnapshot>,
        tasks: Vec<TaskDetail>,
        runs: Vec<RunSnapshot>,
    },
    Event(EventEnvelope),
    CaughtUp,
    OperationResult(Result<LocalResponse, String>),
}

fn request_response(client: &Client, request: LocalRequest) -> Result<LocalResponse, String> {
    match client.request(request).map_err(|error| error.to_string())? {
        ServerFrame::Response { response, .. } => Ok(response),
        ServerFrame::Event { .. } | ServerFrame::TerminalOutput { .. } => {
            Err("daemon returned a stream frame instead of a response".into())
        }
    }
}

fn next_backoff(delay: Duration) -> Duration {
    delay.checked_mul(2).unwrap_or(MAX_BACKOFF).min(MAX_BACKOFF)
}

fn load_projects(client: &Client) -> Result<Vec<ProjectSnapshot>, String> {
    let mut all = Vec::new();
    let mut after = None;
    loop {
        match request_response(
            client,
            LocalRequest::ListProjects {
                after_id: after,
                limit: MAX_PROJECT_PAGE_ITEMS,
            },
        )? {
            LocalResponse::Projects {
                projects,
                next_after_id,
            } => {
                let done = next_after_id.is_none() || projects.is_empty();
                after = next_after_id;
                all.extend(projects);
                if done {
                    return Ok(all);
                }
            }
            other => return Err(format!("unexpected response to ListProjects: {other:?}")),
        }
    }
}

fn load_agents(client: &Client, project_id: &ProjectId) -> Result<Vec<AgentSnapshot>, String> {
    let mut all = Vec::new();
    let mut after = None;
    loop {
        match request_response(
            client,
            LocalRequest::ListAgents {
                project_id: project_id.clone(),
                after_id: after,
                limit: MAX_AGENT_PAGE_ITEMS,
            },
        )? {
            LocalResponse::Agents {
                agents,
                next_after_id,
            } => {
                let done = next_after_id.is_none() || agents.is_empty();
                after = next_after_id;
                all.extend(agents);
                if done {
                    return Ok(all);
                }
            }
            other => return Err(format!("unexpected response to ListAgents: {other:?}")),
        }
    }
}

fn load_tasks(client: &Client, project_id: &ProjectId) -> Result<Vec<TaskDetail>, String> {
    let mut all = Vec::new();
    let mut after = None;
    loop {
        match request_response(
            client,
            LocalRequest::ListTasks {
                project_id: project_id.clone(),
                after_id: after,
                limit: MAX_TASK_PAGE_ITEMS,
            },
        )? {
            LocalResponse::Tasks {
                tasks,
                next_after_id,
            } => {
                let done = next_after_id.is_none() || tasks.is_empty();
                after = next_after_id;
                all.extend(tasks);
                if done {
                    return Ok(all);
                }
            }
            other => return Err(format!("unexpected response to ListTasks: {other:?}")),
        }
    }
}

fn load_runs(client: &Client, project_id: &ProjectId) -> Result<Vec<RunSnapshot>, String> {
    let mut all = Vec::new();
    let mut after = None;
    loop {
        match request_response(
            client,
            LocalRequest::ListRuns {
                project_id: project_id.clone(),
                after_id: after,
                limit: MAX_RUN_PAGE_ITEMS,
            },
        )? {
            LocalResponse::Runs {
                runs,
                next_after_id,
            } => {
                let done = next_after_id.is_none() || runs.is_empty();
                after = next_after_id;
                all.extend(runs);
                if done {
                    return Ok(all);
                }
            }
            other => return Err(format!("unexpected response to ListRuns: {other:?}")),
        }
    }
}

fn load_event_sequence(client: &Client) -> Result<i64, String> {
    match request_response(client, LocalRequest::LatestEventSequence)? {
        LocalResponse::EventHead { sequence } => Ok(sequence),
        other => Err(format!(
            "unexpected response to LatestEventSequence: {other:?}"
        )),
    }
}

type ProjectSnapshotData = (Vec<AgentSnapshot>, Vec<TaskDetail>, Vec<RunSnapshot>, i64);

/// Loads agents/tasks/runs for a project together with the event sequence they're consistent
/// with, retrying (bounded) if the daemon's event head moved mid-load. Mirrors the technique
/// `crates/factoryctl/src/ui.rs::load_consistent_snapshot` uses for the egui UI.
fn load_consistent_project_snapshot(
    client: &Client,
    project_id: &ProjectId,
) -> Result<ProjectSnapshotData, String> {
    for _ in 0..3 {
        let before = load_event_sequence(client)?;
        let agents = load_agents(client, project_id)?;
        let tasks = load_tasks(client, project_id)?;
        let runs = load_runs(client, project_id)?;
        let after = load_event_sequence(client)?;
        if before == after {
            return Ok((agents, tasks, runs, after));
        }
    }
    Err("daemon state changed while loading the project snapshot".into())
}

/// Fetches the project list, retrying with backoff until it succeeds. `main.rs` uses this to
/// decide whether to auto-select a project or show the picker.
pub fn spawn_project_list(client: Client, tx: Sender<NetMsg>) {
    thread::spawn(move || {
        let mut delay = MIN_BACKOFF;
        loop {
            match load_projects(&client) {
                Ok(projects) => {
                    let _ = tx.send(NetMsg::ConnectionLive);
                    let _ = tx.send(NetMsg::Projects(projects));
                    return;
                }
                Err(error) => {
                    if tx.send(NetMsg::ConnectionRetrying(error)).is_err() {
                        return;
                    }
                    thread::sleep(delay);
                    delay = next_backoff(delay);
                }
            }
        }
    });
}

/// Owns the lifecycle of a chosen project: bootstrap snapshot, then subscribe to the daemon's
/// event stream forever with reconnect/backoff. Never returns while `tx` is alive.
pub fn spawn_project_session(client: Client, project_id: ProjectId, tx: Sender<NetMsg>) {
    thread::spawn(move || {
        let mut delay = MIN_BACKOFF;
        let (agents, tasks, runs, mut after_sequence) = loop {
            match load_consistent_project_snapshot(&client, &project_id) {
                Ok(snapshot) => break snapshot,
                Err(error) => {
                    if tx.send(NetMsg::ConnectionRetrying(error)).is_err() {
                        return;
                    }
                    thread::sleep(delay);
                    delay = next_backoff(delay);
                }
            }
        };
        let _ = tx.send(NetMsg::ConnectionLive);
        if tx
            .send(NetMsg::ProjectSnapshot {
                agents,
                tasks,
                runs,
            })
            .is_err()
        {
            return;
        }

        let mut delay = MIN_BACKOFF;
        loop {
            match client.subscribe(after_sequence) {
                Ok(subscription) => {
                    let _ = tx.send(NetMsg::ConnectionLive);
                    let mut failure = "event stream ended".to_owned();
                    for frame in subscription {
                        match frame {
                            Ok(ServerFrame::Event { event, .. }) => {
                                after_sequence = event.sequence;
                                delay = MIN_BACKOFF;
                                if tx.send(NetMsg::Event(event)).is_err() {
                                    return;
                                }
                            }
                            Ok(ServerFrame::Response {
                                response: LocalResponse::CaughtUp { .. },
                                ..
                            }) => {
                                if tx.send(NetMsg::CaughtUp).is_err() {
                                    return;
                                }
                            }
                            Ok(
                                ServerFrame::Response { .. } | ServerFrame::TerminalOutput { .. },
                            ) => {}
                            Err(error) => {
                                failure = error.to_string();
                                break;
                            }
                        }
                    }
                    if tx.send(NetMsg::ConnectionRetrying(failure)).is_err() {
                        return;
                    }
                }
                Err(error) => {
                    if tx
                        .send(NetMsg::ConnectionRetrying(error.to_string()))
                        .is_err()
                    {
                        return;
                    }
                }
            }
            thread::sleep(delay);
            delay = next_backoff(delay);
        }
    });
}

/// Fires one request in the background and reports the result. Used for every operator action
/// (`s`/`c`/`r`/`a`/`x`/`n`/`m`/`S`) so the render loop is never blocked on the daemon.
pub fn spawn_request(client: Client, tx: Sender<NetMsg>, request: LocalRequest) {
    thread::spawn(move || {
        let result = request_response(&client, request);
        let _ = tx.send(NetMsg::OperationResult(result));
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_socket_wins_over_everything() {
        let path = resolve_socket_path(Some("/explicit/f.sock")).unwrap();
        assert_eq!(path, PathBuf::from("/explicit/f.sock"));
    }

    #[test]
    fn next_backoff_doubles_and_caps() {
        let mut delay = MIN_BACKOFF;
        for _ in 0..20 {
            delay = next_backoff(delay);
        }
        assert_eq!(delay, MAX_BACKOFF);
    }
}
