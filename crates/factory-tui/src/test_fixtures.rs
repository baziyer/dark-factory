//! Shared test-only fixture builders for board state (`ProjectSnapshot`/`AgentSnapshot`/
//! `TaskDetail`), previously copy-pasted verbatim across `model/tests.rs`, `model/keymap.rs`'s
//! test module, and `fortress.rs`'s test module. One home for them so a field addition to any of
//! these types only needs updating in one place.

use factory_core::{
    AgentId, AgentRole, AgentSnapshot, ProjectId, ProjectSnapshot, Provider, TaskDetail, TaskId,
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
