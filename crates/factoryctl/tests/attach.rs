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
        atomic::{AtomicBool, Ordering},
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
    stop: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
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
        let stop = Arc::new(AtomicBool::new(false));
        let (attach_ready_tx, attach_ready) = mpsc::channel();
        let thread_inputs = Arc::clone(&inputs);
        let thread_gate = Arc::clone(&output_gate);
        let thread_stop = Arc::clone(&stop);
        let thread = thread::spawn(move || {
            while !thread_stop.load(Ordering::Acquire) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        let inputs = Arc::clone(&thread_inputs);
                        let gate = Arc::clone(&thread_gate);
                        let ready = attach_ready_tx.clone();
                        thread::spawn(move || handle_connection(stream, inputs, gate, ready));
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
            stop,
            thread: Some(thread),
            _directory: directory,
        }
    }

    fn allow_output(&self) {
        self.output_gate.store(true, Ordering::Release);
    }

    fn wait_for_attach(&self) {
        self.attach_ready
            .recv_timeout(Duration::from_secs(5))
            .expect("factoryctl never opened the attach stream");
    }

    fn inputs(&self) -> Vec<Vec<u8>> {
        self.inputs.lock().unwrap().clone()
    }
}

impl Drop for FakeDaemon {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        let _ = UnixStream::connect(&self.socket);
        if let Some(thread) = self.thread.take() {
            thread.join().unwrap();
        }
    }
}

fn handle_connection(
    mut stream: UnixStream,
    inputs: Arc<Mutex<Vec<Vec<u8>>>>,
    output_gate: Arc<AtomicBool>,
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
    daemon.wait_for_attach();
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
    daemon.wait_for_attach();
    // Closing the only stdout reader makes the production writer fail. The
    // output thread must set the lifecycle error, cancel the blocked attach
    // reader, join it, and let RawMode restore before returning.
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
    assert_eq!(
        master.get_termios().map(|termios| format!("{termios:?}")),
        before,
        "raw mode was not restored"
    );
}
