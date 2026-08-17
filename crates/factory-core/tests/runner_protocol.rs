use factory_core::{
    RunId, RunnerInstanceId,
    runner::{
        MAX_RUNNER_ERROR_BYTES, MAX_RUNNER_FRAME_BYTES, MAX_RUNNER_OUTPUT_TEXT_BYTES,
        MAX_RUNNER_SPOOL_BYTES, MAX_STARTUP_STDIN_BYTES, OutputStream, RUNNER_PROTOCOL_VERSION,
        RequestEnvelope, RunnerErrorCode, RunnerEvent, RunnerEventEnvelope, RunnerFrame,
        RunnerRequest,
    },
};

fn run_id(value: &str) -> RunId {
    RunId::try_from(value).unwrap()
}

fn instance_id(value: &str) -> RunnerInstanceId {
    RunnerInstanceId::try_from(value).unwrap()
}

#[test]
fn every_request_carries_the_run_and_random_instance_identity() {
    let requests = [
        (
            RunnerRequest::Subscribe { after_sequence: 7 },
            serde_json::json!({
                "type": "subscribe",
                "data": {"after_sequence": 7}
            }),
        ),
        (
            RunnerRequest::Stop {
                command_id: "command-stop-1".into(),
                grace_ms: 5_000,
            },
            serde_json::json!({
                "type": "stop",
                "data": {"command_id": "command-stop-1", "grace_ms": 5_000}
            }),
        ),
        (
            RunnerRequest::AcknowledgeExit {
                command_id: "command-ack-1".into(),
                terminal_sequence: 12,
            },
            serde_json::json!({
                "type": "acknowledge_exit",
                "data": {"command_id": "command-ack-1", "terminal_sequence": 12}
            }),
        ),
    ];

    for (request, expected_request) in requests {
        let envelope = RequestEnvelope::new(run_id("run-1"), instance_id("instance-7"), request);
        let value = serde_json::to_value(&envelope).unwrap();

        assert_eq!(value["protocol_version"], RUNNER_PROTOCOL_VERSION);
        assert_eq!(value["run_id"], "run-1");
        assert_eq!(value["runner_instance_id"], "instance-7");
        assert_eq!(value["request"], expected_request);
        assert_eq!(
            serde_json::from_value::<RequestEnvelope>(value).unwrap(),
            envelope
        );
    }
}

#[test]
fn hello_freezes_identity_process_and_replay_state() {
    let frame = RunnerFrame::Hello {
        protocol_version: RUNNER_PROTOCOL_VERSION,
        run_id: run_id("run-1"),
        runner_instance_id: instance_id("instance-7"),
        runner_pid: 42,
        replay_through: 9,
        terminal_sequence: Some(9),
    };

    let value = serde_json::to_value(&frame).unwrap();
    assert_eq!(
        value,
        serde_json::json!({
            "type": "hello",
            "data": {
                "protocol_version": RUNNER_PROTOCOL_VERSION,
                "run_id": "run-1",
                "runner_instance_id": "instance-7",
                "runner_pid": 42,
                "replay_through": 9,
                "terminal_sequence": 9
            }
        })
    );
    assert_eq!(frame.protocol_version(), RUNNER_PROTOCOL_VERSION);
    assert_eq!(serde_json::from_value::<RunnerFrame>(value).unwrap(), frame);
}

#[test]
fn hello_uses_null_when_the_child_has_no_terminal_event() {
    let frame = RunnerFrame::Hello {
        protocol_version: RUNNER_PROTOCOL_VERSION,
        run_id: run_id("run-1"),
        runner_instance_id: instance_id("instance-7"),
        runner_pid: 42,
        replay_through: 0,
        terminal_sequence: None,
    };

    let value = serde_json::to_value(frame).unwrap();
    assert_eq!(value["data"]["terminal_sequence"], serde_json::Value::Null);
}

#[test]
fn event_frames_freeze_started_output_and_exited_shapes() {
    let events = [
        (
            RunnerEvent::Started { child_pid: 84 },
            serde_json::json!({
                "type": "started",
                "data": {"child_pid": 84}
            }),
        ),
        (
            RunnerEvent::Output {
                stream: OutputStream::Stdout,
                text: "compiled\n".into(),
                lossy: false,
            },
            serde_json::json!({
                "type": "output",
                "data": {"stream": "stdout", "text": "compiled\n", "lossy": false}
            }),
        ),
        (
            RunnerEvent::Output {
                stream: OutputStream::Stderr,
                text: "warning\n".into(),
                lossy: true,
            },
            serde_json::json!({
                "type": "output",
                "data": {"stream": "stderr", "text": "warning\n", "lossy": true}
            }),
        ),
        (
            RunnerEvent::SpawnFailed {
                message: "child executable was not found".into(),
            },
            serde_json::json!({
                "type": "spawn_failed",
                "data": {"message": "child executable was not found"}
            }),
        ),
        (
            RunnerEvent::OutputTruncated {
                limit_bytes: 16 * 1024 * 1024,
            },
            serde_json::json!({
                "type": "output_truncated",
                "data": {"limit_bytes": 16 * 1024 * 1024}
            }),
        ),
        (
            RunnerEvent::TerminalRaw,
            serde_json::json!({
                "type": "terminal_raw"
            }),
        ),
        (
            RunnerEvent::TerminalRawTimedOut,
            serde_json::json!({
                "type": "terminal_raw_timed_out"
            }),
        ),
        (
            RunnerEvent::Exited {
                exit_code: Some(0),
                signal: None,
            },
            serde_json::json!({
                "type": "exited",
                "data": {"exit_code": 0, "signal": null}
            }),
        ),
        (
            RunnerEvent::Exited {
                exit_code: None,
                signal: Some(15),
            },
            serde_json::json!({
                "type": "exited",
                "data": {"exit_code": null, "signal": 15}
            }),
        ),
    ];

    for (sequence, (event, expected_event)) in events.into_iter().enumerate() {
        let envelope = RunnerEventEnvelope {
            protocol_version: RUNNER_PROTOCOL_VERSION,
            sequence: i64::try_from(sequence + 1).unwrap(),
            occurred_at_ms: 1_723_000_000_000,
            event,
        };
        let frame = RunnerFrame::Event {
            protocol_version: RUNNER_PROTOCOL_VERSION,
            event: envelope,
        };
        let value = serde_json::to_value(&frame).unwrap();

        assert_eq!(value["type"], "event");
        assert_eq!(value["data"]["protocol_version"], RUNNER_PROTOCOL_VERSION);
        assert_eq!(
            value["data"]["event"]["protocol_version"],
            RUNNER_PROTOCOL_VERSION
        );
        assert_eq!(value["data"]["event"]["event"], expected_event);
        assert_eq!(frame.protocol_version(), RUNNER_PROTOCOL_VERSION);
        assert_eq!(serde_json::from_value::<RunnerFrame>(value).unwrap(), frame);
    }
}

/// Adversarial re-review, round 2, finding A: proves the compatibility
/// claim `RunnerEvent::Unknown`'s own doc comment makes is actually true
/// for a dataless event (exactly `TerminalRaw`'s own shape, the only kind
/// of future variant this catch-all fully protects against -- see
/// `RunnerEvent::Unknown`'s own doc comment for the narrower, honest
/// version of this guarantee). A `type` this build's enum does not have a
/// name for -- standing in for a future variant a newer runner sends to an
/// older daemon -- must deserialize into `Unknown`, not fail the frame,
/// and a normal `Exited` event immediately after it must still parse and
/// carry its own real data: an unrecognized event must never desync or
/// poison the rest of the stream, which is the exact failure this
/// catch-all exists to close (a daemon that abandoned the connection on
/// the unrecognized frame would never reach this `Exited` event at all,
/// orphaning the runner -- `crates/factoryd/tests/runner_client.rs`'s
/// `an_unrecognized_future_event_type_deserializes_to_unknown_and_does_not_break_a_later_exit`
/// covers that consumer-side half end to end).
#[test]
fn a_dataless_unrecognized_future_event_type_deserializes_to_unknown_not_a_parse_failure() {
    let unknown_event_json = serde_json::json!({
        "type": "some_future_event_a_newer_runner_added"
    });
    let event: RunnerEvent = serde_json::from_value(unknown_event_json).unwrap();
    assert_eq!(event, RunnerEvent::Unknown);

    // The same shape, wrapped in a full envelope and frame exactly as it
    // would arrive on the wire, still parses -- proving the catch-all
    // works through the same `tag`/`content` adjacently tagged shape
    // every other variant uses, not just in isolation.
    let unknown_frame_json = serde_json::json!({
        "type": "event",
        "data": {
            "protocol_version": RUNNER_PROTOCOL_VERSION,
            "event": {
                "protocol_version": RUNNER_PROTOCOL_VERSION,
                "sequence": 7,
                "occurred_at_ms": 1_723_000_000_000_i64,
                "event": {"type": "some_future_event_a_newer_runner_added"}
            }
        }
    });
    let frame: RunnerFrame = serde_json::from_value(unknown_frame_json).unwrap();
    assert_eq!(
        frame,
        RunnerFrame::Event {
            protocol_version: RUNNER_PROTOCOL_VERSION,
            event: RunnerEventEnvelope {
                protocol_version: RUNNER_PROTOCOL_VERSION,
                sequence: 7,
                occurred_at_ms: 1_723_000_000_000,
                event: RunnerEvent::Unknown,
            },
        }
    );

    // And a real `Exited` frame immediately after an unknown one parses
    // exactly as if the unknown frame had never been there.
    let exited_frame_json = serde_json::json!({
        "type": "event",
        "data": {
            "protocol_version": RUNNER_PROTOCOL_VERSION,
            "event": {
                "protocol_version": RUNNER_PROTOCOL_VERSION,
                "sequence": 8,
                "occurred_at_ms": 1_723_000_000_001_i64,
                "event": {"type": "exited", "data": {"exit_code": 0, "signal": null}}
            }
        }
    });
    let frame: RunnerFrame = serde_json::from_value(exited_frame_json).unwrap();
    assert!(matches!(
        frame,
        RunnerFrame::Event {
            event: RunnerEventEnvelope {
                sequence: 8,
                event: RunnerEvent::Exited {
                    exit_code: Some(0),
                    signal: None,
                },
                ..
            },
            ..
        }
    ));
}

/// The honest boundary of `RunnerEvent::Unknown`'s own doc comment: a
/// future variant whose payload carries `data` is *not* caught by
/// `#[serde(other)]` on a unit variant -- serde requires that attribute on
/// a unit variant, which can only ever match an absent or `null` payload.
/// This is a regression guard on the documented limitation itself, not a
/// gap being papered over: if this test ever starts failing (a future
/// serde version relaxes the unit-variant requirement, say), the doc
/// comment's caveat needs revisiting, not just this assertion.
#[test]
fn an_unrecognized_future_event_type_carrying_data_still_fails_a_documented_limitation() {
    let unknown_event_with_data_json = serde_json::json!({
        "type": "some_future_event_with_a_payload",
        "data": {"whatever": "a newer build put here"}
    });
    assert!(
        serde_json::from_value::<RunnerEvent>(unknown_event_with_data_json).is_err(),
        "a data-bearing unknown variant is not caught by #[serde(other)] on a unit \
         variant -- see RunnerEvent::Unknown's own doc comment"
    );
}

#[test]
fn control_frames_freeze_catch_up_ack_and_error_shapes() {
    let frames = [
        (
            RunnerFrame::CaughtUp {
                protocol_version: RUNNER_PROTOCOL_VERSION,
                sequence: 11,
            },
            serde_json::json!({
                "type": "caught_up",
                "data": {"protocol_version": RUNNER_PROTOCOL_VERSION, "sequence": 11}
            }),
        ),
        (
            RunnerFrame::CommandAck {
                protocol_version: RUNNER_PROTOCOL_VERSION,
                command_id: "command-stop-1".into(),
            },
            serde_json::json!({
                "type": "command_ack",
                "data": {
                    "protocol_version": RUNNER_PROTOCOL_VERSION,
                    "command_id": "command-stop-1"
                }
            }),
        ),
        (
            RunnerFrame::Error {
                protocol_version: RUNNER_PROTOCOL_VERSION,
                code: RunnerErrorCode::Unauthorized,
                message: "runner identity does not match".into(),
            },
            serde_json::json!({
                "type": "error",
                "data": {
                    "protocol_version": RUNNER_PROTOCOL_VERSION,
                    "code": "unauthorized",
                    "message": "runner identity does not match"
                }
            }),
        ),
    ];

    for (frame, expected) in frames {
        assert_eq!(serde_json::to_value(&frame).unwrap(), expected);
        assert_eq!(frame.protocol_version(), RUNNER_PROTOCOL_VERSION);
        assert_eq!(
            serde_json::from_value::<RunnerFrame>(expected).unwrap(),
            frame
        );
    }
}

#[test]
fn every_error_code_has_a_stable_wire_name() {
    let cases = [
        (RunnerErrorCode::InvalidRequest, "invalid_request"),
        (RunnerErrorCode::UnsupportedProtocol, "unsupported_protocol"),
        (RunnerErrorCode::Unauthorized, "unauthorized"),
        (RunnerErrorCode::Conflict, "conflict"),
        (RunnerErrorCode::Internal, "internal"),
    ];

    for (code, expected) in cases {
        assert_eq!(serde_json::to_value(code).unwrap(), expected);
    }
}

#[test]
fn runner_instance_ids_use_the_shared_safe_id_contract() {
    assert_eq!(
        instance_id("random_instance-7").as_str(),
        "random_instance-7"
    );
    for invalid in [
        String::new(),
        "x".repeat(129),
        "has a space".into(),
        "path/segment".into(),
    ] {
        assert!(RunnerInstanceId::try_from(invalid.clone()).is_err());
        assert!(
            serde_json::from_value::<RunnerInstanceId>(serde_json::Value::String(invalid)).is_err()
        );
    }
}

#[test]
fn maximum_output_text_stays_within_the_bounded_frame_contract() {
    assert_eq!(MAX_RUNNER_FRAME_BYTES, 1024 * 1024);
    assert_eq!(MAX_RUNNER_OUTPUT_TEXT_BYTES, 64 * 1024);
    assert_eq!(MAX_RUNNER_ERROR_BYTES, 16 * 1024);
    assert_eq!(MAX_RUNNER_SPOOL_BYTES, 16 * 1024 * 1024);
    assert_eq!(RUNNER_PROTOCOL_VERSION, 1);

    let frame = RunnerFrame::Event {
        protocol_version: RUNNER_PROTOCOL_VERSION,
        event: RunnerEventEnvelope {
            protocol_version: RUNNER_PROTOCOL_VERSION,
            sequence: i64::MAX,
            occurred_at_ms: i64::MAX,
            event: RunnerEvent::Output {
                stream: OutputStream::Stderr,
                // NUL uses JSON's longest common escape and exercises byte expansion.
                text: "\0".repeat(MAX_RUNNER_OUTPUT_TEXT_BYTES),
                lossy: true,
            },
        },
    };

    assert!(serde_json::to_vec(&frame).unwrap().len() <= MAX_RUNNER_FRAME_BYTES);
}

#[test]
fn startup_input_has_one_shared_hard_limit() {
    assert_eq!(MAX_STARTUP_STDIN_BYTES, 1024 * 1024);
}

#[test]
fn maximum_error_text_stays_within_error_and_event_frames() {
    let message = "\0".repeat(MAX_RUNNER_ERROR_BYTES);
    let error = RunnerFrame::Error {
        protocol_version: RUNNER_PROTOCOL_VERSION,
        code: RunnerErrorCode::Internal,
        message: message.clone(),
    };
    let spawn_failed = RunnerFrame::Event {
        protocol_version: RUNNER_PROTOCOL_VERSION,
        event: RunnerEventEnvelope {
            protocol_version: RUNNER_PROTOCOL_VERSION,
            sequence: i64::MAX,
            occurred_at_ms: i64::MAX,
            event: RunnerEvent::SpawnFailed { message },
        },
    };

    assert!(serde_json::to_vec(&error).unwrap().len() <= MAX_RUNNER_FRAME_BYTES);
    assert!(serde_json::to_vec(&spawn_failed).unwrap().len() <= MAX_RUNNER_FRAME_BYTES);
}
