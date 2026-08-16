use factory_core::{AgentRole, FactoryEvent, ObserverHealth, Provider, RunId, RunnerInstanceId};
use factoryd::store::{
    NewAgent, NewProject, NewTask, RunReservation, Store, StoreError, WriteDisposition,
};

fn id<T>(value: &str) -> T
where
    T: TryFrom<String>,
    T::Error: std::fmt::Debug,
{
    T::try_from(value.to_owned()).unwrap()
}

fn reserve_run(store: &mut Store, now_ms: i64) -> (RunId, RunnerInstanceId) {
    store
        .create_project(
            NewProject {
                id: id("project"),
                name: "Project".into(),
                root: "/work/project".into(),
            },
            1,
        )
        .unwrap();
    store
        .create_task(
            NewTask {
                id: id("task"),
                project_id: id("project"),
                parent_task_id: None,
                title: "private task title".into(),
                body: "private task body".into(),
                priority: 0,
            },
            2,
        )
        .unwrap();
    store
        .create_agent(
            NewAgent {
                id: id("agent"),
                project_id: id("project"),
                parent_agent_id: None,
                role: AgentRole::Worker,
                provider: Provider::Codex,
            },
            3,
        )
        .unwrap();
    let run_id: RunId = id("run");
    let runner_instance_id: RunnerInstanceId = id("private-runner-instance");
    let reserved = store
        .reserve_task_run(
            RunReservation {
                project_id: id("project"),
                task_id: id("task"),
                agent_id: id("agent"),
                expected_provider: Provider::Codex,
                run_id: run_id.clone(),
                parent_run_id: None,
                worktree: "/work/project".into(),
                fresh_provider_session_id: None,
                runner_instance_id: runner_instance_id.clone(),
                runner_runtime: "/private/runner/runtime".into(),
            },
            1,
            now_ms,
        )
        .unwrap();
    assert_eq!(reserved.run.observer_health, ObserverHealth::Unknown);
    assert_eq!(reserved.run.observer_health_since_ms, now_ms);
    (run_id, runner_instance_id)
}

#[test]
fn migrates_v3_health_as_unknown_with_a_nonnegative_since_and_reopens() {
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("factory.db");
    {
        let connection = rusqlite::Connection::open(&database).unwrap();
        connection
            .execute_batch(include_str!("../migrations/0001_state_and_events.sql"))
            .unwrap();
        connection
            .execute_batch(include_str!("../migrations/0002_execution_ledger.sql"))
            .unwrap();
        connection
            .execute_batch(include_str!("../migrations/0003_runner_reconciliation.sql"))
            .unwrap();
        connection
            .execute_batch(
                "INSERT INTO projects (id, name, root, created_at_ms, updated_at_ms)
                 VALUES ('project', 'Project', '/work/project', 1, 1);

                 INSERT INTO tasks (
                     id, project_id, assigned_agent_id, title, body, status,
                     priority, created_at_ms, updated_at_ms
                 ) VALUES (
                     'task', 'project', 'agent', 'Old task', 'private old body',
                     'running', 0, 2, 42
                 );

                 INSERT INTO agents (
                     id, project_id, role, provider, created_at_ms, updated_at_ms
                 ) VALUES ('agent', 'project', 'worker', 'codex', 3, 42);

                 INSERT INTO runs (
                     id, project_id, agent_id, task_id, status, worktree,
                     resumes_provider_session, runner_instance_id,
                     runner_protocol_version, runner_runtime, last_runner_sequence,
                     started_at_ms, status_since_ms, updated_at_ms
                 ) VALUES (
                     'run-old', 'project', 'agent', 'task', 'starting', '/work/project',
                     0, 'instance-old', 1, '/private/runners/old', 0,
                     4, 4, -1
                 );",
            )
            .unwrap();
        connection.pragma_update(None, "user_version", 3).unwrap();
    }

    for _ in 0..2 {
        let mut store = Store::open(&database).unwrap();
        let recoverable = store.recoverable_executions().unwrap();
        assert_eq!(recoverable.len(), 1);
        assert_eq!(recoverable[0].observer_health, ObserverHealth::Unknown);

        let unchanged = store
            .set_observer_health(
                &id("run-old"),
                &id("instance-old"),
                ObserverHealth::Unknown,
                100,
            )
            .unwrap();
        assert_eq!(unchanged.disposition, WriteDisposition::Duplicate);
        assert_eq!(unchanged.run.observer_health, ObserverHealth::Unknown);
        assert_eq!(unchanged.run.observer_health_since_ms, 0);
        assert_eq!(unchanged.run.updated_at_ms, -1);
        assert!(unchanged.events.is_empty());
    }

    let connection = rusqlite::Connection::open(&database).unwrap();
    let version: i64 = connection
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .unwrap();
    let stored: (String, i64) = connection
        .query_row(
            "SELECT observer_health, observer_health_since_ms
             FROM runs WHERE id = 'run-old'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(version, 13);
    assert_eq!(stored, ("unknown".into(), 0));
}

#[test]
fn health_changes_are_identity_checked_idempotent_private_and_terminal_safe() {
    let mut store = Store::open_in_memory().unwrap();
    let (run_id, runner_instance_id) = reserve_run(&mut store, 4);
    let initial_head = store.latest_event_sequence().unwrap();

    assert!(matches!(
        store.set_observer_health(
            &run_id,
            &id("different-instance"),
            ObserverHealth::Healthy,
            5,
        ),
        Err(StoreError::RunnerIdentityMismatch)
    ));
    assert_eq!(store.latest_event_sequence().unwrap(), initial_head);

    let changed = store
        .set_observer_health(&run_id, &runner_instance_id, ObserverHealth::Healthy, 5)
        .unwrap();
    assert_eq!(changed.disposition, WriteDisposition::Applied);
    assert_eq!(changed.run.observer_health, ObserverHealth::Healthy);
    assert_eq!(changed.run.observer_health_since_ms, 5);
    assert_eq!(changed.run.updated_at_ms, 5);
    assert_eq!(changed.events.len(), 1);
    assert!(matches!(
        &changed.events[0].event,
        FactoryEvent::RunChanged { run }
            if run.id == run_id && run.observer_health == ObserverHealth::Healthy
    ));
    let public_event = serde_json::to_string(&changed.events).unwrap();
    assert!(!public_event.contains("private task"));
    assert!(!public_event.contains("private-runner-instance"));
    assert!(!public_event.contains("/private/runner/runtime"));

    let changed_head = store.latest_event_sequence().unwrap();
    assert_eq!(changed_head, initial_head + 1);
    let duplicate = store
        .set_observer_health(&run_id, &runner_instance_id, ObserverHealth::Healthy, 99)
        .unwrap();
    assert_eq!(duplicate.disposition, WriteDisposition::Duplicate);
    assert_eq!(duplicate.run.observer_health_since_ms, 5);
    assert_eq!(duplicate.run.updated_at_ms, 5);
    assert!(duplicate.events.is_empty());
    assert_eq!(store.latest_event_sequence().unwrap(), changed_head);

    store
        .fail_run_launch(&run_id, &runner_instance_id, 6)
        .unwrap();
    let terminal_head = store.latest_event_sequence().unwrap();
    let degraded = store
        .set_observer_health(&run_id, &runner_instance_id, ObserverHealth::Degraded, 7)
        .unwrap();
    assert_eq!(degraded.disposition, WriteDisposition::Applied);
    assert_eq!(degraded.run.observer_health, ObserverHealth::Degraded);
    assert_eq!(degraded.run.observer_health_since_ms, 7);
    assert_eq!(degraded.events.len(), 1);
    assert!(matches!(
        degraded.events[0].event,
        FactoryEvent::RunChanged { .. }
    ));
    assert_eq!(store.latest_event_sequence().unwrap(), terminal_head + 1);
}
