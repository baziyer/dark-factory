use std::{
    future::Future, num::NonZeroU32, os::unix::fs::PermissionsExt, path::Path, time::Duration,
};

use factory_core::{
    AgentId, AgentRole, FactoryEvent, PROTOCOL_VERSION, ProjectId, Provider, RunId, RunStatus,
    RunnerInstanceId, TaskId,
    local::{ErrorCode, LocalRequest, LocalResponse, RequestEnvelope, ServerFrame},
    runner::{OutputStream, RUNNER_PROTOCOL_VERSION, RunnerEvent, RunnerEventEnvelope},
};
use factoryd::{
    daemon_state::DaemonState,
    execution,
    local_api::serve,
    store::{
        NewAgent, RunReservation, RunnerEventEffects, RunnerEventInput, Store, TerminalOutcome,
    },
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
            guidance_root: directory.path().to_path_buf(),
            max_active_runs: 1,
            startup_timeout: Duration::from_secs(1),
            connect_grace: Duration::from_secs(1),
            batch_delay: Duration::from_millis(25),
        },
        state.clone(),
    )
    .unwrap();
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let server = tokio::spawn(serve(
        listener,
        state.clone(),
        execution.clone(),
        directory.path().to_path_buf(),
        async {
            let _ = shutdown_rx.await;
        },
    ));

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
async fn local_agent_profile_round_trips_without_changing_public_agent_events() {
    with_server(|socket, _state| async move {
        let root = socket.parent().unwrap().join("project");
        std::fs::create_dir(&root).unwrap();
        assert!(matches!(
            request(
                &socket,
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
                &socket,
                LocalRequest::CreateAgent {
                    id: id::<AgentId>("god"),
                    project_id: id::<ProjectId>("project-1"),
                    parent_agent_id: None,
                    role: AgentRole::Orchestrator,
                    provider: Provider::Codex,
                    model: Some("gpt-5-codex".into()),
                },
            )
            .await,
            ServerFrame::Response {
                response: LocalResponse::AgentCreated { .. },
                ..
            }
        ));
        let updated = request(
            &socket,
            LocalRequest::UpdateAgentProfile {
                project_id: id::<ProjectId>("project-1"),
                agent_id: id::<AgentId>("god"),
                model: Some("gpt-5-codex".into()),
                permission_mode: Some("on-request".into()),
                instructions: "Coordinate the team.".into(),
                memory: "Keep work bounded.".into(),
            },
        )
        .await;
        match updated {
            ServerFrame::Response {
                response: LocalResponse::AgentProfileUpdated { agent },
                ..
            } => {
                assert_eq!(agent.profile.model.as_deref(), Some("gpt-5-codex"));
                assert_eq!(agent.profile.permission_mode.as_deref(), Some("on-request"));
                assert_eq!(agent.profile.instructions, "Coordinate the team.");
                assert!(!agent.instructions_path.is_empty());
                assert!(!agent.memory_path.is_empty());
                assert!(!agent.project_guidance_path.is_empty());
            }
            other => panic!("unexpected profile response: {other:?}"),
        }
        let loaded = request(
            &socket,
            LocalRequest::GetAgent {
                project_id: id::<ProjectId>("project-1"),
                agent_id: id::<AgentId>("god"),
            },
        )
        .await;
        match loaded {
            ServerFrame::Response {
                response: LocalResponse::Agent { agent },
                ..
            } => {
                assert_eq!(agent.profile.memory, "Keep work bounded.");
                assert_eq!(agent.profile.permission_mode.as_deref(), Some("on-request"));
            }
            other => panic!("unexpected agent response: {other:?}"),
        }
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn local_project_guidance_round_trips_as_a_file_backed_sister_folder() {
    with_server(|socket, _state| async move {
        let root = socket.parent().unwrap().join("project");
        std::fs::create_dir(&root).unwrap();
        assert!(matches!(
            request(
                &socket,
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

        // A freshly created project already has an empty, readable PROJECT.md.
        let created = request(
            &socket,
            LocalRequest::GetProject {
                project_id: id::<ProjectId>("project-1"),
            },
        )
        .await;
        match created {
            ServerFrame::Response {
                response: LocalResponse::Project { project },
                ..
            } => {
                assert_eq!(project.guidance, "");
                assert!(project.guidance_path.ends_with("PROJECT.md"));
            }
            other => panic!("unexpected project response: {other:?}"),
        }

        let updated = request(
            &socket,
            LocalRequest::UpdateProjectGuidance {
                project_id: id::<ProjectId>("project-1"),
                text: "# Dark Factory\n\nBuild the thing.\n".into(),
            },
        )
        .await;
        assert!(matches!(
            updated,
            ServerFrame::Response {
                response: LocalResponse::ProjectGuidanceUpdated { project },
                ..
            } if project.guidance == "# Dark Factory\n\nBuild the thing.\n"
        ));

        let reloaded = request(
            &socket,
            LocalRequest::GetProject {
                project_id: id::<ProjectId>("project-1"),
            },
        )
        .await;
        assert!(matches!(
            reloaded,
            ServerFrame::Response {
                response: LocalResponse::Project { project },
                ..
            } if project.guidance == "# Dark Factory\n\nBuild the thing.\n"
        ));
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn local_task_list_exposes_a_real_persisted_result() {
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
                provider: Provider::Codex,
                model: None,
            },
        )
        .await;

        let run_id = id::<RunId>("run-1");
        let runner_instance_id = id::<RunnerInstanceId>("instance-1");
        let root_string = root.to_string_lossy().into_owned();
        let ingest_run_id = run_id.clone();
        let ingest_instance_id = runner_instance_id.clone();
        state
            .commit_and_publish(move |store| {
                let reserved = store.reserve_task_run(
                    RunReservation {
                        project_id: id::<ProjectId>("project-1"),
                        task_id: id::<TaskId>("task-1"),
                        agent_id: id::<AgentId>("agent-1"),
                        expected_provider: Provider::Codex,
                        run_id: run_id.clone(),
                        parent_run_id: None,
                        worktree: root_string.clone(),
                        fresh_provider_session_id: None,
                        runner_instance_id: runner_instance_id.clone(),
                        runner_runtime: root_string.clone() + "/run",
                    },
                    1,
                    10,
                )?;
                let ingested = store.ingest_runner_events(
                    &ingest_run_id,
                    &ingest_instance_id,
                    vec![
                        RunnerEventInput {
                            event: RunnerEventEnvelope {
                                protocol_version: RUNNER_PROTOCOL_VERSION,
                                sequence: 1,
                                occurred_at_ms: 11,
                                event: RunnerEvent::Started { child_pid: 41 },
                            },
                            effects: RunnerEventEffects {
                                confirmed_provider_session_id: None,
                                terminal_outcome: None,
                            },
                        },
                        RunnerEventInput {
                            event: RunnerEventEnvelope {
                                protocol_version: RUNNER_PROTOCOL_VERSION,
                                sequence: 2,
                                occurred_at_ms: 12,
                                event: RunnerEvent::Output {
                                    stream: OutputStream::Stdout,
                                    text: "provider output".into(),
                                    lossy: false,
                                },
                            },
                            effects: RunnerEventEffects {
                                confirmed_provider_session_id: Some(
                                    "01a007ea-f53a-76c2-9a54-d44b3fce0fb7".into(),
                                ),
                                terminal_outcome: None,
                            },
                        },
                        RunnerEventInput {
                            event: RunnerEventEnvelope {
                                protocol_version: RUNNER_PROTOCOL_VERSION,
                                sequence: 3,
                                occurred_at_ms: 13,
                                event: RunnerEvent::Exited {
                                    exit_code: Some(0),
                                    signal: None,
                                },
                            },
                            effects: RunnerEventEffects {
                                confirmed_provider_session_id: None,
                                terminal_outcome: Some(TerminalOutcome::Succeeded {
                                    result: Some("Real local result".into()),
                                }),
                            },
                        },
                    ],
                    20,
                )?;
                let mut events = reserved.events;
                events.extend(ingested.events);
                Ok(((), events))
            })
            .await
            .unwrap();

        match request(
            &socket,
            LocalRequest::ListTasks {
                project_id: id::<ProjectId>("project-1"),
                after_id: None,
                limit: 10,
            },
        )
        .await
        {
            ServerFrame::Response {
                response: LocalResponse::Tasks { tasks, .. },
                ..
            } => assert_eq!(tasks[0].result.as_deref(), Some("Real local result")),
            other => panic!("unexpected response: {other:?}"),
        }
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn local_run_terminal_reads_private_spool_and_stop_controls_the_exact_runner() {
    with_server(|socket, state| async move {
        let root = socket.parent().unwrap().join("terminal-project");
        std::fs::create_dir(&root).unwrap();
        create_project_and_task(&socket, &root, "terminal task").await;
        request(
            &socket,
            LocalRequest::CreateAgent {
                id: id::<AgentId>("agent-1"),
                project_id: id::<ProjectId>("project-1"),
                parent_agent_id: None,
                role: AgentRole::Worker,
                provider: Provider::Codex,
                model: None,
            },
        )
        .await;

        let run_id = id::<RunId>("run-terminal");
        let runner_instance_id = id::<RunnerInstanceId>("instance-terminal");
        let runtime = root.join("run-terminal");
        std::fs::create_dir(&runtime).unwrap();
        let root_string = root.to_string_lossy().into_owned();
        let runtime_string = runtime.to_string_lossy().into_owned();
        let reservation_run_id = run_id.clone();
        let reservation_instance_id = runner_instance_id.clone();
        state
            .commit_and_publish(move |store| {
                let reserved = store.reserve_task_run(
                    RunReservation {
                        project_id: id::<ProjectId>("project-1"),
                        task_id: id::<TaskId>("task-1"),
                        agent_id: id::<AgentId>("agent-1"),
                        expected_provider: Provider::Codex,
                        run_id: reservation_run_id,
                        parent_run_id: None,
                        worktree: root_string,
                        fresh_provider_session_id: None,
                        runner_instance_id: reservation_instance_id,
                        runner_runtime: runtime_string,
                    },
                    1,
                    10,
                )?;
                Ok(((), reserved.events))
            })
            .await
            .unwrap();

        let events = [
            RunnerEventEnvelope {
                protocol_version: RUNNER_PROTOCOL_VERSION,
                sequence: 1,
                occurred_at_ms: 11,
                event: RunnerEvent::Started { child_pid: 41 },
            },
            RunnerEventEnvelope {
                protocol_version: RUNNER_PROTOCOL_VERSION,
                sequence: 2,
                occurred_at_ms: 12,
                event: RunnerEvent::Output {
                    stream: OutputStream::Stdout,
                    text: "ready\n".into(),
                    lossy: false,
                },
            },
            RunnerEventEnvelope {
                protocol_version: RUNNER_PROTOCOL_VERSION,
                sequence: 3,
                occurred_at_ms: 13,
                event: RunnerEvent::Output {
                    stream: OutputStream::Stderr,
                    text: "warning\u{1b}[31m\n".into(),
                    lossy: false,
                },
            },
        ];
        let mut spool = Vec::new();
        for event in events {
            spool.extend(serde_json::to_vec(&event).unwrap());
            spool.push(b'\n');
        }
        std::fs::write(runtime.join("events.ndjson"), spool).unwrap();

        match request(
            &socket,
            LocalRequest::GetRunTerminal {
                project_id: id::<ProjectId>("project-1"),
                run_id: run_id.clone(),
            },
        )
        .await
        {
            ServerFrame::Response {
                response: LocalResponse::RunTerminal { terminal },
                ..
            } => {
                assert_eq!(terminal.run_id, run_id);
                assert_eq!(terminal.head_sequence, 3);
                assert_eq!(terminal.output, "[stdout] ready\n[stderr] warning[31m\n");
                assert!(!terminal.truncated);
            }
            other => panic!("unexpected terminal response: {other:?}"),
        }

        let listener = tokio::net::UnixListener::bind(runtime.join("control.sock")).unwrap();
        let expected_run_id = run_id.clone();
        let expected_instance_id = runner_instance_id.clone();
        let fake_runner = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut reader = BufReader::new(stream);
            let mut line = String::new();
            reader.read_line(&mut line).await.unwrap();
            let envelope: factory_core::runner::RequestEnvelope =
                serde_json::from_str(&line).unwrap();
            assert_eq!(envelope.run_id, expected_run_id);
            assert_eq!(envelope.runner_instance_id, expected_instance_id);
            let command_id = match envelope.request {
                factory_core::runner::RunnerRequest::Stop {
                    command_id,
                    grace_ms: 2_000,
                } => command_id,
                other => panic!("unexpected runner request: {other:?}"),
            };
            let mut stream = reader.into_inner();
            let frame = factory_core::runner::RunnerFrame::CommandAck {
                protocol_version: factory_core::runner::RUNNER_PROTOCOL_VERSION,
                command_id,
            };
            let mut payload = serde_json::to_vec(&frame).unwrap();
            payload.push(b'\n');
            stream.write_all(&payload).await.unwrap();
        });

        assert!(matches!(
            request(
                &socket,
                LocalRequest::StopRun {
                    project_id: id::<ProjectId>("project-1"),
                    run_id: run_id.clone(),
                    grace_ms: 2_000,
                },
            )
            .await,
            ServerFrame::Response {
                response: LocalResponse::RunStopped { run_id: stopped },
                ..
            } if stopped == run_id
        ));
        fake_runner.await.unwrap();
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn get_run_terminal_is_not_found_when_the_spool_is_missing() {
    with_server(|socket, state| async move {
        let root = socket.parent().unwrap().join("missing-spool-project");
        std::fs::create_dir(&root).unwrap();
        create_project_and_task(&socket, &root, "missing spool task").await;
        request(
            &socket,
            LocalRequest::CreateAgent {
                id: id::<AgentId>("agent-1"),
                project_id: id::<ProjectId>("project-1"),
                parent_agent_id: None,
                role: AgentRole::Worker,
                provider: Provider::Codex,
            },
        )
        .await;

        let run_id = id::<RunId>("run-missing-spool");
        let runner_instance_id = id::<RunnerInstanceId>("instance-missing-spool");
        // Deliberately do not create the runtime directory or its spool.
        let runtime = root.join("run-missing-spool");
        let root_string = root.to_string_lossy().into_owned();
        let runtime_string = runtime.to_string_lossy().into_owned();
        let reservation_run_id = run_id.clone();
        state
            .commit_and_publish(move |store| {
                let reserved = store.reserve_task_run(
                    RunReservation {
                        project_id: id::<ProjectId>("project-1"),
                        task_id: id::<TaskId>("task-1"),
                        agent_id: id::<AgentId>("agent-1"),
                        expected_provider: Provider::Codex,
                        run_id: reservation_run_id,
                        parent_run_id: None,
                        worktree: root_string,
                        fresh_provider_session_id: None,
                        runner_instance_id,
                        runner_runtime: runtime_string,
                    },
                    1,
                    10,
                )?;
                Ok(((), reserved.events))
            })
            .await
            .unwrap();

        match request(
            &socket,
            LocalRequest::GetRunTerminal {
                project_id: id::<ProjectId>("project-1"),
                run_id,
            },
        )
        .await
        {
            ServerFrame::Response {
                response:
                    LocalResponse::Error {
                        code: ErrorCode::NotFound,
                        ..
                    },
                ..
            } => {}
            other => panic!("expected a NotFound error, got {other:?}"),
        }
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn get_run_terminal_degrades_a_malformed_non_final_line_to_truncated() {
    with_server(|socket, state| async move {
        let root = socket.parent().unwrap().join("malformed-spool-project");
        std::fs::create_dir(&root).unwrap();
        create_project_and_task(&socket, &root, "malformed spool task").await;
        request(
            &socket,
            LocalRequest::CreateAgent {
                id: id::<AgentId>("agent-1"),
                project_id: id::<ProjectId>("project-1"),
                parent_agent_id: None,
                role: AgentRole::Worker,
                provider: Provider::Codex,
            },
        )
        .await;

        let run_id = id::<RunId>("run-malformed-spool");
        let runner_instance_id = id::<RunnerInstanceId>("instance-malformed-spool");
        let runtime = root.join("run-malformed-spool");
        std::fs::create_dir(&runtime).unwrap();
        let root_string = root.to_string_lossy().into_owned();
        let runtime_string = runtime.to_string_lossy().into_owned();
        let reservation_run_id = run_id.clone();
        state
            .commit_and_publish(move |store| {
                let reserved = store.reserve_task_run(
                    RunReservation {
                        project_id: id::<ProjectId>("project-1"),
                        task_id: id::<TaskId>("task-1"),
                        agent_id: id::<AgentId>("agent-1"),
                        expected_provider: Provider::Codex,
                        run_id: reservation_run_id,
                        parent_run_id: None,
                        worktree: root_string,
                        fresh_provider_session_id: None,
                        runner_instance_id,
                        runner_runtime: runtime_string,
                    },
                    1,
                    10,
                )?;
                Ok(((), reserved.events))
            })
            .await
            .unwrap();

        // A valid record, then a torn/malformed record (as a concurrent
        // writer might leave behind), then another valid record that must
        // never surface: the spool is append-only, so a torn record in the
        // middle can never become readable later.
        let kept = RunnerEventEnvelope {
            protocol_version: RUNNER_PROTOCOL_VERSION,
            sequence: 1,
            occurred_at_ms: 11,
            event: RunnerEvent::Output {
                stream: OutputStream::Stdout,
                text: "kept\n".into(),
                lossy: false,
            },
        };
        let dropped = RunnerEventEnvelope {
            protocol_version: RUNNER_PROTOCOL_VERSION,
            sequence: 3,
            occurred_at_ms: 13,
            event: RunnerEvent::Output {
                stream: OutputStream::Stdout,
                text: "dropped\n".into(),
                lossy: false,
            },
        };
        let mut spool = Vec::new();
        spool.extend(serde_json::to_vec(&kept).unwrap());
        spool.push(b'\n');
        spool.extend(b"{not valid json");
        spool.push(b'\n');
        spool.extend(serde_json::to_vec(&dropped).unwrap());
        spool.push(b'\n');
        std::fs::write(runtime.join("events.ndjson"), spool).unwrap();

        match request(
            &socket,
            LocalRequest::GetRunTerminal {
                project_id: id::<ProjectId>("project-1"),
                run_id,
            },
        )
        .await
        {
            ServerFrame::Response {
                response: LocalResponse::RunTerminal { terminal },
                ..
            } => {
                assert!(terminal.output.contains("kept"));
                assert!(!terminal.output.contains("dropped"));
                assert!(terminal.truncated);
                assert_eq!(terminal.head_sequence, 1);
            }
            other => panic!("unexpected terminal response: {other:?}"),
        }
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn local_attach_terminal_and_terminal_input_proxy_to_the_exact_runner() {
    with_server(|socket, state| async move {
        let root = socket.parent().unwrap().join("attach-project");
        std::fs::create_dir(&root).unwrap();
        create_project_and_task(&socket, &root, "attach task").await;
        request(
            &socket,
            LocalRequest::CreateAgent {
                id: id::<AgentId>("agent-1"),
                project_id: id::<ProjectId>("project-1"),
                parent_agent_id: None,
                role: AgentRole::Worker,
                provider: Provider::Codex,
            },
        )
        .await;

        let run_id = id::<RunId>("run-attach");
        let runner_instance_id = id::<RunnerInstanceId>("instance-attach");
        let runtime = root.join("run-attach");
        std::fs::create_dir(&runtime).unwrap();
        let root_string = root.to_string_lossy().into_owned();
        let runtime_string = runtime.to_string_lossy().into_owned();
        let reservation_run_id = run_id.clone();
        let reservation_instance_id = runner_instance_id.clone();
        state
            .commit_and_publish(move |store| {
                let reserved = store.reserve_task_run(
                    RunReservation {
                        project_id: id::<ProjectId>("project-1"),
                        task_id: id::<TaskId>("task-1"),
                        agent_id: id::<AgentId>("agent-1"),
                        expected_provider: Provider::Codex,
                        run_id: reservation_run_id,
                        parent_run_id: None,
                        worktree: root_string,
                        fresh_provider_session_id: None,
                        runner_instance_id: reservation_instance_id,
                        runner_runtime: runtime_string,
                    },
                    1,
                    10,
                )?;
                Ok(((), reserved.events))
            })
            .await
            .unwrap();

        let listener = tokio::net::UnixListener::bind(runtime.join("control.sock")).unwrap();
        let expected_run_id = run_id.clone();
        let expected_instance_id = runner_instance_id.clone();
        let fake_runner = tokio::spawn(async move {
            // First connection: AttachTerminal streams one output chunk.
            let (stream, _) = listener.accept().await.unwrap();
            let mut reader = BufReader::new(stream);
            let mut line = String::new();
            reader.read_line(&mut line).await.unwrap();
            let envelope: factory_core::runner::RequestEnvelope =
                serde_json::from_str(&line).unwrap();
            assert_eq!(envelope.run_id, expected_run_id);
            assert_eq!(envelope.runner_instance_id, expected_instance_id);
            assert!(matches!(
                envelope.request,
                factory_core::runner::RunnerRequest::AttachTerminal { since_offset: 0 }
            ));
            let mut stream = reader.into_inner();
            let frame = factory_core::runner::RunnerFrame::TerminalOutput {
                protocol_version: factory_core::runner::RUNNER_PROTOCOL_VERSION,
                offset: 0,
                bytes: factory_core::runner::encode_terminal_bytes(b"hello from pty"),
            };
            let mut payload = serde_json::to_vec(&frame).unwrap();
            payload.push(b'\n');
            stream.write_all(&payload).await.unwrap();
            drop(stream);

            // Second connection: TerminalInput as a one-shot request.
            let (stream, _) = listener.accept().await.unwrap();
            let mut reader = BufReader::new(stream);
            let mut line = String::new();
            reader.read_line(&mut line).await.unwrap();
            let envelope: factory_core::runner::RequestEnvelope =
                serde_json::from_str(&line).unwrap();
            match envelope.request {
                factory_core::runner::RunnerRequest::TerminalInput { bytes } => {
                    assert_eq!(
                        factory_core::runner::decode_terminal_bytes(&bytes).unwrap(),
                        b"marco".to_vec()
                    );
                }
                other => panic!("unexpected runner request: {other:?}"),
            }
            let mut stream = reader.into_inner();
            let frame = factory_core::runner::RunnerFrame::CommandAck {
                protocol_version: factory_core::runner::RUNNER_PROTOCOL_VERSION,
                command_id: "terminal-input".into(),
            };
            let mut payload = serde_json::to_vec(&frame).unwrap();
            payload.push(b'\n');
            stream.write_all(&payload).await.unwrap();
        });

        let mut stream = UnixStream::connect(&socket).await.unwrap();
        let mut payload = serde_json::to_vec(&RequestEnvelope::new(LocalRequest::AttachTerminal {
            project_id: id::<ProjectId>("project-1"),
            run_id: run_id.clone(),
            since_offset: 0,
        }))
        .unwrap();
        payload.push(b'\n');
        stream.write_all(&payload).await.unwrap();
        let mut attach_reader = BufReader::new(stream);
        let mut line = String::new();
        attach_reader.read_line(&mut line).await.unwrap();
        match serde_json::from_str::<ServerFrame>(&line).unwrap() {
            ServerFrame::TerminalOutput {
                run_id: got_run_id,
                offset,
                bytes,
                ..
            } => {
                assert_eq!(got_run_id, run_id);
                assert_eq!(offset, 0);
                assert_eq!(
                    factory_core::runner::decode_terminal_bytes(&bytes).unwrap(),
                    b"hello from pty".to_vec()
                );
            }
            other => panic!("unexpected server frame: {other:?}"),
        }
        drop(attach_reader);

        assert!(matches!(
            request(
                &socket,
                LocalRequest::TerminalInput {
                    project_id: id::<ProjectId>("project-1"),
                    run_id: run_id.clone(),
                    bytes: factory_core::runner::encode_terminal_bytes(b"marco"),
                },
            )
            .await,
            ServerFrame::Response {
                response: LocalResponse::TerminalInputAccepted { run_id: accepted },
                ..
            } if accepted == run_id
        ));

        fake_runner.await.unwrap();
    })
    .await;
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
                provider: Provider::Codex,
                model: None,
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
                provider: Provider::Codex,
                model: None,
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
        create_project_and_task(&socket, &root, "private Claude task").await;
        state
            .commit_and_publish(|store| {
                let (agent, event) = store.create_agent(
                    NewAgent {
                        id: id::<AgentId>("claude-agent"),
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
                agent_id: id::<AgentId>("claude-agent"),
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
        assert!(!response.contains("private Claude task"));
        assert!(!response.contains("claude_code"));
        assert!(!response.contains("session"));
    })
    .await;
}
