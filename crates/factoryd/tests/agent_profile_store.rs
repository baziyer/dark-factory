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
fn agent_profile_model_is_durable_and_separate_from_public_agent_snapshot() {
    // Standing instructions and memory used to live here too, as TEXT
    // columns; they are now operator- and agent-editable files under the
    // state directory (see `factoryd::guidance`), exercised end to end
    // through the local API in `tests/local_execution_api.rs`.
    let mut store = fixture();
    let project = ProjectId::try_from("factory").unwrap();
    let agent = AgentId::try_from("god").unwrap();

    let initial = store.get_agent_detail(&project, &agent).unwrap();
    assert_eq!(initial.profile.model, None);
    assert_eq!(initial.profile.permission_mode, None);

    store
        .update_agent_profile(
            &project,
            &agent,
            UpdateAgentProfile {
                model: Some("gpt-5.6-sol".into()),
                reasoning_effort: Some("xhigh".into()),
                model_selection_reason: Some("operator verification".into()),
                permission_mode: Some("on-request".into()),
            },
            3,
        )
        .unwrap();

    let reloaded = store.get_agent_detail(&project, &agent).unwrap();
    assert_eq!(reloaded.profile.model.as_deref(), Some("gpt-5.6-sol"));
    assert_eq!(
        reloaded.profile.permission_mode.as_deref(),
        Some("on-request")
    );
    assert_eq!(reloaded.snapshot.provider, Provider::Codex);
}

#[test]
fn agent_profile_rejects_permission_modes_not_declared_by_the_provider() {
    let mut store = fixture();
    let project = ProjectId::try_from("factory").unwrap();
    let agent = AgentId::try_from("god").unwrap();

    let error = match store.update_agent_profile(
        &project,
        &agent,
        UpdateAgentProfile {
            model: None,
            reasoning_effort: None,
            model_selection_reason: None,
            permission_mode: Some("bypass".into()),
        },
        3,
    ) {
        Ok(_) => panic!("unsupported Codex permission mode was persisted"),
        Err(error) => error,
    };
    assert!(matches!(
        error,
        factoryd::store::StoreError::UnsupportedAgentPermissionMode {
            provider: Provider::Codex,
            mode
        } if mode == "bypass"
    ));
    let profile = store.get_agent_detail(&project, &agent).unwrap().profile;
    assert_eq!(profile.model, None);
    assert_eq!(profile.permission_mode, None);
    assert_eq!(profile.updated_at_ms, 2);
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
            Some("gpt-5.6-luna".into()),
            4,
        )
        .unwrap();

    let detail = store.get_agent_detail(&project, &agent).unwrap();
    assert_eq!(detail.profile.model.as_deref(), Some("gpt-5.6-luna"));
}

#[test]
fn new_codex_workers_get_auditable_routine_defaults_without_rewriting_old_profiles() {
    let mut store = fixture();
    let project = ProjectId::try_from("factory").unwrap();
    let agent = AgentId::try_from("worker").unwrap();
    store
        .create_agent_with_profile(
            NewAgent {
                id: agent.clone(),
                project_id: project.clone(),
                parent_agent_id: Some(AgentId::try_from("god").unwrap()),
                role: AgentRole::Worker,
                provider: Provider::Codex,
            },
            None,
            None,
            None,
            4,
        )
        .unwrap();

    let detail = store.get_agent_detail(&project, &agent).unwrap();
    assert_eq!(
        detail.profile.model.as_deref(),
        Some(factory_core::model_policy::ROUTINE_MODEL)
    );
    assert_eq!(
        detail.profile.reasoning_effort.as_deref(),
        Some(factory_core::model_policy::ROUTINE_REASONING_EFFORT)
    );
    assert_eq!(
        detail.profile.model_selection_reason.as_deref(),
        Some("routine bounded worker default")
    );
    let old = store
        .get_agent_detail(&project, &AgentId::try_from("god").unwrap())
        .unwrap();
    assert_eq!(old.profile.model, None);
    assert_eq!(old.profile.reasoning_effort, None);
    assert_eq!(old.profile.model_selection_reason, None);
}

#[test]
fn explicit_worker_escalation_persists_sol_xhigh_and_its_reason() {
    let mut store = fixture();
    let project = ProjectId::try_from("factory").unwrap();
    let agent = AgentId::try_from("worker").unwrap();
    store
        .create_agent_with_profile(
            NewAgent {
                id: agent.clone(),
                project_id: project.clone(),
                parent_agent_id: Some(AgentId::try_from("god").unwrap()),
                role: AgentRole::Worker,
                provider: Provider::Codex,
            },
            None,
            None,
            Some("security integration after a failed routine attempt".into()),
            4,
        )
        .unwrap();

    let detail = store.get_agent_detail(&project, &agent).unwrap();
    assert_eq!(
        detail.profile.model.as_deref(),
        Some(factory_core::model_policy::ESCALATED_MODEL)
    );
    assert_eq!(
        detail.profile.reasoning_effort.as_deref(),
        Some(factory_core::model_policy::ESCALATED_REASONING_EFFORT)
    );
    assert_eq!(
        detail.profile.model_selection_reason.as_deref(),
        Some("security integration after a failed routine attempt")
    );
}

#[test]
fn profile_escalation_requires_a_reason_and_normalizes_to_xhigh() {
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

    let error = match store.update_agent_profile(
        &project,
        &agent,
        UpdateAgentProfile {
            model: Some(factory_core::model_policy::ESCALATED_MODEL.into()),
            reasoning_effort: None,
            model_selection_reason: None,
            permission_mode: None,
        },
        4,
    ) {
        Ok(_) => panic!("an escalation without a reason was persisted"),
        Err(error) => error,
    };
    assert!(matches!(
        error,
        factoryd::store::StoreError::InvalidAgentModelPolicy(
            factory_core::model_policy::ModelPolicyError::EscalationReasonRequired
        )
    ));

    store
        .update_agent_profile(
            &project,
            &agent,
            UpdateAgentProfile {
                model: Some(factory_core::model_policy::ESCALATED_MODEL.into()),
                reasoning_effort: None,
                model_selection_reason: Some("release integration after failed attempt".into()),
                permission_mode: None,
            },
            5,
        )
        .unwrap();
    let detail = store.get_agent_detail(&project, &agent).unwrap();
    assert_eq!(
        detail.profile.reasoning_effort.as_deref(),
        Some(factory_core::model_policy::ESCALATED_REASONING_EFFORT)
    );
    assert_eq!(
        detail.profile.model_selection_reason.as_deref(),
        Some("release integration after failed attempt")
    );
}
