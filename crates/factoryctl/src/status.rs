use std::io::Write;

use factory_core::{
    SessionState,
    status::{AttentionKind, FleetStatus, WorktreeStatus},
};

pub fn write(output: &mut impl Write, status: &FleetStatus) -> Result<(), String> {
    writeln!(
        output,
        "Dark Factory: auto {} | sessions {}/{} | projects {} | attention {}",
        if status.auto_mode { "on" } else { "off" },
        status.live_sessions,
        status.live_session_cap,
        status.projects.len(),
        status.attention.len()
    )
    .map_err(|error| error.to_string())?;

    for project in &status.projects {
        writeln!(
            output,
            "\n{} ({}) | agents {} | unassigned {}",
            one_line(&project.project.name),
            project.project.id,
            project.agents.len(),
            project.unassigned_queue_depth
        )
        .map_err(|error| error.to_string())?;
        for agent in &project.agents {
            let pause = if agent.agent.paused { " | paused" } else { "" };
            writeln!(
                output,
                "  {} | {}{} | queue {} | inbox {} | {}",
                agent.agent.id,
                session_label(agent.session.as_ref().map(|session| session.state)),
                pause,
                agent.queue_depth,
                agent.inbox_pending,
                worktree_label(agent.worktree.as_ref())
            )
            .map_err(|error| error.to_string())?;
        }
    }

    if !status.attention.is_empty() {
        writeln!(output, "\nAttention:").map_err(|error| error.to_string())?;
        for item in &status.attention {
            let mut subject = item.project_id.to_string();
            if let Some(agent_id) = &item.agent_id {
                subject.push('/');
                subject.push_str(agent_id.as_str());
            }
            if let Some(task_id) = &item.task_id {
                subject.push_str(" task ");
                subject.push_str(task_id.as_str());
            }
            writeln!(
                output,
                "  {} | {} | {}",
                attention_label(item.kind),
                subject,
                one_line(&item.detail)
            )
            .map_err(|error| error.to_string())?;
        }
    }

    output.flush().map_err(|error| error.to_string())
}

const fn session_label(state: Option<SessionState>) -> &'static str {
    match state {
        None => "no session",
        Some(SessionState::Starting) => "starting",
        Some(SessionState::Idle) => "idle",
        Some(SessionState::Working) => "working",
        Some(SessionState::WaitingForInput) => "waiting for input",
        Some(SessionState::Stopped) => "stopped",
        Some(SessionState::Failed) => "failed",
    }
}

fn worktree_label(worktree: Option<&WorktreeStatus>) -> String {
    let Some(worktree) = worktree else {
        return "no worktree".to_owned();
    };
    if let Some(error) = &worktree.error {
        return format!("worktree unavailable: {}", one_line(error));
    }
    let state = if worktree.dirty {
        format!("dirty ({} files)", worktree.changed_files)
    } else {
        "clean".to_owned()
    };
    match &worktree.branch {
        Some(branch) => format!("{state} on {}", one_line(branch)),
        None => format!("{state} on detached HEAD"),
    }
}

const fn attention_label(kind: AttentionKind) -> &'static str {
    match kind {
        AttentionKind::NeedsInput => "needs input",
        AttentionKind::BudgetExhausted => "budget exhausted",
        AttentionKind::SessionFailed => "session failed",
        AttentionKind::TaskBlocked => "task blocked",
        AttentionKind::PausedWithWork => "paused with work",
        AttentionKind::WaitingForCapacity => "waiting for capacity",
    }
}

fn one_line(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use factory_core::{
        AgentBudget, AgentId, AgentRole, AgentSnapshot, ProjectId, ProjectSnapshot, Provider,
        SessionId, SessionSnapshot,
        attention::Attention,
        status::{AgentStatus, AttentionItem, ProjectStatus},
    };

    use super::*;

    fn id<T>(value: &str) -> T
    where
        T: TryFrom<String>,
        <T as TryFrom<String>>::Error: std::fmt::Debug,
    {
        value.to_owned().try_into().unwrap()
    }

    fn session(agent_id: &str, state: SessionState) -> SessionSnapshot {
        SessionSnapshot {
            id: id(&format!("session-{agent_id}")),
            project_id: id("factory"),
            agent_id: id(agent_id),
            provider: Provider::Codex,
            state,
            state_since_ms: 1,
            worktree: format!("/tmp/{agent_id}"),
            provider_session_id: None,
            current_run_id: None,
            activity: None,
            activity_inferred: false,
            last_hook_event: None,
            last_hook_at_ms: None,
            wait_reason: None,
            observer_health: Default::default(),
            observer_health_since_ms: 1,
            started_at_ms: 1,
            updated_at_ms: 1,
            ended_at_ms: None,
            exit_code: None,
            exit_signal: None,
        }
    }

    fn agent(
        agent_id: &str,
        state: Option<SessionState>,
        queue_depth: u32,
        inbox_pending: u32,
        worktree: Option<WorktreeStatus>,
    ) -> AgentStatus {
        AgentStatus {
            agent: AgentSnapshot {
                id: id(agent_id),
                project_id: id("factory"),
                parent_agent_id: None,
                role: AgentRole::Worker,
                provider: Provider::Codex,
                current_run_id: None,
                paused: agent_id == "paused-worker",
                current_session_id: state.map(|_| id(&format!("session-{agent_id}"))),
                worktree: Some(format!("/tmp/{agent_id}")),
                created_at_ms: 1,
                updated_at_ms: 1,
            },
            budget: AgentBudget::default(),
            pause_reasons: Vec::new(),
            worktree,
            session: state.map(|state| session(agent_id, state)),
            current_run: None,
            queue_depth,
            queue: Vec::new(),
            inbox_pending,
            attention: Attention::Routine,
            attention_inferred: false,
        }
    }

    #[test]
    fn fleet_summary_keeps_operational_state_scannable_and_single_line() {
        let project_id: ProjectId = id("factory");
        let status = FleetStatus {
            generated_at_ms: 1,
            auto_mode: true,
            live_session_cap: 4,
            live_sessions: 2,
            projects: vec![
                ProjectStatus {
                    project: ProjectSnapshot {
                        id: project_id.clone(),
                        name: "Dark\nFactory".into(),
                        root: "/work/factory".into(),
                        created_at_ms: 1,
                        updated_at_ms: 1,
                    },
                    agents: vec![
                        agent(
                            "author",
                            Some(SessionState::Working),
                            2,
                            1,
                            Some(WorktreeStatus {
                                path: "/tmp/author".into(),
                                branch: Some("feature/status".into()),
                                changed_files: 3,
                                dirty: true,
                                error: None,
                            }),
                        ),
                        agent(
                            "paused-worker",
                            None,
                            1,
                            0,
                            Some(WorktreeStatus {
                                path: "/tmp/paused-worker".into(),
                                branch: None,
                                changed_files: 0,
                                dirty: false,
                                error: Some("git timed\nout".into()),
                            }),
                        ),
                        agent(
                            "reviewer",
                            Some(SessionState::WaitingForInput),
                            0,
                            0,
                            Some(WorktreeStatus {
                                path: "/tmp/reviewer".into(),
                                branch: Some("review/status".into()),
                                changed_files: 0,
                                dirty: false,
                                error: None,
                            }),
                        ),
                    ],
                    unassigned_queue_depth: 5,
                    unassigned_queue: Vec::new(),
                },
                ProjectStatus {
                    project: ProjectSnapshot {
                        id: id("empty"),
                        name: "Empty".into(),
                        root: "/work/empty".into(),
                        created_at_ms: 1,
                        updated_at_ms: 1,
                    },
                    agents: Vec::new(),
                    unassigned_queue_depth: 0,
                    unassigned_queue: Vec::new(),
                },
            ],
            attention: vec![
                AttentionItem {
                    kind: AttentionKind::NeedsInput,
                    level: Attention::NeedsInput,
                    project_id: project_id.clone(),
                    agent_id: Some(AgentId::try_from("reviewer").unwrap()),
                    task_id: None,
                    session_id: Some(SessionId::try_from("session-reviewer").unwrap()),
                    since_ms: 1,
                    detail: "approve\ncommand".into(),
                },
                AttentionItem {
                    kind: AttentionKind::TaskBlocked,
                    level: Attention::NeedsInput,
                    project_id,
                    agent_id: None,
                    task_id: Some(id("task-7")),
                    session_id: None,
                    since_ms: 2,
                    detail: "dependency missing".into(),
                },
            ],
        };

        let mut output = Vec::new();
        write(&mut output, &status).unwrap();
        assert_eq!(
            String::from_utf8(output).unwrap(),
            concat!(
                "Dark Factory: auto on | sessions 2/4 | projects 2 | attention 2\n",
                "\nDark Factory (factory) | agents 3 | unassigned 5\n",
                "  author | working | queue 2 | inbox 1 | dirty (3 files) on feature/status\n",
                "  paused-worker | no session | paused | queue 1 | inbox 0 | worktree unavailable: git timed out\n",
                "  reviewer | waiting for input | queue 0 | inbox 0 | clean on review/status\n",
                "\nEmpty (empty) | agents 0 | unassigned 0\n",
                "\nAttention:\n",
                "  needs input | factory/reviewer | approve command\n",
                "  task blocked | factory task task-7 | dependency missing\n",
            )
        );
    }

    #[test]
    fn empty_fleet_is_one_concise_line() {
        let status = FleetStatus {
            generated_at_ms: 1,
            auto_mode: false,
            live_session_cap: 3,
            live_sessions: 0,
            projects: Vec::new(),
            attention: Vec::new(),
        };
        let mut output = Vec::new();
        write(&mut output, &status).unwrap();
        assert_eq!(
            String::from_utf8(output).unwrap(),
            "Dark Factory: auto off | sessions 0/3 | projects 0 | attention 0\n"
        );
    }
}
