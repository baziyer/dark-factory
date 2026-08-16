//! Bounded adapter for the Claude Code 2.1.233 stream-JSON protocol.

use std::{
    collections::{HashMap, HashSet},
    ffi::OsString,
    num::NonZeroU32,
    path::PathBuf,
};

use factory_core::{
    RunId, RunnerInstanceId,
    runner::{OutputStream, RunnerEvent},
};
use serde_json::Value;
use uuid::Uuid;

use crate::runner_process::{LaunchSpec, ProviderEnvironment};

pub const MAX_CLAUDE_JSON_LINE_BYTES: usize = 1024 * 1024;
pub const MAX_CLAUDE_PREVIEW_BYTES: usize = 4 * 1024;
const MAX_LABEL_BYTES: usize = 512;
const MAX_TRACKED_TOOLS: usize = 4096;
const MAX_TOKEN_TOTAL: u64 = 1_000_000_000_000;
const MAX_COST_MICROUSD: u64 = 1_000_000_000_000_000;

/// One Claude Code invocation. It deliberately has no `Debug` or `Clone`
/// implementation because it owns task instructions.
pub struct ClaudeLaunch {
    pub runner_program: PathBuf,
    pub claude_program: PathBuf,
    pub run_id: RunId,
    pub runner_instance_id: RunnerInstanceId,
    pub runtime_dir: PathBuf,
    pub cwd: PathBuf,
    pub instructions: String,
    pub session: Session,
    pub max_turns: NonZeroU32,
    pub max_budget_cents: NonZeroU32,
}

pub enum Session {
    New { session_id: String },
    Resume { session_id: String },
}

#[derive(Debug, thiserror::Error)]
pub enum PrepareError {
    #[error("Claude session ID must be a canonical UUID")]
    InvalidSessionId,
}

/// A launch request bound to the only decoder allowed to validate its session.
/// It deliberately has no `Debug` or `Clone` implementation.
pub struct PreparedClaude {
    pub launch_spec: LaunchSpec,
    pub decoder: Decoder,
}

impl PreparedClaude {
    /// Returns the canonical session identity shared by this launch and decoder.
    #[must_use]
    pub fn session_id(&self) -> &str {
        &self.decoder.expected_session_id
    }
}

/// Builds fixed, non-interactive Claude arguments and exact task stdin bytes.
///
/// Task content is present only in `launch_spec.startup_input`. The returned
/// decoder is bound to the caller-allocated fresh or resumed session identity.
pub fn prepare(input: ClaudeLaunch) -> Result<PreparedClaude, PrepareError> {
    let (session_id, resume) = match input.session {
        Session::New { session_id } => (session_id, false),
        Session::Resume { session_id } => (session_id, true),
    };
    let decoder = if resume {
        Decoder::resume(session_id.clone())?
    } else {
        Decoder::fresh(session_id.clone())?
    };
    let mut arguments = [
        "-p",
        "--input-format",
        "text",
        "--output-format",
        "stream-json",
        "--verbose",
        "--prompt-suggestions",
        "false",
        "--permission-mode",
        "acceptEdits",
        "--max-turns",
    ]
    .map(OsString::from)
    .to_vec();
    arguments.push(OsString::from(input.max_turns.get().to_string()));
    arguments.push(OsString::from("--max-budget-usd"));
    arguments.push(OsString::from(format_budget(input.max_budget_cents)));
    arguments.push(OsString::from("--no-chrome"));
    arguments.push(OsString::from("--safe-mode"));
    arguments.push(OsString::from(if resume {
        "--resume"
    } else {
        "--session-id"
    }));
    arguments.push(OsString::from(&session_id));

    Ok(PreparedClaude {
        launch_spec: LaunchSpec {
            runner_program: input.runner_program,
            provider_program: input.claude_program,
            provider_arguments: arguments,
            provider_environment: ProviderEnvironment::Inherited,
            run_id: input.run_id,
            runner_instance_id: input.runner_instance_id,
            runtime_dir: input.runtime_dir,
            cwd: input.cwd,
            startup_input: input.instructions.into_bytes(),
        },
        decoder,
    })
}

fn validate_uuid(value: &str) -> Result<(), PrepareError> {
    let parsed = Uuid::parse_str(value).map_err(|_| PrepareError::InvalidSessionId)?;
    if parsed.hyphenated().to_string() == value {
        Ok(())
    } else {
        Err(PrepareError::InvalidSessionId)
    }
}

fn format_budget(cents: NonZeroU32) -> String {
    let cents = cents.get();
    format!("{}.{:02}", cents / 100, cents % 100)
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BoundedText {
    pub text: String,
    pub truncated: bool,
}

impl BoundedText {
    fn new(value: &str, limit: usize) -> Self {
        let mut text: String = value
            .chars()
            .filter(|character| !character.is_control() || matches!(character, '\n' | '\t'))
            .collect();
        let truncated = text.len() > limit;
        if truncated {
            let mut boundary = limit;
            while !text.is_char_boundary(boundary) {
                boundary -= 1;
            }
            text.truncate(boundary);
        }
        Self { text, truncated }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PermissionMode {
    AcceptEdits,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ToolKind {
    Command,
    Filesystem,
    Subagent,
    Web,
    Todo,
    Mcp,
    UserInput,
    Other,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ToolPhase {
    Started,
    Completed,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ToolResult {
    Succeeded,
    Failed,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MainLoopUsage {
    pub input_tokens: u64,
    pub cache_creation_input_tokens: u64,
    pub cache_read_input_tokens: u64,
    pub output_tokens: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RunUsage {
    pub main_loop: MainLoopUsage,
    pub total_cost_microusd: u64,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ProtocolViolation {
    MalformedJson,
    LineTooLong,
    LossyOutput,
    OutputTruncated,
    InvalidSessionId,
    ConflictingInit,
    InvalidLabel,
    InvalidUsage,
    ConflictingTerminal,
    TooManyTools,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Observation {
    Initialized {
        model: BoundedText,
        version: BoundedText,
        permission_mode: PermissionMode,
    },
    ToolChanged {
        id: String,
        kind: ToolKind,
        parent_tool_use_id: Option<String>,
        phase: ToolPhase,
        result: Option<ToolResult>,
    },
    FinalMessage {
        preview: BoundedText,
    },
    Usage {
        usage: RunUsage,
    },
    ProviderFailed {
        reason: FailureReason,
    },
    /// Stderr content is deliberately reduced to metadata.
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
    Permission,
    Limit,
    Process,
    Spawn,
    Incomplete,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Outcome {
    Succeeded {
        session_id: String,
        usage: RunUsage,
    },
    Failed {
        session_id: String,
        reason: FailureReason,
        usage: Option<RunUsage>,
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
}

#[derive(Eq, PartialEq)]
enum RunnerTerminal {
    SpawnFailed,
    Exited {
        exit_code: Option<i32>,
        signal: Option<i32>,
    },
}

#[derive(Clone, Eq, PartialEq)]
struct ResultState {
    usage: RunUsage,
    preview: Option<BoundedText>,
    failure: Option<FailureReason>,
}

#[derive(Clone, Eq, PartialEq)]
struct InitState {
    model: BoundedText,
    version: BoundedText,
}

#[derive(Clone, Eq, PartialEq)]
struct ToolState {
    kind: ToolKind,
    parent_tool_use_id: Option<String>,
    started: bool,
    result: Option<ToolResult>,
}

pub struct Finished {
    pub observations: Vec<Observation>,
    pub outcome: Outcome,
    pub final_preview: Option<BoundedText>,
}

/// Incremental, bounded decoder for one prepared Claude invocation.
///
/// It deliberately has no `Debug` or `Clone` implementation because it can
/// temporarily hold an incomplete raw provider line.
pub struct Decoder {
    expected_session_id: String,
    buffer: Vec<u8>,
    discard_until_newline: bool,
    integrity_failed: bool,
    init: Option<InitState>,
    result: Option<ResultState>,
    reported_failure: Option<FailureReason>,
    terminal: Option<RunnerTerminal>,
    tools: HashMap<String, ToolState>,
    violations: HashSet<ProtocolViolation>,
}

impl Decoder {
    /// Creates a decoder bound to a caller-allocated fresh session identity.
    ///
    /// # Errors
    ///
    /// Returns [`PrepareError::InvalidSessionId`] unless the expected identity
    /// is a canonical UUID.
    pub fn fresh(expected_session_id: String) -> Result<Self, PrepareError> {
        Self::bound(expected_session_id)
    }

    /// Creates a recovery decoder bound to one exact resumed session identity.
    ///
    /// # Errors
    ///
    /// Returns [`PrepareError::InvalidSessionId`] unless the expected identity
    /// is a canonical UUID.
    pub fn resume(expected_session_id: String) -> Result<Self, PrepareError> {
        Self::bound(expected_session_id)
    }

    fn bound(expected_session_id: String) -> Result<Self, PrepareError> {
        validate_uuid(&expected_session_id)?;
        Ok(Self::new(expected_session_id))
    }

    fn new(expected_session_id: String) -> Self {
        Self {
            expected_session_id,
            buffer: Vec::new(),
            discard_until_newline: false,
            integrity_failed: false,
            init: None,
            result: None,
            reported_failure: None,
            terminal: None,
            tools: HashMap::new(),
            violations: HashSet::new(),
        }
    }

    /// Consumes one durable runner event and returns structurally filtered observations.
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
            } => {
                vec![Observation::Diagnostic {
                    bytes: text.len(),
                    lossy: *lossy,
                }]
            }
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

        let final_preview = self
            .result
            .as_ref()
            .and_then(|result| result.preview.clone());
        let outcome = if self.terminal.is_none() {
            self.failed(
                FailureReason::Incomplete,
                self.result.as_ref().map(|result| result.usage),
            )
        } else if self.integrity_failed {
            self.failed(
                FailureReason::Protocol,
                self.result.as_ref().map(|result| result.usage),
            )
        } else {
            match &self.terminal {
                Some(RunnerTerminal::SpawnFailed) => self.failed(FailureReason::Spawn, None),
                None => unreachable!("handled before integrity reconciliation"),
                Some(RunnerTerminal::Exited { .. })
                    if self.reported_failure.is_some()
                        || self
                            .result
                            .as_ref()
                            .is_some_and(|result| result.failure.is_some()) =>
                {
                    let reason = self
                        .result
                        .as_ref()
                        .and_then(|result| result.failure)
                        .or(self.reported_failure)
                        .expect("guarded above");
                    self.failed(reason, self.result.as_ref().map(|result| result.usage))
                }
                Some(RunnerTerminal::Exited {
                    exit_code: Some(0),
                    signal: None,
                }) => match (&self.init, &self.result) {
                    (Some(_), Some(result)) => Outcome::Succeeded {
                        session_id: self.expected_session_id,
                        usage: result.usage,
                    },
                    _ => self.failed(
                        FailureReason::Incomplete,
                        self.result.as_ref().map(|result| result.usage),
                    ),
                },
                Some(RunnerTerminal::Exited { .. }) => self.failed(
                    FailureReason::Process,
                    self.result.as_ref().map(|result| result.usage),
                ),
            }
        };
        Finished {
            observations,
            outcome,
            final_preview,
        }
    }

    fn failed(&self, reason: FailureReason, usage: Option<RunUsage>) -> Outcome {
        Outcome::Failed {
            session_id: self.expected_session_id.clone(),
            reason,
            usage,
        }
    }

    fn push_stdout(&mut self, bytes: &[u8], lossy: bool) -> Vec<Observation> {
        if lossy {
            self.buffer.clear();
            self.discard_until_newline = !bytes.ends_with(b"\n");
            return self.record_violation(ProtocolViolation::LossyOutput);
        }
        let mut observations = Vec::new();
        let mut remaining = bytes;
        while let Some(newline) = remaining.iter().position(|byte| *byte == b'\n') {
            let (segment, tail) = remaining.split_at(newline);
            remaining = &tail[1..];
            if self.discard_until_newline {
                self.discard_until_newline = false;
                self.buffer.clear();
                continue;
            }
            if self.buffer.len().saturating_add(segment.len()) > MAX_CLAUDE_JSON_LINE_BYTES {
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
            if self.buffer.len().saturating_add(remaining.len()) > MAX_CLAUDE_JSON_LINE_BYTES {
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
            "system" if value.get("subtype").and_then(Value::as_str) == Some("init") => {
                self.parse_init(&value)
            }
            "assistant" => self.parse_assistant(&value),
            "user" => self.parse_user(&value),
            "result" => self.parse_result(&value),
            _ => Vec::new(),
        }
    }

    fn parse_init(&mut self, value: &Value) -> Vec<Observation> {
        if !self.session_matches(value) {
            return self.record_violation(ProtocolViolation::InvalidSessionId);
        }
        let Some(model) = value.get("model").and_then(Value::as_str) else {
            return self.record_violation(ProtocolViolation::MalformedJson);
        };
        let Some(version) = value.get("claude_code_version").and_then(Value::as_str) else {
            return self.record_violation(ProtocolViolation::MalformedJson);
        };
        if value.get("permissionMode").and_then(Value::as_str) != Some("acceptEdits") {
            return self.record_violation(ProtocolViolation::ConflictingInit);
        }
        let init = InitState {
            model: BoundedText::new(model, MAX_LABEL_BYTES),
            version: BoundedText::new(version, MAX_LABEL_BYTES),
        };
        if let Some(existing) = &self.init {
            return if *existing == init {
                Vec::new()
            } else {
                self.record_violation(ProtocolViolation::ConflictingInit)
            };
        }
        self.init = Some(init.clone());
        vec![Observation::Initialized {
            model: init.model,
            version: init.version,
            permission_mode: PermissionMode::AcceptEdits,
        }]
    }

    fn parse_assistant(&mut self, value: &Value) -> Vec<Observation> {
        if !self.session_matches(value) {
            return self.record_violation(ProtocolViolation::InvalidSessionId);
        }
        if value.get("error").is_some() {
            return self.set_reported_failure(FailureReason::Provider);
        }
        let Some(message) = value.get("message") else {
            return self.record_violation(ProtocolViolation::MalformedJson);
        };
        let Some(content) = message.get("content").and_then(Value::as_array) else {
            return self.record_violation(ProtocolViolation::MalformedJson);
        };
        let parent_tool_use_id = match parse_parent_id(value) {
            Ok(parent) => parent,
            Err(()) => return self.record_violation(ProtocolViolation::InvalidLabel),
        };
        let mut observations = Vec::new();
        for block in content {
            if block.get("type").and_then(Value::as_str) == Some("tool_use") {
                observations.extend(self.parse_tool_use(block, parent_tool_use_id.as_deref()));
            }
        }
        observations
    }

    fn parse_tool_use(&mut self, block: &Value, parent: Option<&str>) -> Vec<Observation> {
        let Some(id) = block.get("id").and_then(Value::as_str) else {
            return self.record_violation(ProtocolViolation::InvalidLabel);
        };
        if !valid_label(id) {
            return self.record_violation(ProtocolViolation::InvalidLabel);
        }
        let parent_tool_use_id = parent.map(str::to_owned);
        let kind = tool_kind(block.get("name").and_then(Value::as_str).unwrap_or(""));
        if let Some(existing) = self.tools.get(id) {
            return if existing.started
                && existing.kind == kind
                && existing.parent_tool_use_id == parent_tool_use_id
            {
                Vec::new()
            } else {
                self.record_violation(ProtocolViolation::ConflictingTerminal)
            };
        }
        if self.tools.len() >= MAX_TRACKED_TOOLS {
            return self.record_violation(ProtocolViolation::TooManyTools);
        }
        self.tools.insert(
            id.to_owned(),
            ToolState {
                kind,
                parent_tool_use_id: parent_tool_use_id.clone(),
                started: true,
                result: None,
            },
        );
        vec![Observation::ToolChanged {
            id: id.to_owned(),
            kind,
            parent_tool_use_id,
            phase: ToolPhase::Started,
            result: None,
        }]
    }

    fn parse_user(&mut self, value: &Value) -> Vec<Observation> {
        if !self.session_matches(value) {
            return self.record_violation(ProtocolViolation::InvalidSessionId);
        }
        let parent_tool_use_id = match parse_parent_id(value) {
            Ok(parent) => parent,
            Err(()) => return self.record_violation(ProtocolViolation::InvalidLabel),
        };
        let Some(content) = value
            .get("message")
            .and_then(|message| message.get("content"))
            .and_then(Value::as_array)
        else {
            return Vec::new();
        };
        let mut observations = Vec::new();
        for block in content {
            if block.get("type").and_then(Value::as_str) != Some("tool_result") {
                continue;
            }
            let Some(id) = block.get("tool_use_id").and_then(Value::as_str) else {
                observations.extend(self.record_violation(ProtocolViolation::InvalidLabel));
                continue;
            };
            if !valid_label(id) {
                observations.extend(self.record_violation(ProtocolViolation::InvalidLabel));
                continue;
            }
            let result = if block.get("is_error").and_then(Value::as_bool) == Some(true) {
                ToolResult::Failed
            } else {
                ToolResult::Succeeded
            };
            observations.extend(self.complete_tool(id, parent_tool_use_id.as_deref(), result));
        }
        observations
    }

    fn parse_result(&mut self, value: &Value) -> Vec<Observation> {
        if !self.session_matches(value) {
            return self.record_violation(ProtocolViolation::InvalidSessionId);
        }
        let Some(usage) = parse_usage(value) else {
            return self.record_violation(ProtocolViolation::InvalidUsage);
        };
        let Some(permission_denials) = value.get("permission_denials").and_then(Value::as_array)
        else {
            return self.record_violation(ProtocolViolation::MalformedJson);
        };
        let subtype = value.get("subtype").and_then(Value::as_str).unwrap_or("");
        let is_error = value.get("is_error").and_then(Value::as_bool);
        let completed = optional_terminal_reason(value.get("terminal_reason"))
            && required_stop_reason(value.get("stop_reason"));
        let failure = if !permission_denials.is_empty() {
            Some(FailureReason::Permission)
        } else if matches!(subtype, "error_max_turns" | "error_max_budget_usd") {
            Some(FailureReason::Limit)
        } else if subtype == "success" && is_error == Some(false) && completed {
            None
        } else {
            Some(FailureReason::Provider)
        };
        let preview = if failure.is_none() {
            let Some(result) = value.get("result").and_then(Value::as_str) else {
                return self.record_violation(ProtocolViolation::MalformedJson);
            };
            Some(BoundedText::new(result, MAX_CLAUDE_PREVIEW_BYTES))
        } else {
            None
        };
        let result = ResultState {
            usage,
            preview,
            failure,
        };
        if let Some(existing) = &self.result {
            return if *existing == result {
                Vec::new()
            } else {
                self.record_violation(ProtocolViolation::ConflictingTerminal)
            };
        }
        self.result = Some(result.clone());
        let mut observations = Vec::new();
        if let Some(preview) = result.preview {
            observations.push(Observation::FinalMessage { preview });
        }
        observations.push(Observation::Usage { usage });
        if let Some(reason) = failure {
            observations.extend(self.set_reported_failure(reason));
        }
        observations
    }

    fn session_matches(&self, value: &Value) -> bool {
        value
            .get("session_id")
            .and_then(Value::as_str)
            .is_some_and(|session_id| {
                validate_uuid(session_id).is_ok() && session_id == self.expected_session_id
            })
    }

    fn complete_tool(
        &mut self,
        id: &str,
        parent: Option<&str>,
        result: ToolResult,
    ) -> Vec<Observation> {
        let parent_tool_use_id = parent.map(str::to_owned);
        if let Some(existing) = self.tools.get_mut(id) {
            if existing.parent_tool_use_id != parent_tool_use_id {
                return self.record_violation(ProtocolViolation::ConflictingTerminal);
            }
            if let Some(previous) = existing.result {
                return if previous == result {
                    Vec::new()
                } else {
                    self.record_violation(ProtocolViolation::ConflictingTerminal)
                };
            }
            existing.result = Some(result);
            let kind = existing.kind;
            return vec![Observation::ToolChanged {
                id: id.to_owned(),
                kind,
                parent_tool_use_id,
                phase: ToolPhase::Completed,
                result: Some(result),
            }];
        }
        if self.tools.len() >= MAX_TRACKED_TOOLS {
            return self.record_violation(ProtocolViolation::TooManyTools);
        }
        self.tools.insert(
            id.to_owned(),
            ToolState {
                kind: ToolKind::Other,
                parent_tool_use_id: parent_tool_use_id.clone(),
                started: false,
                result: Some(result),
            },
        );
        vec![Observation::ToolChanged {
            id: id.to_owned(),
            kind: ToolKind::Other,
            parent_tool_use_id,
            phase: ToolPhase::Completed,
            result: Some(result),
        }]
    }

    fn set_reported_failure(&mut self, reason: FailureReason) -> Vec<Observation> {
        match self.reported_failure {
            None => {
                self.reported_failure = Some(reason);
                vec![Observation::ProviderFailed { reason }]
            }
            Some(existing) if existing == reason => Vec::new(),
            Some(FailureReason::Provider)
                if matches!(reason, FailureReason::Permission | FailureReason::Limit) =>
            {
                self.reported_failure = Some(reason);
                vec![Observation::ProviderFailed { reason }]
            }
            Some(existing)
                if matches!(existing, FailureReason::Permission | FailureReason::Limit)
                    && reason == FailureReason::Provider =>
            {
                Vec::new()
            }
            Some(_) => self.record_violation(ProtocolViolation::ConflictingTerminal),
        }
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

fn valid_label(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_LABEL_BYTES
        && value.bytes().all(|byte| byte.is_ascii_graphic())
}

fn optional_terminal_reason(value: Option<&Value>) -> bool {
    match value {
        None | Some(Value::Null) => true,
        Some(Value::String(value)) => value == "completed",
        Some(_) => false,
    }
}

fn required_stop_reason(value: Option<&Value>) -> bool {
    match value {
        Some(Value::Null) => true,
        Some(Value::String(value)) => value == "end_turn",
        Some(_) => false,
        None => false,
    }
}

fn parse_parent_id(value: &Value) -> Result<Option<String>, ()> {
    match value.get("parent_tool_use_id") {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(id)) if valid_label(id) => Ok(Some(id.clone())),
        Some(_) => Err(()),
    }
}

fn tool_kind(name: &str) -> ToolKind {
    match name {
        "Bash" => ToolKind::Command,
        "Read" | "Write" | "Edit" | "Glob" | "Grep" | "NotebookEdit" => ToolKind::Filesystem,
        "Task" | "Agent" => ToolKind::Subagent,
        "WebSearch" | "WebFetch" => ToolKind::Web,
        "TodoWrite" => ToolKind::Todo,
        "AskUserQuestion" => ToolKind::UserInput,
        name if name.starts_with("mcp__") => ToolKind::Mcp,
        _ => ToolKind::Other,
    }
}

fn parse_usage(value: &Value) -> Option<RunUsage> {
    let usage = value.get("usage")?;
    let input_tokens = bounded_token(usage.get("input_tokens")?)?;
    let cache_creation_input_tokens = bounded_token(usage.get("cache_creation_input_tokens")?)?;
    let cache_read_input_tokens = bounded_token(usage.get("cache_read_input_tokens")?)?;
    let output_tokens = bounded_token(usage.get("output_tokens")?)?;
    let cost = value.get("total_cost_usd")?.as_f64()?;
    if !cost.is_finite() || cost.is_sign_negative() {
        return None;
    }
    let cost_microusd = (cost * 1_000_000.0).round();
    if cost_microusd > MAX_COST_MICROUSD as f64 {
        return None;
    }
    Some(RunUsage {
        main_loop: MainLoopUsage {
            input_tokens,
            cache_creation_input_tokens,
            cache_read_input_tokens,
            output_tokens,
        },
        total_cost_microusd: cost_microusd as u64,
    })
}

fn bounded_token(value: &Value) -> Option<u64> {
    value.as_u64().filter(|value| *value <= MAX_TOKEN_TOTAL)
}
