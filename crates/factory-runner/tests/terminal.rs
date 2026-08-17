//! Integration tests for the runner's PTY terminal mode: `AttachTerminal`,
//! `TerminalInput`, `ResizeTerminal`, and the retained `terminal.log`.
//!
//! These spawn the real `factory-runner` binary, exactly like `runner.rs`
//! does for pipe mode, and talk to its control socket directly.

use std::{
    fs,
    io::{BufRead, BufReader, Read, Write},
    os::unix::net::UnixStream,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant},
};

use factory_core::{
    RunId, RunnerInstanceId,
    runner::{
        RUNNER_PROTOCOL_VERSION, RequestEnvelope, RunnerErrorCode, RunnerEvent,
        RunnerEventEnvelope, RunnerFrame, RunnerRequest, decode_terminal_bytes,
        encode_terminal_bytes,
    },
};
use rustix::process::{Pid, Signal, kill_process_group};

const RUN_ID: &str = "run-terminal-1";
const INSTANCE_ID: &str = "instance-terminal-1";

struct RunningTerminalRunner {
    child: Option<Child>,
    stderr: Option<std::process::ChildStderr>,
    runtime: PathBuf,
    _directory: tempfile::TempDir,
}

impl RunningTerminalRunner {
    fn spawn(program: &Path, args: &[&str], cols: u16, rows: u16) -> Self {
        let directory = tempfile::tempdir().unwrap();
        let runtime = directory.path().join("run");
        let child = Command::new(env!("CARGO_BIN_EXE_factory-runner"))
            .arg("--run-id")
            .arg(RUN_ID)
            .arg("--runner-instance-id")
            .arg(INSTANCE_ID)
            .arg("--runtime-dir")
            .arg(&runtime)
            .arg("--cwd")
            .arg(directory.path())
            .arg("--terminal-cols")
            .arg(cols.to_string())
            .arg("--terminal-rows")
            .arg(rows.to_string())
            .arg("--")
            .arg(program)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let mut runner = Self {
            stderr: None,
            child: Some(child),
            runtime,
            _directory: directory,
        };
        runner.stderr = runner.child.as_mut().unwrap().stderr.take();
        runner.wait_until_ready();
        runner
    }

    fn socket(&self) -> PathBuf {
        self.runtime.join("control.sock")
    }

    fn spool(&self) -> PathBuf {
        self.runtime.join("events.ndjson")
    }

    fn terminal_log(&self) -> PathBuf {
        self.runtime.join("terminal.log")
    }

    fn wait_until_ready(&mut self) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if UnixStream::connect(self.socket()).is_ok() {
                return;
            }
            if let Some(status) = self.child.as_mut().unwrap().try_wait().unwrap() {
                let mut stderr = String::new();
                if let Some(mut pipe) = self.stderr.take() {
                    let _ = pipe.read_to_string(&mut stderr);
                }
                panic!("runner exited before ready: {status}; stderr: {stderr:?}");
            }
            thread::sleep(Duration::from_millis(10));
        }
        panic!("runner did not create its control socket");
    }

    fn wait_for_terminal_spool(&self) {
        let deadline = Instant::now() + Duration::from_secs(8);
        while Instant::now() < deadline {
            let spool = fs::read_to_string(self.spool()).unwrap_or_default();
            if spool.lines().any(|line| {
                serde_json::from_str::<RunnerEventEnvelope>(line).is_ok_and(|event| {
                    matches!(
                        event.event,
                        RunnerEvent::SpawnFailed { .. } | RunnerEvent::Exited { .. }
                    )
                })
            }) {
                return;
            }
            thread::sleep(Duration::from_millis(10));
        }
        panic!("runner never recorded a terminal lifecycle event");
    }

    fn terminal_sequence(&self) -> i64 {
        let spool = fs::read_to_string(self.spool()).unwrap();
        spool
            .lines()
            .find_map(|line| {
                let event: RunnerEventEnvelope = serde_json::from_str(line).ok()?;
                matches!(
                    event.event,
                    RunnerEvent::SpawnFailed { .. } | RunnerEvent::Exited { .. }
                )
                .then_some(event.sequence)
            })
            .expect("terminal lifecycle event")
    }

    fn wait_for_clean_exit(mut self) {
        self.wait_for_process_exit();
    }

    /// Like `wait_for_clean_exit`, but does not consume `self`/drop the
    /// backing `TempDir`, so retained files can still be inspected
    /// afterward.
    fn wait_for_process_exit(&mut self) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if let Some(status) = self.child.as_mut().unwrap().try_wait().unwrap() {
                assert!(status.success(), "runner exit was {status}");
                self.child.take();
                return;
            }
            thread::sleep(Duration::from_millis(10));
        }
        panic!("runner did not exit after terminal acknowledgement");
    }
}

impl Drop for RunningTerminalRunner {
    fn drop(&mut self) {
        // Safety net: reap the PTY child's process group even if a test
        // fails before stopping it cleanly, mirroring runner.rs.
        let started_pid = fs::read_to_string(self.spool()).ok().and_then(|spool| {
            spool.lines().find_map(|line| {
                let event: RunnerEventEnvelope = serde_json::from_str(line).ok()?;
                match event.event {
                    RunnerEvent::Started { child_pid } => {
                        i32::try_from(child_pid).ok().and_then(Pid::from_raw)
                    }
                    _ => None,
                }
            })
        });
        if let Some(pid) = started_pid {
            let _ = kill_process_group(pid, Signal::KILL);
        }
        if let Some(child) = self.child.as_mut() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn connect(socket: &Path) -> UnixStream {
    let stream = UnixStream::connect(socket).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    stream
        .set_write_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    stream
}

fn write_request(stream: &mut UnixStream, request: RunnerRequest) {
    let envelope = RequestEnvelope::new(
        RunId::try_from(RUN_ID).unwrap(),
        RunnerInstanceId::try_from(INSTANCE_ID).unwrap(),
        request,
    );
    serde_json::to_writer(&mut *stream, &envelope).unwrap();
    stream.write_all(b"\n").unwrap();
    stream.flush().unwrap();
}

fn read_frame(reader: &mut impl BufRead) -> RunnerFrame {
    let mut line = String::new();
    reader.read_line(&mut line).unwrap();
    assert!(!line.is_empty(), "runner closed before sending a frame");
    serde_json::from_str(&line).unwrap()
}

fn request(runner: &RunningTerminalRunner, request: RunnerRequest) -> RunnerFrame {
    let mut stream = connect(&runner.socket());
    write_request(&mut stream, request);
    read_frame(&mut BufReader::new(stream))
}

fn assert_command_ack(frame: RunnerFrame) {
    assert!(
        matches!(
            &frame,
            RunnerFrame::CommandAck {
                protocol_version: RUNNER_PROTOCOL_VERSION,
                ..
            }
        ),
        "expected a command acknowledgement, got {frame:?}"
    );
}

fn wait_for_path(path: &Path) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if path.exists() {
            return;
        }
        thread::sleep(Duration::from_millis(10));
    }
    panic!("path did not appear: {}", path.display());
}

fn wait_for_nonempty_file(path: &Path) -> String {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let content = fs::read_to_string(path).unwrap_or_default();
        if !content.is_empty() {
            return content;
        }
        assert!(
            Instant::now() < deadline,
            "file stayed empty: {}",
            path.display()
        );
        thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn terminal_mode_attaches_replays_and_streams_written_input() {
    let runner =
        RunningTerminalRunner::spawn(Path::new("/bin/sh"), &["-c", "printf hello; cat"], 80, 24);

    // Attach from the start, collect bytes until we see the initial "hello".
    let mut stream = connect(&runner.socket());
    write_request(
        &mut stream,
        RunnerRequest::AttachTerminal { since_offset: 0 },
    );
    let mut reader = BufReader::new(stream);
    let mut collected = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(5);
    while !String::from_utf8_lossy(&collected).contains("hello") {
        assert!(
            Instant::now() < deadline,
            "timed out waiting for initial output"
        );
        match read_frame(&mut reader) {
            RunnerFrame::TerminalOutput { bytes, .. } => {
                collected.extend(decode_terminal_bytes(&bytes).unwrap());
            }
            other => panic!("unexpected frame while waiting for output: {other:?}"),
        }
    }

    // Write input on a separate one-shot connection; the attached stream
    // should see it echoed back (the pty and/or `cat` copy stdin to stdout).
    assert_command_ack(request(
        &runner,
        RunnerRequest::TerminalInput {
            bytes: encode_terminal_bytes(b"marco\n"),
        },
    ));
    let deadline = Instant::now() + Duration::from_secs(5);
    while !String::from_utf8_lossy(&collected).contains("marco") {
        assert!(
            Instant::now() < deadline,
            "timed out waiting for echoed input"
        );
        match read_frame(&mut reader) {
            RunnerFrame::TerminalOutput { bytes, .. } => {
                collected.extend(decode_terminal_bytes(&bytes).unwrap());
            }
            other => panic!("unexpected frame while waiting for echo: {other:?}"),
        }
    }

    assert_command_ack(request(
        &runner,
        RunnerRequest::Stop {
            command_id: "stop-terminal-1".into(),
            grace_ms: 1_000,
        },
    ));
    runner.wait_for_terminal_spool();
    let terminal = runner.terminal_sequence();
    assert_command_ack(request(
        &runner,
        RunnerRequest::AcknowledgeExit {
            command_id: "ack-terminal-1".into(),
            terminal_sequence: terminal,
        },
    ));
    runner.wait_for_clean_exit();
}

#[test]
fn resize_terminal_is_acknowledged_and_delivers_sigwinch() {
    let ready_marker = tempfile::NamedTempFile::new().unwrap();
    let ready = ready_marker.path().to_owned();
    let resized_marker = tempfile::NamedTempFile::new().unwrap();
    let resized = resized_marker.path().to_owned();
    fs::remove_file(&ready).unwrap();
    fs::remove_file(&resized).unwrap();

    let runner = RunningTerminalRunner::spawn(
        Path::new("/bin/sh"),
        &[
            "-c",
            "trap 'stty size > \"$1\"; exit 0' WINCH; printf ready > \"$2\"; while :; do sleep 1; done",
            "sh",
            resized.to_str().unwrap(),
            ready.to_str().unwrap(),
        ],
        80,
        24,
    );
    wait_for_path(&ready);

    assert_command_ack(request(
        &runner,
        RunnerRequest::ResizeTerminal {
            cols: 100,
            rows: 40,
        },
    ));
    // `>` truncates/creates the file before `stty size` finishes writing to
    // it, so wait for content, not mere existence.
    let reported = wait_for_nonempty_file(&resized);
    // `stty size` prints "<rows> <cols>".
    assert_eq!(reported.trim(), "40 100");

    runner.wait_for_terminal_spool();
    let terminal = runner.terminal_sequence();
    assert_command_ack(request(
        &runner,
        RunnerRequest::AcknowledgeExit {
            command_id: "ack-resize-1".into(),
            terminal_sequence: terminal,
        },
    ));
    runner.wait_for_clean_exit();
}

#[test]
fn attach_terminal_and_terminal_input_are_rejected_on_a_non_terminal_run() {
    let directory = tempfile::tempdir().unwrap();
    let runtime = directory.path().join("run");
    let child = Command::new(env!("CARGO_BIN_EXE_factory-runner"))
        .arg("--run-id")
        .arg(RUN_ID)
        .arg("--runner-instance-id")
        .arg(INSTANCE_ID)
        .arg("--runtime-dir")
        .arg(&runtime)
        .arg("--cwd")
        .arg(directory.path())
        .arg("--stdin-bytes")
        .arg("0")
        .arg("--")
        .arg("/bin/sh")
        .arg("-c")
        .arg("while :; do sleep 1; done")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut runner = RunningTerminalRunner {
        stderr: None,
        child: Some(child),
        runtime,
        _directory: directory,
    };
    runner.stderr = runner.child.as_mut().unwrap().stderr.take();
    runner.wait_until_ready();

    let mut stream = connect(&runner.socket());
    write_request(
        &mut stream,
        RunnerRequest::AttachTerminal { since_offset: 0 },
    );
    match read_frame(&mut BufReader::new(stream)) {
        RunnerFrame::Error {
            code: RunnerErrorCode::InvalidRequest,
            ..
        } => {}
        other => panic!("expected an InvalidRequest error, got {other:?}"),
    }
    match request(
        &runner,
        RunnerRequest::TerminalInput {
            bytes: encode_terminal_bytes(b"hi"),
        },
    ) {
        RunnerFrame::Error {
            code: RunnerErrorCode::InvalidRequest,
            ..
        } => {}
        other => panic!("expected an InvalidRequest error, got {other:?}"),
    }

    assert_command_ack(request(
        &runner,
        RunnerRequest::Stop {
            command_id: "stop-non-terminal".into(),
            grace_ms: 500,
        },
    ));
    runner.wait_for_terminal_spool();
    let terminal = runner.terminal_sequence();
    assert_command_ack(request(
        &runner,
        RunnerRequest::AcknowledgeExit {
            command_id: "ack-non-terminal".into(),
            terminal_sequence: terminal,
        },
    ));
    runner.wait_for_clean_exit();
}

#[test]
fn terminal_log_and_socket_survive_acknowledgement_but_socket_is_removed() {
    let mut runner =
        RunningTerminalRunner::spawn(Path::new("/bin/sh"), &["-c", "printf retained"], 80, 24);
    // Let the child exit on its own (no stop needed for a non-looping program).
    runner.wait_for_terminal_spool();
    let terminal = runner.terminal_sequence();
    assert_command_ack(request(
        &runner,
        RunnerRequest::AcknowledgeExit {
            command_id: "ack-retain-1".into(),
            terminal_sequence: terminal,
        },
    ));
    // Non-consuming: keeps the backing TempDir alive so retained files can
    // still be inspected once the runner process itself has fully exited.
    runner.wait_for_process_exit();

    let socket = runner.socket();
    let terminal_log = runner.terminal_log();
    let spool = runner.spool();
    let deadline = Instant::now() + Duration::from_secs(5);
    while socket.exists() {
        assert!(Instant::now() < deadline, "control socket was not removed");
        thread::sleep(Duration::from_millis(10));
    }
    assert!(terminal_log.exists(), "terminal.log must be retained");
    assert!(spool.exists(), "events.ndjson must be retained");
    let retained = fs::read(&terminal_log).unwrap();
    assert!(String::from_utf8_lossy(&retained).contains("retained"));
}

#[test]
fn late_attach_replays_only_from_the_requested_offset() {
    let runner = RunningTerminalRunner::spawn(
        Path::new("/bin/sh"),
        &["-c", "printf 0123456789; cat"],
        80,
        24,
    );

    // Drain the initial 10 bytes on a first attach to learn a safe offset.
    let mut stream = connect(&runner.socket());
    write_request(
        &mut stream,
        RunnerRequest::AttachTerminal { since_offset: 0 },
    );
    let mut reader = BufReader::new(stream);
    let mut collected = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(5);
    while collected.len() < 10 {
        assert!(
            Instant::now() < deadline,
            "timed out waiting for initial output"
        );
        match read_frame(&mut reader) {
            RunnerFrame::TerminalOutput { bytes, .. } => {
                collected.extend(decode_terminal_bytes(&bytes).unwrap());
            }
            other => panic!("unexpected frame: {other:?}"),
        }
    }
    drop(reader);

    // Reattach from offset 5: replay must start at "56789", never "01234".
    let mut stream = connect(&runner.socket());
    write_request(
        &mut stream,
        RunnerRequest::AttachTerminal { since_offset: 5 },
    );
    let mut reader = BufReader::new(stream);
    let frame = read_frame(&mut reader);
    let RunnerFrame::TerminalOutput { offset, bytes, .. } = frame else {
        panic!("expected terminal output, got {frame:?}");
    };
    assert_eq!(offset, 5);
    let replayed = decode_terminal_bytes(&bytes).unwrap();
    assert!(replayed.starts_with(b"56789"));
    assert!(!replayed.starts_with(b"01234"));

    assert_command_ack(request(
        &runner,
        RunnerRequest::Stop {
            command_id: "stop-late-attach".into(),
            grace_ms: 500,
        },
    ));
    runner.wait_for_terminal_spool();
    let terminal = runner.terminal_sequence();
    assert_command_ack(request(
        &runner,
        RunnerRequest::AcknowledgeExit {
            command_id: "ack-late-attach".into(),
            terminal_sequence: terminal,
        },
    ));
    runner.wait_for_clean_exit();
}

#[test]
fn a_slow_attached_subscriber_is_dropped_and_can_reattach_from_its_offset() {
    // A flooding program that writes much more than the runner's broadcast
    // buffer can hold before a slow reader is serviced.
    let runner = RunningTerminalRunner::spawn(
        Path::new("/bin/sh"),
        &["-c", "yes 0123456789abcdef | head -c 4000000"],
        80,
        24,
    );

    let mut stream = connect(&runner.socket());
    write_request(
        &mut stream,
        RunnerRequest::AttachTerminal { since_offset: 0 },
    );
    // Deliberately do not read: let output pile up faster than the bounded
    // per-subscriber buffer until the runner drops this connection.
    thread::sleep(Duration::from_secs(2));
    let mut reader = BufReader::new(stream);
    let mut saw_conflict = false;
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        let mut line = String::new();
        if reader.read_line(&mut line).unwrap() == 0 {
            break;
        }
        if let Ok(RunnerFrame::Error {
            code: RunnerErrorCode::Conflict,
            ..
        }) = serde_json::from_str::<RunnerFrame>(&line)
        {
            saw_conflict = true;
            break;
        }
    }
    assert!(saw_conflict, "expected the slow subscriber to be dropped");

    // Reattach from offset 0 must still work.
    let mut stream = connect(&runner.socket());
    write_request(
        &mut stream,
        RunnerRequest::AttachTerminal { since_offset: 0 },
    );
    let frame = read_frame(&mut BufReader::new(stream));
    assert!(
        matches!(frame, RunnerFrame::TerminalOutput { offset: 0, .. }),
        "expected a fresh reattach to replay from offset 0, got {frame:?}"
    );

    let _ = request(
        &runner,
        RunnerRequest::Stop {
            command_id: "stop-slow-subscriber".into(),
            grace_ms: 500,
        },
    );
    runner.wait_for_terminal_spool();
    let terminal = runner.terminal_sequence();
    let _ = request(
        &runner,
        RunnerRequest::AcknowledgeExit {
            command_id: "ack-slow-subscriber".into(),
            terminal_sequence: terminal,
        },
    );
    runner.wait_for_clean_exit();
}

/// Terminal-mode equivalent of `runner.rs`'s
/// `natural_leader_exit_terminates_a_descendant_that_retains_output_pipes`:
/// the leader exits entirely on its own (no `Stop` is ever sent), leaving a
/// well-behaved backgrounded descendant that the group cleanup has to reap.
/// `supervise_piped` and `supervise_terminal` share that cleanup
/// (`reap_group_stragglers` in `lib.rs`) but reach it through separate,
/// independently maintained spawn/wait/PTY plumbing, so a regression
/// specific to the PTY path (or to how `supervise_terminal` calls the
/// shared helper) would still slip through without PTY-mode coverage of its
/// own. `sleep`, with no trap, dies from the cleanup's first TERM almost
/// immediately, so this should complete in well under a second, not the
/// ~2s a blind wait would take.
#[test]
fn natural_leader_exit_reaps_a_well_behaved_descendant_without_the_old_multi_second_wait() {
    let directory = tempfile::tempdir().unwrap();
    let marker = directory.path().join("background.pid");
    let runner = RunningTerminalRunner::spawn(
        Path::new("/bin/sh"),
        &[
            "-c",
            "sleep 30 & echo $! > \"$1\"",
            "sh",
            marker.to_str().unwrap(),
        ],
        80,
        24,
    );
    wait_for_path(&marker);
    let started = Instant::now();
    runner.wait_for_terminal_spool();
    let elapsed = started.elapsed();
    assert!(
        elapsed < Duration::from_secs(1),
        "reaping a well-behaved descendant after the leader's natural exit took {elapsed:?}; \
         group cleanup should poll and notice the group is empty almost immediately, not wait \
         out a multi-second grace it doesn't need"
    );
    let terminal = runner.terminal_sequence();
    assert_command_ack(request(
        &runner,
        RunnerRequest::AcknowledgeExit {
            command_id: "ack-natural-exit-descendant".into(),
            terminal_sequence: terminal,
        },
    ));
    runner.wait_for_clean_exit();
}

fn spool_events(runner: &RunningTerminalRunner) -> Vec<RunnerEvent> {
    fs::read_to_string(runner.spool())
        .unwrap_or_default()
        .lines()
        .filter_map(|line| serde_json::from_str::<RunnerEventEnvelope>(line).ok())
        .map(|envelope| envelope.event)
        .collect()
}

fn wait_for_terminal_raw(runner: &RunningTerminalRunner) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if spool_events(runner)
            .iter()
            .any(|event| matches!(event, RunnerEvent::TerminalRaw))
        {
            return;
        }
        thread::sleep(Duration::from_millis(10));
    }
    panic!("runner never recorded RunnerEvent::TerminalRaw");
}

/// The daemon-side synthesis this event exists for (`factoryd::execution::
/// synthesize_codex_session_start`) needs `RunnerEvent::TerminalRaw` to
/// actually show up when a child does what every real interactive CLI
/// does once during its own startup: take its pty out of canonical mode.
/// `stty raw -echo` does exactly that to its own controlling terminal, no
/// fake-agent flag needed.
#[test]
fn terminal_raw_mode_is_recorded_once_a_child_leaves_canonical_mode() {
    let runner = RunningTerminalRunner::spawn(
        Path::new("/bin/sh"),
        &["-c", "stty raw -echo; sleep 5"],
        80,
        24,
    );
    wait_for_terminal_raw(&runner);
    assert_eq!(
        spool_events(&runner)
            .iter()
            .filter(|event| matches!(event, RunnerEvent::TerminalRaw))
            .count(),
        1,
        "TerminalRaw must be logged exactly once, like Started -- the poll \
         thread stops the instant it detects raw mode"
    );

    assert_command_ack(request(
        &runner,
        RunnerRequest::Stop {
            command_id: "stop-raw-mode".into(),
            grace_ms: 500,
        },
    ));
    runner.wait_for_terminal_spool();
    let terminal = runner.terminal_sequence();
    assert_command_ack(request(
        &runner,
        RunnerRequest::AcknowledgeExit {
            command_id: "ack-raw-mode".into(),
            terminal_sequence: terminal,
        },
    ));
    runner.wait_for_clean_exit();
}

/// The other half of the same contract: a child that never leaves
/// canonical mode (an ordinary, non-interactive `/bin/sh` one-liner that
/// just exits) must never get a synthesized-readiness signal it never
/// earned -- `RunnerEvent::TerminalRaw` never appears in its spool at all,
/// not even after it has already exited.
#[test]
fn terminal_raw_mode_is_never_recorded_for_a_child_that_stays_canonical() {
    let mut runner = RunningTerminalRunner::spawn(Path::new("/bin/sh"), &["-c", "sleep 1"], 80, 24);
    // Let the child exit on its own; it never calls `stty raw`.
    runner.wait_for_terminal_spool();
    assert!(
        !spool_events(&runner)
            .iter()
            .any(|event| matches!(event, RunnerEvent::TerminalRaw)),
        "a child that never leaves canonical mode must never get TerminalRaw"
    );
    let terminal = runner.terminal_sequence();
    assert_command_ack(request(
        &runner,
        RunnerRequest::AcknowledgeExit {
            command_id: "ack-stays-canonical".into(),
            terminal_sequence: terminal,
        },
    ));
    runner.wait_for_process_exit();
}
