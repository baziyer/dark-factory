use factory_core::{AgentId, AgentRole, ProjectId, Provider};
use factoryd::store::{NewAgent, NewProject, Store, UpdateAgentProfile};

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
fn agent_profile_is_durable_and_separate_from_public_agent_snapshot() {
    let mut store = fixture();
    let project = ProjectId::try_from("factory").unwrap();
    let agent = AgentId::try_from("god").unwrap();

    let initial = store.get_agent_detail(&project, &agent).unwrap();
    assert_eq!(initial.profile.model, None);
    assert!(initial.profile.instructions.is_empty());

    store
        .update_agent_profile(
            &project,
            &agent,
            UpdateAgentProfile {
                model: Some("gpt-5-codex".into()),
                instructions: "You are the factory orchestrator.".into(),
                memory: "Prefer small, reversible slices.".into(),
            },
            3,
        )
        .unwrap();

    let reloaded = store.get_agent_detail(&project, &agent).unwrap();
    assert_eq!(reloaded.profile.model.as_deref(), Some("gpt-5-codex"));
    assert_eq!(
        reloaded.profile.instructions,
        "You are the factory orchestrator."
    );
    assert_eq!(reloaded.profile.memory, "Prefer small, reversible slices.");
    assert_eq!(reloaded.snapshot.provider, Provider::Codex);
}

#[test]
fn creating_an_agent_can_persist_its_selected_model_in_the_private_profile() {
    let mut store = fixture();
    let project = ProjectId::try_from("factory").unwrap();
    let agent = AgentId::try_from("worker").unwrap();
    store
        .create_agent_with_model(
            NewAgent {
                id: agent.clone(),
                project_id: project.clone(),
                parent_agent_id: Some(AgentId::try_from("god").unwrap()),
                role: AgentRole::Worker,
                provider: Provider::Codex,
            },
            Some("gpt-5-codex".into()),
            4,
        )
        .unwrap();

    let detail = store.get_agent_detail(&project, &agent).unwrap();
    assert_eq!(detail.profile.model.as_deref(), Some("gpt-5-codex"));
}
