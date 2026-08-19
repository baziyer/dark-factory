use factory_core::{
    AgentId, EventEnvelope, FactoryEvent, ObserverHealth, PROTOCOL_VERSION, ProjectId,
    RunFailureReason, RunId, RunSnapshot, RunStatus, TaskId, TaskStatus,
    local::{LocalRequest, RequestEnvelope},
};

fn id<T>(value: &str) -> T
where
    T: for<'a> TryFrom<&'a str>,
    for<'a> <T as TryFrom<&'a str>>::Error: std::fmt::Debug,
{
    T::try_from(value).unwrap()
}

#[test]
fn task_binding_requests_are_rejected_by_pre_binding_daemons() {
    let request = RequestEnvelope::new(LocalRequest::CreateTask {
        id: id("task-binding"),
        project_id: id("project"),
        parent_task_id: None,
        title: "bound".into(),
        body: "body".into(),
        priority: 1,
        agent_id: None,
        worktree: Some("/worktree".into()),
        branch: Some("agent/worker".into()),
        starting_head: Some("0123456789012345678901234567890123456789".into()),
    });
    let wire = serde_json::to_value(request).unwrap();
    assert_eq!(wire["protocol_version"], PROTOCOL_VERSION);
    assert_eq!(PROTOCOL_VERSION, 3);
    assert_ne!(wire["protocol_version"], 2);
}

#[test]
fn task_snapshot_projects_only_operator_target_context() {
    let snapshot = factory_core::TaskSnapshot {
        id: id("task-binding"),
        project_id: id("project"),
        parent_task_id: None,
        assigned_agent_id: None,
        title: "bound".into(),
        worktree_binding: Some(factory_core::WorktreeBinding {
            branch: "agent/worker".into(),
            starting_head: "0123456789012345678901234567890123456789".into(),
        }),
        status: TaskStatus::Queued,
        priority: 1,
        created_at_ms: 1,
        updated_at_ms: 1,
    };
    let value = serde_json::to_value(snapshot).unwrap();
    let binding = &value["worktree_binding"];
    assert_eq!(binding["branch"], "agent/worker");
    assert_eq!(
        binding["starting_head"],
        "0123456789012345678901234567890123456789"
    );
    for private in [
        "path",
        "git_dir",
        "common_dir",
        "worktree_device",
        "worktree_inode",
        "git_dir_device",
        "git_dir_inode",
        "common_dir_device",
        "common_dir_inode",
    ] {
        assert!(
            binding.get(private).is_none(),
            "private field leaked: {private}"
        );
    }
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
    assert_eq!(PROTOCOL_VERSION, 3);

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
fn observer_health_has_stable_wire_names_and_an_unknown_default() {
    let cases = [
        (ObserverHealth::Unknown, "unknown"),
        (ObserverHealth::Healthy, "healthy"),
        (ObserverHealth::Degraded, "degraded"),
    ];

    assert_eq!(ObserverHealth::default(), ObserverHealth::Unknown);
    for (health, expected) in cases {
        assert_eq!(serde_json::to_value(health).unwrap(), expected);
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
                session_id: None,
                closed_by: None,
                status: RunStatus::Waiting,
                activity: Some("Waiting for review".into()),
                wait_reason: None,
                worktree: "/worktrees/task-3".into(),
                observer_health: ObserverHealth::Healthy,
                observer_health_since_ms: 1_723_000_008_000,
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
    assert_eq!(value["event"]["data"]["run"]["observer_health"], "healthy");
    assert_eq!(
        value["event"]["data"]["run"]["observer_health_since_ms"],
        1_723_000_008_000_i64
    );
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
fn old_run_events_default_missing_observer_health_fields() {
    let json = r#"{
        "protocol_version": 1,
        "sequence": 7,
        "occurred_at_ms": 40,
        "event": {
            "type": "run_changed",
            "data": {
                "run": {
                    "id": "run-old",
                    "project_id": "project-old",
                    "agent_id": "agent-old",
                    "status": "running",
                    "worktree": "/work/old",
                    "started_at_ms": 10,
                    "status_since_ms": 20,
                    "updated_at_ms": 30
                }
            }
        }
    }"#;

    let decoded: EventEnvelope = serde_json::from_str(json).unwrap();
    let FactoryEvent::RunChanged { run } = decoded.event else {
        panic!("expected a run event");
    };
    assert_eq!(run.observer_health, ObserverHealth::Unknown);
    assert_eq!(run.observer_health_since_ms, 0);
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
