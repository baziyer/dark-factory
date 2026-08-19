use std::{
    fs,
    io::{BufRead, BufReader, Read, Write},
    os::unix::{fs::PermissionsExt, net::UnixStream},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant},
};

use factory_core::{
    RunId, RunnerInstanceId,
    runner::{
        MAX_RUNNER_FRAME_BYTES, MAX_RUNNER_OUTPUT_TEXT_BYTES, MAX_RUNNER_SPOOL_BYTES, OutputStream,
        RUNNER_PROTOCOL_VERSION, RequestEnvelope, RunnerErrorCode, RunnerEvent,
        RunnerEventEnvelope, RunnerFrame, RunnerRequest,
    },
};
use rustix::process::{Pid, Signal, kill_process, kill_process_group};

const RUN_ID: &str = "run-1";
const INSTANCE_ID: &str = "instance-1";
// Repeated cargo-test runs under host contention measured the 1 MiB pipe
// producer occasionally taking just over the old 2s ceiling. This is only a
// harness deadline; writes still complete as soon as the child drains stdin.
const STARTUP_INPUT_TIMEOUT: Duration = Duration::from_secs(8);
// The cleanup fixture sleeps for 200ms in its TERM trap. A 1s runner grace was
// observed expiring before that trap completed on a loaded host, so keep ample
// test-only scheduling margin without changing the runner's production policy.
const TERM_CLEANUP_GRACE_MS: u64 = 5_000;

struct RunningRunner {
    child: Option<Child>,
    stderr: Option<std::process::ChildStderr>,
    runtime: PathBuf,
    _directory: tempfile::TempDir,
}

impl RunningRunner {
    fn spawn(agent_args: &[&str]) -> Self {
        Self::spawn_program(Path::new(env!("CARGO_BIN_EXE_fake-agent")), agent_args)
    }

    fn spawn_with_startup_input(agent_args: &[&str], input: &[u8]) -> Self {
        Self::spawn_program_with_startup_input(
            Path::new(env!("CARGO_BIN_EXE_fake-agent")),
            agent_args,
            input,
        )
    }

    fn spawn_program(program: &Path, agent_args: &[&str]) -> Self {
        let directory = tempfile::tempdir().unwrap();
        let runtime = directory.path().join("run");
        let mut child = runner_command(&runtime, directory.path(), program, agent_args, None)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let stderr = child.stderr.take();
        let mut runner = Self {
            child: Some(child),
            stderr,
            runtime,
            _directory: directory,
        };
        runner.wait_until_ready();
        runner
    }

    fn spawn_program_with_startup_input(program: &Path, agent_args: &[&str], input: &[u8]) -> Self {
        let directory = tempfile::tempdir().unwrap();
        let runtime = directory.path().join("run");
        let mut child = runner_command(
            &runtime,
            directory.path(),
            program,
            agent_args,
            Some(input.len()),
        )
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
        let stderr = child.stderr.take();
        let mut stdin = child.stdin.take().unwrap();
        let input = input.to_vec();
        let (sent, received) = std::sync::mpsc::channel();
        thread::spawn(move || {
            let result = stdin.write_all(&input);
            drop(stdin);
            let _ = sent.send(result);
        });
        let mut runner = Self {
            child: Some(child),
            stderr,
            runtime,
            _directory: directory,
        };
        received
            .recv_timeout(STARTUP_INPUT_TIMEOUT)
            .expect("startup input producer remained blocked")
            .unwrap();
        runner.wait_until_ready();
        runner
    }

    fn socket(&self) -> PathBuf {
        self.runtime.join("control.sock")
    }

    fn spool(&self) -> PathBuf {
        self.runtime.join("events.ndjson")
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
        panic!("runner did not persist a terminal event");
    }

    fn assert_still_running(&mut self) {
        assert!(self.child.as_mut().unwrap().try_wait().unwrap().is_none());
    }

    fn runner_pid(&self) -> u32 {
        self.child.as_ref().unwrap().id()
    }

    fn wait_for_clean_exit(mut self) {
        self.wait_for_successful_exit();
    }

    fn wait_for_successful_exit(&mut self) {
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

fn runner_command(
    runtime: &Path,
    cwd: &Path,
    program: &Path,
    agent_args: &[&str],
    stdin_bytes: Option<usize>,
) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_factory-runner"));
    command
        .arg("--run-id")
        .arg(RUN_ID)
        .arg("--runner-instance-id")
        .arg(INSTANCE_ID)
        .arg("--runtime-dir")
        .arg(runtime)
        .arg("--cwd")
        .arg(cwd);
    if let Some(stdin_bytes) = stdin_bytes {
        command.arg("--stdin-bytes").arg(stdin_bytes.to_string());
    }
    command.arg("--").arg(program).args(agent_args);
    command
}

impl Drop for RunningRunner {
    fn drop(&mut self) {
        // Teardown is scoped to this fixture's private runtime; never turn a
        // test failure into a process-name or host-wide cleanup sweep.
        assert!(self.runtime.starts_with(self._directory.path()));
        let started_pid = fs::read_to_string(self.spool()).ok().and_then(|spool| {
            let mut started_pid = None;
            for line in spool.lines() {
                let Ok(event) = serde_json::from_str::<RunnerEventEnvelope>(line) else {
                    continue;
                };
                match event.event {
                    RunnerEvent::Started { child_pid } => {
                        started_pid = i32::try_from(child_pid).ok().and_then(Pid::from_raw);
                    }
                    RunnerEvent::SpawnFailed { .. } | RunnerEvent::Exited { .. } => return None,
                    _ => {}
                }
            }
            started_pid
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

fn request(runner: &RunningRunner, instance: &str, request: RunnerRequest) -> RunnerFrame {
    let mut stream = connect(&runner.socket());
    write_request(&mut stream, instance, request);
    read_frame(&mut BufReader::new(stream))
}

fn write_request(stream: &mut UnixStream, instance: &str, request: RunnerRequest) {
    write_envelope(
        stream,
        &RequestEnvelope::new(
            RunId::try_from(RUN_ID).unwrap(),
            RunnerInstanceId::try_from(instance).unwrap(),
            request,
        ),
    );
}

fn write_envelope(stream: &mut UnixStream, envelope: &RequestEnvelope) {
    serde_json::to_writer(&mut *stream, envelope).unwrap();
    stream.write_all(b"\n").unwrap();
    stream.flush().unwrap();
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

fn read_frame(reader: &mut impl BufRead) -> RunnerFrame {
    let mut line = String::new();
    reader.read_line(&mut line).unwrap();
    assert!(!line.is_empty(), "runner closed before sending a frame");
    serde_json::from_str(&line).unwrap()
}

fn subscribe_through_terminal(runner: &RunningRunner, after_sequence: i64) -> Vec<RunnerFrame> {
    let mut stream = connect(&runner.socket());
    write_request(
        &mut stream,
        INSTANCE_ID,
        RunnerRequest::Subscribe { after_sequence },
    );
    let mut reader = BufReader::new(stream);
    let mut frames = Vec::new();
    let mut terminal_in_head = None;
    loop {
        let frame = read_frame(&mut reader);
        match &frame {
            RunnerFrame::Hello {
                terminal_sequence: terminal,
                ..
            } => terminal_in_head = *terminal,
            RunnerFrame::Event { event, .. }
                if matches!(
                    event.event,
                    RunnerEvent::SpawnFailed { .. } | RunnerEvent::Exited { .. }
                ) =>
            {
                frames.push(frame);
                if terminal_in_head.is_none() {
                    break;
                }
                continue;
            }
            RunnerFrame::CaughtUp { sequence, .. }
                if terminal_in_head.is_some_and(|terminal| *sequence >= terminal) =>
            {
                frames.push(frame);
                break;
            }
            _ => {}
        }
        frames.push(frame);
    }
    frames
}

fn terminal_sequence(frames: &[RunnerFrame]) -> i64 {
    frames
        .iter()
        .find_map(|frame| match frame {
            RunnerFrame::Hello {
                terminal_sequence: Some(sequence),
                ..
            } => Some(*sequence),
            RunnerFrame::Event { event, .. }
                if matches!(
                    event.event,
                    RunnerEvent::SpawnFailed { .. } | RunnerEvent::Exited { .. }
                ) =>
            {
                Some(event.sequence)
            }
            _ => None,
        })
        .expect("terminal sequence")
}

fn event_frames(frames: &[RunnerFrame]) -> Vec<(i64, &RunnerEvent)> {
    frames
        .iter()
        .filter_map(|frame| match frame {
            RunnerFrame::Event { event, .. } => Some((event.sequence, &event.event)),
            _ => None,
        })
        .collect()
}

fn acknowledge(runner: &RunningRunner, sequence: i64, command_id: &str) -> RunnerFrame {
    request(
        runner,
        INSTANCE_ID,
        RunnerRequest::AcknowledgeExit {
            command_id: command_id.into(),
            terminal_sequence: sequence,
        },
    )
}

fn assert_command_ack(frame: RunnerFrame, command_id: &str) {
    assert_eq!(
        frame,
        RunnerFrame::CommandAck {
            protocol_version: RUNNER_PROTOCOL_VERSION,
            command_id: command_id.into(),
        }
    );
}

fn stdout_text(frames: &[RunnerFrame]) -> String {
    event_frames(frames)
        .into_iter()
        .filter_map(|(_, event)| match event {
            RunnerEvent::Output {
                stream: OutputStream::Stdout,
                text,
                ..
            } => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

fn assert_startup_input_rejected(declared_bytes: usize, input: &[u8]) {
    let directory = tempfile::tempdir().unwrap();
    let runtime = directory.path().join("run");
    let marker = directory.path().join("spawned");
    let mut child = runner_command(
        &runtime,
        directory.path(),
        Path::new(env!("CARGO_BIN_EXE_fake-agent")),
        &["--touch-marker", marker.to_str().unwrap()],
        Some(declared_bytes),
    )
    .stdin(Stdio::piped())
    .stdout(Stdio::null())
    .stderr(Stdio::null())
    .spawn()
    .unwrap();
    let _ = child.stdin.take().unwrap().write_all(input);

    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        if marker.exists() || runtime.exists() {
            let _ = child.kill();
            let _ = child.wait();
            panic!("invalid startup input reached runtime or child spawn");
        }
        if let Some(status) = child.try_wait().unwrap() {
            assert!(!status.success());
            assert!(!marker.exists());
            assert!(!runtime.exists());
            return;
        }
        thread::sleep(Duration::from_millis(10));
    }
    let _ = child.kill();
    let _ = child.wait();
    panic!("runner did not reject invalid startup input");
}

#[test]
fn wrapped_agent_stdin_is_closed_by_default() {
    let runner = RunningRunner::spawn(&["--stdin-to-stdout"]);
    runner.wait_for_terminal_spool();

    let frames = subscribe_through_terminal(&runner, 0);
    assert_eq!(stdout_text(&frames), "");
    assert!(matches!(
        event_frames(&frames).last().unwrap().1,
        RunnerEvent::Exited {
            exit_code: Some(0),
            signal: None,
        }
    ));

    let terminal = terminal_sequence(&frames);
    assert_command_ack(
        acknowledge(&runner, terminal, "default-stdin-closed"),
        "default-stdin-closed",
    );
    runner.wait_for_clean_exit();
}

#[test]
fn framed_startup_input_is_exact_and_preserves_replay_and_ack() {
    let input = b"first line\n{\"kind\":\"turn\",\"message\":\"hello factory\"}\nlast line\n";
    let runner = RunningRunner::spawn_with_startup_input(&["--stdin-to-stdout"], input);
    runner.wait_for_terminal_spool();

    let frames = subscribe_through_terminal(&runner, 0);
    assert_eq!(stdout_text(&frames).as_bytes(), input);
    let events = event_frames(&frames);
    assert!(matches!(
        events.first().unwrap().1,
        RunnerEvent::Started { .. }
    ));
    assert!(matches!(
        events.last().unwrap().1,
        RunnerEvent::Exited {
            exit_code: Some(0),
            signal: None,
        }
    ));

    let terminal = terminal_sequence(&frames);
    assert_command_ack(
        acknowledge(&runner, terminal, "inherited-stdin-persisted"),
        "inherited-stdin-persisted",
    );
    runner.wait_for_clean_exit();
}

#[test]
fn startup_input_producer_finishes_when_the_child_exits_without_reading() {
    let input = vec![b'x'; 64 * 1024];
    let runner = RunningRunner::spawn_with_startup_input(&[], &input);
    runner.wait_for_terminal_spool();

    let frames = subscribe_through_terminal(&runner, 0);
    let events = event_frames(&frames);
    assert_eq!(events.len(), 2, "unexpected lifecycle: {events:?}");
    assert!(matches!(events[0].1, RunnerEvent::Started { .. }));
    assert!(matches!(
        events[1].1,
        RunnerEvent::Exited {
            exit_code: Some(0),
            signal: None,
        }
    ));
    let terminal = terminal_sequence(&frames);
    assert_command_ack(
        acknowledge(&runner, terminal, "early-exit-input-terminal"),
        "early-exit-input-terminal",
    );
    runner.wait_for_clean_exit();
}

#[test]
fn stop_with_unread_startup_input_allows_term_cleanup_and_records_exit() {
    let directory = tempfile::tempdir().unwrap();
    let ready = directory.path().join("ready");
    let cleanup = directory.path().join("cleanup");
    let input = vec![b'x'; 1024 * 1024];
    let runner = RunningRunner::spawn_program_with_startup_input(
        Path::new("/bin/sh"),
        &[
            "-c",
            "trap 'exec 0<&-; sleep 0.2; echo cleaned > \"$1\"; exit 0' TERM; echo ready > \"$2\"; while :; do sleep 1; done",
            "sh",
            cleanup.to_str().unwrap(),
            ready.to_str().unwrap(),
        ],
        &input,
    );
    wait_for_path(&ready);

    assert_command_ack(
        request(
            &runner,
            INSTANCE_ID,
            RunnerRequest::Stop {
                command_id: "stop-pending-input".into(),
                grace_ms: TERM_CLEANUP_GRACE_MS,
            },
        ),
        "stop-pending-input",
    );
    runner.wait_for_terminal_spool();
    wait_for_path(&cleanup);

    let frames = subscribe_through_terminal(&runner, 0);
    assert!(matches!(
        event_frames(&frames).last().unwrap().1,
        RunnerEvent::Exited {
            exit_code: Some(0),
            signal: None,
        }
    ));
    let terminal = terminal_sequence(&frames);
    assert_command_ack(
        acknowledge(&runner, terminal, "stopped-input-persisted"),
        "stopped-input-persisted",
    );
    runner.wait_for_clean_exit();
}

#[test]
fn invalid_startup_input_is_rejected_before_runtime_or_child_spawn() {
    assert_startup_input_rejected(4, b"abc");
    assert_startup_input_rejected(3, b"four");
    assert_startup_input_rejected(1024 * 1024 + 1, b"");
}

#[test]
fn invalid_cli_is_rejected_before_reading_declared_stdin() {
    let directory = tempfile::tempdir().unwrap();
    let runtime = directory.path().join("run");
    let mut child = Command::new(env!("CARGO_BIN_EXE_factory-runner"))
        .arg("--run-id")
        .arg(RUN_ID)
        .arg("--runner-instance-id")
        .arg(INSTANCE_ID)
        .arg("--runtime-dir")
        .arg(&runtime)
        .arg("--cwd")
        .arg(directory.path())
        .arg("--stdin-bytes")
        .arg("1")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let _open_stdin = child.stdin.take().unwrap();

    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        if let Some(status) = child.try_wait().unwrap() {
            assert!(!status.success());
            assert!(!runtime.exists());
            return;
        }
        thread::sleep(Duration::from_millis(10));
    }
    let _ = child.kill();
    let _ = child.wait();
    panic!("runner read framed stdin before rejecting its missing program");
}

#[test]
fn output_before_connect_replays_contiguously_and_requires_exact_terminal_ack() {
    let mut runner = RunningRunner::spawn(&[
        "--stdout",
        "compiled\n",
        "--stderr",
        "warning\n",
        "--exit",
        "7",
    ]);
    runner.wait_for_terminal_spool();
    runner.assert_still_running();

    assert!(matches!(
        request(
            &runner,
            "wrong-instance",
            RunnerRequest::Subscribe { after_sequence: 0 }
        ),
        RunnerFrame::Error {
            code: RunnerErrorCode::Unauthorized,
            ..
        }
    ));

    let frames = subscribe_through_terminal(&runner, 0);
    let events = event_frames(&frames);
    assert_eq!(
        events
            .iter()
            .map(|(sequence, _)| *sequence)
            .collect::<Vec<_>>(),
        (1..=i64::try_from(events.len()).unwrap()).collect::<Vec<_>>()
    );
    assert!(matches!(events[0].1, RunnerEvent::Started { .. }));
    assert!(events.iter().any(|(_, event)| matches!(
        event,
        RunnerEvent::Output { stream: OutputStream::Stdout, text, lossy: false }
            if text.contains("compiled")
    )));
    assert!(events.iter().any(|(_, event)| matches!(
        event,
        RunnerEvent::Output { stream: OutputStream::Stderr, text, lossy: false }
            if text.contains("warning")
    )));
    assert!(matches!(
        events.last().unwrap().1,
        RunnerEvent::Exited {
            exit_code: Some(7),
            signal: None,
        }
    ));
    let terminal = terminal_sequence(&frames);

    let replay = subscribe_through_terminal(&runner, 2);
    assert!(
        event_frames(&replay)
            .iter()
            .all(|(sequence, _)| *sequence > 2)
    );
    assert!(matches!(
        acknowledge(&runner, terminal - 1, "wrong-boundary"),
        RunnerFrame::Error {
            code: RunnerErrorCode::Conflict,
            ..
        }
    ));
    runner.assert_still_running();
    assert_command_ack(
        acknowledge(&runner, terminal, "terminal-persisted"),
        "terminal-persisted",
    );
    runner.wait_for_clean_exit();
}

#[test]
fn control_plane_rejects_wrong_identity_protocol_cursors_and_oversize_requests() {
    let runner = RunningRunner::spawn(&[]);
    runner.wait_for_terminal_spool();
    let frames = subscribe_through_terminal(&runner, 0);
    let terminal = terminal_sequence(&frames);

    let mut wrong_run = RequestEnvelope::new(
        RunId::try_from("another-run").unwrap(),
        RunnerInstanceId::try_from(INSTANCE_ID).unwrap(),
        RunnerRequest::Subscribe { after_sequence: 0 },
    );
    let mut stream = connect(&runner.socket());
    write_envelope(&mut stream, &wrong_run);
    assert!(matches!(
        read_frame(&mut BufReader::new(stream)),
        RunnerFrame::Error {
            code: RunnerErrorCode::Unauthorized,
            ..
        }
    ));

    wrong_run.run_id = RunId::try_from(RUN_ID).unwrap();
    wrong_run.protocol_version = RUNNER_PROTOCOL_VERSION + 1;
    let mut stream = connect(&runner.socket());
    write_envelope(&mut stream, &wrong_run);
    assert!(matches!(
        read_frame(&mut BufReader::new(stream)),
        RunnerFrame::Error {
            code: RunnerErrorCode::UnsupportedProtocol,
            ..
        }
    ));

    assert!(matches!(
        request(
            &runner,
            INSTANCE_ID,
            RunnerRequest::Subscribe { after_sequence: -1 }
        ),
        RunnerFrame::Error {
            code: RunnerErrorCode::InvalidRequest,
            ..
        }
    ));
    assert!(matches!(
        request(
            &runner,
            INSTANCE_ID,
            RunnerRequest::Subscribe {
                after_sequence: terminal + 1,
            }
        ),
        RunnerFrame::Error {
            code: RunnerErrorCode::Conflict,
            ..
        }
    ));

    let mut stream = connect(&runner.socket());
    stream
        .write_all(&vec![b'x'; MAX_RUNNER_FRAME_BYTES + 1])
        .unwrap();
    stream.write_all(b"\n").unwrap();
    stream.flush().unwrap();
    assert!(matches!(
        read_frame(&mut BufReader::new(stream)),
        RunnerFrame::Error {
            code: RunnerErrorCode::InvalidRequest,
            ..
        }
    ));

    assert_command_ack(
        acknowledge(&runner, terminal, "validation-complete"),
        "validation-complete",
    );
    runner.wait_for_clean_exit();
}

#[test]
fn spawn_failure_is_a_replayable_terminal_event() {
    let mut runner =
        RunningRunner::spawn_program(Path::new("/definitely-not-present/dark-factory-agent"), &[]);
    runner.wait_for_terminal_spool();
    runner.assert_still_running();
    let frames = subscribe_through_terminal(&runner, 0);
    let events = event_frames(&frames);
    assert_eq!(events.len(), 1);
    assert!(matches!(events[0].1, RunnerEvent::SpawnFailed { .. }));
    let terminal = terminal_sequence(&frames);
    assert_command_ack(
        acknowledge(&runner, terminal, "spawn-failure-persisted"),
        "spawn-failure-persisted",
    );
    runner.wait_for_clean_exit();
}

#[test]
fn replay_boundary_delivers_later_output_and_exit_without_polling() {
    let release_directory = tempfile::tempdir().unwrap();
    let release = release_directory.path().join("release");
    let runner = RunningRunner::spawn(&[
        "--stdout",
        "before\n",
        "--wait-before-stderr-file",
        release.to_str().unwrap(),
        "--stderr",
        "after\n",
    ]);
    wait_for_spool_text(&runner.spool(), "before");
    let mut stream = connect(&runner.socket());
    write_request(
        &mut stream,
        INSTANCE_ID,
        RunnerRequest::Subscribe { after_sequence: 0 },
    );
    let mut reader = BufReader::new(stream);
    let mut frames = Vec::new();
    loop {
        let frame = read_frame(&mut reader);
        let caught_up = matches!(frame, RunnerFrame::CaughtUp { .. });
        frames.push(frame);
        if caught_up {
            break;
        }
    }
    let (replay_through, terminal_at_hello) = match &frames[0] {
        RunnerFrame::Hello {
            protocol_version,
            run_id,
            runner_instance_id,
            runner_pid,
            replay_through,
            terminal_sequence,
        } => {
            assert_eq!(*protocol_version, RUNNER_PROTOCOL_VERSION);
            assert_eq!(run_id.as_str(), RUN_ID);
            assert_eq!(runner_instance_id.as_str(), INSTANCE_ID);
            assert_eq!(*runner_pid, runner.runner_pid());
            (*replay_through, *terminal_sequence)
        }
        frame => panic!("first subscription frame was not hello: {frame:?}"),
    };
    assert_eq!(terminal_at_hello, None);
    let caught_up_sequence = match frames.last().unwrap() {
        RunnerFrame::CaughtUp { sequence, .. } => *sequence,
        frame => panic!("replay did not end with caught_up: {frame:?}"),
    };
    assert_eq!(caught_up_sequence, replay_through);
    assert!(
        event_frames(&frames)
            .iter()
            .all(|(sequence, _)| *sequence <= replay_through)
    );
    assert!(event_frames(&frames).iter().any(|(_, event)| matches!(
        event,
        RunnerEvent::Output { text, .. } if text.contains("before")
    )));

    let mut caught_up_stream = connect(&runner.socket());
    write_request(
        &mut caught_up_stream,
        INSTANCE_ID,
        RunnerRequest::Subscribe {
            after_sequence: replay_through,
        },
    );
    let mut caught_up_reader = BufReader::new(caught_up_stream);
    assert!(matches!(
        read_frame(&mut caught_up_reader),
        RunnerFrame::Hello { .. }
    ));
    assert!(matches!(
        read_frame(&mut caught_up_reader),
        RunnerFrame::CaughtUp { sequence, .. } if sequence == replay_through
    ));
    fs::write(&release, b"go").unwrap();
    loop {
        let frame = read_frame(&mut reader);
        let terminal = matches!(
            frame,
            RunnerFrame::Event {
                event: factory_core::runner::RunnerEventEnvelope {
                    event: RunnerEvent::SpawnFailed { .. } | RunnerEvent::Exited { .. },
                    ..
                },
                ..
            }
        );
        frames.push(frame);
        if terminal {
            break;
        }
    }
    let events = event_frames(&frames);
    assert_eq!(
        events
            .iter()
            .map(|(sequence, _)| *sequence)
            .collect::<Vec<_>>(),
        (1..=i64::try_from(events.len()).unwrap()).collect::<Vec<_>>()
    );
    assert!(events.iter().any(|(_, event)| matches!(
        event,
        RunnerEvent::Output { stream: OutputStream::Stderr, text, .. } if text.contains("after")
    )));
    let caught_up_index = frames
        .iter()
        .position(|frame| matches!(frame, RunnerFrame::CaughtUp { .. }))
        .unwrap();
    let after_index = frames
        .iter()
        .position(|frame| matches!(
            frame,
            RunnerFrame::Event { event, .. }
                if matches!(&event.event, RunnerEvent::Output { text, .. } if text.contains("after"))
        ))
        .unwrap();
    assert!(caught_up_index < after_index);
    assert!(
        frames[caught_up_index + 1..]
            .iter()
            .all(|frame| match frame {
                RunnerFrame::Event { event, .. } => event.sequence > replay_through,
                _ => true,
            })
    );
    let terminal = terminal_sequence(&frames);
    assert_command_ack(
        acknowledge(&runner, terminal, "terminal-persisted"),
        "terminal-persisted",
    );
    runner.wait_for_clean_exit();
}

#[test]
fn stop_is_idempotent_and_terminates_the_owned_process_group() {
    let directory = tempfile::tempdir().unwrap();
    let marker = directory.path().join("child.pid");
    let runner = RunningRunner::spawn(&[
        "--spawn-child-marker",
        marker.to_str().unwrap(),
        "--sleep-ms",
        "10000",
    ]);
    let child_pid = wait_for_pid_file(&marker);
    let stop = RunnerRequest::Stop {
        command_id: "stop-1".into(),
        grace_ms: 100,
    };
    let mut lost_reply = connect(&runner.socket());
    write_request(&mut lost_reply, INSTANCE_ID, stop.clone());
    drop(lost_reply);

    runner.wait_for_terminal_spool();
    assert_command_ack(request(&runner, INSTANCE_ID, stop.clone()), "stop-1");
    assert!(matches!(
        request(
            &runner,
            INSTANCE_ID,
            RunnerRequest::Stop {
                command_id: "stop-1".into(),
                grace_ms: 101,
            }
        ),
        RunnerFrame::Error {
            code: RunnerErrorCode::Conflict,
            ..
        }
    ));
    assert_command_ack(request(&runner, INSTANCE_ID, stop), "stop-1");
    let frames = subscribe_through_terminal(&runner, 0);
    assert!(event_frames(&frames).iter().any(|(_, event)| matches!(
        event,
        RunnerEvent::Exited {
            exit_code: None,
            signal: Some(15),
        }
    )));
    wait_for_process_exit(child_pid);
    let terminal = terminal_sequence(&frames);
    assert_command_ack(
        acknowledge(&runner, terminal, "stopped-persisted"),
        "stopped-persisted",
    );
    runner.wait_for_clean_exit();
}

/// A provider tool can outlive its immediate shell, and that tool can have a
/// still deeper child of its own. The stop path must not publish `Exited`
/// between the provider leader's death and the group's final disappearance:
/// doing so would let the daemon clear the session's ownership while these
/// exact processes were still running in the agent worktree.
#[test]
fn forced_stop_reaps_nested_child_and_grandchild_before_terminal_event() {
    let directory = tempfile::tempdir().unwrap();
    let child_marker = directory.path().join("child.pid");
    let grandchild_marker = directory.path().join("grandchild.pid");
    let runner = RunningRunner::spawn_program(
        Path::new("/bin/sh"),
        &[
            "-c",
            "trap '' TERM; /bin/sh -c 'sleep 30 >/dev/null 2>&1 & echo $! > \"$1\"; trap \"\" TERM; while :; do sleep 1; done' sh \"$2\" & echo $! > \"$1\"; while :; do sleep 1; done",
            "sh",
            child_marker.to_str().unwrap(),
            grandchild_marker.to_str().unwrap(),
        ],
    );
    let child_pid = wait_for_pid_file(&child_marker);
    let grandchild_pid = wait_for_pid_file(&grandchild_marker);

    assert_command_ack(
        request(
            &runner,
            INSTANCE_ID,
            RunnerRequest::Stop {
                command_id: "stop-nested".into(),
                grace_ms: 0,
            },
        ),
        "stop-nested",
    );
    runner.wait_for_terminal_spool();
    wait_for_process_exit(child_pid);
    wait_for_process_exit(grandchild_pid);

    let frames = subscribe_through_terminal(&runner, 0);
    assert!(event_frames(&frames).iter().any(|(_, event)| matches!(
        event,
        RunnerEvent::Exited {
            exit_code: None,
            signal: Some(15 | 9),
        }
    )));
    let terminal = terminal_sequence(&frames);
    assert_command_ack(
        acknowledge(&runner, terminal, "stopped-nested-persisted"),
        "stopped-nested-persisted",
    );
    runner.wait_for_clean_exit();
}

/// The leader here exits entirely on its own — no `Stop` is ever sent — so
/// the group cleanup reaches this descendant with `DEFAULT_GROUP_GRACE`
/// (2s), not the shorter `GROUP_CLEANUP_GRACE`: this straggler's TERM is its
/// first and only grace period, not a mop-up of one already spent (see
/// `GROUP_CLEANUP_GRACE`'s doc comment). That grace is still capped at 2s,
/// same as before this fix, but the cleanup now polls instead of blindly
/// waiting it out: `sleep`, with no trap, dies from that first TERM almost
/// immediately, so this should complete in well under a second, not the ~2s
/// the pre-fix blind wait took for this exact scenario.
#[test]
fn natural_leader_exit_terminates_a_descendant_that_retains_output_pipes() {
    let directory = tempfile::tempdir().unwrap();
    let marker = directory.path().join("background.pid");
    let runner = RunningRunner::spawn_program(
        Path::new("/bin/sh"),
        &[
            "-c",
            "sleep 30 & echo $! > \"$1\"",
            "sh",
            marker.to_str().unwrap(),
        ],
    );
    let descendant_pid = wait_for_pid_file(&marker);
    let started = Instant::now();
    runner.wait_for_terminal_spool();
    let elapsed = started.elapsed();
    assert!(
        elapsed < Duration::from_secs(1),
        "reaping a well-behaved descendant after the leader's natural exit took {elapsed:?}; \
         group cleanup should poll and notice the group is empty almost immediately, not wait \
         out a multi-second grace it doesn't need"
    );
    wait_for_process_exit(descendant_pid);
    let frames = subscribe_through_terminal(&runner, 0);
    assert!(event_frames(&frames).iter().any(|(_, event)| matches!(
        event,
        RunnerEvent::Exited {
            exit_code: Some(0),
            signal: None,
        }
    )));
    let terminal = terminal_sequence(&frames);
    assert_command_ack(
        acknowledge(&runner, terminal, "descendants-finished"),
        "descendants-finished",
    );
    runner.wait_for_clean_exit();
}

#[test]
fn runner_sigterm_reaps_the_group_and_preserves_unacknowledged_spool() {
    let directory = tempfile::tempdir().unwrap();
    let marker = directory.path().join("child.pid");
    let mut runner = RunningRunner::spawn(&[
        "--spawn-child-marker",
        marker.to_str().unwrap(),
        "--sleep-ms",
        "10000",
    ]);
    let descendant_pid = wait_for_pid_file(&marker);
    let runner_pid = Pid::from_raw(i32::try_from(runner.runner_pid()).unwrap()).unwrap();
    kill_process(runner_pid, Signal::TERM).unwrap();
    runner.wait_for_terminal_spool();
    wait_for_process_exit(descendant_pid);
    runner.wait_for_successful_exit();
    assert!(runner.spool().exists());
    assert!(runner.socket().exists());
    assert!(UnixStream::connect(runner.socket()).is_err());
    let spool = fs::read_to_string(runner.spool()).unwrap();
    assert!(spool.lines().any(|line| {
        serde_json::from_str::<RunnerEventEnvelope>(line).is_ok_and(|event| {
            matches!(
                event.event,
                RunnerEvent::Exited {
                    exit_code: None,
                    signal: Some(15),
                }
            )
        })
    }));
}

/// The second production leak shape is an interrupted agent/tool turn: the
/// runner receives its own shutdown signal while a nested tool tree is still
/// alive. The runner must terminate that same owned group before it exits,
/// without broad process-name signalling.
#[test]
fn runner_interrupt_reaps_nested_child_and_grandchild() {
    let directory = tempfile::tempdir().unwrap();
    let child_marker = directory.path().join("interrupt-child.pid");
    let grandchild_marker = directory.path().join("interrupt-grandchild.pid");
    let mut runner = RunningRunner::spawn_program(
        Path::new("/bin/sh"),
        &[
            "-c",
            "trap '' TERM; /bin/sh -c 'sleep 30 >/dev/null 2>&1 & echo $! > \"$1\"; trap \"\" TERM; while :; do sleep 1; done' sh \"$2\" & echo $! > \"$1\"; while :; do sleep 1; done",
            "sh",
            child_marker.to_str().unwrap(),
            grandchild_marker.to_str().unwrap(),
        ],
    );
    let child_pid = wait_for_pid_file(&child_marker);
    let grandchild_pid = wait_for_pid_file(&grandchild_marker);
    let runner_pid = Pid::from_raw(i32::try_from(runner.runner_pid()).unwrap()).unwrap();
    kill_process(runner_pid, Signal::TERM).unwrap();

    runner.wait_for_terminal_spool();
    wait_for_process_exit(child_pid);
    wait_for_process_exit(grandchild_pid);
    runner.wait_for_successful_exit();
}

#[test]
fn stop_escalates_only_after_the_requested_grace_period() {
    let directory = tempfile::tempdir().unwrap();
    let marker = directory.path().join("ready");
    let marker_text = marker.to_str().unwrap();
    let runner = RunningRunner::spawn_program(
        Path::new("/bin/sh"),
        &[
            "-c",
            "trap '' TERM; echo ready > \"$1\"; while :; do sleep 1; done",
            "sh",
            marker_text,
        ],
    );
    wait_for_path(&marker);
    let started = Instant::now();
    assert_command_ack(
        request(
            &runner,
            INSTANCE_ID,
            RunnerRequest::Stop {
                command_id: "escalate".into(),
                grace_ms: 200,
            },
        ),
        "escalate",
    );
    runner.wait_for_terminal_spool();
    assert!(started.elapsed() >= Duration::from_millis(150));
    let frames = subscribe_through_terminal(&runner, 0);
    assert!(event_frames(&frames).iter().any(|(_, event)| matches!(
        event,
        RunnerEvent::Exited {
            exit_code: None,
            signal: Some(9),
        }
    )));
    let terminal = terminal_sequence(&frames);
    assert_command_ack(
        acknowledge(&runner, terminal, "escalated-persisted"),
        "escalated-persisted",
    );
    runner.wait_for_clean_exit();
}

#[test]
fn output_is_utf8_safe_and_the_spool_has_one_explicit_hard_limit() {
    let release_directory = tempfile::tempdir().unwrap();
    let release = release_directory.path().join("release-output");
    let runner = RunningRunner::spawn(&[
        "--split-utf8",
        "--invalid-stderr",
        "--wait-before-stdout-bytes-file",
        release.to_str().unwrap(),
        "--stdout-bytes",
        &(MAX_RUNNER_SPOOL_BYTES + 1024 * 1024).to_string(),
    ]);
    let mut non_reader_stream = connect(&runner.socket());
    write_request(
        &mut non_reader_stream,
        INSTANCE_ID,
        RunnerRequest::Subscribe { after_sequence: 0 },
    );
    let mut non_reader = BufReader::new(non_reader_stream);
    let lagged_after = loop {
        if let RunnerFrame::CaughtUp { sequence, .. } = read_frame(&mut non_reader) {
            break sequence;
        }
    };
    fs::write(&release, b"go").unwrap();
    runner.wait_for_terminal_spool();
    let frames = subscribe_through_terminal(&runner, 0);
    let events = event_frames(&frames);
    assert_eq!(
        events
            .iter()
            .map(|(sequence, _)| *sequence)
            .collect::<Vec<_>>(),
        (1..=i64::try_from(events.len()).unwrap()).collect::<Vec<_>>()
    );
    assert!(events.iter().all(|(_, event)| match event {
        RunnerEvent::Output { text, .. } => text.len() <= MAX_RUNNER_OUTPUT_TEXT_BYTES,
        _ => true,
    }));
    let stdout = events
        .iter()
        .filter_map(|(_, event)| match event {
            RunnerEvent::Output {
                stream: OutputStream::Stdout,
                text,
                lossy: false,
            } => Some(text.as_str()),
            _ => None,
        })
        .collect::<String>();
    assert!(stdout.contains('🦀'));
    assert!(events.iter().any(|(_, event)| matches!(
        event,
        RunnerEvent::Output {
            stream: OutputStream::Stderr,
            lossy: true,
            ..
        }
    )));
    let truncations = events
        .iter()
        .filter_map(|(_, event)| match event {
            RunnerEvent::OutputTruncated { limit_bytes } => Some(*limit_bytes),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        truncations,
        vec![u64::try_from(MAX_RUNNER_SPOOL_BYTES).unwrap()]
    );
    assert!(matches!(
        events.last().unwrap().1,
        RunnerEvent::Exited { .. }
    ));
    assert!(
        fs::metadata(runner.spool()).unwrap().len()
            <= u64::try_from(MAX_RUNNER_SPOOL_BYTES).unwrap()
    );
    assert_eq!(
        fs::metadata(&runner.runtime).unwrap().permissions().mode() & 0o777,
        0o700
    );
    assert_eq!(
        fs::metadata(runner.spool()).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert_eq!(
        fs::metadata(runner.socket()).unwrap().permissions().mode() & 0o777,
        0o600
    );
    let terminal = terminal_sequence(&frames);
    assert_eq!(terminal, events.last().unwrap().0);
    let mut lagged_sequences = Vec::new();
    loop {
        if let RunnerFrame::Event { event, .. } = read_frame(&mut non_reader) {
            lagged_sequences.push(event.sequence);
            if matches!(
                event.event,
                RunnerEvent::SpawnFailed { .. } | RunnerEvent::Exited { .. }
            ) {
                break;
            }
        }
    }
    assert_eq!(
        lagged_sequences,
        ((lagged_after + 1)..=terminal).collect::<Vec<_>>()
    );
    assert_command_ack(
        acknowledge(&runner, terminal, "bounded-output-persisted"),
        "bounded-output-persisted",
    );
    drop(non_reader);
    runner.wait_for_clean_exit();
}

#[test]
fn terminal_acknowledgement_survives_a_lost_reply() {
    let runner = RunningRunner::spawn(&[]);
    runner.wait_for_terminal_spool();
    let terminal = terminal_sequence(&subscribe_through_terminal(&runner, 0));
    let mut stream = connect(&runner.socket());
    write_request(
        &mut stream,
        INSTANCE_ID,
        RunnerRequest::AcknowledgeExit {
            command_id: "reply-may-be-lost".into(),
            terminal_sequence: terminal,
        },
    );
    drop(stream);
    runner.wait_for_clean_exit();
}

#[test]
fn preexisting_or_symlinked_runtime_paths_are_rejected_without_spawning() {
    let directory = tempfile::tempdir().unwrap();
    let program = Path::new(env!("CARGO_BIN_EXE_fake-agent"));
    let marker = directory.path().join("spawned");
    let marker_argument = marker.to_str().unwrap();

    let occupied = directory.path().join("occupied");
    fs::create_dir(&occupied).unwrap();
    // Keep this fixture a non-private preexisting path even on hosts whose
    // umask would otherwise make tempfile children mode 0700 (which the
    // daemon legitimately stages and the runner adopts).
    fs::set_permissions(&occupied, fs::Permissions::from_mode(0o755)).unwrap();
    let sentinel = occupied.join("sentinel");
    fs::write(&sentinel, b"preserve").unwrap();
    let status = runner_command(
        &occupied,
        directory.path(),
        program,
        &["--child-marker", marker_argument],
        None,
    )
    .status()
    .unwrap();
    assert!(!status.success());
    assert_eq!(fs::read(&sentinel).unwrap(), b"preserve");
    assert!(!marker.exists());

    let target = directory.path().join("target");
    fs::create_dir(&target).unwrap();
    fs::set_permissions(&target, fs::Permissions::from_mode(0o755)).unwrap();
    let linked = directory.path().join("linked");
    std::os::unix::fs::symlink(&target, &linked).unwrap();
    let status = runner_command(
        &linked,
        directory.path(),
        program,
        &["--child-marker", marker_argument],
        None,
    )
    .status()
    .unwrap();
    assert!(!status.success());
    assert!(!marker.exists());
    assert!(linked.symlink_metadata().unwrap().file_type().is_symlink());
}

fn wait_for_spool_text(spool: &Path, needle: &str) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if fs::read_to_string(spool)
            .unwrap_or_default()
            .contains(needle)
        {
            return;
        }
        thread::sleep(Duration::from_millis(10));
    }
    panic!("spool never contained {needle:?}");
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

fn wait_for_pid_file(path: &Path) -> i32 {
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if let Ok(pid) = fs::read_to_string(path).unwrap_or_default().trim().parse() {
            return pid;
        }
        thread::sleep(Duration::from_millis(10));
    }
    panic!("PID did not appear in {}", path.display());
}

fn wait_for_process_exit(raw_pid: i32) {
    let pid = rustix::process::Pid::from_raw(raw_pid).unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if rustix::process::test_kill_process(pid).is_err() {
            return;
        }
        thread::sleep(Duration::from_millis(10));
    }
    panic!("descendant process {raw_pid} survived group stop");
}
