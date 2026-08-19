use factory_core::{AgentId, AgentRole, ProjectId, Provider};
use factoryd::store::{NewAgent, NewProject, Store, UpdateAgentProfile};
use rusqlite::Connection;

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
fn migration_repairs_legacy_codex_bypass_before_the_next_launch() {
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("factory.db");
    {
        let mut store = Store::open(&database).unwrap();
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
    }

    // Simulate the exact pre-#146 durable state, then let the next Store open
    // perform the 0022 repair as an upgrade would. The fixture was opened by
    // the current binary, so remove only the later #155 columns before
    // rewinding its schema version.
    let connection = Connection::open(&database).unwrap();
    connection
        .execute_batch(
            "ALTER TABLE sessions DROP COLUMN provider_resume_blocked_at_ms;
             ALTER TABLE sessions DROP COLUMN resumed_provider_session;
             ALTER TABLE sessions DROP COLUMN delivery_recovery_stop_requested_at_ms;
             ALTER TABLE agent_profiles DROP COLUMN model_selection_reason;
             ALTER TABLE agent_profiles DROP COLUMN reasoning_effort;",
        )
        .unwrap();
    connection
        .execute(
            "UPDATE agent_profiles SET permission_mode = 'bypass' WHERE agent_id = 'god'",
            [],
        )
        .unwrap();
    // `Store::open` above intentionally created the newest schema so the
    // fixture can seed a real profile. Rewind it as an actual pre-0022
    // database, including removing the post-0022 table; otherwise migration
    // 0024/0025 quite correctly reject duplicate schema changes.
    connection
        .execute_batch("DROP TABLE delivery_attempts;")
        .unwrap();
    connection
        .execute_batch(
            "ALTER TABLE sessions DROP COLUMN observer_reason;
             ALTER TABLE sessions DROP COLUMN notification_kind;",
        )
        .unwrap();
    connection.pragma_update(None, "user_version", 21).unwrap();
    drop(connection);

    let store = Store::open(&database).unwrap();
    let detail = store
        .get_agent_detail(
            &ProjectId::try_from("factory").unwrap(),
            &AgentId::try_from("god").unwrap(),
        )
        .unwrap();
    assert_eq!(detail.profile.permission_mode, None);

    // Codex's provider launch regression test proves that this repaired None
    // value produces its supported on-request posture when auto mode is off,
    // rather than an approval_policy="bypass" argv entry.
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
