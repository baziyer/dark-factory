//! Causal RED for the event-driven orchestrator wake tranche in issue #191.
//!
//! This separate integration target gives the orchestrator one already-idle
//! session, two shell workers parented by that orchestrator, and a small
//! pre-created task tree. Completing one worker while the other is still in
//! its represented gate must cause one bounded orchestrator cycle before the
//! five-second reconciliation tick. Current main publishes terminal task/run
//! events but does not perform that wake, so this test is expected to fail at
//! the causal prompt assertion until the production tranche lands.

use std::{
    fs,
    os::unix::{fs::PermissionsExt, process::CommandExt},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant},
};

use factory_core::{
    AgentId, AgentRole, FactoryEvent, ProjectId, Provider, SessionState, TaskDetail, TaskId,
    TaskStatus,
    local::{LocalRequest, LocalResponse, ServerFrame},
};
use factoryctl::Client;

const PROJECT: &str = "factory";
const READY_TIMEOUT: Duration = Duration::from_secs(30);
const SETUP_TIMEOUT: Duration = Duration::from_secs(30);
const WAKE_RED_TIMEOUT: Duration = Duration::from_secs(2);
const RECOVERY_FENCE_TIMEOUT: Duration = Duration::from_secs(130);
const POLL_INTERVAL: Duration = Duration::from_millis(50);

struct Daemon {
    socket: PathBuf,
    child: Child,
}

impl Daemon {
    fn start(home: &Path) -> Self {
        let socket = home.join("f.sock");
        let mut command = Command::new(env!("CARGO_BIN_EXE_factoryd"));
        command
            .env("DARK_FACTORY_HOME", home)
            .arg("--runner")
            .arg(factory_runner_path())
            .arg("--factoryctl")
            .arg(factoryctl_path())
            .env_remove("DARK_FACTORY_SOCKET")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::from(
                fs::File::create(home.join("daemon.stderr")).unwrap(),
            ))
            .process_group(0);
        let child = command.spawn().expect("spawn factoryd");
        wait_for_socket(&socket);
        Self { socket, child }
    }

    fn client(&self) -> Client {
        Client::new(&self.socket)
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let client = self.client();
        if let Ok(ServerFrame::Response {
            response: LocalResponse::Sessions { sessions, .. },
            ..
        }) = client.request_with_timeout(
            LocalRequest::ListSessions {
                project_id: project_id(),
                after_id: None,
                limit: None,
            },
            Duration::from_secs(2),
        ) {
            for session in sessions
                .into_iter()
                .filter(|session| session.state.is_live())
            {
                let _ = client.request_with_timeout(
                    LocalRequest::StopSession {
                        project_id: project_id(),
                        session_id: session.id,
                        grace_ms: 1_000,
                    },
                    Duration::from_secs(2),
                );
            }
        }
        let _ = Command::new("kill")
            .args(["-TERM", &self.child.id().to_string()])
            .status();
        let _ = self.child.wait();
    }
}

fn project_id() -> ProjectId {
    ProjectId::try_from(PROJECT).unwrap()
}

fn private_tempdir() -> tempfile::TempDir {
    let base = if cfg!(target_os = "macos") {
        "/private/tmp"
    } else {
        "/tmp"
    };
    let directory = tempfile::tempdir_in(base).unwrap();
    fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700)).unwrap();
    directory
}

fn wait_for_socket(socket: &Path) {
    let start = Instant::now();
    while std::os::unix::net::UnixStream::connect(socket).is_err() {
        assert!(
            start.elapsed() <= READY_TIMEOUT,
            "factoryd did not open {} within {READY_TIMEOUT:?}",
            socket.display()
        );
        thread::sleep(POLL_INTERVAL);
    }
}

fn wait_until<T>(description: &str, mut probe: impl FnMut() -> Option<T>) -> T {
    let start = Instant::now();
    loop {
        if let Some(value) = probe() {
            return value;
        }
        assert!(
            start.elapsed() <= SETUP_TIMEOUT,
            "timed out after {SETUP_TIMEOUT:?} waiting for {description}"
        );
        thread::sleep(POLL_INTERVAL);
    }
}

fn wait_until_with_timeout<T>(
    description: &str,
    timeout: Duration,
    mut probe: impl FnMut() -> Option<T>,
) -> T {
    let start = Instant::now();
    loop {
        if let Some(value) = probe() {
            return value;
        }
        assert!(
            start.elapsed() <= timeout,
            "timed out after {timeout:?} waiting for {description}"
        );
        thread::sleep(POLL_INTERVAL);
    }
}

fn workspace_target_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_factoryd"))
        .parent()
        .expect("factoryd binary has a parent")
        .to_path_buf()
}

fn ensure_sibling_binaries_built() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let target = workspace_target_dir();
        if target.join("factory-runner").is_file() && target.join("factoryctl").is_file() {
            return;
        }
        let status = Command::new(env!("CARGO"))
            .args(["build", "-p", "factory-runner", "-p", "factoryctl"])
            .status()
            .expect("build factory-runner and factoryctl");
        assert!(status.success(), "sibling binary build failed");
    });
}

fn factory_runner_path() -> PathBuf {
    ensure_sibling_binaries_built();
    workspace_target_dir().join("factory-runner")
}

fn factoryctl_path() -> PathBuf {
    ensure_sibling_binaries_built();
    workspace_target_dir().join("factoryctl")
}

fn git(root: &Path, args: &[&str]) {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
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

fn init_git_repo(root: &Path) {
    fs::create_dir_all(root).unwrap();
    git(root, &["init", "-q", "-b", "main"]);
    git(root, &["config", "user.email", "test@example.com"]);
    git(root, &["config", "user.name", "Test"]);
    fs::write(root.join("README.md"), b"test\n").unwrap();
    git(root, &["add", "README.md"]);
    git(root, &["commit", "-q", "-m", "initial"]);
}

fn create_project(client: &Client, root: &Path) {
    let response = client
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
        "expected project creation, got {response:?}"
    );
}

fn create_agent(
    client: &Client,
    id: &str,
    parent_agent_id: Option<&str>,
    role: AgentRole,
    model: &Path,
) {
    let response = client
        .request(LocalRequest::CreateAgent {
            id: AgentId::try_from(id).unwrap(),
            project_id: project_id(),
            parent_agent_id: parent_agent_id.map(|parent| AgentId::try_from(parent).unwrap()),
            role,
            provider: Provider::Shell,
            model: Some(model.to_string_lossy().into_owned()),
            reasoning_effort: None,
            model_selection_reason: None,
            worktree: None,
        })
        .unwrap();
    assert!(
        matches!(
            response,
            ServerFrame::Response {
                response: LocalResponse::AgentCreated { .. },
                ..
            }
        ),
        "expected agent creation, got {response:?}"
    );
}

fn create_task(client: &Client, id: &str, title: &str, body: &str) {
    let response = client
        .request(LocalRequest::CreateTask {
            id: TaskId::try_from(id).unwrap(),
            project_id: project_id(),
            parent_task_id: (id != "roadmap-root")
                .then(|| TaskId::try_from("roadmap-root").unwrap()),
            title: title.into(),
            body: body.into(),
            priority: 0,
            agent_id: None,
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
        "expected task creation, got {response:?}"
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
        "expected task assignment, got {response:?}"
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
        panic!("expected task response, got {response:?}");
    };
    task
}

fn session_state(client: &Client, agent_id: &str) -> Option<SessionState> {
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
        panic!("expected sessions response, got {response:?}");
    };
    sessions
        .into_iter()
        .find(|session| session.agent_id.as_str() == agent_id)
        .map(|session| session.state)
}

fn wait_for_task(client: &Client, task_id: &str, status: TaskStatus) -> TaskDetail {
    wait_until(&format!("{task_id} to become {status:?}"), || {
        let task = get_task(client, task_id);
        (task.snapshot.status == status).then_some(task)
    })
}

fn wait_for_agent_state(client: &Client, agent_id: &str, expected: SessionState) {
    wait_until(
        &format!("{agent_id} session to become {expected:?}"),
        || session_state(client, agent_id).filter(|state| *state == expected),
    );
}

fn events_after(client: &Client, sequence: i64) -> Vec<factory_core::EventEnvelope> {
    let response = client
        .request(LocalRequest::EventsAfter {
            sequence,
            limit: 1_000,
        })
        .unwrap();
    let ServerFrame::Response {
        response: LocalResponse::Events { events },
        ..
    } = response
    else {
        panic!("expected events response, got {response:?}");
    };
    events
}

fn write_fixture(home: &Path, mode: &str, log_name: &str, refill_task: Option<&str>) -> PathBuf {
    let script = home.join(format!("{mode}-fixture.py"));
    let log = home.join(log_name);
    let refill = refill_task.unwrap_or("");
    let body = format!(
        r##"#!/usr/bin/env python3
import json
import os
import re
import subprocess
import traceback
import termios
import time
import tty

factoryctl = os.environ["DARK_FACTORY_FACTORYCTL"]
token = os.environ["DARK_FACTORY_SESSION_TOKEN_FILE"]
log_path = {log:?}
mode = {mode:?}
refill_task = {refill:?}

def report_error(error_type, error, trace):
    with open(log_path + ".error", "w", encoding="utf-8") as output:
        traceback.print_exception(error_type, error, trace, file=output)

import sys
sys.excepthook = report_error

def hook(event, payload):
    return subprocess.run(
        [factoryctl, "hook", "--token-file", token, event],
        input=json.dumps(payload), text=True,
        stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, check=False,
    ).stdout.strip() or "{{}}"

def record(text):
    with open(log_path, "a", encoding="utf-8") as output:
        output.write(json.dumps(text) + "\n")

def done(task_id, result):
    subprocess.run([
        factoryctl, "task", "done", "--task", task_id,
        "--result", result,
    ], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, check=False)

def assign_refill():
    subprocess.run([
        factoryctl, "task", "assign", "--task", refill_task,
        "--agent", "worker-a",
    ], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, check=False)

fd = sys.stdin.fileno()
old = termios.tcgetattr(fd)
no_ack_count = 0
tty.setraw(fd)
try:
    hook("SessionStart", {{}})
    buffer = bytearray()
    while True:
        data = os.read(fd, 1)
        if not data:
            break
        if data != b"\r":
            buffer.extend(data)
            continue
        text = buffer.decode("utf-8", errors="replace")
        buffer.clear()
        record(text)
        if mode == "orchestrator-no-ack" and "task:bootstrap-task" not in text:
            no_ack_count += 1
            if no_ack_count == 1:
                time.sleep(40)
                record("late_ack:" + hook("UserPromptSubmit", {{"prompt": text}}))
                continue
            if no_ack_count == 2:
                time.sleep(50)
                continue
        hook("UserPromptSubmit", {{"prompt": text}})
        hook("PreToolUse", {{"tool_name": "Bash"}})
        task = re.search(r"task:([A-Za-z0-9_-]+)", text)
        if mode == "worker-b" and task and task.group(1) == "worker-b-task":
            time.sleep(5)
        if task:
            done(task.group(1), "fixture completed " + task.group(1))
        hook("PostToolUse", {{"tool_name": "Bash"}})
        if mode == "orchestrator" and (not task or task.group(1) != "bootstrap-task"):
            assign_refill()
        hook("Stop", {{}})
finally:
    termios.tcsetattr(fd, termios.TCSANOW, old)
"##,
        log = log.display(),
        mode = mode,
        refill = refill,
    );
    fs::write(&script, body).unwrap();
    fs::set_permissions(&script, fs::Permissions::from_mode(0o700)).unwrap();
    let syntax = Command::new("python3")
        .args(["-m", "py_compile"])
        .arg(&script)
        .status()
        .expect("run fixture syntax check");
    assert!(
        syntax.success(),
        "generated fixture failed python syntax check"
    );
    script
}

fn log_lines(path: &Path) -> Vec<String> {
    fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .map(|line| serde_json::from_str(line).expect("fixture log line is JSON string"))
        .collect()
}

fn wait_for_log_lines(path: &Path, expected: usize, timeout: Duration) -> Vec<String> {
    let start = Instant::now();
    loop {
        let lines = log_lines(path);
        if lines.len() >= expected {
            return lines;
        }
        assert!(
            start.elapsed() <= timeout,
            "expected {expected} prompt(s) in {}, got {lines:?}; daemon stderr: {}",
            path.display(),
            fs::read_to_string(path.parent().unwrap().join("daemon.stderr")).unwrap_or_default()
        );
        thread::sleep(POLL_INTERVAL);
    }
}

fn session_diagnostic(client: &Client) -> String {
    match client.request(LocalRequest::ListSessions {
        project_id: project_id(),
        after_id: None,
        limit: None,
    }) {
        Ok(ServerFrame::Response {
            response: LocalResponse::Sessions { sessions, .. },
            ..
        }) => format!("{sessions:?}"),
        response => format!("{response:?}"),
    }
}

fn wait_for_first_prompt(client: &Client, path: &Path, timeout: Duration) -> Vec<String> {
    let start = Instant::now();
    loop {
        let lines = log_lines(path);
        if !lines.is_empty() {
            return lines;
        }
        assert!(
            start.elapsed() <= timeout,
            "expected bootstrap prompt in {}, got {lines:?}; sessions: {}; fixture error: {}",
            path.display(),
            session_diagnostic(client),
            fs::read_to_string(format!("{}.error", path.display())).unwrap_or_default(),
        );
        thread::sleep(POLL_INTERVAL);
    }
}

#[test]
fn terminal_worker_completion_wakes_one_orchestrator_cycle_and_refills_once() {
    let home = private_tempdir();
    fs::create_dir(home.path().join("projects")).unwrap();
    fs::set_permissions(
        home.path().join("projects"),
        fs::Permissions::from_mode(0o700),
    )
    .unwrap();
    fs::create_dir(home.path().join("projects/factory")).unwrap();
    fs::set_permissions(
        home.path().join("projects/factory"),
        fs::Permissions::from_mode(0o700),
    )
    .unwrap();
    fs::write(home.path().join("projects/factory/PROJECT.md"), []).unwrap();
    fs::set_permissions(
        home.path().join("projects/factory/PROJECT.md"),
        fs::Permissions::from_mode(0o600),
    )
    .unwrap();
    let daemon = Daemon::start(home.path());
    let client = daemon.client();
    assert!(
        home.path().join("projects/factory").is_dir(),
        "guidance fixture path is not a directory: {}",
        home.path().display()
    );
    let root = home.path().join("repo");
    init_git_repo(&root);
    create_project(&client, &root);

    let orchestrator_log = home.path().join("orchestrator-prompts.jsonl");
    let worker_a_log = home.path().join("worker-a-prompts.jsonl");
    let worker_b_log = home.path().join("worker-b-prompts.jsonl");
    let orchestrator_fixture = write_fixture(
        home.path(),
        "orchestrator",
        "orchestrator-prompts.jsonl",
        Some("refill-task"),
    );
    let worker_a_fixture = write_fixture(home.path(), "worker-a", "worker-a-prompts.jsonl", None);
    let worker_b_fixture = write_fixture(home.path(), "worker-b", "worker-b-prompts.jsonl", None);

    create_agent(
        &client,
        "orchestrator",
        None,
        AgentRole::Orchestrator,
        &orchestrator_fixture,
    );
    create_agent(
        &client,
        "worker-a",
        Some("orchestrator"),
        AgentRole::Worker,
        &worker_a_fixture,
    );
    create_agent(
        &client,
        "worker-b",
        Some("orchestrator"),
        AgentRole::Worker,
        &worker_b_fixture,
    );

    // The bounded, pre-created roadmap: the parent row represents the gate,
    // the bootstrap establishes the orchestrator session, the two child rows
    // represent approved worker slots, and refill is the only queued
    // successor the orchestrator may assign.
    create_task(
        &client,
        "roadmap-root",
        "roadmap root",
        "ROADMAP_SECRET_ROOT",
    );
    create_task(
        &client,
        "bootstrap-task",
        "orchestrator bootstrap",
        "bootstrap the bounded cycle",
    );
    create_task(
        &client,
        "worker-a-task",
        "worker A terminal tranche",
        "worker-a-private-body",
    );
    create_task(
        &client,
        "worker-b-task",
        "worker B gate hold",
        "worker-b-private-body",
    );
    create_task(
        &client,
        "refill-task",
        "pre-created refill",
        "refill-private-body",
    );

    assign_task(&client, "bootstrap-task", "orchestrator");
    wait_for_first_prompt(&client, &orchestrator_log, SETUP_TIMEOUT);
    wait_for_task(&client, "bootstrap-task", TaskStatus::Succeeded);
    wait_for_agent_state(&client, "orchestrator", SessionState::Idle);

    assign_task(&client, "worker-b-task", "worker-b");
    wait_for_agent_state(&client, "worker-b", SessionState::Working);
    assign_task(&client, "worker-a-task", "worker-a");
    wait_for_task(&client, "worker-a-task", TaskStatus::Succeeded);

    let events = events_after(&client, 0);
    assert!(events.iter().any(|event| matches!(
        &event.event,
        FactoryEvent::TaskChanged { task }
            if task.id.as_str() == "worker-a-task" && task.status == TaskStatus::Succeeded
    )));
    assert!(events.iter().any(|event| matches!(
        &event.event,
        FactoryEvent::RunChanged { run }
            if run.task_id.as_ref().is_some_and(|task| task.as_str() == "worker-a-task")
                && run.status == factory_core::RunStatus::Succeeded
    )));

    // This is shorter than the production five-second safety tick. A pass
    // therefore proves that the terminal event itself scheduled the cycle; a
    // failure on current main is the causal missing wake.
    let orchestrator_prompts = wait_for_log_lines(&orchestrator_log, 2, WAKE_RED_TIMEOUT);
    let wake_prompts: Vec<_> = orchestrator_prompts
        .iter()
        .filter(|prompt| !prompt.contains("task:bootstrap-task"))
        .collect();
    assert_eq!(wake_prompts.len(), 1, "expected one cycle prompt");
    let wake_prompt = wake_prompts[0];
    for arbitrary_text in [
        "worker A terminal tranche",
        "worker-a-private-body",
        "worker B gate hold",
        "worker-b-private-body",
        "ROADMAP_SECRET_ROOT",
        "refill-private-body",
    ] {
        assert!(
            !wake_prompt.contains(arbitrary_text),
            "cycle prompt leaked arbitrary roadmap text {arbitrary_text:?}: {wake_prompt:?}"
        );
    }

    let refill = wait_for_task(&client, "refill-task", TaskStatus::Succeeded);
    assert_eq!(
        refill
            .snapshot
            .assigned_agent_id
            .as_ref()
            .map(AgentId::as_str),
        Some("worker-a")
    );
    let worker_a_prompts = wait_for_log_lines(&worker_a_log, 2, SETUP_TIMEOUT);
    assert_eq!(
        worker_a_prompts
            .iter()
            .filter(|prompt| prompt.contains("task:worker-a-task"))
            .count(),
        1
    );
    assert_eq!(
        worker_a_prompts
            .iter()
            .filter(|prompt| prompt.contains("task:refill-task"))
            .count(),
        1
    );
    assert_eq!(
        log_lines(&worker_b_log).len(),
        1,
        "busy gate worker duplicated"
    );

    let agent_count = match client
        .request(LocalRequest::ListAgents {
            project_id: project_id(),
            after_id: None,
            limit: 20,
        })
        .unwrap()
    {
        ServerFrame::Response {
            response: LocalResponse::Agents { agents, .. },
            ..
        } => agents.len(),
        response => panic!("expected agents response, got {response:?}"),
    };
    assert_eq!(agent_count, 3, "cycle must not expand authority");

    drop(daemon);
}

#[test]
fn two_unacknowledged_cycles_recover_once_until_a_new_event() {
    let home = private_tempdir();
    fs::create_dir(home.path().join("projects")).unwrap();
    fs::set_permissions(
        home.path().join("projects"),
        fs::Permissions::from_mode(0o700),
    )
    .unwrap();
    fs::create_dir(home.path().join("projects/factory")).unwrap();
    fs::set_permissions(
        home.path().join("projects/factory"),
        fs::Permissions::from_mode(0o700),
    )
    .unwrap();
    fs::write(home.path().join("projects/factory/PROJECT.md"), []).unwrap();
    fs::set_permissions(
        home.path().join("projects/factory/PROJECT.md"),
        fs::Permissions::from_mode(0o600),
    )
    .unwrap();
    let daemon = Daemon::start(home.path());
    let client = daemon.client();
    let root = home.path().join("repo");
    init_git_repo(&root);
    create_project(&client, &root);

    let orchestrator_log = home.path().join("orchestrator-prompts.jsonl");
    let orchestrator_fixture = write_fixture(
        home.path(),
        "orchestrator-no-ack",
        "orchestrator-prompts.jsonl",
        None,
    );
    let worker_fixture = write_fixture(home.path(), "worker-a", "worker-a-prompts.jsonl", None);
    create_agent(
        &client,
        "orchestrator",
        None,
        AgentRole::Orchestrator,
        &orchestrator_fixture,
    );
    create_agent(
        &client,
        "worker-a",
        Some("orchestrator"),
        AgentRole::Worker,
        &worker_fixture,
    );

    create_task(
        &client,
        "roadmap-root",
        "roadmap root",
        "ROADMAP_SECRET_ROOT",
    );
    create_task(
        &client,
        "bootstrap-task",
        "orchestrator bootstrap",
        "bootstrap the bounded cycle",
    );
    create_task(
        &client,
        "worker-a-task",
        "worker A terminal tranche",
        "worker-a-private-body",
    );
    create_task(
        &client,
        "worker-a-task-2",
        "worker A second terminal tranche",
        "worker-a-second-private-body",
    );

    assign_task(&client, "bootstrap-task", "orchestrator");
    wait_for_first_prompt(&client, &orchestrator_log, SETUP_TIMEOUT);
    wait_for_task(&client, "bootstrap-task", TaskStatus::Succeeded);
    wait_for_agent_state(&client, "orchestrator", SessionState::Idle);

    assign_task(&client, "worker-a-task", "worker-a");
    wait_for_task(&client, "worker-a-task", TaskStatus::Succeeded);
    let unique_prompt_count = |lines: &[String]| {
        lines
            .iter()
            .filter(|line| !line.starts_with("late_ack:") && !line.contains("task:bootstrap-task"))
            .fold(Vec::<&String>::new(), |mut prompts, line| {
                if !prompts.contains(&line) {
                    prompts.push(line);
                }
                prompts
            })
            .len()
    };
    let _ = wait_until_with_timeout("the first causal cycle", RECOVERY_FENCE_TIMEOUT, || {
        let lines = log_lines(&orchestrator_log);
        (unique_prompt_count(&lines) >= 1).then_some(lines)
    });

    let prompts =
        wait_until_with_timeout("the one recovery follow-up", RECOVERY_FENCE_TIMEOUT, || {
            let lines = log_lines(&orchestrator_log);
            (unique_prompt_count(&lines) >= 2).then_some(lines)
        });
    assert_eq!(
        unique_prompt_count(&prompts),
        2,
        "two failed acknowledgements must produce exactly one follow-up"
    );
    let lines = wait_until_with_timeout(
        "a rejected late acknowledgement",
        RECOVERY_FENCE_TIMEOUT,
        || {
            let lines = log_lines(&orchestrator_log);
            (lines
                .iter()
                .filter(|line| line.starts_with("late_ack:"))
                .count()
                >= 1)
                .then_some(lines)
        },
    );
    let late_acks: Vec<_> = lines
        .iter()
        .filter(|line| line.starts_with("late_ack:"))
        .collect();
    assert!(
        !late_acks.is_empty(),
        "the terminal attempt must receive a late hook"
    );
    thread::sleep(Duration::from_secs(50));
    assert_eq!(
        unique_prompt_count(&log_lines(&orchestrator_log)),
        2,
        "a later safety tick must not emit a third cycle without a new event"
    );

    assign_task(&client, "worker-a-task-2", "worker-a");
    wait_for_task(&client, "worker-a-task-2", TaskStatus::Succeeded);
    let lines = wait_until_with_timeout(
        "a new causal cycle after the next worker event",
        RECOVERY_FENCE_TIMEOUT,
        || {
            let lines = log_lines(&orchestrator_log);
            (unique_prompt_count(&lines) >= 3).then_some(lines)
        },
    );
    assert_eq!(
        unique_prompt_count(&lines),
        3,
        "one new event must authorize exactly one new cycle"
    );

    drop(daemon);
}
