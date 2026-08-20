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
/// Private runtime file whose kernel lock keeps a newly owned runner from
/// launching its provider before the daemon durably activates the session
/// principal.
pub const RUNNER_ACTIVATION_LOCK_FILE: &str = "activation.lock";
/// Maximum raw bytes accepted in one `TerminalInput` request, before base64
/// encoding. Larger paste operations must be split across requests.
pub const MAX_TERMINAL_INPUT_BYTES: usize = 64 * 1024;
/// Maximum raw PTY bytes read into one `TerminalOutput` chunk, before base64
/// encoding. Comfortably fits inside [`MAX_RUNNER_FRAME_BYTES`] once encoded.
pub const MAX_TERMINAL_OUTPUT_CHUNK_BYTES: usize = 16 * 1024;
/// Raw bytes sent by a default terminal attach. The runner streams this tail
/// in normal output frames; it never builds a replay-sized response.
pub const DEFAULT_TERMINAL_ATTACH_TAIL_BYTES: u64 = 256 * 1024;
/// Maximum raw bytes retained in one terminal log file before it is rotated.
///
/// The retained terminal log (`terminal.log`) is bounded and rotates exactly
/// once: when the active file reaches this size, it is renamed to
/// `terminal.log.1` (replacing any earlier rotation) and a fresh empty
/// `terminal.log` is opened. Up to two files' worth of raw PTY bytes are
/// retained at a time; older bytes are dropped for good.
pub const MAX_TERMINAL_LOG_BYTES: u64 = 64 * 1024 * 1024;

/// How a terminal attach chooses its retained replay window.
///
/// `Legacy` is the serde default so a new runner can still serve an older
/// daemon's `since_offset` request. New daemons always send an explicit mode.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", content = "data", rename_all = "snake_case")]
pub enum TerminalAttachMode {
    #[default]
    Legacy,
    Tail,
    FullHistory,
    Offset {
        generation: Option<u64>,
        offset: u64,
    },
}

impl TerminalAttachMode {
    #[must_use]
    pub const fn is_legacy(&self) -> bool {
        matches!(self, Self::Legacy)
    }
}

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

/// Returns whether a chunk can follow a validated terminal chunk. A live
/// stream may remain in the same retained generation or cross exactly one
/// rotation; a larger jump means bytes were silently skipped.
#[must_use]
pub const fn terminal_generation_is_contiguous(expected: u64, found: u64) -> bool {
    found >= expected && found <= expected.saturating_add(1)
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
    /// Attaches to a terminal-mode run's retained PTY output, using `mode`
    /// when supplied and then streaming live bytes. `since_offset` remains the
    /// legacy cursor for rolling upgrades. Explicit modes begin with
    /// `TerminalAttachReady` or return a structured `TerminalAttachGap`.
    AttachTerminal {
        since_offset: u64,
        #[serde(default, skip_serializing_if = "TerminalAttachMode::is_legacy")]
        mode: TerminalAttachMode,
    },
    /// Negotiates the retained-terminal attach contract before a daemon uses
    /// bounded replay or an offset/generation cursor. Older runners reject
    /// this request explicitly, so a new daemon never mistakes an unknown
    /// mode for an unbounded legacy replay.
    TerminalAttachCapabilities,
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
        /// Additive in the v1 wire: runners predating generation-aware
        /// attach omit this field. A new daemon only treats the value as
        /// authoritative after capability negotiation; legacy output keeps
        /// the historical single-generation cursor contract.
        #[serde(default)]
        generation: u64,
        offset: u64,
        bytes: String,
    },
    /// Retained-log bounds advertised before a bounded attach is attempted.
    TerminalAttachCapabilities {
        protocol_version: u16,
        generation: u64,
        base_generation: u64,
        base_offset: u64,
        end_offset: u64,
    },
    /// Metadata for an explicit attach contract. It precedes retained bytes.
    TerminalAttachReady {
        protocol_version: u16,
        generation: u64,
        base_generation: u64,
        base_offset: u64,
        start_generation: u64,
        start_offset: u64,
        end_offset: u64,
        /// Base64 reset-baseline prefix, not included in byte offsets. It does
        /// not claim to reconstruct application-specific terminal state.
        #[serde(default)]
        reset_prefix: String,
    },
    /// The requested cursor cannot be replayed from the retained generation.
    TerminalAttachGap {
        protocol_version: u16,
        generation: u64,
        base_generation: u64,
        base_offset: u64,
        start_generation: u64,
        start_offset: u64,
        end_offset: u64,
        requested_generation: Option<u64>,
        requested_offset: u64,
        reason: String,
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
            }
            | Self::TerminalAttachCapabilities {
                protocol_version, ..
            }
            | Self::TerminalAttachReady {
                protocol_version, ..
            }
            | Self::TerminalAttachGap {
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
    /// transitions.
    ///
    /// This was the first additive `RunnerEvent` variant since
    /// [`RUNNER_PROTOCOL_VERSION`] 1 shipped, and adding it here is what
    /// caught (verified against a scratch binary built on this exact
    /// `factory-core`) that an older reader did **not** degrade the way an
    /// earlier version of this doc comment claimed: with no catch-all
    /// variant, an unrecognized `type` failed `serde_json` deserialization
    /// for the *whole* `RunnerEventEnvelope`, which `read_frame`
    /// (`crates/factoryd/src/runner_client.rs`) turned into
    /// `RunnerClientError::InvalidJson` for that entire frame -- and
    /// because that error path returns before the exit-acknowledgement
    /// call (`wait_for_runner_exit`/`consume_until_exit`,
    /// `crates/factoryd/src/execution.rs`), it abandoned the control
    /// stream and orphaned the runner (issue #26's shape), not "just the
    /// one frame". `docs/development/WORKFLOW.md`'s "runner control
    /// protocol must stay backward compatible within a major version" is
    /// a real, exercised contract -- "daemon N spawns runner N+1" happens
    /// on every in-place update -- and it did not hold for this variant
    /// until [`Self::Unknown`] existed to catch it. It holds now, for
    /// this dataless variant, at the one-time cost of adding that
    /// catch-all before anything shipped -- [`Self::Unknown`]'s own doc
    /// comment has the important caveat: the same protection is not
    /// automatic for a future variant that carries `data`.
    TerminalRaw,
    /// The poll [`Self::TerminalRaw`]'s own doc comment describes gave up:
    /// the child's tty never left canonical mode within
    /// `factory-runner::RAW_MODE_POLL_TIMEOUT`. Logged once, non-terminal,
    /// exactly like [`Self::TerminalRaw`] itself -- the two are mutually
    /// exclusive outcomes of the same one-shot poll, never both for the
    /// same session. Adversarial review round 2 finding B: an earlier
    /// version of this poll gave up in total silence, so an operator
    /// watching a Codex session stuck `starting` had zero breadcrumbs.
    /// The daemon logs a `tracing::warn!` on this event for a Codex
    /// session (`docs/providers.md`) -- deliberately *not* a session state
    /// or `wait_reason` change: that stays reserved for `#52`'s own
    /// deadline (`SESSION_START_DEADLINE`, keyed off `state == starting`),
    /// which must keep seeing this session as `starting` to ever fire;
    /// changing state here would silently disable that backstop for
    /// exactly the sessions it exists to catch.
    TerminalRawTimedOut,
    Exited {
        exit_code: Option<i32>,
        signal: Option<i32>,
    },
    /// Catch-all for any `type` this build does not recognize (a future
    /// variant a newer runner sends to an older daemon -- the direction
    /// [`Self::TerminalRaw`]'s own doc comment above has the full story
    /// on). `#[serde(other)]` on an adjacently tagged (`tag`/`content`)
    /// enum deserializes an unrecognized `type` into this variant instead
    /// of failing the whole frame, *provided its `data` is absent or
    /// `null`* -- serde requires `#[serde(other)]` to sit on a unit
    /// variant, which can only ever deserialize from an empty/null
    /// payload, never an arbitrary one (verified: a scratch enum with the
    /// same `tag`/`content` shape parses `{"type":"x"}` and
    /// `{"type":"x","data":null}` into its `other` variant, but
    /// `{"type":"x","data":{"a":1}}` still fails outright, "invalid type:
    /// map, expected unit variant"). Every event added so far follows
    /// that shape -- [`Self::TerminalRaw`] itself has no `data` field at
    /// all -- but this is a real, narrower guarantee than "any future
    /// variant is forward-compatible": a future variant whose own payload
    /// carries data reproduces this exact bug for an older reader unless
    /// its own author widens this mechanism (e.g. a hand-written
    /// `Deserialize` impl dispatching through `serde_json::Value`) at the
    /// same time. Known variants are unaffected either way and still
    /// deserialize into themselves.
    ///
    /// Never constructed to be sent -- only ever produced by deserializing
    /// a frame this build's own enum does not have a name (or a matching
    /// shape) for. Every consumer's own match already treats "not
    /// `Exited`, not `TerminalRaw`" as an ordinary no-op
    /// (`wait_for_runner_exit`/`consume_until_exit`, which additionally
    /// log it at `debug` for observability), so this variant needs no
    /// special handling beyond existing at all.
    ///
    /// The reverse direction -- an *older* runner talking to a newer
    /// daemon -- needs no catch-all: an old runner simply never sends
    /// `TerminalRaw` (it does not know the event exists), so a Codex
    /// session it supervises never gets synthesized this way. The session
    /// stays `starting`, where `#52`'s durable session-start deadline can
    /// fail and retry it. `docs/providers.md`'s Codex `SessionStart`
    /// section describes both upgrade directions.
    #[serde(other)]
    Unknown,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OutputStream {
    Stdout,
    Stderr,
}
