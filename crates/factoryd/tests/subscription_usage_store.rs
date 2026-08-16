use factory_core::{AgentId, AgentRole, ProjectId, Provider, TaskStatus};
use factoryd::store::{
    NewAgent, NewProject, Store, SubscriptionFailureCategory, SubscriptionLimitWindow,
    SubscriptionProbe, SubscriptionProbeOutcome, SubscriptionSeverity,
};

fn project_id(value: &str) -> ProjectId {
    ProjectId::try_from(value).unwrap()
}

fn agent_id(value: &str) -> AgentId {
    AgentId::try_from(value).unwrap()
}

fn task_id(value: &str) -> factory_core::TaskId {
    factory_core::TaskId::try_from(value).unwrap()
}

fn fixture() -> Store {
    let mut store = Store::open_in_memory().unwrap();
    store
        .create_project(
            NewProject {
                id: project_id("factory"),
                name: "Factory".into(),
                root: "/tmp/factory".into(),
            },
            1,
        )
        .unwrap();
    store
        .create_agent(
            NewAgent {
                id: agent_id("god"),
                project_id: project_id("factory"),
                parent_agent_id: None,
                role: AgentRole::Orchestrator,
                provider: Provider::Codex,
            },
            2,
        )
        .unwrap();
    store
}

fn observed(
    provider: Provider,
    at: i64,
    used_percent: u8,
    exhausted: bool,
    notification: &str,
) -> SubscriptionProbe {
    SubscriptionProbe {
        project_id: project_id("factory"),
        orchestrator_agent_id: agent_id("god"),
        provider,
        attempted_at_ms: at,
        outcome: SubscriptionProbeOutcome::Observed {
            used_percent,
            limit_window: SubscriptionLimitWindow::Primary,
            resets_at_ms: Some(at + 10_000),
            exhausted,
            windows: Vec::new(),
        },
        notification_task_id: task_id(notification),
    }
}

fn failed(provider: Provider, at: i64, notification: &str) -> SubscriptionProbe {
    SubscriptionProbe {
        project_id: project_id("factory"),
        orchestrator_agent_id: agent_id("god"),
        provider,
        attempted_at_ms: at,
        outcome: SubscriptionProbeOutcome::Failed {
            category: SubscriptionFailureCategory::Timeout,
        },
        notification_task_id: task_id(notification),
    }
}

#[test]
fn exact_capacity_thresholds_only_alert_on_upward_transitions_and_replays_dedupe() {
    let mut store = fixture();

    let ok = store
        .record_subscription_probe(observed(Provider::Codex, 10, 79, false, "unused-1"))
        .unwrap();
    assert_eq!(ok.state.severity, SubscriptionSeverity::Ok);
    assert!(!ok.notification_created);
    assert!(ok.events.is_empty());

    let warning = store
        .record_subscription_probe(observed(Provider::Codex, 20, 80, false, "usage-warning-1"))
        .unwrap();
    assert_eq!(warning.state.severity, SubscriptionSeverity::Warning);
    assert!(warning.notification_created);
    assert_eq!(warning.events.len(), 1);

    let replay = store
        .record_subscription_probe(observed(Provider::Codex, 20, 80, false, "must-not-exist"))
        .unwrap();
    assert_eq!(replay.state.severity, SubscriptionSeverity::Warning);
    assert!(!replay.notification_created);
    assert!(replay.events.is_empty());

    let same_band = store
        .record_subscription_probe(observed(Provider::Codex, 30, 94, false, "unused-2"))
        .unwrap();
    assert!(!same_band.notification_created);

    let critical = store
        .record_subscription_probe(observed(Provider::Codex, 40, 95, false, "usage-critical-1"))
        .unwrap();
    assert_eq!(critical.state.severity, SubscriptionSeverity::Critical);
    assert!(critical.notification_created);

    let recovered = store
        .record_subscription_probe(observed(Provider::Codex, 50, 10, false, "unused-3"))
        .unwrap();
    assert_eq!(recovered.state.severity, SubscriptionSeverity::Ok);
    assert!(!recovered.notification_created);

    let warning_again = store
        .record_subscription_probe(observed(Provider::Codex, 60, 80, false, "usage-warning-2"))
        .unwrap();
    assert!(warning_again.notification_created);

    let exhausted = store
        .record_subscription_probe(observed(
            Provider::ClaudeCode,
            70,
            1,
            true,
            "usage-critical-2",
        ))
        .unwrap();
    assert_eq!(exhausted.state.severity, SubscriptionSeverity::Critical);
    assert!(exhausted.notification_created);

    let tasks = store.list_tasks(&project_id("factory"), None, 20).unwrap();
    assert_eq!(tasks.len(), 4);
    assert!(tasks.iter().all(|task| {
        task.snapshot.assigned_agent_id == Some(agent_id("god"))
            && task.snapshot.status == TaskStatus::Queued
            && !task.body.contains("80")
            && !task.body.contains("95")
    }));
}

#[test]
fn third_consecutive_collection_failure_becomes_visible_once_and_success_recovers() {
    let mut store = fixture();
    store
        .record_subscription_probe(observed(Provider::ClaudeCode, 10, 20, false, "unused-1"))
        .unwrap();

    for (at, id) in [(20, "unused-2"), (30, "unused-3")] {
        let result = store
            .record_subscription_probe(failed(Provider::ClaudeCode, at, id))
            .unwrap();
        assert_eq!(result.state.severity, SubscriptionSeverity::Ok);
        assert!(!result.notification_created);
    }
    let third = store
        .record_subscription_probe(failed(Provider::ClaudeCode, 40, "collector-warning-1"))
        .unwrap();
    assert_eq!(third.state.severity, SubscriptionSeverity::Warning);
    assert_eq!(third.state.consecutive_failures, 3);
    assert!(third.notification_created);

    let fourth = store
        .record_subscription_probe(failed(Provider::ClaudeCode, 50, "unused-4"))
        .unwrap();
    assert_eq!(fourth.state.consecutive_failures, 4);
    assert!(!fourth.notification_created);

    let recovered = store
        .record_subscription_probe(observed(Provider::ClaudeCode, 60, 20, false, "unused-5"))
        .unwrap();
    assert_eq!(recovered.state.severity, SubscriptionSeverity::Ok);
    assert_eq!(recovered.state.consecutive_failures, 0);
    assert!(!recovered.notification_created);

    let snapshot = store.subscription_usage_snapshot().unwrap();
    assert_eq!(snapshot.providers.len(), 1);
    assert_eq!(snapshot.providers[0].used_percent, Some(20));
    assert_eq!(snapshot.providers[0].last_success_at_ms, Some(60));
    assert_eq!(snapshot.overall_severity, SubscriptionSeverity::Ok);
}

#[test]
fn normalized_state_survives_reopen_without_raw_provider_output() {
    let directory = tempfile::tempdir_in("/tmp").unwrap();
    let database = directory.path().join("factory.db");
    {
        let mut store = Store::open(&database).unwrap();
        store
            .create_project(
                NewProject {
                    id: project_id("factory"),
                    name: "Factory".into(),
                    root: "/tmp/factory-persistent".into(),
                },
                1,
            )
            .unwrap();
        store
            .create_agent(
                NewAgent {
                    id: agent_id("god"),
                    project_id: project_id("factory"),
                    parent_agent_id: None,
                    role: AgentRole::Orchestrator,
                    provider: Provider::Codex,
                },
                2,
            )
            .unwrap();
        store
            .record_subscription_probe(observed(Provider::Codex, 10, 81, false, "warning"))
            .unwrap();
    }

    let bytes = std::fs::read(&database).unwrap();
    assert!(!String::from_utf8_lossy(&bytes).contains("private-terminal-sentinel"));
    let reopened = Store::open(&database).unwrap();
    let snapshot = reopened.subscription_usage_snapshot().unwrap();
    assert_eq!(snapshot.providers[0].used_percent, Some(81));
    assert_eq!(
        snapshot.providers[0].severity,
        SubscriptionSeverity::Warning
    );
}

#[test]
fn same_timestamp_with_different_normalized_data_is_a_conflict_and_rolls_back() {
    let mut store = fixture();
    store
        .record_subscription_probe(observed(Provider::Codex, 10, 20, false, "unused-1"))
        .unwrap();
    let error = store
        .record_subscription_probe(observed(Provider::Codex, 10, 95, false, "must-not-exist"))
        .err()
        .unwrap();
    assert!(matches!(
        error,
        factoryd::store::StoreError::InvalidSubscriptionProbe
    ));
    let snapshot = store.subscription_usage_snapshot().unwrap();
    assert_eq!(snapshot.providers[0].used_percent, Some(20));
    assert_eq!(snapshot.providers[0].severity, SubscriptionSeverity::Ok);
    assert!(
        store
            .list_tasks(&project_id("factory"), None, 10)
            .unwrap()
            .is_empty()
    );
}
