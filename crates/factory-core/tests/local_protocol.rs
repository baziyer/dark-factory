use factory_core::{
    AgentId, AgentRole, AgentSnapshot, FactoryEvent, PROTOCOL_VERSION, ProjectId, ProjectSnapshot,
    Provider, ProviderHookEvent, RunId, TaskDetail, TaskId, TaskSnapshot, TaskStatus,
    local::{
        AgentDetail, AgentMessage, AgentProfile, ErrorCode, LocalRequest, LocalResponse,
        MAX_LOCAL_FRAME_BYTES, MAX_REQUEST_CREDENTIAL_BYTES, MAX_TASK_BODY_BYTES,
        RequestCredential, RequestEnvelope, ServerFrame,
    },
};

fn project_id(value: &str) -> ProjectId {
    ProjectId::try_from(value).unwrap()
}

fn task_id(value: &str) -> TaskId {
    TaskId::try_from(value).unwrap()
}

fn agent_id(value: &str) -> AgentId {
    AgentId::try_from(value).unwrap()
}

fn run_id(value: &str) -> RunId {
    RunId::try_from(value).unwrap()
}

#[test]
fn request_envelope_has_a_stable_tagged_shape() {
    let request = RequestEnvelope {
        protocol_version: PROTOCOL_VERSION,
        credential: None,
        request: LocalRequest::EventsAfter {
            sequence: 41,
            limit: 100,
        },
    };

    let value = serde_json::to_value(&request).unwrap();
    assert_eq!(value["protocol_version"], PROTOCOL_VERSION);
    assert_eq!(value["request"]["type"], "events_after");
    assert_eq!(value["request"]["data"]["sequence"], 41);
    assert_eq!(value["request"]["data"]["limit"], 100);
    assert_eq!(
        serde_json::from_value::<RequestEnvelope>(value).unwrap(),
        request
    );
}

#[test]
fn request_credentials_are_outer_opaque_bearers_and_debug_is_redacted() {
    let credential = RequestCredential::new("attempt-secret".into()).unwrap();
    let request = RequestEnvelope::authenticated(
        LocalRequest::CompleteAttempt {
            result: "done".into(),
        },
        credential.clone(),
    );
    let value = serde_json::to_value(&request).unwrap();

    assert_eq!(value["credential"], "attempt-secret");
    assert_eq!(value["request"]["type"], "complete_attempt");
    assert_eq!(
        value["request"]["data"],
        serde_json::json!({"result": "done"})
    );
    assert!(!format!("{request:?}").contains("attempt-secret"));
    assert_eq!(credential.expose_secret(), "attempt-secret");
    assert_eq!(
        serde_json::from_value::<RequestEnvelope>(value).unwrap(),
        request
    );

    assert!(RequestCredential::new(String::new()).is_err());
    assert!(RequestCredential::new("x".repeat(MAX_REQUEST_CREDENTIAL_BYTES + 1)).is_err());
    assert!(serde_json::from_value::<RequestCredential>(serde_json::json!("")).is_err());
}

#[test]
fn live_detail_requests_and_event_head_have_stable_shapes() {
    let request = LocalRequest::GetTask {
        project_id: project_id("project-1"),
        task_id: task_id("task-1"),
    };
    let head = LocalResponse::EventHead { sequence: 41 };

    let request = serde_json::to_value(request).unwrap();
    assert_eq!(request["type"], "get_task");
    assert_eq!(request["data"]["project_id"], "project-1");
    assert_eq!(request["data"]["task_id"], "task-1");
    assert_eq!(
        serde_json::to_value(head).unwrap(),
        serde_json::json!({"type":"event_head","data":{"sequence":41}})
    );
}

#[test]
fn cancel_request_and_finalizing_response_have_stable_shapes() {
    let stop_request = LocalRequest::CancelRun {
        project_id: project_id("project-1"),
        run_id: run_id("run-1"),
        grace_ms: 2_000,
    };
    let finalizing_response = LocalResponse::AttemptFinalizing {
        run_id: run_id("run-1"),
    };
    let stopped_response = LocalResponse::RunCancelled {
        run_id: run_id("run-1"),
    };

    assert_eq!(
        serde_json::to_value(stop_request).unwrap(),
        serde_json::json!({
            "type": "cancel_run",
            "data": {"project_id": "project-1", "run_id": "run-1", "grace_ms": 2000}
        })
    );
    assert_eq!(
        serde_json::to_value(finalizing_response).unwrap(),
        serde_json::json!({
            "type": "attempt_finalizing",
            "data": {"run_id": "run-1"}
        })
    );
    assert_eq!(
        serde_json::to_value(stopped_response).unwrap(),
        serde_json::json!({"type": "run_cancelled", "data": {"run_id": "run-1"}})
    );
}

#[test]
fn task_responses_include_the_body_without_duplicating_snapshot_fields() {
    let detail = TaskDetail {
        snapshot: TaskSnapshot {
            id: task_id("task-1"),
            project_id: project_id("project-1"),
            parent_task_id: None,
            assigned_agent_id: None,
            title: "Build the client".into(),
            status: TaskStatus::Queued,
            priority: 3,
            created_at_ms: 10,
            updated_at_ms: 10,
        },
        body: "Use the local socket protocol.".into(),
        result: Some("The local socket protocol is ready.".into()),
        blocked_reason: None,
    };
    let response = LocalResponse::TaskCreated {
        task: detail.clone(),
    };

    let value = serde_json::to_value(&response).unwrap();
    assert_eq!(value["type"], "task_created");
    assert_eq!(value["data"]["task"]["snapshot"]["id"], "task-1");
    assert_eq!(
        value["data"]["task"]["body"],
        "Use the local socket protocol."
    );
    assert_eq!(
        value["data"]["task"]["result"],
        "The local socket protocol is ready."
    );
    assert_eq!(
        serde_json::from_value::<LocalResponse>(value).unwrap(),
        response
    );
    let event = serde_json::to_value(FactoryEvent::TaskChanged {
        task: detail.snapshot,
    })
    .unwrap();
    assert!(event["data"]["task"].get("result").is_none());
}

#[test]
fn agent_creation_and_task_start_have_small_truthful_wire_shapes() {
    let create = LocalRequest::CreateAgent {
        id: agent_id("agent-1"),
        project_id: project_id("project-1"),
        parent_agent_id: Some(agent_id("agent-parent")),
        role: AgentRole::Worker,
        provider: Provider::Codex,
        model: None,
        reasoning_effort: None,
        model_selection_reason: None,
    };
    let start = LocalRequest::StartTask {
        project_id: project_id("project-1"),
        task_id: task_id("task-1"),
        agent_id: agent_id("agent-1"),
    };
    let created = LocalResponse::AgentCreated {
        agent: AgentSnapshot {
            id: agent_id("agent-1"),
            project_id: project_id("project-1"),
            parent_agent_id: Some(agent_id("agent-parent")),
            role: AgentRole::Worker,
            provider: Provider::Codex,
            current_run_id: None,
            paused: false,
            created_at_ms: 10,
            updated_at_ms: 10,
        },
    };
    let accepted = LocalResponse::RunAccepted {
        run_id: run_id("run-1"),
    };

    let create = serde_json::to_value(create).unwrap();
    assert_eq!(create["type"], "create_agent");
    assert_eq!(create["data"]["role"], "worker");
    assert_eq!(create["data"]["provider"], "codex");

    let start = serde_json::to_value(start).unwrap();
    assert_eq!(start["type"], "start_task");
    assert_eq!(start["data"]["task_id"], "task-1");
    assert!(start["data"].get("worktree").is_none());
    assert!(start["data"].get("parent_run_id").is_none());
    assert!(start["data"].get("body").is_none());
    assert!(start["data"].get("provider_session_id").is_none());

    assert_eq!(
        serde_json::to_value(created).unwrap()["type"],
        "agent_created"
    );
    assert_eq!(
        serde_json::to_value(accepted).unwrap(),
        serde_json::json!({"type":"run_accepted","data":{"run_id":"run-1"}})
    );
}

#[test]
fn agent_creation_can_carry_an_optional_model_without_exposing_an_id_field_contract() {
    let request = LocalRequest::CreateAgent {
        id: agent_id("agent-1"),
        project_id: project_id("project-1"),
        parent_agent_id: Some(agent_id("god")),
        role: AgentRole::Worker,
        provider: Provider::Codex,
        model: Some("gpt-5-codex".into()),
        reasoning_effort: None,
        model_selection_reason: None,
    };
    let value = serde_json::to_value(request).unwrap();
    assert_eq!(value["data"]["model"], "gpt-5-codex");
}

#[test]
fn operator_messages_have_a_private_durable_wire_shape() {
    let message = AgentMessage {
        id: factory_core::MessageId::try_from("message-1").unwrap(),
        project_id: project_id("project-1"),
        sender_agent_id: None,
        recipient_agent_id: agent_id("god"),
        body: "Please inspect the failing launch before the next task.".into(),
        created_at_ms: 10,
        delivered_at_ms: None,
    };
    let request = LocalRequest::SendAgentMessage {
        id: factory_core::MessageId::try_from("message-1").unwrap(),
        project_id: project_id("project-1"),
        recipient_agent_id: agent_id("god"),
        body: message.body.clone(),
    };
    let response = LocalResponse::AgentMessageSent { message };

    assert_eq!(
        serde_json::to_value(request).unwrap()["type"],
        "send_agent_message"
    );
    assert_eq!(
        serde_json::to_value(response).unwrap()["type"],
        "agent_message_sent"
    );
}

#[test]
fn agent_profile_is_available_only_through_private_local_detail() {
    let agent = AgentSnapshot {
        id: agent_id("god"),
        project_id: project_id("factory"),
        parent_agent_id: None,
        role: AgentRole::Orchestrator,
        provider: Provider::Codex,
        current_run_id: None,
        paused: false,
        created_at_ms: 1,
        updated_at_ms: 2,
    };
    let response = LocalResponse::Agent {
        agent: AgentDetail {
            snapshot: agent.clone(),
            profile: AgentProfile {
                model: Some("gpt-5-codex".into()),
                reasoning_effort: None,
                model_selection_reason: None,
                permission_mode: Some("on-request".into()),
                instructions: "Orchestrate the factory.".into(),
                memory: "Prefer narrow slices.".into(),
                updated_at_ms: 3,
            },
            instructions_path:
                "/home/user/.dark-factory/projects/factory/agents/god/instructions.md".into(),
            instructions_health: Default::default(),
            memory_path: "/home/user/.dark-factory/projects/factory/agents/god/memory.md".into(),
            memory_health: Default::default(),
            project_guidance_path: "/home/user/.dark-factory/projects/factory/PROJECT.md".into(),
        },
    };
    let value = serde_json::to_value(response).unwrap();
    assert_eq!(value["data"]["agent"]["profile"]["model"], "gpt-5-codex");
    assert!(
        serde_json::to_value(FactoryEvent::AgentChanged { agent })
            .unwrap()
            .get("profile")
            .is_none()
    );
}

#[test]
fn server_frames_version_responses_and_events_at_the_outer_boundary() {
    let frame = ServerFrame::Response {
        protocol_version: PROTOCOL_VERSION,
        response: LocalResponse::Projects {
            projects: vec![ProjectSnapshot {
                id: project_id("project-1"),
                name: "Dark Factory".into(),
                root: "/work/dark-factory".into(),
                created_at_ms: 1,
                updated_at_ms: 1,
            }],
            next_after_id: None,
        },
    };

    let value = serde_json::to_value(&frame).unwrap();
    assert_eq!(value["type"], "response");
    assert_eq!(value["data"]["protocol_version"], PROTOCOL_VERSION);
    assert_eq!(value["data"]["response"]["type"], "projects");
    assert_eq!(frame.protocol_version(), PROTOCOL_VERSION);
}

#[test]
fn errors_are_explicit_machine_readable_responses() {
    let frame = ServerFrame::Response {
        protocol_version: PROTOCOL_VERSION,
        response: LocalResponse::Error {
            code: ErrorCode::Conflict,
            message: "project root already exists".into(),
        },
    };

    let value = serde_json::to_value(frame).unwrap();
    assert_eq!(value["data"]["response"]["type"], "error");
    assert_eq!(value["data"]["response"]["data"]["code"], "conflict");
    assert_eq!(
        value["data"]["response"]["data"]["message"],
        "project root already exists"
    );
}

#[test]
fn subscription_frames_expose_the_durable_replay_boundary() {
    let subscribed = LocalResponse::Subscribed {
        after_sequence: 7,
        replay_through: 12,
    };
    let caught_up = LocalResponse::CaughtUp { sequence: 12 };

    assert_eq!(
        serde_json::to_value(subscribed).unwrap(),
        serde_json::json!({
            "type": "subscribed",
            "data": {"after_sequence": 7, "replay_through": 12}
        })
    );
    assert_eq!(
        serde_json::to_value(caught_up).unwrap(),
        serde_json::json!({"type": "caught_up", "data": {"sequence": 12}})
    );
}

#[test]
fn collection_requests_and_responses_have_stable_cursors() {
    let request = LocalRequest::ListTasks {
        project_id: project_id("project-1"),
        after_id: Some(task_id("task-9")),
        agent_id: None,
        queue_revision: Some(12),
        history: false,
        limit: 10,
    };
    let response = LocalResponse::Tasks {
        tasks: Vec::new(),
        next_after_id: Some(task_id("task-19")),
        queue_revision: Some(12),
    };

    let request = serde_json::to_value(request).unwrap();
    assert_eq!(request["data"]["after_id"], "task-9");
    assert_eq!(request["data"]["limit"], 10);
    let response = serde_json::to_value(response).unwrap();
    assert_eq!(response["data"]["next_after_id"], "task-19");
}

#[test]
fn retry_task_has_a_small_versioned_local_shape() {
    let request = LocalRequest::RetryTask {
        project_id: project_id("factory"),
        task_id: task_id("task-1"),
    };
    assert_eq!(
        serde_json::to_value(request).unwrap(),
        serde_json::json!({
            "type": "retry_task",
            "data": {"project_id": "factory", "task_id": "task-1"}
        })
    );

    let response = LocalResponse::TaskRetried {
        task: TaskDetail {
            snapshot: TaskSnapshot {
                id: task_id("task-1"),
                project_id: project_id("factory"),
                parent_task_id: None,
                assigned_agent_id: None,
                title: "Retry".into(),
                status: TaskStatus::Queued,
                priority: 0,
                created_at_ms: 1,
                updated_at_ms: 2,
            },
            body: "body".into(),
            result: None,
            blocked_reason: None,
        },
    };
    let value = serde_json::to_value(&response).unwrap();
    assert_eq!(value["type"], "task_retried");
    let decoded = serde_json::from_value::<LocalResponse>(value).unwrap();
    assert_eq!(decoded, response);
}

#[test]
fn assign_task_has_a_small_versioned_local_shape() {
    let request = LocalRequest::AssignTask {
        project_id: project_id("factory"),
        task_id: task_id("task-1"),
        agent_id: Some(agent_id("curie")),
    };
    assert_eq!(
        serde_json::to_value(request).unwrap(),
        serde_json::json!({
            "type": "assign_task",
            "data": {
                "project_id": "factory",
                "task_id": "task-1",
                "agent_id": "curie"
            }
        })
    );

    let unassign = LocalRequest::AssignTask {
        project_id: project_id("factory"),
        task_id: task_id("task-1"),
        agent_id: None,
    };
    assert_eq!(
        serde_json::to_value(unassign).unwrap(),
        serde_json::json!({
            "type": "assign_task",
            "data": {"project_id": "factory", "task_id": "task-1"}
        })
    );

    let response = LocalResponse::TaskAssigned {
        task: TaskDetail {
            snapshot: TaskSnapshot {
                id: task_id("task-1"),
                project_id: project_id("factory"),
                parent_task_id: None,
                assigned_agent_id: Some(agent_id("curie")),
                title: "Assign me".into(),
                status: TaskStatus::Queued,
                priority: 0,
                created_at_ms: 1,
                updated_at_ms: 2,
            },
            body: "body".into(),
            result: None,
            blocked_reason: None,
        },
    };
    let value = serde_json::to_value(&response).unwrap();
    assert_eq!(value["type"], "task_assigned");
    assert_eq!(
        serde_json::from_value::<LocalResponse>(value).unwrap(),
        response
    );
}

#[test]
fn the_largest_valid_task_page_fits_one_local_frame() {
    let tasks = (0..10)
        .map(|index| TaskDetail {
            snapshot: TaskSnapshot {
                id: task_id(&format!("task-{index}")),
                project_id: project_id("project-1"),
                parent_task_id: None,
                assigned_agent_id: None,
                title: "x".repeat(240),
                status: TaskStatus::Queued,
                priority: 0,
                created_at_ms: i64::MAX,
                updated_at_ms: i64::MAX,
            },
            body: "x".repeat(MAX_TASK_BODY_BYTES),
            result: None,
            blocked_reason: None,
        })
        .collect();
    let frame = ServerFrame::Response {
        protocol_version: PROTOCOL_VERSION,
        response: LocalResponse::Tasks {
            tasks,
            next_after_id: Some(task_id("task-9")),
            queue_revision: Some(12),
        },
    };

    assert!(serde_json::to_vec(&frame).unwrap().len() <= MAX_LOCAL_FRAME_BYTES);
}

#[test]
fn provider_hook_carries_an_opaque_payload_and_its_reply_is_printed_verbatim() {
    let request = LocalRequest::ProviderHook {
        event: ProviderHookEvent::PreToolUse,
        payload: serde_json::json!({"tool_name": "Read"}),
    };
    let value = serde_json::to_value(&request).unwrap();
    assert_eq!(
        value,
        serde_json::json!({
            "type": "provider_hook",
            "data": {
                "event": "pre_tool_use",
                "payload": {"tool_name": "Read"}
            }
        })
    );
    assert_eq!(
        serde_json::from_value::<LocalRequest>(value).unwrap(),
        request
    );

    let reply = LocalResponse::ProviderHookReply {
        reply: serde_json::json!({"decision": "block", "reason": "deliver task-1"}),
    };
    let value = serde_json::to_value(&reply).unwrap();
    assert_eq!(
        value,
        serde_json::json!({
            "type": "provider_hook_reply",
            "data": {"reply": {"decision": "block", "reason": "deliver task-1"}}
        })
    );
    assert_eq!(
        serde_json::from_value::<LocalResponse>(value).unwrap(),
        reply
    );
}

#[test]
fn obsolete_provider_hook_events_are_rejected_by_the_wire_type() {
    let value = serde_json::json!({
        "type": "provider_hook",
        "data": {"event": "stop", "payload": {}}
    });
    assert!(serde_json::from_value::<LocalRequest>(value).is_err());
}

#[test]
fn attempt_outcomes_cannot_select_project_task_or_run() {
    assert_eq!(
        serde_json::to_value(LocalRequest::PauseAgent {
            project_id: project_id("project-1"),
            agent_id: agent_id("agent-1"),
        })
        .unwrap(),
        serde_json::json!({
            "type": "pause_agent",
            "data": {"project_id": "project-1", "agent_id": "agent-1"}
        })
    );
    assert_eq!(
        serde_json::to_value(LocalRequest::CompleteAttempt {
            result: "done".into(),
        })
        .unwrap(),
        serde_json::json!({
            "type": "complete_attempt",
            "data": {"result": "done"}
        })
    );
    assert_eq!(
        serde_json::to_value(LocalRequest::BlockAttempt {
            reason: "dependency".into(),
        })
        .unwrap(),
        serde_json::json!({"type": "block_attempt", "data": {"reason": "dependency"}})
    );
}

#[test]
fn health_version_is_additive_so_a_new_client_reads_an_old_daemon() {
    let old_daemon = serde_json::json!({
        "type": "health",
        "data": { "runner_path": "/r", "factoryctl_path": "/c" }
    });
    assert_eq!(
        serde_json::from_value::<LocalResponse>(old_daemon).unwrap(),
        LocalResponse::Health {
            runner_path: "/r".to_owned(),
            factoryctl_path: "/c".to_owned(),
            version: String::new(),
            process_id: 0,
        }
    );
    let value = serde_json::to_value(LocalResponse::Health {
        runner_path: "/r".to_owned(),
        factoryctl_path: "/c".to_owned(),
        version: "0.1.0".to_owned(),
        process_id: 0,
    })
    .unwrap();
    assert_eq!(value["data"]["version"], "0.1.0");
    assert_eq!(value["data"]["process_id"], 0);
}

#[test]
fn status_requests_have_stable_shapes() {
    assert_eq!(
        serde_json::to_value(LocalRequest::SetAutoMode { enabled: false }).unwrap(),
        serde_json::json!({"type":"set_auto_mode","data":{"enabled":false}})
    );
    let fleet = serde_json::to_value(LocalRequest::FleetStatus).unwrap();
    assert_eq!(fleet["type"], "fleet_status");
    let agent = serde_json::to_value(LocalRequest::AgentStatus {
        project_id: project_id("project-1"),
        agent_id: agent_id("agent-1"),
    })
    .unwrap();
    assert_eq!(agent["type"], "agent_status");
    assert_eq!(agent["data"]["project_id"], "project-1");
    assert_eq!(agent["data"]["agent_id"], "agent-1");

    let status = factory_core::status::FleetStatus {
        auto_mode: true,
        generated_at_ms: 7,
        event_sequence: 9,
        active_run_cap: 4,
        active_runs: 1,
        projects: Vec::new(),
        attention: Vec::new(),
    };
    let value = serde_json::to_value(LocalResponse::FleetStatus {
        status: status.clone(),
    })
    .unwrap();
    assert_eq!(value["type"], "fleet_status");
    assert_eq!(value["data"]["status"]["active_run_cap"], 4);
    assert_eq!(value["data"]["status"]["auto_mode"], true);
    assert_eq!(value["data"]["status"]["active_runs"], 1);
    assert_eq!(value["data"]["status"]["event_sequence"], 9);
    assert_eq!(value["data"]["status"]["projects"], serde_json::json!([]));
    assert_eq!(
        serde_json::from_value::<LocalResponse>(value).unwrap(),
        LocalResponse::FleetStatus { status }
    );
}
