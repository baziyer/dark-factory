use factory_core::{
    AgentId, AgentRole, FactoryEvent, MessageId, ProjectId, Provider, RunId, RunnerInstanceId,
    SessionId, TaskId, TaskStatus, local::MAX_TASK_BODY_BYTES,
};
use factoryd::store::{
    ConnectorEventInput, ConnectorEventResult, NewAgent, NewAgentMessage, NewProject, NewSession,
    NewTask, Store, StoreError, UpdateAgentProfile,
};
use std::sync::{
    Arc, Barrier,
    atomic::{AtomicBool, Ordering},
};

struct TrackedStore {
    store: Store,
    dropped: Option<Arc<AtomicBool>>,
}

impl std::ops::Deref for TrackedStore {
    type Target = Store;

    fn deref(&self) -> &Self::Target {
        &self.store
    }
}

impl std::ops::DerefMut for TrackedStore {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.store
    }
}

impl Drop for TrackedStore {
    fn drop(&mut self) {
        if let Some(dropped) = &self.dropped {
            dropped.store(true, Ordering::SeqCst);
        }
    }
}

fn prepare_stores(
    paths: [std::path::PathBuf; 2],
    first_store_dropped: Option<Arc<AtomicBool>>,
) -> Result<(TrackedStore, TrackedStore), String> {
    let first = Store::open(&paths[0])
        .map_err(|error| format!("store setup {}: {error}", paths[0].display()))?;
    let first = TrackedStore {
        store: first,
        dropped: first_store_dropped,
    };
    let second = Store::open(&paths[1])
        .map_err(|error| format!("store setup {}: {error}", paths[1].display()))?;
    Ok((
        first,
        TrackedStore {
            store: second,
            dropped: None,
        },
    ))
}

#[test]
fn store_fixture_setup_failure_reports_a_bounded_diagnostic() {
    let directory = tempfile::tempdir().unwrap();
    let first_database = directory.path().join("factory.db");
    let invalid_database = directory.path().join("directory-database");
    std::fs::create_dir(&invalid_database).unwrap();
    let first_store_dropped = Arc::new(AtomicBool::new(false));

    let error = match prepare_stores(
        [first_database.clone(), invalid_database.clone()],
        Some(Arc::clone(&first_store_dropped)),
    ) {
        Ok(_) => panic!("a directory path must fail store setup"),
        Err(error) => error,
    };
    assert!(first_database.is_file(), "first store was not opened");
    assert!(
        error.contains(&format!("store setup {}", invalid_database.display())),
        "unexpected diagnostic: {error}"
    );
    assert!(
        first_store_dropped.load(Ordering::SeqCst),
        "first store was not dropped after second setup failed"
    );
}

#[test]
fn connector_event_idempotency_is_atomic_and_survives_restart() {
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("factory.db");
    let first_id;
    {
        let mut store = Store::open(&database).unwrap();
        store
            .create_project(
                NewProject {
                    id: project_id("factory"),
                    name: "Factory".into(),
                    root: "/work".into(),
                },
                1,
            )
            .unwrap();
        let input = ConnectorEventInput::Task {
            id: task_id("connector-one"),
            project_id: project_id("factory"),
            title: "Imported".into(),
            body: "Do it".into(),
            priority: 0,
        };
        let (result, events) = store
            .apply_connector_event("monitor", "evt-1", [1; 32], input.clone(), 2)
            .unwrap();
        let ConnectorEventResult::Accepted { id, .. } = result else {
            panic!("first event was not accepted")
        };
        first_id = id;
        assert_eq!(events.len(), 1);
        let (duplicate, events) = store
            .apply_connector_event("monitor", "evt-1", [1; 32], input, 3)
            .unwrap();
        assert_eq!(
            duplicate,
            ConnectorEventResult::Duplicate {
                kind: "task".into(),
                id: first_id.clone()
            }
        );
        assert!(events.is_empty());
    }
    let mut reopened = Store::open(&database).unwrap();
    let (duplicate, events) = reopened
        .apply_connector_event(
            "monitor",
            "evt-1",
            [1; 32],
            ConnectorEventInput::Task {
                id: task_id("connector-one"),
                project_id: project_id("factory"),
                title: "Imported".into(),
                body: "Do it".into(),
                priority: 0,
            },
            4,
        )
        .unwrap();
    assert_eq!(
        duplicate,
        ConnectorEventResult::Duplicate {
            kind: "task".into(),
            id: first_id
        }
    );
    assert!(events.is_empty());
    assert!(matches!(
        reopened.apply_connector_event(
            "monitor",
            "evt-1",
            [2; 32],
            ConnectorEventInput::Message {
                id: factory_core::MessageId::try_from("different").unwrap(),
                project_id: project_id("factory"),
                recipient_agent_id: agent_id("different-target"),
                body: "Changed".into(),
            },
            5,
        ),
        Err(StoreError::ConnectorEventPayloadMismatch)
    ));
}

#[test]
fn rejected_unknown_connector_target_does_not_consume_event_id() {
    let mut store = Store::open_in_memory().unwrap();
    let input = ConnectorEventInput::Task {
        id: task_id("connector-later"),
        project_id: project_id("later"),
        title: "Imported".into(),
        body: "Do it".into(),
        priority: 0,
    };
    assert!(matches!(
        store.apply_connector_event("monitor", "evt-later", [3; 32], input.clone(), 1),
        Err(StoreError::WebhookProjectNotFound)
    ));
    store
        .create_project(
            NewProject {
                id: project_id("later"),
                name: "Later".into(),
                root: "/work".into(),
            },
            2,
        )
        .unwrap();
    assert!(matches!(
        store
            .apply_connector_event("monitor", "evt-later", [3; 32], input, 3)
            .unwrap()
            .0,
        ConnectorEventResult::Accepted { .. }
    ));
}

#[test]
fn concurrent_mismatched_connector_events_keep_the_first_payload_only() {
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("factory.db");
    let mut setup = Store::open(&database).unwrap();
    setup
        .create_project(
            NewProject {
                id: project_id("factory"),
                name: "Factory".into(),
                root: "/work".into(),
            },
            1,
        )
        .unwrap();
    drop(setup);

    let (first_store, second_store) = prepare_stores([database.clone(), database.clone()], None)
        .unwrap_or_else(|error| panic!("store setup failed: {error}"));
    let barrier = Arc::new(Barrier::new(2));
    let results = std::thread::scope(|scope| {
        let handles = [(1_u8, first_store), (2_u8, second_store)].map(|(variant, mut store)| {
            let barrier = Arc::clone(&barrier);
            scope.spawn(move || {
                barrier.wait();
                store.apply_connector_event(
                    "monitor",
                    "evt-race",
                    [variant; 32],
                    ConnectorEventInput::Task {
                        id: task_id(&format!("connector-{variant}")),
                        project_id: project_id("factory"),
                        title: format!("Imported {variant}"),
                        body: format!("Payload {variant}"),
                        priority: 0,
                    },
                    i64::from(variant) + 1,
                )
            })
        });
        handles.map(|handle| handle.join().unwrap())
    });
    assert_eq!(
        results
            .iter()
            .filter(|result| matches!(result, Ok((ConnectorEventResult::Accepted { .. }, _))))
            .count(),
        1
    );
    assert_eq!(
        results
            .iter()
            .filter(|result| matches!(result, Err(StoreError::ConnectorEventPayloadMismatch)))
            .count(),
        1
    );
    assert_eq!(
        Store::open(&database)
            .unwrap()
            .list_tasks(&project_id("factory"), None, 10)
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn connector_tasks_share_cli_title_normalization_and_body_bound() {
    let mut store = Store::open_in_memory().unwrap();
    store
        .create_project(
            NewProject {
                id: project_id("factory"),
                name: "Factory".into(),
                root: "/work".into(),
            },
            1,
        )
        .unwrap();
    store
        .apply_connector_event(
            "monitor",
            "evt-boundary",
            [4; 32],
            ConnectorEventInput::Task {
                id: task_id("connector-boundary"),
                project_id: project_id("factory"),
                title: "  Imported  ".into(),
                body: "x".repeat(MAX_TASK_BODY_BYTES),
                priority: 0,
            },
            2,
        )
        .unwrap();
    let task = store
        .get_task(&project_id("factory"), &task_id("connector-boundary"))
        .unwrap();
    assert_eq!(task.snapshot.title, "Imported");
    assert_eq!(task.body.len(), MAX_TASK_BODY_BYTES);

    for (event_id, digest, title, body) in [
        ("evt-empty-title", [5; 32], "  ".to_owned(), String::new()),
        (
            "evt-large-body",
            [6; 32],
            "Imported".to_owned(),
            "x".repeat(MAX_TASK_BODY_BYTES + 1),
        ),
    ] {
        assert!(matches!(
            store.apply_connector_event(
                "monitor",
                event_id,
                digest,
                ConnectorEventInput::Task {
                    id: task_id(event_id),
                    project_id: project_id("factory"),
                    title,
                    body,
                    priority: 0,
                },
                3,
            ),
            Err(StoreError::InvalidTaskInput)
        ));
    }
}

fn project_id(value: &str) -> ProjectId {
    ProjectId::try_from(value).unwrap()
}

fn task_id(value: &str) -> TaskId {
    TaskId::try_from(value).unwrap()
}

fn agent_id(value: &str) -> AgentId {
    AgentId::try_from(value).unwrap()
}

#[test]
fn tool_call_budget_is_durable_exhausts_and_requires_reset() {
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("factory.db");
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
                    id: agent_id("worker"),
                    project_id: project_id("factory"),
                    parent_agent_id: None,
                    role: AgentRole::Worker,
                    provider: Provider::Shell,
                },
                2,
            )
            .unwrap();
        let (configured, _) = store
            .set_agent_budget(&project_id("factory"), &agent_id("worker"), Some(1), 3)
            .unwrap();
        assert_eq!(configured.max_tool_calls, Some(1));
        let (_, denied, _) = store
            .observe_tool_call(&project_id("factory"), &agent_id("worker"), 4)
            .unwrap();
        assert!(!denied);
    }
    let mut store = Store::open(&database).unwrap();
    let (exhausted, denied, event) = store
        .observe_tool_call(&project_id("factory"), &agent_id("worker"), 5)
        .unwrap();
    assert!(denied);
    assert!(exhausted.exhausted);
    assert!(
        store
            .agent_status(&project_id("factory"), &agent_id("worker"))
            .unwrap()
            .agent
            .paused
    );
    assert!(
        matches!(event.event, FactoryEvent::AgentBudgetChanged { action, .. } if action == "denied")
    );
    assert!(matches!(
        store.resume_agent(&project_id("factory"), &agent_id("worker"), 6),
        Err(StoreError::AgentBudgetExhausted)
    ));
    let (reset, _) = store
        .reset_agent_budget(&project_id("factory"), &agent_id("worker"), 7)
        .unwrap();
    assert_eq!(reset.tool_calls, 0);
    assert!(!reset.exhausted);
    assert!(
        !store
            .agent_status(&project_id("factory"), &agent_id("worker"))
            .unwrap()
            .agent
            .paused
    );

    // The independent ordinary hold survives budget exhaustion and reset.
    store
        .pause_agent(&project_id("factory"), &agent_id("worker"), 8)
        .unwrap();
    assert!(
        !store
            .observe_tool_call(&project_id("factory"), &agent_id("worker"), 9)
            .unwrap()
            .1
    );
    assert!(
        store
            .observe_tool_call(&project_id("factory"), &agent_id("worker"), 10)
            .unwrap()
            .1
    );
    store
        .reset_agent_budget(&project_id("factory"), &agent_id("worker"), 11)
        .unwrap();
    let status = store
        .agent_status(&project_id("factory"), &agent_id("worker"))
        .unwrap();
    assert!(status.agent.paused);
    assert_eq!(
        status.pause_reasons,
        vec![factory_core::status::AgentPauseReason::AgentHold]
    );
    store
        .resume_agent(&project_id("factory"), &agent_id("worker"), 12)
        .unwrap();
    assert!(
        !store
            .agent_is_held(&project_id("factory"), &agent_id("worker"))
            .unwrap()
    );
}

#[test]
fn concurrent_tool_observations_cannot_cross_the_limit() {
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("factory.db");
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
                id: agent_id("worker"),
                project_id: project_id("factory"),
                parent_agent_id: None,
                role: AgentRole::Worker,
                provider: Provider::Shell,
            },
            2,
        )
        .unwrap();
    store
        .set_agent_budget(&project_id("factory"), &agent_id("worker"), Some(1), 3)
        .unwrap();
    drop(store);

    let (first_store, second_store) = prepare_stores([database.clone(), database.clone()], None)
        .unwrap_or_else(|error| panic!("store setup failed: {error}"));
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
    let mut joins = Vec::new();
    for (now, mut store) in [(4, first_store), (5, second_store)] {
        let barrier = barrier.clone();
        joins.push(std::thread::spawn(move || {
            barrier.wait();
            store
                .observe_tool_call(&project_id("factory"), &agent_id("worker"), now)
                .unwrap()
                .1
        }));
    }
    let denied = joins
        .into_iter()
        .map(|join| join.join().unwrap())
        .filter(|denied| *denied)
        .count();
    assert_eq!(denied, 1);
    let store = Store::open(database).unwrap();
    let budget = store
        .agent_budget(&project_id("factory"), &agent_id("worker"))
        .unwrap();
    assert_eq!(budget.tool_calls, 1);
    assert!(budget.exhausted);
}

/// Creates a live session for `agent` and opens a task-episode for `task`
/// inside it, mirroring what the old `reserve_task_run` helper below used
/// to do in one call. `seed` must be unique per call within a test (it
/// seeds the session/runner identity).
fn open_episode(
    store: &mut Store,
    project: &str,
    task: &str,
    agent: &str,
    seed: &str,
    now: i64,
) -> RunId {
    let session_id = SessionId::try_from(format!("session-{seed}")).unwrap();
    store
        .create_session(
            NewSession {
                id: session_id.clone(),
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
            },
            now,
        )
        .unwrap();
    store
        .assign_task(
            &project_id(project),
            &task_id(task),
            Some(&agent_id(agent)),
            now,
        )
        .unwrap();
    store
        .open_run_episode(&session_id, &task_id(task), now)
        .unwrap()
        .run
        .id
}

/// Closes `seed`'s session as an unverifiable process exit -- the episode
/// (if still open) closes `failed`/`process`, `closed_by = session_ended`,
/// mirroring the old `fail_run_launch`/`fail_run_unverifiable` helpers'
/// role in these tests: making a run terminal without revealing anything
/// about *why*.
fn end_episode(store: &mut Store, seed: &str, now: i64) {
    let session_id = SessionId::try_from(format!("session-{seed}")).unwrap();
    store.end_session(&session_id, None, None, now).unwrap();
}

#[test]
fn project_task_and_events_survive_reopen() {
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("factory.db");

    {
        let mut store = Store::open(&database).unwrap();
        let (project, project_event) = store
            .create_project(
                NewProject {
                    id: project_id("dark-factory"),
                    name: "Dark Factory".into(),
                    root: "/work/dark-factory".into(),
                },
                1_000,
            )
            .unwrap();
        let (task, task_event) = store
            .create_task(
                NewTask {
                    id: task_id("task-1"),
                    project_id: project.id.clone(),
                    parent_task_id: None,
                    title: "Prove persistence".into(),
                    body: "Create state and reopen the database.".into(),
                    priority: 10,
                },
                2_000,
            )
            .unwrap();

        assert_eq!(project_event.sequence, 1);
        assert_eq!(task_event.sequence, 2);
        assert_eq!(task.snapshot.title, "Prove persistence");
        assert_eq!(task.body, "Create state and reopen the database.");
    }

    let store = Store::open(&database).unwrap();
    let projects = store.list_projects(None, 100).unwrap();
    let tasks = store
        .list_tasks(&project_id("dark-factory"), None, 100)
        .unwrap();
    let events = store.events_after(0, 100).unwrap();

    assert_eq!(projects.len(), 1);
    assert_eq!(projects[0].name, "Dark Factory");
    assert_eq!(tasks.len(), 1);
    assert_eq!(tasks[0].snapshot.id, task_id("task-1"));
    assert_eq!(events.len(), 2);
    assert_eq!(events[0].sequence, 1);
    assert_eq!(events[1].sequence, 2);
}

#[test]
fn v1_durable_events_replay_with_the_current_store_protocol() {
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("factory.db");
    {
        let mut store = Store::open(&database).unwrap();
        store
            .create_project(
                NewProject {
                    id: project_id("legacy"),
                    name: "Legacy".into(),
                    root: "/work/legacy".into(),
                },
                1,
            )
            .unwrap();
    }
    let connection = rusqlite::Connection::open(&database).unwrap();
    connection
        .execute("UPDATE events SET schema_version = 1", [])
        .unwrap();
    drop(connection);

    let store = Store::open(&database).unwrap();
    let events = store.events_after(0, 10).unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].protocol_version, 1);
}

#[test]
fn a_rejected_state_change_does_not_append_an_event() {
    let mut store = Store::open_in_memory().unwrap();
    let input = NewProject {
        id: project_id("project-1"),
        name: "One".into(),
        root: "/work/one".into(),
    };

    store.create_project(input.clone(), 1_000).unwrap();
    assert!(store.create_project(input, 2_000).is_err());

    let events = store.events_after(0, 100).unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].occurred_at_ms, 1_000);
}

#[test]
fn event_replay_respects_cursor_and_limit() {
    let mut store = Store::open_in_memory().unwrap();
    assert_eq!(store.latest_event_sequence().unwrap(), 0);
    for index in 1..=3 {
        store
            .create_project(
                NewProject {
                    id: project_id(&format!("project-{index}")),
                    name: format!("Project {index}"),
                    root: format!("/work/{index}"),
                },
                index,
            )
            .unwrap();
    }

    let events = store.events_after(1, 1).unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].sequence, 2);
    assert_eq!(store.latest_event_sequence().unwrap(), 3);
}

#[test]
fn task_parents_must_be_distinct_and_in_the_same_project() {
    let mut store = Store::open_in_memory().unwrap();
    for (id, root) in [("project-1", "/work/one"), ("project-2", "/work/two")] {
        store
            .create_project(
                NewProject {
                    id: project_id(id),
                    name: id.into(),
                    root: root.into(),
                },
                1_000,
            )
            .unwrap();
    }
    store
        .create_task(
            NewTask {
                id: task_id("parent"),
                project_id: project_id("project-1"),
                parent_task_id: None,
                title: "Parent".into(),
                body: String::new(),
                priority: 0,
            },
            2_000,
        )
        .unwrap();
    store
        .create_task(
            NewTask {
                id: task_id("valid-child"),
                project_id: project_id("project-1"),
                parent_task_id: Some(task_id("parent")),
                title: "Valid child".into(),
                body: String::new(),
                priority: 0,
            },
            2_500,
        )
        .unwrap();

    let cross_project = store.create_task(
        NewTask {
            id: task_id("cross-project"),
            project_id: project_id("project-2"),
            parent_task_id: Some(task_id("parent")),
            title: "Invalid child".into(),
            body: String::new(),
            priority: 0,
        },
        3_000,
    );
    let self_parent = store.create_task(
        NewTask {
            id: task_id("self-parent"),
            project_id: project_id("project-1"),
            parent_task_id: Some(task_id("self-parent")),
            title: "Invalid self parent".into(),
            body: String::new(),
            priority: 0,
        },
        3_000,
    );

    assert!(cross_project.is_err());
    assert!(self_parent.is_err());
    assert_eq!(store.events_after(0, 100).unwrap().len(), 4);
}

#[test]
fn list_pages_use_a_stable_id_cursor() {
    let mut store = Store::open_in_memory().unwrap();
    for id in ["project-c", "project-a", "project-b"] {
        store
            .create_project(
                NewProject {
                    id: project_id(id),
                    name: id.into(),
                    root: format!("/work/{id}"),
                },
                1_000,
            )
            .unwrap();
    }
    for id in ["task-c", "task-a", "task-b"] {
        store
            .create_task(
                NewTask {
                    id: task_id(id),
                    project_id: project_id("project-a"),
                    parent_task_id: None,
                    title: id.into(),
                    body: String::new(),
                    priority: 0,
                },
                2_000,
            )
            .unwrap();
    }

    let projects = store.list_projects(None, 2).unwrap();
    assert_eq!(
        projects
            .iter()
            .map(|project| project.id.as_str())
            .collect::<Vec<_>>(),
        ["project-a", "project-b"]
    );
    let projects = store
        .list_projects(Some(&project_id("project-b")), 2)
        .unwrap();
    assert_eq!(projects[0].id, project_id("project-c"));

    let (tasks, revision) = store
        .list_tasks_filtered_at_revision(&project_id("project-a"), None, None, true, 2, None)
        .unwrap();
    assert_eq!(
        tasks
            .iter()
            .map(|task| task.snapshot.id.as_str())
            .collect::<Vec<_>>(),
        ["task-a", "task-b"]
    );
    let tasks = store
        .list_tasks_filtered_at_revision(
            &project_id("project-a"),
            Some(&task_id("task-b")),
            None,
            true,
            2,
            Some(revision),
        )
        .unwrap()
        .0;
    assert_eq!(tasks[0].snapshot.id, task_id("task-c"));
}

#[test]
fn failed_tasks_can_be_requeued_without_losing_assignment_or_history() {
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("factory.db");
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
        .create_task(
            NewTask {
                id: task_id("task-1"),
                project_id: project_id("factory"),
                parent_task_id: None,
                title: "Retry me".into(),
                body: "Try again".into(),
                priority: 0,
            },
            2,
        )
        .unwrap();
    store
        .create_agent(
            NewAgent {
                id: AgentId::try_from("god").unwrap(),
                project_id: project_id("factory"),
                parent_agent_id: None,
                role: AgentRole::Orchestrator,
                provider: Provider::Codex,
            },
            3,
        )
        .unwrap();
    open_episode(&mut store, "factory", "task-1", "god", "run-1", 4);
    end_episode(&mut store, "run-1", 5);

    let (task, event) = store
        .retry_task(&project_id("factory"), &task_id("task-1"), 6)
        .unwrap();
    assert_eq!(task.snapshot.status, TaskStatus::Queued);
    assert_eq!(
        task.snapshot.assigned_agent_id,
        Some(AgentId::try_from("god").unwrap())
    );
    assert_eq!(task.result, None);
    assert!(matches!(
        event.event,
        factory_core::FactoryEvent::TaskChanged { .. }
    ));
    let snapshot = store
        .webhook_snapshot(
            &project_id("factory"),
            &AgentId::try_from("god").unwrap(),
            7,
        )
        .unwrap();
    assert_eq!(snapshot.tasks[0].started_at_ms, None);
    assert_eq!(snapshot.tasks[0].completed_at_ms, None);
    assert!(!store.events_after(0, 100).unwrap().is_empty());
    drop(store);

    let reopened = Store::open(&database).unwrap();
    let snapshot = reopened
        .webhook_snapshot(
            &project_id("factory"),
            &AgentId::try_from("god").unwrap(),
            8,
        )
        .unwrap();
    assert_eq!(
        snapshot.tasks[0].status,
        factoryd::store::OperationalTaskStatus::Todo
    );
    assert_eq!(snapshot.tasks[0].started_at_ms, None);
}

#[test]
fn queued_tasks_can_be_assigned_unassigned_and_reopened() {
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("factory.db");
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
    for id in ["curie", "turing"] {
        store
            .create_agent(
                NewAgent {
                    id: AgentId::try_from(id).unwrap(),
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
                id: task_id("task-1"),
                project_id: project_id("factory"),
                parent_task_id: None,
                title: "Queue me".into(),
                body: "body".into(),
                priority: 0,
            },
            3,
        )
        .unwrap();

    let (assigned, assigned_event) = store
        .assign_task(
            &project_id("factory"),
            &task_id("task-1"),
            Some(&AgentId::try_from("curie").unwrap()),
            4,
        )
        .unwrap();
    assert_eq!(
        assigned.snapshot.assigned_agent_id,
        Some(AgentId::try_from("curie").unwrap())
    );
    assert!(matches!(
        assigned_event.event,
        factory_core::FactoryEvent::TaskChanged { .. }
    ));

    let (reassigned, _) = store
        .assign_task(
            &project_id("factory"),
            &task_id("task-1"),
            Some(&AgentId::try_from("turing").unwrap()),
            5,
        )
        .unwrap();
    assert_eq!(
        reassigned.snapshot.assigned_agent_id,
        Some(AgentId::try_from("turing").unwrap())
    );

    let (unassigned, _) = store
        .assign_task(&project_id("factory"), &task_id("task-1"), None, 6)
        .unwrap();
    assert_eq!(unassigned.snapshot.assigned_agent_id, None);
    drop(store);

    let reopened = Store::open(&database).unwrap();
    assert_eq!(
        reopened
            .get_task(&project_id("factory"), &task_id("task-1"))
            .unwrap()
            .snapshot
            .assigned_agent_id,
        None
    );
}

#[test]
fn assigned_creation_is_atomic_and_filtered_queue_order_survives_restart() {
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("factory.db");
    let alice = agent_id("alice");
    let bob = agent_id("bob");
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
        for id in [&alice, &bob] {
            store
                .create_agent(
                    NewAgent {
                        id: id.clone(),
                        project_id: project_id("factory"),
                        parent_agent_id: None,
                        role: AgentRole::Worker,
                        provider: Provider::Shell,
                    },
                    2,
                )
                .unwrap();
        }
        let (task, _) = store
            .create_assigned_task(
                NewTask {
                    id: task_id("alice-first"),
                    project_id: project_id("factory"),
                    parent_task_id: None,
                    title: "First".into(),
                    body: String::new(),
                    priority: 9,
                },
                alice.clone(),
                3,
            )
            .unwrap();
        assert_eq!(task.snapshot.assigned_agent_id, Some(alice.clone()));
        store
            .create_task(
                NewTask {
                    id: task_id("backlog"),
                    project_id: project_id("factory"),
                    parent_task_id: None,
                    title: "Backlog".into(),
                    body: String::new(),
                    priority: 0,
                },
                4,
            )
            .unwrap();
        store
            .create_assigned_task(
                NewTask {
                    id: task_id("alice-second"),
                    project_id: project_id("factory"),
                    parent_task_id: None,
                    title: "Second".into(),
                    body: String::new(),
                    priority: 0,
                },
                alice.clone(),
                5,
            )
            .unwrap();
        assert!(matches!(
            store.create_assigned_task(
                NewTask {
                    id: task_id("bad-agent"),
                    project_id: project_id("factory"),
                    parent_task_id: None,
                    title: "No delivery".into(),
                    body: String::new(),
                    priority: 0,
                },
                AgentId::try_from("deleted").unwrap(),
                6,
            ),
            Err(StoreError::AgentNotFound)
        ));
        assert!(matches!(
            store.get_task(&project_id("factory"), &task_id("bad-agent")),
            Err(StoreError::TaskNotFound)
        ));
    }

    let mut store = Store::open(&database).unwrap();
    let (first_page, revision) = store
        .list_tasks_filtered_at_revision(&project_id("factory"), None, Some(&alice), false, 1, None)
        .unwrap();
    assert_eq!(first_page[0].snapshot.id, task_id("alice-first"));
    store
        .update_task(
            &project_id("factory"),
            &task_id("alice-first"),
            None,
            None,
            Some(-1),
            7,
        )
        .unwrap();
    assert!(matches!(
        store.list_tasks_filtered_at_revision(
            &project_id("factory"),
            Some(&first_page[0].snapshot.id),
            Some(&alice),
            false,
            10,
            Some(revision),
        ),
        Err(StoreError::StaleTaskCursor)
    ));
    assert!(matches!(
        store.list_tasks_filtered(
            &project_id("factory"),
            Some(&first_page[0].snapshot.id),
            Some(&alice),
            false,
            10,
        ),
        Err(StoreError::MissingTaskCursorRevision)
    ));
    let second_page = store
        .list_tasks_filtered(&project_id("factory"), None, Some(&alice), false, 10)
        .unwrap();
    assert_eq!(
        second_page
            .iter()
            .map(|task| task.snapshot.id.clone())
            .collect::<Vec<_>>(),
        vec![task_id("alice-second"), task_id("alice-first")]
    );
    store
        .assign_task(&project_id("factory"), &task_id("alice-first"), None, 8)
        .unwrap();
    let (_, revision) = store
        .list_tasks_filtered_at_revision(&project_id("factory"), None, None, false, 1, None)
        .unwrap();
    assert!(matches!(
        store.list_tasks_filtered_at_revision(
            &project_id("factory"),
            Some(&first_page[0].snapshot.id),
            Some(&alice),
            false,
            10,
            Some(revision - 1),
        ),
        Err(StoreError::StaleTaskCursor)
    ));
    assert_eq!(
        store
            .list_tasks_filtered(&project_id("factory"), None, None, false, 10)
            .unwrap()
            .iter()
            .map(|task| task.snapshot.id.clone())
            .collect::<Vec<_>>(),
        vec![
            task_id("backlog"),
            task_id("alice-second"),
            task_id("alice-first")
        ]
    );
    assert_eq!(bob, agent_id("bob"));
}

#[test]
fn cancel_task_moves_queued_or_blocked_to_cancelled_and_keeps_assignment() {
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
                title: "Cancel me".into(),
                body: "body".into(),
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

    let (cancelled, event) = store
        .cancel_task(&project_id("factory"), &task_id("task-1"), 5)
        .unwrap();
    assert_eq!(cancelled.snapshot.status, TaskStatus::Cancelled);
    assert_eq!(
        cancelled.snapshot.assigned_agent_id,
        Some(agent_id("curie"))
    );
    assert!(matches!(event.event, FactoryEvent::TaskChanged { .. }));

    assert!(matches!(
        store.cancel_task(&project_id("factory"), &task_id("task-1"), 6),
        Err(StoreError::TaskNotCancellable)
    ));

    let (retried, _) = store
        .retry_task(&project_id("factory"), &task_id("task-1"), 7)
        .unwrap();
    assert_eq!(retried.snapshot.status, TaskStatus::Queued);

    // A running task (an open episode) can also be cancelled: the episode
    // closes `operator_cancel`, the task moves to `cancelled`, and the
    // session is left untouched (D1/WIRE lifecycle recap).
    open_episode(&mut store, "factory", "task-1", "curie", "run-1", 8);
    let (cancelled, event) = store
        .cancel_task(&project_id("factory"), &task_id("task-1"), 9)
        .unwrap();
    assert_eq!(cancelled.snapshot.status, TaskStatus::Cancelled);
    assert!(matches!(event.event, FactoryEvent::TaskChanged { .. }));

    assert!(matches!(
        store.cancel_task(&project_id("factory"), &task_id("task-1"), 10),
        Err(StoreError::TaskNotCancellable)
    ));
}

#[test]
fn update_task_edits_a_queued_task_and_rejects_a_running_one() {
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
                title: "Original".into(),
                body: "Original body".into(),
                priority: 0,
            },
            3,
        )
        .unwrap();

    let (updated, event) = store
        .update_task(
            &project_id("factory"),
            &task_id("task-1"),
            Some("New title".into()),
            None,
            None,
            4,
        )
        .unwrap();
    assert_eq!(updated.snapshot.title, "New title");
    assert_eq!(updated.body, "Original body");
    assert!(matches!(event.event, FactoryEvent::TaskChanged { .. }));

    let (updated, _) = store
        .update_task(
            &project_id("factory"),
            &task_id("task-1"),
            None,
            Some("New body".into()),
            None,
            5,
        )
        .unwrap();
    assert_eq!(updated.snapshot.title, "New title");
    assert_eq!(updated.body, "New body");

    open_episode(&mut store, "factory", "task-1", "curie", "run-1", 6);
    assert!(matches!(
        store.update_task(
            &project_id("factory"),
            &task_id("task-1"),
            Some("Too late".into()),
            None,
            None,
            7,
        ),
        Err(StoreError::TaskNotEditable)
    ));
}

#[test]
fn delete_task_requires_no_active_run_and_no_subtasks() {
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
                id: task_id("parent"),
                project_id: project_id("factory"),
                parent_task_id: None,
                title: "Parent".into(),
                body: String::new(),
                priority: 0,
            },
            3,
        )
        .unwrap();
    store
        .create_task(
            NewTask {
                id: task_id("child"),
                project_id: project_id("factory"),
                parent_task_id: Some(task_id("parent")),
                title: "Child".into(),
                body: String::new(),
                priority: 0,
            },
            4,
        )
        .unwrap();

    assert!(matches!(
        store.delete_task(&project_id("factory"), &task_id("parent"), 5),
        Err(StoreError::TaskHasSubtasks)
    ));

    let event = store
        .delete_task(&project_id("factory"), &task_id("child"), 6)
        .unwrap();
    assert!(matches!(event.event, FactoryEvent::TaskDeleted { .. }));
    assert!(
        store
            .list_tasks(&project_id("factory"), None, 100)
            .unwrap()
            .iter()
            .all(|task| task.snapshot.id != task_id("child"))
    );

    open_episode(&mut store, "factory", "parent", "curie", "run-1", 7);
    assert!(matches!(
        store.delete_task(&project_id("factory"), &task_id("parent"), 8),
        Err(StoreError::TaskHasActiveRun)
    ));

    end_episode(&mut store, "run-1", 9);
    let event = store
        .delete_task(&project_id("factory"), &task_id("parent"), 10)
        .unwrap();
    assert!(matches!(event.event, FactoryEvent::TaskDeleted { .. }));
    assert!(
        store
            .list_tasks(&project_id("factory"), None, 100)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn recreating_a_deleted_task_id_gets_a_new_immutable_incarnation() {
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
    let create = |title: &str| NewTask {
        id: task_id("task-1"),
        project_id: project_id("factory"),
        parent_task_id: None,
        title: title.into(),
        body: String::new(),
        priority: 0,
    };
    store.create_task(create("Original"), 2).unwrap();
    let original = store
        .task_delivery_marker(&task_id("task-1"))
        .unwrap()
        .incarnation_id;

    store
        .delete_task(&project_id("factory"), &task_id("task-1"), 3)
        .unwrap();
    store.create_task(create("Replacement"), 4).unwrap();
    let replacement = store
        .task_delivery_marker(&task_id("task-1"))
        .unwrap()
        .incarnation_id;

    assert_ne!(original, replacement);
}

#[test]
fn delete_agent_requires_no_open_run_and_unassigns_its_tasks() {
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
                title: "Queued".into(),
                body: String::new(),
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

    open_episode(&mut store, "factory", "task-1", "curie", "run-1", 5);
    assert!(matches!(
        store.delete_agent(&project_id("factory"), &agent_id("curie"), 6),
        Err(StoreError::AgentHasActiveRun)
    ));

    end_episode(&mut store, "run-1", 7);
    store
        .retry_task(&project_id("factory"), &task_id("task-1"), 8)
        .unwrap();
    store
        .assign_task(
            &project_id("factory"),
            &task_id("task-1"),
            Some(&agent_id("curie")),
            9,
        )
        .unwrap();

    store
        .create_agent(
            NewAgent {
                id: agent_id("child-of-curie"),
                project_id: project_id("factory"),
                parent_agent_id: Some(agent_id("curie")),
                role: AgentRole::Worker,
                provider: Provider::Codex,
            },
            10,
        )
        .unwrap();
    assert!(matches!(
        store.delete_agent(&project_id("factory"), &agent_id("curie"), 11),
        Err(StoreError::AgentHasChildren)
    ));
    store
        .delete_agent(&project_id("factory"), &agent_id("child-of-curie"), 12)
        .unwrap();

    let events = store
        .delete_agent(&project_id("factory"), &agent_id("curie"), 13)
        .unwrap();
    assert!(
        events
            .iter()
            .any(|event| matches!(&event.event, FactoryEvent::AgentDeleted { .. }))
    );
    assert!(events.iter().any(|event| matches!(
        &event.event,
        FactoryEvent::TaskChanged { task } if task.assigned_agent_id.is_none()
    )));
    assert_eq!(
        store
            .get_task(&project_id("factory"), &task_id("task-1"))
            .unwrap()
            .snapshot
            .assigned_agent_id,
        None
    );
    assert!(
        store
            .list_agents(&project_id("factory"), None, 100)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn delete_project_requires_no_active_run_and_cascades() {
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
                body: String::new(),
                priority: 0,
            },
            3,
        )
        .unwrap();

    open_episode(&mut store, "factory", "task-1", "curie", "run-1", 4);
    assert!(matches!(
        store.delete_project(&project_id("factory"), 5),
        Err(StoreError::ProjectHasActiveRun)
    ));

    end_episode(&mut store, "run-1", 6);

    let event = store.delete_project(&project_id("factory"), 7).unwrap();
    assert!(matches!(event.event, FactoryEvent::ProjectDeleted { .. }));
    assert!(
        store
            .list_projects(None, 100)
            .unwrap()
            .iter()
            .all(|project| project.id != project_id("factory"))
    );
    assert!(
        store
            .list_tasks(&project_id("factory"), None, 100)
            .unwrap()
            .is_empty()
    );
    assert!(
        store
            .list_agents(&project_id("factory"), None, 100)
            .unwrap()
            .is_empty()
    );

    assert!(matches!(
        store.delete_project(&project_id("factory"), 8),
        Err(StoreError::ProjectNotFound)
    ));
}

#[test]
fn delete_agent_deletes_its_profile_and_inbox_but_keeps_sent_messages_with_sender_cleared() {
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
        .create_agent(
            NewAgent {
                id: agent_id("god"),
                project_id: project_id("factory"),
                parent_agent_id: None,
                role: AgentRole::Orchestrator,
                provider: Provider::Codex,
            },
            3,
        )
        .unwrap();

    // Give curie a profile row; if the delete cascade forgets to remove it,
    // the deferred foreign key check at commit time fails the delete below.
    store
        .update_agent_profile(
            &project_id("factory"),
            &agent_id("curie"),
            UpdateAgentProfile {
                model: Some("gpt-5.6-luna".into()),
                reasoning_effort: None,
                model_selection_reason: None,
                permission_mode: None,
            },
            4,
        )
        .unwrap();

    // A message curie sent to god: history for god, should survive.
    store
        .send_agent_message(NewAgentMessage {
            id: MessageId::try_from("message-sent-by-curie").unwrap(),
            project_id: project_id("factory"),
            sender_agent_id: Some(agent_id("curie")),
            recipient_agent_id: agent_id("god"),
            body: "Status update.".into(),
            created_at_ms: 5,
        })
        .unwrap();
    // A message addressed to curie: it's curie's inbox, should be deleted.
    store
        .send_agent_message(NewAgentMessage {
            id: MessageId::try_from("message-to-curie").unwrap(),
            project_id: project_id("factory"),
            sender_agent_id: Some(agent_id("god")),
            recipient_agent_id: agent_id("curie"),
            body: "New instructions.".into(),
            created_at_ms: 6,
        })
        .unwrap();

    store
        .delete_agent(&project_id("factory"), &agent_id("curie"), 7)
        .unwrap();

    let gods_inbox = store
        .list_agent_messages(&project_id("factory"), &agent_id("god"), None, 100)
        .unwrap();
    assert_eq!(gods_inbox.len(), 1);
    assert_eq!(
        gods_inbox[0].id,
        MessageId::try_from("message-sent-by-curie").unwrap()
    );
    assert_eq!(gods_inbox[0].sender_agent_id, None);

    let curies_inbox = store
        .list_agent_messages(&project_id("factory"), &agent_id("curie"), None, 100)
        .unwrap();
    assert!(curies_inbox.is_empty());
}

#[test]
fn delete_project_cascades_agent_profiles_and_messages() {
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
        .create_agent(
            NewAgent {
                id: agent_id("god"),
                project_id: project_id("factory"),
                parent_agent_id: None,
                role: AgentRole::Orchestrator,
                provider: Provider::Codex,
            },
            3,
        )
        .unwrap();
    store
        .update_agent_profile(
            &project_id("factory"),
            &agent_id("curie"),
            UpdateAgentProfile {
                model: Some("gpt-5.6-luna".into()),
                reasoning_effort: None,
                model_selection_reason: None,
                permission_mode: None,
            },
            4,
        )
        .unwrap();
    store
        .send_agent_message(NewAgentMessage {
            id: MessageId::try_from("message-1").unwrap(),
            project_id: project_id("factory"),
            sender_agent_id: Some(agent_id("curie")),
            recipient_agent_id: agent_id("god"),
            body: "Hello.".into(),
            created_at_ms: 5,
        })
        .unwrap();

    // Would fail on a deferred foreign key violation if agent_profiles or
    // agent_messages rows for this project were left behind.
    store.delete_project(&project_id("factory"), 6).unwrap();

    assert!(
        store
            .list_agents(&project_id("factory"), None, 100)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn delete_task_nulls_delivered_run_id_on_agent_messages_but_keeps_them_as_history() {
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
                body: String::new(),
                priority: 0,
            },
            3,
        )
        .unwrap();
    store
        .send_agent_message(NewAgentMessage {
            id: MessageId::try_from("message-1").unwrap(),
            project_id: project_id("factory"),
            sender_agent_id: None,
            recipient_agent_id: agent_id("curie"),
            body: "Please look at task-1.".into(),
            created_at_ms: 4,
        })
        .unwrap();
    store
        .assign_task(
            &project_id("factory"),
            &task_id("task-1"),
            Some(&agent_id("curie")),
            5,
        )
        .unwrap();

    // Opening the episode delivers the message into it.
    let run_id = open_episode(&mut store, "factory", "task-1", "curie", "run-1", 6);
    let delivered = store
        .list_agent_messages(&project_id("factory"), &agent_id("curie"), None, 100)
        .unwrap();
    assert_eq!(delivered.len(), 1);
    assert!(delivered[0].delivered_at_ms.is_some());
    assert_eq!(delivered[0].delivered_run_id, Some(run_id.clone()));

    // The episode closes without touching delivery (unlike a launch that
    // never happened), so the task can now be deleted once the run is
    // terminal.
    let closed = store
        .cancel_run(&project_id("factory"), &run_id, 7)
        .unwrap();
    assert_eq!(closed.task.snapshot.status, TaskStatus::Cancelled);

    store
        .delete_task(&project_id("factory"), &task_id("task-1"), 8)
        .unwrap();

    let after_delete = store
        .list_agent_messages(&project_id("factory"), &agent_id("curie"), None, 100)
        .unwrap();
    assert_eq!(after_delete.len(), 1);
    assert!(after_delete[0].delivered_at_ms.is_some());
    assert_eq!(after_delete[0].delivered_run_id, None);
}

/// `open_run_episode`'s delivery-marking is undone if the run never
/// actually starts is future work (5C's PTY-typed-delivery acknowledgement,
/// TRACK5-DESIGN.md A3); today delivery is unconditional once the episode
/// opens, and stays delivered even if that episode is later closed without
/// producing anything (mirrored here by an immediate `cancel_run`).
#[test]
fn open_run_episode_delivers_pending_messages_and_delivery_survives_a_cancelled_episode() {
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
                body: String::new(),
                priority: 0,
            },
            3,
        )
        .unwrap();
    store
        .send_agent_message(NewAgentMessage {
            id: MessageId::try_from("message-1").unwrap(),
            project_id: project_id("factory"),
            sender_agent_id: None,
            recipient_agent_id: agent_id("curie"),
            body: "Please look at task-1.".into(),
            created_at_ms: 4,
        })
        .unwrap();

    let run_id = open_episode(&mut store, "factory", "task-1", "curie", "run-1", 5);
    let delivered = store
        .list_agent_messages(&project_id("factory"), &agent_id("curie"), None, 100)
        .unwrap();
    assert_eq!(delivered.len(), 1);
    assert!(delivered[0].delivered_at_ms.is_some());
    assert_eq!(delivered[0].delivered_run_id, Some(run_id.clone()));

    store
        .cancel_run(&project_id("factory"), &run_id, 6)
        .unwrap();
    let still_delivered = store
        .list_agent_messages(&project_id("factory"), &agent_id("curie"), None, 100)
        .unwrap();
    assert!(still_delivered[0].delivered_at_ms.is_some());
}
