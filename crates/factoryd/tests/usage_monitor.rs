use std::{fs, os::unix::fs::PermissionsExt, time::Duration};

use factory_core::Provider;
use factoryd::usage_monitor::{
    ClaudeCollectorConfig, CollectorError, collect_claude, collect_codex_protocol,
    parse_claude_usage,
};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, duplex};

fn fake_executable(directory: &tempfile::TempDir, name: &str) -> std::path::PathBuf {
    let path = directory.path().join(name);
    fs::write(&path, "#!/bin/sh\nexit 0\n").unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
    path
}

#[tokio::test]
async fn codex_protocol_uses_only_initialize_initialized_and_rate_limit_read() {
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
    assert_eq!(
        usage.limit_window,
        factoryd::store::SubscriptionLimitWindow::Primary
    );
    assert_eq!(usage.resets_at_ms, Some(789_000));
    assert!(!usage.exhausted);
    assert_eq!(usage.windows.len(), 2);
    assert_eq!(usage.windows[0].used_percent, 98);
    assert_eq!(usage.windows[1].used_percent, 96);
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

#[test]
fn claude_screen_reader_parser_normalizes_the_highest_limit() {
    let raw = "Claude Code usage\r\nCurrent session\r\n42% used\r\nResets in 2h\r\nCurrent week (all models)\r\n87% used\r\nResets Aug 18\r\n";
    let usage = parse_claude_usage(raw.as_bytes()).unwrap();
    assert_eq!(usage.provider, Provider::ClaudeCode);
    assert_eq!(usage.used_percent, 87);
    assert!(!usage.exhausted);
    assert_eq!(usage.windows.len(), 2);
    assert_eq!(usage.windows[0].used_percent, 42);
    assert_eq!(usage.windows[1].used_percent, 87);

    let exhausted = parse_claude_usage(
        b"Current session\n100% used\nResets in 2h\nCurrent week (all models)\n20% used\nResets Aug 18\nlimit reached\n",
    )
    .unwrap();
    assert!(exhausted.exhausted);
}

#[tokio::test]
async fn claude_collector_pipes_fixed_usage_command_through_bounded_fake_pty() {
    let directory = tempfile::tempdir_in("/tmp").unwrap();
    let fake_pty = directory.path().join("fake-pty.sh");
    let arguments = directory.path().join("arguments");
    let input = directory.path().join("input");
    fs::write(
        &fake_pty,
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$@\" > '{}'\nprintf 'plan mode on\\n'\ntyped=$(dd bs=1 count=6 2>/dev/null)\nprintf '%s' \"$typed\" > '{}'\nprintf 'Show session cost, plan usage, and activity\\n'\ndd bs=1 count=1 >/dev/null 2>&1\nprintf 'Current session\\n81%% used\\nResets in 2h\\nCurrent week (all models)\\n41%% used\\nResets Aug 18\\n'\n",
            arguments.display(),
            input.display(),
        ),
    )
    .unwrap();
    fs::set_permissions(&fake_pty, fs::Permissions::from_mode(0o700)).unwrap();
    let fake_claude = fake_executable(&directory, "fake-claude.sh");

    let usage = collect_claude(ClaudeCollectorConfig {
        pty_program: fake_pty,
        claude_program: fake_claude.clone(),
        home: directory.path().into(),
        temp_dir: directory.path().into(),
        timeout: Duration::from_secs(1),
    })
    .await
    .unwrap();
    assert_eq!(usage.used_percent, 81);

    let arguments = fs::read_to_string(arguments).unwrap();
    let canonical_claude = fs::canonicalize(&fake_claude).unwrap();
    for required in [
        "-q",
        "/dev/null",
        canonical_claude.to_str().unwrap(),
        "--safe-mode",
        "--ax-screen-reader",
        "--permission-mode",
        "plan",
        "--tools",
        "--no-chrome",
    ] {
        assert!(arguments.lines().any(|line| line == required), "{required}");
    }
    assert_eq!(fs::read_to_string(input).unwrap().trim_end(), "/usage");
}

#[tokio::test]
async fn missing_claude_autocomplete_aborts_without_sending_enter() {
    let directory = tempfile::tempdir_in("/tmp").unwrap();
    let fake_pty = directory.path().join("missing-autocomplete.sh");
    let input = directory.path().join("input");
    fs::write(
        &fake_pty,
        format!(
            "#!/bin/sh\nprintf 'plan mode on\\n'\ntyped=$(dd bs=1 count=6 2>/dev/null)\nprintf '%s' \"$typed\" > '{}'\n",
            input.display(),
        ),
    )
    .unwrap();
    fs::set_permissions(&fake_pty, fs::Permissions::from_mode(0o700)).unwrap();
    let fake_claude = fake_executable(&directory, "fake-claude.sh");

    let error = collect_claude(ClaudeCollectorConfig {
        pty_program: fake_pty,
        claude_program: fake_claude,
        home: directory.path().into(),
        temp_dir: directory.path().into(),
        timeout: Duration::from_secs(1),
    })
    .await
    .unwrap_err();
    assert_eq!(error, CollectorError::Protocol);
    assert_eq!(fs::read(input).unwrap(), b"/usage");
}

#[tokio::test]
async fn claude_collector_timeout_and_parse_errors_never_echo_terminal_output() {
    let directory = tempfile::tempdir_in("/tmp").unwrap();
    let hanging = directory.path().join("hanging-pty.sh");
    let pid_file = directory.path().join("pid");
    fs::write(
        &hanging,
        format!(
            "#!/bin/sh\nprintf '%s' \"$$\" > '{}'\nread ignored\nsleep 2\n",
            pid_file.display()
        ),
    )
    .unwrap();
    fs::set_permissions(&hanging, fs::Permissions::from_mode(0o700)).unwrap();
    let private_program = fake_executable(&directory, "private-terminal-sentinel");
    let error = collect_claude(ClaudeCollectorConfig {
        pty_program: hanging,
        claude_program: private_program,
        home: directory.path().into(),
        temp_dir: directory.path().into(),
        timeout: Duration::from_millis(50),
    })
    .await
    .unwrap_err();
    assert_eq!(error, CollectorError::Timeout);
    let rendered = format!("{error:?} {error}");
    assert!(!rendered.contains("private-terminal-sentinel"));
    let pid = fs::read_to_string(pid_file).unwrap();
    assert!(
        !std::process::Command::new("kill")
            .args(["-0", pid.as_str()])
            .status()
            .unwrap()
            .success()
    );

    let raw = b"private-terminal-sentinel without a percentage";
    let parse_error = parse_claude_usage(raw).unwrap_err();
    assert!(!format!("{parse_error:?} {parse_error}").contains("private-terminal-sentinel"));
}
