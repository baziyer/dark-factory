use std::{
    io::{BufRead, BufReader, Write},
    os::unix::net::UnixListener,
    process::Command,
    thread,
};

use factory_core::{
    EventEnvelope, FactoryEvent, PROTOCOL_VERSION, ProjectId, ProjectSnapshot,
    local::{LocalRequest, LocalResponse, RequestEnvelope, ServerFrame},
};

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
