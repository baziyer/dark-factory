//! Black-box lifecycle checks for the executable `factoryctl attach` path.
//!
//! The daemon is a small isolated Unix-socket fixture and the CLI runs in a
//! real pseudo-terminal for stdin. This deliberately does not call the
//! stdin/output helpers directly: the assertions cover the production
//! attach loop, cancellation socket, raw-mode guard, and thread joins.

use std::{
    fs::OpenOptions,
    io::{BufRead, BufReader, Read, Write},
    os::unix::net::{UnixListener, UnixStream},
    path::PathBuf,
    process::{Child, Command, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
        mpsc::{self, Receiver},
    },
    thread,
    time::{Duration, Instant},
};

use factory_core::{
    PROTOCOL_VERSION, SessionId,
    local::{LocalRequest, LocalResponse, RequestEnvelope, ServerFrame},
    runner::encode_terminal_bytes,
};
use portable_pty::{MasterPty, NativePtySystem, PtySize, PtySystem};

struct FakeDaemon {
    socket: PathBuf,
    inputs: Arc<Mutex<Vec<Vec<u8>>>>,
    attach_ready: Receiver<()>,
    output_gate: Arc<AtomicBool>,
    attach_cancelled: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
    handler_threads: Arc<Mutex<Vec<thread::JoinHandle<()>>>>,
    active_handlers: Arc<AtomicUsize>,
    _directory: tempfile::TempDir,
}

impl FakeDaemon {
    fn start() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("factory.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        listener.set_nonblocking(true).unwrap();
        let inputs = Arc::new(Mutex::new(Vec::new()));
        let output_gate = Arc::new(AtomicBool::new(false));
        let attach_cancelled = Arc::new(AtomicBool::new(false));
        let stop = Arc::new(AtomicBool::new(false));
        let handler_threads = Arc::new(Mutex::new(Vec::new()));
        let active_handlers = Arc::new(AtomicUsize::new(0));
        let (attach_ready_tx, attach_ready) = mpsc::channel();
        let thread_inputs = Arc::clone(&inputs);
        let thread_gate = Arc::clone(&output_gate);
        let thread_cancelled = Arc::clone(&attach_cancelled);
        let thread_stop = Arc::clone(&stop);
        let thread_handler_threads = Arc::clone(&handler_threads);
        let thread_active_handlers = Arc::clone(&active_handlers);
        let thread = thread::spawn(move || {
            while !thread_stop.load(Ordering::Acquire) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        let inputs = Arc::clone(&thread_inputs);
                        let gate = Arc::clone(&thread_gate);
                        let cancelled = Arc::clone(&thread_cancelled);
                        let ready = attach_ready_tx.clone();
                        let active_handlers = Arc::clone(&thread_active_handlers);
                        active_handlers.fetch_add(1, Ordering::AcqRel);
                        let handler = thread::spawn(move || {
                            handle_connection(stream, inputs, gate, cancelled, ready);
                            active_handlers.fetch_sub(1, Ordering::AcqRel);
                        });
                        thread_handler_threads.lock().unwrap().push(handler);
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(2));
                    }
                    Err(_) => break,
                }
            }
        });
        Self {
            socket,
            inputs,
            attach_ready,
            output_gate,
            attach_cancelled,
            stop,
            thread: Some(thread),
            handler_threads,
            active_handlers,
            _directory: directory,
        }
    }

    fn allow_output(&self) {
        self.output_gate.store(true, Ordering::Release);
    }

    fn wait_for_attach(&self, child: &mut Child) {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match self.attach_ready.recv_timeout(Duration::from_millis(50)) {
                Ok(()) => return,
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    panic!("fake daemon stopped before factoryctl opened the attach stream")
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    if let Some(status) = child.try_wait().unwrap() {
                        let mut stderr = String::new();
                        if let Some(stderr_pipe) = child.stderr.as_mut() {
                            stderr_pipe.read_to_string(&mut stderr).unwrap();
                        }
                        panic!(
                            "factoryctl exited before opening the attach stream: status={status:?}, stderr={stderr:?}"
                        );
                    }
                    if Instant::now() >= deadline {
                        let pid = child.id();
                        let _ = child.kill();
                        let _ = child.wait();
                        panic!(
                            "factoryctl remained running but never opened the attach stream (pid={pid})"
                        );
                    }
                }
            }
        }
    }

    fn inputs(&self) -> Vec<Vec<u8>> {
        self.inputs.lock().unwrap().clone()
    }

    fn attach_cancelled(&self) -> bool {
        self.attach_cancelled.load(Ordering::Acquire)
    }
}

impl Drop for FakeDaemon {
    fn drop(&mut self) {
        self.output_gate.store(true, Ordering::Release);
        self.stop.store(true, Ordering::Release);
        let _ = UnixStream::connect(&self.socket);
        if let Some(thread) = self.thread.take() {
            thread.join().unwrap();
        }
        let handlers = self
            .handler_threads
            .lock()
            .unwrap()
            .drain(..)
            .collect::<Vec<_>>();
        for handler in handlers {
            handler.join().unwrap();
        }
        assert_eq!(
            self.active_handlers.load(Ordering::Acquire),
            0,
            "fake daemon leaked a connection handler"
        );
    }
}

fn handle_connection(
    mut stream: UnixStream,
    inputs: Arc<Mutex<Vec<Vec<u8>>>>,
    output_gate: Arc<AtomicBool>,
    attach_cancelled: Arc<AtomicBool>,
    attach_ready: mpsc::Sender<()>,
) {
    let mut line = String::new();
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    if reader.read_line(&mut line).unwrap_or(0) == 0 {
        return;
    }
    let envelope: RequestEnvelope = serde_json::from_str(&line).unwrap();
    match envelope.request {
        LocalRequest::AttachTerminal { session_id, .. } => {
            attach_ready.send(()).unwrap();
            while !output_gate.load(Ordering::Acquire) {
                thread::sleep(Duration::from_millis(2));
            }
            write_frame(
                &mut stream,
                ServerFrame::TerminalOutput {
                    protocol_version: PROTOCOL_VERSION,
                    session_id,
                    generation: 0,
                    offset: 0,
                    bytes: encode_terminal_bytes(b"attached"),
                },
            );
            let mut discard = [0_u8; 1];
            let _ = stream.read(&mut discard);
            attach_cancelled.store(true, Ordering::Release);
        }
        LocalRequest::TerminalInput { bytes, .. } => {
            inputs
                .lock()
                .unwrap()
                .push(factory_core::runner::decode_terminal_bytes(&bytes).unwrap());
            write_frame(
                &mut stream,
                ServerFrame::Response {
                    protocol_version: PROTOCOL_VERSION,
                    response: LocalResponse::TerminalInputAccepted {
                        session_id: SessionId::try_from("session-1").unwrap(),
                    },
                },
            );
        }
        LocalRequest::ResizeTerminal { session_id, .. } => write_frame(
            &mut stream,
            ServerFrame::Response {
                protocol_version: PROTOCOL_VERSION,
                response: LocalResponse::TerminalResized { session_id },
            },
        ),
        _ => {}
    }
}

fn write_frame(stream: &mut UnixStream, frame: ServerFrame) {
    let Ok(mut payload) = serde_json::to_vec(&frame) else {
        return;
    };
    payload.push(b'\n');
    let _ = stream.write_all(&payload);
    let _ = stream.flush();
}

fn spawn_cli(
    daemon: &FakeDaemon,
    stdout: Stdio,
) -> (
    Child,
    Box<dyn Write + Send>,
    Option<String>,
    Box<dyn MasterPty>,
) {
    let pty = NativePtySystem::default()
        .openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .unwrap();
    let tty = pty.master.tty_name().unwrap();
    let stdin = OpenOptions::new().read(true).write(true).open(tty).unwrap();
    let before = pty
        .master
        .get_termios()
        .map(|termios| format!("{termios:?}"));
    let child = Command::new(env!("CARGO_BIN_EXE_factoryctl"))
        .args([
            "--socket",
            daemon.socket.to_str().unwrap(),
            "attach",
            "--project",
            "project-1",
            "--session",
            "session-1",
        ])
        .stdin(Stdio::from(stdin))
        .stdout(stdout)
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let writer = pty.master.take_writer().unwrap();
    (child, writer, before, pty.master)
}

fn wait_promptly(child: &mut Child) -> std::process::ExitStatus {
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            return status;
        }
        assert!(
            Instant::now() < deadline,
            "factoryctl attach did not return promptly"
        );
        thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn executable_attach_ctrl_right_bracket_detaches_and_restores_raw_mode() {
    let daemon = FakeDaemon::start();
    let (mut child, mut input, before, master) = spawn_cli(&daemon, Stdio::null());
    daemon.wait_for_attach(&mut child);
    daemon.allow_output();
    input.write_all(b"before\x03after\x1dignored").unwrap();
    input.flush().unwrap();

    let status = wait_promptly(&mut child);
    assert!(status.success(), "detach returned {status:?}");
    assert_eq!(daemon.inputs(), vec![b"before\x03after".to_vec()]);
    assert_eq!(
        master.get_termios().map(|termios| format!("{termios:?}")),
        before,
        "raw mode was not restored"
    );
}

#[test]
fn executable_attach_output_failure_cancels_reader_and_restores_raw_mode() {
    let daemon = FakeDaemon::start();
    let (mut child, _input, before, master) = spawn_cli(&daemon, Stdio::piped());
    daemon.wait_for_attach(&mut child);
    // Mutation proof: closing the only stdout reader makes the production
    // writer fail. The repaired lifecycle must wake the blocked input wait,
    // cancel the attach socket, join both workers, and let RawMode restore
    // before returning. The old timing-only reader lifecycle hangs here.
    drop(child.stdout.take());
    daemon.allow_output();

    let status = wait_promptly(&mut child);
    assert!(!status.success(), "output failure was reported as success");
    let mut stderr = String::new();
    child
        .stderr
        .take()
        .unwrap()
        .read_to_string(&mut stderr)
        .unwrap();
    assert!(stderr.contains("terminal output stream"), "{stderr}");
    assert!(
        daemon.attach_cancelled(),
        "output failure did not cancel the attach socket"
    );
    assert_eq!(
        master.get_termios().map(|termios| format!("{termios:?}")),
        before,
        "raw mode was not restored"
    );
}

#[test]
fn executable_attach_repeated_start_attach_detach_has_no_handler_or_child_leak() {
    for iteration in 0..8 {
        let daemon = FakeDaemon::start();
        let (mut child, mut input, before, master) = spawn_cli(&daemon, Stdio::null());
        daemon.wait_for_attach(&mut child);
        daemon.allow_output();
        input
            .write_all(format!("before-{iteration}\x03after\x1dignored").as_bytes())
            .unwrap();
        input.flush().unwrap();

        let status = wait_promptly(&mut child);
        assert!(status.success(), "detach returned {status:?}");
        assert_eq!(
            daemon.inputs(),
            vec![format!("before-{iteration}\x03after").into_bytes()]
        );
        assert_eq!(
            master.get_termios().map(|termios| format!("{termios:?}")),
            before,
            "raw mode was not restored on iteration {iteration}"
        );
    }
}
