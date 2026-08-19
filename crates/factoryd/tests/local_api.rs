use std::{future::Future, os::unix::fs::PermissionsExt, path::Path};

use factory_core::{
    AgentId, FactoryEvent, PROTOCOL_VERSION, ProjectId, TaskId,
    local::{
        ErrorCode, LocalRequest, LocalResponse, MAX_EVENT_PAGE_ITEMS, MAX_LOCAL_FRAME_BYTES,
        MAX_TASK_BODY_BYTES, RequestEnvelope, ServerFrame,
    },
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

fn project_id(value: &str) -> ProjectId {
    ProjectId::try_from(value).unwrap()
}

fn task_id(value: &str) -> TaskId {
    TaskId::try_from(value).unwrap()
}

fn agent_id(value: &str) -> AgentId {
    AgentId::try_from(value).unwrap()
}

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

async fn raw_request(socket: &Path, payload: &[u8]) -> ServerFrame {
    let mut stream = UnixStream::connect(socket).await.unwrap();
    stream.write_all(payload).await.unwrap();
    stream.write_all(b"\n").await.unwrap();
    read_frame(&mut BufReader::new(stream)).await
}

async fn with_server<F, Fut>(test: F)
where
    F: FnOnce(std::path::PathBuf) -> Fut,
    Fut: Future<Output = ()>,
{
    let directory = tempfile::tempdir_in("/tmp").unwrap();
    std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let socket = directory.path().join("f.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let state = ApiState::new(Store::open_in_memory().unwrap());
    let (execution, execution_join) =
        execution::spawn(execution_config(directory.path(), &socket), state.clone()).unwrap();
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let server = tokio::spawn(serve(
        listener,
        state,
        execution.clone(),
        directory.path().to_path_buf(),
        async {
            let _ = shutdown_rx.await;
        },
    ));

    test(socket).await;

    let _ = shutdown_tx.send(());
    server.await.unwrap().unwrap();
    execution.shutdown().await.unwrap();
    execution_join.await.unwrap().unwrap();
}

fn execution_config(directory: &Path, socket: &Path) -> execution::Config {
    execution::Config {
        runner_program: directory.join("factory-runner"),
        factoryctl_path: directory.join("factoryctl"),
        runtime_root: directory.join("runs"),
        guidance_root: directory.to_path_buf(),
        socket_path: socket.to_path_buf(),
        max_active_runs: 1,
        session_start_deadline: execution::SESSION_START_DEADLINE,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn repository_requests_require_a_live_session_token_and_accept_no_target_ids() {
    with_server(|socket| async move {
        let response = request(
            &socket,
            LocalRequest::GitPush {
                token: "not-a-session-token".into(),
            },
        )
        .await;
        assert!(matches!(
            response,
            ServerFrame::Response {
                response: LocalResponse::Error {
                    code: ErrorCode::Unauthorized,
                    ref message,
                },
                ..
            } if message == "session authentication failed"
        ));
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn commands_and_live_events_share_the_persisted_cursor() {
    with_server(|socket| async move {
        let project_root = socket.parent().unwrap().join("project");
        std::fs::create_dir(&project_root).unwrap();

        let health = request(&socket, LocalRequest::Health).await;
        assert!(matches!(
            health,
            ServerFrame::Response {
                protocol_version: PROTOCOL_VERSION,
                response: LocalResponse::Health { .. }
            }
        ));

        let created = request(
            &socket,
            LocalRequest::CreateProject {
                id: project_id("project-1"),
                name: "Project One".into(),
                root: project_root.to_string_lossy().into_owned(),
            },
        )
        .await;
        assert!(matches!(
            created,
            ServerFrame::Response {
                response: LocalResponse::ProjectCreated { .. },
                ..
            }
        ));

        let mut subscription = UnixStream::connect(&socket).await.unwrap();
        write_request(
            &mut subscription,
            LocalRequest::Subscribe { after_sequence: 0 },
        )
        .await;
        let mut subscription = BufReader::new(subscription);
        assert!(matches!(
            read_frame(&mut subscription).await,
            ServerFrame::Response {
                response: LocalResponse::Subscribed {
                    after_sequence: 0,
                    replay_through: 1,
                },
                ..
            }
        ));
        assert!(matches!(
            read_frame(&mut subscription).await,
            ServerFrame::Event {
                event: factory_core::EventEnvelope {
                    sequence: 1,
                    event: FactoryEvent::ProjectChanged { .. },
                    ..
                },
                ..
            }
        ));
        assert!(matches!(
            read_frame(&mut subscription).await,
            ServerFrame::Response {
                response: LocalResponse::CaughtUp { sequence: 1 },
                ..
            }
        ));

        let task_created = request(
            &socket,
            LocalRequest::CreateTask {
                id: task_id("task-1"),
                project_id: project_id("project-1"),
                parent_task_id: None,
                title: "Stream a task".into(),
                body: "The observer should receive this without polling.".into(),
                priority: 5,
                agent_id: None,
            },
        )
        .await;
        assert!(matches!(
            task_created,
            ServerFrame::Response {
                response: LocalResponse::TaskCreated { .. },
                ..
            }
        ));

        let live = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            read_frame(&mut subscription),
        )
        .await
        .unwrap();
        assert!(matches!(
            live,
            ServerFrame::Event {
                event: factory_core::EventEnvelope {
                    sequence: 2,
                    event: FactoryEvent::TaskChanged { .. },
                    ..
                },
                ..
            }
        ));

        drop(subscription);
        let second_task = request(
            &socket,
            LocalRequest::CreateTask {
                id: task_id("task-2"),
                project_id: project_id("project-1"),
                parent_task_id: None,
                title: "Survive observer restart".into(),
                body: "This event must replay after reconnect.".into(),
                priority: 1,
                agent_id: None,
            },
        )
        .await;
        assert!(matches!(
            second_task,
            ServerFrame::Response {
                response: LocalResponse::TaskCreated { .. },
                ..
            }
        ));

        let mut reconnected = UnixStream::connect(&socket).await.unwrap();
        write_request(
            &mut reconnected,
            LocalRequest::Subscribe { after_sequence: 2 },
        )
        .await;
        let mut reconnected = BufReader::new(reconnected);
        assert!(matches!(
            read_frame(&mut reconnected).await,
            ServerFrame::Response {
                response: LocalResponse::Subscribed {
                    after_sequence: 2,
                    replay_through: 3,
                },
                ..
            }
        ));
        assert!(matches!(
            read_frame(&mut reconnected).await,
            ServerFrame::Event {
                event: factory_core::EventEnvelope { sequence: 3, .. },
                ..
            }
        ));
        assert!(matches!(
            read_frame(&mut reconnected).await,
            ServerFrame::Response {
                response: LocalResponse::CaughtUp { sequence: 3 },
                ..
            }
        ));

        let tasks = request(
            &socket,
            LocalRequest::ListTasks {
                project_id: project_id("project-1"),
                after_id: None,
                agent_id: None,
                queue_revision: None,
                history: false,
                limit: 10,
            },
        )
        .await;
        match tasks {
            ServerFrame::Response {
                response: LocalResponse::Tasks { tasks, .. },
                ..
            } => {
                assert_eq!(tasks.len(), 2);
                assert_eq!(
                    tasks[0].body,
                    "The observer should receive this without polling."
                );
            }
            other => panic!("unexpected response: {other:?}"),
        }
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn live_task_detail_and_event_head_are_bounded_local_reads() {
    with_server(|socket| async move {
        let project_root = socket.parent().unwrap().join("detail-project");
        std::fs::create_dir(&project_root).unwrap();
        assert!(matches!(
            request(
                &socket,
                LocalRequest::CreateProject {
                    id: project_id("detail-project"),
                    name: "Detail Project".into(),
                    root: project_root.to_string_lossy().into_owned(),
                },
            )
            .await,
            ServerFrame::Response {
                response: LocalResponse::ProjectCreated { .. },
                ..
            }
        ));
        assert!(matches!(
            request(
                &socket,
                LocalRequest::CreateTask {
                    id: task_id("detail-task"),
                    project_id: project_id("detail-project"),
                    parent_task_id: None,
                    title: "Hydrate me".into(),
                    body: "bounded live body".into(),
                    priority: 0,
                    agent_id: None,
                },
            )
            .await,
            ServerFrame::Response {
                response: LocalResponse::TaskCreated { .. },
                ..
            }
        ));

        assert!(matches!(
            request(
                &socket,
                LocalRequest::GetTask {
                    project_id: project_id("detail-project"),
                    task_id: task_id("detail-task"),
                },
            )
            .await,
            ServerFrame::Response {
                response: LocalResponse::Task { task },
                ..
            } if task.body == "bounded live body"
        ));
        assert!(matches!(
            request(&socket, LocalRequest::LatestEventSequence).await,
            ServerFrame::Response {
                response: LocalResponse::EventHead { sequence: 2 },
                ..
            }
        ));
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_future_subscription_cursor_is_rejected_with_the_durable_head() {
    with_server(|socket| async move {
        let frame = request(&socket, LocalRequest::Subscribe { after_sequence: 99 }).await;

        assert!(matches!(
            frame,
            ServerFrame::Response {
                response: LocalResponse::Error {
                    code: ErrorCode::InvalidRequest,
                    ref message,
                },
                ..
            } if message.contains("durable head is 0")
        ));
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_frame_limit_counts_json_but_not_the_newline_delimiter() {
    with_server(|socket| async move {
        let request = serde_json::to_vec(&RequestEnvelope::new(LocalRequest::Health)).unwrap();
        let mut exact = vec![b' '; MAX_LOCAL_FRAME_BYTES - request.len()];
        exact.extend_from_slice(&request);
        assert!(matches!(
            raw_request(&socket, &exact).await,
            ServerFrame::Response {
                response: LocalResponse::Health { .. },
                ..
            }
        ));

        exact.push(b' ');
        assert!(matches!(
            raw_request(&socket, &exact).await,
            ServerFrame::Response {
                response: LocalResponse::Error {
                    code: ErrorCode::InvalidRequest,
                    ..
                },
                ..
            }
        ));
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shutdown_closes_idle_connections_before_returning() {
    let directory = tempfile::tempdir_in("/tmp").unwrap();
    std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let socket = directory.path().join("f.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let state = ApiState::new(Store::open_in_memory().unwrap());
    let (execution, execution_join) =
        execution::spawn(execution_config(directory.path(), &socket), state.clone()).unwrap();
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let server = tokio::spawn(serve(
        listener,
        state,
        execution.clone(),
        directory.path().to_path_buf(),
        async {
            let _ = shutdown_rx.await;
        },
    ));
    let _idle = UnixStream::connect(&socket).await.unwrap();

    shutdown_tx.send(()).unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(1), server)
        .await
        .expect("server should cancel and drain idle handlers")
        .unwrap()
        .unwrap();
    execution.shutdown().await.unwrap();
    execution_join.await.unwrap().unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shutdown_cancels_a_blocked_historical_replay() {
    let directory = tempfile::tempdir_in("/tmp").unwrap();
    std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let socket = directory.path().join("f.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let mut store = Store::open_in_memory().unwrap();
    for index in 0..512 {
        store
            .create_project(
                factoryd::store::NewProject {
                    id: project_id(&format!("project-{index:04}")),
                    name: format!("Project {index}"),
                    root: format!("/{index:04}/{}", "x".repeat(8 * 1024)),
                },
                i64::from(index),
            )
            .unwrap();
    }
    let state = ApiState::new(store);
    let (execution, execution_join) =
        execution::spawn(execution_config(directory.path(), &socket), state.clone()).unwrap();
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let server = tokio::spawn(serve(
        listener,
        state,
        execution.clone(),
        directory.path().to_path_buf(),
        async {
            let _ = shutdown_rx.await;
        },
    ));
    let mut observer = UnixStream::connect(&socket).await.unwrap();
    write_request(&mut observer, LocalRequest::Subscribe { after_sequence: 0 }).await;
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    shutdown_tx.send(()).unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(1), server)
        .await
        .expect("shutdown must cancel a blocked observer replay")
        .unwrap()
        .unwrap();
    execution.shutdown().await.unwrap();
    execution_join.await.unwrap().unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn task_bodies_and_collection_pages_are_bounded_before_commit() {
    with_server(|socket| async move {
        let project_root = socket.parent().unwrap().join("bounded-project");
        std::fs::create_dir(&project_root).unwrap();
        request(
            &socket,
            LocalRequest::CreateProject {
                id: project_id("bounded-project"),
                name: "Bounded project".into(),
                root: project_root.to_string_lossy().into_owned(),
            },
        )
        .await;

        let oversized = request(
            &socket,
            LocalRequest::CreateTask {
                id: task_id("oversized"),
                project_id: project_id("bounded-project"),
                parent_task_id: None,
                title: "Too large".into(),
                body: "x".repeat(MAX_TASK_BODY_BYTES + 1),
                priority: 0,
                agent_id: None,
            },
        )
        .await;
        assert!(matches!(
            oversized,
            ServerFrame::Response {
                response: LocalResponse::Error {
                    code: ErrorCode::InvalidRequest,
                    ..
                },
                ..
            }
        ));

        for index in 0..11 {
            let id = format!("task-{index:02}");
            request(
                &socket,
                LocalRequest::CreateTask {
                    id: task_id(&id),
                    project_id: project_id("bounded-project"),
                    parent_task_id: None,
                    title: id,
                    body: "bounded".into(),
                    priority: 0,
                    agent_id: None,
                },
            )
            .await;
        }

        let first = request(
            &socket,
            LocalRequest::ListTasks {
                project_id: project_id("bounded-project"),
                after_id: None,
                agent_id: None,
                queue_revision: None,
                history: false,
                limit: 10,
            },
        )
        .await;
        let next = match first {
            ServerFrame::Response {
                response:
                    LocalResponse::Tasks {
                        tasks,
                        next_after_id: Some(next),
                        queue_revision: Some(revision),
                    },
                ..
            } => {
                assert_eq!(tasks.len(), 10);
                (next, revision)
            }
            other => panic!("unexpected first page: {other:?}"),
        };
        let second = request(
            &socket,
            LocalRequest::ListTasks {
                project_id: project_id("bounded-project"),
                after_id: Some(next.0),
                agent_id: None,
                queue_revision: Some(next.1),
                history: false,
                limit: 10,
            },
        )
        .await;
        assert!(matches!(
            second,
            ServerFrame::Response {
                response: LocalResponse::Tasks {
                    ref tasks,
                    next_after_id: None,
                    ..
                },
                ..
            } if tasks.len() == 1
        ));

        assert!(matches!(
            request(
                &socket,
                LocalRequest::EventsAfter {
                    sequence: 0,
                    limit: MAX_EVENT_PAGE_ITEMS + 1,
                },
            )
            .await,
            ServerFrame::Response {
                response: LocalResponse::Error {
                    code: ErrorCode::InvalidRequest,
                    ..
                },
                ..
            }
        ));
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unsupported_protocol_cannot_mutate_state() {
    with_server(|socket| async move {
        let mut stream = UnixStream::connect(&socket).await.unwrap();
        let envelope = RequestEnvelope {
            protocol_version: PROTOCOL_VERSION + 1,
            request: LocalRequest::CreateProject {
                id: project_id("never-created"),
                name: "No".into(),
                root: "/tmp/no".into(),
            },
        };
        let mut json = serde_json::to_vec(&envelope).unwrap();
        json.push(b'\n');
        stream.write_all(&json).await.unwrap();
        let frame = read_frame(&mut BufReader::new(stream)).await;

        assert!(matches!(
            frame,
            ServerFrame::Response {
                response: LocalResponse::Error {
                    code: ErrorCode::UnsupportedProtocol,
                    ..
                },
                ..
            }
        ));

        let projects = request(
            &socket,
            LocalRequest::ListProjects {
                after_id: None,
                limit: 100,
            },
        )
        .await;
        assert!(matches!(
            projects,
            ServerFrame::Response {
                response: LocalResponse::Projects { ref projects, .. },
                ..
            } if projects.is_empty()
        ));
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn retry_is_a_local_control_operation_and_does_not_change_queued_tasks() {
    with_server(|socket| async move {
        let project_root = socket.parent().unwrap().join("project");
        std::fs::create_dir(&project_root).unwrap();
        assert!(matches!(
            request(
                &socket,
                LocalRequest::CreateProject {
                    id: project_id("factory"),
                    name: "Factory".into(),
                    root: project_root.to_string_lossy().into_owned(),
                },
            )
            .await,
            ServerFrame::Response {
                response: LocalResponse::ProjectCreated { .. },
                ..
            }
        ));
        assert!(matches!(
            request(
                &socket,
                LocalRequest::CreateTask {
                    id: task_id("task-1"),
                    project_id: project_id("factory"),
                    parent_task_id: None,
                    title: "Queued".into(),
                    body: "Still queued".into(),
                    priority: 0,
                    agent_id: None,
                },
            )
            .await,
            ServerFrame::Response {
                response: LocalResponse::TaskCreated { .. },
                ..
            }
        ));

        let retry = request(
            &socket,
            LocalRequest::RetryTask {
                project_id: project_id("factory"),
                task_id: task_id("task-1"),
            },
        )
        .await;
        assert!(matches!(
            retry,
            ServerFrame::Response {
                response: LocalResponse::Error {
                    code: ErrorCode::Conflict,
                    ..
                },
                ..
            }
        ));
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn queued_task_assignment_is_a_local_control_operation() {
    with_server(|socket| async move {
        let project_root = socket.parent().unwrap().join("project");
        std::fs::create_dir(&project_root).unwrap();
        assert!(matches!(
            request(
                &socket,
                LocalRequest::CreateProject {
                    id: project_id("factory"),
                    name: "Factory".into(),
                    root: project_root.to_string_lossy().into_owned(),
                },
            )
            .await,
            ServerFrame::Response {
                response: LocalResponse::ProjectCreated { .. },
                ..
            }
        ));
        assert!(matches!(
            request(
                &socket,
                LocalRequest::CreateAgent {
                    id: agent_id("curie"),
                    project_id: project_id("factory"),
                    parent_agent_id: None,
                    role: factory_core::AgentRole::Worker,
                    provider: factory_core::Provider::Codex,
                    model: None,
                    worktree: None,
                },
            )
            .await,
            ServerFrame::Response {
                response: LocalResponse::AgentCreated { .. },
                ..
            }
        ));
        assert!(matches!(
            request(
                &socket,
                LocalRequest::CreateTask {
                    id: task_id("task-1"),
                    project_id: project_id("factory"),
                    parent_task_id: None,
                    title: "Queue me".into(),
                    body: "body".into(),
                    priority: 0,
                    agent_id: None,
                },
            )
            .await,
            ServerFrame::Response {
                response: LocalResponse::TaskCreated { .. },
                ..
            }
        ));

        let assigned = request(
            &socket,
            LocalRequest::AssignTask {
                project_id: project_id("factory"),
                task_id: task_id("task-1"),
                agent_id: Some(agent_id("curie")),
            },
        )
        .await;
        assert!(matches!(
            assigned,
            ServerFrame::Response {
                response: LocalResponse::TaskAssigned { ref task },
                ..
            } if task.snapshot.assigned_agent_id == Some(agent_id("curie"))
        ));

        let unassigned = request(
            &socket,
            LocalRequest::AssignTask {
                project_id: project_id("factory"),
                task_id: task_id("task-1"),
                agent_id: None,
            },
        )
        .await;
        assert!(matches!(
            unassigned,
            ServerFrame::Response {
                response: LocalResponse::TaskAssigned { ref task },
                ..
            } if task.snapshot.assigned_agent_id.is_none()
        ));
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn local_agent_messages_round_trip_without_public_events() {
    with_server(|socket| async move {
        let project_root = socket.parent().unwrap().join("project");
        std::fs::create_dir(&project_root).unwrap();
        assert!(matches!(
            request(
                &socket,
                LocalRequest::CreateProject {
                    id: project_id("factory"),
                    name: "Factory".into(),
                    root: project_root.to_string_lossy().into_owned(),
                },
            )
            .await,
            ServerFrame::Response {
                response: LocalResponse::ProjectCreated { .. },
                ..
            }
        ));
        assert!(matches!(
            request(
                &socket,
                LocalRequest::CreateAgent {
                    id: agent_id("god"),
                    project_id: project_id("factory"),
                    parent_agent_id: None,
                    role: factory_core::AgentRole::Orchestrator,
                    provider: factory_core::Provider::Codex,
                    model: None,
                    worktree: None,
                },
            )
            .await,
            ServerFrame::Response {
                response: LocalResponse::AgentCreated { .. },
                ..
            }
        ));

        let sent = request(
            &socket,
            LocalRequest::SendAgentMessage {
                id: factory_core::MessageId::try_from("message-1").unwrap(),
                project_id: project_id("factory"),
                sender_agent_id: None,
                recipient_agent_id: agent_id("god"),
                body: "Please review the queue.".into(),
            },
        )
        .await;
        assert!(matches!(
            sent,
            ServerFrame::Response {
                response: LocalResponse::AgentMessageSent { ref message },
                ..
            } if message.delivered_at_ms.is_none()
        ));

        let listed = request(
            &socket,
            LocalRequest::ListAgentMessages {
                project_id: project_id("factory"),
                agent_id: agent_id("god"),
                after_id: None,
                limit: 10,
            },
        )
        .await;
        assert!(matches!(
            listed,
            ServerFrame::Response {
                response: LocalResponse::AgentMessages { ref messages, .. },
                ..
            } if messages.len() == 1 && messages[0].body == "Please review the queue."
        ));

        assert_eq!(
            request(&socket, LocalRequest::LatestEventSequence,).await,
            ServerFrame::Response {
                protocol_version: PROTOCOL_VERSION,
                // ProjectChanged (1), AgentChanged from CreateAgent (2),
                // AgentChanged again from provisioning its worktree (3) --
                // SendAgentMessage publishes no public event.
                response: LocalResponse::EventHead { sequence: 3 },
            }
        );
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancel_update_and_delete_are_local_control_operations() {
    with_server(|socket| async move {
        let project_root = socket.parent().unwrap().join("project");
        std::fs::create_dir(&project_root).unwrap();
        assert!(matches!(
            request(
                &socket,
                LocalRequest::CreateProject {
                    id: project_id("factory"),
                    name: "Factory".into(),
                    root: project_root.to_string_lossy().into_owned(),
                },
            )
            .await,
            ServerFrame::Response {
                response: LocalResponse::ProjectCreated { .. },
                ..
            }
        ));
        assert!(matches!(
            request(
                &socket,
                LocalRequest::CreateAgent {
                    id: agent_id("curie"),
                    project_id: project_id("factory"),
                    parent_agent_id: None,
                    role: factory_core::AgentRole::Worker,
                    provider: factory_core::Provider::Codex,
                    model: None,
                    worktree: None,
                },
            )
            .await,
            ServerFrame::Response {
                response: LocalResponse::AgentCreated { .. },
                ..
            }
        ));
        assert!(matches!(
            request(
                &socket,
                LocalRequest::CreateTask {
                    id: task_id("task-1"),
                    project_id: project_id("factory"),
                    parent_task_id: None,
                    title: "Cancel me".into(),
                    body: "body".into(),
                    priority: 0,
                    agent_id: None,
                },
            )
            .await,
            ServerFrame::Response {
                response: LocalResponse::TaskCreated { .. },
                ..
            }
        ));
        assert!(matches!(
            request(
                &socket,
                LocalRequest::AssignTask {
                    project_id: project_id("factory"),
                    task_id: task_id("task-1"),
                    agent_id: Some(agent_id("curie")),
                },
            )
            .await,
            ServerFrame::Response {
                response: LocalResponse::TaskAssigned { .. },
                ..
            }
        ));

        let cancelled = request(
            &socket,
            LocalRequest::CancelTask {
                project_id: project_id("factory"),
                task_id: task_id("task-1"),
            },
        )
        .await;
        assert!(matches!(
            cancelled,
            ServerFrame::Response {
                response: LocalResponse::TaskCancelled { ref task },
                ..
            } if task.snapshot.status == factory_core::TaskStatus::Cancelled
                && task.snapshot.assigned_agent_id == Some(agent_id("curie"))
        ));

        let cancel_again = request(
            &socket,
            LocalRequest::CancelTask {
                project_id: project_id("factory"),
                task_id: task_id("task-1"),
            },
        )
        .await;
        assert!(matches!(
            cancel_again,
            ServerFrame::Response {
                response: LocalResponse::Error {
                    code: ErrorCode::Conflict,
                    ..
                },
                ..
            }
        ));

        assert!(matches!(
            request(
                &socket,
                LocalRequest::RetryTask {
                    project_id: project_id("factory"),
                    task_id: task_id("task-1"),
                },
            )
            .await,
            ServerFrame::Response {
                response: LocalResponse::TaskRetried { .. },
                ..
            }
        ));

        let no_fields = request(
            &socket,
            LocalRequest::UpdateTask {
                project_id: project_id("factory"),
                task_id: task_id("task-1"),
                title: None,
                body: None,
                priority: None,
            },
        )
        .await;
        assert!(matches!(
            no_fields,
            ServerFrame::Response {
                response: LocalResponse::Error {
                    code: ErrorCode::InvalidRequest,
                    ..
                },
                ..
            }
        ));

        let updated = request(
            &socket,
            LocalRequest::UpdateTask {
                project_id: project_id("factory"),
                task_id: task_id("task-1"),
                title: Some("Updated title".into()),
                body: None,
                priority: None,
            },
        )
        .await;
        assert!(matches!(
            updated,
            ServerFrame::Response {
                response: LocalResponse::TaskUpdated { ref task },
                ..
            } if task.snapshot.title == "Updated title"
        ));

        let fetched = request(
            &socket,
            LocalRequest::GetTask {
                project_id: project_id("factory"),
                task_id: task_id("task-1"),
            },
        )
        .await;
        assert!(matches!(
            fetched,
            ServerFrame::Response {
                response: LocalResponse::Task { ref task },
                ..
            } if task.snapshot.title == "Updated title"
        ));

        let deleted = request(
            &socket,
            LocalRequest::DeleteTask {
                project_id: project_id("factory"),
                task_id: task_id("task-1"),
            },
        )
        .await;
        assert!(matches!(
            deleted,
            ServerFrame::Response {
                response: LocalResponse::TaskDeleted { ref project_id, ref task_id },
                ..
            } if *project_id == self::project_id("factory") && *task_id == self::task_id("task-1")
        ));

        let missing = request(
            &socket,
            LocalRequest::GetTask {
                project_id: project_id("factory"),
                task_id: task_id("task-1"),
            },
        )
        .await;
        assert!(matches!(
            missing,
            ServerFrame::Response {
                response: LocalResponse::Error {
                    code: ErrorCode::NotFound,
                    ..
                },
                ..
            }
        ));

        // Reusing an operator-facing id creates a new task incarnation,
        // never a continuation of the deleted task's delivery identity.
        let replacement = request(
            &socket,
            LocalRequest::CreateTask {
                id: task_id("task-1"),
                project_id: project_id("factory"),
                parent_task_id: None,
                title: "Replacement".into(),
                body: "different body".into(),
                priority: 0,
                agent_id: None,
            },
        )
        .await;
        assert!(matches!(
            replacement,
            ServerFrame::Response {
                response: LocalResponse::TaskCreated { ref task },
                ..
            } if task.snapshot.title == "Replacement" && task.body == "different body"
        ));

        let guidance_root = socket.parent().unwrap();
        let agent_guidance_dir = factory_core::paths::agent_dir(
            guidance_root,
            &project_id("factory"),
            &agent_id("curie"),
        );
        assert!(
            agent_guidance_dir.is_dir(),
            "agent guidance directory should exist before delete"
        );

        let agent_deleted = request(
            &socket,
            LocalRequest::DeleteAgent {
                project_id: project_id("factory"),
                agent_id: agent_id("curie"),
            },
        )
        .await;
        assert!(matches!(
            agent_deleted,
            ServerFrame::Response {
                response: LocalResponse::AgentDeleted { .. },
                ..
            }
        ));
        assert!(
            !agent_guidance_dir.exists(),
            "agent guidance directory should be removed after delete"
        );

        let project_guidance_dir =
            factory_core::paths::project_dir(guidance_root, &project_id("factory"));
        assert!(
            project_guidance_dir.is_dir(),
            "project guidance directory should exist before delete"
        );

        let project_deleted = request(
            &socket,
            LocalRequest::DeleteProject {
                project_id: project_id("factory"),
            },
        )
        .await;
        assert!(matches!(
            project_deleted,
            ServerFrame::Response {
                response: LocalResponse::ProjectDeleted { .. },
                ..
            }
        ));
        assert!(
            !project_guidance_dir.exists(),
            "project guidance directory should be removed after delete"
        );
    })
    .await;
}

/// Regression test for #42: `DeleteAgent` used to remove an agent's
/// guidance directory with a best-effort `fs::remove_dir_all` that could
/// race the dispatcher's own spawn *preparation* (composing guidance,
/// writing the provider's generated config) for that exact agent, since
/// preparation runs before any session row -- and therefore before
/// `delete_agent`'s "no live session" precondition -- exists. Under CI load
/// that made removal fail with "directory not empty", logged and swallowed
/// rather than surfaced (PR #41, run 32025418170).
///
/// `execution::Handle::begin_delete`/`end_delete` (this fix's mechanism,
/// ARCHITECTURE.md's deletion invariant) close the race structurally: once
/// `begin_delete` returns, no further spawn preparation for that agent can
/// start, and any preparation already running has fully finished
/// (including its own failure cleanup) before `begin_delete` returns. That
/// means this test's assertions hold unconditionally on every iteration
/// regardless of whether this particular run actually overlapped a spawn
/// attempt -- not "usually passes". What makes this a real regression
/// guard rather than a tautology: it drives the exact repro from #42
/// (`AssignTask` to a Codex agent whose `runner_program` does not exist, so
/// the dispatcher immediately attempts and fails a spawn) back-to-back
/// across many fresh agents with zero delay before `DeleteAgent`, which is
/// what made the original bug reproducible at all -- reverting the fix
/// makes at least one of these iterations very likely to fail, though not
/// deterministically so (true determinism -- proving the exact ordering
/// without depending on scheduling luck -- is `execution.rs`'s own
/// `dispatch_agent_declines_to_prepare_for_a_deleting_agent` and
/// `wait_for_drain_blocks_until_the_in_flight_preparation_ends` unit tests,
/// which drive `SpawnBackoff` directly under a paused clock).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn delete_agent_never_leaves_guidance_files_racing_a_spawn_attempt() {
    with_server(|socket| async move {
        let project_root = socket.parent().unwrap().join("project");
        std::fs::create_dir(&project_root).unwrap();
        let project_root = std::fs::canonicalize(&project_root).unwrap();
        assert!(matches!(
            request(
                &socket,
                LocalRequest::CreateProject {
                    id: project_id("factory"),
                    name: "Factory".into(),
                    root: project_root.to_string_lossy().into_owned(),
                },
            )
            .await,
            ServerFrame::Response {
                response: LocalResponse::ProjectCreated { .. },
                ..
            }
        ));

        let guidance_root = socket.parent().unwrap();

        for iteration in 0..20u32 {
            let racer = agent_id(&format!("racer-{iteration}"));
            assert!(matches!(
                request(
                    &socket,
                    LocalRequest::CreateAgent {
                        id: racer.clone(),
                        project_id: project_id("factory"),
                        parent_agent_id: None,
                        role: factory_core::AgentRole::Worker,
                        provider: factory_core::Provider::Codex,
                        model: None,
                        worktree: None,
                    },
                )
                .await,
                ServerFrame::Response {
                    response: LocalResponse::AgentCreated { .. },
                    ..
                }
            ));

            let racer_task = task_id(&format!("task-{iteration}"));
            assert!(matches!(
                request(
                    &socket,
                    LocalRequest::CreateTask {
                        id: racer_task.clone(),
                        project_id: project_id("factory"),
                        parent_task_id: None,
                        title: "Race me".into(),
                        body: "body".into(),
                        priority: 0,
                        agent_id: None,
                    },
                )
                .await,
                ServerFrame::Response {
                    response: LocalResponse::TaskCreated { .. },
                    ..
                }
            ));

            // Triggers the dispatcher's spawn attempt for `racer`: its
            // `runner_program` doesn't exist (`execution_config`), so the
            // attempt durably fails, but not before Codex's `spawn_spec`
            // writes into the agent's guidance directory -- #42's exact
            // repro.
            assert!(matches!(
                request(
                    &socket,
                    LocalRequest::AssignTask {
                        project_id: project_id("factory"),
                        task_id: racer_task,
                        agent_id: Some(racer.clone()),
                    },
                )
                .await,
                ServerFrame::Response {
                    response: LocalResponse::TaskAssigned { .. },
                    ..
                }
            ));

            let racer_guidance_dir =
                factory_core::paths::agent_dir(guidance_root, &project_id("factory"), &racer);

            // No delay here on purpose: issuing `DeleteAgent` as close to
            // `AssignTask` returning as possible is what gives this
            // iteration the best chance of actually overlapping the
            // dispatcher's in-flight spawn preparation.
            let deleted = request(
                &socket,
                LocalRequest::DeleteAgent {
                    project_id: project_id("factory"),
                    agent_id: racer.clone(),
                },
            )
            .await;
            assert!(
                matches!(
                    deleted,
                    ServerFrame::Response {
                        response: LocalResponse::AgentDeleted { .. },
                        ..
                    }
                ),
                "iteration {iteration}: delete must succeed even racing a spawn attempt, got \
                 {deleted:?}"
            );
            assert!(
                !racer_guidance_dir.exists(),
                "iteration {iteration}: guidance directory must be gone immediately after delete"
            );

            // The fix means nothing can recreate these files after
            // `DeleteAgent` has returned -- not just that they happened to
            // be gone at that instant -- so re-check well past any
            // plausible straggling preparation.
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            assert!(
                !racer_guidance_dir.exists(),
                "iteration {iteration}: guidance directory must stay gone"
            );
        }
    })
    .await;
}

/// Regression test for PR #50's review, should-fix 4: a `DeleteAgent`
/// whose guidance-directory removal fails for a reason unrelated to #42's
/// race (a permission problem is the reviewer's own deterministic repro,
/// reused here) must leave the agent's ledger row intact so the operator
/// can fix the problem and retry -- not report the row deleted while its
/// files linger with no `DeleteAgent` left able to target them.
/// `delete_agent_locked` now removes files *before* the database row
/// (see its doc comment), so this is deterministic: chmod the agent's
/// parent `agents/` directory to `0500` (read+execute, no write) so
/// `fs::remove_dir_all` on the agent's own directory fails with `EACCES`
/// -- no timing involved at all.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_failed_guidance_removal_leaves_the_agent_retryable_not_half_deleted() {
    with_server(|socket| async move {
        let project_root = socket.parent().unwrap().join("project");
        std::fs::create_dir(&project_root).unwrap();
        assert!(matches!(
            request(
                &socket,
                LocalRequest::CreateProject {
                    id: project_id("factory"),
                    name: "Factory".into(),
                    root: project_root.to_string_lossy().into_owned(),
                },
            )
            .await,
            ServerFrame::Response {
                response: LocalResponse::ProjectCreated { .. },
                ..
            }
        ));
        assert!(matches!(
            request(
                &socket,
                LocalRequest::CreateAgent {
                    id: agent_id("curie"),
                    project_id: project_id("factory"),
                    parent_agent_id: None,
                    role: factory_core::AgentRole::Worker,
                    provider: factory_core::Provider::Shell,
                    model: None,
                    worktree: None,
                },
            )
            .await,
            ServerFrame::Response {
                response: LocalResponse::AgentCreated { .. },
                ..
            }
        ));

        let guidance_root = socket.parent().unwrap();
        let agents_root = factory_core::paths::agents_root(guidance_root, &project_id("factory"));
        let agent_guidance_dir = factory_core::paths::agent_dir(
            guidance_root,
            &project_id("factory"),
            &agent_id("curie"),
        );
        assert!(agent_guidance_dir.is_dir());

        // Removing an entry needs write+execute on its *parent*, not the
        // entry itself -- chmod the shared `agents/` directory, not
        // `curie`'s own.
        std::fs::set_permissions(&agents_root, std::fs::Permissions::from_mode(0o500)).unwrap();

        let first_attempt = request(
            &socket,
            LocalRequest::DeleteAgent {
                project_id: project_id("factory"),
                agent_id: agent_id("curie"),
            },
        )
        .await;
        assert!(
            matches!(
                first_attempt,
                ServerFrame::Response {
                    response: LocalResponse::Error {
                        code: ErrorCode::Internal,
                        ..
                    },
                    ..
                }
            ),
            "removal failure must surface as this request's own error, got {first_attempt:?}"
        );
        assert!(
            agent_guidance_dir.is_dir(),
            "the failed removal must not have partially removed the directory"
        );

        let still_there = request(
            &socket,
            LocalRequest::GetAgent {
                project_id: project_id("factory"),
                agent_id: agent_id("curie"),
            },
        )
        .await;
        assert!(
            matches!(
                still_there,
                ServerFrame::Response {
                    response: LocalResponse::Agent { .. },
                    ..
                }
            ),
            "the agent's row must survive a failed delete so the operator can retry, got \
             {still_there:?}"
        );

        // Fix the permission problem and retry: this is exactly the
        // recovery path the reordering exists to make possible.
        std::fs::set_permissions(&agents_root, std::fs::Permissions::from_mode(0o700)).unwrap();

        let retry = request(
            &socket,
            LocalRequest::DeleteAgent {
                project_id: project_id("factory"),
                agent_id: agent_id("curie"),
            },
        )
        .await;
        assert!(matches!(
            retry,
            ServerFrame::Response {
                response: LocalResponse::AgentDeleted { .. },
                ..
            }
        ));
        assert!(!agent_guidance_dir.exists());
    })
    .await;
}

/// Regression test for PR #50's re-review, new blocking finding: a
/// *refused* `DeleteAgent` must not destroy the agent's guidance files.
/// `delete_agent_locked`'s reordering (should-fix 4: files before the DB
/// row, so a removal failure is retryable) meant every one of
/// `store.delete_agent`'s own preconditions -- which live inside its
/// transaction, the very last step -- started running *after* the files
/// were already gone. Reproduces the review's exact repro: a parent agent
/// with a child cannot be deleted (`AgentHasChildren`), which used to be
/// exactly the moment its `instructions.md` got wiped anyway.
/// `delete_agent_locked` now calls `store.check_agent_deletable` first, so
/// this refusal costs no files at all.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_refused_delete_agent_leaves_every_file_intact() {
    with_server(|socket| async move {
        let project_root = socket.parent().unwrap().join("project");
        std::fs::create_dir(&project_root).unwrap();
        assert!(matches!(
            request(
                &socket,
                LocalRequest::CreateProject {
                    id: project_id("factory"),
                    name: "Factory".into(),
                    root: project_root.to_string_lossy().into_owned(),
                },
            )
            .await,
            ServerFrame::Response {
                response: LocalResponse::ProjectCreated { .. },
                ..
            }
        ));
        assert!(matches!(
            request(
                &socket,
                LocalRequest::CreateAgent {
                    id: agent_id("boss"),
                    project_id: project_id("factory"),
                    parent_agent_id: None,
                    role: factory_core::AgentRole::Worker,
                    provider: factory_core::Provider::Shell,
                    model: None,
                    worktree: None,
                },
            )
            .await,
            ServerFrame::Response {
                response: LocalResponse::AgentCreated { .. },
                ..
            }
        ));
        assert!(matches!(
            request(
                &socket,
                LocalRequest::CreateAgent {
                    id: agent_id("minion"),
                    project_id: project_id("factory"),
                    parent_agent_id: Some(agent_id("boss")),
                    role: factory_core::AgentRole::Worker,
                    provider: factory_core::Provider::Shell,
                    model: None,
                    worktree: None,
                },
            )
            .await,
            ServerFrame::Response {
                response: LocalResponse::AgentCreated { .. },
                ..
            }
        ));

        const MARKER: &str = "PLEASE DO NOT LOSE THIS";
        assert!(matches!(
            request(
                &socket,
                LocalRequest::UpdateAgentProfile {
                    project_id: project_id("factory"),
                    agent_id: agent_id("boss"),
                    model: None,
                    permission_mode: None,
                    instructions: MARKER.into(),
                    memory: String::new(),
                },
            )
            .await,
            ServerFrame::Response {
                response: LocalResponse::AgentProfileUpdated { .. },
                ..
            }
        ));

        let deleted = request(
            &socket,
            LocalRequest::DeleteAgent {
                project_id: project_id("factory"),
                agent_id: agent_id("boss"),
            },
        )
        .await;
        assert!(
            matches!(
                deleted,
                ServerFrame::Response {
                    response: LocalResponse::Error {
                        code: ErrorCode::Conflict,
                        ..
                    },
                    ..
                }
            ),
            "a parent agent must be refused, not deleted, got {deleted:?}"
        );

        let fetched = request(
            &socket,
            LocalRequest::GetAgent {
                project_id: project_id("factory"),
                agent_id: agent_id("boss"),
            },
        )
        .await;
        assert!(
            matches!(
                &fetched,
                ServerFrame::Response {
                    response: LocalResponse::Agent { agent },
                    ..
                } if agent.profile.instructions == MARKER
            ),
            "instructions.md must survive a refused delete untouched, got {fetched:?}"
        );
    })
    .await;
}

/// Regression test for PR #50's round-3 re-review: `CreateAgent` gated the
/// project and the *new* agent id (round 2) but not `parent_agent_id`, so
/// `AgentHasChildren` -- the one precondition a *different* concurrent
/// request can flip from false to true -- could still go false-to-true
/// between `delete_agent_locked`'s precheck and its DB delete, destroying
/// the parent's files anyway (reproduced 13/16 with 0-12ms delays by the
/// review). `CreateAgent` now also takes the agent-write gate on
/// `parent_agent_id` when present, declining outright if the parent is
/// being deleted.
///
/// No artificial delay between firing the two requests, and no git repo
/// (so this test's race window is narrower than the review's own probe,
/// which used a real worktree for extra width) -- the assertion inside
/// the loop is unconditional either way: whenever the racing `DeleteAgent`
/// is refused, `boss`'s files must be completely intact, regardless of
/// whether this particular iteration's interleaving actually landed
/// inside the (now much narrower, gate-protected) window.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_create_agent_naming_a_deleting_parent_never_destroys_its_files() {
    with_server(|socket| async move {
        let project_root = socket.parent().unwrap().join("project");
        std::fs::create_dir(&project_root).unwrap();
        assert!(matches!(
            request(
                &socket,
                LocalRequest::CreateProject {
                    id: project_id("factory"),
                    name: "Factory".into(),
                    root: project_root.to_string_lossy().into_owned(),
                },
            )
            .await,
            ServerFrame::Response {
                response: LocalResponse::ProjectCreated { .. },
                ..
            }
        ));

        const MARKER: &str = "PLEASE DO NOT LOSE THIS";
        for iteration in 0..20u32 {
            let boss = agent_id(&format!("boss{iteration}"));
            let kid = agent_id(&format!("kid{iteration}"));
            assert!(matches!(
                request(
                    &socket,
                    LocalRequest::CreateAgent {
                        id: boss.clone(),
                        project_id: project_id("factory"),
                        parent_agent_id: None,
                        role: factory_core::AgentRole::Worker,
                        provider: factory_core::Provider::Shell,
                        model: None,
                        worktree: None,
                    },
                )
                .await,
                ServerFrame::Response {
                    response: LocalResponse::AgentCreated { .. },
                    ..
                }
            ));
            assert!(matches!(
                request(
                    &socket,
                    LocalRequest::UpdateAgentProfile {
                        project_id: project_id("factory"),
                        agent_id: boss.clone(),
                        model: None,
                        permission_mode: None,
                        instructions: MARKER.into(),
                        memory: String::new(),
                    },
                )
                .await,
                ServerFrame::Response {
                    response: LocalResponse::AgentProfileUpdated { .. },
                    ..
                }
            ));

            // Fired concurrently, no delay: DeleteAgent(boss) racing
            // CreateAgent(kid, parent=boss).
            let (delete_result, _create_result) = tokio::join!(
                request(
                    &socket,
                    LocalRequest::DeleteAgent {
                        project_id: project_id("factory"),
                        agent_id: boss.clone(),
                    },
                ),
                request(
                    &socket,
                    LocalRequest::CreateAgent {
                        id: kid,
                        project_id: project_id("factory"),
                        parent_agent_id: Some(boss.clone()),
                        role: factory_core::AgentRole::Worker,
                        provider: factory_core::Provider::Shell,
                        model: None,
                        worktree: None,
                    },
                ),
            );

            let delete_refused = matches!(
                delete_result,
                ServerFrame::Response {
                    response: LocalResponse::Error {
                        code: ErrorCode::Conflict,
                        ..
                    },
                    ..
                }
            );
            if delete_refused {
                let fetched = request(
                    &socket,
                    LocalRequest::GetAgent {
                        project_id: project_id("factory"),
                        agent_id: boss.clone(),
                    },
                )
                .await;
                assert!(
                    matches!(
                        &fetched,
                        ServerFrame::Response {
                            response: LocalResponse::Agent { agent },
                            ..
                        } if agent.profile.instructions == MARKER
                    ),
                    "iteration {iteration}: a refused delete racing a concurrent create naming \
                     boss as parent must leave boss's files intact, got {fetched:?}"
                );
            }
        }
    })
    .await;
}

/// `CreateAgent.worktree` validates an operator override (D3): rejects a
/// non-existent path, accepts and durably records an existing one.
/// `CreateAgent` with no `--worktree` auto-provisions one (5C): since the
/// project root here isn't a git repo, that falls back to the project root
/// itself rather than a real `git worktree add` -- every agent ends up with
/// *some* recorded worktree either way, so `StartTask.worktree: None` always
/// has something to default to. Every session-shaped request now has real
/// behavior: `PauseAgent`/`ResumeAgent`/`ListSessions` succeed; requests
/// naming a session/run/task-episode that doesn't exist yet surface the
/// real `NotFound`/`Conflict`; an unrecognized hook token is rejected.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn session_shaped_requests_now_have_real_behavior() {
    with_server(|socket| async move {
        let project_root = socket.parent().unwrap().join("project");
        std::fs::create_dir(&project_root).unwrap();
        // CreateProject durably stores a canonicalized root
        // (`canonical_root`); compare against that below rather than this
        // possibly-symlinked raw path (e.g. macOS's `/tmp` -> `/private/tmp`).
        let project_root = std::fs::canonicalize(&project_root).unwrap();
        assert!(matches!(
            request(
                &socket,
                LocalRequest::CreateProject {
                    id: project_id("factory"),
                    name: "Factory".into(),
                    root: project_root.to_string_lossy().into_owned(),
                },
            )
            .await,
            ServerFrame::Response {
                response: LocalResponse::ProjectCreated { .. },
                ..
            }
        ));

        // A worktree override that doesn't exist on disk is rejected.
        assert!(matches!(
            request(
                &socket,
                LocalRequest::CreateAgent {
                    id: agent_id("curie"),
                    project_id: project_id("factory"),
                    parent_agent_id: None,
                    role: factory_core::AgentRole::Worker,
                    provider: factory_core::Provider::Codex,
                    model: None,
                    worktree: Some("/nonexistent/curie-worktree".into()),
                },
            )
            .await,
            ServerFrame::Response {
                response: LocalResponse::Error {
                    code: ErrorCode::InvalidRequest,
                    ..
                },
                ..
            }
        ));

        // An existing absolute directory is accepted and durably recorded.
        let worktree = project_root.to_string_lossy().into_owned();
        let created = request(
            &socket,
            LocalRequest::CreateAgent {
                id: agent_id("curie"),
                project_id: project_id("factory"),
                parent_agent_id: None,
                role: factory_core::AgentRole::Worker,
                provider: factory_core::Provider::Codex,
                model: None,
                worktree: Some(worktree.clone()),
            },
        )
        .await;
        let ServerFrame::Response {
            response: LocalResponse::AgentCreated { agent },
            ..
        } = created
        else {
            panic!("expected AgentCreated, got {created:?}");
        };
        assert_eq!(agent.worktree, Some(worktree));

        assert!(matches!(
            request(
                &socket,
                LocalRequest::CreateTask {
                    id: task_id("task-1"),
                    project_id: project_id("factory"),
                    parent_task_id: None,
                    title: "Uses the agent's worktree".into(),
                    body: "body".into(),
                    priority: 0,
                    agent_id: None,
                },
            )
            .await,
            ServerFrame::Response {
                response: LocalResponse::TaskCreated { .. },
                ..
            }
        ));

        // Creating a worker with no --worktree in a non-git-repo project
        // auto-provisions the project root itself as its worktree (5C) --
        // there is no longer such a thing as an agent with *no* recorded
        // worktree once CreateAgent has run.
        let auto_provisioned = request(
            &socket,
            LocalRequest::CreateAgent {
                id: agent_id("auto-worktree"),
                project_id: project_id("factory"),
                parent_agent_id: None,
                role: factory_core::AgentRole::Worker,
                provider: factory_core::Provider::Codex,
                model: None,
                worktree: None,
            },
        )
        .await;
        let ServerFrame::Response {
            response: LocalResponse::AgentCreated { agent },
            ..
        } = auto_provisioned
        else {
            panic!("expected AgentCreated, got {auto_provisioned:?}");
        };
        assert_eq!(
            agent.worktree,
            Some(project_root.to_string_lossy().into_owned())
        );

        // StartTask against curie (which has a worktree) fails cleanly with
        // no live session yet -- spawning one requires pending queued work
        // and a valid provider/runner binary, neither of which this fixture
        // (a fake runner/factoryctl path, no real `codex`) provides.
        assert!(matches!(
            request(
                &socket,
                LocalRequest::StartTask {
                    project_id: project_id("factory"),
                    task_id: task_id("task-1"),
                    agent_id: agent_id("curie"),
                    parent_run_id: None,
                    worktree: None,
                },
            )
            .await,
            ServerFrame::Response {
                response: LocalResponse::Error {
                    code: ErrorCode::Conflict,
                    ..
                },
                ..
            }
        ));

        // PauseAgent/ResumeAgent are real, durable control operations.
        let paused = request(
            &socket,
            LocalRequest::PauseAgent {
                project_id: project_id("factory"),
                agent_id: agent_id("curie"),
            },
        )
        .await;
        let ServerFrame::Response {
            response: LocalResponse::AgentPaused { agent },
            ..
        } = paused
        else {
            panic!("expected AgentPaused, got {paused:?}");
        };
        assert!(agent.paused);

        let resumed = request(
            &socket,
            LocalRequest::ResumeAgent {
                project_id: project_id("factory"),
                agent_id: agent_id("curie"),
            },
        )
        .await;
        let ServerFrame::Response {
            response: LocalResponse::AgentResumed { agent },
            ..
        } = resumed
        else {
            panic!("expected AgentResumed, got {resumed:?}");
        };
        assert!(!agent.paused);

        // ListSessions succeeds with an empty page: nothing has spawned one
        // yet.
        assert!(matches!(
            request(
                &socket,
                LocalRequest::ListSessions {
                    project_id: project_id("factory"),
                    after_id: None,
                    limit: None,
                },
            )
            .await,
            ServerFrame::Response {
                response: LocalResponse::Sessions { sessions, next_after_id: None },
                ..
            } if sessions.is_empty()
        ));

        // Requests naming a run/task-episode/session that doesn't exist yet
        // surface the real not-found/conflict, not a blanket "unsupported".
        let run_id = factory_core::RunId::try_from("run-1").unwrap();
        let session_id = factory_core::SessionId::try_from("session-1").unwrap();
        assert!(matches!(
            request(
                &socket,
                LocalRequest::CancelRun {
                    project_id: project_id("factory"),
                    run_id,
                },
            )
            .await,
            ServerFrame::Response {
                response: LocalResponse::Error {
                    code: ErrorCode::NotFound,
                    ..
                },
                ..
            }
        ));
        assert!(matches!(
            request(
                &socket,
                LocalRequest::CompleteTask {
                    project_id: project_id("factory"),
                    task_id: task_id("task-1"),
                    result: "done".into(),
                },
            )
            .await,
            ServerFrame::Response {
                response: LocalResponse::Error {
                    code: ErrorCode::Conflict,
                    ..
                },
                ..
            }
        ));
        assert!(matches!(
            request(
                &socket,
                LocalRequest::BlockTask {
                    project_id: project_id("factory"),
                    task_id: task_id("task-1"),
                    reason: "blocked".into(),
                },
            )
            .await,
            ServerFrame::Response {
                response: LocalResponse::Error {
                    code: ErrorCode::Conflict,
                    ..
                },
                ..
            }
        ));
        assert!(matches!(
            request(
                &socket,
                LocalRequest::StopSession {
                    project_id: project_id("factory"),
                    session_id,
                    grace_ms: 0,
                },
            )
            .await,
            ServerFrame::Response {
                response: LocalResponse::Error {
                    code: ErrorCode::NotFound,
                    ..
                },
                ..
            }
        ));
        assert!(matches!(
            request(
                &socket,
                LocalRequest::ProviderHook {
                    token: "unrecognized-token".into(),
                    event: factory_core::ProviderHookEvent::Stop,
                    payload: serde_json::json!({}),
                },
            )
            .await,
            ServerFrame::Response {
                response: LocalResponse::Error {
                    code: ErrorCode::InvalidRequest,
                    ..
                },
                ..
            }
        ));
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fleet_and_agent_status_are_one_consistent_read() {
    with_server(|socket| async move {
        let project_root = socket.parent().unwrap().join("project");
        std::fs::create_dir(&project_root).unwrap();
        for request_body in [
            LocalRequest::CreateProject {
                id: project_id("factory"),
                name: "Factory".to_owned(),
                root: project_root.to_string_lossy().into_owned(),
            },
            LocalRequest::CreateAgent {
                id: agent_id("curie"),
                project_id: project_id("factory"),
                parent_agent_id: None,
                role: factory_core::AgentRole::Worker,
                provider: factory_core::Provider::Codex,
                model: None,
                worktree: None,
            },
            // Paused first, so assigning work never spawns a session: the
            // status below is deterministic (and this test can't race the
            // dispatcher the way #42 describes).
            LocalRequest::PauseAgent {
                project_id: project_id("factory"),
                agent_id: agent_id("curie"),
            },
            LocalRequest::CreateTask {
                id: task_id("t-assigned"),
                project_id: project_id("factory"),
                parent_task_id: None,
                title: "assigned".to_owned(),
                body: "b".to_owned(),
                priority: 0,
                agent_id: None,
            },
            LocalRequest::CreateTask {
                id: task_id("t-loose"),
                project_id: project_id("factory"),
                parent_task_id: None,
                title: "unassigned".to_owned(),
                body: "b".to_owned(),
                priority: 0,
                agent_id: None,
            },
            LocalRequest::AssignTask {
                project_id: project_id("factory"),
                task_id: task_id("t-assigned"),
                agent_id: Some(agent_id("curie")),
            },
        ] {
            let frame = request(&socket, request_body).await;
            assert!(
                !matches!(
                    frame,
                    ServerFrame::Response {
                        response: LocalResponse::Error { .. },
                        ..
                    }
                ),
                "{frame:?}"
            );
        }

        let ServerFrame::Response {
            response: LocalResponse::FleetStatus { status },
            ..
        } = request(&socket, LocalRequest::FleetStatus).await
        else {
            panic!("expected a fleet status");
        };
        assert_eq!(
            status.live_session_cap, 1,
            "execution_config's max_active_runs"
        );
        assert_eq!(status.live_sessions, 0);
        assert_eq!(status.projects.len(), 1);
        let project = &status.projects[0];
        assert_eq!(project.project.id, project_id("factory"));
        assert_eq!(project.backlog_depth, 1);
        assert_eq!(project.backlog[0].id, task_id("t-loose"));
        assert_eq!(project.agents.len(), 1);
        let curie = &project.agents[0];
        assert_eq!(curie.agent.id, agent_id("curie"));
        assert!(curie.agent.paused);
        assert!(curie.session.is_none());
        assert!(curie.current_run.is_none());
        assert_eq!(curie.queue_depth, 1);
        assert_eq!(curie.queue[0].id, task_id("t-assigned"));
        assert_eq!(curie.inbox_pending, 0);
        assert_eq!(curie.attention, factory_core::attention::Attention::Routine);
        assert!(
            curie.attention_inferred,
            "no session: inferred from (no) run"
        );
        let fleet_worktree = curie
            .worktree
            .as_ref()
            .expect("fleet status includes the agent working directory");
        assert!(fleet_worktree.error.is_some(), "{fleet_worktree:?}");
        assert!(!fleet_worktree.dirty);
        assert_eq!(status.attention.len(), 1);
        let item = &status.attention[0];
        assert_eq!(
            item.kind,
            factory_core::status::AttentionKind::PausedWithWork
        );
        assert_eq!(item.agent_id, Some(agent_id("curie")));
        assert_eq!(item.task_id, Some(task_id("t-assigned")));

        let ServerFrame::Response {
            response: LocalResponse::AgentStatus { status: detail },
            ..
        } = request(
            &socket,
            LocalRequest::AgentStatus {
                project_id: project_id("factory"),
                agent_id: agent_id("curie"),
            },
        )
        .await
        else {
            panic!("expected an agent status");
        };
        assert_eq!(detail.status, *curie, "the same picture the fleet view had");
        assert_eq!(detail.detail.snapshot.id, agent_id("curie"));
        assert!(detail.detail.instructions_path.ends_with("instructions.md"));
        // The project root is not a git repository, so the agent runs in the
        // root itself; the status says so instead of pretending a clean tree.
        let worktree = detail.worktree.expect("the agent has a working directory");
        assert_eq!(Some(&worktree), detail.status.worktree.as_ref());
        assert!(worktree.error.is_some(), "{worktree:?}");
        assert!(!worktree.dirty);

        assert!(matches!(
            request(
                &socket,
                LocalRequest::AgentStatus {
                    project_id: project_id("factory"),
                    agent_id: agent_id("nobody"),
                },
            )
            .await,
            ServerFrame::Response {
                response: LocalResponse::Error {
                    code: ErrorCode::NotFound,
                    ..
                },
                ..
            }
        ));
    })
    .await;
}
