use std::{fs, os::unix::fs::PermissionsExt, path::Path};

use factory_core::{
    AgentId, AgentRole, ChangeId, FactoryEvent, ProjectId, Provider, ProviderHookEvent, RunId,
    RunnerInstanceId, TaskId,
    local::{
        ErrorCode, LocalRequest, LocalResponse, MAX_PROVIDER_HOOK_PAYLOAD_BYTES, RequestCredential,
        RequestEnvelope, ServerFrame,
    },
};
use factoryd::{
    daemon_state::DaemonState,
    execution, local_api,
    store::{
        ChangeReservation, NewAgent, NewProject, NewRunAdmission, NewTask, PreparedProcessIdentity,
        Store,
    },
};
use sha2::{Digest, Sha256};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::{UnixListener, UnixStream},
    sync::oneshot,
};

async fn request(socket: &Path, envelope: RequestEnvelope) -> ServerFrame {
    let mut stream = UnixStream::connect(socket).await.unwrap();
    let mut encoded = serde_json::to_vec(&envelope).unwrap();
    encoded.push(b'\n');
    stream.write_all(&encoded).await.unwrap();
    let mut line = Vec::new();
    BufReader::new(stream)
        .read_until(b'\n', &mut line)
        .await
        .unwrap();
    serde_json::from_slice(&line).unwrap()
}

fn capability_digest(bearer: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bearer.as_bytes());
    format!("{:x}", hasher.finalize())
}

fn seed_running_attempt(store: &mut Store, bearer: &str) -> (ProjectId, AgentId, RunId, i64) {
    let project_id = ProjectId::try_from("factory").unwrap();
    let agent_id = AgentId::try_from("orchestrator").unwrap();
    let run_id = RunId::try_from("11111111-1111-4111-8111-111111111111").unwrap();
    store
        .create_project(
            NewProject {
                id: project_id.clone(),
                name: "Factory".into(),
                root: "/tmp/factory".into(),
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
                id: TaskId::try_from("task").unwrap(),
                project_id: project_id.clone(),
                parent_task_id: None,
                title: "Exercise provider hook policy".into(),
                body: String::new(),
                priority: 0,
            },
            agent_id.clone(),
            3,
        )
        .unwrap();
    store
        .admit_next_run(
            NewRunAdmission {
                run_id: run_id.clone(),
                project_id: project_id.clone(),
                agent_id: agent_id.clone(),
                capability_digest: capability_digest(bearer),
                runtime_claim: "runtime-claim:55555555555545558555555555555555".into(),
                runner_instance_id: RunnerInstanceId::try_from(
                    "22222222-2222-4222-8222-222222222222",
                )
                .unwrap(),
                runner_runtime: "/tmp/factory-runner".into(),
                max_active_runs: 1,
                change_reservation: ChangeReservation {
                    id: ChangeId::try_from("unused-change").unwrap(),
                    source_root: "/tmp/unused-change".into(),
                    max_factory_changes: 1,
                },
                policy_cwd: "/tmp/factory-policy".into(),
            },
            4,
        )
        .unwrap()
        .expect("queued task should be admitted");
    store
        .activate_prepared_run(
            &run_id,
            PreparedProcessIdentity {
                runtime_locator: serde_json::json!({"path":"/tmp/factory-runner"}).to_string(),
                runtime_birth_fingerprint: "runtime-birth".into(),
                runner_locator: serde_json::json!({"pid":9}).to_string(),
                runner_birth_fingerprint: "runner-birth".into(),
                provider_locator: serde_json::json!({"pid":10}).to_string(),
                provider_birth_fingerprint: "provider-birth".into(),
                process_group_locator: serde_json::json!({"pgid":10}).to_string(),
                process_group_birth_fingerprint: "provider-birth".into(),
            },
            5,
        )
        .unwrap();
    let baseline = store.latest_event_sequence().unwrap();
    (project_id, agent_id, run_id, baseline)
}

fn valid_hook_payload_with_encoded_size(size: usize) -> serde_json::Value {
    let mut payload = serde_json::json!({
        "tool_name": "Read",
        "tool_input": {},
        "padding": "",
    });
    let overhead = serde_json::to_vec(&payload).unwrap().len();
    payload["padding"] = serde_json::Value::String("x".repeat(size - overhead));
    assert_eq!(serde_json::to_vec(&payload).unwrap().len(), size);
    payload
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn request_authority_is_explicit_and_taskless_bearers_are_refused() {
    let directory = tempfile::tempdir_in("/tmp").unwrap();
    fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700)).unwrap();
    let socket = directory.path().join("factory.sock");
    let runtime_root = directory.path().join("runs");
    let state = DaemonState::new(Store::open_in_memory().unwrap());
    let (execution, manager) = execution::spawn(
        execution::Config {
            factoryd_program: "/bin/false".into(),
            runner_program: "/bin/false".into(),
            factoryctl_path: "/bin/false".into(),
            git_program: "/bin/false".into(),
            claude_installation: None,
            codex_provider: factoryd::providers::codex::CodexProvider::new(None),
            cargo_program: Some("/bin/false".into()),
            runtime_root,
            changes_root: directory.path().join("changes"),
            artifacts_root: directory.path().join("artifacts"),
            guidance_root: directory.path().join("guidance"),
            socket_path: socket.clone(),
            max_active_runs: 1,
        },
        state.clone(),
    )
    .unwrap();
    let operator = RequestCredential::new("operator-secret".into()).unwrap();
    let listener = UnixListener::bind(&socket).unwrap();
    let (stop, shutdown) = oneshot::channel();
    let server = tokio::spawn(local_api::serve(
        listener,
        state,
        execution.clone(),
        directory.path().join("guidance"),
        operator.clone(),
        async move {
            let _ = shutdown.await;
        },
    ));

    assert!(matches!(
        request(&socket, RequestEnvelope::new(LocalRequest::Health)).await,
        ServerFrame::Response {
            response: LocalResponse::Health { .. },
            ..
        }
    ));
    assert!(matches!(
        request(
            &socket,
            RequestEnvelope::new(LocalRequest::ListProjects {
                after_id: None,
                limit: 1,
            }),
        )
        .await,
        ServerFrame::Response {
            response: LocalResponse::Error {
                code: ErrorCode::Unauthorized,
                ..
            },
            ..
        }
    ));
    assert!(matches!(
        request(
            &socket,
            RequestEnvelope::new(LocalRequest::RustStorageStatus),
        )
        .await,
        ServerFrame::Response {
            response: LocalResponse::Error {
                code: ErrorCode::Unauthorized,
                ..
            },
            ..
        }
    ));
    assert!(matches!(
        request(
            &socket,
            RequestEnvelope::authenticated(LocalRequest::RustStorageStatus, operator.clone()),
        )
        .await,
        ServerFrame::Response {
            response: LocalResponse::RustStorageStatus { storage },
            ..
        } if storage.cache_count == 0
            && storage.cache_bytes == Some(0)
            && storage.complete
    ));

    let taskless = RequestCredential::new("not-an-admitted-attempt".into()).unwrap();
    assert!(matches!(
        request(
            &socket,
            RequestEnvelope::authenticated(
                LocalRequest::CompleteAttempt {
                    result: "done".into(),
                },
                taskless,
            ),
        )
        .await,
        ServerFrame::Response {
            response: LocalResponse::Error {
                code: ErrorCode::Unauthorized,
                ..
            },
            ..
        }
    ));
    assert!(matches!(
        request(
            &socket,
            RequestEnvelope::authenticated(
                LocalRequest::CompleteAttempt {
                    result: "done".into(),
                },
                operator.clone(),
            ),
        )
        .await,
        ServerFrame::Response {
            response: LocalResponse::Error {
                code: ErrorCode::Unauthorized,
                ..
            },
            ..
        }
    ));
    assert!(matches!(
        request(
            &socket,
            RequestEnvelope::authenticated(
                LocalRequest::ListProjects {
                    after_id: None,
                    limit: 1,
                },
                operator,
            ),
        )
        .await,
        ServerFrame::Response {
            response: LocalResponse::Projects { projects, .. },
            ..
        } if projects.is_empty()
    ));

    let _ = stop.send(());
    server.await.unwrap().unwrap();
    execution.shutdown().await.unwrap();
    manager.await.unwrap().unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn provider_hook_payloads_are_bounded_and_malformed_calls_are_denied_and_audited() {
    let directory = tempfile::tempdir_in("/tmp").unwrap();
    fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700)).unwrap();
    let socket = directory.path().join("factory.sock");
    let state = DaemonState::new(Store::open_in_memory().unwrap());
    let (execution, manager) = execution::spawn(
        execution::Config {
            factoryd_program: "/bin/false".into(),
            runner_program: "/bin/false".into(),
            factoryctl_path: "/bin/false".into(),
            git_program: "/bin/false".into(),
            claude_installation: None,
            codex_provider: factoryd::providers::codex::CodexProvider::new(None),
            cargo_program: Some("/bin/false".into()),
            runtime_root: directory.path().join("runs"),
            changes_root: directory.path().join("changes"),
            artifacts_root: directory.path().join("artifacts"),
            guidance_root: directory.path().join("guidance"),
            socket_path: socket.clone(),
            max_active_runs: 1,
        },
        state.clone(),
    )
    .unwrap();
    execution.shutdown().await.unwrap();
    manager.await.unwrap().unwrap();

    let bearer = "attempt-secret";
    let seeded = bearer.to_owned();
    let (project_id, agent_id, run_id, baseline) = state
        .commit_and_publish(move |store| {
            let seeded = seed_running_attempt(store, &seeded);
            Ok((seeded, Vec::new()))
        })
        .await
        .unwrap();
    let credential = RequestCredential::new(bearer.into()).unwrap();
    let operator = RequestCredential::new("operator-secret".into()).unwrap();
    let listener = UnixListener::bind(&socket).unwrap();
    let (stop, shutdown) = oneshot::channel();
    let server = tokio::spawn(local_api::serve(
        listener,
        state.clone(),
        execution,
        directory.path().join("guidance"),
        operator,
        async move {
            let _ = shutdown.await;
        },
    ));

    let at_limit = request(
        &socket,
        RequestEnvelope::authenticated(
            LocalRequest::ProviderHook {
                event: ProviderHookEvent::PreToolUse,
                payload: valid_hook_payload_with_encoded_size(MAX_PROVIDER_HOOK_PAYLOAD_BYTES),
            },
            credential.clone(),
        ),
    )
    .await;
    assert!(matches!(
        at_limit,
        ServerFrame::Response {
            response: LocalResponse::ProviderHookReply { ref reply },
            ..
        } if reply == &serde_json::json!({})
    ));
    let (budget_after_limit, sequence_after_limit) = state
        .with_store({
            let project_id = project_id.clone();
            let agent_id = agent_id.clone();
            move |store| {
                Ok((
                    store.agent_budget(&project_id, &agent_id)?,
                    store.latest_event_sequence()?,
                ))
            }
        })
        .await
        .unwrap();
    assert_eq!(budget_after_limit.tool_calls, 1);
    assert_eq!(sequence_after_limit, baseline + 2);

    let over_limit = request(
        &socket,
        RequestEnvelope::authenticated(
            LocalRequest::ProviderHook {
                event: ProviderHookEvent::PreToolUse,
                payload: valid_hook_payload_with_encoded_size(MAX_PROVIDER_HOOK_PAYLOAD_BYTES + 1),
            },
            credential.clone(),
        ),
    )
    .await;
    assert!(matches!(
        over_limit,
        ServerFrame::Response {
            response: LocalResponse::Error {
                code: ErrorCode::InvalidRequest,
                ref message,
            },
            ..
        } if message == "provider hook payload must be at most 65536 bytes"
    ));
    let (budget_after_rejection, sequence_after_rejection) = state
        .with_store({
            let project_id = project_id.clone();
            let agent_id = agent_id.clone();
            move |store| {
                Ok((
                    store.agent_budget(&project_id, &agent_id)?,
                    store.latest_event_sequence()?,
                ))
            }
        })
        .await
        .unwrap();
    assert_eq!(budget_after_rejection, budget_after_limit);
    assert_eq!(sequence_after_rejection, sequence_after_limit);

    let malformed = [
        (serde_json::json!({}), "invalid_tool", "invalid_tool_name"),
        (
            serde_json::json!({"tool_name":42}),
            "invalid_tool",
            "invalid_tool_name",
        ),
        (
            serde_json::json!({"tool_name":"Bash","tool_input":{}}),
            "Bash",
            "invalid_shell_command",
        ),
        (
            serde_json::json!({"tool_name":"Bash","tool_input":{"command":42}}),
            "Bash",
            "invalid_shell_command",
        ),
    ];
    for (payload, _, rule) in &malformed {
        let response = request(
            &socket,
            RequestEnvelope::authenticated(
                LocalRequest::ProviderHook {
                    event: ProviderHookEvent::PreToolUse,
                    payload: payload.clone(),
                },
                credential.clone(),
            ),
        )
        .await;
        assert!(matches!(
            response,
            ServerFrame::Response {
                response: LocalResponse::ProviderHookReply { ref reply },
                ..
            } if reply["hookSpecificOutput"]["permissionDecision"] == "deny"
                && reply["hookSpecificOutput"]["permissionDecisionReason"]
                    == format!("Dark Factory policy: {rule}")
        ));
    }

    let (budget, events) = state
        .with_store({
            let project_id = project_id.clone();
            let agent_id = agent_id.clone();
            move |store| {
                Ok((
                    store.agent_budget(&project_id, &agent_id)?,
                    store.events_after(baseline, 10)?,
                ))
            }
        })
        .await
        .unwrap();
    assert_eq!(budget.tool_calls, 5);
    assert!(!budget.exhausted);
    assert_eq!(events.len(), 10);
    for (index, pair) in events.chunks_exact(2).enumerate() {
        assert_eq!(pair[1].sequence, pair[0].sequence + 1);
        assert!(matches!(
            &pair[0].event,
            FactoryEvent::AgentBudgetChanged {
                project_id: event_project,
                agent_id: event_agent,
                budget,
                action,
                ..
            } if event_project == &project_id
                && event_agent == &agent_id
                && budget.tool_calls == u64::try_from(index + 1).unwrap()
                && action == "observed"
        ));
        let (tool_name, rule) = if index == 0 {
            ("Read", None)
        } else {
            let (_, tool_name, rule) = &malformed[index - 1];
            (*tool_name, Some(*rule))
        };
        assert!(matches!(
            &pair[1].event,
            FactoryEvent::PolicyDecision {
                project_id: event_project,
                agent_id: event_agent,
                run_id: event_run,
                tool_name: event_tool,
                decision,
                rule: event_rule,
            } if event_project == &project_id
                && event_agent == &agent_id
                && event_run == &run_id
                && event_tool == tool_name
                && decision == if rule.is_some() { "deny" } else { "allow" }
                && event_rule.as_deref() == rule
        ));
    }

    let _ = stop.send(());
    server.await.unwrap().unwrap();
}
