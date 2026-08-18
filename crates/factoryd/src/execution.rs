//! Session spawn, dispatch, and delivery: the daemon-side half of resident
//! interactive sessions (see `TRACK5-DESIGN.md`, `TRACK5-WIRE.md`).
//!
//! A single background dispatcher task (spawned by [`spawn`]) owns every
//! agent's delivery: on a wake trigger (task assigned/retried, message
//! sent, agent resumed, a session's `SessionStart` hook, or the 5 second
//! safety tick) it spawns a resident session for an agent with no live one
//! and pending work, or -- for an agent whose session is already `idle` --
//! composes and PTY-types the next deliverable, waiting for the provider's
//! own hook to acknowledge receipt before committing the delivery durably
//! (`compose_delivery`/`commit_delivery`). A session that is `working` or
//! `waiting_for_input` is instead delivered into via the `Stop`/
//! `SubagentStop` hook's reply (see [`stop_hook_reply`], called directly
//! from `local_api.rs`'s `ProviderHook` handler -- that is the seam 5A left
//! for this track).
//!
//! What replaced the old per-run ephemeral-runner supervision loop this
//! module used to own (deleted by 5A, see `TRACK5-DESIGN.md` §7 and `git
//! show 364274e:crates/factoryd/src/execution.rs`): a freshly spawned
//! session's liveness is now just its `tokio::process::Child` handle
//! (`supervise_child`, far simpler than the old subscribe/replay loop,
//! because hooks -- not decoded stream-JSON -- are the state source now).
//! Only *recovered* sessions (no `Child` handle survives a daemon restart)
//! still need the old subscribe/reconnect pattern; `supervise_recovered`
//! and its `attach_with_grace`/`endpoint_absent`/`classify_unavailable`
//! helpers below are ported from that history, trimmed to "watch for
//! `RunnerEvent::Exited`" since there is no decoder to feed anymore.

use std::{
    collections::HashMap,
    fs, io,
    os::unix::{
        fs::{DirBuilderExt, FileTypeExt, MetadataExt},
        process::ExitStatusExt,
    },
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};

use factory_core::{
    AgentId, AgentRole, EventEnvelope, FactoryEvent, ProjectId, Provider, ProviderHookEvent, RunId,
    RunnerInstanceId, SessionId, SessionSnapshot, SessionState, TaskDetail,
    runner::{RunnerEvent, RunnerEventEnvelope, TerminalSize, encode_terminal_bytes},
};
use thiserror::Error;
use tokio::{
    sync::{broadcast, mpsc, watch},
    task::JoinHandle,
    time::{Instant, sleep_until, timeout, timeout_at},
};
use uuid::Uuid;

use crate::{
    daemon_state::{DaemonState, DaemonStateError},
    guidance,
    providers::{self, SpawnContext, hooks},
    runner_client::{RunnerClient, RunnerClientError, RunnerStreamItem, RunnerSubscription},
    runner_process::{self, LaunchSpec, ProviderEnvironment},
    store::{RecoverableSession, SessionRow, StoreError},
};

const PRIVATE_DIRECTORY_MODE: u32 = 0o700;
/// How long PTY-typed delivery waits for a matching `UserPromptSubmit` hook
/// before retrying once (TRACK5-DESIGN.md §3/A3).
const ACK_TIMEOUT: Duration = Duration::from_secs(20);
/// Gap between a composed delivery's text and its submitting `\r`, sent as
/// two separate `TerminalInput` writes (`type_and_await_ack`'s doc comment
/// has the why: real Claude Code's paste-vs-keystroke heuristic otherwise
/// absorbs a `\r` inside the same burst as just another newline).
const SUBMIT_DELAY: Duration = Duration::from_millis(80);
/// Safety-net reconciliation sweep; a reconciler, not the source of truth
/// (TRACK5-WIRE.md) -- event-driven wakes are expected to beat this in the
/// common case.
const TICK_INTERVAL: Duration = Duration::from_secs(5);
const WAKE_CHANNEL_CAPACITY: usize = 512;
const CONNECT_GRACE: Duration = Duration::from_secs(5);
const CONNECT_RETRY_DELAY: Duration = Duration::from_millis(50);
const RECOVERY_RETRY_DELAY: Duration = Duration::from_millis(250);
const MAX_RECOVERY_RETRY_DELAY: Duration = Duration::from_secs(30);
/// Bounds `supervise_recovered`'s reconnect loop (this track's item 9):
/// without this, a recovered session whose runner is permanently
/// unreachable (its process is actually gone, but the runtime
/// directory/socket did not classify as cleanly `Missing` -- e.g. the
/// socket file lingers but nothing answers) retried forever at
/// [`MAX_RECOVERY_RETRY_DELAY`], staying `starting`/whatever state it was
/// recovered in visible forever, never durably `failed`. 10 attempts is
/// comfortably past the point [`next_retry_delay`]'s doubling has already
/// capped the delay at 30s (attempt 8), so this adds only ~1 extra minute
/// of genuine retrying beyond that plateau before giving up.
const MAX_RECOVERY_ATTEMPTS: u32 = 10;
/// Never spawned under a startup-input deadline (terminal-mode launches
/// carry no startup input to time out on); kept only because
/// `runner_process::spawn_runner` requires a value.
const STARTUP_GRACE: Duration = Duration::from_secs(30);
/// Comfortably under `factory_core::runner::MAX_TERMINAL_INPUT_BYTES`
/// (64 KiB, pre-base64) so a large task body plus guidance files can never
/// make a PTY-typed delivery itself fail; long content stays fully readable
/// via `factoryctl task get`/`agent inbox`, just not fully retyped.
const MAX_DELIVERY_TEXT_BYTES: usize = 48_000;
/// Per-task-body budget inside a composed delivery, leaving room for
/// guidance sections within [`MAX_DELIVERY_TEXT_BYTES`].
const MAX_DELIVERY_TASK_BODY_BYTES: usize = 16_384;
/// Mirrors `store.rs`'s own private `MAX_WAIT_REASON_BYTES` (the
/// `sessions.wait_reason`/`activity` CHECK bound, migration 0014): a spawn
/// failure's error text (`spawn_session_for_agent`) is bounded before it
/// ever reaches `Store::end_session_with_reason`, so an unusually long OS
/// error message can never itself turn a spawn failure into a second,
/// confusing store error.
const MAX_WAIT_REASON_BYTES: usize = 512;
/// Default for [`Config::session_start_deadline`] (issue #24): how long a
/// session may sit `starting` before the daemon gives up on its
/// `SessionStart` hook ever arriving and treats it exactly like a failed
/// spawn attempt. A real Codex session was observed whose TUI rendered and
/// sat fully idle, hooks never firing, for several minutes -- root cause
/// not established, but "starting forever" must not be a reachable steady
/// state regardless. 120s is generous enough for a cold Codex start with
/// many MCP servers/plugins syncing. Not an operator flag, env var, or
/// config file key (AGENTS.md rule 3) -- [`Config::session_start_deadline`]
/// exists only so tests can shorten it; production always uses this
/// constant (`main.rs`).
pub const SESSION_START_DEADLINE: Duration = Duration::from_secs(120);
/// After this many *consecutive* [`SESSION_START_DEADLINE`] expiries in a
/// row for the same agent, [`enforce_start_deadline`] pauses the agent
/// (`Store::pause_agent`) instead of respawning again -- adversarial
/// review of #24 (finding 4): a hookless provider spawns successfully
/// every time, so an ordinary spawn failure's backoff never gets anything
/// to escalate to on its own, and left alone this cycles forever at
/// [`SESSION_START_DEADLINE`]'s cadence, killing and relaunching a real
/// provider process every ~2 minutes. 3 is one generous benefit of the
/// doubt -- a provider that is merely slow to initialize losing the
/// `SessionStart` race twice running is unlikely -- before treating it as
/// persistently broken; `factoryctl agent resume` is the documented way
/// back in and resets the streak (`Handle::resume_backoff`).
const MAX_CONSECUTIVE_START_DEADLINES: u32 = 3;
const ORCHESTRATOR_FOOTER: &str = "As the orchestrator, coordinate the project via `factoryctl` \
(DARK_FACTORY_PROJECT/DARK_FACTORY_AGENT/DARK_FACTORY_SOCKET are already set in this session, so \
--project/--agent are usually optional): `factoryctl task add --title T --body B`, `factoryctl \
agent add --role worker --provider <claude|codex|shell>`, `factoryctl task assign --task <id> \
--agent <agent>`, `factoryctl agent message --to <agent> --body \"...\"`, `factoryctl session \
list`. A worker in `waiting_for_input` needs operator attention: message it or surface its request; \
do not stop, restart, replace, or duplicate it. Before stopping or replacing any worker, inspect \
`factoryctl agent status --agent <agent>` and preserve or explicitly resolve any dirty worktree.";

/// Fixed process and durability bounds for the dispatcher.
pub struct Config {
    /// Trusted absolute path to the installed `factory-runner` executable.
    pub runner_program: PathBuf,
    /// Trusted absolute path to `factoryctl`, embedded in generated hook
    /// commands and exported as `DARK_FACTORY_FACTORYCTL` (see
    /// `docs/providers.md`'s "provider A1" resolution and
    /// `providers::shell::ShellProvider`).
    pub factoryctl_path: PathBuf,
    /// Root directory sessions' runner runtime directories live under,
    /// keyed by session id.
    pub runtime_root: PathBuf,
    /// `$DARK_FACTORY_HOME`: root of the project/agent guidance tree (see
    /// `factory_core::paths` and `guidance`).
    pub guidance_root: PathBuf,
    /// The daemon's own local control socket path, exported as
    /// `DARK_FACTORY_SOCKET` so an agent's own `factoryctl` invocations
    /// (`task done`, `agent message`, ...) can reach the daemon without
    /// relying on `$DARK_FACTORY_HOME` resolution inside the session.
    pub socket_path: PathBuf,
    /// Daemon-wide cap on live sessions (`ended_at_ms IS NULL`, across every
    /// project), enforced by [`dispatch_agent`]: an agent with pending work
    /// and no live session is left alone -- not spawned, not backed off --
    /// while [`Store::live_session_count`](crate::store::Store::live_session_count)
    /// is already at or above this value. The 5 second safety tick retries
    /// automatically once a session ends and count drops, so this is a
    /// resource bound, not a hard failure (`--max-active-runs`, README's
    /// "Local control plane").
    pub max_active_runs: usize,
    /// How long a session may stay `starting` before [`dispatch_agent`]
    /// treats it as a failed spawn attempt (issue #24, see
    /// [`SESSION_START_DEADLINE`]'s doc comment). A struct field rather
    /// than a bare constant purely so a test can shorten it; every real
    /// caller (`main.rs`) passes [`SESSION_START_DEADLINE`] -- there is
    /// deliberately no CLI flag, environment variable, or config file key
    /// for this (AGENTS.md rule 3).
    pub session_start_deadline: Duration,
}

/// One explicit queued task to deliver now into its agent's live, idle
/// session -- the operator's "deliver now" override of ordinary per-agent
/// FIFO auto-delivery (TRACK5-WIRE.md D2). `parent_run_id` and `worktree`
/// are accepted for wire compatibility but unused: a live session's cwd is
/// fixed at spawn time, not re-set per task.
pub struct StartTask {
    pub project_id: ProjectId,
    pub task_id: factory_core::TaskId,
    pub agent_id: AgentId,
    pub parent_run_id: Option<RunId>,
    pub worktree: PathBuf,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StartedRun {
    pub run_id: RunId,
}

#[derive(Debug, Error)]
pub enum Error {
    #[error("execution concurrency must be greater than zero")]
    InvalidConcurrency,
    #[error("runner runtime root is not a private owner-only directory")]
    InvalidRuntimeRoot,
    #[error("daemon state failed: {0}")]
    State(#[from] DaemonStateError),
    #[error("execution manager has stopped")]
    ManagerStopped,
    #[error("agent has no live session")]
    NoLiveSession,
    #[error("agent's live session is not idle")]
    SessionBusy,
    #[error("a delivery for this agent is already in flight; retry shortly")]
    DeliveryInProgress,
    #[error("agent has no worktree; create one first")]
    NoWorktree,
    #[error("system clock is before the Unix epoch")]
    InvalidClock,
    #[error("provider launch failed: {0}")]
    Provider(#[from] providers::ProviderError),
    #[error("could not write session hook token: {0}")]
    HookToken(#[from] hooks::HookTokenError),
    #[error("runner launch failed: {0}")]
    Spawn(#[from] runner_process::Error),
    #[error("runner control failed: {0}")]
    Runner(#[from] RunnerClientError),
    #[error("durable execution metadata is inconsistent")]
    CorruptExecution,
    #[error("generated id is invalid")]
    InvalidId,
    #[error(
        "timed out waiting for an in-flight session spawn to finish before deleting this agent"
    )]
    DeleteDrainTimeout,
}

/// One agent to reconsider for delivery: spawn a session if it has none and
/// pending work, or attempt PTY-typed delivery if its session is idle.
struct WakeAgent {
    project_id: ProjectId,
    agent_id: AgentId,
}

fn send_wake(wake_tx: &mpsc::Sender<WakeAgent>, project_id: ProjectId, agent_id: AgentId) {
    let _ = wake_tx.try_send(WakeAgent {
        project_id,
        agent_id,
    });
}

/// Bounded handle for session spawn, dispatch, and delivery.
#[derive(Clone)]
pub struct Handle {
    state: DaemonState,
    config: Arc<Config>,
    wake_tx: mpsc::Sender<WakeAgent>,
    shutdown: watch::Sender<bool>,
    /// Shared with [`run_dispatcher`]'s own [`SpawnBackoff`] (ARCHITECTURE.md
    /// invariant 9's agent-scoped mechanism): lets
    /// [`Handle::begin_delete`]/[`Handle::end_delete`]/
    /// [`Handle::try_begin_agent_write`]/[`Handle::end_agent_write`],
    /// called from `local_api.rs`'s `DeleteAgent`, `DeleteProject`,
    /// `GetAgent`/`AgentStatus`, `UpdateAgentProfile`, and `CreateAgent`
    /// handlers, mark an agent so no writer -- spawn preparation
    /// (`dispatch_agent`), an idle-session delivery (`deliver_pending`), or
    /// one of those handlers -- ever begins a new write into its guidance
    /// directory while it is being deleted, and wait for one already in
    /// flight to finish -- the same style of seam `local_api.rs` already
    /// uses for [`stop_hook_reply`] and [`Handle::wake`].
    backoff: Arc<SpawnBackoff>,
    /// Project-scoped half of the same invariant (PR #50 review, finding
    /// 3): guards `CreateAgent`, the one writer under a project that
    /// `backoff`'s per-agent marks can never already cover, because the
    /// agent it is about to create does not exist yet for `DeleteProject`
    /// to have marked. See [`Handle::begin_delete_project`].
    project_gate: Arc<DeleteGate<ProjectId>>,
}

impl Handle {
    /// The trusted, already-preflight-checked (`main.rs`'s
    /// `preflight_sibling_binaries`) path to `factory-runner`. Exposed for
    /// `LocalRequest::Health` (`local_api.rs`) so `factoryctl health`
    /// reports exactly what the daemon is actually using, not just its
    /// own configuration.
    #[must_use]
    pub fn runner_program(&self) -> &Path {
        &self.config.runner_program
    }

    /// The trusted, already-preflight-checked path to `factoryctl`. See
    /// [`Handle::runner_program`].
    #[must_use]
    pub fn factoryctl_path(&self) -> &Path {
        &self.config.factoryctl_path
    }

    /// The daemon-wide live-session cap (`factoryd --max-active-runs`).
    #[must_use]
    pub fn max_active_runs(&self) -> usize {
        self.config.max_active_runs
    }

    /// Opens a task-episode inside the agent's live, idle session right
    /// now (bypassing FIFO order -- the operator asked for this exact
    /// task), then makes a best-effort attempt to type its instructions in
    /// and wait for acknowledgement before returning. The episode is
    /// durable once this returns `Ok` even if the best-effort typing did
    /// not: a stuck delivery surfaces as the session going
    /// `waiting_for_input`, observable in `session list`/the TUI, not as a
    /// lost task.
    pub async fn start_task(&self, input: StartTask) -> Result<StartedRun, Error> {
        let StartTask {
            project_id,
            agent_id,
            task_id,
            ..
        } = input;
        self.wake(project_id.clone(), agent_id.clone());

        let lookup_project_id = project_id.clone();
        let lookup_agent_id = agent_id.clone();
        let session = self
            .state
            .with_store(move |store| {
                store.live_session_for_agent(&lookup_project_id, &lookup_agent_id)
            })
            .await?
            .ok_or(Error::NoLiveSession)?;
        if session.state != SessionState::Idle {
            return Err(Error::SessionBusy);
        }
        // Single pending-delivery slot (this track's item 1): held for the
        // rest of this call, so a concurrent `Stop`/`SubagentStop` hook
        // reply racing this exact agent (`stop_hook_reply`, called
        // directly from `local_api.rs`, not through this dispatcher) can
        // never independently compose and deliver the same pending
        // task/messages while this explicit operator request is already
        // mid-flight.
        let Some(_delivery_slot) = self.state.try_delivery_slot(&agent_id) else {
            return Err(Error::DeliveryInProgress);
        };

        let opened_at_ms = now_ms()?;
        let session_id = session.id.clone();
        let open_task_id = task_id.clone();
        let opened = self
            .state
            .commit_and_publish(move |store| {
                let opened = store.open_run_episode(&session_id, &open_task_id, opened_at_ms)?;
                let events = opened.events.clone();
                Ok((opened, events))
            })
            .await?;
        let run_id = opened.run.id.clone();

        let text = compose_text(
            &self.config.guidance_root,
            &project_id,
            &agent_id,
            Some(&opened.task),
            &opened.agent_messages,
            role_hint(&self.state, &project_id, &agent_id).await,
        );
        let target = self
            .state
            .with_store({
                let project_id = project_id.clone();
                let session_id = session.id.clone();
                move |store| store.session_control_target(&project_id, &session_id)
            })
            .await?;
        let client = RunnerClient::new(
            &target.runner_runtime,
            session_run_id(&session.id)?,
            target.runner_instance_id,
        );
        if !type_and_await_ack(&self.state, &client, &session.id, &text).await {
            let reason = "delivery unacknowledged".to_owned();
            let wait_session_id = session.id.clone();
            let wait_at_ms = now_ms()?;
            let _ = self
                .state
                .commit_and_publish(move |store| {
                    let (_, event) =
                        store.mark_session_waiting(&wait_session_id, reason, wait_at_ms)?;
                    Ok(((), vec![event]))
                })
                .await;
        }
        Ok(StartedRun { run_id })
    }

    /// Best-effort: enqueues `agent_id` for the dispatcher's next pass
    /// (spawn if it has no live session and pending work, or attempt
    /// delivery if its session is idle). Never blocks and never fails --
    /// a full wake queue silently defers to the 5 second safety tick.
    pub fn wake(&self, project_id: ProjectId, agent_id: AgentId) {
        send_wake(&self.wake_tx, project_id, agent_id);
    }

    /// Begins deletion of `agent_id` (ARCHITECTURE.md invariant 9's
    /// agent-scoped mechanism): marks the agent so no writer --
    /// [`dispatch_agent`]'s spawn preparation, [`deliver_pending`]'s
    /// delivery composition, or a handler using
    /// [`Handle::try_begin_agent_write`] -- can begin a new write into its
    /// guidance directory, then waits up to [`DELETE_DRAIN_TIMEOUT`] for a
    /// write already in flight to finish, so a caller that gets `Ok` back
    /// knows nothing can write into the agent's guidance directory before
    /// [`Handle::end_delete`] is called. On timeout, clears its own mark
    /// (so the agent is not left permanently undispatchable by a delete
    /// that never completed) and returns [`Error::DeleteDrainTimeout`] --
    /// the caller must surface that as its own error, not log and proceed
    /// (AGENTS.md rule 3).
    pub async fn begin_delete(&self, agent_id: &AgentId) -> Result<(), Error> {
        self.backoff.begin_delete(agent_id);
        if self
            .backoff
            .wait_for_drain(agent_id, DELETE_DRAIN_TIMEOUT)
            .await
        {
            Ok(())
        } else {
            self.backoff.end_delete(agent_id);
            Err(Error::DeleteDrainTimeout)
        }
    }

    /// Ends a deletion begun by [`Handle::begin_delete`]: clears the mark
    /// (never the entry itself, and never the in-flight write count -- see
    /// [`DeleteGate::end_delete`]) so an agent later created with the same
    /// id dispatches normally. Must be called exactly once after every
    /// `begin_delete` that returned `Ok`, regardless of whether the delete
    /// itself went on to succeed (the row may still exist, e.g. a
    /// `DeleteAgent` refused because a write that finished draining left it
    /// with a live session).
    pub fn end_delete(&self, agent_id: &AgentId) {
        self.backoff.end_delete(agent_id);
    }

    /// `true` (and records one write in flight) if `agent_id` is not
    /// currently being deleted; declines (`false`) if it is. Used by
    /// handlers that read-or-lazily-create or overwrite an existing
    /// agent's guidance files outside the dispatcher
    /// (`GetAgent`/`AgentStatus`, `UpdateAgentProfile`) so they
    /// participate in the same drain [`Handle::begin_delete`] waits on --
    /// the same mechanism [`dispatch_agent`] uses for spawn preparation,
    /// just reached from a request handler instead of the dispatcher.
    /// Call [`Handle::end_agent_write`] exactly once after, regardless of
    /// outcome.
    #[must_use]
    pub fn try_begin_agent_write(&self, agent_id: &AgentId) -> bool {
        self.backoff.try_begin_preparation(agent_id)
    }

    /// Ends a write begun by [`Handle::try_begin_agent_write`].
    pub fn end_agent_write(&self, agent_id: &AgentId) {
        self.backoff.end_preparation(agent_id);
    }

    /// Begins deletion of `project_id` (ARCHITECTURE.md invariant 9's
    /// project-scoped mechanism, PR #50 review finding 3): marks the
    /// project so [`Handle::try_begin_project_write`] declines a
    /// `CreateAgent` not yet past its own worktree/guidance-tree writes,
    /// then waits up to [`DELETE_DRAIN_TIMEOUT`] for a `CreateAgent`
    /// already in flight to finish. Same contract as
    /// [`Handle::begin_delete`], one level up: on timeout, clears its own
    /// mark and returns [`Error::DeleteDrainTimeout`].
    pub async fn begin_delete_project(&self, project_id: &ProjectId) -> Result<(), Error> {
        self.project_gate.begin_delete(project_id);
        if self
            .project_gate
            .wait_for_drain(project_id, DELETE_DRAIN_TIMEOUT)
            .await
        {
            Ok(())
        } else {
            self.project_gate.end_delete(project_id);
            Err(Error::DeleteDrainTimeout)
        }
    }

    /// Ends a deletion begun by [`Handle::begin_delete_project`]. Same
    /// contract as [`Handle::end_delete`], one level up.
    pub fn end_delete_project(&self, project_id: &ProjectId) {
        self.project_gate.end_delete(project_id);
    }

    /// `true` (and records one write in flight) if `project_id` is not
    /// currently being deleted; used by `CreateAgent` to gate provisioning
    /// a new agent's worktree and guidance tree under a project that might
    /// be mid-`DeleteProject` -- the one writer `DeleteProject`'s own
    /// per-agent marks can never cover, since the agent being created
    /// doesn't exist yet for it to have marked. Call
    /// [`Handle::end_project_write`] exactly once after, regardless of
    /// outcome.
    #[must_use]
    pub fn try_begin_project_write(&self, project_id: &ProjectId) -> bool {
        self.project_gate.try_begin_preparation(project_id)
    }

    /// Ends a write begun by [`Handle::try_begin_project_write`].
    pub fn end_project_write(&self, project_id: &ProjectId) {
        self.project_gate.end_preparation(project_id);
    }

    /// Clears `agent_id`'s spawn backoff and start-deadline streak (issue
    /// #24 finding 4): called by `ResumeAgent` (`local_api.rs`) right
    /// alongside `Store::resume_agent`, so an operator resuming an agent
    /// the dispatcher paused after [`MAX_CONSECUTIVE_START_DEADLINES`]
    /// gets a clean slate rather than being paused again on its very next
    /// deadline (which, without this, would still count toward the old
    /// streak).
    pub fn resume_backoff(&self, agent_id: &AgentId) {
        self.backoff.record_success(agent_id);
    }

    /// Stops the dispatcher. Live sessions are untouched: closing/crashing
    /// the daemon must not stop agents (`HANDOFF.md`).
    pub async fn shutdown(&self) -> Result<(), Error> {
        let _ = self.shutdown.send(true);
        Ok(())
    }
}

/// Starts the dispatcher: recovers durable sessions from a prior daemon
/// instance, then serves wake triggers and the safety tick until shutdown.
pub fn spawn(
    config: Config,
    state: DaemonState,
) -> Result<(Handle, JoinHandle<Result<(), Error>>), Error> {
    if config.max_active_runs == 0 {
        return Err(Error::InvalidConcurrency);
    }
    prepare_runtime_root(&config.runtime_root)?;
    let runtime = tokio::runtime::Handle::try_current().map_err(|_| Error::ManagerStopped)?;
    let config = Arc::new(config);
    let (wake_tx, wake_rx) = mpsc::channel(WAKE_CHANNEL_CAPACITY);
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    // Shared with `Handle` (see its `backoff` field's doc comment) so
    // `DeleteAgent`/`DeleteProject` can mark an agent as deleting and wait
    // for the dispatcher to drain any in-flight spawn preparation for it.
    let backoff = Arc::new(SpawnBackoff::new());
    let dispatcher_config = Arc::clone(&config);
    let dispatcher_state = state.clone();
    let dispatcher_wake_tx = wake_tx.clone();
    let dispatcher_backoff = Arc::clone(&backoff);
    let join = runtime.spawn(run_dispatcher(
        dispatcher_config,
        dispatcher_state,
        dispatcher_wake_tx,
        wake_rx,
        shutdown_rx,
        dispatcher_backoff,
    ));
    Ok((
        Handle {
            state,
            config,
            wake_tx,
            shutdown: shutdown_tx,
            backoff,
            // Not shared with the dispatcher task at all: `CreateAgent`'s
            // writes it guards run on request-handling tasks, never on
            // `run_dispatcher`'s (see `Handle::try_begin_project_write`).
            project_gate: Arc::new(DeleteGate::new()),
        },
        join,
    ))
}

/// Per-agent exponential backoff for session-spawn attempts (this track's
/// item 1). Without it, a persistently broken spawn path -- the concrete
/// repro that motivated it: `--runner` pointed at a missing
/// `factory-runner` -- retried on every dispatcher wake/tick, unboundedly
/// often. Doubles from [`SPAWN_BACKOFF_INITIAL`] up to [`SPAWN_BACKOFF_MAX`]
/// per agent on each consecutive failure; a successful spawn clears the
/// timer ([`SpawnBackoff::record_success`]). Never pruned, same reasoning
/// as `DaemonState::delivery_slots`.
///
/// Embeds, but is deliberately backed by a *separate* lock from, its own
/// [`DeleteGate<AgentId>`] (PR #50 review, findings 1 and 2): an earlier
/// version kept `deleting`/`preparing` in the same map and entry as this
/// backoff timer, and clearing the timer -- on a successful spawn
/// (`record_success`) or a timed-out delete's own cleanup (`end_delete`)
/// -- was one `state.remove` that erased the *other* concern's state too,
/// silently reopening the exact race this whole mechanism exists to
/// close. Splitting them so neither operation can touch the other's data
/// makes that class of bug structurally impossible rather than merely
/// fixed once.
/// Timing state is in-memory only: a daemon restart forgets
/// every agent's delay, `consecutive_failures`, and
/// `consecutive_start_deadlines` and starts them fresh at
/// [`SPAWN_BACKOFF_INITIAL`]/zero -- acceptable (a restart is already a
/// deliberate, infrequent, operator-visible event, not a hot path this
/// needs to survive) and simpler than persisting transient retry-pacing
/// state durably alongside the actual session/task ledger.
struct SpawnBackoff {
    timing: Mutex<HashMap<AgentId, BackoffTiming>>,
    /// The delete-gating half of this struct (ARCHITECTURE.md invariant
    /// 9): [`SpawnBackoff::try_begin_preparation`]/
    /// [`SpawnBackoff::end_preparation`]/[`SpawnBackoff::begin_delete`]/
    /// [`SpawnBackoff::end_delete`]/[`SpawnBackoff::wait_for_drain`] all
    /// delegate here.
    gate: DeleteGate<AgentId>,
}

struct BackoffTiming {
    next_attempt_at: Instant,
    delay: Duration,
    consecutive_failures: u32,
    /// How many of `consecutive_failures` in a row, most recently, were a
    /// [`SESSION_START_DEADLINE`] expiry specifically (issue #24 finding
    /// 4) rather than any other kind of spawn failure -- cleared only by
    /// [`SpawnBackoff::record_success`]'s whole-entry reset (the session
    /// actually reaching `idle`, or an operator resume,
    /// [`Handle::resume_backoff`]). An ordinary spawn failure
    /// ([`SpawnBackoff::record_failure`]) does *not* clear it: `bump`
    /// only touches `delay`/`consecutive_failures`/`next_attempt_at`, on
    /// purpose -- a spawn that fails outright (e.g. a broken runner path)
    /// says nothing about whether the *provider's own hook* is working,
    /// so it must not excuse a run of deadline expiries. This is "3
    /// deadline failures since the last success", not "3 in an unbroken
    /// row with literally nothing else in between".
    consecutive_start_deadlines: u32,
}

impl Default for BackoffTiming {
    fn default() -> Self {
        Self {
            next_attempt_at: Instant::now(),
            delay: Duration::ZERO,
            consecutive_failures: 0,
            consecutive_start_deadlines: 0,
        }
    }
}

const SPAWN_BACKOFF_INITIAL: Duration = Duration::from_secs(5);
const SPAWN_BACKOFF_MAX: Duration = Duration::from_secs(300);
/// Bounds how long [`Handle::begin_delete`]/[`Handle::begin_delete_project`]
/// wait for an in-flight write to finish before giving up. The window this
/// bounds is not just a handful of file writes: for the agent-scoped gate
/// it is the whole of [`spawn_session_for_agent`] -- several `with_store`
/// round-trips on the shared store mutex, `hooks::write_hook_token`, the
/// provider's `spawn_spec`, a `create_session` commit and event publish,
/// and `runner_process::spawn_runner` -- which PR #50's review measured as
/// reachable in a few seconds on a loaded machine, exactly the condition
/// #42 was filed under. So this is a real, sometimes-hit bound, not a
/// formality: a delete on a busy daemon can genuinely time out and must
/// retry.
const DELETE_DRAIN_TIMEOUT: Duration = Duration::from_secs(5);
/// Poll granularity for [`DeleteGate::wait_for_drain`]. Polling (rather
/// than a `Notify`) is the simplest correct option here: preparation
/// windows are short, deletes are rare, and this reuses the same
/// lock-and-check shape as the rest of [`DeleteGate`] instead of adding a
/// second synchronization primitive.
const DELETE_DRAIN_POLL: Duration = Duration::from_millis(50);

impl SpawnBackoff {
    fn new() -> Self {
        Self {
            timing: Mutex::new(HashMap::new()),
            gate: DeleteGate::new(),
        }
    }

    /// `true` if `agent_id` has never failed a spawn, or its last
    /// recorded failure's delay has fully elapsed.
    fn ready(&self, agent_id: &AgentId) -> bool {
        let timing = self
            .timing
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        timing
            .get(agent_id)
            .is_none_or(|entry| Instant::now() >= entry.next_attempt_at)
    }

    /// Doubles (capped) `entry`'s delay, bumps `consecutive_failures`, and
    /// returns `(new_delay, consecutive_failures)` -- the part
    /// [`SpawnBackoff::record_failure`] and
    /// [`SpawnBackoff::record_start_deadline_failure`] share.
    fn bump(entry: &mut BackoffTiming) -> (Duration, u32) {
        entry.delay = if entry.delay.is_zero() {
            SPAWN_BACKOFF_INITIAL
        } else {
            entry.delay.saturating_mul(2).min(SPAWN_BACKOFF_MAX)
        };
        entry.consecutive_failures += 1;
        entry.next_attempt_at = Instant::now() + entry.delay;
        (entry.delay, entry.consecutive_failures)
    }

    /// Doubles (capped) `agent_id`'s delay and returns `(new_delay,
    /// consecutive_failures)` so the caller can log both.
    fn record_failure(&self, agent_id: &AgentId) -> (Duration, u32) {
        let mut timing = self
            .timing
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let entry = timing.entry(agent_id.clone()).or_default();
        Self::bump(entry)
    }

    /// [`SpawnBackoff::record_failure`], but for a [`SESSION_START_DEADLINE`]
    /// expiry specifically (issue #24 finding 4): shares the exact same
    /// exponential delay/`consecutive_failures` curve as any other spawn
    /// failure (so this still escalates through the documented 5s ->
    /// 5 minute backoff instead of restarting at 5s every ~2 minutes), and
    /// additionally tracks the deadline-specific streak
    /// [`MAX_CONSECUTIVE_START_DEADLINES`] acts on. Returns `(new_delay,
    /// consecutive_failures, consecutive_start_deadlines)`.
    fn record_start_deadline_failure(&self, agent_id: &AgentId) -> (Duration, u32, u32) {
        let mut timing = self
            .timing
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let entry = timing.entry(agent_id.clone()).or_default();
        let (delay, consecutive_failures) = Self::bump(entry);
        entry.consecutive_start_deadlines += 1;
        (
            delay,
            consecutive_failures,
            entry.consecutive_start_deadlines,
        )
    }

    /// Clears `agent_id`'s backoff bookkeeping entirely: delay,
    /// `consecutive_failures`, and `consecutive_start_deadlines` all reset
    /// to a clean slate. Two callers, both a deliberate "start over": a
    /// spawn actually reaching `idle` (`dispatch_agent`'s `Idle` arm --
    /// issue #24 finding 4's redefinition of "success", since a spawn
    /// merely returning `Ok` never gave a hookless provider's repeated
    /// deadline failures anywhere to escalate to), and an operator
    /// explicitly resuming a paused agent (`Handle::resume_backoff`) --
    /// the resume *is* the retry decision. Only touches timing, never the
    /// separate deletion gate introduced by #50.
    fn record_success(&self, agent_id: &AgentId) {
        let mut timing = self
            .timing
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        timing.remove(agent_id);
    }

    fn try_begin_preparation(&self, agent_id: &AgentId) -> bool {
        self.gate.try_begin_preparation(agent_id)
    }

    fn end_preparation(&self, agent_id: &AgentId) {
        self.gate.end_preparation(agent_id);
    }

    /// Marks `agent_id` as being deleted (ARCHITECTURE.md invariant 9).
    /// Deliberately does not touch the backoff timer (nit 8, PR #50
    /// review): `deleting` alone is what `try_begin_preparation` checks,
    /// so resetting timing here changes nothing about admission and used
    /// to just cost a persistently-broken agent its accumulated backoff on
    /// every refused delete attempt (`AgentHasChildren`,
    /// `AgentHasLiveSession`, ...).
    fn begin_delete(&self, agent_id: &AgentId) {
        self.gate.begin_delete(agent_id);
    }

    fn end_delete(&self, agent_id: &AgentId) {
        self.gate.end_delete(agent_id);
    }

    async fn wait_for_drain(&self, agent_id: &AgentId, budget: Duration) -> bool {
        self.gate.wait_for_drain(agent_id, budget).await
    }
}

/// Per-identity "no new state once deletion begins" gate (ARCHITECTURE.md
/// invariant 9): a `deleting` mark plus an in-flight `preparing` count.
/// Generic over the identity type so the exact same mechanism guards an
/// agent's guidance directory ([`SpawnBackoff`]'s `gate` field, keyed by
/// `AgentId`) and a project's, including agents an operator might still be
/// creating under it while the project itself is being deleted
/// (`Handle`'s `project_gate` field, keyed by `ProjectId`) -- see PR #50's
/// review, finding 3.
///
/// Never pruned (same reasoning as `DaemonState::delivery_slots`):
/// [`DeleteGate::end_delete`] clears only `deleting`, never removes the
/// entry and never touches `preparing`. Dropping the entry (or the
/// in-flight count with it) here is exactly the mistake `SpawnBackoff`
/// used to make -- see its doc comment.
struct DeleteGate<Id: Eq + std::hash::Hash + Clone> {
    state: Mutex<HashMap<Id, GateEntry>>,
}

#[derive(Default)]
struct GateEntry {
    deleting: bool,
    preparing: u32,
}

impl<Id: Eq + std::hash::Hash + Clone> DeleteGate<Id> {
    fn new() -> Self {
        Self {
            state: Mutex::new(HashMap::new()),
        }
    }

    /// Atomically checks that `id` is not currently being deleted and, if
    /// not, records one write in flight for it; declines (returns
    /// `false`) if a delete is in progress. Checked and incremented under
    /// the same lock as [`DeleteGate::begin_delete`] and
    /// [`DeleteGate::wait_for_drain`], so a fresh write and a delete
    /// beginning can never race past each other: whichever runs first
    /// under the lock determines the outcome.
    fn try_begin_preparation(&self, id: &Id) -> bool {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let entry = state.entry(id.clone()).or_default();
        if entry.deleting {
            return false;
        }
        entry.preparing += 1;
        true
    }

    /// Ends one write recorded by [`DeleteGate::try_begin_preparation`],
    /// whether it succeeded or failed -- lets a concurrent
    /// [`DeleteGate::wait_for_drain`] proceed once every write for `id`
    /// has ended.
    fn end_preparation(&self, id: &Id) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(entry) = state.get_mut(id) {
            entry.preparing = entry.preparing.saturating_sub(1);
        }
    }

    /// Marks `id` as being deleted: from this call on,
    /// `try_begin_preparation` declines new writes for it. Idempotent.
    fn begin_delete(&self, id: &Id) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.entry(id.clone()).or_default().deleting = true;
    }

    /// Clears only the deleting mark begun by `begin_delete` -- never
    /// removes the entry and never touches `preparing`. `preparing` may
    /// genuinely still be nonzero here: a drain timeout, or a write
    /// admitted the instant before `begin_delete` set the mark and not
    /// yet finished. Dropping it was exactly what let a retried delete
    /// report "drained" while a write was still running (PR #50 review,
    /// finding 2). Idempotent.
    fn end_delete(&self, id: &Id) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(entry) = state.get_mut(id) {
            entry.deleting = false;
        }
    }

    /// Polls (bounded by `budget`) until `id` has no write in flight;
    /// returns `false` on timeout. See [`DELETE_DRAIN_POLL`] for why
    /// polling rather than a `Notify`.
    async fn wait_for_drain(&self, id: &Id, budget: Duration) -> bool {
        let deadline = Instant::now() + budget;
        loop {
            let drained = {
                let state = self
                    .state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                state.get(id).is_none_or(|entry| entry.preparing == 0)
            };
            if drained {
                return true;
            }
            let now = Instant::now();
            if now >= deadline {
                return false;
            }
            sleep_until((now + DELETE_DRAIN_POLL).min(deadline)).await;
        }
    }
}

async fn run_dispatcher(
    config: Arc<Config>,
    state: DaemonState,
    wake_tx: mpsc::Sender<WakeAgent>,
    mut wake_rx: mpsc::Receiver<WakeAgent>,
    mut shutdown_rx: watch::Receiver<bool>,
    backoff: Arc<SpawnBackoff>,
) -> Result<(), Error> {
    recover_sessions(&state, &wake_tx, &shutdown_rx).await;

    let mut tick = tokio::time::interval(TICK_INTERVAL);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            changed = shutdown_rx.changed() => {
                if changed.is_err() || *shutdown_rx.borrow() {
                    return Ok(());
                }
            }
            woken = wake_rx.recv() => {
                let Some(WakeAgent { project_id, agent_id }) = woken else {
                    return Ok(());
                };
                if let Err(error) =
                    dispatch_agent(&config, &state, &wake_tx, &backoff, &project_id, &agent_id).await
                {
                    tracing::warn!(%error, %project_id, %agent_id, "dispatch failed");
                }
            }
            _ = tick.tick() => {
                if let Err(error) = reconcile_all(&config, &state, &wake_tx, &backoff).await {
                    tracing::warn!(%error, "reconcile tick failed");
                }
            }
        }
    }
}

/// The store's generic state-page cap (`store::MAX_STATE_PAGE`, not
/// exported) is smaller than the wire's advertised `MAX_*_PAGE_ITEMS`; a
/// full-table reconciler pages through it rather than assuming everything
/// fits in one call (a `LocalRequest::List*` caller gets the same
/// treatment via `local_api::session_page_limit`'s comment on the same
/// mismatch).
const RECONCILE_PAGE: usize = 100;

/// Re-scans every project's every agent for pending work, so a dropped or
/// never-sent wake trigger is never fatal -- only ever delayed up to
/// [`TICK_INTERVAL`].
async fn reconcile_all(
    config: &Config,
    state: &DaemonState,
    wake_tx: &mpsc::Sender<WakeAgent>,
    backoff: &SpawnBackoff,
) -> Result<(), Error> {
    let mut after_project = None;
    loop {
        let lookup_after_project = after_project.clone();
        let mut projects = state
            .with_store(move |store| {
                store.list_projects(lookup_after_project.as_ref(), RECONCILE_PAGE + 1)
            })
            .await?;
        let next_after_project = (projects.len() > RECONCILE_PAGE)
            .then(|| projects.swap_remove(RECONCILE_PAGE))
            .map(|project| project.id);
        for project in projects {
            let project_id = project.id.clone();
            let mut after_agent = None;
            loop {
                let lookup_after_agent = after_agent.clone();
                let mut agents = state
                    .with_store({
                        let project_id = project_id.clone();
                        move |store| {
                            store.list_agents(
                                &project_id,
                                lookup_after_agent.as_ref(),
                                RECONCILE_PAGE + 1,
                            )
                        }
                    })
                    .await?;
                let next_after_agent = (agents.len() > RECONCILE_PAGE)
                    .then(|| agents.swap_remove(RECONCILE_PAGE))
                    .map(|agent| agent.id);
                for agent in agents {
                    if let Err(error) =
                        dispatch_agent(config, state, wake_tx, backoff, &project_id, &agent.id)
                            .await
                    {
                        tracing::warn!(%error, %project_id, agent_id = %agent.id, "reconcile dispatch failed");
                    }
                }
                match next_after_agent {
                    Some(cursor) => after_agent = Some(cursor),
                    None => break,
                }
            }
        }
        match next_after_project {
            Some(cursor) => after_project = Some(cursor),
            None => break,
        }
    }
    Ok(())
}

async fn dispatch_agent(
    config: &Config,
    state: &DaemonState,
    wake_tx: &mpsc::Sender<WakeAgent>,
    backoff: &SpawnBackoff,
    project_id: &ProjectId,
    agent_id: &AgentId,
) -> Result<(), Error> {
    let hold_project_id = project_id.clone();
    let hold_agent_id = agent_id.clone();
    let held = match state
        .with_store(move |store| store.agent_is_held(&hold_project_id, &hold_agent_id))
        .await
    {
        Ok(held) => held,
        Err(DaemonStateError::Store(StoreError::AgentNotFound)) => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    if held {
        return Ok(());
    }

    let live_project_id = project_id.clone();
    let live_agent_id = agent_id.clone();
    let live = state
        .with_store(move |store| store.live_session_for_agent(&live_project_id, &live_agent_id))
        .await?;

    match live {
        None => {
            if has_pending_work(state, project_id, agent_id).await? {
                // Backoff (this track's item 1): a persistently broken
                // spawn path must not busy-retry on every wake/tick --
                // `backoff.ready` silently declines attempts still inside
                // their delay window (no log spam beyond the one already
                // emitted when the failure that started the delay was
                // recorded).
                if backoff.ready(agent_id)
                    && !at_concurrency_limit(config, state, project_id, agent_id).await?
                {
                    // Deletion (this task's mechanism, ARCHITECTURE.md):
                    // `try_begin_preparation` declines under the same lock
                    // `Handle::begin_delete` uses, so a `DeleteAgent`/
                    // `DeleteProject` already marking this agent and a
                    // fresh preparation can never both proceed -- whichever
                    // wins the lock first decides. A decline here is
                    // silent, matching `backoff.ready`'s: the delete
                    // request draining this agent is what actually reports
                    // the outcome to its caller.
                    if backoff.try_begin_preparation(agent_id) {
                        let spawn_result =
                            spawn_session_for_agent(config, state, wake_tx, project_id, agent_id)
                                .await;
                        backoff.end_preparation(agent_id);
                        // A successful process spawn is not yet a usable
                        // session; retain timing until SessionStart moves
                        // it past `starting`.
                        match spawn_result {
                            Ok(_) => {}
                            Err(error) => {
                                let (retry_in, attempt) = backoff.record_failure(agent_id);
                                tracing::warn!(
                                    %error,
                                    %project_id,
                                    %agent_id,
                                    attempt,
                                    retry_in_secs = retry_in.as_secs(),
                                    "session spawn failed; backing off"
                                );
                            }
                        }
                    }
                }
            }
            Ok(())
        }
        Some(session) if session.state == SessionState::Idle => {
            backoff.record_success(agent_id);
            deliver_pending(
                config,
                state,
                backoff,
                project_id,
                agent_id,
                &session.snapshot(),
            )
            .await
        }
        Some(session) if session.state == SessionState::Starting => {
            enforce_start_deadline(
                config, state, wake_tx, backoff, project_id, agent_id, &session,
            )
            .await
        }
        Some(_) => {
            // `working`/`waiting_for_input` (the only states left --
            // `live_session_for_agent` only ever returns a still-live row,
            // so `stopped`/`failed` can't reach here): reachable only via
            // hook transitions that themselves require `SessionStart` to
            // have already fired (`record_hook_event`'s state machine), so
            // this is exactly as much proof of success as the `Idle` arm
            // above -- a session the dispatcher's own polling never
            // happened to catch sitting `idle` (busy again by the very
            // next observation) must not leave a stale streak behind.
            backoff.record_success(agent_id);
            Ok(())
        }
    }
}

/// Issue #24: a session that has been `starting` for longer than
/// [`Config::session_start_deadline`] is treated exactly like a failed
/// spawn attempt, so "starting forever" is never a reachable steady state
/// even though the missing-hook root cause itself is not established. The
/// deadline is measured from the session's durable `started_at_ms`
/// (`dispatch_agent`'s ordinary per-agent pass -- wake-triggered or the 5
/// second safety tick -- calls this for every `starting` session, so this
/// also catches a session recovered `starting` after a daemon restart, not
/// just a freshly spawned one).
async fn enforce_start_deadline(
    config: &Config,
    state: &DaemonState,
    wake_tx: &mpsc::Sender<WakeAgent>,
    backoff: &SpawnBackoff,
    project_id: &ProjectId,
    agent_id: &AgentId,
    session: &SessionRow,
) -> Result<(), Error> {
    let now = now_ms()?;
    // A negative gap (the clock moved backward since the session's
    // `started_at_ms` was recorded) is treated as "not yet due" rather
    // than an immediate deadline hit -- clock skew must never be the
    // reason a freshly spawned session gets killed.
    let elapsed = u64::try_from(now.saturating_sub(session.started_at_ms)).unwrap_or(0);
    if Duration::from_millis(elapsed) < config.session_start_deadline {
        return Ok(());
    }

    // Adversarial review of #24, finding 6: a `StopSession` already in
    // flight against this still-`starting` session resolves through the
    // ordinary stop-completion path (`supervise_child` observing the
    // runner's real exit, `end_session`/`end_session_now`) with its own
    // real exit status and no reason text -- never through here, and
    // never with this deadline's reason attached to what the operator
    // asked to be a plain `stopped`, not a `failed`. Nothing to back off
    // or respawn either: nobody asked this agent to be respawned.
    if session.stop_requested_at_ms.is_some() {
        return Ok(());
    }

    let session_id = session.id.clone();
    // Adversarial review of #24, findings 1 and 2: commit the `failed`
    // transition *first*, guarded so it only ever applies while the
    // session is still exactly `starting` (`Store::fail_starting_session`,
    // `WHERE state = 'starting'` inside its own transaction -- not
    // `SessionState::is_live()`, which would also accept a session whose
    // own `SessionStart` hook won the race and already moved it to
    // `idle`/`working` in between this function's `await` points). Only a
    // successful, guarded commit goes on to best-effort stop the runner,
    // record the backoff failure, and wake the dispatcher; a lost guard
    // (the hook won) makes this entire call a no-op, and -- unlike the
    // previous stop-then-fail order -- a `supervise_child` racing the
    // runner's own exit can never beat this commit to `SessionNotLive`
    // and silently swallow the reason/backoff/wake the operator depends
    // on to see why the session failed and that a retry is coming.
    let reason = format!(
        "SessionStart hook not received within {}s (the provider started but its hooks did not \
         reach factoryd)",
        config.session_start_deadline.as_secs()
    );
    let fail_session_id = session_id.clone();
    let outcome = state
        .commit_and_publish(move |store| {
            match store.fail_starting_session(&fail_session_id, reason, now)? {
                Some((_snapshot, events)) => Ok((true, events)),
                None => Ok((false, Vec::new())),
            }
        })
        .await?;
    if !outcome {
        // Lost the guard: the session's own `SessionStart` hook (or some
        // other transition) already moved it out of `starting` before
        // this committed. It is exactly as healthy as its own state now
        // says -- nothing here to stop, back off, or wake for.
        return Ok(());
    }

    // Stop the runner (best-effort, reusing the same control path
    // `local_api.rs`'s `StopSession` handler uses): if the control socket
    // is already unreachable there is nothing left to stop -- the session
    // is already durably recorded `failed` above regardless, so this can
    // never be the reason a stuck session stays visible as `starting`.
    // Adversarial review of #24, finding 7: a failed stop here can leave
    // the old provider process alive, holding the worktree, while the
    // backoff retry below launches a new runner into the same worktree --
    // the same orphan class issue #26 already covers, just reachable as a
    // steady-state path now instead of only across a daemon restart; no
    // reaper for it here, `tracing::error!` (not `warn!`) so it is not
    // mistaken for the routine, expected case.
    let target_project_id = project_id.clone();
    let target_session_id = session_id.clone();
    match state
        .with_store(move |store| {
            store.session_control_target(&target_project_id, &target_session_id)
        })
        .await
    {
        Ok(target) => {
            if let Ok(control_run_id) = session_run_id(&session_id) {
                if let Err(error) = RunnerClient::new(
                    target.runner_runtime,
                    control_run_id,
                    target.runner_instance_id,
                )
                .stop(0)
                .await
                {
                    tracing::error!(
                        %error,
                        %project_id,
                        %agent_id,
                        %session_id,
                        "could not stop a session past its start deadline; its provider process \
                         may still be running and holding the worktree"
                    );
                }
            }
        }
        Err(error) => {
            tracing::error!(
                %error,
                %project_id,
                %agent_id,
                %session_id,
                "could not resolve a session past its start deadline's control target; its \
                 provider process may still be running and holding the worktree"
            );
        }
    }

    // Adversarial review of #24, finding 4: this still drives the exact
    // same exponential delay/`consecutive_failures` an ordinary spawn
    // failure does (5s doubling to a 5 minute cap, `SpawnBackoff::bump`),
    // but also tracks how many of this agent's consecutive failures were
    // a start-deadline expiry specifically, so a hookless provider -- one
    // whose spawn always succeeds, only its hook never arrives -- has
    // somewhere to escalate to instead of cycling forever at this
    // deadline's own ~2 minute cadence.
    let (retry_in, attempt, consecutive_start_deadlines) =
        backoff.record_start_deadline_failure(agent_id);

    if consecutive_start_deadlines >= MAX_CONSECUTIVE_START_DEADLINES {
        // Pause instead of respawning: the session just committed above
        // stays `failed` with its own reason, which is already enough for
        // `factory-core::attention::agent_attention` to report this agent
        // as observed `Failed` and for `factoryctl status`/`agent
        // status`/the TUI to route the operator to it -- no new state, no
        // new code path there. `factoryctl agent resume` is the
        // documented way back in and resets this streak
        // (`Handle::resume_backoff`). Deliberately no `send_wake`: a
        // paused agent's own `dispatch_agent` call returns immediately
        // (its very first check), so there is nothing for a wake to do
        // until the operator resumes it.
        let pause_project_id = project_id.clone();
        let pause_agent_id = agent_id.clone();
        match state
            .commit_and_publish(move |store| {
                let (agent, event) = store.pause_agent(&pause_project_id, &pause_agent_id, now)?;
                Ok((agent, vec![event]))
            })
            .await
        {
            Ok(_) => {
                tracing::warn!(
                    %project_id,
                    %agent_id,
                    %session_id,
                    consecutive_start_deadlines,
                    "session start deadline exceeded too many times in a row; pausing the agent \
                     instead of respawning (factoryctl agent resume to retry)"
                );
            }
            Err(error) => {
                tracing::warn!(
                    %error,
                    %project_id,
                    %agent_id,
                    "could not pause an agent after repeated session start deadlines"
                );
            }
        }
        return Ok(());
    }

    // Same shape of event/log line an ordinary spawn failure already
    // emits (`dispatch_agent`'s own `None` branch, above): the next
    // attempt is backed off, and the dispatcher is woken so it respawns
    // once that backoff elapses rather than waiting a full extra safety
    // tick.
    tracing::warn!(
        %project_id,
        %agent_id,
        %session_id,
        attempt,
        consecutive_start_deadlines,
        retry_in_secs = retry_in.as_secs(),
        "session start deadline exceeded; recorded failed and backing off"
    );
    send_wake(wake_tx, project_id.clone(), agent_id.clone());
    Ok(())
}

async fn has_pending_work(
    state: &DaemonState,
    project_id: &ProjectId,
    agent_id: &AgentId,
) -> Result<bool, Error> {
    let hold_project_id = project_id.clone();
    let hold_agent_id = agent_id.clone();
    if state
        .with_store(move |store| store.agent_is_held(&hold_project_id, &hold_agent_id))
        .await?
    {
        return Ok(false);
    }
    let task_project_id = project_id.clone();
    let task_agent_id = agent_id.clone();
    let task = state
        .with_store(move |store| store.next_deliverable(&task_project_id, &task_agent_id))
        .await?;
    if task.is_some() {
        return Ok(true);
    }
    let message_project_id = project_id.clone();
    let message_agent_id = agent_id.clone();
    let messages = state
        .with_store(move |store| {
            store.undelivered_messages_for_agent(&message_project_id, &message_agent_id)
        })
        .await?;
    Ok(!messages.is_empty())
}

/// `true` if the daemon is already at `Config::max_active_runs` live
/// sessions (this track's item 2): the caller must leave `agent_id`
/// unspawned this pass rather than attempt it. Deliberately does not touch
/// [`SpawnBackoff`] -- this is not a broken spawn path, just a full resource
/// pool, and the 5 second safety tick (`reconcile_all`) re-checks every
/// agent with pending work on its own, so the next session to end frees a
/// slot within one tick without needing a per-agent wake. Logged at `info`
/// (not `warn`, matching `dispatch_agent`'s own failure log next to it) so
/// an operator watching `factoryd`'s log can see *why* an agent with
/// pending work is not starting, since -- unlike an actual spawn failure --
/// there is no `starting`/`failed` session row to carry a `wait_reason`
/// (nothing was ever created).
async fn at_concurrency_limit(
    config: &Config,
    state: &DaemonState,
    project_id: &ProjectId,
    agent_id: &AgentId,
) -> Result<bool, Error> {
    let live_count = state.with_store(|store| store.live_session_count()).await?;
    if live_count < config.max_active_runs {
        return Ok(false);
    }
    tracing::info!(
        %project_id,
        %agent_id,
        live_count,
        max_active_runs = config.max_active_runs,
        "session spawn deferred: max-active-runs reached"
    );
    Ok(true)
}

// --- Session spawn ---------------------------------------------------

fn select_provider(kind: Provider) -> Box<dyn providers::Provider + Send> {
    match kind {
        Provider::ClaudeCode => Box::new(providers::claude::ClaudeProvider::new()),
        Provider::Codex => Box::new(providers::codex::CodexProvider::new()),
        Provider::Shell => Box::new(providers::shell::ShellProvider),
    }
}

async fn spawn_session_for_agent(
    config: &Config,
    state: &DaemonState,
    wake_tx: &mpsc::Sender<WakeAgent>,
    project_id: &ProjectId,
    agent_id: &AgentId,
) -> Result<SessionSnapshot, Error> {
    let detail_project_id = project_id.clone();
    let detail_agent_id = agent_id.clone();
    let agent = state
        .with_store(move |store| store.get_agent_detail(&detail_project_id, &detail_agent_id))
        .await?;
    let auto_mode = state.with_store(|store| store.auto_mode()).await?;
    let worktree = agent.snapshot.worktree.clone().ok_or(Error::NoWorktree)?;
    let worktree_path = PathBuf::from(&worktree);

    let provider_impl = select_provider(agent.snapshot.provider);
    let capabilities = provider_impl.capabilities();

    // The most recent provider-session identity this agent's sessions ever
    // confirmed, live or historical -- generic across providers by design
    // (TRACK5-DESIGN.md §1): Claude's is assigned by the daemon itself at
    // creation time (`Store::create_session`, below), so it is always
    // already set; Codex's is learned back from its own first
    // `SessionStart` hook payload and persisted by
    // `Store::set_provider_session_id` (`local_api.rs`'s `ProviderHook`
    // handler, TRACK5D item 5). Either way, a fresh spawn just asks "does
    // this agent have a prior session with one" and resumes it when
    // `capabilities.resume` allows.
    let resume_project_id = project_id.clone();
    let resume_agent_id = agent_id.clone();
    let resume = if capabilities.resume {
        state
            .with_store(move |store| {
                store.last_provider_session_id(&resume_project_id, &resume_agent_id)
            })
            .await?
    } else {
        None
    };

    let session_id = new_session_id()?;
    let runtime_dir = config.runtime_root.join(session_id.as_str());
    let hook_token_path = runtime_dir.join("hook.token");
    let hook_token = hooks::write_hook_token(&hook_token_path)?;

    let agent_dir = factory_core::paths::agent_dir(&config.guidance_root, project_id, agent_id);

    let ctx = SpawnContext {
        agent_id: agent_id.clone(),
        project_id: project_id.clone(),
        session_id: session_id.clone(),
        worktree: worktree_path.clone(),
        model: agent.profile.model.clone(),
        permission_mode: agent.profile.permission_mode.clone(),
        auto_mode,
        resume: resume.clone(),
        hook_token_path: hook_token_path.clone(),
        factoryctl_path: config.factoryctl_path.clone(),
        agent_dir,
        socket_path: config.socket_path.clone(),
    };
    let launch = provider_impl.spawn_spec(&ctx)?;

    let runner_instance_id = new_runner_instance_id()?;
    let (codex_home, extra_env) = split_provider_environment(launch.env);
    let mut session_environment = vec![
        (
            "DARK_FACTORY_AGENT".to_owned(),
            agent_id.as_str().to_owned(),
        ),
        (
            "DARK_FACTORY_PROJECT".to_owned(),
            project_id.as_str().to_owned(),
        ),
        (
            "DARK_FACTORY_SOCKET".to_owned(),
            config.socket_path.to_string_lossy().into_owned(),
        ),
        (
            "DARK_FACTORY_SESSION_TOKEN_FILE".to_owned(),
            hook_token_path.to_string_lossy().into_owned(),
        ),
        (
            "DARK_FACTORY_AGENT_DIR".to_owned(),
            ctx.agent_dir.to_string_lossy().into_owned(),
        ),
    ];
    session_environment.extend(extra_env);
    let provider_environment = codex_home
        .clone()
        .map_or(ProviderEnvironment::Inherited, |home| {
            ProviderEnvironment::CodexHome(PathBuf::from(home))
        });

    let provider_session_id = match agent.snapshot.provider {
        Provider::ClaudeCode => Some(
            resume
                .clone()
                .unwrap_or_else(|| session_id.as_str().to_owned()),
        ),
        Provider::Codex => resume.clone(),
        Provider::Shell => None,
    };

    let launch_spec = LaunchSpec {
        runner_program: config.runner_program.clone(),
        factoryctl_path: config.factoryctl_path.clone(),
        provider_program: launch.program,
        provider_arguments: launch
            .args
            .into_iter()
            .map(std::ffi::OsString::from)
            .collect(),
        provider_environment,
        session_environment,
        run_id: session_run_id(&session_id)?,
        runner_instance_id: runner_instance_id.clone(),
        runtime_dir: runtime_dir.clone(),
        cwd: worktree_path,
        startup_input: Vec::new(),
        terminal: Some(TerminalSize {
            cols: 200,
            rows: 50,
        }),
    };
    // Created *before* the process spawn attempt (this track's item 1): a
    // `starting` session row makes a spawn failure durably visible
    // (`session list`/the TUI, an announcement + a red X) instead of
    // silent -- previously the daemon's own log was the only place a
    // persistently broken spawn path (concretely: `--runner` pointed at a
    // missing `factory-runner`) ever showed up, and it retried forever
    // with no visible trace and no runtime-directory cleanup (18 leaked
    // `runs/<uuid>/` directories in the repro that motivated this).
    let created_at_ms = now_ms()?;
    let new_session = crate::store::NewSession {
        id: session_id.clone(),
        project_id: project_id.clone(),
        agent_id: agent_id.clone(),
        provider: agent.snapshot.provider,
        runtime_model: launch.runtime.model,
        runtime_reasoning_effort: launch.runtime.reasoning_effort,
        runtime_permission_mode: launch.runtime.permission_mode,
        runtime_control_mode: launch.runtime.control_mode,
        provider_session_id,
        worktree,
        codex_home,
        hook_token,
        runner_instance_id: runner_instance_id.clone(),
        runner_runtime: runtime_dir.to_string_lossy().into_owned(),
        runner_protocol_version: 1,
    };
    let snapshot = state
        .commit_and_publish(move |store| {
            let (snapshot, event) = store.create_session(new_session, created_at_ms)?;
            Ok((snapshot, vec![event]))
        })
        .await?;

    let child = match runner_process::spawn_runner(launch_spec, STARTUP_GRACE).await {
        Ok(child) => child,
        Err(error) => {
            // Nothing ever ran in this attempt's runtime directory (the
            // hook token file, and any provider-seeded config written
            // into it by `spawn_spec` above) -- remove it rather than
            // leaving one leaked directory behind per failed attempt.
            let _ = fs::remove_dir_all(&runtime_dir);
            let reason = truncate_utf8(&error.to_string(), MAX_WAIT_REASON_BYTES);
            let fail_session_id = session_id.clone();
            let fail_at_ms = now_ms().unwrap_or(created_at_ms);
            let _ = state
                .commit_and_publish(move |store| {
                    let (snapshot, events) = store.end_session_with_reason(
                        &fail_session_id,
                        None,
                        None,
                        Some(reason),
                        fail_at_ms,
                    )?;
                    Ok((snapshot, events))
                })
                .await;
            return Err(Error::Spawn(error));
        }
    };

    tokio::spawn(supervise_child(
        state.clone(),
        wake_tx.clone(),
        session_id.clone(),
        agent.snapshot.provider == Provider::Codex,
        runtime_dir,
        session_run_id(&session_id)?,
        runner_instance_id,
        child,
    ));
    Ok(snapshot)
}

/// Codex 0.147 does not fire its own `SessionStart` hook at TUI/process
/// startup, even with `source: "startup"` and
/// `--dangerously-bypass-hook-trust` passed: confirmed live (2 of 2 real
/// dogfood sessions sat in `starting` indefinitely, unblocked only by an
/// operator manually posting the session's own `SessionStart` hook by
/// hand) and empirically (a throwaway `CODEX_HOME` with only a
/// `SessionStart` hook produced zero invocations while idling at Codex's
/// own ready-to-type prompt; the identical hook fired exactly once, tagged
/// `"source":"startup"`, only once a prompt was actually submitted -- see
/// `docs/providers.md`). Codex defers session/thread creation, and
/// therefore hook dispatch, to the first turn, not process launch.
///
/// That collides with this dispatcher's own invariant: nothing is ever
/// PTY-typed into a session that is not already `Idle`
/// (`Handle::start_task`, `dispatch_agent`), and a session only reaches
/// `Idle` via a `SessionStart` hook (`Store::record_hook_event`). Left
/// alone, a fresh Codex session deadlocks forever -- Codex waits for a
/// typed prompt to begin the turn that would fire `SessionStart`; the
/// daemon waits for `SessionStart` before typing anything.
///
/// This synthesizes that exact transition (`Store::synthesize_session_start`,
/// which -- unlike a real hook -- leaves `last_hook_event` alone, so the
/// synthesis stays durably distinguishable from a genuine hook POST) once
/// [`RunnerEvent::TerminalRaw`] reports the provider's own tty left
/// canonical mode (`wait_for_runner_exit`/`consume_until_exit`, below,
/// which call this): a kernel-level fact about the child's own terminal
/// setup, not terminal-output inference (`ARCHITECTURE.md` invariant 5's
/// Codex carve-out), and specifically *not* "the process merely spawned" --
/// an earlier version of this fix synthesized immediately on spawn
/// confirmation, before the pty leaves canonical mode with echo on, which a
/// real repro showed silently truncates (and can duplicate) the very first
/// delivery typed into it (`MAX_CANON`, `docs/providers.md`). If Codex's
/// own real (once-delayed) `SessionStart(source=startup)` arrives later
/// anyway -- e.g. after the first turn it caused -- it is a harmless no-op:
/// `record_hook_event`'s `SessionStart` arm only transitions a session that
/// is still `Starting`, and `local_api.rs`'s `set_provider_session_id` call
/// is already idempotent once a provider session id is set.
///
/// Best-effort like [`Handle::wake`]: a store error here leaves the session
/// `starting` (recoverable the moment a real hook -- or another wake --
/// eventually lands) rather than unwinding an already-running provider
/// process that cannot be un-spawned.
async fn synthesize_codex_session_start(
    state: &DaemonState,
    wake_tx: &mpsc::Sender<WakeAgent>,
    session_id: &SessionId,
) {
    let Ok(hook_at_ms) = now_ms() else {
        return;
    };
    let hook_session_id = session_id.clone();
    let result = state
        .commit_and_publish(move |store| {
            match store.synthesize_session_start(&hook_session_id, hook_at_ms)? {
                Some((snapshot, event)) => Ok((Some(snapshot), vec![event])),
                None => Ok((None, Vec::new())),
            }
        })
        .await;
    if let Ok(Some(snapshot)) = result {
        send_wake(wake_tx, snapshot.project_id, snapshot.agent_id);
    }
}

fn split_provider_environment(
    launch_env: Vec<(String, String)>,
) -> (Option<String>, Vec<(String, String)>) {
    let mut codex_home = None;
    let mut rest = Vec::new();
    for (name, value) in launch_env {
        if name == "CODEX_HOME" {
            codex_home = Some(value);
        } else {
            rest.push((name, value));
        }
    }
    (codex_home, rest)
}

/// A freshly spawned session's *state* liveness is driven by hooks, not a
/// decoded stream (unlike the pre-5A model) -- but the underlying
/// `factory-runner` process's own liveness is not simply "until its `Child`
/// handle resolves": `factory_runner::run` deliberately does not exit after
/// an ordinary (non-signalled) termination of the program it supervises
/// until a client sends it `AcknowledgeExit` for the exact terminal
/// sequence it durably logged (this is what lets a recovered daemon replay
/// a session's tail after a crash -- the runner holds the retained spool
/// open until someone confirms they saw it). A bare `child.wait()` here
/// would therefore hang forever after every ordinary session end (a
/// `StopSession`, or the provider process just exiting on its own) unless
/// the whole daemon itself is shutting down (which signals the runner
/// directly, taking the `RunnerSignalled` bypass in `factory_runner::run`).
/// So: subscribe like a recovered session does, wait for the runner's own
/// `RunnerEvent::Exited`, acknowledge it (which is what actually lets the
/// runner's process finish), and only then reap the `Child` handle. The
/// exit code/signal in that event are the underlying PTY child's -- the
/// wrapper `factory-runner` process's own `Child::wait()` status is not
/// useful for that (its own exit code is 0/1 for its own success/failure,
/// unrelated to the program it supervised), so it is only a fallback if
/// the control socket could not be reached at all.
#[allow(clippy::too_many_arguments)]
async fn supervise_child(
    state: DaemonState,
    wake_tx: mpsc::Sender<WakeAgent>,
    session_id: SessionId,
    synthesize_on_raw_mode: bool,
    runtime_dir: PathBuf,
    run_id: RunId,
    runner_instance_id: RunnerInstanceId,
    mut child: tokio::process::Child,
) {
    let event_exit = wait_for_runner_exit(
        &state,
        &wake_tx,
        &session_id,
        synthesize_on_raw_mode,
        &runtime_dir,
        run_id,
        runner_instance_id,
    )
    .await;
    let wait_status = child.wait().await;
    let (exit_code, exit_signal) = match event_exit {
        Some(status) => status,
        None => match wait_status {
            Ok(status) => (status.code(), status.signal()),
            Err(_) => (None, None),
        },
    };
    end_session_now(&state, &wake_tx, &session_id, exit_code, exit_signal).await;
}

/// Whether `event` is the trigger [`synthesize_codex_session_start`] exists
/// for -- `synthesize_on_raw_mode` is `true` only for a Codex session still
/// waiting on it (`supervise_child`'s and `supervise_recovered`'s own
/// callers gate this on `Provider::Codex`, once, before either loop below
/// ever starts -- this function does not re-derive it from the event
/// itself, so it is exactly as testable in isolation as "does synthesis
/// happen for Codex and never for Claude/shell" asks for).
fn should_synthesize_session_start(synthesize_on_raw_mode: bool, event: &RunnerEvent) -> bool {
    synthesize_on_raw_mode && matches!(event, RunnerEvent::TerminalRaw)
}

/// Logs (at `debug` -- expected, not a fault) any runner event this daemon
/// build does not recognize (`RunnerEvent::Unknown`'s own doc comment has
/// the compatibility story: a future variant a newer runner sent that this
/// build has no name, or no matching shape, for). A no-op for every known
/// event. Called from both `wait_for_runner_exit` and `consume_until_exit`
/// right where they already treat "not `Exited`" as ordinary -- observability
/// only, per adversarial review round 2 finding A: an unrecognized event
/// must never be silent, even though it is always harmless.
fn log_unrecognized_runner_event(session_id: &SessionId, envelope: &RunnerEventEnvelope) {
    if matches!(envelope.event, RunnerEvent::Unknown) {
        tracing::debug!(
            session_id = %session_id,
            sequence = envelope.sequence,
            "ignoring a runner event this daemon build does not recognize"
        );
    }
}

/// Adversarial review round 2, finding B: `RunnerEvent::TerminalRawTimedOut`
/// (its own doc comment has the full story) used to vanish in total
/// silence -- an operator watching a Codex session stuck `starting` had no
/// breadcrumb at all. Logs a `tracing::warn!`, deliberately the *only*
/// reaction: no session state or `wait_reason` change, which stays
/// reserved for `#52`'s own deadline (keyed off `state == starting`) --
/// changing state here would make this session invisible to that
/// deadline's own check, silently disabling the actual backstop for
/// exactly the session it exists to catch. Only meaningful for a Codex
/// session still waiting on synthesis (`synthesize_on_raw_mode`); a
/// no-op for Claude/`shell`, which never depend on this signal.
fn warn_on_raw_mode_timeout(
    session_id: &SessionId,
    synthesize_on_raw_mode: bool,
    envelope: &RunnerEventEnvelope,
) {
    if synthesize_on_raw_mode && matches!(envelope.event, RunnerEvent::TerminalRawTimedOut) {
        tracing::warn!(
            session_id = %session_id,
            "Codex session's pty never left canonical mode within the runner's raw-mode \
             poll window; SessionStart was never synthesized for it and it may still be \
             starting -- see docs/providers.md's Codex SessionStart section"
        );
    }
}

/// Subscribes to a freshly spawned session's own runner (retrying for up to
/// [`CONNECT_GRACE`] -- `runner_process::spawn_runner` does not itself wait
/// for the control socket to exist in terminal mode, so this can genuinely
/// race the runner's own startup), then watches its event stream until
/// `RunnerEvent::Exited`, synthesizing Codex's `SessionStart` along the way
/// the moment `RunnerEvent::TerminalRaw` arrives (`synthesize_on_raw_mode`;
/// see `synthesize_codex_session_start`'s own doc comment), and returns the
/// exit's `(exit_code, exit_signal)`. Returns `None` if the control socket
/// was never reachable or the connection was lost before an exit event
/// arrived -- best-effort, since the caller still has its own
/// `Child::wait()` to fall back on rather than hang the dispatcher on one
/// wedged session forever.
async fn wait_for_runner_exit(
    state: &DaemonState,
    wake_tx: &mpsc::Sender<WakeAgent>,
    session_id: &SessionId,
    synthesize_on_raw_mode: bool,
    runtime_dir: &Path,
    run_id: RunId,
    runner_instance_id: RunnerInstanceId,
) -> Option<(Option<i32>, Option<i32>)> {
    let client = RunnerClient::new(runtime_dir, run_id, runner_instance_id);
    let deadline = Instant::now() + CONNECT_GRACE;
    let mut subscription = loop {
        match client.subscribe().await {
            Ok(subscription) => break subscription,
            Err(error) if unavailable(&error) && Instant::now() < deadline => {
                sleep_until((Instant::now() + CONNECT_RETRY_DELAY).min(deadline)).await;
            }
            Err(error) => {
                tracing::warn!(
                    %error,
                    "could not subscribe to a freshly spawned runner's control socket"
                );
                return None;
            }
        }
    };
    loop {
        match subscription.next_item().await {
            Ok(RunnerStreamItem::Event(envelope)) => {
                if let RunnerEvent::Exited { exit_code, signal } = envelope.event {
                    if let Err(error) = client.acknowledge_exit(envelope.sequence).await {
                        tracing::warn!(%error, "failed to acknowledge a runner's terminal event");
                    }
                    return Some((exit_code, signal));
                }
                log_unrecognized_runner_event(session_id, &envelope);
                warn_on_raw_mode_timeout(session_id, synthesize_on_raw_mode, &envelope);
                if should_synthesize_session_start(synthesize_on_raw_mode, &envelope.event) {
                    synthesize_codex_session_start(state, wake_tx, session_id).await;
                }
            }
            Ok(RunnerStreamItem::CaughtUp { .. }) => {}
            Err(error) => {
                tracing::warn!(%error, "runner control connection failed before an exit event arrived");
                return None;
            }
        }
    }
}

// --- Delivery ----------------------------------------------------------

/// The text (and, when it opens a task-episode, which task) the dispatcher
/// or a `Stop`/`SubagentStop` hook reply should deliver next for an agent.
struct Delivery {
    task_id: Option<factory_core::TaskId>,
    task_incarnation_id: Option<String>,
    prior_run_count: Option<usize>,
    text: String,
}

async fn compose_delivery(
    state: &DaemonState,
    guidance_root: &Path,
    project_id: &ProjectId,
    agent_id: &AgentId,
    session_id: &SessionId,
) -> Result<Option<Delivery>, DaemonStateError> {
    let guidance_root = guidance_root.to_path_buf();
    let project_id = project_id.clone();
    let agent_id = agent_id.clone();
    let session_id = session_id.clone();
    state
        .with_store(move |store| {
            if store.agent_is_held(&project_id, &agent_id)? {
                return Ok(None);
            }
            let task_id = store.next_deliverable(&project_id, &agent_id)?;
            let messages = store.undelivered_messages_for_agent(&project_id, &agent_id)?;
            if task_id.is_none() && messages.is_empty() {
                return Ok(None);
            }
            let task = task_id
                .as_ref()
                .map(|id| store.get_task(&project_id, id))
                .transpose()?;
            let task_marker = task_id
                .as_ref()
                .map(|id| store.task_delivery_marker(&session_id, id))
                .transpose()?;
            let (task_incarnation_id, prior_run_count) = task_marker
                .map(|(incarnation_id, count)| (Some(incarnation_id), Some(count)))
                .unwrap_or((None, None));
            let agent = store.get_agent_detail(&project_id, &agent_id)?;
            let text = compose_text(
                &guidance_root,
                &project_id,
                &agent_id,
                task.as_ref(),
                &messages,
                agent.snapshot.role,
            );
            Ok(Some(Delivery {
                task_id,
                task_incarnation_id,
                prior_run_count,
                text,
            }))
        })
        .await
}

/// Commits an already-typed-and-acknowledged [`Delivery`]: opens the task
/// episode (which also delivers any pending messages alongside it,
/// `Store::open_run_episode`) or, for a message-only delivery, marks the
/// messages delivered.
async fn commit_delivery(
    state: &DaemonState,
    project_id: &ProjectId,
    agent_id: &AgentId,
    session_id: &SessionId,
    delivery: Delivery,
    now_ms: i64,
) -> Result<Option<RunId>, DaemonStateError> {
    let project_id = project_id.clone();
    let agent_id = agent_id.clone();
    let session_id = session_id.clone();
    state
        .commit_and_publish(move |store| {
            match (
                delivery.task_id,
                delivery.task_incarnation_id,
                delivery.prior_run_count,
            ) {
                (Some(task_id), Some(task_incarnation_id), Some(prior_run_count)) => {
                    match store.open_run_episode(&session_id, &task_id, now_ms) {
                        Ok(opened) => {
                            let run_id = opened.run.id.clone();
                            Ok((Some(run_id), opened.events))
                        }
                        // The synchronous `UserPromptSubmit` hook commit can
                        // win this dispatcher's later ack-driven commit. A
                        // sufficiently fast client can also finish that run
                        // before this retry, yielding `TaskNotQueued` instead
                        // of `AgentUnavailable`. The immutable incarnation and
                        // run count captured while composing are the durable
                        // attempt identity: only a new run for this exact task
                        // row in this session proves the candidate committed.
                        // Historical retries and delete/recreate ABA do not.
                        Err(StoreError::AgentUnavailable | StoreError::TaskNotQueued)
                            if store.delivery_attempt_committed(
                                &session_id,
                                &task_id,
                                &task_incarnation_id,
                                prior_run_count,
                            )? =>
                        {
                            Ok((None, Vec::new()))
                        }
                        Err(error) => Err(error),
                    }
                }
                (None, None, None) => {
                    store.deliver_agent_messages(&project_id, &agent_id, &session_id, now_ms)?;
                    Ok((None, Vec::new()))
                }
                _ => Err(StoreError::InvalidExecutionMetadata),
            }
        })
        .await
}

/// Best-effort: if a pending task/message delivery exists for `session`'s
/// agent, commits it (opens the run episode / marks messages delivered)
/// right now, in-line. Called synchronously from `local_api.rs`'s
/// `ProviderHook` handler for `UserPromptSubmit`, *before* that hook
/// request's own reply reaches the client.
///
/// Why this needs to exist at all, found manually building this track's
/// own E2E test (TRACK5E-BRIEF.md item 1): `deliver_pending`'s
/// `type_and_await_ack` treats "a `UserPromptSubmit` hook for this
/// session, timestamped after the write" as proof a PTY-typed delivery
/// landed, then commits it (`commit_delivery`, above) -- but that ack
/// detection is a *separate* task (a broadcast-event subscriber) racing
/// the real client's own next steps, which the client takes as soon as
/// its *own* `factoryctl hook ... UserPromptSubmit` call returns, with no
/// reason to wait for the daemon's unrelated internal bookkeeping to
/// finish first. A client that reacts fast enough -- a zero-latency
/// deterministic test fixture reliably does under any real machine load;
/// a real Claude Code/Codex turn practically never does, but the daemon
/// must not depend on that -- can call `factoryctl task done` before
/// `commit_delivery` ran, which `Store::open_run_for_task` then rejects
/// (no run open yet) with nothing but a swallowed error on the client
/// side and a task stuck `running`-that-never-opened forever.
///
/// The fix is not "wait longer" (raising `ACK_TIMEOUT` does not help: the
/// commit that needs to win the race is instant, the problem is *when* it
/// runs relative to the client, not how long anything waits) -- it is to
/// make the commit happen as part of handling the exact hook request the
/// client is itself blocked on, so it is durable before that request's
/// response can reach the client at all. `deliver_pending`'s own later
/// `commit_delivery` call becomes a redundant, harmless retry of the same
/// idempotent commit once its own ack-wait notices (the
/// `StoreError::AgentUnavailable` tolerance above).
///
/// Gated on [`DaemonState::delivery_in_flight`]: *every* `UserPromptSubmit`
/// reaches this function (an operator's own direct keystroke into an
/// attached terminal fires the identical hook, indistinguishable from the
/// dispatcher's own typed delivery from inside the hook payload alone), so
/// composing and committing unconditionally would auto-attach whatever
/// task happens to be next in this agent's queue to a turn that has
/// nothing to do with it -- silently "delivering" a task nobody typed,
/// permanently blocking its real delivery behind `runs_one_open_per_agent`
/// with no way for the agent to ever complete it (it does not know the
/// task exists). Only when the dispatcher's own `deliver_pending`/
/// `Handle::start_task` is actively holding the slot -- meaning this
/// prompt submission is at least plausibly the one they just typed -- is
/// it safe to treat as that delivery's ack.
pub(crate) async fn commit_pending_delivery_on_prompt(
    state: &DaemonState,
    guidance_root: &Path,
    session: &SessionSnapshot,
) -> Result<(), DaemonStateError> {
    if !state.delivery_in_flight(&session.agent_id) {
        return Ok(());
    }
    let Some(delivery) = compose_delivery(
        state,
        guidance_root,
        &session.project_id,
        &session.agent_id,
        &session.id,
    )
    .await?
    else {
        return Ok(());
    };
    let now = i64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_millis())
            .unwrap_or(0),
    )
    .unwrap_or(0);
    commit_delivery(
        state,
        &session.project_id,
        &session.agent_id,
        &session.id,
        delivery,
        now,
    )
    .await?;
    Ok(())
}

/// The passive per-agent auto-delivery path: compose, type, wait for
/// acknowledgement, and only commit the episode/messages once acknowledged
/// -- unlike [`Handle::start_task`], nothing is committed on failure, so a
/// retried wake starts clean.
async fn deliver_pending(
    config: &Config,
    state: &DaemonState,
    backoff: &SpawnBackoff,
    project_id: &ProjectId,
    agent_id: &AgentId,
    session: &SessionSnapshot,
) -> Result<(), Error> {
    // Single pending-delivery slot (this track's item 1), claimed *before*
    // `compose_delivery`'s read: a `Stop`/`SubagentStop` hook for this same
    // agent can arrive concurrently (on a per-connection task in
    // `local_api.rs`, entirely independent of this dispatcher loop) and
    // race straight into `stop_hook_reply`'s own `compose_delivery` call.
    // Without claiming the slot first, both could read the same pending
    // task/messages before either commits, and both would then act on it
    // -- typing it into the PTY here *and* replying `block` with it there.
    // Skip silently on a lost race: the winner's own commit already
    // satisfies whatever this wake was for.
    let Some(_delivery_slot) = state.try_delivery_slot(agent_id) else {
        return Ok(());
    };
    // Deletion (ARCHITECTURE.md invariant 9, PR #50 review finding 5):
    // `compose_delivery` calls `compose_text`, which lazily recreates this
    // agent's guidance files (`guidance::read_or_create`) if missing --
    // gated exactly like spawn preparation, under the same per-agent lock,
    // so `Handle::begin_delete`'s drain can never miss it. A decline here
    // is silent, matching the delivery-slot miss above: whatever wake this
    // was for gets retried once the delete (or whatever else is deleting
    // this agent) finishes.
    if !backoff.try_begin_preparation(agent_id) {
        return Ok(());
    }
    let delivery_result = compose_delivery(
        state,
        &config.guidance_root,
        project_id,
        agent_id,
        &session.id,
    )
    .await;
    backoff.end_preparation(agent_id);
    let Some(delivery) = delivery_result? else {
        return Ok(());
    };
    let target = state
        .with_store({
            let project_id = project_id.clone();
            let session_id = session.id.clone();
            move |store| store.session_control_target(&project_id, &session_id)
        })
        .await?;
    let client = RunnerClient::new(
        &target.runner_runtime,
        session_run_id(&session.id)?,
        target.runner_instance_id,
    );
    let text = delivery.text.clone();
    if type_and_await_ack(state, &client, &session.id, &text).await {
        commit_delivery(
            state,
            project_id,
            agent_id,
            &session.id,
            delivery,
            now_ms()?,
        )
        .await?;
    } else {
        let reason = "delivery unacknowledged".to_owned();
        let wait_session_id = session.id.clone();
        let wait_at_ms = now_ms()?;
        state
            .commit_and_publish(move |store| {
                let (_, event) =
                    store.mark_session_waiting(&wait_session_id, reason, wait_at_ms)?;
                Ok(((), vec![event]))
            })
            .await?;
    }
    Ok(())
}

/// Types `text` into `session_id`'s PTY, then submits it with a trailing
/// `\r` sent as its own later write, waiting up to [`ACK_TIMEOUT`] for a
/// `UserPromptSubmit` hook to confirm receipt; retries once on timeout.
/// Subscribing to the daemon's event stream *before* writing (not after)
/// avoids missing a hook that fires between the write and the subscribe
/// call.
///
/// The text and its submitting `\r` are deliberately two separate
/// `TerminalInput` writes, not one buffer ending in `\r` (found manually
/// against real Claude Code -- TRACK5C-BRIEF.md step 7's manual check,
/// not a hypothetical): a multi-line composed delivery arrives at Claude
/// Code's own input box as a burst; its paste-vs-keystroke heuristic reads
/// a `\r` inside that same burst as just another inserted newline, not a
/// submission, leaving the whole delivery sitting typed-but-unsent (the
/// session durably parks at `waiting_for_input`/`delivery unacknowledged`,
/// this function's ack wait always losing the race). A short pause after
/// the text lets that burst visibly end before `\r` arrives on its own,
/// which is what actually submits it.
///
/// Deliberate simplification of TRACK5-DESIGN.md/A3's "bounded prefix
/// compare" of the acknowledged prompt against what was typed: the daemon
/// only durably publishes a hook's *category* (`last_hook_event`) and a
/// bounded, often-generic `activity` label, never the raw prompt text (see
/// `local_api::compute_hook_fields`) -- plumbing the exact text through
/// `commit_and_publish`'s event bus for this alone was out of proportion
/// here. Ack is instead "the next `UserPromptSubmit` hook for this exact
/// session, timestamped no earlier than this write" -- sound because
/// `TerminalInput` is the only writer besides the operator's own keystrokes
/// during an unacknowledged delivery, an edge case out of scope for v1.
async fn type_and_await_ack(
    state: &DaemonState,
    client: &RunnerClient,
    session_id: &SessionId,
    text: &str,
) -> bool {
    let body = encode_terminal_bytes(text.as_bytes());
    let submit = encode_terminal_bytes(b"\r");
    for _attempt in 0..2 {
        let mut events = state.subscribe();
        let Ok(write_started_at_ms) = now_ms() else {
            return false;
        };
        if client.terminal_input(body.clone()).await.is_err() {
            continue;
        }
        sleep_until(Instant::now() + SUBMIT_DELAY).await;
        if client.terminal_input(submit.clone()).await.is_err() {
            continue;
        }
        if wait_for_ack(&mut events, session_id, write_started_at_ms, ACK_TIMEOUT).await {
            return true;
        }
    }
    false
}

async fn wait_for_ack(
    events: &mut broadcast::Receiver<EventEnvelope>,
    session_id: &SessionId,
    after_ms: i64,
    ack_timeout: Duration,
) -> bool {
    let deadline = Instant::now() + ack_timeout;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return false;
        }
        match timeout(remaining, events.recv()).await {
            Ok(Ok(envelope)) => {
                if let FactoryEvent::SessionChanged { session } = &envelope.event {
                    if &session.id == session_id
                        && session.last_hook_event == Some(ProviderHookEvent::UserPromptSubmit)
                        && session.last_hook_at_ms.is_some_and(|at| at >= after_ms)
                    {
                        return true;
                    }
                }
            }
            Ok(Err(broadcast::error::RecvError::Lagged(_))) => {}
            Ok(Err(broadcast::error::RecvError::Closed)) | Err(_) => return false,
        }
    }
}

/// Composes the `Stop`/`SubagentStop` hook reply for a session that is
/// `working`/`waiting_for_input`: delivery here means replying
/// `{"decision":"block","reason":<text>}` instead of typing into the PTY,
/// so the provider's own turn loop keeps going instead of settling idle.
/// Called directly by `local_api.rs`'s `ProviderHook` handler -- the seam
/// 5A left as `stop_hook_reply` for this track.
///
/// `stop_hook_active` guards the loop the Claude/Munder Stop-hook contract
/// warns about (TRACK5-DESIGN.md §3): when the provider reports it, this
/// always replies `{}` even if work is pending, leaving it for the next
/// wake/tick instead of risking a hook that never lets its own CLI settle.
pub async fn stop_hook_reply(
    state: &DaemonState,
    guidance_root: &Path,
    session: &SessionSnapshot,
    stop_hook_active: bool,
) -> Result<serde_json::Value, DaemonStateError> {
    if stop_hook_active {
        return Ok(serde_json::json!({}));
    }
    // Single pending-delivery slot (this track's item 1), claimed *before*
    // `compose_delivery`'s read -- see `deliver_pending`'s matching
    // comment for the exact race this closes (the dispatcher's
    // tick-driven PTY-typed delivery racing this same hook reply). Losing
    // the race replies `{}`: safe, since either the winner is this same
    // agent's dispatcher path (which will actually deliver the pending
    // work) or another concurrent `Stop`/`SubagentStop` reply for this
    // same session (which will).
    let Some(_delivery_slot) = state.try_delivery_slot(&session.agent_id) else {
        return Ok(serde_json::json!({}));
    };
    let Some(delivery) = compose_delivery(
        state,
        guidance_root,
        &session.project_id,
        &session.agent_id,
        &session.id,
    )
    .await?
    else {
        return Ok(serde_json::json!({}));
    };
    let reason = delivery.text.clone();
    let now = i64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_millis())
            .unwrap_or(0),
    )
    .unwrap_or(0);
    commit_delivery(
        state,
        &session.project_id,
        &session.agent_id,
        &session.id,
        delivery,
        now,
    )
    .await?;
    Ok(serde_json::json!({"decision": "block", "reason": reason}))
}

fn compose_text(
    guidance_root: &Path,
    project_id: &ProjectId,
    agent_id: &AgentId,
    task: Option<&TaskDetail>,
    messages: &[crate::store::AgentMessage],
    role: AgentRole,
) -> String {
    let mut sections = Vec::new();
    if let Some(task) = task {
        let id = task.snapshot.id.as_str();
        let title = &task.snapshot.title;
        let body = truncate_utf8(&task.body, MAX_DELIVERY_TASK_BODY_BYTES);
        sections.push(format!(
            "Task {id}: {title} (task:{id})\nWhen finished, run: factoryctl task done --task {id} \
             --result \"<summary>\"\nIf blocked, run: factoryctl task blocked --task {id} --reason \
             \"<why>\"\n\n{body}"
        ));
    }
    if !messages.is_empty() {
        let mut block = String::from("Messages:");
        for message in messages {
            let from = message
                .sender_agent_id
                .as_ref()
                .map(AgentId::as_str)
                .unwrap_or("operator");
            block.push_str(&format!("\n- from {from}: {}", message.body));
        }
        sections.push(block);
    }
    if let Ok(project_guidance) = guidance::read_or_create(
        &factory_core::paths::project_guidance_path(guidance_root, project_id),
    ) {
        if !project_guidance.trim().is_empty() {
            sections.push(format!(
                "Project guidance (PROJECT.md):\n{project_guidance}"
            ));
        }
    }
    if let Ok(instructions) = guidance::read_or_create(
        &factory_core::paths::agent_instructions_path(guidance_root, project_id, agent_id),
    ) {
        if !instructions.trim().is_empty() {
            sections.push(format!("Standing instructions:\n{instructions}"));
        }
    }
    let memory_path = factory_core::paths::agent_memory_path(guidance_root, project_id, agent_id);
    sections.push(format!(
        "Append durable lessons to your memory file: {}",
        memory_path.display()
    ));
    if matches!(role, AgentRole::Orchestrator) {
        sections.push(ORCHESTRATOR_FOOTER.to_owned());
    }
    truncate_utf8(&sections.join("\n\n"), MAX_DELIVERY_TEXT_BYTES)
}

/// Only used by [`Handle::start_task`], which already loaded `task`/
/// `messages` via `open_run_episode` but not the agent's role; a second
/// small read is cheaper than widening `OpenedEpisode`.
async fn role_hint(state: &DaemonState, project_id: &ProjectId, agent_id: &AgentId) -> AgentRole {
    let project_id = project_id.clone();
    let agent_id = agent_id.clone();
    state
        .with_store(move |store| store.get_agent_detail(&project_id, &agent_id))
        .await
        .map(|agent| agent.snapshot.role)
        .unwrap_or(AgentRole::Worker)
}

fn truncate_utf8(value: &str, max_bytes: usize) -> String {
    if value.len() <= max_bytes {
        return value.to_owned();
    }
    let mut end = max_bytes;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\n...[truncated]", &value[..end])
}

// --- Stop/cancel/end -----------------------------------------------------

async fn end_session_now(
    state: &DaemonState,
    wake_tx: &mpsc::Sender<WakeAgent>,
    session_id: &SessionId,
    exit_code: Option<i32>,
    exit_signal: Option<i32>,
) {
    let Ok(now) = now_ms() else { return };
    let session_id = session_id.clone();
    let result = state
        .commit_and_publish(move |store| {
            let (snapshot, events) = store.end_session(&session_id, exit_code, exit_signal, now)?;
            Ok((snapshot, events))
        })
        .await;
    match result {
        Ok(snapshot) if snapshot.state == SessionState::Stopped => {
            // A clean/operator-requested end frees its agent up: if other
            // work is still queued, re-dispatch now rather than waiting for
            // the safety tick (design's "session spawned/ended" wake
            // trigger).
            send_wake(wake_tx, snapshot.project_id, snapshot.agent_id);
        }
        Ok(_) => {
            // A crash (`Failed`) deliberately does *not* get an immediate
            // re-wake: if spawning is persistently broken (a missing/
            // misconfigured provider binary), an immediate retry loop would
            // busy-spin spawn attempts as fast as they fail. The 5 second
            // safety tick still retries -- just rate-limited to once per
            // tick instead of unbounded.
        }
        Err(error) => {
            // A session already ended by another path (an operator
            // `StopSession` racing this same exit, or a second recovery
            // attempt) lands here too; not worth escalating.
            tracing::debug!(%error, "end_session did not apply");
        }
    }
}

// --- Recovery --------------------------------------------------------

/// Reconnects every session still recorded live from a prior daemon
/// instance. A session's PTY-backed process is long-lived and independent
/// of the daemon (`HANDOFF.md`: closing/rebuilding the operator surface
/// must not stop agents) -- this is what proves that survived a restart,
/// same machinery as `recoverable_sessions()`'s doc comment describes.
async fn recover_sessions(
    state: &DaemonState,
    wake_tx: &mpsc::Sender<WakeAgent>,
    shutdown_rx: &watch::Receiver<bool>,
) {
    let recoverable = match state.with_store(|store| store.recoverable_sessions()).await {
        Ok(list) => list,
        Err(error) => {
            tracing::warn!(%error, "could not load recoverable sessions");
            return;
        }
    };
    for recovered in recoverable {
        let state = state.clone();
        let wake_tx = wake_tx.clone();
        let shutdown_rx = shutdown_rx.clone();
        tokio::spawn(supervise_recovered(
            state,
            wake_tx,
            recovered,
            shutdown_rx,
            MAX_RECOVERY_ATTEMPTS,
        ));
    }
}

/// `max_attempts` is [`MAX_RECOVERY_ATTEMPTS`] in production
/// (`recover_sessions`, above); a parameter (not the bare constant) purely
/// so this track's own test can drive the give-up path without waiting
/// through the real constant's ~10 attempts (each gated by
/// [`CONNECT_GRACE`], itself not test-configurable -- see that test's own
/// comment).
async fn supervise_recovered(
    state: DaemonState,
    wake_tx: mpsc::Sender<WakeAgent>,
    recovered: RecoverableSession,
    mut shutdown_rx: watch::Receiver<bool>,
    max_attempts: u32,
) {
    let runtime_dir = PathBuf::from(&recovered.runner_runtime);
    let Ok(control_run_id) = session_run_id(&recovered.session_id) else {
        return;
    };
    let client = RunnerClient::new(
        &runtime_dir,
        control_run_id,
        recovered.runner_instance_id.clone(),
    );
    let mut retry_delay = RECOVERY_RETRY_DELAY;
    let mut attempt: u32 = 0;
    loop {
        if shutdown_requested(&shutdown_rx) {
            return;
        }
        let attach = match attach_with_grace(&client, &runtime_dir, CONNECT_GRACE, &mut shutdown_rx)
            .await
        {
            Ok(attach) => attach,
            Err(error) => {
                // Same reasoning as `Attach::Unreachable`/`ExitOutcome::
                // Reconnect` below (this track's item 9): a protocol error
                // or a corrupt-looking runtime directory is not
                // recoverable by retrying, but leaving the session
                // dangling forever in whatever state it was recovered in
                // is exactly the bug this track closes. Durably fail it
                // (`unverifiable`, like every other "gave up" exit in this
                // function) instead of just logging and abandoning it.
                tracing::warn!(%error, session_id = %recovered.session_id, "recovery attach failed");
                end_session_now(&state, &wake_tx, &recovered.session_id, None, None).await;
                return;
            }
        };
        let subscription = match attach {
            Attach::Connected(subscription) => subscription,
            Attach::Shutdown => return,
            Attach::Missing => {
                end_session_now(&state, &wake_tx, &recovered.session_id, None, None).await;
                return;
            }
            Attach::Unreachable => {
                attempt += 1;
                if attempt >= max_attempts {
                    tracing::warn!(
                        session_id = %recovered.session_id,
                        attempt,
                        "recovered session's runner stayed unreachable; giving up"
                    );
                    end_session_now(&state, &wake_tx, &recovered.session_id, None, None).await;
                    return;
                }
                tokio::select! {
                    _ = wait_for_shutdown(&mut shutdown_rx) => return,
                    () = sleep_until(Instant::now() + retry_delay) => {}
                }
                retry_delay = next_retry_delay(retry_delay);
                continue;
            }
        };
        match consume_until_exit(
            &state,
            &wake_tx,
            &recovered.session_id,
            recovered.provider == Provider::Codex,
            &client,
            subscription,
            &mut shutdown_rx,
        )
        .await
        {
            ExitOutcome::Exited {
                exit_code,
                exit_signal,
            } => {
                end_session_now(
                    &state,
                    &wake_tx,
                    &recovered.session_id,
                    exit_code,
                    exit_signal,
                )
                .await;
                return;
            }
            ExitOutcome::Shutdown => return,
            ExitOutcome::Reconnect => {
                attempt += 1;
                if attempt >= max_attempts {
                    tracing::warn!(
                        session_id = %recovered.session_id,
                        attempt,
                        "recovered session's runner connection kept dropping; giving up"
                    );
                    end_session_now(&state, &wake_tx, &recovered.session_id, None, None).await;
                    return;
                }
                tokio::select! {
                    _ = wait_for_shutdown(&mut shutdown_rx) => return,
                    () = sleep_until(Instant::now() + retry_delay) => {}
                }
                retry_delay = next_retry_delay(retry_delay);
            }
        }
    }
}

enum ExitOutcome {
    Exited {
        exit_code: Option<i32>,
        exit_signal: Option<i32>,
    },
    Shutdown,
    Reconnect,
}

/// Like `wait_for_runner_exit`, but for a session recovered after a daemon
/// restart, whose subscription is a fresh "replay-plus-live" stream from
/// sequence zero every time this reconnects (`RunnerClient::subscribe`'s
/// own doc comment): a Codex session recovered while still `starting`
/// therefore sees `RunnerEvent::TerminalRaw` again on replay if the runner
/// already logged it before the restart -- "the runner re-reports on
/// subscribe" is exactly this, no separate request needed
/// (`docs/providers.md`). `synthesize_codex_session_start` is idempotent
/// (`Store::synthesize_session_start`'s own doc comment) so seeing it again
/// on every reconnect attempt is harmless.
async fn consume_until_exit(
    state: &DaemonState,
    wake_tx: &mpsc::Sender<WakeAgent>,
    session_id: &SessionId,
    synthesize_on_raw_mode: bool,
    client: &RunnerClient,
    mut subscription: RunnerSubscription,
    shutdown: &mut watch::Receiver<bool>,
) -> ExitOutcome {
    loop {
        let item = tokio::select! {
            _ = wait_for_shutdown(shutdown) => return ExitOutcome::Shutdown,
            item = subscription.next_item() => item,
        };
        match item {
            Ok(RunnerStreamItem::Event(envelope)) => {
                if let RunnerEvent::Exited { exit_code, signal } = envelope.event {
                    // As in `wait_for_runner_exit` (`supervise_child`'s
                    // sibling for a freshly spawned session): the runner
                    // will not let its own process exit after an ordinary
                    // termination until this exact acknowledgement arrives.
                    // A recovered session's runner is otherwise
                    // indistinguishable from a fresh one here -- without
                    // this, every session that happens to exit *after* a
                    // daemon restart (not just before/during one) would
                    // orphan its runner forever.
                    if let Err(error) = client.acknowledge_exit(envelope.sequence).await {
                        tracing::warn!(%error, "failed to acknowledge a recovered runner's terminal event");
                    }
                    return ExitOutcome::Exited {
                        exit_code,
                        exit_signal: signal,
                    };
                }
                log_unrecognized_runner_event(session_id, &envelope);
                warn_on_raw_mode_timeout(session_id, synthesize_on_raw_mode, &envelope);
                if should_synthesize_session_start(synthesize_on_raw_mode, &envelope.event) {
                    synthesize_codex_session_start(state, wake_tx, session_id).await;
                }
            }
            Ok(RunnerStreamItem::CaughtUp { .. }) => {}
            Err(_) => return ExitOutcome::Reconnect,
        }
    }
}

enum Attach {
    Connected(RunnerSubscription),
    Missing,
    Unreachable,
    Shutdown,
}

/// Ported from the pre-5A execution.rs (`git show
/// 364274e:crates/factoryd/src/execution.rs`), trimmed of everything about
/// `Child`/`launch_pending` tracking (a recovered session never has a
/// `Child` handle -- see this module's top doc comment).
async fn attach_with_grace(
    client: &RunnerClient,
    runtime_dir: &Path,
    grace: Duration,
    shutdown: &mut watch::Receiver<bool>,
) -> Result<Attach, Error> {
    let deadline = Instant::now() + grace;
    loop {
        if shutdown_requested(shutdown) {
            return Ok(Attach::Shutdown);
        }
        let attempted = tokio::select! {
            _ = wait_for_shutdown(shutdown) => return Ok(Attach::Shutdown),
            result = timeout_at(deadline, client.subscribe()) => result,
        };
        match attempted {
            Ok(Ok(subscription)) => return Ok(Attach::Connected(subscription)),
            Ok(Err(error)) if unavailable(&error) => {}
            Ok(Err(error)) => return Err(Error::Runner(error)),
            Err(_) => return Ok(Attach::Unreachable),
        }
        if Instant::now() >= deadline {
            return classify_unavailable(runtime_dir);
        }
        let wake = (Instant::now() + CONNECT_RETRY_DELAY).min(deadline);
        tokio::select! {
            _ = wait_for_shutdown(shutdown) => return Ok(Attach::Shutdown),
            () = sleep_until(wake) => {}
        }
    }
}

fn unavailable(error: &RunnerClientError) -> bool {
    matches!(
        error,
        RunnerClientError::Io(source)
            if matches!(source.kind(), io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused)
    )
}

fn classify_unavailable(runtime_dir: &Path) -> Result<Attach, Error> {
    if endpoint_absent(runtime_dir)? {
        Ok(Attach::Missing)
    } else {
        Ok(Attach::Unreachable)
    }
}

fn endpoint_absent(runtime_dir: &Path) -> Result<bool, Error> {
    let runtime = match fs::symlink_metadata(runtime_dir) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(true),
        Err(_) => return Err(Error::CorruptExecution),
    };
    if runtime.file_type().is_symlink()
        || !runtime.is_dir()
        || runtime.uid() != rustix::process::geteuid().as_raw()
        || runtime.mode() & 0o777 != PRIVATE_DIRECTORY_MODE
    {
        return Err(Error::CorruptExecution);
    }
    let socket = match fs::symlink_metadata(runtime_dir.join("control.sock")) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(true),
        Err(_) => return Err(Error::CorruptExecution),
    };
    if !socket.file_type().is_socket()
        || socket.uid() != rustix::process::geteuid().as_raw()
        || socket.mode() & 0o777 != 0o600
    {
        return Err(Error::CorruptExecution);
    }
    Ok(false)
}

fn next_retry_delay(current: Duration) -> Duration {
    current
        .checked_mul(2)
        .unwrap_or(MAX_RECOVERY_RETRY_DELAY)
        .min(MAX_RECOVERY_RETRY_DELAY)
}

fn shutdown_requested(shutdown: &watch::Receiver<bool>) -> bool {
    *shutdown.borrow()
}

async fn wait_for_shutdown(shutdown: &mut watch::Receiver<bool>) {
    if *shutdown.borrow() {
        return;
    }
    let _ = shutdown.wait_for(|requested| *requested).await;
}

// --- Small helpers -----------------------------------------------------

fn session_run_id(session_id: &SessionId) -> Result<RunId, Error> {
    RunId::try_from(session_id.as_str()).map_err(|_| Error::CorruptExecution)
}

fn new_session_id() -> Result<SessionId, Error> {
    SessionId::try_from(Uuid::new_v4().hyphenated().to_string()).map_err(|_| Error::InvalidId)
}

fn new_runner_instance_id() -> Result<RunnerInstanceId, Error> {
    RunnerInstanceId::try_from(Uuid::new_v4().hyphenated().to_string())
        .map_err(|_| Error::InvalidId)
}

fn now_ms() -> Result<i64, Error> {
    let millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|_| Error::InvalidClock)?
        .as_millis();
    i64::try_from(millis).map_err(|_| Error::InvalidClock)
}

fn prepare_runtime_root(path: &std::path::Path) -> Result<(), Error> {
    if !path.is_absolute() {
        return Err(Error::InvalidRuntimeRoot);
    }
    let parent = path.parent().ok_or(Error::InvalidRuntimeRoot)?;
    match fs::symlink_metadata(parent) {
        Ok(metadata) => verify_private_directory_metadata(&metadata)?,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            fs::DirBuilder::new()
                .recursive(true)
                .mode(PRIVATE_DIRECTORY_MODE)
                .create(parent)
                .map_err(|_| Error::InvalidRuntimeRoot)?;
            let metadata = fs::symlink_metadata(parent).map_err(|_| Error::InvalidRuntimeRoot)?;
            verify_private_directory_metadata(&metadata)?;
        }
        Err(_) => return Err(Error::InvalidRuntimeRoot),
    }
    match fs::symlink_metadata(path) {
        Ok(metadata) => verify_private_directory_metadata(&metadata)?,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            fs::DirBuilder::new()
                .mode(PRIVATE_DIRECTORY_MODE)
                .create(path)
                .map_err(|_| Error::InvalidRuntimeRoot)?;
            let metadata = fs::symlink_metadata(path).map_err(|_| Error::InvalidRuntimeRoot)?;
            verify_private_directory_metadata(&metadata)?;
        }
        Err(_) => return Err(Error::InvalidRuntimeRoot),
    }
    Ok(())
}

fn verify_private_directory_metadata(metadata: &fs::Metadata) -> Result<(), Error> {
    if metadata.file_type().is_symlink()
        || !metadata.is_dir()
        || metadata.uid() != rustix::process::geteuid().as_raw()
        || metadata.mode() & 0o777 != PRIVATE_DIRECTORY_MODE
    {
        return Err(Error::InvalidRuntimeRoot);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use factory_core::TaskId;

    use super::*;
    use crate::store::Store;

    fn private_tempdir() -> tempfile::TempDir {
        let directory = tempfile::tempdir_in("/tmp").unwrap();
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700)).unwrap();
        directory
    }

    fn config(directory: &std::path::Path) -> Config {
        Config {
            runner_program: directory.join("factory-runner"),
            factoryctl_path: directory.join("factoryctl"),
            runtime_root: directory.join("runs"),
            guidance_root: directory.to_path_buf(),
            socket_path: directory.join("f.sock"),
            max_active_runs: 1,
            session_start_deadline: SESSION_START_DEADLINE,
        }
    }

    #[tokio::test]
    async fn spawn_rejects_zero_concurrency() {
        let directory = private_tempdir();
        let state = DaemonState::new(Store::open_in_memory().unwrap());
        let mut cfg = config(directory.path());
        cfg.max_active_runs = 0;
        assert!(matches!(spawn(cfg, state), Err(Error::InvalidConcurrency)));
    }

    /// This track's item 2: `max_active_runs` must actually bound live
    /// sessions, not just be validated non-zero at startup
    /// (`spawn_rejects_zero_concurrency`, above). Drives `dispatch_agent`
    /// directly (no real `factory-runner`/PTY needed -- the real E2E spawn
    /// path is exercised by `sessions_e2e.rs`) against a daemon already at
    /// its one-session cap: `waiter` has pending work and no live session,
    /// but must be left entirely alone -- not just failed-and-backed-off --
    /// while `occupant`'s session is still live. Asserting on the *session
    /// count* (not just `live_session_for_agent(waiter)`, which would also
    /// read `None` after a failed spawn attempt, since a failed session's
    /// `ended_at_ms` is set) is what actually distinguishes "never
    /// attempted" from "attempted and failed": `config.runner_program`
    /// deliberately points at a nonexistent binary, so a real spawn
    /// attempt here would durably fail loudly, not silently succeed.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn dispatch_agent_defers_spawn_at_the_concurrency_limit() {
        let directory = private_tempdir();
        let project_id = ProjectId::try_from("factory").unwrap();
        let occupant_id = AgentId::try_from("occupant").unwrap();
        let waiter_id = AgentId::try_from("waiter").unwrap();
        let worktree = directory.path().join("repo").to_string_lossy().into_owned();

        let mut store = Store::open_in_memory().unwrap();
        store
            .create_project(
                crate::store::NewProject {
                    id: project_id.clone(),
                    name: "Factory".to_owned(),
                    root: worktree.clone(),
                },
                1_000,
            )
            .unwrap();
        for agent_id in [&occupant_id, &waiter_id] {
            store
                .create_agent(
                    crate::store::NewAgent {
                        id: agent_id.clone(),
                        project_id: project_id.clone(),
                        parent_agent_id: None,
                        role: AgentRole::Worker,
                        provider: Provider::Shell,
                    },
                    1_000,
                )
                .unwrap();
        }
        // `occupant` already holds the daemon's one concurrency slot.
        store
            .create_session(
                crate::store::NewSession {
                    id: SessionId::try_from("11111111-1111-4111-8111-111111111111").unwrap(),
                    project_id: project_id.clone(),
                    agent_id: occupant_id.clone(),
                    provider: Provider::Shell,
                    runtime_model: None,
                    runtime_reasoning_effort: None,
                    runtime_permission_mode: None,
                    runtime_control_mode: None,
                    provider_session_id: None,
                    worktree: worktree.clone(),
                    codex_home: None,
                    hook_token: "a".repeat(64),
                    runner_instance_id: RunnerInstanceId::try_from(
                        "22222222-2222-4222-8222-222222222222",
                    )
                    .unwrap(),
                    runner_runtime: directory
                        .path()
                        .join("runs")
                        .join("session-1")
                        .to_string_lossy()
                        .into_owned(),
                    runner_protocol_version: 1,
                },
                1_000,
            )
            .unwrap();
        // `waiter` has pending work and no live session of its own.
        store
            .create_task(
                crate::store::NewTask {
                    id: TaskId::try_from("task-1").unwrap(),
                    project_id: project_id.clone(),
                    parent_task_id: None,
                    title: "Do the thing".to_owned(),
                    body: "Do the thing.".to_owned(),
                    priority: 0,
                },
                1_000,
            )
            .unwrap();
        store
            .assign_task(
                &project_id,
                &TaskId::try_from("task-1").unwrap(),
                Some(&waiter_id),
                1_000,
            )
            .unwrap();

        let state = DaemonState::new(store);
        let mut cfg = config(directory.path());
        cfg.max_active_runs = 1;
        let (wake_tx, _wake_rx) = mpsc::channel(8);
        let backoff = SpawnBackoff::new();

        assert!(
            at_concurrency_limit(&cfg, &state, &project_id, &waiter_id)
                .await
                .unwrap(),
            "one live session must already saturate max_active_runs: 1"
        );

        dispatch_agent(&cfg, &state, &wake_tx, &backoff, &project_id, &waiter_id)
            .await
            .unwrap();

        let sessions = state
            .with_store({
                let project_id = project_id.clone();
                move |store| store.list_sessions(&project_id, None, 10)
            })
            .await
            .unwrap();
        assert_eq!(
            sessions.len(),
            1,
            "waiter must not get a session row at all -- not attempted, not failed"
        );
        assert_eq!(sessions[0].agent_id, occupant_id);

        // Freeing the slot lets the very next dispatch through -- no
        // artificial backoff delay left over from being deferred.
        state
            .commit_and_publish({
                let session_id = sessions[0].id.clone();
                move |store| {
                    let (snapshot, events) =
                        store.end_session_with_reason(&session_id, None, None, None, 2_000)?;
                    Ok((snapshot, events))
                }
            })
            .await
            .unwrap();
        assert!(
            !at_concurrency_limit(&cfg, &state, &project_id, &waiter_id)
                .await
                .unwrap(),
            "ending occupant's session must free the slot immediately"
        );
    }

    /// Deletion's ordering half of this task's mechanism (ARCHITECTURE.md's
    /// "once deletion begins, no component may create new state under that
    /// identity"): once `SpawnBackoff::begin_delete` has marked an agent,
    /// `dispatch_agent` must not create a session row for it at all -- not
    /// attempt-then-fail, not attempt-then-succeed -- exactly the same
    /// "not attempted" assertion `dispatch_agent_defers_spawn_at_the_concurrency_limit`
    /// makes for the concurrency limit, above. Deterministic: no timing,
    /// just direct state.
    ///
    /// The agent is given a real worktree on disk (unlike that sibling
    /// test) precisely so this one is *not* vacuous (PR #50 review,
    /// should-fix 6): without a worktree, `spawn_session_for_agent` bails
    /// at `Error::NoWorktree` before ever reaching `create_session`, so
    /// `sessions.is_empty()` would hold whether or not
    /// `try_begin_preparation`'s gate exists at all. With a worktree,
    /// `spawn_session_for_agent` gets as far as `create_session` before
    /// failing at the nonexistent `runner_program` -- verified by mutation:
    /// hard-coding `try_begin_preparation` to always return `true` makes
    /// this assertion fail (a session row does get created), and reverting
    /// that makes it pass again.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn dispatch_agent_declines_to_prepare_for_a_deleting_agent() {
        let directory = private_tempdir();
        let project_id = ProjectId::try_from("factory").unwrap();
        let agent_id = AgentId::try_from("curie").unwrap();
        let worktree = directory.path().join("repo").to_string_lossy().into_owned();

        let mut store = Store::open_in_memory().unwrap();
        store
            .create_project(
                crate::store::NewProject {
                    id: project_id.clone(),
                    name: "Factory".to_owned(),
                    root: worktree.clone(),
                },
                1_000,
            )
            .unwrap();
        store
            .create_agent(
                crate::store::NewAgent {
                    id: agent_id.clone(),
                    project_id: project_id.clone(),
                    parent_agent_id: None,
                    role: AgentRole::Worker,
                    provider: Provider::Shell,
                },
                1_000,
            )
            .unwrap();
        store
            .set_agent_worktree(&project_id, &agent_id, worktree.clone(), 1_000)
            .unwrap();
        std::fs::create_dir_all(&worktree).unwrap();
        store
            .create_task(
                crate::store::NewTask {
                    id: TaskId::try_from("task-1").unwrap(),
                    project_id: project_id.clone(),
                    parent_task_id: None,
                    title: "Do the thing".to_owned(),
                    body: "Do the thing.".to_owned(),
                    priority: 0,
                },
                1_000,
            )
            .unwrap();
        store
            .assign_task(
                &project_id,
                &TaskId::try_from("task-1").unwrap(),
                Some(&agent_id),
                1_000,
            )
            .unwrap();

        let state = DaemonState::new(store);
        let cfg = config(directory.path());
        let (wake_tx, _wake_rx) = mpsc::channel(8);
        let backoff = SpawnBackoff::new();

        // Simulates `Handle::begin_delete`'s first step: mark the agent
        // deleting before anything else runs.
        backoff.begin_delete(&agent_id);
        assert!(
            !backoff.try_begin_preparation(&agent_id),
            "a deleting agent must never begin a new spawn preparation"
        );

        dispatch_agent(&cfg, &state, &wake_tx, &backoff, &project_id, &agent_id)
            .await
            .unwrap();

        let sessions = state
            .with_store({
                let project_id = project_id.clone();
                move |store| store.list_sessions(&project_id, None, 10)
            })
            .await
            .unwrap();
        assert!(
            sessions.is_empty(),
            "a deleting agent must not get a session row at all -- not attempted, not failed"
        );

        // Clearing the mark (`Handle::end_delete`'s job) restores normal
        // dispatch for a future agent with the same id.
        backoff.end_delete(&agent_id);
        assert!(
            backoff.try_begin_preparation(&agent_id),
            "clearing the mark must restore normal dispatch"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn dispatch_agent_never_spawns_for_an_exhausted_budget() {
        let directory = private_tempdir();
        let project_id = ProjectId::try_from("factory").unwrap();
        let agent_id = AgentId::try_from("worker").unwrap();
        let task_id = TaskId::try_from("task-1").unwrap();
        let worktree = directory.path().join("repo").to_string_lossy().into_owned();
        std::fs::create_dir_all(&worktree).unwrap();
        let mut store = Store::open_in_memory().unwrap();
        store
            .create_project(
                crate::store::NewProject {
                    id: project_id.clone(),
                    name: "Factory".into(),
                    root: worktree.clone(),
                },
                1,
            )
            .unwrap();
        store
            .create_agent(
                crate::store::NewAgent {
                    id: agent_id.clone(),
                    project_id: project_id.clone(),
                    parent_agent_id: None,
                    role: AgentRole::Worker,
                    provider: Provider::Shell,
                },
                2,
            )
            .unwrap();
        store
            .set_agent_worktree(&project_id, &agent_id, worktree, 3)
            .unwrap();
        store
            .create_task(
                crate::store::NewTask {
                    id: task_id.clone(),
                    project_id: project_id.clone(),
                    parent_task_id: None,
                    title: "Task".into(),
                    body: String::new(),
                    priority: 0,
                },
                4,
            )
            .unwrap();
        store
            .assign_task(&project_id, &task_id, Some(&agent_id), 5)
            .unwrap();
        store
            .set_agent_budget(&project_id, &agent_id, Some(1), 6)
            .unwrap();
        assert!(
            !store
                .observe_tool_call(&project_id, &agent_id, 7)
                .unwrap()
                .1
        );
        assert!(
            store
                .observe_tool_call(&project_id, &agent_id, 8)
                .unwrap()
                .1
        );

        let state = DaemonState::new(store);
        let (wake_tx, _wake_rx) = mpsc::channel(8);
        dispatch_agent(
            &config(directory.path()),
            &state,
            &wake_tx,
            &SpawnBackoff::new(),
            &project_id,
            &agent_id,
        )
        .await
        .unwrap();
        let sessions = state
            .with_store({
                let project_id = project_id.clone();
                move |store| store.list_sessions(&project_id, None, 10)
            })
            .await
            .unwrap();
        assert!(
            sessions.is_empty(),
            "budget hold must prevent even a failed spawn attempt"
        );
    }

    /// Deletion's waiting half: [`SpawnBackoff::wait_for_drain`] must not
    /// observe an agent as drained while a preparation `try_begin_preparation`
    /// already admitted is still in flight, and must unblock as soon as
    /// [`SpawnBackoff::end_preparation`] ends it. Uses a paused clock
    /// (`start_paused`) rather than a real sleep-and-hope: `SpawnBackoff`
    /// times entirely through `tokio::time::Instant`, so advancing the
    /// virtual clock is what actually drives `wait_for_drain`'s poll loop,
    /// making the "still waiting" assertion below deterministic rather than
    /// a timing guess.
    #[tokio::test(start_paused = true)]
    async fn wait_for_drain_blocks_until_the_in_flight_preparation_ends() {
        let backoff = Arc::new(SpawnBackoff::new());
        let agent_id = AgentId::try_from("curie").unwrap();

        assert!(
            backoff.try_begin_preparation(&agent_id),
            "simulates the dispatcher having just begun a spawn preparation"
        );

        let waiter_backoff = Arc::clone(&backoff);
        let waiter_agent_id = agent_id.clone();
        let waiter = tokio::spawn(async move {
            waiter_backoff.begin_delete(&waiter_agent_id);
            waiter_backoff
                .wait_for_drain(&waiter_agent_id, Duration::from_secs(5))
                .await
        });

        // Let the waiter run up to its first `sleep_until` and register on
        // the (paused) clock; it cannot possibly have observed a drain yet
        // because time has not moved and `end_preparation` has not been
        // called.
        tokio::task::yield_now().await;
        assert!(
            !waiter.is_finished(),
            "must still be waiting on the in-flight preparation"
        );

        backoff.end_preparation(&agent_id);
        // Advances past one poll tick so `wait_for_drain`'s loop wakes,
        // re-checks, and observes the drain.
        tokio::time::advance(DELETE_DRAIN_POLL).await;
        assert!(
            waiter.await.unwrap(),
            "must observe the drain once the preparation ends, well within its budget"
        );
    }

    /// PR #50 review, blocking finding 1, reproduced verbatim as a unit
    /// probe: a spawn that *succeeds* while a delete is draining used to
    /// have its `record_success` (`state.remove`, back when `SpawnBackoff`
    /// kept backoff timing and the delete mark in one entry) erase the
    /// concurrent `deleting` mark, so the drain reported "drained" with no
    /// mark set at all -- reopening #42's exact race for the *next*
    /// dispatch. Now that `SpawnBackoff::timing` and `SpawnBackoff::gate`
    /// are separate locks, `record_success` cannot reach `gate` no matter
    /// what it does.
    #[tokio::test]
    async fn record_success_during_a_drain_does_not_erase_the_deleting_mark() {
        let backoff = SpawnBackoff::new();
        let agent_id = AgentId::try_from("curie").unwrap();

        assert!(backoff.try_begin_preparation(&agent_id));
        backoff.begin_delete(&agent_id);
        // dispatch_agent's own sequence on a successful spawn
        // (execution.rs's spawn branch): end the preparation, then record
        // the backoff success.
        backoff.end_preparation(&agent_id);
        backoff.record_success(&agent_id);

        assert!(
            backoff
                .wait_for_drain(&agent_id, DELETE_DRAIN_TIMEOUT)
                .await,
            "the preparation already ended, so this must drain immediately"
        );
        assert!(
            !backoff.try_begin_preparation(&agent_id),
            "the deleting mark must survive a concurrent record_success"
        );
    }

    /// PR #50 review, blocking finding 2, reproduced verbatim: the retry
    /// `Handle::begin_delete`'s own error message tells the operator to
    /// make ("try the delete again") used to bypass the drain entirely,
    /// because `end_delete`'s timeout-branch call was `state.remove`,
    /// which discarded the `preparing` count of the write that was *still
    /// running*. The very next `begin_delete` then saw a fresh, empty
    /// entry and reported "drained" immediately. `end_delete` now clears
    /// only `deleting`, so a retry keeps seeing the real in-flight count
    /// until the write actually ends.
    #[tokio::test(start_paused = true)]
    async fn a_timed_out_drain_retry_still_waits_for_the_still_in_flight_preparation() {
        let backoff = SpawnBackoff::new();
        let agent_id = AgentId::try_from("curie").unwrap();

        // A preparation that outlives the drain timeout entirely.
        assert!(backoff.try_begin_preparation(&agent_id));

        backoff.begin_delete(&agent_id);
        assert!(
            !backoff
                .wait_for_drain(&agent_id, DELETE_DRAIN_TIMEOUT)
                .await,
            "must time out while the preparation is still in flight"
        );
        // `Handle::begin_delete`'s timeout branch, verbatim.
        backoff.end_delete(&agent_id);

        // The operator retries, exactly as the error message says to.
        backoff.begin_delete(&agent_id);
        assert!(
            !backoff
                .wait_for_drain(&agent_id, DELETE_DRAIN_TIMEOUT)
                .await,
            "the retry must still see the still-in-flight preparation, not report drained"
        );

        // The preparation finally ends -- a third attempt now genuinely
        // drains.
        backoff.end_preparation(&agent_id);
        backoff.begin_delete(&agent_id);
        assert!(
            backoff
                .wait_for_drain(&agent_id, DELETE_DRAIN_TIMEOUT)
                .await
        );
    }

    /// PR #50 review, blocking finding 3/should-fix 5: `Handle`'s
    /// project-scoped `DeleteGate<ProjectId>` (`try_begin_project_write`/
    /// `begin_delete_project`, used by `CreateAgent`/`DeleteProject`) is
    /// the exact same generic mechanism as the agent-scoped one the tests
    /// above already prove correct, just instantiated over `ProjectId`
    /// instead of `AgentId`. This drives it directly rather than only
    /// trusting that the generic code behaves identically for a different
    /// key type: a `CreateAgent`-equivalent write declines once the
    /// project is marked deleting, and a fresh write is accepted again
    /// once the mark clears.
    #[tokio::test]
    async fn project_delete_gate_declines_a_create_agent_style_write_while_deleting() {
        let gate: DeleteGate<ProjectId> = DeleteGate::new();
        let project_id = ProjectId::try_from("factory").unwrap();

        assert!(
            gate.try_begin_preparation(&project_id),
            "an ordinary CreateAgent-style write is accepted before any delete begins"
        );
        gate.end_preparation(&project_id);

        gate.begin_delete(&project_id);
        assert!(
            !gate.try_begin_preparation(&project_id),
            "CreateAgent must decline outright once DeleteProject has marked the project"
        );

        gate.end_delete(&project_id);
        assert!(
            gate.try_begin_preparation(&project_id),
            "clearing the mark must restore normal CreateAgent behavior"
        );
    }

    /// PR #50 re-review, round 3's remaining finding: `CreateAgent` must
    /// decline when its `parent_agent_id` is currently being deleted, not
    /// just when the project or the new agent's own id is -- otherwise
    /// `AgentHasChildren` (the one delete precondition a *different*
    /// request can flip from false to true) can go stale between
    /// `DeleteAgent`'s precheck and its actual removals. Deterministic:
    /// this is the exact `try_begin_preparation`/`begin_delete`/
    /// `end_delete` sequence `Handle::begin_delete` and `CreateAgent`'s
    /// new parent-id check both go through, with no timing involved.
    #[test]
    fn create_agent_declines_a_parent_currently_being_deleted() {
        let backoff = SpawnBackoff::new();
        let parent_id = AgentId::try_from("boss").unwrap();

        // Simulates `Handle::begin_delete`'s first step for `DeleteAgent
        // boss`: the mark is set before its precheck or any file removal
        // runs.
        backoff.begin_delete(&parent_id);

        // This is exactly what `CreateAgent`'s new parent-id check calls.
        assert!(
            !backoff.try_begin_preparation(&parent_id),
            "CreateAgent must decline when its intended parent is currently being deleted"
        );

        // `Handle::end_delete`, called once `DeleteAgent boss` finishes
        // (refused or not) -- restores normal `CreateAgent` behavior for
        // the same id.
        backoff.end_delete(&parent_id);
        assert!(
            backoff.try_begin_preparation(&parent_id),
            "clearing the mark must restore normal CreateAgent behavior for the parent id"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn dispatch_agent_never_touches_a_stop_requested_starting_session_past_its_deadline() {
        let directory = private_tempdir();
        let project_id = ProjectId::try_from("factory").unwrap();
        let agent_id = AgentId::try_from("stuck").unwrap();
        let worktree = directory.path().join("repo").to_string_lossy().into_owned();
        let session_id = SessionId::try_from("11111111-1111-4111-8111-111111111111").unwrap();

        let mut store = Store::open_in_memory().unwrap();
        store
            .create_project(
                crate::store::NewProject {
                    id: project_id.clone(),
                    name: "Factory".to_owned(),
                    root: worktree.clone(),
                },
                1_000,
            )
            .unwrap();
        store
            .create_agent(
                crate::store::NewAgent {
                    id: agent_id.clone(),
                    project_id: project_id.clone(),
                    parent_agent_id: None,
                    role: AgentRole::Worker,
                    provider: Provider::Shell,
                },
                1_000,
            )
            .unwrap();
        store
            .create_session(
                crate::store::NewSession {
                    id: session_id.clone(),
                    project_id: project_id.clone(),
                    agent_id: agent_id.clone(),
                    provider: Provider::Shell,
                    runtime_model: None,
                    runtime_reasoning_effort: None,
                    runtime_permission_mode: None,
                    runtime_control_mode: None,
                    provider_session_id: None,
                    worktree: worktree.clone(),
                    codex_home: None,
                    hook_token: "a".repeat(64),
                    runner_instance_id: RunnerInstanceId::try_from(
                        "22222222-2222-4222-8222-222222222222",
                    )
                    .unwrap(),
                    runner_runtime: directory
                        .path()
                        .join("runs")
                        .join("session-1")
                        .to_string_lossy()
                        .into_owned(),
                    runner_protocol_version: 1,
                },
                1_000,
            )
            .unwrap();
        store
            .request_session_stop(&project_id, &session_id, 1_500)
            .unwrap();

        let state = DaemonState::new(store);
        let cfg = config(directory.path());
        let (wake_tx, _wake_rx) = mpsc::channel(8);
        let backoff = SpawnBackoff::new();
        dispatch_agent(&cfg, &state, &wake_tx, &backoff, &project_id, &agent_id)
            .await
            .unwrap();

        let sessions = state
            .with_store({
                let project_id = project_id.clone();
                move |store| store.list_sessions(&project_id, None, 10)
            })
            .await
            .unwrap();
        let session = sessions
            .into_iter()
            .find(|session| session.id == session_id)
            .unwrap();
        assert_eq!(session.state, SessionState::Starting);
        assert!(session.ended_at_ms.is_none());
        assert!(session.wait_reason.is_none());
        assert!(backoff.ready(&agent_id));
    }

    /// Q1 fix (`synthesize_codex_session_start`'s own doc comment has the
    /// live/empirical evidence): Codex 0.147 never fires its own
    /// `SessionStart` hook at TUI startup, so a fresh Codex session would
    /// otherwise sit in `starting` forever. This test proves the automatic
    /// transition wakes delivery and stays distinguishable from a real hook.
    #[tokio::test]
    async fn synthesize_codex_session_start_moves_a_fresh_codex_session_to_idle_and_wakes_it() {
        let project_id = ProjectId::try_from("factory").unwrap();
        let agent_id = AgentId::try_from("worker-1").unwrap();
        let mut store = Store::open_in_memory().unwrap();
        store
            .create_project(
                crate::store::NewProject {
                    id: project_id.clone(),
                    name: "Factory".to_owned(),
                    root: "/tmp/factory".to_owned(),
                },
                1_000,
            )
            .unwrap();
        store
            .create_agent(
                crate::store::NewAgent {
                    id: agent_id.clone(),
                    project_id: project_id.clone(),
                    parent_agent_id: None,
                    role: AgentRole::Worker,
                    provider: Provider::Codex,
                },
                1_000,
            )
            .unwrap();
        let session_id = SessionId::try_from("11111111-1111-4111-8111-111111111111").unwrap();
        let (snapshot, _) = store
            .create_session(
                crate::store::NewSession {
                    id: session_id.clone(),
                    project_id: project_id.clone(),
                    agent_id: agent_id.clone(),
                    provider: Provider::Codex,
                    runtime_model: None,
                    runtime_reasoning_effort: None,
                    runtime_permission_mode: None,
                    runtime_control_mode: None,
                    provider_session_id: None,
                    worktree: "/tmp/factory/worktree".to_owned(),
                    codex_home: Some("/tmp/factory/codex-home".to_owned()),
                    hook_token: "a".repeat(64),
                    runner_instance_id: RunnerInstanceId::try_from(
                        "22222222-2222-4222-8222-222222222222",
                    )
                    .unwrap(),
                    runner_runtime: "/tmp/factory/runs/session-1".to_owned(),
                    runner_protocol_version: 1,
                },
                1_000,
            )
            .unwrap();
        assert_eq!(snapshot.state, SessionState::Starting);

        let state = DaemonState::new(store);
        let (wake_tx, mut wake_rx) = mpsc::channel(8);
        synthesize_codex_session_start(&state, &wake_tx, &session_id).await;

        let sessions = state
            .with_store({
                let project_id = project_id.clone();
                move |store| store.list_sessions(&project_id, None, 10)
            })
            .await
            .unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].state, SessionState::Idle);
        assert_eq!(sessions[0].last_hook_event, None);

        let woken = wake_rx
            .try_recv()
            .expect("synthesis must wake the dispatcher");
        assert_eq!(woken.project_id, project_id);
        assert_eq!(woken.agent_id, agent_id);
    }

    /// Adversarial review finding 1's fix, condition (b): a pure unit test
    /// of the exact gate `wait_for_runner_exit`/`consume_until_exit` use --
    /// `synthesize_on_raw_mode` is derived once, at each caller's own spawn
    /// or recovery site, from `provider == Provider::Codex`; this proves
    /// the gate itself only ever says yes for that combination.
    #[test]
    fn should_synthesize_session_start_only_for_codex_and_only_on_terminal_raw() {
        assert!(should_synthesize_session_start(
            true,
            &RunnerEvent::TerminalRaw
        ));
        assert!(!should_synthesize_session_start(
            false,
            &RunnerEvent::TerminalRaw
        ));
        assert!(!should_synthesize_session_start(
            true,
            &RunnerEvent::Started { child_pid: 1 }
        ));
        assert!(!should_synthesize_session_start(
            false,
            &RunnerEvent::Started { child_pid: 1 }
        ));
    }

    /// End-to-end version of the same gate (review finding 9b): three
    /// sessions -- Codex, Claude, `shell` -- each `starting`, each observes
    /// the same `RunnerEvent::TerminalRaw` with `synthesize_on_raw_mode`
    /// derived exactly as `spawn_session_for_agent`/`supervise_recovered` do
    /// it (`provider == Provider::Codex`). Only the Codex session reaches
    /// `idle`; Claude and `shell` are left alone for their own real hooks.
    #[tokio::test]
    async fn terminal_raw_only_synthesizes_session_start_for_codex_never_for_claude_or_shell() {
        let project_id = ProjectId::try_from("factory").unwrap();
        let mut store = Store::open_in_memory().unwrap();
        store
            .create_project(
                crate::store::NewProject {
                    id: project_id.clone(),
                    name: "Factory".to_owned(),
                    root: "/tmp/factory".to_owned(),
                },
                1_000,
            )
            .unwrap();

        let providers = [
            (
                "codex-agent",
                Provider::Codex,
                "11111111-1111-4111-8111-111111111111",
            ),
            (
                "claude-agent",
                Provider::ClaudeCode,
                "22222222-2222-4222-8222-222222222222",
            ),
            (
                "shell-agent",
                Provider::Shell,
                "33333333-3333-4333-8333-333333333333",
            ),
        ];
        let mut session_ids = Vec::new();
        for (agent_name, provider, session_uuid) in providers {
            let agent_id = AgentId::try_from(agent_name).unwrap();
            store
                .create_agent(
                    crate::store::NewAgent {
                        id: agent_id.clone(),
                        project_id: project_id.clone(),
                        parent_agent_id: None,
                        role: AgentRole::Worker,
                        provider,
                    },
                    1_000,
                )
                .unwrap();
            let session_id = SessionId::try_from(session_uuid).unwrap();
            store
                .create_session(
                    crate::store::NewSession {
                        id: session_id.clone(),
                        project_id: project_id.clone(),
                        agent_id,
                        provider,
                        runtime_model: None,
                        runtime_reasoning_effort: None,
                        runtime_permission_mode: None,
                        runtime_control_mode: None,
                        provider_session_id: None,
                        worktree: "/tmp/factory/worktree".to_owned(),
                        codex_home: None,
                        hook_token: "a".repeat(64),
                        runner_instance_id: RunnerInstanceId::try_from(session_uuid).unwrap(),
                        runner_runtime: format!("/tmp/factory/runs/{agent_name}"),
                        runner_protocol_version: 1,
                    },
                    1_000,
                )
                .unwrap();
            session_ids.push((provider, session_id));
        }

        let state = DaemonState::new(store);
        let (wake_tx, _wake_rx) = mpsc::channel(8);
        for (provider, session_id) in &session_ids {
            let synthesize_on_raw_mode = *provider == Provider::Codex;
            if should_synthesize_session_start(synthesize_on_raw_mode, &RunnerEvent::TerminalRaw) {
                synthesize_codex_session_start(&state, &wake_tx, session_id).await;
            }
        }

        let sessions = state
            .with_store({
                let project_id = project_id.clone();
                move |store| store.list_sessions(&project_id, None, 10)
            })
            .await
            .unwrap();
        for session in sessions {
            if session.provider == Provider::Codex {
                assert_eq!(
                    session.state,
                    SessionState::Idle,
                    "the Codex session must be synthesized to idle"
                );
            } else {
                assert_eq!(
                    session.state,
                    SessionState::Starting,
                    "{:?} must never be synthesized -- it waits for its own real hook",
                    session.provider
                );
            }
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn start_task_without_a_live_session_fails_clearly() {
        let directory = private_tempdir();
        let state = DaemonState::new(Store::open_in_memory().unwrap());
        let (handle, join) = spawn(config(directory.path()), state).unwrap();
        let result = handle
            .start_task(StartTask {
                project_id: ProjectId::try_from("factory").unwrap(),
                task_id: TaskId::try_from("task-1").unwrap(),
                agent_id: AgentId::try_from("agent-1").unwrap(),
                parent_run_id: None,
                worktree: directory.path().to_path_buf(),
            })
            .await;
        assert!(matches!(result, Err(Error::NoLiveSession)));
        handle.shutdown().await.unwrap();
        join.await.unwrap().unwrap();
    }

    /// This track's item 9: a recovered session's runner that never
    /// becomes reachable again (a stale `control.sock` -- bound once,
    /// then its listener dropped without unlinking the file, exactly
    /// what a runner process's own OS-level teardown can leave behind if
    /// it never gets the chance to run its normal cleanup) must not stay
    /// dangling in whatever state it was recovered in forever; it must
    /// durably fail. Drives `supervise_recovered` directly with
    /// `max_attempts: 1` so this test only waits through one
    /// `CONNECT_GRACE` window (~5s) instead of production's real
    /// `MAX_RECOVERY_ATTEMPTS` -- see `supervise_recovered`'s own doc
    /// comment for why that parameter exists at all.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_recovered_session_whose_runner_stays_unreachable_is_durably_failed_not_left_dangling()
     {
        let directory = private_tempdir();
        let project_id = ProjectId::try_from("factory").unwrap();
        let agent_id = AgentId::try_from("curie").unwrap();
        let worktree = directory.path().join("repo").to_string_lossy().into_owned();

        let mut store = Store::open_in_memory().unwrap();
        store
            .create_project(
                crate::store::NewProject {
                    id: project_id.clone(),
                    name: "Factory".to_owned(),
                    root: worktree.clone(),
                },
                1_000,
            )
            .unwrap();
        store
            .create_agent(
                crate::store::NewAgent {
                    id: agent_id.clone(),
                    project_id: project_id.clone(),
                    parent_agent_id: None,
                    role: AgentRole::Worker,
                    provider: Provider::Shell,
                },
                1_000,
            )
            .unwrap();

        let runtime_dir = directory.path().join("runs").join("session-1");
        fs::DirBuilder::new()
            .recursive(true)
            .mode(PRIVATE_DIRECTORY_MODE)
            .create(&runtime_dir)
            .unwrap();
        let socket_path = runtime_dir.join("control.sock");
        {
            let listener = std::os::unix::net::UnixListener::bind(&socket_path).unwrap();
            fs::set_permissions(&socket_path, fs::Permissions::from_mode(0o600)).unwrap();
            drop(listener);
        }

        let session_id = SessionId::try_from("11111111-1111-4111-8111-111111111111").unwrap();
        let runner_instance_id =
            RunnerInstanceId::try_from("22222222-2222-4222-8222-222222222222").unwrap();
        store
            .create_session(
                crate::store::NewSession {
                    id: session_id.clone(),
                    project_id: project_id.clone(),
                    agent_id: agent_id.clone(),
                    provider: Provider::Shell,
                    runtime_model: None,
                    runtime_reasoning_effort: None,
                    runtime_permission_mode: None,
                    runtime_control_mode: None,
                    provider_session_id: None,
                    worktree: worktree.clone(),
                    codex_home: None,
                    hook_token: "a".repeat(64),
                    runner_instance_id: runner_instance_id.clone(),
                    runner_runtime: runtime_dir.to_string_lossy().into_owned(),
                    runner_protocol_version: 1,
                },
                1_000,
            )
            .unwrap();

        let state = DaemonState::new(store);
        let (wake_tx, _wake_rx) = mpsc::channel(8);
        let (_shutdown_tx, shutdown_rx) = watch::channel(false);
        let recovered = crate::store::RecoverableSession {
            session_id: session_id.clone(),
            provider: Provider::Shell,
            provider_session_id: None,
            worktree,
            runner_instance_id,
            runner_runtime: runtime_dir.to_string_lossy().into_owned(),
            runner_protocol_version: 1,
            observer_health: factory_core::ObserverHealth::Unknown,
        };

        supervise_recovered(state.clone(), wake_tx, recovered, shutdown_rx, 1).await;

        let sessions = state
            .with_store(move |store| store.list_sessions(&project_id, None, 10))
            .await
            .unwrap();
        let session = sessions
            .into_iter()
            .find(|session| session.id == session_id)
            .expect("the recovered session must still exist");
        assert_eq!(session.state, SessionState::Failed);
        assert!(!session.state.is_live());
    }

    /// Issue #85 and PR #90 review: only a run created after candidate
    /// composition proves the synchronous hook won this attempt. An older
    /// episode for the same task/session must not hide either error form
    /// after that task is retried.
    #[tokio::test]
    async fn delivery_commit_ignores_only_a_task_already_run_in_the_same_session() {
        let project_id = ProjectId::try_from("factory").unwrap();
        let agent_id = AgentId::try_from("curie").unwrap();
        let session_id = SessionId::try_from("11111111-1111-4111-8111-111111111111").unwrap();
        let race_task_id = TaskId::try_from("race-task").unwrap();
        let aba_task_id = TaskId::try_from("aba-task").unwrap();
        let task_id = TaskId::try_from("task-1").unwrap();
        let occupying_task_id = TaskId::try_from("task-2").unwrap();
        let mut store = Store::open_in_memory().unwrap();
        store
            .create_project(
                crate::store::NewProject {
                    id: project_id.clone(),
                    name: "Factory".to_owned(),
                    root: "/tmp/factory".to_owned(),
                },
                1_000,
            )
            .unwrap();
        store
            .create_agent(
                crate::store::NewAgent {
                    id: agent_id.clone(),
                    project_id: project_id.clone(),
                    parent_agent_id: None,
                    role: AgentRole::Worker,
                    provider: Provider::Shell,
                },
                1_000,
            )
            .unwrap();
        store
            .create_session(
                crate::store::NewSession {
                    id: session_id.clone(),
                    project_id: project_id.clone(),
                    agent_id: agent_id.clone(),
                    provider: Provider::Shell,
                    runtime_model: None,
                    runtime_reasoning_effort: None,
                    runtime_permission_mode: None,
                    runtime_control_mode: None,
                    provider_session_id: None,
                    worktree: "/tmp/factory".to_owned(),
                    codex_home: None,
                    hook_token: "a".repeat(64),
                    runner_instance_id: RunnerInstanceId::try_from(
                        "22222222-2222-4222-8222-222222222222",
                    )
                    .unwrap(),
                    runner_runtime: "/tmp/factory-runner".to_owned(),
                    runner_protocol_version: 1,
                },
                1_000,
            )
            .unwrap();
        store
            .record_hook_event(
                &session_id,
                ProviderHookEvent::SessionStart,
                None,
                false,
                None,
                1_001,
            )
            .unwrap();
        for id in [&race_task_id, &aba_task_id, &task_id, &occupying_task_id] {
            store
                .create_task(
                    crate::store::NewTask {
                        id: id.clone(),
                        project_id: project_id.clone(),
                        parent_task_id: None,
                        title: "Do the thing".to_owned(),
                        body: "Do the thing.".to_owned(),
                        priority: 0,
                    },
                    1_002,
                )
                .unwrap();
            store
                .assign_task(&project_id, id, Some(&agent_id), 1_003)
                .unwrap();
        }

        // Original race: the candidate observes no prior episode, then the
        // hook opens one and the fast client finishes it before this commit.
        let (race_incarnation, prior_run_count) = store
            .task_delivery_marker(&session_id, &race_task_id)
            .unwrap();
        store
            .open_run_episode(&session_id, &race_task_id, 1_004)
            .unwrap();
        store
            .complete_task(&project_id, &race_task_id, "done".to_owned(), 1_005)
            .unwrap();

        // ABA counterexample: retain a delivery marker for the old row,
        // delete it, then create and run a different task with the same
        // operator-facing id. Its replacement run must not prove the old
        // composed text was delivered.
        let (old_incarnation, old_run_count) = store
            .task_delivery_marker(&session_id, &aba_task_id)
            .unwrap();
        store.delete_task(&project_id, &aba_task_id, 1_006).unwrap();
        store
            .create_task(
                crate::store::NewTask {
                    id: aba_task_id.clone(),
                    project_id: project_id.clone(),
                    parent_task_id: None,
                    title: "Replacement".to_owned(),
                    body: "Different work.".to_owned(),
                    priority: 0,
                },
                1_007,
            )
            .unwrap();
        store
            .assign_task(&project_id, &aba_task_id, Some(&agent_id), 1_008)
            .unwrap();
        store
            .open_run_episode(&session_id, &aba_task_id, 1_009)
            .unwrap();
        store
            .complete_task(
                &project_id,
                &aba_task_id,
                "replacement done".to_owned(),
                1_010,
            )
            .unwrap();
        let event_count = store.events_after(0, 100).unwrap().len();
        let state = DaemonState::new(store);

        let result = commit_delivery(
            &state,
            &project_id,
            &agent_id,
            &session_id,
            Delivery {
                task_id: Some(race_task_id),
                task_incarnation_id: Some(race_incarnation),
                prior_run_count: Some(prior_run_count),
                text: "already delivered".to_owned(),
            },
            1_011,
        )
        .await
        .unwrap();
        assert_eq!(result, None);
        let events = state
            .with_store(move |store| store.events_after(0, 100))
            .await
            .unwrap();
        assert_eq!(events.len(), event_count, "the retry commits no events");

        let aba = commit_delivery(
            &state,
            &project_id,
            &agent_id,
            &session_id,
            Delivery {
                task_id: Some(aba_task_id),
                task_incarnation_id: Some(old_incarnation),
                prior_run_count: Some(old_run_count),
                text: "old task body".to_owned(),
            },
            1_012,
        )
        .await
        .unwrap_err();
        assert!(matches!(
            aba,
            DaemonStateError::Store(StoreError::TaskNotQueued)
        ));

        // Historical episode -> retry -> unrelated cancellation. The run
        // count did not advance after this candidate was composed.
        state
            .commit_and_publish({
                let session_id = session_id.clone();
                let task_id = task_id.clone();
                move |store| {
                    let opened = store.open_run_episode(&session_id, &task_id, 1_007)?;
                    Ok((opened.run, opened.events))
                }
            })
            .await
            .unwrap();
        state
            .commit_and_publish({
                let project_id = project_id.clone();
                let task_id = task_id.clone();
                move |store| {
                    let (task, event) = store.cancel_task(&project_id, &task_id, 1_008)?;
                    Ok((task, vec![event]))
                }
            })
            .await
            .unwrap();
        state
            .commit_and_publish({
                let project_id = project_id.clone();
                let task_id = task_id.clone();
                move |store| {
                    let (task, event) = store.retry_task(&project_id, &task_id, 1_009)?;
                    Ok((task, vec![event]))
                }
            })
            .await
            .unwrap();
        let (task_incarnation, retry_run_count) = state
            .with_store({
                let session_id = session_id.clone();
                let task_id = task_id.clone();
                move |store| store.task_delivery_marker(&session_id, &task_id)
            })
            .await
            .unwrap();
        state
            .commit_and_publish({
                let project_id = project_id.clone();
                let task_id = task_id.clone();
                move |store| {
                    let (task, event) = store.cancel_task(&project_id, &task_id, 1_010)?;
                    Ok((task, vec![event]))
                }
            })
            .await
            .unwrap();
        let task_not_queued = commit_delivery(
            &state,
            &project_id,
            &agent_id,
            &session_id,
            Delivery {
                task_id: Some(task_id.clone()),
                task_incarnation_id: Some(task_incarnation),
                prior_run_count: Some(retry_run_count),
                text: "cancelled retry".to_owned(),
            },
            1_011,
        )
        .await
        .unwrap_err();
        assert!(matches!(
            task_not_queued,
            DaemonStateError::Store(StoreError::TaskNotQueued)
        ));

        // Historical episode -> retry -> a different current run. The old
        // task/session match must not hide the resulting AgentUnavailable.
        state
            .commit_and_publish({
                let project_id = project_id.clone();
                let task_id = task_id.clone();
                move |store| {
                    let (task, event) = store.retry_task(&project_id, &task_id, 1_012)?;
                    Ok((task, vec![event]))
                }
            })
            .await
            .unwrap();
        let (task_incarnation, retry_run_count) = state
            .with_store({
                let session_id = session_id.clone();
                let task_id = task_id.clone();
                move |store| store.task_delivery_marker(&session_id, &task_id)
            })
            .await
            .unwrap();
        state
            .commit_and_publish({
                let session_id = session_id.clone();
                let occupying_task_id = occupying_task_id.clone();
                move |store| {
                    let opened = store.open_run_episode(&session_id, &occupying_task_id, 1_013)?;
                    Ok((opened.run, opened.events))
                }
            })
            .await
            .unwrap();
        let unavailable = commit_delivery(
            &state,
            &project_id,
            &agent_id,
            &session_id,
            Delivery {
                task_id: Some(task_id),
                task_incarnation_id: Some(task_incarnation),
                prior_run_count: Some(retry_run_count),
                text: "blocked by another run".to_owned(),
            },
            1_014,
        )
        .await
        .unwrap_err();
        assert!(matches!(
            unavailable,
            DaemonStateError::Store(StoreError::AgentUnavailable)
        ));
    }

    #[test]
    fn truncate_utf8_stays_on_a_char_boundary() {
        let text = "hello 🏭 world";
        let truncated = truncate_utf8(text, 8);
        assert!(truncated.starts_with("hello"));
    }

    fn task(id: &str, title: &str, body: &str) -> TaskDetail {
        TaskDetail {
            snapshot: factory_core::TaskSnapshot {
                id: TaskId::try_from(id).unwrap(),
                project_id: ProjectId::try_from("factory").unwrap(),
                parent_task_id: None,
                assigned_agent_id: Some(AgentId::try_from("curie").unwrap()),
                title: title.to_owned(),
                status: factory_core::TaskStatus::Running,
                priority: 0,
                created_at_ms: 0,
                updated_at_ms: 0,
            },
            body: body.to_owned(),
            result: None,
            blocked_reason: None,
        }
    }

    #[test]
    fn compose_text_embeds_the_task_colon_id_marker_the_shell_fixture_looks_for() {
        let directory = tempfile::tempdir().unwrap();
        let text = compose_text(
            directory.path(),
            &ProjectId::try_from("factory").unwrap(),
            &AgentId::try_from("curie").unwrap(),
            Some(&task("task-1", "Build the thing", "Do the work.")),
            &[],
            AgentRole::Worker,
        );
        assert!(text.starts_with("Task task-1: Build the thing (task:task-1)"));
        assert!(text.contains("factoryctl task done --task task-1"));
        assert!(text.contains("factoryctl task blocked --task task-1"));
        assert!(text.contains("Do the work."));
        assert!(!text.contains("As the orchestrator"));
    }

    #[test]
    fn compose_text_appends_the_orchestrator_footer_only_for_orchestrators() {
        let directory = tempfile::tempdir().unwrap();
        let text = compose_text(
            directory.path(),
            &ProjectId::try_from("factory").unwrap(),
            &AgentId::try_from("god").unwrap(),
            None,
            &[],
            AgentRole::Orchestrator,
        );
        assert!(text.contains("As the orchestrator"));
        assert!(text.contains("factoryctl agent add"));
        assert!(text.contains("waiting_for_input"));
        assert!(text.contains("do not stop, restart, replace, or duplicate it"));
        assert!(text.contains("factoryctl agent status"));
        assert!(text.contains("dirty worktree"));
    }

    #[test]
    fn compose_text_never_exceeds_the_delivery_bound() {
        let directory = tempfile::tempdir().unwrap();
        let text = compose_text(
            directory.path(),
            &ProjectId::try_from("factory").unwrap(),
            &AgentId::try_from("curie").unwrap(),
            Some(&task("task-1", "Big task", &"x".repeat(200_000))),
            &[],
            AgentRole::Worker,
        );
        assert!(text.len() <= MAX_DELIVERY_TEXT_BYTES + "\n...[truncated]".len());
        assert!(text.starts_with("Task task-1"));
    }
}
