use std::io::Write;

use factory_core::{
    RunOutcome, RunPhase, RunSnapshot,
    status::{FleetStatus, age_text, display_text},
};

pub fn write_with_daemon_version(
    output: &mut impl Write,
    status: &FleetStatus,
    daemon_version: Option<&str>,
) -> Result<(), String> {
    let versions = daemon_version
        .filter(|version| !version.is_empty())
        .map_or_else(
            || {
                format!(
                    "factoryctl v{} | active runtime version unknown",
                    env!("CARGO_PKG_VERSION")
                )
            },
            |version| {
                format!(
                    "factoryctl v{} | active runtime v{version}",
                    env!("CARGO_PKG_VERSION")
                )
            },
        );
    let decisions: Vec<_> = status
        .attention
        .iter()
        .filter(|item| item.level.needs_operator() && item.needs_operator_decision())
        .collect();
    writeln!(
        output,
        "Dark Factory: {versions} | auto {} | attempts {}/{} | projects {} | attention {}",
        if status.auto_mode { "on" } else { "off" },
        status.active_runs,
        status.active_run_cap,
        status.projects.len(),
        decisions.len()
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
                "  {} | {}{} | queue {} | inbox {}",
                agent.agent.id,
                run_label(agent.current_run.as_ref(), agent.latest_run.as_ref()),
                pause,
                agent.queue_depth,
                agent.inbox_pending,
            )
            .map_err(|error| error.to_string())?;
        }
    }

    if !decisions.is_empty() {
        writeln!(output, "\nAttention:").map_err(|error| error.to_string())?;
        for item in decisions {
            let mut subject = item.project_id.to_string();
            if let Some(agent_id) = &item.agent_id {
                subject.push('/');
                subject.push_str(agent_id.as_str());
            }
            let decision = item.decision();
            let action = decision
                .recommended
                .and_then(|index| decision.choices.get(index))
                .map_or("inspect status", |choice| choice.label.as_str());
            writeln!(
                output,
                "  {} | {} | age {} | cause: {} | evidence: {} | action: {}",
                item.reason.kind.label(),
                subject,
                age_text(status.generated_at_ms, item.since_ms),
                display_text(&decision.cause),
                display_text(&decision.evidence),
                action,
            )
            .map_err(|error| error.to_string())?;
            for (index, choice) in decision.choices.iter().enumerate() {
                writeln!(
                    output,
                    "    {}. {}{} — {}",
                    index + 1,
                    choice.label,
                    if decision.recommended == Some(index) {
                        " (recommended)"
                    } else {
                        ""
                    },
                    choice.consequence,
                )
                .map_err(|error| error.to_string())?;
            }
        }
    }

    output.flush().map_err(|error| error.to_string())
}

fn run_label(current: Option<&RunSnapshot>, latest: Option<&RunSnapshot>) -> String {
    if let Some(run) = current {
        let phase = match run.phase {
            RunPhase::Admitted => "admitted",
            RunPhase::Running => "running",
            RunPhase::Finalizing => "finalizing",
            RunPhase::Terminal => "terminal",
        };
        return format!("attempt {} {phase}", run.id);
    }
    match latest.and_then(|run| run.outcome.as_ref()) {
        None => "no attempt".to_owned(),
        Some(RunOutcome::Succeeded) => "last attempt succeeded".to_owned(),
        Some(RunOutcome::Blocked { .. }) => "last attempt blocked".to_owned(),
        Some(RunOutcome::Failed { .. }) => "last attempt failed".to_owned(),
        Some(RunOutcome::Cancelled { .. }) => "last attempt cancelled".to_owned(),
    }
}
