use factory_core::{
    AgentId, EventEnvelope, FactoryEvent, ObserverHealth, PROTOCOL_VERSION, ProjectId, Provider,
    RunFailureReason, RunId, RunOutcome, RunPhase, RunSnapshot, TaskId, TaskStatus,
};

fn id<T>(value: &str) -> T
where
    T: for<'a> TryFrom<&'a str>,
    for<'a> <T as TryFrom<&'a str>>::Error: std::fmt::Debug,
{
    T::try_from(value).unwrap()
}

#[test]
fn run_phases_have_stable_wire_names_and_only_running_grants_authority() {
    assert_eq!(PROTOCOL_VERSION, 6);
    let cases = [
        (RunPhase::Admitted, "admitted"),
        (RunPhase::Running, "running"),
        (RunPhase::Finalizing, "finalizing"),
        (RunPhase::Terminal, "terminal"),
    ];
    for (phase, expected) in cases {
        assert_eq!(serde_json::to_value(phase).unwrap(), expected);
        assert_eq!(phase.grants_attempt_authority(), phase == RunPhase::Running);
    }
}

#[test]
fn legacy_repository_audit_events_remain_decodable() {
    let value = serde_json::json!({
        "type": "repository_operation",
        "data": {
            "project_id": "project-1",
            "agent_id": "worker-1",
            "run_id": "run-1",
            "operation": "git_push",
            "phase": "finished",
            "success": true,
            "reference": "deadbeef"
        }
    });
    assert!(matches!(
        serde_json::from_value::<FactoryEvent>(value).unwrap(),
        FactoryEvent::LegacyRepositoryOperation { .. }
    ));
}

#[test]
fn run_phase_transition_contract_requires_finalization_before_terminal() {
    assert!(RunPhase::Admitted.can_transition_to(RunPhase::Running));
    assert!(RunPhase::Admitted.can_transition_to(RunPhase::Finalizing));
    assert!(RunPhase::Running.can_transition_to(RunPhase::Finalizing));
    assert!(RunPhase::Finalizing.can_transition_to(RunPhase::Terminal));

    for forbidden in [
        (RunPhase::Admitted, RunPhase::Terminal),
        (RunPhase::Running, RunPhase::Terminal),
        (RunPhase::Finalizing, RunPhase::Running),
        (RunPhase::Terminal, RunPhase::Running),
    ] {
        assert!(!forbidden.0.can_transition_to(forbidden.1));
    }
}

#[test]
fn immutable_run_outcomes_have_stable_bounded_shapes() {
    let cases = [
        (
            RunOutcome::Succeeded,
            serde_json::json!({"type": "succeeded"}),
        ),
        (
            RunOutcome::Blocked {
                reason: "wait".into(),
            },
            serde_json::json!({"type": "blocked", "data": {"reason": "wait"}}),
        ),
        (
            RunOutcome::Failed {
                reason: RunFailureReason::Process,
            },
            serde_json::json!({"type": "failed", "data": {"reason": "process"}}),
        ),
        (
            RunOutcome::Cancelled {
                reason: "operator".into(),
            },
            serde_json::json!({"type": "cancelled", "data": {"reason": "operator"}}),
        ),
    ];
    for (outcome, expected) in cases {
        let value = serde_json::to_value(&outcome).unwrap();
        assert_eq!(value, expected);
        assert_eq!(
            serde_json::from_value::<RunOutcome>(value).unwrap(),
            outcome
        );
    }
}

#[test]
fn an_event_envelope_round_trips_the_attempt_phase_and_outcome() {
    let envelope = EventEnvelope {
        protocol_version: PROTOCOL_VERSION,
        sequence: 42,
        occurred_at_ms: 1_723_000_010_000,
        event: FactoryEvent::RunChanged {
            run: Box::new(RunSnapshot {
                id: id::<RunId>("run-7"),
                project_id: id::<ProjectId>("project-1"),
                agent_id: id::<AgentId>("worker-2"),
                task_id: id::<TaskId>("task-3"),
                provider: Provider::Codex,
                phase: RunPhase::Finalizing,
                outcome: Some(RunOutcome::Succeeded),
                runner_instance_id: None,
                runtime_model: Some("gpt-5.6-luna".into()),
                runtime_reasoning_effort: Some("medium".into()),
                runtime_execution_mode: None,
                runtime_control_mode: None,
                activity: Some("releasing resources".into()),
                wait_reason: None,
                observer_health: ObserverHealth::Healthy,
                observer_reason: None,
                admitted_at_ms: 1_723_000_000_000,
                started_at_ms: Some(1_723_000_001_000),
                phase_since_ms: 1_723_000_009_000,
                updated_at_ms: 1_723_000_010_000,
                ended_at_ms: None,
                exit_code: None,
                exit_signal: None,
            }),
        },
    };

    let value = serde_json::to_value(&envelope).unwrap();
    let FactoryEvent::RunChanged { run } = &envelope.event else {
        panic!("expected run event");
    };
    assert!(run.has_valid_phase_outcome());
    assert_eq!(value["event"]["data"]["run"]["phase"], "finalizing");
    assert_eq!(
        value["event"]["data"]["run"]["outcome"]["type"],
        "succeeded"
    );
    assert!(value["event"]["data"]["run"].get("session_id").is_none());
    assert!(value["event"]["data"]["run"].get("parent_run_id").is_none());
    assert_eq!(
        serde_json::from_value::<EventEnvelope>(value).unwrap(),
        envelope
    );
}

#[test]
fn phase_outcome_contract_rejects_authority_with_an_outcome_or_finalizing_without_one() {
    let mut run = RunSnapshot {
        id: id::<RunId>("run-1"),
        project_id: id::<ProjectId>("project-1"),
        agent_id: id::<AgentId>("agent-1"),
        task_id: id::<TaskId>("task-1"),
        provider: Provider::Shell,
        phase: RunPhase::Running,
        outcome: None,
        runner_instance_id: None,
        runtime_model: None,
        runtime_reasoning_effort: None,
        runtime_execution_mode: None,
        runtime_control_mode: None,
        activity: None,
        wait_reason: None,
        observer_health: ObserverHealth::Healthy,
        observer_reason: None,
        admitted_at_ms: 1,
        started_at_ms: Some(2),
        phase_since_ms: 2,
        updated_at_ms: 2,
        ended_at_ms: None,
        exit_code: None,
        exit_signal: None,
    };
    assert!(run.has_valid_phase_outcome());
    run.outcome = Some(RunOutcome::Succeeded);
    assert!(!run.has_valid_phase_outcome());
    run.phase = RunPhase::Finalizing;
    assert!(run.has_valid_phase_outcome());
    run.outcome = None;
    assert!(!run.has_valid_phase_outcome());
}

#[test]
fn ids_reject_empty_unbounded_and_unsafe_values() {
    for invalid in [
        String::new(),
        "x".repeat(129),
        "has a space".into(),
        "line\nbreak".into(),
        "path/segment".into(),
    ] {
        assert!(ProjectId::try_from(invalid.clone()).is_err());
        let json = serde_json::to_string(&invalid).unwrap();
        assert!(serde_json::from_str::<ProjectId>(&json).is_err());
    }
}

#[test]
fn task_terminal_statuses_remain_a_separate_projection() {
    assert!(!TaskStatus::Queued.is_terminal());
    assert!(!TaskStatus::Running.is_terminal());
    assert!(!TaskStatus::Blocked.is_terminal());
    assert!(TaskStatus::Succeeded.is_terminal());
    assert!(TaskStatus::Failed.is_terminal());
    assert!(TaskStatus::Cancelled.is_terminal());
}
