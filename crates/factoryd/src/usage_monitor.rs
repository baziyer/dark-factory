//! Deterministic, zero-model subscription allowance collectors.

use std::{
    fs,
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};

use factory_core::Provider;
use serde_json::{Value, json};
use thiserror::Error;
use tokio::{
    io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader},
    process::Command,
    time::{sleep, timeout},
};

use crate::store::SubscriptionLimitWindow;

const MAX_RAW_OUTPUT: usize = 64 * 1024;
const CLAUDE_READY_MARKER: &[u8] = b"plan mode on";
const CLAUDE_USAGE_DESCRIPTION: &[u8] = b"Show session cost, plan usage, and activity";
const KEYSTROKE_DELAY: Duration = Duration::from_millis(25);
const CLEAN_PATH: &str = "/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin";
const CLEAN_TERM: &str = "xterm-256color";
const CLEANUP_GRACE: Duration = Duration::from_secs(1);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct NormalizedUsageWindow {
    pub window: SubscriptionLimitWindow,
    pub used_percent: u8,
    pub resets_at_ms: Option<i64>,
    pub exhausted: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NormalizedUsage {
    pub provider: Provider,
    pub used_percent: u8,
    pub limit_window: SubscriptionLimitWindow,
    pub resets_at_ms: Option<i64>,
    pub exhausted: bool,
    pub windows: Vec<NormalizedUsageWindow>,
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum CollectorError {
    #[error("collector timed out")]
    Timeout,
    #[error("collector protocol was not recognized")]
    Protocol,
    #[error("collector process failed")]
    Process,
    #[error("collector output exceeded its bound")]
    OutputLimit,
}

pub struct ClaudeCollectorConfig {
    pub pty_program: PathBuf,
    pub claude_program: PathBuf,
    pub home: PathBuf,
    pub temp_dir: PathBuf,
    pub timeout: Duration,
}

pub struct CodexCollectorConfig {
    pub codex_program: PathBuf,
    pub codex_home: PathBuf,
    pub home: PathBuf,
    pub temp_dir: PathBuf,
    pub timeout: Duration,
}

pub async fn collect_codex(
    config: CodexCollectorConfig,
) -> Result<NormalizedUsage, CollectorError> {
    let codex_program = validated_executable(&config.codex_program)?;
    let codex_home = validated_private_directory(&config.codex_home)?;
    let home = validated_private_directory(&config.home)?;
    let temp_dir = validated_temp_directory(&config.temp_dir)?;
    let mut child = Command::new(codex_program)
        .arg("app-server")
        .arg("--listen")
        .arg("stdio://")
        .env_clear()
        .env("CODEX_HOME", codex_home)
        .env("HOME", home)
        .env("PATH", CLEAN_PATH)
        .env("TERM", CLEAN_TERM)
        .env("TMPDIR", temp_dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(|_| CollectorError::Process)?;
    let input = child.stdin.take().ok_or(CollectorError::Process)?;
    let output = child.stdout.take().ok_or(CollectorError::Process)?;
    let result = collect_codex_protocol(output, input, config.timeout).await;
    if child
        .try_wait()
        .map_err(|_| CollectorError::Process)?
        .is_none()
    {
        child.kill().await.map_err(|_| CollectorError::Process)?;
    }
    child.wait().await.map_err(|_| CollectorError::Process)?;
    result
}

/// Perform only the app-server initialization handshake and the structured
/// account rate-limit read. No thread, turn, or credit method is emitted.
pub async fn collect_codex_protocol<R, W>(
    read: R,
    mut write: W,
    limit: Duration,
) -> Result<NormalizedUsage, CollectorError>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    timeout(limit, async move {
        write_json_line(
            &mut write,
            &json!({
                "id": 1,
                "method": "initialize",
                "params": {
                    "clientInfo": {
                        "name": "factory-usage-monitor",
                        "version": env!("CARGO_PKG_VERSION")
                    },
                    "capabilities": { "experimentalApi": true }
                }
            }),
        )
        .await?;
        let mut lines = BoundedJsonLines::new(read);
        wait_for_response(&mut lines, 1).await?;

        write_json_line(&mut write, &json!({ "method": "initialized" })).await?;
        write_json_line(
            &mut write,
            &json!({
                "id": 2,
                "method": "account/rateLimits/read",
                "params": null
            }),
        )
        .await?;
        let result = wait_for_response(&mut lines, 2).await?;
        parse_codex_rate_limits(&result)
    })
    .await
    .map_err(|_| CollectorError::Timeout)?
}

async fn write_json_line<W: AsyncWrite + Unpin>(
    write: &mut W,
    value: &Value,
) -> Result<(), CollectorError> {
    let mut encoded = serde_json::to_vec(value).map_err(|_| CollectorError::Protocol)?;
    encoded.push(b'\n');
    write
        .write_all(&encoded)
        .await
        .map_err(|_| CollectorError::Process)?;
    write.flush().await.map_err(|_| CollectorError::Process)
}

struct BoundedJsonLines<R> {
    read: BufReader<R>,
    consumed: usize,
}

impl<R: AsyncRead + Unpin> BoundedJsonLines<R> {
    fn new(read: R) -> Self {
        Self {
            read: BufReader::new(read),
            consumed: 0,
        }
    }

    async fn next(&mut self) -> Result<Value, CollectorError> {
        let mut line = Vec::new();
        loop {
            let available = self
                .read
                .fill_buf()
                .await
                .map_err(|_| CollectorError::Process)?;
            if available.is_empty() {
                return Err(CollectorError::Protocol);
            }
            let ending = available
                .iter()
                .position(|byte| *byte == b'\n')
                .map_or(available.len(), |position| position + 1);
            if self.consumed.saturating_add(ending) > MAX_RAW_OUTPUT {
                return Err(CollectorError::OutputLimit);
            }
            line.extend_from_slice(&available[..ending]);
            self.read.consume(ending);
            self.consumed += ending;
            if line.last() == Some(&b'\n') {
                break;
            }
        }
        serde_json::from_slice(&line).map_err(|_| CollectorError::Protocol)
    }
}

async fn wait_for_response<R: AsyncRead + Unpin>(
    lines: &mut BoundedJsonLines<R>,
    expected_id: i64,
) -> Result<Value, CollectorError> {
    loop {
        let message = lines.next().await?;
        match message.get("id").and_then(Value::as_i64) {
            Some(id) if id == expected_id => {
                if message.get("error").is_some() {
                    return Err(CollectorError::Protocol);
                }
                return message
                    .get("result")
                    .cloned()
                    .ok_or(CollectorError::Protocol);
            }
            Some(_) => return Err(CollectorError::Protocol),
            None => {
                // Bounded notifications are unrelated to this read and require
                // no client response. They are intentionally discarded.
            }
        }
    }
}

fn parse_codex_rate_limits(result: &Value) -> Result<NormalizedUsage, CollectorError> {
    let mut selected: Option<(u8, SubscriptionLimitWindow, Option<i64>)> = None;
    let mut windows = Vec::new();
    let mut exhausted = false;
    let historical = result
        .get("rateLimits")
        .and_then(Value::as_object)
        .ok_or(CollectorError::Protocol)?;
    inspect_codex_snapshot(historical, &mut selected, &mut windows, &mut exhausted)?;
    if let Some(buckets) = result.get("rateLimitsByLimitId") {
        if !buckets.is_null() {
            let buckets = buckets.as_object().ok_or(CollectorError::Protocol)?;
            for snapshot in buckets.values() {
                inspect_codex_snapshot(
                    snapshot.as_object().ok_or(CollectorError::Protocol)?,
                    &mut selected,
                    &mut windows,
                    &mut exhausted,
                )?;
            }
        }
    }
    let (used_percent, limit_window, resets_at_ms) = selected.ok_or(CollectorError::Protocol)?;
    Ok(NormalizedUsage {
        provider: Provider::Codex,
        used_percent,
        limit_window,
        resets_at_ms,
        exhausted,
        windows: coalesce_usage_windows(windows),
    })
}

fn inspect_codex_snapshot(
    snapshot: &serde_json::Map<String, Value>,
    selected: &mut Option<(u8, SubscriptionLimitWindow, Option<i64>)>,
    windows: &mut Vec<NormalizedUsageWindow>,
    exhausted: &mut bool,
) -> Result<(), CollectorError> {
    let snapshot_exhausted = snapshot
        .get("rateLimitReachedType")
        .is_some_and(|value| !value.is_null());
    *exhausted |= snapshot_exhausted;
    for (name, window) in [
        ("primary", SubscriptionLimitWindow::Primary),
        ("secondary", SubscriptionLimitWindow::Secondary),
    ] {
        let Some(value) = snapshot.get(name) else {
            continue;
        };
        if value.is_null() {
            continue;
        }
        let value = value.as_object().ok_or(CollectorError::Protocol)?;
        let percent = value
            .get("usedPercent")
            .and_then(Value::as_i64)
            .and_then(|value| u8::try_from(value).ok())
            .filter(|value| *value <= 100)
            .ok_or(CollectorError::Protocol)?;
        let reset = value
            .get("resetsAt")
            .filter(|value| !value.is_null())
            .map(|value| {
                value
                    .as_i64()
                    .filter(|seconds| *seconds >= 0)
                    .and_then(|seconds| seconds.checked_mul(1_000))
                    .ok_or(CollectorError::Protocol)
            })
            .transpose()?;
        windows.push(NormalizedUsageWindow {
            window,
            used_percent: percent,
            resets_at_ms: reset,
            exhausted: snapshot_exhausted,
        });
        if selected
            .as_ref()
            .is_none_or(|(current, _, _)| percent > *current)
        {
            *selected = Some((percent, window, reset));
        }
    }
    Ok(())
}

/// Run Claude's built-in `/usage` command in a real PTY. Enter is withheld
/// until the exact built-in autocomplete description proves this cannot be a
/// model prompt.
pub async fn collect_claude(
    config: ClaudeCollectorConfig,
) -> Result<NormalizedUsage, CollectorError> {
    let pty_program = validated_executable(&config.pty_program)?;
    let claude_program = validated_executable(&config.claude_program)?;
    let home = validated_private_directory(&config.home)?;
    let temp_dir = validated_temp_directory(&config.temp_dir)?;
    let mut child = Command::new(pty_program)
        .arg("-q")
        .arg("/dev/null")
        .arg(claude_program)
        .arg("--safe-mode")
        .arg("--ax-screen-reader")
        .arg("--permission-mode")
        .arg("plan")
        .arg("--tools")
        .arg("")
        .arg("--no-chrome")
        .env_clear()
        .env("HOME", home)
        .env("PATH", CLEAN_PATH)
        .env("TERM", CLEAN_TERM)
        .env("TMPDIR", temp_dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(|_| CollectorError::Process)?;
    let mut input = child.stdin.take().ok_or(CollectorError::Process)?;
    let mut output = child.stdout.take().ok_or(CollectorError::Process)?;

    let interaction = timeout(config.timeout, async {
        let mut raw = Vec::new();
        read_until_marker(&mut output, &mut raw, CLAUDE_READY_MARKER, true, 0).await?;
        let autocomplete_start = raw.len();
        for byte in b"/usage" {
            input
                .write_all(&[*byte])
                .await
                .map_err(|_| CollectorError::Process)?;
            input.flush().await.map_err(|_| CollectorError::Process)?;
            sleep(KEYSTROKE_DELAY).await;
        }
        read_until_marker(
            &mut output,
            &mut raw,
            CLAUDE_USAGE_DESCRIPTION,
            false,
            autocomplete_start,
        )
        .await?;
        input
            .write_all(b"\r")
            .await
            .map_err(|_| CollectorError::Process)?;
        input.flush().await.map_err(|_| CollectorError::Process)?;
        loop {
            if let Ok(usage) = parse_claude_usage(&raw) {
                return Ok(usage);
            }
            read_more_bounded(&mut output, &mut raw).await?;
        }
    })
    .await;

    let result = match interaction {
        Ok(result) => result,
        Err(_) => Err(CollectorError::Timeout),
    };
    terminate_pty(&mut child, input).await?;
    result
}

async fn terminate_pty(
    child: &mut tokio::process::Child,
    mut input: tokio::process::ChildStdin,
) -> Result<(), CollectorError> {
    // Interrupt the foreground PTY process group before closing the master.
    // BSD `script` then reaps its child; SIGKILL is only a bounded fallback for
    // a broken wrapper.
    let _ = input.write_all(b"\x03\x04").await;
    let _ = input.flush().await;
    let _ = input.shutdown().await;
    drop(input);
    if child
        .try_wait()
        .map_err(|_| CollectorError::Process)?
        .is_some()
    {
        return Ok(());
    }
    match timeout(CLEANUP_GRACE, child.wait()).await {
        Ok(status) => {
            status.map_err(|_| CollectorError::Process)?;
        }
        Err(_) => {
            child.kill().await.map_err(|_| CollectorError::Process)?;
            child.wait().await.map_err(|_| CollectorError::Process)?;
        }
    }
    Ok(())
}

fn validated_executable(path: &Path) -> Result<PathBuf, CollectorError> {
    let canonical = fs::canonicalize(path).map_err(|_| CollectorError::Process)?;
    let metadata = fs::metadata(&canonical).map_err(|_| CollectorError::Process)?;
    let current_uid = rustix::process::getuid().as_raw();
    if !metadata.file_type().is_file()
        || metadata.mode() & 0o111 == 0
        || metadata.mode() & 0o022 != 0
        || !matches!(metadata.uid(), 0) && metadata.uid() != current_uid
    {
        return Err(CollectorError::Process);
    }
    Ok(canonical)
}

fn validated_private_directory(path: &Path) -> Result<PathBuf, CollectorError> {
    let canonical = fs::canonicalize(path).map_err(|_| CollectorError::Process)?;
    let metadata = fs::metadata(&canonical).map_err(|_| CollectorError::Process)?;
    if !metadata.file_type().is_dir()
        || metadata.uid() != rustix::process::getuid().as_raw()
        || metadata.mode() & 0o022 != 0
    {
        return Err(CollectorError::Process);
    }
    Ok(canonical)
}

fn validated_temp_directory(path: &Path) -> Result<PathBuf, CollectorError> {
    let canonical = fs::canonicalize(path).map_err(|_| CollectorError::Process)?;
    let metadata = fs::metadata(&canonical).map_err(|_| CollectorError::Process)?;
    let current_uid = rustix::process::getuid().as_raw();
    let private = metadata.uid() == current_uid && metadata.mode() & 0o022 == 0;
    let sticky_system = metadata.uid() == 0 && metadata.mode() & 0o1000 != 0;
    if !metadata.file_type().is_dir() || !(private || sticky_system) {
        return Err(CollectorError::Process);
    }
    Ok(canonical)
}

async fn read_until_marker<R: AsyncRead + Unpin>(
    read: &mut R,
    raw: &mut Vec<u8>,
    marker: &[u8],
    case_insensitive: bool,
    search_from: usize,
) -> Result<(), CollectorError> {
    loop {
        let searchable = raw.get(search_from..).unwrap_or_default();
        let found = if case_insensitive {
            contains_ascii_case_insensitive(searchable, marker)
        } else {
            searchable
                .windows(marker.len())
                .any(|window| window == marker)
        };
        if found {
            return Ok(());
        }
        read_more_bounded(read, raw).await?;
    }
}

async fn read_more_bounded<R: AsyncRead + Unpin>(
    read: &mut R,
    raw: &mut Vec<u8>,
) -> Result<(), CollectorError> {
    let mut chunk = [0_u8; 4096];
    let count = read
        .read(&mut chunk)
        .await
        .map_err(|_| CollectorError::Process)?;
    if count == 0 {
        return Err(CollectorError::Protocol);
    }
    if raw.len().saturating_add(count) > MAX_RAW_OUTPUT {
        return Err(CollectorError::OutputLimit);
    }
    raw.extend_from_slice(&chunk[..count]);
    Ok(())
}

fn contains_ascii_case_insensitive(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|window| {
        window
            .iter()
            .zip(needle)
            .all(|(left, right)| left.eq_ignore_ascii_case(right))
    })
}

/// Parse only screen-reader usage blocks. Free text containing percentages is
/// rejected unless both the exact current-session and current-week sections
/// are present.
pub fn parse_claude_usage(raw: &[u8]) -> Result<NormalizedUsage, CollectorError> {
    let text = sanitized_terminal_text(raw)?;
    let lines = text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>();
    let session_headings = lines
        .iter()
        .enumerate()
        .filter_map(|(index, line)| (*line == "Current session").then_some(index))
        .collect::<Vec<_>>();
    if session_headings.len() != 1 {
        return Err(CollectorError::Protocol);
    }
    let session_percent = parse_claude_usage_block(&lines, session_headings[0])?;
    let week_headings = lines
        .iter()
        .enumerate()
        .filter_map(|(index, line)| line.starts_with("Current week").then_some(index))
        .collect::<Vec<_>>();
    if week_headings.is_empty()
        || week_headings.iter().enumerate().any(|(ordinal, index)| {
            week_headings[..ordinal]
                .iter()
                .any(|earlier| lines[*earlier] == lines[*index])
        })
    {
        return Err(CollectorError::Protocol);
    }

    let mut selected = (session_percent, SubscriptionLimitWindow::CurrentSession);
    let mut windows = vec![NormalizedUsageWindow {
        window: SubscriptionLimitWindow::CurrentSession,
        used_percent: session_percent,
        resets_at_ms: None,
        exhausted: session_percent == 100,
    }];
    for heading in week_headings {
        let percent = parse_claude_usage_block(&lines, heading)?;
        windows.push(NormalizedUsageWindow {
            window: SubscriptionLimitWindow::CurrentWeek,
            used_percent: percent,
            resets_at_ms: None,
            exhausted: percent == 100,
        });
        if percent > selected.0 {
            selected = (percent, SubscriptionLimitWindow::CurrentWeek);
        }
    }
    let exhausted = selected.0 == 100 || text.to_ascii_lowercase().contains("limit reached");
    Ok(NormalizedUsage {
        provider: Provider::ClaudeCode,
        used_percent: selected.0,
        limit_window: selected.1,
        resets_at_ms: None,
        exhausted,
        windows: coalesce_usage_windows(windows),
    })
}

fn coalesce_usage_windows(windows: Vec<NormalizedUsageWindow>) -> Vec<NormalizedUsageWindow> {
    let mut result: Vec<NormalizedUsageWindow> = Vec::with_capacity(windows.len());
    for window in windows {
        if let Some(existing) = result
            .iter_mut()
            .find(|current| current.window == window.window)
        {
            if window.used_percent > existing.used_percent {
                existing.used_percent = window.used_percent;
                existing.resets_at_ms = window.resets_at_ms;
            }
            existing.exhausted |= window.exhausted;
        } else {
            result.push(window);
        }
    }
    result
}

fn parse_claude_usage_block(lines: &[&str], heading: usize) -> Result<u8, CollectorError> {
    let percent = lines
        .get(heading + 1)
        .and_then(|line| parse_used_percent(line))
        .ok_or(CollectorError::Protocol)?;
    let reset = lines
        .get(heading + 2)
        .and_then(|line| line.strip_prefix("Resets"))
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or(CollectorError::Protocol)?;
    let _localized_reset_label = reset;
    // Claude currently renders a localized human reset label rather than a
    // stable machine timestamp. Presence is required, but guessing an epoch is
    // intentionally forbidden; the normalized reset is therefore `None`.
    Ok(percent)
}

fn sanitized_terminal_text(raw: &[u8]) -> Result<String, CollectorError> {
    let mut clean = Vec::with_capacity(raw.len());
    let mut index = 0_usize;
    while index < raw.len() {
        if raw[index] != 0x1b {
            if raw[index] == b'\n'
                || raw[index] == b'\r'
                || raw[index] == b'\t'
                || raw[index] >= 0x20
            {
                clean.push(raw[index]);
            }
            index += 1;
            continue;
        }
        let Some(kind) = raw.get(index + 1).copied() else {
            return Err(CollectorError::Protocol);
        };
        match kind {
            b'[' => {
                index += 2;
                let ending = raw[index..]
                    .iter()
                    .position(|byte| (0x40..=0x7e).contains(byte))
                    .ok_or(CollectorError::Protocol)?;
                index += ending + 1;
            }
            b']' => {
                index += 2;
                let mut terminated = false;
                while index < raw.len() {
                    if raw[index] == 0x07 {
                        index += 1;
                        terminated = true;
                        break;
                    }
                    if raw[index] == 0x1b && raw.get(index + 1) == Some(&b'\\') {
                        index += 2;
                        terminated = true;
                        break;
                    }
                    index += 1;
                }
                if !terminated {
                    return Err(CollectorError::Protocol);
                }
            }
            _ => {
                index += 2;
            }
        }
    }
    Ok(String::from_utf8_lossy(&clean).into_owned())
}

fn parse_used_percent(line: &str) -> Option<u8> {
    let digits = line.strip_suffix("% used")?.trim();
    if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    digits.parse::<u8>().ok().filter(|value| *value <= 100)
}
