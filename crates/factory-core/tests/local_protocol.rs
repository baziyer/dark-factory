use factory_core::{
    AgentId, AgentRole, AgentSnapshot, FactoryEvent, ObserverHealth, PROTOCOL_VERSION, ProjectId,
    ProjectSnapshot, Provider, ProviderHookEvent, RunId, RunnerInstanceId, SessionId,
    SessionSnapshot, SessionState, TaskDetail, TaskId, TaskSnapshot, TaskStatus,
    local::{
        AgentDetail, AgentMessage, AgentProfile, AttachRefusal, AttachRefusalReason, ErrorCode,
        LocalRequest, LocalResponse, MAX_LOCAL_FRAME_BYTES, MAX_TASK_BODY_BYTES, RequestEnvelope,
        RunTerminal, ServerFrame,
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

fn session_id(value: &str) -> SessionId {
    SessionId::try_from(value).unwrap()
}

fn runner_instance_id(value: &str) -> RunnerInstanceId {
    RunnerInstanceId::try_from(value).unwrap()
}

#[test]
fn request_envelope_has_a_stable_tagged_shape() {
    let request = RequestEnvelope {
        protocol_version: PROTOCOL_VERSION,
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
fn run_terminal_and_stop_requests_have_additive_local_shapes() {
    let terminal_request = LocalRequest::GetRunTerminal {
        project_id: project_id("project-1"),
        run_id: run_id("run-1"),
    };
    let stop_request = LocalRequest::StopRun {
        project_id: project_id("project-1"),
        run_id: run_id("run-1"),
        grace_ms: 2_000,
    };
    let terminal_response = LocalResponse::RunTerminal {
        terminal: RunTerminal {
            run_id: run_id("run-1"),
            head_sequence: 4,
            output: "[stdout] ready".into(),
            truncated: false,
        },
    };
    let stopped_response = LocalResponse::RunStopped {
        run_id: run_id("run-1"),
    };

    assert_eq!(
        serde_json::to_value(terminal_request).unwrap(),
        serde_json::json!({
            "type": "get_run_terminal",
            "data": {"project_id": "project-1", "run_id": "run-1"}
        })
    );
    assert_eq!(
        serde_json::to_value(stop_request).unwrap(),
        serde_json::json!({
            "type": "stop_run",
            "data": {"project_id": "project-1", "run_id": "run-1", "grace_ms": 2000}
        })
    );
    assert_eq!(
        serde_json::to_value(terminal_response).unwrap(),
        serde_json::json!({
            "type": "run_terminal",
            "data": {"terminal": {
                "run_id": "run-1",
                "head_sequence": 4,
                "output": "[stdout] ready",
                "truncated": false
            }}
        })
    );
    assert_eq!(
        serde_json::to_value(stopped_response).unwrap(),
        serde_json::json!({"type": "run_stopped", "data": {"run_id": "run-1"}})
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
        worktree: None,
    };
    let start = LocalRequest::StartTask {
        project_id: project_id("project-1"),
        task_id: task_id("task-1"),
        agent_id: agent_id("agent-1"),
        parent_run_id: Some(run_id("run-parent")),
        worktree: Some("/work/dark-factory-agent-1".into()),
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
            current_session_id: None,
            worktree: None,
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
    assert_eq!(start["data"]["worktree"], "/work/dark-factory-agent-1");
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
        worktree: None,
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
        sender_agent_id: None,
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
        current_session_id: None,
        worktree: None,
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
            memory_archive_path:
                "/home/user/.dark-factory/projects/factory/agents/god/memory-archive".into(),
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
        token: "hook-token".into(),
        event: ProviderHookEvent::Stop,
        payload: serde_json::json!({"stop_hook_active": true, "session_id": "provider-session-1"}),
    };
    let value = serde_json::to_value(&request).unwrap();
    assert_eq!(
        value,
        serde_json::json!({
            "type": "provider_hook",
            "data": {
                "token": "hook-token",
                "event": "stop",
                "payload": {"stop_hook_active": true, "session_id": "provider-session-1"}
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
fn session_snapshot_omits_unset_optionals_and_session_changed_carries_it() {
    let session = SessionSnapshot {
        id: session_id("session-1"),
        project_id: project_id("project-1"),
        agent_id: agent_id("agent-1"),
        provider: Provider::ClaudeCode,
        runtime_model: None,
        runtime_reasoning_effort: None,
        runtime_permission_mode: None,
        runtime_control_mode: None,
        state: SessionState::Idle,
        state_since_ms: 10,
        worktree: "/work/agent-1".into(),
        provider_session_id: None,
        runner_instance_id: Some(runner_instance_id("runner-1")),
        current_run_id: None,
        activity: None,
        activity_inferred: false,
        last_hook_event: None,
        notification_kind: None,
        last_hook_at_ms: None,
        wait_reason: None,
        observer_reason: None,
        observer_health: ObserverHealth::Unknown,
        observer_health_since_ms: 0,
        started_at_ms: 5,
        updated_at_ms: 10,
        ended_at_ms: None,
        exit_code: None,
        exit_signal: None,
    };
    let value = serde_json::to_value(&session).unwrap();
    assert_eq!(
        value,
        serde_json::json!({
            "id": "session-1",
            "project_id": "project-1",
            "agent_id": "agent-1",
            "provider": "claude_code",
            "state": "idle",
            "state_since_ms": 10,
            "worktree": "/work/agent-1",
            "activity_inferred": false,
            "observer_health": "unknown",
            "observer_health_since_ms": 0,
            "started_at_ms": 5,
            "updated_at_ms": 10
        })
    );
    assert_eq!(
        serde_json::from_value::<SessionSnapshot>(value).unwrap(),
        session
    );

    let event = FactoryEvent::SessionChanged {
        session: session.clone(),
    };
    let value = serde_json::to_value(&event).unwrap();
    assert_eq!(value["type"], "session_changed");
    assert_eq!(value["data"]["session"]["state"], "idle");
    assert_eq!(
        serde_json::from_value::<FactoryEvent>(value).unwrap(),
        event
    );
}

#[test]
fn an_old_session_snapshot_without_runtime_metadata_is_still_readable() {
    let old_wire = serde_json::json!({
        "id": "session-1",
        "project_id": "project-1",
        "agent_id": "agent-1",
        "provider": "codex",
        "state": "stopped",
        "state_since_ms": 10,
        "worktree": "/work/agent-1",
        "activity_inferred": false,
        "observer_health": "unknown",
        "observer_health_since_ms": 0,
        "started_at_ms": 5,
        "updated_at_ms": 10
    });
    let session: SessionSnapshot = serde_json::from_value(old_wire).unwrap();
    assert_eq!(session.runtime_model, None);
    assert_eq!(session.runtime_reasoning_effort, None);
    assert_eq!(session.runtime_permission_mode, None);
    assert_eq!(session.runtime_control_mode, None);
}

#[test]
fn session_snapshot_carries_its_bounded_optionals_when_set() {
    let session = SessionSnapshot {
        id: session_id("session-1"),
        project_id: project_id("project-1"),
        agent_id: agent_id("agent-1"),
        provider: Provider::Codex,
        runtime_model: None,
        runtime_reasoning_effort: None,
        runtime_permission_mode: None,
        runtime_control_mode: None,
        state: SessionState::WaitingForInput,
        state_since_ms: 20,
        worktree: "/work/agent-1".into(),
        provider_session_id: Some("thread-1".into()),
        runner_instance_id: Some(runner_instance_id("runner-1")),
        current_run_id: Some(run_id("run-1")),
        activity: Some("tool: Read".into()),
        activity_inferred: true,
        last_hook_event: Some(ProviderHookEvent::Notification),
        notification_kind: None,
        last_hook_at_ms: Some(19),
        wait_reason: Some("permission prompt".into()),
        observer_reason: None,
        observer_health: ObserverHealth::Healthy,
        observer_health_since_ms: 15,
        started_at_ms: 5,
        updated_at_ms: 20,
        ended_at_ms: None,
        exit_code: None,
        exit_signal: None,
    };
    let value = serde_json::to_value(&session).unwrap();
    assert_eq!(value["provider_session_id"], "thread-1");
    assert_eq!(value["current_run_id"], "run-1");
    assert_eq!(value["activity"], "tool: Read");
    assert_eq!(value["activity_inferred"], true);
    assert_eq!(value["last_hook_event"], "notification");
    assert_eq!(value["wait_reason"], "permission prompt");
    assert_eq!(
        serde_json::from_value::<SessionSnapshot>(value).unwrap(),
        session
    );
}

#[test]
fn terminal_requests_and_frames_are_keyed_by_session_id() {
    let attach = LocalRequest::AttachTerminal {
        project_id: project_id("project-1"),
        session_id: session_id("session-1"),
        since_offset: 4,
    };
    assert_eq!(
        serde_json::to_value(attach).unwrap(),
        serde_json::json!({
            "type": "attach_terminal",
            "data": {"project_id": "project-1", "session_id": "session-1", "since_offset": 4}
        })
    );

    let input = LocalRequest::TerminalInput {
        project_id: project_id("project-1"),
        session_id: session_id("session-1"),
        bytes: "aGk=".into(),
    };
    assert_eq!(
        serde_json::to_value(input).unwrap(),
        serde_json::json!({
            "type": "terminal_input",
            "data": {"project_id": "project-1", "session_id": "session-1", "bytes": "aGk="}
        })
    );

    let accepted = LocalResponse::TerminalInputAccepted {
        session_id: session_id("session-1"),
    };
    assert_eq!(
        serde_json::to_value(accepted).unwrap(),
        serde_json::json!({"type": "terminal_input_accepted", "data": {"session_id": "session-1"}})
    );

    let frame = ServerFrame::TerminalOutput {
        protocol_version: PROTOCOL_VERSION,
        session_id: session_id("session-1"),
        offset: 4,
        bytes: "aGk=".into(),
    };
    let value = serde_json::to_value(&frame).unwrap();
    assert_eq!(value["type"], "terminal_output");
    assert_eq!(value["data"]["session_id"], "session-1");
    assert_eq!(serde_json::from_value::<ServerFrame>(value).unwrap(), frame);

    let ready = ServerFrame::TerminalOutput {
        protocol_version: PROTOCOL_VERSION,
        session_id: session_id("session-1"),
        offset: 4,
        bytes: String::new(),
    };
    assert_eq!(
        serde_json::to_value(&ready).unwrap(),
        serde_json::json!({
            "type": "terminal_output",
            "data": {
                "protocol_version": PROTOCOL_VERSION,
                "session_id": "session-1",
                "offset": 4,
                "bytes": ""
            }
        })
    );
}

#[test]
fn attach_refusal_is_a_bounded_typed_frame_with_session_and_runner_identity() {
    let response = LocalResponse::AttachRefused {
        refusal: AttachRefusal {
            project_id: project_id("project-1"),
            session_id: session_id("session-1"),
            runner_instance_id: Some(RunnerInstanceId::try_from("runner-1").unwrap()),
            session_state: Some(SessionState::Idle),
            reason: AttachRefusalReason::RunnerReplaced,
        },
    };
    let value = serde_json::to_value(&response).unwrap();
    assert_eq!(value["type"], "attach_refused");
    assert_eq!(value["data"]["refusal"]["reason"], "runner_replaced");
    assert_eq!(value["data"]["refusal"]["session_id"], "session-1");
    assert_eq!(value["data"]["refusal"]["runner_instance_id"], "runner-1");
    assert_eq!(
        serde_json::from_value::<LocalResponse>(value).unwrap(),
        response
    );
}

#[test]
fn session_lifecycle_requests_have_small_versioned_local_shapes() {
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
        serde_json::to_value(LocalRequest::CompleteTask {
            project_id: project_id("project-1"),
            task_id: task_id("task-1"),
            result: "done".into(),
        })
        .unwrap(),
        serde_json::json!({
            "type": "complete_task",
            "data": {"project_id": "project-1", "task_id": "task-1", "result": "done"}
        })
    );
    assert_eq!(
        serde_json::to_value(LocalRequest::ListSessions {
            project_id: project_id("project-1"),
            after_id: None,
            limit: None,
        })
        .unwrap(),
        serde_json::json!({"type": "list_sessions", "data": {"project_id": "project-1"}})
    );
    assert_eq!(
        serde_json::to_value(LocalResponse::SessionStopped {
            session_id: session_id("session-1"),
        })
        .unwrap(),
        serde_json::json!({"type": "session_stopped", "data": {"session_id": "session-1"}})
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
        live_session_cap: 4,
        live_sessions: 1,
        projects: Vec::new(),
        attention: Vec::new(),
    };
    let value = serde_json::to_value(LocalResponse::FleetStatus {
        status: status.clone(),
    })
    .unwrap();
    assert_eq!(value["type"], "fleet_status");
    assert_eq!(value["data"]["status"]["live_session_cap"], 4);
    assert_eq!(value["data"]["status"]["auto_mode"], true);
    assert_eq!(value["data"]["status"]["live_sessions"], 1);
    assert_eq!(value["data"]["status"]["event_sequence"], 9);
    assert_eq!(value["data"]["status"]["projects"], serde_json::json!([]));
    assert_eq!(
        serde_json::from_value::<LocalResponse>(value).unwrap(),
        LocalResponse::FleetStatus { status }
    );
}

#[test]
fn protocol_v1_attention_rows_decode_in_both_old_and_new_shapes() {
    use factory_core::status::{AttentionAction, AttentionKind, AttentionReasonKind};
    use serde::Deserialize;

    let old_daemon = serde_json::json!({
        "type": "fleet_status",
        "data": {"status": {
            "generated_at_ms": 7,
            "auto_mode": true,
            "live_session_cap": 4,
            "live_sessions": 1,
            "projects": [],
            "attention": [{
                "kind": "needs_input",
                "level": "needs_input",
                "project_id": "project-1",
                "agent_id": "agent-1",
                "session_id": "session-1",
                "since_ms": 6,
                "detail": "legacy provider wait"
            }]
        }}
    });
    let LocalResponse::FleetStatus { status } =
        serde_json::from_value::<LocalResponse>(old_daemon).unwrap()
    else {
        panic!("expected fleet status")
    };
    assert_eq!(status.event_sequence, -1);
    assert_eq!(status.attention.len(), 1);
    assert_eq!(
        status.attention[0].reason.kind,
        AttentionReasonKind::Inferred
    );
    assert_eq!(
        status.attention[0].reason.action,
        AttentionAction::InspectInferredState
    );

    #[derive(Deserialize)]
    struct OldAttentionItem {
        kind: AttentionKind,
        detail: String,
    }
    #[derive(Deserialize)]
    struct OldFleetStatus {
        attention: Vec<OldAttentionItem>,
    }
    let new_daemon = serde_json::to_value(LocalResponse::FleetStatus {
        status: factory_core::status::FleetStatus {
            generated_at_ms: 8,
            event_sequence: 12,
            auto_mode: true,
            live_session_cap: 4,
            live_sessions: 1,
            projects: Vec::new(),
            attention: status.attention,
        },
    })
    .unwrap();
    let old = OldFleetStatus::deserialize(&new_daemon["data"]["status"]).unwrap();
    assert_eq!(old.attention.len(), 1);
    assert_eq!(old.attention[0].kind, AttentionKind::NeedsInput);
    assert_eq!(old.attention[0].detail, "legacy provider wait");
}
