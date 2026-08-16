use std::{
    future::Future, num::NonZeroU32, os::unix::fs::PermissionsExt, path::Path, time::Duration,
};

use factory_core::{
    AgentId, AgentRole, FactoryEvent, PROTOCOL_VERSION, ProjectId, Provider, RunStatus, TaskId,
    local::{ErrorCode, LocalRequest, LocalResponse, RequestEnvelope, ServerFrame},
};
use factoryd::{
    daemon_state::DaemonState,
    execution,
    local_api::serve,
    store::{NewAgent, Store},
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::{UnixListener, UnixStream},
    sync::oneshot,
};

fn id<T>(value: &str) -> T
where
    T: TryFrom<String>,
    T::Error: std::fmt::Debug,
{
    T::try_from(value.to_owned()).unwrap()
}

async fn request(socket: &Path, request: LocalRequest) -> ServerFrame {
    let mut stream = UnixStream::connect(socket).await.unwrap();
    let mut payload = serde_json::to_vec(&RequestEnvelope::new(request)).unwrap();
    payload.push(b'\n');
    stream.write_all(&payload).await.unwrap();
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line).await.unwrap();
    assert!(!line.is_empty(), "daemon closed without a response");
    serde_json::from_str(&line).unwrap()
}

async fn with_server<F, Fut>(test: F)
where
    F: FnOnce(std::path::PathBuf, DaemonState) -> Fut,
    Fut: Future<Output = ()>,
{
    let directory = tempfile::tempdir_in("/tmp").unwrap();
    std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let socket = directory.path().join("f.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let state = DaemonState::new(Store::open_in_memory().unwrap());
    let (execution, execution_join) = execution::spawn(
        execution::Config {
            runner_program: directory.path().join("missing-factory-runner"),
            codex_program: directory.path().join("missing-codex"),
            claude_program: directory.path().join("missing-claude"),
            claude_max_turns: NonZeroU32::new(20).unwrap(),
            claude_max_budget_cents: NonZeroU32::new(500).unwrap(),
            runtime_root: directory.path().join("runs"),
            max_active_runs: 1,
            startup_timeout: Duration::from_secs(1),
            connect_grace: Duration::from_secs(1),
            batch_delay: Duration::from_millis(25),
        },
        state.clone(),
    )
    .unwrap();
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let server = tokio::spawn(serve(listener, state.clone(), execution.clone(), async {
        let _ = shutdown_rx.await;
    }));

    test(socket, state).await;

    let _ = shutdown_tx.send(());
    server.await.unwrap().unwrap();
    execution.shutdown().await.unwrap();
    execution_join.await.unwrap().unwrap();
}

async fn create_project_and_task(socket: &Path, root: &Path, task_body: &str) {
    assert!(matches!(
        request(
            socket,
            LocalRequest::CreateProject {
                id: id::<ProjectId>("project-1"),
                name: "Dark Factory".into(),
                root: root.to_string_lossy().into_owned(),
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
            socket,
            LocalRequest::CreateTask {
                id: id::<TaskId>("task-1"),
                project_id: id::<ProjectId>("project-1"),
                parent_task_id: None,
                title: "First durable run".into(),
                body: task_body.into(),
                priority: 0,
            },
        )
        .await,
        ServerFrame::Response {
            response: LocalResponse::TaskCreated { .. },
            ..
        }
    ));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn agent_creation_and_run_acceptance_are_durable_before_the_response() {
    with_server(|socket, state| async move {
        let root = socket.parent().unwrap().join("project");
        std::fs::create_dir(&root).unwrap();
        let task_body = "private task body must not enter the start response";
        create_project_and_task(&socket, &root, task_body).await;

        let created = request(
            &socket,
            LocalRequest::CreateAgent {
                id: id::<AgentId>("agent-1"),
                project_id: id::<ProjectId>("project-1"),
                parent_agent_id: None,
                role: AgentRole::Worker,
            },
        )
        .await;
        assert!(matches!(
            created,
            ServerFrame::Response {
                protocol_version: PROTOCOL_VERSION,
                response: LocalResponse::AgentCreated { ref agent },
            } if agent.id == id::<AgentId>("agent-1") && agent.provider == Provider::Codex
        ));

        let accepted = request(
            &socket,
            LocalRequest::StartTask {
                project_id: id::<ProjectId>("project-1"),
                task_id: id::<TaskId>("task-1"),
                agent_id: id::<AgentId>("agent-1"),
                parent_run_id: None,
                worktree: root.to_string_lossy().into_owned(),
            },
        )
        .await;
        let run_id = match &accepted {
            ServerFrame::Response {
                protocol_version: PROTOCOL_VERSION,
                response: LocalResponse::RunAccepted { run_id },
            } => run_id.clone(),
            other => panic!("unexpected start response: {other:?}"),
        };
        let response_json = serde_json::to_string(&accepted).unwrap();
        assert!(!response_json.contains(task_body));
        assert!(!response_json.contains(root.to_str().unwrap()));

        let events = state
            .with_store(|store| store.events_after(0, 100))
            .await
            .unwrap();
        assert!(events.iter().any(|event| matches!(
            &event.event,
            FactoryEvent::AgentChanged { agent }
                if agent.id == id::<AgentId>("agent-1") && agent.provider == Provider::Codex
        )));
        assert!(events.iter().any(|event| matches!(
            &event.event,
            FactoryEvent::RunChanged { run }
                if run.id == run_id && run.status == RunStatus::Starting
        )));
        assert!(
            state
                .with_store(move |store| store.execution_target(&run_id).map(|_| ()))
                .await
                .is_ok()
        );
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_invalid_worktree_is_rejected_without_reserving_the_task_or_echoing_the_path() {
    with_server(|socket, state| async move {
        let root = socket.parent().unwrap().join("project");
        std::fs::create_dir(&root).unwrap();
        create_project_and_task(&socket, &root, "bounded task").await;
        request(
            &socket,
            LocalRequest::CreateAgent {
                id: id::<AgentId>("agent-1"),
                project_id: id::<ProjectId>("project-1"),
                parent_agent_id: None,
                role: AgentRole::Worker,
            },
        )
        .await;
        let baseline = state
            .with_store(|store| store.latest_event_sequence())
            .await
            .unwrap();
        let private_missing = root.join("private-missing-worktree");

        let rejected = request(
            &socket,
            LocalRequest::StartTask {
                project_id: id::<ProjectId>("project-1"),
                task_id: id::<TaskId>("task-1"),
                agent_id: id::<AgentId>("agent-1"),
                parent_run_id: None,
                worktree: private_missing.to_string_lossy().into_owned(),
            },
        )
        .await;
        assert!(matches!(
            rejected,
            ServerFrame::Response {
                response: LocalResponse::Error {
                    code: ErrorCode::InvalidRequest,
                    ..
                },
                ..
            }
        ));
        assert!(
            !serde_json::to_string(&rejected)
                .unwrap()
                .contains(private_missing.to_str().unwrap())
        );
        assert_eq!(
            state
                .with_store(|store| store.latest_event_sequence())
                .await
                .unwrap(),
            baseline
        );
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_claude_agent_is_accepted_with_a_durable_fresh_session() {
    with_server(|socket, state| async move {
        let root = socket.parent().unwrap().join("project");
        std::fs::create_dir(&root).unwrap();
        create_project_and_task(&socket, &root, "private migration task").await;
        state
            .commit_and_publish(|store| {
                let (agent, event) = store.create_agent(
                    NewAgent {
                        id: id::<AgentId>("imported-claude"),
                        project_id: id::<ProjectId>("project-1"),
                        parent_agent_id: None,
                        role: AgentRole::Worker,
                        provider: Provider::ClaudeCode,
                    },
                    10,
                )?;
                Ok((agent, vec![event]))
            })
            .await
            .unwrap();
        let accepted = request(
            &socket,
            LocalRequest::StartTask {
                project_id: id::<ProjectId>("project-1"),
                task_id: id::<TaskId>("task-1"),
                agent_id: id::<AgentId>("imported-claude"),
                parent_run_id: None,
                worktree: root.to_string_lossy().into_owned(),
            },
        )
        .await;
        let run_id = match &accepted {
            ServerFrame::Response {
                protocol_version: PROTOCOL_VERSION,
                response: LocalResponse::RunAccepted { run_id },
            } => run_id.clone(),
            other => panic!("unexpected Claude start response: {other:?}"),
        };
        let target = state
            .with_store(move |store| store.execution_target(&run_id))
            .await
            .unwrap();
        assert_eq!(target.provider, Provider::ClaudeCode);
        assert!(!target.resumes_provider_session);
        assert!(target.provider_session_id.is_some());

        let response = serde_json::to_string(&accepted).unwrap();
        assert!(!response.contains("private migration task"));
        assert!(!response.contains("claude_code"));
        assert!(!response.contains("session"));
    })
    .await;
}
