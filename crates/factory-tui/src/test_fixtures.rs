//! Shared test-only fixture builders for board state (`ProjectSnapshot`/`AgentSnapshot`/
//! `TaskDetail`), previously copy-pasted verbatim across `model/tests.rs`, `model/keymap.rs`'s
//! test module, and `fortress.rs`'s test module. One home for them so a field addition to any of
//! these types only needs updating in one place.

use factory_core::{
    AgentId, AgentRole, AgentSnapshot, ObserverHealth, ProjectId, ProjectSnapshot, Provider, RunId,
    RunSnapshot, RunStatus, SessionId, SessionSnapshot, SessionState, TaskDetail, TaskId,
    TaskSnapshot, TaskStatus,
};

pub(crate) fn project(id: &str, created_at_ms: i64) -> ProjectSnapshot {
    ProjectSnapshot {
        id: ProjectId::try_from(id).unwrap(),
        name: id.to_owned(),
        root: "/work".into(),
        created_at_ms,
        updated_at_ms: created_at_ms,
    }
}

pub(crate) fn agent(
    id: &str,
    project: &str,
    role: AgentRole,
    parent: Option<&str>,
) -> AgentSnapshot {
    AgentSnapshot {
        id: AgentId::try_from(id).unwrap(),
        project_id: ProjectId::try_from(project).unwrap(),
        parent_agent_id: parent.map(|p| AgentId::try_from(p).unwrap()),
        role,
        provider: Provider::ClaudeCode,
        current_run_id: None,
        paused: false,
        current_session_id: None,
        worktree: None,
        created_at_ms: 0,
        updated_at_ms: 0,
    }
}

pub(crate) fn session(
    id: &str,
    agent_id: &str,
    project: &str,
    state: SessionState,
) -> SessionSnapshot {
    SessionSnapshot {
        id: SessionId::try_from(id).unwrap(),
        project_id: ProjectId::try_from(project).unwrap(),
        agent_id: AgentId::try_from(agent_id).unwrap(),
        provider: Provider::ClaudeCode,
        runtime_model: None,
        runtime_reasoning_effort: None,
        runtime_permission_mode: None,
        runtime_control_mode: None,
        state,
        cleanup_state: factory_core::SessionCleanupState::None,
        state_since_ms: 0,
        worktree: "/work".into(),
        provider_session_id: None,
        current_run_id: None,
        activity: None,
        activity_inferred: false,
        last_hook_event: None,
        last_hook_at_ms: None,
        wait_reason: None,
        observer_health: ObserverHealth::Unknown,
        observer_health_since_ms: 0,
        started_at_ms: 0,
        updated_at_ms: 0,
        ended_at_ms: None,
        exit_code: None,
        exit_signal: None,
    }
}

pub(crate) fn task(
    id: &str,
    project: &str,
    status: TaskStatus,
    assigned: Option<&str>,
    created_at_ms: i64,
) -> TaskDetail {
    TaskDetail {
        snapshot: TaskSnapshot {
            id: TaskId::try_from(id).unwrap(),
            project_id: ProjectId::try_from(project).unwrap(),
            parent_task_id: None,
            assigned_agent_id: assigned.map(|a| AgentId::try_from(a).unwrap()),
            title: id.to_owned(),
            status,
            priority: 0,
            created_at_ms,
            updated_at_ms: created_at_ms,
        },
        body: String::new(),
        result: None,
        blocked_reason: None,
    }
}

pub(crate) fn run(
    agent_id: &str,
    project: &str,
    status: RunStatus,
    started_at_ms: i64,
) -> RunSnapshot {
    RunSnapshot {
        id: RunId::try_from(format!("run-{agent_id}-{started_at_ms}")).unwrap(),
        project_id: ProjectId::try_from(project).unwrap(),
        agent_id: AgentId::try_from(agent_id).unwrap(),
        parent_run_id: None,
        task_id: None,
        session_id: None,
        closed_by: None,
        status,
        activity: None,
        wait_reason: None,
        worktree: "/work".into(),
        observer_health: ObserverHealth::Unknown,
        observer_health_since_ms: 0,
        started_at_ms,
        status_since_ms: started_at_ms,
        updated_at_ms: started_at_ms,
        ended_at_ms: None,
        exit_code: None,
        exit_signal: None,
        failure_reason: None,
    }
}
