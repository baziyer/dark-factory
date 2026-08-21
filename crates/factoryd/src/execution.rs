//! Durable one-process-per-attempt execution.
//!
//! SQLite is the authority. This module only turns admitted runs into exact
//! runner processes and drives their durable finalization; it owns no shadow
//! session or delivery state.

use std::{
    collections::{HashMap, HashSet, VecDeque},
    ffi::OsString,
    fs, io,
    os::unix::{fs::DirBuilderExt, fs::MetadataExt},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};

use factory_core::{
    AgentId, AgentRole, ChangeId, ChangePhase, ChangeSnapshot, ProjectId, Provider,
    RunFailureReason, RunId, RunPhase, RunnerInstanceId, runner::RunnerEvent,
};
#[cfg(not(target_os = "linux"))]
use rustix::process::test_kill_process;
use rustix::process::{Pid, Signal, kill_process_group, test_kill_process_group};
use sha2::{Digest, Sha256};
use thiserror::Error;
use tokio::{
    process::Child,
    sync::{mpsc, watch},
    task::{JoinHandle, JoinSet},
    time::{Instant, sleep, sleep_until, timeout},
};
use uuid::Uuid;

use crate::{
    change_source::{self, DirectoryIdentity, MaterializerInvocation, SourceLimits},
    daemon_state::{DaemonState, DaemonStateError},
    providers::{self, SpawnContext, hooks},
    runner_client::{
        PreparedRunner, RunnerClient, RunnerClientError, RunnerStreamItem, RunnerSubscription,
    },
    runner_process::{self, LaunchSpec, ProviderEnvironment},
    rust_verify,
    store::{
        AdmittedRun, Change, ChangeBaseIdentity, ChangeMaterialization, ChangeRemovalKind,
        ChangeReservation, ChangeSourceIdentity, KernelResource, KernelResourceKind,
        KernelResourceState, MAX_RUST_CACHE_BYTES, MAX_RUST_CACHE_COUNT, NewRunAdmission,
        PreparedProcessIdentity, RecoverableKernelRun, RustBuildCache, RustCacheLifecycle,
        RustCompletionCheck, RustCompletionPhase, StoreError,
    },
};

const PRIVATE_DIRECTORY_MODE: u32 = 0o700;
const COMMAND_CAPACITY: usize = 256;
const RECONCILE_INTERVAL: Duration = Duration::from_secs(5);
const CONNECT_GRACE: Duration = Duration::from_secs(5);
const CONNECT_RETRY: Duration = Duration::from_millis(50);
const RUNNER_EXIT_GRACE: Duration = Duration::from_secs(5);
const RUST_VERIFICATION_TIMEOUT: Duration = Duration::from_secs(30 * 60);
const DEFAULT_FINALIZE_GRACE_MS: u64 = 5_000;
const DELETE_DRAIN_TIMEOUT: Duration = Duration::from_secs(5);
const DELETE_DRAIN_POLL: Duration = Duration::from_millis(50);
const STATE_PAGE: usize = 100;
pub const MAX_RETAINED_CHANGES_FACTORY_WIDE: usize = 64;
const CHANGE_SCAN_MAX_ENTRIES: u64 = 2_000_000;
const CHANGE_SCAN_MAX_BYTES: u64 = 1_099_511_627_776;

pub struct Config {
    pub factoryd_program: PathBuf,
    pub runner_program: PathBuf,
    pub factoryctl_path: PathBuf,
    pub git_program: PathBuf,
    pub cargo_program: Option<PathBuf>,
    pub runtime_root: PathBuf,
    pub changes_root: PathBuf,
    pub artifacts_root: PathBuf,
    pub guidance_root: PathBuf,
    pub socket_path: PathBuf,
    pub max_active_runs: usize,
}

struct Admission {
    pub project_id: ProjectId,
    pub agent_id: AgentId,
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
    #[error("agent or project is being deleted")]
    DeleteInProgress,
    #[error("timed out draining writes before deletion")]
    DeleteDrainTimeout,
    #[error("system clock is before the Unix epoch")]
    InvalidClock,
    #[error("generated id is invalid")]
    InvalidId,
    #[error("provider launch failed: {0}")]
    Provider(#[from] providers::ProviderError),
    #[error("runner launch failed: {0}")]
    Spawn(#[from] runner_process::Error),
    #[error("runner control failed: {0}")]
    Runner(#[from] RunnerClientError),
    #[error("runtime I/O failed at {path}: {source}")]
    Runtime { path: PathBuf, source: io::Error },
    #[error("process identity was unavailable for pid {0}")]
    ProcessIdentityUnavailable(u32),
    #[error("change source operation failed: {0}")]
    ChangeSource(#[from] change_source::Error),
    #[error("Rust completion verification failed: {0}")]
    RustVerify(#[from] rust_verify::RustVerifyError),
    #[error("change filesystem task failed: {0}")]
    ChangeTask(#[from] tokio::task::JoinError),
}

enum Command {
    WakeAgent {
        project_id: ProjectId,
        agent_id: AgentId,
    },
    ReconcileRun {
        run_id: RunId,
        grace_ms: u64,
    },
    ReconcileChange {
        project_id: ProjectId,
        change_id: ChangeId,
    },
    ObserverFinished(RunId),
}

#[derive(Clone)]
pub struct Handle {
    state: DaemonState,
    config: Arc<Config>,
    commands: mpsc::Sender<Command>,
    shutdown: watch::Sender<bool>,
    agent_gate: Arc<DeleteGate<AgentId>>,
    project_gate: Arc<DeleteGate<ProjectId>>,
}

impl Handle {
    #[must_use]
    pub fn runner_program(&self) -> &Path {
        &self.config.runner_program
    }

    #[must_use]
    pub fn factoryctl_path(&self) -> &Path {
        &self.config.factoryctl_path
    }

    #[must_use]
    pub fn max_active_runs(&self) -> usize {
        self.config.max_active_runs
    }

    #[must_use]
    pub const fn max_retained_changes_factory_wide(&self) -> usize {
        MAX_RETAINED_CHANGES_FACTORY_WIDE
    }

    #[must_use]
    pub fn rust_verification_available(&self) -> bool {
        self.config.cargo_program.is_some()
    }

    pub fn wake(&self, project_id: ProjectId, agent_id: AgentId) {
        let _ = self.commands.try_send(Command::WakeAgent {
            project_id,
            agent_id,
        });
    }

    pub fn wake_run(&self, run_id: RunId) {
        let _ = self.commands.try_send(Command::ReconcileRun {
            run_id,
            grace_ms: DEFAULT_FINALIZE_GRACE_MS,
        });
    }

    pub async fn cancel_run(
        &self,
        project_id: ProjectId,
        run_id: RunId,
        grace_ms: u64,
    ) -> Result<(), Error> {
        let lookup_run_id = run_id.clone();
        let run = self
            .state
            .with_store(move |store| store.kernel_run(&lookup_run_id))
            .await?
            .ok_or(DaemonStateError::Store(StoreError::RunNotFound))?;
        if run.project_id != project_id {
            return Err(DaemonStateError::Store(StoreError::RunNotFound).into());
        }
        let cancel_run_id = run_id.clone();
        let at_ms = now_ms()?;
        self.state
            .commit_and_publish(move |store| {
                let (_, events) = store.cancel_admitted_or_running_run(
                    &cancel_run_id,
                    "operator cancellation".into(),
                    at_ms,
                )?;
                Ok(((), events))
            })
            .await?;
        self.commands
            .send(Command::ReconcileRun { run_id, grace_ms })
            .await
            .map_err(|_| Error::ManagerStopped)
    }

    pub async fn remove_change(
        &self,
        project_id: ProjectId,
        change_id: ChangeId,
        expected_revision: i64,
    ) -> Result<ChangeSnapshot, Error> {
        let lookup_project = project_id.clone();
        let lookup_change = change_id.clone();
        let change = self
            .state
            .with_store(move |store| {
                store
                    .change(&lookup_project, &lookup_change)?
                    .ok_or(StoreError::ChangeNotFound)
            })
            .await?;
        verify_managed_change_path(&self.config.changes_root, &change)?;
        let begin_project = project_id.clone();
        let begin_change = change_id.clone();
        let at_ms = now_ms()?;
        let removing = self
            .state
            .commit_and_publish(move |store| {
                let mutation = store.begin_change_removal(
                    &begin_project,
                    &begin_change,
                    expected_revision,
                    at_ms,
                )?;
                let (change, events) = mutation.into_parts();
                Ok((change.snapshot(), events))
            })
            .await?;
        self.commands
            .send(Command::ReconcileChange {
                project_id,
                change_id,
            })
            .await
            .map_err(|_| Error::ManagerStopped)?;
        Ok(removing)
    }

    pub async fn begin_delete(&self, agent_id: &AgentId) -> Result<(), Error> {
        self.agent_gate.begin_delete(agent_id);
        if self
            .agent_gate
            .wait_for_drain(agent_id, DELETE_DRAIN_TIMEOUT)
            .await
        {
            Ok(())
        } else {
            self.agent_gate.end_delete(agent_id);
            Err(Error::DeleteDrainTimeout)
        }
    }

    pub fn end_delete(&self, agent_id: &AgentId) {
        self.agent_gate.end_delete(agent_id);
    }

    #[must_use]
    pub fn try_begin_agent_write(&self, agent_id: &AgentId) -> bool {
        self.agent_gate.try_begin_write(agent_id)
    }

    pub fn end_agent_write(&self, agent_id: &AgentId) {
        self.agent_gate.end_write(agent_id);
    }

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

    pub fn end_delete_project(&self, project_id: &ProjectId) {
        self.project_gate.end_delete(project_id);
    }

    #[must_use]
    pub fn try_begin_project_write(&self, project_id: &ProjectId) -> bool {
        self.project_gate.try_begin_write(project_id)
    }

    pub fn end_project_write(&self, project_id: &ProjectId) {
        self.project_gate.end_write(project_id);
    }

    pub async fn shutdown(&self) -> Result<(), Error> {
        let _ = self.shutdown.send(true);
        Ok(())
    }
}

pub fn spawn(
    config: Config,
    state: DaemonState,
) -> Result<(Handle, JoinHandle<Result<(), Error>>), Error> {
    if config.max_active_runs == 0 {
        return Err(Error::InvalidConcurrency);
    }
    prepare_runtime_root(&config.runtime_root)?;
    prepare_runtime_root(&config.changes_root)?;
    prepare_runtime_root(&config.changes_root.join(".checkpoints"))?;
    prepare_runtime_root(&config.artifacts_root)?;
    prepare_runtime_root(&config.artifacts_root.join("cache"))?;
    prepare_runtime_root(&config.artifacts_root.join("tmp"))?;
    let runtime = tokio::runtime::Handle::try_current().map_err(|_| Error::ManagerStopped)?;
    let config = Arc::new(config);
    let (commands, receiver) = mpsc::channel(COMMAND_CAPACITY);
    let (shutdown, shutdown_rx) = watch::channel(false);
    let agent_gate = Arc::new(DeleteGate::new());
    let project_gate = Arc::new(DeleteGate::new());
    let join = runtime.spawn(run_manager(
        Arc::clone(&config),
        state.clone(),
        commands.clone(),
        receiver,
        shutdown_rx,
        Arc::clone(&agent_gate),
    ));
    Ok((
        Handle {
            state,
            config,
            commands,
            shutdown,
            agent_gate,
            project_gate,
        },
        join,
    ))
}

async fn run_manager(
    config: Arc<Config>,
    state: DaemonState,
    commands: mpsc::Sender<Command>,
    mut receiver: mpsc::Receiver<Command>,
    mut shutdown: watch::Receiver<bool>,
    agent_gate: Arc<DeleteGate<AgentId>>,
) -> Result<(), Error> {
    let mut observed = HashSet::new();
    let mut change_finalizer = ChangeFinalizer::new();
    let mut rust_maintenance = RustMaintenance::new();
    reconcile_runs(&state, &commands, &mut observed).await?;
    schedule_recoverable_changes(&state, &mut change_finalizer).await;
    change_finalizer.start_next(Arc::clone(&config), state.clone());
    schedule_recoverable_rust_checks(&state, &mut rust_maintenance).await;
    rust_maintenance.schedule_storage();
    rust_maintenance.start_next(Arc::clone(&config), state.clone());
    let mut tick = tokio::time::interval(RECONCILE_INTERVAL);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() {
                    return Ok(());
                }
            }
            command = receiver.recv() => {
                let Some(command) = command else { return Ok(()); };
                match command {
                    Command::WakeAgent { project_id, agent_id } => {
                        if let Err(error) = dispatch_agent(
                            Arc::clone(&config), state.clone(), commands.clone(),
                            &mut observed, Arc::clone(&agent_gate), project_id, agent_id,
                        ).await {
                            tracing::warn!(%error, "attempt dispatch failed");
                        }
                    }
                    Command::ReconcileRun { run_id, grace_ms } => {
                        if let Err(error) = reconcile_one(
                            &state, &commands, &mut observed, run_id, grace_ms,
                        ).await {
                            tracing::warn!(%error, "attempt reconciliation paused");
                        }
                    }
                    Command::ReconcileChange { project_id, change_id } => {
                        change_finalizer.schedule(project_id, change_id);
                        change_finalizer.start_next(Arc::clone(&config), state.clone());
                    }
                    Command::ObserverFinished(run_id) => {
                        observed.remove(&run_id);
                    }
                }
            }
            completed = change_finalizer.join_next(), if change_finalizer.is_active() => {
                change_finalizer.finish(completed);
                change_finalizer.start_next(Arc::clone(&config), state.clone());
            }
            completed = rust_maintenance.join_next(), if rust_maintenance.is_active() => {
                rust_maintenance.finish(completed);
                rust_maintenance.start_next(Arc::clone(&config), state.clone());
            }
            _ = tick.tick() => {
                reconcile_runs(&state, &commands, &mut observed).await?;
                schedule_recoverable_changes(&state, &mut change_finalizer).await;
                change_finalizer.start_next(Arc::clone(&config), state.clone());
                schedule_recoverable_rust_checks(&state, &mut rust_maintenance).await;
                rust_maintenance.schedule_storage();
                rust_maintenance.start_next(Arc::clone(&config), state.clone());
                if let Err(error) = reconcile_agents(
                    Arc::clone(&config), state.clone(), commands.clone(),
                    &mut observed, Arc::clone(&agent_gate),
                ).await {
                    tracing::warn!(%error, "automatic dispatch reconciliation paused");
                }
            }
        }
    }
}

/// Runs one verifier or cache-maintenance step at a time without blocking
/// attempt dispatch. SQLite remains the restart queue and writer authority.
struct RustMaintenance {
    pending: VecDeque<RunId>,
    scheduled: HashSet<RunId>,
    storage_pending: bool,
    active: Option<RustMaintenanceWork>,
    tasks: JoinSet<Result<(), Error>>,
}

enum RustMaintenanceWork {
    Completion(RunId),
    Storage,
}

impl RustMaintenance {
    fn new() -> Self {
        Self {
            pending: VecDeque::new(),
            scheduled: HashSet::new(),
            storage_pending: false,
            active: None,
            tasks: JoinSet::new(),
        }
    }

    fn schedule(&mut self, run_id: RunId) {
        if self.scheduled.insert(run_id.clone()) {
            self.pending.push_back(run_id);
        }
    }

    fn schedule_storage(&mut self) {
        self.storage_pending = true;
    }

    fn start_next(&mut self, config: Arc<Config>, state: DaemonState) {
        if self.active.is_some() {
            return;
        }
        let Some(work) = self.take_next() else {
            return;
        };
        match &work {
            RustMaintenanceWork::Completion(run_id) => {
                let run_id = run_id.clone();
                self.tasks
                    .spawn(async move { reconcile_rust_completion(&config, &state, run_id).await });
            }
            RustMaintenanceWork::Storage => {
                self.tasks
                    .spawn(async move { reconcile_rust_storage(&config, &state).await });
            }
        }
        self.active = Some(work);
    }

    fn take_next(&mut self) -> Option<RustMaintenanceWork> {
        if self.storage_pending {
            self.storage_pending = false;
            Some(RustMaintenanceWork::Storage)
        } else {
            self.pending
                .pop_front()
                .map(RustMaintenanceWork::Completion)
        }
    }

    const fn is_active(&self) -> bool {
        self.active.is_some()
    }

    async fn join_next(&mut self) -> Option<Result<Result<(), Error>, tokio::task::JoinError>> {
        self.tasks.join_next().await
    }

    fn finish(&mut self, completed: Option<Result<Result<(), Error>, tokio::task::JoinError>>) {
        let Some(work) = self.active.take() else {
            return;
        };
        match work {
            RustMaintenanceWork::Completion(run_id) => {
                self.scheduled.remove(&run_id);
                match completed {
                    Some(Ok(Ok(()))) => {}
                    Some(Ok(Err(error))) => {
                        tracing::warn!(%run_id, %error, "Rust completion verification paused");
                    }
                    Some(Err(error)) => {
                        tracing::warn!(%run_id, %error, "Rust completion verifier task failed");
                    }
                    None => tracing::warn!(%run_id, "Rust completion verifier task disappeared"),
                }
            }
            RustMaintenanceWork::Storage => match completed {
                Some(Ok(Ok(()))) => {}
                Some(Ok(Err(error))) => tracing::warn!(%error, "Rust cache maintenance paused"),
                Some(Err(error)) => tracing::warn!(%error, "Rust cache maintenance task failed"),
                None => tracing::warn!("Rust cache maintenance task disappeared"),
            },
        }
    }
}

type ChangeKey = (ProjectId, ChangeId);

/// Runs at most one filesystem finalizer without blocking the manager loop.
/// Durable Change state remains the retry queue across ticks and restarts.
struct ChangeFinalizer {
    pending: VecDeque<ChangeKey>,
    scheduled: HashSet<ChangeKey>,
    active: Option<ChangeKey>,
    tasks: JoinSet<Result<(), Error>>,
}

impl ChangeFinalizer {
    fn new() -> Self {
        Self {
            pending: VecDeque::new(),
            scheduled: HashSet::new(),
            active: None,
            tasks: JoinSet::new(),
        }
    }

    fn schedule(&mut self, project_id: ProjectId, change_id: ChangeId) {
        let key = (project_id, change_id);
        if self.scheduled.contains(&key) {
            return;
        }
        if self.scheduled.len() >= MAX_RETAINED_CHANGES_FACTORY_WIDE {
            tracing::warn!("Change finalization queue is at its durable Change bound");
            return;
        }
        self.scheduled.insert(key.clone());
        self.pending.push_back(key);
    }

    fn start_next(&mut self, config: Arc<Config>, state: DaemonState) {
        if self.active.is_some() {
            return;
        }
        let Some((project_id, change_id)) = self.pending.pop_front() else {
            return;
        };
        self.active = Some((project_id.clone(), change_id.clone()));
        self.tasks
            .spawn(async move { reconcile_change(&config, &state, project_id, change_id).await });
    }

    const fn is_active(&self) -> bool {
        self.active.is_some()
    }

    async fn join_next(&mut self) -> Option<Result<Result<(), Error>, tokio::task::JoinError>> {
        self.tasks.join_next().await
    }

    fn finish(&mut self, completed: Option<Result<Result<(), Error>, tokio::task::JoinError>>) {
        let Some((project_id, change_id)) = self.active.take() else {
            return;
        };
        self.scheduled
            .remove(&(project_id.clone(), change_id.clone()));
        match completed {
            Some(Ok(Ok(()))) => {}
            Some(Ok(Err(error))) => {
                tracing::warn!(%project_id, %change_id, %error, "Change finalization paused");
            }
            Some(Err(error)) => {
                tracing::warn!(%project_id, %change_id, %error, "Change finalizer task failed");
            }
            None => {
                tracing::warn!(%project_id, %change_id, "Change finalizer task disappeared");
            }
        }
    }
}

async fn start_and_observe(
    config: Arc<Config>,
    state: DaemonState,
    commands: mpsc::Sender<Command>,
    observed: &mut HashSet<RunId>,
    agent_gate: Arc<DeleteGate<AgentId>>,
    input: Admission,
) -> Result<(), Error> {
    let agent_id = input.agent_id.clone();
    if !agent_gate.try_begin_write(&agent_id) {
        return Err(Error::DeleteInProgress);
    }
    let result = start_run(&config, &state, input).await;
    agent_gate.end_write(&agent_id);
    if let Some(StartedProcess { run, child }) = result? {
        observed.insert(run.run.id.clone());
        spawn_observer(state, commands, run, Some(child));
    }
    Ok(())
}

struct StartedProcess {
    run: RecoverableKernelRun,
    child: Child,
}

async fn start_run(
    config: &Config,
    state: &DaemonState,
    input: Admission,
) -> Result<Option<StartedProcess>, Error> {
    let run_id = new_run_id()?;
    let runner_instance_id = new_runner_instance_id()?;
    let runtime_nonce = Uuid::new_v4().simple().to_string();
    let runtime_claim = format!("runtime-claim:{runtime_nonce}");
    let runtime_dir = config.runtime_root.join(&runtime_nonce);
    let policy_dir = runtime_dir.join("policy");
    let bearer = random_bearer();
    let digest = capability_digest(&bearer);
    let change_id = new_change_id()?;
    let change_reservation = ChangeReservation {
        source_root: config
            .changes_root
            .join(parse_change_uuid(&change_id)?.simple().to_string())
            .to_string_lossy()
            .into_owned(),
        id: change_id,
        max_factory_changes: MAX_RETAINED_CHANGES_FACTORY_WIDE,
    };
    let admission = NewRunAdmission {
        run_id: run_id.clone(),
        project_id: input.project_id,
        agent_id: input.agent_id,
        capability_digest: digest,
        runtime_claim,
        runner_instance_id: runner_instance_id.clone(),
        runner_runtime: runtime_dir.to_string_lossy().into_owned(),
        max_active_runs: config.max_active_runs,
        change_reservation,
        policy_cwd: policy_dir.to_string_lossy().into_owned(),
    };
    let admitted_at_ms = now_ms()?;
    let admitted = state
        .commit_and_publish(move |store| {
            let admitted = store.admit_next_run(admission, admitted_at_ms)?;
            let events = admitted
                .as_ref()
                .map_or_else(Vec::new, |admitted| admitted.events.clone());
            Ok((admitted, events))
        })
        .await?;
    let Some(admitted) = admitted else {
        return Ok(None);
    };

    match launch_admitted(config, state, admitted, bearer).await {
        Ok(started) => Ok(Some(started)),
        Err((error, child, run)) => {
            cleanup_unactivated(state, &run, child).await;
            Err(error)
        }
    }
}

async fn launch_admitted(
    config: &Config,
    state: &DaemonState,
    admitted: AdmittedRun,
    bearer: String,
) -> Result<StartedProcess, (Error, Option<Child>, RecoverableKernelRun)> {
    let recovery = recovery_from_admission(&admitted);
    let runtime_dir = PathBuf::from(&admitted.target.runner_runtime);
    if let Err(error) = ensure_private_directory(&runtime_dir) {
        return Err((error, None, recovery));
    }
    let runtime_locator = runtime_locator(&runtime_dir);
    let runtime_birth = match runtime_birth_fingerprint(&runtime_dir) {
        Ok(Some(fingerprint)) => fingerprint,
        Ok(None) => return Err((Error::InvalidRuntimeRoot, None, recovery)),
        Err(error) => return Err((error, None, recovery)),
    };
    let register_runtime_run_id = admitted.run.id.clone();
    let registered_runtime_claim = admitted.target.runtime_claim.clone();
    let runtime_registered_at_ms = match now_ms() {
        Ok(value) => value,
        Err(error) => return Err((error, None, recovery)),
    };
    if let Err(error) = state
        .commit_and_publish(move |store| {
            store.register_admitted_runtime(
                &register_runtime_run_id,
                &runtime_locator,
                &registered_runtime_claim,
                &runtime_birth,
                runtime_registered_at_ms,
            )?;
            Ok(((), Vec::new()))
        })
        .await
    {
        return Err((error.into(), None, recovery));
    }
    let provisioning = admitted.target.change_phase == Some(ChangePhase::Provisioning);
    let policy_dir = runtime_dir.join("policy");
    if admitted.target.role == AgentRole::Orchestrator || provisioning {
        match ensure_private_directory(&policy_dir) {
            Ok(()) => {}
            Err(error) => return Err((error, None, recovery)),
        }
    }
    let hook_token_path = runtime_dir.join("attempt.token");
    if let Err(source) = hooks::write_private_file(&hook_token_path, bearer.as_bytes()) {
        return Err((
            Error::Runtime {
                path: hook_token_path,
                source,
            },
            None,
            recovery,
        ));
    }
    let startup_input = match compose_startup(config, &admitted) {
        Ok(input) => input,
        Err(error) => return Err((error, None, recovery)),
    };
    let provider = select_provider(admitted.target.provider);
    let context = SpawnContext {
        run_id: admitted.run.id.clone(),
        source_root: PathBuf::from(&admitted.target.source_root),
        startup_input,
        model: admitted.target.model.clone(),
        reasoning_effort: admitted.target.reasoning_effort.clone(),
        permission_mode: admitted.target.permission_mode.clone(),
        auto_mode: admitted.target.auto_mode,
        hook_token_path,
        factoryctl_path: config.factoryctl_path.clone(),
        agent_dir: runtime_dir.join("provider"),
    };
    let mut launch = match provider.spawn_spec(&context) {
        Ok(launch) => launch,
        Err(error) => return Err((error.into(), None, recovery)),
    };
    if provisioning {
        let change_id = match admitted.target.change_id.as_ref() {
            Some(change_id) => change_id,
            None => return Err((Error::InvalidId, None, recovery)),
        };
        let change_uuid = match parse_change_uuid(change_id) {
            Ok(value) => value,
            Err(error) => return Err((error, None, recovery)),
        };
        let lookup_project = admitted.target.project_id.clone();
        let repository_root = match state
            .with_store(move |store| Ok(store.get_project(&lookup_project)?.root))
            .await
        {
            Ok(root) => PathBuf::from(root),
            Err(error) => return Err((error.into(), None, recovery)),
        };
        let provider_program = match runner_process::resolve_provider_executable(&launch.program) {
            Ok(program) => program,
            Err(error) => return Err((error.into(), None, recovery)),
        };
        let invocation_path = runtime_dir.join("materializer.json");
        let limits = match SourceLimits::new(CHANGE_SCAN_MAX_ENTRIES, CHANGE_SCAN_MAX_BYTES) {
            Ok(limits) => limits,
            Err(error) => return Err((error.into(), None, recovery)),
        };
        let invocation = MaterializerInvocation {
            git_program: config.git_program.clone(),
            repository_root,
            changes_root: config.changes_root.clone(),
            change_id: change_uuid,
            limits,
            selection_record_path: selection_record_path(&config.changes_root, change_uuid),
            selection_activation_path: runtime_dir.join("source.selection.activate"),
            ready_record_path: runtime_dir.join("source.ready.json"),
            provider_activation_path: runtime_dir.join("source.provider.activate"),
            activation_poll_ms: 50,
            provider_program,
            provider_arguments: launch.args,
        };
        if let Err(error) =
            change_source::write_materializer_invocation(&invocation_path, &invocation)
        {
            return Err((error.into(), None, recovery));
        }
        launch.program = config.factoryd_program.clone();
        launch.args = vec![
            "--materialize-change".into(),
            invocation_path.to_string_lossy().into_owned(),
        ];
    }
    let (provider_environment, attempt_environment) = provider_environment(launch.env);
    let spec = LaunchSpec {
        runner_program: config.runner_program.clone(),
        factoryctl_path: config.factoryctl_path.clone(),
        provider_program: launch.program,
        provider_arguments: launch.args.into_iter().map(Into::into).collect(),
        provider_environment,
        attempt_environment: base_environment(config, &admitted, attempt_environment),
        run_id: admitted.run.id.clone(),
        runner_instance_id: admitted.target.runner_instance_id.clone(),
        runtime_dir: runtime_dir.clone(),
        cwd: if provisioning {
            policy_dir
        } else {
            PathBuf::from(&admitted.target.source_root)
        },
        source_root: PathBuf::from(&admitted.target.source_root),
        startup_input: launch.startup_input,
    };
    let prepared_runner = match runner_process::prepare_runner(spec).await {
        Ok(prepared) => prepared,
        Err(error) => return Err((error.into(), None, recovery)),
    };
    let runner_pid = prepared_runner.child_pid();
    let runner_locator = runner_locator(runner_pid, &admitted.target.runner_instance_id);
    let runner_birth = match process_birth_fingerprint(runner_pid) {
        Ok(Some(fingerprint)) => fingerprint,
        Ok(None) => {
            return Err((
                Error::ProcessIdentityUnavailable(runner_pid),
                None,
                recovery,
            ));
        }
        Err(error) => return Err((error, None, recovery)),
    };
    let register_run_id = admitted.run.id.clone();
    let registered_at_ms = match now_ms() {
        Ok(value) => value,
        Err(error) => return Err((error, None, recovery)),
    };
    if let Err(error) = state
        .commit_and_publish(move |store| {
            store.register_admitted_runner(
                &register_run_id,
                &runner_locator,
                &runner_birth,
                registered_at_ms,
            )?;
            Ok(((), Vec::new()))
        })
        .await
    {
        return Err((error.into(), None, recovery));
    }
    let child = match prepared_runner.activate().await {
        Ok(child) => child,
        Err(error) => return Err((error.into(), None, recovery)),
    };
    let client = RunnerClient::new(
        &runtime_dir,
        admitted.run.id.clone(),
        admitted.target.runner_instance_id.clone(),
    );
    let prepared = match prepare_with_grace(&client).await {
        Ok(prepared) => prepared,
        Err(error) => return Err((error.into(), Some(child), recovery)),
    };
    let identity = match prepared_identity(&admitted, &prepared) {
        Ok(identity) => identity,
        Err(error) => return Err((error, Some(child), recovery)),
    };
    let activate_run_id = admitted.run.id.clone();
    let activated_at_ms = match now_ms() {
        Ok(value) => value,
        Err(error) => return Err((error, Some(child), recovery)),
    };
    let (activated_run, resources) = match state
        .commit_and_publish(move |store| {
            let (run, events) =
                store.activate_prepared_run(&activate_run_id, identity, activated_at_ms)?;
            let resources = store.kernel_resources(&activate_run_id)?;
            Ok(((run, resources), events))
        })
        .await
    {
        Ok(run) => run,
        Err(error) => return Err((error.into(), Some(child), recovery)),
    };
    if let Err(error) = prepared.activate().await {
        // Running is already durable. The observer resolves whether the gate
        // accepted activation; returning the admitted run is safer than
        // manufacturing a second attempt after an ambiguous acknowledgement.
        tracing::warn!(run_id = %admitted.run.id, %error, "runner activation acknowledgement was lost");
    }
    Ok(StartedProcess {
        run: RecoverableKernelRun {
            run: activated_run,
            change_id: admitted.target.change_id,
            source_root: admitted.target.source_root,
            runner_instance_id: admitted.target.runner_instance_id,
            runner_runtime: admitted.target.runner_runtime,
            resources,
        },
        child,
    })
}

fn recovery_from_admission(admitted: &AdmittedRun) -> RecoverableKernelRun {
    RecoverableKernelRun {
        run: admitted.run.clone(),
        change_id: admitted.target.change_id.clone(),
        source_root: admitted.target.source_root.clone(),
        runner_instance_id: admitted.target.runner_instance_id.clone(),
        runner_runtime: admitted.target.runner_runtime.clone(),
        resources: Vec::new(),
    }
}

async fn prepare_with_grace(client: &RunnerClient) -> Result<PreparedRunner, RunnerClientError> {
    let deadline = Instant::now() + CONNECT_GRACE;
    loop {
        match client.prepare().await {
            Ok(prepared) => return Ok(prepared),
            Err(RunnerClientError::Io(error))
                if matches!(
                    error.kind(),
                    io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
                ) && Instant::now() < deadline =>
            {
                sleep(CONNECT_RETRY).await;
            }
            Err(error) => return Err(error),
        }
    }
}

fn prepared_identity(
    admitted: &AdmittedRun,
    prepared: &PreparedRunner,
) -> Result<PreparedProcessIdentity, Error> {
    let runner_pid = prepared.runner_pid();
    let provider_pid = prepared.child_pid();
    let process_group = prepared.process_group_id();
    let runtime_dir = Path::new(&admitted.target.runner_runtime);
    let runtime_birth = runtime_birth_fingerprint(runtime_dir)?.ok_or(Error::InvalidRuntimeRoot)?;
    let runner_birth = process_birth_fingerprint(runner_pid)?
        .ok_or(Error::ProcessIdentityUnavailable(runner_pid))?;
    let provider_birth = process_birth_fingerprint(provider_pid)?
        .ok_or(Error::ProcessIdentityUnavailable(provider_pid))?;
    Ok(PreparedProcessIdentity {
        runtime_locator: runtime_locator(runtime_dir),
        runtime_birth_fingerprint: runtime_birth,
        runner_locator: runner_locator(runner_pid, &admitted.target.runner_instance_id),
        runner_birth_fingerprint: runner_birth,
        provider_locator: serde_json::json!({ "pid": provider_pid }).to_string(),
        provider_birth_fingerprint: provider_birth.clone(),
        process_group_locator: serde_json::json!({ "pgid": process_group }).to_string(),
        process_group_birth_fingerprint: provider_birth,
    })
}

fn base_environment(
    config: &Config,
    admitted: &AdmittedRun,
    mut provider_environment: Vec<(String, String)>,
) -> Vec<(String, String)> {
    let mut environment = vec![
        (
            "DARK_FACTORY_AGENT".into(),
            admitted.target.agent_id.to_string(),
        ),
        (
            "DARK_FACTORY_PROJECT".into(),
            admitted.target.project_id.to_string(),
        ),
        (
            "DARK_FACTORY_SOCKET".into(),
            config.socket_path.to_string_lossy().into_owned(),
        ),
        (
            "DARK_FACTORY_ATTEMPT_TOKEN_FILE".into(),
            PathBuf::from(&admitted.target.runner_runtime)
                .join("attempt.token")
                .to_string_lossy()
                .into_owned(),
        ),
    ];
    environment.append(&mut provider_environment);
    environment
}

fn provider_environment(
    environment: Vec<(String, String)>,
) -> (ProviderEnvironment, Vec<(String, String)>) {
    let mut codex_home = None;
    let mut rest = Vec::new();
    for (name, value) in environment {
        if name == "CODEX_HOME" {
            codex_home = Some(PathBuf::from(value));
        } else {
            rest.push((name, value));
        }
    }
    (
        codex_home.map_or(
            ProviderEnvironment::Inherited,
            ProviderEnvironment::CodexHome,
        ),
        rest,
    )
}

fn select_provider(kind: Provider) -> Box<dyn providers::Provider + Send> {
    match kind {
        Provider::ClaudeCode => Box::new(providers::claude::ClaudeProvider),
        Provider::Codex => Box::new(providers::codex::CodexProvider::new()),
        Provider::Shell => Box::new(providers::shell::ShellProvider),
    }
}

fn compose_startup(config: &Config, admitted: &AdmittedRun) -> Result<Vec<u8>, Error> {
    let project_guidance = read_guidance(&factory_core::paths::project_guidance_path(
        &config.guidance_root,
        &admitted.target.project_id,
    ))?;
    let instructions = read_guidance(&factory_core::paths::agent_instructions_path(
        &config.guidance_root,
        &admitted.target.project_id,
        &admitted.target.agent_id,
    ))?;
    let memory = read_guidance(&factory_core::paths::agent_memory_path(
        &config.guidance_root,
        &admitted.target.project_id,
        &admitted.target.agent_id,
    ))?;
    let mut prompt = format!(
        "# Dark Factory attempt {}\n\nProject guidance:\n{}\n\nAgent rules:\n{}\n\nAgent memory:\n{}\n\nTask: {}\n\n{}\n",
        admitted.run.id,
        project_guidance,
        instructions,
        memory,
        admitted.target.task_title,
        admitted.target.task_body,
    );
    if !admitted.target.messages.is_empty() {
        prompt.push_str("\nMessages:\n");
        for message in &admitted.target.messages {
            prompt.push_str("- ");
            prompt.push_str(&message.body);
            prompt.push('\n');
        }
    }
    if admitted.target.role == AgentRole::Orchestrator {
        prompt.push_str(
            "\nYou schedule and prioritize through factoryctl. factoryd owns admission, source, processes, and finalization.\n",
        );
    } else {
        prompt.push_str(
            "\nWhen the project uses Rust workspace verification, `factoryctl task done` owns the final build and tests. Do not run `cargo`, `rustc`, `rustup`, or executables from mutable Cargo target directories yourself.\n",
        );
    }
    prompt.push_str(
        "\nWhen finished run `factoryctl task done --result <summary>`. If genuinely blocked run `factoryctl task blocked --reason <reason>`. Your credential identifies this exact attempt; do not supply task or run IDs.\n",
    );
    Ok(prompt.into_bytes())
}

fn read_guidance(path: &Path) -> Result<String, Error> {
    fs::read_to_string(path).map_err(|source| Error::Runtime {
        path: path.to_path_buf(),
        source,
    })
}

async fn dispatch_agent(
    config: Arc<Config>,
    state: DaemonState,
    commands: mpsc::Sender<Command>,
    observed: &mut HashSet<RunId>,
    agent_gate: Arc<DeleteGate<AgentId>>,
    project_id: ProjectId,
    agent_id: AgentId,
) -> Result<(), Error> {
    let result = start_and_observe(
        config,
        state,
        commands,
        observed,
        agent_gate,
        Admission {
            project_id,
            agent_id,
        },
    )
    .await;
    if let Err(error) = &result
        && is_admission_capacity(error)
    {
        return Ok(());
    }
    result.map(|_| ())
}

fn is_admission_capacity(error: &Error) -> bool {
    matches!(
        error,
        Error::State(DaemonStateError::Store(
            StoreError::CapacityReached { .. } | StoreError::ChangeCapacityReached { .. }
        ))
    )
}

async fn reconcile_agents(
    config: Arc<Config>,
    state: DaemonState,
    commands: mpsc::Sender<Command>,
    observed: &mut HashSet<RunId>,
    agent_gate: Arc<DeleteGate<AgentId>>,
) -> Result<(), Error> {
    let mut project_cursor = None;
    loop {
        let after = project_cursor.clone();
        let mut projects = state
            .with_store(move |store| store.list_projects(after.as_ref(), STATE_PAGE + 1))
            .await?;
        let next = (projects.len() > STATE_PAGE).then(|| projects.swap_remove(STATE_PAGE).id);
        for project in projects {
            let mut agent_cursor = None;
            loop {
                let project_id = project.id.clone();
                let after = agent_cursor.clone();
                let mut agents = state
                    .with_store(move |store| {
                        store.list_agents(&project_id, after.as_ref(), STATE_PAGE + 1)
                    })
                    .await?;
                let next_agent =
                    (agents.len() > STATE_PAGE).then(|| agents.swap_remove(STATE_PAGE).id);
                for agent in agents {
                    if let Err(error) = dispatch_agent(
                        Arc::clone(&config),
                        state.clone(),
                        commands.clone(),
                        observed,
                        Arc::clone(&agent_gate),
                        project.id.clone(),
                        agent.id,
                    )
                    .await
                    {
                        tracing::warn!(%error, "agent dispatch paused");
                    }
                }
                match next_agent {
                    Some(cursor) => agent_cursor = Some(cursor),
                    None => break,
                }
            }
        }
        match next {
            Some(cursor) => project_cursor = Some(cursor),
            None => break,
        }
    }
    Ok(())
}

async fn reconcile_runs(
    state: &DaemonState,
    commands: &mpsc::Sender<Command>,
    observed: &mut HashSet<RunId>,
) -> Result<(), Error> {
    let recoverable = state
        .with_store(|store| store.recoverable_kernel_runs())
        .await?;
    for run in recoverable {
        let run_id = run.run.id.clone();
        let result = async {
            if run.run.phase == RunPhase::Admitted {
                recover_admitted_run(state, commands, observed, run).await?;
                return Ok::<(), Error>(());
            }
            if release_absent_resources(state, &run).await? {
                return Ok(());
            }
            if observed.insert(run.run.id.clone()) {
                spawn_observer(state.clone(), commands.clone(), run, None);
            }
            Ok(())
        }
        .await;
        if let Err(error) = result {
            tracing::warn!(%run_id, %error, "attempt reconciliation paused");
        }
    }
    Ok(())
}

async fn schedule_recoverable_changes(state: &DaemonState, finalizer: &mut ChangeFinalizer) {
    let changes = match state.with_store(|store| store.recoverable_changes()).await {
        Ok(changes) => changes,
        Err(error) => {
            tracing::warn!(%error, "Change reconciliation paused");
            return;
        }
    };
    for change in changes {
        finalizer.schedule(change.project_id, change.id);
    }
}

async fn schedule_recoverable_rust_checks(state: &DaemonState, finalizer: &mut RustMaintenance) {
    let checks = match state
        .with_store(|store| store.recoverable_rust_completion_checks())
        .await
    {
        Ok(checks) => checks,
        Err(error) => {
            tracing::warn!(%error, "could not scan recoverable Rust completion checks");
            return;
        }
    };
    for check in checks {
        finalizer.schedule(check.run_id);
    }
}

async fn reconcile_rust_completion(
    config: &Config,
    state: &DaemonState,
    run_id: RunId,
) -> Result<(), Error> {
    let lookup_run = run_id.clone();
    let (check, run) = state
        .with_store(move |store| {
            let check = store
                .rust_completion_check(&lookup_run)?
                .ok_or(StoreError::RustCompletionCheckNotFound)?;
            let run = store
                .recoverable_kernel_runs()?
                .into_iter()
                .find(|candidate| candidate.run.id == lookup_run)
                .ok_or(StoreError::RunNotFound)?;
            Ok((check, run))
        })
        .await?;
    if run.run.phase != RunPhase::Finalizing {
        return Err(DaemonStateError::Store(StoreError::InvalidRunState).into());
    }
    if !initial_attempt_resources_released(&run.resources) {
        return Ok(());
    }
    match check.phase {
        RustCompletionPhase::Pending => {
            let change = load_available_completion_change(state, &check).await?;
            start_rust_completion(config, state, check, change).await
        }
        RustCompletionPhase::Running => recover_rust_completion(config, state, check).await,
        RustCompletionPhase::Passed | RustCompletionPhase::Failed => Ok(()),
    }
}

async fn load_available_completion_change(
    state: &DaemonState,
    check: &RustCompletionCheck,
) -> Result<Change, Error> {
    let project_id = check.project_id.clone();
    let change_id = check.change_id.clone();
    let change = state
        .with_store(move |store| {
            store
                .change(&project_id, &change_id)?
                .ok_or(StoreError::ChangeNotFound)
        })
        .await?;
    if change.phase != ChangePhase::Available {
        return Err(DaemonStateError::Store(StoreError::InvalidRunState).into());
    }
    Ok(change)
}

fn initial_attempt_resources_released(resources: &[KernelResource]) -> bool {
    resources
        .iter()
        .filter(|resource| {
            matches!(
                resource.kind,
                KernelResourceKind::RunnerProcess
                    | KernelResourceKind::ProviderProcess
                    | KernelResourceKind::ProcessGroup
                    | KernelResourceKind::RuntimeRoot
            )
        })
        .all(|resource| resource.state == KernelResourceState::Released)
}

async fn start_rust_completion(
    config: &Config,
    state: &DaemonState,
    check: RustCompletionCheck,
    change: Change,
) -> Result<(), Error> {
    let Some(cargo) = config.cargo_program.clone() else {
        return fail_rust_completion(
            state,
            &check,
            "no fixed Cargo and rustc toolchain was available at daemon startup",
        )
        .await;
    };
    let incarnation = check.project_incarnation_id.clone();
    let key_cargo = cargo.clone();
    let cache_key =
        tokio::task::spawn_blocking(move || rust_verify::cache_key(&incarnation, &key_cargo))
            .await??;
    let temporary = match prepare_completion_temporary_root(config, state, &check).await {
        Ok(temporary) => temporary,
        Err(error) => {
            return fail_and_release_rust_setup(state, &check, &error.to_string()).await;
        }
    };
    let claim_run = check.run_id.clone();
    let claim_key = cache_key.clone();
    let claimed_at_ms = now_ms()?;
    let check = state
        .commit_and_publish(move |store| {
            let check = store.claim_rust_completion_check(&claim_run, &claim_key, claimed_at_ms)?;
            Ok((check, Vec::new()))
        })
        .await?;
    let cache_key = check.cache_key.as_deref().ok_or(DaemonStateError::Store(
        StoreError::InvalidRustBuildMetadata,
    ))?;
    let cache = match prepare_rust_cache(config, state, &check, cache_key).await {
        Ok(cache) => cache,
        Err(error) => return fail_claimed_rust_setup(config, state, &check, &error).await,
    };
    if let Err(error) =
        prepare_rust_worker_invocation(config, &check, &change, &temporary, &cache).await
    {
        return fail_claimed_rust_setup(config, state, &check, &error).await;
    }

    let worker_result = match run_rust_worker_effect(config, state, &check, &temporary.path).await {
        Ok(RustWorkerEffectOutcome::Released(result)) => result,
        Ok(RustWorkerEffectOutcome::Pending) => return Ok(()),
        Err(error) => {
            return fail_claimed_rust_setup(config, state, &check, &error).await;
        }
    };
    finish_released_rust_effect(config, state, &check, worker_result).await
}

async fn prepare_rust_worker_invocation(
    config: &Config,
    check: &RustCompletionCheck,
    change: &Change,
    temporary: &CompletionTemporaryRoot,
    cache: &RustBuildCache,
) -> Result<(), Error> {
    let cargo = config.cargo_program.clone().ok_or(DaemonStateError::Store(
        StoreError::InvalidRustBuildMetadata,
    ))?;
    let change_identity = rust_verify::ExactDirectoryIdentity {
        device: change.source_dev.ok_or(DaemonStateError::Store(
            StoreError::InvalidRustBuildMetadata,
        ))?,
        inode: change.source_inode.ok_or(DaemonStateError::Store(
            StoreError::InvalidRustBuildMetadata,
        ))?,
    };
    let cache_identity = rust_verify::ExactDirectoryIdentity {
        device: cache.dev.ok_or(DaemonStateError::Store(
            StoreError::InvalidRustBuildMetadata,
        ))?,
        inode: cache.inode.ok_or(DaemonStateError::Store(
            StoreError::InvalidRustBuildMetadata,
        ))?,
    };
    let change_uuid = parse_change_uuid(&change.id)?;
    let invocation = rust_verify::WorkerInvocation {
        cargo,
        changes_root: config.changes_root.clone(),
        change_id: change_uuid,
        project_incarnation_id: check.project_incarnation_id.clone(),
        cache_root: PathBuf::from(&cache.path),
        temporary_root: temporary.path.clone(),
        change_identity,
        cache_identity,
        temporary_identity: temporary.identity,
    };
    let invocation_path = temporary.path.join("worker.json");
    let write_path = invocation_path.clone();
    tokio::task::spawn_blocking(move || {
        rust_verify::write_worker_invocation(&write_path, &invocation)
    })
    .await??;
    Ok(())
}

async fn run_rust_worker_effect(
    config: &Config,
    state: &DaemonState,
    check: &RustCompletionCheck,
    temporary_root: &Path,
) -> Result<RustWorkerEffectOutcome, Error> {
    let nonce = Uuid::new_v4().simple().to_string();
    let invocation_path = temporary_root.join("worker.json");
    let result_path = temporary_root.join("result.json");
    let finish_path = temporary_root.join(format!("finish-{nonce}"));
    let activation_path = temporary_root.join(format!("effect-exec-{nonce}.activate"));
    let expected_parent = rustix::process::getpid().as_raw_nonzero().get().to_string();
    let prepared = runner_process::prepare_effect(runner_process::EffectLaunchSpec {
        gate_program: config.runner_program.clone(),
        program: config.factoryd_program.clone(),
        arguments: vec![
            OsString::from("--rust-verify-worker"),
            invocation_path.into_os_string(),
            result_path.clone().into_os_string(),
            finish_path.clone().into_os_string(),
            OsString::from(expected_parent),
        ],
        environment: Vec::new(),
        cwd: temporary_root.to_owned(),
        activation_path,
    })
    .await?;
    let pid = prepared.child_pid();
    let birth = process_birth_fingerprint(pid)?.ok_or(Error::ProcessIdentityUnavailable(pid))?;
    if let Err(error) =
        register_effect_resources(state, &check.run_id, &nonce, pid, &birth, &finish_path).await
    {
        // The registration operation may have committed before its result
        // was lost. Dropping `prepared` reaps a gate that never executed, but
        // only durable reconciliation may decide whether effect rows exist.
        tracing::warn!(
            run_id = %check.run_id,
            %error,
            "Rust verifier registration is uncertain; deferring to durable reconciliation"
        );
        return Ok(RustWorkerEffectOutcome::Pending);
    }
    let mut child = match prepared.activate().await {
        Ok(child) => child,
        Err(error) => {
            let result = failed_worker_result(&error.to_string());
            return match terminate_effect_group(state, &check.run_id, None, &finish_path).await {
                Ok(()) => Ok(RustWorkerEffectOutcome::Released(result)),
                Err(cleanup_error) => {
                    tracing::warn!(
                        run_id = %check.run_id,
                        %cleanup_error,
                        "registered Rust verifier activation failed before exact resources were released"
                    );
                    Ok(RustWorkerEffectOutcome::Pending)
                }
            };
        }
    };
    let worker_result =
        wait_for_worker_result(&mut child, &result_path, RUST_VERIFICATION_TIMEOUT).await;
    if let Err(error) =
        terminate_effect_group(state, &check.run_id, Some(&mut child), &finish_path).await
    {
        tracing::warn!(
            run_id = %check.run_id,
            %error,
            "registered Rust verifier has not been proven released"
        );
        return Ok(RustWorkerEffectOutcome::Pending);
    }
    Ok(RustWorkerEffectOutcome::Released(match worker_result {
        Ok(result) => result,
        Err(error) => failed_worker_result(&error.to_string()),
    }))
}

enum RustWorkerEffectOutcome {
    Released(rust_verify::WorkerResult),
    Pending,
}

struct CompletionTemporaryRoot {
    path: PathBuf,
    identity: rust_verify::ExactDirectoryIdentity,
}

struct CompletionTemporarySpec {
    path: PathBuf,
    locator: String,
    resource_id: String,
    claim: String,
}

fn completion_temporary_spec(
    config: &Config,
    check: &RustCompletionCheck,
) -> Result<CompletionTemporarySpec, Error> {
    let nonce = Uuid::parse_str(check.run_id.as_str())
        .map_err(|_| Error::InvalidId)?
        .simple()
        .to_string();
    let claim = format!("temp-claim:{nonce}");
    let path = config.artifacts_root.join("tmp").join(&nonce);
    let locator = runtime_locator(&path);
    let resource_id = format!("{}:rust-temp", check.run_id.as_str());
    Ok(CompletionTemporarySpec {
        path,
        locator,
        resource_id,
        claim,
    })
}

async fn prepare_completion_temporary_root(
    config: &Config,
    state: &DaemonState,
    check: &RustCompletionCheck,
) -> Result<CompletionTemporaryRoot, Error> {
    let spec = completion_temporary_spec(config, check)?;
    let resources = state
        .with_store({
            let run_id = check.run_id.clone();
            move |store| store.kernel_resources(&run_id)
        })
        .await?;
    if resources.iter().any(|resource| {
        resource.kind == KernelResourceKind::TemporaryRoot
            && resource.id != spec.resource_id
            && resource.state != KernelResourceState::Released
    }) {
        return Err(DaemonStateError::Store(StoreError::InvalidExecutionMetadata).into());
    }
    let existing = resources
        .iter()
        .find(|resource| resource.id == spec.resource_id);
    if let Some(resource) = existing {
        if resource.kind != KernelResourceKind::TemporaryRoot
            || resource.locator != spec.locator
            || resource.state == KernelResourceState::Released
        {
            return Err(DaemonStateError::Store(StoreError::ResourceIdentityMismatch).into());
        }
        if resource.state == KernelResourceState::Active {
            return validate_active_completion_temporary(spec, resource);
        }
        if resource.state != KernelResourceState::Declared
            || resource.birth_fingerprint.as_deref() != Some(spec.claim.as_str())
        {
            return Err(DaemonStateError::Store(StoreError::ResourceIdentityMismatch).into());
        }
    } else {
        let declare_run = check.run_id.clone();
        let declare_id = spec.resource_id.clone();
        let declare_locator = spec.locator.clone();
        let declare_claim = spec.claim.clone();
        let declared_at_ms = now_ms()?;
        state
            .commit_and_publish(move |store| {
                store.declare_finalizing_resource(
                    &declare_run,
                    &declare_id,
                    KernelResourceKind::TemporaryRoot,
                    &declare_locator,
                    Some(&declare_claim),
                    declared_at_ms,
                )?;
                Ok(((), Vec::new()))
            })
            .await?;
    }
    ensure_empty_private_directory(&spec.path)?;
    let fingerprint = runtime_birth_fingerprint(&spec.path)?.ok_or(Error::InvalidRuntimeRoot)?;
    let identity = rust_verify::exact_directory_identity(&spec.path)?;
    let declare_run = check.run_id.clone();
    let bind_id = spec.resource_id;
    let bind_locator = spec.locator;
    let claim = spec.claim;
    let bound_at_ms = now_ms()?;
    state
        .commit_and_publish(move |store| {
            store.bind_claimed_finalizing_root(
                &declare_run,
                &bind_id,
                &bind_locator,
                &claim,
                &fingerprint,
                bound_at_ms,
            )?;
            Ok(((), Vec::new()))
        })
        .await?;
    Ok(CompletionTemporaryRoot {
        path: spec.path,
        identity,
    })
}

fn validate_active_completion_temporary(
    spec: CompletionTemporarySpec,
    resource: &KernelResource,
) -> Result<CompletionTemporaryRoot, Error> {
    let fingerprint = runtime_birth_fingerprint(&spec.path)?.ok_or(Error::InvalidRuntimeRoot)?;
    if resource.birth_fingerprint.as_deref() != Some(fingerprint.as_str()) {
        return Err(DaemonStateError::Store(StoreError::ResourceIdentityMismatch).into());
    }
    Ok(CompletionTemporaryRoot {
        identity: rust_verify::exact_directory_identity(&spec.path)?,
        path: spec.path,
    })
}

fn ensure_empty_private_directory(path: &Path) -> Result<(), Error> {
    ensure_private_directory(path)?;
    let mut entries = fs::read_dir(path).map_err(|source| Error::Runtime {
        path: path.to_owned(),
        source,
    })?;
    if entries
        .next()
        .transpose()
        .map_err(|source| Error::Runtime {
            path: path.to_owned(),
            source,
        })?
        .is_some()
    {
        return Err(DaemonStateError::Store(StoreError::InvalidExecutionMetadata).into());
    }
    Ok(())
}

async fn prepare_rust_cache(
    config: &Config,
    state: &DaemonState,
    check: &RustCompletionCheck,
    cache_key: &str,
) -> Result<crate::store::RustBuildCache, Error> {
    let path = config.artifacts_root.join("cache").join(cache_key);
    let path_text = path.to_string_lossy().into_owned();
    let declare_run = check.run_id.clone();
    let declare_path = path_text.clone();
    let declared_at_ms = now_ms()?;
    let cache = state
        .commit_and_publish(move |store| {
            let cache = store.declare_rust_cache(&declare_run, &declare_path, declared_at_ms)?;
            Ok((cache, Vec::new()))
        })
        .await?;
    if cache.lifecycle == RustCacheLifecycle::Available {
        let identity = rust_verify::exact_directory_identity(&path)?;
        if cache.dev == Some(identity.device) && cache.inode == Some(identity.inode) {
            return Ok(cache);
        }
        return Err(DaemonStateError::Store(StoreError::InvalidRustBuildMetadata).into());
    }
    if cache.lifecycle != RustCacheLifecycle::Declared {
        return Err(DaemonStateError::Store(StoreError::InvalidRustBuildMetadata).into());
    }
    let measurement = bindable_declared_cache(&path)?;
    let bind_run = check.run_id.clone();
    let bind_path = path_text;
    let bound_at_ms = now_ms()?;
    state
        .commit_and_publish(move |store| {
            let cache = store.bind_rust_cache_identity(
                &bind_run,
                &bind_path,
                measurement.device,
                measurement.inode,
                bound_at_ms,
            )?;
            Ok((cache, Vec::new()))
        })
        .await
        .map_err(Into::into)
}

fn bindable_declared_cache(path: &Path) -> Result<rust_verify::ExactTreeMeasurement, Error> {
    ensure_private_directory(path)?;
    let mut entries = fs::read_dir(path).map_err(|source| Error::Runtime {
        path: path.to_owned(),
        source,
    })?;
    if entries
        .next()
        .transpose()
        .map_err(|source| Error::Runtime {
            path: path.to_owned(),
            source,
        })?
        .is_some()
    {
        return Err(DaemonStateError::Store(StoreError::InvalidRustBuildMetadata).into());
    }
    rust_verify::measure_exact_tree(path).map_err(Into::into)
}

async fn register_effect_resources(
    state: &DaemonState,
    run_id: &RunId,
    nonce: &str,
    pid: u32,
    birth: &str,
    finish_path: &Path,
) -> Result<(), Error> {
    let process_id = format!("{}:rust-effect:{nonce}", run_id.as_str());
    let group_id = format!("{}:rust-effect-group:{nonce}", run_id.as_str());
    let process_locator = serde_json::json!({ "pid": pid, "finish": finish_path }).to_string();
    let group_locator = serde_json::json!({ "pgid": pid, "finish": finish_path }).to_string();
    let register_run = run_id.clone();
    let register_birth = birth.to_owned();
    let registered_at_ms = now_ms()?;
    state
        .commit_and_publish(move |store| {
            for (id, kind, locator) in [
                (
                    process_id.as_str(),
                    KernelResourceKind::EffectProcess,
                    process_locator.as_str(),
                ),
                (
                    group_id.as_str(),
                    KernelResourceKind::EffectGroup,
                    group_locator.as_str(),
                ),
            ] {
                store.declare_finalizing_resource(
                    &register_run,
                    id,
                    kind,
                    locator,
                    Some(&register_birth),
                    registered_at_ms,
                )?;
                store.bind_finalizing_resource(
                    &register_run,
                    id,
                    locator,
                    &register_birth,
                    registered_at_ms,
                )?;
            }
            Ok(((), Vec::new()))
        })
        .await?;
    Ok(())
}

async fn wait_for_worker_result(
    child: &mut Child,
    result_path: &Path,
    budget: Duration,
) -> Result<rust_verify::WorkerResult, Error> {
    let deadline = Instant::now() + budget;
    loop {
        if result_path.try_exists().map_err(|source| Error::Runtime {
            path: result_path.to_owned(),
            source,
        })? {
            let path = result_path.to_owned();
            return tokio::task::spawn_blocking(move || rust_verify::read_worker_result(&path))
                .await?
                .map_err(Into::into);
        }
        if let Some(status) = child.try_wait().map_err(|source| Error::Runtime {
            path: result_path.to_owned(),
            source,
        })? {
            return Err(Error::Runtime {
                path: result_path.to_owned(),
                source: io::Error::other(format!(
                    "Rust completion worker exited before its result: {status}"
                )),
            });
        }
        if Instant::now() >= deadline {
            return Err(Error::Runner(RunnerClientError::TimedOut {
                operation: "Rust completion verification",
            }));
        }
        sleep(CONNECT_RETRY).await;
    }
}

fn failed_worker_result(failure: &str) -> rust_verify::WorkerResult {
    rust_verify::WorkerResult {
        snapshot_digest: None,
        bundle_digest: None,
        success: false,
        diagnostic: bounded_completion_failure(failure),
        bundle_staging: None,
    }
}

async fn remeasure_rust_cache(
    config: &Config,
    state: &DaemonState,
    check: &RustCompletionCheck,
) -> Result<(), Error> {
    let cache_key = check.cache_key.as_deref().ok_or(DaemonStateError::Store(
        StoreError::InvalidRustBuildMetadata,
    ))?;
    let cache_path = config.artifacts_root.join("cache").join(cache_key);
    let cache_text = cache_path.to_string_lossy().into_owned();
    let cache =
        tokio::task::spawn_blocking(move || rust_verify::measure_exact_tree(&cache_path)).await??;
    let cache_run = check.run_id.clone();
    let expected_revision = check.revision;
    let measured_at_ms = now_ms()?;
    state
        .commit_and_publish(move |store| {
            store.record_rust_cache_measurement(
                &cache_run,
                expected_revision,
                &cache_text,
                cache.device,
                cache.inode,
                cache.allocated_bytes,
                measured_at_ms,
            )?;
            Ok(((), Vec::new()))
        })
        .await?;
    Ok(())
}

async fn terminate_effect_group(
    state: &DaemonState,
    run_id: &RunId,
    mut child: Option<&mut Child>,
    _finish_path: &Path,
) -> Result<(), Error> {
    let _resources = state
        .with_store({
            let run_id = run_id.clone();
            move |store| store.kernel_resources(&run_id)
        })
        .await?;
    #[cfg(target_os = "linux")]
    if let Some(group) = _resources.iter().find(|resource| {
        resource.kind == KernelResourceKind::EffectGroup
            && resource.state != KernelResourceState::Released
    }) {
        kill_registered_effect_group(group)?;
    }
    #[cfg(not(target_os = "linux"))]
    write_finish_signal(_finish_path)?;
    if let Some(child) = child.as_mut() {
        let _ = timeout(RUNNER_EXIT_GRACE, child.wait()).await;
    }
    release_effect_resources(state, run_id).await
}

#[cfg(target_os = "linux")]
fn kill_registered_effect_group(resource: &KernelResource) -> Result<(), Error> {
    let pgid = locator_number(&resource.locator, "pgid").ok_or(DaemonStateError::Store(
        StoreError::InvalidExecutionMetadata,
    ))?;
    let expected = resource
        .birth_fingerprint
        .as_deref()
        .ok_or(DaemonStateError::Store(
            StoreError::InvalidExecutionMetadata,
        ))?;
    if !exact_effect_group_leader(Some(expected), process_birth_fingerprint(pgid)?.as_deref()) {
        if process_group_absent(resource)? {
            return Ok(());
        }
        // A process group may outlive its leader. Without that leader's exact
        // birth identity, signalling the numeric PGID could hit a reused
        // group. Keep the check and its cache nonterminal until group absence
        // is independently proven.
        return Err(Error::ProcessIdentityUnavailable(pgid));
    }
    let Some(pid) = Pid::from_raw(pgid as i32) else {
        return Ok(());
    };
    match kill_process_group(pid, Signal::KILL) {
        Ok(()) | Err(rustix::io::Errno::SRCH) => Ok(()),
        Err(error) => Err(Error::Runtime {
            path: PathBuf::from(format!("effect-group:{pgid}")),
            source: io::Error::from_raw_os_error(error.raw_os_error()),
        }),
    }
}

#[cfg(any(target_os = "linux", test))]
fn exact_effect_group_leader(expected: Option<&str>, current: Option<&str>) -> bool {
    expected.is_some() && expected == current
}

#[cfg(not(target_os = "linux"))]
fn write_finish_signal(path: &Path) -> Result<(), Error> {
    use std::os::unix::fs::OpenOptionsExt as _;

    let opened = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path);
    let file = match opened {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            let metadata = fs::symlink_metadata(path).map_err(|source| Error::Runtime {
                path: path.to_owned(),
                source,
            })?;
            if metadata.file_type().is_symlink()
                || !metadata.is_file()
                || metadata.uid() != rustix::process::geteuid().as_raw()
                || metadata.mode() & 0o777 != 0o600
            {
                return Err(Error::InvalidRuntimeRoot);
            }
            return Ok(());
        }
        Err(source) => {
            return Err(Error::Runtime {
                path: path.to_owned(),
                source,
            });
        }
    };
    file.sync_all().map_err(|source| Error::Runtime {
        path: path.to_owned(),
        source,
    })
}

async fn release_effect_resources(state: &DaemonState, run_id: &RunId) -> Result<(), Error> {
    let deadline = Instant::now() + RUNNER_EXIT_GRACE;
    loop {
        let resources = state
            .with_store({
                let run_id = run_id.clone();
                move |store| store.kernel_resources(&run_id)
            })
            .await?;
        let mut pending = false;
        for resource in resources.iter().filter(|resource| {
            matches!(
                resource.kind,
                KernelResourceKind::EffectProcess | KernelResourceKind::EffectGroup
            ) && resource.state != KernelResourceState::Released
        }) {
            let absent = match resource.kind {
                KernelResourceKind::EffectProcess => process_resource_absent(resource)?,
                KernelResourceKind::EffectGroup => process_group_absent(resource)?,
                _ => false,
            };
            if absent {
                release_resource(state, resource).await?;
            } else {
                pending = true;
            }
        }
        if !pending {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(Error::Runner(RunnerClientError::TimedOut {
                operation: "Rust completion effect exit",
            }));
        }
        sleep(CONNECT_RETRY).await;
    }
}

async fn persist_measured_rust_worker_result(
    config: &Config,
    state: &DaemonState,
    check: &RustCompletionCheck,
    result: rust_verify::WorkerResult,
) -> Result<(), Error> {
    if let Err(error) = remeasure_rust_cache(config, state, check).await {
        let failure = bounded_completion_failure(&format!(
            "Rust cache could not be measured after its writer exited: {error}"
        ));
        return fail_running_with_cache_handoff(state, check, &failure).await;
    }
    persist_rust_worker_result(state, check, result).await
}

async fn finish_released_rust_effect(
    config: &Config,
    state: &DaemonState,
    check: &RustCompletionCheck,
    result: rust_verify::WorkerResult,
) -> Result<(), Error> {
    persist_measured_rust_worker_result(config, state, check, result).await?;
    release_completion_temporary_root(state, &check.run_id).await
}

async fn persist_rust_worker_result(
    state: &DaemonState,
    check: &RustCompletionCheck,
    result: rust_verify::WorkerResult,
) -> Result<(), Error> {
    if !result.success {
        return fail_rust_completion(state, check, &result.diagnostic).await;
    }
    let Some(source_digest) = result.snapshot_digest else {
        return fail_rust_completion(state, check, "Rust verifier returned no source digest").await;
    };
    let Some(bundle_digest) = result.bundle_digest else {
        return fail_rust_completion(state, check, "Rust verifier returned no bundle digest").await;
    };
    let Some(staging) = result.bundle_staging else {
        return fail_rust_completion(state, check, "Rust verifier returned no prepared bundle")
            .await;
    };
    let verify_source = source_digest.clone();
    let verify_bundle = bundle_digest.clone();
    let verify_cache = check.cache_key.clone().ok_or(DaemonStateError::Store(
        StoreError::InvalidRustBuildMetadata,
    ))?;
    let verified = tokio::task::spawn_blocking(move || {
        rust_verify::verify_exact_bundle(&staging, &verify_source, &verify_bundle, &verify_cache)
    })
    .await?;
    if let Err(error) = verified {
        return fail_rust_completion(
            state,
            check,
            &format!("prepared Rust test bundle failed final verification: {error}"),
        )
        .await;
    }

    let pass_run = check.run_id.clone();
    let pass_revision = check.revision;
    let passed_at_ms = now_ms()?;
    state
        .commit_and_publish(move |store| {
            store.pass_rust_completion_check(
                &pass_run,
                pass_revision,
                &source_digest,
                &bundle_digest,
                passed_at_ms,
            )?;
            Ok(((), Vec::new()))
        })
        .await?;
    Ok(())
}

async fn fail_rust_completion(
    state: &DaemonState,
    check: &RustCompletionCheck,
    failure: &str,
) -> Result<(), Error> {
    let failure = bounded_completion_failure(failure);
    let run_id = check.run_id.clone();
    let revision = check.revision;
    let failed_at_ms = now_ms()?;
    state
        .commit_and_publish(move |store| {
            store.fail_rust_completion_check(&run_id, revision, &failure, failed_at_ms)?;
            Ok(((), Vec::new()))
        })
        .await?;
    Ok(())
}

async fn fail_and_release_rust_setup(
    state: &DaemonState,
    check: &RustCompletionCheck,
    failure: &str,
) -> Result<(), Error> {
    fail_rust_completion(state, check, failure).await?;
    release_completion_temporary_root(state, &check.run_id).await
}

async fn fail_claimed_rust_setup(
    config: &Config,
    state: &DaemonState,
    check: &RustCompletionCheck,
    error: &Error,
) -> Result<(), Error> {
    let measurement = remeasure_rust_cache(config, state, check).await;
    let failure = bounded_completion_failure(&match &measurement {
        Ok(()) => error.to_string(),
        Err(measurement_error) => format!(
            "{error}; Rust cache could not be measured after setup failure: {measurement_error}"
        ),
    });
    if measurement.is_ok() {
        fail_rust_completion(state, check, &failure).await?;
    } else {
        fail_running_with_cache_handoff(state, check, &failure).await?;
    }
    release_completion_temporary_root(state, &check.run_id).await
}

async fn fail_running_with_cache_handoff(
    state: &DaemonState,
    check: &RustCompletionCheck,
    failure: &str,
) -> Result<(), Error> {
    let run_id = check.run_id.clone();
    let revision = check.revision;
    let failure = bounded_completion_failure(failure);
    let at_ms = now_ms()?;
    state
        .commit_and_publish(move |store| {
            match store.fail_rust_completion_and_reclaim_cache(&run_id, revision, &failure, at_ms) {
                Ok(_) => {}
                Err(StoreError::InvalidRustBuildMetadata) => {
                    store.fail_rust_completion_check(&run_id, revision, &failure, at_ms)?;
                }
                Err(error) => return Err(error),
            }
            Ok(((), Vec::new()))
        })
        .await?;
    Ok(())
}

fn bounded_completion_failure(failure: &str) -> String {
    let failure = if failure.is_empty() {
        "Rust completion verification failed"
    } else {
        failure
    };
    let mut end = failure.len().min(4096);
    while !failure.is_char_boundary(end) {
        end -= 1;
    }
    failure[..end].to_owned()
}

async fn recover_rust_completion(
    config: &Config,
    state: &DaemonState,
    check: RustCompletionCheck,
) -> Result<(), Error> {
    let resources = state
        .with_store({
            let run_id = check.run_id.clone();
            move |store| store.kernel_resources(&run_id)
        })
        .await?;
    if rust_effect_was_attempted(&resources) {
        return recover_prior_rust_effect(config, state, check, resources).await;
    }

    let change = load_available_completion_change(state, &check).await?;
    let temporary = match prepare_completion_temporary_root(config, state, &check).await {
        Ok(temporary) => temporary,
        Err(error) => {
            return fail_and_release_rust_setup(state, &check, &error.to_string()).await;
        }
    };
    let cache_key = check.cache_key.as_deref().ok_or(DaemonStateError::Store(
        StoreError::InvalidRustBuildMetadata,
    ))?;
    let cache = match prepare_rust_cache(config, state, &check, cache_key).await {
        Ok(cache) => cache,
        Err(error) => return fail_claimed_rust_setup(config, state, &check, &error).await,
    };
    if let Err(error) =
        prepare_rust_worker_invocation(config, &check, &change, &temporary, &cache).await
    {
        return fail_claimed_rust_setup(config, state, &check, &error).await;
    }
    let path = temporary.path;
    let result = match run_rust_worker_effect(config, state, &check, &path).await {
        Ok(RustWorkerEffectOutcome::Released(result)) => result,
        Ok(RustWorkerEffectOutcome::Pending) => return Ok(()),
        Err(error) => return fail_claimed_rust_setup(config, state, &check, &error).await,
    };
    finish_released_rust_effect(config, state, &check, result).await
}

async fn recover_prior_rust_effect(
    config: &Config,
    state: &DaemonState,
    check: RustCompletionCheck,
    resources: Vec<KernelResource>,
) -> Result<(), Error> {
    if let Some(effect) = resources.iter().find(|resource| {
        matches!(
            resource.kind,
            KernelResourceKind::EffectProcess | KernelResourceKind::EffectGroup
        ) && resource.state != KernelResourceState::Released
    }) {
        let finish_path = locator_named_path(&effect.locator, "finish").ok_or(
            DaemonStateError::Store(StoreError::InvalidExecutionMetadata),
        )?;
        terminate_effect_group(state, &check.run_id, None, &finish_path).await?;
    }

    let temporary = match prepare_completion_temporary_root(config, state, &check).await {
        Ok(temporary) => temporary,
        Err(error) => {
            return finish_released_rust_effect(
                config,
                state,
                &check,
                failed_worker_result(&error.to_string()),
            )
            .await;
        }
    };
    let path = temporary.path;
    let result_path = path.join("result.json");
    let result_exists = result_path.try_exists().map_err(|source| Error::Runtime {
        path: result_path.clone(),
        source,
    })?;
    let result = if result_exists {
        let read_path = result_path;
        match tokio::task::spawn_blocking(move || rust_verify::read_worker_result(&read_path))
            .await?
        {
            Ok(result) => result,
            Err(error) => failed_worker_result(&error.to_string()),
        }
    } else {
        failed_worker_result(
            "Rust verification was interrupted by daemon restart; refusing to rebuild from mutable Change source",
        )
    };
    finish_released_rust_effect(config, state, &check, result).await
}

fn rust_effect_was_attempted(resources: &[KernelResource]) -> bool {
    resources.iter().any(|resource| {
        matches!(
            resource.kind,
            KernelResourceKind::EffectProcess | KernelResourceKind::EffectGroup
        )
    })
}

async fn release_completion_temporary_root(
    state: &DaemonState,
    run_id: &RunId,
) -> Result<(), Error> {
    let resources = state
        .with_store({
            let run_id = run_id.clone();
            move |store| store.kernel_resources(&run_id)
        })
        .await?;
    for resource in resources.iter().filter(|resource| {
        resource.kind == KernelResourceKind::TemporaryRoot
            && resource.state != KernelResourceState::Released
    }) {
        let Some(path) = locator_path(&resource.locator) else {
            mark_resource_unresolved(state, resource, "temporary locator is invalid").await?;
            continue;
        };
        let removal = match resource.birth_fingerprint.as_deref() {
            Some(claim) if claim.starts_with("temp-claim:") => {
                let Some(nonce) = claim.strip_prefix("temp-claim:") else {
                    continue;
                };
                let quarantine = path.with_file_name(format!(".finalize-{nonce}"));
                remove_runtime_if_claimed(&path, &quarantine, nonce)
            }
            fingerprint => {
                let quarantine = path.with_file_name(format!(".finalize-{}", run_id.as_str()));
                remove_runtime_if_exact(&path, &quarantine, fingerprint)
            }
        };
        match removal {
            Ok(RuntimeRemoval::Missing | RuntimeRemoval::Removed) => {
                release_resource(state, resource).await?;
            }
            Ok(RuntimeRemoval::Unproven) => {
                mark_resource_unresolved(
                    state,
                    resource,
                    "temporary root identity could not be proven",
                )
                .await?;
            }
            Err(error) => {
                mark_resource_unresolved(state, resource, &error.to_string()).await?;
            }
        }
    }
    Ok(())
}

async fn reconcile_rust_storage(config: &Config, state: &DaemonState) -> Result<(), Error> {
    let declared = state
        .with_store(|store| store.recoverable_rust_cache_declarations())
        .await?;
    if let Some(candidate) = declared.into_iter().next() {
        return reconcile_declared_rust_cache(state, candidate).await;
    }
    let recoverable = state
        .with_store(|store| store.recoverable_rust_reclaims())
        .await?;
    if let Some(candidate) = recoverable.into_iter().next() {
        return reclaim_rust_artifact(state, candidate).await;
    }
    let summary = state
        .with_store(|store| store.rust_storage_summary())
        .await?;
    let over_limit = summary.cache_count > MAX_RUST_CACHE_COUNT
        || summary
            .cache_bytes
            .is_some_and(|bytes| bytes > MAX_RUST_CACHE_BYTES);
    if !over_limit {
        return Ok(());
    }
    let candidate = state
        .with_store(|store| Ok(store.rust_reclaim_candidates(1)?.into_iter().next()))
        .await?;
    if let Some(candidate) = candidate {
        reclaim_rust_artifact(state, candidate).await?;
    } else {
        tracing::warn!(
            artifacts_root = %config.artifacts_root.display(),
            "Rust storage exceeds policy but every exact cache is protected or has failed reconciliation"
        );
    }
    Ok(())
}

async fn reconcile_declared_rust_cache(
    state: &DaemonState,
    candidate: RustBuildCache,
) -> Result<(), Error> {
    let path = PathBuf::from(candidate.path);
    let incarnation = candidate.project_incarnation_id;
    let digest = candidate.cache_key;
    let quarantine = format!(".reclaim-cache-{digest}");
    let removed = tokio::task::spawn_blocking(move || {
        rust_verify::remove_empty_claimed_directory(&path, &quarantine)
    })
    .await?;
    let at_ms = now_ms()?;
    match removed {
        Ok(()) => {
            state
                .commit_and_publish(move |store| {
                    store.finish_absent_declared_rust_cache(&incarnation, &digest)?;
                    Ok(((), Vec::new()))
                })
                .await?;
        }
        Err(error) => {
            let failure = bounded_completion_failure(&error.to_string());
            state
                .commit_and_publish(move |store| {
                    store.record_rust_cache_failure(&incarnation, &digest, &failure, at_ms)?;
                    Ok(((), Vec::new()))
                })
                .await?;
        }
    }
    Ok(())
}

async fn reclaim_rust_artifact(
    state: &DaemonState,
    candidate: RustBuildCache,
) -> Result<(), Error> {
    let path = PathBuf::from(&candidate.path);
    let device = candidate.dev.ok_or(DaemonStateError::Store(
        StoreError::InvalidRustBuildMetadata,
    ))?;
    let inode = candidate.inode.ok_or(DaemonStateError::Store(
        StoreError::InvalidRustBuildMetadata,
    ))?;
    let quarantine = format!(".reclaim-cache-{}", candidate.cache_key);
    let incarnation = candidate.project_incarnation_id.clone();
    let digest = candidate.cache_key.clone();
    let claim = candidate.clone();
    let claimed_at_ms = now_ms()?;
    state
        .commit_and_publish(move |store| {
            store.begin_rust_cache_reclaim(&claim, claimed_at_ms)?;
            Ok(((), Vec::new()))
        })
        .await?;
    let removed = tokio::task::spawn_blocking(move || {
        rust_verify::remove_exact_tree(&path, device, inode, &quarantine)
    })
    .await?;
    let at_ms = now_ms()?;
    match removed {
        Ok(()) => {
            state
                .commit_and_publish(move |store| {
                    store.finish_rust_cache_reclaim(&incarnation, &digest)?;
                    Ok(((), Vec::new()))
                })
                .await?;
        }
        Err(error) => {
            let failure = bounded_completion_failure(&error.to_string());
            state
                .commit_and_publish(move |store| {
                    store.record_rust_cache_failure(&incarnation, &digest, &failure, at_ms)?;
                    Ok(((), Vec::new()))
                })
                .await?;
        }
    }
    Ok(())
}

async fn reconcile_change(
    config: &Config,
    state: &DaemonState,
    project_id: ProjectId,
    change_id: ChangeId,
) -> Result<(), Error> {
    let lookup_project = project_id.clone();
    let lookup_change = change_id.clone();
    let change = state
        .with_store(move |store| {
            store
                .change(&lookup_project, &lookup_change)?
                .ok_or(StoreError::ChangeNotFound)
        })
        .await?;
    verify_managed_change_path(&config.changes_root, &change)?;
    match change.phase {
        ChangePhase::Provisioning if change.size_bytes.is_none() => {
            measure_provisioning_change(&config.changes_root, state, change).await
        }
        ChangePhase::Provisioning | ChangePhase::Removed => Ok(()),
        ChangePhase::Available if change.size_bytes.is_none() => {
            measure_change(&config.changes_root, state, change).await
        }
        ChangePhase::Available => Ok(()),
        ChangePhase::Removing => remove_change_source(&config.changes_root, state, change).await,
    }
}

async fn measure_provisioning_change(
    changes_root: &Path,
    state: &DaemonState,
    change: Change,
) -> Result<(), Error> {
    let change_uuid = parse_change_uuid(&change.id)?;
    let changes_root = changes_root.to_owned();
    let checkpoint_path = selection_record_path(&changes_root, change_uuid);
    let limits = SourceLimits::new(CHANGE_SCAN_MAX_ENTRIES, CHANGE_SCAN_MAX_BYTES)?;
    let measurement = tokio::task::spawn_blocking(move || {
        let selection = change_source::read_selection_record(&checkpoint_path)?;
        change_source::measure_quiescent_provisioning_source(
            &changes_root,
            change_uuid,
            &selection,
            limits,
        )
    })
    .await??;
    let measured_at_ms = now_ms()?;
    state
        .commit_and_publish(move |store| {
            let mutation = store.record_provisioning_measurement(
                &change.project_id,
                &change.id,
                change.revision,
                measurement.allocated_bytes,
                measured_at_ms,
            )?;
            Ok(((), mutation.into_parts().1))
        })
        .await?;
    Ok(())
}

async fn measure_change(
    changes_root: &Path,
    state: &DaemonState,
    change: Change,
) -> Result<(), Error> {
    let identity = change_source_identity(&change)?;
    let change_uuid = parse_change_uuid(&change.id)?;
    let changes_root = changes_root.to_owned();
    let limits = SourceLimits::new(CHANGE_SCAN_MAX_ENTRIES, CHANGE_SCAN_MAX_BYTES)?;
    let expected = DirectoryIdentity {
        device: identity.device,
        inode: identity.inode,
    };
    let measured = tokio::task::spawn_blocking(move || {
        change_source::measure_quiescent_source(&changes_root, change_uuid, expected, limits)
    })
    .await??;
    let measurement = ChangeSourceIdentity {
        size_bytes: measured.allocated_bytes,
        ..identity
    };
    let measured_at_ms = now_ms()?;
    state
        .commit_and_publish(move |store| {
            let mutation = store.record_change_measurement(
                &change.project_id,
                &change.id,
                change.revision,
                &measurement,
                measured_at_ms,
            )?;
            Ok(((), mutation.into_parts().1))
        })
        .await?;
    Ok(())
}

async fn remove_change_source(
    changes_root: &Path,
    state: &DaemonState,
    change: Change,
) -> Result<(), Error> {
    let change_uuid = parse_change_uuid(&change.id)?;
    let changes_root = changes_root.to_owned();
    let removal_kind = change.removal_kind().map_err(DaemonStateError::Store)?;
    let removal = tokio::task::spawn_blocking(move || match removal_kind {
        ChangeRemovalKind::Published(identity) => {
            let expected = DirectoryIdentity {
                device: identity.device,
                inode: identity.inode,
            };
            change_source::remove_quiescent_source(&changes_root, change_uuid, expected)
        }
        ChangeRemovalKind::Provisioning => {
            let checkpoint_path = selection_record_path(&changes_root, change_uuid);
            let checkpoint = match fs::symlink_metadata(&checkpoint_path) {
                Err(error) if error.kind() == io::ErrorKind::NotFound => None,
                Ok(_) => Some(change_source::read_selection_record(&checkpoint_path)?),
                Err(source) => {
                    return Err(change_source::Error::Io {
                        path: checkpoint_path,
                        source,
                    });
                }
            };
            change_source::remove_provisioning_source(
                &changes_root,
                change_uuid,
                checkpoint.as_ref(),
            )?;
            if let Some(checkpoint) = checkpoint {
                change_source::remove_selection_record(&checkpoint_path, &checkpoint)?;
            }
            Ok(change_source::RemovalOutcome::Removed)
        }
    })
    .await?;
    match removal {
        Ok(_) => {
            let removed_at_ms = now_ms()?;
            state
                .commit_and_publish(move |store| {
                    let mutation = store.mark_change_removed(
                        &change.project_id,
                        &change.id,
                        change.revision,
                        removed_at_ms,
                    )?;
                    Ok(((), mutation.into_parts().1))
                })
                .await?;
            Ok(())
        }
        Err(error) => {
            let failure = error.to_string();
            let failed_at_ms = now_ms()?;
            state
                .commit_and_publish(move |store| {
                    let mutation = store.record_change_failure(
                        &change.project_id,
                        &change.id,
                        change.revision,
                        &failure,
                        failed_at_ms,
                    )?;
                    Ok(((), mutation.into_parts().1))
                })
                .await?;
            Err(error.into())
        }
    }
}

fn parse_change_uuid(change_id: &ChangeId) -> Result<Uuid, Error> {
    Uuid::parse_str(change_id.as_str()).map_err(|_| Error::InvalidId)
}

fn selection_record_path(changes_root: &Path, change_id: Uuid) -> PathBuf {
    changes_root
        .join(".checkpoints")
        .join(format!("{}.selection.json", change_id.simple()))
}

fn verify_managed_change_path(changes_root: &Path, change: &Change) -> Result<(), Error> {
    let change_uuid = parse_change_uuid(&change.id)?;
    let expected = changes_root.join(change_uuid.simple().to_string());
    if Path::new(&change.source_root) == expected {
        Ok(())
    } else {
        Err(DaemonStateError::Store(StoreError::ChangeIdentityMismatch).into())
    }
}

fn change_source_identity(change: &Change) -> Result<ChangeSourceIdentity, Error> {
    let invalid = || DaemonStateError::Store(StoreError::InvalidChangeMetadata);
    Ok(ChangeSourceIdentity {
        source_root: change.source_root.clone(),
        device: change.source_dev.ok_or_else(invalid)?,
        inode: change.source_inode.ok_or_else(invalid)?,
        size_bytes: change.size_bytes.unwrap_or(0),
    })
}

async fn reconcile_one(
    state: &DaemonState,
    commands: &mpsc::Sender<Command>,
    observed: &mut HashSet<RunId>,
    run_id: RunId,
    grace_ms: u64,
) -> Result<(), Error> {
    let lookup = run_id.clone();
    let run = state
        .with_store(move |store| {
            Ok(store
                .recoverable_kernel_runs()?
                .into_iter()
                .find(|candidate| candidate.run.id == lookup))
        })
        .await?;
    let Some(run) = run else {
        return Ok(());
    };
    if run.run.phase == RunPhase::Admitted {
        recover_admitted_run(state, commands, observed, run).await?;
        return Ok(());
    }
    if run.run.phase == RunPhase::Finalizing {
        let client = RunnerClient::new(
            &run.runner_runtime,
            run.run.id.clone(),
            run.runner_instance_id.clone(),
        );
        if let Err(error) = client.stop(grace_ms).await {
            tracing::debug!(run_id = %run.run.id, %error, "exact runner stop deferred to finalizer");
            kill_registered_processes(&run.resources)?;
        }
    }
    if release_absent_resources(state, &run).await? {
        return Ok(());
    }
    if observed.insert(run.run.id.clone()) {
        spawn_observer(state.clone(), commands.clone(), run, None);
    }
    Ok(())
}

async fn recover_admitted_run(
    state: &DaemonState,
    commands: &mpsc::Sender<Command>,
    observed: &mut HashSet<RunId>,
    run: RecoverableKernelRun,
) -> Result<(), Error> {
    let client = RunnerClient::new(
        &run.runner_runtime,
        run.run.id.clone(),
        run.runner_instance_id.clone(),
    );
    let prepared = match prepare_with_grace(&client).await {
        Ok(prepared) => prepared,
        Err(error) => {
            tracing::warn!(run_id = %run.run.id, %error, "admitted runner was not recoverable");
            fail_unrecoverable_admission(state, &run).await?;
            return Ok(());
        }
    };
    let identity = match recovered_prepared_identity(&run, &prepared) {
        Ok(identity) => identity,
        Err(error) => {
            drop(prepared);
            fail_unrecoverable_admission(state, &run).await?;
            return Err(error);
        }
    };
    let run_id = run.run.id.clone();
    let activated_at_ms = now_ms()?;
    let (activated_run, resources) = state
        .commit_and_publish(move |store| {
            let (activated, events) =
                store.activate_prepared_run(&run_id, identity, activated_at_ms)?;
            let resources = store.kernel_resources(&run_id)?;
            Ok(((activated, resources), events))
        })
        .await?;
    if let Err(error) = prepared.activate().await {
        // Running is already durable. A lost acknowledgement is ambiguous,
        // so observation of this exact runner, never a second launch, decides.
        tracing::warn!(run_id = %activated_run.id, %error, "recovered runner activation acknowledgement was lost");
    }
    let recovered = RecoverableKernelRun {
        run: activated_run,
        change_id: run.change_id,
        source_root: run.source_root,
        runner_instance_id: run.runner_instance_id,
        runner_runtime: run.runner_runtime,
        resources,
    };
    if observed.insert(recovered.run.id.clone()) {
        spawn_observer(state.clone(), commands.clone(), recovered, None);
    }
    Ok(())
}

fn recovered_prepared_identity(
    run: &RecoverableKernelRun,
    prepared: &PreparedRunner,
) -> Result<PreparedProcessIdentity, Error> {
    let runtime = Path::new(&run.runner_runtime);
    let runtime_birth = runtime_birth_fingerprint(runtime)?.ok_or(Error::InvalidRuntimeRoot)?;
    let runner_pid = prepared.runner_pid();
    let provider_pid = prepared.child_pid();
    let process_group = prepared.process_group_id();
    let runner_birth = process_birth_fingerprint(runner_pid)?
        .ok_or(Error::ProcessIdentityUnavailable(runner_pid))?;
    let provider_birth = process_birth_fingerprint(provider_pid)?
        .ok_or(Error::ProcessIdentityUnavailable(provider_pid))?;
    Ok(PreparedProcessIdentity {
        runtime_locator: runtime_locator(runtime),
        runtime_birth_fingerprint: runtime_birth,
        runner_locator: runner_locator(runner_pid, &run.runner_instance_id),
        runner_birth_fingerprint: runner_birth,
        provider_locator: serde_json::json!({ "pid": provider_pid }).to_string(),
        provider_birth_fingerprint: provider_birth.clone(),
        process_group_locator: serde_json::json!({ "pgid": process_group }).to_string(),
        process_group_birth_fingerprint: provider_birth,
    })
}

async fn fail_unrecoverable_admission(
    state: &DaemonState,
    run: &RecoverableKernelRun,
) -> Result<(), Error> {
    let run_id = run.run.id.clone();
    let failed_at_ms = now_ms()?;
    state
        .commit_and_publish(move |store| {
            let (_, events) =
                store.fail_admitted_run(&run_id, RunFailureReason::Spawn, failed_at_ms)?;
            Ok(((), events))
        })
        .await?;
    kill_registered_processes(&run.resources)?;
    let refreshed = state
        .with_store({
            let run_id = run.run.id.clone();
            move |store| {
                let recovered = store
                    .recoverable_kernel_runs()?
                    .into_iter()
                    .find(|candidate| candidate.run.id == run_id)
                    .ok_or(StoreError::RunNotFound)?;
                Ok(recovered)
            }
        })
        .await?;
    // A declared runner has no process identity and never passed Prepare.
    // The bounded authenticated connection failure above is the authority to
    // abandon that declaration; active identities still require absence.
    for resource in refreshed.resources.iter().filter(|resource| {
        resource.kind == KernelResourceKind::RunnerProcess
            && resource.state == KernelResourceState::Releasing
            && resource.birth_fingerprint.is_none()
    }) {
        release_resource(state, resource).await?;
    }
    let _ = release_absent_resources(state, &refreshed).await?;
    Ok(())
}

fn spawn_observer(
    state: DaemonState,
    commands: mpsc::Sender<Command>,
    run: RecoverableKernelRun,
    child: Option<Child>,
) {
    tokio::spawn(async move {
        let run_id = run.run.id.clone();
        if let Err(error) = observe_run(&state, &run, child).await {
            tracing::warn!(%run_id, %error, "attempt finalizer paused");
        }
        let _ = commands.send(Command::ObserverFinished(run_id)).await;
    });
}

async fn observe_run(
    state: &DaemonState,
    run: &RecoverableKernelRun,
    mut child: Option<Child>,
) -> Result<(), Error> {
    let client = RunnerClient::new(
        &run.runner_runtime,
        run.run.id.clone(),
        run.runner_instance_id.clone(),
    );
    if run.run.phase == RunPhase::Finalizing
        && let Err(error) = client.stop(DEFAULT_FINALIZE_GRACE_MS).await
    {
        tracing::debug!(run_id = %run.run.id, %error, "runner stop failed; using registered identities");
        kill_registered_processes(&run.resources)?;
    }
    let mut subscription = match subscribe_with_grace(&client).await {
        Ok(subscription) => subscription,
        Err(error) if run.run.phase == RunPhase::Admitted => {
            cleanup_unactivated(state, run, child).await;
            return Err(error.into());
        }
        Err(error) => {
            // The runner may have exited and removed its listener while this
            // observer was retrying. Refresh durable phase first: an exact
            // absent identity is stronger evidence than a stale socket error
            // and can complete an already-requested outcome without turning
            // a successful attempt into a process failure.
            let run_id = run.run.id.clone();
            let refreshed = state
                .with_store(move |store| {
                    store
                        .recoverable_kernel_runs()?
                        .into_iter()
                        .find(|candidate| candidate.run.id == run_id)
                        .ok_or(StoreError::RunNotFound)
                })
                .await?;
            if release_absent_resources(state, &refreshed).await? {
                return Ok(());
            }
            mark_runner_unresolved(state, &refreshed, &error.to_string()).await;
            if refreshed.run.phase == RunPhase::Running {
                let run_id = refreshed.run.id.clone();
                let failed_at_ms = now_ms()?;
                state
                    .commit_and_publish(move |store| {
                        let (_, events) = store.fail_running_run(
                            &run_id,
                            RunFailureReason::Process,
                            failed_at_ms,
                        )?;
                        Ok(((), events))
                    })
                    .await?;
            }
            if matches!(
                refreshed.run.phase,
                RunPhase::Running | RunPhase::Finalizing
            ) {
                kill_registered_processes(&refreshed.resources)?;
            }
            return Err(error.into());
        }
    };
    let observed = match drive_change_activation(state, run, &mut subscription).await? {
        Some(exit) => exit,
        None => consume_until_exit(&mut subscription).await?,
    };
    let observe_run_id = run.run.id.clone();
    let observed_at_ms = now_ms()?;
    state
        .commit_and_publish(move |store| {
            let (_, events) = store.observe_attempt_exit(
                &observe_run_id,
                observed.terminal_sequence,
                observed.exit_code,
                observed.exit_signal,
                observed.failure_reason,
                observed_at_ms,
            )?;
            Ok(((), events))
        })
        .await?;
    client.acknowledge_exit(observed.terminal_sequence).await?;
    if let Some(child) = child.as_mut() {
        if timeout(RUNNER_EXIT_GRACE, child.wait()).await.is_err()
            && let Some(pid) = child.id().and_then(|value| Pid::from_raw(value as i32))
        {
            let _ = kill_process_group(pid, Signal::KILL);
            let _ = child.wait().await;
        }
    } else {
        wait_for_registered_runner_exit(run).await?;
    }
    release_completed_resources(state, run).await
}

async fn subscribe_with_grace(
    client: &RunnerClient,
) -> Result<RunnerSubscription, RunnerClientError> {
    let deadline = Instant::now() + CONNECT_GRACE;
    loop {
        match client.subscribe().await {
            Ok(subscription) => return Ok(subscription),
            Err(RunnerClientError::Io(error))
                if matches!(
                    error.kind(),
                    io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
                ) && Instant::now() < deadline =>
            {
                sleep(CONNECT_RETRY).await;
            }
            Err(error) => return Err(error),
        }
    }
}

async fn consume_until_exit(
    subscription: &mut RunnerSubscription,
) -> Result<ObservedExit, RunnerClientError> {
    loop {
        if let Some(exit) = runner_stream_exit(subscription.next_item().await?) {
            return Ok(exit);
        }
    }
}

fn runner_stream_exit(item: RunnerStreamItem) -> Option<ObservedExit> {
    let RunnerStreamItem::Event(event) = item else {
        return None;
    };
    match event.event {
        RunnerEvent::Exited { exit_code, signal } => Some(ObservedExit {
            terminal_sequence: event.sequence,
            exit_code,
            exit_signal: signal,
            failure_reason: None,
        }),
        RunnerEvent::SpawnFailed { .. } => Some(ObservedExit {
            terminal_sequence: event.sequence,
            exit_code: None,
            exit_signal: None,
            failure_reason: Some(RunFailureReason::Spawn),
        }),
        RunnerEvent::Started { .. } => None,
    }
}

enum Checkpoint<T> {
    Record(T),
    Exit(ObservedExit),
}

async fn drive_change_activation(
    state: &DaemonState,
    run: &RecoverableKernelRun,
    subscription: &mut RunnerSubscription,
) -> Result<Option<ObservedExit>, Error> {
    let Some(change_id) = run.change_id.as_ref() else {
        return Ok(None);
    };
    let invocation_path = Path::new(&run.runner_runtime).join("materializer.json");
    match fs::symlink_metadata(&invocation_path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            return Err(Error::Runtime {
                path: invocation_path,
                source,
            });
        }
        Ok(_) => {}
    }
    let invocation = change_source::read_materializer_invocation(&invocation_path)?;
    if invocation.change_id != parse_change_uuid(change_id)?
        || Path::new(&run.source_root)
            != invocation
                .changes_root
                .join(invocation.change_id.simple().to_string())
    {
        return Err(DaemonStateError::Store(StoreError::ChangeIdentityMismatch).into());
    }
    let lookup_project = run.run.project_id.clone();
    let lookup_change = change_id.clone();
    let mut change = state
        .with_store(move |store| {
            store
                .change(&lookup_project, &lookup_change)?
                .ok_or(StoreError::ChangeNotFound)
        })
        .await?;

    let selection = if change.phase == ChangePhase::Provisioning {
        let selection =
            match wait_for_selection_record(&invocation.selection_record_path, subscription).await?
            {
                Checkpoint::Record(selection) => selection,
                Checkpoint::Exit(exit) => {
                    record_provisioning_exit(state, &change, &exit).await?;
                    return Ok(Some(exit));
                }
            };
        let base_identity = ChangeBaseIdentity {
            repository_root: selection.repository_root.to_string_lossy().into_owned(),
            device: selection.repository_device,
            inode: selection.repository_inode,
        };
        let project_id = change.project_id.clone();
        let exact_change_id = change.id.clone();
        let base_oid = selection.base_oid.as_str().to_owned();
        let revision = change.revision;
        let recorded_at_ms = now_ms()?;
        change = state
            .commit_and_publish(move |store| {
                let mutation = store.record_change_base(
                    &project_id,
                    &exact_change_id,
                    revision,
                    &base_oid,
                    &base_identity,
                    recorded_at_ms,
                )?;
                let (change, events) = mutation.into_parts();
                Ok((change, events))
            })
            .await?;
        change_source::activate_checkpoint(&invocation.selection_activation_path)?;
        selection
    } else if change.phase == ChangePhase::Available {
        match change_source::read_selection_record(&invocation.selection_record_path) {
            Ok(selection) => change_source::remove_selection_record(
                &invocation.selection_record_path,
                &selection,
            )?,
            Err(change_source::Error::Io { source, .. })
                if source.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        change_source::activate_checkpoint(&invocation.provider_activation_path)?;
        return Ok(None);
    } else {
        return Err(DaemonStateError::Store(StoreError::InvalidChangeState).into());
    };

    let ready = match wait_for_ready_record(&invocation.ready_record_path, subscription).await? {
        Checkpoint::Record(ready) => ready,
        Checkpoint::Exit(exit) => {
            record_provisioning_exit(state, &change, &exit).await?;
            return Ok(Some(exit));
        }
    };
    if ready.base_oid != selection.base_oid {
        return Err(DaemonStateError::Store(StoreError::ChangeIdentityMismatch).into());
    }
    let expected = DirectoryIdentity {
        device: ready.device,
        inode: ready.inode,
    };
    let changes_root = invocation.changes_root.clone();
    let change_uuid = invocation.change_id;
    let limits = invocation.limits;
    let measured = tokio::task::spawn_blocking(move || {
        change_source::measure_quiescent_source(&changes_root, change_uuid, expected, limits)
    })
    .await??;
    if measured.logical_bytes != ready.size {
        return Err(DaemonStateError::Store(StoreError::ChangeIdentityMismatch).into());
    }
    let source_identity = ChangeSourceIdentity {
        source_root: run.source_root.clone(),
        device: ready.device,
        inode: ready.inode,
        size_bytes: measured.allocated_bytes,
    };
    let base_identity = ChangeBaseIdentity {
        repository_root: selection.repository_root.to_string_lossy().into_owned(),
        device: selection.repository_device,
        inode: selection.repository_inode,
    };
    let project_id = change.project_id.clone();
    let exact_change_id = change.id.clone();
    let revision = change.revision;
    let materialization = ChangeMaterialization {
        base_oid: selection.base_oid.as_str().to_owned(),
        base: base_identity,
        source: source_identity,
    };
    let available_at_ms = now_ms()?;
    state
        .commit_and_publish(move |store| {
            let mutation = store.mark_change_available(
                &project_id,
                &exact_change_id,
                revision,
                &materialization,
                available_at_ms,
            )?;
            Ok(((), mutation.into_parts().1))
        })
        .await?;
    change_source::remove_selection_record(&invocation.selection_record_path, &selection)?;
    change_source::activate_checkpoint(&invocation.provider_activation_path)?;
    Ok(None)
}

async fn wait_for_selection_record(
    path: &Path,
    subscription: &mut RunnerSubscription,
) -> Result<Checkpoint<change_source::SelectionRecord>, Error> {
    loop {
        match change_source::read_selection_record(path) {
            Ok(record) => return Ok(Checkpoint::Record(record)),
            Err(change_source::Error::Io { source, .. })
                if source.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        tokio::select! {
            item = subscription.next_item() => {
                if let Some(exit) = runner_stream_exit(item?) {
                    return Ok(Checkpoint::Exit(exit));
                }
            }
            () = sleep(CONNECT_RETRY) => {}
        }
    }
}

async fn wait_for_ready_record(
    path: &Path,
    subscription: &mut RunnerSubscription,
) -> Result<Checkpoint<change_source::ReadyRecord>, Error> {
    loop {
        match change_source::read_ready_record(path) {
            Ok(record) => return Ok(Checkpoint::Record(record)),
            Err(change_source::Error::Io { source, .. })
                if source.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        tokio::select! {
            item = subscription.next_item() => {
                if let Some(exit) = runner_stream_exit(item?) {
                    return Ok(Checkpoint::Exit(exit));
                }
            }
            () = sleep(CONNECT_RETRY) => {}
        }
    }
}

async fn record_provisioning_exit(
    state: &DaemonState,
    change: &Change,
    exit: &ObservedExit,
) -> Result<(), Error> {
    let failure = format!(
        "materializer exited before source activation (code {:?}, signal {:?})",
        exit.exit_code, exit.exit_signal
    );
    let project_id = change.project_id.clone();
    let change_id = change.id.clone();
    let revision = change.revision;
    let failed_at_ms = now_ms()?;
    state
        .commit_and_publish(move |store| {
            let mutation = store.record_change_failure(
                &project_id,
                &change_id,
                revision,
                &failure,
                failed_at_ms,
            )?;
            Ok(((), mutation.into_parts().1))
        })
        .await?;
    Ok(())
}

struct ObservedExit {
    terminal_sequence: i64,
    exit_code: Option<i32>,
    exit_signal: Option<i32>,
    failure_reason: Option<RunFailureReason>,
}

async fn cleanup_unactivated(
    state: &DaemonState,
    run: &RecoverableKernelRun,
    mut child: Option<Child>,
) {
    if let Some(child) = child.as_mut() {
        if let Some(pid) = child.id().and_then(|value| Pid::from_raw(value as i32)) {
            let _ = kill_process_group(pid, Signal::KILL);
        }
        let _ = child.wait().await;
    }
    let run_id = run.run.id.clone();
    if let Ok(at_ms) = now_ms() {
        let _ = state
            .commit_and_publish(move |store| {
                let (_, events) =
                    store.fail_admitted_run(&run_id, RunFailureReason::Spawn, at_ms)?;
                Ok(((), events))
            })
            .await;
    }
    if let Ok(resources) = state
        .with_store({
            let run_id = run.run.id.clone();
            move |store| store.kernel_resources(&run_id)
        })
        .await
    {
        for resource in resources.iter().filter(|resource| {
            resource.kind == KernelResourceKind::RunnerProcess
                && resource.birth_fingerprint.is_none()
                && resource.state != KernelResourceState::Released
        }) {
            let _ = release_resource(state, resource).await;
        }
    }
    let _ = release_completed_resources(state, run).await;
}

async fn release_completed_resources(
    state: &DaemonState,
    run: &RecoverableKernelRun,
) -> Result<(), Error> {
    let refreshed = state
        .with_store({
            let run_id = run.run.id.clone();
            move |store| {
                let recovered = store
                    .recoverable_kernel_runs()?
                    .into_iter()
                    .find(|candidate| candidate.run.id == run_id)
                    .ok_or(StoreError::RunNotFound)?;
                Ok(recovered)
            }
        })
        .await?;
    let _ = release_absent_resources(state, &refreshed).await?;
    Ok(())
}

async fn release_absent_resources(
    state: &DaemonState,
    run: &RecoverableKernelRun,
) -> Result<bool, Error> {
    for resource in &run.resources {
        if resource.state == KernelResourceState::Released {
            continue;
        }
        let absent = match resource.kind {
            KernelResourceKind::RunnerProcess
            | KernelResourceKind::ProviderProcess
            | KernelResourceKind::EffectProcess => process_resource_absent(resource)?,
            KernelResourceKind::ProcessGroup | KernelResourceKind::EffectGroup => {
                process_group_absent(resource)?
            }
            _ => false,
        };
        if absent {
            release_resource(state, resource).await?;
        }
    }
    let resources = state
        .with_store({
            let run_id = run.run.id.clone();
            move |store| store.kernel_resources(&run_id)
        })
        .await?;
    let processes_released = resources
        .iter()
        .filter(|resource| {
            matches!(
                resource.kind,
                KernelResourceKind::RunnerProcess
                    | KernelResourceKind::ProviderProcess
                    | KernelResourceKind::ProcessGroup
            )
        })
        .all(|resource| resource.state == KernelResourceState::Released);
    let runner_released = resources.iter().any(|resource| {
        resource.kind == KernelResourceKind::RunnerProcess
            && resource.state == KernelResourceState::Released
    });
    if runner_released && run.run.phase == RunPhase::Running {
        let run_id = run.run.id.clone();
        let failed_at_ms = now_ms()?;
        state
            .commit_and_publish(move |store| {
                let (_, events) =
                    store.fail_running_run(&run_id, RunFailureReason::Process, failed_at_ms)?;
                Ok(((), events))
            })
            .await?;
        kill_registered_group(&resources)?;
    } else if runner_released && run.run.phase == RunPhase::Finalizing {
        kill_registered_group(&resources)?;
    }
    if processes_released && run.run.phase == RunPhase::Finalizing {
        for resource in resources.iter().filter(|resource| {
            resource.kind == KernelResourceKind::RuntimeRoot
                && resource.state != KernelResourceState::Released
        }) {
            release_registered_runtime(state, run, resource).await?;
        }
        let completion = state
            .with_store({
                let run_id = run.run.id.clone();
                move |store| store.rust_completion_check(&run_id)
            })
            .await?;
        if !rust_completion_allows_finalization(completion.as_ref()) {
            // Every provider resource is already gone. The verifier is now
            // the only owner with work to do, so a dead runner socket must
            // not manufacture another observer or a process failure.
            return Ok(true);
        }
        if completion.is_some() {
            if let Some(effect) = resources.iter().find(|resource| {
                matches!(
                    resource.kind,
                    KernelResourceKind::EffectProcess | KernelResourceKind::EffectGroup
                ) && resource.state != KernelResourceState::Released
            }) {
                let finish_path = locator_named_path(&effect.locator, "finish")
                    .ok_or(Error::InvalidRuntimeRoot)?;
                terminate_effect_group(state, &run.run.id, None, &finish_path).await?;
            }
            release_completion_temporary_root(state, &run.run.id).await?;
        }
        let resources = state
            .with_store({
                let run_id = run.run.id.clone();
                move |store| store.kernel_resources(&run_id)
            })
            .await?;
        if !resources
            .iter()
            .all(|resource| resource.state == KernelResourceState::Released)
        {
            return Ok(false);
        }
        let run_id = run.run.id.clone();
        let finalized_at_ms = now_ms()?;
        state
            .commit_and_publish(move |store| {
                let (_, events) = store.finalize_run(&run_id, finalized_at_ms)?;
                Ok(((), events))
            })
            .await?;
        return Ok(true);
    }
    Ok(false)
}

fn rust_completion_allows_finalization(completion: Option<&RustCompletionCheck>) -> bool {
    completion.is_none_or(|check| check.phase.is_terminal())
}

async fn release_registered_runtime(
    state: &DaemonState,
    run: &RecoverableKernelRun,
    resource: &KernelResource,
) -> Result<(), Error> {
    let Some(path) = locator_path(&resource.locator) else {
        mark_resource_unresolved(state, resource, "runtime locator is invalid").await?;
        return Ok(());
    };
    if path != Path::new(&run.runner_runtime) {
        mark_resource_unresolved(state, resource, "runtime locator does not match its run").await?;
        return Ok(());
    }
    let claim_nonce = resource
        .birth_fingerprint
        .as_deref()
        .and_then(runtime_claim_nonce);
    let quarantine = claim_nonce.map_or_else(
        || path.with_file_name(format!(".finalize-{}", run.run.id.as_str())),
        |nonce| path.with_file_name(format!(".finalize-{nonce}")),
    );
    let removal = if let Some(nonce) = claim_nonce {
        remove_runtime_if_claimed(&path, &quarantine, nonce)
    } else {
        remove_runtime_if_exact(&path, &quarantine, resource.birth_fingerprint.as_deref())
    };
    match removal {
        Ok(RuntimeRemoval::Missing | RuntimeRemoval::Removed) => {
            release_resource(state, resource).await
        }
        Ok(RuntimeRemoval::Unproven) if resource.birth_fingerprint.is_none() => {
            mark_resource_unresolved(
                state,
                resource,
                "declared runtime exists without a durable birth fingerprint",
            )
            .await
        }
        Ok(RuntimeRemoval::Unproven) => {
            mark_resource_unresolved(state, resource, "runtime birth fingerprint changed").await
        }
        Err(error) => mark_resource_unresolved(state, resource, &error.to_string()).await,
    }
}

fn runtime_claim_nonce(fingerprint: &str) -> Option<&str> {
    let nonce = fingerprint.strip_prefix("runtime-claim:")?;
    let parsed = Uuid::parse_str(nonce).ok()?;
    (parsed.simple().to_string() == nonce).then_some(nonce)
}

fn remove_runtime_if_claimed(
    path: &Path,
    quarantine: &Path,
    nonce: &str,
) -> Result<RuntimeRemoval, Error> {
    let expected_quarantine = format!(".finalize-{nonce}");
    if path.file_name().and_then(|name| name.to_str()) != Some(nonce)
        || quarantine.file_name().and_then(|name| name.to_str())
            != Some(expected_quarantine.as_str())
    {
        return Ok(RuntimeRemoval::Unproven);
    }
    let current = match runtime_birth_fingerprint(path)? {
        Some(fingerprint) => Some(fingerprint),
        None => runtime_birth_fingerprint(quarantine)?,
    };
    let Some(current) = current else {
        return Ok(RuntimeRemoval::Missing);
    };
    remove_runtime_if_exact(path, quarantine, Some(&current))
}

#[cfg(target_os = "linux")]
fn kill_registered_group(resources: &[KernelResource]) -> Result<(), Error> {
    for group in registered_process_groups(resources) {
        let Some(pgid) = locator_number(&group.locator, "pgid") else {
            continue;
        };
        // A reused process-group number must never authorize a signal. The
        // group leader's birth fingerprint is the durable proof that this is
        // still the group factoryd registered before execution began.
        if process_birth_fingerprint(pgid)?.as_deref() != group.birth_fingerprint.as_deref() {
            continue;
        }
        let Some(pid) = Pid::from_raw(pgid as i32) else {
            continue;
        };
        match kill_process_group(pid, Signal::KILL) {
            Ok(()) | Err(rustix::io::Errno::SRCH) => {}
            Err(error) => {
                return Err(Error::Runtime {
                    path: PathBuf::from(format!("process-group:{pgid}")),
                    source: io::Error::from_raw_os_error(error.raw_os_error()),
                });
            }
        }
    }
    Ok(())
}

#[cfg(any(target_os = "linux", test))]
fn registered_process_groups(
    resources: &[KernelResource],
) -> impl Iterator<Item = &KernelResource> {
    resources.iter().filter(|resource| {
        resource.kind == KernelResourceKind::ProcessGroup
            && resource.state != KernelResourceState::Released
    })
}

#[cfg(not(target_os = "linux"))]
fn kill_registered_group(_resources: &[KernelResource]) -> Result<(), Error> {
    // macOS exposes only second-resolution process start time through the
    // safe APIs available here. That is sufficient to remain unresolved,
    // never to authorize a destructive signal across a check/kill race.
    Ok(())
}

#[cfg(target_os = "linux")]
fn kill_registered_processes(resources: &[KernelResource]) -> Result<(), Error> {
    kill_registered_group(resources)?;
    let Some(runner) = resources.iter().find(|resource| {
        resource.kind == KernelResourceKind::RunnerProcess
            && resource.state != KernelResourceState::Released
    }) else {
        return Ok(());
    };
    let Some(pid_number) = locator_number(&runner.locator, "pid") else {
        return Ok(());
    };
    if process_birth_fingerprint(pid_number)?.as_deref() != runner.birth_fingerprint.as_deref() {
        return Ok(());
    }
    let Some(pid) = Pid::from_raw(pid_number as i32) else {
        return Ok(());
    };
    match kill_process_group(pid, Signal::KILL) {
        Ok(()) | Err(rustix::io::Errno::SRCH) => Ok(()),
        Err(error) => Err(Error::Runtime {
            path: PathBuf::from(format!("runner-process-group:{pid_number}")),
            source: io::Error::from_raw_os_error(error.raw_os_error()),
        }),
    }
}

#[cfg(not(target_os = "linux"))]
fn kill_registered_processes(_resources: &[KernelResource]) -> Result<(), Error> {
    // Finalization first uses the authenticated runner socket. If that fails,
    // weak PID metadata cannot authorize a fallback signal; the durable
    // resources remain unresolved until absence can be established.
    Ok(())
}

async fn release_resource(state: &DaemonState, resource: &KernelResource) -> Result<(), Error> {
    let id = resource.id.clone();
    let locator = resource.locator.clone();
    let fingerprint = resource.birth_fingerprint.clone();
    let at_ms = now_ms()?;
    state
        .commit_and_publish(move |store| {
            store.mark_resource_released(&id, &locator, fingerprint.as_deref(), at_ms)?;
            Ok(((), Vec::new()))
        })
        .await?;
    Ok(())
}

async fn mark_runner_unresolved(state: &DaemonState, run: &RecoverableKernelRun, failure: &str) {
    let Some(resource) = run
        .resources
        .iter()
        .find(|resource| resource.kind == KernelResourceKind::RunnerProcess)
    else {
        return;
    };
    let id = resource.id.clone();
    let failure: String = failure.chars().take(4096).collect();
    if let Ok(at_ms) = now_ms() {
        let _ = state
            .commit_and_publish(move |store| {
                store.mark_resource_unresolved(&id, &failure, at_ms)?;
                Ok(((), Vec::new()))
            })
            .await;
    }
}

async fn mark_resource_unresolved(
    state: &DaemonState,
    resource: &KernelResource,
    failure: &str,
) -> Result<(), Error> {
    let id = resource.id.clone();
    let failure: String = failure.chars().take(4096).collect();
    let at_ms = now_ms()?;
    state
        .commit_and_publish(move |store| {
            store.mark_resource_unresolved(&id, &failure, at_ms)?;
            Ok(((), Vec::new()))
        })
        .await?;
    Ok(())
}

async fn wait_for_registered_runner_exit(run: &RecoverableKernelRun) -> Result<(), Error> {
    let Some(resource) = run
        .resources
        .iter()
        .find(|resource| resource.kind == KernelResourceKind::RunnerProcess)
    else {
        return Ok(());
    };
    let deadline = Instant::now() + RUNNER_EXIT_GRACE;
    loop {
        if process_resource_absent(resource)? {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(Error::Runner(RunnerClientError::TimedOut {
                operation: "registered runner exit",
            }));
        }
        sleep(CONNECT_RETRY).await;
    }
}

fn process_resource_absent(resource: &KernelResource) -> Result<bool, Error> {
    let Some(pid) = locator_number(&resource.locator, "pid") else {
        return Ok(false);
    };
    let current = process_birth_fingerprint(pid)?;
    if current.is_none() {
        return Ok(true);
    }
    #[cfg(target_os = "linux")]
    return Ok(resource
        .birth_fingerprint
        .as_deref()
        .is_some_and(|expected| current.as_deref() != Some(expected)));
    #[cfg(not(target_os = "linux"))]
    Ok(false)
}

fn process_group_absent(resource: &KernelResource) -> Result<bool, Error> {
    let Some(pgid) = locator_number(&resource.locator, "pgid") else {
        return Ok(false);
    };
    let Some(pid) = Pid::from_raw(pgid as i32) else {
        return Ok(true);
    };
    match test_kill_process_group(pid) {
        Ok(()) | Err(rustix::io::Errno::PERM) => {
            #[cfg(not(target_os = "linux"))]
            return Ok(false);
            #[cfg(target_os = "linux")]
            let current = process_birth_fingerprint(pgid)?;
            #[cfg(target_os = "linux")]
            Ok(current.is_some() && current.as_deref() != resource.birth_fingerprint.as_deref())
        }
        Err(rustix::io::Errno::SRCH) => Ok(true),
        Err(error) => Err(Error::Runtime {
            path: PathBuf::from(format!("process-group:{pgid}")),
            source: io::Error::from_raw_os_error(error.raw_os_error()),
        }),
    }
}

fn locator_number(locator: &str, key: &str) -> Option<u32> {
    serde_json::from_str::<serde_json::Value>(locator)
        .ok()?
        .get(key)?
        .as_u64()?
        .try_into()
        .ok()
}

fn locator_path(locator: &str) -> Option<PathBuf> {
    serde_json::from_str::<serde_json::Value>(locator)
        .ok()?
        .get("path")?
        .as_str()
        .map(PathBuf::from)
}

fn locator_named_path(locator: &str, key: &str) -> Option<PathBuf> {
    serde_json::from_str::<serde_json::Value>(locator)
        .ok()?
        .get(key)?
        .as_str()
        .map(PathBuf::from)
}

fn runtime_locator(path: &Path) -> String {
    serde_json::json!({ "path": path }).to_string()
}

fn runner_locator(pid: u32, runner_instance_id: &RunnerInstanceId) -> String {
    serde_json::json!({
        "pid": pid,
        "runner_instance_id": runner_instance_id.as_str(),
    })
    .to_string()
}

fn runtime_birth_fingerprint(path: &Path) -> Result<Option<String>, Error> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            return Err(Error::Runtime {
                path: path.to_path_buf(),
                source,
            });
        }
    };
    verify_private_directory_metadata(&metadata)?;
    Ok(Some(format!(
        "unix-device:{}:inode:{}",
        metadata.dev(),
        metadata.ino()
    )))
}

#[cfg(target_os = "linux")]
fn process_birth_fingerprint(pid: u32) -> Result<Option<String>, Error> {
    let path = PathBuf::from(format!("/proc/{pid}/stat"));
    let stat = match fs::read_to_string(&path) {
        Ok(stat) => stat,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(source) => return Err(Error::Runtime { path, source }),
    };
    let Some(after_name) = stat.rsplit_once(") ").map(|(_, fields)| fields) else {
        return Err(Error::ProcessIdentityUnavailable(pid));
    };
    let Some(start_ticks) = after_name.split_whitespace().nth(19) else {
        return Err(Error::ProcessIdentityUnavailable(pid));
    };
    Ok(Some(format!("linux-start-ticks:{start_ticks}")))
}

#[cfg(not(target_os = "linux"))]
fn process_birth_fingerprint(pid: u32) -> Result<Option<String>, Error> {
    let Some(pid) = Pid::from_raw(pid as i32) else {
        return Ok(None);
    };
    match test_kill_process(pid) {
        Ok(()) | Err(rustix::io::Errno::PERM) => Ok(Some("weak-presence-only".to_owned())),
        Err(rustix::io::Errno::SRCH) => Ok(None),
        Err(error) => Err(Error::Runtime {
            path: PathBuf::from(format!("process:{pid}")),
            source: io::Error::from_raw_os_error(error.raw_os_error()),
        }),
    }
}

fn remove_runtime(path: &Path) -> Result<(), Error> {
    match fs::remove_dir_all(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(Error::Runtime {
            path: path.to_path_buf(),
            source,
        }),
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RuntimeRemoval {
    Missing,
    Removed,
    Unproven,
}

fn remove_runtime_if_exact(
    path: &Path,
    quarantine: &Path,
    expected_birth_fingerprint: Option<&str>,
) -> Result<RuntimeRemoval, Error> {
    if quarantine.parent() != path.parent() || quarantine == path {
        return Ok(RuntimeRemoval::Unproven);
    }
    let parent = path.parent().ok_or(Error::InvalidRuntimeRoot)?;
    let parent_metadata = fs::symlink_metadata(parent).map_err(|source| Error::Runtime {
        path: parent.to_path_buf(),
        source,
    })?;
    verify_private_directory_metadata(&parent_metadata)?;

    if let Some(quarantined) = runtime_birth_fingerprint(quarantine)? {
        if runtime_birth_fingerprint(path)?.is_some()
            || expected_birth_fingerprint != Some(quarantined.as_str())
        {
            return Ok(RuntimeRemoval::Unproven);
        }
        remove_runtime(quarantine)?;
        return Ok(if runtime_birth_fingerprint(path)?.is_none() {
            RuntimeRemoval::Removed
        } else {
            RuntimeRemoval::Unproven
        });
    }

    let Some(current) = runtime_birth_fingerprint(path)? else {
        return Ok(RuntimeRemoval::Missing);
    };
    if expected_birth_fingerprint != Some(current.as_str()) {
        return Ok(RuntimeRemoval::Unproven);
    }
    match fs::rename(path, quarantine) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(RuntimeRemoval::Missing);
        }
        Err(source) => {
            return Err(Error::Runtime {
                path: path.to_path_buf(),
                source,
            });
        }
    }
    if runtime_birth_fingerprint(path)?.is_some()
        || runtime_birth_fingerprint(quarantine)?.as_deref() != expected_birth_fingerprint
    {
        return Ok(RuntimeRemoval::Unproven);
    }
    remove_runtime(quarantine)?;
    Ok(if runtime_birth_fingerprint(path)?.is_none() {
        RuntimeRemoval::Removed
    } else {
        RuntimeRemoval::Unproven
    })
}

fn random_bearer() -> String {
    format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple())
}

fn capability_digest(bearer: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bearer.as_bytes());
    format!("{:x}", hasher.finalize())
}

fn new_run_id() -> Result<RunId, Error> {
    RunId::try_from(Uuid::new_v4().hyphenated().to_string()).map_err(|_| Error::InvalidId)
}

fn new_change_id() -> Result<ChangeId, Error> {
    ChangeId::try_from(Uuid::new_v4().hyphenated().to_string()).map_err(|_| Error::InvalidId)
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

fn prepare_runtime_root(path: &Path) -> Result<(), Error> {
    if !path.is_absolute() {
        return Err(Error::InvalidRuntimeRoot);
    }
    let parent = path.parent().ok_or(Error::InvalidRuntimeRoot)?;
    ensure_private_directory(parent)?;
    ensure_private_directory(path)
}

fn ensure_private_directory(path: &Path) -> Result<(), Error> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => verify_private_directory_metadata(&metadata),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            fs::DirBuilder::new()
                .recursive(true)
                .mode(PRIVATE_DIRECTORY_MODE)
                .create(path)
                .map_err(|_| Error::InvalidRuntimeRoot)?;
            let metadata = fs::symlink_metadata(path).map_err(|_| Error::InvalidRuntimeRoot)?;
            verify_private_directory_metadata(&metadata)
        }
        Err(_) => Err(Error::InvalidRuntimeRoot),
    }
}

fn verify_private_directory_metadata(metadata: &fs::Metadata) -> Result<(), Error> {
    if metadata.file_type().is_symlink()
        || !metadata.is_dir()
        || metadata.uid() != rustix::process::geteuid().as_raw()
        || metadata.mode() & 0o777 != PRIVATE_DIRECTORY_MODE
    {
        Err(Error::InvalidRuntimeRoot)
    } else {
        Ok(())
    }
}

struct DeleteGate<Id: Eq + std::hash::Hash + Clone> {
    state: Mutex<HashMap<Id, GateEntry>>,
}

#[derive(Default)]
struct GateEntry {
    deleting: bool,
    writers: u32,
}

impl<Id: Eq + std::hash::Hash + Clone> DeleteGate<Id> {
    fn new() -> Self {
        Self {
            state: Mutex::new(HashMap::new()),
        }
    }

    fn try_begin_write(&self, id: &Id) -> bool {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let entry = state.entry(id.clone()).or_default();
        if entry.deleting {
            return false;
        }
        entry.writers = entry.writers.saturating_add(1);
        true
    }

    fn end_write(&self, id: &Id) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(entry) = state.get_mut(id) {
            entry.writers = entry.writers.saturating_sub(1);
        }
    }

    fn begin_delete(&self, id: &Id) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.entry(id.clone()).or_default().deleting = true;
    }

    fn end_delete(&self, id: &Id) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(entry) = state.get_mut(id) {
            entry.deleting = false;
        }
    }

    async fn wait_for_drain(&self, id: &Id, budget: Duration) -> bool {
        let deadline = Instant::now() + budget;
        loop {
            let drained = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .get(id)
                .is_none_or(|entry| entry.writers == 0);
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};

    fn private_tempdir() -> tempfile::TempDir {
        let directory = tempfile::tempdir_in("/tmp").unwrap();
        fs::set_permissions(
            directory.path(),
            fs::Permissions::from_mode(PRIVATE_DIRECTORY_MODE),
        )
        .unwrap();
        directory
    }

    fn rust_check(phase: RustCompletionPhase) -> RustCompletionCheck {
        RustCompletionCheck {
            run_id: RunId::try_from("22222222-2222-4222-8222-222222222222").unwrap(),
            project_id: ProjectId::try_from("project").unwrap(),
            project_incarnation_id: "incarnation".to_owned(),
            change_id: ChangeId::try_from("change").unwrap(),
            phase,
            cache_key: None,
            source_digest: None,
            bundle_digest: None,
            failure: None,
            revision: 0,
            requested_at_ms: 1,
            updated_at_ms: 1,
            terminal_at_ms: None,
        }
    }

    fn execution_config(root: &Path) -> Config {
        Config {
            factoryd_program: PathBuf::from("/bin/factoryd"),
            runner_program: PathBuf::from("/bin/factory-runner"),
            factoryctl_path: PathBuf::from("/bin/factoryctl"),
            git_program: PathBuf::from("/usr/bin/git"),
            cargo_program: Some(PathBuf::from("/usr/bin/cargo")),
            runtime_root: root.join("runs"),
            changes_root: root.join("changes"),
            artifacts_root: root.join("artifacts"),
            guidance_root: root.join("guidance"),
            socket_path: root.join("factory.sock"),
            max_active_runs: 1,
        }
    }

    #[test]
    fn bearer_is_lowercase_hex_with_the_store_digest_shape() {
        let bearer = random_bearer();
        assert_eq!(bearer.len(), 64);
        assert!(bearer.bytes().all(|byte| byte.is_ascii_hexdigit()));
        assert_eq!(capability_digest(&bearer).len(), 64);
    }

    #[test]
    fn both_attempt_and_change_capacity_pause_only_the_current_dispatch() {
        for error in [
            StoreError::CapacityReached { limit: 8 },
            StoreError::ChangeCapacityReached { limit: 64 },
        ] {
            assert!(is_admission_capacity(&Error::State(
                DaemonStateError::Store(error)
            )));
        }
        assert!(!is_admission_capacity(&Error::InvalidId));
    }

    #[test]
    fn rust_maintenance_runs_pending_storage_before_more_completions() {
        let mut maintenance = RustMaintenance::new();
        let run_id = RunId::try_from("run-1").unwrap();
        maintenance.schedule(run_id.clone());
        maintenance.schedule_storage();

        assert!(matches!(
            maintenance.take_next(),
            Some(RustMaintenanceWork::Storage)
        ));
        assert!(matches!(
            maintenance.take_next(),
            Some(RustMaintenanceWork::Completion(next)) if next == run_id
        ));
    }

    #[tokio::test]
    async fn delete_gate_refuses_new_writes_and_drains_the_exact_identity() {
        let gate = DeleteGate::new();
        let id = AgentId::try_from("worker").unwrap();
        assert!(gate.try_begin_write(&id));
        gate.begin_delete(&id);
        assert!(!gate.try_begin_write(&id));
        assert!(!gate.wait_for_drain(&id, Duration::ZERO).await);
        gate.end_write(&id);
        assert!(gate.wait_for_drain(&id, Duration::ZERO).await);
    }

    #[test]
    fn pid_locator_is_typed_json_not_display_text() {
        let locator = serde_json::json!({ "pid": 42 }).to_string();
        assert_eq!(locator_number(&locator, "pid"), Some(42));
        assert_eq!(locator_number("pid 42", "pid"), None);
    }

    #[test]
    fn finalizer_selects_every_unreleased_process_group() {
        let run_id = RunId::try_from("run-1").unwrap();
        let resource = |id: &str, kind, state| KernelResource {
            id: id.to_owned(),
            run_id: run_id.clone(),
            kind,
            state,
            locator: serde_json::json!({ "pgid": 42 }).to_string(),
            birth_fingerprint: Some("fingerprint".to_owned()),
            retry_count: 0,
            last_failure: None,
            declared_at_ms: 1,
            updated_at_ms: 1,
            released_at_ms: None,
        };
        let resources = vec![
            resource(
                "provider-group",
                KernelResourceKind::ProcessGroup,
                KernelResourceState::Releasing,
            ),
            resource(
                "secondary-provider-group",
                KernelResourceKind::ProcessGroup,
                KernelResourceState::Active,
            ),
            resource(
                "released-group",
                KernelResourceKind::ProcessGroup,
                KernelResourceState::Released,
            ),
            resource(
                "runtime",
                KernelResourceKind::RuntimeRoot,
                KernelResourceState::Releasing,
            ),
        ];

        assert_eq!(
            registered_process_groups(&resources)
                .map(|resource| resource.id.as_str())
                .collect::<Vec<_>>(),
            ["provider-group", "secondary-provider-group"]
        );
    }

    #[test]
    fn completion_waits_for_every_initial_attempt_resource() {
        let run_id = RunId::try_from("run-1").unwrap();
        let resource = |id: &str, kind, state| KernelResource {
            id: id.to_owned(),
            run_id: run_id.clone(),
            kind,
            state,
            locator: "{}".to_owned(),
            birth_fingerprint: Some("fingerprint".to_owned()),
            retry_count: 0,
            last_failure: None,
            declared_at_ms: 1,
            updated_at_ms: 1,
            released_at_ms: (state == KernelResourceState::Released).then_some(1),
        };
        let mut resources = vec![
            resource(
                "runner",
                KernelResourceKind::RunnerProcess,
                KernelResourceState::Released,
            ),
            resource(
                "provider",
                KernelResourceKind::ProviderProcess,
                KernelResourceState::Released,
            ),
            resource(
                "group",
                KernelResourceKind::ProcessGroup,
                KernelResourceState::Released,
            ),
            resource(
                "runtime",
                KernelResourceKind::RuntimeRoot,
                KernelResourceState::Released,
            ),
        ];
        assert!(initial_attempt_resources_released(&resources));
        resources[2].state = KernelResourceState::Releasing;
        resources[2].released_at_ms = None;
        assert!(!initial_attempt_resources_released(&resources));

        // Completion-owned effects are deliberately outside this gate: they
        // can only exist after the initial attempt resources have drained.
        resources.push(resource(
            "effect",
            KernelResourceKind::EffectProcess,
            KernelResourceState::Active,
        ));
        resources[2].state = KernelResourceState::Released;
        resources[2].released_at_ms = Some(1);
        assert!(initial_attempt_resources_released(&resources));
    }

    #[tokio::test]
    async fn rust_verifier_wait_has_a_fixed_deadline() {
        let directory = private_tempdir();
        let result = directory.path().join("result.json");
        let mut child = tokio::process::Command::new("/bin/sleep")
            .arg("30")
            .spawn()
            .unwrap();

        let error = wait_for_worker_result(&mut child, &result, Duration::ZERO)
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            Error::Runner(RunnerClientError::TimedOut {
                operation: "Rust completion verification"
            })
        ));
        child.kill().await.unwrap();
        child.wait().await.unwrap();
    }

    #[test]
    fn declared_cache_recovery_adopts_only_its_empty_private_claim() {
        let directory = private_tempdir();
        let cache = directory.path().join("cache");
        let first = bindable_declared_cache(&cache).unwrap();
        assert_eq!(first.path, cache);
        let recovered = bindable_declared_cache(&cache).unwrap();
        assert_eq!(recovered.device, first.device);
        assert_eq!(recovered.inode, first.inode);

        fs::write(cache.join("unregistered-effect"), b"contents").unwrap();
        assert!(matches!(
            bindable_declared_cache(&cache),
            Err(Error::State(DaemonStateError::Store(
                StoreError::InvalidRustBuildMetadata
            )))
        ));
    }

    #[test]
    fn completion_temporary_root_is_deterministic_and_declared_recovery_requires_empty() {
        let directory = private_tempdir();
        let check = rust_check(RustCompletionPhase::Pending);
        let config = execution_config(directory.path());

        let first = completion_temporary_spec(&config, &check).unwrap();
        let second = completion_temporary_spec(&config, &check).unwrap();
        assert_eq!(first.path, second.path);
        assert_eq!(
            first.path.file_name().and_then(|name| name.to_str()),
            Some("22222222222242228222222222222222")
        );
        ensure_empty_private_directory(&first.path).unwrap();
        fs::write(first.path.join("unregistered-effect"), b"contents").unwrap();
        assert!(matches!(
            ensure_empty_private_directory(&first.path),
            Err(Error::State(DaemonStateError::Store(
                StoreError::InvalidExecutionMetadata
            )))
        ));
    }

    #[test]
    fn running_recovery_refuses_a_replacement_temporary_root() {
        let directory = private_tempdir();
        let check = rust_check(RustCompletionPhase::Running);
        let config = execution_config(directory.path());
        let spec = completion_temporary_spec(&config, &check).unwrap();
        ensure_empty_private_directory(&spec.path).unwrap();
        let fingerprint = runtime_birth_fingerprint(&spec.path).unwrap().unwrap();
        let resource = KernelResource {
            id: spec.resource_id.clone(),
            run_id: check.run_id.clone(),
            kind: KernelResourceKind::TemporaryRoot,
            state: KernelResourceState::Active,
            locator: spec.locator.clone(),
            birth_fingerprint: Some(fingerprint),
            retry_count: 0,
            last_failure: None,
            declared_at_ms: 1,
            updated_at_ms: 1,
            released_at_ms: None,
        };
        validate_active_completion_temporary(spec, &resource).unwrap();

        let replacement = completion_temporary_spec(&config, &check).unwrap();
        fs::rename(
            &replacement.path,
            replacement.path.with_extension("original"),
        )
        .unwrap();
        ensure_empty_private_directory(&replacement.path).unwrap();
        assert!(matches!(
            validate_active_completion_temporary(replacement, &resource),
            Err(Error::State(DaemonStateError::Store(
                StoreError::ResourceIdentityMismatch
            )))
        ));
    }

    #[test]
    fn running_recovery_routes_a_prior_effect_before_a_replacement_source() {
        let directory = private_tempdir();
        let source = directory.path().join("change");
        fs::DirBuilder::new()
            .mode(PRIVATE_DIRECTORY_MODE)
            .create(&source)
            .unwrap();
        let selected_file = source.join("selected");
        fs::write(&selected_file, b"A\n").unwrap();
        let selected_identity = rust_verify::exact_directory_identity(&source).unwrap();

        // This is the crash boundary exercised by the external smoke: the
        // first verifier selected A, then the retained Change was replaced
        // before daemon recovery. Prior effect evidence routes recovery to
        // reaping/result handling before the replacement Change is consulted.
        fs::rename(&source, directory.path().join("selected-a")).unwrap();
        fs::DirBuilder::new()
            .mode(PRIVATE_DIRECTORY_MODE)
            .create(&source)
            .unwrap();
        fs::write(source.join("selected"), b"B\n").unwrap();
        assert_ne!(
            rust_verify::exact_directory_identity(&source).unwrap(),
            selected_identity
        );

        let run_id = RunId::try_from("run-1").unwrap();
        let resource = |id: &str, kind| KernelResource {
            id: id.to_owned(),
            run_id: run_id.clone(),
            kind,
            state: KernelResourceState::Active,
            locator: "{}".to_owned(),
            birth_fingerprint: Some("first-verifier".to_owned()),
            retry_count: 0,
            last_failure: None,
            declared_at_ms: 1,
            updated_at_ms: 2,
            released_at_ms: None,
        };
        let prior_effect = vec![
            resource("effect", KernelResourceKind::EffectProcess),
            resource("effect-group", KernelResourceKind::EffectGroup),
        ];

        assert!(rust_effect_was_attempted(&prior_effect));
        let released_effect = prior_effect
            .into_iter()
            .map(|mut resource| {
                resource.state = KernelResourceState::Released;
                resource
            })
            .collect::<Vec<_>>();
        assert!(
            rust_effect_was_attempted(&released_effect),
            "durable evidence of any prior verifier must prevent setup replay"
        );
        assert!(!rust_effect_was_attempted(&[]));
    }

    #[test]
    fn verifier_group_kill_requires_the_exact_live_leader() {
        assert!(exact_effect_group_leader(Some("birth-a"), Some("birth-a")));
        assert!(!exact_effect_group_leader(Some("birth-a"), None));
        assert!(!exact_effect_group_leader(Some("birth-a"), Some("birth-b")));
        assert!(!exact_effect_group_leader(None, None));
    }

    #[test]
    fn pending_rust_completion_prevents_premature_run_finalization() {
        assert!(!rust_completion_allows_finalization(Some(&rust_check(
            RustCompletionPhase::Pending
        ))));
        assert!(!rust_completion_allows_finalization(Some(&rust_check(
            RustCompletionPhase::Running
        ))));
        assert!(rust_completion_allows_finalization(Some(&rust_check(
            RustCompletionPhase::Passed
        ))));
        assert!(rust_completion_allows_finalization(None));
    }

    #[test]
    fn completion_failure_is_nonempty_utf8_and_store_bounded() {
        assert_eq!(
            bounded_completion_failure(""),
            "Rust completion verification failed"
        );
        let oversized = "a".repeat(4095) + "é";
        let bounded = bounded_completion_failure(&oversized);
        assert_eq!(bounded.len(), 4095);
        assert!(bounded.is_char_boundary(bounded.len()));
        assert!(oversized.starts_with(&bounded));
    }

    #[cfg(not(target_os = "linux"))]
    #[test]
    fn weak_process_identity_mismatch_never_proves_absence() {
        let resource = KernelResource {
            id: "runner-resource".to_owned(),
            run_id: RunId::try_from("run-1").unwrap(),
            kind: KernelResourceKind::RunnerProcess,
            state: KernelResourceState::Releasing,
            locator: serde_json::json!({ "pid": std::process::id() }).to_string(),
            birth_fingerprint: Some("different-weak-fingerprint".to_owned()),
            retry_count: 0,
            last_failure: None,
            declared_at_ms: 1,
            updated_at_ms: 1,
            released_at_ms: None,
        };

        assert!(!process_resource_absent(&resource).unwrap());
    }

    #[test]
    fn runtime_removal_refuses_a_replacement_inode() {
        let parent = private_tempdir();
        let runtime = parent.path().join("runtime");
        fs::DirBuilder::new()
            .mode(PRIVATE_DIRECTORY_MODE)
            .create(&runtime)
            .unwrap();
        let original = runtime_birth_fingerprint(&runtime).unwrap().unwrap();
        fs::rename(&runtime, parent.path().join("original")).unwrap();
        fs::DirBuilder::new()
            .mode(PRIVATE_DIRECTORY_MODE)
            .create(&runtime)
            .unwrap();

        assert_eq!(
            remove_runtime_if_exact(
                &runtime,
                &parent.path().join(".finalize-test"),
                Some(&original),
            )
            .unwrap(),
            RuntimeRemoval::Unproven
        );
        assert!(runtime.is_dir(), "replacement runtime must not be deleted");
    }

    #[test]
    fn runtime_removal_accepts_exact_identity_and_absence() {
        let parent = private_tempdir();
        let runtime = parent.path().join("runtime");
        fs::DirBuilder::new()
            .mode(PRIVATE_DIRECTORY_MODE)
            .create(&runtime)
            .unwrap();
        let identity = runtime_birth_fingerprint(&runtime).unwrap().unwrap();
        assert_eq!(
            remove_runtime_if_exact(
                &runtime,
                &parent.path().join(".finalize-test"),
                Some(&identity),
            )
            .unwrap(),
            RuntimeRemoval::Removed
        );
        assert_eq!(
            remove_runtime_if_exact(
                &runtime,
                &parent.path().join(".finalize-test"),
                Some(&identity),
            )
            .unwrap(),
            RuntimeRemoval::Missing
        );
    }

    #[test]
    fn runtime_removal_recovers_an_exact_post_rename_quarantine() {
        let parent = private_tempdir();
        let runtime = parent.path().join("runtime");
        let quarantine = parent.path().join(".finalize-test");
        fs::DirBuilder::new()
            .mode(PRIVATE_DIRECTORY_MODE)
            .create(&runtime)
            .unwrap();
        let identity = runtime_birth_fingerprint(&runtime).unwrap().unwrap();
        fs::rename(&runtime, &quarantine).unwrap();

        assert_eq!(
            remove_runtime_if_exact(&runtime, &quarantine, Some(&identity)).unwrap(),
            RuntimeRemoval::Removed
        );
        assert!(!runtime.exists());
        assert!(!quarantine.exists());
    }

    #[test]
    fn durable_claim_reaps_runtime_created_before_inode_binding() {
        let parent = private_tempdir();
        let nonce = "22222222222242228222222222222222";
        let runtime = parent.path().join(nonce);
        let quarantine = parent.path().join(format!(".finalize-{nonce}"));
        fs::DirBuilder::new()
            .mode(PRIVATE_DIRECTORY_MODE)
            .create(&runtime)
            .unwrap();

        assert_eq!(
            remove_runtime_if_claimed(&runtime, &quarantine, nonce).unwrap(),
            RuntimeRemoval::Removed
        );
        assert!(!runtime.exists());
    }
}
