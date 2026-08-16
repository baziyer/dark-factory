use factory_core::{
    AgentRole, FactoryEvent, ProjectId, Provider, RunFailureReason, RunId, RunStatus,
    RunnerInstanceId, TaskId, TaskStatus,
    runner::{OutputStream, RUNNER_PROTOCOL_VERSION, RunnerEvent, RunnerEventEnvelope},
};
use factoryd::store::{
    IngestDisposition, NewAgent, NewProject, NewTask, RunReservation, RunnerEventEffects, Store,
    StoreError, TerminalOutcome, WriteDisposition,
};

fn id<T>(value: &str) -> T
where
    T: TryFrom<String>,
    T::Error: std::fmt::Debug,
{
    T::try_from(value.to_owned()).unwrap()
}

fn create_project_and_task(store: &mut Store, project: &str, task: &str, now_ms: i64) {
    store
        .create_project(
            NewProject {
                id: id(project),
                name: project.into(),
                root: format!("/work/{project}"),
            },
            now_ms,
        )
        .unwrap();
    store
        .create_task(
            NewTask {
                id: id(task),
                project_id: id(project),
                parent_task_id: None,
                title: task.into(),
                body: format!("private instructions for {task}"),
                priority: 10,
            },
            now_ms + 1,
        )
        .unwrap();
}

fn create_worker(
    store: &mut Store,
    project: &str,
    agent: &str,
    parent_agent_id: Option<&str>,
    now_ms: i64,
) {
    store
        .create_agent(
            NewAgent {
                id: id(agent),
                project_id: id(project),
                parent_agent_id: parent_agent_id.map(id),
                role: AgentRole::Worker,
                provider: Provider::Codex,
            },
            now_ms,
        )
        .unwrap();
}

fn reservation(
    project: &str,
    task: &str,
    agent: &str,
    run: &str,
    parent_run_id: Option<&str>,
) -> RunReservation {
    RunReservation {
        project_id: id(project),
        task_id: id(task),
        agent_id: id(agent),
        run_id: id(run),
        parent_run_id: parent_run_id.map(id),
        worktree: format!("/work/{project}"),
        fresh_provider_session_id: None,
        runner_instance_id: id(&format!("instance-{run}")),
        runner_runtime: format!("/private/runners/{run}"),
    }
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
fn migrates_v1_to_v3_without_losing_state_or_event_head() {
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("factory.db");
    {
        let connection = rusqlite::Connection::open(&database).unwrap();
        connection
            .execute_batch(include_str!("../migrations/0001_state_and_events.sql"))
            .unwrap();
        connection
            .execute(
                "INSERT INTO projects (id, name, root, created_at_ms, updated_at_ms)
                 VALUES ('project-v1', 'Version one', '/work/v1', 1, 1)",
                [],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO tasks (
                    id, project_id, parent_task_id, assigned_agent_id, title, body,
                    status, priority, created_at_ms, updated_at_ms
                 ) VALUES ('task-v1', 'project-v1', NULL, NULL, 'Old task', 'Old body',
                           'queued', 0, 2, 2)",
                [],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO events (
                    occurred_at_ms, project_id, task_id, agent_id, run_id,
                    kind, schema_version, payload_json
                 ) VALUES (2, 'project-v1', 'task-v1', NULL, NULL,
                           'task_changed', 1,
                           '{\"type\":\"task_changed\",\"data\":{\"task\":{\"id\":\"task-v1\",\"project_id\":\"project-v1\",\"title\":\"Old task\",\"status\":\"queued\",\"priority\":0,\"created_at_ms\":2,\"updated_at_ms\":2}}}')",
                [],
            )
            .unwrap();
        connection.pragma_update(None, "user_version", 1).unwrap();
    }

    let mut store = Store::open(&database).unwrap();
    assert_eq!(store.latest_event_sequence().unwrap(), 1);
    assert_eq!(store.events_after(0, 10).unwrap().len(), 1);
    assert_eq!(
        store
            .list_tasks(&id::<ProjectId>("project-v1"), None, 10)
            .unwrap()[0]
            .body,
        "Old body"
    );
    create_worker(&mut store, "project-v1", "worker-v2", None, 3);
    store
        .reserve_task_run(
            reservation("project-v1", "task-v1", "worker-v2", "run-v2", None),
            1,
            4,
        )
        .unwrap();
    drop(store);

    let connection = rusqlite::Connection::open(&database).unwrap();
    let version: i64 = connection
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .unwrap();
    assert_eq!(version, 3);
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
    assert_eq!(
        Store::open(&database)
            .unwrap()
            .latest_event_sequence()
            .unwrap(),
        5
    );
}

#[test]
fn reservation_is_atomic_explicit_and_capacity_bounded() {
    let mut store = Store::open_in_memory().unwrap();
    create_project_and_task(&mut store, "project-one", "task-one", 1);
    store
        .create_task(
            NewTask {
                id: id("task-two"),
                project_id: id("project-one"),
                parent_task_id: None,
                title: "task-two".into(),
                body: "second private body".into(),
                priority: 999,
            },
            3,
        )
        .unwrap();
    create_worker(&mut store, "project-one", "worker-one", None, 4);
    create_worker(&mut store, "project-one", "worker-two", None, 5);

    let reserved = store
        .reserve_task_run(
            reservation("project-one", "task-one", "worker-one", "run-one", None),
            1,
            6,
        )
        .unwrap();

    assert_eq!(reserved.task.id, id::<TaskId>("task-one"));
    assert_eq!(reserved.task.status, TaskStatus::Running);
    assert_eq!(reserved.task.assigned_agent_id, Some(id("worker-one")));
    assert_eq!(reserved.agent.current_run_id, Some(id("run-one")));
    assert_eq!(reserved.run.status, RunStatus::Starting);
    assert_eq!(reserved.run.project_id, id::<ProjectId>("project-one"));
    assert_eq!(reserved.events.len(), 3);
    let target = store.execution_target(&id("run-one")).unwrap();
    assert_eq!(target.provider, Provider::Codex);
    assert_eq!(target.project_root, "/work/project-one");
    assert_eq!(target.task_body, "private instructions for task-one");
    assert_eq!(target.worktree, "/work/project-one");
    assert_eq!(target.runner_instance_id, id("instance-run-one"));
    assert_eq!(target.runner_protocol_version, RUNNER_PROTOCOL_VERSION);
    assert_eq!(target.runner_runtime, "/private/runners/run-one");
    assert_eq!(target.last_committed_runner_sequence, 0);
    assert_eq!(target.provider_session_id, None);
    assert!(!target.resumes_provider_session);

    let head = store.latest_event_sequence().unwrap();
    let error = store
        .reserve_task_run(
            reservation("project-one", "task-two", "worker-two", "run-two", None),
            1,
            7,
        )
        .err()
        .unwrap();
    assert!(matches!(error, StoreError::CapacityReached { limit: 1 }));
    assert_eq!(store.latest_event_sequence().unwrap(), head);
    let tasks = store.list_tasks(&id("project-one"), None, 10).unwrap();
    let second = tasks
        .iter()
        .find(|task| task.snapshot.id == id::<TaskId>("task-two"))
        .unwrap();
    assert_eq!(second.snapshot.status, TaskStatus::Queued);
    assert_eq!(second.snapshot.assigned_agent_id, None);

    let persisted_events = serde_json::to_string(&store.events_after(0, 100).unwrap()).unwrap();
    assert!(!persisted_events.contains("private instructions"));
    assert!(!persisted_events.contains("/private/runners"));
    assert!(!persisted_events.contains("instance-run"));
}

#[test]
fn runner_ingestion_requires_exact_identity_and_contiguous_sequences() {
    let mut store = Store::open_in_memory().unwrap();
    create_project_and_task(&mut store, "project", "task", 1);
    create_worker(&mut store, "project", "worker", None, 3);
    let mut input = reservation("project", "task", "worker", "run", None);
    input.fresh_provider_session_id = Some("expected-session".into());
    let instance = input.runner_instance_id.clone();
    store.reserve_task_run(input, 1, 4).unwrap();
    let head = store.latest_event_sequence().unwrap();

    let started = runner_event(1, RunnerEvent::Started { child_pid: 99 });
    let mut wrong_protocol = started.clone();
    wrong_protocol.protocol_version += 1;
    assert!(matches!(
        store.ingest_runner_event(&id("run"), &instance, &wrong_protocol, no_effects(), 5,),
        Err(StoreError::RunnerProtocolMismatch { .. })
    ));
    assert!(matches!(
        store.ingest_runner_event(
            &id("run"),
            &instance,
            &runner_event(0, RunnerEvent::Started { child_pid: 99 }),
            no_effects(),
            5,
        ),
        Err(StoreError::InvalidRunnerSequence(0))
    ));
    assert!(matches!(
        store.ingest_runner_event(&id("missing-run"), &instance, &started, no_effects(), 5,),
        Err(StoreError::RunNotFound)
    ));
    let wrong_identity =
        store.ingest_runner_event(&id("run"), &id("wrong-instance"), &started, no_effects(), 5);
    assert!(matches!(
        wrong_identity,
        Err(StoreError::RunnerIdentityMismatch)
    ));
    let gap = store.ingest_runner_event(
        &id("run"),
        &instance,
        &runner_event(
            2,
            RunnerEvent::Output {
                stream: OutputStream::Stdout,
                text: "never persisted".into(),
                lossy: false,
            },
        ),
        no_effects(),
        5,
    );
    assert!(matches!(
        gap,
        Err(StoreError::RunnerSequenceGap {
            expected: 1,
            found: 2
        })
    ));
    assert_eq!(store.latest_event_sequence().unwrap(), head);

    let applied = store
        .ingest_runner_event(&id("run"), &instance, &started, no_effects(), 6)
        .unwrap();
    assert_eq!(applied.disposition, IngestDisposition::Recorded);
    assert_eq!(applied.events.len(), 1);
    assert!(matches!(
        &applied.events[0].event,
        FactoryEvent::RunChanged { run } if run.status == RunStatus::Running
    ));

    let duplicate = store
        .ingest_runner_event(&id("run"), &instance, &started, no_effects(), 999)
        .unwrap();
    assert_eq!(duplicate.disposition, IngestDisposition::Duplicate);
    assert!(duplicate.events.is_empty());
    assert_eq!(store.latest_event_sequence().unwrap(), head + 1);

    let conflict = store.ingest_runner_event(
        &id("run"),
        &instance,
        &runner_event(
            2,
            RunnerEvent::Output {
                stream: OutputStream::Stdout,
                text: "provider payload".into(),
                lossy: false,
            },
        ),
        RunnerEventEffects {
            confirmed_provider_session_id: Some("different-session".into()),
            terminal_outcome: None,
        },
        7,
    );
    assert!(matches!(conflict, Err(StoreError::ProviderSessionConflict)));
    assert_eq!(store.latest_event_sequence().unwrap(), head + 1);
    let confirmed = store
        .ingest_runner_event(
            &id("run"),
            &instance,
            &runner_event(
                2,
                RunnerEvent::Output {
                    stream: OutputStream::Stdout,
                    text: "provider session init".into(),
                    lossy: false,
                },
            ),
            RunnerEventEffects {
                confirmed_provider_session_id: Some("expected-session".into()),
                terminal_outcome: None,
            },
            7,
        )
        .unwrap();
    assert_eq!(confirmed.disposition, IngestDisposition::Recorded);
    assert!(confirmed.events.is_empty());
}

#[test]
fn terminal_frame_outcome_and_public_transitions_commit_atomically() {
    let mut store = Store::open_in_memory().unwrap();
    create_project_and_task(&mut store, "project", "task", 1);
    create_worker(&mut store, "project", "worker", None, 3);
    let mut input = reservation("project", "task", "worker", "run", None);
    input.fresh_provider_session_id = Some("session".into());
    let instance = input.runner_instance_id.clone();
    store.reserve_task_run(input, 1, 4).unwrap();
    store
        .ingest_runner_event(
            &id("run"),
            &instance,
            &runner_event(1, RunnerEvent::Started { child_pid: 99 }),
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
                    text: "authenticated init".into(),
                    lossy: false,
                },
            ),
            RunnerEventEffects {
                confirmed_provider_session_id: Some("session".into()),
                terminal_outcome: None,
            },
            6,
        )
        .unwrap();

    let terminal = runner_event(
        3,
        RunnerEvent::Exited {
            exit_code: Some(0),
            signal: None,
        },
    );
    let head = store.latest_event_sequence().unwrap();
    let missing_outcome =
        store.ingest_runner_event(&id("run"), &instance, &terminal, no_effects(), 7);
    assert!(matches!(
        missing_outcome,
        Err(StoreError::TerminalOutcomeRequired)
    ));
    assert_eq!(store.latest_event_sequence().unwrap(), head);

    let completed = store
        .ingest_runner_event(
            &id("run"),
            &instance,
            &terminal,
            RunnerEventEffects {
                confirmed_provider_session_id: None,
                terminal_outcome: Some(TerminalOutcome::Succeeded),
            },
            8,
        )
        .unwrap();
    assert_eq!(completed.disposition, IngestDisposition::Recorded);
    assert_eq!(completed.events.len(), 3);
    assert!(completed.events.iter().any(|event| matches!(
        &event.event,
        FactoryEvent::TaskChanged { task } if task.status == TaskStatus::Succeeded
    )));

    let terminal_head = store.latest_event_sequence().unwrap();
    let terminal_retry = store
        .ingest_runner_event(
            &id("run"),
            &instance,
            &terminal,
            RunnerEventEffects {
                confirmed_provider_session_id: None,
                terminal_outcome: Some(TerminalOutcome::Succeeded),
            },
            999,
        )
        .unwrap();
    assert_eq!(terminal_retry.disposition, IngestDisposition::Duplicate);
    assert!(terminal_retry.events.is_empty());
    assert_eq!(store.latest_event_sequence().unwrap(), terminal_head);
    assert!(matches!(
        store.ingest_runner_event(
            &id("run"),
            &instance,
            &terminal,
            RunnerEventEffects {
                confirmed_provider_session_id: None,
                terminal_outcome: Some(TerminalOutcome::Failed(RunFailureReason::Provider)),
            },
            999,
        ),
        Err(StoreError::InvalidTerminalOutcome)
    ));
    assert!(matches!(
        store.ingest_runner_event(
            &id("run"),
            &instance,
            &terminal,
            RunnerEventEffects {
                confirmed_provider_session_id: Some("other-session".into()),
                terminal_outcome: Some(TerminalOutcome::Succeeded),
            },
            999,
        ),
        Err(StoreError::ProviderSessionConflict)
    ));
    assert!(matches!(
        store.ingest_runner_event(
            &id("run"),
            &instance,
            &runner_event(
                4,
                RunnerEvent::Output {
                    stream: OutputStream::Stdout,
                    text: "after terminal".into(),
                    lossy: false,
                },
            ),
            no_effects(),
            999,
        ),
        Err(StoreError::RunnerAlreadyTerminal)
    ));
    assert!(completed.events.iter().any(|event| matches!(
        &event.event,
        FactoryEvent::AgentChanged { agent } if agent.current_run_id.is_none()
    )));
    assert!(completed.events.iter().any(|event| matches!(
        &event.event,
        FactoryEvent::RunChanged { run }
            if run.status == RunStatus::Succeeded
                && run.exit_code == Some(0)
                && run.exit_signal.is_none()
                && run.failure_reason.is_none()
    )));

    let recoverable = store.recoverable_runs().unwrap();
    assert_eq!(recoverable.len(), 1);
    assert_eq!(recoverable[0].target.last_committed_runner_sequence, 3);
    assert_eq!(recoverable[0].terminal_runner_sequence, Some(3));
    assert_eq!(recoverable[0].runner_reconciled_at_ms, None);

    let wrong_ack = store.mark_runner_terminal_reconciled(&id("run"), &instance, 1, 9);
    assert!(matches!(
        wrong_ack,
        Err(StoreError::TerminalSequenceMismatch {
            expected: 3,
            found: 1
        })
    ));
    assert_eq!(
        store
            .mark_runner_terminal_reconciled(&id("run"), &instance, 3, 10)
            .unwrap(),
        WriteDisposition::Applied
    );
    assert_eq!(
        store
            .mark_runner_terminal_reconciled(&id("run"), &instance, 3, 99)
            .unwrap(),
        WriteDisposition::Duplicate
    );
    assert!(store.recoverable_runs().unwrap().is_empty());
}

#[test]
fn success_is_rejected_for_a_nonzero_or_signalled_process() {
    for (run, exit_code, signal) in [
        ("run-nonzero", Some(9), None),
        ("run-signal", None, Some(15)),
    ] {
        let mut store = Store::open_in_memory().unwrap();
        create_project_and_task(&mut store, "project", "task", 1);
        create_worker(&mut store, "project", "worker", None, 3);
        let input = reservation("project", "task", "worker", run, None);
        let instance = input.runner_instance_id.clone();
        store.reserve_task_run(input, 1, 4).unwrap();
        store
            .ingest_runner_event(
                &id(run),
                &instance,
                &runner_event(1, RunnerEvent::Started { child_pid: 8 }),
                no_effects(),
                5,
            )
            .unwrap();
        store
            .ingest_runner_event(
                &id(run),
                &instance,
                &runner_event(
                    2,
                    RunnerEvent::Output {
                        stream: OutputStream::Stdout,
                        text: "authenticated init".into(),
                        lossy: false,
                    },
                ),
                RunnerEventEffects {
                    confirmed_provider_session_id: Some("session".into()),
                    terminal_outcome: None,
                },
                6,
            )
            .unwrap();
        let error = store.ingest_runner_event(
            &id(run),
            &instance,
            &runner_event(3, RunnerEvent::Exited { exit_code, signal }),
            RunnerEventEffects {
                confirmed_provider_session_id: None,
                terminal_outcome: Some(TerminalOutcome::Succeeded),
            },
            7,
        );
        assert!(matches!(error, Err(StoreError::InvalidTerminalOutcome)));
        let recovery = store.recoverable_runs().unwrap();
        assert_eq!(recovery[0].target.last_committed_runner_sequence, 2);
        assert_eq!(recovery[0].terminal_runner_sequence, None);
        assert_eq!(
            store.list_tasks(&id("project"), None, 10).unwrap()[0]
                .snapshot
                .status,
            TaskStatus::Running
        );
    }
}

#[test]
fn launch_failure_is_terminal_atomic_and_not_recoverable() {
    let mut store = Store::open_in_memory().unwrap();
    create_project_and_task(&mut store, "project", "task", 1);
    create_worker(&mut store, "project", "worker", None, 3);
    let input = reservation("project", "task", "worker", "run", None);
    let instance = input.runner_instance_id.clone();
    store.reserve_task_run(input, 1, 4).unwrap();
    let head = store.latest_event_sequence().unwrap();

    let wrong = store.fail_run_launch(&id("run"), &id("wrong-instance"), 5);
    assert!(matches!(wrong, Err(StoreError::RunnerIdentityMismatch)));
    assert_eq!(store.latest_event_sequence().unwrap(), head);

    let failed = store.fail_run_launch(&id("run"), &instance, 6).unwrap();
    assert_eq!(failed.disposition, WriteDisposition::Applied);
    assert_eq!(failed.events.len(), 3);
    assert_eq!(failed.run.status, RunStatus::Failed);
    assert_eq!(failed.run.failure_reason, Some(RunFailureReason::Spawn));
    assert_eq!(failed.task.status, TaskStatus::Failed);
    assert!(failed.agent.current_run_id.is_none());
    assert!(store.recoverable_runs().unwrap().is_empty());
    let retry_head = store.latest_event_sequence().unwrap();
    let retried = store.fail_run_launch(&id("run"), &instance, 999).unwrap();
    assert_eq!(retried.disposition, WriteDisposition::Duplicate);
    assert!(retried.events.is_empty());
    assert_eq!(retried.run.updated_at_ms, 6);
    assert_eq!(store.latest_event_sequence().unwrap(), retry_head);
}

#[test]
fn terminal_kind_and_effects_are_validated_before_any_cursor_change() {
    let mut store = Store::open_in_memory().unwrap();
    create_project_and_task(&mut store, "project", "task", 1);
    create_worker(&mut store, "project", "worker", None, 3);
    let input = reservation("project", "task", "worker", "run", None);
    let instance = input.runner_instance_id.clone();
    store.reserve_task_run(input, 1, 4).unwrap();
    let head = store.latest_event_sequence().unwrap();

    let nonterminal_with_outcome = store.ingest_runner_event(
        &id("run"),
        &instance,
        &runner_event(1, RunnerEvent::Started { child_pid: 9 }),
        RunnerEventEffects {
            confirmed_provider_session_id: None,
            terminal_outcome: Some(TerminalOutcome::Failed(RunFailureReason::Provider)),
        },
        5,
    );
    assert!(matches!(
        nonterminal_with_outcome,
        Err(StoreError::UnexpectedTerminalOutcome)
    ));
    assert_eq!(store.latest_event_sequence().unwrap(), head);

    let spawn_succeeded = store.ingest_runner_event(
        &id("run"),
        &instance,
        &runner_event(
            1,
            RunnerEvent::SpawnFailed {
                message: "must stay transient".into(),
            },
        ),
        RunnerEventEffects {
            confirmed_provider_session_id: None,
            terminal_outcome: Some(TerminalOutcome::Succeeded),
        },
        5,
    );
    assert!(matches!(
        spawn_succeeded,
        Err(StoreError::InvalidTerminalOutcome)
    ));
    assert_eq!(store.latest_event_sequence().unwrap(), head);

    let spawned = store
        .ingest_runner_event(
            &id("run"),
            &instance,
            &runner_event(
                1,
                RunnerEvent::SpawnFailed {
                    message: "must stay transient".into(),
                },
            ),
            RunnerEventEffects {
                confirmed_provider_session_id: None,
                terminal_outcome: Some(TerminalOutcome::Failed(RunFailureReason::Spawn)),
            },
            6,
        )
        .unwrap();
    assert_eq!(spawned.disposition, IngestDisposition::Recorded);
    assert!(spawned.events.iter().any(|event| matches!(
        &event.event,
        FactoryEvent::RunChanged { run }
            if run.status == RunStatus::Failed
                && run.failure_reason == Some(RunFailureReason::Spawn)
    )));
}

#[test]
fn signalled_process_failure_round_trips_without_raw_output() {
    let mut store = Store::open_in_memory().unwrap();
    create_project_and_task(&mut store, "project", "task", 1);
    create_worker(&mut store, "project", "worker", None, 3);
    let input = reservation("project", "task", "worker", "run", None);
    let instance = input.runner_instance_id.clone();
    store.reserve_task_run(input, 1, 4).unwrap();
    store
        .ingest_runner_event(
            &id("run"),
            &instance,
            &runner_event(1, RunnerEvent::Started { child_pid: 8 }),
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
                RunnerEvent::Exited {
                    exit_code: None,
                    signal: Some(15),
                },
            ),
            RunnerEventEffects {
                confirmed_provider_session_id: None,
                terminal_outcome: Some(TerminalOutcome::Failed(RunFailureReason::Process)),
            },
            6,
        )
        .unwrap();

    let recovery = store.recoverable_runs().unwrap();
    assert_eq!(recovery[0].run.exit_code, None);
    assert_eq!(recovery[0].run.exit_signal, Some(15));
    assert_eq!(
        recovery[0].run.failure_reason,
        Some(RunFailureReason::Process)
    );
}

#[test]
fn recovery_survives_reopen_with_only_private_bounded_metadata() {
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("factory.db");
    let instance: RunnerInstanceId;
    {
        let mut store = Store::open(&database).unwrap();
        create_project_and_task(&mut store, "project", "task", 1);
        create_worker(&mut store, "project", "worker", None, 3);
        let mut input = reservation("project", "task", "worker", "run", None);
        input.fresh_provider_session_id = Some("private-session-id".into());
        instance = input.runner_instance_id.clone();
        store.reserve_task_run(input, 1, 4).unwrap();
        store
            .ingest_runner_event(
                &id("run"),
                &instance,
                &runner_event(1, RunnerEvent::Started { child_pid: 1234 }),
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
                        text: "raw secret output must not enter sqlite".into(),
                        lossy: false,
                    },
                ),
                RunnerEventEffects {
                    confirmed_provider_session_id: Some("private-session-id".into()),
                    terminal_outcome: None,
                },
                6,
            )
            .unwrap();
    }

    let mut store = Store::open(&database).unwrap();
    let recoverable = store.recoverable_runs().unwrap();
    assert_eq!(recoverable.len(), 1);
    assert_eq!(recoverable[0].run.id, id::<RunId>("run"));
    assert_eq!(recoverable[0].run.status, RunStatus::Running);
    assert_eq!(recoverable[0].target.provider, Provider::Codex);
    assert_eq!(
        recoverable[0].target.provider_session_id.as_deref(),
        Some("private-session-id")
    );
    assert!(!recoverable[0].target.resumes_provider_session);
    assert!(recoverable[0].provider_session_confirmed_at_ms.is_some());
    assert_eq!(recoverable[0].target.runner_instance_id, instance);
    assert_eq!(
        recoverable[0].target.runner_protocol_version,
        RUNNER_PROTOCOL_VERSION
    );
    assert_eq!(recoverable[0].target.runner_runtime, "/private/runners/run");
    assert_eq!(recoverable[0].target.last_committed_runner_sequence, 2);
    assert_eq!(recoverable[0].terminal_runner_sequence, None);

    let events = serde_json::to_string(&store.events_after(0, 100).unwrap()).unwrap();
    assert!(!events.contains("private-session-id"));
    assert!(!events.contains("raw secret"));

    store
        .ingest_runner_event(
            &id("run"),
            &instance,
            &runner_event(
                3,
                RunnerEvent::Exited {
                    exit_code: Some(0),
                    signal: None,
                },
            ),
            RunnerEventEffects {
                confirmed_provider_session_id: None,
                terminal_outcome: Some(TerminalOutcome::Succeeded),
            },
            7,
        )
        .unwrap();
    store
        .mark_runner_terminal_reconciled(&id("run"), &instance, 3, 8)
        .unwrap();
    store
        .create_task(
            NewTask {
                id: id("task-two"),
                project_id: id("project"),
                parent_task_id: None,
                title: "task-two".into(),
                body: "next turn".into(),
                priority: 0,
            },
            9,
        )
        .unwrap();
    let resumed = store
        .reserve_task_run(
            reservation("project", "task-two", "worker", "run-two", None),
            1,
            10,
        )
        .unwrap();
    assert!(resumed.target.resumes_provider_session);
    assert_eq!(
        resumed.target.provider_session_id.as_deref(),
        Some("private-session-id")
    );
    drop(store);

    let bytes = std::fs::read(&database).unwrap();
    let database_text = String::from_utf8_lossy(&bytes);
    assert!(!database_text.contains("raw secret output"));
}

#[test]
fn parent_agent_and_parent_run_lineage_cannot_cross_projects() {
    let mut store = Store::open_in_memory().unwrap();
    create_project_and_task(&mut store, "project-one", "task-one", 1);
    create_project_and_task(&mut store, "project-two", "task-two", 10);
    create_worker(&mut store, "project-one", "parent", None, 20);

    let cross_project_agent = store.create_agent(
        NewAgent {
            id: id("cross-project-child"),
            project_id: id("project-two"),
            parent_agent_id: Some(id("parent")),
            role: AgentRole::Worker,
            provider: Provider::Codex,
        },
        21,
    );
    assert!(cross_project_agent.is_err());
    let self_parent = store.create_agent(
        NewAgent {
            id: id("self-parent"),
            project_id: id("project-one"),
            parent_agent_id: Some(id("self-parent")),
            role: AgentRole::Worker,
            provider: Provider::Codex,
        },
        21,
    );
    assert!(self_parent.is_err());

    create_worker(&mut store, "project-one", "child", Some("parent"), 22);
    let durable_child_without_parent_attempt = store
        .reserve_task_run(
            reservation("project-one", "task-one", "child", "child-run", None),
            2,
            23,
        )
        .unwrap();
    assert_eq!(durable_child_without_parent_attempt.run.parent_run_id, None);
    let child_instance = durable_child_without_parent_attempt
        .target
        .runner_instance_id
        .clone();
    store
        .fail_run_launch(&id("child-run"), &child_instance, 24)
        .unwrap();

    create_worker(&mut store, "project-two", "worker-two", None, 25);
    let parent_input = reservation(
        "project-two",
        "task-two",
        "worker-two",
        "parent-run-two",
        None,
    );
    let parent_instance = parent_input.runner_instance_id.clone();
    store.reserve_task_run(parent_input, 2, 26).unwrap();
    store
        .ingest_runner_event(
            &id("parent-run-two"),
            &parent_instance,
            &runner_event(1, RunnerEvent::Started { child_pid: 8 }),
            no_effects(),
            27,
        )
        .unwrap();
    store
        .ingest_runner_event(
            &id("parent-run-two"),
            &parent_instance,
            &runner_event(
                2,
                RunnerEvent::Exited {
                    exit_code: Some(1),
                    signal: None,
                },
            ),
            RunnerEventEffects {
                confirmed_provider_session_id: None,
                terminal_outcome: Some(TerminalOutcome::Failed(RunFailureReason::Process)),
            },
            28,
        )
        .unwrap();

    store
        .create_task(
            NewTask {
                id: id("task-three"),
                project_id: id("project-one"),
                parent_task_id: None,
                title: "task-three".into(),
                body: "fresh lineage test".into(),
                priority: 0,
            },
            29,
        )
        .unwrap();

    let cross_project_run = store.reserve_task_run(
        reservation(
            "project-one",
            "task-three",
            "child",
            "child-run-two",
            Some("parent-run-two"),
        ),
        2,
        30,
    );
    assert!(matches!(
        cross_project_run,
        Err(StoreError::ParentRunLineageMismatch)
    ));
}

#[test]
fn event_replay_rejects_a_deleted_sequence_and_run_events_are_project_scoped() {
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("factory.db");
    {
        let mut store = Store::open(&database).unwrap();
        create_project_and_task(&mut store, "project", "task", 1);
        create_worker(&mut store, "project", "worker", None, 3);
        store
            .reserve_task_run(reservation("project", "task", "worker", "run", None), 1, 4)
            .unwrap();
    }
    let connection = rusqlite::Connection::open(&database).unwrap();
    let run_project: String = connection
        .query_row(
            "SELECT project_id FROM events
             WHERE run_id = 'run' AND kind = 'run_changed'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(run_project, "project");
    connection
        .execute("DELETE FROM events WHERE id = 2", [])
        .unwrap();
    drop(connection);

    let store = Store::open(&database).unwrap();
    assert!(store.events_after(i64::MAX, 1).unwrap().is_empty());
    assert!(matches!(
        store.events_after(0, 100),
        Err(StoreError::EventSequenceGap {
            expected: 2,
            found: 3
        })
    ));
}

#[test]
fn two_store_connections_cannot_claim_the_same_task() {
    use std::sync::{Arc, Barrier};

    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("factory.db");
    {
        let mut store = Store::open(&database).unwrap();
        create_project_and_task(&mut store, "project", "task", 1);
        create_worker(&mut store, "project", "worker-one", None, 3);
        create_worker(&mut store, "project", "worker-two", None, 4);
    }
    let before = Store::open(&database)
        .unwrap()
        .latest_event_sequence()
        .unwrap();
    let barrier = Arc::new(Barrier::new(2));
    let handles = [("worker-one", "run-one"), ("worker-two", "run-two")].map(|(agent, run)| {
        let database = database.clone();
        let barrier = Arc::clone(&barrier);
        std::thread::spawn(move || {
            let mut store = Store::open(database).unwrap();
            barrier.wait();
            match store.reserve_task_run(reservation("project", "task", agent, run, None), 2, 5) {
                Ok(_) => true,
                Err(StoreError::TaskNotQueued) => false,
                Err(error) => panic!("unexpected reservation result: {error}"),
            }
        })
    });
    let claimed = handles
        .into_iter()
        .map(|handle| handle.join().unwrap())
        .filter(|claimed| *claimed)
        .count();
    assert_eq!(claimed, 1);

    let store = Store::open(&database).unwrap();
    assert_eq!(store.latest_event_sequence().unwrap(), before + 3);
    let task = &store.list_tasks(&id("project"), None, 10).unwrap()[0].snapshot;
    assert_eq!(task.status, TaskStatus::Running);
    assert!(task.assigned_agent_id.is_some());
}

#[test]
fn reservation_collision_rolls_back_task_and_events() {
    let mut store = Store::open_in_memory().unwrap();
    create_project_and_task(&mut store, "project", "task-one", 1);
    create_worker(&mut store, "project", "worker", None, 3);
    let first = reservation("project", "task-one", "worker", "same-run", None);
    let instance = first.runner_instance_id.clone();
    store.reserve_task_run(first, 1, 4).unwrap();
    store
        .fail_run_launch(&id("same-run"), &instance, 5)
        .unwrap();
    store
        .create_task(
            NewTask {
                id: id("task-two"),
                project_id: id("project"),
                parent_task_id: None,
                title: "task-two".into(),
                body: "second task".into(),
                priority: 0,
            },
            6,
        )
        .unwrap();
    let head = store.latest_event_sequence().unwrap();
    assert!(
        store
            .reserve_task_run(
                reservation("project", "task-two", "worker", "same-run", None),
                1,
                7,
            )
            .is_err()
    );
    assert_eq!(store.latest_event_sequence().unwrap(), head);
    let task = store
        .list_tasks(&id("project"), None, 10)
        .unwrap()
        .into_iter()
        .find(|task| task.snapshot.id == id::<TaskId>("task-two"))
        .unwrap();
    assert_eq!(task.snapshot.status, TaskStatus::Queued);
    assert_eq!(task.snapshot.assigned_agent_id, None);
}

#[test]
fn runner_lifecycle_rejects_prestart_output_second_start_and_invented_session() {
    let mut store = Store::open_in_memory().unwrap();
    create_project_and_task(&mut store, "project", "task", 1);
    create_worker(&mut store, "project", "agent", None, 3);
    let input = reservation("project", "task", "agent", "run", None);
    let instance = input.runner_instance_id.clone();
    store.reserve_task_run(input, 1, 4).unwrap();

    let output = runner_event(
        1,
        RunnerEvent::Output {
            stream: OutputStream::Stdout,
            text: "not started".into(),
            lossy: false,
        },
    );
    assert!(matches!(
        store.ingest_runner_event(&id("run"), &instance, &output, no_effects(), 5),
        Err(StoreError::InvalidRunnerLifecycle)
    ));
    assert!(matches!(
        store.ingest_runner_event(
            &id("run"),
            &instance,
            &runner_event(1, RunnerEvent::Started { child_pid: 8 }),
            RunnerEventEffects {
                confirmed_provider_session_id: Some("invented".into()),
                terminal_outcome: None,
            },
            5,
        ),
        Err(StoreError::InvalidSessionConfirmation)
    ));
    store
        .ingest_runner_event(
            &id("run"),
            &instance,
            &runner_event(1, RunnerEvent::Started { child_pid: 8 }),
            no_effects(),
            6,
        )
        .unwrap();
    assert!(matches!(
        store.ingest_runner_event(
            &id("run"),
            &instance,
            &runner_event(2, RunnerEvent::Started { child_pid: 8 }),
            no_effects(),
            7,
        ),
        Err(StoreError::InvalidRunnerLifecycle)
    ));
    assert_eq!(
        store
            .execution_target(&id("run"))
            .unwrap()
            .last_committed_runner_sequence,
        1
    );
    assert!(matches!(
        store.fail_run_launch(&id("run"), &instance, 8),
        Err(StoreError::InvalidRunState)
    ));
}

#[test]
fn a_provider_session_has_only_one_agent_owner() {
    let mut store = Store::open_in_memory().unwrap();
    create_project_and_task(&mut store, "project", "task-one", 1);
    store
        .create_task(
            NewTask {
                id: id("task-two"),
                project_id: id("project"),
                parent_task_id: None,
                title: "task-two".into(),
                body: "second".into(),
                priority: 0,
            },
            3,
        )
        .unwrap();
    create_worker(&mut store, "project", "agent-one", None, 4);
    create_worker(&mut store, "project", "agent-two", None, 5);

    let first = reservation("project", "task-one", "agent-one", "run-one", None);
    let first_instance = first.runner_instance_id.clone();
    store.reserve_task_run(first, 2, 6).unwrap();
    store
        .ingest_runner_event(
            &id("run-one"),
            &first_instance,
            &runner_event(1, RunnerEvent::Started { child_pid: 8 }),
            no_effects(),
            7,
        )
        .unwrap();
    store
        .ingest_runner_event(
            &id("run-one"),
            &first_instance,
            &runner_event(
                2,
                RunnerEvent::Output {
                    stream: OutputStream::Stdout,
                    text: "authenticated".into(),
                    lossy: false,
                },
            ),
            RunnerEventEffects {
                confirmed_provider_session_id: Some("shared-session".into()),
                terminal_outcome: None,
            },
            8,
        )
        .unwrap();
    store
        .ingest_runner_event(
            &id("run-one"),
            &first_instance,
            &runner_event(
                3,
                RunnerEvent::Exited {
                    exit_code: Some(1),
                    signal: None,
                },
            ),
            RunnerEventEffects {
                confirmed_provider_session_id: None,
                terminal_outcome: Some(TerminalOutcome::Failed(RunFailureReason::Provider)),
            },
            9,
        )
        .unwrap();

    let second = reservation("project", "task-two", "agent-two", "run-two", None);
    let second_instance = second.runner_instance_id.clone();
    store.reserve_task_run(second, 2, 10).unwrap();
    store
        .ingest_runner_event(
            &id("run-two"),
            &second_instance,
            &runner_event(1, RunnerEvent::Started { child_pid: 9 }),
            no_effects(),
            11,
        )
        .unwrap();
    let head = store.latest_event_sequence().unwrap();
    assert!(matches!(
        store.ingest_runner_event(
            &id("run-two"),
            &second_instance,
            &runner_event(
                2,
                RunnerEvent::Output {
                    stream: OutputStream::Stdout,
                    text: "conflicting auth".into(),
                    lossy: false,
                },
            ),
            RunnerEventEffects {
                confirmed_provider_session_id: Some("shared-session".into()),
                terminal_outcome: None,
            },
            12,
        ),
        Err(StoreError::ProviderSessionConflict)
    ));
    assert_eq!(store.latest_event_sequence().unwrap(), head);
    assert_eq!(
        store
            .execution_target(&id("run-two"))
            .unwrap()
            .last_committed_runner_sequence,
        1
    );
}

#[test]
fn invalid_process_exit_shapes_do_not_advance_the_cursor() {
    for (name, exit_code, signal) in [
        ("neither", None, None),
        ("both", Some(1), Some(9)),
        ("negative-code", Some(-1), None),
        ("zero-signal", None, Some(0)),
        ("negative-signal", None, Some(-9)),
    ] {
        let mut store = Store::open_in_memory().unwrap();
        create_project_and_task(&mut store, "project", "task", 1);
        create_worker(&mut store, "project", "agent", None, 3);
        let run = format!("run-{name}");
        let input = reservation("project", "task", "agent", &run, None);
        let instance = input.runner_instance_id.clone();
        store.reserve_task_run(input, 1, 4).unwrap();
        store
            .ingest_runner_event(
                &id(&run),
                &instance,
                &runner_event(1, RunnerEvent::Started { child_pid: 8 }),
                no_effects(),
                5,
            )
            .unwrap();
        let head = store.latest_event_sequence().unwrap();
        assert!(matches!(
            store.ingest_runner_event(
                &id(&run),
                &instance,
                &runner_event(2, RunnerEvent::Exited { exit_code, signal }),
                RunnerEventEffects {
                    confirmed_provider_session_id: None,
                    terminal_outcome: Some(TerminalOutcome::Failed(RunFailureReason::Process)),
                },
                6,
            ),
            Err(StoreError::InvalidTerminalOutcome)
        ));
        assert_eq!(store.latest_event_sequence().unwrap(), head);
        let target = store.execution_target(&id(&run)).unwrap();
        assert_eq!(target.last_committed_runner_sequence, 1);
    }
}

#[test]
fn a_child_run_may_reference_its_parent_agents_same_project_run() {
    let mut store = Store::open_in_memory().unwrap();
    create_project_and_task(&mut store, "project", "parent-task", 1);
    store
        .create_task(
            NewTask {
                id: id("child-task"),
                project_id: id("project"),
                parent_task_id: Some(id("parent-task")),
                title: "child-task".into(),
                body: "child work".into(),
                priority: 0,
            },
            3,
        )
        .unwrap();
    create_worker(&mut store, "project", "parent-agent", None, 4);
    create_worker(
        &mut store,
        "project",
        "child-agent",
        Some("parent-agent"),
        5,
    );
    store
        .reserve_task_run(
            reservation("project", "parent-task", "parent-agent", "parent-run", None),
            2,
            6,
        )
        .unwrap();
    let child = store
        .reserve_task_run(
            reservation(
                "project",
                "child-task",
                "child-agent",
                "child-run",
                Some("parent-run"),
            ),
            2,
            7,
        )
        .unwrap();
    assert_eq!(child.run.parent_run_id, Some(id("parent-run")));
}
