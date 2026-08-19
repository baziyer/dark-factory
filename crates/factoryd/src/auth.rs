//! Daemon-owned principals, capabilities, and request scopes.
//!
//! The Unix socket remains the operator control plane when a request has no
//! credential. Provider sessions use the exact live session token carried in
//! the envelope. No request field is trusted to choose the agent behind that
//! credential.

use factory_core::{AgentId, AgentRole, ProjectId, RunId, SessionId, TaskId, local::LocalRequest};
use thiserror::Error;

use crate::{
    daemon_state::{DaemonState, DaemonStateError},
    store::StoreError,
};

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Principal {
    Operator,
    Session(SessionPrincipal),
    /// HTTP integrations are authenticated before they enter the shared
    /// operation layer. Keeping this principal here makes the capability
    /// profiles explicit even though the current local socket has no
    /// integration credential transport.
    Integration(IntegrationPrincipal),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionPrincipal {
    pub project_id: ProjectId,
    pub agent_id: AgentId,
    pub session_id: SessionId,
    pub parent_agent_id: Option<AgentId>,
    pub role: AgentRole,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct IntegrationPrincipal {
    pub endpoint_id: String,
    pub project_id: Option<ProjectId>,
    pub orchestrator_agent_id: Option<AgentId>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Capability {
    Health,
    SetAutoMode,
    FleetStatus,
    AgentStatus,
    CreateProject,
    ListProjects,
    GetProject,
    UpdateProjectGuidance,
    SetRepositoryAuthority,
    CreateTask,
    CreateAgent,
    GetAgent,
    UpdateAgentProfile,
    SetAgentBudget,
    ResetAgentBudget,
    SendAgentMessage,
    ListAgentMessages,
    ListAgents,
    StartTask,
    ListTasks,
    GetTask,
    RetryTask,
    CancelTask,
    UpdateTask,
    DeleteTask,
    DeleteAgent,
    DeleteProject,
    AssignTask,
    GetRunTerminal,
    StopRun,
    CancelRun,
    CompleteTask,
    BlockTask,
    PauseAgent,
    ResumeAgent,
    ListSessions,
    StopSession,
    ProviderHook,
    GitStatus,
    GitDiff,
    GitCommit,
    GitPush,
    PrOpen,
    PrUpdate,
    AttachTerminal,
    TerminalInput,
    ResizeTerminal,
    ListRuns,
    EventsAfter,
    LatestEventSequence,
    Subscribe,
    ConnectorEvent,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Scope {
    Global,
    Project(ProjectId),
    Agent {
        project_id: ProjectId,
        agent_id: AgentId,
    },
    Task {
        project_id: ProjectId,
        task_id: TaskId,
    },
    Run {
        project_id: ProjectId,
        run_id: RunId,
    },
    Session {
        project_id: ProjectId,
        session_id: SessionId,
    },
    Message {
        project_id: ProjectId,
        recipient_agent_id: AgentId,
    },
    AuthenticatedSession,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RequestDescriptor {
    pub capability: Capability,
    pub scope: Scope,
}

#[derive(Debug, Error)]
pub enum AuthError {
    #[error("request requires an authenticated live session")]
    MissingCredential,
    #[error("session authentication failed")]
    InvalidCredential,
    #[error("authenticated principal is not allowed to use capability {0:?}")]
    CapabilityDenied(Capability),
    #[error("authenticated principal is outside the requested scope")]
    ScopeDenied,
    #[error("request identity must be derived from the authenticated session")]
    SpoofedIdentity,
    #[error("authenticated session is not the requested live session")]
    SessionMismatch,
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    State(#[from] DaemonStateError),
}

/// Resolve the only local credential into a daemon-owned principal. A live
/// session is found by constant-time token comparison and its role/hierarchy
/// is read from durable agent state, never from the request.
pub async fn resolve(
    state: &DaemonState,
    session_token: Option<String>,
) -> Result<Principal, AuthError> {
    let Some(token) = session_token else {
        return Ok(Principal::Operator);
    };
    if token.is_empty() {
        return Err(AuthError::InvalidCredential);
    }
    let session = state
        .with_store(move |store| store.find_session_by_hook_token(&token))
        .await?
        .ok_or(AuthError::InvalidCredential)?;
    let project_id = session.project_id.clone();
    let agent_id = session.agent_id.clone();
    let agent = state
        .with_store(move |store| store.get_agent_detail(&project_id, &agent_id))
        .await?;
    Ok(Principal::Session(SessionPrincipal {
        project_id: session.project_id,
        agent_id: session.agent_id,
        session_id: session.id,
        parent_agent_id: agent.snapshot.parent_agent_id,
        role: agent.snapshot.role,
    }))
}

/// Bind the request to a principal and enforce its fixed profile/scope.
/// Returning the request allows the server to fill an omitted message sender
/// from the authenticated identity before any business handler sees it.
pub async fn authorize(
    state: &DaemonState,
    principal: &Principal,
    request: LocalRequest,
) -> Result<LocalRequest, AuthError> {
    let descriptor = describe(&request);
    match principal {
        Principal::Operator => Ok(request),
        Principal::Integration(integration) => {
            authorize_integration(integration, &descriptor)?;
            Ok(request)
        }
        Principal::Session(session) => {
            authorize_session(state, session, &descriptor, &request).await?;
            derive_session_identity(session, request)
        }
    }
}

/// Exhaustive capability and scope map. Adding a `LocalRequest` variant
/// without adding it here is a compile error, so policy review cannot miss a
/// new operation by falling through a wildcard.
pub fn describe(request: &LocalRequest) -> RequestDescriptor {
    use Capability::*;
    let (capability, scope) = match request {
        LocalRequest::Health => (Health, Scope::Global),
        LocalRequest::SetAutoMode { .. } => (SetAutoMode, Scope::Global),
        LocalRequest::FleetStatus => (FleetStatus, Scope::Global),
        LocalRequest::AgentStatus {
            project_id,
            agent_id,
        } => (
            AgentStatus,
            Scope::Agent {
                project_id: project_id.clone(),
                agent_id: agent_id.clone(),
            },
        ),
        LocalRequest::CreateProject { .. } => (CreateProject, Scope::Global),
        LocalRequest::ListProjects { .. } => (ListProjects, Scope::Global),
        LocalRequest::GetProject { project_id } => (GetProject, Scope::Project(project_id.clone())),
        LocalRequest::UpdateProjectGuidance { project_id, .. } => {
            (UpdateProjectGuidance, Scope::Project(project_id.clone()))
        }
        LocalRequest::SetProjectRepositoryAuthority { project_id, .. } => {
            (SetRepositoryAuthority, Scope::Project(project_id.clone()))
        }
        LocalRequest::CreateTask { project_id, .. } => {
            (CreateTask, Scope::Project(project_id.clone()))
        }
        LocalRequest::CreateAgent { project_id, .. } => {
            (CreateAgent, Scope::Project(project_id.clone()))
        }
        LocalRequest::GetAgent {
            project_id,
            agent_id,
        } => (
            GetAgent,
            Scope::Agent {
                project_id: project_id.clone(),
                agent_id: agent_id.clone(),
            },
        ),
        LocalRequest::UpdateAgentProfile {
            project_id,
            agent_id,
            ..
        } => (
            UpdateAgentProfile,
            Scope::Agent {
                project_id: project_id.clone(),
                agent_id: agent_id.clone(),
            },
        ),
        LocalRequest::SetAgentBudget {
            project_id,
            agent_id,
            ..
        } => (
            SetAgentBudget,
            Scope::Agent {
                project_id: project_id.clone(),
                agent_id: agent_id.clone(),
            },
        ),
        LocalRequest::ResetAgentBudget {
            project_id,
            agent_id,
        } => (
            ResetAgentBudget,
            Scope::Agent {
                project_id: project_id.clone(),
                agent_id: agent_id.clone(),
            },
        ),
        LocalRequest::SendAgentMessage {
            project_id,
            recipient_agent_id,
            ..
        } => (
            SendAgentMessage,
            Scope::Message {
                project_id: project_id.clone(),
                recipient_agent_id: recipient_agent_id.clone(),
            },
        ),
        LocalRequest::ListAgentMessages {
            project_id,
            agent_id,
            ..
        } => (
            ListAgentMessages,
            Scope::Agent {
                project_id: project_id.clone(),
                agent_id: agent_id.clone(),
            },
        ),
        LocalRequest::ListAgents { project_id, .. } => {
            (ListAgents, Scope::Project(project_id.clone()))
        }
        LocalRequest::StartTask {
            project_id,
            task_id,
            ..
        } => (
            StartTask,
            Scope::Task {
                project_id: project_id.clone(),
                task_id: task_id.clone(),
            },
        ),
        LocalRequest::ListTasks { project_id, .. } => {
            (ListTasks, Scope::Project(project_id.clone()))
        }
        LocalRequest::GetTask {
            project_id,
            task_id,
        } => (
            GetTask,
            Scope::Task {
                project_id: project_id.clone(),
                task_id: task_id.clone(),
            },
        ),
        LocalRequest::RetryTask {
            project_id,
            task_id,
        } => (
            RetryTask,
            Scope::Task {
                project_id: project_id.clone(),
                task_id: task_id.clone(),
            },
        ),
        LocalRequest::CancelTask {
            project_id,
            task_id,
        } => (
            CancelTask,
            Scope::Task {
                project_id: project_id.clone(),
                task_id: task_id.clone(),
            },
        ),
        LocalRequest::UpdateTask {
            project_id,
            task_id,
            ..
        } => (
            UpdateTask,
            Scope::Task {
                project_id: project_id.clone(),
                task_id: task_id.clone(),
            },
        ),
        LocalRequest::DeleteTask {
            project_id,
            task_id,
        } => (
            DeleteTask,
            Scope::Task {
                project_id: project_id.clone(),
                task_id: task_id.clone(),
            },
        ),
        LocalRequest::DeleteAgent {
            project_id,
            agent_id,
        } => (
            DeleteAgent,
            Scope::Agent {
                project_id: project_id.clone(),
                agent_id: agent_id.clone(),
            },
        ),
        LocalRequest::DeleteProject { project_id } => {
            (DeleteProject, Scope::Project(project_id.clone()))
        }
        LocalRequest::AssignTask {
            project_id,
            task_id,
            ..
        } => (
            AssignTask,
            Scope::Task {
                project_id: project_id.clone(),
                task_id: task_id.clone(),
            },
        ),
        LocalRequest::GetRunTerminal { project_id, run_id } => (
            GetRunTerminal,
            Scope::Run {
                project_id: project_id.clone(),
                run_id: run_id.clone(),
            },
        ),
        LocalRequest::StopRun {
            project_id, run_id, ..
        } => (
            StopRun,
            Scope::Run {
                project_id: project_id.clone(),
                run_id: run_id.clone(),
            },
        ),
        LocalRequest::CancelRun { project_id, run_id } => (
            CancelRun,
            Scope::Run {
                project_id: project_id.clone(),
                run_id: run_id.clone(),
            },
        ),
        LocalRequest::CompleteTask {
            project_id,
            task_id,
            ..
        } => (
            CompleteTask,
            Scope::Task {
                project_id: project_id.clone(),
                task_id: task_id.clone(),
            },
        ),
        LocalRequest::BlockTask {
            project_id,
            task_id,
            ..
        } => (
            BlockTask,
            Scope::Task {
                project_id: project_id.clone(),
                task_id: task_id.clone(),
            },
        ),
        LocalRequest::PauseAgent {
            project_id,
            agent_id,
        } => (
            PauseAgent,
            Scope::Agent {
                project_id: project_id.clone(),
                agent_id: agent_id.clone(),
            },
        ),
        LocalRequest::ResumeAgent {
            project_id,
            agent_id,
        } => (
            ResumeAgent,
            Scope::Agent {
                project_id: project_id.clone(),
                agent_id: agent_id.clone(),
            },
        ),
        LocalRequest::ListSessions { project_id, .. } => {
            (ListSessions, Scope::Project(project_id.clone()))
        }
        LocalRequest::StopSession {
            project_id,
            session_id,
            ..
        } => (
            StopSession,
            Scope::Session {
                project_id: project_id.clone(),
                session_id: session_id.clone(),
            },
        ),
        LocalRequest::ProviderHook { .. } => (ProviderHook, Scope::AuthenticatedSession),
        LocalRequest::GitStatus { .. } => (GitStatus, Scope::AuthenticatedSession),
        LocalRequest::GitDiff { .. } => (GitDiff, Scope::AuthenticatedSession),
        LocalRequest::GitCommit { .. } => (GitCommit, Scope::AuthenticatedSession),
        LocalRequest::GitPush { .. } => (GitPush, Scope::AuthenticatedSession),
        LocalRequest::PrOpen { .. } => (PrOpen, Scope::AuthenticatedSession),
        LocalRequest::PrUpdate { .. } => (PrUpdate, Scope::AuthenticatedSession),
        LocalRequest::AttachTerminal {
            project_id,
            session_id,
            ..
        } => (
            AttachTerminal,
            Scope::Session {
                project_id: project_id.clone(),
                session_id: session_id.clone(),
            },
        ),
        LocalRequest::TerminalInput {
            project_id,
            session_id,
            ..
        } => (
            TerminalInput,
            Scope::Session {
                project_id: project_id.clone(),
                session_id: session_id.clone(),
            },
        ),
        LocalRequest::ResizeTerminal {
            project_id,
            session_id,
            ..
        } => (
            ResizeTerminal,
            Scope::Session {
                project_id: project_id.clone(),
                session_id: session_id.clone(),
            },
        ),
        LocalRequest::ListRuns { project_id, .. } => (ListRuns, Scope::Project(project_id.clone())),
        LocalRequest::EventsAfter { .. } => (EventsAfter, Scope::Global),
        LocalRequest::LatestEventSequence => (LatestEventSequence, Scope::Global),
        LocalRequest::Subscribe { .. } => (Subscribe, Scope::Global),
    };
    RequestDescriptor { capability, scope }
}

async fn authorize_session(
    state: &DaemonState,
    principal: &SessionPrincipal,
    descriptor: &RequestDescriptor,
    request: &LocalRequest,
) -> Result<(), AuthError> {
    use Capability::*;
    if matches!(descriptor.capability, Health) {
        return Ok(());
    }
    if matches!(descriptor.scope, Scope::Global) {
        return Err(AuthError::ScopeDenied);
    }
    if let Some(project_id) = scope_project(&descriptor.scope)
        && project_id != &principal.project_id
    {
        return Err(AuthError::ScopeDenied);
    }

    let allowed = match descriptor.capability {
        Health => true,
        GetProject | ListAgents | ListTasks | GetTask => true,
        AgentStatus | GetAgent | ListAgentMessages => {
            let target = scope_agent(&descriptor.scope).ok_or(AuthError::ScopeDenied)?;
            target == &principal.agent_id
                || (principal.role == AgentRole::Orchestrator
                    && state
                        .with_store({
                            let project_id = principal.project_id.clone();
                            let root = principal.agent_id.clone();
                            let target = target.clone();
                            move |store| store.agent_is_descendant(&project_id, &root, &target)
                        })
                        .await?)
        }
        SendAgentMessage => {
            let Scope::Message {
                recipient_agent_id, ..
            } = &descriptor.scope
            else {
                return Err(AuthError::ScopeDenied);
            };
            if principal.role == AgentRole::Orchestrator {
                state
                    .with_store({
                        let project_id = principal.project_id.clone();
                        let root = principal.agent_id.clone();
                        let target = recipient_agent_id.clone();
                        move |store| store.agent_is_descendant(&project_id, &root, &target)
                    })
                    .await?
            } else {
                principal.parent_agent_id.as_ref() == Some(recipient_agent_id)
            }
        }
        ListRuns | ListSessions => principal.role == AgentRole::Orchestrator,
        CreateTask => match request {
            LocalRequest::CreateTask { agent_id, .. } => match (principal.role, agent_id) {
                (AgentRole::Worker, None) => true,
                (AgentRole::Worker, Some(_)) => false,
                (AgentRole::Orchestrator, None) => true,
                (AgentRole::Orchestrator, Some(target)) => {
                    state
                        .with_store({
                            let project_id = principal.project_id.clone();
                            let root = principal.agent_id.clone();
                            let target = target.clone();
                            move |store| store.agent_is_descendant(&project_id, &root, &target)
                        })
                        .await?
                }
            },
            _ => false,
        },
        StartTask => match request {
            LocalRequest::StartTask { agent_id, .. }
                if principal.role == AgentRole::Orchestrator =>
            {
                state
                    .with_store({
                        let project_id = principal.project_id.clone();
                        let root = principal.agent_id.clone();
                        let target = agent_id.clone();
                        move |store| store.agent_is_descendant(&project_id, &root, &target)
                    })
                    .await?
            }
            _ => false,
        },
        CompleteTask | BlockTask => {
            let Scope::Task {
                project_id,
                task_id,
            } = &descriptor.scope
            else {
                return Err(AuthError::ScopeDenied);
            };
            state
                .with_store({
                    let project_id = project_id.clone();
                    let task_id = task_id.clone();
                    let agent_id = principal.agent_id.clone();
                    move |store| store.task_assigned_to_agent(&project_id, &task_id, &agent_id)
                })
                .await?
        }
        ProviderHook => match request {
            LocalRequest::ProviderHook { token, .. } => {
                let session = state
                    .with_store({
                        let token = token.clone();
                        move |store| store.find_session_by_hook_token(&token)
                    })
                    .await?
                    .ok_or(AuthError::InvalidCredential)?;
                session.id == principal.session_id
            }
            _ => false,
        },
        GitStatus | GitDiff | GitCommit | GitPush | PrOpen | PrUpdate => true,
        AttachTerminal | TerminalInput | ResizeTerminal => {
            let Scope::Session { session_id, .. } = &descriptor.scope else {
                return Err(AuthError::ScopeDenied);
            };
            session_id == &principal.session_id
        }
        SetAgentBudget | ResetAgentBudget | UpdateAgentProfile | PauseAgent | ResumeAgent
        | DeleteAgent | DeleteTask | RetryTask | CancelTask | UpdateTask | AssignTask | StopRun
        | CancelRun | StopSession | GetRunTerminal => false,
        SetAutoMode
        | FleetStatus
        | CreateProject
        | ListProjects
        | UpdateProjectGuidance
        | SetRepositoryAuthority
        | CreateAgent
        | DeleteProject
        | EventsAfter
        | LatestEventSequence
        | Subscribe
        | ConnectorEvent => false,
    };
    if allowed {
        Ok(())
    } else {
        Err(AuthError::CapabilityDenied(descriptor.capability))
    }
}

pub fn authorize_integration(
    principal: &IntegrationPrincipal,
    descriptor: &RequestDescriptor,
) -> Result<(), AuthError> {
    let Some(project_id) = &principal.project_id else {
        return Err(AuthError::ScopeDenied);
    };
    if !matches!(&descriptor.scope, Scope::Project(target) if target == project_id) {
        return Err(AuthError::ScopeDenied);
    }
    match descriptor.capability {
        Capability::GetProject
        | Capability::CreateTask
        | Capability::GetTask
        | Capability::ListTasks
        | Capability::ConnectorEvent => Ok(()),
        capability => Err(AuthError::CapabilityDenied(capability)),
    }
}

fn derive_session_identity(
    principal: &SessionPrincipal,
    request: LocalRequest,
) -> Result<LocalRequest, AuthError> {
    match request {
        LocalRequest::SendAgentMessage {
            id,
            project_id,
            sender_agent_id,
            recipient_agent_id,
            body,
        } => {
            if sender_agent_id
                .as_ref()
                .is_some_and(|sender| sender != &principal.agent_id)
            {
                return Err(AuthError::SpoofedIdentity);
            }
            Ok(LocalRequest::SendAgentMessage {
                id,
                project_id,
                sender_agent_id: Some(principal.agent_id.clone()),
                recipient_agent_id,
                body,
            })
        }
        request => Ok(request),
    }
}

fn scope_project(scope: &Scope) -> Option<&ProjectId> {
    match scope {
        Scope::Project(project_id)
        | Scope::Agent { project_id, .. }
        | Scope::Task { project_id, .. }
        | Scope::Run { project_id, .. }
        | Scope::Session { project_id, .. }
        | Scope::Message { project_id, .. } => Some(project_id),
        Scope::Global | Scope::AuthenticatedSession => None,
    }
}

fn scope_agent(scope: &Scope) -> Option<&AgentId> {
    match scope {
        Scope::Agent { agent_id, .. }
        | Scope::Message {
            recipient_agent_id: agent_id,
            ..
        } => Some(agent_id),
        Scope::Global
        | Scope::Project(_)
        | Scope::Task { .. }
        | Scope::Run { .. }
        | Scope::Session { .. }
        | Scope::AuthenticatedSession => None,
    }
}

#[cfg(test)]
mod tests {
    use factory_core::{Provider, RunnerInstanceId};

    use super::*;
    use crate::store::{NewAgent, NewProject, NewSession, Store};

    fn principal(role: AgentRole) -> SessionPrincipal {
        SessionPrincipal {
            project_id: ProjectId::try_from("project-a").unwrap(),
            agent_id: AgentId::try_from("worker-a").unwrap(),
            session_id: SessionId::try_from("11111111-1111-4111-8111-111111111111").unwrap(),
            parent_agent_id: Some(AgentId::try_from("orchestrator").unwrap()),
            role,
        }
    }

    #[tokio::test]
    async fn session_identity_cannot_be_spoofed_or_cross_projected() {
        let state = DaemonState::new(Store::open_in_memory().unwrap());
        let worker = principal(AgentRole::Worker);
        let spoofed = LocalRequest::SendAgentMessage {
            id: "message-1".try_into().unwrap(),
            project_id: worker.project_id.clone(),
            sender_agent_id: Some(AgentId::try_from("worker-b").unwrap()),
            recipient_agent_id: worker.parent_agent_id.clone().unwrap(),
            body: "spoof".into(),
        };
        assert!(matches!(
            authorize(&state, &Principal::Session(worker.clone()), spoofed).await,
            Err(AuthError::SpoofedIdentity)
        ));

        let foreign_project = LocalRequest::GetProject {
            project_id: ProjectId::try_from("project-b").unwrap(),
        };
        assert!(matches!(
            authorize(&state, &Principal::Session(worker), foreign_project).await,
            Err(AuthError::ScopeDenied)
        ));
    }

    #[tokio::test]
    async fn worker_cannot_read_another_agent_but_operator_socket_is_unchanged() {
        let state = DaemonState::new(Store::open_in_memory().unwrap());
        let worker = principal(AgentRole::Worker);
        let other = LocalRequest::GetAgent {
            project_id: worker.project_id.clone(),
            agent_id: AgentId::try_from("worker-b").unwrap(),
        };
        assert!(matches!(
            authorize(&state, &Principal::Session(worker), other).await,
            Err(AuthError::CapabilityDenied(Capability::GetAgent))
        ));

        let operator_request = LocalRequest::SetAutoMode { enabled: true };
        assert_eq!(
            authorize(&state, &Principal::Operator, operator_request.clone())
                .await
                .unwrap(),
            operator_request
        );
        let envelope = serde_json::to_value(factory_core::local::RequestEnvelope::new(
            LocalRequest::Health,
        ))
        .unwrap();
        assert!(envelope.get("session_token").is_none());
    }

    #[test]
    fn integration_profile_is_fixed_to_its_configured_project() {
        let project_id = ProjectId::try_from("project-a").unwrap();
        let integration = IntegrationPrincipal {
            endpoint_id: "github".into(),
            project_id: Some(project_id.clone()),
            orchestrator_agent_id: None,
        };
        assert!(
            authorize_integration(
                &integration,
                &RequestDescriptor {
                    capability: Capability::GetProject,
                    scope: Scope::Project(project_id.clone()),
                }
            )
            .is_ok()
        );
        assert!(matches!(
            authorize_integration(
                &integration,
                &RequestDescriptor {
                    capability: Capability::GetProject,
                    scope: Scope::Project(ProjectId::try_from("project-b").unwrap()),
                }
            ),
            Err(AuthError::ScopeDenied)
        ));
        assert!(matches!(
            authorize_integration(
                &integration,
                &RequestDescriptor {
                    capability: Capability::SetAutoMode,
                    scope: Scope::Global,
                }
            ),
            Err(AuthError::ScopeDenied)
        ));
    }

    #[tokio::test]
    async fn ended_session_token_is_revoked() {
        let project_id = ProjectId::try_from("project-a").unwrap();
        let agent_id = AgentId::try_from("worker-a").unwrap();
        let session_id = SessionId::try_from("11111111-1111-4111-8111-111111111111").unwrap();
        let token = "a".repeat(64);
        let mut store = Store::open_in_memory().unwrap();
        store
            .create_project(
                NewProject {
                    id: project_id.clone(),
                    name: "Project A".into(),
                    root: "/tmp".into(),
                },
                1,
            )
            .unwrap();
        store
            .create_agent(
                NewAgent {
                    id: agent_id.clone(),
                    project_id: project_id.clone(),
                    parent_agent_id: None,
                    role: AgentRole::Worker,
                    provider: Provider::Shell,
                },
                1,
            )
            .unwrap();
        store
            .create_session(
                NewSession {
                    id: session_id.clone(),
                    project_id: project_id.clone(),
                    agent_id: agent_id.clone(),
                    provider: Provider::Shell,
                    runtime_model: None,
                    runtime_reasoning_effort: None,
                    runtime_permission_mode: None,
                    runtime_control_mode: None,
                    provider_session_id: None,
                    worktree: "/tmp".into(),
                    codex_home: None,
                    hook_token: token.clone(),
                    runner_instance_id: RunnerInstanceId::try_from(
                        "22222222-2222-4222-8222-222222222222",
                    )
                    .unwrap(),
                    runner_runtime: "/tmp".into(),
                    runner_protocol_version: 1,
                },
                1,
            )
            .unwrap();
        let state = DaemonState::new(store);
        assert!(matches!(
            resolve(&state, Some(token.clone())).await,
            Ok(Principal::Session(_))
        ));
        state
            .commit_and_publish(move |store| {
                let (_, events) = store.end_session(&session_id, Some(0), None, 2)?;
                Ok(((), events))
            })
            .await
            .unwrap();
        assert!(matches!(
            resolve(&state, Some(token)).await,
            Err(AuthError::InvalidCredential)
        ));
    }
}
