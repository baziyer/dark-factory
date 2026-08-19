use std::{
    io::{BufRead, BufReader, ErrorKind, Write},
    os::unix::net::UnixListener,
    thread,
};

use factory_core::{
    EventEnvelope, FactoryEvent, PROTOCOL_VERSION, ProjectId, ProjectSnapshot,
    local::{LocalRequest, LocalResponse, RequestEnvelope, ServerFrame},
};
use factoryctl::{Client, ClientError, MAX_FRAME_BYTES};

fn write_oversized_frame(stream: &mut impl Write) {
    let chunk = [b'x'; 16 * 1024];
    let mut written = 0;
    while written < MAX_FRAME_BYTES {
        let chunk_len = (MAX_FRAME_BYTES - written).min(chunk.len());
        let count = stream
            .write(&chunk[..chunk_len])
            .expect("server write failed before crossing the frame-size boundary");
        assert!(
            count > 0,
            "server write made no progress before the boundary"
        );
        written += count;
    }
    assert_eq!(written, MAX_FRAME_BYTES);

    match stream.write_all(b"x\n") {
        Ok(()) => {}
        Err(error) if error.kind() == ErrorKind::BrokenPipe => {}
        Err(error) => panic!("unexpected server teardown error after the boundary: {error}"),
    }
}

#[test]
fn request_writes_one_json_line_and_reads_one_versioned_frame() {
    let directory = tempfile::tempdir().unwrap();
    let socket = directory.path().join("factory.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut line = String::new();
        BufReader::new(stream.try_clone().unwrap())
            .read_line(&mut line)
            .unwrap();
        assert!(line.ends_with('\n'));
        assert_eq!(
            serde_json::from_str::<RequestEnvelope>(&line).unwrap(),
            RequestEnvelope::new(LocalRequest::Health)
        );

        let frame = ServerFrame::Response {
            protocol_version: PROTOCOL_VERSION,
            response: LocalResponse::Health {
                runner_path: "/opt/factory-runner".to_owned(),
                factoryctl_path: "/opt/factoryctl".to_owned(),
                version: "0.1.0".to_owned(),
                process_id: 0,
            },
        };
        serde_json::to_writer(&mut stream, &frame).unwrap();
        stream.write_all(b"\n").unwrap();
    });

    let frame = Client::new(&socket).request(LocalRequest::Health).unwrap();
    assert_eq!(
        frame,
        ServerFrame::Response {
            protocol_version: PROTOCOL_VERSION,
            response: LocalResponse::Health {
                runner_path: "/opt/factory-runner".to_owned(),
                factoryctl_path: "/opt/factoryctl".to_owned(),
                version: "0.1.0".to_owned(),
                process_id: 0,
            },
        }
    );
    server.join().unwrap();
}

#[test]
fn subscribe_exposes_each_frame_without_polling() {
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

    let mut subscription = Client::new(&socket).subscribe(7).unwrap();
    assert!(matches!(
        subscription.next().unwrap().unwrap(),
        ServerFrame::Response {
            response: LocalResponse::Subscribed {
                after_sequence: 7,
                replay_through: 7,
            },
            ..
        }
    ));
    assert!(matches!(
        subscription.next().unwrap().unwrap(),
        ServerFrame::Event {
            event: EventEnvelope { sequence: 8, .. },
            ..
        }
    ));
    assert!(matches!(
        subscription.next().unwrap().unwrap_err(),
        ClientError::Disconnected { after_sequence: 8 }
    ));
    assert!(subscription.next().is_none());
    server.join().unwrap();
}

#[test]
fn rejects_an_oversized_server_frame_before_parsing_json() {
    for _ in 0..8 {
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("factory.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut line = String::new();
            BufReader::new(stream.try_clone().unwrap())
                .read_line(&mut line)
                .unwrap();
            write_oversized_frame(&mut stream);
        });

        let error = Client::new(&socket)
            .request(LocalRequest::Health)
            .unwrap_err();
        assert!(matches!(error, ClientError::FrameTooLarge { .. }));
        server.join().unwrap();
    }
}
