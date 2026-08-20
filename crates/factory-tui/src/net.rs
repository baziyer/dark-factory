//! All socket I/O for the board lives here. Everything runs on background threads and reports
//! back to the render loop over an `mpsc` channel (`NetMsg`) — the render loop never blocks on
//! the daemon, and the daemon connection is retried with backoff rather than crashing the UI.
//!
//! Two kinds of thread run over the crate's lifetime:
//! - [`spawn_fleet_session`]: loads every project's agents/tasks/runs, then follows events
//!   fleet-wide — see `model/mod.rs`'s module doc), then subscribes to the daemon's event stream
//!   forever, reconnecting with backoff on failure. This is the only source of ongoing updates —
//!   no polling.
//! - [`spawn_request`]: fire-and-forget, one per operator action.

use std::path::PathBuf;
use std::sync::mpsc::Sender;
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use factory_core::local::{LocalRequest, LocalResponse, MAX_TASK_PAGE_ITEMS, ServerFrame};
use factory_core::status::FleetStatus;
use factory_core::{
    AgentSnapshot, EventEnvelope, ProjectId, ProjectSnapshot, RunSnapshot, TaskDetail, TaskId,
};

use factoryctl::Client;
use factoryctl::update::{self, UpdateCheck};

const MIN_BACKOFF: Duration = Duration::from_millis(400);
const MAX_BACKOFF: Duration = Duration::from_secs(5);
const FLEET_STATUS_REFRESH: Duration = Duration::from_secs(5);
const FLEET_STATUS_REQUEST_TIMEOUT: Duration = Duration::from_secs(3);

/// Page size for project, agent, and run listings.
/// against the daemon this crate was built against, every one of those `list_*` store methods
/// (`crates/factoryd/src/store.rs`) independently caps at its own `MAX_STATE_PAGE = 101` and
/// rejects anything larger with `InvalidStateLimit` — so a client honoring the *documented*
/// per-endpoint maximum from `factory-core` gets every listing call rejected. This is a
/// pre-existing mismatch between `factory-core`'s declared limits and `factoryd`'s enforced one,
/// out of this track's scope to fix (factory-tui may only touch its own crate); `100` is a safe
/// page size under either bound.
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
    DaemonHealth {
        version: String,
    },
    FleetSnapshot {
        projects: Vec<ProjectSnapshot>,
        agents: Vec<AgentSnapshot>,
        tasks: Vec<TaskDetail>,
        runs: Vec<RunSnapshot>,
        event_sequence: i64,
    },
    Event(EventEnvelope),
    /// The bounded connect-time backfill (`REPLAY_BACKFILL_EVENTS`), oldest first — fed through
    /// `Board::apply_replay`, never `apply_event` (see that method's doc comment for why).
    EventsReplay(Vec<EventEnvelope>),
    CaughtUp,
    OperationResult {
        operation_id: u64,
        request: LocalRequest,
        result: Result<LocalResponse, String>,
    },
    CapacityResult(Result<factoryctl::capacity::CapacityChange, String>),
    /// The hourly release-manifest check and the validated installed runtime
    /// it must be compared with (`spawn_update_check`).
    UpdateCheck {
        check: UpdateCheck,
        active_version: Result<Option<String>, String>,
    },
    UpdateProgress(factoryctl::managed_update::UpdateProgress),
    UpdateFinished(Result<factoryctl::managed_update::InstalledUpdate, String>),
    /// The daemon's `FleetStatus` — the same request `factoryctl status` makes.
    FleetStatus(FleetStatus),
}

fn request_response(client: &Client, request: LocalRequest) -> Result<LocalResponse, String> {
    match client.request(request).map_err(|error| error.to_string())? {
        ServerFrame::Response { response, .. } => Ok(response),
        ServerFrame::Event { .. } => {
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
        ServerFrame::Event { .. } => {
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
        ServerFrame::Response { .. } => None,
    }
}

fn next_backoff(delay: Duration) -> Duration {
    delay.checked_mul(2).unwrap_or(MAX_BACKOFF).min(MAX_BACKOFF)
}

/// Drains every page of a `List*` request whose response carries `(items, next_after_id)`.
/// `build` constructs the request for a given cursor (starting from `None`); `extract` pulls the
/// items/cursor pair out of the response, or maps anything else to an error.
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
    i64,
);

/// Loads every project's agents/tasks/runs together with the event sequence they're
/// consistent with, retrying (bounded) if the daemon's event head moved mid-load.
fn load_consistent_fleet_snapshot(client: &Client) -> Result<FleetSnapshotData, String> {
    for _ in 0..3 {
        let before = load_event_sequence(client)?;
        let projects = load_projects(client)?;
        let mut agents = Vec::new();
        let mut tasks = Vec::new();
        let mut runs = Vec::new();
        for project in &projects {
            agents.extend(load_agents(client, &project.id)?);
            tasks.extend(load_tasks(client, &project.id)?);
            runs.extend(load_runs(client, &project.id)?);
        }
        let after = load_event_sequence(client)?;
        if before == after {
            return Ok((projects, agents, tasks, runs, after));
        }
    }
    Err("daemon state changed while loading the fleet snapshot".into())
}

fn load_daemon_health(client: &Client) -> Result<String, String> {
    match request_response(client, LocalRequest::Health)? {
        LocalResponse::Health { version, .. } => Ok(version),
        other => Err(format!("unexpected response to Health: {other:?}")),
    }
}

/// Owns the whole network lifecycle: bootstrap the fleet snapshot, then subscribe to the
/// daemon's event stream forever with reconnect/backoff. Never returns while `tx` is alive.
pub fn spawn_fleet_session(client: Client, tx: Sender<NetMsg>) {
    thread::spawn(move || {
        let mut delay = MIN_BACKOFF;
        if tx
            .send(NetMsg::DaemonHealth {
                version: load_daemon_health(&client).unwrap_or_default(),
            })
            .is_err()
        {
            return;
        }
        let (projects, agents, tasks, runs, mut after_sequence) = loop {
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
                    if tx
                        .send(NetMsg::DaemonHealth {
                            version: load_daemon_health(&client).unwrap_or_default(),
                        })
                        .is_err()
                    {
                        return;
                    }
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

/// Refreshes independently projected fleet attention and capacity.
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
pub fn spawn_update_check(socket: PathBuf, tx: Sender<NetMsg>, now_ms: i64) {
    let Ok(home) = factory_core::paths::dark_factory_home() else {
        return;
    };
    thread::spawn(move || {
        let recovery = factoryctl::managed_update::recover_pending(&home, &socket);
        let mut check = update::check(&home, &update::manifest_url(), now_ms, false);
        if let Err(error) = recovery {
            check.latest = None;
            check.error = Some(format!("interrupted update recovery failed: {error}"));
        }
        let active_version = update::active_version(&home);
        let _ = tx.send(NetMsg::UpdateCheck {
            check,
            active_version,
        });
    });
}

/// The release that can make both the active runtime and this running viewer
/// current. Comparing only with `bin/current` would hide the action from an
/// older TUI after `factoryctl update --install` has already activated the
/// release; comparing only with the compiled viewer could downgrade a newer
/// active runtime.
pub fn manual_update_candidate<'a>(
    check: &'a UpdateCheck,
    active: Option<&str>,
) -> Option<&'a update::Manifest> {
    let latest = check.latest.as_ref()?;
    if update::validate_manifest(latest).is_err()
        || !latest.assets.contains_key(update::platform_key())
        || update::is_newer(&check.current, &latest.version)
        || active.is_some_and(|active| update::is_newer(active, &latest.version))
    {
        return None;
    }
    (update::is_newer(&latest.version, &check.current)
        || active.is_none_or(|active| update::is_newer(&latest.version, active)))
    .then_some(latest)
}

/// Runs the exact installer transaction used by `factoryctl update --install`
/// without blocking rendering. Unlike the CLI, the TUI requires the managed
/// launchd job because it cannot safely restart an unknown daemon owner.
pub fn spawn_update_install(
    socket: PathBuf,
    tx: Sender<NetMsg>,
    check: UpdateCheck,
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        let _ = tx.send(NetMsg::UpdateProgress(
            factoryctl::managed_update::UpdateProgress::Checking,
        ));
        let result = (|| {
            let home =
                factory_core::paths::dark_factory_home().map_err(|error| error.to_string())?;
            let active = update::active_version(&home)?;
            let manifest = manual_update_candidate(&check, active.as_deref())
                .ok_or("the shown release can no longer update this viewer safely")?
                .clone();
            let mut progress = |stage| {
                let _ = tx.send(NetMsg::UpdateProgress(stage));
            };
            factoryctl::managed_update::install(
                &home,
                &socket,
                &manifest,
                true,
                true,
                &mut progress,
                &mut |_| {},
            )
            .map_err(|error| error.to_string())
        })();
        let _ = tx.send(NetMsg::UpdateFinished(result));
    })
}

/// Fires one request in the background and reports the result. Used for every operator action
/// so the render loop is never blocked on the daemon.
pub fn spawn_request(client: Client, tx: Sender<NetMsg>, operation_id: u64, request: LocalRequest) {
    thread::spawn(move || {
        let request_for_result = request.clone();
        let result = request_response(&client, request);
        let _ = tx.send(NetMsg::OperationResult {
            operation_id,
            request: request_for_result,
            result,
        });
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
