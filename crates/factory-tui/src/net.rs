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
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

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
const FLEET_STATUS_REFRESH: Duration = Duration::from_secs(5);
const FLEET_STATUS_REQUEST_TIMEOUT: Duration = Duration::from_secs(3);

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

/// How many past events `spawn_fleet_session` backfills into announcements/activity on connect
/// (issue #67, #70), via the same `EventsAfter` request `factoryctl events` already uses — a fresh
/// client otherwise starts with empty history even though the daemon retains everything. Bounded,
/// not "everything ever": a busy fleet emits far more than this in an hour, so this is "recent",
/// not "complete".
const REPLAY_BACKFILL_EVENTS: u32 = 200;

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
        event_sequence: i64,
    },
    Event(EventEnvelope),
    /// The bounded connect-time backfill (`REPLAY_BACKFILL_EVENTS`), oldest first — fed through
    /// `Board::apply_replay`, never `apply_event` (see that method's doc comment for why).
    EventsReplay(Vec<EventEnvelope>),
    CaughtUp,
    OperationResult(Result<LocalResponse, String>),
    CapacityResult(Result<factoryctl::capacity::CapacityChange, String>),
    /// The hourly release-manifest check and the validated installed runtime
    /// it must be compared with (`spawn_update_check`).
    UpdateCheck {
        check: UpdateCheck,
        active_version: Result<Option<String>, String>,
    },
    /// The daemon's `FleetStatus` — the same request `factoryctl status` makes — refreshed in a
    /// separate worker because worktree git state changes without durable events.
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

fn request_response_with_timeout(
    client: &Client,
    request: LocalRequest,
    timeout: Duration,
) -> Result<LocalResponse, String> {
    match client
        .request_with_timeout(request, timeout)
        .map_err(|error| error.to_string())?
    {
        ServerFrame::Response { response, .. } => Ok(response),
        ServerFrame::Event { .. } | ServerFrame::TerminalOutput { .. } => {
            Err("daemon returned a stream frame instead of a response".into())
        }
    }
}

fn subscription_message(frame: ServerFrame) -> Option<NetMsg> {
    match frame {
        ServerFrame::Event { event, .. } => Some(NetMsg::Event(event)),
        ServerFrame::Response {
            response: LocalResponse::CaughtUp { .. },
            ..
        } => Some(NetMsg::CaughtUp),
        ServerFrame::Response { .. } | ServerFrame::TerminalOutput { .. } => None,
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
    let mut all = Vec::new();
    let mut cursor: Option<(TaskId, i64)> = None;
    loop {
        let request = LocalRequest::ListTasks {
            project_id: project_id.clone(),
            after_id: cursor.as_ref().map(|(id, _)| id.clone()),
            agent_id: None,
            queue_revision: cursor.as_ref().map(|(_, revision)| *revision),
            // The board keeps history separately in the task map so failed
            // and cancelled rows remain actionable; the visible worker
            // queue still comes only from Board::active_tasks_for_agent.
            history: true,
            limit: MAX_TASK_PAGE_ITEMS,
        };
        let response = request_response(client, request)?;
        let (tasks, next_after_id, revision) = match response {
            LocalResponse::Tasks {
                tasks,
                next_after_id,
                queue_revision,
            } => (tasks, next_after_id, queue_revision),
            other => return Err(format!("unexpected response to ListTasks: {other:?}")),
        };
        let done = next_after_id.is_none() || tasks.is_empty();
        all.extend(tasks);
        if done {
            return Ok(all);
        }
        cursor = Some((
            next_after_id.expect("non-empty paginated task response has a cursor"),
            revision.ok_or_else(|| "daemon omitted the task page revision".to_owned())?,
        ));
    }
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
                event_sequence: after_sequence,
            })
            .is_err()
        {
            return;
        }
        // Backfill the last `REPLAY_BACKFILL_EVENTS` events (issue #67/#70) via the cursor the
        // live subscribe below is about to start from — `after_sequence - N` (floored at 0),
        // best-effort: a daemon too old for `EventsAfter`, or any other error, just leaves the
        // board with the empty history it already had rather than blocking startup on it.
        let backfill_from = after_sequence.saturating_sub(i64::from(REPLAY_BACKFILL_EVENTS));
        if let Ok(LocalResponse::Events { events }) = request_response(
            &client,
            LocalRequest::EventsAfter {
                sequence: backfill_from.max(0),
                limit: REPLAY_BACKFILL_EVENTS,
            },
        ) {
            if !events.is_empty() && tx.send(NetMsg::EventsReplay(events)).is_err() {
                return;
            }
        }
        let mut delay = MIN_BACKOFF;
        loop {
            match client.subscribe(after_sequence) {
                Ok(subscription) => {
                    let _ = tx.send(NetMsg::ConnectionLive);
                    let mut failure = "event stream ended".to_owned();
                    for frame in subscription {
                        match frame {
                            Ok(frame) => {
                                if let ServerFrame::Event { event, .. } = &frame {
                                    after_sequence = event.sequence;
                                    delay = MIN_BACKOFF;
                                }
                                if let Some(message) = subscription_message(frame) {
                                    if tx.send(message).is_err() {
                                        return;
                                    }
                                }
                            }
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

/// Refreshes non-event state without delaying subscription reads. Git worktree changes publish
/// no daemon event, so connect-time-only status becomes stale as soon as an agent edits a file.
pub struct FleetStatusRefresh {
    shutdown: Arc<(Mutex<bool>, Condvar)>,
    thread: Option<thread::JoinHandle<()>>,
}

impl Drop for FleetStatusRefresh {
    fn drop(&mut self) {
        let (lock, wake) = &*self.shutdown;
        *lock.lock().unwrap() = true;
        wake.notify_one();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

pub fn spawn_fleet_status_refresh(client: Client, tx: Sender<NetMsg>) -> FleetStatusRefresh {
    spawn_fleet_status_refresh_with(
        client,
        tx,
        FLEET_STATUS_REFRESH,
        FLEET_STATUS_REQUEST_TIMEOUT,
    )
}

fn spawn_fleet_status_refresh_with(
    client: Client,
    tx: Sender<NetMsg>,
    interval: Duration,
    request_timeout: Duration,
) -> FleetStatusRefresh {
    let shutdown = Arc::new((Mutex::new(false), Condvar::new()));
    let worker_shutdown = Arc::clone(&shutdown);
    let thread = thread::spawn(move || {
        loop {
            let started = Instant::now();
            if let Ok(LocalResponse::FleetStatus { status }) =
                request_response_with_timeout(&client, LocalRequest::FleetStatus, request_timeout)
            {
                if tx.send(NetMsg::FleetStatus(status)).is_err() {
                    return;
                }
            }
            let remaining = interval.saturating_sub(started.elapsed());
            let (lock, wake) = &*worker_shutdown;
            let stopped = lock.lock().unwrap();
            if *stopped {
                return;
            }
            let (stopped, _) = wake.wait_timeout(stopped, remaining).unwrap();
            if *stopped {
                return;
            }
        }
    });
    FleetStatusRefresh {
        shutdown,
        thread: Some(thread),
    }
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
        let active_version = update::active_version(&home);
        let _ = tx.send(NetMsg::UpdateCheck {
            check,
            active_version,
        });
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

/// Runs the same operator-owned launchd capacity operation as
/// `factoryctl capacity set`; the render loop never blocks on launchd or the
/// post-reload health wait.
pub fn spawn_capacity_update(socket: PathBuf, tx: Sender<NetMsg>, capacity: usize) {
    thread::spawn(move || {
        let result = factoryctl::capacity::set_from_environment(&socket, capacity);
        let _ = tx.send(NetMsg::CapacityResult(result));
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixListener;

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

    fn empty_fleet(generated_at_ms: i64) -> FleetStatus {
        FleetStatus {
            generated_at_ms,
            event_sequence: 0,
            auto_mode: false,
            live_session_cap: 4,
            live_sessions: 0,
            projects: Vec::new(),
            attention: Vec::new(),
        }
    }

    #[test]
    fn refresh_worker_repeats_start_to_start_and_is_independent_of_other_messages() {
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("factory.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let (timing_tx, timing_rx) = std::sync::mpsc::channel();
        let server = thread::spawn(move || {
            for sequence in 1..=2 {
                let (mut stream, _) = listener.accept().unwrap();
                let started = Instant::now();
                let mut request = String::new();
                BufReader::new(stream.try_clone().unwrap())
                    .read_line(&mut request)
                    .unwrap();
                thread::sleep(Duration::from_secs(2));
                serde_json::to_writer(
                    &mut stream,
                    &ServerFrame::Response {
                        protocol_version: factory_core::PROTOCOL_VERSION,
                        response: LocalResponse::FleetStatus {
                            status: empty_fleet(sequence),
                        },
                    },
                )
                .unwrap();
                stream.write_all(b"\n").unwrap();
                timing_tx.send((started, Instant::now())).unwrap();
            }
        });
        let (tx, rx) = std::sync::mpsc::channel();
        let worker = spawn_fleet_status_refresh_with(
            Client::new(&socket),
            tx.clone(),
            Duration::from_secs(4),
            Duration::from_secs(3),
        );
        tx.send(NetMsg::CaughtUp).unwrap();
        assert!(matches!(
            rx.recv_timeout(Duration::from_millis(50)).unwrap(),
            NetMsg::CaughtUp
        ));
        let first = rx.recv_timeout(Duration::from_secs(4)).unwrap();
        let second = rx.recv_timeout(Duration::from_secs(6)).unwrap();
        assert!(matches!(first, NetMsg::FleetStatus(status) if status.generated_at_ms == 1));
        assert!(matches!(second, NetMsg::FleetStatus(status) if status.generated_at_ms == 2));
        let (first_start, first_response) = timing_rx.recv().unwrap();
        let (second_start, _) = timing_rx.recv().unwrap();
        let response_duration = first_response.duration_since(first_start);
        let tolerated_scheduler_delay = response_duration.mul_f32(0.75);
        assert!(
            second_start.duration_since(first_start)
                < Duration::from_secs(4) + tolerated_scheduler_delay,
            "refresh interval included the response duration"
        );
        drop(worker);
        server.join().unwrap();
    }

    #[test]
    fn refresh_worker_shutdown_interrupts_failure_backoff() {
        let directory = tempfile::tempdir().unwrap();
        let client = Client::new(directory.path().join("missing.sock"));
        let (tx, _rx) = std::sync::mpsc::channel();
        let worker = spawn_fleet_status_refresh_with(
            client,
            tx,
            Duration::from_secs(60),
            Duration::from_millis(50),
        );
        thread::sleep(Duration::from_millis(20));

        let started = Instant::now();
        drop(worker);
        assert!(started.elapsed() < Duration::from_millis(200));
    }

    #[test]
    fn tui_subscription_accepts_stored_v1_replay_and_caught_up_boundary() {
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("factory.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = String::new();
            BufReader::new(stream.try_clone().unwrap())
                .read_line(&mut request)
                .unwrap();
            let frames = [
                ServerFrame::Response {
                    protocol_version: factory_core::PROTOCOL_VERSION,
                    response: LocalResponse::Subscribed {
                        after_sequence: 0,
                        replay_through: 1,
                    },
                },
                ServerFrame::Event {
                    protocol_version: factory_core::PROTOCOL_VERSION,
                    event: factory_core::EventEnvelope {
                        protocol_version: 1,
                        sequence: 1,
                        occurred_at_ms: 10,
                        event: factory_core::FactoryEvent::AutoModeChanged { enabled: true },
                    },
                },
                ServerFrame::Response {
                    protocol_version: factory_core::PROTOCOL_VERSION,
                    response: LocalResponse::CaughtUp { sequence: 1 },
                },
            ];
            for frame in frames {
                serde_json::to_writer(&mut stream, &frame).unwrap();
                stream.write_all(b"\n").unwrap();
            }
        });

        let mut subscription = Client::new(&socket).subscribe(0).unwrap();
        assert!(subscription_message(subscription.next().unwrap().unwrap()).is_none());
        assert!(matches!(
            subscription_message(subscription.next().unwrap().unwrap()),
            Some(NetMsg::Event(factory_core::EventEnvelope {
                protocol_version: 1,
                sequence: 1,
                ..
            }))
        ));
        assert!(matches!(
            subscription_message(subscription.next().unwrap().unwrap()),
            Some(NetMsg::CaughtUp)
        ));
        server.join().unwrap();
    }
}
