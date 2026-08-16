use std::{fs, num::NonZeroU32, os::unix::fs::PermissionsExt, path::PathBuf, time::Duration};

use factory_core::{
    AgentId, AgentRole, FactoryEvent, ObserverHealth, ProjectId, Provider, RunStatus, TaskId,
};
use factoryd::{
    daemon_state::DaemonState,
    execution::{self, Config, StartTask},
    store::{AdoptedProviderSession, NewAgent, NewProject, NewTask, Store},
};
use tokio::{task::yield_now, time::timeout};

const THREAD_ID: &str = "0195d40a-1111-7000-8000-000000000001";
const CLAUDE_SESSION_ID: &str = "0195d40a-2222-7000-8000-000000000002";

fn id<T>(value: &str) -> T
where
    T: TryFrom<String>,
    T::Error: std::fmt::Debug,
{
    T::try_from(value.to_owned()).unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn real_runner_resumes_an_adopted_codex_session_and_cleans_up() {
    let directory = tempfile::tempdir_in("/tmp").unwrap();
    fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700)).unwrap();
    let database = directory.path().join("factory.db");
    let project_root = directory.path().join("project");
    fs::create_dir(&project_root).unwrap();
    fs::set_permissions(&project_root, fs::Permissions::from_mode(0o700)).unwrap();
    let project_root = fs::canonicalize(project_root).unwrap();
    let runtime_root = directory.path().join("runs");
    fs::create_dir(&runtime_root).unwrap();
    fs::set_permissions(&runtime_root, fs::Permissions::from_mode(0o700)).unwrap();
    let provider_input = directory.path().join("provider-input.txt");
    let provider_arguments = directory.path().join("provider-arguments.txt");
    let provider_home = directory.path().join("provider-home.txt");
    let codex_home = directory.path().join("codex-home");
    fs::create_dir(&codex_home).unwrap();
    fs::set_permissions(&codex_home, fs::Permissions::from_mode(0o755)).unwrap();
    let codex_home = fs::canonicalize(codex_home).unwrap();
    let provider = directory.path().join("fake-codex");
    let thread_started = format!("{{\"type\":\"thread.started\",\"thread_id\":\"{THREAD_ID}\"}}");
    let final_message = "{\"type\":\"item.completed\",\"item\":{\"id\":\"final\",\"type\":\"agent_message\",\"text\":\"codex done\"}}";
    let provider_script = format!(
        "#!/bin/sh\nprintf '%s\\n' \"$@\" > '{}'\nprintf '%s' \"$CODEX_HOME\" > '{}'\ncat > '{}'\nprintf '%s\\n' '{}' '{}' '{}'\n",
        provider_arguments.display(),
        provider_home.display(),
        provider_input.display(),
        thread_started,
        final_message,
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
        .adopt_agent(
            NewAgent {
                id: agent_id.clone(),
                project_id: project_id.clone(),
                parent_agent_id: None,
                role: AgentRole::Orchestrator,
                provider: Provider::Codex,
            },
            AdoptedProviderSession::Codex {
                thread_id: THREAD_ID.into(),
                cwd: project_root.to_str().unwrap().into(),
                codex_home: Some(codex_home.to_str().unwrap().into()),
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
            claude_program: PathBuf::from("unused-claude"),
            claude_max_turns: NonZeroU32::new(20).unwrap(),
            claude_max_budget_cents: NonZeroU32::new(500).unwrap(),
            runtime_root: runtime_root.clone(),
            guidance_root: directory.path().to_path_buf(),
            max_active_runs: 1,
            startup_timeout: Duration::from_secs(5),
            connect_grace: Duration::from_secs(5),
            batch_delay: Duration::from_millis(10),
        },
        state.clone(),
    )
    .unwrap();
    let memory_path =
        factory_core::paths::agent_memory_path(directory.path(), &project_id, &agent_id);
    let started = timeout(
        Duration::from_secs(10),
        handle.start_task(StartTask {
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
        format!(
            "Task instructions:\nreal runner private task\n\nYour memory file is {}. Append durable lessons there; keep it under 16 KB.",
            memory_path.display()
        )
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
            "--ignore-user-config",
            "resume",
            THREAD_ID,
            "-",
        ]
    );
    assert!(!arguments.contains("real runner private task"));
    assert_eq!(
        fs::read_to_string(&provider_home).unwrap(),
        codex_home.to_str().unwrap()
    );

    let run_id = started.run_id.clone();
    let project_for_snapshot = id::<ProjectId>("project");
    let agent_for_snapshot = id::<AgentId>("agent");
    let (events, target, result) = state
        .with_store(move |store| {
            Ok((
                store.events_after(baseline, 100)?,
                store.execution_target(&run_id)?,
                store
                    .webhook_snapshot(&project_for_snapshot, &agent_for_snapshot, 20)?
                    .tasks
                    .into_iter()
                    .find(|task| task.id == id::<TaskId>("task"))
                    .and_then(|task| task.result),
            ))
        })
        .await
        .unwrap();
    assert_eq!(result.as_deref(), Some("codex done"));
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
    assert_eq!(run_truth.len(), 4);
    assert_eq!(
        run_truth.first(),
        Some(&(RunStatus::Starting, ObserverHealth::Unknown))
    );
    assert_eq!(
        run_truth.last(),
        Some(&(RunStatus::Succeeded, ObserverHealth::Healthy))
    );
    assert!(run_truth.windows(2).all(|window| {
        let status_changed = window[0].0 != window[1].0;
        let health_changed = window[0].1 != window[1].1;
        status_changed ^ health_changed
    }));
    let mut lifecycle = run_truth
        .iter()
        .map(|(status, _)| *status)
        .collect::<Vec<_>>();
    lifecycle.dedup();
    assert_eq!(
        lifecycle,
        [
            RunStatus::Starting,
            RunStatus::Running,
            RunStatus::Succeeded
        ]
    );
    let mut health = run_truth
        .iter()
        .map(|(_, health)| *health)
        .collect::<Vec<_>>();
    health.dedup();
    assert_eq!(health, [ObserverHealth::Unknown, ObserverHealth::Healthy]);
    let public_json = serde_json::to_string(&events).unwrap();
    for private in [
        "real runner private task",
        THREAD_ID,
        codex_home.to_str().unwrap(),
        target.runner_instance_id.as_str(),
        target.runner_runtime.as_str(),
    ] {
        assert!(!public_json.contains(private));
    }

    handle.shutdown().await.unwrap();
    join.await.unwrap().unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn real_runner_resumes_an_adopted_claude_session_and_cleans_up() {
    let directory = tempfile::tempdir_in("/tmp").unwrap();
    fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700)).unwrap();
    let database = directory.path().join("factory.db");
    let project_root = directory.path().join("claude-project");
    fs::create_dir(&project_root).unwrap();
    fs::set_permissions(&project_root, fs::Permissions::from_mode(0o700)).unwrap();
    let project_root = fs::canonicalize(project_root).unwrap();
    let runtime_root = directory.path().join("claude-runs");
    fs::create_dir(&runtime_root).unwrap();
    fs::set_permissions(&runtime_root, fs::Permissions::from_mode(0o700)).unwrap();
    let provider_input = directory.path().join("claude-input.txt");
    let provider_arguments = directory.path().join("claude-arguments.txt");
    let provider_environment = directory.path().join("claude-environment.txt");
    let provider_cwd = directory.path().join("claude-cwd.txt");
    let provider = directory.path().join("fake-claude");
    let init = format!(
        "{{\"type\":\"system\",\"subtype\":\"init\",\"session_id\":\"{CLAUDE_SESSION_ID}\",\"model\":\"claude-sonnet-4-6\",\"permissionMode\":\"acceptEdits\",\"claude_code_version\":\"2.1.233\"}}"
    );
    let result = format!(
        "{{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false,\"session_id\":\"{CLAUDE_SESSION_ID}\",\"result\":\"done\",\"terminal_reason\":\"completed\",\"stop_reason\":\"end_turn\",\"permission_denials\":[],\"total_cost_usd\":0.01,\"usage\":{{\"input_tokens\":1,\"cache_creation_input_tokens\":0,\"cache_read_input_tokens\":0,\"output_tokens\":1}}}}"
    );
    let provider_script = format!(
        "#!/bin/sh\nprintf '%s\\n' \"$@\" > '{}'\npwd -P > '{}'\nenv > '{}'\ncat > '{}'\nprintf '%s\\n' '{}' '{}'\n",
        provider_arguments.display(),
        provider_cwd.display(),
        provider_environment.display(),
        provider_input.display(),
        init,
        result,
    );
    fs::write(&provider, provider_script).unwrap();
    fs::set_permissions(&provider, fs::Permissions::from_mode(0o700)).unwrap();

    let project_id = id::<ProjectId>("claude-project");
    let task_id = id::<TaskId>("claude-task");
    let agent_id = id::<AgentId>("claude-agent");
    let mut store = Store::open(&database).unwrap();
    store
        .create_project(
            NewProject {
                id: project_id.clone(),
                name: "Claude Project".into(),
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
                title: "Claude Task".into(),
                body: "real runner private Claude task".into(),
                priority: 0,
            },
            2,
        )
        .unwrap();
    store
        .adopt_agent(
            NewAgent {
                id: agent_id.clone(),
                project_id: project_id.clone(),
                parent_agent_id: None,
                role: AgentRole::Orchestrator,
                provider: Provider::ClaudeCode,
            },
            AdoptedProviderSession::ClaudeCode {
                session_id: CLAUDE_SESSION_ID.into(),
                cwd: project_root.to_str().unwrap().into(),
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
            codex_program: PathBuf::from("unused-codex"),
            claude_program: provider,
            claude_max_turns: NonZeroU32::new(20).unwrap(),
            claude_max_budget_cents: NonZeroU32::new(500).unwrap(),
            runtime_root: runtime_root.clone(),
            guidance_root: directory.path().to_path_buf(),
            max_active_runs: 1,
            startup_timeout: Duration::from_secs(5),
            connect_grace: Duration::from_secs(5),
            batch_delay: Duration::from_millis(10),
        },
        state.clone(),
    )
    .unwrap();
    let memory_path =
        factory_core::paths::agent_memory_path(directory.path(), &project_id, &agent_id);
    let started = timeout(
        Duration::from_secs(10),
        handle.start_task(StartTask {
            project_id,
            task_id,
            agent_id,
            parent_run_id: None,
            worktree: project_root.clone(),
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

    let private_task = "real runner private Claude task";
    let composed_input = format!(
        "Task instructions:\n{private_task}\n\nYour memory file is {}. Append durable lessons there; keep it under 16 KB.",
        memory_path.display()
    );
    assert_eq!(fs::read_to_string(&provider_input).unwrap(), composed_input);
    let arguments = fs::read_to_string(&provider_arguments).unwrap();
    assert_eq!(
        arguments.lines().collect::<Vec<_>>(),
        [
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
            "20",
            "--max-budget-usd",
            "5.00",
            "--no-chrome",
            "--safe-mode",
            "--resume",
            CLAUDE_SESSION_ID,
        ]
    );
    let environment = fs::read_to_string(&provider_environment).unwrap();
    assert!(!arguments.contains(private_task));
    assert!(!environment.contains(private_task));
    assert_eq!(
        fs::read_to_string(&provider_cwd).unwrap().trim_end(),
        project_root.to_str().unwrap()
    );

    let run_id = started.run_id.clone();
    let project_for_snapshot = id::<ProjectId>("claude-project");
    let agent_for_snapshot = id::<AgentId>("claude-agent");
    let (events, target, result) = state
        .with_store(move |store| {
            Ok((
                store.events_after(baseline, 100)?,
                store.execution_target(&run_id)?,
                store
                    .webhook_snapshot(&project_for_snapshot, &agent_for_snapshot, 20)?
                    .tasks
                    .into_iter()
                    .find(|task| task.id == id::<TaskId>("claude-task"))
                    .and_then(|task| task.result),
            ))
        })
        .await
        .unwrap();
    assert_eq!(result.as_deref(), Some("done"));
    assert_eq!(target.provider, Provider::ClaudeCode);
    assert_eq!(
        target.provider_session_id.as_deref(),
        Some(CLAUDE_SESSION_ID)
    );
    assert!(target.resumes_provider_session);
    let run_truth = events
        .iter()
        .filter_map(|event| match &event.event {
            FactoryEvent::RunChanged { run } if run.id == started.run_id => {
                Some((run.status, run.observer_health))
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(run_truth.len(), 4);
    assert_eq!(
        run_truth.first(),
        Some(&(RunStatus::Starting, ObserverHealth::Unknown))
    );
    assert_eq!(
        run_truth.last(),
        Some(&(RunStatus::Succeeded, ObserverHealth::Healthy))
    );
    assert!(run_truth.windows(2).all(|window| {
        let status_changed = window[0].0 != window[1].0;
        let health_changed = window[0].1 != window[1].1;
        status_changed ^ health_changed
    }));
    let mut lifecycle = run_truth
        .iter()
        .map(|(status, _)| *status)
        .collect::<Vec<_>>();
    lifecycle.dedup();
    assert_eq!(
        lifecycle,
        [
            RunStatus::Starting,
            RunStatus::Running,
            RunStatus::Succeeded
        ]
    );
    let mut health = run_truth
        .iter()
        .map(|(_, health)| *health)
        .collect::<Vec<_>>();
    health.dedup();
    assert_eq!(health, [ObserverHealth::Unknown, ObserverHealth::Healthy]);
    let public_json = serde_json::to_string(&events).unwrap();
    for private in [
        private_task,
        CLAUDE_SESSION_ID,
        target.runner_instance_id.as_str(),
        target.runner_runtime.as_str(),
    ] {
        assert!(!public_json.contains(private));
    }

    handle.shutdown().await.unwrap();
    join.await.unwrap().unwrap();
}
