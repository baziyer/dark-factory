use std::{num::NonZeroU32, path::PathBuf};

use factory_core::{
    RunId, RunnerInstanceId,
    runner::{MAX_RUNNER_OUTPUT_TEXT_BYTES, OutputStream, RunnerEvent},
};
use factoryd::providers::claude::{
    ClaudeLaunch, Decoder, FailureReason, MAX_CLAUDE_JSON_LINE_BYTES, MAX_CLAUDE_PREVIEW_BYTES,
    MainLoopUsage, Observation, Outcome, PrepareError, ProtocolViolation, RunUsage, Session,
    ToolKind, ToolPhase, ToolResult, prepare,
};
use serde_json::{Value, json};
use uuid::Uuid;

const SESSION_ID: &str = "0195d40a-2222-7000-8000-000000000002";
const OTHER_SESSION_ID: &str = "0195d40a-3333-7000-8000-000000000003";
const FIXTURE: &str = include_str!("fixtures/claude-2.1.233.jsonl");

fn id<T>(value: &str) -> T
where
    T: TryFrom<String>,
    T::Error: std::fmt::Debug,
{
    T::try_from(value.to_owned()).unwrap()
}

fn launch(session: Session, instructions: &str) -> ClaudeLaunch {
    ClaudeLaunch {
        runner_program: PathBuf::from("/trusted/factory-runner"),
        claude_program: PathBuf::from("claude"),
        run_id: id::<RunId>("run-claude"),
        runner_instance_id: id::<RunnerInstanceId>("runner-claude"),
        runtime_dir: PathBuf::from("/runtime/run-claude"),
        cwd: PathBuf::from("/worktree"),
        instructions: instructions.to_owned(),
        session,
        max_turns: NonZeroU32::new(20).unwrap(),
        max_budget_cents: NonZeroU32::new(500).unwrap(),
    }
}

fn stdout(text: impl Into<String>) -> RunnerEvent {
    RunnerEvent::Output {
        stream: OutputStream::Stdout,
        text: text.into(),
        lossy: false,
    }
}

fn exited(exit_code: Option<i32>, signal: Option<i32>) -> RunnerEvent {
    RunnerEvent::Exited { exit_code, signal }
}

fn minimal_success(session_id: &str) -> String {
    format!(
        "{}\n{}",
        serde_json::to_string(&init(session_id, "acceptEdits")).unwrap(),
        serde_json::to_string(&result(
            session_id,
            "success",
            false,
            "done",
            0.25,
            json!([]),
        ))
        .unwrap(),
    )
}

fn init(session_id: &str, permission_mode: &str) -> Value {
    json!({
        "type": "system",
        "subtype": "init",
        "session_id": session_id,
        "model": "claude-sonnet-4-6",
        "permissionMode": permission_mode,
        "claude_code_version": "2.1.233",
    })
}

fn result(
    session_id: &str,
    subtype: &str,
    is_error: bool,
    text: &str,
    cost_usd: f64,
    permission_denials: Value,
) -> Value {
    json!({
        "type": "result",
        "subtype": subtype,
        "is_error": is_error,
        "session_id": session_id,
        "result": text,
        "terminal_reason": "completed",
        "stop_reason": "end_turn",
        "permission_denials": permission_denials,
        "total_cost_usd": cost_usd,
        "usage": {
            "input_tokens": 1,
            "cache_creation_input_tokens": 2,
            "cache_read_input_tokens": 3,
            "output_tokens": 4,
        },
    })
}

fn stream(values: &[Value]) -> String {
    values
        .iter()
        .map(|value| serde_json::to_string(value).unwrap())
        .collect::<Vec<_>>()
        .join("\n")
}

fn decode(text: &str, terminal: RunnerEvent) -> (Vec<Observation>, Outcome) {
    let mut decoder = prepare(launch(
        Session::Resume {
            session_id: SESSION_ID.to_owned(),
        },
        "",
    ))
    .unwrap()
    .decoder;
    let mut observations = decoder.push(&stdout(text));
    observations.extend(decoder.push(&terminal));
    let finished = decoder.finish();
    observations.extend(finished.observations);
    (observations, finished.outcome)
}

#[test]
fn fresh_launch_uses_preallocated_identity_with_exact_safe_args_and_raw_stdin() {
    let task = "PRIVATE_TASK\nwith 🏭 and spaces";
    let prepared = prepare(launch(
        Session::New {
            session_id: SESSION_ID.to_owned(),
        },
        task,
    ))
    .unwrap();
    let args: Vec<_> = prepared
        .launch_spec
        .provider_arguments
        .iter()
        .map(|argument| argument.to_string_lossy())
        .collect();
    assert_eq!(args.len(), 18);
    assert_eq!(
        &args[..18],
        [
            "-p",
            "--input-format",
            "text",
            "--output-format",
            "stream-json",
            "--verbose",
            "--prompt-suggestions",
            "false",
            "--permission-mode",
            "acceptEdits",
            "--max-turns",
            "20",
            "--max-budget-usd",
            "5.00",
            "--no-chrome",
            "--safe-mode",
            "--session-id",
            SESSION_ID,
        ]
    );
    let session_id = args[17].as_ref();
    assert_eq!(Uuid::parse_str(session_id).unwrap().to_string(), session_id);
    assert_eq!(session_id, SESSION_ID);
    assert_eq!(prepared.session_id(), session_id);
    assert!(!args.iter().any(|arg| arg.contains("PRIVATE_TASK")));
    for forbidden in [
        "--replay-user-messages",
        "--bare",
        "--no-session-persistence",
    ] {
        assert!(!args.iter().any(|arg| arg.as_ref() == forbidden));
    }

    assert_eq!(prepared.launch_spec.startup_input, task.as_bytes());
}

#[test]
fn resume_is_canonical_exact_and_bound_to_its_decoder_without_echoing_invalid_ids() {
    let prepared = prepare(launch(
        Session::Resume {
            session_id: SESSION_ID.to_owned(),
        },
        "resume task",
    ))
    .unwrap();
    let args: Vec<_> = prepared
        .launch_spec
        .provider_arguments
        .iter()
        .map(|argument| argument.to_string_lossy())
        .collect();
    assert_eq!(args.len(), 18);
    assert_eq!(&args[16..], ["--resume", SESSION_ID]);
    assert_eq!(prepared.session_id(), SESSION_ID);
    assert_eq!(prepared.launch_spec.startup_input, b"resume task");

    let secret_invalid = "not-a-uuid-PRIVATE_RESUME_SECRET";
    let error = match prepare(launch(
        Session::Resume {
            session_id: secret_invalid.to_owned(),
        },
        "PRIVATE_TASK",
    )) {
        Ok(_) => panic!("invalid session was accepted"),
        Err(error) => error,
    };
    assert!(matches!(&error, PrepareError::InvalidSessionId));
    assert!(!error.to_string().contains(secret_invalid));
    assert!(!format!("{error:?}").contains(secret_invalid));
    assert!(!format!("{error:?}").contains("PRIVATE_TASK"));

    let error = match prepare(launch(
        Session::New {
            session_id: secret_invalid.to_owned(),
        },
        "PRIVATE_TASK",
    )) {
        Ok(_) => panic!("invalid fresh session was accepted"),
        Err(error) => error,
    };
    assert!(matches!(&error, PrepareError::InvalidSessionId));
    assert!(!error.to_string().contains(secret_invalid));
    assert!(!format!("{error:?}").contains(secret_invalid));
    assert!(!format!("{error:?}").contains("PRIVATE_TASK"));
}

#[test]
fn launch_independent_decoders_bind_fresh_and_resume_to_one_exact_identity() {
    for mut decoder in [
        Decoder::fresh(SESSION_ID.to_owned()).unwrap(),
        Decoder::resume(SESSION_ID.to_owned()).unwrap(),
    ] {
        let observations = decoder.push(&stdout(minimal_success(SESSION_ID)));
        let _ = decoder.push(&exited(Some(0), None));
        let finished = decoder.finish();
        assert!(matches!(
            finished.outcome,
            Outcome::Succeeded { ref session_id, .. } if session_id == SESSION_ID
        ));
        assert!(
            observations
                .iter()
                .any(|observation| matches!(observation, Observation::Initialized { .. }))
        );
        assert!(!format!("{observations:?}").contains(SESSION_ID));
    }

    for build in [
        Decoder::fresh as fn(String) -> Result<Decoder, PrepareError>,
        Decoder::resume,
    ] {
        let secret_invalid = "not-a-uuid-PRIVATE_DECODER_SECRET";
        let error = match build(secret_invalid.to_owned()) {
            Ok(_) => panic!("invalid recovery identity was accepted"),
            Err(error) => error,
        };
        assert!(matches!(&error, PrepareError::InvalidSessionId));
        assert!(!error.to_string().contains(secret_invalid));
        assert!(!format!("{error:?}").contains(secret_invalid));
    }
}

#[test]
fn fixture_normalizes_only_safe_activity_and_terminal_truth() {
    let mut prepared = prepare(launch(
        Session::Resume {
            session_id: SESSION_ID.to_owned(),
        },
        "unused",
    ))
    .unwrap();
    let mut observations = Vec::new();
    for line in FIXTURE.split_inclusive('\n') {
        observations.extend(prepared.decoder.push(&stdout(line)));
    }
    observations.extend(prepared.decoder.push(&exited(Some(0), None)));
    let finished = prepared.decoder.finish();
    observations.extend(finished.observations);

    assert_eq!(
        finished.outcome,
        Outcome::Succeeded {
            session_id: SESSION_ID.to_owned(),
            usage: RunUsage {
                main_loop: MainLoopUsage {
                    input_tokens: 101,
                    cache_creation_input_tokens: 7,
                    cache_read_input_tokens: 23,
                    output_tokens: 11,
                },
                total_cost_microusd: 12_345,
            },
        }
    );
    assert!(observations.iter().any(|event| matches!(
        event,
        Observation::Initialized { model, version, .. }
            if model.text == "claude-sonnet-4-6" && version.text == "2.1.233"
    )));
    assert!(observations.contains(&Observation::ToolChanged {
        id: "toolu_01".to_owned(),
        kind: ToolKind::Command,
        parent_tool_use_id: None,
        phase: ToolPhase::Started,
        result: None,
    }));
    assert!(observations.contains(&Observation::ToolChanged {
        id: "toolu_01".to_owned(),
        kind: ToolKind::Command,
        parent_tool_use_id: None,
        phase: ToolPhase::Completed,
        result: Some(ToolResult::Succeeded),
    }));
    assert!(observations.contains(&Observation::ToolChanged {
        id: "toolu_agent".to_owned(),
        kind: ToolKind::Subagent,
        parent_tool_use_id: None,
        phase: ToolPhase::Started,
        result: None,
    }));
    assert!(observations.contains(&Observation::ToolChanged {
        id: "toolu_nested".to_owned(),
        kind: ToolKind::Command,
        parent_tool_use_id: Some("toolu_agent".to_owned()),
        phase: ToolPhase::Completed,
        result: Some(ToolResult::Failed),
    }));
    assert!(observations.iter().any(|event| matches!(
        event,
        Observation::FinalMessage { preview }
            if preview.text == "Implemented safely 🏭" && !preview.truncated
    )));

    let normalized = format!("{observations:?}{:?}", finished.outcome);
    for secret in [
        "CWD_SECRET",
        "MCP_SERVER_SECRET",
        "SLASH_SECRET",
        "KEY_SOURCE_SECRET",
        "AGENT_SECRET",
        "SKILL_SECRET",
        "PLUGIN_SECRET",
        "PLUGIN_PATH_SECRET",
        "HOOK_SECRET",
        "HOOK_PATH_SECRET",
        "THINKING_SECRET",
        "SIGNATURE_SECRET",
        "INTERMEDIATE_SECRET",
        "COMMAND_PATH_SECRET",
        "TOOL_INPUT_SECRET",
        "TOOL_RESULT_SECRET",
        "RESULT_PATH_SECRET",
        "RAW_RESULT_SECRET",
        "PERMISSION_SECRET",
        "ASSISTANT_FINAL_SECRET",
        "AGENT_PROMPT_SECRET",
        "NESTED_COMMAND_SECRET",
        "NESTED_RESULT_SECRET",
        "AGENT_RESULT_SECRET",
        "DENIAL_COMMAND_SECRET",
    ] {
        assert!(
            !normalized.contains(secret),
            "normalized output leaked {secret}"
        );
    }
}

#[test]
fn terminal_reconciliation_is_fail_closed() {
    let cases = [
        (Some(0), None, None),
        (Some(7), None, Some(FailureReason::Process)),
        (None, Some(9), Some(FailureReason::Process)),
    ];
    for (code, signal, expected_failure) in cases {
        let mut decoder = prepare(launch(
            Session::Resume {
                session_id: SESSION_ID.to_owned(),
            },
            "",
        ))
        .unwrap()
        .decoder;
        let _ = decoder.push(&stdout(minimal_success(SESSION_ID)));
        let _ = decoder.push(&exited(code, signal));
        let outcome = decoder.finish().outcome;
        assert_eq!(outcome.failure_reason(), expected_failure);
    }

    let error_stream = stream(&[
        init(SESSION_ID, "acceptEdits"),
        result(
            SESSION_ID,
            "error_max_budget_usd",
            true,
            "PROVIDER_ERROR_SECRET",
            6.0,
            json!([]),
        ),
    ]);
    let mut decoder = prepare(launch(
        Session::Resume {
            session_id: SESSION_ID.to_owned(),
        },
        "",
    ))
    .unwrap()
    .decoder;
    let _ = decoder.push(&stdout(&error_stream));
    assert_eq!(
        decoder.finish().outcome.failure_reason(),
        Some(FailureReason::Incomplete),
        "provider output cannot terminate a live runner"
    );

    let mut decoder = prepare(launch(
        Session::Resume {
            session_id: SESSION_ID.to_owned(),
        },
        "",
    ))
    .unwrap()
    .decoder;
    let observations = decoder.push(&stdout(&error_stream));
    let _ = decoder.push(&exited(Some(1), None));
    let finished = decoder.finish();
    assert_eq!(
        finished.outcome.failure_reason(),
        Some(FailureReason::Limit)
    );
    assert!(matches!(
        finished.outcome,
        Outcome::Failed {
            usage: Some(RunUsage {
                total_cost_microusd: 6_000_000,
                ..
            }),
            ..
        }
    ));
    assert!(!format!("{observations:?}").contains("PROVIDER_ERROR_SECRET"));

    let mut decoder = prepare(launch(
        Session::New {
            session_id: SESSION_ID.to_owned(),
        },
        "",
    ))
    .unwrap()
    .decoder;
    let _ = decoder.push(&RunnerEvent::SpawnFailed {
        message: "SPAWN_SECRET".to_owned(),
    });
    let outcome = decoder.finish().outcome;
    assert_eq!(outcome.failure_reason(), Some(FailureReason::Spawn));
    assert!(!format!("{outcome:?}").contains("SPAWN_SECRET"));

    let mut decoder = prepare(launch(
        Session::New {
            session_id: SESSION_ID.to_owned(),
        },
        "",
    ))
    .unwrap()
    .decoder;
    let _ = decoder.push(&stdout("{not-json}\n"));
    assert_eq!(
        decoder.finish().outcome.failure_reason(),
        Some(FailureReason::Incomplete),
        "even corrupt provider output cannot terminate a live runner"
    );
}

#[test]
fn identity_integrity_and_final_unterminated_line_are_enforced() {
    let mut decoder = Decoder::resume(SESSION_ID.to_owned()).unwrap();
    let wrong = minimal_success(OTHER_SESSION_ID);
    let observations = decoder.push(&stdout(&wrong));
    let _ = decoder.push(&exited(Some(0), None));
    let finished = decoder.finish();
    assert_eq!(
        finished.outcome.failure_reason(),
        Some(FailureReason::Protocol)
    );
    assert!(!format!("{observations:?}{:?}", finished.outcome).contains(OTHER_SESSION_ID));

    let mut decoder = Decoder::resume(SESSION_ID.to_owned()).unwrap();
    let _ = decoder.push(&stdout(minimal_success(SESSION_ID)));
    let _ = decoder.push(&exited(Some(0), None));
    assert!(matches!(
        decoder.finish().outcome,
        Outcome::Succeeded { .. }
    ));
}

#[test]
fn malformed_lossy_truncated_and_stderr_never_leak_or_succeed() {
    for corrupt in [
        stdout("{not-json}\n"),
        RunnerEvent::Output {
            stream: OutputStream::Stdout,
            text: "LOSSY_SECRET\n".to_owned(),
            lossy: true,
        },
        RunnerEvent::OutputTruncated { limit_bytes: 99 },
    ] {
        let mut decoder = prepare(launch(
            Session::Resume {
                session_id: SESSION_ID.to_owned(),
            },
            "",
        ))
        .unwrap()
        .decoder;
        let _ = decoder.push(&corrupt);
        let _ = decoder.push(&stdout(minimal_success(SESSION_ID)));
        let _ = decoder.push(&exited(Some(0), None));
        assert_eq!(
            decoder.finish().outcome.failure_reason(),
            Some(FailureReason::Protocol)
        );
    }

    let mut decoder = prepare(launch(
        Session::New {
            session_id: SESSION_ID.to_owned(),
        },
        "",
    ))
    .unwrap()
    .decoder;
    let diagnostic = decoder.push(&RunnerEvent::Output {
        stream: OutputStream::Stderr,
        text: "STDERR_PATH_SECRET /private/file TOKEN_SECRET".to_owned(),
        lossy: false,
    });
    assert!(!format!("{diagnostic:?}").contains("STDERR_PATH_SECRET"));
    assert!(matches!(
        diagnostic.as_slice(),
        [Observation::Diagnostic { .. }]
    ));

    let mut decoder = prepare(launch(
        Session::Resume {
            session_id: SESSION_ID.to_owned(),
        },
        "",
    ))
    .unwrap()
    .decoder;
    let diagnostic = decoder.push(&RunnerEvent::Output {
        stream: OutputStream::Stderr,
        text: "LOSSY_STDERR_SECRET".to_owned(),
        lossy: true,
    });
    let _ = decoder.push(&stdout(minimal_success(SESSION_ID)));
    let _ = decoder.push(&exited(Some(0), None));
    assert!(matches!(
        decoder.finish().outcome,
        Outcome::Succeeded { .. }
    ));
    assert!(matches!(
        diagnostic.as_slice(),
        [Observation::Diagnostic { lossy: true, .. }]
    ));
    assert!(!format!("{diagnostic:?}").contains("LOSSY_STDERR_SECRET"));
}

#[test]
fn previews_are_utf8_bounded_and_unknown_additive_events_are_ignored() {
    let long = "🏭".repeat(MAX_CLAUDE_PREVIEW_BYTES);
    let stream = stream(&[
        json!({"type": "future.additive", "secret": "UNKNOWN_SECRET"}),
        init(SESSION_ID, "acceptEdits"),
        json!({
            "type": "assistant",
            "session_id": SESSION_ID,
            "message": {"content": [{"type": "text", "text": "INTERMEDIATE_SECRET"}]},
        }),
        result(SESSION_ID, "success", false, &long, 0.0, json!([])),
    ]);
    let mut decoder = prepare(launch(
        Session::Resume {
            session_id: SESSION_ID.to_owned(),
        },
        "",
    ))
    .unwrap()
    .decoder;
    let mut observations = decoder.push(&stdout(&stream));
    let _ = decoder.push(&exited(Some(0), None));
    let finished = decoder.finish();
    let final_preview = finished.final_preview.as_ref().unwrap();
    assert!(final_preview.truncated);
    assert!(final_preview.text.len() <= MAX_CLAUDE_PREVIEW_BYTES);
    observations.extend(finished.observations);
    assert!(matches!(finished.outcome, Outcome::Succeeded { .. }));
    let preview = observations.iter().find_map(|event| match event {
        Observation::FinalMessage { preview } => Some(preview),
        _ => None,
    });
    let preview = preview.unwrap();
    assert!(preview.truncated);
    assert!(preview.text.len() <= MAX_CLAUDE_PREVIEW_BYTES);
    assert!(!format!("{observations:?}").contains("UNKNOWN_SECRET"));
    assert!(!format!("{observations:?}").contains("INTERMEDIATE_SECRET"));
}

#[test]
fn success_rejects_adverse_terminal_fields_and_requires_the_fixed_init_permission() {
    for (field, value) in [
        ("terminal_reason", "tool_deferred"),
        ("terminal_reason", "background_requested"),
        ("terminal_reason", "api_error"),
        ("terminal_reason", "model_error"),
        ("stop_reason", "refusal"),
        ("stop_reason", "max_tokens"),
    ] {
        let mut terminal = result(
            SESSION_ID,
            "success",
            false,
            "TERMINAL_RESULT_SECRET",
            0.25,
            json!([]),
        );
        terminal[field] = Value::String(value.to_owned());
        let text = stream(&[init(SESSION_ID, "acceptEdits"), terminal]);
        let (observations, outcome) = decode(&text, exited(Some(0), None));
        assert_eq!(
            outcome.failure_reason(),
            Some(FailureReason::Provider),
            "accepted {field}={value}"
        );
        assert!(!format!("{observations:?}{outcome:?}").contains("TERMINAL_RESULT_SECRET"));
    }

    let mut optional_terminal = result(
        SESSION_ID,
        "success",
        false,
        "optional terminal fields",
        0.25,
        json!([]),
    );
    optional_terminal
        .as_object_mut()
        .unwrap()
        .remove("terminal_reason");
    optional_terminal["stop_reason"] = Value::Null;
    let text = stream(&[init(SESSION_ID, "acceptEdits"), optional_terminal]);
    assert!(matches!(
        decode(&text, exited(Some(0), None)).1,
        Outcome::Succeeded { .. }
    ));

    let mut missing_stop_reason = result(
        SESSION_ID,
        "success",
        false,
        "missing stop reason",
        0.25,
        json!([]),
    );
    missing_stop_reason
        .as_object_mut()
        .unwrap()
        .remove("stop_reason");
    let text = stream(&[init(SESSION_ID, "acceptEdits"), missing_stop_reason]);
    assert_eq!(
        decode(&text, exited(Some(0), None)).1.failure_reason(),
        Some(FailureReason::Provider)
    );

    let text = stream(&[
        init(SESSION_ID, "bypassPermissions"),
        result(SESSION_ID, "success", false, "done", 0.25, json!([])),
    ]);
    assert_eq!(
        decode(&text, exited(Some(0), None)).1.failure_reason(),
        Some(FailureReason::Protocol)
    );
}

#[test]
fn error_results_are_fixed_metadata_and_keep_their_spend() {
    let cases = [
        ("success", true, json!([]), FailureReason::Provider),
        (
            "error_during_execution",
            true,
            json!([]),
            FailureReason::Provider,
        ),
        ("error_max_turns", true, json!([]), FailureReason::Limit),
        (
            "error_max_budget_usd",
            true,
            json!([]),
            FailureReason::Limit,
        ),
        (
            "success",
            false,
            json!([{"tool_name": "Bash", "tool_input": {"command": "DENIED_SECRET"}}]),
            FailureReason::Permission,
        ),
    ];
    for (subtype, is_error, denials, expected) in cases {
        let text = stream(&[
            init(SESSION_ID, "acceptEdits"),
            result(
                SESSION_ID,
                subtype,
                is_error,
                "RAW_PROVIDER_FAILURE_SECRET",
                7.25,
                denials,
            ),
        ]);
        let (observations, outcome) = decode(&text, exited(Some(1), None));
        assert_eq!(outcome.failure_reason(), Some(expected));
        assert!(matches!(
            outcome,
            Outcome::Failed {
                usage: Some(RunUsage {
                    total_cost_microusd: 7_250_000,
                    ..
                }),
                ..
            }
        ));
        let normalized = format!("{observations:?}{outcome:?}");
        assert!(!normalized.contains("RAW_PROVIDER_FAILURE_SECRET"));
        assert!(!normalized.contains("DENIED_SECRET"));
    }

    let text = format!(
        "{}\n{}\n{}",
        serde_json::to_string(&json!({
            "type": "error",
            "message": "UNDOCUMENTED_ERROR_SECRET",
        }))
        .unwrap(),
        serde_json::to_string(&init(SESSION_ID, "acceptEdits")).unwrap(),
        serde_json::to_string(&result(
            SESSION_ID,
            "success",
            false,
            "done",
            6.0,
            json!([]),
        ))
        .unwrap(),
    );
    let (observations, outcome) = decode(&text, exited(Some(0), None));
    assert!(matches!(outcome, Outcome::Succeeded { .. }));
    assert!(!format!("{observations:?}{outcome:?}").contains("UNDOCUMENTED_ERROR_SECRET"));
}

#[test]
fn assistant_top_level_error_wins_and_unknown_content_is_additive() {
    let text = stream(&[
        init(SESSION_ID, "acceptEdits"),
        json!({
            "type": "assistant",
            "session_id": SESSION_ID,
            "error": "AUTH_FAILURE_SECRET",
            "message": {
                "content": [{"type": "text", "text": "AUTH_ASSISTANT_SECRET"}],
            },
        }),
        result(
            SESSION_ID,
            "success",
            false,
            "nominal result",
            0.25,
            json!([]),
        ),
    ]);
    let (observations, outcome) = decode(&text, exited(Some(0), None));
    assert_eq!(outcome.failure_reason(), Some(FailureReason::Provider));
    let normalized = format!("{observations:?}{outcome:?}");
    assert!(!normalized.contains("AUTH_FAILURE_SECRET"));
    assert!(!normalized.contains("AUTH_ASSISTANT_SECRET"));

    let text = stream(&[
        init(SESSION_ID, "acceptEdits"),
        json!({
            "type": "assistant",
            "session_id": SESSION_ID,
            "message": {
                "content": [{"type": "error", "message": "UNKNOWN_BLOCK_SECRET"}],
            },
        }),
        result(
            SESSION_ID,
            "success",
            false,
            "nominal result",
            0.25,
            json!([]),
        ),
    ]);
    let (observations, outcome) = decode(&text, exited(Some(0), None));
    assert!(matches!(outcome, Outcome::Succeeded { .. }));
    assert!(!format!("{observations:?}{outcome:?}").contains("UNKNOWN_BLOCK_SECRET"));
}

#[test]
fn exact_replay_is_idempotent_but_conflicting_replay_fails_integrity() {
    let replay = format!("{FIXTURE}{FIXTURE}");
    let mut decoder = prepare(launch(
        Session::Resume {
            session_id: SESSION_ID.to_owned(),
        },
        "",
    ))
    .unwrap()
    .decoder;
    let observations = decoder.push(&stdout(&replay));
    let _ = decoder.push(&exited(Some(0), None));
    let _ = decoder.push(&exited(Some(0), None));
    assert!(matches!(
        decoder.finish().outcome,
        Outcome::Succeeded { .. }
    ));
    assert_eq!(
        observations
            .iter()
            .filter(|event| matches!(event, Observation::Initialized { .. }))
            .count(),
        1
    );
    assert_eq!(
        observations
            .iter()
            .filter(|event| matches!(event, Observation::FinalMessage { .. }))
            .count(),
        1
    );

    let conflicts = [
        stream(&[
            init(SESSION_ID, "acceptEdits"),
            json!({
                "type": "system",
                "subtype": "init",
                "session_id": SESSION_ID,
                "model": "different-model",
                "permissionMode": "acceptEdits",
                "claude_code_version": "2.1.233",
            }),
            result(SESSION_ID, "success", false, "done", 0.25, json!([])),
        ]),
        stream(&[
            init(SESSION_ID, "acceptEdits"),
            json!({
                "type": "assistant",
                "session_id": SESSION_ID,
                "parent_tool_use_id": null,
                "message": {"content": [{"type": "tool_use", "id": "same", "name": "Bash"}]},
            }),
            json!({
                "type": "assistant",
                "session_id": SESSION_ID,
                "parent_tool_use_id": null,
                "message": {"content": [{"type": "tool_use", "id": "same", "name": "Read"}]},
            }),
            result(SESSION_ID, "success", false, "done", 0.25, json!([])),
        ]),
        stream(&[
            init(SESSION_ID, "acceptEdits"),
            json!({
                "type": "user",
                "session_id": SESSION_ID,
                "parent_tool_use_id": null,
                "message": {"content": [{"type": "tool_result", "tool_use_id": "same", "is_error": false}]},
            }),
            json!({
                "type": "user",
                "session_id": SESSION_ID,
                "parent_tool_use_id": null,
                "message": {"content": [{"type": "tool_result", "tool_use_id": "same", "is_error": true}]},
            }),
            result(SESSION_ID, "success", false, "done", 0.25, json!([])),
        ]),
        stream(&[
            init(SESSION_ID, "acceptEdits"),
            result(SESSION_ID, "success", false, "first", 0.25, json!([])),
            result(SESSION_ID, "success", false, "second", 0.25, json!([])),
        ]),
    ];
    for conflict in conflicts {
        let (observations, outcome) = decode(&conflict, exited(Some(0), None));
        assert_eq!(outcome.failure_reason(), Some(FailureReason::Protocol));
        assert!(
            observations.contains(&Observation::ProtocolViolation {
                kind: ProtocolViolation::ConflictingTerminal,
            }) || observations.contains(&Observation::ProtocolViolation {
                kind: ProtocolViolation::ConflictingInit,
            })
        );
    }
}

#[test]
fn decoder_is_incremental_recovers_after_oversize_and_caps_completed_only_tools() {
    let valid = minimal_success(SESSION_ID);
    let mut decoder = prepare(launch(
        Session::Resume {
            session_id: SESSION_ID.to_owned(),
        },
        "",
    ))
    .unwrap()
    .decoder;
    for character in valid.chars() {
        let _ = decoder.push(&stdout(character.to_string()));
    }
    let _ = decoder.push(&exited(Some(0), None));
    assert!(matches!(
        decoder.finish().outcome,
        Outcome::Succeeded { .. }
    ));

    let oversized = "OVERSIZED_SECRET".repeat(MAX_CLAUDE_JSON_LINE_BYTES / 16 + 1);
    let mut decoder = prepare(launch(
        Session::Resume {
            session_id: SESSION_ID.to_owned(),
        },
        "",
    ))
    .unwrap()
    .decoder;
    let mut observations = Vec::new();
    for chunk in oversized.as_bytes().chunks(MAX_RUNNER_OUTPUT_TEXT_BYTES) {
        observations.extend(decoder.push(&stdout(std::str::from_utf8(chunk).unwrap())));
    }
    observations.extend(decoder.push(&stdout(format!("\n{valid}"))));
    observations.extend(decoder.push(&exited(Some(0), None)));
    let finished = decoder.finish();
    observations.extend(finished.observations);
    assert_eq!(
        finished.outcome.failure_reason(),
        Some(FailureReason::Protocol)
    );
    assert!(observations.contains(&Observation::ProtocolViolation {
        kind: ProtocolViolation::LineTooLong,
    }));
    assert!(!format!("{observations:?}").contains("OVERSIZED_SECRET"));

    let mut decoder = prepare(launch(
        Session::Resume {
            session_id: SESSION_ID.to_owned(),
        },
        "",
    ))
    .unwrap()
    .decoder;
    let _ = decoder.push(&stdout(format!(
        "{}\n",
        serde_json::to_string(&init(SESSION_ID, "acceptEdits")).unwrap()
    )));
    let mut observations = Vec::new();
    for index in 0..=4096 {
        let line = serde_json::to_string(&json!({
            "type": "user",
            "session_id": SESSION_ID,
            "parent_tool_use_id": null,
            "message": {"content": [{
                "type": "tool_result",
                "tool_use_id": format!("completed_{index}"),
                "is_error": false,
            }]},
        }))
        .unwrap();
        observations.extend(decoder.push(&stdout(format!("{line}\n"))));
    }
    let _ = decoder.push(&stdout(
        serde_json::to_string(&result(
            SESSION_ID,
            "success",
            false,
            "done",
            0.25,
            json!([]),
        ))
        .unwrap(),
    ));
    let _ = decoder.push(&exited(Some(0), None));
    assert_eq!(
        decoder.finish().outcome.failure_reason(),
        Some(FailureReason::Protocol)
    );
    assert!(observations.contains(&Observation::ProtocolViolation {
        kind: ProtocolViolation::TooManyTools,
    }));
}
