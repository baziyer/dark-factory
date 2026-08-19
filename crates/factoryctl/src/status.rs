use std::io::Write;

use factory_core::{
    SessionState,
    status::{FleetStatus, WorktreeStatus, age_text, display_text},
};

pub fn write_with_daemon_version(
    output: &mut impl Write,
    status: &FleetStatus,
    daemon_version: Option<&str>,
) -> Result<(), String> {
    let versions = daemon_version.map_or_else(
        || {
            format!(
                "factoryctl v{} | active runtime version unknown",
                env!("CARGO_PKG_VERSION")
            )
        },
        |version| {
            if version.is_empty() {
                format!(
                    "factoryctl v{} | active runtime version unknown",
                    env!("CARGO_PKG_VERSION")
                )
            } else {
                format!(
                    "factoryctl v{} | active runtime v{version}",
                    env!("CARGO_PKG_VERSION")
                )
            }
        },
    );
    writeln!(
        output,
        "Dark Factory: {versions} | auto {} | sessions {}/{} | projects {} | attention {}",
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
            "\n{} ({}) | agents {} | backlog {}",
            display_text(&project.project.name),
            project.project.id,
            project.agents.len(),
            project.backlog_depth
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
            writeln!(
                output,
                "  {} | {} | task {} | session {} | run {} | age {} | {} | action: {}",
                item.reason.kind.label(),
                subject,
                item.task_id
                    .as_ref()
                    .map_or("—", factory_core::TaskId::as_str),
                item.session_id
                    .as_ref()
                    .map_or("—", factory_core::SessionId::as_str),
                item.run_id
                    .as_ref()
                    .map_or("—", factory_core::RunId::as_str),
                age_text(status.generated_at_ms, item.since_ms),
                display_text(&item.reason.summary),
                item.action_text(),
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
        return format!("worktree unavailable: {}", display_text(error));
    }
    let state = if worktree.dirty {
        format!("dirty ({} files)", worktree.changed_files)
    } else {
        "clean".to_owned()
    };
    match &worktree.branch {
        Some(branch) => format!("{state} on {}", display_text(branch)),
        None => format!("{state} on detached HEAD"),
    }
}

#[cfg(test)]
mod tests {
    use factory_core::{
        AgentBudget, AgentId, AgentRole, AgentSnapshot, ProjectId, ProjectSnapshot, Provider,
        SessionId, SessionSnapshot,
        attention::Attention,
        status::{
            AgentStatus, AttentionAction, AttentionItem, AttentionReason, AttentionReasonKind,
            MAX_ATTENTION_SUMMARY_CHARS, ProjectStatus,
        },
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
            runtime_model: None,
            runtime_reasoning_effort: None,
            runtime_permission_mode: None,
            runtime_control_mode: None,
            state,
            state_since_ms: 1,
            worktree: format!("/tmp/{agent_id}"),
            provider_session_id: None,
            runner_instance_id: Some(id(&format!("runner-{agent_id}"))),
            current_run_id: None,
            activity: None,
            activity_inferred: false,
            last_hook_event: None,
            notification_kind: None,
            last_hook_at_ms: None,
            wait_reason: None,
            observer_reason: None,
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
            latest_run: None,
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
            event_sequence: 0,
            auto_mode: true,
            live_session_cap: 4,
            live_sessions: 2,
            projects: vec![
                ProjectStatus {
                    project: ProjectSnapshot {
                        id: project_id.clone(),
                        name: "Dark\nFactory \u{1b}[2J 東京🛠️e\u{301}".into(),
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
                                branch: Some("feature/\u{9}status\u{1b}]0;forged\u{7}".into()),
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
                                error: Some("git timed\nout\u{85}\u{0}".into()),
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
                    backlog_depth: 5,
                    backlog: Vec::new(),
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
                    backlog_depth: 0,
                    backlog: Vec::new(),
                },
            ],
            attention: vec![
                AttentionItem {
                    level: Attention::NeedsInput,
                    project_id: project_id.clone(),
                    agent_id: Some(AgentId::try_from("reviewer").unwrap()),
                    task_id: None,
                    session_id: Some(SessionId::try_from("session-reviewer").unwrap()),
                    run_id: None,
                    since_ms: 1,
                    reason: AttentionReason {
                        kind: AttentionReasonKind::ProviderPermission,
                        summary: display_text("approve\ncommand \u{1b}[2J\u{202e}FORGED\u{2066}"),
                        action: AttentionAction::ReviewProviderPermission,
                    },
                },
                AttentionItem {
                    level: Attention::NeedsInput,
                    project_id,
                    agent_id: None,
                    task_id: Some(id("task-7")),
                    session_id: None,
                    run_id: None,
                    since_ms: 2,
                    reason: AttentionReason {
                        kind: AttentionReasonKind::WorkerBlocked,
                        summary: "dependency missing".into(),
                        action: AttentionAction::RetryTask,
                    },
                },
            ],
        };

        let mut output = Vec::new();
        write_with_daemon_version(&mut output, &status, None).unwrap();
        let output = String::from_utf8(output).unwrap();
        assert_eq!(
            output,
            concat!(
                "Dark Factory: factoryctl v",
                env!("CARGO_PKG_VERSION"),
                " | active runtime version unknown | auto on | sessions 2/4 | projects 2 | attention 2\n",
                "\nDark Factory [2J 東京🛠️é (factory) | agents 3 | backlog 5\n",
                "  author | working | queue 2 | inbox 1 | dirty (3 files) on feature/ status]0;forged\n",
                "  paused-worker | no session | paused | queue 1 | inbox 0 | worktree unavailable: git timed out\n",
                "  reviewer | waiting for input | queue 0 | inbox 0 | clean on review/status\n",
                "\nEmpty (empty) | agents 0 | backlog 0\n",
                "\nAttention:\n",
                "  provider permission | factory/reviewer | task — | session session-reviewer | run — | age 0s | approve command [2JFORGED | action: review the provider prompt before entering terminal typing\n",
                "  worker blocked | factory | task task-7 | session — | run — | age 0s | dependency missing | action: factoryctl task retry --project factory --task task-7\n",
            )
        );
        assert!(
            output
                .chars()
                .all(|character| character == '\n' || !character.is_control()),
            "human output contains a terminal control: {output:?}"
        );
        assert!(output.contains("東京🛠️e\u{301}"));
        assert!(!output.contains('\u{202e}'));
        assert!(!output.contains('\u{2066}'));
    }

    #[test]
    fn display_text_is_bounded_after_normalizing_and_removing_controls() {
        let value = format!("  safe\t\u{1b}[2J\u{9b}{}  ", "界".repeat(200));
        let rendered = display_text(&value);
        assert_eq!(rendered.chars().count(), MAX_ATTENTION_SUMMARY_CHARS);
        assert!(rendered.starts_with("safe [2J"));
        assert!(rendered.ends_with('…'));
        assert!(rendered.chars().all(|character| !character.is_control()));
    }

    #[test]
    fn empty_fleet_is_one_concise_line() {
        let status = FleetStatus {
            generated_at_ms: 1,
            event_sequence: 0,
            auto_mode: false,
            live_session_cap: 3,
            live_sessions: 0,
            projects: Vec::new(),
            attention: Vec::new(),
        };
        let mut output = Vec::new();
        write_with_daemon_version(&mut output, &status, None).unwrap();
        assert_eq!(
            String::from_utf8(output).unwrap(),
            concat!(
                "Dark Factory: factoryctl v",
                env!("CARGO_PKG_VERSION"),
                " | active runtime version unknown | auto off | sessions 0/3 | projects 0 | attention 0\n"
            )
        );
    }

    #[test]
    fn human_status_names_client_and_daemon_versions() {
        let status = FleetStatus {
            generated_at_ms: 1,
            event_sequence: 0,
            auto_mode: false,
            live_session_cap: 1,
            live_sessions: 0,
            projects: Vec::new(),
            attention: Vec::new(),
        };
        let mut output = Vec::new();
        write_with_daemon_version(&mut output, &status, Some("0.3.0")).unwrap();
        let output = String::from_utf8(output).unwrap();
        assert!(output.starts_with(concat!(
            "Dark Factory: factoryctl v",
            env!("CARGO_PKG_VERSION"),
            " | active runtime v0.3.0 |"
        )));
    }
}
