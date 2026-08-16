use std::{
    fs,
    num::NonZeroU32,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    time::Duration,
};

use factory_core::{
    AgentId, AgentRole, FactoryEvent, ObserverHealth, ProjectId, Provider, RunId, RunStatus,
    RunnerInstanceId, TaskId,
    runner::{
        OutputStream, RUNNER_PROTOCOL_VERSION, RequestEnvelope, RunnerEvent, RunnerEventEnvelope,
        RunnerFrame, RunnerRequest,
    },
};
use factoryd::{
    daemon_state::{DaemonState, DaemonStateError},
    execution::{self, Config, StartTask},
    store::{
        AdoptedProviderSession, NewAgent, NewProject, NewTask, RunReservation, RunnerEventEffects,
        Store, StoreError, TerminalOutcome,
    },
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::UnixListener,
    task::yield_now,
    time::{advance, timeout},
};
use uuid::Uuid;

const THREAD_ID: &str = "0195d40a-1111-7000-8000-000000000001";
const MAX_TEST_VIRTUAL_DRIVE: Duration = Duration::from_secs(1);
const SCRIPTED_RUNNER_SOURCE: &str = r####"
use std::{
    env, fs,
    fs::OpenOptions,
    io::{BufRead, BufReader, Read, Write},
    os::unix::{fs::PermissionsExt, net::UnixListener},
    path::{Path, PathBuf},
    thread,
};

const LAUNCHED: &str = __LAUNCHED__;
const RELEASE: &str = __RELEASE__;
const DONE: &str = __DONE__;
const REQUESTS: &str = __REQUESTS__;
const ARGUMENTS: &str = __ARGUMENTS__;
const INPUT: &str = __INPUT__;
const DISCONNECT_MODE: &str = __DISCONNECT_MODE__;
const DISCONNECTED: &str = __DISCONNECTED__;
const LAUNCH_SHUTDOWN_MODE: &str = __LAUNCH_SHUTDOWN_MODE__;
const INPUT_RECEIVED: &str = __INPUT_RECEIVED__;
const EXIT_BEFORE_HELLO_MODE: &str = __EXIT_BEFORE_HELLO_MODE__;
const PRE_HELLO_HANG_MODE: &str = __PRE_HELLO_HANG_MODE__;
const RUNNER_PID: &str = __RUNNER_PID__;
const SOCKET_READY: &str = __SOCKET_READY__;
const EXIT: &str = __EXIT__;
const THREAD_ID: &str = "0195d40a-1111-7000-8000-000000000001";

fn argument(arguments: &[String], name: &str) -> String {
    let index = arguments.iter().position(|argument| argument == name).unwrap();
    arguments[index + 1].clone()
}

fn append_request(line: &str) {
    let mut log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(REQUESTS)
        .unwrap();
    log.write_all(line.as_bytes()).unwrap();
}

fn write_frame(
    stream: &mut std::os::unix::net::UnixStream,
    frame: &str,
) -> std::io::Result<()> {
    stream.write_all(frame.as_bytes())?;
    stream.write_all(b"\n")?;
    stream.flush()
}

fn main() {
    let arguments = env::args().skip(1).collect::<Vec<_>>();
    let run_id = argument(&arguments, "--run-id");
    let instance_id = argument(&arguments, "--runner-instance-id");
    let runtime = PathBuf::from(argument(&arguments, "--runtime-dir"));
    fs::write(RUNNER_PID, std::process::id().to_string()).unwrap();
    fs::write(ARGUMENTS, arguments.join("\n")).unwrap();
    let mut input = String::new();
    if Path::new(LAUNCH_SHUTDOWN_MODE).exists() {
        fs::write(LAUNCHED, b"").unwrap();
        while !Path::new(RELEASE).exists() {
            thread::yield_now();
        }
        std::io::stdin().read_to_string(&mut input).unwrap();
        fs::write(INPUT, input).unwrap();
        fs::write(INPUT_RECEIVED, b"").unwrap();
        while !Path::new(EXIT).exists() {
            thread::yield_now();
        }
        fs::write(DONE, b"").unwrap();
        return;
    }
    std::io::stdin().read_to_string(&mut input).unwrap();
    fs::write(INPUT, input).unwrap();
    fs::write(LAUNCHED, b"").unwrap();
    while !Path::new(RELEASE).exists() {
        thread::yield_now();
    }
    if Path::new(EXIT_BEFORE_HELLO_MODE).exists() {
        fs::write(DONE, b"").unwrap();
        return;
    }

    fs::create_dir(&runtime).unwrap();
    fs::set_permissions(&runtime, fs::Permissions::from_mode(0o700)).unwrap();
    let socket = runtime.join("control.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    fs::set_permissions(&socket, fs::Permissions::from_mode(0o600)).unwrap();
    if Path::new(PRE_HELLO_HANG_MODE).exists() {
        fs::write(SOCKET_READY, b"").unwrap();
        loop {
            thread::park();
        }
    }

    let (mut subscription, _) = listener.accept().unwrap();
    let mut request = String::new();
    BufReader::new(subscription.try_clone().unwrap())
        .read_line(&mut request)
        .unwrap();
    append_request(&request);
    if Path::new(DISCONNECT_MODE).exists() {
        let hello = format!(
            "{{\"type\":\"hello\",\"data\":{{\"protocol_version\":1,\"run_id\":\"{run_id}\",\"runner_instance_id\":\"{instance_id}\",\"runner_pid\":42,\"replay_through\":0,\"terminal_sequence\":null}}}}"
        );
        write_frame(&mut subscription, &hello).expect("hello");
        write_frame(
            &mut subscription,
            r#"{"type":"caught_up","data":{"protocol_version":1,"sequence":0}}"#,
        )
        .expect("caught-up");
        drop(subscription);
        drop(listener);
        fs::remove_file(&socket).unwrap();
        fs::write(DISCONNECTED, b"").unwrap();
        while !Path::new(EXIT).exists() {
            thread::yield_now();
        }
        fs::write(DONE, b"").unwrap();
        return;
    }
    let hello = format!(
        "{{\"type\":\"hello\",\"data\":{{\"protocol_version\":1,\"run_id\":\"{run_id}\",\"runner_instance_id\":\"{instance_id}\",\"runner_pid\":42,\"replay_through\":3,\"terminal_sequence\":3}}}}"
    );
    write_frame(&mut subscription, &hello).expect("hello");
    write_frame(
        &mut subscription,
        r#"{"type":"event","data":{"protocol_version":1,"event":{"protocol_version":1,"sequence":1,"occurred_at_ms":1001,"event":{"type":"started","data":{"child_pid":42}}}}}"#,
    )
    .expect("started");
    let claude_session_id = arguments.windows(2).find_map(|pair| {
        matches!(pair[0].as_str(), "--session-id" | "--resume")
            .then_some(pair[1].as_str())
    });
    let provider_output = if let Some(session_id) = claude_session_id {
        format!(
            "{{\"type\":\"system\",\"subtype\":\"init\",\"session_id\":\"{session_id}\",\"model\":\"claude-sonnet-4-6\",\"permissionMode\":\"acceptEdits\",\"claude_code_version\":\"2.1.233\"}}\n\
             {{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false,\"session_id\":\"{session_id}\",\"result\":\"done\",\"terminal_reason\":\"completed\",\"stop_reason\":\"end_turn\",\"permission_denials\":[],\"total_cost_usd\":0.25,\"usage\":{{\"input_tokens\":1,\"cache_creation_input_tokens\":2,\"cache_read_input_tokens\":3,\"output_tokens\":4}}}}\n"
        )
    } else {
        format!(
            "{{\"type\":\"thread.started\",\"thread_id\":\"{THREAD_ID}\"}}\n\
             {{\"type\":\"turn.completed\",\"usage\":{{\"input_tokens\":1,\"cached_input_tokens\":0,\"cache_write_input_tokens\":0,\"output_tokens\":1,\"reasoning_output_tokens\":0}}}}\n"
        )
    };
    let output = format!(
        r#"{{"type":"event","data":{{"protocol_version":1,"event":{{"protocol_version":1,"sequence":2,"occurred_at_ms":1002,"event":{{"type":"output","data":{{"stream":"stdout","text":{provider_output:?},"lossy":false}}}}}}}}}}"#
    );
    write_frame(&mut subscription, &output).expect("output");
    write_frame(
        &mut subscription,
        r#"{"type":"event","data":{"protocol_version":1,"event":{"protocol_version":1,"sequence":3,"occurred_at_ms":1003,"event":{"type":"exited","data":{"exit_code":0,"signal":null}}}}}"#,
    )
    .expect("exited");
    write_frame(
        &mut subscription,
        r#"{"type":"caught_up","data":{"protocol_version":1,"sequence":3}}"#,
    )
    .expect("caught-up");

    let (mut acknowledgement, _) = listener.accept().unwrap();
    let mut request = String::new();
    BufReader::new(acknowledgement.try_clone().unwrap())
        .read_line(&mut request)
        .unwrap();
    append_request(&request);
    write_frame(
        &mut acknowledgement,
        r#"{"type":"command_ack","data":{"protocol_version":1,"command_id":"ack-3"}}"#,
    )
    .expect("ack");
    fs::write(DONE, b"").unwrap();
}
"####;

fn id<T>(value: &str) -> T
where
    T: TryFrom<String>,
    T::Error: std::fmt::Debug,
{
    T::try_from(value.to_owned()).unwrap()
}

fn private_directory(path: &Path) {
    fs::create_dir(path).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
}

fn runner_event(sequence: i64, event: RunnerEvent) -> RunnerEventEnvelope {
    RunnerEventEnvelope {
        protocol_version: RUNNER_PROTOCOL_VERSION,
        sequence,
        occurred_at_ms: 1_000 + sequence,
        event,
    }
}

fn claude_init_event(sequence: i64) -> RunnerEventEnvelope {
    runner_event(
        sequence,
        RunnerEvent::Output {
            stream: OutputStream::Stdout,
            text: format!(
                "{}\n",
                serde_json::json!({
                    "type": "system",
                    "subtype": "init",
                    "session_id": THREAD_ID,
                    "model": "claude-sonnet-4-6",
                    "permissionMode": "acceptEdits",
                    "claude_code_version": "2.1.233",
                })
            ),
            lossy: false,
        },
    )
}

fn claude_result_event(sequence: i64) -> RunnerEventEnvelope {
    runner_event(
        sequence,
        RunnerEvent::Output {
            stream: OutputStream::Stdout,
            text: format!(
                "{}\n",
                serde_json::json!({
                    "type": "result",
                    "subtype": "success",
                    "is_error": false,
                    "session_id": THREAD_ID,
                    "result": "done",
                    "terminal_reason": "completed",
                    "stop_reason": "end_turn",
                    "permission_denials": [],
                    "total_cost_usd": 0.25,
                    "usage": {
                        "input_tokens": 1,
                        "cache_creation_input_tokens": 2,
                        "cache_read_input_tokens": 3,
                        "output_tokens": 4,
                    },
                })
            ),
            lossy: false,
        },
    )
}

struct RecoveryFixture {
    _directory: tempfile::TempDir,
    database: PathBuf,
    state: DaemonState,
    runtime_root: PathBuf,
    runtime_dir: PathBuf,
    run_id: RunId,
    runner_instance_id: RunnerInstanceId,
    baseline: i64,
}

impl RecoveryFixture {
    fn terminal(runtime_exists: bool) -> Self {
        Self::new(runtime_exists, true)
    }

    fn active(runtime_exists: bool) -> Self {
        Self::new(runtime_exists, false)
    }

    fn active_claude(runtime_exists: bool) -> Self {
        Self::new_for_provider(runtime_exists, false, Provider::ClaudeCode)
    }

    fn new(runtime_exists: bool, terminal: bool) -> Self {
        Self::new_for_provider(runtime_exists, terminal, Provider::Codex)
    }

    fn new_for_provider(runtime_exists: bool, terminal: bool, provider: Provider) -> Self {
        // macOS Unix-domain socket paths are short; keep the fixture root well
        // below that limit so a bind failure cannot masquerade as recovery.
        let directory = tempfile::tempdir_in("/tmp").unwrap();
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let database = directory.path().join("factory.db");
        let project_root = directory.path().join("project");
        private_directory(&project_root);
        let project_root = fs::canonicalize(project_root).unwrap();
        let runtime_root = directory.path().join("new-runs");
        private_directory(&runtime_root);
        let runtime_dir = directory.path().join("recovered-run");
        if runtime_exists {
            private_directory(&runtime_dir);
        }

        let project_id = id::<ProjectId>("project");
        let task_id = id::<TaskId>("task");
        let agent_id = id::<AgentId>("agent");
        let run_id = id::<RunId>("run");
        let runner_instance_id = id::<RunnerInstanceId>("instance");
        let mut store = Store::open(&database).unwrap();
        store
            .create_project(
                NewProject {
                    id: project_id.clone(),
                    name: "Project".into(),
                    root: project_root.to_str().unwrap().into(),
                },
                1,
            )
            .unwrap();
        store
            .create_task(
                NewTask {
                    id: task_id.clone(),
                    project_id: project_id.clone(),
                    parent_task_id: None,
                    title: "Task".into(),
                    body: "private task body".into(),
                    priority: 0,
                },
                2,
            )
            .unwrap();
        store
            .create_agent(
                NewAgent {
                    id: agent_id.clone(),
                    project_id: project_id.clone(),
                    parent_agent_id: None,
                    role: AgentRole::Worker,
                    provider,
                },
                3,
            )
            .unwrap();
        store
            .reserve_task_run(
                RunReservation {
                    project_id,
                    task_id,
                    agent_id,
                    expected_provider: provider,
                    run_id: run_id.clone(),
                    parent_run_id: None,
                    worktree: project_root.to_str().unwrap().into(),
                    fresh_provider_session_id: (provider == Provider::ClaudeCode)
                        .then(|| THREAD_ID.into()),
                    runner_instance_id: runner_instance_id.clone(),
                    runner_runtime: runtime_dir.to_str().unwrap().into(),
                },
                1,
                4,
            )
            .unwrap();

        if terminal {
            store
                .ingest_runner_event(
                    &run_id,
                    &runner_instance_id,
                    &runner_event(1, RunnerEvent::Started { child_pid: 42 }),
                    RunnerEventEffects {
                        confirmed_provider_session_id: None,
                        terminal_outcome: None,
                    },
                    5,
                )
                .unwrap();
            store
                .ingest_runner_event(
                    &run_id,
                    &runner_instance_id,
                    &runner_event(
                        2,
                        RunnerEvent::Output {
                            stream: OutputStream::Stdout,
                            text: format!(
                                "{{\"type\":\"thread.started\",\"thread_id\":\"{THREAD_ID}\"}}\n\
                                 {{\"type\":\"turn.completed\",\"usage\":{{\"input_tokens\":1,\"cached_input_tokens\":0,\"cache_write_input_tokens\":0,\"output_tokens\":1,\"reasoning_output_tokens\":0}}}}\n"
                            ),
                            lossy: false,
                        },
                    ),
                    RunnerEventEffects {
                        confirmed_provider_session_id: Some(THREAD_ID.into()),
                        terminal_outcome: None,
                    },
                    6,
                )
                .unwrap();
            store
                .ingest_runner_event(
                    &run_id,
                    &runner_instance_id,
                    &runner_event(
                        3,
                        RunnerEvent::Exited {
                            exit_code: Some(0),
                            signal: None,
                        },
                    ),
                    RunnerEventEffects {
                        confirmed_provider_session_id: None,
                        terminal_outcome: Some(TerminalOutcome::Succeeded { result: None }),
                    },
                    7,
                )
                .unwrap();
        }

        store
            .set_observer_health(&run_id, &runner_instance_id, ObserverHealth::Healthy, 8)
            .unwrap();

        let baseline = store.latest_event_sequence().unwrap();
        Self {
            _directory: directory,
            database,
            state: DaemonState::new(store),
            runtime_root,
            runtime_dir,
            run_id,
            runner_instance_id,
            baseline,
        }
    }

    fn config(&self, grace: Duration) -> Config {
        Config {
            runner_program: PathBuf::from("/unused/factory-runner"),
            codex_program: PathBuf::from("/unused/codex"),
            claude_program: PathBuf::from("/unused/claude"),
            claude_max_turns: NonZeroU32::new(20).unwrap(),
            claude_max_budget_cents: NonZeroU32::new(500).unwrap(),
            runtime_root: self.runtime_root.clone(),
            max_active_runs: 1,
            startup_timeout: Duration::from_secs(1),
            connect_grace: grace,
            batch_delay: Duration::from_millis(10),
        }
    }

    async fn remains_recoverable(&self) -> bool {
        let run_id = self.run_id.clone();
        self.state
            .with_store(move |store| {
                Ok(store
                    .recoverable_runs()?
                    .into_iter()
                    .any(|run| run.run.id == run_id))
            })
            .await
            .unwrap()
    }

    async fn observer_health(&self) -> Option<ObserverHealth> {
        let run_id = self.run_id.clone();
        self.state
            .with_store(move |store| {
                Ok(store
                    .recoverable_runs()?
                    .into_iter()
                    .find(|run| run.run.id == run_id)
                    .map(|run| run.run.observer_health))
            })
            .await
            .unwrap()
    }

    async fn wait_for_observer_health(&self, expected: ObserverHealth) {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            if self.observer_health().await == Some(expected) {
                return;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "timed out waiting for observer health {expected:?}"
            );
            yield_now().await;
        }
    }

    async fn drive_until_observer_health(&self, expected: ObserverHealth) {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        let virtual_deadline = tokio::time::Instant::now() + MAX_TEST_VIRTUAL_DRIVE;
        loop {
            if self.observer_health().await == Some(expected) {
                return;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "timed out driving observer health to {expected:?}"
            );
            assert!(
                tokio::time::Instant::now() < virtual_deadline,
                "observer health exceeded its virtual-time bound"
            );
            advance_and_settle(Duration::from_millis(10)).await;
        }
    }

    async fn drive_until_recoverable(&self, expected: bool) {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        let virtual_deadline = tokio::time::Instant::now() + MAX_TEST_VIRTUAL_DRIVE;
        loop {
            if self.remains_recoverable().await == expected {
                return;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "timed out waiting for recoverable={expected}"
            );
            assert!(
                tokio::time::Instant::now() < virtual_deadline,
                "recovery transition exceeded its virtual-time bound"
            );
            advance_and_settle(Duration::from_millis(10)).await;
        }
    }
}

async fn settle() {
    for _ in 0..64 {
        yield_now().await;
    }
}

async fn advance_and_settle(duration: Duration) {
    advance(duration).await;
    settle().await;
}

async fn receive_oneshot<T>(
    receiver: &mut tokio::sync::oneshot::Receiver<T>,
    description: &str,
) -> T {
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        match receiver.try_recv() {
            Ok(value) => return value,
            Err(tokio::sync::oneshot::error::TryRecvError::Empty) => {}
            Err(tokio::sync::oneshot::error::TryRecvError::Closed) => {
                panic!("{description} sender closed")
            }
        }
        assert!(
            std::time::Instant::now() < deadline,
            "timed out waiting for {description}"
        );
        yield_now().await;
    }
}

async fn receive_oneshot_while_advancing<T>(
    receiver: &mut tokio::sync::oneshot::Receiver<T>,
    description: &str,
) -> T {
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    let virtual_deadline = tokio::time::Instant::now() + MAX_TEST_VIRTUAL_DRIVE;
    loop {
        match receiver.try_recv() {
            Ok(value) => return value,
            Err(tokio::sync::oneshot::error::TryRecvError::Empty) => {}
            Err(tokio::sync::oneshot::error::TryRecvError::Closed) => {
                panic!("{description} sender closed")
            }
        }
        assert!(
            std::time::Instant::now() < deadline,
            "timed out waiting for {description}"
        );
        assert!(
            tokio::time::Instant::now() < virtual_deadline,
            "{description} exceeded its virtual-time bound"
        );
        advance_and_settle(Duration::from_millis(10)).await;
    }
}

async fn receive_mpsc_while_advancing<T>(
    receiver: &mut tokio::sync::mpsc::Receiver<T>,
    description: &str,
    max_virtual_wait: Duration,
) -> T {
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    let virtual_deadline = tokio::time::Instant::now() + max_virtual_wait;
    loop {
        match receiver.try_recv() {
            Ok(value) => return value,
            Err(tokio::sync::mpsc::error::TryRecvError::Empty) => {}
            Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => {
                panic!("{description} sender closed")
            }
        }
        assert!(
            std::time::Instant::now() < deadline,
            "timed out waiting for {description}"
        );
        assert!(
            tokio::time::Instant::now() < virtual_deadline,
            "{description} exceeded its virtual-time bound"
        );
        advance_and_settle(Duration::from_millis(10)).await;
    }
}

async fn stop(
    handle: execution::Handle,
    join: tokio::task::JoinHandle<Result<(), execution::Error>>,
) {
    handle.shutdown().await.unwrap();
    join.await.unwrap().unwrap();
}

struct QueuedFixture {
    _directory: tempfile::TempDir,
    state: DaemonState,
    runtime_root: PathBuf,
    project_root: PathBuf,
    project_id: ProjectId,
    task_id: TaskId,
    agent_id: AgentId,
    baseline: i64,
}

impl QueuedFixture {
    fn new() -> Self {
        Self::with_body("private queued task body".into())
    }

    fn with_body(body: String) -> Self {
        Self::with_provider(body, Provider::Codex, false)
    }

    fn fresh_claude() -> Self {
        Self::with_provider(
            "private fresh Claude task body".into(),
            Provider::ClaudeCode,
            false,
        )
    }

    fn adopted_claude() -> Self {
        Self::with_provider(
            "private adopted Claude task body".into(),
            Provider::ClaudeCode,
            true,
        )
    }

    fn with_provider(body: String, provider: Provider, adopted: bool) -> Self {
        let directory = tempfile::tempdir_in("/tmp").unwrap();
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let database = directory.path().join("factory.db");
        let project_root = directory.path().join("project");
        private_directory(&project_root);
        let project_root = fs::canonicalize(project_root).unwrap();
        let runtime_root = directory.path().join("runs");
        private_directory(&runtime_root);
        let project_id = id::<ProjectId>("queued-project");
        let task_id = id::<TaskId>("queued-task");
        let agent_id = id::<AgentId>("queued-agent");
        let mut store = Store::open(database).unwrap();
        store
            .create_project(
                NewProject {
                    id: project_id.clone(),
                    name: "Queued Project".into(),
                    root: project_root.to_str().unwrap().into(),
                },
                1,
            )
            .unwrap();
        store
            .create_task(
                NewTask {
                    id: task_id.clone(),
                    project_id: project_id.clone(),
                    parent_task_id: None,
                    title: "Queued Task".into(),
                    body,
                    priority: 0,
                },
                2,
            )
            .unwrap();
        let agent = NewAgent {
            id: agent_id.clone(),
            project_id: project_id.clone(),
            parent_agent_id: None,
            role: AgentRole::Worker,
            provider,
        };
        if adopted {
            store
                .adopt_agent(
                    agent,
                    AdoptedProviderSession::ClaudeCode {
                        session_id: THREAD_ID.into(),
                        cwd: project_root.to_str().unwrap().into(),
                    },
                    3,
                )
                .unwrap();
        } else {
            store.create_agent(agent, 3).unwrap();
        }
        let baseline = store.latest_event_sequence().unwrap();
        Self {
            _directory: directory,
            state: DaemonState::new(store),
            runtime_root,
            project_root,
            project_id,
            task_id,
            agent_id,
            baseline,
        }
    }

    fn config(&self) -> Config {
        Config {
            runner_program: PathBuf::from("/unused/factory-runner"),
            codex_program: PathBuf::from("/unused/codex"),
            claude_program: PathBuf::from("/unused/claude"),
            claude_max_turns: NonZeroU32::new(20).unwrap(),
            claude_max_budget_cents: NonZeroU32::new(500).unwrap(),
            runtime_root: self.runtime_root.clone(),
            max_active_runs: 1,
            startup_timeout: Duration::from_secs(1),
            connect_grace: Duration::from_millis(100),
            batch_delay: Duration::from_millis(10),
        }
    }

    async fn assert_unchanged(&self) {
        let project = self.project_id.clone();
        let task = self.task_id.clone();
        let (head, task) = self
            .state
            .with_store(move |store| {
                let task = store
                    .list_tasks(&project, None, 10)?
                    .into_iter()
                    .find(|detail| detail.snapshot.id == task)
                    .unwrap();
                Ok((store.latest_event_sequence()?, task))
            })
            .await
            .unwrap();
        assert_eq!(head, self.baseline);
        assert_eq!(task.snapshot.status, factory_core::TaskStatus::Queued);
        assert!(task.snapshot.assigned_agent_id.is_none());
    }
}

struct ScriptedRunner {
    program: PathBuf,
    codex_provider: PathBuf,
    claude_provider: PathBuf,
    launched: PathBuf,
    release: PathBuf,
    done: PathBuf,
    requests: PathBuf,
    arguments: PathBuf,
    input: PathBuf,
    disconnect_mode: PathBuf,
    disconnected: PathBuf,
    launch_shutdown_mode: PathBuf,
    input_received: PathBuf,
    exit_before_hello_mode: PathBuf,
    pre_hello_hang_mode: PathBuf,
    runner_pid: PathBuf,
    socket_ready: PathBuf,
    exit: PathBuf,
}

impl ScriptedRunner {
    fn compile(directory: &Path) -> Self {
        let source = directory.join("scripted-runner.rs");
        let program = directory.join("scripted-runner");
        let codex_provider = directory.join("fake-codex");
        let claude_provider = directory.join("fake-claude");
        let launched = directory.join("runner-launched");
        let release = directory.join("release-runner");
        let done = directory.join("runner-done");
        let requests = directory.join("runner-requests.jsonl");
        let arguments = directory.join("runner-arguments.txt");
        let input = directory.join("runner-input.txt");
        let disconnect_mode = directory.join("disconnect-mode");
        let disconnected = directory.join("runner-disconnected");
        let launch_shutdown_mode = directory.join("launch-shutdown-mode");
        let input_received = directory.join("runner-input-received");
        let exit_before_hello_mode = directory.join("exit-before-hello-mode");
        let pre_hello_hang_mode = directory.join("pre-hello-hang-mode");
        let runner_pid = directory.join("runner-pid");
        let socket_ready = directory.join("runner-socket-ready");
        let exit = directory.join("exit-runner");

        let source_text = SCRIPTED_RUNNER_SOURCE
            .replace("__LAUNCHED__", &format!("{launched:?}"))
            .replace("__RELEASE__", &format!("{release:?}"))
            .replace("__DONE__", &format!("{done:?}"))
            .replace("__REQUESTS__", &format!("{requests:?}"))
            .replace("__ARGUMENTS__", &format!("{arguments:?}"))
            .replace("__INPUT__", &format!("{input:?}"))
            .replace("__DISCONNECT_MODE__", &format!("{disconnect_mode:?}"))
            .replace("__DISCONNECTED__", &format!("{disconnected:?}"))
            .replace(
                "__LAUNCH_SHUTDOWN_MODE__",
                &format!("{launch_shutdown_mode:?}"),
            )
            .replace("__INPUT_RECEIVED__", &format!("{input_received:?}"))
            .replace(
                "__EXIT_BEFORE_HELLO_MODE__",
                &format!("{exit_before_hello_mode:?}"),
            )
            .replace(
                "__PRE_HELLO_HANG_MODE__",
                &format!("{pre_hello_hang_mode:?}"),
            )
            .replace("__RUNNER_PID__", &format!("{runner_pid:?}"))
            .replace("__SOCKET_READY__", &format!("{socket_ready:?}"))
            .replace("__EXIT__", &format!("{exit:?}"));
        fs::write(&source, source_text).unwrap();
        let compiled = std::process::Command::new("rustc")
            .arg("--edition=2024")
            .arg(&source)
            .arg("-o")
            .arg(&program)
            .output()
            .unwrap();
        assert!(
            compiled.status.success(),
            "scripted runner compilation failed: {}",
            String::from_utf8_lossy(&compiled.stderr)
        );
        for provider in [&codex_provider, &claude_provider] {
            fs::write(provider, "#!/bin/sh\nexit 0\n").unwrap();
            fs::set_permissions(provider, fs::Permissions::from_mode(0o700)).unwrap();
        }
        Self {
            program,
            codex_provider,
            claude_provider,
            launched,
            release,
            done,
            requests,
            arguments,
            input,
            disconnect_mode,
            disconnected,
            launch_shutdown_mode,
            input_received,
            exit_before_hello_mode,
            pre_hello_hang_mode,
            runner_pid,
            socket_ready,
            exit,
        }
    }
}

async fn wait_for_path(path: &Path) {
    let path = path.to_owned();
    tokio::task::spawn_blocking(move || {
        let deadline = std::time::Instant::now() + Duration::from_secs(15);
        while !path.exists() {
            assert!(
                std::time::Instant::now() < deadline,
                "timed out waiting for scripted runner marker"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    })
    .await
    .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fresh_start_crosses_real_process_boundary_and_completes_durably() {
    let fixture = QueuedFixture::new();
    let scripted = ScriptedRunner::compile(fixture._directory.path());
    let mut config = fixture.config();
    config.runner_program = scripted.program.clone();
    config.codex_program = scripted.codex_provider.clone();
    config.connect_grace = Duration::from_secs(5);
    let (handle, join) = execution::spawn(config, fixture.state.clone()).unwrap();

    let start_handle = handle.clone();
    let project_id = fixture.project_id.clone();
    let task_id = fixture.task_id.clone();
    let agent_id = fixture.agent_id.clone();
    let worktree = fixture.project_root.clone();
    let start = tokio::spawn(async move {
        start_handle
            .start_task(StartTask {
                project_id,
                task_id,
                agent_id,
                parent_run_id: None,
                worktree,
            })
            .await
    });

    wait_for_path(&scripted.launched).await;
    let started = timeout(Duration::from_secs(5), start)
        .await
        .expect("durable start did not return before runner readiness")
        .unwrap()
        .unwrap();
    assert!(
        !scripted.release.exists(),
        "the scripted runner was released before durable acceptance"
    );
    let project_id = fixture.project_id.clone();
    let (task, pre_ready_events) = fixture
        .state
        .with_store(move |store| {
            Ok((
                store.list_tasks(&project_id, None, 10)?.remove(0),
                store.events_after(fixture.baseline, 100)?,
            ))
        })
        .await
        .unwrap();
    assert_eq!(task.snapshot.status, factory_core::TaskStatus::Running);
    assert_eq!(
        task.snapshot.assigned_agent_id.as_ref(),
        Some(&fixture.agent_id)
    );
    let pre_ready_statuses = pre_ready_events
        .iter()
        .filter_map(|event| match &event.event {
            FactoryEvent::RunChanged { run } if run.id == started.run_id => Some(run.status),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(pre_ready_statuses, [RunStatus::Starting]);

    fs::write(&scripted.release, b"").unwrap();
    wait_for_path(&scripted.done).await;
    timeout(Duration::from_secs(5), async {
        while fixture
            .state
            .with_store(|store| Ok(!store.recoverable_runs()?.is_empty()))
            .await
            .unwrap()
        {
            yield_now().await;
        }
    })
    .await
    .expect("terminal ack was not durably reconciled");

    let requests = fs::read_to_string(&scripted.requests).unwrap();
    let requests = requests
        .lines()
        .map(|line| serde_json::from_str::<RequestEnvelope>(line).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0].run_id, started.run_id);
    assert_eq!(requests[1].run_id, started.run_id);
    assert_eq!(
        requests[0].runner_instance_id,
        requests[1].runner_instance_id
    );
    assert_eq!(
        requests[0].request,
        RunnerRequest::Subscribe { after_sequence: 0 }
    );
    assert_eq!(
        requests[1].request,
        RunnerRequest::AcknowledgeExit {
            command_id: "ack-3".into(),
            terminal_sequence: 3,
        }
    );
    assert_eq!(
        fs::read_to_string(&scripted.input).unwrap(),
        "private queued task body"
    );
    let arguments = fs::read_to_string(&scripted.arguments).unwrap();
    assert!(!arguments.contains("private queued task body"));

    let baseline = fixture.baseline;
    let events = fixture
        .state
        .with_store(move |store| store.events_after(baseline, 100))
        .await
        .unwrap();
    let statuses = events
        .iter()
        .filter_map(|event| match &event.event {
            FactoryEvent::RunChanged { run } if run.id == started.run_id => Some(run.status),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        statuses,
        [
            RunStatus::Starting,
            RunStatus::Running,
            RunStatus::Succeeded,
            RunStatus::Succeeded,
        ]
    );
    let public_json = serde_json::to_string(&events).unwrap();
    for private in [
        "private queued task body",
        THREAD_ID,
        requests[0].runner_instance_id.as_str(),
    ] {
        assert!(!public_json.contains(private));
    }

    stop(handle, join).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fresh_and_adopted_claude_use_the_durable_provider_session_and_exact_cwd() {
    for adopted in [false, true] {
        let fixture = if adopted {
            QueuedFixture::adopted_claude()
        } else {
            QueuedFixture::fresh_claude()
        };
        let scripted = ScriptedRunner::compile(fixture._directory.path());
        let mut config = fixture.config();
        config.runner_program = scripted.program.clone();
        config.claude_program = scripted.claude_provider.clone();
        config.connect_grace = Duration::from_secs(5);
        let (handle, join) = execution::spawn(config, fixture.state.clone()).unwrap();

        let started = handle
            .start_task(StartTask {
                project_id: fixture.project_id.clone(),
                task_id: fixture.task_id.clone(),
                agent_id: fixture.agent_id.clone(),
                parent_run_id: None,
                worktree: fixture.project_root.clone(),
            })
            .await
            .unwrap();
        wait_for_path(&scripted.launched).await;

        let arguments = fs::read_to_string(&scripted.arguments).unwrap();
        let arguments = arguments.lines().collect::<Vec<_>>();
        let session_flag = if adopted { "--resume" } else { "--session-id" };
        let session_index = arguments
            .iter()
            .position(|argument| *argument == session_flag)
            .unwrap();
        let launched_session = arguments[session_index + 1];
        if adopted {
            assert_eq!(launched_session, THREAD_ID);
            assert!(!arguments.contains(&"--session-id"));
        } else {
            assert_eq!(
                Uuid::parse_str(launched_session).unwrap().to_string(),
                launched_session
            );
            assert!(!arguments.contains(&"--resume"));
        }
        for fixed in [
            ["--max-turns", "20"],
            ["--max-budget-usd", "5.00"],
            ["--permission-mode", "acceptEdits"],
        ] {
            assert!(arguments.windows(2).any(|pair| pair == fixed));
        }

        let run_id = started.run_id.clone();
        let (stored_provider, stored_session, resumes) = fixture
            .state
            .with_store(move |store| {
                let target = store.execution_target(&run_id)?;
                Ok((
                    target.provider,
                    target.provider_session_id,
                    target.resumes_provider_session,
                ))
            })
            .await
            .unwrap();
        assert_eq!(stored_provider, Provider::ClaudeCode);
        assert_eq!(stored_session.as_deref(), Some(launched_session));
        assert_eq!(resumes, adopted);

        fs::write(&scripted.release, b"").unwrap();
        wait_for_path(&scripted.done).await;
        timeout(Duration::from_secs(5), async {
            while fixture
                .state
                .with_store(|store| Ok(!store.recoverable_runs()?.is_empty()))
                .await
                .unwrap()
            {
                yield_now().await;
            }
        })
        .await
        .expect("Claude terminal acknowledgement was not durably reconciled");
        let expected_input = if adopted {
            "private adopted Claude task body"
        } else {
            "private fresh Claude task body"
        };
        assert_eq!(fs::read_to_string(&scripted.input).unwrap(), expected_input);
        let task_id = fixture.task_id.clone();
        let result = fixture
            .state
            .with_store(move |store| {
                Ok(store
                    .list_tasks(&fixture.project_id, None, 10)?
                    .into_iter()
                    .find(|task| task.snapshot.id == task_id)
                    .and_then(|task| task.result))
            })
            .await
            .unwrap();
        assert_eq!(result.as_deref(), Some("done"));
        let baseline = fixture.baseline;
        let public = fixture
            .state
            .with_store(move |store| {
                Ok(serde_json::to_string(&store.events_after(baseline, 100)?)?)
            })
            .await
            .unwrap();
        assert!(!public.contains(launched_session));
        assert!(!public.contains(expected_input));

        stop(handle, join).await;
    }
}

#[tokio::test(start_paused = true)]
async fn startup_input_timeout_kills_the_unready_wrapper_and_allows_shutdown() {
    let instructions = "x".repeat(factory_core::runner::MAX_STARTUP_STDIN_BYTES);
    let fixture = QueuedFixture::with_body(instructions.clone());
    let scripted = ScriptedRunner::compile(fixture._directory.path());
    fs::write(&scripted.launch_shutdown_mode, b"").unwrap();
    let mut config = fixture.config();
    config.runner_program = scripted.program.clone();
    config.codex_program = scripted.codex_provider.clone();
    config.startup_timeout = Duration::from_millis(50);
    let (handle, join) = execution::spawn(config, fixture.state.clone()).unwrap();

    let started = handle
        .start_task(StartTask {
            project_id: fixture.project_id.clone(),
            task_id: fixture.task_id.clone(),
            agent_id: fixture.agent_id.clone(),
            parent_run_id: None,
            worktree: fixture.project_root.clone(),
        })
        .await
        .unwrap();
    wait_for_path(&scripted.launched).await;

    let shutdown = tokio::spawn({
        let handle = handle.clone();
        async move { handle.shutdown().await }
    });
    for _ in 0..64 {
        yield_now().await;
    }
    assert!(
        !shutdown.is_finished(),
        "shutdown cancelled the in-flight startup-input transfer"
    );

    for _ in 0..10 {
        advance_and_settle(Duration::from_millis(50)).await;
        if shutdown.is_finished() {
            break;
        }
    }
    tokio::time::resume();
    timeout(Duration::from_secs(5), shutdown)
        .await
        .expect("a wedged startup-input transfer blocked daemon shutdown")
        .unwrap()
        .unwrap();
    join.await.unwrap().unwrap();
    let raw_pid = fs::read_to_string(&scripted.runner_pid)
        .unwrap()
        .parse::<i32>()
        .unwrap();
    let pid = rustix::process::Pid::from_raw(raw_pid).unwrap();
    let was_alive_after_shutdown = rustix::process::test_kill_process(pid).is_ok();
    if was_alive_after_shutdown {
        fs::write(&scripted.release, b"").unwrap();
        wait_for_path(&scripted.input_received).await;
        fs::write(&scripted.exit, b"").unwrap();
        wait_for_path(&scripted.done).await;
    }
    assert!(
        !was_alive_after_shutdown,
        "startup timeout did not kill and reap the unready wrapper"
    );
    assert!(rustix::process::test_kill_process(pid).is_err());
    let events = fixture
        .state
        .with_store(move |store| store.events_after(fixture.baseline, 100))
        .await
        .unwrap();
    assert!(matches!(
        events.last().map(|event| &event.event),
        Some(FactoryEvent::RunChanged { run })
            if run.id == started.run_id
                && run.status == RunStatus::Failed
                && run.failure_reason == Some(factory_core::RunFailureReason::Spawn)
    ));
}

#[tokio::test(start_paused = true)]
async fn pre_hello_child_exit_with_stale_socket_is_a_spawn_failure() {
    let fixture = QueuedFixture::new();
    let scripted = ScriptedRunner::compile(fixture._directory.path());
    fs::write(&scripted.exit_before_hello_mode, b"").unwrap();
    let mut config = fixture.config();
    config.runner_program = scripted.program.clone();
    config.codex_program = scripted.codex_provider.clone();
    config.connect_grace = Duration::from_millis(50);
    let (handle, join) = execution::spawn(config, fixture.state.clone()).unwrap();

    let started = handle
        .start_task(StartTask {
            project_id: fixture.project_id.clone(),
            task_id: fixture.task_id.clone(),
            agent_id: fixture.agent_id.clone(),
            parent_run_id: None,
            worktree: fixture.project_root.clone(),
        })
        .await
        .unwrap();
    wait_for_path(&scripted.launched).await;
    let runtime_dir = fixture.runtime_root.join(started.run_id.as_str());
    private_directory(&runtime_dir);
    let socket = runtime_dir.join("control.sock");
    let stale = std::os::unix::net::UnixListener::bind(&socket).unwrap();
    fs::set_permissions(&socket, fs::Permissions::from_mode(0o600)).unwrap();
    drop(stale);
    fs::write(&scripted.release, b"").unwrap();
    wait_for_path(&scripted.done).await;

    for _ in 0..10 {
        advance_and_settle(Duration::from_millis(100)).await;
        let run_id = started.run_id.clone();
        if !fixture
            .state
            .with_store(move |store| {
                Ok(store
                    .recoverable_runs()?
                    .into_iter()
                    .any(|run| run.run.id == run_id))
            })
            .await
            .unwrap()
        {
            break;
        }
    }
    let run_id = started.run_id.clone();
    let (recoverable, events) = fixture
        .state
        .with_store(move |store| {
            Ok((
                store
                    .recoverable_runs()?
                    .into_iter()
                    .any(|run| run.run.id == run_id),
                store.events_after(fixture.baseline, 100)?,
            ))
        })
        .await
        .unwrap();
    assert!(!recoverable);
    assert!(matches!(
        events.last().map(|event| &event.event),
        Some(FactoryEvent::RunChanged { run })
            if run.id == started.run_id
                && run.status == RunStatus::Failed
                && run.failure_reason == Some(factory_core::RunFailureReason::Spawn)
    ));

    stop(handle, join).await;
}

#[tokio::test(start_paused = true)]
async fn signalled_pre_hello_wrapper_does_not_release_potentially_live_provider_work() {
    let fixture = QueuedFixture::new();
    let scripted = ScriptedRunner::compile(fixture._directory.path());
    fs::write(&scripted.pre_hello_hang_mode, b"").unwrap();
    let mut config = fixture.config();
    config.runner_program = scripted.program.clone();
    config.codex_program = scripted.codex_provider.clone();
    config.connect_grace = Duration::from_millis(50);
    let (handle, join) = execution::spawn(config, fixture.state.clone()).unwrap();

    let started = handle
        .start_task(StartTask {
            project_id: fixture.project_id.clone(),
            task_id: fixture.task_id.clone(),
            agent_id: fixture.agent_id.clone(),
            parent_run_id: None,
            worktree: fixture.project_root.clone(),
        })
        .await
        .unwrap();
    fs::write(&scripted.release, b"").unwrap();
    wait_for_path(&scripted.socket_ready).await;
    let raw_pid = fs::read_to_string(&scripted.runner_pid)
        .unwrap()
        .parse::<i32>()
        .unwrap();
    let pid = rustix::process::Pid::from_raw(raw_pid).unwrap();
    rustix::process::kill_process(pid, rustix::process::Signal::KILL).unwrap();

    for _ in 0..10 {
        advance_and_settle(Duration::from_millis(100)).await;
    }
    let run_id = started.run_id.clone();
    let (health, events) = fixture
        .state
        .with_store(move |store| {
            let recovery = store
                .recoverable_runs()?
                .into_iter()
                .find(|run| run.run.id == run_id);
            Ok((
                recovery.map(|run| run.run.observer_health),
                store.events_after(fixture.baseline, 100)?,
            ))
        })
        .await
        .unwrap();
    assert_eq!(health, Some(ObserverHealth::Degraded));
    let health_changes = events
        .iter()
        .filter_map(|event| match &event.event {
            FactoryEvent::RunChanged { run } if run.id == started.run_id => {
                Some(run.observer_health)
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        health_changes,
        [ObserverHealth::Unknown, ObserverHealth::Degraded]
    );

    stop(handle, join).await;

    let runtime = fixture.runtime_root.join(started.run_id.as_str());
    if runtime.exists() {
        fs::remove_dir_all(runtime).unwrap();
    }
    let mut recovery_config = fixture.config();
    recovery_config.runner_program = scripted.program.clone();
    recovery_config.codex_program = scripted.codex_provider.clone();
    recovery_config.connect_grace = Duration::from_millis(50);
    let (recovery_handle, recovery_join) =
        execution::spawn(recovery_config, fixture.state.clone()).unwrap();
    for _ in 0..10 {
        advance_and_settle(Duration::from_millis(100)).await;
    }
    let run_id = started.run_id.clone();
    let recovery = fixture
        .state
        .with_store(move |store| {
            Ok(store
                .recoverable_runs()?
                .into_iter()
                .find(|run| run.run.id == run_id)
                .map(|run| (run.run.status, run.run.observer_health)))
        })
        .await
        .unwrap();
    assert_eq!(
        recovery,
        Some((RunStatus::Starting, ObserverHealth::Degraded))
    );
    stop(recovery_handle, recovery_join).await;
}

#[tokio::test(start_paused = true)]
async fn signalled_authenticated_wrapper_keeps_the_run_assigned_and_recoverable() {
    let fixture = QueuedFixture::new();
    let scripted = ScriptedRunner::compile(fixture._directory.path());
    fs::write(&scripted.disconnect_mode, b"").unwrap();
    let mut config = fixture.config();
    config.runner_program = scripted.program.clone();
    config.codex_program = scripted.codex_provider.clone();
    config.connect_grace = Duration::from_millis(50);
    let (handle, join) = execution::spawn(config, fixture.state.clone()).unwrap();

    let started = handle
        .start_task(StartTask {
            project_id: fixture.project_id.clone(),
            task_id: fixture.task_id.clone(),
            agent_id: fixture.agent_id.clone(),
            parent_run_id: None,
            worktree: fixture.project_root.clone(),
        })
        .await
        .unwrap();
    fs::write(&scripted.release, b"").unwrap();
    let socket = fixture
        .runtime_root
        .join(started.run_id.as_str())
        .join("control.sock");
    for _ in 0..100_000 {
        if socket.exists() || scripted.disconnected.exists() {
            break;
        }
        yield_now().await;
    }
    assert!(socket.exists() || scripted.disconnected.exists());
    advance_and_settle(Duration::from_millis(25)).await;
    wait_for_path(&scripted.disconnected).await;
    let raw_pid = fs::read_to_string(&scripted.runner_pid)
        .unwrap()
        .parse::<i32>()
        .unwrap();
    let pid = rustix::process::Pid::from_raw(raw_pid).unwrap();
    rustix::process::kill_process(pid, rustix::process::Signal::KILL).unwrap();

    for _ in 0..10 {
        advance_and_settle(Duration::from_millis(100)).await;
    }
    let run_id = started.run_id.clone();
    let (health, events) = fixture
        .state
        .with_store(move |store| {
            let recovery = store
                .recoverable_runs()?
                .into_iter()
                .find(|run| run.run.id == run_id);
            Ok((
                recovery.map(|run| run.run.observer_health),
                store.events_after(fixture.baseline, 100)?,
            ))
        })
        .await
        .unwrap();
    assert_eq!(health, Some(ObserverHealth::Degraded));
    let health_changes = events
        .iter()
        .filter_map(|event| match &event.event {
            FactoryEvent::RunChanged { run } if run.id == started.run_id => {
                Some(run.observer_health)
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        health_changes,
        [
            ObserverHealth::Unknown,
            ObserverHealth::Healthy,
            ObserverHealth::Degraded,
        ]
    );

    stop(handle, join).await;
}

#[tokio::test(start_paused = true)]
async fn authenticated_disconnect_cannot_fail_a_still_running_wrapper() {
    let fixture = QueuedFixture::new();
    let scripted = ScriptedRunner::compile(fixture._directory.path());
    fs::write(&scripted.disconnect_mode, b"").unwrap();
    let mut config = fixture.config();
    config.runner_program = scripted.program.clone();
    config.codex_program = scripted.codex_provider.clone();
    config.connect_grace = Duration::from_millis(50);
    let (handle, join) = execution::spawn(config, fixture.state.clone()).unwrap();

    let started = handle
        .start_task(StartTask {
            project_id: fixture.project_id.clone(),
            task_id: fixture.task_id.clone(),
            agent_id: fixture.agent_id.clone(),
            parent_run_id: None,
            worktree: fixture.project_root.clone(),
        })
        .await
        .unwrap();
    fs::write(&scripted.release, b"").unwrap();
    let socket = fixture
        .runtime_root
        .join(started.run_id.as_str())
        .join("control.sock");
    for _ in 0..100_000 {
        if socket.exists() || scripted.disconnected.exists() {
            break;
        }
        yield_now().await;
    }
    assert!(
        socket.exists() || scripted.disconnected.exists(),
        "scripted wrapper did not bind its control socket"
    );
    advance_and_settle(Duration::from_millis(25)).await;
    wait_for_path(&scripted.disconnected).await;
    advance_and_settle(Duration::from_millis(400)).await;
    let run_id = started.run_id.clone();
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    let virtual_deadline = tokio::time::Instant::now() + MAX_TEST_VIRTUAL_DRIVE;
    loop {
        let run_id = run_id.clone();
        let health = fixture
            .state
            .with_store(move |store| {
                Ok(store
                    .recoverable_runs()?
                    .into_iter()
                    .find(|run| run.run.id == run_id)
                    .map(|run| run.run.observer_health))
            })
            .await
            .unwrap();
        if health == Some(ObserverHealth::Degraded) {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "timed out waiting for the live wrapper to become degraded"
        );
        assert!(
            tokio::time::Instant::now() < virtual_deadline,
            "live-wrapper degradation exceeded its virtual-time bound"
        );
        advance_and_settle(Duration::from_millis(10)).await;
    }

    let run_id = started.run_id.clone();
    let baseline = fixture.baseline;
    let (health, events) = fixture
        .state
        .with_store(move |store| {
            let recovery = store
                .recoverable_runs()?
                .into_iter()
                .find(|run| run.run.id == run_id);
            Ok((
                recovery.map(|run| run.run.observer_health),
                store.events_after(baseline, 100)?,
            ))
        })
        .await
        .unwrap();
    assert_eq!(
        health,
        Some(ObserverHealth::Degraded),
        "the live wrapper was marked unverifiable"
    );
    let health_changes = events
        .iter()
        .filter_map(|event| match &event.event {
            FactoryEvent::RunChanged { run } if run.id == started.run_id => {
                Some(run.observer_health)
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        health_changes,
        [
            ObserverHealth::Unknown,
            ObserverHealth::Healthy,
            ObserverHealth::Degraded,
        ]
    );

    stop(handle, join).await;
    fs::write(&scripted.exit, b"").unwrap();
    wait_for_path(&scripted.done).await;
    let requests = fs::read_to_string(&scripted.requests).unwrap();
    assert_eq!(requests.lines().count(), 1);
}

#[tokio::test]
async fn insecure_runtime_root_is_rejected_before_db_or_event_mutation() {
    let fixture = QueuedFixture::new();
    fs::set_permissions(&fixture.runtime_root, fs::Permissions::from_mode(0o755)).unwrap();
    let result = execution::spawn(fixture.config(), fixture.state.clone());
    assert!(matches!(result, Err(execution::Error::InvalidRuntimeRoot)));
    fixture.assert_unchanged().await;
}

#[tokio::test]
async fn invalid_worktree_is_rejected_before_db_or_event_mutation() {
    let fixture = QueuedFixture::new();
    let mut published = fixture.state.subscribe();
    let (handle, join) = execution::spawn(fixture.config(), fixture.state.clone()).unwrap();
    let error = handle
        .start_task(StartTask {
            project_id: fixture.project_id.clone(),
            task_id: fixture.task_id.clone(),
            agent_id: fixture.agent_id.clone(),
            parent_run_id: None,
            worktree: fixture.project_root.join("does-not-exist"),
        })
        .await
        .unwrap_err();
    assert!(matches!(error, execution::Error::InvalidWorktree));
    assert!(matches!(
        published.try_recv(),
        Err(tokio::sync::broadcast::error::TryRecvError::Empty)
    ));
    fixture.assert_unchanged().await;
    stop(handle, join).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn post_commit_provider_launch_failure_keeps_run_id_and_fails_asynchronously() {
    let fixture = QueuedFixture::new();
    let mut config = fixture.config();
    config.runner_program = PathBuf::from("/bin/true");
    config.codex_program = fixture._directory.path().join("missing-codex");
    let (handle, join) = execution::spawn(config, fixture.state.clone()).unwrap();

    let started = timeout(
        Duration::from_secs(2),
        handle.start_task(StartTask {
            project_id: fixture.project_id.clone(),
            task_id: fixture.task_id.clone(),
            agent_id: fixture.agent_id.clone(),
            parent_run_id: None,
            worktree: fixture.project_root.clone(),
        }),
    )
    .await
    .expect("durable start was held behind provider launch")
    .expect("a post-commit launch failure was returned as request rejection");

    let baseline = fixture.baseline;
    let events = timeout(Duration::from_secs(2), async {
        loop {
            let events = fixture
                .state
                .with_store(move |store| store.events_after(baseline, 100))
                .await
                .unwrap();
            if events.len() == 6 {
                break events;
            }
            yield_now().await;
        }
    })
    .await
    .expect("launch failure did not reach durable terminal state");

    assert_eq!(events.len(), 6);
    assert!(matches!(
        &events[3].event,
        FactoryEvent::TaskChanged { task }
            if task.status == factory_core::TaskStatus::Failed
    ));
    assert!(matches!(
        &events[4].event,
        FactoryEvent::AgentChanged { agent }
            if agent.current_run_id.is_none()
    ));
    assert!(matches!(
        &events[5].event,
        FactoryEvent::RunChanged { run }
            if run.id == started.run_id
                && run.status == RunStatus::Failed
                && run.failure_reason == Some(factory_core::RunFailureReason::Spawn)
    ));

    stop(handle, join).await;
}

#[tokio::test(start_paused = true)]
async fn unknown_missing_recovery_becomes_degraded_without_releasing_work() {
    let fixture = RecoveryFixture::active(false);
    let run_id = fixture.run_id.clone();
    let runner_instance_id = fixture.runner_instance_id.clone();
    fixture
        .state
        .commit_and_publish(move |store| {
            let transition = store.set_observer_health(
                &run_id,
                &runner_instance_id,
                ObserverHealth::Unknown,
                9,
            )?;
            Ok(((), transition.events))
        })
        .await
        .unwrap();
    let baseline = fixture
        .state
        .with_store(|store| store.latest_event_sequence())
        .await
        .unwrap();
    let (handle, join) = execution::spawn(
        fixture.config(Duration::from_millis(50)),
        fixture.state.clone(),
    )
    .unwrap();

    settle().await;
    advance_and_settle(Duration::from_millis(75)).await;
    fixture
        .drive_until_observer_health(ObserverHealth::Degraded)
        .await;

    assert!(fixture.remains_recoverable().await);
    let run_id = fixture.run_id.clone();
    let (health, events) = fixture
        .state
        .with_store(move |store| {
            let health = store
                .recoverable_runs()?
                .into_iter()
                .find(|run| run.run.id == run_id)
                .expect("unknown recovery disappeared")
                .run
                .observer_health;
            Ok((health, store.events_after(baseline, 10)?))
        })
        .await
        .unwrap();
    assert_eq!(health, ObserverHealth::Degraded);
    assert_eq!(events.len(), 1);
    assert!(matches!(
        &events[0].event,
        FactoryEvent::RunChanged { run }
            if run.id == fixture.run_id
                && run.status == RunStatus::Starting
                && run.observer_health == ObserverHealth::Degraded
    ));
    stop(handle, join).await;
}

#[tokio::test(start_paused = true)]
async fn missing_active_runner_becomes_unverifiable_after_grace() {
    let fixture = RecoveryFixture::active(false);
    let mut published = fixture.state.subscribe();
    let (handle, join) = execution::spawn(
        fixture.config(Duration::from_millis(50)),
        fixture.state.clone(),
    )
    .unwrap();

    settle().await;
    advance_and_settle(Duration::from_millis(75)).await;

    fixture.drive_until_recoverable(false).await;
    let baseline = fixture.baseline;
    let events = fixture
        .state
        .with_store(move |store| store.events_after(baseline, 10))
        .await
        .unwrap();
    assert_eq!(events.len(), 3);
    assert!(events.iter().any(|event| matches!(
        &event.event,
        FactoryEvent::RunChanged { run }
            if run.status == RunStatus::Failed
                && run.failure_reason == Some(factory_core::RunFailureReason::Unverifiable)
    )));
    let mut broadcast = Vec::new();
    while let Ok(event) = published.try_recv() {
        broadcast.push(event);
    }
    assert_eq!(broadcast, events);
    stop(handle, join).await;
}

#[tokio::test(start_paused = true)]
async fn missing_terminal_runner_reconciles_without_public_state_change() {
    let fixture = RecoveryFixture::terminal(false);
    let mut published = fixture.state.subscribe();
    let (handle, join) = execution::spawn(
        fixture.config(Duration::from_millis(50)),
        fixture.state.clone(),
    )
    .unwrap();

    settle().await;
    advance_and_settle(Duration::from_millis(75)).await;

    fixture.drive_until_recoverable(false).await;
    let head = fixture
        .state
        .with_store(|store| store.latest_event_sequence())
        .await
        .unwrap();
    assert_eq!(head, fixture.baseline);
    assert!(matches!(
        published.try_recv(),
        Err(tokio::sync::broadcast::error::TryRecvError::Empty)
    ));
    stop(handle, join).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hostile_runner_hello_identity_or_protocol_cannot_mutate_execution_truth() {
    for wrong_protocol in [false, true] {
        let fixture = RecoveryFixture::active(true);
        let socket = fixture.runtime_dir.join("control.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        fs::set_permissions(&socket, fs::Permissions::from_mode(0o600)).unwrap();
        let run_id = if wrong_protocol {
            fixture.run_id.clone()
        } else {
            id::<RunId>("attacker-run")
        };
        let protocol_version = if wrong_protocol {
            RUNNER_PROTOCOL_VERSION + 1
        } else {
            RUNNER_PROTOCOL_VERSION
        };
        let instance = fixture.runner_instance_id.clone();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut reader = BufReader::new(stream);
            let mut request = String::new();
            reader.read_line(&mut request).await.unwrap();
            let request: RequestEnvelope = serde_json::from_str(&request).unwrap();
            let hello = RunnerFrame::Hello {
                protocol_version,
                run_id,
                runner_instance_id: instance,
                runner_pid: 42,
                replay_through: 0,
                terminal_sequence: None,
            };
            reader
                .get_mut()
                .write_all(&serde_json::to_vec(&hello).unwrap())
                .await
                .unwrap();
            reader.get_mut().write_all(b"\n").await.unwrap();
            let mut eof = String::new();
            reader.read_line(&mut eof).await.unwrap();
            assert!(eof.is_empty());
            request
        });
        let mut published = fixture.state.subscribe();
        let (handle, join) = execution::spawn(
            fixture.config(Duration::from_secs(1)),
            fixture.state.clone(),
        )
        .unwrap();
        let request = server.await.unwrap();
        assert_eq!(
            request,
            RequestEnvelope::new(
                fixture.run_id.clone(),
                fixture.runner_instance_id.clone(),
                RunnerRequest::Subscribe { after_sequence: 0 },
            )
        );
        settle().await;

        let remains_recoverable = fixture.remains_recoverable().await;
        let baseline = fixture.baseline;
        let health_events = fixture
            .state
            .with_store(move |store| store.events_after(baseline, 10))
            .await
            .unwrap();
        assert!(
            remains_recoverable,
            "hostile hello changed recovery membership: wrong_protocol={wrong_protocol}, baseline={}, events={health_events:?}",
            fixture.baseline,
        );
        assert_eq!(health_events.len(), 1);
        assert!(matches!(
            &health_events[0].event,
            FactoryEvent::RunChanged { run }
                if run.id == fixture.run_id
                    && run.status == RunStatus::Starting
                    && run.observer_health == ObserverHealth::Degraded
        ));
        assert_eq!(published.try_recv().unwrap(), health_events[0]);
        assert!(matches!(
            published.try_recv(),
            Err(tokio::sync::broadcast::error::TryRecvError::Empty)
        ));
        stop(handle, join).await;
    }
}

#[tokio::test(start_paused = true)]
async fn hostile_hello_is_quarantined_then_recovery_retries_from_zero() {
    let fixture = RecoveryFixture::active(true);
    let socket = fixture.runtime_dir.join("control.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    fs::set_permissions(&socket, fs::Permissions::from_mode(0o600)).unwrap();
    let expected_run = fixture.run_id.clone();
    let expected_instance = fixture.runner_instance_id.clone();
    let (hostile_tx, mut hostile_rx) = tokio::sync::oneshot::channel();
    let (attached_tx, mut attached_rx) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let (first, _) = listener.accept().await.unwrap();
        let mut first = BufReader::new(first);
        let mut first_request = String::new();
        first.read_line(&mut first_request).await.unwrap();
        let first_request: RequestEnvelope = serde_json::from_str(&first_request).unwrap();
        let hostile = RunnerFrame::Hello {
            protocol_version: RUNNER_PROTOCOL_VERSION,
            run_id: id("hostile-run"),
            runner_instance_id: expected_instance.clone(),
            runner_pid: 42,
            replay_through: 0,
            terminal_sequence: None,
        };
        first
            .get_mut()
            .write_all(&serde_json::to_vec(&hostile).unwrap())
            .await
            .unwrap();
        first.get_mut().write_all(b"\n").await.unwrap();
        drop(first);
        let _ = hostile_tx.send(());

        let (second, _) = listener.accept().await.unwrap();
        let mut second = BufReader::new(second);
        let mut second_request = String::new();
        second.read_line(&mut second_request).await.unwrap();
        let second_request: RequestEnvelope = serde_json::from_str(&second_request).unwrap();
        for frame in [
            RunnerFrame::Hello {
                protocol_version: RUNNER_PROTOCOL_VERSION,
                run_id: expected_run,
                runner_instance_id: expected_instance,
                runner_pid: 42,
                replay_through: 0,
                terminal_sequence: None,
            },
            RunnerFrame::CaughtUp {
                protocol_version: RUNNER_PROTOCOL_VERSION,
                sequence: 0,
            },
        ] {
            second
                .get_mut()
                .write_all(&serde_json::to_vec(&frame).unwrap())
                .await
                .unwrap();
            second.get_mut().write_all(b"\n").await.unwrap();
        }
        let _ = attached_tx.send((first_request, second_request));
        let mut eof = String::new();
        second.read_line(&mut eof).await.unwrap();
        assert!(eof.is_empty());
    });

    let mut published = fixture.state.subscribe();
    let (handle, join) = execution::spawn(
        fixture.config(Duration::from_millis(100)),
        fixture.state.clone(),
    )
    .unwrap();
    receive_oneshot(&mut hostile_rx, "hostile Hello delivery").await;
    fixture
        .wait_for_observer_health(ObserverHealth::Degraded)
        .await;
    let (first, second) =
        receive_oneshot_while_advancing(&mut attached_rx, "healthy recovery retry").await;
    fixture
        .wait_for_observer_health(ObserverHealth::Healthy)
        .await;
    let expected = RequestEnvelope::new(
        fixture.run_id.clone(),
        fixture.runner_instance_id.clone(),
        RunnerRequest::Subscribe { after_sequence: 0 },
    );
    assert_eq!(first, expected);
    assert_eq!(second, expected);
    assert!(fixture.remains_recoverable().await);
    let baseline = fixture.baseline;
    let health_events = fixture
        .state
        .with_store(move |store| store.events_after(baseline, 10))
        .await
        .unwrap();
    let health = health_events
        .iter()
        .filter_map(|event| match &event.event {
            FactoryEvent::RunChanged { run } if run.id == fixture.run_id => {
                Some(run.observer_health)
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(health, [ObserverHealth::Degraded, ObserverHealth::Healthy]);
    for event in &health_events {
        assert_eq!(published.try_recv().unwrap(), *event);
    }
    assert!(matches!(
        published.try_recv(),
        Err(tokio::sync::broadcast::error::TryRecvError::Empty)
    ));

    stop(handle, join).await;
    server.await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn repeated_caught_up_disconnects_back_off_instead_of_polling_forever() {
    let fixture = RecoveryFixture::active(true);
    let socket = fixture.runtime_dir.join("control.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    fs::set_permissions(&socket, fs::Permissions::from_mode(0o600)).unwrap();
    let expected_run = fixture.run_id.clone();
    let expected_instance = fixture.runner_instance_id.clone();
    let (attempt_tx, mut attempt_rx) = tokio::sync::mpsc::channel(3);
    let server = tokio::spawn(async move {
        for attempt in 1..=3 {
            let (stream, _) = listener.accept().await.unwrap();
            let mut stream = BufReader::new(stream);
            let mut request = String::new();
            stream.read_line(&mut request).await.unwrap();
            let request: RequestEnvelope = serde_json::from_str(&request).unwrap();
            assert_eq!(
                request.request,
                RunnerRequest::Subscribe { after_sequence: 0 }
            );
            let hello = RunnerFrame::Hello {
                protocol_version: RUNNER_PROTOCOL_VERSION,
                run_id: expected_run.clone(),
                runner_instance_id: expected_instance.clone(),
                runner_pid: 42,
                replay_through: 0,
                terminal_sequence: None,
            };
            for frame in [
                hello,
                RunnerFrame::CaughtUp {
                    protocol_version: RUNNER_PROTOCOL_VERSION,
                    sequence: 0,
                },
            ] {
                stream
                    .get_mut()
                    .write_all(&serde_json::to_vec(&frame).unwrap())
                    .await
                    .unwrap();
                stream.get_mut().write_all(b"\n").await.unwrap();
            }
            drop(stream);
            attempt_tx.send(attempt).await.unwrap();
        }
    });

    let (handle, join) = execution::spawn(
        fixture.config(Duration::from_millis(100)),
        fixture.state.clone(),
    )
    .unwrap();
    assert_eq!(
        receive_mpsc_while_advancing(
            &mut attempt_rx,
            "first caught-up attempt",
            Duration::from_millis(100),
        )
        .await,
        1
    );
    fixture
        .wait_for_observer_health(ObserverHealth::Degraded)
        .await;
    let first_retry_started = tokio::time::Instant::now();
    advance_and_settle(Duration::from_millis(249)).await;
    assert!(attempt_rx.try_recv().is_err());
    assert_eq!(
        receive_mpsc_while_advancing(
            &mut attempt_rx,
            "second caught-up attempt",
            Duration::from_millis(100),
        )
        .await,
        2
    );
    assert!(first_retry_started.elapsed() >= Duration::from_millis(250));
    assert!(first_retry_started.elapsed() <= Duration::from_millis(350));
    fixture
        .wait_for_observer_health(ObserverHealth::Degraded)
        .await;
    let second_retry_started = tokio::time::Instant::now();
    advance_and_settle(Duration::from_millis(499)).await;
    assert!(attempt_rx.try_recv().is_err());
    assert_eq!(
        receive_mpsc_while_advancing(
            &mut attempt_rx,
            "third caught-up attempt",
            Duration::from_millis(100),
        )
        .await,
        3
    );
    assert!(second_retry_started.elapsed() >= Duration::from_millis(500));
    assert!(second_retry_started.elapsed() <= Duration::from_millis(600));

    stop(handle, join).await;
    server.await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn stale_private_socket_refusal_is_not_proof_that_a_runner_vanished() {
    for terminal in [false, true] {
        let fixture = if terminal {
            RecoveryFixture::terminal(true)
        } else {
            RecoveryFixture::active(true)
        };
        let socket = fixture.runtime_dir.join("control.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        fs::set_permissions(&socket, fs::Permissions::from_mode(0o600)).unwrap();
        drop(listener);
        let mut published = fixture.state.subscribe();
        let (handle, join) = execution::spawn(
            fixture.config(Duration::from_millis(50)),
            fixture.state.clone(),
        )
        .unwrap();

        settle().await;
        advance_and_settle(Duration::from_millis(100)).await;
        fixture
            .drive_until_observer_health(ObserverHealth::Degraded)
            .await;

        assert!(
            fixture.remains_recoverable().await,
            "a present owner-only socket was treated as proof that the runner vanished"
        );
        let baseline = fixture.baseline;
        let health_events = fixture
            .state
            .with_store(move |store| store.events_after(baseline, 10))
            .await
            .unwrap();
        assert_eq!(health_events.len(), 1);
        assert!(matches!(
            &health_events[0].event,
            FactoryEvent::RunChanged { run }
                if run.id == fixture.run_id
                    && run.observer_health == ObserverHealth::Degraded
                    && run.status == if terminal { RunStatus::Succeeded } else { RunStatus::Starting }
        ));
        assert_eq!(published.try_recv().unwrap(), health_events[0]);
        assert!(matches!(
            published.try_recv(),
            Err(tokio::sync::broadcast::error::TryRecvError::Empty)
        ));

        fs::remove_file(&socket).unwrap();
        advance_and_settle(Duration::from_secs(2)).await;
        assert!(
            fixture.remains_recoverable().await,
            "a degraded run used disappearance as proof of termination"
        );
        let head = fixture
            .state
            .with_store(|store| store.latest_event_sequence())
            .await
            .unwrap();
        assert_eq!(head, fixture.baseline + 1);
        stop(handle, join).await;
    }
}

#[tokio::test(start_paused = true)]
async fn invalid_control_endpoint_metadata_is_not_proof_of_runner_absence() {
    let fixture = RecoveryFixture::terminal(true);
    std::os::unix::fs::symlink(
        fixture.runtime_dir.join("missing-target"),
        fixture.runtime_dir.join("control.sock"),
    )
    .unwrap();
    let (handle, join) = execution::spawn(
        fixture.config(Duration::from_millis(50)),
        fixture.state.clone(),
    )
    .unwrap();

    settle().await;
    advance_and_settle(Duration::from_millis(75)).await;

    assert!(
        fixture.remains_recoverable().await,
        "a present, invalid control endpoint was incorrectly treated as a vanished runner"
    );
    stop(handle, join).await;
}

#[tokio::test(start_paused = true)]
async fn invalid_endpoint_after_terminal_ack_failure_is_not_absence_proof() {
    let fixture = RecoveryFixture::terminal(true);
    let socket = fixture.runtime_dir.join("control.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    fs::set_permissions(&socket, fs::Permissions::from_mode(0o600)).unwrap();
    let expected_run = fixture.run_id.clone();
    let expected_instance = fixture.runner_instance_id.clone();
    let (invalid_tx, invalid_rx) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut reader = BufReader::new(stream);
        let mut request = String::new();
        reader.read_line(&mut request).await.unwrap();
        let request: RequestEnvelope = serde_json::from_str(&request).unwrap();
        assert_eq!(
            request,
            RequestEnvelope::new(
                expected_run.clone(),
                expected_instance.clone(),
                RunnerRequest::Subscribe { after_sequence: 0 },
            )
        );

        let frames = [
            RunnerFrame::Hello {
                protocol_version: RUNNER_PROTOCOL_VERSION,
                run_id: expected_run,
                runner_instance_id: expected_instance,
                runner_pid: 42,
                replay_through: 3,
                terminal_sequence: Some(3),
            },
            RunnerFrame::Event {
                protocol_version: RUNNER_PROTOCOL_VERSION,
                event: runner_event(1, RunnerEvent::Started { child_pid: 42 }),
            },
            RunnerFrame::Event {
                protocol_version: RUNNER_PROTOCOL_VERSION,
                event: runner_event(
                    2,
                    RunnerEvent::Output {
                        stream: OutputStream::Stdout,
                        text: format!(
                            "{{\"type\":\"thread.started\",\"thread_id\":\"{THREAD_ID}\"}}\n\
                             {{\"type\":\"turn.completed\",\"usage\":{{\"input_tokens\":1,\"cached_input_tokens\":0,\"cache_write_input_tokens\":0,\"output_tokens\":1,\"reasoning_output_tokens\":0}}}}\n"
                        ),
                        lossy: false,
                    },
                ),
            },
            RunnerFrame::Event {
                protocol_version: RUNNER_PROTOCOL_VERSION,
                event: runner_event(
                    3,
                    RunnerEvent::Exited {
                        exit_code: Some(0),
                        signal: None,
                    },
                ),
            },
        ];
        for frame in frames {
            reader
                .get_mut()
                .write_all(&serde_json::to_vec(&frame).unwrap())
                .await
                .unwrap();
            reader.get_mut().write_all(b"\n").await.unwrap();
        }

        // The live subscription remains valid, but the independent ack
        // connection now sees malicious/invalid endpoint metadata.
        drop(listener);
        fs::remove_file(&socket).unwrap();
        std::os::unix::fs::symlink(socket.with_extension("missing"), &socket).unwrap();
        let caught_up = RunnerFrame::CaughtUp {
            protocol_version: RUNNER_PROTOCOL_VERSION,
            sequence: 3,
        };
        reader
            .get_mut()
            .write_all(&serde_json::to_vec(&caught_up).unwrap())
            .await
            .unwrap();
        reader.get_mut().write_all(b"\n").await.unwrap();
        let _ = invalid_tx.send(());
        let mut eof = String::new();
        match reader.read_line(&mut eof).await {
            Ok(0) => {}
            Err(error) if error.kind() == std::io::ErrorKind::ConnectionReset => {}
            Ok(bytes) => panic!("subscription peer sent {bytes} unexpected bytes"),
            Err(error) => panic!("subscription close failed: {error}"),
        }
    });

    let (handle, join) = execution::spawn(
        fixture.config(Duration::from_millis(50)),
        fixture.state.clone(),
    )
    .unwrap();
    invalid_rx.await.unwrap();
    advance_and_settle(Duration::from_millis(1)).await;

    assert!(
        fixture.remains_recoverable().await,
        "invalid endpoint metadata after an ack failure was treated as proof of absence"
    );
    server.await.unwrap();
    stop(handle, join).await;
}

#[tokio::test(start_paused = true)]
async fn repeated_terminal_ack_failures_use_the_same_exponential_backoff() {
    let fixture = RecoveryFixture::terminal(true);
    let socket = fixture.runtime_dir.join("control.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    fs::set_permissions(&socket, fs::Permissions::from_mode(0o600)).unwrap();
    let expected_run = fixture.run_id.clone();
    let expected_instance = fixture.runner_instance_id.clone();
    let (attempt_tx, mut attempt_rx) = tokio::sync::mpsc::channel(3);
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut subscription = BufReader::new(stream);
        let mut request = String::new();
        subscription.read_line(&mut request).await.unwrap();
        let frames = [
            RunnerFrame::Hello {
                protocol_version: RUNNER_PROTOCOL_VERSION,
                run_id: expected_run,
                runner_instance_id: expected_instance,
                runner_pid: 42,
                replay_through: 3,
                terminal_sequence: Some(3),
            },
            RunnerFrame::Event {
                protocol_version: RUNNER_PROTOCOL_VERSION,
                event: runner_event(1, RunnerEvent::Started { child_pid: 42 }),
            },
            RunnerFrame::Event {
                protocol_version: RUNNER_PROTOCOL_VERSION,
                event: runner_event(
                    2,
                    RunnerEvent::Output {
                        stream: OutputStream::Stdout,
                        text: format!(
                            "{{\"type\":\"thread.started\",\"thread_id\":\"{THREAD_ID}\"}}\n\
                             {{\"type\":\"turn.completed\",\"usage\":{{\"input_tokens\":1,\"cached_input_tokens\":0,\"cache_write_input_tokens\":0,\"output_tokens\":1,\"reasoning_output_tokens\":0}}}}\n"
                        ),
                        lossy: false,
                    },
                ),
            },
            RunnerFrame::Event {
                protocol_version: RUNNER_PROTOCOL_VERSION,
                event: runner_event(
                    3,
                    RunnerEvent::Exited {
                        exit_code: Some(0),
                        signal: None,
                    },
                ),
            },
            RunnerFrame::CaughtUp {
                protocol_version: RUNNER_PROTOCOL_VERSION,
                sequence: 3,
            },
        ];
        for frame in frames {
            subscription
                .get_mut()
                .write_all(&serde_json::to_vec(&frame).unwrap())
                .await
                .unwrap();
            subscription.get_mut().write_all(b"\n").await.unwrap();
        }

        for attempt in 1..=3 {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ack = BufReader::new(stream);
            let mut request = String::new();
            ack.read_line(&mut request).await.unwrap();
            let request: RequestEnvelope = serde_json::from_str(&request).unwrap();
            let command_id = match request.request {
                RunnerRequest::AcknowledgeExit {
                    command_id,
                    terminal_sequence: 3,
                } => command_id,
                other => panic!("unexpected acknowledgement: {other:?}"),
            };
            if attempt == 3 {
                let response = RunnerFrame::CommandAck {
                    protocol_version: RUNNER_PROTOCOL_VERSION,
                    command_id,
                };
                ack.get_mut()
                    .write_all(&serde_json::to_vec(&response).unwrap())
                    .await
                    .unwrap();
                ack.get_mut().write_all(b"\n").await.unwrap();
            }
            drop(ack);
            attempt_tx.send(attempt).await.unwrap();
        }
    });

    let (handle, join) = execution::spawn(
        fixture.config(Duration::from_millis(100)),
        fixture.state.clone(),
    )
    .unwrap();
    assert_eq!(attempt_rx.recv().await, Some(1));
    settle().await;
    advance_and_settle(Duration::from_millis(249)).await;
    assert!(attempt_rx.try_recv().is_err());
    advance_and_settle(Duration::from_millis(1)).await;
    assert_eq!(attempt_rx.try_recv(), Ok(2));
    settle().await;
    advance_and_settle(Duration::from_millis(499)).await;
    assert!(attempt_rx.try_recv().is_err());
    advance_and_settle(Duration::from_millis(1)).await;
    assert_eq!(attempt_rx.try_recv(), Ok(3));
    server.await.unwrap();
    settle().await;
    assert!(!fixture.remains_recoverable().await);
    stop(handle, join).await;
}

#[tokio::test(start_paused = true)]
async fn delayed_socket_within_grace_attaches_and_always_subscribes_from_zero() {
    let fixture = RecoveryFixture::active(false);
    let (handle, join) = execution::spawn(
        fixture.config(Duration::from_millis(100)),
        fixture.state.clone(),
    )
    .unwrap();

    // Prove at least one absent-endpoint retry before making the runner
    // reachable. The deterministic 25 ms advance matches the actor's fixed
    // retry cadence and remains inside the 100 ms grace.
    settle().await;
    advance_and_settle(Duration::from_millis(25)).await;
    private_directory(&fixture.runtime_dir);
    let listener = UnixListener::bind(fixture.runtime_dir.join("control.sock")).unwrap();
    fs::set_permissions(
        fixture.runtime_dir.join("control.sock"),
        fs::Permissions::from_mode(0o600),
    )
    .unwrap();
    let expected_run = fixture.run_id.clone();
    let expected_instance = fixture.runner_instance_id.clone();
    let (attached_tx, attached_rx) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut reader = BufReader::new(stream);
        let mut request = String::new();
        reader.read_line(&mut request).await.unwrap();
        let request: RequestEnvelope = serde_json::from_str(&request).unwrap();
        let hello = RunnerFrame::Hello {
            protocol_version: RUNNER_PROTOCOL_VERSION,
            run_id: expected_run,
            runner_instance_id: expected_instance,
            runner_pid: 42,
            replay_through: 0,
            terminal_sequence: None,
        };
        let caught_up = RunnerFrame::CaughtUp {
            protocol_version: RUNNER_PROTOCOL_VERSION,
            sequence: 0,
        };
        for frame in [hello, caught_up] {
            reader
                .get_mut()
                .write_all(&serde_json::to_vec(&frame).unwrap())
                .await
                .unwrap();
            reader.get_mut().write_all(b"\n").await.unwrap();
        }
        let _ = attached_tx.send(request);
        let mut eof = String::new();
        reader.read_line(&mut eof).await.unwrap();
    });

    advance_and_settle(Duration::from_millis(25)).await;
    let request = attached_rx.await.unwrap();
    assert_eq!(
        request,
        RequestEnvelope::new(
            fixture.run_id.clone(),
            fixture.runner_instance_id.clone(),
            RunnerRequest::Subscribe { after_sequence: 0 },
        )
    );
    assert!(fixture.remains_recoverable().await);

    stop(handle, join).await;
    server.await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn durable_capacity_rejects_start_without_an_in_memory_holding_queue() {
    let fixture = RecoveryFixture::active(true);
    let socket = fixture.runtime_dir.join("control.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    fs::set_permissions(&socket, fs::Permissions::from_mode(0o600)).unwrap();
    let expected_run = fixture.run_id.clone();
    let expected_instance = fixture.runner_instance_id.clone();
    let (attached_tx, attached_rx) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut reader = BufReader::new(stream);
        let mut request = String::new();
        reader.read_line(&mut request).await.unwrap();
        let request: RequestEnvelope = serde_json::from_str(&request).unwrap();
        for frame in [
            RunnerFrame::Hello {
                protocol_version: RUNNER_PROTOCOL_VERSION,
                run_id: expected_run,
                runner_instance_id: expected_instance,
                runner_pid: 42,
                replay_through: 0,
                terminal_sequence: None,
            },
            RunnerFrame::CaughtUp {
                protocol_version: RUNNER_PROTOCOL_VERSION,
                sequence: 0,
            },
        ] {
            reader
                .get_mut()
                .write_all(&serde_json::to_vec(&frame).unwrap())
                .await
                .unwrap();
            reader.get_mut().write_all(b"\n").await.unwrap();
        }
        let _ = attached_tx.send(());
        let mut eof = String::new();
        reader.read_line(&mut eof).await.unwrap();
        request
    });
    let (handle, join) = execution::spawn(
        fixture.config(Duration::from_secs(1)),
        fixture.state.clone(),
    )
    .unwrap();
    attached_rx.await.unwrap();

    let first_overflow = timeout(
        Duration::from_secs(2),
        handle.start_task(StartTask {
            project_id: id("project"),
            task_id: id("task"),
            agent_id: id("agent"),
            parent_run_id: None,
            worktree: fixture._directory.path().join("project"),
        }),
    )
    .await
    .expect("SQLite capacity rejection was held behind recovery observation");
    assert!(matches!(
        first_overflow,
        Err(execution::Error::State(DaemonStateError::Store(
            StoreError::CapacityReached { limit: 1 }
        )))
    ));

    stop(handle, join).await;
    let request = server.await.unwrap();
    assert_eq!(
        request,
        RequestEnvelope::new(
            fixture.run_id.clone(),
            fixture.runner_instance_id.clone(),
            RunnerRequest::Subscribe { after_sequence: 0 },
        )
    );
    assert!(fixture.remains_recoverable().await);
    let head = fixture
        .state
        .with_store(|store| store.latest_event_sequence())
        .await
        .unwrap();
    assert_eq!(head, fixture.baseline);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn fixed_start_attempt_bound_returns_backpressure_without_waiting_for_store() {
    let fixture = QueuedFixture::new();
    let (handle, join) = execution::spawn(fixture.config(), fixture.state.clone()).unwrap();

    // This reply proves actor startup (including its one recovery query) has
    // completed before the store is deliberately held below.
    let initialized = handle
        .start_task(StartTask {
            project_id: fixture.project_id.clone(),
            task_id: fixture.task_id.clone(),
            agent_id: fixture.agent_id.clone(),
            parent_run_id: None,
            worktree: fixture.project_root.join("missing"),
        })
        .await;
    assert!(matches!(
        initialized,
        Err(execution::Error::InvalidWorktree)
    ));
    settle().await;

    let (entered_tx, entered_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let store_state = fixture.state.clone();
    let held_store = tokio::spawn(async move {
        store_state
            .with_store(move |_| {
                entered_tx.send(()).unwrap();
                release_rx.recv().unwrap();
                Ok(())
            })
            .await
    });
    tokio::task::spawn_blocking(move || entered_rx.recv())
        .await
        .unwrap()
        .unwrap();

    let mut starts = tokio::task::JoinSet::new();
    for _ in 0..40 {
        let caller = handle.clone();
        let project_id = fixture.project_id.clone();
        let task_id = fixture.task_id.clone();
        let agent_id = fixture.agent_id.clone();
        let worktree = fixture.project_root.clone();
        starts.spawn(async move {
            caller
                .start_task(StartTask {
                    project_id,
                    task_id,
                    agent_id,
                    parent_run_id: None,
                    worktree,
                })
                .await
        });
    }
    let overflow = timeout(Duration::from_secs(2), starts.join_next())
        .await
        .expect("the fixed start bound accepted every call while the store was held")
        .expect("the start set was unexpectedly empty")
        .unwrap();
    assert!(matches!(overflow, Err(execution::Error::StartBackpressure)));

    release_tx.send(()).unwrap();
    held_store.await.unwrap().unwrap();
    timeout(Duration::from_secs(5), async {
        while starts.join_next().await.is_some() {}
    })
    .await
    .expect("accepted start attempts did not drain after releasing the store");
    stop(handle, join).await;
}

#[tokio::test]
async fn reconnect_replays_duplicate_prefix_into_fresh_decoder_then_acks_exact_terminal() {
    let fixture = RecoveryFixture::active(true);
    let socket = fixture.runtime_dir.join("control.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    fs::set_permissions(&socket, fs::Permissions::from_mode(0o600)).unwrap();
    let expected_run = fixture.run_id.clone();
    let expected_instance = fixture.runner_instance_id.clone();
    let started = runner_event(1, RunnerEvent::Started { child_pid: 42 });
    let thread = runner_event(
        2,
        RunnerEvent::Output {
            stream: OutputStream::Stdout,
            text: format!("{{\"type\":\"thread.started\",\"thread_id\":\"{THREAD_ID}\"}}\n"),
            lossy: false,
        },
    );
    let turn = runner_event(
        3,
        RunnerEvent::Output {
            stream: OutputStream::Stdout,
            text: "{\"type\":\"turn.completed\",\"usage\":{\"input_tokens\":1,\"cached_input_tokens\":0,\"cache_write_input_tokens\":0,\"output_tokens\":1,\"reasoning_output_tokens\":0}}\n".into(),
            lossy: false,
        },
    );
    let exited = runner_event(
        4,
        RunnerEvent::Exited {
            exit_code: Some(0),
            signal: None,
        },
    );
    let (first_replay_tx, first_replay_rx) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let mut requests = Vec::new();

        let (first, _) = listener.accept().await.unwrap();
        let mut first = BufReader::new(first);
        let mut line = String::new();
        first.read_line(&mut line).await.unwrap();
        requests.push(serde_json::from_str::<RequestEnvelope>(&line).unwrap());
        for frame in [
            RunnerFrame::Hello {
                protocol_version: RUNNER_PROTOCOL_VERSION,
                run_id: expected_run.clone(),
                runner_instance_id: expected_instance.clone(),
                runner_pid: 42,
                replay_through: 2,
                terminal_sequence: None,
            },
            RunnerFrame::Event {
                protocol_version: RUNNER_PROTOCOL_VERSION,
                event: started.clone(),
            },
            RunnerFrame::Event {
                protocol_version: RUNNER_PROTOCOL_VERSION,
                event: thread.clone(),
            },
            RunnerFrame::CaughtUp {
                protocol_version: RUNNER_PROTOCOL_VERSION,
                sequence: 2,
            },
        ] {
            first
                .get_mut()
                .write_all(&serde_json::to_vec(&frame).unwrap())
                .await
                .unwrap();
            first.get_mut().write_all(b"\n").await.unwrap();
        }
        drop(first);
        let _ = first_replay_tx.send(());

        let (second, _) = listener.accept().await.unwrap();
        let mut second = BufReader::new(second);
        let mut line = String::new();
        second.read_line(&mut line).await.unwrap();
        requests.push(serde_json::from_str::<RequestEnvelope>(&line).unwrap());
        for frame in [
            RunnerFrame::Hello {
                protocol_version: RUNNER_PROTOCOL_VERSION,
                run_id: expected_run,
                runner_instance_id: expected_instance,
                runner_pid: 42,
                replay_through: 4,
                terminal_sequence: Some(4),
            },
            RunnerFrame::Event {
                protocol_version: RUNNER_PROTOCOL_VERSION,
                event: started,
            },
            RunnerFrame::Event {
                protocol_version: RUNNER_PROTOCOL_VERSION,
                event: thread,
            },
            RunnerFrame::Event {
                protocol_version: RUNNER_PROTOCOL_VERSION,
                event: turn,
            },
            RunnerFrame::Event {
                protocol_version: RUNNER_PROTOCOL_VERSION,
                event: exited,
            },
            RunnerFrame::CaughtUp {
                protocol_version: RUNNER_PROTOCOL_VERSION,
                sequence: 4,
            },
        ] {
            second
                .get_mut()
                .write_all(&serde_json::to_vec(&frame).unwrap())
                .await
                .unwrap();
            second.get_mut().write_all(b"\n").await.unwrap();
        }

        let (ack, _) = listener.accept().await.unwrap();
        let mut ack = BufReader::new(ack);
        let mut line = String::new();
        ack.read_line(&mut line).await.unwrap();
        let request = serde_json::from_str::<RequestEnvelope>(&line).unwrap();
        requests.push(request.clone());
        let command_id = match request.request {
            RunnerRequest::AcknowledgeExit { command_id, .. } => command_id,
            other => panic!("expected terminal acknowledgement, got {other:?}"),
        };
        let response = RunnerFrame::CommandAck {
            protocol_version: RUNNER_PROTOCOL_VERSION,
            command_id,
        };
        ack.get_mut()
            .write_all(&serde_json::to_vec(&response).unwrap())
            .await
            .unwrap();
        ack.get_mut().write_all(b"\n").await.unwrap();
        requests
    });

    let baseline = fixture
        .state
        .with_store(|store| store.latest_event_sequence())
        .await
        .unwrap();
    let mut published = fixture.state.subscribe();
    let (handle, join) = execution::spawn(
        fixture.config(Duration::from_millis(100)),
        fixture.state.clone(),
    )
    .unwrap();
    timeout(Duration::from_secs(5), first_replay_rx)
        .await
        .expect("initial replay subscription timed out")
        .unwrap();
    let run_id = fixture.run_id.clone();
    timeout(Duration::from_secs(5), async {
        loop {
            let run_id = run_id.clone();
            let committed = fixture
                .state
                .with_store(move |store| {
                    Ok(store
                        .execution_target(&run_id)?
                        .last_committed_runner_sequence)
                })
                .await
                .unwrap();
            if committed == 2 {
                break;
            }
            yield_now().await;
        }
    })
    .await
    .expect("initial replay prefix did not commit");
    let requests = timeout(Duration::from_secs(5), server)
        .await
        .expect("reconnect and exact terminal acknowledgement timed out")
        .unwrap();
    settle().await;

    assert_eq!(
        requests,
        vec![
            RequestEnvelope::new(
                fixture.run_id.clone(),
                fixture.runner_instance_id.clone(),
                RunnerRequest::Subscribe { after_sequence: 0 },
            ),
            RequestEnvelope::new(
                fixture.run_id.clone(),
                fixture.runner_instance_id.clone(),
                RunnerRequest::Subscribe { after_sequence: 0 },
            ),
            RequestEnvelope::new(
                fixture.run_id.clone(),
                fixture.runner_instance_id.clone(),
                RunnerRequest::AcknowledgeExit {
                    command_id: "ack-4".into(),
                    terminal_sequence: 4,
                },
            ),
        ]
    );
    assert!(
        !fixture.remains_recoverable().await,
        "exact ack must reconcile the durable terminal"
    );

    let events = fixture
        .state
        .with_store(move |store| store.events_after(baseline, 100))
        .await
        .unwrap();
    assert!(events.iter().any(|event| matches!(
        &event.event,
        FactoryEvent::RunChanged { run }
            if run.id == fixture.run_id && run.status == RunStatus::Succeeded
    )));
    let public_json = serde_json::to_string(&events).unwrap();
    for private in [
        THREAD_ID,
        "private task body",
        fixture.runner_instance_id.as_str(),
        fixture.runtime_dir.to_str().unwrap(),
    ] {
        assert!(
            !public_json.contains(private),
            "public factory event leaked private execution material"
        );
    }

    while let Ok(event) = published.try_recv() {
        let database = fixture.database.clone();
        let visible = tokio::task::spawn_blocking(move || {
            Store::open(database)
                .unwrap()
                .latest_event_sequence()
                .unwrap()
        })
        .await
        .unwrap();
        assert!(
            visible >= event.sequence,
            "event was published before its SQLite transaction committed"
        );
    }

    stop(handle, join).await;
}

#[tokio::test(start_paused = true)]
async fn shutdown_after_nonterminal_caught_up_sends_no_stop_or_acknowledgement() {
    let fixture = RecoveryFixture::active(true);
    let listener = UnixListener::bind(fixture.runtime_dir.join("control.sock")).unwrap();
    let expected_run = fixture.run_id.clone();
    let expected_instance = fixture.runner_instance_id.clone();
    let (caught_up_tx, caught_up_rx) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut reader = BufReader::new(stream);
        let mut request = String::new();
        reader.read_line(&mut request).await.unwrap();
        let request: RequestEnvelope = serde_json::from_str(&request).unwrap();
        let hello = RunnerFrame::Hello {
            protocol_version: RUNNER_PROTOCOL_VERSION,
            run_id: expected_run,
            runner_instance_id: expected_instance,
            runner_pid: 42,
            replay_through: 0,
            terminal_sequence: None,
        };
        let caught_up = RunnerFrame::CaughtUp {
            protocol_version: RUNNER_PROTOCOL_VERSION,
            sequence: 0,
        };
        let stream = reader.get_mut();
        stream
            .write_all(&serde_json::to_vec(&hello).unwrap())
            .await
            .unwrap();
        stream.write_all(b"\n").await.unwrap();
        stream
            .write_all(&serde_json::to_vec(&caught_up).unwrap())
            .await
            .unwrap();
        stream.write_all(b"\n").await.unwrap();
        let _ = caught_up_tx.send(());

        let mut unexpected = String::new();
        reader.read_line(&mut unexpected).await.unwrap();
        (request, unexpected, listener)
    });

    let (handle, join) = execution::spawn(
        fixture.config(Duration::from_millis(100)),
        fixture.state.clone(),
    )
    .unwrap();
    caught_up_rx.await.unwrap();
    stop(handle, join).await;

    let (request, unexpected, listener) = server.await.unwrap();
    assert_eq!(
        request,
        RequestEnvelope::new(
            fixture.run_id.clone(),
            fixture.runner_instance_id.clone(),
            RunnerRequest::Subscribe { after_sequence: 0 },
        )
    );
    assert!(
        unexpected.is_empty(),
        "shutdown sent a command on the subscription connection"
    );

    // There must not be a second connection carrying Stop or AcknowledgeExit.
    let probe =
        tokio::spawn(async move { timeout(Duration::from_millis(1), listener.accept()).await });
    advance_and_settle(Duration::from_millis(1)).await;
    assert!(probe.await.unwrap().is_err());
    assert!(fixture.remains_recoverable().await);
}

#[tokio::test]
async fn claude_recovery_replays_from_zero_with_a_fresh_provider_decoder() {
    let fixture = RecoveryFixture::active_claude(true);
    let socket = fixture.runtime_dir.join("control.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    fs::set_permissions(&socket, fs::Permissions::from_mode(0o600)).unwrap();
    let expected_run = fixture.run_id.clone();
    let expected_instance = fixture.runner_instance_id.clone();
    let started = runner_event(1, RunnerEvent::Started { child_pid: 42 });
    let initialized = claude_init_event(2);
    let result = claude_result_event(3);
    let exited = runner_event(
        4,
        RunnerEvent::Exited {
            exit_code: Some(0),
            signal: None,
        },
    );
    let (first_replay_tx, first_replay_rx) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let mut requests = Vec::new();

        let (first, _) = listener.accept().await.unwrap();
        let mut first = BufReader::new(first);
        let mut line = String::new();
        first.read_line(&mut line).await.unwrap();
        requests.push(serde_json::from_str::<RequestEnvelope>(&line).unwrap());
        for frame in [
            RunnerFrame::Hello {
                protocol_version: RUNNER_PROTOCOL_VERSION,
                run_id: expected_run.clone(),
                runner_instance_id: expected_instance.clone(),
                runner_pid: 42,
                replay_through: 2,
                terminal_sequence: None,
            },
            RunnerFrame::Event {
                protocol_version: RUNNER_PROTOCOL_VERSION,
                event: started.clone(),
            },
            RunnerFrame::Event {
                protocol_version: RUNNER_PROTOCOL_VERSION,
                event: initialized.clone(),
            },
            RunnerFrame::CaughtUp {
                protocol_version: RUNNER_PROTOCOL_VERSION,
                sequence: 2,
            },
        ] {
            first
                .get_mut()
                .write_all(&serde_json::to_vec(&frame).unwrap())
                .await
                .unwrap();
            first.get_mut().write_all(b"\n").await.unwrap();
        }
        drop(first);
        let _ = first_replay_tx.send(());

        let (second, _) = listener.accept().await.unwrap();
        let mut second = BufReader::new(second);
        let mut line = String::new();
        second.read_line(&mut line).await.unwrap();
        requests.push(serde_json::from_str::<RequestEnvelope>(&line).unwrap());
        for frame in [
            RunnerFrame::Hello {
                protocol_version: RUNNER_PROTOCOL_VERSION,
                run_id: expected_run,
                runner_instance_id: expected_instance,
                runner_pid: 42,
                replay_through: 4,
                terminal_sequence: Some(4),
            },
            RunnerFrame::Event {
                protocol_version: RUNNER_PROTOCOL_VERSION,
                event: started,
            },
            RunnerFrame::Event {
                protocol_version: RUNNER_PROTOCOL_VERSION,
                event: initialized,
            },
            RunnerFrame::Event {
                protocol_version: RUNNER_PROTOCOL_VERSION,
                event: result,
            },
            RunnerFrame::Event {
                protocol_version: RUNNER_PROTOCOL_VERSION,
                event: exited,
            },
            RunnerFrame::CaughtUp {
                protocol_version: RUNNER_PROTOCOL_VERSION,
                sequence: 4,
            },
        ] {
            second
                .get_mut()
                .write_all(&serde_json::to_vec(&frame).unwrap())
                .await
                .unwrap();
            second.get_mut().write_all(b"\n").await.unwrap();
        }

        let (ack, _) = listener.accept().await.unwrap();
        let mut ack = BufReader::new(ack);
        let mut line = String::new();
        ack.read_line(&mut line).await.unwrap();
        let request = serde_json::from_str::<RequestEnvelope>(&line).unwrap();
        requests.push(request.clone());
        let command_id = match request.request {
            RunnerRequest::AcknowledgeExit { command_id, .. } => command_id,
            other => panic!("expected terminal acknowledgement, got {other:?}"),
        };
        let response = RunnerFrame::CommandAck {
            protocol_version: RUNNER_PROTOCOL_VERSION,
            command_id,
        };
        ack.get_mut()
            .write_all(&serde_json::to_vec(&response).unwrap())
            .await
            .unwrap();
        ack.get_mut().write_all(b"\n").await.unwrap();
        requests
    });

    let baseline = fixture
        .state
        .with_store(|store| store.latest_event_sequence())
        .await
        .unwrap();
    let (handle, join) = execution::spawn(
        fixture.config(Duration::from_millis(100)),
        fixture.state.clone(),
    )
    .unwrap();
    timeout(Duration::from_secs(5), first_replay_rx)
        .await
        .expect("initial Claude replay subscription timed out")
        .unwrap();
    let requests = timeout(Duration::from_secs(5), server)
        .await
        .expect("Claude reconnect and terminal acknowledgement timed out")
        .unwrap();
    timeout(Duration::from_secs(5), async {
        while fixture.remains_recoverable().await {
            yield_now().await;
        }
    })
    .await
    .expect("Claude terminal acknowledgement was not reconciled");

    assert_eq!(
        requests,
        vec![
            RequestEnvelope::new(
                fixture.run_id.clone(),
                fixture.runner_instance_id.clone(),
                RunnerRequest::Subscribe { after_sequence: 0 },
            ),
            RequestEnvelope::new(
                fixture.run_id.clone(),
                fixture.runner_instance_id.clone(),
                RunnerRequest::Subscribe { after_sequence: 0 },
            ),
            RequestEnvelope::new(
                fixture.run_id.clone(),
                fixture.runner_instance_id.clone(),
                RunnerRequest::AcknowledgeExit {
                    command_id: "ack-4".into(),
                    terminal_sequence: 4,
                },
            ),
        ]
    );
    let events = fixture
        .state
        .with_store(move |store| store.events_after(baseline, 100))
        .await
        .unwrap();
    assert!(events.iter().any(|event| matches!(
        &event.event,
        FactoryEvent::RunChanged { run }
            if run.id == fixture.run_id && run.status == RunStatus::Succeeded
    )));
    let public = serde_json::to_string(&events).unwrap();
    assert!(!public.contains(THREAD_ID));
    assert!(!public.contains("private task body"));

    stop(handle, join).await;
}
