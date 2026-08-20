//! Track 5A: sessions store, migration 0014, and the hook state machine.

use factory_core::{
    AgentId, AgentRole, FactoryEvent, MessageId, ProjectId, Provider, ProviderHookEvent,
    ProviderNotificationKind, RunId, RunnerInstanceId, SessionId, SessionState, TaskId, TaskStatus,
};
use factoryd::session_work::Phase as SessionWorkPhase;
use factoryd::store::{
    DeliveryAttemptState, NewAgent, NewAgentMessage, NewDeliveryAttempt, NewProject, NewSession,
    NewTask, ProviderDeliveryRecovery, Store, StoreError,
};

fn project_id(value: &str) -> ProjectId {
    ProjectId::try_from(value).unwrap()
}

fn agent_id(value: &str) -> AgentId {
    AgentId::try_from(value).unwrap()
}

fn task_id(value: &str) -> TaskId {
    TaskId::try_from(value).unwrap()
}

fn session_id(value: &str) -> SessionId {
    SessionId::try_from(value).unwrap()
}

fn new_session(seed: &str, project: &str, agent: &str) -> NewSession {
    NewSession {
        id: session_id(seed),
        project_id: project_id(project),
        agent_id: agent_id(agent),
        provider: Provider::Codex,
        runtime_model: None,
        runtime_reasoning_effort: None,
        runtime_permission_mode: None,
        runtime_control_mode: None,
        provider_session_id: None,
        worktree: format!("/work/{project}"),
        codex_home: None,
        hook_token: "a".repeat(64),
        runner_instance_id: RunnerInstanceId::try_from(format!("instance-{seed}")).unwrap(),
        runner_runtime: format!("/private/runners/{seed}"),
        runner_protocol_version: 1,
    }
}

fn rewind_session_work_migration(database: &std::path::Path) {
    let connection = rusqlite::Connection::open(database).unwrap();
    connection
        .execute_batch(
            "PRAGMA foreign_keys = OFF;
             DROP TABLE session_work;
             DROP INDEX delivery_attempts_session_work_identity;
             DROP INDEX IF EXISTS runs_one_open_per_session;
             ALTER TABLE delivery_attempts DROP COLUMN run_id;
             ALTER TABLE delivery_attempts DROP COLUMN task_revision;
             ALTER TABLE tasks DROP COLUMN work_revision;
             ALTER TABLE sessions DROP COLUMN principal_version;
             PRAGMA user_version = 28;
             PRAGMA foreign_keys = ON;",
        )
        .unwrap();
}

#[test]
fn runtime_metadata_is_retained_after_a_session_ends() {
    let mut store = fixture();
    let mut session = new_session("runtime-session", "factory", "curie");
    session.runtime_model = Some("gpt-5.6".into());
    session.runtime_reasoning_effort = Some("xhigh".into());
    session.runtime_permission_mode = Some("on-request".into());
    session.runtime_control_mode = Some("approval_policy=on-request".into());
    store.create_session(session, 5).unwrap();
    store
        .end_session(&session_id("runtime-session"), Some(0), None, 6)
        .unwrap();

    let sessions = store
        .list_sessions(&project_id("factory"), None, 10)
        .unwrap();
    let session = sessions
        .into_iter()
        .find(|session| session.id == session_id("runtime-session"))
        .unwrap();
    assert_eq!(session.runtime_model.as_deref(), Some("gpt-5.6"));
    assert_eq!(session.runtime_reasoning_effort.as_deref(), Some("xhigh"));
    assert_eq!(
        session.runtime_permission_mode.as_deref(),
        Some("on-request")
    );
    assert_eq!(
        session.runtime_control_mode.as_deref(),
        Some("approval_policy=on-request")
    );
}

fn fixture() -> Store {
    let mut store = Store::open_in_memory().unwrap();
    store
        .create_project(
            NewProject {
                id: project_id("factory"),
                name: "Factory".into(),
                root: "/work/factory".into(),
            },
            1,
        )
        .unwrap();
    store
        .create_agent(
            NewAgent {
                id: agent_id("curie"),
                project_id: project_id("factory"),
                parent_agent_id: None,
                role: AgentRole::Worker,
                provider: Provider::Codex,
            },
            2,
        )
        .unwrap();
    store
        .create_task(
            NewTask {
                id: task_id("task-1"),
                project_id: project_id("factory"),
                parent_task_id: None,
                title: "Task".into(),
                body: "Do the work".into(),
                priority: 0,
            },
            3,
        )
        .unwrap();
    store
        .assign_task(
            &project_id("factory"),
            &task_id("task-1"),
            Some(&agent_id("curie")),
            4,
        )
        .unwrap();
    store
}

#[test]
fn session_work_cas_fences_every_successor_until_exact_completion() {
    let mut store = fixture();
    let (session, _) = store
        .create_session(new_session("authority-session", "factory", "curie"), 5)
        .unwrap();
    store
        .record_hook_event(
            &session.id,
            ProviderHookEvent::SessionStart,
            None,
            false,
            None,
            6,
        )
        .unwrap();
    let marker = store.task_delivery_marker(&task_id("task-1")).unwrap();
    let first = NewDeliveryAttempt {
        id: "authority-attempt".into(),
        project_id: project_id("factory"),
        agent_id: agent_id("curie"),
        session_id: session.id.clone(),
        task_id: Some(task_id("task-1")),
        task_incarnation_id: Some(marker.incarnation_id.clone()),
        task_revision: Some(marker.task_revision),
        require_queue_head: false,
        message_ids: Vec::new(),
        text: "exact authority prompt".into(),
        created_at_ms: 7,
    };
    let reserved = store.ensure_delivery_attempt(first.clone()).unwrap();
    let work = store.session_work(&session.id).unwrap().work.unwrap();
    assert_eq!(work.revision, 1);
    assert!(matches!(
        work.phase,
        SessionWorkPhase::Delivering(ref lease) if lease.attempt_id == first.id
    ));

    let mut successor = first.clone();
    successor.id = "successor-attempt".into();
    let retained = store.ensure_delivery_attempt(successor).unwrap();
    assert_eq!(retained.id, first.id);
    assert_eq!(
        store
            .session_work(&session.id)
            .unwrap()
            .work
            .unwrap()
            .revision,
        1,
        "a recomposed successor must retain the exact durable reservation"
    );
    assert!(
        store
            .begin_delivery_attempt("wrong-attempt", 8)
            .unwrap()
            .is_none()
    );
    assert_eq!(
        store
            .session_work(&session.id)
            .unwrap()
            .work
            .unwrap()
            .revision,
        1
    );

    store
        .begin_delivery_attempt(&reserved.id, 9)
        .unwrap()
        .unwrap();
    assert!(matches!(
        store.session_work(&session.id).unwrap().work.unwrap().phase,
        SessionWorkPhase::Uncertain(ref lease) if lease.attempt_id == first.id
    ));
    assert!(
        store
            .begin_delivery_attempt(&reserved.id, 10)
            .unwrap()
            .is_none()
    );
    assert!(matches!(
        store.reset_delivery_attempt(&project_id("factory"), &agent_id("curie"), 11),
        Err(StoreError::SessionWorkConflict)
    ));

    let opened = store
        .open_run_episode_with_delivery_attempt(
            &session.id,
            &task_id("task-1"),
            Some(&[]),
            Some(&reserved.id),
            12,
        )
        .unwrap();
    assert_eq!(Some(opened.run.id.clone()), reserved.run_id);
    assert!(matches!(
        store.session_work(&session.id).unwrap().work.unwrap().phase,
        SessionWorkPhase::Running(ref lease)
            if lease.task.as_ref().map(|task| &task.run_id) == Some(&opened.run.id)
    ));
    store
        .complete_task(
            &project_id("factory"),
            &task_id("task-1"),
            &session.id,
            "done".into(),
            13,
        )
        .unwrap();
    let empty = store.session_work(&session.id).unwrap().work.unwrap();
    assert_eq!(empty.revision, 4);
    assert!(matches!(empty.phase, SessionWorkPhase::Empty));
}

#[test]
fn session_lifecycle_updates_and_projects_the_live_agent_relation() {
    let mut store = fixture();
    let (created, create_events) = store
        .create_session(new_session("lifecycle-session", "factory", "curie"), 5)
        .unwrap();
    assert!(matches!(
        create_events.as_slice(),
        [
            factory_core::EventEnvelope {
                event: FactoryEvent::AgentChanged { agent },
                ..
            },
            factory_core::EventEnvelope {
                event: FactoryEvent::SessionChanged { session },
                ..
            }
        ] if session.id == created.id
            && agent.current_session_id == Some(created.id.clone())
    ));
    assert_eq!(
        store.list_agents(&project_id("factory"), None, 10).unwrap()[0].current_session_id,
        Some(created.id.clone())
    );

    let (_, end_events) = store.end_session(&created.id, Some(0), None, 6).unwrap();
    assert!(end_events.iter().any(|event| matches!(
        &event.event,
        FactoryEvent::AgentChanged { agent }
            if agent.id == agent_id("curie") && agent.current_session_id.is_none()
    )));
    assert_eq!(
        store.list_agents(&project_id("factory"), None, 10).unwrap()[0].current_session_id,
        None
    );
}

#[test]
fn acknowledged_delivery_wins_the_atomic_recovery_fence() {
    let mut store = fixture();
    let thread = "9b1deb4d-3b7d-4bad-9bdd-2b0d7b3dcb6d";
    let mut initial = new_session("ack-initial", "factory", "curie");
    initial.provider_session_id = Some(thread.into());
    let (initial, _) = store.create_session(initial, 5).unwrap();
    store.end_session(&initial.id, Some(0), None, 6).unwrap();
    let mut resumed = new_session("ack-resumed", "factory", "curie");
    resumed.provider_session_id = Some(thread.into());
    let (resumed, _) = store.create_session(resumed, 7).unwrap();
    let task = task_id("ack-task");
    store
        .create_task(
            NewTask {
                id: task.clone(),
                project_id: project_id("factory"),
                parent_task_id: None,
                title: "Ack first".into(),
                body: String::new(),
                priority: 0,
            },
            8,
        )
        .unwrap();
    store
        .assign_task(&project_id("factory"), &task, Some(&agent_id("curie")), 8)
        .unwrap();
    let marker = store.task_delivery_marker(&task).unwrap();
    store
        .ensure_delivery_attempt(NewDeliveryAttempt {
            id: "ack-attempt".into(),
            project_id: project_id("factory"),
            agent_id: agent_id("curie"),
            session_id: resumed.id.clone(),
            task_id: Some(task.clone()),
            task_incarnation_id: Some(marker.incarnation_id),
            task_revision: Some(marker.task_revision),
            require_queue_head: false,
            message_ids: Vec::new(),
            text: "accepted prompt".into(),
            created_at_ms: 8,
        })
        .unwrap();
    store
        .open_run_episode_with_delivery_attempt(
            &resumed.id,
            &task,
            Some(&[]),
            Some("ack-attempt"),
            9,
        )
        .unwrap();

    assert!(matches!(
        store
            .request_fresh_provider_recovery(
                &project_id("factory"),
                &resumed.id,
                "ack-attempt",
                10,
            )
            .unwrap(),
        (ProviderDeliveryRecovery::Acknowledged, None)
    ));
    let live = store
        .live_session_for_agent(&project_id("factory"), &agent_id("curie"))
        .unwrap()
        .unwrap();
    assert_eq!(live.stop_requested_at_ms, None);
    assert_eq!(live.delivery_recovery_stop_requested_at_ms, None);
    assert_eq!(
        store
            .get_task(&project_id("factory"), &task)
            .unwrap()
            .snapshot
            .status,
        TaskStatus::Running,
        "recovery must not cancel an admitted prompt or its run"
    );
}

// --- Migration -------------------------------------------------------

/// Builds a raw pre-0014 database (schema 13, the pre-sessions shape) with
/// one legacy *open* run, then opens it through the real `Store::open` --
/// which always migrates to the current `SCHEMA_VERSION`, 30 after the
/// connector-event migration, runtime metadata, legacy permission repair,
/// model policy, delivery attempts, provider resume recovery, observer
/// reason, typed notification cause, and the widened Claude notification
/// constraint
/// (0015 widened `last_hook_event` for `permission_request`) -- and
/// asserts: the legacy open run is force-closed by 0014 (not left
/// dangling), and `PRAGMA foreign_key_check` is clean after the full
/// chain including 0015's `sessions` rebuild, 0016's task incarnations, and
/// 0021's historical runtime metadata columns and 0022's legacy permission
/// repair.
#[test]
fn migration_0014_force_closes_a_legacy_open_run_and_reaches_current_schema() {
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("legacy.db");
    {
        let connection = rusqlite::Connection::open(&database).unwrap();
        connection
            .execute_batch("PRAGMA foreign_keys = OFF; PRAGMA journal_mode = WAL;")
            .unwrap();
        for migration in [
            include_str!("../migrations/0001_state_and_events.sql"),
            include_str!("../migrations/0002_execution_ledger.sql"),
            include_str!("../migrations/0003_runner_reconciliation.sql"),
            include_str!("../migrations/0004_observer_health.sql"),
            include_str!("../migrations/0005_provider_session_context.sql"),
            include_str!("../migrations/0006_webhooks.sql"),
            include_str!("../migrations/0007_subscription_usage.sql"),
            include_str!("../migrations/0008_subscription_windows.sql"),
            include_str!("../migrations/0009_agent_profiles.sql"),
            include_str!("../migrations/0010_agent_messages.sql"),
            include_str!("../migrations/0011_run_stop_intent.sql"),
            include_str!("../migrations/0012_drop_subscription_usage_and_task_dependencies.sql"),
            include_str!("../migrations/0013_agent_profile_files.sql"),
        ] {
            connection.execute_batch(migration).unwrap();
        }

        connection
            .execute(
                "INSERT INTO projects (id, name, root, created_at_ms, updated_at_ms)
                 VALUES ('factory', 'Factory', '/work/factory', 1, 1)",
                [],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO agents (
                    id, project_id, parent_agent_id, role, provider, created_at_ms, updated_at_ms
                 ) VALUES ('curie', 'factory', NULL, 'worker', 'codex', 2, 2)",
                [],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO agent_profiles (agent_id, model, permission_mode, updated_at_ms)
                 VALUES ('curie', NULL, NULL, 2)",
                [],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO tasks (
                    id, project_id, parent_task_id, assigned_agent_id, title, body, status,
                    priority, created_at_ms, updated_at_ms, started_at_ms, completed_at_ms, result
                 ) VALUES (
                    'task-1', 'factory', NULL, 'curie', 'Legacy task', 'body', 'running',
                    0, 3, 3, 3, NULL, NULL
                 )",
                [],
            )
            .unwrap();
        // A legacy run still "open" (ended_at_ms IS NULL) from the dead
        // ephemeral-runner model -- exactly what 0014 must force-close.
        connection
            .execute(
                "INSERT INTO runs (
                    id, project_id, agent_id, parent_run_id, task_id, status, activity,
                    wait_reason, worktree, provider_session_id, resumes_provider_session,
                    provider_session_confirmed_at_ms, runner_instance_id,
                    runner_protocol_version, runner_runtime, last_runner_sequence,
                    terminal_runner_sequence, runner_reconciled_at_ms, runner_terminal_kind,
                    observer_health, observer_health_since_ms, started_at_ms, status_since_ms,
                    updated_at_ms, ended_at_ms, exit_code, exit_signal, failure_reason,
                    stop_requested_at_ms
                 ) VALUES (
                    'run-legacy', 'factory', 'curie', NULL, 'task-1', 'running', NULL, NULL,
                    '/work/factory', NULL, 0, NULL, 'instance-legacy', 1,
                    '/private/runners/run-legacy', 1, NULL, NULL, NULL, 'unknown', 4, 4, 4, 4,
                    NULL, NULL, NULL, NULL, NULL
                 )",
                [],
            )
            .unwrap();
        connection.pragma_update(None, "user_version", 13).unwrap();
    }

    // Opening through the real store runs migrations 0014 through 0030.
    let store = Store::open(&database).unwrap();

    let connection = rusqlite::Connection::open(&database).unwrap();
    let version: i64 = connection
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .unwrap();
    assert_eq!(version, 30);
    assert!(
        store.auto_mode().unwrap(),
        "pre-17 databases default auto mode on"
    );

    connection
        .execute_batch("PRAGMA foreign_keys = ON;")
        .unwrap();
    let violations: i64 = connection
        .query_row("SELECT COUNT(*) FROM pragma_foreign_key_check", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(
        violations, 0,
        "migration chain left a foreign key violation"
    );

    let run = store
        .list_runs(&project_id("factory"), None, 10)
        .unwrap()
        .into_iter()
        .find(|run| run.id == RunId::try_from("run-legacy").unwrap())
        .unwrap();
    assert!(
        run.status.is_terminal(),
        "legacy open run must be force-closed"
    );
    assert!(run.ended_at_ms.is_some());
    assert_eq!(run.closed_by, Some(factory_core::RunClosedBy::SessionEnded));
    assert!(
        run.session_id.is_none(),
        "a legacy run has no session to backfill"
    );

    // The rebuilt schema is otherwise usable: a fresh session can be
    // created and used normally.
    let mut store = store;
    store
        .create_session(new_session("run-2", "factory", "curie"), 5)
        .unwrap();
}

#[test]
fn migrations_0019_through_0030_follow_the_budget_schema_in_order() {
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("schema-18.db");
    drop(Store::open(&database).unwrap());
    {
        let connection = rusqlite::Connection::open(&database).unwrap();
        connection
            .execute_batch(
                "PRAGMA foreign_keys = OFF;
                 DROP TABLE session_work;
                 DROP INDEX delivery_attempts_session_work_identity;
                 DROP INDEX runs_one_open_per_session;
                 ALTER TABLE tasks DROP COLUMN work_revision;
                 ALTER TABLE sessions DROP COLUMN provider_resume_blocked_at_ms;
                 ALTER TABLE sessions DROP COLUMN resumed_provider_session;
                 ALTER TABLE sessions DROP COLUMN delivery_recovery_stop_requested_at_ms;
                 ALTER TABLE sessions DROP COLUMN principal_version;
                 DROP TABLE delivery_attempts;
                 DROP TABLE connector_events;
                 DROP TABLE project_repository_authority;
                 ALTER TABLE agent_profiles DROP COLUMN model_selection_reason;
                 ALTER TABLE agent_profiles DROP COLUMN reasoning_effort;
                 ALTER TABLE sessions DROP COLUMN observer_reason;
                 ALTER TABLE sessions DROP COLUMN notification_kind;
                 PRAGMA user_version = 18;
                 PRAGMA foreign_keys = ON;",
            )
            .unwrap();
    }

    drop(Store::open(&database).unwrap());
    let connection = rusqlite::Connection::open(&database).unwrap();
    let version: i64 = connection
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .unwrap();
    assert_eq!(version, 30);
    connection
        .prepare("SELECT remote_url, base_branch FROM project_repository_authority")
        .unwrap();
    connection
        .prepare("SELECT payload_digest, event_kind FROM connector_events")
        .unwrap();
    let violations: i64 = connection
        .query_row("SELECT COUNT(*) FROM pragma_foreign_key_check", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(violations, 0);
}

#[test]
fn migration_0030_marks_existing_sessions_legacy_and_new_sessions_authenticated() {
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("schema-29.db");
    {
        let mut store = Store::open(&database).unwrap();
        store
            .create_project(
                NewProject {
                    id: project_id("factory"),
                    name: "Factory".into(),
                    root: "/work/factory".into(),
                },
                1,
            )
            .unwrap();
        for (id, timestamp) in [("curie", 2), ("feynman", 3)] {
            store
                .create_agent(
                    NewAgent {
                        id: agent_id(id),
                        project_id: project_id("factory"),
                        parent_agent_id: None,
                        role: AgentRole::Worker,
                        provider: Provider::Codex,
                    },
                    timestamp,
                )
                .unwrap();
        }
        store
            .create_session(new_session("legacy-session", "factory", "curie"), 4)
            .unwrap();
    }
    {
        let connection = rusqlite::Connection::open(&database).unwrap();
        connection
            .execute_batch(
                "ALTER TABLE sessions DROP COLUMN principal_version;
                 PRAGMA user_version = 29;",
            )
            .unwrap();
    }

    let mut store = Store::open(&database).unwrap();
    let mut fresh = new_session("fresh-session", "factory", "feynman");
    fresh.hook_token = "b".repeat(64);
    store.create_session(fresh, 5).unwrap();
    drop(store);

    let connection = rusqlite::Connection::open(&database).unwrap();
    for (session, expected) in [("legacy-session", 0_i64), ("fresh-session", 1_i64)] {
        let version: i64 = connection
            .query_row(
                "SELECT principal_version FROM sessions WHERE id = ?1",
                [session],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            version, expected,
            "unexpected principal version for {session}"
        );
    }
}

#[test]
fn migration_0028_rebuilds_a_populated_session_graph_with_foreign_keys() {
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("populated-v26.db");
    {
        let mut store = Store::open(&database).unwrap();
        store
            .create_project(
                NewProject {
                    id: project_id("factory"),
                    name: "Factory".into(),
                    root: "/work/factory".into(),
                },
                1,
            )
            .unwrap();
        store
            .create_agent(
                NewAgent {
                    id: agent_id("curie"),
                    project_id: project_id("factory"),
                    parent_agent_id: None,
                    role: AgentRole::Worker,
                    provider: Provider::Codex,
                },
                2,
            )
            .unwrap();
        let (session, _) = store
            .create_session(new_session("migration-session", "factory", "curie"), 3)
            .unwrap();
        store
            .record_hook_event(
                &session.id,
                ProviderHookEvent::SessionStart,
                None,
                false,
                None,
                4,
            )
            .unwrap();
        let message_id = MessageId::try_from("migration-message").unwrap();
        store
            .send_agent_message(NewAgentMessage {
                id: message_id.clone(),
                project_id: project_id("factory"),
                sender_agent_id: None,
                recipient_agent_id: agent_id("curie"),
                body: "message retained across the rebuild".into(),
                created_at_ms: 5,
            })
            .unwrap();
        store
            .ensure_delivery_attempt(NewDeliveryAttempt {
                id: "migration-delivery".into(),
                project_id: project_id("factory"),
                agent_id: agent_id("curie"),
                session_id: session.id,
                task_id: None,
                task_incarnation_id: None,
                task_revision: None,
                require_queue_head: false,
                message_ids: vec![message_id],
                text: "deliver this message".into(),
                created_at_ms: 6,
            })
            .unwrap();
    }

    // Remove the v29 authority additions before lowering this populated
    // database to v27. This forces the real 0028 rebuild and subsequent
    // 0029 backfill; the child rows make an FK-on DROP fail with SQLite 787,
    // so the fixture also guards 0028's foreign-key rebuild discipline.
    {
        let connection = rusqlite::Connection::open(&database).unwrap();
        connection
            .execute_batch(
                "PRAGMA foreign_keys = OFF;
                 DROP TABLE session_work;
                 DROP INDEX delivery_attempts_session_work_identity;
                 DROP INDEX runs_one_open_per_session;
                 ALTER TABLE delivery_attempts DROP COLUMN run_id;
                 ALTER TABLE delivery_attempts DROP COLUMN task_revision;
                 ALTER TABLE tasks DROP COLUMN work_revision;
                 ALTER TABLE sessions DROP COLUMN principal_version;
                 PRAGMA user_version = 27;
                 PRAGMA foreign_keys = ON;",
            )
            .unwrap();
    }
    let store = Store::open(&database).unwrap();
    let connection = rusqlite::Connection::open(&database).unwrap();
    let version: i64 = connection
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .unwrap();
    assert_eq!(version, 30);
    let message_count: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM agent_messages WHERE id = 'migration-message'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let delivery_count: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM delivery_attempts WHERE id = 'migration-delivery'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(message_count, 1);
    assert_eq!(delivery_count, 1);
    connection
        .execute_batch("PRAGMA foreign_keys = ON;")
        .unwrap();
    let violations: i64 = connection
        .query_row("SELECT COUNT(*) FROM pragma_foreign_key_check", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(violations, 0);
    assert!(
        store
            .list_sessions(&project_id("factory"), None, 10)
            .unwrap()
            .iter()
            .any(|session| session.id == session_id("migration-session"))
    );
}

#[test]
fn migration_0029_quarantines_duplicate_open_session_owners_before_unique_defense() {
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("corrupt-v28.db");
    let session = session_id("corrupt-session");
    {
        let mut store = Store::open(&database).unwrap();
        store
            .create_project(
                NewProject {
                    id: project_id("factory"),
                    name: "Factory".into(),
                    root: "/work/factory".into(),
                },
                1,
            )
            .unwrap();
        for agent in ["curie", "fermi"] {
            store
                .create_agent(
                    NewAgent {
                        id: agent_id(agent),
                        project_id: project_id("factory"),
                        parent_agent_id: None,
                        role: AgentRole::Worker,
                        provider: Provider::Codex,
                    },
                    2,
                )
                .unwrap();
        }
        for (task, owner) in [("task-a", "curie"), ("task-b", "fermi")] {
            store
                .create_task(
                    NewTask {
                        id: task_id(task),
                        project_id: project_id("factory"),
                        parent_task_id: None,
                        title: task.into(),
                        body: String::new(),
                        priority: 0,
                    },
                    3,
                )
                .unwrap();
            store
                .assign_task(
                    &project_id("factory"),
                    &task_id(task),
                    Some(&agent_id(owner)),
                    4,
                )
                .unwrap();
        }
        store
            .create_session(new_session("corrupt-session", "factory", "curie"), 5)
            .unwrap();
        store
            .open_run_episode(&session, &task_id("task-a"), 6)
            .unwrap();
    }

    // v28 had no session-scoped uniqueness. Inject a second exact-owner run
    // and active attempt, then remove the v29 columns exactly as an upgrade
    // would encounter them.
    {
        let connection = rusqlite::Connection::open(&database).unwrap();
        connection
            .execute_batch(
                "DROP INDEX runs_one_open_per_session;
                 INSERT INTO runs (
                    id, project_id, agent_id, session_id, parent_run_id, task_id,
                    status, activity, wait_reason, worktree, started_at_ms,
                    status_since_ms, updated_at_ms, ended_at_ms, closed_by,
                    failure_reason, stop_requested_at_ms
                 ) VALUES (
                    'duplicate-run', 'factory', 'fermi', 'corrupt-session', NULL,
                    'task-b', 'running', NULL, NULL, '/work/factory', 7, 7, 7,
                    NULL, NULL, NULL, NULL
                 );
                 UPDATE tasks SET status = 'running' WHERE id = 'task-b';
                 INSERT INTO delivery_attempts (
                    id, project_id, agent_id, session_id, task_id,
                    task_incarnation_id, prior_run_count, message_ids_json, text,
                    failure_count, next_attempt_at_ms, state, created_at_ms,
                    updated_at_ms, task_revision, run_id
                 ) SELECT
                    'contradictory-attempt', 'factory', 'fermi', 'corrupt-session',
                    'task-b', incarnation_id, 0, '[]', 'legacy uncertain prompt',
                    2, 7, 'retryable', 7, 7, work_revision,
                    '33333333-3333-4333-8333-333333333333'
                 FROM tasks WHERE id = 'task-b';
                 PRAGMA foreign_keys = OFF;
                 DROP TABLE session_work;
                 DROP INDEX delivery_attempts_session_work_identity;
                 ALTER TABLE delivery_attempts DROP COLUMN run_id;
                 ALTER TABLE delivery_attempts DROP COLUMN task_revision;
                 ALTER TABLE tasks DROP COLUMN work_revision;
                 ALTER TABLE sessions DROP COLUMN principal_version;
                 PRAGMA user_version = 28;
                 PRAGMA foreign_keys = ON;",
            )
            .unwrap();
    }

    let mut store = Store::open(&database).unwrap();
    let quarantined = store.session_work(&session).unwrap();
    assert!(quarantined.work.is_none());
    assert!(
        quarantined
            .quarantine_reason
            .as_deref()
            .is_some_and(|reason| reason.contains("does not belong to the session"))
    );
    let connection = rusqlite::Connection::open(&database).unwrap();
    let open: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM runs WHERE session_id = 'corrupt-session' AND ended_at_ms IS NULL",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let failed: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM runs WHERE session_id = 'corrupt-session' AND status = 'failed'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!((open, failed), (0, 2));
    for task in ["task-a", "task-b"] {
        assert_eq!(
            store
                .get_task(&project_id("factory"), &task_id(task))
                .unwrap()
                .snapshot
                .status,
            TaskStatus::Failed,
            "terminalized duplicate run must terminalize its task too"
        );
    }
    let unique_index: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM sqlite_schema
             WHERE type = 'index' AND name = 'runs_one_open_per_session'
               AND sql LIKE 'CREATE UNIQUE INDEX%'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(unique_index, 1);
    drop(connection);

    assert!(matches!(
        store.begin_delivery_attempt("contradictory-attempt", 8),
        Err(StoreError::SessionWorkQuarantined(_))
    ));
    store.end_session(&session, Some(0), None, 8).unwrap();
    assert!(matches!(
        store.session_work(&session).unwrap().work.unwrap().phase,
        SessionWorkPhase::Empty
    ));
    assert_eq!(
        store
            .delivery_attempt_state("contradictory-attempt")
            .unwrap(),
        Some(DeliveryAttemptState::Cancelled)
    );
    let agent = store
        .get_agent_detail(&project_id("factory"), &agent_id("curie"))
        .unwrap();
    assert_eq!(agent.snapshot.current_run_id, None);
    assert_eq!(agent.snapshot.current_session_id, None);
}

#[test]
fn migration_0029_quarantines_a_lone_cross_agent_run_and_end_repairs_its_task() {
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("cross-agent-run-v28.db");
    let session = session_id("cross-run-session");
    {
        let mut store = Store::open(&database).unwrap();
        store
            .create_project(
                NewProject {
                    id: project_id("factory"),
                    name: "Factory".into(),
                    root: "/work/factory".into(),
                },
                1,
            )
            .unwrap();
        for agent in ["curie", "fermi"] {
            store
                .create_agent(
                    NewAgent {
                        id: agent_id(agent),
                        project_id: project_id("factory"),
                        parent_agent_id: None,
                        role: AgentRole::Worker,
                        provider: Provider::Codex,
                    },
                    2,
                )
                .unwrap();
        }
        store
            .create_task(
                NewTask {
                    id: task_id("fermi-task"),
                    project_id: project_id("factory"),
                    parent_task_id: None,
                    title: "Foreign owner".into(),
                    body: String::new(),
                    priority: 0,
                },
                3,
            )
            .unwrap();
        store
            .assign_task(
                &project_id("factory"),
                &task_id("fermi-task"),
                Some(&agent_id("fermi")),
                4,
            )
            .unwrap();
        store
            .create_session(new_session("cross-run-session", "factory", "curie"), 5)
            .unwrap();
    }
    {
        let connection = rusqlite::Connection::open(&database).unwrap();
        connection
            .execute_batch(
                "INSERT INTO runs (
                    id, project_id, agent_id, session_id, parent_run_id, task_id,
                    status, activity, wait_reason, worktree, started_at_ms,
                    status_since_ms, updated_at_ms, ended_at_ms, closed_by,
                    failure_reason, stop_requested_at_ms
                 ) VALUES (
                    'cross-agent-run', 'factory', 'fermi', 'cross-run-session', NULL,
                    'fermi-task', 'running', NULL, NULL, '/work/factory', 6, 6, 6,
                    NULL, NULL, NULL, NULL
                 );
                 UPDATE tasks SET status = 'running' WHERE id = 'fermi-task';",
            )
            .unwrap();
    }
    rewind_session_work_migration(&database);

    let mut store = Store::open(&database).unwrap();
    let work = store.session_work(&session).unwrap();
    assert!(work.work.is_none());
    assert!(
        work.quarantine_reason
            .as_deref()
            .is_some_and(|reason| reason.contains("does not belong to the session"))
    );
    store.end_session(&session, None, None, 7).unwrap();
    assert_eq!(
        store
            .get_task(&project_id("factory"), &task_id("fermi-task"))
            .unwrap()
            .snapshot
            .status,
        TaskStatus::Failed
    );
    assert!(matches!(
        store.session_work(&session).unwrap().work.unwrap().phase,
        SessionWorkPhase::Empty
    ));
}

#[test]
fn migration_0029_quarantines_a_lone_cross_agent_attempt() {
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("cross-agent-attempt-v28.db");
    let session = session_id("cross-attempt-session");
    {
        let mut store = Store::open(&database).unwrap();
        store
            .create_project(
                NewProject {
                    id: project_id("factory"),
                    name: "Factory".into(),
                    root: "/work/factory".into(),
                },
                1,
            )
            .unwrap();
        for agent in ["curie", "fermi"] {
            store
                .create_agent(
                    NewAgent {
                        id: agent_id(agent),
                        project_id: project_id("factory"),
                        parent_agent_id: None,
                        role: AgentRole::Worker,
                        provider: Provider::Codex,
                    },
                    2,
                )
                .unwrap();
        }
        store
            .create_task(
                NewTask {
                    id: task_id("fermi-task"),
                    project_id: project_id("factory"),
                    parent_task_id: None,
                    title: "Foreign attempt".into(),
                    body: String::new(),
                    priority: 0,
                },
                3,
            )
            .unwrap();
        store
            .assign_task(
                &project_id("factory"),
                &task_id("fermi-task"),
                Some(&agent_id("fermi")),
                4,
            )
            .unwrap();
        store
            .create_session(new_session("cross-attempt-session", "factory", "curie"), 5)
            .unwrap();
    }
    {
        let connection = rusqlite::Connection::open(&database).unwrap();
        connection
            .execute_batch(
                "INSERT INTO delivery_attempts (
                    id, project_id, agent_id, session_id, task_id,
                    task_incarnation_id, prior_run_count, message_ids_json, text,
                    failure_count, next_attempt_at_ms, state, created_at_ms,
                    updated_at_ms, task_revision, run_id
                 ) SELECT
                    'cross-agent-attempt', 'factory', 'fermi', 'cross-attempt-session',
                    'fermi-task', incarnation_id, 0, '[]', 'foreign prompt',
                    2, NULL, 'terminal', 6, 6, work_revision,
                    '33333333-3333-4333-8333-333333333333'
                 FROM tasks WHERE id = 'fermi-task';",
            )
            .unwrap();
    }
    rewind_session_work_migration(&database);

    let store = Store::open(&database).unwrap();
    let work = store.session_work(&session).unwrap();
    assert!(work.work.is_none());
    assert!(
        work.quarantine_reason
            .as_deref()
            .is_some_and(|reason| reason.contains("does not belong to the session"))
    );
    assert_eq!(
        store.delivery_attempt_state("cross-agent-attempt").unwrap(),
        Some(DeliveryAttemptState::Terminal)
    );
}

#[test]
fn migration_0029_resolves_ended_session_ownership_and_emits_terminal_events() {
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("ended-owned-work-v28.db");
    let session = session_id("ended-owned-session");
    {
        let mut store = Store::open(&database).unwrap();
        store
            .create_project(
                NewProject {
                    id: project_id("factory"),
                    name: "Factory".into(),
                    root: "/work/factory".into(),
                },
                1,
            )
            .unwrap();
        store
            .create_agent(
                NewAgent {
                    id: agent_id("curie"),
                    project_id: project_id("factory"),
                    parent_agent_id: None,
                    role: AgentRole::Worker,
                    provider: Provider::Codex,
                },
                2,
            )
            .unwrap();
        store
            .create_task(
                NewTask {
                    id: task_id("ended-task"),
                    project_id: project_id("factory"),
                    parent_task_id: None,
                    title: "Ended ownership".into(),
                    body: String::new(),
                    priority: 0,
                },
                3,
            )
            .unwrap();
        store
            .assign_task(
                &project_id("factory"),
                &task_id("ended-task"),
                Some(&agent_id("curie")),
                4,
            )
            .unwrap();
        store
            .create_session(new_session("ended-owned-session", "factory", "curie"), 5)
            .unwrap();
        store.end_session(&session, Some(0), None, 6).unwrap();
    }
    {
        let connection = rusqlite::Connection::open(&database).unwrap();
        connection
            .execute_batch(
                "INSERT INTO runs (
                    id, project_id, agent_id, session_id, parent_run_id, task_id,
                    status, activity, wait_reason, worktree, started_at_ms,
                    status_since_ms, updated_at_ms, ended_at_ms, closed_by,
                    failure_reason, stop_requested_at_ms
                 ) VALUES (
                    'ended-open-run', 'factory', 'curie', 'ended-owned-session', NULL,
                    'ended-task', 'running', NULL, NULL, '/work/factory', 7, 7, 7,
                    NULL, NULL, NULL, NULL
                 );
                 UPDATE tasks SET status = 'running' WHERE id = 'ended-task';
                 INSERT INTO delivery_attempts (
                    id, project_id, agent_id, session_id, task_id,
                    task_incarnation_id, prior_run_count, message_ids_json, text,
                    failure_count, next_attempt_at_ms, state, created_at_ms,
                    updated_at_ms, task_revision, run_id
                 ) VALUES (
                    'ended-attempt', 'factory', 'curie', 'ended-owned-session', NULL,
                    NULL, 0, '[]', 'ended prompt', 1, 8, 'retryable', 8, 8,
                    NULL, NULL
                 );",
            )
            .unwrap();
    }
    rewind_session_work_migration(&database);

    let store = Store::open(&database).unwrap();
    assert_eq!(
        store
            .get_task(&project_id("factory"), &task_id("ended-task"))
            .unwrap()
            .snapshot
            .status,
        TaskStatus::Failed
    );
    assert_eq!(
        store.delivery_attempt_state("ended-attempt").unwrap(),
        Some(DeliveryAttemptState::Cancelled)
    );
    assert!(matches!(
        store.session_work(&session).unwrap().work.unwrap().phase,
        SessionWorkPhase::Empty
    ));
    let events = store.events_after(0, 1_000).unwrap();
    assert!(events.iter().any(|event| matches!(
        &event.event,
        FactoryEvent::TaskChanged { task }
            if task.id == task_id("ended-task") && task.status == TaskStatus::Failed
    )));
    assert!(events.iter().any(|event| matches!(
        &event.event,
        FactoryEvent::RunChanged { run }
            if run.id == RunId::try_from("ended-open-run").unwrap()
                && run.status == factory_core::RunStatus::Failed
    )));
}

/// Builds a raw pre-0015 database (schema 14, `0014_sessions.sql`'s
/// original `sessions` shape) with one pre-existing session row whose
/// `last_hook_event` is a value 0014 already allowed (`'stop'`), then
/// opens it through the real `Store::open` -- which runs 0015's `sessions`
/// rebuild -- and asserts the widened `last_hook_event` CHECK now also
/// accepts `'permission_request'` without a constraint violation. A SQL
/// CHECK typo here is exactly the kind of bug Rust's own type checking
/// cannot catch (`ProviderHookEvent::PermissionRequest` would compile and
/// round-trip fine even if the CHECK list omitted the string it maps to).
#[test]
fn migration_0015_widens_the_last_hook_event_check_to_accept_permission_request() {
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("pre015.db");
    {
        let connection = rusqlite::Connection::open(&database).unwrap();
        connection
            .execute_batch("PRAGMA foreign_keys = OFF; PRAGMA journal_mode = WAL;")
            .unwrap();
        for migration in [
            include_str!("../migrations/0001_state_and_events.sql"),
            include_str!("../migrations/0002_execution_ledger.sql"),
            include_str!("../migrations/0003_runner_reconciliation.sql"),
            include_str!("../migrations/0004_observer_health.sql"),
            include_str!("../migrations/0005_provider_session_context.sql"),
            include_str!("../migrations/0006_webhooks.sql"),
            include_str!("../migrations/0007_subscription_usage.sql"),
            include_str!("../migrations/0008_subscription_windows.sql"),
            include_str!("../migrations/0009_agent_profiles.sql"),
            include_str!("../migrations/0010_agent_messages.sql"),
            include_str!("../migrations/0011_run_stop_intent.sql"),
            include_str!("../migrations/0012_drop_subscription_usage_and_task_dependencies.sql"),
            include_str!("../migrations/0013_agent_profile_files.sql"),
            include_str!("../migrations/0014_sessions.sql"),
        ] {
            connection.execute_batch(migration).unwrap();
        }
        connection.pragma_update(None, "user_version", 14).unwrap();

        connection
            .execute(
                "INSERT INTO projects (id, name, root, created_at_ms, updated_at_ms)
                 VALUES ('factory', 'Factory', '/work/factory', 1, 1)",
                [],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO agents (
                    id, project_id, parent_agent_id, role, provider, created_at_ms, updated_at_ms
                 ) VALUES ('curie', 'factory', NULL, 'worker', 'shell', 2, 2)",
                [],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO tasks (
                    id, project_id, title, body, status, priority, created_at_ms, updated_at_ms
                 ) VALUES ('legacy-task', 'factory', 'Legacy', '', 'queued', 0, 2, 2)",
                [],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO sessions (
                    id, project_id, agent_id, provider, worktree, hook_token, state,
                    state_since_ms, runner_instance_id, runner_runtime,
                    runner_protocol_version, last_hook_event, last_hook_at_ms,
                    started_at_ms, updated_at_ms
                 ) VALUES (
                    'session-legacy', 'factory', 'curie', 'shell', '/work/factory',
                    ?1, 'idle',
                    3, 'instance-legacy', '/private/runners/session-legacy', 1, 'stop', 3, 3, 3
                 )",
                [&"a".repeat(64)],
            )
            .unwrap();
    }

    // Opening through the real store runs the 0015 through 0030 migrations.
    let mut store = Store::open(&database).unwrap();

    let connection = rusqlite::Connection::open(&database).unwrap();
    let version: i64 = connection
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .unwrap();
    assert_eq!(version, 30);
    assert!(
        store.auto_mode().unwrap(),
        "pre-17 databases default auto mode on"
    );
    connection
        .execute_batch("PRAGMA foreign_keys = ON;")
        .unwrap();
    let violations: i64 = connection
        .query_row("SELECT COUNT(*) FROM pragma_foreign_key_check", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(
        violations, 0,
        "migration chain left a foreign key violation"
    );
    let incarnation: String = connection
        .query_row(
            "SELECT incarnation_id FROM tasks WHERE id = 'legacy-task'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(incarnation, "legacy:legacy-task");

    // The widened CHECK now accepts `permission_request` for a session
    // that survived the rebuild from before 0015 existed.
    let (session, _) = store
        .record_hook_event(
            &session_id("session-legacy"),
            ProviderHookEvent::PermissionRequest,
            None,
            false,
            Some("provider approval prompt: shell".into()),
            4,
        )
        .unwrap();
    assert_eq!(session.state, SessionState::WaitingForInput);
    assert_eq!(
        session.wait_reason.as_deref(),
        Some("provider approval prompt: shell")
    );
}

// --- Sessions: one live per agent, control target -----------------------

#[test]
fn only_one_live_session_per_agent_is_allowed() {
    let mut store = fixture();
    store
        .create_session(new_session("s1", "factory", "curie"), 5)
        .unwrap();
    let error = store
        .create_session(new_session("s2", "factory", "curie"), 6)
        .unwrap_err();
    assert!(matches!(error, StoreError::SessionAlreadyLive));

    // Ending the first session frees the agent up for a new one.
    store
        .end_session(&session_id("s1"), Some(0), None, 7)
        .unwrap();
    store
        .create_session(new_session("s2", "factory", "curie"), 8)
        .unwrap();
}

#[test]
fn session_control_target_resolves_by_session_and_by_run() {
    let mut store = fixture();
    let (snapshot, _) = store
        .create_session(new_session("s1", "factory", "curie"), 5)
        .unwrap();
    let target = store
        .session_control_target(&project_id("factory"), &snapshot.id)
        .unwrap();
    assert_eq!(target.runner_instance_id.as_str(), "instance-s1");

    let opened = store
        .open_run_episode(&snapshot.id, &task_id("task-1"), 6)
        .unwrap();
    let via_run = store
        .run_control_target(&project_id("factory"), &opened.run.id)
        .unwrap();
    assert_eq!(via_run.runner_instance_id.as_str(), "instance-s1");

    assert!(matches!(
        store.session_control_target(&project_id("factory"), &session_id("missing")),
        Err(StoreError::SessionNotFound)
    ));
}

// --- Provider session identity (Codex resume, item 5) ---------------------

#[test]
fn set_provider_session_id_persists_and_publishes_an_event() {
    let mut store = fixture();
    let (snapshot, _) = store
        .create_session(new_session("s1", "factory", "curie"), 5)
        .unwrap();
    assert_eq!(snapshot.provider_session_id, None);

    let (session, event) = store
        .set_provider_session_id(&snapshot.id, "9b1deb4d-3b7d-4bad-9bdd-2b0d7b3dcb6d", 6)
        .unwrap()
        .expect("first call sets it");
    assert_eq!(
        session.provider_session_id.as_deref(),
        Some("9b1deb4d-3b7d-4bad-9bdd-2b0d7b3dcb6d")
    );
    assert!(matches!(
        event.event,
        FactoryEvent::SessionChanged { session: ref changed }
            if changed.provider_session_id.as_deref()
                == Some("9b1deb4d-3b7d-4bad-9bdd-2b0d7b3dcb6d")
    ));
}

#[test]
fn set_provider_session_id_is_a_no_op_once_already_set() {
    let mut store = fixture();
    let (snapshot, _) = store
        .create_session(new_session("s1", "factory", "curie"), 5)
        .unwrap();
    store
        .set_provider_session_id(&snapshot.id, "9b1deb4d-3b7d-4bad-9bdd-2b0d7b3dcb6d", 6)
        .unwrap()
        .expect("first call sets it");

    // A second call (e.g. a duplicate/replayed SessionStart hook) with a
    // different value never clobbers the established identity.
    let outcome = store
        .set_provider_session_id(&snapshot.id, "22222222-2222-4222-8222-222222222222", 7)
        .unwrap();
    assert!(outcome.is_none());

    let target = store
        .session_control_target(&project_id("factory"), &snapshot.id)
        .unwrap();
    assert_eq!(target.runner_instance_id.as_str(), "instance-s1");
    let stored = store
        .last_provider_session_id(&project_id("factory"), &agent_id("curie"))
        .unwrap();
    assert_eq!(
        stored.as_deref(),
        Some("9b1deb4d-3b7d-4bad-9bdd-2b0d7b3dcb6d")
    );
}

#[test]
fn failed_codex_resume_identity_is_excluded_until_a_fresh_thread_is_confirmed() {
    let mut store = fixture();
    let old_thread = "9b1deb4d-3b7d-4bad-9bdd-2b0d7b3dcb6d";
    let new_thread = "f31a566f-544b-46f0-bd03-9c9ec3231c90";
    let mut initial = new_session("initial", "factory", "curie");
    initial.provider_session_id = Some(old_thread.to_owned());
    let (initial, _) = store.create_session(initial, 5).unwrap();
    assert!(
        !store
            .session_resumed_provider_thread(&project_id("factory"), &initial.id)
            .unwrap()
    );
    store.end_session(&initial.id, Some(0), None, 6).unwrap();

    let mut resumed = new_session("resumed", "factory", "curie");
    resumed.provider_session_id = Some(old_thread.to_owned());
    let (resumed, _) = store.create_session(resumed, 7).unwrap();
    assert!(
        store
            .session_resumed_provider_thread(&project_id("factory"), &resumed.id)
            .unwrap()
    );

    let task = task_id("recovery-task");
    store
        .create_task(
            NewTask {
                id: task.clone(),
                project_id: project_id("factory"),
                parent_task_id: None,
                title: "Recover delivery".into(),
                body: String::new(),
                priority: 0,
            },
            7,
        )
        .unwrap();
    store
        .assign_task(&project_id("factory"), &task, Some(&agent_id("curie")), 7)
        .unwrap();
    let marker = store.task_delivery_marker(&task).unwrap();
    store
        .ensure_delivery_attempt(NewDeliveryAttempt {
            id: "recovery-attempt".into(),
            project_id: project_id("factory"),
            agent_id: agent_id("curie"),
            session_id: resumed.id.clone(),
            task_id: Some(task),
            task_incarnation_id: Some(marker.incarnation_id),
            task_revision: Some(marker.task_revision),
            require_queue_head: false,
            message_ids: Vec::new(),
            text: "recover me".into(),
            created_at_ms: 7,
        })
        .unwrap();
    store
        .begin_delivery_attempt("recovery-attempt", 8)
        .unwrap()
        .expect("delivery becomes externally uncertain before recovery");

    store
        .request_fresh_provider_recovery(&project_id("factory"), &resumed.id, "recovery-attempt", 9)
        .unwrap();
    assert_eq!(
        store.delivery_attempt_state("recovery-attempt").unwrap(),
        Some(DeliveryAttemptState::Cancelled),
        "recovery must durably fence a late exact prompt hook"
    );
    let recoverable = store.recoverable_sessions().unwrap();
    assert!(
        recoverable
            .iter()
            .any(|session| session.session_id == resumed.id
                && session.delivery_recovery_stop_requested),
        "a crash before runner control must leave replayable stop work"
    );
    assert_eq!(
        store
            .last_provider_session_id(&project_id("factory"), &agent_id("curie"))
            .unwrap(),
        None,
        "the poisoned provider thread must not be selected again"
    );

    store.end_session(&resumed.id, Some(0), None, 10).unwrap();
    let (fresh, _) = store
        .create_session(new_session("fresh", "factory", "curie"), 11)
        .unwrap();
    assert!(
        !store
            .session_resumed_provider_thread(&project_id("factory"), &fresh.id)
            .unwrap()
    );
    store
        .set_provider_session_id(&fresh.id, new_thread, 12)
        .unwrap()
        .expect("fresh Codex session confirms a new thread");
    assert!(
        !store
            .session_resumed_provider_thread(&project_id("factory"), &fresh.id)
            .unwrap(),
        "SessionStart identity assignment must not rewrite launch provenance"
    );
    assert_eq!(
        store
            .last_provider_session_id(&project_id("factory"), &agent_id("curie"))
            .unwrap()
            .as_deref(),
        Some(new_thread)
    );
}

#[test]
fn set_provider_session_id_is_a_no_op_when_already_set_at_session_creation() {
    // Claude's provider_session_id is assigned by the daemon up front (the
    // session's own id) rather than learned from a hook payload, so a
    // later `set_provider_session_id` call -- as if a hook also reported
    // one -- is correctly a no-op, the same as a duplicated Codex
    // SessionStart.
    let mut store = fixture();
    let mut session_already_identified = new_session("s1", "factory", "curie");
    session_already_identified.provider_session_id =
        Some("2f5a1e2e-2222-4444-8888-0123456789ab".to_owned());
    let (snapshot, _) = store.create_session(session_already_identified, 5).unwrap();
    assert_eq!(
        snapshot.provider_session_id.as_deref(),
        Some("2f5a1e2e-2222-4444-8888-0123456789ab")
    );

    let outcome = store
        .set_provider_session_id(&snapshot.id, "9b1deb4d-3b7d-4bad-9bdd-2b0d7b3dcb6d", 6)
        .unwrap();
    assert!(outcome.is_none());
}

#[test]
fn set_provider_session_id_rejects_an_unknown_session() {
    let mut store = fixture();
    let error = store
        .set_provider_session_id(
            &session_id("missing"),
            "9b1deb4d-3b7d-4bad-9bdd-2b0d7b3dcb6d",
            5,
        )
        .unwrap_err();
    assert!(matches!(error, StoreError::SessionNotFound));
}

#[test]
fn set_provider_session_id_rejects_an_invalid_identity() {
    let mut store = fixture();
    let (snapshot, _) = store
        .create_session(new_session("s1", "factory", "curie"), 5)
        .unwrap();
    let error = store
        .set_provider_session_id(&snapshot.id, "", 6)
        .unwrap_err();
    assert!(matches!(error, StoreError::InvalidExecutionMetadata));
}

// --- Hook state machine --------------------------------------------------

#[test]
fn session_start_moves_a_starting_session_to_idle_and_clears_activity() {
    let mut store = fixture();
    let (snapshot, _) = store
        .create_session(new_session("s1", "factory", "curie"), 5)
        .unwrap();
    assert_eq!(snapshot.state, SessionState::Starting);

    let (session, _) = store
        .record_hook_event(
            &snapshot.id,
            ProviderHookEvent::SessionStart,
            None,
            false,
            None,
            6,
        )
        .unwrap();
    assert_eq!(session.state, SessionState::Idle);

    // A second SessionStart (already idle) is recorded but does not
    // clobber the durable state.
    let (session, _) = store
        .record_hook_event(
            &snapshot.id,
            ProviderHookEvent::SessionStart,
            None,
            false,
            None,
            7,
        )
        .unwrap();
    assert_eq!(session.state, SessionState::Idle);
    assert_eq!(
        session.last_hook_event,
        Some(ProviderHookEvent::SessionStart)
    );
}

#[test]
fn user_prompt_pre_tool_and_post_tool_move_the_session_to_working() {
    let mut store = fixture();
    let (snapshot, _) = store
        .create_session(new_session("s1", "factory", "curie"), 5)
        .unwrap();
    store
        .record_hook_event(
            &snapshot.id,
            ProviderHookEvent::SessionStart,
            None,
            false,
            None,
            6,
        )
        .unwrap();

    let (session, _) = store
        .record_hook_event(
            &snapshot.id,
            ProviderHookEvent::UserPromptSubmit,
            Some("thinking".into()),
            true,
            None,
            7,
        )
        .unwrap();
    assert_eq!(session.state, SessionState::Working);

    let (session, _) = store
        .record_hook_event(
            &snapshot.id,
            ProviderHookEvent::PreToolUse,
            Some("tool: Read".into()),
            false,
            None,
            8,
        )
        .unwrap();
    assert_eq!(session.state, SessionState::Working);
    assert_eq!(session.last_hook_event, Some(ProviderHookEvent::PreToolUse));

    let (session, _) = store
        .record_hook_event(
            &snapshot.id,
            ProviderHookEvent::PostToolUse,
            Some("thinking".into()),
            true,
            None,
            9,
        )
        .unwrap();
    assert_eq!(session.state, SessionState::Working);
}

/// Adversarial review finding 9b: Codex's own real `SessionStart` always
/// arrives *after* the daemon has already synthesized one and moved a
/// session past `starting` (`docs/providers.md`'s Codex `SessionStart`
/// section) -- often once a turn is already `working`. A second
/// `SessionStart` mid-turn must never drop the session back to `idle`, and
/// must never disturb its own `activity`/`state_since_ms`, since nothing
/// about the session's real progress changed.
#[test]
fn a_second_session_start_while_working_does_not_regress_the_session() {
    let mut store = fixture();
    let (snapshot, _) = store
        .create_session(new_session("s1", "factory", "curie"), 5)
        .unwrap();
    store
        .record_hook_event(
            &snapshot.id,
            ProviderHookEvent::SessionStart,
            None,
            false,
            None,
            6,
        )
        .unwrap();
    let (working, _) = store
        .record_hook_event(
            &snapshot.id,
            ProviderHookEvent::UserPromptSubmit,
            Some("thinking".into()),
            true,
            None,
            7,
        )
        .unwrap();
    assert_eq!(working.state, SessionState::Working);

    let (session, _) = store
        .record_hook_event(
            &snapshot.id,
            ProviderHookEvent::SessionStart,
            None,
            false,
            None,
            8,
        )
        .unwrap();
    assert_eq!(
        session.state,
        SessionState::Working,
        "a second SessionStart while working must not drop the session back to idle"
    );
    assert_eq!(session.activity, working.activity);
    assert_eq!(
        session.state_since_ms, working.state_since_ms,
        "state did not change, so state_since_ms must not either"
    );
    assert_eq!(
        session.last_hook_event,
        Some(ProviderHookEvent::SessionStart)
    );
}

/// `Store::synthesize_session_start` (`crates/factoryd/src/execution.rs`'s
/// `synthesize_codex_session_start` calls this once `RunnerEvent::
/// TerminalRaw` arrives for a Codex session -- see its own doc comment)
/// makes the exact same `starting -> idle` transition a real `SessionStart`
/// hook does, but must stay durably distinguishable from one: it never
/// touches `last_hook_event`, so `state = idle` with no `last_hook_event`
/// can only be reached this way.
#[test]
fn synthesize_session_start_moves_starting_to_idle_without_claiming_a_hook_fired() {
    let mut store = fixture();
    let (snapshot, _) = store
        .create_session(new_session("s1", "factory", "curie"), 5)
        .unwrap();
    assert_eq!(snapshot.state, SessionState::Starting);
    assert_eq!(snapshot.last_hook_event, None);

    let (session, _) = store
        .synthesize_session_start(&snapshot.id, 6)
        .unwrap()
        .expect("a starting session must synthesize");
    assert_eq!(session.state, SessionState::Idle);
    assert_eq!(
        session.last_hook_event, None,
        "synthesis must never claim a hook that never fired"
    );

    // Idempotent: a second synthesis attempt (e.g. a recovered session
    // replaying RunnerEvent::TerminalRaw again on reconnect) is a no-op,
    // not a second SessionChanged event for nothing.
    assert!(
        store
            .synthesize_session_start(&snapshot.id, 7)
            .unwrap()
            .is_none()
    );

    // And once a real hook does arrive, it is recorded normally --
    // synthesis never blocks the real signal from eventually landing.
    let (session, _) = store
        .record_hook_event(
            &snapshot.id,
            ProviderHookEvent::SessionStart,
            None,
            false,
            None,
            8,
        )
        .unwrap();
    assert_eq!(session.state, SessionState::Idle);
    assert_eq!(
        session.last_hook_event,
        Some(ProviderHookEvent::SessionStart)
    );
}

/// A session that already moved on (working, or ended) before synthesis is
/// ever attempted -- e.g. the real hook won the race -- must be left alone.
#[test]
fn synthesize_session_start_is_a_no_op_once_the_session_is_no_longer_starting() {
    let mut store = fixture();
    let (snapshot, _) = store
        .create_session(new_session("s1", "factory", "curie"), 5)
        .unwrap();
    store
        .record_hook_event(
            &snapshot.id,
            ProviderHookEvent::SessionStart,
            None,
            false,
            None,
            6,
        )
        .unwrap();
    let (working, _) = store
        .record_hook_event(
            &snapshot.id,
            ProviderHookEvent::UserPromptSubmit,
            Some("thinking".into()),
            true,
            None,
            7,
        )
        .unwrap();
    assert_eq!(working.state, SessionState::Working);

    assert!(
        store
            .synthesize_session_start(&snapshot.id, 8)
            .unwrap()
            .is_none(),
        "a session no longer starting must never be touched by synthesis"
    );
    let target = store
        .session_control_target(&project_id("factory"), &snapshot.id)
        .unwrap();
    assert_eq!(target.runner_instance_id.as_str(), "instance-s1");
}

#[test]
fn notification_moves_to_waiting_for_input_with_a_wait_reason() {
    let mut store = fixture();
    let (snapshot, _) = store
        .create_session(new_session("s1", "factory", "curie"), 5)
        .unwrap();
    let (session, _) = store
        .record_hook_event_with_notification(
            &snapshot.id,
            ProviderHookEvent::Notification,
            None,
            false,
            Some("permission prompt".into()),
            Some(ProviderNotificationKind::PermissionPrompt),
            6,
        )
        .unwrap();
    assert_eq!(session.state, SessionState::WaitingForInput);
    assert_eq!(session.wait_reason.as_deref(), Some("permission prompt"));
}

#[test]
fn routine_claude_notification_causes_do_not_create_attention_waits() {
    for kind in [
        ProviderNotificationKind::AgentCompleted,
        ProviderNotificationKind::IdlePrompt,
        ProviderNotificationKind::AuthSuccess,
        ProviderNotificationKind::ElicitationComplete,
        ProviderNotificationKind::ElicitationResponse,
    ] {
        let mut store = fixture();
        let (snapshot, _) = store
            .create_session(new_session("s1", "factory", "curie"), 5)
            .unwrap();
        let (session, _) = store
            .record_hook_event_with_notification(
                &snapshot.id,
                ProviderHookEvent::Notification,
                None,
                false,
                Some("Approve delivery?".into()),
                Some(kind),
                6,
            )
            .unwrap();
        assert_eq!(session.state, SessionState::Idle);
        assert_eq!(session.wait_reason, None);
        assert_eq!(session.notification_kind, Some(kind));
    }
}

#[test]
fn current_claude_actionable_notification_causes_wait_for_input() {
    for kind in [
        ProviderNotificationKind::ElicitationUrlDialog,
        ProviderNotificationKind::AgentNeedsInput,
    ] {
        let mut store = fixture();
        let (snapshot, _) = store
            .create_session(new_session("s1", "factory", "curie"), 5)
            .unwrap();
        let (session, _) = store
            .record_hook_event_with_notification(
                &snapshot.id,
                ProviderHookEvent::Notification,
                None,
                false,
                Some("answer required".into()),
                Some(kind),
                6,
            )
            .unwrap();
        assert_eq!(session.state, SessionState::WaitingForInput);
        assert_eq!(session.notification_kind, Some(kind));
    }
}

#[test]
fn terminal_attach_failure_is_durable_actionable_and_clears_on_recovery() {
    use factory_core::status::{AttentionAction, AttentionReasonKind, attention_items};

    let mut store = fixture();
    let (snapshot, _) = store
        .create_session(new_session("s1", "factory", "curie"), 5)
        .unwrap();
    let (failed, _) = store
        .record_terminal_attach_health(
            &snapshot.id,
            Some("terminal attach failed: runner socket unavailable".into()),
            6,
        )
        .unwrap()
        .unwrap();
    assert_eq!(
        failed.observer_health,
        factory_core::ObserverHealth::Degraded
    );
    assert_eq!(failed.observer_health_since_ms, 6);
    assert_eq!(
        failed.observer_reason.as_deref(),
        Some("terminal attach failed: runner socket unavailable")
    );

    let agent = store
        .agent_status(&project_id("factory"), &agent_id("curie"))
        .unwrap();
    let item = attention_items(&project_id("factory"), &[agent], &[], false)
        .into_iter()
        .next()
        .unwrap();
    assert_eq!(item.reason.kind, AttentionReasonKind::ObserverProblem);
    assert_eq!(item.reason.action, AttentionAction::InspectObserver);
    assert_eq!(item.session_id.as_ref(), Some(&snapshot.id));
    assert_eq!(item.since_ms, 6);

    let (healthy, _) = store
        .record_terminal_attach_health(&snapshot.id, None, 7)
        .unwrap()
        .unwrap();
    assert_eq!(
        healthy.observer_health,
        factory_core::ObserverHealth::Healthy
    );
    assert!(healthy.wait_reason.is_none());
    assert!(healthy.observer_reason.is_none());
    let agent = store
        .agent_status(&project_id("factory"), &agent_id("curie"))
        .unwrap();
    assert!(attention_items(&project_id("factory"), &[agent], &[], false).is_empty());
}

#[test]
fn terminal_attach_health_preserves_independent_wait_causes() {
    let mut store = fixture();
    let (snapshot, _) = store
        .create_session(new_session("s1", "factory", "curie"), 5)
        .unwrap();
    let cases = [
        (
            ProviderHookEvent::Notification,
            "Which branch?",
            Some(ProviderNotificationKind::ElicitationDialog),
        ),
        (
            ProviderHookEvent::PermissionRequest,
            "Approve shell command?",
            None,
        ),
    ];
    let mut now = 6;
    for (hook, reason, notification_kind) in cases {
        store
            .record_hook_event_with_notification(
                &snapshot.id,
                hook,
                None,
                false,
                Some(reason.to_owned()),
                notification_kind,
                now,
            )
            .unwrap();
        now += 1;
        let (failed, _) = store
            .record_terminal_attach_health(
                &snapshot.id,
                Some("terminal attach failed: runner unavailable".into()),
                now,
            )
            .unwrap()
            .unwrap();
        assert_eq!(failed.wait_reason.as_deref(), Some(reason));
        now += 1;
        let (recovered, _) = store
            .record_terminal_attach_health(&snapshot.id, None, now)
            .unwrap()
            .unwrap();
        assert_eq!(recovered.wait_reason.as_deref(), Some(reason));
        assert!(recovered.observer_reason.is_none());
        now += 1;
    }

    store
        .mark_session_waiting(&snapshot.id, "delivery unacknowledged".to_owned(), now)
        .unwrap();
    let (failed, _) = store
        .record_terminal_attach_health(
            &snapshot.id,
            Some("terminal attach failed: runner unavailable".into()),
            now + 1,
        )
        .unwrap()
        .unwrap();
    assert_eq!(
        failed.wait_reason.as_deref(),
        Some("delivery unacknowledged")
    );
    let (recovered, _) = store
        .record_terminal_attach_health(&snapshot.id, None, now + 2)
        .unwrap()
        .unwrap();
    assert_eq!(
        recovered.wait_reason.as_deref(),
        Some("delivery unacknowledged")
    );
}

#[test]
fn permission_request_wait_ends_on_the_next_activity_hook() {
    // Codex 0.147's own approval-prompt hook (docs/dogfood/2026-08-17.md,
    // "a session blocked on a provider approval prompt still shows
    // `working`"): projects the same way `Notification` already does
    // above. Any subsequent activity hook clears it back to `Working`,
    // whether that is the approved tool starting or finishing, or the
    // operator submitting another prompt.
    let mut store = fixture();
    let (snapshot, _) = store
        .create_session(new_session("s1", "factory", "curie"), 5)
        .unwrap();
    for (offset, activity_event) in [
        ProviderHookEvent::PreToolUse,
        ProviderHookEvent::PostToolUse,
        ProviderHookEvent::UserPromptSubmit,
    ]
    .into_iter()
    .enumerate()
    {
        let now = 6 + i64::try_from(offset).unwrap() * 2;
        let (session, _) = store
            .record_hook_event(
                &snapshot.id,
                ProviderHookEvent::PermissionRequest,
                None,
                false,
                Some("provider approval prompt: shell".into()),
                now,
            )
            .unwrap();
        assert_eq!(session.state, SessionState::WaitingForInput);
        assert_eq!(
            session.wait_reason.as_deref(),
            Some("provider approval prompt: shell")
        );

        let (session, _) = store
            .record_hook_event(
                &snapshot.id,
                activity_event,
                Some("activity resumed".into()),
                false,
                None,
                now + 1,
            )
            .unwrap();
        assert_eq!(session.state, SessionState::Working);
        assert_eq!(session.wait_reason, None);
    }
}

#[test]
fn stop_moves_to_idle_and_clears_activity_but_subagent_stop_only_records() {
    let mut store = fixture();
    let (snapshot, _) = store
        .create_session(new_session("s1", "factory", "curie"), 5)
        .unwrap();
    store
        .record_hook_event(
            &snapshot.id,
            ProviderHookEvent::PreToolUse,
            Some("tool: Bash".into()),
            false,
            None,
            6,
        )
        .unwrap();

    let (session, _) = store
        .record_hook_event(
            &snapshot.id,
            ProviderHookEvent::SubagentStop,
            None,
            false,
            None,
            7,
        )
        .unwrap();
    // A subagent stopping does not idle the top-level session.
    assert_eq!(session.state, SessionState::Working);
    assert_eq!(
        session.last_hook_event,
        Some(ProviderHookEvent::SubagentStop)
    );

    let (session, _) = store
        .record_hook_event(&snapshot.id, ProviderHookEvent::Stop, None, false, None, 8)
        .unwrap();
    assert_eq!(session.state, SessionState::Idle);
}

#[test]
fn hook_events_are_rejected_once_the_session_has_ended() {
    let mut store = fixture();
    let (snapshot, _) = store
        .create_session(new_session("s1", "factory", "curie"), 5)
        .unwrap();
    store.end_session(&snapshot.id, Some(0), None, 6).unwrap();
    assert!(matches!(
        store.record_hook_event(&snapshot.id, ProviderHookEvent::Stop, None, false, None, 7),
        Err(StoreError::SessionNotLive)
    ));
}

// --- fail_starting_session (issue #24's session-start deadline) --------
//
// Adversarial review of #24 findings 1/2/6: `execution.rs`'s
// `enforce_start_deadline` used to commit the `failed` transition guarded
// only by `SessionState::is_live()` (via `end_session_with_reason`), which
// also accepts `idle`/`working` -- so a `SessionStart` hook landing in the
// `await` gap between reading a session as `starting` and committing its
// failure could be silently overwritten with a false reason.
// `fail_starting_session` is guarded on `state = 'starting'` specifically;
// these are the reviewer's own throwaway probes, now permanent.

#[test]
fn fail_starting_session_ends_a_starting_session_failed_with_the_reason() {
    let mut store = fixture();
    let (snapshot, _) = store
        .create_session(new_session("s1", "factory", "curie"), 5)
        .unwrap();
    let (session, events) = store
        .fail_starting_session(&snapshot.id, "no hook arrived".into(), 6)
        .unwrap()
        .expect("a starting session must be failed");
    assert_eq!(session.state, SessionState::Failed);
    assert_eq!(session.wait_reason.as_deref(), Some("no hook arrived"));
    assert_eq!(session.ended_at_ms, Some(6));
    assert!(events.iter().any(
        |event| matches!(&event.event, FactoryEvent::SessionChanged { session }
                if session.id == snapshot.id && session.state == SessionState::Failed)
    ));
}

#[test]
fn fail_starting_session_is_a_no_op_once_the_hook_already_moved_it_to_idle() {
    let mut store = fixture();
    let (snapshot, _) = store
        .create_session(new_session("s1", "factory", "curie"), 5)
        .unwrap();
    // The session's own `SessionStart` hook wins the race first.
    store
        .record_hook_event(
            &snapshot.id,
            ProviderHookEvent::SessionStart,
            None,
            false,
            None,
            6,
        )
        .unwrap();

    let outcome = store
        .fail_starting_session(&snapshot.id, "no hook arrived".into(), 7)
        .unwrap();
    assert!(
        outcome.is_none(),
        "a session already past starting must not be failed"
    );

    // Untouched: still idle, never ended, no false reason attached.
    let sessions = store
        .list_sessions(&project_id("factory"), None, 10)
        .unwrap();
    let session = sessions
        .iter()
        .find(|session| session.id == snapshot.id)
        .unwrap();
    assert_eq!(session.state, SessionState::Idle);
    assert!(session.ended_at_ms.is_none());
    assert!(session.wait_reason.is_none());
}

#[test]
fn fail_starting_session_ends_a_stop_requested_session_stopped_not_failed() {
    let mut store = fixture();
    let (snapshot, _) = store
        .create_session(new_session("s1", "factory", "curie"), 5)
        .unwrap();
    store
        .request_session_stop(&project_id("factory"), &snapshot.id, 6)
        .unwrap();

    let (session, _) = store
        .fail_starting_session(&snapshot.id, "no hook arrived".into(), 7)
        .unwrap()
        .expect("a starting session (even stop-requested) is still guarded on `starting`");
    assert_eq!(
        session.state,
        SessionState::Stopped,
        "an operator stop already in flight must end the session stopped, not failed"
    );
}

// --- end_session closes the open episode --------------------------------

#[test]
fn end_session_closes_the_open_episode_as_failed_process_session_ended() {
    let mut store = fixture();
    let (snapshot, _) = store
        .create_session(new_session("s1", "factory", "curie"), 5)
        .unwrap();
    let opened = store
        .open_run_episode(&snapshot.id, &task_id("task-1"), 6)
        .unwrap();

    let (session, events) = store.end_session(&snapshot.id, None, Some(9), 7).unwrap();
    assert_eq!(session.state, SessionState::Failed);
    assert!(events.iter().any(
        |event| matches!(&event.event, FactoryEvent::RunChanged { run }
                if run.id == opened.run.id
                    && run.status == factory_core::RunStatus::Failed
                    && run.closed_by == Some(factory_core::RunClosedBy::SessionEnded)
                    && run.failure_reason == Some(factory_core::RunFailureReason::Process))
    ));
    let task = store
        .get_task(&project_id("factory"), &task_id("task-1"))
        .unwrap();
    assert_eq!(task.snapshot.status, TaskStatus::Failed);
}

#[test]
fn end_session_with_no_confirmed_exit_status_is_unverifiable_not_process() {
    // A session recovered after a daemon restart whose control endpoint is
    // simply gone (no OS exit status was ever observed) is a different,
    // operationally distinct failure from a confirmed crash.
    let mut store = fixture();
    let (snapshot, _) = store
        .create_session(new_session("s1", "factory", "curie"), 5)
        .unwrap();
    let opened = store
        .open_run_episode(&snapshot.id, &task_id("task-1"), 6)
        .unwrap();

    let (session, events) = store.end_session(&snapshot.id, None, None, 7).unwrap();
    assert_eq!(session.state, SessionState::Failed);
    assert!(events.iter().any(
        |event| matches!(&event.event, FactoryEvent::RunChanged { run }
            if run.id == opened.run.id
                && run.failure_reason == Some(factory_core::RunFailureReason::Unverifiable))
    ));
}

#[test]
fn end_session_with_a_clean_exit_is_stopped_not_failed() {
    let mut store = fixture();
    let (snapshot, _) = store
        .create_session(new_session("s1", "factory", "curie"), 5)
        .unwrap();
    let (session, _) = store.end_session(&snapshot.id, Some(0), None, 6).unwrap();
    assert_eq!(session.state, SessionState::Stopped);
}

#[test]
fn end_session_after_an_operator_stop_closes_the_episode_stopped_not_failed() {
    // TRACK5-DESIGN.md §6: an operator-requested StopSession/StopRun closes
    // the open episode `stopped`/`closed_by = operator_stop`, task
    // `cancelled` -- distinct from a crash, which closes it
    // `failed`/`closed_by = session_ended` (the test above this one).
    let mut store = fixture();
    let (snapshot, _) = store
        .create_session(new_session("s1", "factory", "curie"), 5)
        .unwrap();
    let opened = store
        .open_run_episode(&snapshot.id, &task_id("task-1"), 6)
        .unwrap();
    store
        .request_session_stop(&project_id("factory"), &snapshot.id, 7)
        .unwrap();

    let (session, events) = store.end_session(&snapshot.id, None, Some(15), 8).unwrap();
    assert_eq!(session.state, SessionState::Stopped);
    assert!(events.iter().any(
        |event| matches!(&event.event, FactoryEvent::RunChanged { run }
            if run.id == opened.run.id
                && run.status == factory_core::RunStatus::Stopped
                && run.closed_by == Some(factory_core::RunClosedBy::OperatorStop)
                && run.failure_reason.is_none())
    ));
    let task = store
        .get_task(&project_id("factory"), &task_id("task-1"))
        .unwrap();
    assert_eq!(task.snapshot.status, TaskStatus::Cancelled);
}

// --- complete_task / block_task / cancel_run -----------------------------

#[test]
fn complete_task_closes_the_episode_succeeded_with_the_result() {
    let mut store = fixture();
    let (snapshot, _) = store
        .create_session(new_session("s1", "factory", "curie"), 5)
        .unwrap();
    store
        .open_run_episode(&snapshot.id, &task_id("task-1"), 6)
        .unwrap();

    let closed = store
        .complete_task(
            &project_id("factory"),
            &task_id("task-1"),
            &snapshot.id,
            "all done".into(),
            7,
        )
        .unwrap();
    assert_eq!(closed.run.status, factory_core::RunStatus::Succeeded);
    assert_eq!(
        closed.run.closed_by,
        Some(factory_core::RunClosedBy::TaskDone)
    );
    assert_eq!(closed.task.snapshot.status, TaskStatus::Succeeded);
    assert_eq!(closed.task.result.as_deref(), Some("all done"));

    // The session survives and is still live (idle sessions can take more
    // work); only the episode closed.
    let live = store
        .live_session_for_agent(&project_id("factory"), &agent_id("curie"))
        .unwrap();
    assert!(live.is_some());
}

#[test]
fn a_different_live_session_cannot_complete_another_sessions_task() {
    let mut store = fixture();
    store
        .create_agent(
            NewAgent {
                id: agent_id("feynman"),
                project_id: project_id("factory"),
                parent_agent_id: None,
                role: AgentRole::Worker,
                provider: Provider::Codex,
            },
            5,
        )
        .unwrap();
    let (owner, _) = store
        .create_session(new_session("owner-session", "factory", "curie"), 6)
        .unwrap();
    store
        .open_run_episode(&owner.id, &task_id("task-1"), 7)
        .unwrap();
    let (other, _) = store
        .create_session(new_session("other-session", "factory", "feynman"), 8)
        .unwrap();

    assert!(
        store
            .complete_task(
                &project_id("factory"),
                &task_id("task-1"),
                &other.id,
                "spoofed completion".into(),
                9,
            )
            .is_err(),
        "completion was accepted without binding the calling session"
    );
}

#[test]
fn complete_task_without_an_open_episode_is_a_conflict() {
    let mut store = fixture();
    assert!(matches!(
        store.complete_task(
            &project_id("factory"),
            &task_id("task-1"),
            &session_id("missing"),
            "done".into(),
            5,
        ),
        Err(StoreError::TaskNotRunning)
    ));
}

#[test]
fn block_task_closes_the_episode_and_retry_requeues_it_after_resolution() {
    let mut store = fixture();
    let (snapshot, _) = store
        .create_session(new_session("s1", "factory", "curie"), 5)
        .unwrap();
    store
        .open_run_episode(&snapshot.id, &task_id("task-1"), 6)
        .unwrap();

    let closed = store
        .block_task(
            &project_id("factory"),
            &task_id("task-1"),
            &snapshot.id,
            "needs input".into(),
            7,
        )
        .unwrap();
    assert_eq!(closed.run.status, factory_core::RunStatus::Stopped);
    assert_eq!(
        closed.run.closed_by,
        Some(factory_core::RunClosedBy::TaskBlocked)
    );
    assert_eq!(closed.task.snapshot.status, TaskStatus::Blocked);
    assert_eq!(closed.task.blocked_reason.as_deref(), Some("needs input"));
    let blocked = store.blocked_tasks(&project_id("factory")).unwrap();
    assert_eq!(blocked.len(), 1);
    assert_eq!(blocked[0].task.id, task_id("task-1"));
    assert_eq!(blocked[0].reason.as_deref(), Some("needs input"));

    let (retried, event) = store
        .retry_task(&project_id("factory"), &task_id("task-1"), 8)
        .unwrap();
    assert_eq!(retried.snapshot.status, TaskStatus::Queued);
    assert_eq!(retried.blocked_reason, None);
    assert!(matches!(event.event, FactoryEvent::TaskChanged { .. }));
    assert!(
        store
            .blocked_tasks(&project_id("factory"))
            .unwrap()
            .is_empty()
    );
}

#[test]
fn cancel_run_closes_the_episode_without_touching_the_session() {
    let mut store = fixture();
    let (snapshot, _) = store
        .create_session(new_session("s1", "factory", "curie"), 5)
        .unwrap();
    let opened = store
        .open_run_episode(&snapshot.id, &task_id("task-1"), 6)
        .unwrap();

    let closed = store
        .cancel_run(&project_id("factory"), &opened.run.id, 7)
        .unwrap();
    assert_eq!(closed.run.status, factory_core::RunStatus::Stopped);
    assert_eq!(
        closed.run.closed_by,
        Some(factory_core::RunClosedBy::OperatorCancel)
    );
    assert_eq!(closed.task.snapshot.status, TaskStatus::Cancelled);

    let live = store
        .live_session_for_agent(&project_id("factory"), &agent_id("curie"))
        .unwrap()
        .unwrap();
    assert_eq!(live.state, SessionState::Starting);

    assert!(matches!(
        store.cancel_run(&project_id("factory"), &opened.run.id, 8),
        Err(StoreError::RunNotStoppable)
    ));
}

// --- next_deliverable -----------------------------------------------------

#[test]
fn next_deliverable_is_the_oldest_queued_task_by_created_at_then_id() {
    let mut store = Store::open_in_memory().unwrap();
    store
        .create_project(
            NewProject {
                id: project_id("factory"),
                name: "Factory".into(),
                root: "/work/factory".into(),
            },
            1,
        )
        .unwrap();
    store
        .create_agent(
            NewAgent {
                id: agent_id("curie"),
                project_id: project_id("factory"),
                parent_agent_id: None,
                role: AgentRole::Worker,
                provider: Provider::Codex,
            },
            2,
        )
        .unwrap();
    for (id, created_at) in [("task-b", 10), ("task-a", 5), ("task-c", 10)] {
        store
            .create_task(
                NewTask {
                    id: task_id(id),
                    project_id: project_id("factory"),
                    parent_task_id: None,
                    title: id.into(),
                    body: String::new(),
                    priority: 0,
                },
                created_at,
            )
            .unwrap();
        store
            .assign_task(
                &project_id("factory"),
                &task_id(id),
                Some(&agent_id("curie")),
                created_at,
            )
            .unwrap();
    }

    assert_eq!(
        store
            .next_deliverable(&project_id("factory"), &agent_id("curie"))
            .unwrap(),
        Some(task_id("task-a"))
    );

    store
        .pause_agent(&project_id("factory"), &agent_id("curie"), 20)
        .unwrap();
    assert_eq!(
        store
            .next_deliverable(&project_id("factory"), &agent_id("curie"))
            .unwrap(),
        None,
        "a paused agent has no deliverable work"
    );

    store
        .resume_agent(&project_id("factory"), &agent_id("curie"), 21)
        .unwrap();
    assert_eq!(
        store
            .next_deliverable(&project_id("factory"), &agent_id("curie"))
            .unwrap(),
        Some(task_id("task-a"))
    );

    // Once task-a is delivered (running), the next oldest queued task among
    // the same-timestamp pair breaks the tie by id.
    let (snapshot, _) = store
        .create_session(new_session("s1", "factory", "curie"), 22)
        .unwrap();
    store
        .open_run_episode(&snapshot.id, &task_id("task-a"), 23)
        .unwrap();
    assert_eq!(
        store
            .next_deliverable(&project_id("factory"), &agent_id("curie"))
            .unwrap(),
        Some(task_id("task-b"))
    );
}

// --- Message delivery marking by session ---------------------------------

#[test]
fn undelivered_messages_for_agent_lists_only_pending_inbox_in_order() {
    let mut store = fixture();
    store
        .send_agent_message(NewAgentMessage {
            id: MessageId::try_from("message-2").unwrap(),
            project_id: project_id("factory"),
            sender_agent_id: None,
            recipient_agent_id: agent_id("curie"),
            body: "second".into(),
            created_at_ms: 6,
        })
        .unwrap();
    store
        .send_agent_message(NewAgentMessage {
            id: MessageId::try_from("message-1").unwrap(),
            project_id: project_id("factory"),
            sender_agent_id: None,
            recipient_agent_id: agent_id("curie"),
            body: "first".into(),
            created_at_ms: 5,
        })
        .unwrap();

    let pending = store
        .undelivered_messages_for_agent(&project_id("factory"), &agent_id("curie"))
        .unwrap();
    assert_eq!(
        pending
            .iter()
            .map(|message| message.body.as_str())
            .collect::<Vec<_>>(),
        ["first", "second"]
    );

    let (snapshot, _) = store
        .create_session(new_session("s1", "factory", "curie"), 7)
        .unwrap();
    store
        .deliver_agent_messages(&project_id("factory"), &agent_id("curie"), &snapshot.id, 8)
        .unwrap();
    assert!(
        store
            .undelivered_messages_for_agent(&project_id("factory"), &agent_id("curie"))
            .unwrap()
            .is_empty()
    );
}
