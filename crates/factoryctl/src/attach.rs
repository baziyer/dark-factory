//! `factoryctl attach`: raw terminal passthrough to one terminal-mode run's
//! PTY. This is an operator escape hatch and a proof that the runner/daemon
//! terminal wire works end to end; it is CLI-only and separate from the
//! `factory-tui` board's embedded panes.

use std::{
    io::{Read, Write},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread,
};

use factory_core::{
    ProjectId, SessionId,
    local::{LocalRequest, LocalResponse, ServerFrame},
    runner::{decode_terminal_bytes, encode_terminal_bytes},
};
use factoryctl::Client;
use rustix::termios::{self, OptionalActions, Termios};

/// Byte the operator types to detach: Ctrl-] (ASCII GS, 0x1D), the classic
/// escape character used by telnet and terminal multiplexers. It is
/// consumed locally and never forwarded to the remote PTY.
const DETACH_BYTE: u8 = 0x1D;
const STDIN_CHUNK_BYTES: usize = 4096;

/// Attaches to `session_id`'s PTY: puts the local terminal in raw mode,
/// forwards stdin as `TerminalInput`, prints `TerminalOutput` to stdout,
/// sends an initial resize plus one on every `SIGWINCH`, and restores the
/// terminal on exit (including on panic, via unwind) or on `Ctrl-]`.
pub fn run(
    client: &Client,
    project_id: &str,
    session_id: &str,
    since_offset: u64,
) -> Result<i32, String> {
    let project_id = ProjectId::try_from(project_id.to_owned())
        .map_err(|error| format!("invalid project ID: {error}"))?;
    let session_id = SessionId::try_from(session_id.to_owned())
        .map_err(|error| format!("invalid session ID: {error}"))?;

    let frames = client
        .attach_terminal(LocalRequest::AttachTerminal {
            project_id: project_id.clone(),
            session_id: session_id.clone(),
            since_offset,
        })
        .map_err(|error| error.to_string())?;

    let raw_mode = RawMode::enable().map_err(|error| error.to_string())?;

    let failed = Arc::new(AtomicBool::new(false));
    let failure: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    let output_thread = spawn_output_thread(frames, Arc::clone(&failed), Arc::clone(&failure));

    send_resize(client, &project_id, &session_id);
    spawn_resize_watcher(client.clone(), project_id.clone(), session_id.clone());

    let exit_code = forward_stdin(client, &project_id, &session_id, &failed);

    drop(raw_mode);
    let _ = output_thread.join();
    if let Some(message) = failure
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .take()
    {
        return Err(message);
    }
    Ok(exit_code)
}

fn spawn_output_thread(
    frames: factoryctl::TerminalFrames,
    done: Arc<AtomicBool>,
    failure: Arc<Mutex<Option<String>>>,
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        let mut stdout = std::io::stdout();
        for frame in frames {
            match frame {
                Ok(ServerFrame::TerminalOutput { bytes, .. }) => {
                    match decode_terminal_bytes(&bytes) {
                        Ok(raw) => {
                            if stdout.write_all(&raw).is_err() || stdout.flush().is_err() {
                                break;
                            }
                        }
                        Err(_) => {
                            set_failure(&failure, "daemon sent unreadable terminal output".into());
                            break;
                        }
                    }
                }
                Ok(ServerFrame::Response {
                    response: LocalResponse::Error { message, .. },
                    ..
                }) => {
                    set_failure(&failure, message);
                    break;
                }
                Ok(_) => {}
                Err(error) => {
                    set_failure(&failure, error.to_string());
                    break;
                }
            }
        }
        done.store(true, Ordering::SeqCst);
    })
}

fn set_failure(failure: &Mutex<Option<String>>, message: String) {
    *failure.lock().unwrap_or_else(|error| error.into_inner()) = Some(message);
}

/// Reads stdin and forwards it as `TerminalInput` until EOF, `Ctrl-]`, or the
/// output side reports failure. Returns the process exit code.
fn forward_stdin(
    client: &Client,
    project_id: &ProjectId,
    session_id: &SessionId,
    failed: &AtomicBool,
) -> i32 {
    let mut stdin = std::io::stdin();
    let mut buffer = [0_u8; STDIN_CHUNK_BYTES];
    loop {
        if failed.load(Ordering::SeqCst) {
            return 2;
        }
        let read = match stdin.read(&mut buffer) {
            Ok(0) | Err(_) => return 0,
            Ok(read) => read,
        };
        let chunk = &buffer[..read];
        if let Some(detach_at) = chunk.iter().position(|byte| *byte == DETACH_BYTE) {
            if detach_at > 0 {
                send_input(client, project_id, session_id, &chunk[..detach_at]);
            }
            return 0;
        }
        send_input(client, project_id, session_id, chunk);
    }
}

fn send_input(client: &Client, project_id: &ProjectId, session_id: &SessionId, bytes: &[u8]) {
    if bytes.is_empty() {
        return;
    }
    let _ = client.request(LocalRequest::TerminalInput {
        project_id: project_id.clone(),
        session_id: session_id.clone(),
        bytes: encode_terminal_bytes(bytes),
    });
}

fn send_resize(client: &Client, project_id: &ProjectId, session_id: &SessionId) {
    if let Ok((cols, rows)) = local_terminal_size() {
        let _ = client.request(LocalRequest::ResizeTerminal {
            project_id: project_id.clone(),
            session_id: session_id.clone(),
            cols,
            rows,
        });
    }
}

fn local_terminal_size() -> std::io::Result<(u16, u16)> {
    let winsize = termios::tcgetwinsize(std::io::stdout()).map_err(std::io::Error::from)?;
    Ok((winsize.ws_col, winsize.ws_row))
}

fn spawn_resize_watcher(client: Client, project_id: ProjectId, session_id: SessionId) {
    let Ok(mut signals) = signal_hook::iterator::Signals::new([signal_hook::consts::SIGWINCH])
    else {
        return;
    };
    thread::spawn(move || {
        for _ in signals.forever() {
            send_resize(&client, &project_id, &session_id);
        }
    });
}

/// Puts the local terminal into raw mode for the guard's lifetime and
/// restores the original settings when it is dropped, including on a
/// panicking unwind (this crate does not set `panic = "abort"`).
struct RawMode {
    original: Termios,
}

impl RawMode {
    fn enable() -> std::io::Result<Self> {
        let stdin = std::io::stdin();
        let original = termios::tcgetattr(&stdin).map_err(std::io::Error::from)?;
        let mut raw = original.clone();
        raw.make_raw();
        termios::tcsetattr(&stdin, OptionalActions::Flush, &raw).map_err(std::io::Error::from)?;
        Ok(Self { original })
    }
}

impl Drop for RawMode {
    fn drop(&mut self) {
        let stdin = std::io::stdin();
        let _ = termios::tcsetattr(&stdin, OptionalActions::Flush, &self.original);
    }
}
