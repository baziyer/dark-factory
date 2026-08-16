use std::{
    io::{BufRead, BufReader, Write},
    os::unix::net::UnixListener,
    process::Command,
    thread,
};

use factory_core::{
    AgentId, AgentRole, AgentSnapshot, EventEnvelope, FactoryEvent, PROTOCOL_VERSION, ProjectId,
    ProjectSnapshot, Provider, RunId, TaskId,
    local::{LocalRequest, LocalResponse, RequestEnvelope, ServerFrame},
};

fn write_response(stream: &mut std::os::unix::net::UnixStream, response: LocalResponse) {
    serde_json::to_writer(
        &mut *stream,
        &ServerFrame::Response {
            protocol_version: PROTOCOL_VERSION,
            response,
        },
    )
    .unwrap();
    stream.write_all(b"\n").unwrap();
}

#[test]
fn health_prints_exactly_one_machine_readable_server_frame() {
    let directory = tempfile::tempdir().unwrap();
    let socket = directory.path().join("factory.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut line = String::new();
        BufReader::new(stream.try_clone().unwrap())
            .read_line(&mut line)
            .unwrap();
        assert_eq!(
            serde_json::from_str::<RequestEnvelope>(&line).unwrap(),
            RequestEnvelope::new(LocalRequest::Health)
        );
        serde_json::to_writer(
            &mut stream,
            &ServerFrame::Response {
                protocol_version: PROTOCOL_VERSION,
                response: LocalResponse::Health,
            },
        )
        .unwrap();
        stream.write_all(b"\n").unwrap();
    });

    let output = Command::new(env!("CARGO_BIN_EXE_factoryctl"))
        .args(["--socket", socket.to_str().unwrap(), "health"])
        .output()
        .unwrap();

    assert!(output.status.success());
    assert!(output.stderr.is_empty());
    assert_eq!(
        output.stdout.iter().filter(|byte| **byte == b'\n').count(),
        1
    );
    assert!(matches!(
        serde_json::from_slice::<ServerFrame>(&output.stdout).unwrap(),
        ServerFrame::Response {
            response: LocalResponse::Health,
            ..
        }
    ));
    server.join().unwrap();
}

#[test]
fn events_follow_reports_the_replay_cursor_when_the_daemon_disconnects() {
    let directory = tempfile::tempdir().unwrap();
    let socket = directory.path().join("factory.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut line = String::new();
        BufReader::new(stream.try_clone().unwrap())
            .read_line(&mut line)
            .unwrap();
        assert_eq!(
            serde_json::from_str::<RequestEnvelope>(&line).unwrap(),
            RequestEnvelope::new(LocalRequest::Subscribe { after_sequence: 7 })
        );
        for frame in [
            ServerFrame::Response {
                protocol_version: PROTOCOL_VERSION,
                response: LocalResponse::Subscribed {
                    after_sequence: 7,
                    replay_through: 7,
                },
            },
            ServerFrame::Event {
                protocol_version: PROTOCOL_VERSION,
                event: EventEnvelope {
                    protocol_version: PROTOCOL_VERSION,
                    sequence: 8,
                    occurred_at_ms: 1_000,
                    event: FactoryEvent::ProjectChanged {
                        project: ProjectSnapshot {
                            id: ProjectId::try_from("project-1").unwrap(),
                            name: "Project One".into(),
                            root: "/work/project-one".into(),
                            created_at_ms: 1_000,
                            updated_at_ms: 1_000,
                        },
                    },
                },
            },
        ] {
            serde_json::to_writer(&mut stream, &frame).unwrap();
            stream.write_all(b"\n").unwrap();
        }
    });

    let output = Command::new(env!("CARGO_BIN_EXE_factoryctl"))
        .args([
            "--socket",
            socket.to_str().unwrap(),
            "events",
            "--after",
            "7",
            "--follow",
        ])
        .output()
        .unwrap();

    assert!(!output.status.success());
    assert_eq!(
        output.stdout.iter().filter(|byte| **byte == b'\n').count(),
        2
    );
    let error: serde_json::Value = serde_json::from_slice(&output.stderr).unwrap();
    assert_eq!(
        error["error"],
        "event stream disconnected; resume after sequence 8"
    );
    server.join().unwrap();
}

#[test]
fn agent_add_and_task_start_each_emit_one_machine_readable_response() {
    let directory = tempfile::tempdir().unwrap();
    let socket = directory.path().join("factory.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let server = thread::spawn(move || {
        let (mut agent_stream, _) = listener.accept().unwrap();
        let mut line = String::new();
        BufReader::new(agent_stream.try_clone().unwrap())
            .read_line(&mut line)
            .unwrap();
        let request = serde_json::from_str::<RequestEnvelope>(&line).unwrap();
        let LocalRequest::CreateAgent {
            id,
            project_id,
            parent_agent_id,
            role,
        } = request.request
        else {
            panic!("expected create-agent request");
        };
        assert!(uuid::Uuid::parse_str(id.as_str()).is_ok());
        assert_eq!(project_id, ProjectId::try_from("project-1").unwrap());
        assert_eq!(parent_agent_id, None);
        assert_eq!(role, AgentRole::Worker);
        write_response(
            &mut agent_stream,
            LocalResponse::AgentCreated {
                agent: AgentSnapshot {
                    id,
                    project_id,
                    parent_agent_id,
                    role,
                    provider: Provider::Codex,
                    current_run_id: None,
                    created_at_ms: 1,
                    updated_at_ms: 1,
                },
            },
        );

        let (mut start_stream, _) = listener.accept().unwrap();
        line.clear();
        BufReader::new(start_stream.try_clone().unwrap())
            .read_line(&mut line)
            .unwrap();
        assert_eq!(
            serde_json::from_str::<RequestEnvelope>(&line).unwrap(),
            RequestEnvelope::new(LocalRequest::StartTask {
                project_id: ProjectId::try_from("project-1").unwrap(),
                task_id: TaskId::try_from("task-1").unwrap(),
                agent_id: AgentId::try_from("agent-1").unwrap(),
                parent_run_id: None,
                worktree: "/work/agent-1".into(),
            })
        );
        write_response(
            &mut start_stream,
            LocalResponse::RunAccepted {
                run_id: RunId::try_from("run-1").unwrap(),
            },
        );
    });

    let agent = Command::new(env!("CARGO_BIN_EXE_factoryctl"))
        .args([
            "--socket",
            socket.to_str().unwrap(),
            "agent",
            "add",
            "--project",
            "project-1",
            "--role",
            "worker",
        ])
        .output()
        .unwrap();
    assert!(agent.status.success());
    assert!(agent.stderr.is_empty());
    assert_eq!(
        agent.stdout.iter().filter(|byte| **byte == b'\n').count(),
        1
    );
    assert!(matches!(
        serde_json::from_slice::<ServerFrame>(&agent.stdout).unwrap(),
        ServerFrame::Response {
            response: LocalResponse::AgentCreated { .. },
            ..
        }
    ));

    let start = Command::new(env!("CARGO_BIN_EXE_factoryctl"))
        .args([
            "--socket",
            socket.to_str().unwrap(),
            "task",
            "start",
            "--project",
            "project-1",
            "--task",
            "task-1",
            "--agent",
            "agent-1",
            "--worktree",
            "/work/agent-1",
        ])
        .output()
        .unwrap();
    assert!(start.status.success());
    assert!(start.stderr.is_empty());
    assert_eq!(
        start.stdout.iter().filter(|byte| **byte == b'\n').count(),
        1
    );
    assert!(matches!(
        serde_json::from_slice::<ServerFrame>(&start.stdout).unwrap(),
        ServerFrame::Response {
            response: LocalResponse::RunAccepted { .. },
            ..
        }
    ));
    server.join().unwrap();
}
