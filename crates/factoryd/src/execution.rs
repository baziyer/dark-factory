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
    fs, io,
    os::unix::{
        fs::{DirBuilderExt, FileTypeExt, MetadataExt},
        process::ExitStatusExt,
    },
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use factory_core::{
    AgentId, AgentRole, EventEnvelope, FactoryEvent, ProjectId, Provider, ProviderHookEvent, RunId,
    RunnerInstanceId, SessionId, SessionSnapshot, SessionState, TaskDetail,
    runner::{RunnerEvent, TerminalSize, encode_terminal_bytes},
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
    store::{RecoverableSession, StoreError},
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
const ORCHESTRATOR_FOOTER: &str = "As the orchestrator, coordinate the project via `factoryctl` \
(DARK_FACTORY_PROJECT/DARK_FACTORY_AGENT/DARK_FACTORY_SOCKET are already set in this session, so \
--project/--agent are usually optional): `factoryctl task add --title T --body B`, `factoryctl \
agent add --role worker --provider <claude|codex|shell>`, `factoryctl task assign --task <id> \
--agent <agent>`, `factoryctl agent message --to <agent> --body \"...\"`, `factoryctl session \
list`, `factoryctl session stop --session <id>`.";

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
    pub max_active_runs: usize,
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
}

impl Handle {
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
    let dispatcher_config = Arc::clone(&config);
    let dispatcher_state = state.clone();
    let dispatcher_wake_tx = wake_tx.clone();
    let join = runtime.spawn(run_dispatcher(
        dispatcher_config,
        dispatcher_state,
        dispatcher_wake_tx,
        wake_rx,
        shutdown_rx,
    ));
    Ok((
        Handle {
            state,
            config,
            wake_tx,
            shutdown: shutdown_tx,
        },
        join,
    ))
}

async fn run_dispatcher(
    config: Arc<Config>,
    state: DaemonState,
    wake_tx: mpsc::Sender<WakeAgent>,
    mut wake_rx: mpsc::Receiver<WakeAgent>,
    mut shutdown_rx: watch::Receiver<bool>,
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
                    dispatch_agent(&config, &state, &wake_tx, &project_id, &agent_id).await
                {
                    tracing::warn!(%error, %project_id, %agent_id, "dispatch failed");
                }
            }
            _ = tick.tick() => {
                if let Err(error) = reconcile_all(&config, &state, &wake_tx).await {
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
                        dispatch_agent(config, state, wake_tx, &project_id, &agent.id).await
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
    project_id: &ProjectId,
    agent_id: &AgentId,
) -> Result<(), Error> {
    let lookup_project_id = project_id.clone();
    let lookup_agent_id = agent_id.clone();
    let agent = match state
        .with_store(move |store| store.get_agent_detail(&lookup_project_id, &lookup_agent_id))
        .await
    {
        Ok(agent) => agent,
        Err(DaemonStateError::Store(StoreError::AgentNotFound)) => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    if agent.snapshot.paused {
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
                if let Err(error) =
                    spawn_session_for_agent(config, state, wake_tx, project_id, agent_id).await
                {
                    tracing::warn!(%error, %project_id, %agent_id, "session spawn failed");
                }
            }
            Ok(())
        }
        Some(session) if session.state == SessionState::Idle => {
            deliver_pending(config, state, project_id, agent_id, &session.snapshot()).await
        }
        Some(_) => Ok(()),
    }
}

async fn has_pending_work(
    state: &DaemonState,
    project_id: &ProjectId,
    agent_id: &AgentId,
) -> Result<bool, Error> {
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
    let child = runner_process::spawn_runner(launch_spec, STARTUP_GRACE).await?;

    let created_at_ms = now_ms()?;
    let new_session = crate::store::NewSession {
        id: session_id.clone(),
        project_id: project_id.clone(),
        agent_id: agent_id.clone(),
        provider: agent.snapshot.provider,
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

    tokio::spawn(supervise_child(
        state.clone(),
        wake_tx.clone(),
        session_id.clone(),
        runtime_dir,
        session_run_id(&session_id)?,
        runner_instance_id,
        child,
    ));
    Ok(snapshot)
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
async fn supervise_child(
    state: DaemonState,
    wake_tx: mpsc::Sender<WakeAgent>,
    session_id: SessionId,
    runtime_dir: PathBuf,
    run_id: RunId,
    runner_instance_id: RunnerInstanceId,
    mut child: tokio::process::Child,
) {
    let event_exit = wait_for_runner_exit(&runtime_dir, run_id, runner_instance_id).await;
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

/// Subscribes to a freshly spawned session's own runner (retrying for up to
/// [`CONNECT_GRACE`] -- `runner_process::spawn_runner` does not itself wait
/// for the control socket to exist in terminal mode, so this can genuinely
/// race the runner's own startup), then waits for and acknowledges its
/// `RunnerEvent::Exited`, returning its `(exit_code, exit_signal)`. Returns
/// `None` if the control socket was never reachable or the connection was
/// lost before an exit event arrived -- best-effort, since the caller still
/// has its own `Child::wait()` to fall back on rather than hang the
/// dispatcher on one wedged session forever.
async fn wait_for_runner_exit(
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
    text: String,
}

async fn compose_delivery(
    state: &DaemonState,
    guidance_root: &Path,
    project_id: &ProjectId,
    agent_id: &AgentId,
) -> Result<Option<Delivery>, DaemonStateError> {
    let guidance_root = guidance_root.to_path_buf();
    let project_id = project_id.clone();
    let agent_id = agent_id.clone();
    state
        .with_store(move |store| {
            let task_id = store.next_deliverable(&project_id, &agent_id)?;
            let messages = store.undelivered_messages_for_agent(&project_id, &agent_id)?;
            if task_id.is_none() && messages.is_empty() {
                return Ok(None);
            }
            let task = task_id
                .as_ref()
                .map(|id| store.get_task(&project_id, id))
                .transpose()?;
            let agent = store.get_agent_detail(&project_id, &agent_id)?;
            let text = compose_text(
                &guidance_root,
                &project_id,
                &agent_id,
                task.as_ref(),
                &messages,
                agent.snapshot.role,
            );
            Ok(Some(Delivery { task_id, text }))
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
        .commit_and_publish(move |store| match delivery.task_id {
            Some(task_id) => {
                let opened = store.open_run_episode(&session_id, &task_id, now_ms)?;
                let run_id = opened.run.id.clone();
                Ok((Some(run_id), opened.events))
            }
            None => {
                store.deliver_agent_messages(&project_id, &agent_id, &session_id, now_ms)?;
                Ok((None, Vec::new()))
            }
        })
        .await
}

/// The passive per-agent auto-delivery path: compose, type, wait for
/// acknowledgement, and only commit the episode/messages once acknowledged
/// -- unlike [`Handle::start_task`], nothing is committed on failure, so a
/// retried wake starts clean.
async fn deliver_pending(
    config: &Config,
    state: &DaemonState,
    project_id: &ProjectId,
    agent_id: &AgentId,
    session: &SessionSnapshot,
) -> Result<(), Error> {
    let Some(delivery) =
        compose_delivery(state, &config.guidance_root, project_id, agent_id).await?
    else {
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
    let Some(delivery) =
        compose_delivery(state, guidance_root, &session.project_id, &session.agent_id).await?
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
        tokio::spawn(supervise_recovered(state, wake_tx, recovered, shutdown_rx));
    }
}

async fn supervise_recovered(
    state: DaemonState,
    wake_tx: mpsc::Sender<WakeAgent>,
    recovered: RecoverableSession,
    mut shutdown_rx: watch::Receiver<bool>,
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
    loop {
        if shutdown_requested(&shutdown_rx) {
            return;
        }
        let attach = match attach_with_grace(&client, &runtime_dir, CONNECT_GRACE, &mut shutdown_rx)
            .await
        {
            Ok(attach) => attach,
            Err(error) => {
                tracing::warn!(%error, session_id = %recovered.session_id, "recovery attach failed");
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
                tokio::select! {
                    _ = wait_for_shutdown(&mut shutdown_rx) => return,
                    () = sleep_until(Instant::now() + retry_delay) => {}
                }
                retry_delay = next_retry_delay(retry_delay);
                continue;
            }
        };
        match consume_until_exit(&client, subscription, &mut shutdown_rx).await {
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

async fn consume_until_exit(
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
