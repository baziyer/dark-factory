use factory_core::{
    AgentId, AgentRole, AgentSnapshot, FactoryEvent, PROTOCOL_VERSION, ProjectId, ProjectSnapshot,
    Provider, RunId, TaskDetail, TaskId, TaskSnapshot, TaskStatus,
    local::{
        ErrorCode, LocalRequest, LocalResponse, MAX_LOCAL_FRAME_BYTES, MAX_TASK_BODY_BYTES,
        RequestEnvelope, ServerFrame, SubscriptionLimitWindow, SubscriptionProviderStatus,
        SubscriptionSeverity, SubscriptionUsageStatus,
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
fn task_responses_include_the_body_without_duplicating_snapshot_fields() {
    let detail = TaskDetail {
        snapshot: TaskSnapshot {
            id: task_id("task-1"),
            project_id: project_id("project-1"),
            parent_task_id: None,
            depends_on: Vec::new(),
            assigned_agent_id: None,
            title: "Build the client".into(),
            status: TaskStatus::Queued,
            priority: 3,
            created_at_ms: 10,
            updated_at_ms: 10,
        },
        body: "Use the local socket protocol.".into(),
        result: Some("The local socket protocol is ready.".into()),
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
    };
    let start = LocalRequest::StartTask {
        project_id: project_id("project-1"),
        task_id: task_id("task-1"),
        agent_id: agent_id("agent-1"),
        parent_run_id: Some(run_id("run-parent")),
        worktree: "/work/dark-factory-agent-1".into(),
    };
    let created = LocalResponse::AgentCreated {
        agent: AgentSnapshot {
            id: agent_id("agent-1"),
            project_id: project_id("project-1"),
            parent_agent_id: Some(agent_id("agent-parent")),
            role: AgentRole::Worker,
            provider: Provider::Codex,
            current_run_id: None,
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
fn subscription_headroom_has_a_small_normalized_local_shape() {
    let request = LocalRequest::SubscriptionUsage;
    let response = LocalResponse::SubscriptionUsage {
        usage: SubscriptionUsageStatus {
            overall_severity: SubscriptionSeverity::Warning,
            providers: vec![SubscriptionProviderStatus {
                provider: Provider::Codex,
                last_attempt_at_ms: 50,
                last_success_at_ms: Some(50),
                used_percent: Some(82),
                limit_window: Some(SubscriptionLimitWindow::Secondary),
                resets_at_ms: Some(1_000),
                exhausted: Some(false),
                severity: SubscriptionSeverity::Warning,
                consecutive_failures: 0,
            }],
        },
    };

    assert_eq!(
        serde_json::to_value(request).unwrap()["type"],
        "subscription_usage"
    );
    let value = serde_json::to_value(&response).unwrap();
    assert_eq!(value["type"], "subscription_usage");
    assert_eq!(value["data"]["usage"]["providers"][0]["used_percent"], 82);
    assert!(serde_json::from_value::<LocalResponse>(value).is_ok());
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
        limit: 10,
    };
    let response = LocalResponse::Tasks {
        tasks: Vec::new(),
        next_after_id: Some(task_id("task-19")),
    };

    let request = serde_json::to_value(request).unwrap();
    assert_eq!(request["data"]["after_id"], "task-9");
    assert_eq!(request["data"]["limit"], 10);
    let response = serde_json::to_value(response).unwrap();
    assert_eq!(response["data"]["next_after_id"], "task-19");
}

#[test]
fn the_largest_valid_task_page_fits_one_local_frame() {
    let tasks = (0..10)
        .map(|index| TaskDetail {
            snapshot: TaskSnapshot {
                id: task_id(&format!("task-{index}")),
                project_id: project_id("project-1"),
                parent_task_id: None,
                depends_on: Vec::new(),
                assigned_agent_id: None,
                title: "x".repeat(240),
                status: TaskStatus::Queued,
                priority: 0,
                created_at_ms: i64::MAX,
                updated_at_ms: i64::MAX,
            },
            body: "x".repeat(MAX_TASK_BODY_BYTES),
            result: None,
        })
        .collect();
    let frame = ServerFrame::Response {
        protocol_version: PROTOCOL_VERSION,
        response: LocalResponse::Tasks {
            tasks,
            next_after_id: Some(task_id("task-9")),
        },
    };

    assert!(serde_json::to_vec(&frame).unwrap().len() <= MAX_LOCAL_FRAME_BYTES);
}
