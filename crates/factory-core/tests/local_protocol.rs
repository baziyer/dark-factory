use factory_core::{
    PROTOCOL_VERSION, ProjectId, ProjectSnapshot, TaskDetail, TaskId, TaskSnapshot, TaskStatus,
    local::{
        ErrorCode, LocalRequest, LocalResponse, MAX_LOCAL_FRAME_BYTES, MAX_TASK_BODY_BYTES,
        RequestEnvelope, ServerFrame,
    },
};

fn project_id(value: &str) -> ProjectId {
    ProjectId::try_from(value).unwrap()
}

fn task_id(value: &str) -> TaskId {
    TaskId::try_from(value).unwrap()
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
        serde_json::from_value::<LocalResponse>(value).unwrap(),
        response
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
