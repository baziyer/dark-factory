//! Bounded authenticated client for one stable runner control socket.

use std::{fmt, io, path::Path, time::Duration};

use factory_core::{
    RunId, RunnerInstanceId,
    runner::{
        MAX_RUNNER_FRAME_BYTES, RUNNER_PROTOCOL_VERSION, RequestEnvelope, RunnerErrorCode,
        RunnerEvent, RunnerEventEnvelope, RunnerFrame, RunnerRequest, TerminalAttachMode,
    },
};
use thiserror::Error;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::UnixStream,
    time::timeout,
};
use uuid::Uuid;

const CONTROL_SOCKET_FILE: &str = "control.sock";
const CONTROL_TIMEOUT: Duration = Duration::from_secs(15);
const LEGACY_ATTACH_READY_TIMEOUT: Duration = Duration::from_millis(250);
const READ_CHUNK_BYTES: usize = 8 * 1024;
const MAX_STOP_GRACE_MS: u64 = 60_000;

/// Authenticated address of exactly one stable runner.
///
/// This type deliberately has no `Debug`: the random runner instance ID is a
/// private control-plane credential.
#[derive(Clone)]
pub struct RunnerClient {
    socket: std::path::PathBuf,
    run_id: RunId,
    runner_instance_id: RunnerInstanceId,
}

impl RunnerClient {
    #[must_use]
    pub fn new(
        runtime_dir: impl AsRef<Path>,
        run_id: RunId,
        runner_instance_id: RunnerInstanceId,
    ) -> Self {
        Self {
            socket: runtime_dir.as_ref().join(CONTROL_SOCKET_FILE),
            run_id,
            runner_instance_id,
        }
    }

    /// Opens a replay-plus-live subscription from sequence zero.
    ///
    /// There is intentionally no cursor parameter: daemon recovery must rebuild
    /// transient provider decoder state from the retained spool before using
    /// SQLite's committed cursor to suppress duplicate durable effects.
    pub async fn subscribe(&self) -> Result<RunnerSubscription, RunnerClientError> {
        let mut reader = self
            .request(RunnerRequest::Subscribe { after_sequence: 0 })
            .await?;
        let frame = read_control_frame(&mut reader, "runner hello").await?;
        validate_protocol(frame.protocol_version())?;
        let RunnerFrame::Hello {
            run_id,
            runner_instance_id,
            replay_through,
            terminal_sequence,
            ..
        } = frame
        else {
            return Err(frame_error(frame));
        };
        if run_id != self.run_id || runner_instance_id != self.runner_instance_id {
            return Err(RunnerClientError::WrongIdentity);
        }
        if replay_through < 0 {
            return Err(RunnerClientError::InvalidReplayHead {
                found: replay_through,
            });
        }
        if terminal_sequence.is_some_and(|sequence| sequence <= 0 || sequence != replay_through) {
            return Err(RunnerClientError::InvalidTerminalHead {
                replay_through,
                terminal_sequence,
            });
        }
        Ok(RunnerSubscription {
            reader,
            head: RunnerReplayHead {
                replay_through,
                terminal_sequence,
            },
            next_sequence: 1,
            caught_up: false,
            terminal_seen: false,
        })
    }

    /// Acknowledges the exact terminal boundary on a fresh control connection.
    pub async fn acknowledge_exit(&self, terminal_sequence: i64) -> Result<(), RunnerClientError> {
        if terminal_sequence <= 0 {
            return Err(RunnerClientError::InvalidTerminalSequence {
                found: terminal_sequence,
            });
        }
        let command_id = format!("ack-{terminal_sequence}");
        let mut reader = self
            .request(RunnerRequest::AcknowledgeExit {
                command_id: command_id.clone(),
                terminal_sequence,
            })
            .await?;
        let frame = read_control_frame(&mut reader, "terminal acknowledgement").await?;
        validate_protocol(frame.protocol_version())?;
        match frame {
            RunnerFrame::CommandAck {
                command_id: received,
                ..
            } if received == command_id => Ok(()),
            RunnerFrame::CommandAck { .. } => Err(RunnerClientError::CommandMismatch),
            RunnerFrame::Error { code, .. } => Err(RunnerClientError::RunnerRejected { code }),
            frame => Err(frame_error(frame)),
        }
    }

    /// Requests a graceful stop from the exact authenticated runner.
    ///
    /// The command ID is intentionally fresh per user action. The runner
    /// still deduplicates retries of the same wire command, while repeated UI
    /// clicks remain explicit actions instead of sharing hidden daemon state.
    pub async fn stop(&self, grace_ms: u64) -> Result<(), RunnerClientError> {
        if grace_ms > MAX_STOP_GRACE_MS {
            return Err(RunnerClientError::InvalidStopGrace { found: grace_ms });
        }
        let command_id = format!("stop-{}", Uuid::new_v4().simple());
        let mut reader = self
            .request(RunnerRequest::Stop {
                command_id: command_id.clone(),
                grace_ms,
            })
            .await?;
        let frame = read_control_frame(&mut reader, "runner stop").await?;
        validate_protocol(frame.protocol_version())?;
        match frame {
            RunnerFrame::CommandAck {
                command_id: received,
                ..
            } if received == command_id => Ok(()),
            RunnerFrame::CommandAck { .. } => Err(RunnerClientError::CommandMismatch),
            RunnerFrame::Error { code, .. } => Err(RunnerClientError::RunnerRejected { code }),
            frame => Err(frame_error(frame)),
        }
    }

    /// Opens a retained-then-live PTY output stream from a terminal-mode
    /// run, starting at `since_offset`. Rejected with
    /// [`RunnerErrorCode::InvalidRequest`] on a run that is not in terminal
    /// mode.
    pub async fn attach_terminal(
        &self,
        since_offset: u64,
        mode: TerminalAttachMode,
    ) -> Result<TerminalSubscription, RunnerClientError> {
        let mut reader = self
            .request(RunnerRequest::AttachTerminal { since_offset, mode })
            .await?;
        let (info, pending) = match timeout(LEGACY_ATTACH_READY_TIMEOUT, reader.read_frame()).await
        {
            Ok(result) => {
                let frame = result?;
                validate_protocol(frame.protocol_version())?;
                match frame {
                    RunnerFrame::TerminalOutput { offset, bytes, .. } => {
                        (None, Some((offset, bytes)))
                    }
                    RunnerFrame::TerminalAttachReady {
                        generation,
                        base_offset,
                        end_offset,
                        ..
                    } => (
                        Some(TerminalAttachInfo {
                            generation,
                            base_offset,
                            end_offset,
                        }),
                        None,
                    ),
                    RunnerFrame::TerminalAttachGap {
                        generation,
                        base_offset,
                        end_offset,
                        requested_generation,
                        requested_offset,
                        reason,
                        ..
                    } => {
                        return Err(RunnerClientError::TerminalAttachGap {
                            generation,
                            base_offset,
                            end_offset,
                            requested_generation,
                            requested_offset,
                            reason,
                        });
                    }
                    RunnerFrame::Error { code, .. } => {
                        return Err(RunnerClientError::RunnerRejected { code });
                    }
                    frame => return Err(frame_error(frame)),
                }
            }
            // v1 runners had no empty-replay acknowledgement. A silent surviving
            // runner is still a valid attachment; retain its reader for later output.
            Err(_) => (None, None),
        };
        Ok(TerminalSubscription {
            reader,
            info,
            pending,
        })
    }

    /// Writes operator input to a terminal-mode run's PTY. `bytes` is
    /// already [`factory_core::runner::encode_terminal_bytes`]-encoded;
    /// this is a pass-through so the daemon never needs to decode opaque
    /// terminal bytes.
    pub async fn terminal_input(&self, bytes: String) -> Result<(), RunnerClientError> {
        let mut reader = self.request(RunnerRequest::TerminalInput { bytes }).await?;
        let frame = read_control_frame(&mut reader, "terminal input").await?;
        validate_protocol(frame.protocol_version())?;
        match frame {
            RunnerFrame::CommandAck { .. } => Ok(()),
            RunnerFrame::Error { code, .. } => Err(RunnerClientError::RunnerRejected { code }),
            frame => Err(frame_error(frame)),
        }
    }

    /// Resizes a terminal-mode run's PTY.
    pub async fn resize_terminal(&self, cols: u16, rows: u16) -> Result<(), RunnerClientError> {
        let mut reader = self
            .request(RunnerRequest::ResizeTerminal { cols, rows })
            .await?;
        let frame = read_control_frame(&mut reader, "resize terminal").await?;
        validate_protocol(frame.protocol_version())?;
        match frame {
            RunnerFrame::CommandAck { .. } => Ok(()),
            RunnerFrame::Error { code, .. } => Err(RunnerClientError::RunnerRejected { code }),
            frame => Err(frame_error(frame)),
        }
    }

    async fn request(&self, request: RunnerRequest) -> Result<FrameReader, RunnerClientError> {
        let stream = timeout(CONTROL_TIMEOUT, UnixStream::connect(&self.socket))
            .await
            .map_err(|_| RunnerClientError::TimedOut {
                operation: "runner connection",
            })??;
        let mut reader = FrameReader::new(stream);
        let envelope = RequestEnvelope::new(
            self.run_id.clone(),
            self.runner_instance_id.clone(),
            request,
        );
        let mut payload =
            serde_json::to_vec(&envelope).map_err(|_| RunnerClientError::InvalidJson)?;
        if payload.len() > MAX_RUNNER_FRAME_BYTES {
            return Err(RunnerClientError::FrameTooLarge);
        }
        payload.push(b'\n');
        timeout(CONTROL_TIMEOUT, reader.stream.write_all(&payload))
            .await
            .map_err(|_| RunnerClientError::TimedOut {
                operation: "runner request",
            })??;
        timeout(CONTROL_TIMEOUT, reader.stream.flush())
            .await
            .map_err(|_| RunnerClientError::TimedOut {
                operation: "runner request",
            })??;
        Ok(reader)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RunnerReplayHead {
    pub replay_through: i64,
    pub terminal_sequence: Option<i64>,
}

#[derive(Eq, PartialEq)]
pub enum RunnerStreamItem {
    Event(RunnerEventEnvelope),
    CaughtUp { sequence: i64 },
}

impl fmt::Debug for RunnerStreamItem {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Event(event) => formatter
                .debug_struct("Event")
                .field("sequence", &event.sequence)
                .field("kind", &runner_event_kind(&event.event))
                .finish(),
            Self::CaughtUp { sequence } => formatter
                .debug_struct("CaughtUp")
                .field("sequence", sequence)
                .finish(),
        }
    }
}

/// Retained-then-live raw PTY output stream for one terminal-mode run.
///
/// `bytes` stays [`factory_core::runner::encode_terminal_bytes`]-encoded end
/// to end: the daemon proxy forwards it opaquely without decoding.
pub struct TerminalSubscription {
    reader: FrameReader,
    info: Option<TerminalAttachInfo>,
    pending: Option<(u64, String)>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TerminalAttachInfo {
    pub generation: u64,
    pub base_offset: u64,
    pub end_offset: u64,
}

impl TerminalSubscription {
    #[must_use]
    pub const fn info(&self) -> Option<TerminalAttachInfo> {
        self.info
    }

    /// Returns the next chunk's byte-stream offset and base64-encoded bytes.
    pub async fn next_chunk(&mut self) -> Result<(u64, String), RunnerClientError> {
        if let Some(chunk) = self.pending.take() {
            return Ok(chunk);
        }
        let frame = self.reader.read_frame().await?;
        validate_protocol(frame.protocol_version())?;
        match frame {
            RunnerFrame::TerminalOutput { offset, bytes, .. } => Ok((offset, bytes)),
            RunnerFrame::TerminalAttachReady { .. } => Err(RunnerClientError::UnexpectedFrame {
                expected: "terminal output",
            }),
            RunnerFrame::TerminalAttachGap {
                generation,
                base_offset,
                end_offset,
                requested_generation,
                requested_offset,
                reason,
                ..
            } => Err(RunnerClientError::TerminalAttachGap {
                generation,
                base_offset,
                end_offset,
                requested_generation,
                requested_offset,
                reason,
            }),
            RunnerFrame::Error { code, .. } => Err(RunnerClientError::RunnerRejected { code }),
            frame => Err(frame_error(frame)),
        }
    }
}

/// Replay-plus-live stream for one authenticated runner.
///
/// Its reader owns partial frame bytes so cancelling `next_item` cannot discard
/// data already consumed from the socket.
pub struct RunnerSubscription {
    reader: FrameReader,
    head: RunnerReplayHead,
    next_sequence: i64,
    caught_up: bool,
    terminal_seen: bool,
}

impl fmt::Debug for RunnerSubscription {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RunnerSubscription")
            .field("head", &self.head)
            .field("next_sequence", &self.next_sequence)
            .field("caught_up", &self.caught_up)
            .field("terminal_seen", &self.terminal_seen)
            .finish_non_exhaustive()
    }
}

impl RunnerSubscription {
    #[must_use]
    pub const fn head(&self) -> RunnerReplayHead {
        self.head
    }

    /// Returns the next validated replay boundary or durable runner event.
    pub async fn next_item(&mut self) -> Result<RunnerStreamItem, RunnerClientError> {
        let frame = self.reader.read_frame().await?;
        validate_protocol(frame.protocol_version())?;
        match frame {
            RunnerFrame::Event {
                protocol_version: _,
                event,
            } => self.accept_event(event),
            RunnerFrame::CaughtUp { sequence, .. } if !self.caught_up => {
                let expected_next = self.head.replay_through.checked_add(1).ok_or(
                    RunnerClientError::InvalidReplayHead {
                        found: self.head.replay_through,
                    },
                )?;
                if sequence != self.head.replay_through || self.next_sequence != expected_next {
                    return Err(RunnerClientError::CaughtUpMismatch {
                        expected: self.head.replay_through,
                        found: sequence,
                    });
                }
                if self.head.terminal_sequence.is_some() && !self.terminal_seen {
                    return Err(RunnerClientError::TerminalMismatch);
                }
                self.caught_up = true;
                Ok(RunnerStreamItem::CaughtUp { sequence })
            }
            RunnerFrame::Error { code, .. } => Err(RunnerClientError::RunnerRejected { code }),
            frame => Err(frame_error(frame)),
        }
    }

    fn accept_event(
        &mut self,
        event: RunnerEventEnvelope,
    ) -> Result<RunnerStreamItem, RunnerClientError> {
        validate_protocol(event.protocol_version)?;
        if self.terminal_seen {
            return Err(RunnerClientError::EventAfterTerminal);
        }
        if event.sequence != self.next_sequence {
            return Err(RunnerClientError::SequenceMismatch {
                expected: self.next_sequence,
                found: event.sequence,
            });
        }
        if !self.caught_up && event.sequence > self.head.replay_through {
            return Err(RunnerClientError::ReplayBoundary {
                replay_through: self.head.replay_through,
                found: event.sequence,
            });
        }

        let is_terminal = is_terminal(&event.event);
        if !self.caught_up {
            match (is_terminal, self.head.terminal_sequence) {
                (true, Some(expected)) if event.sequence == expected => {}
                (true, _) => return Err(RunnerClientError::TerminalMismatch),
                (false, Some(expected)) if event.sequence == expected => {
                    return Err(RunnerClientError::TerminalMismatch);
                }
                _ => {}
            }
        }
        self.terminal_seen = is_terminal;
        self.next_sequence =
            event
                .sequence
                .checked_add(1)
                .ok_or(RunnerClientError::SequenceMismatch {
                    expected: event.sequence,
                    found: event.sequence,
                })?;
        Ok(RunnerStreamItem::Event(event))
    }
}

fn is_terminal(event: &RunnerEvent) -> bool {
    matches!(
        event,
        RunnerEvent::SpawnFailed { .. } | RunnerEvent::Exited { .. }
    )
}

fn runner_event_kind(event: &RunnerEvent) -> &'static str {
    match event {
        RunnerEvent::Started { .. } => "started",
        RunnerEvent::Output { .. } => "output",
        RunnerEvent::SpawnFailed { .. } => "spawn_failed",
        RunnerEvent::OutputTruncated { .. } => "output_truncated",
        RunnerEvent::TerminalRaw => "terminal_raw",
        RunnerEvent::TerminalRawTimedOut => "terminal_raw_timed_out",
        RunnerEvent::Exited { .. } => "exited",
        RunnerEvent::Unknown => "unknown",
    }
}

fn validate_protocol(found: u16) -> Result<(), RunnerClientError> {
    if found == RUNNER_PROTOCOL_VERSION {
        Ok(())
    } else {
        Err(RunnerClientError::WrongProtocol {
            found,
            supported: RUNNER_PROTOCOL_VERSION,
        })
    }
}

fn frame_error(frame: RunnerFrame) -> RunnerClientError {
    match frame {
        RunnerFrame::Error { code, .. } => RunnerClientError::RunnerRejected { code },
        RunnerFrame::Hello { .. } => RunnerClientError::UnexpectedFrame {
            expected: "runner event",
        },
        RunnerFrame::Event { .. } => RunnerClientError::UnexpectedFrame {
            expected: "runner hello or acknowledgement",
        },
        RunnerFrame::CaughtUp { .. } => RunnerClientError::UnexpectedFrame {
            expected: "runner event",
        },
        RunnerFrame::CommandAck { .. } => RunnerClientError::UnexpectedFrame {
            expected: "runner event",
        },
        RunnerFrame::TerminalOutput { .. } => RunnerClientError::UnexpectedFrame {
            expected: "runner command acknowledgement",
        },
        RunnerFrame::TerminalAttachReady { .. } | RunnerFrame::TerminalAttachGap { .. } => {
            RunnerClientError::UnexpectedFrame {
                expected: "runner command acknowledgement",
            }
        }
    }
}

async fn read_control_frame(
    reader: &mut FrameReader,
    operation: &'static str,
) -> Result<RunnerFrame, RunnerClientError> {
    timeout(CONTROL_TIMEOUT, reader.read_frame())
        .await
        .map_err(|_| RunnerClientError::TimedOut { operation })?
}

struct FrameReader {
    stream: UnixStream,
    buffered: Vec<u8>,
}

impl FrameReader {
    fn new(stream: UnixStream) -> Self {
        Self {
            stream,
            buffered: Vec::new(),
        }
    }

    async fn read_frame(&mut self) -> Result<RunnerFrame, RunnerClientError> {
        loop {
            if let Some(newline) = self.buffered.iter().position(|byte| *byte == b'\n') {
                if newline > MAX_RUNNER_FRAME_BYTES {
                    return Err(RunnerClientError::FrameTooLarge);
                }
                let mut payload = self.buffered.drain(..=newline).collect::<Vec<_>>();
                payload.pop();
                if payload.last() == Some(&b'\r') {
                    payload.pop();
                }
                return serde_json::from_slice(&payload)
                    .map_err(|_| RunnerClientError::InvalidJson);
            }
            if self.buffered.len() > MAX_RUNNER_FRAME_BYTES {
                return Err(RunnerClientError::FrameTooLarge);
            }

            let remaining = MAX_RUNNER_FRAME_BYTES + 1 - self.buffered.len();
            let mut chunk = [0_u8; READ_CHUNK_BYTES];
            let read = self
                .stream
                .read(&mut chunk[..remaining.min(READ_CHUNK_BYTES)])
                .await?;
            if read == 0 {
                return if self.buffered.is_empty() {
                    Err(RunnerClientError::UnexpectedEof)
                } else {
                    Err(RunnerClientError::UnterminatedFrame)
                };
            }
            self.buffered.extend_from_slice(&chunk[..read]);
        }
    }
}

#[derive(Debug, Error)]
pub enum RunnerClientError {
    #[error("runner I/O failed: {0}")]
    Io(#[from] io::Error),
    #[error("runner frame is not valid protocol JSON")]
    InvalidJson,
    #[error("runner frame exceeds the 1 MiB limit")]
    FrameTooLarge,
    #[error("runner frame ended before its newline delimiter")]
    UnterminatedFrame,
    #[error("runner closed before sending the required frame")]
    UnexpectedEof,
    #[error("runner protocol {found} is unsupported; expected {supported}")]
    WrongProtocol { found: u16, supported: u16 },
    #[error("runner identity does not match the reserved run")]
    WrongIdentity,
    #[error("runner replay head {found} is invalid")]
    InvalidReplayHead { found: i64 },
    #[error(
        "runner terminal boundary {terminal_sequence:?} does not match replay head {replay_through}"
    )]
    InvalidTerminalHead {
        replay_through: i64,
        terminal_sequence: Option<i64>,
    },
    #[error("runner event sequence mismatch: expected {expected}, found {found}")]
    SequenceMismatch { expected: i64, found: i64 },
    #[error("runner event {found} crossed replay head {replay_through} before caught-up")]
    ReplayBoundary { replay_through: i64, found: i64 },
    #[error("runner caught-up boundary mismatch: expected {expected}, found {found}")]
    CaughtUpMismatch { expected: i64, found: i64 },
    #[error("runner terminal metadata does not match its durable event stream")]
    TerminalMismatch,
    #[error("runner emitted an event after its terminal boundary")]
    EventAfterTerminal,
    #[error("runner sent an unexpected frame while awaiting {expected}")]
    UnexpectedFrame { expected: &'static str },
    #[error("runner rejected the request with {code:?}")]
    RunnerRejected { code: RunnerErrorCode },
    #[error(
        "terminal attach cursor is unavailable ({reason}); retained generation {generation}, offsets {base_offset}..{end_offset}"
    )]
    TerminalAttachGap {
        generation: u64,
        base_offset: u64,
        end_offset: u64,
        requested_generation: Option<u64>,
        requested_offset: u64,
        reason: String,
    },
    #[error("runner terminal sequence {found} must be positive")]
    InvalidTerminalSequence { found: i64 },
    #[error("runner acknowledged a different command")]
    CommandMismatch,
    #[error("runner stop grace {found} ms exceeds the 60 second limit")]
    InvalidStopGrace { found: u64 },
    #[error("timed out while waiting for {operation}")]
    TimedOut { operation: &'static str },
}
