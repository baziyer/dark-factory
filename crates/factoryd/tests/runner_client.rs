use std::path::{Path, PathBuf};

use factory_core::{
    RunId, RunnerInstanceId,
    runner::{
        MAX_RUNNER_FRAME_BYTES, RUNNER_PROTOCOL_VERSION, RequestEnvelope, RunnerErrorCode,
        RunnerEvent, RunnerEventEnvelope, RunnerFrame, RunnerRequest,
    },
};
use factoryd::runner_client::{
    RunnerClient, RunnerClientError, RunnerReplayHead, RunnerStreamItem,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::{UnixListener, UnixStream},
    sync::oneshot,
    task::JoinHandle,
    time::{Duration, timeout},
};

const RUN: &str = "run-1";
const INSTANCE: &str = "instance-1";

fn run_id() -> RunId {
    RunId::try_from(RUN).unwrap()
}

fn instance_id() -> RunnerInstanceId {
    RunnerInstanceId::try_from(INSTANCE).unwrap()
}

fn client(runtime_dir: &Path) -> RunnerClient {
    RunnerClient::new(runtime_dir, run_id(), instance_id())
}

fn hello(replay_through: i64, terminal_sequence: Option<i64>) -> RunnerFrame {
    RunnerFrame::Hello {
        protocol_version: RUNNER_PROTOCOL_VERSION,
        run_id: run_id(),
        runner_instance_id: instance_id(),
        runner_pid: 42_424,
        replay_through,
        terminal_sequence,
    }
}

fn event(sequence: i64, event: RunnerEvent) -> RunnerFrame {
    RunnerFrame::Event {
        protocol_version: RUNNER_PROTOCOL_VERSION,
        event: RunnerEventEnvelope {
            protocol_version: RUNNER_PROTOCOL_VERSION,
            sequence,
            occurred_at_ms: 1_000 + sequence,
            event,
        },
    }
}

fn caught_up(sequence: i64) -> RunnerFrame {
    RunnerFrame::CaughtUp {
        protocol_version: RUNNER_PROTOCOL_VERSION,
        sequence,
    }
}

fn encoded(frame: RunnerFrame) -> Vec<u8> {
    let mut bytes = serde_json::to_vec(&frame).unwrap();
    bytes.push(b'\n');
    bytes
}

struct FakeRunner {
    _directory: tempfile::TempDir,
    runtime_dir: PathBuf,
    requests: JoinHandle<Vec<RequestEnvelope>>,
}

async fn fake_runner(connections: Vec<Vec<Vec<u8>>>) -> FakeRunner {
    let directory = tempfile::tempdir_in("/tmp").unwrap();
    let runtime_dir = directory.path().join("runtime");
    std::fs::create_dir(&runtime_dir).unwrap();
    let listener = UnixListener::bind(runtime_dir.join("control.sock")).unwrap();
    let requests = tokio::spawn(async move {
        let mut requests = Vec::new();
        for replies in connections {
            let (stream, _) = listener.accept().await.unwrap();
            let mut reader = BufReader::new(stream);
            let mut line = Vec::new();
            reader.read_until(b'\n', &mut line).await.unwrap();
            requests.push(serde_json::from_slice(&line).unwrap());
            let mut stream = reader.into_inner();
            for reply in replies {
                if stream.write_all(&reply).await.is_err() {
                    break;
                }
            }
        }
        requests
    });
    FakeRunner {
        _directory: directory,
        runtime_dir,
        requests,
    }
}

async fn assert_subscribe_request(fake: FakeRunner) {
    assert_eq!(
        fake.requests.await.unwrap(),
        vec![RequestEnvelope::new(
            run_id(),
            instance_id(),
            RunnerRequest::Subscribe { after_sequence: 0 },
        )]
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn prepare_holds_one_exact_process_identity_until_activation() {
    let directory = tempfile::tempdir_in("/tmp").unwrap();
    let runtime_dir = directory.path().join("runtime");
    std::fs::create_dir(&runtime_dir).unwrap();
    let listener = UnixListener::bind(runtime_dir.join("control.sock")).unwrap();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut reader = BufReader::new(stream);
        let mut line = Vec::new();
        reader.read_until(b'\n', &mut line).await.unwrap();
        let prepared: RequestEnvelope = serde_json::from_slice(&line).unwrap();
        let RunnerRequest::Prepare { command_id } = &prepared.request else {
            panic!("first request did not prepare the exec gate");
        };
        let command_id = command_id.clone();
        let response = encoded(RunnerFrame::Prepared {
            protocol_version: RUNNER_PROTOCOL_VERSION,
            command_id: command_id.clone(),
            runner_pid: 1233,
            child_pid: 1234,
            process_group_id: 1234,
        });
        reader.get_mut().write_all(&response).await.unwrap();

        line.clear();
        reader.read_until(b'\n', &mut line).await.unwrap();
        let activated: RequestEnvelope = serde_json::from_slice(&line).unwrap();
        assert_eq!(activated.run_id, prepared.run_id);
        assert_eq!(activated.runner_instance_id, prepared.runner_instance_id);
        assert_eq!(
            activated.request,
            RunnerRequest::Activate {
                command_id: command_id.clone(),
            }
        );
        let response = encoded(RunnerFrame::CommandAck {
            protocol_version: RUNNER_PROTOCOL_VERSION,
            command_id,
        });
        reader.get_mut().write_all(&response).await.unwrap();
    });

    let prepared = client(&runtime_dir).prepare().await.unwrap();
    assert_eq!(prepared.runner_pid(), 1233);
    assert_eq!(prepared.child_pid(), 1234);
    assert_eq!(prepared.process_group_id(), 1234);
    prepared.activate().await.unwrap();
    server.await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn replay_boundary_and_live_terminal_are_delivered_in_exact_order() {
    let fake = fake_runner(vec![vec![
        encoded(hello(2, None)),
        encoded(event(1, RunnerEvent::Started { child_pid: 98 })),
        encoded(event(2, RunnerEvent::Started { child_pid: 99 })),
        encoded(caught_up(2)),
        encoded(event(
            3,
            RunnerEvent::Exited {
                exit_code: Some(0),
                signal: None,
            },
        )),
    ]])
    .await;

    let mut subscription = client(&fake.runtime_dir).subscribe().await.unwrap();
    assert_eq!(
        subscription.head(),
        RunnerReplayHead {
            replay_through: 2,
            terminal_sequence: None,
        }
    );
    assert!(matches!(
        subscription.next_item().await.unwrap(),
        RunnerStreamItem::Event(RunnerEventEnvelope { sequence: 1, .. })
    ));
    assert!(matches!(
        subscription.next_item().await.unwrap(),
        RunnerStreamItem::Event(RunnerEventEnvelope { sequence: 2, .. })
    ));
    assert_eq!(
        subscription.next_item().await.unwrap(),
        RunnerStreamItem::CaughtUp { sequence: 2 }
    );
    assert!(matches!(
        subscription.next_item().await.unwrap(),
        RunnerStreamItem::Event(RunnerEventEnvelope {
            sequence: 3,
            event: RunnerEvent::Exited { .. },
            ..
        })
    ));
    drop(subscription);
    assert_subscribe_request(fake).await;
}

async fn subscribe_error(replies: Vec<Vec<u8>>) -> RunnerClientError {
    let fake = fake_runner(vec![replies]).await;
    let error = client(&fake.runtime_dir).subscribe().await.unwrap_err();
    assert_subscribe_request(fake).await;
    error
}

async fn next_error(replies: Vec<Vec<u8>>, successful_items: usize) -> RunnerClientError {
    let fake = fake_runner(vec![replies]).await;
    let mut subscription = client(&fake.runtime_dir).subscribe().await.unwrap();
    for _ in 0..successful_items {
        subscription.next_item().await.unwrap();
    }
    let error = subscription.next_item().await.unwrap_err();
    drop(subscription);
    assert_subscribe_request(fake).await;
    error
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hello_requires_exact_identity_protocol_and_valid_terminal_head() {
    let mut wrong_protocol = hello(0, None);
    if let RunnerFrame::Hello {
        protocol_version, ..
    } = &mut wrong_protocol
    {
        *protocol_version += 1;
    }
    assert!(matches!(
        subscribe_error(vec![encoded(wrong_protocol)]).await,
        RunnerClientError::WrongProtocol { .. }
    ));

    let mut wrong_run = hello(0, None);
    if let RunnerFrame::Hello { run_id, .. } = &mut wrong_run {
        *run_id = RunId::try_from("other-run").unwrap();
    }
    assert!(matches!(
        subscribe_error(vec![encoded(wrong_run)]).await,
        RunnerClientError::WrongIdentity
    ));

    assert!(matches!(
        subscribe_error(vec![encoded(hello(-1, None))]).await,
        RunnerClientError::InvalidReplayHead { .. }
    ));
    for invalid in [Some(0), Some(1), Some(3)] {
        assert!(matches!(
            subscribe_error(vec![encoded(hello(2, invalid))]).await,
            RunnerClientError::InvalidTerminalHead { .. }
        ));
    }

    assert!(matches!(
        subscribe_error(vec![encoded(caught_up(0))]).await,
        RunnerClientError::UnexpectedFrame { .. }
    ));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn frame_reader_is_bounded_newline_delimited_and_rejects_untrusted_errors() {
    assert!(matches!(
        subscribe_error(Vec::new()).await,
        RunnerClientError::UnexpectedEof
    ));
    assert!(matches!(
        subscribe_error(vec![b"{}".to_vec()]).await,
        RunnerClientError::UnterminatedFrame
    ));
    assert!(matches!(
        subscribe_error(vec![b"not-json\n".to_vec()]).await,
        RunnerClientError::InvalidJson
    ));

    let mut oversized = vec![b'x'; MAX_RUNNER_FRAME_BYTES + 1];
    oversized.push(b'\n');
    assert!(matches!(
        subscribe_error(vec![oversized]).await,
        RunnerClientError::FrameTooLarge
    ));

    let secret = "DO-NOT-RETAIN-provider-output-or-secrets";
    let malformed = format!("{{\"type\":\"{secret}\"}}\n").into_bytes();
    let error = subscribe_error(vec![malformed]).await;
    assert!(matches!(error, RunnerClientError::InvalidJson));
    assert!(!format!("{error}").contains(secret));
    assert!(!format!("{error:?}").contains(secret));

    let error = subscribe_error(vec![encoded(RunnerFrame::Error {
        protocol_version: RUNNER_PROTOCOL_VERSION,
        code: RunnerErrorCode::Unauthorized,
        message: secret.into(),
    })])
    .await;
    assert!(matches!(
        error,
        RunnerClientError::RunnerRejected {
            code: RunnerErrorCode::Unauthorized
        }
    ));
    assert!(!format!("{error}").contains(secret));
    assert!(!format!("{error:?}").contains(secret));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn replay_requires_nested_protocol_positive_contiguity_and_exact_boundary() {
    let mut wrong_outer = event(1, RunnerEvent::Started { child_pid: 1 });
    if let RunnerFrame::Event {
        protocol_version, ..
    } = &mut wrong_outer
    {
        *protocol_version += 1;
    }
    assert!(matches!(
        next_error(vec![encoded(hello(1, None)), encoded(wrong_outer)], 0).await,
        RunnerClientError::WrongProtocol { .. }
    ));

    let mut wrong_nested = event(1, RunnerEvent::Started { child_pid: 1 });
    if let RunnerFrame::Event { event, .. } = &mut wrong_nested {
        event.protocol_version += 1;
    }
    assert!(matches!(
        next_error(vec![encoded(hello(1, None)), encoded(wrong_nested)], 0).await,
        RunnerClientError::WrongProtocol { .. }
    ));

    assert!(matches!(
        next_error(
            vec![
                encoded(hello(2, None)),
                encoded(event(2, RunnerEvent::Started { child_pid: 1 })),
            ],
            0,
        )
        .await,
        RunnerClientError::SequenceMismatch { .. }
    ));
    assert!(matches!(
        next_error(
            vec![
                encoded(hello(2, None)),
                encoded(event(1, RunnerEvent::Started { child_pid: 1 })),
                encoded(event(1, RunnerEvent::Started { child_pid: 1 })),
            ],
            1,
        )
        .await,
        RunnerClientError::SequenceMismatch { .. }
    ));
    assert!(matches!(
        next_error(
            vec![
                encoded(hello(0, None)),
                encoded(event(1, RunnerEvent::Started { child_pid: 1 })),
            ],
            0,
        )
        .await,
        RunnerClientError::ReplayBoundary { .. }
    ));
    assert!(matches!(
        next_error(vec![encoded(hello(1, None)), encoded(caught_up(1))], 0).await,
        RunnerClientError::CaughtUpMismatch { .. }
    ));
    assert!(matches!(
        next_error(
            vec![
                encoded(hello(1, None)),
                encoded(event(1, RunnerEvent::Started { child_pid: 1 })),
                encoded(caught_up(0)),
            ],
            1,
        )
        .await,
        RunnerClientError::CaughtUpMismatch { .. }
    ));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn terminal_metadata_must_match_the_observed_terminal_and_end_the_stream() {
    assert!(matches!(
        next_error(
            vec![
                encoded(hello(1, Some(1))),
                encoded(event(1, RunnerEvent::Started { child_pid: 1 })),
            ],
            0,
        )
        .await,
        RunnerClientError::TerminalMismatch
    ));
    assert!(matches!(
        next_error(
            vec![
                encoded(hello(1, None)),
                encoded(event(
                    1,
                    RunnerEvent::Exited {
                        exit_code: Some(0),
                        signal: None,
                    },
                )),
            ],
            0,
        )
        .await,
        RunnerClientError::TerminalMismatch
    ));

    let terminal = event(
        1,
        RunnerEvent::Exited {
            exit_code: Some(0),
            signal: None,
        },
    );
    assert!(matches!(
        next_error(
            vec![
                encoded(hello(1, Some(1))),
                encoded(terminal),
                encoded(caught_up(1)),
                encoded(event(2, RunnerEvent::Started { child_pid: 99 })),
            ],
            2,
        )
        .await,
        RunnerClientError::EventAfterTerminal
    ));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn live_stream_rejects_duplicates_gaps_and_all_non_event_frames() {
    for sequence in [0, 2] {
        assert!(matches!(
            next_error(
                vec![
                    encoded(hello(0, None)),
                    encoded(caught_up(0)),
                    encoded(event(sequence, RunnerEvent::Started { child_pid: 1 })),
                ],
                1,
            )
            .await,
            RunnerClientError::SequenceMismatch { .. }
        ));
    }

    let unexpected = [
        hello(0, None),
        caught_up(0),
        RunnerFrame::CommandAck {
            protocol_version: RUNNER_PROTOCOL_VERSION,
            command_id: "not-for-subscribe".into(),
        },
    ];
    for frame in unexpected {
        assert!(matches!(
            next_error(
                vec![
                    encoded(hello(0, None)),
                    encoded(caught_up(0)),
                    encoded(frame),
                ],
                1,
            )
            .await,
            RunnerClientError::UnexpectedFrame { .. }
        ));
    }
    assert!(matches!(
        next_error(vec![encoded(hello(0, None)), encoded(caught_up(0))], 1).await,
        RunnerClientError::UnexpectedEof
    ));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn acknowledge_exit_uses_a_fresh_connection_and_an_exact_derived_command_id() {
    let fake = fake_runner(vec![vec![encoded(RunnerFrame::CommandAck {
        protocol_version: RUNNER_PROTOCOL_VERSION,
        command_id: "ack-7".into(),
    })]])
    .await;
    client(&fake.runtime_dir).acknowledge_exit(7).await.unwrap();
    assert_eq!(
        fake.requests.await.unwrap(),
        vec![RequestEnvelope::new(
            run_id(),
            instance_id(),
            RunnerRequest::AcknowledgeExit {
                command_id: "ack-7".into(),
                terminal_sequence: 7,
            },
        )]
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn acknowledge_exit_requires_positive_sequence_and_exact_ack() {
    let directory = tempfile::tempdir_in("/tmp").unwrap();
    assert!(matches!(
        client(directory.path())
            .acknowledge_exit(0)
            .await
            .unwrap_err(),
        RunnerClientError::InvalidTerminalSequence { .. }
    ));

    let cases = [
        RunnerFrame::CommandAck {
            protocol_version: RUNNER_PROTOCOL_VERSION + 1,
            command_id: "ack-7".into(),
        },
        RunnerFrame::CommandAck {
            protocol_version: RUNNER_PROTOCOL_VERSION,
            command_id: "wrong".into(),
        },
        hello(0, None),
    ];
    for (index, frame) in cases.into_iter().enumerate() {
        let fake = fake_runner(vec![vec![encoded(frame)]]).await;
        let error = client(&fake.runtime_dir)
            .acknowledge_exit(7)
            .await
            .unwrap_err();
        match index {
            0 => assert!(matches!(error, RunnerClientError::WrongProtocol { .. })),
            1 => assert!(matches!(error, RunnerClientError::CommandMismatch)),
            _ => assert!(matches!(error, RunnerClientError::UnexpectedFrame { .. })),
        }
        fake.requests.await.unwrap();
    }

    let fake = fake_runner(vec![vec![encoded(RunnerFrame::Error {
        protocol_version: RUNNER_PROTOCOL_VERSION,
        code: RunnerErrorCode::Conflict,
        message: "untrusted detail".into(),
    })]])
    .await;
    assert!(matches!(
        client(&fake.runtime_dir)
            .acknowledge_exit(7)
            .await
            .unwrap_err(),
        RunnerClientError::RunnerRejected {
            code: RunnerErrorCode::Conflict
        }
    ));
    fake.requests.await.unwrap();

    let fake = fake_runner(vec![Vec::new()]).await;
    assert!(matches!(
        client(&fake.runtime_dir)
            .acknowledge_exit(7)
            .await
            .unwrap_err(),
        RunnerClientError::UnexpectedEof
    ));
    fake.requests.await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancelling_next_item_preserves_an_already_consumed_partial_frame() {
    let directory = tempfile::tempdir_in("/tmp").unwrap();
    let runtime_dir = directory.path().join("runtime");
    std::fs::create_dir(&runtime_dir).unwrap();
    let listener = UnixListener::bind(runtime_dir.join("control.sock")).unwrap();
    let (partial_tx, partial_rx) = oneshot::channel();
    let (finish_tx, finish_rx) = oneshot::channel();
    let complete = encoded(event(1, RunnerEvent::Started { child_pid: 9 }));
    let split = complete.len() / 2;
    let first = complete[..split].to_vec();
    let second = complete[split..].to_vec();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut reader = BufReader::new(stream);
        let mut request = Vec::new();
        reader.read_until(b'\n', &mut request).await.unwrap();
        let mut stream: UnixStream = reader.into_inner();
        stream.write_all(&encoded(hello(1, None))).await.unwrap();
        stream.write_all(&first).await.unwrap();
        partial_tx.send(()).unwrap();
        finish_rx.await.unwrap();
        stream.write_all(&second).await.unwrap();
        stream.write_all(&encoded(caught_up(1))).await.unwrap();
    });

    let mut subscription = client(&runtime_dir).subscribe().await.unwrap();
    partial_rx.await.unwrap();
    assert!(
        timeout(Duration::from_millis(50), subscription.next_item())
            .await
            .is_err(),
        "partial frame unexpectedly completed"
    );
    finish_tx.send(()).unwrap();
    assert!(matches!(
        subscription.next_item().await.unwrap(),
        RunnerStreamItem::Event(RunnerEventEnvelope { sequence: 1, .. })
    ));
    assert_eq!(
        subscription.next_item().await.unwrap(),
        RunnerStreamItem::CaughtUp { sequence: 1 }
    );
    drop(subscription);
    server.await.unwrap();
}
