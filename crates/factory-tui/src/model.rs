//! The operator board's view-model: pure data + pure key-handling, no I/O.
//!
//! Everything in this module is deliberately free of sockets, threads, and PTYs so it can be
//! unit tested directly (see the `tests` module at the bottom). `net.rs` feeds it `NetMsg`s;
//! `main.rs`'s event loop feeds it `crossterm` key/paste events and applies the `Intent`s it
//! returns (sending requests, spawning/killing a zoom pane, quitting).
//!
//! ## Deriving agent state (`agent_state`)
//!
//! The board is Dwarf-Fortress-flavored: every agent is a glyph on the floor and a row in the
//! "units" list, colored by one of five states (idle / working / waiting / stopped / failed).
//! Today that state is derived from the agent's *latest* [`RunSnapshot`] (there is no durable
//! per-session state yet — see `README.md`). A later track is expected to replace the source
//! with real session state from provider hooks; **only [`agent_state`] should need to change**
//! when that happens, which is why every other piece of code in this crate calls it instead of
//! inspecting `RunStatus` directly.

use std::collections::{BTreeMap, VecDeque};

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use factory_core::local::{ErrorCode, LocalRequest, LocalResponse};
use factory_core::{
    AgentId, AgentRole, AgentSnapshot, EventEnvelope, FactoryEvent, ProjectId, ProjectSnapshot,
    RunId, RunSnapshot, RunStatus, TaskDetail, TaskId, TaskStatus,
};

/// How many announcement lines the ring buffer keeps. Old lines fall off the front.
pub const ANNOUNCEMENT_CAPACITY: usize = 500;
/// Width (in minutes) of one activity bucket. "Events per minute" per the design brief.
const ACTIVITY_BUCKET_MS: i64 = 60_000;
/// How many one-minute buckets of history each agent's sparkline retains.
const ACTIVITY_WINDOW: usize = 30;
/// A status/error line is shown in place of the key-help footer for this long after being set.
const STATUS_STICKY_MS: i64 = 6_000;

// ---------------------------------------------------------------------------------------------
// Agent state
// ---------------------------------------------------------------------------------------------

/// The five states the Dwarf-Fortress-style floor and unit list can show for an agent.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AgentState {
    Idle,
    Working,
    Waiting,
    Stopped,
    Failed,
}

impl AgentState {
    #[must_use]
    pub const fn glyph(self) -> char {
        match self {
            Self::Idle => '@',
            Self::Working => '@',
            Self::Waiting => '?',
            Self::Stopped => '@',
            Self::Failed => '!',
        }
    }

    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Working => "working",
            Self::Waiting => "waiting",
            Self::Stopped => "stopped",
            Self::Failed => "failed",
        }
    }
}

/// The single mapping point from durable daemon state to the board's `AgentState`.
///
/// `latest_run` is the agent's most recent [`RunSnapshot`] by `started_at_ms` (see
/// [`Board::latest_run_for`]), or `None` if the agent has never run. Mapping, per the design
/// brief: no run, or a run that finished successfully, reads as idle (the agent is available and
/// nothing is wrong); a run still in flight reads as working; a run waiting on input or blocked
/// reads as waiting; a run that ended in failure or was stopped keeps showing that outcome
/// (rather than reverting to idle) until the operator retries it, so a problem stays visible on
/// the floor.
///
/// `RunStatus::Paused` isn't named explicitly in the design brief's mapping; it's folded into
/// `Waiting` here (a paused run isn't actively doing work, but it isn't a terminal outcome
/// either) — see `README.md` for this judgment call.
#[must_use]
pub fn agent_state(latest_run: Option<&RunSnapshot>) -> AgentState {
    match latest_run.map(|run| run.status) {
        None | Some(RunStatus::Succeeded) => AgentState::Idle,
        Some(RunStatus::Starting | RunStatus::Running) => AgentState::Working,
        Some(RunStatus::Waiting | RunStatus::Blocked | RunStatus::Paused) => AgentState::Waiting,
        Some(RunStatus::Failed) => AgentState::Failed,
        Some(RunStatus::Stopped) => AgentState::Stopped,
    }
}

// ---------------------------------------------------------------------------------------------
// Ring buffer
// ---------------------------------------------------------------------------------------------

/// A fixed-capacity FIFO. Pushing past capacity drops the oldest item. Used for the
/// announcements log.
#[derive(Debug)]
pub struct RingBuffer<T> {
    items: VecDeque<T>,
    capacity: usize,
}

impl<T> RingBuffer<T> {
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        Self {
            items: VecDeque::with_capacity(capacity.min(1024)),
            capacity: capacity.max(1),
        }
    }

    pub fn push(&mut self, item: T) {
        if self.items.len() >= self.capacity {
            self.items.pop_front();
        }
        self.items.push_back(item);
    }

    pub fn iter(&self) -> impl DoubleEndedIterator<Item = &T> {
        self.items.iter()
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.items.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }
}

// ---------------------------------------------------------------------------------------------
// Announcements
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Announcement {
    pub at_ms: i64,
    pub text: String,
}

fn clock(at_ms: i64) -> String {
    // No calendar/timezone crate in the dependency tree (see SPIKE.md's MSRV-pinning care); a
    // clock readout only needs time-of-day, which is a modulus, not a calendar. Shown in UTC.
    let ms_of_day = at_ms.rem_euclid(86_400_000);
    let hours = ms_of_day / 3_600_000;
    let minutes = (ms_of_day % 3_600_000) / 60_000;
    format!("{hours:02}:{minutes:02}")
}

fn truncate_id(id: &str, max: usize) -> String {
    if id.chars().count() <= max {
        id.to_owned()
    } else {
        let head: String = id.chars().take(max.saturating_sub(1)).collect();
        format!("{head}\u{2026}")
    }
}

fn task_status_word(status: TaskStatus) -> &'static str {
    match status {
        TaskStatus::Queued => "queued",
        TaskStatus::Running => "started",
        TaskStatus::Blocked => "blocked",
        TaskStatus::Succeeded => "done",
        TaskStatus::Failed => "failed",
        TaskStatus::Cancelled => "cancelled",
    }
}

fn run_status_word(status: RunStatus) -> &'static str {
    match status {
        RunStatus::Starting => "starting",
        RunStatus::Running => "running",
        RunStatus::Waiting => "waiting for input",
        RunStatus::Blocked => "blocked",
        RunStatus::Paused => "paused",
        RunStatus::Succeeded => "succeeded",
        RunStatus::Failed => "failed",
        RunStatus::Stopped => "stopped",
    }
}

/// Formats one Dwarf-Fortress-terse announcement line for an event, e.g.
/// `20:41 bob     task#42 done`. Returns `None` for events this board doesn't narrate
/// (project-level bookkeeping isn't agent/task activity).
#[must_use]
pub fn format_event(event: &EventEnvelope) -> Option<Announcement> {
    let time = clock(event.occurred_at_ms);
    let text = match &event.event {
        FactoryEvent::TaskChanged { task } => {
            let actor = task
                .assigned_agent_id
                .as_ref()
                .map_or_else(|| "-".to_owned(), |id| truncate_id(id.as_str(), 10));
            format!(
                "{time} {actor:<10} task#{} {}",
                truncate_id(task.id.as_str(), 12),
                task_status_word(task.status)
            )
        }
        FactoryEvent::RunChanged { run } => {
            format!(
                "{time} {:<10} run {}",
                truncate_id(run.agent_id.as_str(), 10),
                run_status_word(run.status)
            )
        }
        FactoryEvent::AgentChanged { agent } => {
            format!(
                "{time} {:<10} agent updated",
                truncate_id(agent.id.as_str(), 10)
            )
        }
        FactoryEvent::TaskDeleted { task_id, .. } => {
            format!("{time} task#{} deleted", truncate_id(task_id.as_str(), 12))
        }
        FactoryEvent::AgentDeleted { agent_id, .. } => {
            format!("{time} {} removed", truncate_id(agent_id.as_str(), 10))
        }
        // TRANSITION: sessions land in 5A/5C; the board has no session-based
        // narration yet.
        FactoryEvent::SessionChanged { .. }
        | FactoryEvent::ProjectChanged { .. }
        | FactoryEvent::ProjectDeleted { .. } => return None,
    };
    Some(Announcement {
        at_ms: event.occurred_at_ms,
        text,
    })
}

/// Whether an event should bump an agent's activity sparkline, and which agent.
#[must_use]
fn event_agent(event: &FactoryEvent) -> Option<&AgentId> {
    match event {
        FactoryEvent::TaskChanged { task } => task.assigned_agent_id.as_ref(),
        FactoryEvent::RunChanged { run } => Some(&run.agent_id),
        FactoryEvent::AgentChanged { agent } => Some(&agent.id),
        FactoryEvent::AgentDeleted { agent_id, .. } => Some(agent_id),
        FactoryEvent::SessionChanged { .. }
        | FactoryEvent::TaskDeleted { .. }
        | FactoryEvent::ProjectChanged { .. }
        | FactoryEvent::ProjectDeleted { .. } => None,
    }
}

/// The project this event belongs to, so a board scoped to one project can ignore the rest.
#[must_use]
fn event_project(event: &FactoryEvent) -> &ProjectId {
    match event {
        FactoryEvent::ProjectChanged { project } => &project.id,
        FactoryEvent::TaskChanged { task } => &task.project_id,
        FactoryEvent::AgentChanged { agent } => &agent.project_id,
        FactoryEvent::RunChanged { run } => &run.project_id,
        FactoryEvent::SessionChanged { session } => &session.project_id,
        FactoryEvent::TaskDeleted { project_id, .. }
        | FactoryEvent::AgentDeleted { project_id, .. }
        | FactoryEvent::ProjectDeleted { project_id } => project_id,
    }
}

// ---------------------------------------------------------------------------------------------
// Activity sparklines
// ---------------------------------------------------------------------------------------------

/// A rolling per-minute event count for one agent, rendered as a braille sparkline. A stand-in
/// for a real tokens/turns series until provider hooks exist (see `README.md`).
#[derive(Debug, Default)]
pub struct ActivitySeries {
    /// `(bucket_start_ms, count)`, oldest first, one entry per minute.
    buckets: VecDeque<(i64, u32)>,
}

impl ActivitySeries {
    /// Advances the window so the newest bucket covers `at_ms`, without incrementing anything.
    /// Called on every UI tick so idle agents' sparklines still slide forward in real time.
    pub fn roll_to(&mut self, at_ms: i64) {
        let bucket_start = at_ms - at_ms.rem_euclid(ACTIVITY_BUCKET_MS);
        match self.buckets.back() {
            None => self.buckets.push_back((bucket_start, 0)),
            Some(&(last_start, _)) if bucket_start > last_start => {
                let mut next = last_start;
                while next < bucket_start {
                    next += ACTIVITY_BUCKET_MS;
                    self.buckets.push_back((next, 0));
                }
            }
            _ => {}
        }
        while self.buckets.len() > ACTIVITY_WINDOW {
            self.buckets.pop_front();
        }
    }

    /// Records one event at `at_ms`, rolling the window forward first if needed. An event whose
    /// timestamp lands before the current bucket (clock skew, replayed history) is folded into
    /// the newest bucket rather than rolling backward.
    pub fn record(&mut self, at_ms: i64) {
        self.roll_to(at_ms);
        if let Some(last) = self.buckets.back_mut() {
            last.1 += 1;
        }
    }

    /// Bucket counts, oldest first, suitable for a sparkline.
    #[must_use]
    pub fn counts(&self) -> Vec<u64> {
        self.buckets
            .iter()
            .map(|&(_, count)| u64::from(count))
            .collect()
    }
}

/// Braille fill levels from empty to full, matching the gradient the owner's design note uses
/// (`⣀⣠⣤⣴⣶⣾⣿`) plus a true-empty glyph for zero-count buckets.
const BRAILLE_LEVELS: [char; 8] = ['\u{2800}', '⣀', '⣠', '⣤', '⣴', '⣶', '⣾', '⣿'];

/// Renders `counts` (oldest first) as a fixed-width braille sparkline, right-aligned to the most
/// recent `width` buckets. Pure integer math throughout (no float precision-loss lint dodging
/// needed): each bar is `round(count * 7 / max)` levels tall.
#[must_use]
pub fn braille_sparkline(counts: &[u64], width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    let start = counts.len().saturating_sub(width);
    let slice = &counts[start..];
    let max = slice.iter().copied().max().unwrap_or(0);
    let mut out = String::with_capacity(width);
    for _ in 0..width.saturating_sub(slice.len()) {
        out.push(BRAILLE_LEVELS[0]);
    }
    for &count in slice {
        let level = if max == 0 {
            0
        } else {
            ((count.saturating_mul(7) + max / 2) / max).min(7) as usize
        };
        out.push(BRAILLE_LEVELS[level]);
    }
    out
}

// ---------------------------------------------------------------------------------------------
// Status line
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StatusLevel {
    Info,
    Error,
}

#[derive(Clone, Debug)]
pub struct StatusMessage {
    pub text: String,
    pub level: StatusLevel,
    pub at_ms: i64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Connection {
    Connecting,
    Live,
    Retrying,
}

// ---------------------------------------------------------------------------------------------
// Prompts, pickers, confirmation
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PromptKind {
    NewTask,
    MessageAgent(AgentId),
}

/// A minimal in-TUI multi-field text prompt. `Tab`/`Enter` advance fields; `Enter` on the last
/// field submits; `Esc` cancels; `Backspace` edits; anything else printable is appended.
#[derive(Clone, Debug)]
pub struct PromptState {
    pub kind: PromptKind,
    pub labels: Vec<&'static str>,
    pub values: Vec<String>,
    pub field: usize,
}

impl PromptState {
    #[must_use]
    fn new_task() -> Self {
        Self {
            kind: PromptKind::NewTask,
            labels: vec!["title", "body"],
            values: vec![String::new(), String::new()],
            field: 0,
        }
    }

    #[must_use]
    fn message_agent(agent_id: AgentId) -> Self {
        Self {
            kind: PromptKind::MessageAgent(agent_id),
            labels: vec!["message"],
            values: vec![String::new()],
            field: 0,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PickerKind {
    Project,
    AssignAgent(TaskId),
}

#[derive(Clone, Debug)]
pub struct PickerState {
    pub kind: PickerKind,
    pub cursor: usize,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Panel {
    Units,
    Jobs,
}

#[derive(Clone, Debug)]
pub enum Mode {
    Normal,
    /// Second `x` confirms deleting this task; any other key cancels.
    ConfirmDelete(TaskId),
    Prompt(PromptState),
    Picker(PickerState),
    /// Full-screen on the selected agent's terminal (see `README.md` for what's stubbed).
    Zoomed(AgentId),
}

/// A side effect for `main.rs` to carry out. `Board` never touches a socket or a PTY itself.
#[derive(Clone, Debug)]
pub enum Intent {
    None,
    Redraw,
    Quit,
    Send(LocalRequest),
    EnterZoom(AgentId),
    ExitZoom,
    /// Only meaningful while zoomed with `--dev-local-pty`; `main.rs` encodes and forwards.
    ForwardKey(KeyEvent),
    /// The operator picked a project from the startup picker. `Board` has already updated its
    /// own state (`select_project`); `main.rs` still needs to start that project's network
    /// session (`net::spawn_project_session`), which `Board` can't do itself.
    ProjectSelected(ProjectId),
}

// ---------------------------------------------------------------------------------------------
// Board
// ---------------------------------------------------------------------------------------------

pub struct Board {
    pub dev_local_pty: bool,
    pub now_ms: i64,

    pub connection: Connection,
    pub connection_detail: Option<String>,

    pub projects: Vec<ProjectSnapshot>,
    pub project: Option<ProjectSnapshot>,

    pub agents: BTreeMap<AgentId, AgentSnapshot>,
    pub tasks: BTreeMap<TaskId, TaskDetail>,
    pub runs: BTreeMap<RunId, RunSnapshot>,

    pub announcements: RingBuffer<Announcement>,
    pub activity: BTreeMap<AgentId, ActivitySeries>,

    pub units_selected: usize,
    pub jobs_selected: usize,
    pub focus: Panel,

    pub mode: Mode,
    /// Only meaningful while `mode` is `Zoomed`: forwarding keys to the pane (spike's "Pane"
    /// mode) vs. this board's zoom-control substate (spike's "Control" mode). Toggled by the
    /// spike's `Ctrl-]` prefix key, reused verbatim.
    pub zoom_forwarding: bool,

    pub status: Option<StatusMessage>,

    pub caught_up: bool,
    pub quit: bool,
}

impl Board {
    #[must_use]
    pub fn new(dev_local_pty: bool, now_ms: i64) -> Self {
        Self {
            dev_local_pty,
            now_ms,
            connection: Connection::Connecting,
            connection_detail: None,
            projects: Vec::new(),
            project: None,
            agents: BTreeMap::new(),
            tasks: BTreeMap::new(),
            runs: BTreeMap::new(),
            announcements: RingBuffer::new(ANNOUNCEMENT_CAPACITY),
            activity: BTreeMap::new(),
            units_selected: 0,
            jobs_selected: 0,
            focus: Panel::Units,
            mode: Mode::Normal,
            zoom_forwarding: true,
            status: None,
            caught_up: false,
            quit: false,
        }
    }

    // -- derived views -------------------------------------------------------------------

    fn role_rank(role: AgentRole) -> u8 {
        match role {
            AgentRole::Orchestrator => 0,
            AgentRole::Worker => 1,
        }
    }

    /// Agents in display order: the orchestrator(s) first (the "god" desk), then workers by id.
    #[must_use]
    pub fn units(&self) -> Vec<&AgentSnapshot> {
        let mut units: Vec<&AgentSnapshot> = self.agents.values().collect();
        units.sort_by(|a, b| {
            Self::role_rank(a.role)
                .cmp(&Self::role_rank(b.role))
                .then_with(|| a.id.as_str().cmp(b.id.as_str()))
        });
        units
    }

    /// Tasks in queue order (oldest first).
    #[must_use]
    pub fn jobs(&self) -> Vec<&TaskDetail> {
        let mut jobs: Vec<&TaskDetail> = self.tasks.values().collect();
        jobs.sort_by(|a, b| {
            a.snapshot
                .created_at_ms
                .cmp(&b.snapshot.created_at_ms)
                .then_with(|| a.snapshot.id.as_str().cmp(b.snapshot.id.as_str()))
        });
        jobs
    }

    #[must_use]
    pub fn selected_agent(&self) -> Option<&AgentSnapshot> {
        self.units().into_iter().nth(self.units_selected)
    }

    #[must_use]
    pub fn selected_task(&self) -> Option<&TaskDetail> {
        self.jobs().into_iter().nth(self.jobs_selected)
    }

    /// The agent's most recent run attempt by start time, or `None` if it has never run.
    #[must_use]
    pub fn latest_run_for(&self, agent_id: &AgentId) -> Option<&RunSnapshot> {
        self.runs
            .values()
            .filter(|run| &run.agent_id == agent_id)
            .max_by_key(|run| (run.started_at_ms, run.id.clone()))
    }

    #[must_use]
    pub fn state_of(&self, agent_id: &AgentId) -> AgentState {
        agent_state(self.latest_run_for(agent_id))
    }

    fn clamp_cursors(&mut self) {
        let units_len = self.agents.len();
        self.units_selected = if units_len == 0 {
            0
        } else {
            self.units_selected.min(units_len - 1)
        };
        let jobs_len = self.tasks.len();
        self.jobs_selected = if jobs_len == 0 {
            0
        } else {
            self.jobs_selected.min(jobs_len - 1)
        };
    }

    // -- status/help text -----------------------------------------------------------------

    fn set_status(&mut self, text: impl Into<String>, level: StatusLevel) {
        self.status = Some(StatusMessage {
            text: text.into(),
            level,
            at_ms: self.now_ms,
        });
    }

    /// A public status-line setter for `main.rs`, which has no other way to surface things like
    /// "couldn't spawn the zoom pane" (a PTY-level failure `Board` itself never sees).
    pub fn note_error(&mut self, text: impl Into<String>) {
        self.set_status(text, StatusLevel::Error);
    }

    /// The text the bottom status/help line should show right now: a recent status/error
    /// message if one is still "sticky", otherwise the key-help reminder for the current mode.
    #[must_use]
    pub fn status_line_text(&self) -> String {
        if let Some(status) = &self.status {
            if self.now_ms - status.at_ms < STATUS_STICKY_MS {
                return status.text.clone();
            }
        }
        self.help_text()
    }

    #[must_use]
    pub fn status_line_is_error(&self) -> bool {
        self.status.as_ref().is_some_and(|status| {
            status.level == StatusLevel::Error && self.now_ms - status.at_ms < STATUS_STICKY_MS
        })
    }

    /// The key-help reminder for the current mode (used as the status line's default text, and
    /// as a hint inside prompt overlays).
    #[must_use]
    pub fn help_text(&self) -> String {
        match &self.mode {
            Mode::Normal => "Tab switch  j/k move  Enter/l/z look  \
                s start  c cancel  r retry  a assign  x delete  n new  m message  S stop  q quit"
                .to_owned(),
            Mode::ConfirmDelete(_) => "press x again to delete, any other key cancels".to_owned(),
            Mode::Prompt(prompt) => format!(
                "{}: type to edit, Tab/Enter next field, Esc cancels",
                prompt.labels.get(prompt.field).unwrap_or(&"")
            ),
            Mode::Picker(picker) => match picker.kind {
                PickerKind::Project => "j/k move, Enter select project, q quit".to_owned(),
                PickerKind::AssignAgent(_) => "j/k move, Enter assign, Esc cancel".to_owned(),
            },
            Mode::Zoomed(_) if self.dev_local_pty && self.zoom_forwarding => {
                "Ctrl-] zoom-control mode".to_owned()
            }
            Mode::Zoomed(_) if self.dev_local_pty => {
                "Ctrl-] back to pane, Esc/z exit zoom".to_owned()
            }
            Mode::Zoomed(_) => "attach protocol pending; Esc/z/Enter exit zoom".to_owned(),
        }
    }

    // -- applying network state -------------------------------------------------------------

    pub fn set_retrying(&mut self, detail: impl Into<String>) {
        self.connection = Connection::Retrying;
        self.connection_detail = Some(detail.into());
    }

    pub fn set_live(&mut self) {
        self.connection = Connection::Live;
        self.connection_detail = None;
    }

    pub fn set_projects(&mut self, projects: Vec<ProjectSnapshot>) {
        self.projects = projects;
    }

    /// Selects a project (auto-selected because it was the only one, matched `--project`, or
    /// picked by the operator) and resets project-scoped state ready for a fresh snapshot load.
    pub fn select_project(&mut self, project: ProjectSnapshot) {
        self.project = Some(project);
        self.agents.clear();
        self.tasks.clear();
        self.runs.clear();
        self.activity.clear();
        self.announcements = RingBuffer::new(ANNOUNCEMENT_CAPACITY);
        self.units_selected = 0;
        self.jobs_selected = 0;
        self.mode = Mode::Normal;
    }

    pub fn apply_project_snapshot(
        &mut self,
        agents: Vec<AgentSnapshot>,
        tasks: Vec<TaskDetail>,
        runs: Vec<RunSnapshot>,
    ) {
        self.agents = agents
            .into_iter()
            .map(|agent| (agent.id.clone(), agent))
            .collect();
        self.tasks = tasks
            .into_iter()
            .map(|task| (task.snapshot.id.clone(), task))
            .collect();
        self.runs = runs.into_iter().map(|run| (run.id.clone(), run)).collect();
        self.clamp_cursors();
    }

    pub fn apply_event(&mut self, event: EventEnvelope) {
        if let Some(project) = &self.project {
            if event_project(&event.event) != &project.id {
                return;
            }
        } else {
            return;
        }

        if let Some(agent_id) = event_agent(&event.event) {
            self.activity
                .entry(agent_id.clone())
                .or_default()
                .record(event.occurred_at_ms);
        }

        if let Some(announcement) = format_event(&event) {
            self.announcements.push(announcement);
        }

        match event.event {
            FactoryEvent::TaskChanged { task } => {
                // `TaskChanged` only carries the durable snapshot, not `body`/`result` (those
                // live in `TaskDetail`, loaded separately) - preserve whatever we already have
                // for this task rather than blanking it out.
                let existing = self.tasks.get(&task.id);
                let body = existing.map_or_else(String::new, |detail| detail.body.clone());
                let result = existing.and_then(|detail| detail.result.clone());
                self.tasks.insert(
                    task.id.clone(),
                    TaskDetail {
                        snapshot: task,
                        body,
                        result,
                    },
                );
            }
            FactoryEvent::AgentChanged { agent } => {
                self.agents.insert(agent.id.clone(), agent);
            }
            FactoryEvent::RunChanged { run } => {
                self.runs.insert(run.id.clone(), run);
            }
            FactoryEvent::TaskDeleted { task_id, .. } => {
                self.tasks.remove(&task_id);
            }
            FactoryEvent::AgentDeleted { agent_id, .. } => {
                self.agents.remove(&agent_id);
                self.activity.remove(&agent_id);
            }
            // TRANSITION: sessions land in 5A/5C; the board has no session
            // state to update yet.
            FactoryEvent::SessionChanged { .. }
            | FactoryEvent::ProjectChanged { .. }
            | FactoryEvent::ProjectDeleted { .. } => {}
        }
        self.clamp_cursors();
    }

    pub fn apply_response(&mut self, result: Result<LocalResponse, String>) {
        match result {
            Ok(LocalResponse::Error { code, message }) => {
                self.set_status(
                    format!("{}: {message}", error_code_word(code)),
                    StatusLevel::Error,
                );
            }
            Ok(response) => {
                let text = self.merge_response(response);
                self.set_status(text, StatusLevel::Info);
            }
            Err(error) => {
                self.set_status(format!("request failed: {error}"), StatusLevel::Error);
            }
        }
        self.clamp_cursors();
    }

    /// Folds a successful response's payload back into board state immediately (in addition to
    /// the `TaskChanged`/`AgentChanged`/etc. event that will also arrive over the subscription;
    /// this just removes the round-trip latency for the client that made the request) and
    /// returns a short human-readable description for the status line.
    fn merge_response(&mut self, response: LocalResponse) -> String {
        match response {
            LocalResponse::TaskCreated { task } => {
                let id = task.snapshot.id.clone();
                self.tasks.insert(task.snapshot.id.clone(), task);
                format!("created task#{id}")
            }
            LocalResponse::Task { task }
            | LocalResponse::TaskRetried { task }
            | LocalResponse::TaskCancelled { task }
            | LocalResponse::TaskUpdated { task }
            | LocalResponse::TaskAssigned { task } => {
                let id = task.snapshot.id.clone();
                let status = task.snapshot.status;
                self.tasks.insert(task.snapshot.id.clone(), task);
                format!("task#{id} {}", task_status_word(status))
            }
            LocalResponse::TaskDeleted { task_id, .. } => {
                self.tasks.remove(&task_id);
                format!("deleted task#{task_id}")
            }
            LocalResponse::AgentCreated { agent } => {
                let id = agent.id.clone();
                self.agents.insert(agent.id.clone(), agent);
                format!("created agent {id}")
            }
            LocalResponse::Agent { agent } | LocalResponse::AgentProfileUpdated { agent } => {
                let id = agent.snapshot.id.clone();
                self.agents
                    .insert(agent.snapshot.id.clone(), agent.snapshot);
                format!("agent {id} updated")
            }
            LocalResponse::AgentDeleted { agent_id, .. } => {
                self.agents.remove(&agent_id);
                self.activity.remove(&agent_id);
                format!("removed agent {agent_id}")
            }
            LocalResponse::AgentMessageSent { message } => {
                format!("message sent to {}", message.recipient_agent_id)
            }
            LocalResponse::RunAccepted { run_id } => format!("started run {run_id}"),
            LocalResponse::RunStopped { run_id } => format!("stop requested for run {run_id}"),
            _ => "ok".to_owned(),
        }
    }

    // -- ticking --------------------------------------------------------------------------

    /// Called on every ~1s tick (and whenever `now_ms` otherwise needs bumping) so elapsed-time
    /// displays and sparklines keep moving even for idle agents.
    pub fn tick(&mut self, now_ms: i64) {
        self.now_ms = now_ms;
        for series in self.activity.values_mut() {
            series.roll_to(now_ms);
        }
    }

    // -- key handling -----------------------------------------------------------------------

    pub fn handle_key(&mut self, key: KeyEvent) -> Intent {
        match &self.mode {
            Mode::Zoomed(agent_id) => {
                let agent_id = agent_id.clone();
                self.handle_zoomed_key(key, &agent_id)
            }
            Mode::Normal => self.handle_normal_key(key),
            Mode::ConfirmDelete(task_id) => {
                let task_id = task_id.clone();
                self.handle_confirm_key(key, &task_id)
            }
            Mode::Prompt(_) => self.handle_prompt_key(key),
            Mode::Picker(_) => self.handle_picker_key(key),
        }
    }

    fn handle_zoomed_key(&mut self, key: KeyEvent, agent_id: &AgentId) -> Intent {
        if !self.dev_local_pty {
            return match key.code {
                KeyCode::Esc | KeyCode::Char('z' | 'q') | KeyCode::Enter => {
                    self.mode = Mode::Normal;
                    Intent::ExitZoom
                }
                _ => Intent::None,
            };
        }
        if key == crate::keys::PREFIX_KEY {
            self.zoom_forwarding = !self.zoom_forwarding;
            return Intent::Redraw;
        }
        if self.zoom_forwarding {
            return Intent::ForwardKey(key);
        }
        match key.code {
            KeyCode::Esc | KeyCode::Char('z') => {
                self.mode = Mode::Normal;
                let _ = agent_id;
                Intent::ExitZoom
            }
            _ => Intent::None,
        }
    }

    fn handle_normal_key(&mut self, key: KeyEvent) -> Intent {
        match key.code {
            KeyCode::Tab => {
                self.focus = match self.focus {
                    Panel::Units => Panel::Jobs,
                    Panel::Jobs => Panel::Units,
                };
                Intent::Redraw
            }
            KeyCode::Char('j') | KeyCode::Down => {
                self.move_cursor(1);
                Intent::Redraw
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.move_cursor(-1);
                Intent::Redraw
            }
            KeyCode::Enter | KeyCode::Char('l' | 'z') => self.look_at_selection(),
            KeyCode::Char('s') => self.start_selected_task(),
            KeyCode::Char('c') => self.cancel_selected_task(),
            KeyCode::Char('r') => self.retry_selected_task(),
            KeyCode::Char('a') => self.begin_assign(),
            KeyCode::Char('x') => self.begin_delete(),
            KeyCode::Char('n') => {
                self.mode = Mode::Prompt(PromptState::new_task());
                Intent::Redraw
            }
            KeyCode::Char('m') => self.begin_message(),
            KeyCode::Char('S') => self.stop_selected_agent(),
            KeyCode::Char('q') => {
                self.quit = true;
                Intent::Quit
            }
            _ => Intent::None,
        }
    }

    fn move_cursor(&mut self, delta: isize) {
        let (cursor, len) = match self.focus {
            Panel::Units => (&mut self.units_selected, self.agents.len()),
            Panel::Jobs => (&mut self.jobs_selected, self.tasks.len()),
        };
        if len == 0 {
            return;
        }
        let next = isize::try_from(*cursor).unwrap_or(0) + delta;
        *cursor = next.clamp(0, isize::try_from(len - 1).unwrap_or(0)) as usize;
    }

    fn require_project(&mut self) -> Option<ProjectId> {
        if let Some(project) = &self.project {
            Some(project.id.clone())
        } else {
            self.set_status("no project selected yet", StatusLevel::Error);
            None
        }
    }

    fn look_at_selection(&mut self) -> Intent {
        let target = match self.focus {
            Panel::Units => self.selected_agent().map(|agent| agent.id.clone()),
            Panel::Jobs => self
                .selected_task()
                .and_then(|task| task.snapshot.assigned_agent_id.clone()),
        };
        let Some(agent_id) = target else {
            self.set_status(
                "nothing to look at (task has no assigned agent)",
                StatusLevel::Error,
            );
            return Intent::Redraw;
        };
        self.mode = Mode::Zoomed(agent_id.clone());
        self.zoom_forwarding = true;
        Intent::EnterZoom(agent_id)
    }

    fn start_selected_task(&mut self) -> Intent {
        let Some(project_id) = self.require_project() else {
            return Intent::Redraw;
        };
        let Some(task) = self.selected_task() else {
            self.set_status("no task selected", StatusLevel::Error);
            return Intent::Redraw;
        };
        let Some(agent_id) = task.snapshot.assigned_agent_id.clone() else {
            self.set_status("assign a task to an agent first (a)", StatusLevel::Error);
            return Intent::Redraw;
        };
        let task_id = task.snapshot.id.clone();
        let Some(project) = &self.project else {
            return Intent::Redraw;
        };
        let worktree = project.root.clone();
        Intent::Send(LocalRequest::StartTask {
            project_id,
            task_id,
            agent_id,
            parent_run_id: None,
            worktree: Some(worktree),
        })
    }

    fn cancel_selected_task(&mut self) -> Intent {
        let Some(project_id) = self.require_project() else {
            return Intent::Redraw;
        };
        let Some(task) = self.selected_task() else {
            self.set_status("no task selected", StatusLevel::Error);
            return Intent::Redraw;
        };
        Intent::Send(LocalRequest::CancelTask {
            project_id,
            task_id: task.snapshot.id.clone(),
        })
    }

    fn retry_selected_task(&mut self) -> Intent {
        let Some(project_id) = self.require_project() else {
            return Intent::Redraw;
        };
        let Some(task) = self.selected_task() else {
            self.set_status("no task selected", StatusLevel::Error);
            return Intent::Redraw;
        };
        Intent::Send(LocalRequest::RetryTask {
            project_id,
            task_id: task.snapshot.id.clone(),
        })
    }

    fn begin_assign(&mut self) -> Intent {
        let Some(task) = self.selected_task() else {
            self.set_status("no task selected", StatusLevel::Error);
            return Intent::Redraw;
        };
        if self.agents.is_empty() {
            self.set_status("no agents to assign", StatusLevel::Error);
            return Intent::Redraw;
        }
        self.mode = Mode::Picker(PickerState {
            kind: PickerKind::AssignAgent(task.snapshot.id.clone()),
            cursor: 0,
        });
        Intent::Redraw
    }

    fn begin_delete(&mut self) -> Intent {
        let Some(task) = self.selected_task() else {
            self.set_status("no task selected", StatusLevel::Error);
            return Intent::Redraw;
        };
        self.mode = Mode::ConfirmDelete(task.snapshot.id.clone());
        Intent::Redraw
    }

    fn begin_message(&mut self) -> Intent {
        let Some(agent) = self.selected_agent() else {
            self.set_status("no agent selected", StatusLevel::Error);
            return Intent::Redraw;
        };
        self.mode = Mode::Prompt(PromptState::message_agent(agent.id.clone()));
        Intent::Redraw
    }

    fn stop_selected_agent(&mut self) -> Intent {
        let Some(project_id) = self.require_project() else {
            return Intent::Redraw;
        };
        let Some(agent) = self.selected_agent() else {
            self.set_status("no agent selected", StatusLevel::Error);
            return Intent::Redraw;
        };
        let Some(run_id) = agent.current_run_id.clone() else {
            self.set_status("agent has no active run", StatusLevel::Error);
            return Intent::Redraw;
        };
        Intent::Send(LocalRequest::StopRun {
            project_id,
            run_id,
            grace_ms: 5_000,
        })
    }

    fn handle_confirm_key(&mut self, key: KeyEvent, task_id: &TaskId) -> Intent {
        if key.code == KeyCode::Char('x') {
            self.mode = Mode::Normal;
            let Some(project_id) = self.require_project() else {
                return Intent::Redraw;
            };
            Intent::Send(LocalRequest::DeleteTask {
                project_id,
                task_id: task_id.clone(),
            })
        } else {
            self.mode = Mode::Normal;
            self.set_status("delete cancelled", StatusLevel::Info);
            Intent::Redraw
        }
    }

    fn handle_prompt_key(&mut self, key: KeyEvent) -> Intent {
        let Mode::Prompt(prompt) = &mut self.mode else {
            return Intent::None;
        };
        match key.code {
            KeyCode::Esc => {
                self.mode = Mode::Normal;
                self.set_status("cancelled", StatusLevel::Info);
                Intent::Redraw
            }
            KeyCode::Backspace => {
                prompt.values[prompt.field].pop();
                Intent::Redraw
            }
            KeyCode::Tab => {
                prompt.field = (prompt.field + 1) % prompt.labels.len();
                Intent::Redraw
            }
            KeyCode::Enter => {
                if prompt.field + 1 < prompt.labels.len() {
                    prompt.field += 1;
                    Intent::Redraw
                } else {
                    self.submit_prompt()
                }
            }
            KeyCode::Char(character)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                prompt.values[prompt.field].push(character);
                Intent::Redraw
            }
            _ => Intent::None,
        }
    }

    fn submit_prompt(&mut self) -> Intent {
        let Mode::Prompt(prompt) = std::mem::replace(&mut self.mode, Mode::Normal) else {
            return Intent::None;
        };
        let Some(project) = &self.project else {
            self.set_status("no project selected yet", StatusLevel::Error);
            return Intent::Redraw;
        };
        let project_id = project.id.clone();
        match prompt.kind {
            PromptKind::NewTask => {
                let title = prompt.values.first().cloned().unwrap_or_default();
                let body = prompt.values.get(1).cloned().unwrap_or_default();
                if title.trim().is_empty() {
                    self.set_status("task title can't be empty", StatusLevel::Error);
                    return Intent::Redraw;
                }
                let Ok(id) = TaskId::try_from(new_id()) else {
                    self.set_status("failed to generate a task id", StatusLevel::Error);
                    return Intent::Redraw;
                };
                Intent::Send(LocalRequest::CreateTask {
                    id,
                    project_id,
                    parent_task_id: None,
                    title,
                    body,
                    priority: 0,
                })
            }
            PromptKind::MessageAgent(recipient_agent_id) => {
                let body = prompt.values.first().cloned().unwrap_or_default();
                if body.trim().is_empty() {
                    self.set_status("message can't be empty", StatusLevel::Error);
                    return Intent::Redraw;
                }
                let Ok(id) = factory_core::MessageId::try_from(new_id()) else {
                    self.set_status("failed to generate a message id", StatusLevel::Error);
                    return Intent::Redraw;
                };
                Intent::Send(LocalRequest::SendAgentMessage {
                    id,
                    project_id,
                    sender_agent_id: None,
                    recipient_agent_id,
                    body,
                })
            }
        }
    }

    fn handle_picker_key(&mut self, key: KeyEvent) -> Intent {
        let Mode::Picker(picker) = &mut self.mode else {
            return Intent::None;
        };
        let len = match &picker.kind {
            PickerKind::Project => self.projects.len(),
            PickerKind::AssignAgent(_) => self.agents.len(),
        };
        match key.code {
            KeyCode::Esc => {
                self.mode = Mode::Normal;
                Intent::Redraw
            }
            KeyCode::Char('j') | KeyCode::Down => {
                if len > 0 {
                    picker.cursor = (picker.cursor + 1).min(len - 1);
                }
                Intent::Redraw
            }
            KeyCode::Char('k') | KeyCode::Up => {
                picker.cursor = picker.cursor.saturating_sub(1);
                Intent::Redraw
            }
            KeyCode::Enter => self.submit_picker(),
            KeyCode::Char('q') if picker.kind == PickerKind::Project => {
                self.quit = true;
                Intent::Quit
            }
            _ => Intent::None,
        }
    }

    fn submit_picker(&mut self) -> Intent {
        let Mode::Picker(picker) = &self.mode else {
            return Intent::None;
        };
        match &picker.kind {
            PickerKind::Project => {
                let Some(project) = self.projects.get(picker.cursor).cloned() else {
                    return Intent::Redraw;
                };
                let id = project.id.clone();
                self.select_project(project);
                Intent::ProjectSelected(id)
            }
            PickerKind::AssignAgent(task_id) => {
                let task_id = task_id.clone();
                let Some(agent) = self.units().into_iter().nth(picker.cursor).cloned() else {
                    self.mode = Mode::Normal;
                    return Intent::Redraw;
                };
                self.mode = Mode::Normal;
                let Some(project_id) = self.require_project() else {
                    return Intent::Redraw;
                };
                Intent::Send(LocalRequest::AssignTask {
                    project_id,
                    task_id,
                    agent_id: Some(agent.id),
                })
            }
        }
    }
}

fn new_id() -> String {
    uuid::Uuid::new_v4().hyphenated().to_string()
}

fn error_code_word(code: ErrorCode) -> &'static str {
    match code {
        ErrorCode::InvalidRequest => "invalid request",
        ErrorCode::UnsupportedProtocol => "unsupported protocol",
        ErrorCode::NotFound => "not found",
        ErrorCode::Conflict => "conflict",
        ErrorCode::Internal => "internal error",
    }
}

#[allow(clippy::too_many_lines)]
#[cfg(test)]
mod tests {
    use super::*;
    use factory_core::{AgentRole, ObserverHealth, Provider};

    fn agent_id(s: &str) -> AgentId {
        AgentId::try_from(s).unwrap()
    }

    fn task_id(s: &str) -> TaskId {
        TaskId::try_from(s).unwrap()
    }

    fn project_id(s: &str) -> ProjectId {
        ProjectId::try_from(s).unwrap()
    }

    fn run(status: RunStatus, agent: &str, started_at_ms: i64) -> RunSnapshot {
        RunSnapshot {
            id: RunId::try_from(format!("run-{agent}-{started_at_ms}")).unwrap(),
            project_id: project_id("proj"),
            agent_id: agent_id(agent),
            parent_run_id: None,
            task_id: None,
            session_id: None,
            closed_by: None,
            status,
            activity: None,
            wait_reason: None,
            worktree: "/work".into(),
            observer_health: ObserverHealth::Unknown,
            observer_health_since_ms: 0,
            started_at_ms,
            status_since_ms: started_at_ms,
            updated_at_ms: started_at_ms,
            ended_at_ms: None,
            exit_code: None,
            exit_signal: None,
            failure_reason: None,
        }
    }

    fn agent(id: &str, role: AgentRole, current_run_id: Option<&str>) -> AgentSnapshot {
        AgentSnapshot {
            id: agent_id(id),
            project_id: project_id("proj"),
            parent_agent_id: None,
            role,
            provider: Provider::ClaudeCode,
            current_run_id: current_run_id.map(|id| RunId::try_from(id.to_owned()).unwrap()),
            paused: false,
            current_session_id: None,
            worktree: None,
            created_at_ms: 0,
            updated_at_ms: 0,
        }
    }

    fn task(
        id: &str,
        status: TaskStatus,
        assigned: Option<&str>,
        created_at_ms: i64,
    ) -> TaskDetail {
        TaskDetail {
            snapshot: factory_core::TaskSnapshot {
                id: task_id(id),
                project_id: project_id("proj"),
                parent_task_id: None,
                assigned_agent_id: assigned.map(agent_id),
                title: id.to_owned(),
                status,
                priority: 0,
                created_at_ms,
                updated_at_ms: created_at_ms,
            },
            body: String::new(),
            result: None,
        }
    }

    fn project(id: &str) -> ProjectSnapshot {
        ProjectSnapshot {
            id: project_id(id),
            name: id.to_owned(),
            root: "/work".into(),
            created_at_ms: 0,
            updated_at_ms: 0,
        }
    }

    // -- agent_state --------------------------------------------------------------------

    #[test]
    fn agent_state_maps_no_run_to_idle() {
        assert_eq!(agent_state(None), AgentState::Idle);
    }

    #[test]
    fn agent_state_maps_run_statuses() {
        let cases = [
            (RunStatus::Starting, AgentState::Working),
            (RunStatus::Running, AgentState::Working),
            (RunStatus::Waiting, AgentState::Waiting),
            (RunStatus::Blocked, AgentState::Waiting),
            (RunStatus::Paused, AgentState::Waiting),
            (RunStatus::Succeeded, AgentState::Idle),
            (RunStatus::Failed, AgentState::Failed),
            (RunStatus::Stopped, AgentState::Stopped),
        ];
        for (status, expected) in cases {
            let snapshot = run(status, "alice", 0);
            assert_eq!(agent_state(Some(&snapshot)), expected, "{status:?}");
        }
    }

    #[test]
    fn latest_run_for_picks_most_recent_by_start_time() {
        let mut board = Board::new(false, 0);
        board.project = Some(project("proj"));
        board.runs.insert(
            RunId::try_from("r1".to_owned()).unwrap(),
            run(RunStatus::Failed, "alice", 100),
        );
        board.runs.insert(
            RunId::try_from("r2".to_owned()).unwrap(),
            run(RunStatus::Running, "alice", 200),
        );
        board.runs.insert(
            RunId::try_from("r3".to_owned()).unwrap(),
            run(RunStatus::Succeeded, "bob", 500),
        );
        assert_eq!(board.state_of(&agent_id("alice")), AgentState::Working);
        assert_eq!(board.state_of(&agent_id("bob")), AgentState::Idle);
        assert_eq!(board.state_of(&agent_id("carol")), AgentState::Idle);
    }

    // -- ring buffer ----------------------------------------------------------------------

    #[test]
    fn ring_buffer_drops_oldest_past_capacity() {
        let mut buf = RingBuffer::new(3);
        for value in 0..5 {
            buf.push(value);
        }
        assert_eq!(buf.len(), 3);
        assert_eq!(buf.iter().copied().collect::<Vec<_>>(), vec![2, 3, 4]);
    }

    #[test]
    fn ring_buffer_capacity_is_at_least_one() {
        let mut buf: RingBuffer<i32> = RingBuffer::new(0);
        buf.push(1);
        buf.push(2);
        assert_eq!(buf.iter().copied().collect::<Vec<_>>(), vec![2]);
    }

    // -- sparkline bucketing ----------------------------------------------------------------

    #[test]
    fn activity_series_buckets_events_per_minute() {
        let mut series = ActivitySeries::default();
        series.record(0);
        series.record(30_000);
        series.record(65_000);
        let counts = series.counts();
        assert_eq!(counts, vec![2, 1]);
    }

    #[test]
    fn activity_series_rolls_forward_on_idle_ticks() {
        let mut series = ActivitySeries::default();
        series.record(0);
        series.roll_to(3 * ACTIVITY_BUCKET_MS);
        let counts = series.counts();
        assert_eq!(counts, vec![1, 0, 0, 0]);
    }

    #[test]
    fn activity_series_caps_window_length() {
        let mut series = ActivitySeries::default();
        for minute in 0..(ACTIVITY_WINDOW as i64 + 10) {
            series.record(minute * ACTIVITY_BUCKET_MS);
        }
        assert_eq!(series.counts().len(), ACTIVITY_WINDOW);
    }

    #[test]
    fn braille_sparkline_quantizes_and_pads() {
        let counts = [0, 1, 2, 4];
        let rendered = braille_sparkline(&counts, 6);
        assert_eq!(rendered.chars().count(), 6);
        // Two leading pad (empty) columns, then an ascending gradient ending at full.
        assert_eq!(rendered.chars().next(), Some(BRAILLE_LEVELS[0]));
        assert_eq!(rendered.chars().last(), Some(BRAILLE_LEVELS[7]));
    }

    #[test]
    fn braille_sparkline_all_zero_stays_empty() {
        let counts = [0, 0, 0];
        let rendered = braille_sparkline(&counts, 3);
        assert!(rendered.chars().all(|glyph| glyph == BRAILLE_LEVELS[0]));
    }

    // -- key handling in Control (Normal) mode -----------------------------------------------

    fn board_with_two_agents_and_task() -> Board {
        let mut board = Board::new(false, 0);
        board.project = Some(project("proj"));
        board
            .agents
            .insert(agent_id("alice"), agent("alice", AgentRole::Worker, None));
        board
            .agents
            .insert(agent_id("bob"), agent("bob", AgentRole::Worker, None));
        board
            .tasks
            .insert(task_id("t1"), task("t1", TaskStatus::Queued, None, 0));
        board
    }

    #[test]
    fn j_k_move_the_focused_panel_cursor() {
        let mut board = board_with_two_agents_and_task();
        assert_eq!(board.focus, Panel::Units);
        assert_eq!(board.units_selected, 0);
        board.handle_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE));
        assert_eq!(board.units_selected, 1);
        board.handle_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE));
        assert_eq!(board.units_selected, 1, "cursor should clamp at the end");
        board.handle_key(KeyEvent::new(KeyCode::Char('k'), KeyModifiers::NONE));
        assert_eq!(board.units_selected, 0);
    }

    #[test]
    fn tab_switches_focus_between_units_and_jobs() {
        let mut board = board_with_two_agents_and_task();
        board.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        assert_eq!(board.focus, Panel::Jobs);
        board.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        assert_eq!(board.focus, Panel::Units);
    }

    #[test]
    fn s_without_assignment_sets_an_error_status_and_sends_nothing() {
        let mut board = board_with_two_agents_and_task();
        board.focus = Panel::Jobs;
        let intent = board.handle_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::NONE));
        assert!(matches!(intent, Intent::Redraw));
        assert!(board.status_line_is_error());
    }

    #[test]
    fn s_on_an_assigned_task_sends_start_task_with_project_root_as_worktree() {
        let mut board = board_with_two_agents_and_task();
        board.tasks.insert(
            task_id("t1"),
            task("t1", TaskStatus::Queued, Some("alice"), 0),
        );
        board.focus = Panel::Jobs;
        let intent = board.handle_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::NONE));
        match intent {
            Intent::Send(LocalRequest::StartTask {
                agent_id,
                worktree,
                task_id: sent_task_id,
                ..
            }) => {
                assert_eq!(agent_id.as_str(), "alice");
                assert_eq!(worktree.as_deref(), Some("/work"));
                assert_eq!(sent_task_id.as_str(), "t1");
            }
            other => panic!("expected StartTask, got {other:?}"),
        }
    }

    #[test]
    fn x_then_x_confirms_delete_and_sends_delete_task() {
        let mut board = board_with_two_agents_and_task();
        board.focus = Panel::Jobs;
        let first = board.handle_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE));
        assert!(matches!(first, Intent::Redraw));
        assert!(matches!(board.mode, Mode::ConfirmDelete(_)));
        let second = board.handle_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE));
        assert!(matches!(
            second,
            Intent::Send(LocalRequest::DeleteTask { .. })
        ));
        assert!(matches!(board.mode, Mode::Normal));
    }

    #[test]
    fn x_then_other_key_cancels_delete() {
        let mut board = board_with_two_agents_and_task();
        board.focus = Panel::Jobs;
        board.handle_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE));
        let intent = board.handle_key(KeyEvent::new(KeyCode::Char('k'), KeyModifiers::NONE));
        assert!(matches!(intent, Intent::Redraw));
        assert!(matches!(board.mode, Mode::Normal));
    }

    #[test]
    fn n_opens_new_task_prompt_and_two_fields_submit_create_task() {
        let mut board = board_with_two_agents_and_task();
        board.handle_key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE));
        assert!(matches!(board.mode, Mode::Prompt(_)));
        for ch in "fix bug".chars() {
            board.handle_key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE));
        }
        board.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        for ch in "do the thing".chars() {
            board.handle_key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE));
        }
        let intent = board.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        match intent {
            Intent::Send(LocalRequest::CreateTask { title, body, .. }) => {
                assert_eq!(title, "fix bug");
                assert_eq!(body, "do the thing");
            }
            other => panic!("expected CreateTask, got {other:?}"),
        }
        assert!(matches!(board.mode, Mode::Normal));
    }

    #[test]
    fn empty_task_title_is_rejected() {
        let mut board = board_with_two_agents_and_task();
        board.handle_key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE));
        // First Enter just advances title -> body (title is still empty); second Enter submits.
        board.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        let intent = board.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(matches!(intent, Intent::Redraw));
        assert!(board.status_line_is_error());
    }

    #[test]
    fn a_opens_assign_picker_and_enter_sends_assign_task() {
        let mut board = board_with_two_agents_and_task();
        board.focus = Panel::Jobs;
        board.handle_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE));
        assert!(matches!(board.mode, Mode::Picker(_)));
        board.handle_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE));
        let intent = board.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        match intent {
            Intent::Send(LocalRequest::AssignTask { agent_id, .. }) => {
                assert_eq!(agent_id.unwrap().as_str(), "bob");
            }
            other => panic!("expected AssignTask, got {other:?}"),
        }
    }

    #[test]
    fn m_opens_message_prompt_targeting_the_selected_agent() {
        let mut board = board_with_two_agents_and_task();
        board.handle_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE)); // select bob
        board.handle_key(KeyEvent::new(KeyCode::Char('m'), KeyModifiers::NONE));
        match &board.mode {
            Mode::Prompt(prompt) => {
                assert_eq!(prompt.kind, PromptKind::MessageAgent(agent_id("bob")));
            }
            other => panic!("expected Prompt, got {other:?}"),
        }
        for ch in "hello".chars() {
            board.handle_key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE));
        }
        let intent = board.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        match intent {
            Intent::Send(LocalRequest::SendAgentMessage {
                recipient_agent_id,
                body,
                ..
            }) => {
                assert_eq!(recipient_agent_id.as_str(), "bob");
                assert_eq!(body, "hello");
            }
            other => panic!("expected SendAgentMessage, got {other:?}"),
        }
    }

    #[test]
    fn capital_s_stops_the_selected_agents_current_run() {
        let mut board = board_with_two_agents_and_task();
        board.agents.insert(
            agent_id("alice"),
            agent("alice", AgentRole::Worker, Some("run-1")),
        );
        let intent = board.handle_key(KeyEvent::new(KeyCode::Char('S'), KeyModifiers::NONE));
        match intent {
            Intent::Send(LocalRequest::StopRun { run_id, .. }) => {
                assert_eq!(run_id.as_str(), "run-1");
            }
            other => panic!("expected StopRun, got {other:?}"),
        }
    }

    #[test]
    fn capital_s_without_a_current_run_errors() {
        let mut board = board_with_two_agents_and_task();
        let intent = board.handle_key(KeyEvent::new(KeyCode::Char('S'), KeyModifiers::NONE));
        assert!(matches!(intent, Intent::Redraw));
        assert!(board.status_line_is_error());
    }

    #[test]
    fn q_quits() {
        let mut board = board_with_two_agents_and_task();
        let intent = board.handle_key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE));
        assert!(matches!(intent, Intent::Quit));
        assert!(board.quit);
    }

    #[test]
    fn enter_on_units_zooms_the_selected_agent() {
        let mut board = board_with_two_agents_and_task();
        let intent = board.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        match intent {
            Intent::EnterZoom(agent_id) => assert_eq!(agent_id.as_str(), "alice"),
            other => panic!("expected EnterZoom, got {other:?}"),
        }
        assert!(matches!(board.mode, Mode::Zoomed(_)));
    }

    #[test]
    fn zoom_without_dev_local_pty_exits_on_escape_without_forwarding() {
        let mut board = board_with_two_agents_and_task();
        board.mode = Mode::Zoomed(agent_id("alice"));
        let intent = board.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(matches!(intent, Intent::ExitZoom));
        assert!(matches!(board.mode, Mode::Normal));
    }

    #[test]
    fn zoom_with_dev_local_pty_forwards_keys_until_prefix_toggles_control() {
        let mut board = Board::new(true, 0);
        board.mode = Mode::Zoomed(agent_id("alice"));
        board.zoom_forwarding = true;
        let forwarded = board.handle_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE));
        assert!(matches!(forwarded, Intent::ForwardKey(_)));
        let toggled = board.handle_key(crate::keys::PREFIX_KEY);
        assert!(matches!(toggled, Intent::Redraw));
        assert!(!board.zoom_forwarding);
        let exited = board.handle_key(KeyEvent::new(KeyCode::Char('z'), KeyModifiers::NONE));
        assert!(matches!(exited, Intent::ExitZoom));
    }
}
