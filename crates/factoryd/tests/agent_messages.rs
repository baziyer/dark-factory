use factory_core::{
    AgentId, AgentRole, MessageId, ProjectId, Provider, RunnerInstanceId, SessionId, TaskId,
};
use factoryd::store::{NewAgent, NewAgentMessage, NewProject, NewSession, NewTask, Store};

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

    let session_id = SessionId::try_from("session-god").unwrap();
    store
        .create_session(
            NewSession {
                id: session_id.clone(),
                project_id: project.clone(),
                agent_id: agent.clone(),
                provider: Provider::Codex,
                runtime_model: None,
                runtime_reasoning_effort: None,
                runtime_permission_mode: None,
                runtime_control_mode: None,
                provider_session_id: None,
                worktree: "/work/factory".into(),
                codex_home: None,
                hook_token: "a".repeat(64),
                runner_instance_id: RunnerInstanceId::try_from("instance-god").unwrap(),
                runner_runtime: "/private/runners/god".into(),
                runner_protocol_version: 1,
            },
            3,
        )
        .unwrap();

    let delivered = store
        .deliver_agent_messages(&project, &agent, &session_id, 4)
        .unwrap();
    assert_eq!(delivered.len(), 1);
    assert_eq!(delivered[0].body, "Continue from the last checkpoint.");
    assert_eq!(delivered[0].delivered_at_ms, Some(4));
    assert_eq!(delivered[0].delivered_session_id, Some(session_id.clone()));
    assert!(
        store
            .deliver_agent_messages(&project, &agent, &session_id, 5)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn opening_a_run_episode_delivers_messages_into_the_new_episode() {
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
    store
        .assign_task(
            &project,
            &TaskId::try_from("task-1").unwrap(),
            Some(&agent),
            5,
        )
        .unwrap();

    let session_id = SessionId::try_from("session-worker").unwrap();
    store
        .create_session(
            NewSession {
                id: session_id.clone(),
                project_id: project.clone(),
                agent_id: agent.clone(),
                provider: Provider::Codex,
                runtime_model: None,
                runtime_reasoning_effort: None,
                runtime_permission_mode: None,
                runtime_control_mode: None,
                provider_session_id: None,
                worktree: "/work/factory".into(),
                codex_home: None,
                hook_token: "b".repeat(64),
                runner_instance_id: RunnerInstanceId::try_from("instance-worker").unwrap(),
                runner_runtime: "/private/runners/worker".into(),
                runner_protocol_version: 1,
            },
            6,
        )
        .unwrap();
    // Compose the durable attempt's message identity first. A later inbox
    // message must not be swept into the task episode merely because it is
    // undelivered when the acknowledgement commits.
    let captured_message_ids = store
        .undelivered_messages_for_agent(&project, &agent)
        .unwrap()
        .into_iter()
        .map(|message| message.id)
        .collect::<Vec<_>>();
    store
        .send_agent_message(NewAgentMessage {
            id: MessageId::try_from("message-3").unwrap(),
            project_id: project.clone(),
            sender_agent_id: Some(AgentId::try_from("god").unwrap()),
            recipient_agent_id: agent.clone(),
            body: "Arrived after composition.".into(),
            created_at_ms: 7,
        })
        .unwrap();
    let opened = store
        .open_run_episode_with_message_ids(
            &session_id,
            &TaskId::try_from("task-1").unwrap(),
            Some(&captured_message_ids),
            8,
        )
        .unwrap();
    assert_eq!(opened.agent_messages.len(), 1);
    assert_eq!(
        opened.agent_messages[0].sender_agent_id.as_ref(),
        Some(&AgentId::try_from("god").unwrap())
    );
    assert_eq!(opened.agent_messages[0].delivered_at_ms, Some(8));
    assert_eq!(
        opened.agent_messages[0].delivered_run_id,
        Some(opened.run.id.clone())
    );
    assert_eq!(
        opened.agent_messages[0].delivered_session_id,
        Some(session_id)
    );

    let persisted = store
        .list_agent_messages(&project, &agent, None, 100)
        .unwrap();
    assert_eq!(persisted.len(), 2);
    assert!(persisted.iter().any(|message| {
        message.id == MessageId::try_from("message-3").unwrap()
            && message.delivered_at_ms.is_none()
            && message.delivered_run_id.is_none()
    }));
    assert!(persisted.iter().any(|message| {
        message.id == MessageId::try_from("message-2").unwrap()
            && message.delivered_run_id == Some(opened.run.id.clone())
    }));
}
