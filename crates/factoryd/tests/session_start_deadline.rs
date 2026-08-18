//! Issue #24: a session whose `SessionStart` hook never reaches the daemon
//! must not stay `starting` forever. `execution::Config::session_start_deadline`
//! is the only seam this is injectable through -- deliberately not an
//! operator flag/env var/config key (AGENTS.md rule 3) -- which is why
//! these tests cannot reuse `sessions_e2e.rs`'s subprocess-`factoryd`
//! harness (its `Daemon` only exposes CLI flags to the real binary, with
//! no way to shorten a `Duration` field buried inside `main.rs`'s own
//! `execution::Config` construction). Instead this drives `execution::spawn`
//! and `local_api::serve` in-process, like `tests/local_api.rs`'s
//! `with_server`, but against a *real* `factory-runner`/`factoryctl` and the
//! `shell` provider, like `tests/sessions_e2e.rs` -- so "the runner process
//! is gone" is a real `ps` check, not an assumption.

use std::{
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::Command,
    time::Duration,
};

use factory_core::{
    AgentId, AgentRole, FactoryEvent, PROTOCOL_VERSION, ProjectId, Provider, SessionId,
    SessionSnapshot, SessionState, TaskId,
    local::{LocalRequest, LocalResponse, RequestEnvelope, ServerFrame},
};
use factoryd::{
    execution,
    local_api::{ApiState, serve},
    store::Store,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::{UnixListener, UnixStream},
    sync::oneshot,
};

// --- Real sibling binaries (same technique as `sessions_e2e.rs`) --------

/// `env!("CARGO_BIN_EXE_*")` only resolves for binaries owned by *this*
/// package, not a dev-dependency's own binary targets (confirmed by
/// `sessions_e2e.rs`, which this mirrors): build them explicitly, once per
/// test process, into the same workspace `target/` `factoryd`'s own binary
/// lives in.
fn ensure_sibling_binaries_built() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let status = Command::new(env!("CARGO"))
            .args(["build", "-p", "factory-runner", "-p", "factoryctl"])
            .status()
            .expect("could not run cargo build for factory-runner/factoryctl");
        assert!(
            status.success(),
            "cargo build -p factory-runner -p factoryctl failed"
        );
    });
}

fn workspace_target_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_factoryd"))
        .parent()
        .expect("factoryd binary has a parent directory")
        .to_path_buf()
}

fn factory_runner_path() -> PathBuf {
    ensure_sibling_binaries_built();
    workspace_target_dir().join("factory-runner")
}

fn factoryctl_path() -> PathBuf {
    ensure_sibling_binaries_built();
    workspace_target_dir().join("factoryctl")
}

fn shell_agent_path() -> String {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/shell-agent.sh")
        .to_string_lossy()
        .into_owned()
}

/// Short enough that `<home>/runs/<session-uuid>/control.sock` still fits
/// inside `sockaddr_un`'s ~104-byte `sun_path` (`sessions_e2e.rs`'s own
/// `private_tempdir` doc comment has the full reasoning), and owner-only
/// (0700) so `execution::spawn`'s `prepare_runtime_root` accepts it as the
/// private parent of `runs/`.
fn private_tempdir() -> tempfile::TempDir {
    let directory = tempfile::tempdir_in("/tmp").unwrap();
    std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    directory
}

/// Whether some `factory-runner` process on this machine was launched with
/// `--runtime-dir runtime_dir` -- i.e. whether one particular session's
/// runner (and the real program it supervises) is still alive.
fn runner_alive(runtime_dir: &Path) -> bool {
    let needle = format!("--runtime-dir {}", runtime_dir.display());
    let output = Command::new("ps")
        .args(["-axo", "command"])
        .output()
        .expect("ps");
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .any(|line| line.contains("factory-runner") && line.contains(needle.as_str()))
}

fn runner_control_ready(runtime_dir: &Path) -> bool {
    runner_alive(runtime_dir)
        && std::os::unix::net::UnixStream::connect(runtime_dir.join("control.sock")).is_ok()
}

// --- Local protocol helpers (same technique as `tests/local_api.rs`) ----

async fn write_request(stream: &mut UnixStream, request: LocalRequest) {
    let envelope = RequestEnvelope {
        protocol_version: PROTOCOL_VERSION,
        request,
    };
    let mut json = serde_json::to_vec(&envelope).unwrap();
    json.push(b'\n');
    stream.write_all(&json).await.unwrap();
}

async fn read_frame(reader: &mut BufReader<UnixStream>) -> ServerFrame {
    let mut line = String::new();
    reader.read_line(&mut line).await.unwrap();
    assert!(!line.is_empty(), "server closed before sending a frame");
    serde_json::from_str(&line).unwrap()
}

async fn request(socket: &Path, request: LocalRequest) -> ServerFrame {
    let mut stream = UnixStream::connect(socket).await.unwrap();
    write_request(&mut stream, request).await;
    read_frame(&mut BufReader::new(stream)).await
}

async fn subscribe_from_current_head(socket: &Path) -> BufReader<UnixStream> {
    let head = match request(socket, LocalRequest::LatestEventSequence).await {
        ServerFrame::Response {
            response: LocalResponse::EventHead { sequence },
            ..
        } => sequence,
        frame => panic!("expected event head, got {frame:?}"),
    };
    let mut stream = UnixStream::connect(socket).await.unwrap();
    write_request(
        &mut stream,
        LocalRequest::Subscribe {
            after_sequence: head,
        },
    )
    .await;
    let mut reader = BufReader::new(stream);
    assert!(matches!(
        read_frame(&mut reader).await,
        ServerFrame::Response {
            response: LocalResponse::Subscribed { .. },
            ..
        }
    ));
    assert!(matches!(
        read_frame(&mut reader).await,
        ServerFrame::Response {
            response: LocalResponse::CaughtUp { sequence },
            ..
        } if sequence == head
    ));
    reader
}

async fn next_event<T>(
    reader: &mut BufReader<UnixStream>,
    label: &str,
    mut select: impl FnMut(FactoryEvent) -> Option<T>,
) -> T {
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            if let ServerFrame::Event { event, .. } = read_frame(reader).await
                && let Some(value) = select(event.event)
            {
                return value;
            }
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for event: {label}"))
}

fn project_id() -> ProjectId {
    ProjectId::try_from("factory").unwrap()
}

async fn create_project(socket: &Path, root: &Path) {
    std::fs::create_dir_all(root).unwrap();
    let response = request(
        socket,
        LocalRequest::CreateProject {
            id: project_id(),
            name: "Factory".into(),
            root: root.to_string_lossy().into_owned(),
        },
    )
    .await;
    assert!(
        matches!(
            response,
            ServerFrame::Response {
                response: LocalResponse::ProjectCreated { .. },
                ..
            }
        ),
        "{response:?}"
    );
}

/// A `shell` provider agent running `sh -lc command` (no native
/// `shell-agent.sh` fixture) with an explicit pre-created worktree, so the
/// caller controls exactly what the "provider process" does -- including,
/// for this track's whole point, never calling `factoryctl hook ...
/// SessionStart` at all.
async fn create_shell_agent(socket: &Path, agent_id: &str, worktree: &Path, command: String) {
    std::fs::create_dir_all(worktree).unwrap();
    let response = request(
        socket,
        LocalRequest::CreateAgent {
            id: AgentId::try_from(agent_id).unwrap(),
            project_id: project_id(),
            parent_agent_id: None,
            role: AgentRole::Worker,
            provider: Provider::Shell,
            model: Some(command),
            worktree: Some(worktree.to_string_lossy().into_owned()),
        },
    )
    .await;
    assert!(
        matches!(
            response,
            ServerFrame::Response {
                response: LocalResponse::AgentCreated { .. },
                ..
            }
        ),
        "{response:?}"
    );
}

async fn create_task(socket: &Path, id: &str, title: &str, body: &str) {
    let response = request(
        socket,
        LocalRequest::CreateTask {
            id: TaskId::try_from(id).unwrap(),
            project_id: project_id(),
            parent_task_id: None,
            title: title.into(),
            body: body.into(),
            priority: 0,
        },
    )
    .await;
    assert!(
        matches!(
            response,
            ServerFrame::Response {
                response: LocalResponse::TaskCreated { .. },
                ..
            }
        ),
        "{response:?}"
    );
}

/// Assigning a task is itself the (only) trigger that spawns a session for
/// an agent with none live (D2's per-agent FIFO auto-delivery, via
/// `AssignTask`'s own `execution.wake` -- no separate `StartTask` needed).
async fn assign_task(socket: &Path, task_id: &str, agent_id: &str) {
    let response = request(
        socket,
        LocalRequest::AssignTask {
            project_id: project_id(),
            task_id: TaskId::try_from(task_id).unwrap(),
            agent_id: Some(AgentId::try_from(agent_id).unwrap()),
        },
    )
    .await;
    assert!(
        matches!(
            response,
            ServerFrame::Response {
                response: LocalResponse::TaskAssigned { .. },
                ..
            }
        ),
        "{response:?}"
    );
}

/// Durably holds the agent's queue -- no new spawn, no delivery -- so the
/// dispatcher stops respawning it (`ARCHITECTURE.md`'s `agent
/// pause`/`resume`). Used only to stop a deliberately-always-failing test
/// agent's retry cascade before cleanup, not to test pause itself.
async fn pause_agent(socket: &Path, agent_id: &str) {
    let _ = request(
        socket,
        LocalRequest::PauseAgent {
            project_id: project_id(),
            agent_id: AgentId::try_from(agent_id).unwrap(),
        },
    )
    .await;
}

/// The documented way back in after the dispatcher pauses an agent on its
/// own (issue #24 finding 4's escalation, after
/// `MAX_CONSECUTIVE_START_DEADLINES` consecutive deadline failures) --
/// also resets its spawn backoff/start-deadline streak
/// (`execution::Handle::resume_backoff`, wired into this exact request in
/// `local_api.rs`).
async fn resume_agent(socket: &Path, agent_id: &str) {
    let response = request(
        socket,
        LocalRequest::ResumeAgent {
            project_id: project_id(),
            agent_id: AgentId::try_from(agent_id).unwrap(),
        },
    )
    .await;
    assert!(
        matches!(
            response,
            ServerFrame::Response {
                response: LocalResponse::AgentResumed { .. },
                ..
            }
        ),
        "{response:?}"
    );
}

async fn agent_paused(socket: &Path, agent_id: &str) -> bool {
    let response = request(
        socket,
        LocalRequest::GetAgent {
            project_id: project_id(),
            agent_id: AgentId::try_from(agent_id).unwrap(),
        },
    )
    .await;
    let ServerFrame::Response {
        response: LocalResponse::Agent { agent },
        ..
    } = response
    else {
        panic!("expected Agent, got {response:?}");
    };
    agent.snapshot.paused
}

async fn sessions_for_agent(socket: &Path, agent_id: &str) -> Vec<SessionSnapshot> {
    let response = request(
        socket,
        LocalRequest::ListSessions {
            project_id: project_id(),
            after_id: None,
            limit: None,
        },
    )
    .await;
    let ServerFrame::Response {
        response: LocalResponse::Sessions { sessions, .. },
        ..
    } = response
    else {
        panic!("expected Sessions, got {response:?}");
    };
    sessions
        .into_iter()
        .filter(|session| session.agent_id.as_str() == agent_id)
        .collect()
}

async fn stop_session(socket: &Path, session_id: SessionId) {
    let _ = request(
        socket,
        LocalRequest::StopSession {
            project_id: project_id(),
            session_id,
            grace_ms: 0,
        },
    )
    .await;
}

async fn wait_for<F, Fut, T>(timeout: Duration, mut probe: F) -> T
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Option<T>>,
{
    let start = tokio::time::Instant::now();
    loop {
        if let Some(value) = probe().await {
            return value;
        }
        assert!(
            start.elapsed() <= timeout,
            "timed out after {timeout:?} waiting for a condition"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

async fn wait_until(timeout: Duration, mut predicate: impl FnMut() -> bool) {
    let start = tokio::time::Instant::now();
    loop {
        if predicate() {
            return;
        }
        assert!(
            start.elapsed() <= timeout,
            "timed out after {timeout:?} waiting for a condition"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

// --- Harness: a real daemon (execution + local_api), in-process ---------

struct Harness {
    execution: execution::Handle,
    execution_join: tokio::task::JoinHandle<Result<(), execution::Error>>,
    server: tokio::task::JoinHandle<std::io::Result<()>>,
    shutdown_tx: oneshot::Sender<()>,
    socket: PathBuf,
    home: tempfile::TempDir,
    session_start_deadline: Duration,
}

impl Harness {
    async fn start(session_start_deadline: Duration) -> Self {
        let home = private_tempdir();
        let socket = home.path().join("f.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let state = ApiState::new(Store::open_in_memory().unwrap());
        let config = execution::Config {
            runner_program: factory_runner_path(),
            factoryctl_path: factoryctl_path(),
            runtime_root: home.path().join("runs"),
            guidance_root: home.path().to_path_buf(),
            socket_path: socket.clone(),
            max_active_runs: 4,
            session_start_deadline,
        };
        let (execution, execution_join) = execution::spawn(config, state.clone()).unwrap();
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let server = tokio::spawn(serve(
            listener,
            state,
            execution.clone(),
            home.path().to_path_buf(),
            async move {
                let _ = shutdown_rx.await;
            },
        ));
        Self {
            execution,
            execution_join,
            server,
            shutdown_tx,
            socket,
            home,
            session_start_deadline,
        }
    }

    fn runtime_dir(&self, session_id: &str) -> PathBuf {
        self.home.path().join("runs").join(session_id)
    }

    fn wake(&self, agent_id: &str) {
        self.execution
            .wake(project_id(), AgentId::try_from(agent_id).unwrap());
    }

    async fn wake_when_start_deadline_is_due(&self, session: &SessionSnapshot) {
        let deadline_ms = session.started_at_ms.saturating_add(
            i64::try_from(self.session_start_deadline.as_millis()).unwrap_or(i64::MAX),
        );
        let now_ms = i64::try_from(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_millis(),
        )
        .unwrap();
        if deadline_ms > now_ms {
            tokio::time::sleep(Duration::from_millis((deadline_ms - now_ms) as u64)).await;
        }
        self.wake(session.agent_id.as_str());
    }

    async fn stop(self) {
        let _ = self.shutdown_tx.send(());
        self.server.await.unwrap().unwrap();
        self.execution.shutdown().await.unwrap();
        self.execution_join.await.unwrap().unwrap();
    }
}

// --- Tests ----------------------------------------------------------------

/// The load-bearing case (issue #24): a session whose provider process
/// starts but never calls `factoryctl hook ... SessionStart` (here: `sh -lc
/// "sleep 300"`) must not stay `starting` forever. With the deadline
/// shortened to 150ms, this asserts all three parts of the design: the
/// session is recorded `failed` with a reason explaining what happened, its
/// runner process is actually gone (a real `ps` check, not just a store
/// row), and the agent's next spawn attempt is backed off and retried
/// (a second, distinct session eventually appears).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_starting_session_past_its_deadline_fails_and_is_retried() {
    let harness = Harness::start(Duration::from_millis(150)).await;
    let socket = harness.socket.clone();
    create_project(&socket, &harness.home.path().join("repo")).await;
    create_shell_agent(
        &socket,
        "stuck",
        &harness.home.path().join("worktree-stuck"),
        "sleep 300".to_owned(),
    )
    .await;
    create_task(&socket, "task-1", "Do it", "do it").await;
    assign_task(&socket, "task-1", "stuck").await;

    // Whatever state the first observed session is already in by the time
    // this first succeeds (almost always `starting`, since the poll below
    // checks immediately, before ever sleeping) is not asserted on: a
    // badly loaded box could in principle stall long enough for even the
    // real 5 second safety tick to have already caught the (shortened)
    // deadline before this first observation lands, and the rest of this
    // test drives toward `failed` regardless of which state it starts
    // from -- asserting `Starting` here would make that a spurious
    // failure instead of the harmless race it actually is.
    let first = wait_for(Duration::from_secs(10), || async {
        sessions_for_agent(&socket, "stuck")
            .await
            .into_iter()
            .next()
    })
    .await;
    let runtime_dir = harness.runtime_dir(first.id.as_str());
    if first.state == SessionState::Starting {
        wait_until(Duration::from_secs(5), || {
            runner_control_ready(&runtime_dir)
        })
        .await;
    }

    // Past the (shortened) deadline in real wall-clock time: nudge
    // periodically (a harmless no-op both before the deadline is due and
    // once it has already been caught by the real safety tick) instead of
    // a single fixed sleep-then-wake, so this never depends on winning a
    // race against that tick either.
    let failed = wait_for(Duration::from_secs(10), || async {
        harness.wake("stuck");
        sessions_for_agent(&socket, "stuck")
            .await
            .into_iter()
            .find(|session| session.id == first.id && session.state == SessionState::Failed)
    })
    .await;
    assert!(!failed.state.is_live());
    let reason = failed.wait_reason.as_deref().unwrap_or_default();
    assert!(
        reason.contains("SessionStart hook not received"),
        "unexpected reason: {reason}"
    );

    wait_until(Duration::from_secs(5), || !runner_alive(&runtime_dir)).await;

    // Backoff schedules a retry (5s doubling, ARCHITECTURE.md's own
    // description): nudge periodically -- a no-op while still inside the
    // backoff window -- until a second, distinct session shows up, proving
    // the agent was actually respawned rather than left dead.
    let retried = wait_for(Duration::from_secs(15), || async {
        harness.wake("stuck");
        sessions_for_agent(&socket, "stuck")
            .await
            .into_iter()
            .find(|session| session.id != first.id)
    })
    .await;
    assert_ne!(retried.id, first.id);

    // `stuck` never sends `SessionStart` no matter how many times it is
    // respawned, so left alone it would keep cycling through this same
    // fail/backoff/retry path for the rest of the test's lifetime. Pause it
    // (no new spawn) before stopping whatever is currently live, so cleanup
    // cannot race a fresh respawn and orphan a runner process behind this
    // test (the exact risk issue #26 records for a test harness racing its
    // own process exit against a runner's outstanding `AcknowledgeExit`).
    pause_agent(&socket, "stuck").await;
    for session in sessions_for_agent(&socket, "stuck").await {
        if session.state.is_live() {
            let runtime_dir = harness.runtime_dir(session.id.as_str());
            stop_session(&socket, session.id.clone()).await;
            wait_until(Duration::from_secs(10), || !runner_alive(&runtime_dir)).await;
        }
    }
    harness.stop().await;
}

/// A session that sent `SessionStart` well before the deadline is left
/// completely alone by it -- `dispatch_agent`'s deadline check only ever
/// runs for a session still `state == Starting`, so once the real
/// `shell-agent.sh` fixture's immediate `SessionStart` moves it past that,
/// a wake occurring after the (shortened) deadline has elapsed in
/// wall-clock time must not fail or otherwise touch it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_session_that_already_sent_session_start_is_left_alone() {
    let harness = Harness::start(Duration::from_millis(150)).await;
    let socket = harness.socket.clone();
    create_project(&socket, &harness.home.path().join("repo")).await;
    create_shell_agent(
        &socket,
        "curie",
        &harness.home.path().join("worktree-curie"),
        shell_agent_path(),
    )
    .await;
    create_task(&socket, "task-1", "Do it", "do it").await;
    assign_task(&socket, "task-1", "curie").await;

    // `is_live() && != Starting`, not just `!= Starting`: the latter would
    // also match `Failed`/`Stopped`, so a regression that let the deadline
    // fail this session before its `SessionStart` hook won the race would
    // silently satisfy this wait and only surface later, at the `is_live()`
    // assert below -- further from the actual point of the mistake.
    let started = wait_for(Duration::from_secs(15), || async {
        sessions_for_agent(&socket, "curie")
            .await
            .into_iter()
            .find(|session| session.state.is_live() && session.state != SessionState::Starting)
    })
    .await;
    let session_id = started.id.clone();

    tokio::time::sleep(Duration::from_millis(400)).await;
    harness.wake("curie");
    tokio::time::sleep(Duration::from_millis(500)).await;

    let after = sessions_for_agent(&socket, "curie")
        .await
        .into_iter()
        .find(|session| session.id == session_id)
        .expect("the session should still exist, untouched");
    assert!(
        after.state.is_live(),
        "session unexpectedly ended: {after:?}"
    );

    // If the PTY-typed delivery triggered by `assign_task` hasn't
    // committed yet, `task-1` is still queued, so stopping this session
    // would wake the dispatcher into spawning a fresh one right as this
    // test tears down (`Handle::shutdown` only signals the dispatcher, it
    // does not stop runners) -- issue #26's orphan risk again, same fix as
    // test 1: pause first so there is nothing left to respawn into.
    pause_agent(&socket, "curie").await;
    let runtime_dir = harness.runtime_dir(session_id.as_str());
    stop_session(&socket, session_id).await;
    wait_until(Duration::from_secs(10), || !runner_alive(&runtime_dir)).await;
    harness.stop().await;
}

/// Issue #24 finding 4's escalation: a hookless provider (`sleep 300`,
/// same fixture as the first test) always spawns successfully, so an
/// ordinary spawn failure's backoff alone never caps the fail/backoff/
/// retry cycle. After `MAX_CONSECUTIVE_START_DEADLINES` (3) consecutive
/// deadline expiries in a row, the dispatcher must pause the agent instead
/// of spawning a fourth session, and `factoryctl agent resume` must be
/// the way back in.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn three_consecutive_start_deadlines_pause_the_agent_and_resume_retries() {
    let harness = Harness::start(Duration::from_millis(120)).await;
    let socket = harness.socket.clone();
    create_project(&socket, &harness.home.path().join("repo")).await;
    create_shell_agent(
        &socket,
        "stuck",
        &harness.home.path().join("worktree-stuck"),
        "sleep 300".to_owned(),
    )
    .await;
    create_task(&socket, "task-1", "Do it", "do it").await;
    let mut events = subscribe_from_current_head(&socket).await;
    assign_task(&socket, "task-1", "stuck").await;

    // Drive three full spawn -> deadline -> failed cycles from durable
    // transition events. The prior test repeatedly polled ListSessions and
    // wake() behind independent 10-second clocks; under load it could observe
    // a row only after the timeout intended for the next transition. Here a
    // session's own started_at timestamp determines the one deadline wake,
    // and the event stream acknowledges the exact transition before the test
    // advances to the next cycle.
    let mut seen: Vec<SessionId> = Vec::new();
    for _ in 0..3 {
        let spawned = next_event(&mut events, "a distinct starting session", |event| {
            let FactoryEvent::SessionChanged { session } = event else {
                return None;
            };
            (session.agent_id.as_str() == "stuck"
                && session.state == SessionState::Starting
                && !seen.contains(&session.id))
            .then_some(session)
        })
        .await;
        seen.push(spawned.id.clone());

        let runtime_dir = harness.runtime_dir(spawned.id.as_str());
        wait_until(Duration::from_secs(5), || {
            runner_control_ready(&runtime_dir)
        })
        .await;
        harness.wake_when_start_deadline_is_due(&spawned).await;
        let failed = next_event(&mut events, "that session's deadline failure", |event| {
            let FactoryEvent::SessionChanged { session } = event else {
                return None;
            };
            (session.id == spawned.id && session.state == SessionState::Failed).then_some(session)
        })
        .await;
        assert!(
            failed
                .wait_reason
                .as_deref()
                .is_some_and(|reason| reason.contains("SessionStart hook not received")),
            "unexpected deadline failure: {failed:?}"
        );
        wait_until(Duration::from_secs(5), || !runner_alive(&runtime_dir)).await;
    }

    let paused = next_event(
        &mut events,
        "agent pause after the third deadline",
        |event| {
            let FactoryEvent::AgentChanged { agent } = event else {
                return None;
            };
            (agent.id.as_str() == "stuck" && agent.paused).then_some(agent)
        },
    )
    .await;
    assert!(paused.paused);
    let sessions = sessions_for_agent(&socket, "stuck").await;
    assert_eq!(
        sessions.len(),
        3,
        "no fourth session must be spawned once the agent is paused"
    );
    assert!(
        agent_paused(&socket, "stuck").await,
        "agent must be paused after 3 consecutive start-deadline failures"
    );
    // The third session's own `failed` reason is still exactly what an
    // operator sees (`factoryctl status`/`agent status`/the TUI's
    // attention view) -- no new state, nothing extra to check here.

    resume_agent(&socket, "stuck").await;
    let fourth = next_event(&mut events, "a new session after resume", |event| {
        let FactoryEvent::SessionChanged { session } = event else {
            return None;
        };
        (session.agent_id.as_str() == "stuck"
            && session.state == SessionState::Starting
            && !seen.contains(&session.id))
        .then_some(session)
    })
    .await;
    assert!(
        !seen.contains(&fourth.id),
        "resume must actually spawn a new session, not reuse a paused-over one"
    );

    // Cleanup: pause again so nothing keeps respawning `sleep 300` behind
    // this test, then stop whatever is currently live (issue #26).
    pause_agent(&socket, "stuck").await;
    for session in sessions_for_agent(&socket, "stuck").await {
        if session.state.is_live() {
            let runtime_dir = harness.runtime_dir(session.id.as_str());
            wait_until(Duration::from_secs(5), || {
                runner_control_ready(&runtime_dir)
            })
            .await;
            stop_session(&socket, session.id.clone()).await;
            wait_until(Duration::from_secs(10), || !runner_alive(&runtime_dir)).await;
        }
    }
    harness.stop().await;
}
