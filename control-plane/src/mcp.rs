use axum::{
    body::Bytes,
    extract::State,
    http::{HeaderMap, StatusCode, header::CONTENT_TYPE},
    response::{IntoResponse as _, Response},
};
use serde_json::{Map, Value, json};

use crate::{
    BrokerState,
    access::AccessAuthority,
    github_app::{
        AppAuthority, ClosePullRequest, CreateIssue, CreatePullRequest, DispatchControlPlaneDeploy,
        EnqueuePullRequest, MergePullRequestAtHead, ObserveControlPlaneDeploy, ObserveFile,
        ObserveIssue, ObservePullRequestChecks, ObservePullRequestMerge,
        ObservePullRequestWorkflows, ObserveRef, ObserveRelease, ObserveReleaseWorkflow,
        ObserveRepository, ObserveTree, OperationError, PublishCommit, PublishReleaseTag,
        ReadPullRequestJobLog, RecoverRelease, RerunFailedPullRequestJobs, ResolveIssue,
        SubmitPullRequestReview, UpdatePullRequestBody, canonical_operation_id,
    },
    journal::DeliveryJournal,
};

pub(crate) const PATH: &str = "/mcp";
// `publish_commit` permits 50 files with one million encoded characters each.
// Leave room for their paths and the JSON-RPC envelope without accepting an
// unbounded allocation before typed validation.
pub(crate) const MAX_BODY_BYTES: usize = 52 * 1024 * 1024;
const PROTOCOL_VERSION: &str = "2025-06-18";

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct ObserveOperation {
    operation_id: String,
}

#[derive(Clone)]
pub(crate) struct McpState {
    access: AccessAuthority,
    app: AppAuthority,
    journal: DeliveryJournal,
}

impl McpState {
    pub(crate) const fn new(
        access: AccessAuthority,
        app: AppAuthority,
        journal: DeliveryJournal,
    ) -> Self {
        Self {
            access,
            app,
            journal,
        }
    }

    /// Cloudflare Access is this surface's own live dependency, and the App
    /// authority has already been proved by the maintainer readiness path, so
    /// this adds a signal instead of repeating one.
    #[cfg(target_arch = "wasm32")]
    pub(crate) async fn ready(&self) -> Result<(), ()> {
        self.access.ready().await.map_err(|_| ())
    }

    pub(crate) const fn headless(&self) -> bool {
        self.access.headless()
    }
}

#[worker::send]
pub(crate) async fn receive(
    State(state): State<BrokerState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let Some(mcp) = state.mcp.as_ref() else {
        return error_response(StatusCode::SERVICE_UNAVAILABLE, "inactive");
    };
    if mcp.access.authorize(&headers).await.is_err() {
        return error_response(StatusCode::UNAUTHORIZED, "unauthorized");
    }
    match single_header(&headers, CONTENT_TYPE.as_str()) {
        Some(value) if value == "application/json" || value.starts_with("application/json;") => {}
        _ => return error_response(StatusCode::UNSUPPORTED_MEDIA_TYPE, "invalid_content_type"),
    }
    let request: Value = match serde_json::from_slice(&body) {
        Ok(request) => request,
        Err(_) => return json_rpc_error(Value::Null, -32700, "Parse error"),
    };
    dispatch(request, mcp).await
}

async fn dispatch(request: Value, mcp: &McpState) -> Response {
    let Some(request) = request.as_object() else {
        return json_rpc_error(Value::Null, -32600, "Invalid Request");
    };
    if request.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
        return json_rpc_error(Value::Null, -32600, "Invalid Request");
    }
    let Some(method) = request.get("method").and_then(Value::as_str) else {
        return json_rpc_error(Value::Null, -32600, "Invalid Request");
    };
    let Some(id) = request_id(request.get("id")) else {
        return StatusCode::ACCEPTED.into_response();
    };
    match method {
        "initialize" => json_rpc_result(
            id,
            json!({
                "protocolVersion": PROTOCOL_VERSION,
                "capabilities": {"tools": {"listChanged": false}},
                "serverInfo": {"name": "dark-factory-maintainer", "version": "0.1.0"},
                "instructions": "Every tool names its `owner/name` repository, and acts only on repositories this App is installed on. Read status before a write. Every write is exact-head bound and may fail closed."
            }),
        ),
        "tools/list" => json_rpc_result(id, tools()),
        "tools/call" => call_tool(id, request, mcp).await,
        _ => json_rpc_error(id, -32601, "Method not found"),
    }
}

fn workflow_run_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "run_id": {"type": "integer"},
            "run_attempt": {"type": "integer"},
            "name": {"type": "string"},
            "path": {"type": "string"},
            "event": {"type": "string"},
            "status": {"type": "string"},
            "conclusion": {"type": ["string", "null"]},
            "url": {"type": "string"},
            "jobs": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {
                        "job_id": {"type": "integer"},
                        "name": {"type": "string"},
                        "status": {"type": "string"},
                        "conclusion": {"type": ["string", "null"]},
                        "url": {"type": "string"},
                        "steps": {
                            "type": "array",
                            "items": {
                                "type": "object",
                                "properties": {
                                    "name": {"type": "string"},
                                    "status": {"type": "string"},
                                    "conclusion": {"type": ["string", "null"]}
                                },
                                "required": ["name", "status", "conclusion"],
                                "additionalProperties": false
                            }
                        }
                    },
                    "required": ["job_id", "name", "status", "conclusion", "url", "steps"],
                    "additionalProperties": false
                }
            }
        },
        "required": ["run_id", "run_attempt", "name", "path", "event", "status", "conclusion", "url", "jobs"],
        "additionalProperties": false
    })
}

fn release_schema() -> Value {
    json!({
        "type": ["object", "null"],
        "properties": {
            "tag": {"type": "string"},
            "url": {"type": "string"},
            "draft": {"type": "boolean", "const": false},
            "prerelease": {"type": "boolean"},
            "assets": {
                "type": "array",
                "minItems": 5,
                "maxItems": 5,
                "items": {
                    "type": "object",
                    "properties": {
                        "name": {"type": "string"},
                        "size": {"type": "integer", "minimum": 1},
                        "digest": {"type": "string", "pattern": "^sha256:[0-9a-fA-F]{64}$"},
                        "url": {"type": "string"}
                    },
                    "required": ["name", "size", "digest", "url"],
                    "additionalProperties": false
                }
            }
        },
        "required": ["tag", "url", "draft", "prerelease", "assets"],
        "additionalProperties": false
    })
}

fn tools() -> Value {
    json!({"tools": [{
        "name": "maintainer_status",
        "title": "Verify authority and default head",
        "description": "Verify the exact Dark Factory Maintainer GitHub App installation and permission revision, and return the live default branch head before repository operations.",
        "inputSchema": {"type": "object", "properties": {"repository": {"type": "string", "pattern": "^[A-Za-z0-9-]{1,39}/[A-Za-z0-9._-]{1,100}$"}}, "required": ["repository"], "additionalProperties": false},
        "outputSchema": {
            "type": "object",
            "properties": {
                "repository": {"type": "string"},
                "repository_id": {"type": "integer"},
                "permission_revision": {"type": "string"},
                "default_branch": {"type": "string"},
                "default_sha": {"type": "string"}
            },
            "required": ["repository", "repository_id", "permission_revision", "default_branch", "default_sha"],
            "additionalProperties": false
        },
        "annotations": {"readOnlyHint": true, "destructiveHint": false, "openWorldHint": true}
    }, {
        "name": "observe_operation",
        "title": "Observe a durable write operation",
        "description": "Report whether one operation UUID reached the durable journal and return its request digest, state, and completed typed result. This never begins or retries the operation.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "operation_id": {"type": "string", "pattern": "^[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}$"}
            },
            "required": ["operation_id"],
            "additionalProperties": false
        },
        "outputSchema": {
            "type": "object",
            "properties": {
                "operation_id": {"type": "string"},
                "state": {"type": "string", "enum": ["missing", "planned", "executing", "completed", "indeterminate"]},
                "kind": {"type": ["string", "null"]},
                "request_digest": {"type": ["string", "null"]},
                "result": {"type": ["object", "null"]}
            },
            "required": ["operation_id", "state", "kind", "request_digest", "result"],
            "additionalProperties": false
        },
        "annotations": {"readOnlyHint": true, "destructiveHint": false, "openWorldHint": false}
    }, {
        "name": "create_issue",
        "title": "Create a bounded issue",
        "description": "Create one repository-bound issue with a marker containing the durable operation UUID and complete request digest. Replays require the same UUID and request.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "repository": {"type": "string", "pattern": "^[A-Za-z0-9-]{1,39}/[A-Za-z0-9._-]{1,100}$"},
                "operation_id": {"type": "string", "pattern": "^[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}$"},
                "title": {"type": "string", "minLength": 1, "maxLength": 256},
                "body": {"type": "string", "maxLength": 30000}
            },
            "required": ["repository", "operation_id", "title", "body"],
            "additionalProperties": false
        },
        "outputSchema": {
            "type": "object",
            "properties": {
                "number": {"type": "integer"},
                "url": {"type": "string"}
            },
            "required": ["number", "url"],
            "additionalProperties": false
        },
        "annotations": {"readOnlyHint": false, "destructiveHint": false, "idempotentHint": true, "openWorldHint": true}
    }, {
        "name": "observe_file",
        "title": "Read one file at one commit",
        "description": "Return one file's exact bytes, base64 encoded, as of one exact commit, or null when the path does not exist there. Use this to read another agent's work instead of fetching git objects.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "repository": {"type": "string", "pattern": "^[A-Za-z0-9-]{1,39}/[A-Za-z0-9._-]{1,100}$"},
                "commit_sha": {"type": "string", "pattern": "^[0-9a-f]{40}$"},
                "path": {"type": "string", "minLength": 1, "maxLength": 4096}
            },
            "required": ["repository", "commit_sha", "path"],
            "additionalProperties": false
        },
        "outputSchema": {
            "type": "object",
            "properties": {
                "path": {"type": "string"},
                "commit_sha": {"type": "string"},
                "content_base64": {"type": ["string", "null"]}
            },
            "required": ["path", "commit_sha", "content_base64"],
            "additionalProperties": false
        },
        "annotations": {"readOnlyHint": true, "destructiveHint": false, "openWorldHint": true}
    }, {
        "name": "observe_tree",
        "title": "List one commit's tree",
        "description": "Return one exact commit's parents and every entry in its recursive tree: path, git object kind, file mode and object id. Compare a commit against an ancestor to learn what that branch changed. Only mode 100644 and 100755 round-trip through observe_file and publish_commit; a symlink is a blob with mode 120000.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "repository": {"type": "string", "pattern": "^[A-Za-z0-9-]{1,39}/[A-Za-z0-9._-]{1,100}$"},
                "commit_sha": {"type": "string", "pattern": "^[0-9a-f]{40}$"}
            },
            "required": ["repository", "commit_sha"],
            "additionalProperties": false
        },
        "outputSchema": {
            "type": "object",
            "properties": {
                "commit_sha": {"type": "string"},
                "tree_sha": {"type": "string"},
                "parents": {"type": "array", "items": {"type": "string"}},
                "entries": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "path": {"type": "string"},
                            "kind": {"type": "string"},
                            "mode": {"type": "string"},
                            "sha": {"type": "string"}
                        },
                        "required": ["path", "kind", "mode", "sha"],
                        "additionalProperties": false
                    }
                }
            },
            "required": ["commit_sha", "tree_sha", "parents", "entries"],
            "additionalProperties": false
        },
        "annotations": {"readOnlyHint": true, "destructiveHint": false, "openWorldHint": true}
    }, {
        "name": "observe_ref",
        "title": "Observe a branch head",
        "description": "Return the exact commit a branch points at now, or null when the branch does not exist. Read this before binding a write to a head you did not just publish.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "repository": {"type": "string", "pattern": "^[A-Za-z0-9-]{1,39}/[A-Za-z0-9._-]{1,100}$"},
                "branch": {"type": "string", "minLength": 1, "maxLength": 240}
            },
            "required": ["repository", "branch"],
            "additionalProperties": false
        },
        "outputSchema": {
            "type": "object",
            "properties": {
                "branch": {"type": "string"},
                "head_sha": {"type": ["string", "null"]}
            },
            "required": ["branch", "head_sha"],
            "additionalProperties": false
        },
        "annotations": {"readOnlyHint": true, "destructiveHint": false, "openWorldHint": true}
    }, {
        "name": "observe_issue",
        "title": "Observe one issue",
        "description": "Return the live state of one repository issue. Pull requests are refused.",
        "inputSchema": {
            "type": "object",
            "properties": {"repository": {"type": "string", "pattern": "^[A-Za-z0-9-]{1,39}/[A-Za-z0-9._-]{1,100}$"}, "issue_number": {"type": "integer", "minimum": 1}},
            "required": ["repository", "issue_number"],
            "additionalProperties": false
        },
        "outputSchema": {
            "type": "object",
            "properties": {
                "number": {"type": "integer"},
                "url": {"type": "string"},
                "state": {"type": "string", "enum": ["open", "closed"]},
                "state_reason": {"type": ["string", "null"]}
            },
            "required": ["number", "url", "state", "state_reason"],
            "additionalProperties": false
        },
        "annotations": {"readOnlyHint": true, "destructiveHint": false, "openWorldHint": true}
    }, {
        "name": "resolve_issue",
        "title": "Resolve an issue with evidence",
        "description": "Post one bounded evidence comment and close the same real issue as completed or not planned. A durable marker reconciles partial completion without duplicate comments.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "repository": {"type": "string", "pattern": "^[A-Za-z0-9-]{1,39}/[A-Za-z0-9._-]{1,100}$"},
                "operation_id": {"type": "string", "pattern": "^[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}$"},
                "issue_number": {"type": "integer", "minimum": 1},
                "body": {"type": "string", "minLength": 1, "maxLength": 16000},
                "state_reason": {"type": "string", "enum": ["completed", "not_planned"]}
            },
            "required": ["repository", "operation_id", "issue_number", "body", "state_reason"],
            "additionalProperties": false
        },
        "outputSchema": {
            "type": "object",
            "properties": {
                "number": {"type": "integer"},
                "url": {"type": "string"},
                "comment_url": {"type": "string"},
                "state": {"type": "string", "const": "closed"},
                "state_reason": {"type": "string", "enum": ["completed", "not_planned"]}
            },
            "required": ["number", "url", "comment_url", "state", "state_reason"],
            "additionalProperties": false
        },
        "annotations": {"readOnlyHint": false, "destructiveHint": true, "idempotentHint": true, "openWorldHint": true}
    }, {
        "name": "publish_release_tag",
        "title": "Publish an immutable release tag",
        "description": "Create one semver release tag only at the live default-branch commit. Existing exact tags reconcile; moved tags conflict.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "repository": {"type": "string", "pattern": "^[A-Za-z0-9-]{1,39}/[A-Za-z0-9._-]{1,100}$"},
                "operation_id": {"type": "string", "pattern": "^[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}$"},
                "tag": {"type": "string", "pattern": "^v[0-9]+\\.[0-9]+\\.[0-9]+(?:-[A-Za-z0-9.-]+)?$"},
                "commit_sha": {"type": "string", "pattern": "^[0-9a-f]{40}$"}
            },
            "required": ["repository", "operation_id", "tag", "commit_sha"],
            "additionalProperties": false
        },
        "outputSchema": {"type": "object", "properties": {"tag": {"type": "string"}, "commit_sha": {"type": "string"}}, "required": ["tag", "commit_sha"], "additionalProperties": false},
        "annotations": {"readOnlyHint": false, "destructiveHint": false, "idempotentHint": true, "openWorldHint": true}
    }, {
        "name": "recover_release",
        "title": "Recover an exact release workflow",
        "description": "Dispatch only the fixed release.yml recovery workflow from the exact live default-branch commit for an existing exact tag, then verify GitHub's returned run ID before completion.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "repository": {"type": "string", "pattern": "^[A-Za-z0-9-]{1,39}/[A-Za-z0-9._-]{1,100}$"},
                "operation_id": {"type": "string", "pattern": "^[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}$"},
                "tag": {"type": "string", "pattern": "^v[0-9]+\\.[0-9]+\\.[0-9]+(?:-[A-Za-z0-9.-]+)?$"},
                "commit_sha": {"type": "string", "pattern": "^[0-9a-f]{40}$"},
                "workflow_sha": {"type": "string", "pattern": "^[0-9a-f]{40}$"}
            },
            "required": ["repository", "operation_id", "tag", "commit_sha", "workflow_sha"],
            "additionalProperties": false
        },
        "outputSchema": {"type": "object", "properties": {"operation_id": {"type": "string"}, "workflow": {"type": "string"}, "commit_sha": {"type": "string"}, "run_id": {"type": "integer"}, "run_attempt": {"type": "integer"}}, "required": ["operation_id", "workflow", "commit_sha", "run_id", "run_attempt"], "additionalProperties": false},
        "annotations": {"readOnlyHint": false, "destructiveHint": false, "idempotentHint": true, "openWorldHint": true}
    }, {
        "name": "observe_release",
        "title": "Observe an exact release",
        "description": "Verify an immutable tag and return its release assets plus the one fixed tag-push release workflow run for that exact commit.",
        "inputSchema": {"type": "object", "properties": {"repository": {"type": "string", "pattern": "^[A-Za-z0-9-]{1,39}/[A-Za-z0-9._-]{1,100}$"}, "tag": {"type": "string", "pattern": "^v[0-9]+\\.[0-9]+\\.[0-9]+(?:-[A-Za-z0-9.-]+)?$"}, "commit_sha": {"type": "string", "pattern": "^[0-9a-f]{40}$"}}, "required": ["repository", "tag", "commit_sha"], "additionalProperties": false},
        "outputSchema": {"type": "object", "properties": {"tag": {"type": "string"}, "commit_sha": {"type": "string"}, "release": release_schema(), "workflow_run": workflow_run_schema()}, "required": ["tag", "commit_sha", "release", "workflow_run"], "additionalProperties": false},
        "annotations": {"readOnlyHint": true, "destructiveHint": false, "openWorldHint": true}
    }, {
        "name": "observe_release_workflow",
        "title": "Observe an exact release recovery workflow",
        "description": "Read the one workflow run returned by recover_release and re-prove its fixed workflow, complete request digest, immutable tag, and dispatch commit.",
        "inputSchema": {"type": "object", "properties": {"repository": {"type": "string", "pattern": "^[A-Za-z0-9-]{1,39}/[A-Za-z0-9._-]{1,100}$"}, "operation_id": {"type": "string", "pattern": "^[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}$"}, "tag": {"type": "string", "pattern": "^v[0-9]+\\.[0-9]+\\.[0-9]+(?:-[A-Za-z0-9.-]+)?$"}, "tag_sha": {"type": "string", "pattern": "^[0-9a-f]{40}$"}, "workflow_sha": {"type": "string", "pattern": "^[0-9a-f]{40}$"}, "run_id": {"type": "integer", "minimum": 1}}, "required": ["repository", "operation_id", "tag", "tag_sha", "workflow_sha", "run_id"], "additionalProperties": false},
        "outputSchema": {"type": "object", "properties": {"operation_id": {"type": "string"}, "tag": {"type": "string"}, "tag_sha": {"type": "string"}, "workflow_sha": {"type": "string"}, "workflow_run": workflow_run_schema()}, "required": ["operation_id", "tag", "tag_sha", "workflow_sha", "workflow_run"], "additionalProperties": false},
        "annotations": {"readOnlyHint": true, "destructiveHint": false, "openWorldHint": true}
    }, {
        "name": "dispatch_control_plane_deploy",
        "title": "Dispatch the fixed control-plane deployment",
        "description": "Dispatch only deploy-control-plane.yml from the live default branch, bound to the exact commit and reviewed source tree.",
        "inputSchema": {"type": "object", "properties": {"repository": {"type": "string", "pattern": "^[A-Za-z0-9-]{1,39}/[A-Za-z0-9._-]{1,100}$"}, "operation_id": {"type": "string", "pattern": "^[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}$"}, "commit_sha": {"type": "string", "pattern": "^[0-9a-f]{40}$"}, "reviewed_tree": {"type": "string", "pattern": "^[0-9a-f]{40}$"}, "promote": {"type": "boolean"}}, "required": ["repository", "operation_id", "commit_sha", "reviewed_tree", "promote"], "additionalProperties": false},
        "outputSchema": {"type": "object", "properties": {"operation_id": {"type": "string"}, "workflow": {"type": "string"}, "commit_sha": {"type": "string"}, "run_id": {"type": "integer"}, "run_attempt": {"type": "integer"}}, "required": ["operation_id", "workflow", "commit_sha", "run_id", "run_attempt"], "additionalProperties": false},
        "annotations": {"readOnlyHint": false, "destructiveHint": false, "idempotentHint": true, "openWorldHint": true}
    }, {
        "name": "observe_control_plane_deploy",
        "title": "Observe an exact control-plane deployment",
        "description": "Read the one workflow run returned by dispatch_control_plane_deploy and re-prove its fixed workflow, complete request digest, exact commit, reviewed tree, and promotion mode.",
        "inputSchema": {"type": "object", "properties": {"repository": {"type": "string", "pattern": "^[A-Za-z0-9-]{1,39}/[A-Za-z0-9._-]{1,100}$"}, "operation_id": {"type": "string", "pattern": "^[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}$"}, "commit_sha": {"type": "string", "pattern": "^[0-9a-f]{40}$"}, "reviewed_tree": {"type": "string", "pattern": "^[0-9a-f]{40}$"}, "promote": {"type": "boolean"}, "run_id": {"type": "integer", "minimum": 1}}, "required": ["repository", "operation_id", "commit_sha", "reviewed_tree", "promote", "run_id"], "additionalProperties": false},
        "outputSchema": {"type": "object", "properties": {"operation_id": {"type": "string"}, "commit_sha": {"type": "string"}, "reviewed_tree": {"type": "string"}, "promote": {"type": "boolean"}, "workflow_run": workflow_run_schema()}, "required": ["operation_id", "commit_sha", "reviewed_tree", "promote", "workflow_run"], "additionalProperties": false},
        "annotations": {"readOnlyHint": true, "destructiveHint": false, "openWorldHint": true}
    }, {
        "name": "create_pull_request",
        "title": "Create an exact-head pull request",
        "description": "Create one repository-bound pull request after verifying the exact head and base commit IDs. Replays require the same operation UUID and request.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "repository": {"type": "string", "pattern": "^[A-Za-z0-9-]{1,39}/[A-Za-z0-9._-]{1,100}$"},
                "operation_id": {"type": "string", "pattern": "^[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}$"},
                "issue_number": {"type": "integer", "minimum": 1},
                "head": {"type": "string", "minLength": 1, "maxLength": 240},
                "head_sha": {"type": "string", "pattern": "^[0-9a-f]{40}$"},
                "base": {"type": "string", "minLength": 1, "maxLength": 240},
                "base_sha": {"type": "string", "pattern": "^[0-9a-f]{40}$"},
                "title": {"type": "string", "minLength": 1, "maxLength": 256},
                "body": {"type": "string", "maxLength": 30000},
                "draft": {"type": "boolean"}
            },
            "required": ["repository", "operation_id", "issue_number", "head", "head_sha", "base", "base_sha", "title", "body", "draft"],
            "additionalProperties": false
        },
        "outputSchema": {
            "type": "object",
            "properties": {
                "number": {"type": "integer"},
                "url": {"type": "string"},
                "head_sha": {"type": "string"},
                "base_sha": {"type": "string"}
            },
            "required": ["number", "url", "head_sha", "base_sha"],
            "additionalProperties": false
        },
        "annotations": {"readOnlyHint": false, "destructiveHint": false, "idempotentHint": true, "openWorldHint": true}
    }, {
        "name": "update_pull_request_body",
        "title": "Replace an exact-head pull request body",
        "description": "Replace one open pull request body only while it still names the stated head commit. The App renders its operation marker; the supplied body cannot contain an App operation or review verdict marker. Replays require the same operation UUID and request.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "repository": {"type": "string", "pattern": "^[A-Za-z0-9-]{1,39}/[A-Za-z0-9._-]{1,100}$"},
                "operation_id": {"type": "string", "pattern": "^[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}$"},
                "pull_number": {"type": "integer", "minimum": 1},
                "head_sha": {"type": "string", "pattern": "^[0-9a-f]{40}$"},
                "body": {"type": "string", "maxLength": 30000}
            },
            "required": ["repository", "operation_id", "pull_number", "head_sha", "body"],
            "additionalProperties": false
        },
        "outputSchema": {
            "type": "object",
            "properties": {
                "number": {"type": "integer"},
                "url": {"type": "string"},
                "head_sha": {"type": "string"},
                "base_sha": {"type": "string"}
            },
            "required": ["number", "url", "head_sha", "base_sha"],
            "additionalProperties": false
        },
        "annotations": {"readOnlyHint": false, "destructiveHint": false, "idempotentHint": true, "openWorldHint": true}
    }, {
        "name": "close_pull_request",
        "title": "Close an exact-head pull request",
        "description": "Close one pull request only while it still names the stated head commit. Replays require the same operation UUID and request.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "repository": {"type": "string", "pattern": "^[A-Za-z0-9-]{1,39}/[A-Za-z0-9._-]{1,100}$"},
                "operation_id": {"type": "string", "pattern": "^[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}$"},
                "pull_number": {"type": "integer", "minimum": 1},
                "head_sha": {"type": "string", "pattern": "^[0-9a-f]{40}$"}
            },
            "required": ["repository", "operation_id", "pull_number", "head_sha"],
            "additionalProperties": false
        },
        "outputSchema": {
            "type": "object",
            "properties": {
                "pull_number": {"type": "integer"},
                "head_sha": {"type": "string"},
                "url": {"type": "string"},
                "state": {"type": "string", "const": "closed"}
            },
            "required": ["pull_number", "head_sha", "url", "state"],
            "additionalProperties": false
        },
        "annotations": {"readOnlyHint": false, "destructiveHint": true, "idempotentHint": true, "openWorldHint": true}
    }, {
        "name": "submit_pull_request_review",
        "title": "Submit an exact-head pull request review",
        "description": "Record an adversarial-review verdict against one pull request head commit. ALLOW satisfies the required `review` check; REQUEST_CHANGES blocks it, and a block at a head is cleared only by pushing a fix, never by a second ALLOW at that same head; COMMENT decides nothing. All three are this App's own words and none is a GitHub review state: the App authors the pull requests it reviews and GitHub refuses a self-review either way, so every verdict is submitted as a GitHub COMMENT and the verdict itself rides in a line the App writes. That is the line the `review` check reads, which is why `body` carries the reviewer's findings and must not contain one. Replays require the same operation UUID and request.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "repository": {"type": "string", "pattern": "^[A-Za-z0-9-]{1,39}/[A-Za-z0-9._-]{1,100}$"},
                "operation_id": {"type": "string", "pattern": "^[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}$"},
                "pull_number": {"type": "integer", "minimum": 1},
                "head_sha": {"type": "string", "pattern": "^[0-9a-f]{40}$"},
                "event": {"type": "string", "enum": ["ALLOW", "COMMENT", "REQUEST_CHANGES"]},
                "body": {"type": "string", "minLength": 1, "maxLength": 16000}
            },
            "required": ["repository", "operation_id", "pull_number", "head_sha", "event", "body"],
            "additionalProperties": false
        },
        "outputSchema": {
            "type": "object",
            "properties": {
                "review_id": {"type": "integer"},
                "url": {"type": "string"},
                "head_sha": {"type": "string"},
                "state": {"type": "string"},
                "verdict": {"type": "string", "enum": ["allow", "note", "block"]}
            },
            "required": ["review_id", "url", "head_sha", "state", "verdict"],
            "additionalProperties": false
        },
        "annotations": {"readOnlyHint": false, "destructiveHint": false, "idempotentHint": true, "openWorldHint": true}
    }, {
        "name": "observe_pull_request_checks",
        "title": "Observe exact-head pull request checks",
        "description": "Return the complete bounded set of GitHub check runs for one exact pull request head commit.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "repository": {"type": "string", "pattern": "^[A-Za-z0-9-]{1,39}/[A-Za-z0-9._-]{1,100}$"},
                "pull_number": {"type": "integer", "minimum": 1},
                "head_sha": {"type": "string", "pattern": "^[0-9a-f]{40}$"}
            },
            "required": ["repository", "pull_number", "head_sha"],
            "additionalProperties": false
        },
        "outputSchema": {
            "type": "object",
            "properties": {
                "pull_number": {"type": "integer"},
                "head_sha": {"type": "string"},
                "checks": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "name": {"type": "string"},
                            "status": {"type": "string"},
                            "conclusion": {"type": ["string", "null"]},
                            "url": {"type": "string"}
                        },
                        "required": ["name", "status", "conclusion", "url"],
                        "additionalProperties": false
                    }
                }
            },
            "required": ["pull_number", "head_sha", "checks"],
            "additionalProperties": false
        },
        "annotations": {"readOnlyHint": true, "destructiveHint": false, "openWorldHint": true}
    }, {
        "name": "observe_pull_request_workflows",
        "title": "Observe exact-head pull request workflows",
        "description": "Return the bounded CI workflow runs, jobs, and steps for one exact pull request head and live default base.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "repository": {"type": "string", "pattern": "^[A-Za-z0-9-]{1,39}/[A-Za-z0-9._-]{1,100}$"},
                "workflow_path": {"type": "string", "pattern": "^\\.github/workflows/[A-Za-z0-9._-]{1,100}\\.ya?ml$"},
                "pull_number": {"type": "integer", "minimum": 1},
                "head_sha": {"type": "string", "pattern": "^[0-9a-f]{40}$"},
                "base": {"type": "string", "minLength": 1, "maxLength": 240}
            },
            "required": ["repository", "workflow_path", "pull_number", "head_sha", "base"],
            "additionalProperties": false
        },
        "outputSchema": {
            "type": "object",
            "properties": {
                "pull_number": {"type": "integer"},
                "head_sha": {"type": "string"},
                "base": {"type": "string"},
                "runs": {
                    "type": "array",
                    "items": workflow_run_schema()
                }
            },
            "required": ["pull_number", "head_sha", "base", "runs"],
            "additionalProperties": false
        },
        "annotations": {"readOnlyHint": true, "destructiveHint": false, "openWorldHint": true}
    }, {
        "name": "read_pull_request_job_log",
        "title": "Read one failed pull request job log",
        "description": "Return at most the last 64 KiB of one completed failed job log after re-proving its exact pull request head, base, run, and attempt.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "repository": {"type": "string", "pattern": "^[A-Za-z0-9-]{1,39}/[A-Za-z0-9._-]{1,100}$"},
                "workflow_path": {"type": "string", "pattern": "^\\.github/workflows/[A-Za-z0-9._-]{1,100}\\.ya?ml$"},
                "pull_number": {"type": "integer", "minimum": 1},
                "head_sha": {"type": "string", "pattern": "^[0-9a-f]{40}$"},
                "base": {"type": "string", "minLength": 1, "maxLength": 240},
                "run_id": {"type": "integer", "minimum": 1},
                "run_attempt": {"type": "integer", "minimum": 1},
                "job_id": {"type": "integer", "minimum": 1}
            },
            "required": ["repository", "workflow_path", "pull_number", "head_sha", "base", "run_id", "run_attempt", "job_id"],
            "additionalProperties": false
        },
        "outputSchema": {
            "type": "object",
            "properties": {
                "pull_number": {"type": "integer"},
                "head_sha": {"type": "string"},
                "base": {"type": "string"},
                "run_id": {"type": "integer"},
                "run_attempt": {"type": "integer"},
                "job_id": {"type": "integer"},
                "text": {"type": "string"}
            },
            "required": ["pull_number", "head_sha", "base", "run_id", "run_attempt", "job_id", "text"],
            "additionalProperties": false
        },
        "annotations": {"readOnlyHint": true, "destructiveHint": false, "openWorldHint": true}
    }, {
        "name": "rerun_failed_pull_request_jobs",
        "title": "Rerun failed jobs for an exact pull request workflow",
        "description": "Rerun only failed jobs from one completed failed CI run after re-proving its exact pull request head, base, run, and attempt. Replays require the same operation UUID and request.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "repository": {"type": "string", "pattern": "^[A-Za-z0-9-]{1,39}/[A-Za-z0-9._-]{1,100}$"},
                "workflow_path": {"type": "string", "pattern": "^\\.github/workflows/[A-Za-z0-9._-]{1,100}\\.ya?ml$"},
                "operation_id": {"type": "string", "pattern": "^[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}$"},
                "pull_number": {"type": "integer", "minimum": 1},
                "head_sha": {"type": "string", "pattern": "^[0-9a-f]{40}$"},
                "base": {"type": "string", "minLength": 1, "maxLength": 240},
                "run_id": {"type": "integer", "minimum": 1},
                "run_attempt": {"type": "integer", "minimum": 1}
            },
            "required": ["repository", "workflow_path", "operation_id", "pull_number", "head_sha", "base", "run_id", "run_attempt"],
            "additionalProperties": false
        },
        "outputSchema": {
            "type": "object",
            "properties": {
                "pull_number": {"type": "integer"},
                "head_sha": {"type": "string"},
                "base": {"type": "string"},
                "run_id": {"type": "integer"},
                "run_attempt": {"type": "integer"}
            },
            "required": ["pull_number", "head_sha", "base", "run_id", "run_attempt"],
            "additionalProperties": false
        },
        "annotations": {"readOnlyHint": false, "destructiveHint": false, "idempotentHint": true, "openWorldHint": true}
    }, {
        "name": "observe_pull_request_merge",
        "title": "Observe an exact-head merge outcome",
        "description": "Bind to the completed App enqueue attempt and return whether that exact pull request head is still in its default-branch merge queue, merged after the attempt, or no longer queued. NOT_QUEUED does not guess why the entry disappeared.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "repository": {"type": "string", "pattern": "^[A-Za-z0-9-]{1,39}/[A-Za-z0-9._-]{1,100}$"},
                "enqueue_operation_id": {"type": "string", "pattern": "^[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}$"},
                "pull_number": {"type": "integer", "minimum": 1},
                "head_sha": {"type": "string", "pattern": "^[0-9a-f]{40}$"},
                "base": {"type": "string", "minLength": 1, "maxLength": 240}
            },
            "required": ["repository", "enqueue_operation_id", "pull_number", "head_sha", "base"],
            "additionalProperties": false
        },
        "outputSchema": {
            "type": "object",
            "properties": {
                "pull_number": {"type": "integer"},
                "head_sha": {"type": "string"},
                "base": {"type": "string"},
                "pull_state": {"type": "string", "enum": ["open", "closed"]},
                "state": {"type": "string", "enum": ["ACTIVE_QUEUE", "MERGED_AFTER_ENQUEUE_ATTEMPT", "NOT_QUEUED"]},
                "entry_id": {"type": "string"},
                "queue_state": {"type": ["string", "null"]},
                "merge_commit_sha": {"type": ["string", "null"]}
            },
            "required": ["pull_number", "head_sha", "base", "pull_state", "state", "entry_id", "queue_state", "merge_commit_sha"],
            "additionalProperties": false
        },
        "annotations": {"readOnlyHint": true, "destructiveHint": false, "openWorldHint": true}
    }, {
        "name": "publish_commit",
        "title": "Publish an exact-head commit",
        "description": "Publish one commit to a repository branch, but only while that branch still points at the stated commit. A file with no content is deleted, so a mode may only be stated alongside content. The .github tree itself, .github/workflows, CODEOWNERS and dependabot config cannot be written. Replays require the same operation UUID and request.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "repository": {"type": "string", "pattern": "^[A-Za-z0-9-]{1,39}/[A-Za-z0-9._-]{1,100}$"},
                "operation_id": {"type": "string", "pattern": "^[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}$"},
                "branch": {"type": "string", "minLength": 1, "maxLength": 240},
                "expected_head_sha": {"type": "string", "pattern": "^[0-9a-f]{40}$"},
                "message": {"type": "string", "minLength": 1, "maxLength": 4096},
                "changes": {
                    "type": "array",
                    "minItems": 1,
                    "maxItems": 50,
                    "items": {
                        "type": "object",
                        "properties": {
                            "path": {"type": "string", "minLength": 1, "maxLength": 240},
                            "content_base64": {"type": "string", "maxLength": 1000000},
                            "mode": {"type": "string", "enum": ["100644", "100755"]}
                        },
                        "required": ["path"],
                        "additionalProperties": false
                    }
                }
            },
            "required": ["repository", "operation_id", "branch", "expected_head_sha", "message", "changes"],
            "additionalProperties": false
        },
        "outputSchema": {
            "type": "object",
            "properties": {
                "branch": {"type": "string"},
                "commit_sha": {"type": "string"},
                "parent_sha": {"type": "string"}
            },
            "required": ["branch", "commit_sha", "parent_sha"],
            "additionalProperties": false
        },
        "annotations": {"readOnlyHint": false, "destructiveHint": false, "idempotentHint": true, "openWorldHint": true}
    }, {
        "name": "enqueue_pull_request",
        "title": "Add a pull request to its base branch's merge queue at an exact head",
        "description": "Enqueue one pull request only while its head is still the stated commit and its base is still the stated branch. This operation requires that base's merge queue; it never falls back to direct merge. GitHub tests the entry against the queue's latest base and merges it. A refusal names its typed reason, and the same operation UUID stays retryable. Replays require the same operation UUID and request.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "repository": {"type": "string", "pattern": "^[A-Za-z0-9-]{1,39}/[A-Za-z0-9._-]{1,100}$"},
                "operation_id": {"type": "string", "pattern": "^[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}$"},
                "pull_number": {"type": "integer", "minimum": 1},
                "head_sha": {"type": "string", "pattern": "^[0-9a-f]{40}$"},
                "base": {"type": "string", "minLength": 1, "maxLength": 255}
            },
            "required": ["repository", "operation_id", "pull_number", "head_sha", "base"],
            "additionalProperties": false
        },
        "outputSchema": {
            "type": "object",
            "properties": {
                "pull_number": {"type": "integer"},
                "head_sha": {"type": "string"},
                "entry_id": {"type": "string"},
                "state_when_recorded": {"type": "string", "enum": ["QUEUED", "AWAITING_CHECKS", "MERGEABLE", "UNMERGEABLE", "LOCKED"]}
            },
            "required": ["pull_number", "head_sha", "entry_id", "state_when_recorded"],
            "additionalProperties": false
        },
        "annotations": {"readOnlyHint": false, "destructiveHint": false, "idempotentHint": true, "openWorldHint": true}
    }, {
        "name": "merge_pull_request_at_head",
        "title": "Merge an exact pull request head",
        "description": "Squash-merge one pull request at its exact head into the current default base only after exact-head ALLOW, no BLOCK, and all checks pass. A protected base requires a strict squash ruleset with no App bypass; exact rules-read 403 on a private repo instead requires protected:false, squash enabled, explicit no-queue reads, and an unchanged base re-read. Classic protection alone is unsupported. Replays require the same operation UUID and request.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "repository": {"type": "string", "pattern": "^[A-Za-z0-9-]{1,39}/[A-Za-z0-9._-]{1,100}$"},
                "operation_id": {"type": "string", "pattern": "^[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}$"},
                "review_operation_id": {"type": "string", "pattern": "^[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}$"},
                "pull_number": {"type": "integer", "minimum": 1},
                "head_sha": {"type": "string", "pattern": "^[0-9a-f]{40}$"},
                "base": {"type": "string", "minLength": 1, "maxLength": 240}
            },
            "required": ["repository", "operation_id", "review_operation_id", "pull_number", "head_sha", "base"],
            "additionalProperties": false
        },
        "outputSchema": {
            "type": "object",
            "properties": {
                "pull_number": {"type": "integer"},
                "head_sha": {"type": "string"},
                "base": {"type": "string"},
                "merge_commit_sha": {"type": "string"}
            },
            "required": ["pull_number", "head_sha", "base", "merge_commit_sha"],
            "additionalProperties": false
        },
        "annotations": {"readOnlyHint": false, "destructiveHint": true, "idempotentHint": true, "openWorldHint": true}
    }]})
}

async fn call_tool(id: Value, request: &Map<String, Value>, mcp: &McpState) -> Response {
    let params = request.get("params").and_then(Value::as_object);
    let name = params
        .and_then(|params| params.get("name"))
        .and_then(Value::as_str);
    let arguments = params
        .and_then(|params| params.get("arguments"))
        .cloned()
        .unwrap_or_else(|| json!({}));
    match name {
        Some("maintainer_status") => {
            let Ok(arguments) = serde_json::from_value::<ObserveRepository>(arguments) else {
                return json_rpc_error(id, -32602, "Invalid params");
            };
            match mcp.app.observe_repository(arguments).await {
                Ok(result) => tool_result(
                    id,
                    json!({
                        "repository": result.repository,
                        "repository_id": result.repository_id,
                        "permission_revision": mcp.app.permission_revision(),
                        "default_branch": result.default_branch,
                        "default_sha": result.default_sha
                    }),
                    "Dark Factory Maintainer authority and default head are ready.",
                ),
                Err(error) => operation_error(id, error),
            }
        }
        Some("observe_operation") => {
            let Ok(mut arguments) = serde_json::from_value::<ObserveOperation>(arguments) else {
                return json_rpc_error(id, -32602, "Invalid params");
            };
            if canonical_operation_id(&mut arguments.operation_id).is_err() {
                return json_rpc_error(id, -32602, "Invalid params");
            }
            match mcp.journal.observe_operation(&arguments.operation_id).await {
                Ok(None) => tool_result(
                    id,
                    json!({
                        "operation_id": arguments.operation_id,
                        "state": "missing",
                        "kind": null,
                        "request_digest": null,
                        "result": null
                    }),
                    "Operation UUID has not reached the durable journal.",
                ),
                Ok(Some(observation)) => {
                    let result = match observation.result_json {
                        Some(result) => match serde_json::from_str::<Value>(&result) {
                            Ok(result) if result.is_object() => result,
                            _ => {
                                return tool_error(
                                    id,
                                    "unavailable",
                                    "The durable operation record is invalid.",
                                );
                            }
                        },
                        None => Value::Null,
                    };
                    tool_result(
                        id,
                        json!({
                            "operation_id": arguments.operation_id,
                            "state": observation.state,
                            "kind": observation.kind,
                            "request_digest": observation.request_digest,
                            "result": result
                        }),
                        "Durable operation state was observed.",
                    )
                }
                Err(_) => tool_error(
                    id,
                    "unavailable",
                    "The durable operation journal is unavailable.",
                ),
            }
        }
        Some("create_issue") => {
            let Ok(arguments) = serde_json::from_value::<CreateIssue>(arguments) else {
                return json_rpc_error(id, -32602, "Invalid params");
            };
            match mcp.app.create_issue(&mcp.journal, arguments).await {
                Ok(result) => serialized_tool_result(id, &result, "Issue is durably recorded."),
                Err(error) => operation_error(id, error),
            }
        }
        Some("observe_file") => {
            let Ok(arguments) = serde_json::from_value::<ObserveFile>(arguments) else {
                return json_rpc_error(id, -32602, "Invalid params");
            };
            match mcp.app.observe_file(arguments).await {
                Ok(result) => serialized_tool_result(id, &result, "File is observed."),
                Err(error) => operation_error(id, error),
            }
        }
        Some("observe_tree") => {
            let Ok(arguments) = serde_json::from_value::<ObserveTree>(arguments) else {
                return json_rpc_error(id, -32602, "Invalid params");
            };
            match mcp.app.observe_tree(arguments).await {
                Ok(result) => serialized_tool_result(id, &result, "Tree is observed."),
                Err(error) => operation_error(id, error),
            }
        }
        Some("observe_ref") => {
            let Ok(arguments) = serde_json::from_value::<ObserveRef>(arguments) else {
                return json_rpc_error(id, -32602, "Invalid params");
            };
            match mcp.app.observe_ref(arguments).await {
                Ok(result) => serialized_tool_result(id, &result, "Branch head is observed."),
                Err(error) => operation_error(id, error),
            }
        }
        Some("observe_issue") => {
            let Ok(arguments) = serde_json::from_value::<ObserveIssue>(arguments) else {
                return json_rpc_error(id, -32602, "Invalid params");
            };
            match mcp.app.observe_issue(arguments).await {
                Ok(result) => serialized_tool_result(id, &result, "Issue state was observed."),
                Err(error) => operation_error(id, error),
            }
        }
        Some("resolve_issue") => {
            let Ok(arguments) = serde_json::from_value::<ResolveIssue>(arguments) else {
                return json_rpc_error(id, -32602, "Invalid params");
            };
            match mcp.app.resolve_issue(&mcp.journal, arguments).await {
                Ok(result) => serialized_tool_result(id, &result, "Issue is durably resolved."),
                Err(error) => operation_error(id, error),
            }
        }
        Some("publish_release_tag") => {
            let Ok(arguments) = serde_json::from_value::<PublishReleaseTag>(arguments) else {
                return json_rpc_error(id, -32602, "Invalid params");
            };
            match mcp.app.publish_release_tag(&mcp.journal, arguments).await {
                Ok(result) => {
                    serialized_tool_result(id, &result, "Release tag is durably published.")
                }
                Err(error) => operation_error(id, error),
            }
        }
        Some("recover_release") => {
            let Ok(arguments) = serde_json::from_value::<RecoverRelease>(arguments) else {
                return json_rpc_error(id, -32602, "Invalid params");
            };
            match mcp.app.recover_release(&mcp.journal, arguments).await {
                Ok(result) => {
                    serialized_tool_result(id, &result, "Release recovery is durably dispatched.")
                }
                Err(error) => operation_error(id, error),
            }
        }
        Some("observe_release") => {
            let Ok(arguments) = serde_json::from_value::<ObserveRelease>(arguments) else {
                return json_rpc_error(id, -32602, "Invalid params");
            };
            match mcp.app.observe_release(arguments).await {
                Ok(result) => {
                    serialized_tool_result(id, &result, "Exact release state was observed.")
                }
                Err(error) => operation_error(id, error),
            }
        }
        Some("observe_release_workflow") => {
            let Ok(arguments) = serde_json::from_value::<ObserveReleaseWorkflow>(arguments) else {
                return json_rpc_error(id, -32602, "Invalid params");
            };
            match mcp.app.observe_release_workflow(arguments).await {
                Ok(result) => serialized_tool_result(
                    id,
                    &result,
                    "Exact release recovery workflow state was observed.",
                ),
                Err(error) => operation_error(id, error),
            }
        }
        Some("dispatch_control_plane_deploy") => {
            let Ok(arguments) = serde_json::from_value::<DispatchControlPlaneDeploy>(arguments)
            else {
                return json_rpc_error(id, -32602, "Invalid params");
            };
            match mcp
                .app
                .dispatch_control_plane_deploy(&mcp.journal, arguments)
                .await
            {
                Ok(result) => serialized_tool_result(
                    id,
                    &result,
                    "Control-plane deployment is durably dispatched.",
                ),
                Err(error) => operation_error(id, error),
            }
        }
        Some("observe_control_plane_deploy") => {
            let Ok(arguments) = serde_json::from_value::<ObserveControlPlaneDeploy>(arguments)
            else {
                return json_rpc_error(id, -32602, "Invalid params");
            };
            match mcp.app.observe_control_plane_deploy(arguments).await {
                Ok(result) => serialized_tool_result(
                    id,
                    &result,
                    "Exact control-plane deployment state was observed.",
                ),
                Err(error) => operation_error(id, error),
            }
        }
        Some("create_pull_request") => {
            let Ok(arguments) = serde_json::from_value::<CreatePullRequest>(arguments) else {
                return json_rpc_error(id, -32602, "Invalid params");
            };
            match mcp.app.create_pull_request(&mcp.journal, arguments).await {
                Ok(result) => {
                    serialized_tool_result(id, &result, "Pull request is durably recorded.")
                }
                Err(error) => operation_error(id, error),
            }
        }
        Some("update_pull_request_body") => {
            let Ok(arguments) = serde_json::from_value::<UpdatePullRequestBody>(arguments) else {
                return json_rpc_error(id, -32602, "Invalid params");
            };
            match mcp
                .app
                .update_pull_request_body(&mcp.journal, arguments)
                .await
            {
                Ok(result) => {
                    serialized_tool_result(id, &result, "Pull request body is durably recorded.")
                }
                Err(error) => operation_error(id, error),
            }
        }
        Some("close_pull_request") => {
            let Ok(arguments) = serde_json::from_value::<ClosePullRequest>(arguments) else {
                return json_rpc_error(id, -32602, "Invalid params");
            };
            match mcp.app.close_pull_request(&mcp.journal, arguments).await {
                Ok(result) => {
                    serialized_tool_result(id, &result, "Pull request is durably closed.")
                }
                Err(error) => operation_error(id, error),
            }
        }
        Some("submit_pull_request_review") => {
            let Ok(arguments) = serde_json::from_value::<SubmitPullRequestReview>(arguments) else {
                return json_rpc_error(id, -32602, "Invalid params");
            };
            match mcp
                .app
                .submit_pull_request_review(&mcp.journal, arguments)
                .await
            {
                Ok(result) => {
                    serialized_tool_result(id, &result, "Pull request review is durably recorded.")
                }
                Err(error) => operation_error(id, error),
            }
        }
        Some("publish_commit") => {
            let Ok(arguments) = serde_json::from_value::<PublishCommit>(arguments) else {
                return json_rpc_error(id, -32602, "Invalid params");
            };
            match mcp.app.publish_commit(&mcp.journal, arguments).await {
                Ok(result) => serialized_tool_result(id, &result, "Commit is durably published."),
                Err(error) => operation_error(id, error),
            }
        }
        Some("enqueue_pull_request") => {
            let Ok(arguments) = serde_json::from_value::<EnqueuePullRequest>(arguments) else {
                return json_rpc_error(id, -32602, "Invalid params");
            };
            match mcp.app.enqueue_pull_request(&mcp.journal, arguments).await {
                Ok(result) => {
                    serialized_tool_result(id, &result, "Pull request is durably queued.")
                }
                Err(error) => operation_error(id, error),
            }
        }
        Some("merge_pull_request_at_head") => {
            let Ok(arguments) = serde_json::from_value::<MergePullRequestAtHead>(arguments) else {
                return json_rpc_error(id, -32602, "Invalid params");
            };
            match mcp
                .app
                .merge_pull_request_at_head(&mcp.journal, arguments)
                .await
            {
                Ok(result) => {
                    serialized_tool_result(id, &result, "Pull request is durably merged.")
                }
                Err(error) => operation_error(id, error),
            }
        }
        Some("observe_pull_request_checks") => {
            let Ok(arguments) = serde_json::from_value::<ObservePullRequestChecks>(arguments)
            else {
                return json_rpc_error(id, -32602, "Invalid params");
            };
            match mcp.app.observe_pull_request_checks(arguments).await {
                Ok(result) => {
                    serialized_tool_result(id, &result, "Exact-head check runs were observed.")
                }
                Err(error) => operation_error(id, error),
            }
        }
        Some("observe_pull_request_workflows") => {
            let Ok(arguments) = serde_json::from_value::<ObservePullRequestWorkflows>(arguments)
            else {
                return json_rpc_error(id, -32602, "Invalid params");
            };
            match mcp.app.observe_pull_request_workflows(arguments).await {
                Ok(result) => {
                    serialized_tool_result(id, &result, "Exact-head workflow runs were observed.")
                }
                Err(error) => operation_error(id, error),
            }
        }
        Some("read_pull_request_job_log") => {
            let Ok(arguments) = serde_json::from_value::<ReadPullRequestJobLog>(arguments) else {
                return json_rpc_error(id, -32602, "Invalid params");
            };
            match mcp.app.read_pull_request_job_log(arguments).await {
                Ok(result) => serialized_tool_result(id, &result, "Failed job log tail was read."),
                Err(error) => operation_error(id, error),
            }
        }
        Some("rerun_failed_pull_request_jobs") => {
            let Ok(arguments) = serde_json::from_value::<RerunFailedPullRequestJobs>(arguments)
            else {
                return json_rpc_error(id, -32602, "Invalid params");
            };
            match mcp
                .app
                .rerun_failed_pull_request_jobs(&mcp.journal, arguments)
                .await
            {
                Ok(result) => {
                    serialized_tool_result(id, &result, "Failed jobs were durably rerun.")
                }
                Err(error) => operation_error(id, error),
            }
        }
        Some("observe_pull_request_merge") => {
            let Ok(arguments) = serde_json::from_value::<ObservePullRequestMerge>(arguments) else {
                return json_rpc_error(id, -32602, "Invalid params");
            };
            match mcp
                .app
                .observe_pull_request_merge(&mcp.journal, arguments)
                .await
            {
                Ok(result) => {
                    serialized_tool_result(id, &result, "Exact-head merge state was observed.")
                }
                Err(error) => operation_error(id, error),
            }
        }
        _ => json_rpc_error(id, -32602, "Invalid params"),
    }
}

fn tool_result(id: Value, structured: Value, message: &'static str) -> Response {
    json_rpc_result(
        id,
        json!({
            "structuredContent": structured,
            "content": [{"type": "text", "text": message}],
            "isError": false
        }),
    )
}

fn serialized_tool_result<T: serde::Serialize>(
    id: Value,
    result: &T,
    message: &'static str,
) -> Response {
    match serde_json::to_value(result) {
        Ok(structured) => tool_result(id, structured, message),
        Err(_) => tool_error(id, "unavailable", "Maintainer authority is unavailable."),
    }
}

fn operation_error(id: Value, error: OperationError) -> Response {
    match error {
        OperationError::InvalidInput => {
            tool_error(id, "invalid_input", "Operation input is invalid.")
        }
        // Not only heads: a read whose answer does not match what was asked
        // for -- a path that is a directory, a symlink, or a submodule -- is
        // the same class of "you did not get what you named", and an
        // observation has no head binding to have changed.
        OperationError::Conflict => tool_error(
            id,
            "conflict",
            "The exact-head, operation, or observed-object binding did not match.",
        ),
        // The reason names the fact; the wrapper must not promise more than the
        // reason supports. Some refusals are preconditions that can come to
        // hold, and some -- a tree GitHub cannot return whole -- are permanent
        // for that input, so telling an autonomous caller to retry sends it to
        // do the one thing guaranteed never to work. Reads reach this too, so
        // it cannot say "mutation" either.
        OperationError::Refused(reason) => tool_error(
            id,
            "refused",
            &format!("The request was refused: {reason}."),
        ),
        OperationError::Indeterminate => tool_error(
            id,
            "indeterminate",
            "The operation outcome is indeterminate and was not repeated.",
        ),
        OperationError::Unavailable => {
            tool_error(id, "unavailable", "Maintainer authority is unavailable.")
        }
    }
}

fn tool_error(id: Value, code: &'static str, message: &str) -> Response {
    json_rpc_result(
        id,
        json!({
            "content": [{"type": "text", "text": format!("{code}: {message}")}],
            "isError": true
        }),
    )
}

fn request_id(value: Option<&Value>) -> Option<Value> {
    match value {
        None => None,
        Some(Value::Null) => Some(Value::Null),
        Some(Value::String(value)) if value.len() <= 128 => Some(Value::String(value.clone())),
        Some(Value::Number(value)) if value.is_i64() || value.is_u64() => {
            Some(Value::Number(value.clone()))
        }
        _ => Some(Value::Null),
    }
}

fn single_header<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    let mut values = headers.get_all(name).iter();
    let value = values.next()?;
    if values.next().is_some() {
        return None;
    }
    let value = value.to_str().ok()?;
    (!value.contains(',')).then_some(value)
}

fn json_rpc_result(id: Value, result: Value) -> Response {
    json_response(
        StatusCode::OK,
        json!({"jsonrpc": "2.0", "id": id, "result": result}),
    )
}

fn json_rpc_error(id: Value, code: i64, message: &'static str) -> Response {
    json_response(
        StatusCode::OK,
        json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}}),
    )
}

fn json_response(status: StatusCode, body: Value) -> Response {
    (
        status,
        [
            ("content-type", "application/json"),
            ("cache-control", "no-store"),
        ],
        body.to_string(),
    )
        .into_response()
}

fn error_response(status: StatusCode, error: &'static str) -> Response {
    json_response(status, json!({"error": error}))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn protocol_surface_is_stateless_and_typed() {
        let request = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"});
        let object = request.as_object().unwrap();
        assert_eq!(
            object.get("method").and_then(Value::as_str),
            Some("tools/list")
        );
        assert_eq!(PROTOCOL_VERSION, "2025-06-18");
        assert_eq!(request_id(object.get("id")), Some(json!(1)));
        assert_eq!(request_id(Some(&json!({"bad": true}))), Some(Value::Null));
        assert_eq!(tools()["tools"][0]["name"], "maintainer_status");
    }
}
