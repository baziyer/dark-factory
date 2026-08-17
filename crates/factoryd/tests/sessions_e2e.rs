//! End-to-end tests for resident interactive sessions: a real `factoryd`
//! daemon (temp `$DARK_FACTORY_HOME`) supervising a real `factory-runner`
//! process running the `shell-agent.sh` fixture under a real PTY, driven
//! entirely over the real local socket protocol (`factoryctl::Client`).
//! Deterministic and free -- the shell fixture speaks the exact
//! `factoryctl hook`/`task done`/`task blocked` protocol a real Claude
//! Code or Codex session would (see
//! `crates/factoryd/tests/fixtures/shell-agent.sh`), so nothing here spawns
//! a real provider CLI or costs a token; that is the separate manual check
//! recorded in this track's final report.
//!
//! `factoryd`/`factory-runner`/`factoryctl` are located via
//! `env!("CARGO_BIN_EXE_*")` (factory-runner and factoryctl are
//! [dev-dependencies] of this crate purely so Cargo builds and exposes
//! them for that -- see `Cargo.toml`), so every test always exercises the
//! exact binaries this workspace just built, never anything installed
//! system-wide.

use std::{
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

use factory_core::{
    AgentId, AgentRole, ProjectId, Provider, RunSnapshot, SessionSnapshot, SessionState,
    TaskDetail, TaskId, TaskStatus,
    local::{LocalRequest, LocalResponse, ServerFrame},
    runner::decode_terminal_bytes,
};
use factoryctl::Client;

/// Both ceilings were 10s/20s originally; raised (this track's item 10)
/// because this machine runs with a third-party process pegging a core
/// (not ours), which was flaking the shorter ceilings under load -- not a
/// product timing guarantee, just headroom for a noisy neighbour. Actual
/// pass-case latency is unaffected: `poll_until` still returns the moment
/// its condition is true, polling every 100ms (see `poll_until` below).
const READY_TIMEOUT: Duration = Duration::from_secs(30);
const DELIVERY_TIMEOUT: Duration = Duration::from_secs(200);

// --- Daemon harness ------------------------------------------------------

/// A real `factoryd` process on a fresh, isolated `$DARK_FACTORY_HOME`.
/// Never touches `~/.dark-factory` or launchd -- every path is under a
/// `tempfile::TempDir` this test owns.
struct Daemon {
    socket: PathBuf,
    child: Child,
}

impl Daemon {
    fn start(home: &Path) -> Self {
        Self::start_with_runner(home, &factory_runner_path())
    }

    /// Like [`Daemon::start`], but with an explicit `--runner` path
    /// instead of the real built binary -- this track's item 1 E2E test
    /// uses this to point at a *writable copy* of the real
    /// `factory-runner` it can delete out from under an already-running
    /// daemon (simulating a runtime spawn failure distinct from the
    /// misconfiguration item 2's own startup preflight already catches:
    /// the file is present and valid when this daemon process starts, so
    /// preflight passes and the daemon boots normally).
    fn start_with_runner(home: &Path, runner: &Path) -> Self {
        let socket = home.join("f.sock");
        let child = Command::new(env!("CARGO_BIN_EXE_factoryd"))
            .env("DARK_FACTORY_HOME", home)
            .arg("--runner")
            .arg(runner)
            .arg("--factoryctl")
            .arg(factoryctl_path())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn factoryd");
        wait_for_socket(&socket, READY_TIMEOUT);
        Self { socket, child }
    }

    fn client(&self) -> Client {
        Client::new(&self.socket)
    }

    /// SIGTERM (the same clean-shutdown path `lifecycle.rs` tests) and
    /// waits for exit. Deliberately does not touch anything else: a
    /// resident session's `factory-runner` (and the shell fixture under
    /// it) is an independent process tree that must survive this
    /// (`HANDOFF.md`: closing/rebuilding the operator surface must not
    /// stop agents).
    fn stop(mut self) {
        let pid = self.child.id().to_string();
        let _ = Command::new("kill").args(["-TERM", &pid]).status();
        let _ = self.child.wait();
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        // Safety net for a test that panics (an assertion failure) before
        // reaching its own explicit `cleanup_session`/`daemon.stop()`
        // calls: Rust's default panic behavior unwinds rather than aborts,
        // so this still runs. A resident session's `factory-runner` (and
        // the real process under it) deliberately outlives the daemon
        // that spawned it (D7/restart-proofing's whole point), so without
        // this a panicking test leaks a process for the rest of the
        // machine's uptime -- best-effort and bounded: if the daemon
        // itself is already gone or wedged, this must not hang teardown.
        let mut stopped_any = false;
        if let Ok(ServerFrame::Response {
            response: LocalResponse::Sessions { sessions, .. },
            ..
        }) = self.client().request_with_timeout(
            LocalRequest::ListSessions {
                project_id: project_id(),
                after_id: None,
                limit: None,
            },
            Duration::from_secs(2),
        ) {
            for session in sessions {
                if session.state.is_live() {
                    stopped_any = true;
                    let _ = self.client().request_with_timeout(
                        LocalRequest::StopSession {
                            project_id: project_id(),
                            session_id: session.id,
                            grace_ms: 2_000,
                        },
                        Duration::from_secs(2),
                    );
                }
            }
        }
        // Give the daemon's own background acknowledgement of each
        // session's terminal event (`execution.rs`'s `wait_for_runner_exit`)
        // a bounded chance to finish before this kills the daemon out from
        // under it -- otherwise the runner is orphaned forever, still
        // waiting for an acknowledgement nothing will ever send again. Not
        // `poll_until`: this runs during a panic's unwind too, and a panic
        // inside a `Drop` invoked while already panicking aborts the
        // process instead of finishing teardown.
        if stopped_any {
            let deadline = Instant::now() + Duration::from_secs(5);
            while Instant::now() < deadline {
                let Ok(ServerFrame::Response {
                    response: LocalResponse::Sessions { sessions, .. },
                    ..
                }) = self.client().request_with_timeout(
                    LocalRequest::ListSessions {
                        project_id: project_id(),
                        after_id: None,
                        limit: None,
                    },
                    Duration::from_secs(2),
                )
                else {
                    break;
                };
                if !sessions.iter().any(|session| session.state.is_live()) {
                    break;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// `env!("CARGO_BIN_EXE_*")` only resolves for binaries owned by *this*
/// package (confirmed against every other integration test in this
/// workspace -- none cross a package boundary either); it does not extend
/// to a dev-dependency's own binary targets. Instead: build them
/// explicitly, once per test process, into the same workspace `target/`
/// directory `factoryd`'s own binary lives in (no `--target-dir` override,
/// so it is guaranteed to be the same directory), then reference them by
/// that now-known path.
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

/// A fresh `$DARK_FACTORY_HOME` candidate at the private, owner-only mode
/// `lifecycle::claim()` requires of its state directory's parent, and short
/// enough that `<home>/runs/<session-uuid>/control.sock` still fits inside
/// `sockaddr_un`'s ~104-byte `sun_path` (macOS's default `$TMPDIR`, unlike
/// `/tmp`, resolves under `/var/folders/<hash>/<hash>/T/`, which alone can
/// already eat most of that budget -- `execution.rs`'s own tests use this
/// same `tempdir_in("/tmp")` pattern for the same reason).
/// `tempfile::tempdir()`'s own default mode also depends on the calling
/// process's umask, which is not reliably `0700` under `cargo test`.
fn private_tempdir() -> tempfile::TempDir {
    let directory = tempfile::tempdir_in("/tmp").unwrap();
    std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    directory
}

fn wait_for_socket(socket: &Path, deadline: Duration) {
    let start = Instant::now();
    loop {
        if std::os::unix::net::UnixStream::connect(socket).is_ok() {
            return;
        }
        assert!(
            start.elapsed() <= deadline,
            "factoryd did not open its socket at {} within {deadline:?}",
            socket.display()
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn poll_until<T>(deadline: Duration, mut probe: impl FnMut() -> Option<T>) -> T {
    let start = Instant::now();
    loop {
        if let Some(value) = probe() {
            return value;
        }
        assert!(
            start.elapsed() <= deadline,
            "timed out after {deadline:?} waiting for a condition"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
}

// --- git ------------------------------------------------------------

fn git(project_root: &Path, args: &[&str]) {
    let output = Command::new("git")
        .arg("-C")
        .arg(project_root)
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn init_git_repo(project_root: &Path) {
    git(project_root, &["init", "-q", "-b", "main"]);
    git(project_root, &["config", "user.email", "test@example.com"]);
    git(project_root, &["config", "user.name", "Test"]);
    std::fs::write(project_root.join("README.md"), b"hello\n").unwrap();
    git(project_root, &["add", "README.md"]);
    git(project_root, &["commit", "-q", "-m", "initial"]);
}

// --- Local protocol helpers ------------------------------------------

fn project_id() -> ProjectId {
    ProjectId::try_from("factory").unwrap()
}

fn shell_agent_path() -> String {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/shell-agent.sh")
        .to_string_lossy()
        .into_owned()
}

/// Starts a daemon on `home`, creates a real git repo project at
/// `home/repo`, and returns everything a test needs to drive it further.
struct Project {
    daemon: Daemon,
    root: PathBuf,
}

fn setup_project(home: &Path) -> Project {
    let daemon = Daemon::start(home);
    let root = home.join("repo");
    std::fs::create_dir(&root).unwrap();
    init_git_repo(&root);
    let response = daemon
        .client()
        .request(LocalRequest::CreateProject {
            id: project_id(),
            name: "Factory".into(),
            root: root.to_string_lossy().into_owned(),
        })
        .unwrap();
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
    Project { daemon, root }
}

/// Creates a worker agent on the `shell` provider running
/// `shell-agent.sh`, with no `--worktree` override (so `CreateAgent`
/// auto-provisions a real git worktree, D3).
fn create_shell_agent(client: &Client, agent_id: &str) -> factory_core::AgentSnapshot {
    let response = client
        .request(LocalRequest::CreateAgent {
            id: AgentId::try_from(agent_id).unwrap(),
            project_id: project_id(),
            parent_agent_id: None,
            role: AgentRole::Worker,
            provider: Provider::Shell,
            model: Some(shell_agent_path()),
            worktree: None,
        })
        .unwrap();
    let ServerFrame::Response {
        response: LocalResponse::AgentCreated { agent },
        ..
    } = response
    else {
        panic!("expected AgentCreated, got {response:?}");
    };
    agent
}

/// Like [`create_shell_agent`], but `sh -lc <command>` runs an arbitrary
/// one-off `command` instead of the standard `shell-agent.sh` fixture --
/// for tests that only need to simulate one specific hook payload/protocol
/// detail `shell-agent.sh` itself does not exercise.
fn create_shell_agent_with_command(
    client: &Client,
    agent_id: &str,
    command: String,
) -> factory_core::AgentSnapshot {
    let response = client
        .request(LocalRequest::CreateAgent {
            id: AgentId::try_from(agent_id).unwrap(),
            project_id: project_id(),
            parent_agent_id: None,
            role: AgentRole::Worker,
            provider: Provider::Shell,
            model: Some(command),
            worktree: None,
        })
        .unwrap();
    let ServerFrame::Response {
        response: LocalResponse::AgentCreated { agent },
        ..
    } = response
    else {
        panic!("expected AgentCreated, got {response:?}");
    };
    agent
}

fn create_task(client: &Client, id: &str, title: &str, body: &str) {
    let response = client
        .request(LocalRequest::CreateTask {
            id: TaskId::try_from(id).unwrap(),
            project_id: project_id(),
            parent_task_id: None,
            title: title.into(),
            body: body.into(),
            priority: 0,
        })
        .unwrap();
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

fn assign_task(client: &Client, task_id: &str, agent_id: &str) {
    let response = client
        .request(LocalRequest::AssignTask {
            project_id: project_id(),
            task_id: TaskId::try_from(task_id).unwrap(),
            agent_id: Some(AgentId::try_from(agent_id).unwrap()),
        })
        .unwrap();
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

fn get_task(client: &Client, task_id: &str) -> TaskDetail {
    let response = client
        .request(LocalRequest::GetTask {
            project_id: project_id(),
            task_id: TaskId::try_from(task_id).unwrap(),
        })
        .unwrap();
    let ServerFrame::Response {
        response: LocalResponse::Task { task },
        ..
    } = response
    else {
        panic!("expected Task, got {response:?}");
    };
    task
}

fn list_sessions(client: &Client) -> Vec<SessionSnapshot> {
    let response = client
        .request(LocalRequest::ListSessions {
            project_id: project_id(),
            after_id: None,
            limit: None,
        })
        .unwrap();
    let ServerFrame::Response {
        response: LocalResponse::Sessions { sessions, .. },
        ..
    } = response
    else {
        panic!("expected Sessions, got {response:?}");
    };
    sessions
}

fn list_runs(client: &Client) -> Vec<RunSnapshot> {
    let response = client
        .request(LocalRequest::ListRuns {
            project_id: project_id(),
            after_id: None,
            limit: 100,
        })
        .unwrap();
    let ServerFrame::Response {
        response: LocalResponse::Runs { runs, .. },
        ..
    } = response
    else {
        panic!("expected Runs, got {response:?}");
    };
    runs
}

fn events_after(client: &Client, sequence: i64) -> Vec<factory_core::EventEnvelope> {
    let response = client
        .request(LocalRequest::EventsAfter {
            sequence,
            limit: 1000,
        })
        .unwrap();
    let ServerFrame::Response {
        response: LocalResponse::Events { events },
        ..
    } = response
    else {
        panic!("expected Events, got {response:?}");
    };
    events
}

fn session_by_agent(client: &Client, agent_id: &str) -> Option<SessionSnapshot> {
    list_sessions(client)
        .into_iter()
        .find(|session| session.agent_id.as_str() == agent_id)
}

fn wait_for_session_state(client: &Client, agent_id: &str, state: SessionState) -> SessionSnapshot {
    poll_until(DELIVERY_TIMEOUT, || {
        session_by_agent(client, agent_id).filter(|session| session.state == state)
    })
}

/// Like [`wait_for_session_state`] for `Idle`, but confirms it *stays*
/// idle continuously across a full settle window before returning, not
/// just at one single later instant. A single composed delivery is typed
/// as one multi-line PTY write; the shell fixture reads it back line by
/// line (its own doc comment), running a full hook cycle per line -- so
/// it can pass through `idle` transiently between the task header line
/// (which finishes the task) and a later line (e.g. the memory-file
/// footer, always present) before truly settling. This track's item 10:
/// a single re-check after a fixed sleep (the original approach) is only
/// as robust as that one sleep being longer than however long the
/// remaining lines take to process, which is not a safe assumption on a
/// loaded machine -- found live, `factoryd_restart_does_not_lose_a_live_session`
/// flaking with the session observed `Working` immediately after a
/// restart that followed what this function had just certified as stable
/// `Idle`. Polling continuously through the whole window and restarting
/// the wait on any non-idle observation closes that gap: a fixture still
/// mid-delivery is caught well before the window elapses, not raced
/// against it. A precise fix would need the fixture to tell the daemon it
/// is done with an entire delivery, not just one line; this is a
/// test-side settle check, since only this suite's own timing depends on
/// "idle" meaning "fully done," not the product's own delivery/ack logic.
const IDLE_SETTLE_WINDOW: Duration = Duration::from_secs(3);
const IDLE_SETTLE_POLL: Duration = Duration::from_millis(150);

fn wait_for_stable_idle(client: &Client, agent_id: &str) -> SessionSnapshot {
    loop {
        let idle = wait_for_session_state(client, agent_id, SessionState::Idle);
        let settle_deadline = Instant::now() + IDLE_SETTLE_WINDOW;
        let mut stayed_idle = true;
        while Instant::now() < settle_deadline {
            std::thread::sleep(IDLE_SETTLE_POLL);
            match session_by_agent(client, agent_id) {
                Some(session) if session.id == idle.id && session.state == SessionState::Idle => {}
                _ => {
                    stayed_idle = false;
                    break;
                }
            }
        }
        if !stayed_idle {
            continue;
        }
        if let Some(final_snapshot) = session_by_agent(client, agent_id) {
            if final_snapshot.id == idle.id && final_snapshot.state == SessionState::Idle {
                return final_snapshot;
            }
        }
    }
}

fn wait_for_task_status(client: &Client, task_id: &str, status: TaskStatus) -> TaskDetail {
    poll_until(DELIVERY_TIMEOUT, || {
        let task = get_task(client, task_id);
        (task.snapshot.status == status).then_some(task)
    })
}

/// `StopSession` on `agent_id`'s live session, if any, and waits for it to
/// actually go non-live before returning. A resident session's
/// `factory-runner` (and the real process under it, e.g. `shell-agent.sh`)
/// deliberately outlives the daemon that spawned it (that is the whole
/// point of D7/restart-proofing) -- so every test that leaves one live must
/// clean it up explicitly, or it leaks for the rest of the machine's
/// uptime. `StopSession`'s SIGTERM-then-SIGKILL sequence is verified (in
/// this track's manual debugging) to reliably terminate the whole process
/// group, including a mid-`sleep` grandchild. Waiting here (rather than
/// firing the request and immediately calling `daemon.stop()`, as a caller
/// might otherwise do right after) matters for a subtler reason than just
/// "the process is still exiting": `StopSession`'s RPC returns as soon as
/// the runner *accepts* the stop command, well before the daemon's own
/// background `supervise_child` task finishes subscribing to and
/// acknowledging the runner's terminal event (`execution.rs`'s
/// `wait_for_runner_exit` -- `factory_runner::run` will not let its own
/// process exit without that acknowledgement). If the daemon is killed
/// before that in-flight background task completes, the runner is
/// orphaned forever, still waiting for an acknowledgement nothing will
/// ever send again.
fn cleanup_session(client: &Client, agent_id: &str) {
    if let Some(session) = session_by_agent(client, agent_id) {
        if session.state.is_live() {
            let _ = client.request(LocalRequest::StopSession {
                project_id: project_id(),
                session_id: session.id,
                grace_ms: 2_000,
            });
            poll_until(DELIVERY_TIMEOUT, || {
                session_by_agent(client, agent_id).filter(|session| !session.state.is_live())
            });
        }
    }
}

// --- (a) auto-delivery, real worktree, hook events, second task via
//     Stop-hook block-reply --------------------------------------------

#[test]
fn task_auto_delivers_and_a_second_task_delivers_via_stop_hook_reply() {
    let home = private_tempdir();
    let Project { daemon, root } = setup_project(home.path());
    let client = daemon.client();

    let agent = create_shell_agent(&client, "curie");

    // D3: CreateAgent with no --worktree, in a real git repo, provisioned
    // a real git worktree -- not the project-root fallback.
    let worktree = agent
        .worktree
        .clone()
        .expect("agent should have a worktree");
    assert_ne!(
        PathBuf::from(&worktree),
        root,
        "should be a real git worktree, not the project-root fallback"
    );
    let branch = Command::new("git")
        .args(["-C", &worktree, "rev-parse", "--abbrev-ref", "HEAD"])
        .output()
        .unwrap();
    assert_eq!(
        String::from_utf8_lossy(&branch.stdout).trim(),
        "agent/curie"
    );

    // task-1's title embeds `sleep:2`, landing on the same composed-text
    // header line as `task:task-1` (see shell-agent.sh): the fixture
    // stays `working` for 2s mid-turn, giving this test a window to
    // assign task-2 while curie is busy.
    create_task(&client, "task-1", "First (sleep:2)", "do the first thing");
    assign_task(&client, "task-1", "curie");

    // Session spawns automatically (no StartTask needed -- D2's per-agent
    // FIFO auto-delivery) and reaches `working`: proof SessionStart fired,
    // PTY-typed delivery was acknowledged (a UserPromptSubmit hook), and
    // the episode opened.
    let session = wait_for_session_state(&client, "curie", SessionState::Working);
    assert_eq!(session.provider, Provider::Shell);

    // Assigned while curie is `working`: the dispatcher only PTY-types into
    // an *idle* session, so this can only ever reach the agent via the
    // Stop-hook block-reply path once task-1's turn ends.
    create_task(&client, "task-2", "Second", "do the second thing");
    assign_task(&client, "task-2", "curie");

    let task1 = wait_for_task_status(&client, "task-1", TaskStatus::Succeeded);
    assert_eq!(
        task1.result.as_deref(),
        Some("done: Task task-1: First (sleep:2) (task:task-1)")
    );

    // Unlike task-1 (typed in as one line among possibly several, so the
    // fixture completes it on the exact header line alone), task-2 arrives
    // via the Stop-hook block-reply path as one atomic `reason` string --
    // the *entire* composed delivery (header, guidance, memory footer),
    // matching how a real provider's Stop hook reply works: `reason` is one
    // whole instruction, not a sequence of separately-submitted lines. Only
    // the recognizable header prefix is asserted exactly.
    let task2 = wait_for_task_status(&client, "task-2", TaskStatus::Succeeded);
    let task2_result = task2.result.as_deref().unwrap_or_default();
    assert!(
        task2_result.starts_with("done: Task task-2: Second (task:task-2)"),
        "unexpected task-2 result: {task2_result:?}"
    );

    let idle = wait_for_session_state(&client, "curie", SessionState::Idle);
    assert_eq!(
        idle.id, session.id,
        "the same resident session handled both tasks -- no respawn"
    );

    let runs = list_runs(&client);
    let run1 = runs
        .iter()
        .find(|run| run.task_id.as_ref().map(TaskId::as_str) == Some("task-1"))
        .expect("run for task-1");
    assert_eq!(run1.status, factory_core::RunStatus::Succeeded);
    assert_eq!(run1.closed_by, Some(factory_core::RunClosedBy::TaskDone));
    let run2 = runs
        .iter()
        .find(|run| run.task_id.as_ref().map(TaskId::as_str) == Some("task-2"))
        .expect("run for task-2");
    assert_eq!(run2.status, factory_core::RunStatus::Succeeded);
    assert_eq!(run2.closed_by, Some(factory_core::RunClosedBy::TaskDone));

    // Hook events actually flowed through the daemon (not just the task
    // outcome): SessionStart, UserPromptSubmit, PreToolUse, PostToolUse,
    // and Stop each recorded at least once.
    let hook_events: Vec<_> = events_after(&client, 0)
        .into_iter()
        .filter_map(|envelope| match envelope.event {
            factory_core::FactoryEvent::SessionChanged { session } => session.last_hook_event,
            _ => None,
        })
        .collect();
    for expected in [
        factory_core::ProviderHookEvent::SessionStart,
        factory_core::ProviderHookEvent::UserPromptSubmit,
        factory_core::ProviderHookEvent::PreToolUse,
        factory_core::ProviderHookEvent::PostToolUse,
        factory_core::ProviderHookEvent::Stop,
    ] {
        assert!(
            hook_events.contains(&expected),
            "expected {expected:?} among recorded hook events, got {hook_events:?}"
        );
    }

    cleanup_session(&client, "curie");
    daemon.stop();
}

// --- (b) message-only delivery ----------------------------------------

#[test]
fn a_standalone_message_delivers_without_opening_a_run() {
    let home = private_tempdir();
    let Project { daemon, .. } = setup_project(home.path());
    let client = daemon.client();
    create_shell_agent(&client, "curie");

    let sent = client
        .request(LocalRequest::SendAgentMessage {
            id: factory_core::MessageId::try_from("msg-1").unwrap(),
            project_id: project_id(),
            sender_agent_id: None,
            recipient_agent_id: AgentId::try_from("curie").unwrap(),
            body: "Please check the queue.".into(),
        })
        .unwrap();
    assert!(matches!(
        sent,
        ServerFrame::Response {
            response: LocalResponse::AgentMessageSent { .. },
            ..
        }
    ));

    // A session spawns for the message alone (no task exists at all) and
    // the message is marked delivered.
    poll_until(DELIVERY_TIMEOUT, || {
        let response = client
            .request(LocalRequest::ListAgentMessages {
                project_id: project_id(),
                agent_id: AgentId::try_from("curie").unwrap(),
                after_id: None,
                limit: 10,
            })
            .unwrap();
        let ServerFrame::Response {
            response: LocalResponse::AgentMessages { messages, .. },
            ..
        } = response
        else {
            panic!("expected AgentMessages, got {response:?}");
        };
        messages
            .first()
            .is_some_and(|message| message.delivered_at_ms.is_some())
            .then_some(())
    });

    wait_for_session_state(&client, "curie", SessionState::Idle);
    assert!(
        list_runs(&client).is_empty(),
        "a standalone message must never open a run episode"
    );

    cleanup_session(&client, "curie");
    daemon.stop();
}

// --- (c) pause/resume --------------------------------------------------

#[test]
fn pausing_an_agent_holds_its_queue_until_resumed() {
    let home = private_tempdir();
    let Project { daemon, .. } = setup_project(home.path());
    let client = daemon.client();
    create_shell_agent(&client, "curie");

    let paused = client
        .request(LocalRequest::PauseAgent {
            project_id: project_id(),
            agent_id: AgentId::try_from("curie").unwrap(),
        })
        .unwrap();
    assert!(matches!(
        paused,
        ServerFrame::Response {
            response: LocalResponse::AgentPaused { agent },
            ..
        } if agent.paused
    ));

    create_task(&client, "task-1", "Held", "should not start yet");
    assign_task(&client, "task-1", "curie");

    // Give the dispatcher several ticks' worth of time to (incorrectly)
    // start something; a paused agent's session must never spawn.
    std::thread::sleep(Duration::from_secs(2));
    assert!(
        session_by_agent(&client, "curie").is_none(),
        "a paused agent must not spawn a session"
    );
    assert_eq!(
        get_task(&client, "task-1").snapshot.status,
        TaskStatus::Queued
    );

    let resumed = client
        .request(LocalRequest::ResumeAgent {
            project_id: project_id(),
            agent_id: AgentId::try_from("curie").unwrap(),
        })
        .unwrap();
    assert!(matches!(
        resumed,
        ServerFrame::Response {
            response: LocalResponse::AgentResumed { agent },
            ..
        } if !agent.paused
    ));

    wait_for_task_status(&client, "task-1", TaskStatus::Succeeded);

    cleanup_session(&client, "curie");
    daemon.stop();
}

// --- (d) StopSession closes the episode and cancels the task -----------

#[test]
fn stop_session_closes_the_open_episode_and_cancels_the_task() {
    let home = private_tempdir();
    let Project { daemon, .. } = setup_project(home.path());
    let client = daemon.client();
    create_shell_agent(&client, "curie");

    // Sleeps long enough that the test can reliably observe `working` and
    // still stop it before the fixture would complete the turn on its own.
    create_task(&client, "task-1", "Slow (sleep:20)", "long-running");
    assign_task(&client, "task-1", "curie");

    let session = wait_for_session_state(&client, "curie", SessionState::Working);

    let stopped = client
        .request(LocalRequest::StopSession {
            project_id: project_id(),
            session_id: session.id.clone(),
            grace_ms: 2_000,
        })
        .unwrap();
    assert!(matches!(
        stopped,
        ServerFrame::Response {
            response: LocalResponse::SessionStopped { .. },
            ..
        }
    ));

    // The session process actually exits and the daemon records it
    // terminal; the open episode closes stopped/operator_stop, task
    // cancelled -- not failed (this track's step-5 fix).
    poll_until(DELIVERY_TIMEOUT, || {
        session_by_agent(&client, "curie")
            .filter(|session| !session.state.is_live())
            .map(|session| session.state)
    });
    let task = wait_for_task_status(&client, "task-1", TaskStatus::Cancelled);
    assert!(task.blocked_reason.is_none());

    let runs = list_runs(&client);
    let run = runs
        .iter()
        .find(|run| run.task_id.as_ref().map(TaskId::as_str) == Some("task-1"))
        .expect("run for task-1");
    assert_eq!(run.status, factory_core::RunStatus::Stopped);
    assert_eq!(run.closed_by, Some(factory_core::RunClosedBy::OperatorStop));

    daemon.stop();
}

// --- (e) restart: a live session survives a daemon restart (D7) --------

#[test]
fn factoryd_restart_does_not_lose_a_live_session() {
    let home = private_tempdir();
    let project = setup_project(home.path());
    let client = project.daemon.client();
    create_shell_agent(&client, "curie");

    create_task(&client, "task-1", "First", "before the restart");
    assign_task(&client, "task-1", "curie");
    wait_for_task_status(&client, "task-1", TaskStatus::Succeeded);
    let session_before = wait_for_stable_idle(&client, "curie");

    // Kill (SIGTERM, the clean-shutdown path) and start a brand new
    // daemon process on the exact same $DARK_FACTORY_HOME. The session's
    // factory-runner (and the shell fixture under it) is an independent
    // process tree and must still be alive and idle -- not respawned,
    // not lost.
    project.daemon.stop();
    let daemon = Daemon::start(home.path());
    let client = daemon.client();

    let sessions = list_sessions(&client);
    assert_eq!(
        sessions.len(),
        1,
        "the recovered session must not be duplicated"
    );
    let recovered = &sessions[0];
    assert_eq!(
        recovered.id, session_before.id,
        "same session id across the restart"
    );
    assert_eq!(recovered.state, SessionState::Idle);

    // `AttachTerminal` from the new daemon replays retained PTY output
    // through the reconnected runner: task-1's typed delivery (echoed by
    // the PTY) is still there.
    let frames = read_attach_frames(
        &client,
        LocalRequest::AttachTerminal {
            project_id: project_id(),
            session_id: recovered.id.clone(),
            since_offset: 0,
        },
        8,
        Duration::from_secs(30),
    );
    let replayed = decode_terminal_frames(&frames);
    assert!(
        replayed.contains("task-1"),
        "replayed terminal output should contain task-1's delivered text, got {replayed:?}"
    );

    // The recovered session still works: a task assigned after the
    // restart delivers into the exact same (never-respawned) process.
    create_task(&client, "task-2", "Second", "after the restart");
    assign_task(&client, "task-2", "curie");
    wait_for_task_status(&client, "task-2", TaskStatus::Succeeded);
    let after = session_by_agent(&client, "curie").unwrap();
    assert_eq!(
        after.id, session_before.id,
        "delivery after a restart must reuse the recovered session, not spawn a new one"
    );

    cleanup_session(&client, "curie");
    daemon.stop();
}

// --- (f) direct attach: operator keystrokes reach the real process -----

#[test]
fn terminal_input_reaches_the_live_process_through_attach() {
    let home = private_tempdir();
    let Project { daemon, .. } = setup_project(home.path());
    let client = daemon.client();
    create_shell_agent(&client, "curie");

    create_task(&client, "task-1", "First", "warm the session up");
    assign_task(&client, "task-1", "curie");
    wait_for_task_status(&client, "task-1", TaskStatus::Succeeded);
    let session = wait_for_session_state(&client, "curie", SessionState::Idle);

    // Open the attach connection *before* the exit keystroke, not after the
    // session is confirmed stopped: `factory_runner::run` only keeps its
    // control socket (and the retained spool behind it) open until a client
    // acknowledges its terminal event -- once that happens (promptly, this
    // track's supervise_child fix), the runner process itself exits and the
    // socket is gone. `since_offset: 0` always replays full history
    // regardless of when a connection attaches, so attaching early (while
    // definitely still live) and reading through the live exit is the
    // correct way to observe it, not a fresh post-mortem attach.
    let attach_client = client.clone();
    let attach_session_id = session.id.clone();
    let attach_handle = std::thread::spawn(move || {
        read_attach_frames(
            &attach_client,
            LocalRequest::AttachTerminal {
                project_id: project_id(),
                session_id: attach_session_id,
                since_offset: 0,
            },
            16,
            Duration::from_secs(30),
        )
    });
    // Give the attach connection a moment to actually subscribe before
    // racing it against the runner's own exit.
    std::thread::sleep(Duration::from_millis(200));

    // A raw operator keystroke line, typed directly (not composed by the
    // dispatcher): the fixture's own protocol treats a bare `exit` line as
    // a request to post SessionEnd and exit cleanly.
    let accepted = client
        .request(LocalRequest::TerminalInput {
            project_id: project_id(),
            session_id: session.id.clone(),
            bytes: factory_core::runner::encode_terminal_bytes(b"exit\r"),
        })
        .unwrap();
    assert!(matches!(
        accepted,
        ServerFrame::Response {
            response: LocalResponse::TerminalInputAccepted { .. },
            ..
        }
    ));

    // The real process actually read it and exited: a clean, non-live
    // session, not a crash.
    let ended = poll_until(DELIVERY_TIMEOUT, || {
        session_by_agent(&client, "curie").filter(|session| !session.state.is_live())
    });
    assert_eq!(ended.state, SessionState::Stopped);

    let frames = attach_handle.join().expect("attach reader thread panicked");
    let replayed = decode_terminal_frames(&frames);
    assert!(
        replayed.contains("exit"),
        "the typed `exit` keystroke should be echoed in the retained terminal output, got {replayed:?}"
    );

    daemon.stop();
}

// --- (g) TRACK5D item 5: a SessionStart hook payload's session_id
//     persists as the session's provider_session_id (Codex resume) -------

#[test]
fn session_start_hook_payload_session_id_persists_as_provider_session_id() {
    // Codex reports its own thread id back in its first SessionStart
    // hook's payload (a Claude-shaped `session_id` field); the daemon
    // persists it via `Store::set_provider_session_id` so a later spawn
    // for this agent can `codex resume <thread-id>`. Simulated here with
    // the shell provider (no real Codex session, no tokens spent) posting
    // the exact payload shape by hand.
    const FAKE_CODEX_THREAD_ID: &str = "9b1deb4d-3b7d-4bad-9bdd-2b0d7b3dcb6d";

    let home = private_tempdir();
    let Project { daemon, .. } = setup_project(home.path());
    let client = daemon.client();

    // `agent_profiles.model` is validated as a single-line, control-
    // character-free string bounded at 256 bytes (`validate_agent_model`),
    // so this is `;`-separated and as short as it can be rather than
    // newline-separated like `shell-agent.sh`.
    let command = "printf '{\"session_id\":\"9b1deb4d-3b7d-4bad-9bdd-2b0d7b3dcb6d\"}' | \
\"$DARK_FACTORY_FACTORYCTL\" hook --token-file \"$DARK_FACTORY_SESSION_TOKEN_FILE\" SessionStart \
>/dev/null; while :; do sleep 3600; done"
        .to_owned();
    create_shell_agent_with_command(&client, "curie", command);
    // Some pending work is required to trigger the initial spawn at all;
    // this custom command never reads stdin, so the delivery itself is
    // irrelevant to what this test checks (and is not waited on).
    create_task(&client, "task-1", "Trigger a spawn", "irrelevant body");
    assign_task(&client, "task-1", "curie");

    let session = poll_until(DELIVERY_TIMEOUT, || {
        session_by_agent(&client, "curie").filter(|session| session.provider_session_id.is_some())
    });
    assert_eq!(
        session.provider_session_id.as_deref(),
        Some(FAKE_CODEX_THREAD_ID)
    );

    cleanup_session(&client, "curie");
    daemon.stop();
}

// --- (h) TRACK5D item 1: a bare `factoryctl` (no absolute path, no
//     DARK_FACTORY_FACTORYCTL) resolves via PATH inside a resident
//     session -----------------------------------------------------------

#[test]
fn bare_factoryctl_resolves_via_path_inside_a_terminal_mode_session() {
    let home = private_tempdir();
    let Project { daemon, .. } = setup_project(home.path());
    let client = daemon.client();

    // Deliberately unset the one env var the shell fixture would normally
    // fall back to, and never reference an absolute path: this only
    // succeeds if factoryctl's own directory was actually prepended to
    // PATH for this terminal-mode launch (`runner_process.rs`'s
    // `apply_runner_environment`).
    let command = "unset DARK_FACTORY_FACTORYCTL; \
TOKEN_FILE=\"$DARK_FACTORY_SESSION_TOKEN_FILE\"; \
printf '{}' | factoryctl hook --token-file \"$TOKEN_FILE\" SessionStart >/dev/null; \
while :; do sleep 3600; done"
        .to_owned();
    create_shell_agent_with_command(&client, "curie", command);
    create_task(&client, "task-1", "Trigger a spawn", "irrelevant body");
    assign_task(&client, "task-1", "curie");

    // A SessionStart hook moves a `starting` session to `idle`
    // (`Store::record_hook_event`): this only happens if the bare
    // `factoryctl hook ...` call actually resolved and reached the
    // daemon.
    wait_for_session_state(&client, "curie", SessionState::Idle);

    cleanup_session(&client, "curie");
    daemon.stop();
}

// --- (i) TRACK5E item 1: a spawn-failure storm is visible, backed off,
//     and recovers with exactly one delivery once the runner is available
//     again ---------------------------------------------------------------

#[test]
fn spawn_failure_is_visible_backs_off_and_recovers_with_a_single_delivery() {
    let home = private_tempdir();

    // A writable copy of the real `factory-runner`, not the build
    // artifact itself: this test deletes and restores it out from under a
    // running daemon, which must never touch the actual binary every
    // other test in this process also depends on.
    let runner_copy = home.path().join("factory-runner");
    let restore_runner = || {
        std::fs::copy(factory_runner_path(), &runner_copy).unwrap();
        std::fs::set_permissions(&runner_copy, std::fs::Permissions::from_mode(0o755)).unwrap();
    };
    restore_runner();

    // The daemon starts against a *valid* --runner path (this track's
    // item 2 preflight would otherwise refuse to start it at all -- a
    // separate, stricter guarantee than this test is about): the file is
    // only removed afterward, simulating a spawn-time failure that arises
    // after a daemon has already booted successfully (a deleted/corrupted
    // binary, a bad reinstall, ...), which is what the original bug
    // report actually was.
    let daemon = Daemon::start_with_runner(home.path(), &runner_copy);
    let client = daemon.client();
    let root = home.path().join("repo");
    std::fs::create_dir(&root).unwrap();
    init_git_repo(&root);
    let created = client
        .request(LocalRequest::CreateProject {
            id: project_id(),
            name: "Factory".into(),
            root: root.to_string_lossy().into_owned(),
        })
        .unwrap();
    assert!(matches!(
        created,
        ServerFrame::Response {
            response: LocalResponse::ProjectCreated { .. },
            ..
        }
    ));

    std::fs::remove_file(&runner_copy).unwrap();

    create_shell_agent(&client, "curie");
    // `sleep:2` (the shell fixture's own timing-control marker, also used
    // by `stop_session_closes_the_open_episode_and_cancels_the_task`
    // above) is defensive, not load-bearing: building this test found a
    // real race (a fast-reacting client's `factoryctl task done` landing
    // before the daemon's own commit opened the run episode
    // `Store::open_run_for_task` requires, silently rejected by the
    // fixture's own `|| true`), now closed at the source
    // (`execution::commit_pending_delivery_on_prompt`, run synchronously
    // inside the `UserPromptSubmit` hook handler itself, before that
    // hook's reply can reach the client). Kept anyway as cheap insurance
    // against a regression in that fix, matching this suite's existing
    // convention for timing-sensitive fixture interactions.
    create_task(
        &client,
        "task-1",
        "First (sleep:2)",
        "recovers after a spawn failure",
    );
    assign_task(&client, "task-1", "curie");

    // Visible: a `starting` session row is created and then durably
    // recorded `failed` (not silently absent from `session list`/the
    // TUI), carrying the spawn error as its `wait_reason`.
    let failed = poll_until(DELIVERY_TIMEOUT, || {
        session_by_agent(&client, "curie").filter(|session| session.state == SessionState::Failed)
    });
    assert!(
        failed
            .wait_reason
            .as_deref()
            .is_some_and(|reason| !reason.is_empty()),
        "a failed spawn must record why, got {failed:?}"
    );
    // No leaked runtime directory for the failed attempt: `runs/<session
    // id>/` is removed on spawn failure (this track's item 1), not left
    // behind the way the original bug report's 18 attempts were.
    assert!(
        !home.path().join("runs").join(failed.id.as_str()).exists(),
        "a failed spawn attempt's runtime directory must be cleaned up"
    );
    assert_eq!(
        get_task(&client, "task-1").snapshot.status,
        TaskStatus::Queued,
        "the task must stay durably queued through repeated spawn failures, never lost"
    );

    // Backed off, not stormed: `SPAWN_BACKOFF_INITIAL` (5s) then doubling
    // bounds retries to a handful in this window, unlike the original
    // bug's 18 attempts in a similar span with no backoff at all.
    std::thread::sleep(Duration::from_secs(16));
    let attempts = list_sessions(&client)
        .into_iter()
        .filter(|session| session.agent_id.as_str() == "curie")
        .count();
    assert!(
        attempts <= 4,
        "backoff should bound retries within ~16s to a handful, got {attempts}"
    );

    // Recover: restore the runner binary at the exact same --runner path
    // and restart the daemon on it (same $DARK_FACTORY_HOME/db) --
    // TRACK5E-BRIEF.md item 1's own wording for how to simulate this.
    restore_runner();
    daemon.stop();
    let daemon = Daemon::start_with_runner(home.path(), &runner_copy);
    let client = daemon.client();

    wait_for_task_status(&client, "task-1", TaskStatus::Succeeded);

    // Exactly one delivery: one run ever opened for task-1, not the ~8x
    // duplicate Stop-hook block-reply chain the original bug produced.
    let runs_for_task = list_runs(&client)
        .into_iter()
        .filter(|run| run.task_id.as_ref().map(TaskId::as_str) == Some("task-1"))
        .count();
    assert_eq!(
        runs_for_task, 1,
        "task-1 must be delivered exactly once after recovery"
    );

    cleanup_session(&client, "curie");
    daemon.stop();
}

// --- (j) TRACK5F: a sandboxed provider's `task done` falls back to the
//     file outbox, drained by the very next hook -----------------------

#[test]
fn task_done_falls_back_to_the_file_outbox_when_forced_and_drains_via_the_next_hook() {
    let home = private_tempdir();
    let Project { daemon, .. } = setup_project(home.path());
    let client = daemon.client();

    // `DARK_FACTORY_FORCE_OUTBOX=1`, prefixed onto the fixture's own
    // command (not baked into `shell-agent.sh` itself, nor into the
    // daemon's fixed `SESSION_ENVIRONMENT_NAMES` allowlist -- this is a
    // test-only override, see `outbox.rs`'s `FORCE_OUTBOX_ENV`), makes
    // every outbox-eligible `factoryctl` call the fixture makes (here,
    // `task done`) skip its direct daemon attempt and queue to
    // `$DARK_FACTORY_AGENT_DIR/outbox/` instead -- the same fallback path
    // a genuinely sandboxed provider's blocked socket connect would
    // trigger, without this test needing to actually break the socket.
    let command = format!("DARK_FACTORY_FORCE_OUTBOX=1 {}", shell_agent_path());
    create_shell_agent_with_command(&client, "curie", command);

    create_task(
        &client,
        "task-1",
        "Queue via the outbox",
        "prove the fallback",
    );
    assign_task(&client, "task-1", "curie");

    // `shell-agent.sh` discards `task done`'s output and swallows its
    // exit code (`|| true`): if queuing or the drain were broken, this
    // task would simply stay `running` forever rather than fail loudly.
    // Reaching `Succeeded` with the exact composed result is only
    // possible if the queued `CompleteTask` request was durably carried
    // to the daemon by the very next `factoryctl hook` call
    // (`shell-agent.sh`'s own `Stop` hook, immediately following `task
    // done` in `process_turn`) -- proving `outbox::drain` ran before that
    // hook was sent, not after.
    let task = wait_for_task_status(&client, "task-1", TaskStatus::Succeeded);
    assert_eq!(
        task.result.as_deref(),
        Some("done: Task task-1: Queue via the outbox (task:task-1)")
    );

    let runs = list_runs(&client);
    let run = runs
        .iter()
        .find(|run| run.task_id.as_ref().map(TaskId::as_str) == Some("task-1"))
        .expect("run for task-1");
    assert_eq!(run.status, factory_core::RunStatus::Succeeded);
    assert_eq!(run.closed_by, Some(factory_core::RunClosedBy::TaskDone));

    // The queued request's file is gone: `drain` deletes on success, not
    // just on read.
    let outbox = factory_core::paths::agent_dir(
        home.path(),
        &project_id(),
        &AgentId::try_from("curie").unwrap(),
    )
    .join("outbox");
    let remaining = std::fs::read_dir(&outbox)
        .map(|entries| entries.count())
        .unwrap_or(0);
    assert_eq!(
        remaining, 0,
        "the drained outbox file must be deleted, found {outbox:?} non-empty"
    );

    cleanup_session(&client, "curie");
    daemon.stop();
}

// --- attach helpers ----------------------------------------------------

/// Reads up to `max_frames` frames from an `AttachTerminal` connection in
/// a background thread (its `Iterator::next` has no read timeout of its
/// own), bounded overall by `deadline`.
fn read_attach_frames(
    client: &Client,
    request: LocalRequest,
    max_frames: usize,
    deadline: Duration,
) -> Vec<ServerFrame> {
    let client = client.clone();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let Ok(frames) = client.attach_terminal(request) else {
            return;
        };
        for frame in frames.take(max_frames) {
            let Ok(frame) = frame else { break };
            if tx.send(frame).is_err() {
                break;
            }
        }
    });
    let mut collected = Vec::new();
    let deadline = Instant::now() + deadline;
    while collected.len() < max_frames {
        let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
            break;
        };
        match rx.recv_timeout(remaining) {
            Ok(frame) => collected.push(frame),
            Err(_) => break,
        }
    }
    collected
}

fn decode_terminal_frames(frames: &[ServerFrame]) -> String {
    let mut text = String::new();
    for frame in frames {
        if let ServerFrame::TerminalOutput { bytes, .. } = frame {
            if let Ok(decoded) = decode_terminal_bytes(bytes) {
                text.push_str(&String::from_utf8_lossy(&decoded));
            }
        }
    }
    text
}
