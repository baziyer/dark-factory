use std::{env, path::PathBuf, process, time::Duration};

use factory_core::{
    AgentId, PROTOCOL_VERSION, ProjectId, Provider, TaskId,
    local::{
        LocalRequest, LocalResponse, MAX_LOCAL_FRAME_BYTES, RequestEnvelope, ServerFrame,
        SubscriptionFailureCategory, SubscriptionLimitWindow, SubscriptionProbeOutcome,
        SubscriptionSeverity,
    },
};
use factoryd::usage_monitor::{
    ClaudeCollectorConfig, CodexCollectorConfig, CollectorError, NormalizedUsage, collect_claude,
    collect_codex,
};
use serde_json::json;
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::UnixStream,
    time::timeout,
};
use uuid::Uuid;

const USAGE: &str = "usage: factory-usage-monitor --socket PATH --project ID --orchestrator ID --codex PATH --codex-home PATH --claude PATH --home PATH [--tmpdir PATH] [--pty PATH] [--timeout-seconds N]";

struct Cli {
    socket: PathBuf,
    project_id: ProjectId,
    orchestrator_agent_id: AgentId,
    codex: PathBuf,
    codex_home: PathBuf,
    claude: PathBuf,
    home: PathBuf,
    temp_dir: PathBuf,
    pty: PathBuf,
    timeout: Duration,
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_target(false)
        .compact()
        .init();
    let arguments = env::args().skip(1).collect::<Vec<_>>();
    if arguments
        .iter()
        .any(|argument| argument == "-h" || argument == "--help")
    {
        println!("{USAGE}");
        return;
    }
    let cli = match parse_arguments(arguments) {
        Ok(cli) => cli,
        Err(message) => {
            eprintln!(
                "{}",
                json!({"ok": false, "category": "usage", "message": message})
            );
            process::exit(2);
        }
    };
    match run(cli).await {
        Ok(summary) => println!("{summary}"),
        Err(message) => {
            eprintln!(
                "{}",
                json!({"ok": false, "category": "monitor", "message": message})
            );
            process::exit(1);
        }
    }
}

async fn run(cli: Cli) -> Result<serde_json::Value, &'static str> {
    let attempted_at_ms = unix_time_ms();
    let (codex, claude) = tokio::join!(
        collect_codex(CodexCollectorConfig {
            codex_program: cli.codex,
            codex_home: cli.codex_home,
            home: cli.home.clone(),
            temp_dir: cli.temp_dir.clone(),
            timeout: cli.timeout,
        }),
        collect_claude(ClaudeCollectorConfig {
            pty_program: cli.pty,
            claude_program: cli.claude,
            home: cli.home,
            temp_dir: cli.temp_dir,
            timeout: cli.timeout,
        })
    );

    let mut observed = 0_u8;
    let mut failed = 0_u8;
    let mut notifications = 0_u8;
    for (provider, result) in [(Provider::Codex, codex), (Provider::ClaudeCode, claude)] {
        let (outcome, outcome_label) = match result {
            Ok(usage) => {
                observed = observed.saturating_add(1);
                (observed_outcome(usage), "observed")
            }
            Err(error) => {
                failed = failed.saturating_add(1);
                (
                    SubscriptionProbeOutcome::Failed {
                        category: failure_category(error),
                    },
                    "failed",
                )
            }
        };
        let notification_id = TaskId::try_from(format!(
            "subscription-{}-{}",
            provider_name(provider),
            Uuid::new_v4()
        ))
        .map_err(|_| "notification identity unavailable")?;
        let response = record_probe(
            &cli.socket,
            LocalRequest::RecordSubscriptionProbe {
                project_id: cli.project_id.clone(),
                orchestrator_agent_id: cli.orchestrator_agent_id.clone(),
                provider,
                attempted_at_ms,
                outcome,
                notification_task_id: notification_id,
            },
        )
        .await?;
        let (severity, consecutive_failures, notification_created) = match response {
            LocalResponse::SubscriptionProbeRecorded {
                provider: recorded_provider,
                severity,
                consecutive_failures,
                notification_created,
            } if recorded_provider == provider => {
                (severity, consecutive_failures, notification_created)
            }
            _ => return Err("usage state commit failed"),
        };
        notifications = notifications.saturating_add(u8::from(notification_created));
        tracing::info!(
            event = "subscription_probe_committed",
            provider = provider_name(provider),
            outcome = outcome_label,
            severity = severity_name(severity),
            consecutive_failures,
            notification_created
        );
    }
    Ok(json!({
        "ok": true,
        "healthy": failed == 0,
        "providersObserved": observed,
        "providersFailed": failed,
        "notificationsCreated": notifications
    }))
}

async fn record_probe(
    socket: &std::path::Path,
    request: LocalRequest,
) -> Result<LocalResponse, &'static str> {
    timeout(Duration::from_secs(15), async {
        let mut stream = UnixStream::connect(socket)
            .await
            .map_err(|_| "factory daemon unavailable")?;
        let mut payload = serde_json::to_vec(&RequestEnvelope::new(request))
            .map_err(|_| "usage request encoding failed")?;
        if payload.len() > MAX_LOCAL_FRAME_BYTES {
            return Err("usage request exceeds local limit");
        }
        payload.push(b'\n');
        stream
            .write_all(&payload)
            .await
            .map_err(|_| "usage request failed")?;
        stream.flush().await.map_err(|_| "usage request failed")?;
        let mut reader = BufReader::new(stream).take((MAX_LOCAL_FRAME_BYTES + 2) as u64);
        let mut response = Vec::new();
        let read = reader
            .read_until(b'\n', &mut response)
            .await
            .map_err(|_| "usage response failed")?;
        if read == 0
            || response.last() != Some(&b'\n')
            || response.len() - 1 > MAX_LOCAL_FRAME_BYTES
        {
            return Err("usage response was invalid");
        }
        response.pop();
        let frame: ServerFrame =
            serde_json::from_slice(&response).map_err(|_| "usage response was invalid")?;
        match frame {
            ServerFrame::Response {
                protocol_version: PROTOCOL_VERSION,
                response: LocalResponse::Error { .. },
            } => Err("usage state commit failed"),
            ServerFrame::Response {
                protocol_version: PROTOCOL_VERSION,
                response,
            } => Ok(response),
            _ => Err("usage response protocol mismatch"),
        }
    })
    .await
    .map_err(|_| "usage state commit timed out")?
}

const fn observed_outcome(usage: NormalizedUsage) -> SubscriptionProbeOutcome {
    SubscriptionProbeOutcome::Observed {
        used_percent: usage.used_percent,
        limit_window: match usage.limit_window {
            factoryd::store::SubscriptionLimitWindow::Primary => SubscriptionLimitWindow::Primary,
            factoryd::store::SubscriptionLimitWindow::Secondary => {
                SubscriptionLimitWindow::Secondary
            }
            factoryd::store::SubscriptionLimitWindow::CurrentSession => {
                SubscriptionLimitWindow::CurrentSession
            }
            factoryd::store::SubscriptionLimitWindow::CurrentWeek => {
                SubscriptionLimitWindow::CurrentWeek
            }
        },
        resets_at_ms: usage.resets_at_ms,
        exhausted: usage.exhausted,
    }
}

const fn failure_category(error: CollectorError) -> SubscriptionFailureCategory {
    match error {
        CollectorError::Timeout => SubscriptionFailureCategory::Timeout,
        CollectorError::Protocol => SubscriptionFailureCategory::Protocol,
        CollectorError::Process => SubscriptionFailureCategory::Process,
        CollectorError::OutputLimit => SubscriptionFailureCategory::OutputLimit,
    }
}

const fn provider_name(provider: Provider) -> &'static str {
    match provider {
        Provider::ClaudeCode => "claude",
        Provider::Codex => "codex",
    }
}

const fn severity_name(severity: SubscriptionSeverity) -> &'static str {
    match severity {
        SubscriptionSeverity::Ok => "ok",
        SubscriptionSeverity::Warning => "warning",
        SubscriptionSeverity::Critical => "critical",
    }
}

fn unix_time_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| {
            i64::try_from(duration.as_millis()).unwrap_or(i64::MAX)
        })
}

fn parse_arguments(mut arguments: Vec<String>) -> Result<Cli, &'static str> {
    let socket = take_required_path(&mut arguments, "--socket")?;
    let project_id = ProjectId::try_from(take_required(&mut arguments, "--project")?)
        .map_err(|_| "--project is invalid")?;
    let orchestrator_agent_id = AgentId::try_from(take_required(&mut arguments, "--orchestrator")?)
        .map_err(|_| "--orchestrator is invalid")?;
    let codex = take_required_path(&mut arguments, "--codex")?;
    let codex_home = take_required_path(&mut arguments, "--codex-home")?;
    let claude = take_required_path(&mut arguments, "--claude")?;
    let home = take_required_path(&mut arguments, "--home")?;
    let temp_dir = take_optional(&mut arguments, "--tmpdir")?
        .map_or_else(|| PathBuf::from("/tmp"), PathBuf::from);
    let pty = take_optional(&mut arguments, "--pty")?
        .map_or_else(|| PathBuf::from("/usr/bin/script"), PathBuf::from);
    let timeout_seconds =
        take_optional(&mut arguments, "--timeout-seconds")?.map_or(Ok(15_u64), |value| {
            value
                .parse::<u64>()
                .ok()
                .filter(|seconds| (1..=60).contains(seconds))
                .ok_or("--timeout-seconds must be between 1 and 60")
        })?;
    if !arguments.is_empty() {
        return Err("unknown or duplicated argument");
    }
    Ok(Cli {
        socket,
        project_id,
        orchestrator_agent_id,
        codex,
        codex_home,
        claude,
        home,
        temp_dir,
        pty,
        timeout: Duration::from_secs(timeout_seconds),
    })
}

fn take_required(arguments: &mut Vec<String>, option: &str) -> Result<String, &'static str> {
    take_optional(arguments, option)?
        .filter(|value| !value.is_empty())
        .ok_or("a required option is missing")
}

fn take_required_path(arguments: &mut Vec<String>, option: &str) -> Result<PathBuf, &'static str> {
    take_required(arguments, option).map(PathBuf::from)
}

fn take_optional(
    arguments: &mut Vec<String>,
    option: &str,
) -> Result<Option<String>, &'static str> {
    let positions = arguments
        .iter()
        .enumerate()
        .filter_map(|(index, argument)| (argument == option).then_some(index))
        .collect::<Vec<_>>();
    if positions.len() > 1 {
        return Err("an option was provided more than once");
    }
    let Some(index) = positions.first().copied() else {
        return Ok(None);
    };
    if index + 1 >= arguments.len() || arguments[index + 1].starts_with("--") {
        return Err("an option value is missing");
    }
    arguments.remove(index);
    Ok(Some(arguments.remove(index)))
}

#[cfg(test)]
mod tests {
    use super::parse_arguments;

    #[test]
    fn account_and_provider_paths_are_explicit() {
        let parsed = parse_arguments(
            [
                "--socket",
                "/state/factory.sock",
                "--project",
                "factory",
                "--orchestrator",
                "god",
                "--codex",
                "/bin/codex",
                "--codex-home",
                "/account/codex",
                "--claude",
                "/bin/claude",
                "--home",
                "/account/home",
            ]
            .into_iter()
            .map(str::to_owned)
            .collect(),
        )
        .unwrap();
        assert_eq!(parsed.timeout.as_secs(), 15);
    }

    #[test]
    fn unknown_private_values_are_not_echoed() {
        let error = parse_arguments(vec!["--private-sentinel".into()])
            .err()
            .unwrap();
        assert!(!error.contains("private-sentinel"));
    }
}
