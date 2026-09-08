import assert from "node:assert/strict";
import test from "node:test";
import {
  STAGE_SEQUENCE,
  agentActivity,
  agentStatus,
  agentCurrentTask,
  agentGlyph,
  factoryCounters,
  orderTasksForHome,
  primaryAgent,
  stageMeterFill,
  stageOfTask,
} from "../dist/src/console-view.js";
import { fixtureState } from "../../../fixtures/state.mjs";

const agentID = [...fixtureState.agents.keys()][0];
const pausedAgentID = [...fixtureState.agents.keys()][1];
const idleAgentID = [...fixtureState.agents.keys()][2];

function task(status, id = "77".repeat(16)) {
  return { id, project_id: "11".repeat(16), assigned_agent_id: agentID, title: "t", status, priority: 1, revision: 1n };
}

test("every durable task status maps to exactly one console stage", () => {
  assert.equal(stageOfTask(task("queued")), "queued");
  assert.equal(stageOfTask(task("running")), "building");
  assert.equal(stageOfTask(task("blocked")), "blocked");
  assert.equal(stageOfTask(task("succeeded")), "done");
  assert.equal(stageOfTask(task("failed")), "failed");
  assert.equal(stageOfTask(task("cancelled")), "failed");
});

test("meter fill is monotonic along the stage sequence, full for done, empty for failed", () => {
  let previous = 0;
  for (const stage of STAGE_SEQUENCE) {
    const fill = stageMeterFill(stage);
    assert.ok(fill > previous, stage);
    previous = fill;
  }
  assert.equal(stageMeterFill("done"), STAGE_SEQUENCE.length);
  assert.equal(stageMeterFill("failed"), 0);
});

test("agent activity precedence: an open question outranks work, pause outranks waiting", () => {
  const state = fixtureState;
  assert.equal(agentActivity(state.agents.get(agentID), state), "needs-you");
  assert.equal(agentActivity(state.agents.get(pausedAgentID), state), "idle");
  assert.equal(agentActivity(state.agents.get(idleAgentID), state), "waiting");
  const noRequests = { ...state, humanRequests: new Map() };
  assert.equal(agentActivity(state.agents.get(agentID), noRequests), "busy");
  assert.equal(agentCurrentTask(state.agents.get(agentID), state)?.status, "running");

  const blockedTask = task("blocked");
  const blocked = { ...noRequests, tasks: new Map([[blockedTask.id, blockedTask]]) };
  assert.equal(agentCurrentTask(state.agents.get(agentID), blocked), undefined);
  assert.equal(agentActivity(state.agents.get(agentID), blocked), "waiting");
});

test("operator statuses do not expose idle-policy implementation words", () => {
  const state = fixtureState;
  assert.equal(agentStatus(state.agents.get(agentID), state), "needs-you");
  assert.equal(agentStatus(state.agents.get(pausedAgentID), state), "paused");
  assert.equal(agentStatus(state.agents.get(idleAgentID), state), "ready");
  const noRequests = { ...state, humanRequests: new Map() };
  assert.equal(agentStatus(state.agents.get(agentID), noRequests), "working");
  const queued = { ...noRequests, tasks: new Map([...noRequests.tasks].map(([id, value]) => [id, value.assigned_agent_id === agentID ? { ...value, status: "queued" } : value])) };
  assert.equal(agentStatus(queued.agents.get(agentID), queued), "ready");
  const paused = state.agents.get(pausedAgentID);
  const pausedRunning = { ...task("running", "78".repeat(16)), assigned_agent_id: paused.id };
  assert.equal(agentStatus(paused, { ...state, tasks: new Map([[pausedRunning.id, pausedRunning]]) }), "working");
});

test("the first console selection prefers the overseer deterministically", () => {
  const state = fixtureState;
  assert.equal(primaryAgent(state).role, "orchestrator");
  const workers = [...state.agents.values()].filter((agent) => agent.role === "worker");
  assert.equal(primaryAgent({ ...state, agents: new Map(workers.map((agent) => [agent.id, agent])) }).id, workers.slice().sort((left, right) => (left.name < right.name ? -1 : left.name > right.name ? 1 : 0) || (left.id < right.id ? -1 : left.id > right.id ? 1 : 0))[0].id);
});

test("counters count only store-backed facts", () => {
  const counters = factoryCounters(fixtureState);
  assert.equal(counters.queued, 1);
  assert.equal(counters.needsYou, 1);
  assert.deepEqual(factoryCounters(undefined), { queued: undefined, needsYou: undefined });
});

test("home ordering puts active work first and finished work last", () => {
  const ordered = orderTasksForHome(fixtureState).map((item) => item.status);
  assert.deepEqual(ordered, ["running", "queued", "succeeded", "failed"]);
});

test("agent glyphs derive only from the served role and provider", () => {
  const worker = fixtureState.agents.get(agentID);
  const orchestrator = fixtureState.agents.get(pausedAgentID);
  assert.equal(worker.provider, "claude_code");
  assert.equal(agentGlyph(worker), "C");
  assert.equal(agentGlyph({ ...worker, provider: "codex" }), "X");
  assert.equal(agentGlyph({ ...worker, provider: "shell" }), "s");
  assert.equal(agentGlyph({ ...orchestrator, provider: "codex" }), "◆");
});
