use factory_core::{
    AgentId, AgentRole, ProjectId, Provider, RunId, RunnerInstanceId, TaskId, TaskStatus,
};
use factoryd::store::{NewAgent, NewProject, NewTask, RunReservation, Store};

fn project_id(value: &str) -> ProjectId {
    ProjectId::try_from(value).unwrap()
}

fn task_id(value: &str) -> TaskId {
    TaskId::try_from(value).unwrap()
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

    let tasks = store.list_tasks(&project_id("project-a"), None, 2).unwrap();
    assert_eq!(
        tasks
            .iter()
            .map(|task| task.snapshot.id.as_str())
            .collect::<Vec<_>>(),
        ["task-a", "task-b"]
    );
    let tasks = store
        .list_tasks(&project_id("project-a"), Some(&task_id("task-b")), 2)
        .unwrap();
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
    let reservation = RunReservation {
        project_id: project_id("factory"),
        task_id: task_id("task-1"),
        agent_id: AgentId::try_from("god").unwrap(),
        expected_provider: Provider::Codex,
        run_id: RunId::try_from("run-1").unwrap(),
        parent_run_id: None,
        worktree: "/work/factory".into(),
        fresh_provider_session_id: None,
        runner_instance_id: RunnerInstanceId::try_from("instance-1").unwrap(),
        runner_runtime: "/private/runners/run-1".into(),
    };
    store.reserve_task_run(reservation, 1, 4).unwrap();
    store
        .fail_run_launch(
            &RunId::try_from("run-1").unwrap(),
            &RunnerInstanceId::try_from("instance-1").unwrap(),
            5,
        )
        .unwrap();

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
    assert_eq!(store.events_after(0, 100).unwrap().len(), 10);
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
