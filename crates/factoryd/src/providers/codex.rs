//! Bounded adapter for the Codex 0.147 JSONL protocol.
//!
//! [`CodexProvider`] (bottom of this file) is the interactive-session
//! `Provider` impl used by resident sessions; everything above it
//! (`Decoder`, `Observation`, the non-interactive `prepare`) is the old
//! `codex exec --json` pipe-mode adapter, kept compiling for
//! `execution.rs`'s existing path until a later track switches execution
//! over and deletes it (see `TRACK5-DESIGN.md` §7).

use std::{
    collections::HashSet,
    ffi::OsString,
    fs,
    path::{Path, PathBuf},
};

use factory_core::{
    RunId, RunnerInstanceId,
    runner::{OutputStream, RunnerEvent},
};
use serde_json::Value;
use uuid::Uuid;

use crate::{
    providers::{Capabilities, InteractiveLaunch, Provider, ProviderError, SpawnContext, hooks},
    runner_process::{LaunchSpec, ProviderEnvironment},
};

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
    /// An explicit provider model, or `None` for the Codex CLI default.
    pub model: Option<String>,
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
    if let Some(model) = input.model.as_deref() {
        arguments.push(OsString::from("--model"));
        arguments.push(OsString::from(model));
    }

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
            terminal: None,
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

// --- Interactive-session provider (`Provider` trait impl) ---
//
// This is the resident-session launch path, replacing the pipe-mode
// `prepare()` above once `execution.rs` switches over (`TRACK5-DESIGN.md`
// §5/§9). It shares nothing with the decoder machinery except
// `validate_thread_id`.

/// The permission-mode strings the daemon validates and stores per-agent
/// for Codex (product decision D4, `TRACK5-DESIGN.md`): `on-request` or
/// `never`, both within `--sandbox workspace-write` (OS-level command
/// sandboxing, orthogonal to the approval policy and not something this
/// provider overrides — the seeded `config.toml` carries forward whatever
/// the operator's real config already set for it).
pub const PERMISSION_MODES: [&str; 2] = ["on-request", "never"];

const HOOKS_BEGIN_MARKER: &str = "# --- dark-factory hooks BEGIN ---";
const HOOKS_END_MARKER: &str = "# --- dark-factory hooks END ---";
const MINIMAL_CONFIG_TOML: &str =
    "# Dark Factory generated Codex home (no ~/.codex/config.toml was found to copy).\n";

/// Interactive-session [`Provider`] for Codex. Launches `codex
/// --dangerously-bypass-hook-trust [--model M] [-c
/// approval_policy="<permission_mode>"] [resume <thread-id>]` with
/// `CODEX_HOME` pointed at this agent's own seeded home (per orchestrator
/// amendment A2, `TRACK5-DESIGN.md`: per *agent*, not per session, so
/// `codex resume` can find its own prior rollout file across a stop and
/// restart). `--dangerously-bypass-hook-trust` is unconditional: the hooks
/// this provider writes are 100% daemon-authored into an isolated
/// `CODEX_HOME` the operator never hand-edits, which already is the
/// vetting Codex's normal hook-trust prompt would otherwise ask for. See
/// `docs/providers.md`.
pub struct CodexProvider {
    /// The operator's real Codex home to seed a fresh per-agent
    /// `CODEX_HOME` from (`config.toml`, `auth.json`). Defaults to
    /// `$HOME/.codex`; overridable for tests via
    /// [`CodexProvider::with_source_home`].
    source_home: Option<PathBuf>,
}

impl CodexProvider {
    /// Resolves the seed source from `$HOME/.codex`, matching Codex's own
    /// default `CODEX_HOME`. `None` (no `$HOME`) means a fresh per-agent
    /// home always starts from [`MINIMAL_CONFIG_TOML`] with no `auth.json`
    /// link — Codex will then have no subscription credentials, same as
    /// running `codex` with no prior login.
    #[must_use]
    pub fn new() -> Self {
        Self {
            source_home: std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".codex")),
        }
    }

    #[cfg(test)]
    fn with_source_home(source_home: PathBuf) -> Self {
        Self {
            source_home: Some(source_home),
        }
    }
}

impl Default for CodexProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl Provider for CodexProvider {
    fn spawn_spec(&self, ctx: &SpawnContext) -> Result<InteractiveLaunch, ProviderError> {
        let codex_home = ctx.agent_dir.join("codex-home");
        seed_codex_home_once(&codex_home, self.source_home.as_deref())?;
        rewrite_hooks_block(&codex_home, &ctx.factoryctl_path, &ctx.hook_token_path)?;

        let mut args = vec!["--dangerously-bypass-hook-trust".to_owned()];
        if let Some(model) = &ctx.model {
            args.push("--model".to_owned());
            args.push(model.clone());
        }
        if let Some(permission_mode) = &ctx.permission_mode {
            args.push("-c".to_owned());
            args.push(format!("approval_policy=\"{permission_mode}\""));
        }
        if let Some(thread_id) = &ctx.resume {
            validate_thread_id(thread_id).map_err(|_| ProviderError::ResumeIdNotUuid {
                value: thread_id.clone(),
            })?;
            args.push("resume".to_owned());
            args.push(thread_id.clone());
        }

        Ok(InteractiveLaunch {
            program: PathBuf::from("codex"),
            args,
            env: vec![(
                "CODEX_HOME".to_owned(),
                codex_home.to_string_lossy().into_owned(),
            )],
            generated_files: vec![codex_home.join("config.toml")],
        })
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            hooks: true,
            resume: true,
            permission_modes: &PERMISSION_MODES,
        }
    }
}

/// Idempotently seeds `codex_home` (mode `0700`, created if missing) the
/// first time it is used: copies `source_home/config.toml` if present, else
/// writes [`MINIMAL_CONFIG_TOML`]; symlinks `source_home/auth.json` if
/// present and not already linked. Existing files are never overwritten —
/// this is a one-time seed, not a sync. The hooks block is refreshed
/// separately, every spawn, by [`rewrite_hooks_block`].
fn seed_codex_home_once(
    codex_home: &Path,
    source_home: Option<&Path>,
) -> Result<(), ProviderError> {
    hooks::ensure_private_dir(codex_home).map_err(|source| ProviderError::Seed {
        path: codex_home.to_path_buf(),
        source,
    })?;

    let config_path = codex_home.join("config.toml");
    if !config_path.exists() {
        let contents = source_home
            .map(|home| home.join("config.toml"))
            .and_then(|path| fs::read(path).ok())
            .unwrap_or_else(|| MINIMAL_CONFIG_TOML.as_bytes().to_vec());
        hooks::write_private_file(&config_path, &contents).map_err(|source| {
            ProviderError::Seed {
                path: config_path.clone(),
                source,
            }
        })?;
    }

    if let Some(source_home) = source_home {
        let auth_path = codex_home.join("auth.json");
        let source_auth = source_home.join("auth.json");
        if fs::symlink_metadata(&auth_path).is_err() && source_auth.exists() {
            std::os::unix::fs::symlink(&source_auth, &auth_path).map_err(|source| {
                ProviderError::Seed {
                    path: auth_path.clone(),
                    source,
                }
            })?;
        }
    }
    Ok(())
}

/// Idempotently rewrites the daemon-owned hooks block in `codex_home`'s
/// `config.toml`, replacing everything between the `BEGIN`/`END` markers if
/// present, else appending a fresh block. Called on every spawn: the hook
/// token path changes per session, so this keeps the seeded config current
/// without disturbing whatever the operator's real `config.toml` carried
/// forward from the one-time seed (model, provider, trust settings, ...).
fn rewrite_hooks_block(
    codex_home: &Path,
    factoryctl_path: &Path,
    hook_token_path: &Path,
) -> Result<(), ProviderError> {
    let config_path = codex_home.join("config.toml");
    let existing = fs::read_to_string(&config_path).map_err(|source| ProviderError::Seed {
        path: config_path.clone(),
        source,
    })?;
    let mut rewritten = strip_hooks_block(&existing);
    if !rewritten.is_empty() {
        rewritten.push_str("\n\n");
    }
    rewritten.push_str(&hooks_block_toml(factoryctl_path, hook_token_path));
    hooks::write_private_file(&config_path, rewritten.as_bytes()).map_err(|source| {
        ProviderError::Seed {
            path: config_path.clone(),
            source,
        }
    })
}

/// Removes a previously written daemon hooks block (markers inclusive), if
/// present, leaving the rest of the file untouched (trailing whitespace
/// trimmed). Not a general TOML parser: it operates on the exact marker
/// lines [`hooks_block_toml`] writes.
fn strip_hooks_block(config: &str) -> String {
    let Some(begin) = config.find(HOOKS_BEGIN_MARKER) else {
        return config.trim_end().to_owned();
    };
    let before = &config[..begin];
    let after_marker = &config[begin..];
    let after = after_marker
        .find(HOOKS_END_MARKER)
        .map_or("", |end_offset| {
            &after_marker[end_offset + HOOKS_END_MARKER.len()..]
        });
    format!("{}{}", before.trim_end(), after)
        .trim_end()
        .to_owned()
}

fn hooks_block_toml(factoryctl_path: &Path, hook_token_path: &Path) -> String {
    let mut block = String::new();
    block.push_str(HOOKS_BEGIN_MARKER);
    block.push('\n');
    for event in hooks::HOOK_EVENTS {
        let name = event.provider_event_name();
        let command = hooks::hook_command(factoryctl_path, hook_token_path, event);
        block.push_str(&format!("[[hooks.{name}]]\n"));
        block.push_str(&format!("[[hooks.{name}.hooks]]\n"));
        block.push_str("type = \"command\"\n");
        block.push_str(&format!("command = \"{}\"\n", toml_escape(&command)));
        block.push_str("timeout = 30\n\n");
    }
    block.push_str(HOOKS_END_MARKER);
    block.push('\n');
    block
}

fn toml_escape(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

#[cfg(test)]
mod provider_tests {
    use std::os::unix::fs::PermissionsExt;

    use factory_core::{AgentId, ProjectId, SessionId};

    use super::*;

    fn context(directory: &Path) -> SpawnContext {
        SpawnContext {
            agent_id: AgentId::try_from("worker-1").unwrap(),
            project_id: ProjectId::try_from("factory").unwrap(),
            session_id: SessionId::try_from("2f5a1e2e-2222-4444-8888-0123456789ab").unwrap(),
            worktree: directory.join("worktree"),
            model: None,
            permission_mode: None,
            resume: None,
            hook_token_path: directory.join("runtime").join("hook.token"),
            factoryctl_path: PathBuf::from("/abs/factoryctl"),
            agent_dir: directory.join("agent-dir"),
            socket_path: directory.join("f.sock"),
        }
    }

    #[test]
    fn fresh_launch_has_no_resume_argument_and_sets_codex_home() {
        let directory = tempfile::tempdir().unwrap();
        let ctx = context(directory.path());
        let launch = CodexProvider::with_source_home(directory.path().join("no-real-home"))
            .spawn_spec(&ctx)
            .unwrap();

        assert_eq!(launch.program, PathBuf::from("codex"));
        assert_eq!(
            launch.args,
            vec!["--dangerously-bypass-hook-trust".to_owned()]
        );
        let codex_home = directory.path().join("agent-dir").join("codex-home");
        assert_eq!(
            launch.env,
            vec![(
                "CODEX_HOME".to_owned(),
                codex_home.to_string_lossy().into_owned()
            )]
        );
    }

    #[test]
    fn resume_launch_passes_model_approval_policy_and_thread_id_in_order() {
        let directory = tempfile::tempdir().unwrap();
        let mut ctx = context(directory.path());
        ctx.model = Some("gpt-5-codex".to_owned());
        ctx.permission_mode = Some("never".to_owned());
        ctx.resume = Some("9b1deb4d-3b7d-4bad-9bdd-2b0d7b3dcb6d".to_owned());
        let launch = CodexProvider::with_source_home(directory.path().join("no-real-home"))
            .spawn_spec(&ctx)
            .unwrap();

        assert_eq!(
            launch.args,
            vec![
                "--dangerously-bypass-hook-trust".to_owned(),
                "--model".to_owned(),
                "gpt-5-codex".to_owned(),
                "-c".to_owned(),
                "approval_policy=\"never\"".to_owned(),
                "resume".to_owned(),
                "9b1deb4d-3b7d-4bad-9bdd-2b0d7b3dcb6d".to_owned(),
            ]
        );
    }

    #[test]
    fn resume_rejects_a_non_uuid_thread_id() {
        let directory = tempfile::tempdir().unwrap();
        let mut ctx = context(directory.path());
        ctx.resume = Some("not-a-uuid".to_owned());
        let result =
            CodexProvider::with_source_home(directory.path().join("no-real-home")).spawn_spec(&ctx);
        assert!(matches!(result, Err(ProviderError::ResumeIdNotUuid { .. })));
    }

    #[test]
    fn seeds_a_minimal_config_when_no_real_codex_home_exists() {
        let directory = tempfile::tempdir().unwrap();
        let ctx = context(directory.path());
        CodexProvider::with_source_home(directory.path().join("missing"))
            .spawn_spec(&ctx)
            .unwrap();

        let config_path = directory
            .path()
            .join("agent-dir")
            .join("codex-home")
            .join("config.toml");
        let contents = fs::read_to_string(&config_path).unwrap();
        assert!(contents.starts_with(MINIMAL_CONFIG_TOML.trim_end()));
        assert!(contents.contains(HOOKS_BEGIN_MARKER));
        assert!(contents.contains(HOOKS_END_MARKER));
        assert!(
            !directory
                .path()
                .join("agent-dir")
                .join("codex-home")
                .join("auth.json")
                .exists()
        );

        let metadata = fs::metadata(config_path.parent().unwrap()).unwrap();
        assert_eq!(metadata.permissions().mode() & 0o777, 0o700);
    }

    #[test]
    fn copies_the_real_config_and_symlinks_auth_json_on_first_seed_only() {
        let directory = tempfile::tempdir().unwrap();
        let real_home = directory.path().join("real-codex-home");
        fs::create_dir_all(&real_home).unwrap();
        fs::write(real_home.join("config.toml"), "model = \"gpt-5.6\"\n").unwrap();
        fs::write(real_home.join("auth.json"), "{\"token\":\"secret\"}").unwrap();

        let ctx = context(directory.path());
        let provider = CodexProvider::with_source_home(real_home.clone());
        provider.spawn_spec(&ctx).unwrap();

        let codex_home = directory.path().join("agent-dir").join("codex-home");
        let config_contents = fs::read_to_string(codex_home.join("config.toml")).unwrap();
        assert!(config_contents.starts_with("model = \"gpt-5.6\""));
        assert!(config_contents.contains(HOOKS_BEGIN_MARKER));
        let auth_link = fs::read_link(codex_home.join("auth.json")).unwrap();
        assert_eq!(auth_link, real_home.join("auth.json"));

        // A real user edit to the seeded config.toml after the first spawn
        // is preserved by later spawns: only the hooks block is refreshed.
        let seeded = fs::read_to_string(codex_home.join("config.toml")).unwrap();
        let base = strip_hooks_block(&seeded);
        fs::write(
            codex_home.join("config.toml"),
            format!("{base}\nmodel_reasoning_effort = \"xhigh\"\n"),
        )
        .unwrap();
        provider.spawn_spec(&ctx).unwrap();
        let after_second_spawn = fs::read_to_string(codex_home.join("config.toml")).unwrap();
        assert!(after_second_spawn.contains("model_reasoning_effort = \"xhigh\""));
        assert_eq!(after_second_spawn.matches(HOOKS_BEGIN_MARKER).count(), 1);
    }

    #[test]
    fn hooks_block_is_rewritten_idempotently_across_spawns() {
        let directory = tempfile::tempdir().unwrap();
        let ctx = context(directory.path());
        let provider = CodexProvider::with_source_home(directory.path().join("missing"));
        provider.spawn_spec(&ctx).unwrap();
        provider.spawn_spec(&ctx).unwrap();
        provider.spawn_spec(&ctx).unwrap();

        let config_path = directory
            .path()
            .join("agent-dir")
            .join("codex-home")
            .join("config.toml");
        let contents = fs::read_to_string(&config_path).unwrap();
        assert_eq!(contents.matches(HOOKS_BEGIN_MARKER).count(), 1);
        assert_eq!(contents.matches(HOOKS_END_MARKER).count(), 1);
        assert_eq!(contents.matches("[[hooks.Stop]]").count(), 1);
    }

    #[test]
    fn hooks_block_toml_has_the_exact_designed_shape_for_one_event() {
        let block = hooks_block_toml(
            Path::new("/abs/factoryctl"),
            Path::new("/abs/runs/session-1/hook.token"),
        );
        assert!(block.starts_with(HOOKS_BEGIN_MARKER));
        assert!(block.trim_end().ends_with(HOOKS_END_MARKER));
        assert!(block.contains(
            "[[hooks.Stop]]\n[[hooks.Stop.hooks]]\ntype = \"command\"\ncommand = \"'/abs/factoryctl' hook --token-file '/abs/runs/session-1/hook.token' Stop\"\ntimeout = 30\n"
        ));
    }

    #[test]
    fn capabilities_declare_hooks_resume_and_the_supported_permission_modes() {
        let capabilities = CodexProvider::new().capabilities();
        assert!(capabilities.hooks);
        assert!(capabilities.resume);
        assert_eq!(capabilities.permission_modes, PERMISSION_MODES);
    }

    #[test]
    fn config_toml_generated_by_a_fresh_seed_parses_under_codex_doctor() {
        // Guards against a schema regression without spawning a real
        // interactive session: `codex doctor` is a read-only diagnostic
        // that parses `CODEX_HOME/config.toml` and reports whether it
        // loaded, matching the manual verification recorded in this
        // track's report (`config.toml parse: ok` under `--strict-config`
        // for exactly this generated shape).
        if std::process::Command::new("codex")
            .arg("--version")
            .output()
            .is_err()
        {
            eprintln!("skipping: codex is not installed in this environment");
            return;
        }
        let directory = tempfile::tempdir().unwrap();
        let ctx = context(directory.path());
        CodexProvider::with_source_home(directory.path().join("missing"))
            .spawn_spec(&ctx)
            .unwrap();
        let codex_home = directory.path().join("agent-dir").join("codex-home");

        let output = std::process::Command::new("codex")
            .env("CODEX_HOME", &codex_home)
            .args(["--strict-config", "doctor", "--json"])
            .output()
            .unwrap();
        let report: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(
            report["checks"]["config.load"]["details"]["config.toml parse"],
            "ok"
        );
    }
}
