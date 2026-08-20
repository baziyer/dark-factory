use std::{
    io::{self, BufRead, BufReader, Write},
    os::unix::net::UnixListener,
    sync::mpsc,
    thread,
};

use factory_core::{
    EventEnvelope, FactoryEvent, PROTOCOL_VERSION, ProjectId, ProjectSnapshot,
    local::{LocalRequest, LocalResponse, RequestEnvelope, ServerFrame},
};
use factoryctl::{Client, ClientError, MAX_FRAME_BYTES};

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
        let (boundary_tx, boundary_rx) = mpsc::channel();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut line = String::new();
            BufReader::new(stream.try_clone().unwrap())
                .read_line(&mut line)
                .unwrap();
            stream.write_all(&vec![b'x'; MAX_FRAME_BYTES])?;
            stream.write_all(b"x")?;
            boundary_rx
                .recv()
                .expect("client never observed FrameTooLarge");
            match stream.write_all(b"x") {
                Ok(()) => Ok(()),
                Err(error) if error.kind() == io::ErrorKind::BrokenPipe => Ok(()),
                Err(error) => Err(error),
            }
        });

        let error = Client::new(&socket)
            .request(LocalRequest::Health)
            .unwrap_err();
        let expected = matches!(
            &error,
            ClientError::FrameTooLarge {
                max: MAX_FRAME_BYTES
            }
        );
        if expected {
            boundary_tx
                .send(())
                .expect("server stopped before post-boundary probe");
        }
        drop(boundary_tx);
        assert!(expected, "expected FrameTooLarge, got {error:?}");
        match server.join().expect("oversized frame server panicked") {
            Ok(()) => {}
            Err(error) => panic!("oversized frame server write failed unexpectedly: {error}"),
        }
    }
}
