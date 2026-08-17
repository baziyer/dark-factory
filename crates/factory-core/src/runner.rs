//! Stable, provider-blind control and event wire for one runner process.

use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde::{Deserialize, Serialize};

use crate::{RunId, RunnerInstanceId};

/// Runner wire version, intentionally independent from the local factory API.
pub const RUNNER_PROTOCOL_VERSION: u16 = 1;
/// Maximum JSON payload size. The newline delimiter is not part of this limit.
pub const MAX_RUNNER_FRAME_BYTES: usize = 1024 * 1024;
/// Maximum UTF-8 bytes in one output event's `text` field.
pub const MAX_RUNNER_OUTPUT_TEXT_BYTES: usize = 64 * 1024;
/// Maximum UTF-8 bytes in an error or spawn-failure message.
pub const MAX_RUNNER_ERROR_BYTES: usize = 16 * 1024;
/// Maximum bytes in the complete durable event spool for one runner attempt.
pub const MAX_RUNNER_SPOOL_BYTES: usize = 16 * 1024 * 1024;
/// Maximum task bytes transferred from the daemon to a new runner over stdin.
pub const MAX_STARTUP_STDIN_BYTES: usize = 1024 * 1024;
/// Maximum raw bytes accepted in one `TerminalInput` request, before base64
/// encoding. Larger paste operations must be split across requests.
pub const MAX_TERMINAL_INPUT_BYTES: usize = 64 * 1024;
/// Maximum raw PTY bytes read into one `TerminalOutput` chunk, before base64
/// encoding. Comfortably fits inside [`MAX_RUNNER_FRAME_BYTES`] once encoded.
pub const MAX_TERMINAL_OUTPUT_CHUNK_BYTES: usize = 16 * 1024;
/// Maximum raw bytes retained in one terminal log file before it is rotated.
///
/// The retained terminal log (`terminal.log`) is bounded and rotates exactly
/// once: when the active file reaches this size, it is renamed to
/// `terminal.log.1` (replacing any earlier rotation) and a fresh empty
/// `terminal.log` is opened. Up to two files' worth of raw PTY bytes are
/// retained at a time; older bytes are dropped for good.
pub const MAX_TERMINAL_LOG_BYTES: u64 = 64 * 1024 * 1024;

/// Terminal dimensions for an interactive PTY-mode run.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TerminalSize {
    pub cols: u16,
    pub rows: u16,
}

/// Encodes raw bytes for a binary-safe JSON string field.
///
/// PTY output and operator keystrokes are not guaranteed to be valid UTF-8
/// (raw control bytes, partial multi-byte sequences, arbitrary pasted
/// binary). Standard base64 keeps the wire format plain newline-delimited
/// JSON, matching every other runner frame, instead of adding a second
/// binary framing scheme just for terminal bytes.
#[must_use]
pub fn encode_terminal_bytes(bytes: &[u8]) -> String {
    STANDARD.encode(bytes)
}

/// Decodes bytes previously produced by [`encode_terminal_bytes`].
///
/// # Errors
///
/// Returns [`InvalidTerminalBytes`] when `encoded` is not valid standard
/// base64.
pub fn decode_terminal_bytes(encoded: &str) -> Result<Vec<u8>, InvalidTerminalBytes> {
    STANDARD.decode(encoded).map_err(|_| InvalidTerminalBytes)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
#[error("terminal bytes are not valid base64")]
pub struct InvalidTerminalBytes;

/// One authenticated, newline-delimited request sent to a runner.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct RequestEnvelope {
    pub protocol_version: u16,
    pub run_id: RunId,
    pub runner_instance_id: RunnerInstanceId,
    pub request: RunnerRequest,
}

impl RequestEnvelope {
    #[must_use]
    pub const fn new(
        run_id: RunId,
        runner_instance_id: RunnerInstanceId,
        request: RunnerRequest,
    ) -> Self {
        Self {
            protocol_version: RUNNER_PROTOCOL_VERSION,
            run_id,
            runner_instance_id,
            request,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum RunnerRequest {
    Subscribe {
        after_sequence: i64,
    },
    Stop {
        command_id: String,
        grace_ms: u64,
    },
    AcknowledgeExit {
        command_id: String,
        terminal_sequence: i64,
    },
    /// Attaches to a terminal-mode run's retained PTY output, replaying from
    /// `since_offset` and then streaming live bytes. Rejected with
    /// [`RunnerErrorCode::InvalidRequest`] on a run that was not launched
    /// with a PTY.
    AttachTerminal {
        since_offset: u64,
    },
    /// Writes operator input to the PTY master. `bytes` is
    /// [`encode_terminal_bytes`]-encoded and at most
    /// [`MAX_TERMINAL_INPUT_BYTES`] once decoded.
    TerminalInput {
        bytes: String,
    },
    /// Resizes the PTY, which delivers `SIGWINCH` to the foreground process
    /// group through the controlling terminal.
    ResizeTerminal {
        cols: u16,
        rows: u16,
    },
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RunnerErrorCode {
    InvalidRequest,
    UnsupportedProtocol,
    Unauthorized,
    Conflict,
    Internal,
}

/// One newline-delimited frame sent by a runner.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum RunnerFrame {
    Hello {
        protocol_version: u16,
        run_id: RunId,
        runner_instance_id: RunnerInstanceId,
        runner_pid: u32,
        replay_through: i64,
        terminal_sequence: Option<i64>,
    },
    Event {
        protocol_version: u16,
        event: RunnerEventEnvelope,
    },
    CaughtUp {
        protocol_version: u16,
        sequence: i64,
    },
    CommandAck {
        protocol_version: u16,
        command_id: String,
    },
    Error {
        protocol_version: u16,
        code: RunnerErrorCode,
        message: String,
    },
    /// One chunk of retained-then-live PTY output for an attached terminal
    /// session. `offset` is the byte-stream position of `bytes[0]`; the next
    /// expected offset is `offset + bytes.len()` after decoding.
    TerminalOutput {
        protocol_version: u16,
        offset: u64,
        bytes: String,
    },
}

impl RunnerFrame {
    #[must_use]
    pub const fn protocol_version(&self) -> u16 {
        match self {
            Self::Hello {
                protocol_version, ..
            }
            | Self::Event {
                protocol_version, ..
            }
            | Self::CaughtUp {
                protocol_version, ..
            }
            | Self::CommandAck {
                protocol_version, ..
            }
            | Self::Error {
                protocol_version, ..
            }
            | Self::TerminalOutput {
                protocol_version, ..
            } => *protocol_version,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct RunnerEventEnvelope {
    pub protocol_version: u16,
    pub sequence: i64,
    pub occurred_at_ms: i64,
    pub event: RunnerEvent,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum RunnerEvent {
    Started {
        child_pid: u32,
    },
    Output {
        stream: OutputStream,
        text: String,
        lossy: bool,
    },
    SpawnFailed {
        message: String,
    },
    OutputTruncated {
        limit_bytes: u64,
    },
    /// The spawned child took its controlling tty out of canonical mode
    /// (`ICANON` cleared, observed via `tcgetattr` on the pty master --
    /// `crates/factory-runner/src/lib.rs`'s `supervise_terminal`) --
    /// a kernel-level fact about the child's own terminal setup, not
    /// terminal-output inference (`ARCHITECTURE.md` invariant 5's
    /// Codex carve-out). A newly opened pty pair starts in canonical mode
    /// with echo on (`openpty`'s own default, `termp = NULL`); most
    /// interactive CLIs (including every real Codex/Claude release so
    /// far) switch to raw mode once during their own startup and never
    /// switch back, so this is logged as a lifecycle event exactly like
    /// [`Self::Started`] (once, non-terminal), not a stream of
    /// transitions. Additive to the wire since [`RUNNER_PROTOCOL_VERSION`]
    /// 1: an older reader that does not know this variant fails to parse
    /// only the one frame carrying it (`serde_json` rejects an unknown
    /// `type` for the whole enum), which is the same degraded-but-isolated
    /// outcome any future additive `RunnerEvent` variant already has under
    /// `docs/development/WORKFLOW.md`'s "runner control protocol must stay
    /// backward compatible within a major version" contract; every other
    /// frame on the same connection, including this session's own
    /// `Exited`, is unaffected.
    TerminalRaw,
    Exited {
        exit_code: Option<i32>,
        signal: Option<i32>,
    },
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OutputStream {
    Stdout,
    Stderr,
}
