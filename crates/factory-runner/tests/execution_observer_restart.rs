use std::{
    fs, io,
    os::unix::fs::{MetadataExt, PermissionsExt},
    path::{Path, PathBuf},
    thread,
    time::{Duration, Instant},
};

use factory_core::{
    AgentId, AgentRole, ChangeId, ProjectId, Provider, RunFailureReason, RunId, RunOutcome,
    RunPhase, RunnerInstanceId, TaskId,
};
use factoryd::{
    daemon_state::DaemonState,
    execution, providers,
    runner_client::{PreparedRunner, RunnerClient},
    runner_process::{self, LaunchSpec, ProviderEnvironment},
    store::{
        AdmittedRun, ChangeReservation, KernelResourceKind, KernelResourceState, NewAgent,
        NewProject, NewRunAdmission, NewTask, PreparedProcessIdentity, Store,
    },
};
use rustix::{
    io::Errno,
    process::{Pid, test_kill_process},
};
use tokio::process::Child;

const RUN_ID: &str = "11111111-1111-4111-8111-111111111111";
const RUNNER_INSTANCE_ID: &str = "22222222-2222-4222-8222-222222222222";
const RUNTIME_NONCE: &str = "33333333333343338333333333333333";
const CHILD_REAP_TIMEOUT: Duration = Duration::from_secs(5);

struct Fixture {
    _root: tempfile::TempDir,
    database: PathBuf,
    runtime_root: PathBuf,
    runtime: PathBuf,
    marker: PathBuf,
    provider_release: PathBuf,
    descendant_release: PathBuf,
    descendant_pid: PathBuf,
    provider: PathBuf,
    run_id: RunId,
    runner_instance_id: RunnerInstanceId,
}

impl Fixture {
    fn new() -> (Self, Store, AdmittedRun) {
        let root = tempfile::tempdir_in("/tmp").unwrap();
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let database = root.path().join("state.db");
        let runtime_root = root.path().join("runs");
        private_directory(&runtime_root);
        let runtime = runtime_root.join(RUNTIME_NONCE);
        private_directory(&runtime);
        let policy = runtime.join("policy");
        private_directory(&policy);
        let project = root.path().join("project");
        private_directory(&project);

        let run_id = RunId::try_from(RUN_ID).unwrap();
        let runner_instance_id = RunnerInstanceId::try_from(RUNNER_INSTANCE_ID).unwrap();
        let project_id = ProjectId::try_from("factory").unwrap();
        let agent_id = AgentId::try_from("orchestrator").unwrap();
        let mut store = Store::open(&database).unwrap();
        store
            .create_project(
                NewProject {
                    id: project_id.clone(),
                    name: "Factory".into(),
                    root: project.to_string_lossy().into_owned(),
                },
                1,
            )
            .unwrap();
        store
            .create_agent(
                NewAgent {
                    id: agent_id.clone(),
                    project_id: project_id.clone(),
                    parent_agent_id: None,
                    role: AgentRole::Orchestrator,
                    provider: Provider::Shell,
                },
                2,
            )
            .unwrap();
        store
            .create_assigned_task(
                NewTask {
                    id: TaskId::try_from("task-1").unwrap(),
                    project_id: project_id.clone(),
                    parent_task_id: None,
                    title: "prove observer convergence".into(),
                    body: "fixture only".into(),
                    priority: 0,
                },
                agent_id.clone(),
                3,
            )
            .unwrap();
        let admitted = store
            .admit_next_run(
                NewRunAdmission {
                    run_id: run_id.clone(),
                    project_id,
                    agent_id,
                    capability_digest: "a".repeat(64),
                    runtime_claim: format!("runtime-claim:{RUNTIME_NONCE}"),
                    runner_instance_id: runner_instance_id.clone(),
                    runner_runtime: runtime.to_string_lossy().into_owned(),
                    max_active_runs: 1,
                    change_reservation: ChangeReservation {
                        id: ChangeId::try_from("unused-change").unwrap(),
                        source_root: root
                            .path()
                            .join("unused-change")
                            .to_string_lossy()
                            .into_owned(),
                        max_factory_changes: 1,
                    },
                    policy_cwd: policy.to_string_lossy().into_owned(),
                },
                4,
            )
            .unwrap()
            .unwrap();
        let provider = root.path().join("provider.sh");
        fs::write(
            &provider,
            "#!/bin/sh\nset -eu\n(while [ ! -e \"$3\" ]; do /bin/sleep 0.01; done) &\nprintf %s \"$!\" > \"$4\"\nprintf x >> \"$1\"\nwhile [ ! -e \"$2\" ]; do /bin/sleep 0.01; done\nexit 17\n",
        )
        .unwrap();
        fs::set_permissions(&provider, fs::Permissions::from_mode(0o700)).unwrap();
        let fixture = Self {
            marker: root.path().join("provider-ran"),
            provider_release: root.path().join("provider-release"),
            descendant_release: root.path().join("descendant-release"),
            descendant_pid: root.path().join("descendant-pid"),
            provider,
            _root: root,
            database,
            runtime_root,
            runtime,
            run_id,
            runner_instance_id,
        };
        (fixture, store, admitted)
    }

    fn reopen(&self) -> Store {
        Store::open(&self.database).unwrap()
    }

    fn launch_spec(&self) -> LaunchSpec {
        LaunchSpec {
            runner_program: PathBuf::from(env!("CARGO_BIN_EXE_factory-runner")),
            factoryctl_path: PathBuf::from("/usr/bin/true"),
            provider_program: self.provider.clone(),
            provider_arguments: vec![
                self.marker.as_os_str().to_owned(),
                self.provider_release.as_os_str().to_owned(),
                self.descendant_release.as_os_str().to_owned(),
                self.descendant_pid.as_os_str().to_owned(),
            ],
            provider_environment: ProviderEnvironment::Inherited,
            attempt_environment: Vec::new(),
            run_id: self.run_id.clone(),
            runner_instance_id: self.runner_instance_id.clone(),
            runtime_dir: self.runtime.clone(),
            cwd: self._root.path().to_owned(),
            source_root: self._root.path().to_owned(),
            startup_input: Vec::new(),
        }
    }

    fn execution_config(&self) -> execution::Config {
        execution::Config {
            factoryd_program: PathBuf::from("/usr/bin/false"),
            runner_program: PathBuf::from(env!("CARGO_BIN_EXE_factory-runner")),
            factoryctl_path: PathBuf::from("/usr/bin/true"),
            git_program: PathBuf::from("/usr/bin/false"),
            claude_installation: None,
            codex_provider: providers::codex::CodexProvider::new(None),
            cargo_program: None,
            runtime_root: self.runtime_root.clone(),
            changes_root: self._root.path().join("changes"),
            artifacts_root: self._root.path().join("artifacts"),
            guidance_root: self._root.path().join("guidance"),
            socket_path: self._root.path().join("factory.sock"),
            max_active_runs: 1,
        }
    }
}

struct ReleaseOnDrop(PathBuf);

impl Drop for ReleaseOnDrop {
    fn drop(&mut self) {
        let _ = fs::write(&self.0, b"release");
    }
}

struct OwnedRunner(Option<Child>);

impl OwnedRunner {
    fn new(child: Child) -> Self {
        Self(Some(child))
    }

    async fn wait(&mut self) -> std::process::ExitStatus {
        let status = self.0.as_mut().unwrap().wait().await.unwrap();
        self.0.take();
        status
    }

    fn kill_and_reap(&mut self) -> io::Result<std::process::ExitStatus> {
        let child = self.0.as_mut().expect("owned runner is already reaped");
        child.start_kill()?;
        let deadline = Instant::now() + CHILD_REAP_TIMEOUT;
        loop {
            if let Some(status) = child.try_wait()? {
                self.0.take();
                return Ok(status);
            }
            if Instant::now() >= deadline {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "owned runner did not reap after exact-child kill",
                ));
            }
            thread::sleep(Duration::from_millis(10));
        }
    }
}

impl Drop for OwnedRunner {
    fn drop(&mut self) {
        if self.0.is_some()
            && let Err(error) = self.kill_and_reap()
            && !thread::panicking()
        {
            panic!("failed to reap owned runner during cleanup: {error}");
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn production_observer_defers_terminal_until_external_group_absence_across_restart() {
    let (fixture, mut store, admitted) = Fixture::new();
    let _provider_release = ReleaseOnDrop(fixture.provider_release.clone());
    let _descendant_release = ReleaseOnDrop(fixture.descendant_release.clone());
    let (mut runner, prepared_provider, runner_pid, provider_pid) =
        manually_seed_running_observer_fixture(&fixture, &mut store, &admitted).await;
    drop(store);

    prepared_provider.activate().await.unwrap();
    wait_for_path(&fixture.marker).await;
    wait_for_path(&fixture.descendant_pid).await;
    let descendant_pid = fs::read_to_string(&fixture.descendant_pid)
        .unwrap()
        .parse::<u32>()
        .unwrap();
    assert_eq!(fs::read_to_string(&fixture.marker).unwrap(), "x");

    // The observer begins from a real running child with no invented outcome.
    assert_eq!(
        run(&fixture.reopen(), &fixture.run_id).phase,
        RunPhase::Running
    );

    // The production observer, not this fixture, must consume the terminal
    // event, persist its outcome, acknowledge the exact replay boundary, and
    // observe runner absence. A surviving descendant holds the process-group
    // resource open so this ordering is externally inspectable.
    let state = DaemonState::new(fixture.reopen());
    let (handle, manager) = execution::spawn(fixture.execution_config(), state.clone()).unwrap();
    fs::write(&fixture.provider_release, b"release").unwrap();
    assert!(
        tokio::time::timeout(Duration::from_secs(5), runner.wait())
            .await
            .expect("production observer did not acknowledge the runner exit")
            .success()
    );
    wait_for_observer_cut(&state, &fixture.run_id).await;
    assert!(process_absent(runner_pid));
    assert!(process_absent(provider_pid));
    assert!(!process_absent(descendant_pid));
    handle.shutdown().await.unwrap();
    manager.await.unwrap().unwrap();
    drop(state);

    let store = fixture.reopen();
    let finalizing = run(&store, &fixture.run_id);
    assert_eq!(finalizing.phase, RunPhase::Finalizing);
    assert_eq!(finalizing.exit_code, Some(17));
    assert_eq!(
        finalizing.outcome,
        Some(RunOutcome::Failed {
            reason: RunFailureReason::Process
        })
    );
    let resources = store.kernel_resources(&fixture.run_id).unwrap();
    assert!(resources.iter().any(|resource| {
        resource.kind == KernelResourceKind::RunnerProcess
            && resource.state == KernelResourceState::Released
    }));
    assert!(resources.iter().any(|resource| {
        resource.kind == KernelResourceKind::ProcessGroup
            && resource.state == KernelResourceState::Releasing
    }));
    assert!(fixture.runtime.exists());
    drop(store);

    // Cut: the acknowledged runner and provider are gone, but the surviving
    // external group keeps finalization durable across manager shutdown.
    fs::write(&fixture.descendant_release, b"release").unwrap();
    wait_for_process_absence(descendant_pid).await;
    let store = fixture.reopen();
    assert_eq!(run(&store, &fixture.run_id).phase, RunPhase::Finalizing);
    assert!(
        store
            .kernel_resources(&fixture.run_id)
            .unwrap()
            .iter()
            .any(|resource| resource.kind == KernelResourceKind::ProcessGroup
                && resource.state == KernelResourceState::Releasing)
    );
    drop(store);

    // Restart after exact external cleanup. The production manager durably
    // acknowledges the remaining releases, removes the exact runtime, and
    // only then writes Terminal.
    let state = DaemonState::new(fixture.reopen());
    let (handle, manager) = execution::spawn(fixture.execution_config(), state.clone()).unwrap();
    wait_for_terminal(&state, &fixture.run_id).await;
    let terminal = state
        .with_store({
            let run_id = fixture.run_id.clone();
            move |store| store.kernel_run(&run_id)
        })
        .await
        .unwrap()
        .unwrap();
    assert_eq!(terminal.phase, RunPhase::Terminal);
    assert_eq!(
        terminal.outcome,
        Some(RunOutcome::Failed {
            reason: RunFailureReason::Process
        })
    );
    let resources = state
        .with_store({
            let run_id = fixture.run_id.clone();
            move |store| store.kernel_resources(&run_id)
        })
        .await
        .unwrap();
    assert!(
        resources
            .iter()
            .all(|resource| resource.state == KernelResourceState::Released)
    );
    assert!(!fixture.runtime.exists());
    assert_eq!(fs::read_to_string(&fixture.marker).unwrap(), "x");

    handle.shutdown().await.unwrap();
    manager.await.unwrap().unwrap();
}

#[tokio::test]
async fn cancelled_wait_is_followed_by_exact_child_kill_and_reap() {
    let child = tokio::process::Command::new("/bin/sleep")
        .arg("30")
        .spawn()
        .unwrap();
    let raw_pid = child.id().unwrap();
    let mut runner = OwnedRunner::new(child);

    let wait = tokio::time::timeout(Duration::from_millis(20), runner.wait()).await;
    assert!(
        wait.is_err(),
        "the live child exited before wait cancellation"
    );
    assert!(
        runner.0.is_some(),
        "cancelling wait disarmed exact Child cleanup authority"
    );
    drop(runner);
    assert!(
        process_absent(raw_pid),
        "dropping the exact Child owner did not reap the runner"
    );
}

// Manual observer-boundary fixture: setup registration, runner PID
// registration, provider PID/PGID registration, and the Running Store
// transition deliberately bypass production launch orchestration. This test
// therefore proves no launch ordering or launch micro-window.
async fn manually_seed_running_observer_fixture(
    fixture: &Fixture,
    store: &mut Store,
    admitted: &AdmittedRun,
) -> (OwnedRunner, PreparedRunner, u32, u32) {
    register_runtime(store, fixture, admitted, 5);
    let setup = runner_process::prepare_runner(fixture.launch_spec())
        .await
        .unwrap();
    let setup_locator =
        runner_setup_locator(setup.setup_path(), &admitted.target.runner_instance_id);
    let setup_birth = runner_setup_birth(setup.setup_device(), setup.setup_inode());
    store
        .register_admitted_runner_setup(&fixture.run_id, &setup_locator, &setup_birth, 6)
        .unwrap();
    let prepared_runner = setup.spawn().unwrap();
    let runner_pid = prepared_runner.child_pid();
    let runner_locator = runner_locator(runner_pid, &fixture.runner_instance_id);
    let runner_birth = process_birth(runner_pid).unwrap();
    store
        .register_admitted_runner(
            &fixture.run_id,
            &setup_locator,
            &setup_birth,
            &runner_locator,
            &runner_birth,
            7,
        )
        .unwrap();
    let runner = OwnedRunner::new(prepared_runner.activate().await.unwrap());
    let client = RunnerClient::new(
        &fixture.runtime,
        fixture.run_id.clone(),
        fixture.runner_instance_id.clone(),
    );
    let prepared_provider = prepare_with_grace(&client).await;
    let provider_pid = prepared_provider.child_pid();
    let provider_birth = process_birth(provider_pid).unwrap();
    let (running, _) = store
        .activate_prepared_run(
            &fixture.run_id,
            PreparedProcessIdentity {
                runtime_locator: runtime_locator(&fixture.runtime),
                runtime_birth_fingerprint: runtime_birth(&fixture.runtime),
                runner_locator,
                runner_birth_fingerprint: runner_birth,
                provider_locator: serde_json::json!({ "pid": provider_pid }).to_string(),
                provider_birth_fingerprint: provider_birth.clone(),
                process_group_locator: serde_json::json!({
                    "pgid": prepared_provider.process_group_id()
                })
                .to_string(),
                process_group_birth_fingerprint: provider_birth,
            },
            8,
        )
        .unwrap();
    assert_eq!(running.phase, RunPhase::Running);
    (runner, prepared_provider, runner_pid, provider_pid)
}

async fn prepare_with_grace(client: &RunnerClient) -> PreparedRunner {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match client.prepare().await {
            Ok(prepared) => return prepared,
            Err(error) if Instant::now() < deadline => {
                let _ = error;
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            Err(error) => panic!("runner did not prepare provider: {error}"),
        }
    }
}

fn register_runtime(store: &mut Store, fixture: &Fixture, admitted: &AdmittedRun, at_ms: i64) {
    store
        .register_admitted_runtime(
            &fixture.run_id,
            &runtime_locator(&fixture.runtime),
            &admitted.target.runtime_claim,
            &runtime_birth(&fixture.runtime),
            at_ms,
        )
        .unwrap();
}

fn run(store: &Store, run_id: &RunId) -> factory_core::RunSnapshot {
    store.kernel_run(run_id).unwrap().unwrap()
}

fn private_directory(path: &Path) {
    fs::create_dir(path).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
}

fn runtime_locator(path: &Path) -> String {
    serde_json::json!({ "path": path }).to_string()
}

fn runtime_birth(path: &Path) -> String {
    let metadata = fs::symlink_metadata(path).unwrap();
    format!("unix-device:{}:inode:{}", metadata.dev(), metadata.ino())
}

fn runner_setup_locator(path: &Path, runner_instance_id: &RunnerInstanceId) -> String {
    serde_json::json!({
        "runner_instance_id": runner_instance_id.as_str(),
        "setup_path": path,
    })
    .to_string()
}

fn runner_locator(pid: u32, runner_instance_id: &RunnerInstanceId) -> String {
    serde_json::json!({
        "pid": pid,
        "runner_instance_id": runner_instance_id.as_str(),
    })
    .to_string()
}

fn runner_setup_birth(device: u64, inode: u64) -> String {
    format!("unix-device:{device}:inode:{inode}")
}

#[cfg(target_os = "linux")]
fn process_birth(pid: u32) -> Option<String> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let fields = stat.rsplit_once(") ")?.1;
    let ticks = fields.split_whitespace().nth(19)?;
    Some(format!("linux-start-ticks:{ticks}"))
}

#[cfg(not(target_os = "linux"))]
fn process_birth(pid: u32) -> Option<String> {
    test_kill_process(self::pid(pid)?).ok()?;
    Some("weak-presence-only".into())
}

fn pid(value: u32) -> Option<Pid> {
    i32::try_from(value).ok().and_then(Pid::from_raw)
}

fn process_absent(raw_pid: u32) -> bool {
    match test_kill_process(pid(raw_pid).unwrap()) {
        Err(Errno::SRCH) => true,
        Ok(()) => false,
        Err(error) => panic!("process {raw_pid} presence probe failed: {error}"),
    }
}

async fn wait_for_path(path: &Path) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !path.exists() && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(path.exists(), "{} was not created", path.display());
}

async fn wait_for_process_absence(raw_pid: u32) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !process_absent(raw_pid) && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(
        process_absent(raw_pid),
        "process {raw_pid} survived its release gate"
    );
}

async fn wait_for_observer_cut(state: &DaemonState, run_id: &RunId) {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let (run, resources) = state
            .with_store({
                let run_id = run_id.clone();
                move |store| {
                    Ok((
                        store.kernel_run(&run_id)?.unwrap(),
                        store.kernel_resources(&run_id)?,
                    ))
                }
            })
            .await
            .unwrap();
        let released = |kind| {
            resources.iter().any(|resource| {
                resource.kind == kind && resource.state == KernelResourceState::Released
            })
        };
        let group_pending = resources.iter().any(|resource| {
            resource.kind == KernelResourceKind::ProcessGroup
                && resource.state == KernelResourceState::Releasing
        });
        if run.phase == RunPhase::Finalizing
            && run.exit_code == Some(17)
            && released(KernelResourceKind::RunnerProcess)
            && released(KernelResourceKind::ProviderProcess)
            && group_pending
        {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "production observer did not reach the acknowledged external-resource cut"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

async fn wait_for_terminal(state: &DaemonState, run_id: &RunId) {
    let deadline = Instant::now() + Duration::from_secs(8);
    loop {
        let phase = state
            .with_store({
                let run_id = run_id.clone();
                move |store| Ok(store.kernel_run(&run_id)?.unwrap().phase)
            })
            .await
            .unwrap();
        if phase == RunPhase::Terminal {
            return;
        }
        assert!(Instant::now() < deadline, "run did not terminalize");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}
