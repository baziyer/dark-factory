use std::{
    io::{BufRead, BufReader, Write},
    os::unix::{fs::PermissionsExt, net::UnixListener},
    process::{Command, Stdio},
    thread,
};

use factory_core::{
    AgentId, AgentRole, AgentSnapshot, EventEnvelope, FactoryEvent, PROTOCOL_VERSION, ProjectId,
    ProjectSnapshot, Provider, ProviderHookEvent, RunId, TaskId,
    local::{LocalRequest, LocalResponse, RequestEnvelope, ServerFrame},
    status::FleetStatus,
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

fn factoryctl_command() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_factoryctl"));
    command.env_remove("DARK_FACTORY_SESSION_TOKEN_FILE");
    command
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

    let output = factoryctl_command()
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
    let output = factoryctl_command()
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
                response: LocalResponse::Health {
                    runner_path: "/opt/factory-runner".to_owned(),
                    factoryctl_path: "/opt/factoryctl".to_owned(),
                    version: "0.1.0".to_owned(),
                    process_id: 0,
                },
            },
        )
        .unwrap();
        stream.write_all(b"\n").unwrap();
    });

    let output = factoryctl_command()
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
            response: LocalResponse::Health { .. },
            ..
        }
    ));
    server.join().unwrap();
}

#[test]
fn status_is_human_by_default_and_json_preserves_the_protocol_frame() {
    let directory = tempfile::tempdir().unwrap();
    let socket = directory.path().join("factory.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let status = FleetStatus {
        generated_at_ms: 123,
        event_sequence: 0,
        auto_mode: true,
        live_session_cap: 4,
        live_sessions: 0,
        projects: Vec::new(),
        attention: Vec::new(),
    };
    let expected_frame = ServerFrame::Response {
        protocol_version: PROTOCOL_VERSION,
        response: LocalResponse::FleetStatus {
            status: status.clone(),
        },
    };
    let mut expected_json = serde_json::to_vec(&expected_frame).unwrap();
    expected_json.push(b'\n');
    let server = thread::spawn(move || {
        for _ in 0..3 {
            let (mut stream, _) = listener.accept().unwrap();
            let mut line = String::new();
            BufReader::new(stream.try_clone().unwrap())
                .read_line(&mut line)
                .unwrap();
            match serde_json::from_str::<RequestEnvelope>(&line)
                .unwrap()
                .request
            {
                LocalRequest::FleetStatus => write_response(
                    &mut stream,
                    LocalResponse::FleetStatus {
                        status: status.clone(),
                    },
                ),
                LocalRequest::Health => write_response(
                    &mut stream,
                    LocalResponse::Health {
                        runner_path: "/runner".to_owned(),
                        factoryctl_path: "/factoryctl".to_owned(),
                        version: env!("CARGO_PKG_VERSION").to_owned(),
                        process_id: 1,
                    },
                ),
                request => panic!("unexpected request: {request:?}"),
            }
        }
    });

    let human = factoryctl_command()
        .args(["--socket", socket.to_str().unwrap(), "status"])
        .output()
        .unwrap();
    assert!(human.status.success(), "{human:?}");
    assert!(human.stderr.is_empty());
    assert_eq!(
        String::from_utf8(human.stdout).unwrap(),
        concat!(
            "Dark Factory: factoryctl v",
            env!("CARGO_PKG_VERSION"),
            " | active runtime v",
            env!("CARGO_PKG_VERSION"),
            " | auto on | sessions 0/4 | projects 0 | attention 0\n"
        )
    );

    let json = factoryctl_command()
        .args(["--socket", socket.to_str().unwrap(), "status", "--json"])
        .output()
        .unwrap();
    assert!(json.status.success(), "{json:?}");
    assert!(json.stderr.is_empty());
    assert_eq!(json.stdout, expected_json);
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

    let output = factoryctl_command()
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
            reasoning_effort,
            model_selection_reason,
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
        assert_eq!(reasoning_effort, None);
        assert_eq!(model_selection_reason, None);
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

    let agent = factoryctl_command()
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

    let start = factoryctl_command()
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

/// `factoryctl hook` forwards its stdin payload and the token file's
/// contents to the daemon, then prints the daemon's `reply` verbatim (not
/// wrapped in a `ServerFrame`) and exits 0.
#[test]
fn hook_forwards_the_stdin_payload_and_prints_the_daemon_reply_verbatim() {
    let directory = tempfile::tempdir().unwrap();
    let socket = directory.path().join("factory.sock");
    let token_path = directory.path().join("hook.token");
    std::fs::write(&token_path, "session-token-value").unwrap();
    let listener = UnixListener::bind(&socket).unwrap();
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut line = String::new();
        BufReader::new(stream.try_clone().unwrap())
            .read_line(&mut line)
            .unwrap();
        assert_eq!(
            serde_json::from_str::<RequestEnvelope>(&line).unwrap(),
            RequestEnvelope::authenticated(
                LocalRequest::ProviderHook {
                    event: ProviderHookEvent::Stop,
                    payload: serde_json::json!({"stop_hook_active": false}),
                },
                factory_core::local::SessionCredential::new("session-token-value".into()),
            )
        );
        write_response(
            &mut stream,
            LocalResponse::ProviderHookReply {
                reply: serde_json::json!({"decision": "block", "reason": "deliver next task"}),
            },
        );
    });

    let mut child = factoryctl_command()
        .args([
            "--socket",
            socket.to_str().unwrap(),
            "hook",
            "--token-file",
            token_path.to_str().unwrap(),
            "Stop",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(br#"{"stop_hook_active": false}"#)
        .unwrap();
    let output = child.wait_with_output().unwrap();

    assert!(output.status.success());
    let reply: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(
        reply,
        serde_json::json!({"decision": "block", "reason": "deliver next task"})
    );
    server.join().unwrap();
}

/// A hook invocation must never wedge the operator's live provider session:
/// an unreachable daemon socket still exits 0 and prints `{}`.
#[test]
fn hook_fails_open_and_prints_an_empty_object_when_the_daemon_is_unreachable() {
    let directory = tempfile::tempdir().unwrap();
    let token_path = directory.path().join("hook.token");
    std::fs::write(&token_path, "session-token-value").unwrap();
    let unreachable_socket = directory.path().join("no-daemon-here.sock");

    let mut child = factoryctl_command()
        .args([
            "--socket",
            unreachable_socket.to_str().unwrap(),
            "hook",
            "--token-file",
            token_path.to_str().unwrap(),
            "SessionStart",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(b"{}").unwrap();
    let output = child.wait_with_output().unwrap();

    assert!(output.status.success());
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&output.stdout).unwrap(),
        serde_json::json!({})
    );
}

/// Same fail-open contract when the token file itself cannot be read (a
/// provider misconfiguration, not a daemon problem) — still exits 0 and
/// prints `{}`, without ever attempting to contact the daemon.
#[test]
fn hook_fails_open_when_the_token_file_is_missing() {
    let directory = tempfile::tempdir().unwrap();
    let unreachable_socket = directory.path().join("no-daemon-here.sock");
    let missing_token = directory.path().join("missing.token");

    let mut child = factoryctl_command()
        .args([
            "--socket",
            unreachable_socket.to_str().unwrap(),
            "hook",
            "--token-file",
            missing_token.to_str().unwrap(),
            "Notification",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(b"{}").unwrap();
    let output = child.wait_with_output().unwrap();

    assert!(output.status.success());
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&output.stdout).unwrap(),
        serde_json::json!({})
    );
}

#[test]
fn pre_tool_use_fails_closed_when_the_policy_daemon_is_unavailable() {
    let directory = tempfile::tempdir().unwrap();
    let missing_token = directory.path().join("missing.token");
    let mut child = factoryctl_command()
        .args([
            "--socket",
            directory.path().join("missing.sock").to_str().unwrap(),
            "hook",
            "--token-file",
            missing_token.to_str().unwrap(),
            "PreToolUse",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(b"{}").unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success());
    let reply: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(reply["hookSpecificOutput"]["permissionDecision"], "deny");
}

/// `--project` may be omitted anywhere it is otherwise required if
/// `$DARK_FACTORY_PROJECT` is set in the process environment, matching how
/// the daemon exports it into a session.
#[test]
fn project_flag_falls_back_to_the_dark_factory_project_environment_variable() {
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
            RequestEnvelope::new(LocalRequest::GetProject {
                project_id: ProjectId::try_from("factory").unwrap(),
            })
        );
        // The request having reached the daemon with the env-resolved
        // project ID, matched above, is what this test is checking; the
        // response contents are irrelevant, so a minimal frame is enough.
        write_response(
            &mut stream,
            LocalResponse::Health {
                runner_path: "/opt/factory-runner".to_owned(),
                factoryctl_path: "/opt/factoryctl".to_owned(),
                version: "0.1.0".to_owned(),
                process_id: 0,
            },
        );
    });

    let output = factoryctl_command()
        .args(["--socket", socket.to_str().unwrap(), "project", "get"])
        .env("DARK_FACTORY_PROJECT", "factory")
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    server.join().unwrap();
}

/// Sender identity is absent from the request body; the daemon derives it
/// from an authenticated session or records an operator sender.
#[test]
fn agent_message_sender_is_not_present_in_the_request_body() {
    let directory = tempfile::tempdir().unwrap();
    let socket = directory.path().join("factory.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut line = String::new();
        BufReader::new(stream.try_clone().unwrap())
            .read_line(&mut line)
            .unwrap();
        let request = serde_json::from_str::<RequestEnvelope>(&line).unwrap();
        let LocalRequest::SendAgentMessage {
            recipient_agent_id,
            body,
            ..
        } = request.request
        else {
            panic!("expected send-agent-message request");
        };
        assert_eq!(recipient_agent_id, AgentId::try_from("god").unwrap());
        assert_eq!(body, "status update");
        write_response(
            &mut stream,
            LocalResponse::Health {
                runner_path: "/opt/factory-runner".to_owned(),
                factoryctl_path: "/opt/factoryctl".to_owned(),
                version: "0.1.0".to_owned(),
                process_id: 0,
            },
        );
    });

    let output = factoryctl_command()
        .args([
            "--socket",
            socket.to_str().unwrap(),
            "agent",
            "message",
            "--project",
            "factory",
            "--to",
            "god",
            "--body",
            "status update",
        ])
        .env("DARK_FACTORY_AGENT", "worker-1")
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
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
        let output = factoryctl_command().args(&full_args).output().unwrap();
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
