//! Track 5A: sessions store, migration 0014, and the hook state machine.

use factory_core::{
    AgentId, AgentRole, FactoryEvent, MessageId, ProjectId, Provider, ProviderHookEvent, RunId,
    RunnerInstanceId, SessionId, SessionState, TaskId, TaskStatus,
};
use factoryd::store::{
    NewAgent, NewAgentMessage, NewProject, NewSession, NewTask, Store, StoreError,
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

// --- Migration -------------------------------------------------------

/// Builds a raw pre-0014 database (schema 13, the pre-sessions shape) with
/// one legacy *open* run, then opens it through the real `Store::open` --
/// which always migrates to the current `SCHEMA_VERSION`, 26 after the
/// connector-event migration, runtime metadata, legacy permission repair,
/// delivery-attempt, managed-change, and managed-change recovery migrations
/// (0015 widened `last_hook_event` for `permission_request`) -- and
/// asserts: the legacy open run is force-closed by 0014 (not left
/// dangling), and `PRAGMA foreign_key_check` is clean after the full
/// chain including 0015's `sessions` rebuild, 0016's task incarnations, and
/// 0021's historical runtime metadata columns, 0022's legacy permission
/// repair, and 0024/0025/0026's delivery and managed-change ownership schema.
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

    // Opening through the real store runs migrations 0014 through 0026.
    let store = Store::open(&database).unwrap();

    let connection = rusqlite::Connection::open(&database).unwrap();
    let version: i64 = connection
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .unwrap();
    assert_eq!(version, 26);
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
fn migrations_0019_and_0020_follow_the_budget_schema_in_order() {
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("schema-18.db");
    drop(Store::open(&database).unwrap());
    {
        let connection = rusqlite::Connection::open(&database).unwrap();
        connection
            .execute_batch(
                "DROP TABLE delivery_attempts;
                 DROP TABLE connector_events;
                 DROP TABLE project_repository_authority;
                 DROP TABLE managed_changes;
                 ALTER TABLE agent_profiles DROP COLUMN model_selection_reason;
                 ALTER TABLE agent_profiles DROP COLUMN reasoning_effort;
                 PRAGMA user_version = 18;",
            )
            .unwrap();
    }

    drop(Store::open(&database).unwrap());
    let connection = rusqlite::Connection::open(&database).unwrap();
    let version: i64 = connection
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .unwrap();
    assert_eq!(version, 26);
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

    // Opening through the real store runs the 0015 through 0026 migrations.
    let mut store = Store::open(&database).unwrap();

    let connection = rusqlite::Connection::open(&database).unwrap();
    let version: i64 = connection
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .unwrap();
    assert_eq!(version, 26);
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
        .record_hook_event(
            &snapshot.id,
            ProviderHookEvent::Notification,
            None,
            false,
            Some("permission prompt".into()),
            6,
        )
        .unwrap();
    assert_eq!(session.state, SessionState::WaitingForInput);
    assert_eq!(session.wait_reason.as_deref(), Some("permission prompt"));
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
fn complete_task_without_an_open_episode_is_a_conflict() {
    let mut store = fixture();
    assert!(matches!(
        store.complete_task(&project_id("factory"), &task_id("task-1"), "done".into(), 5),
        Err(StoreError::TaskNotRunning)
    ));
}

#[test]
fn block_task_closes_the_episode_stopped_with_a_blocked_reason() {
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
