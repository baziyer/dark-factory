//! Stable, provider-blind control and event wire for one runner process.

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
