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

const READY_TIMEOUT: Duration = Duration::from_secs(10);
const DELIVERY_TIMEOUT: Duration = Duration::from_secs(20);

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
        let socket = home.join("f.sock");
        let child = Command::new(env!("CARGO_BIN_EXE_factoryd"))
            .env("DARK_FACTORY_HOME", home)
            .arg("--runner")
            .arg(factory_runner_path())
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
/// idle for a short settle window before returning. A single composed
/// delivery is typed as one multi-line PTY write; the shell fixture reads
/// it back line by line (its own doc comment), running a full hook cycle
/// per line -- so it can pass through `idle` transiently between the task
/// header line (which finishes the task) and a later line (e.g. the
/// memory-file footer, always present) before truly settling. A precise
/// fix would need the fixture to tell the daemon it is done with an entire
/// delivery, not just one line; simplest for now is a test-side settle
/// check, since only this suite's own timing depends on "idle" meaning
/// "fully done," not the product's own delivery/ack logic.
fn wait_for_stable_idle(client: &Client, agent_id: &str) -> SessionSnapshot {
    loop {
        let idle = wait_for_session_state(client, agent_id, SessionState::Idle);
        std::thread::sleep(Duration::from_millis(750));
        if let Some(still) = session_by_agent(client, agent_id) {
            if still.id == idle.id && still.state == SessionState::Idle {
                return still;
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
        Duration::from_secs(5),
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
            Duration::from_secs(10),
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
