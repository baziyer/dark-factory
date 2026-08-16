//! Bounded adapter for the Codex 0.147 JSONL protocol.

use std::{collections::HashSet, ffi::OsString, path::PathBuf};

use factory_core::{
    RunId, RunnerInstanceId,
    runner::{OutputStream, RunnerEvent},
};
use serde_json::Value;
use uuid::Uuid;

use crate::runner_process::{LaunchSpec, ProviderEnvironment};

pub const MAX_CODEX_JSON_LINE_BYTES: usize = 1024 * 1024;
pub const MAX_CODEX_PREVIEW_BYTES: usize = 4 * 1024;
const MAX_CODEX_ITEM_ID_BYTES: usize = 256;
const MAX_TRACKED_ITEM_EVENTS: usize = 4096;

/// One Codex invocation. This has no `Debug` or `Clone` implementation because
/// it owns the task instructions.
pub struct CodexLaunch {
    pub runner_program: PathBuf,
    pub codex_program: PathBuf,
    pub run_id: RunId,
    pub runner_instance_id: RunnerInstanceId,
    pub runtime_dir: PathBuf,
    pub cwd: PathBuf,
    /// An exact imported Codex home, or `None` for the existing sanitized
    /// `HOME` default used by newly allocated agents.
    pub codex_home: Option<PathBuf>,
    pub instructions: String,
    pub session: Session,
}

pub enum Session {
    New,
    Resume { thread_id: String },
}

#[derive(Debug, thiserror::Error)]
#[error("Codex thread ID must be a canonical UUID")]
pub struct InvalidThreadId;

/// A launch request bound to the decoder that will validate its session.
///
/// This has no `Debug` or `Clone` implementation because the launch request
/// owns task instructions.
pub struct PreparedCodex {
    pub launch_spec: LaunchSpec,
    pub decoder: Decoder,
}

/// Prepares one stable-runner launch and its matching Codex stream decoder.
///
/// Instructions are returned only as bounded runner stdin. Provider arguments
/// are fixed, non-secret process metadata; the caller cannot inject flags.
/// User configuration is disabled while authentication and session state may
/// still use an explicit Codex home.
///
/// # Errors
///
/// Returns [`InvalidThreadId`] when a resume target is not the canonical UUID
/// previously emitted by Codex.
pub fn prepare(input: CodexLaunch) -> Result<PreparedCodex, InvalidThreadId> {
    let mut arguments = [
        "exec",
        "--json",
        "--color",
        "never",
        "--sandbox",
        "workspace-write",
        "-c",
        "approval_policy=\"never\"",
        "--ignore-user-config",
    ]
    .map(OsString::from)
    .to_vec();

    let decoder = match input.session {
        Session::New => Decoder::fresh(),
        Session::Resume { thread_id } => {
            let decoder = Decoder::resume(thread_id.clone())?;
            arguments.push(OsString::from("resume"));
            arguments.push(OsString::from(&thread_id));
            decoder
        }
    };
    arguments.push(OsString::from("-"));

    Ok(PreparedCodex {
        launch_spec: LaunchSpec {
            runner_program: input.runner_program,
            provider_program: input.codex_program,
            provider_arguments: arguments,
            provider_environment: input.codex_home.map_or(
                ProviderEnvironment::Inherited,
                ProviderEnvironment::CodexHome,
            ),
            run_id: input.run_id,
            runner_instance_id: input.runner_instance_id,
            runtime_dir: input.runtime_dir,
            cwd: input.cwd,
            startup_input: input.instructions.into_bytes(),
        },
        decoder,
    })
}

fn validate_thread_id(value: &str) -> Result<(), InvalidThreadId> {
    let parsed = Uuid::parse_str(value).map_err(|_| InvalidThreadId)?;
    if parsed.hyphenated().to_string() == value {
        Ok(())
    } else {
        Err(InvalidThreadId)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BoundedText {
    pub text: String,
    pub truncated: bool,
}

impl BoundedText {
    fn new(value: &str) -> Self {
        let mut text: String = value
            .chars()
            .filter(|character| !character.is_control() || matches!(character, '\n' | '\t'))
            .collect();
        let truncated = text.len() > MAX_CODEX_PREVIEW_BYTES;
        if truncated {
            let mut boundary = MAX_CODEX_PREVIEW_BYTES;
            while !text.is_char_boundary(boundary) {
                boundary -= 1;
            }
            text.truncate(boundary);
        }
        Self { text, truncated }
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ItemKind {
    AgentMessage,
    Reasoning,
    Command,
    FileChange,
    McpTool,
    WebSearch,
    TodoList,
    Collaboration,
    Error,
    Other,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ItemPhase {
    Started,
    Updated,
    Completed,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ItemResult {
    Succeeded,
    Failed,
    Unknown,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TokenUsage {
    pub input_tokens: u64,
    pub cached_input_tokens: u64,
    pub cache_write_input_tokens: u64,
    pub output_tokens: u64,
    pub reasoning_output_tokens: u64,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ProtocolViolation {
    MalformedJson,
    LineTooLong,
    LossyStdout,
    OutputTruncated,
    InvalidThreadId,
    ConflictingThreadId,
    InvalidItemId,
    InvalidUsage,
    ConflictingTerminal,
    TooManyItems,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Observation {
    ThreadStarted {
        thread_id: String,
    },
    TurnStarted,
    ItemChanged {
        id: String,
        kind: ItemKind,
        phase: ItemPhase,
        result: Option<ItemResult>,
        preview: Option<BoundedText>,
    },
    TurnCompleted {
        usage: TokenUsage,
    },
    TurnFailed,
    Error,
    /// Stderr is deliberately represented by metadata, never provider text.
    Diagnostic {
        bytes: usize,
        lossy: bool,
    },
    ProtocolViolation {
        kind: ProtocolViolation,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FailureReason {
    Protocol,
    Provider,
    Process,
    Spawn,
    Incomplete,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Outcome {
    Succeeded {
        thread_id: String,
        usage: TokenUsage,
    },
    Failed {
        thread_id: Option<String>,
        reason: FailureReason,
    },
}

impl Outcome {
    #[must_use]
    pub const fn failure_reason(&self) -> Option<FailureReason> {
        match self {
            Self::Succeeded { .. } => None,
            Self::Failed { reason, .. } => Some(*reason),
        }
    }

    #[must_use]
    pub fn thread_id(&self) -> Option<&str> {
        match self {
            Self::Succeeded { thread_id, .. } => Some(thread_id),
            Self::Failed { thread_id, .. } => thread_id.as_deref(),
        }
    }
}

#[derive(Debug, Eq, PartialEq)]
enum RunnerTerminal {
    SpawnFailed,
    Exited {
        exit_code: Option<i32>,
        signal: Option<i32>,
    },
}

pub struct Finished {
    pub observations: Vec<Observation>,
    pub outcome: Outcome,
    pub final_preview: Option<BoundedText>,
}

/// Incremental, bounded normalizer for one Codex attempt.
///
/// This type deliberately has no `Debug` or `Clone` implementation: it may
/// temporarily own an incomplete provider JSON line.
pub struct Decoder {
    buffer: Vec<u8>,
    discard_until_newline: bool,
    integrity_failed: bool,
    expected_thread_id: Option<String>,
    thread_id: Option<String>,
    turn_started: bool,
    completed_usage: Option<TokenUsage>,
    provider_failed: bool,
    final_preview: Option<BoundedText>,
    terminal: Option<RunnerTerminal>,
    item_events: HashSet<(String, ItemPhase)>,
    violations: HashSet<ProtocolViolation>,
}

impl Decoder {
    /// Creates a decoder for a fresh Codex thread.
    ///
    /// Recovery uses this constructor even after the fresh attempt has
    /// confirmed a thread ID: replay from runner sequence zero rebuilds and
    /// revalidates that identity from the retained provider stream and the
    /// store's durable session ownership.
    #[must_use]
    pub fn fresh() -> Self {
        Self::new(None)
    }

    /// Creates a decoder bound to one previously confirmed Codex thread.
    ///
    /// # Errors
    ///
    /// Returns [`InvalidThreadId`] unless `thread_id` is a canonical UUID.
    pub fn resume(thread_id: String) -> Result<Self, InvalidThreadId> {
        validate_thread_id(&thread_id)?;
        Ok(Self::new(Some(thread_id)))
    }

    fn new(expected_thread_id: Option<String>) -> Self {
        Self {
            buffer: Vec::new(),
            discard_until_newline: false,
            integrity_failed: false,
            expected_thread_id,
            thread_id: None,
            turn_started: false,
            completed_usage: None,
            provider_failed: false,
            final_preview: None,
            terminal: None,
            item_events: HashSet::new(),
            violations: HashSet::new(),
        }
    }

    /// Consumes one durable runner event and returns privacy-safe observations.
    #[must_use]
    pub fn push(&mut self, event: &RunnerEvent) -> Vec<Observation> {
        match event {
            RunnerEvent::Started { .. } => Vec::new(),
            RunnerEvent::Output {
                stream: OutputStream::Stdout,
                text,
                lossy,
            } => self.push_stdout(text.as_bytes(), *lossy),
            RunnerEvent::Output {
                stream: OutputStream::Stderr,
                text,
                lossy,
            } => vec![Observation::Diagnostic {
                bytes: text.len(),
                lossy: *lossy,
            }],
            RunnerEvent::OutputTruncated { .. } => {
                self.record_violation(ProtocolViolation::OutputTruncated)
            }
            RunnerEvent::SpawnFailed { .. } => self.set_terminal(RunnerTerminal::SpawnFailed),
            RunnerEvent::Exited { exit_code, signal } => {
                self.set_terminal(RunnerTerminal::Exited {
                    exit_code: *exit_code,
                    signal: *signal,
                })
            }
        }
    }

    #[must_use]
    pub fn finish(mut self) -> Finished {
        let mut observations = Vec::new();
        if !self.discard_until_newline && !self.buffer.is_empty() {
            let line = std::mem::take(&mut self.buffer);
            observations.extend(self.parse_line(&line));
        }

        let thread_id = self.thread_id.clone();
        let outcome = if self.integrity_failed {
            Outcome::Failed {
                thread_id,
                reason: FailureReason::Protocol,
            }
        } else if self.provider_failed {
            Outcome::Failed {
                thread_id,
                reason: FailureReason::Provider,
            }
        } else {
            match self.terminal {
                Some(RunnerTerminal::SpawnFailed) => Outcome::Failed {
                    thread_id,
                    reason: FailureReason::Spawn,
                },
                Some(RunnerTerminal::Exited {
                    exit_code: Some(0),
                    signal: None,
                }) => match (thread_id, self.completed_usage) {
                    (Some(thread_id), Some(usage)) => Outcome::Succeeded { thread_id, usage },
                    (thread_id, None) | (thread_id @ None, Some(_)) => Outcome::Failed {
                        thread_id,
                        reason: FailureReason::Incomplete,
                    },
                },
                Some(RunnerTerminal::Exited { .. }) => Outcome::Failed {
                    thread_id,
                    reason: FailureReason::Process,
                },
                None => Outcome::Failed {
                    thread_id,
                    reason: FailureReason::Incomplete,
                },
            }
        };

        Finished {
            observations,
            outcome,
            final_preview: self.final_preview,
        }
    }

    fn push_stdout(&mut self, bytes: &[u8], lossy: bool) -> Vec<Observation> {
        if lossy {
            self.buffer.clear();
            self.discard_until_newline = !bytes.ends_with(b"\n");
            return self.record_violation(ProtocolViolation::LossyStdout);
        }

        let mut observations = Vec::new();
        let mut remaining = bytes;
        while let Some(newline) = remaining.iter().position(|byte| *byte == b'\n') {
            let (segment, after_segment) = remaining.split_at(newline);
            remaining = &after_segment[1..];
            if self.discard_until_newline {
                self.discard_until_newline = false;
                self.buffer.clear();
                continue;
            }
            if self.buffer.len().saturating_add(segment.len()) > MAX_CODEX_JSON_LINE_BYTES {
                self.buffer.clear();
                observations.extend(self.record_violation(ProtocolViolation::LineTooLong));
                continue;
            }
            self.buffer.extend_from_slice(segment);
            let mut line = std::mem::take(&mut self.buffer);
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            observations.extend(self.parse_line(&line));
        }

        if !self.discard_until_newline {
            if self.buffer.len().saturating_add(remaining.len()) > MAX_CODEX_JSON_LINE_BYTES {
                self.buffer.clear();
                self.discard_until_newline = true;
                observations.extend(self.record_violation(ProtocolViolation::LineTooLong));
            } else {
                self.buffer.extend_from_slice(remaining);
            }
        }
        observations
    }

    fn parse_line(&mut self, line: &[u8]) -> Vec<Observation> {
        if line.iter().all(u8::is_ascii_whitespace) {
            return Vec::new();
        }
        let value: Value = match serde_json::from_slice(line) {
            Ok(value) => value,
            Err(_) => return self.record_violation(ProtocolViolation::MalformedJson),
        };
        let Some(event_type) = value.get("type").and_then(Value::as_str) else {
            return self.record_violation(ProtocolViolation::MalformedJson);
        };

        match event_type {
            "thread.started" => self.parse_thread_started(&value),
            "turn.started" => {
                if self.turn_started {
                    Vec::new()
                } else {
                    self.turn_started = true;
                    vec![Observation::TurnStarted]
                }
            }
            "item.started" => self.parse_item(&value, ItemPhase::Started),
            "item.updated" => self.parse_item(&value, ItemPhase::Updated),
            "item.completed" => self.parse_item(&value, ItemPhase::Completed),
            "turn.completed" => self.parse_turn_completed(&value),
            "turn.failed" => {
                self.provider_failed = true;
                let mut observations = vec![Observation::TurnFailed];
                if self.completed_usage.is_some() {
                    observations
                        .extend(self.record_violation(ProtocolViolation::ConflictingTerminal));
                }
                observations
            }
            "error" => {
                self.provider_failed = true;
                let mut observations = vec![Observation::Error];
                if self.completed_usage.is_some() {
                    observations
                        .extend(self.record_violation(ProtocolViolation::ConflictingTerminal));
                }
                observations
            }
            _ => Vec::new(),
        }
    }

    fn parse_thread_started(&mut self, value: &Value) -> Vec<Observation> {
        let Some(thread_id) = value.get("thread_id").and_then(Value::as_str) else {
            return self.record_violation(ProtocolViolation::InvalidThreadId);
        };
        if validate_thread_id(thread_id).is_err() {
            return self.record_violation(ProtocolViolation::InvalidThreadId);
        }
        if self
            .expected_thread_id
            .as_deref()
            .is_some_and(|expected| expected != thread_id)
        {
            return self.record_violation(ProtocolViolation::ConflictingThreadId);
        }
        if let Some(existing) = &self.thread_id {
            return if existing == thread_id {
                Vec::new()
            } else {
                self.record_violation(ProtocolViolation::ConflictingThreadId)
            };
        }

        self.thread_id = Some(thread_id.to_owned());
        vec![Observation::ThreadStarted {
            thread_id: thread_id.to_owned(),
        }]
    }

    fn parse_item(&mut self, value: &Value, phase: ItemPhase) -> Vec<Observation> {
        let Some(item) = value.get("item") else {
            return self.record_violation(ProtocolViolation::MalformedJson);
        };
        let Some(id) = item.get("id").and_then(Value::as_str) else {
            return self.record_violation(ProtocolViolation::InvalidItemId);
        };
        if !valid_item_id(id) {
            return self.record_violation(ProtocolViolation::InvalidItemId);
        }
        let kind = item
            .get("type")
            .and_then(Value::as_str)
            .map(item_kind)
            .unwrap_or(ItemKind::Other);
        let key = (id.to_owned(), phase);
        if self.item_events.contains(&key) {
            return Vec::new();
        }
        if self.item_events.len() >= MAX_TRACKED_ITEM_EVENTS {
            return self.record_violation(ProtocolViolation::TooManyItems);
        }
        self.item_events.insert(key);

        let result = if phase == ItemPhase::Completed {
            Some(if kind == ItemKind::Error {
                ItemResult::Failed
            } else {
                match item.get("status").and_then(Value::as_str) {
                    Some("completed") => ItemResult::Succeeded,
                    Some("failed") => ItemResult::Failed,
                    _ => ItemResult::Unknown,
                }
            })
        } else {
            None
        };
        let preview = if kind == ItemKind::AgentMessage && phase == ItemPhase::Completed {
            item.get("text")
                .and_then(Value::as_str)
                .map(BoundedText::new)
        } else if kind == ItemKind::Error && phase == ItemPhase::Completed {
            Some(BoundedText::new("Codex item failed"))
        } else {
            None
        };
        if kind == ItemKind::AgentMessage && phase == ItemPhase::Completed {
            self.final_preview = preview.clone();
        }
        vec![Observation::ItemChanged {
            id: id.to_owned(),
            kind,
            phase,
            result,
            preview,
        }]
    }

    fn parse_turn_completed(&mut self, value: &Value) -> Vec<Observation> {
        let Some(usage) = parse_usage(value.get("usage")) else {
            return self.record_violation(ProtocolViolation::InvalidUsage);
        };
        if let Some(existing) = self.completed_usage {
            return if existing == usage {
                Vec::new()
            } else {
                self.record_violation(ProtocolViolation::ConflictingTerminal)
            };
        }
        if self.provider_failed {
            return self.record_violation(ProtocolViolation::ConflictingTerminal);
        }
        self.completed_usage = Some(usage);
        vec![Observation::TurnCompleted { usage }]
    }

    fn set_terminal(&mut self, terminal: RunnerTerminal) -> Vec<Observation> {
        match &self.terminal {
            None => {
                self.terminal = Some(terminal);
                Vec::new()
            }
            Some(existing) if *existing == terminal => Vec::new(),
            Some(_) => self.record_violation(ProtocolViolation::ConflictingTerminal),
        }
    }

    fn record_violation(&mut self, kind: ProtocolViolation) -> Vec<Observation> {
        self.integrity_failed = true;
        if self.violations.insert(kind) {
            vec![Observation::ProtocolViolation { kind }]
        } else {
            Vec::new()
        }
    }
}

fn valid_item_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_CODEX_ITEM_ID_BYTES
        && value.bytes().all(|byte| byte.is_ascii_graphic())
}

fn item_kind(value: &str) -> ItemKind {
    match value {
        "agent_message" => ItemKind::AgentMessage,
        "reasoning" => ItemKind::Reasoning,
        "command_execution" => ItemKind::Command,
        "file_change" => ItemKind::FileChange,
        "mcp_tool_call" => ItemKind::McpTool,
        "web_search" => ItemKind::WebSearch,
        "todo_list" => ItemKind::TodoList,
        "collab_tool_call" => ItemKind::Collaboration,
        "error" => ItemKind::Error,
        _ => ItemKind::Other,
    }
}

fn parse_usage(value: Option<&Value>) -> Option<TokenUsage> {
    let value = value?;
    Some(TokenUsage {
        input_tokens: value.get("input_tokens")?.as_u64()?,
        cached_input_tokens: value.get("cached_input_tokens")?.as_u64()?,
        cache_write_input_tokens: value.get("cache_write_input_tokens")?.as_u64()?,
        output_tokens: value.get("output_tokens")?.as_u64()?,
        reasoning_output_tokens: value.get("reasoning_output_tokens")?.as_u64()?,
    })
}
