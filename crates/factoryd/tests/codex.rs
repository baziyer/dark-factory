use std::{ffi::OsString, path::PathBuf};

use factory_core::{
    RunId, RunnerInstanceId,
    runner::{MAX_RUNNER_OUTPUT_TEXT_BYTES, OutputStream, RunnerEvent},
};
use factoryd::providers::codex::{
    CodexLaunch, Decoder, FailureReason, ItemKind, ItemPhase, ItemResult,
    MAX_CODEX_JSON_LINE_BYTES, MAX_CODEX_PREVIEW_BYTES, Observation, Outcome, ProtocolViolation,
    Session, TokenUsage, prepare,
};

const THREAD_ID: &str = "0195d40a-1111-7000-8000-000000000001";
const OTHER_THREAD_ID: &str = "0195d40a-3333-7000-8000-000000000003";
const FIXTURE: &str = include_str!("fixtures/codex-0.147.jsonl");

fn id<T>(value: &str) -> T
where
    T: TryFrom<String>,
    T::Error: std::fmt::Debug,
{
    T::try_from(value.to_owned()).unwrap()
}

fn launch(session: Session, instructions: &str) -> CodexLaunch {
    CodexLaunch {
        runner_program: PathBuf::from("/trusted/factory-runner"),
        codex_program: PathBuf::from("codex"),
        run_id: id::<RunId>("run-codex-adapter"),
        runner_instance_id: id::<RunnerInstanceId>("runner-codex-adapter"),
        runtime_dir: PathBuf::from("/private/runtime"),
        cwd: PathBuf::from("/workspace/project"),
        codex_home: Some(PathBuf::from("/private/codex-home")),
        instructions: instructions.to_owned(),
        session,
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

fn complete_line(thread_id: &str) -> String {
    format!(
        "{{\"type\":\"thread.started\",\"thread_id\":\"{thread_id}\"}}\n\
         {{\"type\":\"turn.completed\",\"usage\":{{\"input_tokens\":1,\"cached_input_tokens\":2,\"cache_write_input_tokens\":3,\"output_tokens\":4,\"reasoning_output_tokens\":5}}}}\n"
    )
}

fn decode(text: &str, terminal: RunnerEvent) -> (Vec<Observation>, Outcome) {
    let mut decoder = prepare(launch(Session::New, "")).unwrap().decoder;
    let mut observations = decoder.push(&stdout(text));
    observations.extend(decoder.push(&terminal));
    let finished = decoder.finish();
    observations.extend(finished.observations);
    (observations, finished.outcome)
}

#[test]
fn recovery_decoders_rebuild_fresh_or_exact_resumed_thread_identity() {
    let mut fresh = Decoder::fresh();
    let observations = fresh.push(&stdout(complete_line(THREAD_ID)));
    assert!(observations.contains(&Observation::ThreadStarted {
        thread_id: THREAD_ID.to_owned(),
    }));
    let _ = fresh.push(&exited(Some(0), None));
    assert_eq!(fresh.finish().outcome.thread_id(), Some(THREAD_ID));

    let mut resumed = Decoder::resume(THREAD_ID.to_owned()).unwrap();
    let observations = resumed.push(&stdout(complete_line(THREAD_ID)));
    assert!(observations.contains(&Observation::ThreadStarted {
        thread_id: THREAD_ID.to_owned(),
    }));
    let _ = resumed.push(&exited(Some(0), None));
    assert_eq!(resumed.finish().outcome.thread_id(), Some(THREAD_ID));

    let mut mismatched = Decoder::resume(THREAD_ID.to_owned()).unwrap();
    let observations = mismatched.push(&stdout(complete_line(OTHER_THREAD_ID)));
    assert!(
        observations
            .iter()
            .all(|observation| !matches!(observation, Observation::ThreadStarted { .. }))
    );
    let _ = mismatched.push(&exited(Some(0), None));
    let finished = mismatched.finish();
    assert_eq!(finished.outcome.thread_id(), None);
    assert_eq!(
        finished.outcome.failure_reason(),
        Some(FailureReason::Protocol)
    );

    for invalid in ["", "--last", "not-a-uuid", "0195d40a\nsecret"] {
        let error = match Decoder::resume(invalid.to_owned()) {
            Ok(_) => panic!("invalid recovery thread ID was accepted"),
            Err(error) => error,
        };
        if !invalid.is_empty() {
            assert!(!error.to_string().contains(invalid));
        }
    }
}

#[test]
fn launch_arguments_are_fixed_and_instructions_exist_only_on_stdin() {
    let instructions = "private task 🏭\nwith spaces";
    let fresh = prepare(launch(Session::New, instructions))
        .unwrap()
        .launch_spec;
    assert_eq!(
        fresh.provider_arguments,
        [
            "exec",
            "--json",
            "--color",
            "never",
            "--sandbox",
            "workspace-write",
            "-c",
            "approval_policy=\"never\"",
            "--ignore-user-config",
            "-",
        ]
        .map(OsString::from)
    );
    assert_eq!(fresh.startup_input, instructions.as_bytes());
    assert!(
        fresh
            .provider_arguments
            .iter()
            .all(|arg| arg != instructions)
    );
    assert!(matches!(
        &fresh.provider_environment,
        factoryd::runner_process::ProviderEnvironment::CodexHome(path)
            if path == &PathBuf::from("/private/codex-home")
    ));

    let resumed = prepare(launch(
        Session::Resume {
            thread_id: THREAD_ID.to_owned(),
        },
        instructions,
    ))
    .unwrap()
    .launch_spec;
    assert_eq!(
        resumed.provider_arguments,
        [
            "exec",
            "--json",
            "--color",
            "never",
            "--sandbox",
            "workspace-write",
            "-c",
            "approval_policy=\"never\"",
            "--ignore-user-config",
            "resume",
            THREAD_ID,
            "-",
        ]
        .map(OsString::from)
    );
    assert_eq!(resumed.startup_input, instructions.as_bytes());
    assert!(matches!(
        &resumed.provider_environment,
        factoryd::runner_process::ProviderEnvironment::CodexHome(path)
            if path == &PathBuf::from("/private/codex-home")
    ));

    let mut inherited = launch(Session::New, instructions);
    inherited.codex_home = None;
    let inherited = prepare(inherited).unwrap().launch_spec;
    assert!(matches!(
        inherited.provider_environment,
        factoryd::runner_process::ProviderEnvironment::Inherited
    ));

    for invalid in ["", "--last", "not-a-uuid", "0195d40a\nsecret"] {
        let error = match prepare(launch(
            Session::Resume {
                thread_id: invalid.to_owned(),
            },
            "DO_NOT_ECHO_THIS_TASK",
        )) {
            Ok(_) => panic!("invalid resume ID was accepted"),
            Err(error) => error,
        };
        let message = error.to_string();
        assert!(message.contains("thread ID"));
        if !invalid.is_empty() {
            assert!(!message.contains(invalid));
        }
        assert!(!message.contains("DO_NOT_ECHO_THIS_TASK"));
    }
}

#[test]
fn sanitized_fixture_normalizes_activity_without_private_tool_payloads() {
    let (observations, outcome) = decode(FIXTURE, exited(Some(0), None));

    assert_eq!(
        outcome,
        Outcome::Succeeded {
            thread_id: THREAD_ID.to_owned(),
            usage: TokenUsage {
                input_tokens: 101,
                cached_input_tokens: 23,
                cache_write_input_tokens: 7,
                output_tokens: 11,
                reasoning_output_tokens: 5,
            },
        }
    );
    assert!(observations.contains(&Observation::ThreadStarted {
        thread_id: THREAD_ID.to_owned()
    }));
    assert!(observations.contains(&Observation::TurnStarted));
    for kind in [
        ItemKind::Reasoning,
        ItemKind::Command,
        ItemKind::FileChange,
        ItemKind::McpTool,
        ItemKind::WebSearch,
        ItemKind::TodoList,
        ItemKind::Collaboration,
        ItemKind::Error,
        ItemKind::AgentMessage,
        ItemKind::Other,
    ] {
        assert!(observations.iter().any(|observation| matches!(
            observation,
            Observation::ItemChanged { kind: actual, .. } if *actual == kind
        )));
    }
    assert!(observations.contains(&Observation::ItemChanged {
        id: "item_message".to_owned(),
        kind: ItemKind::AgentMessage,
        phase: ItemPhase::Completed,
        result: Some(ItemResult::Unknown),
        preview: Some(factoryd::providers::codex::BoundedText {
            text: "Adapter ready 🏭".to_owned(),
            truncated: false,
        }),
    }));
    assert!(observations.contains(&Observation::ItemChanged {
        id: "item_error".to_owned(),
        kind: ItemKind::Error,
        phase: ItemPhase::Completed,
        result: Some(ItemResult::Failed),
        preview: Some(factoryd::providers::codex::BoundedText {
            text: "Codex item failed".to_owned(),
            truncated: false,
        }),
    }));
    assert!(observations.contains(&Observation::ItemChanged {
        id: "item_command".to_owned(),
        kind: ItemKind::Command,
        phase: ItemPhase::Completed,
        result: Some(ItemResult::Succeeded),
        preview: None,
    }));

    let normalized = format!("{observations:?}{outcome:?}");
    for secret in [
        "PRIVATE_REASONING",
        "COMMAND_SECRET",
        "OUTPUT_SECRET",
        "FILE_SECRET",
        "MCP_SECRET",
        "MCP_RESULT_SECRET",
        "SEARCH_SECRET",
        "TODO_SECRET",
        "COLLAB_SECRET",
        "UNKNOWN_SECRET",
        "UNKNOWN_ITEM_SECRET",
        "ITEM_ERROR_SECRET",
        "private-server",
        "private-tool",
    ] {
        assert!(
            !normalized.contains(secret),
            "normalized output leaked {secret}"
        );
    }
}

#[test]
fn decoder_reassembles_all_boundaries_and_accepts_a_valid_final_line() {
    let ascii = complete_line(THREAD_ID);
    for split in 0..=ascii.len() {
        let mut decoder = prepare(launch(Session::New, "")).unwrap().decoder;
        let mut observations = decoder.push(&stdout(&ascii[..split]));
        observations.extend(decoder.push(&stdout(&ascii[split..])));
        observations.extend(decoder.push(&exited(Some(0), None)));
        let finished = decoder.finish();
        assert!(matches!(finished.outcome, Outcome::Succeeded { .. }));
    }

    let unicode = format!(
        "\n \t\n\r\n{{\"type\":\"thread.started\",\"thread_id\":\"{THREAD_ID}\"}}\r\n\
         {{\"type\":\"item.completed\",\"item\":{{\"id\":\"item_unicode\",\"type\":\"agent_message\",\"text\":\"hello 🏭\"}}}}\n\
         {{\"type\":\"turn.completed\",\"usage\":{{\"input_tokens\":1,\"cached_input_tokens\":2,\"cache_write_input_tokens\":3,\"output_tokens\":4,\"reasoning_output_tokens\":5}}}}"
    );
    let boundaries: Vec<_> = unicode
        .char_indices()
        .map(|(index, _)| index)
        .chain(std::iter::once(unicode.len()))
        .collect();
    for split in boundaries {
        let mut decoder = prepare(launch(Session::New, "")).unwrap().decoder;
        let mut observations = decoder.push(&stdout(&unicode[..split]));
        observations.extend(decoder.push(&stdout(&unicode[split..])));
        observations.extend(decoder.push(&exited(Some(0), None)));
        let finished = decoder.finish();
        assert_eq!(
            finished
                .final_preview
                .as_ref()
                .map(|preview| preview.text.as_str()),
            Some("hello 🏭")
        );
        observations.extend(finished.observations);
        assert!(matches!(finished.outcome, Outcome::Succeeded { .. }));
        assert!(observations.iter().any(|observation| matches!(
            observation,
            Observation::ItemChanged { preview: Some(preview), .. }
                if preview.text == "hello 🏭"
        )));
    }
}

#[test]
fn malformed_oversized_lossy_and_truncated_output_recover_but_fail_integrity() {
    let valid = complete_line(THREAD_ID);
    let oversized_secret = "OVERSIZED_SECRET".repeat(MAX_CODEX_JSON_LINE_BYTES / 16 + 1);
    let mut decoder = prepare(launch(Session::New, "")).unwrap().decoder;
    let mut observations = decoder.push(&stdout("{not-json}\n"));
    for chunk in oversized_secret
        .as_bytes()
        .chunks(MAX_RUNNER_OUTPUT_TEXT_BYTES)
    {
        observations.extend(decoder.push(&stdout(std::str::from_utf8(chunk).unwrap())));
    }
    observations.extend(decoder.push(&stdout(format!("\n{valid}"))));
    observations.extend(decoder.push(&exited(Some(0), None)));
    let finished = decoder.finish();
    observations.extend(finished.observations);
    let outcome = finished.outcome;
    assert_eq!(outcome.failure_reason(), Some(FailureReason::Protocol));
    assert!(observations.contains(&Observation::ProtocolViolation {
        kind: ProtocolViolation::MalformedJson,
    }));
    assert!(observations.contains(&Observation::ProtocolViolation {
        kind: ProtocolViolation::LineTooLong,
    }));
    assert!(observations.contains(&Observation::ThreadStarted {
        thread_id: THREAD_ID.to_owned(),
    }));
    assert!(
        observations
            .iter()
            .any(|observation| matches!(observation, Observation::TurnCompleted { .. }))
    );
    assert!(!format!("{observations:?}").contains("OVERSIZED_SECRET"));

    let (observations, outcome) = decode(
        &format!("{valid}{}", "{\"type\":\"PRIVATE_MALFORMED_TAIL\""),
        exited(Some(0), None),
    );
    assert_eq!(outcome.failure_reason(), Some(FailureReason::Protocol));
    assert!(!format!("{observations:?}").contains("PRIVATE_MALFORMED_TAIL"));

    let mut lossy = prepare(launch(Session::New, "")).unwrap().decoder;
    let mut observations = lossy.push(&stdout("{\"type\":\"thread"));
    observations.extend(lossy.push(&RunnerEvent::Output {
        stream: OutputStream::Stdout,
        text: "BROKEN_SECRET\n".to_owned(),
        lossy: true,
    }));
    observations.extend(lossy.push(&stdout(&valid)));
    observations.extend(lossy.push(&RunnerEvent::OutputTruncated {
        limit_bytes: 16 * 1024 * 1024,
    }));
    observations.extend(lossy.push(&exited(Some(0), None)));
    let finished = lossy.finish();
    observations.extend(finished.observations);
    assert_eq!(
        finished.outcome.failure_reason(),
        Some(FailureReason::Protocol)
    );
    assert!(observations.contains(&Observation::ProtocolViolation {
        kind: ProtocolViolation::LossyStdout,
    }));
    assert!(observations.contains(&Observation::ProtocolViolation {
        kind: ProtocolViolation::OutputTruncated,
    }));
    assert!(observations.contains(&Observation::ThreadStarted {
        thread_id: THREAD_ID.to_owned(),
    }));
    assert!(
        observations
            .iter()
            .any(|observation| matches!(observation, Observation::TurnCompleted { .. }))
    );
    assert!(!format!("{observations:?}").contains("BROKEN_SECRET"));
}

#[test]
fn duplicate_item_replay_is_idempotent_and_stderr_is_diagnostic_only() {
    let mut decoder = prepare(launch(Session::New, "")).unwrap().decoder;
    let mut observations = decoder.push(&stdout(FIXTURE));
    observations.extend(decoder.push(&stdout(FIXTURE)));
    observations.extend(decoder.push(&RunnerEvent::Output {
        stream: OutputStream::Stderr,
        text: "STDERR_SECRET".to_owned(),
        lossy: false,
    }));
    observations.extend(decoder.push(&exited(Some(0), None)));
    let finished = decoder.finish();
    observations.extend(finished.observations);

    assert!(matches!(finished.outcome, Outcome::Succeeded { .. }));
    for phase in [ItemPhase::Started, ItemPhase::Updated, ItemPhase::Completed] {
        assert_eq!(
            observations
                .iter()
                .filter(|observation| matches!(
                    observation,
                    Observation::ItemChanged { id, phase: actual, .. }
                        if id == "item_command" && *actual == phase
                ))
                .count(),
            1
        );
    }
    assert!(observations.contains(&Observation::Diagnostic {
        bytes: "STDERR_SECRET".len(),
        lossy: false,
    }));
    assert!(!format!("{observations:?}").contains("STDERR_SECRET"));
}

#[test]
fn terminal_reconciliation_requires_provider_and_runner_success() {
    let completed = complete_line(THREAD_ID);
    assert!(matches!(
        decode(&completed, exited(Some(0), None)).1,
        Outcome::Succeeded { .. }
    ));
    assert_eq!(
        decode(&completed, exited(Some(7), None)).1.failure_reason(),
        Some(FailureReason::Process)
    );
    assert_eq!(
        decode(&completed, exited(None, Some(15)))
            .1
            .failure_reason(),
        Some(FailureReason::Process)
    );

    let failed = format!(
        "{{\"type\":\"thread.started\",\"thread_id\":\"{THREAD_ID}\"}}\n\
         {{\"type\":\"error\",\"message\":\"TOP_LEVEL_ERROR_SECRET\"}}\n\
         {{\"type\":\"turn.failed\",\"error\":{{\"message\":\"TURN_FAILURE_SECRET\"}}}}\n"
    );
    let (observations, outcome) = decode(&failed, exited(Some(0), None));
    assert_eq!(outcome.failure_reason(), Some(FailureReason::Provider));
    let normalized = format!("{observations:?}{outcome:?}");
    assert!(!normalized.contains("TOP_LEVEL_ERROR_SECRET"));
    assert!(!normalized.contains("TURN_FAILURE_SECRET"));

    let conflicted = format!(
        "{completed}{{\"type\":\"turn.failed\",\"error\":{{\"message\":\"LATE_SECRET\"}}}}\n"
    );
    let (observations, outcome) = decode(&conflicted, exited(Some(0), None));
    assert_eq!(outcome.failure_reason(), Some(FailureReason::Protocol));
    assert!(observations.contains(&Observation::ProtocolViolation {
        kind: ProtocolViolation::ConflictingTerminal,
    }));
    assert!(!format!("{observations:?}").contains("LATE_SECRET"));
    let only_message = format!(
        "{{\"type\":\"thread.started\",\"thread_id\":\"{THREAD_ID}\"}}\n\
         {{\"type\":\"item.completed\",\"item\":{{\"id\":\"message\",\"type\":\"agent_message\",\"text\":\"not terminal\"}}}}\n"
    );
    assert_eq!(
        decode(&only_message, exited(Some(0), None))
            .1
            .failure_reason(),
        Some(FailureReason::Incomplete)
    );
    let (_, spawn_failed) = decode(
        "",
        RunnerEvent::SpawnFailed {
            message: "PRIVATE_SPAWN_ERROR".to_owned(),
        },
    );
    assert_eq!(spawn_failed.failure_reason(), Some(FailureReason::Spawn));
    assert!(!format!("{spawn_failed:?}").contains("PRIVATE_SPAWN_ERROR"));
}

#[test]
fn thread_identity_is_idempotent_but_never_silently_replaced() {
    let duplicate = format!(
        "{{\"type\":\"thread.started\",\"thread_id\":\"{THREAD_ID}\"}}\n\
         {{\"type\":\"thread.started\",\"thread_id\":\"{THREAD_ID}\"}}\n\
         {{\"type\":\"thread.started\",\"thread_id\":\"{OTHER_THREAD_ID}\"}}\n\
         {{\"type\":\"turn.completed\",\"usage\":{{\"input_tokens\":1,\"cached_input_tokens\":2,\"cache_write_input_tokens\":3,\"output_tokens\":4,\"reasoning_output_tokens\":5}}}}\n"
    );
    let mut decoder = prepare(launch(
        Session::Resume {
            thread_id: THREAD_ID.to_owned(),
        },
        "",
    ))
    .unwrap()
    .decoder;
    let mut observations = decoder.push(&stdout(duplicate));
    observations.extend(decoder.push(&exited(Some(0), None)));
    let finished = decoder.finish();
    observations.extend(finished.observations);
    assert_eq!(
        finished.outcome.failure_reason(),
        Some(FailureReason::Protocol)
    );
    assert_eq!(finished.outcome.thread_id(), Some(THREAD_ID));
    assert_eq!(
        observations
            .iter()
            .filter(|item| matches!(item, Observation::ThreadStarted { .. }))
            .count(),
        1
    );
    assert!(observations.contains(&Observation::ProtocolViolation {
        kind: ProtocolViolation::ConflictingThreadId,
    }));

    let mut mismatch = prepare(launch(
        Session::Resume {
            thread_id: OTHER_THREAD_ID.to_owned(),
        },
        "",
    ))
    .unwrap()
    .decoder;
    let mut observations = mismatch.push(&stdout(complete_line(THREAD_ID)));
    observations.extend(mismatch.push(&exited(Some(0), None)));
    let finished = mismatch.finish();
    assert_eq!(
        finished.outcome.failure_reason(),
        Some(FailureReason::Protocol)
    );
    assert_eq!(finished.outcome.thread_id(), None);
}

#[test]
fn invalid_usage_and_sensitive_text_are_bounded_without_breaking_utf8() {
    let negative = format!(
        "{{\"type\":\"thread.started\",\"thread_id\":\"{THREAD_ID}\"}}\n\
         {{\"type\":\"turn.completed\",\"usage\":{{\"input_tokens\":-1,\"cached_input_tokens\":2,\"cache_write_input_tokens\":3,\"output_tokens\":4,\"reasoning_output_tokens\":5}}}}\n"
    );
    let (observations, outcome) = decode(&negative, exited(Some(0), None));
    assert_eq!(outcome.failure_reason(), Some(FailureReason::Protocol));
    assert!(observations.contains(&Observation::ProtocolViolation {
        kind: ProtocolViolation::InvalidUsage,
    }));

    let text = format!("visible\u{0}{}tail", "🏭".repeat(MAX_CODEX_PREVIEW_BYTES));
    let stream = format!(
        "{{\"type\":\"thread.started\",\"thread_id\":\"{THREAD_ID}\"}}\n\
         {{\"type\":\"item.completed\",\"item\":{{\"id\":\"long_message\",\"type\":\"agent_message\",\"text\":{}}}}}\n{}",
        serde_json::to_string(&text).unwrap(),
        complete_line(THREAD_ID)
            .lines()
            .skip(1)
            .collect::<Vec<_>>()
            .join("\n")
    );
    let (observations, outcome) = decode(&stream, exited(Some(0), None));
    assert!(matches!(outcome, Outcome::Succeeded { .. }));
    let preview = observations
        .iter()
        .find_map(|item| match item {
            Observation::ItemChanged {
                preview: Some(preview),
                ..
            } => Some(preview),
            _ => None,
        })
        .unwrap();
    assert!(preview.truncated);
    assert!(preview.text.len() <= MAX_CODEX_PREVIEW_BYTES);
    assert!(!preview.text.contains('\0'));
    assert!(std::str::from_utf8(preview.text.as_bytes()).is_ok());
}
