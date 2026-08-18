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
                model: Some("gpt-5-codex".into()),
                permission_mode: Some("on-request".into()),
            },
            3,
        )
        .unwrap();

    let reloaded = store.get_agent_detail(&project, &agent).unwrap();
    assert_eq!(reloaded.profile.model.as_deref(), Some("gpt-5-codex"));
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
    // perform the 0022 repair as an upgrade would.
    let connection = Connection::open(&database).unwrap();
    connection
        .execute(
            "UPDATE agent_profiles SET permission_mode = 'bypass' WHERE agent_id = 'god'",
            [],
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
            Some("gpt-5-codex".into()),
            4,
        )
        .unwrap();

    let detail = store.get_agent_detail(&project, &agent).unwrap();
    assert_eq!(detail.profile.model.as_deref(), Some("gpt-5-codex"));
}
