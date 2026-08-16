use std::{future::Future, num::NonZeroU32, os::unix::fs::PermissionsExt, path::Path};

use factory_core::{
    AgentId, FactoryEvent, PROTOCOL_VERSION, ProjectId, TaskId,
    local::{
        ErrorCode, LocalRequest, LocalResponse, MAX_LOCAL_FRAME_BYTES, MAX_TASK_BODY_BYTES,
        RequestEnvelope, ServerFrame,
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
        execution::spawn(execution_config(directory.path()), state.clone()).unwrap();
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let server = tokio::spawn(serve(listener, state, execution.clone(), async {
        let _ = shutdown_rx.await;
    }));

    test(socket).await;

    let _ = shutdown_tx.send(());
    server.await.unwrap().unwrap();
    execution.shutdown().await.unwrap();
    execution_join.await.unwrap().unwrap();
}

fn execution_config(directory: &Path) -> execution::Config {
    execution::Config {
        runner_program: directory.join("missing-factory-runner"),
        codex_program: directory.join("missing-codex"),
        claude_program: directory.join("missing-claude"),
        claude_max_turns: NonZeroU32::new(20).unwrap(),
        claude_max_budget_cents: NonZeroU32::new(500).unwrap(),
        runtime_root: directory.join("runs"),
        max_active_runs: 1,
        startup_timeout: std::time::Duration::from_secs(1),
        connect_grace: std::time::Duration::from_secs(1),
        batch_delay: std::time::Duration::from_millis(25),
    }
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
                response: LocalResponse::Health
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
                response: LocalResponse::Health,
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
        execution::spawn(execution_config(directory.path()), state.clone()).unwrap();
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let server = tokio::spawn(serve(listener, state, execution.clone(), async {
        let _ = shutdown_rx.await;
    }));
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
        execution::spawn(execution_config(directory.path()), state.clone()).unwrap();
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let server = tokio::spawn(serve(listener, state, execution.clone(), async {
        let _ = shutdown_rx.await;
    }));
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
                },
            )
            .await;
        }

        let first = request(
            &socket,
            LocalRequest::ListTasks {
                project_id: project_id("bounded-project"),
                after_id: None,
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
                    },
                ..
            } => {
                assert_eq!(tasks.len(), 10);
                next
            }
            other => panic!("unexpected first page: {other:?}"),
        };
        let second = request(
            &socket,
            LocalRequest::ListTasks {
                project_id: project_id("bounded-project"),
                after_id: Some(next),
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
                },
                ..
            } if tasks.len() == 1
        ));

        assert!(matches!(
            request(
                &socket,
                LocalRequest::EventsAfter {
                    sequence: 0,
                    limit: 101,
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
