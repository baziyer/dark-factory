use factory_core::{
    AgentId, EventEnvelope, FactoryEvent, PROTOCOL_VERSION, ProjectId, RunFailureReason, RunId,
    RunSnapshot, RunStatus, TaskId, TaskStatus,
};

fn id<T>(value: &str) -> T
where
    T: for<'a> TryFrom<&'a str>,
    for<'a> <T as TryFrom<&'a str>>::Error: std::fmt::Debug,
{
    T::try_from(value).unwrap()
}

#[test]
fn all_run_status_values_have_stable_wire_names() {
    let cases = [
        (RunStatus::Starting, "starting"),
        (RunStatus::Running, "running"),
        (RunStatus::Waiting, "waiting"),
        (RunStatus::Blocked, "blocked"),
        (RunStatus::Paused, "paused"),
        (RunStatus::Succeeded, "succeeded"),
        (RunStatus::Failed, "failed"),
        (RunStatus::Stopped, "stopped"),
    ];

    for (status, expected) in cases {
        assert_eq!(serde_json::to_value(status).unwrap(), expected);
    }
}

#[test]
fn all_run_failure_reasons_have_stable_wire_names() {
    assert_eq!(PROTOCOL_VERSION, 1);

    let cases = [
        (RunFailureReason::Protocol, "protocol"),
        (RunFailureReason::Provider, "provider"),
        (RunFailureReason::Permission, "permission"),
        (RunFailureReason::Limit, "limit"),
        (RunFailureReason::Process, "process"),
        (RunFailureReason::Spawn, "spawn"),
        (RunFailureReason::Incomplete, "incomplete"),
        (RunFailureReason::Unverifiable, "unverifiable"),
    ];

    for (reason, expected) in cases {
        assert_eq!(serde_json::to_value(reason).unwrap(), expected);
    }
}

#[test]
fn an_event_envelope_round_trips_run_hierarchy_and_activity() {
    let envelope = EventEnvelope {
        protocol_version: PROTOCOL_VERSION,
        sequence: 42,
        occurred_at_ms: 1_723_000_010_000,
        event: FactoryEvent::RunChanged {
            run: RunSnapshot {
                id: id::<RunId>("run-7"),
                project_id: id::<ProjectId>("project-1"),
                agent_id: id::<AgentId>("worker-2"),
                parent_run_id: Some(id::<RunId>("run-1")),
                task_id: Some(id::<TaskId>("task-3")),
                status: RunStatus::Waiting,
                activity: Some("Waiting for review".into()),
                wait_reason: None,
                worktree: "/worktrees/task-3".into(),
                started_at_ms: 1_723_000_000_000,
                status_since_ms: 1_723_000_009_000,
                updated_at_ms: 1_723_000_010_000,
                ended_at_ms: None,
                exit_code: None,
                exit_signal: None,
                failure_reason: None,
            },
        },
    };

    let json = serde_json::to_string(&envelope).unwrap();
    let decoded: EventEnvelope = serde_json::from_str(&json).unwrap();
    let value: serde_json::Value = serde_json::from_str(&json).unwrap();

    assert_eq!(decoded, envelope);
    assert_eq!(value["protocol_version"], PROTOCOL_VERSION);
    assert_eq!(value["sequence"], 42);
    assert_eq!(value["event"]["type"], "run_changed");
    assert_eq!(value["event"]["data"]["run"]["parent_run_id"], "run-1");
    assert_eq!(value["event"]["data"]["run"]["project_id"], "project-1");
    assert_eq!(value["event"]["data"]["run"]["status"], "waiting");
    assert!(value["event"]["data"]["run"].get("ended_at_ms").is_none());
    assert!(value["event"]["data"]["run"].get("exit_code").is_none());
    assert!(value["event"]["data"]["run"].get("exit_signal").is_none());
    assert!(
        value["event"]["data"]["run"]
            .get("failure_reason")
            .is_none()
    );
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
fn only_finished_states_are_terminal() {
    assert!(!RunStatus::Starting.is_terminal());
    assert!(!RunStatus::Running.is_terminal());
    assert!(!RunStatus::Waiting.is_terminal());
    assert!(!RunStatus::Blocked.is_terminal());
    assert!(!RunStatus::Paused.is_terminal());
    assert!(RunStatus::Succeeded.is_terminal());
    assert!(RunStatus::Failed.is_terminal());
    assert!(RunStatus::Stopped.is_terminal());

    assert!(!TaskStatus::Queued.is_terminal());
    assert!(!TaskStatus::Running.is_terminal());
    assert!(!TaskStatus::Blocked.is_terminal());
    assert!(TaskStatus::Succeeded.is_terminal());
    assert!(TaskStatus::Failed.is_terminal());
    assert!(TaskStatus::Cancelled.is_terminal());
}
