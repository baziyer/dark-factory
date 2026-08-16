use std::{fs, os::unix::fs::PermissionsExt, path::PathBuf, time::Duration};

use factory_core::{
    AgentId, AgentRole, FactoryEvent, ObserverHealth, ProjectId, Provider, RunStatus, TaskId,
};
use factoryd::{
    daemon_state::DaemonState,
    execution::{self, Config, StartCodex},
    store::{NewAgent, NewProject, NewTask, Store},
};
use tokio::{task::yield_now, time::timeout};

const THREAD_ID: &str = "0195d40a-1111-7000-8000-000000000001";

fn id<T>(value: &str) -> T
where
    T: TryFrom<String>,
    T::Error: std::fmt::Debug,
{
    T::try_from(value.to_owned()).unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn real_runner_executes_fake_codex_and_cleans_up_after_exact_ack() {
    let directory = tempfile::tempdir_in("/tmp").unwrap();
    fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700)).unwrap();
    let database = directory.path().join("factory.db");
    let project_root = directory.path().join("project");
    fs::create_dir(&project_root).unwrap();
    fs::set_permissions(&project_root, fs::Permissions::from_mode(0o700)).unwrap();
    let runtime_root = directory.path().join("runs");
    fs::create_dir(&runtime_root).unwrap();
    fs::set_permissions(&runtime_root, fs::Permissions::from_mode(0o700)).unwrap();
    let provider_input = directory.path().join("provider-input.txt");
    let provider_arguments = directory.path().join("provider-arguments.txt");
    let provider = directory.path().join("fake-codex");
    let thread_started = format!("{{\"type\":\"thread.started\",\"thread_id\":\"{THREAD_ID}\"}}");
    let provider_script = format!(
        "#!/bin/sh\nprintf '%s\\n' \"$@\" > '{}'\ncat > '{}'\nprintf '%s\\n' '{}' '{}'\n",
        provider_arguments.display(),
        provider_input.display(),
        thread_started,
        "{\"type\":\"turn.completed\",\"usage\":{\"input_tokens\":1,\"cached_input_tokens\":0,\"cache_write_input_tokens\":0,\"output_tokens\":1,\"reasoning_output_tokens\":0}}",
    );
    fs::write(&provider, provider_script).unwrap();
    fs::set_permissions(&provider, fs::Permissions::from_mode(0o700)).unwrap();

    let project_id = id::<ProjectId>("project");
    let task_id = id::<TaskId>("task");
    let agent_id = id::<AgentId>("agent");
    let mut store = Store::open(&database).unwrap();
    store
        .create_project(
            NewProject {
                id: project_id.clone(),
                name: "Project".into(),
                root: project_root.to_str().unwrap().into(),
            },
            1,
        )
        .unwrap();
    store
        .create_task(
            NewTask {
                id: task_id.clone(),
                project_id: project_id.clone(),
                parent_task_id: None,
                title: "Task".into(),
                body: "real runner private task".into(),
                priority: 0,
            },
            2,
        )
        .unwrap();
    store
        .create_agent(
            NewAgent {
                id: agent_id.clone(),
                project_id: project_id.clone(),
                parent_agent_id: None,
                role: AgentRole::Worker,
                provider: Provider::Codex,
            },
            3,
        )
        .unwrap();
    let baseline = store.latest_event_sequence().unwrap();
    let state = DaemonState::new(store);
    let runner = PathBuf::from(env!("CARGO_BIN_EXE_factory-runner"));
    assert!(runner.is_absolute());
    let (handle, join) = execution::spawn(
        Config {
            runner_program: runner,
            codex_program: provider,
            runtime_root: runtime_root.clone(),
            max_active_runs: 1,
            startup_timeout: Duration::from_secs(5),
            connect_grace: Duration::from_secs(5),
            batch_delay: Duration::from_millis(10),
        },
        state.clone(),
    )
    .unwrap();
    let started = timeout(
        Duration::from_secs(10),
        handle.start_codex(StartCodex {
            project_id,
            task_id,
            agent_id,
            parent_run_id: None,
            worktree: project_root,
        }),
    )
    .await
    .expect("real runner did not authenticate")
    .unwrap();

    timeout(Duration::from_secs(10), async {
        loop {
            let run_id = started.run_id.clone();
            let reconciled = state
                .with_store(move |store| {
                    Ok(!store
                        .recoverable_runs()?
                        .into_iter()
                        .any(|run| run.run.id == run_id))
                })
                .await
                .unwrap();
            let runtime_clean = fs::read_dir(&runtime_root).unwrap().next().is_none();
            if reconciled && runtime_clean {
                break;
            }
            yield_now().await;
        }
    })
    .await
    .expect("real runner terminal was not acknowledged and cleaned up");

    assert_eq!(
        fs::read_to_string(&provider_input).unwrap(),
        "real runner private task"
    );
    let arguments = fs::read_to_string(&provider_arguments).unwrap();
    assert_eq!(
        arguments.lines().collect::<Vec<_>>(),
        [
            "exec",
            "--json",
            "--color",
            "never",
            "--sandbox",
            "workspace-write",
            "-c",
            "approval_policy=\"never\"",
            "-",
        ]
    );
    assert!(!arguments.contains("real runner private task"));

    let run_id = started.run_id.clone();
    let (events, target) = state
        .with_store(move |store| {
            Ok((
                store.events_after(baseline, 100)?,
                store.execution_target(&run_id)?,
            ))
        })
        .await
        .unwrap();
    assert_eq!(target.provider_session_id.as_deref(), Some(THREAD_ID));
    let run_truth = events
        .iter()
        .filter_map(|event| match &event.event {
            FactoryEvent::RunChanged { run } if run.id == started.run_id => {
                Some((run.status, run.observer_health))
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert!(
        matches!(
            run_truth.as_slice(),
            [
                (RunStatus::Starting, ObserverHealth::Unknown),
                (RunStatus::Running, ObserverHealth::Unknown),
                (RunStatus::Running, ObserverHealth::Healthy),
                (RunStatus::Succeeded, ObserverHealth::Healthy),
            ] | [
                (RunStatus::Starting, ObserverHealth::Unknown),
                (RunStatus::Running, ObserverHealth::Unknown),
                (RunStatus::Succeeded, ObserverHealth::Unknown),
                (RunStatus::Succeeded, ObserverHealth::Healthy),
            ]
        ),
        "runner lifecycle and observer health were not monotonic: {run_truth:?}"
    );
    let public_json = serde_json::to_string(&events).unwrap();
    for private in [
        "real runner private task",
        THREAD_ID,
        target.runner_instance_id.as_str(),
        target.runner_runtime.as_str(),
    ] {
        assert!(!public_json.contains(private));
    }

    handle.shutdown().await.unwrap();
    join.await.unwrap().unwrap();
}
