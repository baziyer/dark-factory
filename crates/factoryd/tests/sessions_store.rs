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
        provider_session_id: None,
        worktree: format!("/work/{project}"),
        codex_home: None,
        hook_token: "a".repeat(64),
        runner_instance_id: RunnerInstanceId::try_from(format!("instance-{seed}")).unwrap(),
        runner_runtime: format!("/private/runners/{seed}"),
        runner_protocol_version: 1,
    }
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
/// one legacy *open* run, then opens it through the real `Store::open` and
/// asserts: the schema lands on 14, the legacy open run is force-closed
/// (not left dangling), and `PRAGMA foreign_key_check` is clean.
#[test]
fn migration_0014_force_closes_a_legacy_open_run_and_reaches_schema_14() {
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

    // Opening through the real store runs the 0014 migration.
    let store = Store::open(&database).unwrap();

    let connection = rusqlite::Connection::open(&database).unwrap();
    let version: i64 = connection
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .unwrap();
    assert_eq!(version, 14);

    connection
        .execute_batch("PRAGMA foreign_keys = ON;")
        .unwrap();
    let violations: i64 = connection
        .query_row("SELECT COUNT(*) FROM pragma_foreign_key_check", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(violations, 0, "migration 0014 left a foreign key violation");

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
fn end_session_with_a_clean_exit_is_stopped_not_failed() {
    let mut store = fixture();
    let (snapshot, _) = store
        .create_session(new_session("s1", "factory", "curie"), 5)
        .unwrap();
    let (session, _) = store.end_session(&snapshot.id, Some(0), None, 6).unwrap();
    assert_eq!(session.state, SessionState::Stopped);
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
