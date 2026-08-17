//! On-demand Codex subscription usage probe.
//!
//! `factoryctl usage` runs this directly against `codex` on `PATH`; there is
//! no daemon involvement and nothing is persisted. Claude's usage is read by
//! the operator running `/usage` inside Claude's own interactive terminal.

use std::{
    env, fs,
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};

use factory_core::Provider;
use serde_json::{Value, json};
use thiserror::Error;
use tokio::{
    io::{AsyncRead, AsyncWrite, AsyncWriteExt, BufReader},
    process::Command,
    time::timeout,
};

const MAX_RAW_OUTPUT: usize = 64 * 1024;
const CLEAN_PATH: &str = "/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin";
const CLEAN_TERM: &str = "xterm-256color";
const PROBE_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SubscriptionLimitWindow {
    Primary,
    Secondary,
}

impl SubscriptionLimitWindow {
    const fn label(self) -> &'static str {
        match self {
            Self::Primary => "primary",
            Self::Secondary => "secondary",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct NormalizedUsage {
    pub provider: Provider,
    pub used_percent: u8,
    pub limit_window: SubscriptionLimitWindow,
    pub resets_at_ms: Option<i64>,
    pub exhausted: bool,
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum CollectorError {
    #[error("codex executable was not found on PATH")]
    NotFound,
    #[error("collector timed out")]
    Timeout,
    #[error("collector protocol was not recognized")]
    Protocol,
    #[error("collector process failed")]
    Process,
    #[error("collector output exceeded its bound")]
    OutputLimit,
}

impl CollectorError {
    const fn category(self) -> &'static str {
        match self {
            Self::NotFound => "not_found",
            Self::Timeout => "timeout",
            Self::Protocol => "protocol",
            Self::Process => "process",
            Self::OutputLimit => "output_limit",
        }
    }
}

struct CodexCollectorConfig {
    codex_program: PathBuf,
    codex_home: PathBuf,
    home: PathBuf,
    temp_dir: PathBuf,
    timeout: Duration,
}

/// Run the probe against `codex` on `PATH` and print a JSON summary to stdout.
///
/// Output shape (documented here and in README.md, since this no longer goes
/// through the daemon protocol):
/// `{"ok":true,"provider":"codex","usedPercent":42,"limitWindow":"primary","resetsAtMs":1234,"exhausted":false}`
/// or on failure: `{"ok":false,"provider":"codex","category":"timeout"}`.
pub fn run() -> i32 {
    let outcome = match crate::probes::locate_on_path("codex") {
        Some(codex_program) => {
            let config = CodexCollectorConfig {
                codex_program,
                codex_home: env::var_os("CODEX_HOME")
                    .map(PathBuf::from)
                    .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".codex")))
                    .unwrap_or_else(|| PathBuf::from(".codex")),
                home: env::var_os("HOME")
                    .map(PathBuf::from)
                    .unwrap_or_else(|| PathBuf::from("/")),
                temp_dir: env::var_os("TMPDIR")
                    .map(PathBuf::from)
                    .unwrap_or_else(|| PathBuf::from("/tmp")),
                timeout: PROBE_TIMEOUT,
            };
            let runtime = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(runtime) => runtime,
                Err(_) => {
                    print_failure(CollectorError::Process);
                    return 1;
                }
            };
            runtime.block_on(collect_codex(config))
        }
        None => Err(CollectorError::NotFound),
    };
    match outcome {
        Ok(usage) => {
            print_observed(usage);
            0
        }
        Err(error) => {
            print_failure(error);
            1
        }
    }
}

fn print_observed(usage: NormalizedUsage) {
    println!(
        "{}",
        json!({
            "ok": true,
            "provider": "codex",
            "usedPercent": usage.used_percent,
            "limitWindow": usage.limit_window.label(),
            "resetsAtMs": usage.resets_at_ms,
            "exhausted": usage.exhausted,
        })
    );
}

fn print_failure(error: CollectorError) {
    eprintln!(
        "{}",
        json!({ "ok": false, "provider": "codex", "category": error.category() })
    );
}

async fn collect_codex(config: CodexCollectorConfig) -> Result<NormalizedUsage, CollectorError> {
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
async fn collect_codex_protocol<R, W>(
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
                        "name": "factoryctl-usage",
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
        use tokio::io::AsyncBufReadExt;
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
    let mut exhausted = false;
    let historical = result
        .get("rateLimits")
        .and_then(Value::as_object)
        .ok_or(CollectorError::Protocol)?;
    inspect_codex_snapshot(historical, &mut selected, &mut exhausted)?;
    if let Some(buckets) = result.get("rateLimitsByLimitId") {
        if !buckets.is_null() {
            let buckets = buckets.as_object().ok_or(CollectorError::Protocol)?;
            for snapshot in buckets.values() {
                inspect_codex_snapshot(
                    snapshot.as_object().ok_or(CollectorError::Protocol)?,
                    &mut selected,
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
    })
}

fn inspect_codex_snapshot(
    snapshot: &serde_json::Map<String, Value>,
    selected: &mut Option<(u8, SubscriptionLimitWindow, Option<i64>)>,
    exhausted: &mut bool,
) -> Result<(), CollectorError> {
    *exhausted |= snapshot
        .get("rateLimitReachedType")
        .is_some_and(|value| !value.is_null());
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
        if selected
            .as_ref()
            .is_none_or(|(current, _, _)| percent > *current)
        {
            *selected = Some((percent, window, reset));
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

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use factory_core::Provider;
    use tokio::io::{AsyncWriteExt, BufReader, duplex};

    use super::{CollectorError, SubscriptionLimitWindow, collect_codex_protocol};

    #[tokio::test]
    async fn codex_protocol_uses_only_initialize_initialized_and_rate_limit_read() {
        use tokio::io::AsyncBufReadExt;
        let (client, server) = duplex(16 * 1024);
        let (client_read, client_write) = tokio::io::split(client);
        let (server_read, mut server_write) = tokio::io::split(server);
        let server = tokio::spawn(async move {
            let mut lines = BufReader::new(server_read).lines();
            let initialize: serde_json::Value =
                serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
            assert_eq!(initialize["method"], "initialize");
            assert_eq!(
                initialize["params"]["capabilities"]["experimentalApi"],
                true
            );
            server_write
                .write_all(b"{\"method\":\"account/rateLimits/updated\",\"params\":{}}\n{\"id\":1,\"result\":null}\n")
                .await
                .unwrap();

            let initialized: serde_json::Value =
                serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
            assert_eq!(initialized["method"], "initialized");
            assert!(initialized.get("id").is_none());
            let read: serde_json::Value =
                serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
            assert_eq!(read["method"], "account/rateLimits/read");
            assert!(read.get("params").is_some_and(serde_json::Value::is_null));
            let rendered = format!("{initialize}\n{initialized}\n{read}");
            assert!(!rendered.contains("thread/"));
            assert!(!rendered.contains("turn/"));
            assert!(!rendered.contains("consume"));
            server_write
                .write_all(
                    b"{\"method\":\"unrelated/notification\",\"params\":{}}\n{\"id\":2,\"result\":{\"rateLimits\":{\"primary\":{\"usedPercent\":82,\"resetsAt\":123},\"secondary\":{\"usedPercent\":96,\"resetsAt\":456},\"rateLimitReachedType\":null},\"rateLimitsByLimitId\":{\"codex\":{\"primary\":{\"usedPercent\":98,\"resetsAt\":789},\"secondary\":{\"usedPercent\":60,\"resetsAt\":999},\"rateLimitReachedType\":null}}}}\n",
                )
                .await
                .unwrap();
        });

        let usage = collect_codex_protocol(client_read, client_write, Duration::from_secs(1))
            .await
            .unwrap();
        server.await.unwrap();
        assert_eq!(usage.provider, Provider::Codex);
        assert_eq!(usage.used_percent, 98);
        assert_eq!(usage.limit_window, SubscriptionLimitWindow::Primary);
        assert_eq!(usage.resets_at_ms, Some(789_000));
        assert!(!usage.exhausted);
    }

    #[tokio::test]
    async fn codex_protocol_bounds_unknown_notifications_without_leaking_them() {
        let (client, server) = duplex(128 * 1024);
        let (client_read, client_write) = tokio::io::split(client);
        let (_server_read, mut server_write) = tokio::io::split(server);
        let server = tokio::spawn(async move {
            let oversized = format!(
                "{{\"method\":\"private-terminal-sentinel\",\"params\":{{\"text\":\"{}\"}}}}\n",
                "x".repeat(70 * 1024)
            );
            let _ = server_write.write_all(oversized.as_bytes()).await;
        });
        let error = collect_codex_protocol(client_read, client_write, Duration::from_secs(1))
            .await
            .unwrap_err();
        server.await.unwrap();
        assert_eq!(error, CollectorError::OutputLimit);
        assert!(!format!("{error:?} {error}").contains("private-terminal-sentinel"));
    }
}
