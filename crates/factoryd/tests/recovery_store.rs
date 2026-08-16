use factory_core::{
    AgentRole, FactoryEvent, ProjectId, Provider, RunFailureReason, RunId, RunStatus,
    RunnerInstanceId, TaskStatus,
    runner::{OutputStream, RUNNER_PROTOCOL_VERSION, RunnerEvent, RunnerEventEnvelope},
};
use factoryd::store::{
    NewAgent, NewProject, NewTask, RunReservation, RunnerEventEffects, Store, StoreError,
    WriteDisposition,
};

fn id<T>(value: &str) -> T
where
    T: TryFrom<String>,
    T::Error: std::fmt::Debug,
{
    T::try_from(value.to_owned()).unwrap()
}

fn create_reserved_run(
    store: &mut Store,
    project: &str,
    task: &str,
    agent: &str,
    run: &str,
) -> RunnerInstanceId {
    store
        .create_project(
            NewProject {
                id: id(project),
                name: project.into(),
                root: format!("/work/{project}"),
            },
            1,
        )
        .unwrap();
    store
        .create_task(
            NewTask {
                id: id(task),
                project_id: id(project),
                parent_task_id: None,
                title: task.into(),
                body: "private task body".into(),
                priority: 0,
            },
            2,
        )
        .unwrap();
    store
        .create_agent(
            NewAgent {
                id: id(agent),
                project_id: id(project),
                parent_agent_id: None,
                role: AgentRole::Worker,
                provider: Provider::Codex,
            },
            3,
        )
        .unwrap();
    let instance: RunnerInstanceId = id(&format!("instance-{run}"));
    store
        .reserve_task_run(
            RunReservation {
                project_id: id(project),
                task_id: id(task),
                agent_id: id(agent),
                expected_provider: Provider::Codex,
                run_id: id(run),
                parent_run_id: None,
                worktree: format!("/work/{project}"),
                fresh_provider_session_id: None,
                runner_instance_id: instance.clone(),
                runner_runtime: format!("/private/runners/{run}"),
            },
            1,
            4,
        )
        .unwrap();
    instance
}

fn runner_event(sequence: i64, event: RunnerEvent) -> RunnerEventEnvelope {
    RunnerEventEnvelope {
        protocol_version: RUNNER_PROTOCOL_VERSION,
        sequence,
        occurred_at_ms: 10_000 + sequence,
        event,
    }
}

fn no_effects() -> RunnerEventEffects {
    RunnerEventEffects {
        confirmed_provider_session_id: None,
        terminal_outcome: None,
    }
}

#[test]
fn migrates_v2_acknowledgements_to_reconciliation_without_changing_recovery() {
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
            .execute_batch(
                "INSERT INTO projects (id, name, root, created_at_ms, updated_at_ms)
                 VALUES ('project', 'Project', '/work/project', 1, 1);

                 INSERT INTO tasks (
                     id, project_id, assigned_agent_id, title, body, status,
                     priority, created_at_ms, updated_at_ms
                 ) VALUES
                     ('task-reconciled', 'project', 'agent-reconciled', 'Done', 'private',
                      'succeeded', 0, 2, 8),
                     ('task-unreconciled', 'project', 'agent-unreconciled', 'Pending cleanup',
                      'private', 'succeeded', 0, 3, 9);

                 INSERT INTO agents (
                     id, project_id, role, provider, created_at_ms, updated_at_ms
                 ) VALUES
                     ('agent-reconciled', 'project', 'worker', 'codex', 4, 8),
                     ('agent-unreconciled', 'project', 'worker', 'codex', 5, 9);

                 INSERT INTO runs (
                     id, project_id, agent_id, task_id, status, worktree,
                     resumes_provider_session, runner_instance_id,
                     runner_protocol_version, runner_runtime, last_runner_sequence,
                     terminal_runner_sequence, runner_acknowledged_at_ms,
                     runner_terminal_kind, started_at_ms, status_since_ms,
                     updated_at_ms, ended_at_ms, exit_code
                 ) VALUES
                     ('run-reconciled', 'project', 'agent-reconciled', 'task-reconciled',
                      'succeeded', '/work/project', 0, 'instance-reconciled', 1,
                      '/private/runners/reconciled', 1, 1, 11, 'exited', 6, 8, 8, 8, 0),
                     ('run-unreconciled', 'project', 'agent-unreconciled', 'task-unreconciled',
                      'succeeded', '/work/project', 0, 'instance-unreconciled', 1,
                      '/private/runners/unreconciled', 1, 1, NULL, 'exited', 7, 9, 9, 9, 0);",
            )
            .unwrap();
        connection.pragma_update(None, "user_version", 2).unwrap();
    }

    let store = Store::open(&database).unwrap();
    let recoverable = store.recoverable_runs().unwrap();
    assert_eq!(recoverable.len(), 1);
    assert_eq!(recoverable[0].run.id, id::<RunId>("run-unreconciled"));
    assert_eq!(recoverable[0].runner_reconciled_at_ms, None);
    drop(store);

    let connection = rusqlite::Connection::open(&database).unwrap();
    let version: i64 = connection
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .unwrap();
    assert_eq!(version, 12);
    let reconciled: Option<i64> = connection
        .query_row(
            "SELECT runner_reconciled_at_ms FROM runs WHERE id = 'run-reconciled'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let unreconciled: Option<i64> = connection
        .query_row(
            "SELECT runner_reconciled_at_ms FROM runs WHERE id = 'run-unreconciled'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(reconciled, Some(11));
    assert_eq!(unreconciled, None);
    let recovery_index: String = connection
        .query_row(
            "SELECT sql FROM sqlite_schema WHERE type = 'index' AND name = 'runs_recoverable'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert!(recovery_index.contains("runner_reconciled_at_ms"));
    assert!(!recovery_index.contains("runner_acknowledged_at_ms"));
    let runs_table: String = connection
        .query_row(
            "SELECT sql FROM sqlite_schema WHERE type = 'table' AND name = 'runs'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert!(runs_table.contains("runner_reconciled_at_ms"));
    assert!(!runs_table.contains("runner_acknowledged_at_ms"));
    connection
        .execute_batch("PRAGMA foreign_keys = ON")
        .unwrap();
    let violations: i64 = connection
        .query_row("SELECT COUNT(*) FROM pragma_foreign_key_check", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(violations, 0);
    drop(connection);

    let reopened = Store::open(&database).unwrap();
    let recoverable = reopened.recoverable_runs().unwrap();
    assert_eq!(recoverable.len(), 1);
    assert_eq!(recoverable[0].run.id, id::<RunId>("run-unreconciled"));
}

#[test]
fn unverifiable_failure_is_atomic_idempotent_and_preserves_runner_cursor() {
    for (run_status, committed_cursor, task_status) in [
        ("starting", 0, "running"),
        ("running", 2, "running"),
        ("waiting", 2, "running"),
        ("paused", 2, "running"),
        ("blocked", 2, "blocked"),
    ] {
        let directory = tempfile::tempdir().unwrap();
        let database = directory.path().join("factory.db");
        let mut store = Store::open(&database).unwrap();
        let instance = create_reserved_run(&mut store, "project", "task", "agent", "run");
        if run_status != "starting" {
            store
                .ingest_runner_event(
                    &id("run"),
                    &instance,
                    &runner_event(1, RunnerEvent::Started { child_pid: 42 }),
                    no_effects(),
                    5,
                )
                .unwrap();
            store
                .ingest_runner_event(
                    &id("run"),
                    &instance,
                    &runner_event(
                        2,
                        RunnerEvent::Output {
                            stream: OutputStream::Stdout,
                            text: "private output".into(),
                            lossy: false,
                        },
                    ),
                    no_effects(),
                    6,
                )
                .unwrap();
            drop(store);
            let connection = rusqlite::Connection::open(&database).unwrap();
            connection
                .execute(
                    "UPDATE runs SET status = ?1, status_since_ms = 7 WHERE id = 'run'",
                    [run_status],
                )
                .unwrap();
            connection
                .execute(
                    "UPDATE tasks SET status = ?1 WHERE id = 'task'",
                    [task_status],
                )
                .unwrap();
            drop(connection);
            store = Store::open(&database).unwrap();
        }
        let head = store.latest_event_sequence().unwrap();
        assert!(matches!(
            store.fail_run_unverifiable(&id("run"), &id("wrong-instance"), 7),
            Err(StoreError::RunnerIdentityMismatch)
        ));
        assert_eq!(store.latest_event_sequence().unwrap(), head);
        assert_eq!(
            store
                .execution_target(&id("run"))
                .unwrap()
                .last_committed_runner_sequence,
            committed_cursor
        );

        let failed = store
            .fail_run_unverifiable(&id("run"), &instance, 8)
            .unwrap();
        assert_eq!(failed.disposition, WriteDisposition::Applied);
        assert_eq!(failed.events.len(), 3);
        assert_eq!(failed.run.status, RunStatus::Failed);
        assert_eq!(
            failed.run.failure_reason,
            Some(RunFailureReason::Unverifiable)
        );
        assert_eq!(failed.run.ended_at_ms, Some(8));
        assert_eq!(failed.task.status, TaskStatus::Failed);
        assert!(failed.agent.current_run_id.is_none());
        assert!(matches!(
            &failed.events[0].event,
            FactoryEvent::TaskChanged { task } if task.status == TaskStatus::Failed
        ));
        assert!(matches!(
            &failed.events[1].event,
            FactoryEvent::AgentChanged { agent } if agent.current_run_id.is_none()
        ));
        assert!(matches!(
            &failed.events[2].event,
            FactoryEvent::RunChanged { run }
                if run.status == RunStatus::Failed
                    && run.failure_reason == Some(RunFailureReason::Unverifiable)
        ));
        assert_eq!(store.latest_event_sequence().unwrap(), head + 3);
        assert_eq!(
            store
                .execution_target(&id("run"))
                .unwrap()
                .last_committed_runner_sequence,
            committed_cursor
        );
        assert!(store.recoverable_runs().unwrap().is_empty());

        let retry_head = store.latest_event_sequence().unwrap();
        let retry = store
            .fail_run_unverifiable(&id("run"), &instance, 999)
            .unwrap();
        assert_eq!(retry.disposition, WriteDisposition::Duplicate);
        assert!(retry.events.is_empty());
        assert_eq!(retry.run.updated_at_ms, 8);
        assert_eq!(store.latest_event_sequence().unwrap(), retry_head);
        drop(store);

        let connection = rusqlite::Connection::open(&database).unwrap();
        let private_ledger: (i64, Option<i64>, Option<i64>, String) = connection
            .query_row(
                "SELECT last_runner_sequence, terminal_runner_sequence,
                        runner_reconciled_at_ms, failure_reason
                 FROM runs WHERE id = 'run'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .unwrap();
        assert_eq!(
            private_ledger,
            (committed_cursor, None, None, "unverifiable".into())
        );
    }
}

#[test]
fn unverifiable_failure_rejects_other_terminal_runs_without_mutation() {
    let mut store = Store::open_in_memory().unwrap();
    let instance = create_reserved_run(&mut store, "project", "task", "agent", "run");
    store.fail_run_launch(&id("run"), &instance, 5).unwrap();
    let head = store.latest_event_sequence().unwrap();

    assert!(matches!(
        store.fail_run_unverifiable(&id("run"), &id("wrong-instance"), 6),
        Err(StoreError::RunnerIdentityMismatch)
    ));
    assert!(matches!(
        store.fail_run_unverifiable(&id("run"), &instance, 6),
        Err(StoreError::InvalidRunState)
    ));
    assert_eq!(store.latest_event_sequence().unwrap(), head);
    let run = store.recoverable_runs().unwrap();
    assert!(run.is_empty());
    let tasks = store
        .list_tasks(&id::<ProjectId>("project"), None, 10)
        .unwrap();
    assert_eq!(tasks[0].snapshot.status, TaskStatus::Failed);
}
