use factory_core::{AgentRole, FactoryEvent, Provider, TaskStatus};
use factoryd::store::{
    AdoptedProviderSession, NewAgent, NewProject, NewTask, RunReservation, Store, StoreError,
};

const SESSION_ID: &str = "0195d40a-1111-7000-8000-000000000001";
const OTHER_SESSION_ID: &str = "0195d40a-2222-7000-8000-000000000002";
const AMBIGUOUS_SESSION_ID: &str = "0195d40a-3333-7000-8000-000000000003";

fn id<T>(value: &str) -> T
where
    T: TryFrom<String>,
    T::Error: std::fmt::Debug,
{
    T::try_from(value.to_owned()).unwrap()
}

fn project(store: &mut Store) {
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
}

fn agent(agent: &str, provider: Provider) -> NewAgent {
    NewAgent {
        id: id(agent),
        project_id: id("project"),
        parent_agent_id: None,
        role: AgentRole::Worker,
        provider,
    }
}

fn task(store: &mut Store, task_id: &str) {
    store
        .create_task(
            NewTask {
                id: id(task_id),
                project_id: id("project"),
                parent_task_id: None,
                title: "Task".into(),
                body: "private instructions".into(),
                priority: 0,
            },
            2,
        )
        .unwrap();
}

fn reservation(task: &str, agent: &str, run: &str, worktree: &str) -> RunReservation {
    RunReservation {
        project_id: id("project"),
        task_id: id(task),
        agent_id: id(agent),
        expected_provider: Provider::Codex,
        run_id: id(run),
        parent_run_id: None,
        worktree: worktree.into(),
        fresh_provider_session_id: None,
        runner_instance_id: id(&format!("instance-{run}")),
        runner_runtime: format!("/private/runners/{run}"),
    }
}

#[test]
fn adopted_sessions_are_atomic_provider_bound_unique_and_private() {
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("factory.db");
    let mut store = Store::open(&database).unwrap();
    project(&mut store);

    let (claude, claude_event) = store
        .adopt_agent(
            agent("claude-agent", Provider::ClaudeCode),
            AdoptedProviderSession::ClaudeCode {
                session_id: SESSION_ID.into(),
                cwd: "/work/private-claude-cwd".into(),
            },
            2,
        )
        .unwrap();
    let (codex, codex_event) = store
        .adopt_agent(
            agent("codex-agent", Provider::Codex),
            AdoptedProviderSession::Codex {
                thread_id: SESSION_ID.into(),
                cwd: "/work/private-codex-cwd".into(),
                codex_home: Some("/private/codex-home".into()),
            },
            3,
        )
        .unwrap();
    let (_, default_home_event) = store
        .adopt_agent(
            agent("codex-default-agent", Provider::Codex),
            AdoptedProviderSession::Codex {
                thread_id: OTHER_SESSION_ID.into(),
                cwd: "/work/private-codex-default-cwd".into(),
                codex_home: None,
            },
            3,
        )
        .unwrap();

    assert_eq!(claude.provider, Provider::ClaudeCode);
    assert_eq!(codex.provider, Provider::Codex);
    assert!(matches!(
        claude_event.event,
        FactoryEvent::AgentChanged { .. }
    ));
    assert!(matches!(
        codex_event.event,
        FactoryEvent::AgentChanged { .. }
    ));
    let public = serde_json::to_string(&[claude_event, codex_event, default_home_event]).unwrap();
    for private in [
        SESSION_ID,
        OTHER_SESSION_ID,
        "/work/private-claude-cwd",
        "/work/private-codex-cwd",
        "/work/private-codex-default-cwd",
        "/private/codex-home",
    ] {
        assert!(!public.contains(private));
    }

    let head = store.latest_event_sequence().unwrap();
    let duplicate = store.adopt_agent(
        agent("duplicate-codex", Provider::Codex),
        AdoptedProviderSession::Codex {
            thread_id: SESSION_ID.into(),
            cwd: "/work/duplicate-cwd".into(),
            codex_home: Some("/private/duplicate-home".into()),
        },
        4,
    );
    assert!(matches!(
        duplicate,
        Err(StoreError::ProviderSessionConflict)
    ));
    assert_eq!(store.latest_event_sequence().unwrap(), head);

    let mismatch = store.adopt_agent(
        agent("provider-mismatch", Provider::ClaudeCode),
        AdoptedProviderSession::Codex {
            thread_id: OTHER_SESSION_ID.into(),
            cwd: "/work/provider-mismatch".into(),
            codex_home: Some("/private/provider-mismatch".into()),
        },
        5,
    );
    assert!(matches!(
        mismatch,
        Err(StoreError::InvalidProviderSessionAdoption)
    ));
    assert_eq!(store.latest_event_sequence().unwrap(), head);

    for invalid in [
        store.adopt_agent(
            agent("invalid-session", Provider::ClaudeCode),
            AdoptedProviderSession::ClaudeCode {
                session_id: "not-a-uuid-PRIVATE".into(),
                cwd: "/work/invalid-session".into(),
            },
            6,
        ),
        store.adopt_agent(
            agent("relative-cwd", Provider::ClaudeCode),
            AdoptedProviderSession::ClaudeCode {
                session_id: OTHER_SESSION_ID.into(),
                cwd: "relative/private-cwd".into(),
            },
            6,
        ),
        store.adopt_agent(
            agent("relative-home", Provider::Codex),
            AdoptedProviderSession::Codex {
                thread_id: OTHER_SESSION_ID.into(),
                cwd: "/work/valid-cwd".into(),
                codex_home: Some("relative/private-home".into()),
            },
            6,
        ),
        store.adopt_agent(
            agent("noncanonical-cwd", Provider::ClaudeCode),
            AdoptedProviderSession::ClaudeCode {
                session_id: OTHER_SESSION_ID.into(),
                cwd: "/work/../PRIVATE_noncanonical_cwd".into(),
            },
            6,
        ),
        store.adopt_agent(
            agent("noncanonical-home", Provider::Codex),
            AdoptedProviderSession::Codex {
                thread_id: OTHER_SESSION_ID.into(),
                cwd: "/work/valid-cwd".into(),
                codex_home: Some("/private/./PRIVATE_noncanonical_home".into()),
            },
            6,
        ),
    ] {
        let error = invalid.unwrap_err();
        assert!(matches!(error, StoreError::InvalidExecutionMetadata));
        let message = error.to_string();
        assert!(!message.contains("PRIVATE"));
        assert!(!message.contains("relative/private"));
    }
    assert_eq!(store.latest_event_sequence().unwrap(), head);
    drop(store);

    let connection = rusqlite::Connection::open(&database).unwrap();
    let rows: Vec<(String, String, String, Option<String>)> = connection
        .prepare(
            "SELECT provider, provider_session_id, provider_session_cwd, codex_home
             FROM agents ORDER BY id",
        )
        .unwrap()
        .query_map([], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
        })
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(
        rows,
        vec![
            (
                "claude_code".into(),
                SESSION_ID.into(),
                "/work/private-claude-cwd".into(),
                None,
            ),
            (
                "codex".into(),
                SESSION_ID.into(),
                "/work/private-codex-cwd".into(),
                Some("/private/codex-home".into()),
            ),
            (
                "codex".into(),
                OTHER_SESSION_ID.into(),
                "/work/private-codex-default-cwd".into(),
                None,
            ),
        ]
    );
}

#[test]
fn adopted_session_cwd_is_an_exact_private_reservation_boundary() {
    let mut store = Store::open_in_memory().unwrap();
    project(&mut store);
    task(&mut store, "task");
    store
        .adopt_agent(
            agent("codex-agent", Provider::Codex),
            AdoptedProviderSession::Codex {
                thread_id: SESSION_ID.into(),
                cwd: "/work/exact-cwd".into(),
                codex_home: Some("/private/exact-home".into()),
            },
            3,
        )
        .unwrap();
    let identity = store
        .agent_execution_identity(&id("project"), &id("codex-agent"))
        .unwrap();
    assert_eq!(identity.provider, Provider::Codex);
    assert!(identity.has_provider_session);

    let head = store.latest_event_sequence().unwrap();
    let mismatch = store.reserve_task_run(
        reservation(
            "task",
            "codex-agent",
            "wrong-run",
            "/work/private-wrong-cwd",
        ),
        1,
        4,
    );
    let error = match mismatch {
        Err(error) => error,
        Ok(_) => panic!("mismatched adopted cwd was accepted"),
    };
    assert!(matches!(error, StoreError::ProviderSessionCwdMismatch));
    assert!(!error.to_string().contains("private-wrong-cwd"));
    assert!(!error.to_string().contains("exact-cwd"));
    assert_eq!(store.latest_event_sequence().unwrap(), head);
    let queued = &store.list_tasks(&id("project"), None, 10).unwrap()[0].snapshot;
    assert_eq!(queued.status, TaskStatus::Queued);
    assert_eq!(queued.assigned_agent_id, None);

    let reserved = store
        .reserve_task_run(
            reservation("task", "codex-agent", "run", "/work/exact-cwd"),
            1,
            5,
        )
        .unwrap();
    assert_eq!(
        reserved.target.provider_session_id.as_deref(),
        Some(SESSION_ID)
    );
    assert!(reserved.target.resumes_provider_session);
    assert_eq!(
        reserved.target.codex_home.as_deref(),
        Some("/private/exact-home")
    );
    let public = serde_json::to_string(&reserved.events).unwrap();
    assert!(!public.contains(SESSION_ID));
    assert!(!public.contains("/private/exact-home"));
}

#[test]
fn adopted_codex_default_home_stays_explicitly_absent_in_the_private_target() {
    let mut store = Store::open_in_memory().unwrap();
    project(&mut store);
    task(&mut store, "task");
    store
        .adopt_agent(
            agent("codex-agent", Provider::Codex),
            AdoptedProviderSession::Codex {
                thread_id: SESSION_ID.into(),
                cwd: "/work/default-home-cwd".into(),
                codex_home: None,
            },
            3,
        )
        .unwrap();

    let reserved = store
        .reserve_task_run(
            reservation("task", "codex-agent", "run", "/work/default-home-cwd"),
            1,
            4,
        )
        .unwrap();
    assert!(reserved.target.resumes_provider_session);
    assert_eq!(
        reserved.target.provider_session_id.as_deref(),
        Some(SESSION_ID)
    );
    assert_eq!(reserved.target.codex_home, None);
}

#[test]
fn migrates_v4_session_cwd_without_changing_the_event_head() {
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
            .execute_batch(include_str!("../migrations/0004_observer_health.sql"))
            .unwrap();
        connection
            .execute_batch(&format!(
                "INSERT INTO projects (id, name, root, created_at_ms, updated_at_ms)
                 VALUES ('project', 'Project', '/work/project', 1, 1);

                 INSERT INTO tasks (
                     id, project_id, assigned_agent_id, title, body, status,
                     priority, created_at_ms, updated_at_ms
                 ) VALUES (
                     'task', 'project', 'agent', 'Old task', 'private old body',
                     'running', 0, 2, 4
                 );

                 INSERT INTO agents (
                     id, project_id, role, provider, provider_session_id,
                     created_at_ms, updated_at_ms
                 ) VALUES ('agent', 'project', 'worker', 'codex', '{SESSION_ID}', 3, 4);

                 INSERT INTO agents (
                     id, project_id, role, provider, provider_session_id,
                     created_at_ms, updated_at_ms
                 ) VALUES (
                     'unbound-agent', 'project', 'worker', 'claude_code',
                     '{OTHER_SESSION_ID}', 3, 4
                 );

                 INSERT INTO agents (
                     id, project_id, role, provider, provider_session_id,
                     created_at_ms, updated_at_ms
                 ) VALUES (
                     'ambiguous-agent', 'project', 'worker', 'codex',
                     '{AMBIGUOUS_SESSION_ID}', 3, 4
                 );

                 INSERT INTO runs (
                     id, project_id, agent_id, task_id, status, worktree,
                     provider_session_id, resumes_provider_session,
                     provider_session_confirmed_at_ms, runner_instance_id,
                     runner_protocol_version, runner_runtime, last_runner_sequence,
                     started_at_ms, status_since_ms, updated_at_ms
                 ) VALUES (
                     'run-old', 'project', 'agent', 'task', 'running', '/work/exact-old-cwd',
                     '{SESSION_ID}', 1, 4, 'instance-old', 1,
                     '/private/runners/old', 1, 4, 4, 4
                 );

                 INSERT INTO runs (
                     id, project_id, agent_id, status, worktree,
                     provider_session_id, resumes_provider_session,
                     provider_session_confirmed_at_ms, runner_instance_id,
                     runner_protocol_version, runner_runtime, last_runner_sequence,
                     terminal_runner_sequence, runner_terminal_kind,
                     started_at_ms, status_since_ms, updated_at_ms, ended_at_ms,
                     exit_code, failure_reason
                 ) VALUES (
                     'run-newer-other-session', 'project', 'agent', 'failed',
                     '/work/PRIVATE_wrong-newer-cwd', '{OTHER_SESSION_ID}', 0, 9,
                     'instance-newer', 1, '/private/runners/newer', 1, 1,
                     'exited', 9, 10, 10, 10, 1, 'process'
                 );

                 INSERT INTO runs (
                     id, project_id, agent_id, status, worktree,
                     provider_session_id, resumes_provider_session,
                     provider_session_confirmed_at_ms, runner_instance_id,
                     runner_protocol_version, runner_runtime, last_runner_sequence,
                     terminal_runner_sequence, runner_terminal_kind,
                     started_at_ms, status_since_ms, updated_at_ms, ended_at_ms,
                     exit_code, failure_reason
                 ) VALUES
                     (
                         'ambiguous-a', 'project', 'ambiguous-agent', 'failed',
                         '/work/ambiguous-a', '{AMBIGUOUS_SESSION_ID}', 1, 20,
                         'instance-ambiguous-a', 1, '/private/runners/ambiguous-a',
                         1, 1, 'exited', 20, 21, 21, 21, 1, 'process'
                     ),
                     (
                         'ambiguous-z', 'project', 'ambiguous-agent', 'failed',
                         '/work/ambiguous-z', '{AMBIGUOUS_SESSION_ID}', 1, 20,
                         'instance-ambiguous-z', 1, '/private/runners/ambiguous-z',
                         1, 1, 'exited', 20, 21, 21, 21, 1, 'process'
                     );

                 INSERT INTO events (
                     occurred_at_ms, project_id, agent_id, kind,
                     schema_version, payload_json
                 ) VALUES (
                     4, 'project', 'agent', 'agent_changed', 1,
                     '{{\"type\":\"agent_changed\",\"data\":{{\"agent\":{{\"id\":\"agent\",\"project_id\":\"project\",\"role\":\"worker\",\"provider\":\"codex\",\"created_at_ms\":3,\"updated_at_ms\":4}}}}}}'
                 );"
            ))
            .unwrap();
        connection.pragma_update(None, "user_version", 4).unwrap();
    }

    for _ in 0..2 {
        let store = Store::open(&database).unwrap();
        assert_eq!(store.latest_event_sequence().unwrap(), 1);
        assert_eq!(store.events_after(0, 10).unwrap().len(), 1);
    }

    let mut store = Store::open(&database).unwrap();
    task(&mut store, "unbound-task");
    let mut unbound = reservation(
        "unbound-task",
        "unbound-agent",
        "unbound-run",
        "/work/project",
    );
    unbound.expected_provider = Provider::ClaudeCode;
    assert!(matches!(
        store.reserve_task_run(unbound, 2, 5),
        Err(StoreError::InvalidExecutionMetadata)
    ));
    drop(store);

    let connection = rusqlite::Connection::open(&database).unwrap();
    let version: i64 = connection
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .unwrap();
    let stored: (String, Option<String>) = connection
        .query_row(
            "SELECT provider_session_cwd, codex_home FROM agents WHERE id = 'agent'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(version, 13);
    assert_eq!(stored, ("/work/exact-old-cwd".into(), None));
    let unbound_cwd: Option<String> = connection
        .query_row(
            "SELECT provider_session_cwd FROM agents WHERE id = 'unbound-agent'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(unbound_cwd, None);
    let ambiguous_cwd: Option<String> = connection
        .query_row(
            "SELECT provider_session_cwd FROM agents WHERE id = 'ambiguous-agent'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(ambiguous_cwd, None);
}
