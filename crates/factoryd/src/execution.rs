//! Durable one-process-per-attempt execution.
//!
//! SQLite is the authority. This module only turns admitted runs into exact
//! runner processes and drives their durable finalization; it owns no shadow
//! session or delivery state.

use std::{
    collections::{HashMap, HashSet},
    fs, io,
    os::unix::{fs::DirBuilderExt, fs::MetadataExt},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};

use factory_core::{
    AgentId, AgentRole, ProjectId, Provider, RunFailureReason, RunId, RunPhase, RunnerInstanceId,
    TaskId, runner::RunnerEvent,
};
#[cfg(not(target_os = "linux"))]
use rustix::process::test_kill_process;
use rustix::process::{Pid, Signal, kill_process_group, test_kill_process_group};
use sha2::{Digest, Sha256};
use thiserror::Error;
use tokio::{
    process::Child,
    sync::{mpsc, oneshot, watch},
    task::JoinHandle,
    time::{Instant, sleep, sleep_until, timeout},
};
use uuid::Uuid;

use crate::{
    daemon_state::{DaemonState, DaemonStateError},
    providers::{self, SpawnContext, hooks},
    runner_client::{
        PreparedRunner, RunnerClient, RunnerClientError, RunnerStreamItem, RunnerSubscription,
    },
    runner_process::{self, LaunchSpec, ProviderEnvironment},
    store::{
        AdmittedRun, KernelResource, KernelResourceKind, KernelResourceState, NewRunAdmission,
        PreparedProcessIdentity, RecoverableKernelRun, StoreError,
    },
};

const PRIVATE_DIRECTORY_MODE: u32 = 0o700;
const COMMAND_CAPACITY: usize = 256;
const RECONCILE_INTERVAL: Duration = Duration::from_secs(5);
const CONNECT_GRACE: Duration = Duration::from_secs(5);
const CONNECT_RETRY: Duration = Duration::from_millis(50);
const RUNNER_EXIT_GRACE: Duration = Duration::from_secs(5);
const DEFAULT_FINALIZE_GRACE_MS: u64 = 5_000;
const DELETE_DRAIN_TIMEOUT: Duration = Duration::from_secs(5);
const DELETE_DRAIN_POLL: Duration = Duration::from_millis(50);
const STATE_PAGE: usize = 100;

pub struct Config {
    pub runner_program: PathBuf,
    pub factoryctl_path: PathBuf,
    pub runtime_root: PathBuf,
    pub guidance_root: PathBuf,
    pub socket_path: PathBuf,
    pub max_active_runs: usize,
}

pub struct StartTask {
    pub project_id: ProjectId,
    pub task_id: TaskId,
    pub agent_id: AgentId,
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
    #[error("execution request was cancelled")]
    RequestCancelled,
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
}

enum Command {
    Start {
        input: StartTask,
        reply: oneshot::Sender<Result<StartedRun, Error>>,
    },
    WakeAgent {
        project_id: ProjectId,
        agent_id: AgentId,
    },
    ReconcileRun {
        run_id: RunId,
        grace_ms: u64,
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

    pub async fn start_task(&self, input: StartTask) -> Result<StartedRun, Error> {
        let (reply, receive) = oneshot::channel();
        self.commands
            .send(Command::Start { input, reply })
            .await
            .map_err(|_| Error::ManagerStopped)?;
        receive.await.map_err(|_| Error::RequestCancelled)?
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

    pub async fn lock_assignment_slot(&self) -> tokio::sync::OwnedMutexGuard<()> {
        self.state.lock_assignment_slot().await
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
    reconcile_runs(&state, &commands, &mut observed).await?;
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
                    Command::Start { input, reply } => {
                        let result = start_and_observe(
                            Arc::clone(&config), state.clone(), commands.clone(),
                            &mut observed, Arc::clone(&agent_gate), input,
                        ).await;
                        let _ = reply.send(result);
                    }
                    Command::WakeAgent { project_id, agent_id } => {
                        if let Err(error) = dispatch_agent(
                            Arc::clone(&config), state.clone(), commands.clone(),
                            &mut observed, Arc::clone(&agent_gate), project_id, agent_id,
                        ).await {
                            tracing::warn!(%error, "attempt dispatch failed");
                        }
                    }
                    Command::ReconcileRun { run_id, grace_ms } => {
                        reconcile_one(
                            &state, &commands, &mut observed, run_id, grace_ms,
                        ).await?;
                    }
                    Command::ObserverFinished(run_id) => {
                        observed.remove(&run_id);
                    }
                }
            }
            _ = tick.tick() => {
                reconcile_runs(&state, &commands, &mut observed).await?;
                reconcile_agents(
                    Arc::clone(&config), state.clone(), commands.clone(),
                    &mut observed, Arc::clone(&agent_gate),
                ).await?;
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
    input: StartTask,
) -> Result<StartedRun, Error> {
    let agent_id = input.agent_id.clone();
    if !agent_gate.try_begin_write(&agent_id) {
        return Err(Error::DeleteInProgress);
    }
    let result = start_run(&config, &state, input).await;
    agent_gate.end_write(&agent_id);
    result.map(|started| {
        let StartedProcess { run, child } = started;
        let run_id = run.run.id.clone();
        observed.insert(run_id.clone());
        spawn_observer(state, commands, run, Some(child));
        StartedRun { run_id }
    })
}

struct StartedProcess {
    run: RecoverableKernelRun,
    child: Child,
}

async fn start_run(
    config: &Config,
    state: &DaemonState,
    input: StartTask,
) -> Result<StartedProcess, Error> {
    let run_id = new_run_id()?;
    let runner_instance_id = new_runner_instance_id()?;
    let runtime_nonce = Uuid::new_v4().simple().to_string();
    let runtime_claim = format!("runtime-claim:{runtime_nonce}");
    let runtime_dir = config.runtime_root.join(&runtime_nonce);
    let policy_dir = runtime_dir.join("policy");
    let bearer = random_bearer();
    let digest = capability_digest(&bearer);
    let lookup_project = input.project_id.clone();
    let lookup_agent = input.agent_id.clone();
    let provider = state
        .with_store(move |store| {
            Ok(store
                .agent_status(&lookup_project, &lookup_agent)?
                .agent
                .provider)
        })
        .await?;
    let admission = NewRunAdmission {
        run_id: run_id.clone(),
        project_id: input.project_id,
        task_id: input.task_id,
        agent_id: input.agent_id,
        expected_provider: provider,
        capability_digest: digest,
        runtime_claim,
        runner_instance_id: runner_instance_id.clone(),
        runner_runtime: runtime_dir.to_string_lossy().into_owned(),
        max_active_runs: config.max_active_runs,
        change_id: None,
        policy_cwd: Some(policy_dir.to_string_lossy().into_owned()),
    };
    let admitted_at_ms = now_ms()?;
    let admitted = state
        .commit_and_publish(move |store| {
            let admitted = store.admit_run(admission, admitted_at_ms)?;
            let events = admitted.events.clone();
            Ok((admitted, events))
        })
        .await?;

    match launch_admitted(config, state, admitted, bearer).await {
        Ok(started) => Ok(started),
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
    if admitted.target.role == AgentRole::Orchestrator {
        let policy_dir = PathBuf::from(&admitted.target.worktree);
        if let Err(error) = ensure_private_directory(&policy_dir) {
            return Err((error, None, recovery));
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
        worktree: PathBuf::from(&admitted.target.worktree),
        startup_input,
        model: admitted.target.model.clone(),
        reasoning_effort: admitted.target.reasoning_effort.clone(),
        permission_mode: admitted.target.permission_mode.clone(),
        auto_mode: admitted.target.auto_mode,
        hook_token_path,
        factoryctl_path: config.factoryctl_path.clone(),
        agent_dir: runtime_dir.join("provider"),
    };
    let launch = match provider.spawn_spec(&context) {
        Ok(launch) => launch,
        Err(error) => return Err((error.into(), None, recovery)),
    };
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
        cwd: PathBuf::from(&admitted.target.worktree),
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
    let lookup_project = project_id.clone();
    let lookup_agent = agent_id.clone();
    let (auto, status) = state
        .with_store(move |store| {
            Ok((
                store.auto_mode()?,
                store.agent_status(&lookup_project, &lookup_agent)?,
            ))
        })
        .await?;
    if !auto || status.agent.paused || status.current_run.is_some() {
        return Ok(());
    }
    let Some(task) = status
        .queue
        .into_iter()
        .find(|task| task.status == factory_core::TaskStatus::Queued)
    else {
        return Ok(());
    };
    let result = start_and_observe(
        config,
        state,
        commands,
        observed,
        agent_gate,
        StartTask {
            project_id,
            task_id: task.id,
            agent_id,
        },
    )
    .await;
    if let Err(Error::State(DaemonStateError::Store(
        StoreError::SourceProvisioningUnavailable | StoreError::CapacityReached { .. },
    ))) = result
    {
        return Ok(());
    }
    result.map(|_| ())
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
                    dispatch_agent(
                        Arc::clone(&config),
                        state.clone(),
                        commands.clone(),
                        observed,
                        Arc::clone(&agent_gate),
                        project.id.clone(),
                        agent.id,
                    )
                    .await?;
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
        if run.run.phase == RunPhase::Admitted {
            recover_admitted_run(state, commands, observed, run).await?;
            continue;
        }
        if release_absent_resources(state, &run).await? {
            continue;
        }
        if observed.insert(run.run.id.clone()) {
            spawn_observer(state.clone(), commands.clone(), run, None);
        }
    }
    Ok(())
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
            mark_runner_unresolved(state, run, &error.to_string()).await;
            if run.run.phase == RunPhase::Running {
                let run_id = run.run.id.clone();
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
            if matches!(run.run.phase, RunPhase::Running | RunPhase::Finalizing) {
                kill_registered_processes(&run.resources)?;
            }
            return Err(error.into());
        }
    };
    let observed = consume_until_exit(&mut subscription).await?;
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
        match subscription.next_item().await? {
            RunnerStreamItem::CaughtUp { .. } => {}
            RunnerStreamItem::Event(event) => match event.event {
                RunnerEvent::Exited { exit_code, signal } => {
                    return Ok(ObservedExit {
                        terminal_sequence: event.sequence,
                        exit_code,
                        exit_signal: signal,
                        failure_reason: None,
                    });
                }
                RunnerEvent::SpawnFailed { .. } => {
                    return Ok(ObservedExit {
                        terminal_sequence: event.sequence,
                        exit_code: None,
                        exit_signal: None,
                        failure_reason: Some(RunFailureReason::Spawn),
                    });
                }
                RunnerEvent::Started { .. } => {}
            },
        }
    }
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
            KernelResourceKind::RunnerProcess | KernelResourceKind::ProviderProcess => {
                process_resource_absent(resource)?
            }
            KernelResourceKind::ProcessGroup => process_group_absent(resource)?,
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
    let Some(group) = resources.iter().find(|resource| {
        resource.kind == KernelResourceKind::ProcessGroup
            && resource.state != KernelResourceState::Released
    }) else {
        return Ok(());
    };
    let Some(pgid) = locator_number(&group.locator, "pgid") else {
        return Ok(());
    };
    // A reused process-group number must never authorize a signal. The group
    // leader's birth fingerprint is the durable proof that this is still the
    // group factoryd registered before provider execution began.
    if process_birth_fingerprint(pgid)?.as_deref() != group.birth_fingerprint.as_deref() {
        return Ok(());
    }
    let Some(pid) = Pid::from_raw(pgid as i32) else {
        return Ok(());
    };
    match kill_process_group(pid, Signal::KILL) {
        Ok(()) | Err(rustix::io::Errno::SRCH) => Ok(()),
        Err(error) => Err(Error::Runtime {
            path: PathBuf::from(format!("process-group:{pgid}")),
            source: io::Error::from_raw_os_error(error.raw_os_error()),
        }),
    }
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

    #[test]
    fn bearer_is_lowercase_hex_with_the_store_digest_shape() {
        let bearer = random_bearer();
        assert_eq!(bearer.len(), 64);
        assert!(bearer.bytes().all(|byte| byte.is_ascii_hexdigit()));
        assert_eq!(capability_digest(&bearer).len(), 64);
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
