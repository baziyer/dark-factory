//! All socket I/O for the board lives here. Everything runs on background threads and reports
//! back to the render loop over an `mpsc` channel (`NetMsg`) — the render loop never blocks on
//! the daemon, and the daemon connection is retried with backoff rather than crashing the UI.
//!
//! Two kinds of thread run over the crate's lifetime:
//! - [`spawn_fleet_session`]: loads **every** project's agents/tasks/runs/sessions (FORTRESS is
//!   fleet-wide — see `model/mod.rs`'s module doc), then subscribes to the daemon's event stream
//!   forever, reconnecting with backoff on failure. This is the only source of ongoing updates —
//!   no polling.
//! - [`spawn_request`]: fire-and-forget, one per operator action (`CreateTask`, `StopSession`, …).
//!
//! `ListSessions` is called per-project alongside agents/tasks/runs; per the shared brief, the
//! daemon may not implement it yet ("not implemented" error) — [`load_sessions`] tolerates that
//! by treating any `LocalResponse::Error` as "no sessions yet" rather than a load failure, so the
//! board still comes up (agents shown idle from run status, per `model::state::agent_state_from_run`)
//! against a daemon that hasn't landed 5A/5C.

use std::path::PathBuf;
use std::sync::mpsc::Sender;
use std::thread;
use std::time::Duration;

use factory_core::local::{LocalRequest, LocalResponse, MAX_TASK_PAGE_ITEMS, ServerFrame};
use factory_core::status::FleetStatus;
use factory_core::{
    AgentSnapshot, EventEnvelope, ProjectId, ProjectSnapshot, RunSnapshot, SessionSnapshot,
    TaskDetail, TaskId,
};

use factoryctl::Client;
use factoryctl::update::{self, UpdateCheck};

const MIN_BACKOFF: Duration = Duration::from_millis(400);
const MAX_BACKOFF: Duration = Duration::from_secs(5);

/// Page size for projects/agents/runs/sessions listing. Deliberately **not**
/// `factory_core::local::MAX_{PROJECT,AGENT,RUN,SESSION}_PAGE_ITEMS` (each declared as `1000`):
/// against the daemon this crate was built against, every one of those `list_*` store methods
/// (`crates/factoryd/src/store.rs`) independently caps at its own `MAX_STATE_PAGE = 101` and
/// rejects anything larger with `InvalidStateLimit` — so a client honoring the *documented*
/// per-endpoint maximum from `factory-core` gets every listing call rejected. This is a
/// pre-existing mismatch between `factory-core`'s declared limits and `factoryd`'s enforced one,
/// out of this track's scope to fix (factory-tui may only touch its own crate); `100` is a safe
/// page size under either bound, and this crate's pagination loops (`load_agents`, `load_runs`,
/// `load_sessions`, `load_projects`) already handle multiple pages correctly regardless of size.
const SAFE_STATE_PAGE_SIZE: u32 = 100;

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
///
/// `large_enum_variant` is allowed deliberately: these cross a channel at
/// most once per network round-trip, not in a hot loop, and boxing payloads
/// would ripple into every construction/match site for no runtime benefit.
#[allow(clippy::large_enum_variant)]
pub enum NetMsg {
    ConnectionRetrying(String),
    ConnectionLive,
    FleetSnapshot {
        projects: Vec<ProjectSnapshot>,
        agents: Vec<AgentSnapshot>,
        tasks: Vec<TaskDetail>,
        runs: Vec<RunSnapshot>,
        sessions: Vec<SessionSnapshot>,
    },
    Event(EventEnvelope),
    CaughtUp,
    OperationResult(Result<LocalResponse, String>),
    /// The result of a background `GetTask` fetch issued by `spawn_task_detail_request` — kept
    /// distinct from `OperationResult` because `Board::apply_task_detail_result` needs to know
    /// *which* task id a failure was for (to clear `pending_detail` and allow a retry), which a
    /// generic `LocalResponse::Error` carries no correlation for.
    TaskDetailResult {
        task_id: TaskId,
        result: Result<LocalResponse, String>,
    },
    /// The result of the hourly release-manifest check (`spawn_update_check`).
    UpdateCheck(UpdateCheck),
    /// The daemon's `FleetStatus` — the same request `factoryctl status` makes — fetched once
    /// after the bootstrap snapshot for the fields the board doesn't otherwise learn (today: the
    /// live-session cap; live counts and attention are then kept from events).
    FleetStatus(FleetStatus),
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

/// Drains every page of a `List*` request whose response carries `(items, next_after_id)`.
/// `build` constructs the request for a given cursor (starting from `None`); `extract` pulls the
/// items/cursor pair out of the response, or maps anything else to an error — see `load_sessions`
/// for how a "not implemented" `LocalResponse::Error` is tolerated as "empty" from within
/// `extract` rather than needing a separate flag here.
fn paginate<T, A>(
    client: &Client,
    mut build: impl FnMut(Option<A>) -> LocalRequest,
    extract: impl Fn(LocalResponse) -> Result<(Vec<T>, Option<A>), String>,
) -> Result<Vec<T>, String> {
    let mut all = Vec::new();
    let mut after = None;
    loop {
        let (items, next_after_id) = extract(request_response(client, build(after))?)?;
        let done = next_after_id.is_none() || items.is_empty();
        after = next_after_id;
        all.extend(items);
        if done {
            return Ok(all);
        }
    }
}

fn load_projects(client: &Client) -> Result<Vec<ProjectSnapshot>, String> {
    paginate(
        client,
        |after_id| LocalRequest::ListProjects {
            after_id,
            limit: SAFE_STATE_PAGE_SIZE,
        },
        |response| match response {
            LocalResponse::Projects {
                projects,
                next_after_id,
            } => Ok((projects, next_after_id)),
            other => Err(format!("unexpected response to ListProjects: {other:?}")),
        },
    )
}

fn load_agents(client: &Client, project_id: &ProjectId) -> Result<Vec<AgentSnapshot>, String> {
    paginate(
        client,
        |after_id| LocalRequest::ListAgents {
            project_id: project_id.clone(),
            after_id,
            limit: SAFE_STATE_PAGE_SIZE,
        },
        |response| match response {
            LocalResponse::Agents {
                agents,
                next_after_id,
            } => Ok((agents, next_after_id)),
            other => Err(format!("unexpected response to ListAgents: {other:?}")),
        },
    )
}

fn load_tasks(client: &Client, project_id: &ProjectId) -> Result<Vec<TaskDetail>, String> {
    paginate(
        client,
        |after_id| LocalRequest::ListTasks {
            project_id: project_id.clone(),
            after_id,
            limit: MAX_TASK_PAGE_ITEMS,
        },
        |response| match response {
            LocalResponse::Tasks {
                tasks,
                next_after_id,
            } => Ok((tasks, next_after_id)),
            other => Err(format!("unexpected response to ListTasks: {other:?}")),
        },
    )
}

fn load_runs(client: &Client, project_id: &ProjectId) -> Result<Vec<RunSnapshot>, String> {
    paginate(
        client,
        |after_id| LocalRequest::ListRuns {
            project_id: project_id.clone(),
            after_id,
            limit: SAFE_STATE_PAGE_SIZE,
        },
        |response| match response {
            LocalResponse::Runs {
                runs,
                next_after_id,
            } => Ok((runs, next_after_id)),
            other => Err(format!("unexpected response to ListRuns: {other:?}")),
        },
    )
}

/// Loads a project's sessions, tolerating a daemon that doesn't implement `ListSessions` yet: any
/// `LocalResponse::Error` (the shared brief's documented "not implemented" case, but really any
/// error — a daemon that can't list sessions for some other reason shouldn't block the rest of
/// the board coming up either) is treated as "no sessions", not a load failure.
fn load_sessions(client: &Client, project_id: &ProjectId) -> Result<Vec<SessionSnapshot>, String> {
    paginate(
        client,
        |after_id| LocalRequest::ListSessions {
            project_id: project_id.clone(),
            after_id,
            limit: Some(SAFE_STATE_PAGE_SIZE as usize),
        },
        |response| match response {
            LocalResponse::Sessions {
                sessions,
                next_after_id,
            } => Ok((sessions, next_after_id)),
            LocalResponse::Error { .. } => Ok((Vec::new(), None)),
            other => Err(format!("unexpected response to ListSessions: {other:?}")),
        },
    )
}

fn load_event_sequence(client: &Client) -> Result<i64, String> {
    match request_response(client, LocalRequest::LatestEventSequence)? {
        LocalResponse::EventHead { sequence } => Ok(sequence),
        other => Err(format!(
            "unexpected response to LatestEventSequence: {other:?}"
        )),
    }
}

type FleetSnapshotData = (
    Vec<ProjectSnapshot>,
    Vec<AgentSnapshot>,
    Vec<TaskDetail>,
    Vec<RunSnapshot>,
    Vec<SessionSnapshot>,
    i64,
);

/// Loads every project's agents/tasks/runs/sessions together with the event sequence they're
/// consistent with, retrying (bounded) if the daemon's event head moved mid-load.
fn load_consistent_fleet_snapshot(client: &Client) -> Result<FleetSnapshotData, String> {
    for _ in 0..3 {
        let before = load_event_sequence(client)?;
        let projects = load_projects(client)?;
        let mut agents = Vec::new();
        let mut tasks = Vec::new();
        let mut runs = Vec::new();
        let mut sessions = Vec::new();
        for project in &projects {
            agents.extend(load_agents(client, &project.id)?);
            tasks.extend(load_tasks(client, &project.id)?);
            runs.extend(load_runs(client, &project.id)?);
            sessions.extend(load_sessions(client, &project.id)?);
        }
        let after = load_event_sequence(client)?;
        if before == after {
            return Ok((projects, agents, tasks, runs, sessions, after));
        }
    }
    Err("daemon state changed while loading the fleet snapshot".into())
}

/// Owns the whole network lifecycle: bootstrap the fleet snapshot, then subscribe to the
/// daemon's event stream forever with reconnect/backoff. Never returns while `tx` is alive.
pub fn spawn_fleet_session(client: Client, tx: Sender<NetMsg>) {
    thread::spawn(move || {
        let mut delay = MIN_BACKOFF;
        let (projects, agents, tasks, runs, sessions, mut after_sequence) = loop {
            match load_consistent_fleet_snapshot(&client) {
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
            .send(NetMsg::FleetSnapshot {
                projects,
                agents,
                tasks,
                runs,
                sessions,
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
                    // The cap is per daemon process: re-read it on every (re)connect, so a
                    // daemon restarted with a different --max-active-runs is reflected.
                    if let Ok(LocalResponse::FleetStatus { status }) =
                        request_response(&client, LocalRequest::FleetStatus)
                    {
                        if tx.send(NetMsg::FleetStatus(status)).is_err() {
                            return;
                        }
                    }
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

/// Runs one release-manifest check in the background (`factoryctl::update::check`: served from
/// `$DARK_FACTORY_HOME/update-check.json` while that is under an hour old, otherwise one `curl`
/// of the manifest) and reports it. `main.rs` calls this at startup and then hourly from the 1 Hz
/// tick — the check itself is what caps the network cost, so a board restarted every minute
/// still fetches at most once an hour. No `$DARK_FACTORY_HOME` (no `HOME`) means no check.
pub fn spawn_update_check(tx: Sender<NetMsg>, now_ms: i64) {
    let Ok(home) = factory_core::paths::dark_factory_home() else {
        return;
    };
    thread::spawn(move || {
        let check = update::check(&home, &update::manifest_url(), now_ms, false);
        let _ = tx.send(NetMsg::UpdateCheck(check));
    });
}

/// Fires one request in the background and reports the result. Used for every operator action
/// so the render loop is never blocked on the daemon.
pub fn spawn_request(client: Client, tx: Sender<NetMsg>, request: LocalRequest) {
    thread::spawn(move || {
        let result = request_response(&client, request);
        let _ = tx.send(NetMsg::OperationResult(result));
    });
}

/// Fires one `GetTask` request in the background for `task_id`'s full detail
/// (body/result/blocked_reason) and reports back via [`NetMsg::TaskDetailResult`] rather than
/// the generic `OperationResult` path — see that variant's doc comment for why. Paired with
/// `Board::begin_task_detail_fetch`, called from `main.rs`'s main loop whenever WORKSHOP's
/// selected task's cached detail is missing or stale.
pub fn spawn_task_detail_request(
    client: Client,
    tx: Sender<NetMsg>,
    project_id: ProjectId,
    task_id: TaskId,
) {
    thread::spawn(move || {
        let result = request_response(
            &client,
            LocalRequest::GetTask {
                project_id,
                task_id: task_id.clone(),
            },
        );
        let _ = tx.send(NetMsg::TaskDetailResult { task_id, result });
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
