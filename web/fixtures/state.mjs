const projectID = "11".repeat(16);
const secondProjectID = "12".repeat(16);
const agentID = "21".repeat(16);
const secondAgentID = "22".repeat(16);
const thirdAgentID = "23".repeat(16);
const taskID = "31".repeat(16);
const queuedTaskID = "32".repeat(16);
const doneTaskID = "33".repeat(16);
const failedTaskID = "34".repeat(16);
const requestID = "41".repeat(16);

export const fixtureState = {
  head: 42n,
  factory: { dispatch_enabled: true, capacity: 8, active_runs: 2, revision: 42n },
  projects: new Map([
    [projectID, { id: projectID, name: "North Workshop", revision: 4n }],
    [secondProjectID, { id: secondProjectID, name: "South Workshop", revision: 5n }],
  ]),
  agents: new Map([
    [agentID, { id: agentID, project_id: projectID, name: "Builder One", role: "worker", provider: "claude_code", paused: false, model: "claude-opus-5", reasoning_effort: "high", revision: 10n }],
    [secondAgentID, { id: secondAgentID, project_id: secondProjectID, name: "Dispatch Lead", role: "orchestrator", provider: "claude_code", paused: true, model: "claude-opus-5", reasoning_effort: "", revision: 11n }],
    [thirdAgentID, { id: thirdAgentID, project_id: projectID, name: "Builder Two", role: "worker", provider: "codex", paused: false, model: "", reasoning_effort: "", revision: 12n }],
  ]),
  tasks: new Map([
    [taskID, { id: taskID, project_id: projectID, assigned_agent_id: agentID, title: "Review the state projection", status: "running", priority: 10, revision: 12n }],
    [queuedTaskID, { id: queuedTaskID, project_id: secondProjectID, assigned_agent_id: thirdAgentID, title: "Tighten the queue ordering", status: "queued", priority: 6, revision: 13n }],
    [doneTaskID, { id: doneTaskID, project_id: projectID, assigned_agent_id: thirdAgentID, title: "Close the resize race", status: "succeeded", priority: 8, revision: 14n }],
    [failedTaskID, { id: failedTaskID, project_id: secondProjectID, assigned_agent_id: thirdAgentID, title: "Probe the flaky gate", status: "failed", priority: 4, revision: 15n }],
  ]),
  humanRequests: new Map([
    [requestID, { id: requestID, project_id: projectID, agent_id: agentID, task_id: taskID, created_at: 40n, updated_at: 42n, revision: 13n, kind: "question", status: "open", reply_max_bytes: 8192, can_reply: true }],
  ]),
};

const nodeID = (prefix) => prefix.repeat(32);

/** The first project's structure: a repository root and its direct children. */
export const fixtureTopology = {
  projectId: projectID,
  digest: "ab".repeat(32),
  sourceRevision: "c3".repeat(20),
  nodes: [
    { id: nodeID("a1"), parent_id: "", kind: "repository", path: ".", label: "north-workshop", language: "", size_bucket: "large" },
    { id: nodeID("b2"), parent_id: nodeID("a1"), kind: "package", path: "internal/kernel", label: "kernel", language: "go", size_bucket: "medium" },
    { id: nodeID("c3"), parent_id: nodeID("a1"), kind: "module", path: "web", label: "web", language: "typescript", size_bucket: "small" },
    { id: nodeID("d4"), parent_id: nodeID("b2"), kind: "directory", path: "internal/kernel/store", label: "store", language: "go", size_bucket: "tiny" },
  ],
};
