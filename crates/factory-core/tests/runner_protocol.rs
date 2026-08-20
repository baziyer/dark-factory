use factory_core::{
    RunId, RunnerInstanceId,
    runner::{
        MAX_RUNNER_ERROR_BYTES, MAX_RUNNER_FRAME_BYTES, MAX_STARTUP_STDIN_BYTES,
        RUNNER_PROTOCOL_VERSION, RequestEnvelope, RunnerErrorCode, RunnerEvent,
        RunnerEventEnvelope, RunnerFrame, RunnerRequest,
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
fn event_frames_freeze_lifecycle_shapes() {
    let events = [
        (
            RunnerEvent::Started { child_pid: 84 },
            serde_json::json!({
                "type": "started",
                "data": {"child_pid": 84}
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
