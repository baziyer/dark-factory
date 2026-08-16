use factory_core::{AgentId, AgentRole, MessageId, ProjectId, Provider, TaskId};
use factoryd::store::{NewAgent, NewAgentMessage, NewProject, NewTask, RunReservation, Store};

fn fixture() -> Store {
    let mut store = Store::open_in_memory().unwrap();
    store
        .create_project(
            NewProject {
                id: ProjectId::try_from("factory").unwrap(),
                name: "Factory".into(),
                root: "/tmp/factory".into(),
            },
            1,
        )
        .unwrap();
    store
        .create_agent(
            NewAgent {
                id: AgentId::try_from("god").unwrap(),
                project_id: ProjectId::try_from("factory").unwrap(),
                parent_agent_id: None,
                role: AgentRole::Orchestrator,
                provider: Provider::Codex,
            },
            2,
        )
        .unwrap();
    store
}

#[test]
fn operator_message_is_durable_and_delivered_once() {
    let mut store = fixture();
    let project = ProjectId::try_from("factory").unwrap();
    let agent = AgentId::try_from("god").unwrap();
    store
        .send_agent_message(NewAgentMessage {
            id: MessageId::try_from("message-1").unwrap(),
            project_id: project.clone(),
            sender_agent_id: None,
            recipient_agent_id: agent.clone(),
            body: "Continue from the last checkpoint.".into(),
            created_at_ms: 3,
        })
        .unwrap();

    let pending = store
        .list_agent_messages(&project, &agent, None, 10)
        .unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].delivered_at_ms, None);

    let delivered = store.deliver_agent_messages(&project, &agent, 4).unwrap();
    assert_eq!(delivered.len(), 1);
    assert_eq!(delivered[0].body, "Continue from the last checkpoint.");
    assert_eq!(delivered[0].delivered_at_ms, Some(4));
    assert!(
        store
            .deliver_agent_messages(&project, &agent, 5)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn reservation_delivers_messages_into_the_next_explicit_launch() {
    let mut store = fixture();
    let project = ProjectId::try_from("factory").unwrap();
    let agent = AgentId::try_from("worker").unwrap();
    store
        .create_agent(
            NewAgent {
                id: agent.clone(),
                project_id: project.clone(),
                parent_agent_id: Some(AgentId::try_from("god").unwrap()),
                role: AgentRole::Worker,
                provider: Provider::Codex,
            },
            3,
        )
        .unwrap();
    store
        .create_task(
            NewTask {
                id: TaskId::try_from("task-1").unwrap(),
                project_id: project.clone(),
                parent_task_id: None,
                title: "Next task".into(),
                body: "Do the work".into(),
                priority: 0,
            },
            4,
        )
        .unwrap();
    store
        .send_agent_message(NewAgentMessage {
            id: MessageId::try_from("message-2").unwrap(),
            project_id: project.clone(),
            sender_agent_id: Some(AgentId::try_from("god").unwrap()),
            recipient_agent_id: agent.clone(),
            body: "Use the small test slice.".into(),
            created_at_ms: 5,
        })
        .unwrap();

    let reserved = store
        .reserve_task_run(
            RunReservation {
                project_id: project.clone(),
                task_id: TaskId::try_from("task-1").unwrap(),
                agent_id: agent.clone(),
                expected_provider: Provider::Codex,
                run_id: factory_core::RunId::try_from("run-1").unwrap(),
                parent_run_id: None,
                worktree: "/work/factory".into(),
                fresh_provider_session_id: None,
                runner_instance_id: factory_core::RunnerInstanceId::try_from("runner-1").unwrap(),
                runner_runtime: "/private/factory-runner-1".into(),
            },
            1,
            6,
        )
        .unwrap();
    assert_eq!(reserved.target.agent_messages.len(), 1);
    assert_eq!(
        reserved.target.agent_messages[0].sender_agent_id.as_ref(),
        Some(&AgentId::try_from("god").unwrap())
    );
    assert_eq!(reserved.target.agent_messages[0].delivered_at_ms, Some(6));
    let recovered_target = store
        .execution_target(&factory_core::RunId::try_from("run-1").unwrap())
        .unwrap();
    assert_eq!(
        recovered_target.agent_messages,
        reserved.target.agent_messages
    );
}
