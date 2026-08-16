use std::{
    io::{BufRead, BufReader, Write},
    os::unix::{fs::PermissionsExt, net::UnixListener},
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

/// `factoryctl usage` never touches the daemon: it probes `codex` on `PATH`
/// directly. This exercises the real subprocess/JSON-RPC path against a fake
/// `codex` script rather than the real provider CLI.
#[test]
fn usage_prints_observed_codex_snapshot_from_a_fake_codex_on_path() {
    let directory = tempfile::tempdir().unwrap();
    let fake_codex = directory.path().join("codex");
    std::fs::write(
        &fake_codex,
        "#!/bin/sh\nread -r _initialize\nprintf '{\"id\":1,\"result\":{}}\\n'\nread -r _initialized\nread -r _rate_limits\nprintf '{\"id\":2,\"result\":{\"rateLimits\":{\"primary\":{\"usedPercent\":42,\"resetsAt\":100}}}}\\n'\n",
    )
    .unwrap();
    std::fs::set_permissions(&fake_codex, std::fs::Permissions::from_mode(0o700)).unwrap();
    let home = directory.path().join("home");
    let codex_home = directory.path().join("codex-home");
    std::fs::create_dir(&home).unwrap();
    std::fs::create_dir(&codex_home).unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_factoryctl"))
        .args(["usage"])
        .env("PATH", directory.path())
        .env("HOME", &home)
        .env("CODEX_HOME", &codex_home)
        .env("TMPDIR", "/tmp")
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["ok"], true);
    assert_eq!(value["provider"], "codex");
    assert_eq!(value["usedPercent"], 42);
    assert_eq!(value["limitWindow"], "primary");
    assert_eq!(value["exhausted"], false);
}

#[test]
fn usage_fails_clearly_when_codex_is_not_on_path() {
    let directory = tempfile::tempdir().unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_factoryctl"))
        .args(["usage"])
        .env("PATH", directory.path())
        .output()
        .unwrap();
    assert!(!output.status.success());
    let value: serde_json::Value = serde_json::from_slice(&output.stderr).unwrap();
    assert_eq!(value["ok"], false);
    assert_eq!(value["category"], "not_found");
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
            provider,
            model,
            worktree,
        } = request.request
        else {
            panic!("expected create-agent request");
        };
        assert!(uuid::Uuid::parse_str(id.as_str()).is_ok());
        assert_eq!(project_id, ProjectId::try_from("project-1").unwrap());
        assert_eq!(parent_agent_id, None);
        assert_eq!(role, AgentRole::Worker);
        assert_eq!(provider, Provider::Codex);
        assert_eq!(model, None);
        assert_eq!(worktree, None);
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
                    paused: false,
                    current_session_id: None,
                    worktree: None,
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
                worktree: Some("/work/agent-1".into()),
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
            "--provider",
            "codex",
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

/// `--help` at every level must print human usage text and exit 0, and it
/// must never attempt to reach the daemon: an unreachable socket path
/// proves that, since any real request would fail to connect.
#[test]
fn help_prints_human_usage_and_exits_zero_without_touching_the_daemon() {
    let unreachable_socket = "/nonexistent/dark-factory-help-test.sock";
    for arguments in [
        vec!["--help"],
        vec!["-h"],
        vec!["help"],
        vec!["task", "--help"],
        vec!["task", "add", "--help"],
        vec!["task", "cancel", "--help"],
        vec!["task", "delete", "--help"],
        vec!["task", "update", "--help"],
        vec!["task", "get", "--help"],
        vec!["agent", "--help"],
        vec!["agent", "delete", "--help"],
        vec!["run", "--help"],
        vec!["run", "stop", "--help"],
        vec!["project", "--help"],
        vec!["project", "delete", "--help"],
        vec!["events", "--help"],
    ] {
        let mut full_args = vec!["--socket", unreachable_socket];
        full_args.extend(arguments.iter().copied());
        let output = Command::new(env!("CARGO_BIN_EXE_factoryctl"))
            .args(&full_args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "factoryctl {full_args:?} did not exit 0: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let stdout = String::from_utf8(output.stdout).unwrap();
        assert!(
            stdout.starts_with("usage:") || stdout.starts_with("Dark Factory"),
            "factoryctl {full_args:?} did not print usage text: {stdout:?}"
        );
        assert!(
            serde_json::from_str::<ServerFrame>(&stdout).is_err(),
            "factoryctl {full_args:?} printed a JSON frame instead of help text"
        );
    }
}
