import assert from "node:assert/strict";
import test from "node:test";
import { ProtocolError, SessionError } from "@dark-factory/client";
import { FactoryAppController } from "../dist/src/factory-app-controller.js";
import { fixtureState } from "../../../fixtures/state.mjs";

const agent = [...fixtureState.agents.values()][0];
const secondAgent = [...fixtureState.agents.values()][1];
const thirdAgent = [...fixtureState.agents.values()][2];
const request = [...fixtureState.humanRequests.values()][0];
const target = Object.freeze({});

function deferred() {
  let resolve;
  let reject;
  const promise = new Promise((accept, refuse) => { resolve = accept; reject = refuse; });
  return { promise, resolve, reject };
}

async function flush() {
  for (let index = 0; index < 5; index += 1) await Promise.resolve();
}

function stateAt(head, overrides = {}) {
  return { ...fixtureState, head: BigInt(head), ...overrides };
}

function terminalHarness({ closeStatus = false, fail, failError = new SessionError("connection"), detachImpl, acquireImpl, enqueueImpl } = {}) {
  let attachReset = false;
  const snapshots = [];
  const calls = [];
  const targetGates = [];
  const handles = [];
  let sessionCloses = 0;
  let clientOptions;
  let handleOptions;
  const session = {
    getHumanRequestDetail: async () => ({
      requestId: request.id,
      revision: request.revision,
      question: "Continue?",
      canReply: true,
      replyMaxBytes: 8192,
      terminalTarget: Object.freeze({}),
      cancelRun: Object.freeze({ requestId: request.id, expectedRequestRevision: request.revision, expectedRunRevision: 17n }),
    }),
    replyHumanRequest: async () => ({ status: "resolved" }),
    cancelHumanRequest: async () => ({ request_id: request.id }),
    enqueueAgentTask: async (value) => {
      calls.push({ kind: "enqueue", value });
      if (enqueueImpl !== undefined) return enqueueImpl(value);
      return { taskId: "51".repeat(16), revision: 1n };
    },
    resolveAgentTerminal: (value) => {
      calls.push({ kind: "resolve", value });
      const gate = deferred();
      targetGates.push(gate);
      return gate.promise;
    },
    openTerminal: (value, options) => {
      calls.push({ kind: "open", value, afterSequence: options.afterSequence, afterSessionId: options.afterSessionId });
      if (fail === "open") throw failError;
      handleOptions = options;
      const handle = {
        attach: async () => { calls.push({ kind: "attach" }); if (fail === "attach") throw failError; if (attachReset) return { kind: "reset", freshAttachRequired: true, sessionId: "31".repeat(16), floor: 5n, head: 9n }; return options.afterSequence === 9n ? { sessionId: "31".repeat(16), floor: 8n, head: 12n, acknowledgedSequence: 9n, maxUnackedBytes: 65536n } : { sessionId: "31".repeat(16), floor: 0n, head: 0n, acknowledgedSequence: 0n, maxUnackedBytes: 65536n }; },
        acquireInput: async () => { calls.push({ kind: "acquire" }); if (acquireImpl !== undefined) return acquireImpl(); if (fail === "acquire") throw failError; return { generation: 1n }; },
        sendInput: async (bytes) => { calls.push({ kind: "input", bytes }); if (fail === "input") throw failError; return { status: "accepted", accepted_bytes: BigInt(bytes.length) }; },
        resize: async (rows, cols) => { calls.push({ kind: "resize", rows, cols }); if (fail === "resize") throw failError; return { rows, cols }; },
        detach: async () => { calls.push({ kind: "detach" }); await detachImpl?.(); options.onClose?.(); },
        get writable() { return true; },
      };
      handles.push(handle);
      return handle;
    },
    close: () => { sessionCloses += 1; if (closeStatus) clientOptions?.onStatus("closed"); },
  };
  const client = {
    session,
    connect: () => Promise.resolve(),
    close: () => { sessionCloses += 1; },
  };
  const controller = new FactoryAppController({
    origin: "https://app.darkfactory.build",
    location: { hash: "", pathname: "/factory", search: "" },
    history: { state: null, replaceState: () => {} },
    onChange: (snapshot) => snapshots.push(snapshot),
    clientFactory: (value) => { clientOptions = value; return client; },
  });
  return {
    controller,
    session,
    client,
    snapshots,
    calls,
    handles,
    targetGates,
    sessionCloses: () => sessionCloses,
    clientOptions: () => clientOptions,
    handleOptions: () => handleOptions,
    surfaceFailure: fail === "surface",
    setAttachReset: (value) => { attachReset = value; },
    latest: () => snapshots.at(-1),
    ready: (state = fixtureState) => { clientOptions.onState(state); clientOptions.onStatus("ready"); },
  };
}

async function openTerminal(context, selectedAgent = agent) {
  context.controller.selectAgent(selectedAgent);
  const token = {};
  const writes = [];
  context.controller.beginTerminalSurface(token);
  context.controller.setTerminalSurface(token, {
    write: async (bytes) => { writes.push(bytes); if (context.surfaceFailure) throw new Error("display failed"); },
    abort: () => {},
  });
  await flush();
  assert.equal(context.targetGates.length > 0, true);
  context.targetGates.at(-1).resolve(target);
  await flush();
  assert.deepEqual(context.calls.slice(-3).map((call) => call.kind), ["open", "attach", "acquire"]);
  assert.equal(context.latest().terminal.phase, "ready");
  return { token, writes, options: context.handleOptions() };
}

test("public terminal composition buffers direct input until writable authority", async () => {
  const context = terminalHarness();
  context.controller.start();
  context.ready();
  context.controller.selectAgent(agent);
  assert.equal(context.latest().terminal.phase, "idle");
  assert.equal(context.calls.some((call) => call.kind === "resolve"), false);

  const token = {};
  context.controller.beginTerminalSurface(token);
  context.controller.sendTerminalText(token, "before");
  assert.equal(context.calls.some((call) => call.kind === "input"), false);
  context.controller.setTerminalSurface(token, { write: async () => {}, abort: () => {} });
  await flush();
  assert.equal(context.calls.at(-1).kind, "resolve");
  assert.deepEqual(context.calls.at(-1).value, { agentId: agent.id, expectedAgentRevision: agent.revision, expectedHead: fixtureState.head });
  context.targetGates[0].resolve(target);
  await flush();
  assert.deepEqual(context.calls.slice(-4).map((call) => call.kind), ["open", "attach", "acquire", "input"]);
  assert.deepEqual(context.calls.filter((call) => call.kind === "input").map((call) => [...call.bytes]), [[98, 101, 102, 111, 114, 101]]);
  context.controller.sendTerminalText(token, "é");
  context.controller.sendTerminalBinary(token, String.fromCharCode(0, 255));
  await flush();
  assert.deepEqual(context.calls.filter((call) => call.kind === "input").map((call) => [...call.bytes]), [[98, 101, 102, 111, 114, 101], [0xc3, 0xa9], [0, 255]]);
});

test("completed work leaves its configured agent idle and able to enqueue a durable instruction", async () => {
  const context = terminalHarness();
  context.controller.start();
  const tasks = new Map(fixtureState.tasks);
  for (const [id, task] of tasks) if (task.assigned_agent_id === thirdAgent.id && task.status === "queued") tasks.delete(id);
  context.ready(stateAt(42, { tasks }));
  context.controller.selectAgent(thirdAgent);
  assert.equal([...tasks.values()].some((task) => task.assigned_agent_id === thirdAgent.id && ["succeeded", "failed"].includes(task.status)), true);
  assert.equal(context.latest().selectedAgent.id, thirdAgent.id);
  assert.equal(context.latest().terminal.taskTitle, undefined);
  assert.equal(context.latest().terminal.instructionPending, false);
  assert.equal(context.latest().terminal.queued, false);
  assert.equal(context.targetGates.length, 0, "idle selection must not probe for a fake terminal");

  const queued = context.controller.enqueueAgentInstruction("  Investigate the build\n  ");
  assert.equal(context.latest().terminal.instructionPending, true);
  assert.deepEqual(context.calls.at(-1), {
    kind: "enqueue",
    value: { agentId: thirdAgent.id, expectedAgentRevision: thirdAgent.revision, instruction: "Investigate the build" },
  });
  assert.equal(await queued, true);
  assert.equal(context.latest().selectedAgent.id, thirdAgent.id);
  assert.equal(context.latest().terminal.instructionPending, false);
  assert.equal(context.latest().terminal.queued, true);
});

test("a selected idle agent follows its canonical pause revision without exposing input", async () => {
  const context = terminalHarness();
  context.controller.start();
  const tasks = new Map(fixtureState.tasks);
  for (const [id, task] of tasks) if (task.assigned_agent_id === thirdAgent.id && task.status === "queued") tasks.delete(id);
  context.ready(stateAt(42, { tasks }));
  context.controller.selectAgent(thirdAgent);

  const paused = { ...thirdAgent, paused: true, revision: thirdAgent.revision + 1n };
  const agents = new Map(fixtureState.agents);
  agents.set(paused.id, paused);
  context.clientOptions().onState(stateAt(43, { agents, tasks }));

  assert.equal(context.latest().selectedAgent.id, paused.id);
  assert.equal(context.latest().selectedAgent.revision, paused.revision);
  assert.equal(context.latest().terminal.paused, true);
  assert.equal(context.latest().terminal.phase, "idle");
  assert.equal(context.latest().error, undefined);
  assert.equal(context.targetGates.length, 0);
  assert.equal(await context.controller.enqueueAgentInstruction("Must not enqueue while paused"), false);
  assert.equal(context.calls.some((call) => call.kind === "enqueue"), false);
});

test("a newly running instruction turns the same idle sidebar into the real terminal", async () => {
  const context = terminalHarness();
  context.controller.start();
  const initialTasks = new Map(fixtureState.tasks);
  for (const [id, task] of initialTasks) if (task.assigned_agent_id === thirdAgent.id && task.status === "queued") initialTasks.delete(id);
  context.ready(stateAt(42, { tasks: initialTasks }));
  context.controller.selectAgent(thirdAgent);

  const tasks = new Map(initialTasks);
  tasks.set("51".repeat(16), {
    id: "51".repeat(16),
    project_id: thirdAgent.project_id,
    assigned_agent_id: thirdAgent.id,
    title: "Direct instruction",
    status: "running",
    priority: 0,
    revision: 1n,
  });
  context.clientOptions().onState(stateAt(43, { tasks }));
  assert.equal(context.latest().selectedAgent.id, thirdAgent.id);
  assert.equal(context.latest().terminal.taskTitle, "Direct instruction");
  const token = {};
  context.controller.beginTerminalSurface(token);
  context.controller.setTerminalSurface(token, { write: async () => {}, abort: () => {} });
  await flush();
  assert.equal(context.targetGates.length, 1);
  context.targetGates[0].resolve(target);
  await flush();
  assert.equal(context.latest().terminal.phase, "ready");
});

test("current running work wins over the selected agent's queued instruction", async () => {
  const context = terminalHarness();
  context.controller.start();
  const initialTasks = new Map(fixtureState.tasks);
  for (const [id, task] of initialTasks) if (task.assigned_agent_id === thirdAgent.id && task.status === "queued") initialTasks.delete(id);
  context.ready(stateAt(42, { tasks: initialTasks }));
  context.controller.selectAgent(thirdAgent);
  assert.equal(await context.controller.enqueueAgentInstruction("Do this next"), true);

  const tasks = new Map(initialTasks);
  tasks.set("51".repeat(16), {
    id: "51".repeat(16), project_id: thirdAgent.project_id, assigned_agent_id: thirdAgent.id,
    title: "Direct instruction", status: "queued", priority: 0, revision: 1n,
  });
  tasks.set("52".repeat(16), {
    id: "52".repeat(16), project_id: thirdAgent.project_id, assigned_agent_id: thirdAgent.id,
    title: "Already running", status: "running", priority: 1, revision: 2n,
  });
  context.clientOptions().onState(stateAt(43, { tasks }));
  assert.equal(context.latest().terminal.taskTitle, "Already running");
  assert.equal(context.latest().terminal.queued, false);
});

test("a terminal task observed before its enqueue response cannot leave the sidebar queued", async () => {
  const response = deferred();
  const context = terminalHarness({ enqueueImpl: () => response.promise });
  context.controller.start();
  const initialTasks = new Map(fixtureState.tasks);
  for (const [id, task] of initialTasks) if (task.assigned_agent_id === thirdAgent.id && task.status === "queued") initialTasks.delete(id);
  context.ready(stateAt(42, { tasks: initialTasks }));
  context.controller.selectAgent(thirdAgent);
  const submission = context.controller.enqueueAgentInstruction("Finish quickly");

  const tasks = new Map(initialTasks);
  tasks.set("51".repeat(16), {
    id: "51".repeat(16), project_id: thirdAgent.project_id, assigned_agent_id: thirdAgent.id,
    title: "Direct instruction", status: "succeeded", priority: 0, revision: 2n,
  });
  context.clientOptions().onState(stateAt(43, { tasks }));
  response.resolve({ taskId: "51".repeat(16), revision: 2n });
  assert.equal(await submission, true);
  assert.equal(context.latest().terminal.queued, false);
  assert.equal(context.latest().terminal.taskTitle, undefined);
});

test("when queued task A terminals, queued task B remains pinned", async () => {
  const context = terminalHarness();
  context.controller.start();
  const taskA = {
    id: "61".repeat(16), project_id: thirdAgent.project_id, assigned_agent_id: thirdAgent.id,
    title: "Queued A", status: "queued", priority: 2, revision: 1n,
  };
  const taskB = {
    id: "62".repeat(16), project_id: thirdAgent.project_id, assigned_agent_id: thirdAgent.id,
    title: "Queued B", status: "queued", priority: 1, revision: 1n,
  };
  const tasks = new Map(
    [...fixtureState.tasks].filter(([, task]) => task.assigned_agent_id !== thirdAgent.id || task.status !== "queued"),
  );
  tasks.set(taskA.id, taskA);
  tasks.set(taskB.id, taskB);
  context.ready(stateAt(42, { tasks }));
  context.controller.selectAgent(thirdAgent);
  assert.equal(context.latest().terminal.queued, true);

  const afterA = new Map(tasks);
  afterA.set(taskA.id, { ...taskA, status: "succeeded", revision: 2n });
  context.clientOptions().onState(stateAt(43, { tasks: afterA }));
  assert.equal(context.latest().selectedAgent.id, thirdAgent.id);
  assert.equal(context.latest().terminal.phase, "idle");
  assert.equal(context.latest().terminal.queued, true, "B still owns the queued affordance");
  const enqueues = context.calls.filter((call) => call.kind === "enqueue").length;
  assert.equal(await context.controller.enqueueAgentInstruction("Do not create C"), false);
  assert.equal(context.calls.filter((call) => call.kind === "enqueue").length, enqueues);
});

test("crypto-unavailable enqueue preflight stays selected with a definite failure", async () => {
  const context = terminalHarness({ enqueueImpl: async () => { throw new SessionError("crypto_unavailable"); } });
  context.controller.start();
  const tasks = new Map(fixtureState.tasks);
  for (const [id, task] of tasks) if (task.assigned_agent_id === thirdAgent.id && task.status === "queued") tasks.delete(id);
  context.ready(stateAt(42, { tasks }));
  context.controller.selectAgent(thirdAgent);
  assert.equal(await context.controller.enqueueAgentInstruction("Try once"), false);
  assert.equal(context.latest().selectedAgent.id, thirdAgent.id);
  assert.equal(context.latest().terminal.queued, false);
  assert.equal(context.latest().terminal.instructionError.code, "crypto_unavailable");
});

test("oversized direct input is rejected before terminal discovery", () => {
  for (const send of ["sendTerminalText", "sendTerminalBinary"]) {
    const context = terminalHarness();
    context.controller.start();
    context.ready();
    context.controller.selectAgent(agent);
    const token = {};
    context.controller.beginTerminalSurface(token);
    context.controller[send](token, "x".repeat(64 * 1024 + 1));
    assert.equal(context.latest().error.code, "too_large", send);
    assert.equal(context.latest().selectedAgent, undefined, send);
    assert.equal(context.calls.some((call) => call.kind === "resolve"), false, send);
  }
});

test("malformed direct binary input is rejected before terminal discovery", () => {
  const context = terminalHarness();
  context.controller.start();
  context.ready();
  context.controller.selectAgent(agent);
  const token = {};
  context.controller.beginTerminalSurface(token);
  context.controller.sendTerminalBinary(token, "\u0100");
  assert.equal(context.latest().error.code, "malformed");
  assert.equal(context.latest().selectedAgent, undefined);
  assert.equal(context.calls.some((call) => call.kind === "resolve"), false);
});

test("a later state head does not restart a live terminal, while waiting discovery captures the latest head", async () => {
  const context = terminalHarness();
  context.controller.start();
  context.ready();
  const live = await openTerminal(context);
  const resolveCount = context.targetGates.length;
  context.clientOptions().onState(stateAt(43));
  context.controller.selectAgent(agent);
  assert.equal(context.sessionCloses(), 0);
  assert.equal(context.targetGates.length, resolveCount);
  context.controller.sendTerminalText(live.token, "same session");
  await flush();
  assert.equal(context.calls.at(-1).kind, "input");

  const waiting = terminalHarness();
  waiting.controller.start();
  waiting.ready();
  waiting.controller.selectAgent(agent);
  waiting.clientOptions().onState(stateAt(44));
  const token = {};
  waiting.controller.beginTerminalSurface(token);
  waiting.controller.setTerminalSurface(token, { write: async () => {}, abort: () => {} });
  await flush();
  assert.equal(waiting.calls.at(-1).value.expectedHead, 44n);
});

async function assertNoAutomaticRestart(context) {
  const attempts = context.targetGates.length;
  context.clientOptions().onStatus("connecting");
  context.clientOptions().onState(fixtureState);
  context.clientOptions().onStatus("ready");
  await flush();
  assert.equal(context.targetGates.length, attempts);
  assert.equal(context.latest().selectedAgent, undefined);
}

test("active-task target absence waits for the next public head before retrying", async () => {
  const context = terminalHarness();
  context.controller.start();
  context.ready();
  context.controller.selectAgent(agent);
  const token = {};
  context.controller.beginTerminalSurface(token);
  context.controller.setTerminalSurface(token, { write: async () => {}, abort: () => {} });
  context.controller.sendTerminalText(token, "typed immediately");
  await flush();
  assert.equal(context.calls.some((call) => call.kind === "input"), false);
  context.targetGates[0].resolve(null);
  await flush();
  assert.equal(context.latest().selectedAgent.id, agent.id);
  assert.equal(context.latest().terminal.phase, "idle");
  assert.equal(context.latest().terminal.finishing, false);
  assert.equal(context.latest().error, undefined);
  assert.equal(context.sessionCloses(), 0);

  await remountSurface(context);
  assert.equal(context.targetGates.length, 1, "same public head must not spin discovery");
  context.clientOptions().onState(stateAt(43));
  await flush();
  assert.equal(context.targetGates.length, 2);
  assert.equal(context.calls.at(-1).value.expectedHead, 43n);
  context.targetGates[1].resolve(target);
  await flush();
  assert.equal(context.latest().terminal.phase, "ready");
  const inputCalls = context.calls.filter((call) => call.kind === "input");
  assert.equal(inputCalls.length, 1, "input survives discovery retry and is sent exactly once");
  assert.equal(new TextDecoder().decode(inputCalls[0].bytes), "typed immediately");
});

test("pending direct input never crosses tasks on the same agent", async () => {
  const context = terminalHarness();
  context.controller.start();
  context.ready();
  context.controller.selectAgent(agent);
  const token = {};
  context.controller.beginTerminalSurface(token);
  context.controller.setTerminalSurface(token, { write: async () => {}, abort: () => {} });
  context.controller.sendTerminalText(token, "for task A");
  await flush();
  context.targetGates[0].resolve(null);
  await flush();

  const tasks = new Map(fixtureState.tasks);
  const taskA = [...tasks.values()].find(
    (task) => task.assigned_agent_id === agent.id && task.status === "running",
  );
  assert.notEqual(taskA, undefined);
  tasks.set(taskA.id, { ...taskA, status: "succeeded", revision: taskA.revision + 1n });
  tasks.set("fe".repeat(16), {
    ...taskA,
    id: "fe".repeat(16),
    title: "Task B",
    status: "running",
    revision: 1n,
  });

  await remountSurface(context);
  context.clientOptions().onState(stateAt(43, { tasks }));
  await flush();
  assert.equal(context.targetGates.length, 2);
  context.targetGates[1].resolve(target);
  await flush();
  assert.equal(context.latest().terminal.phase, "ready");
  assert.equal(context.calls.some((call) => call.kind === "input"), false);
});

test("pending direct input never crosses a connection restart", async () => {
  const context = terminalHarness();
  context.controller.start();
  context.ready();
  context.controller.selectAgent(agent);
  const token = {};
  context.controller.beginTerminalSurface(token);
  context.controller.sendTerminalText(token, "old connection");
  context.clientOptions().onStatus("connecting");
  context.controller.sendTerminalText(token, "still disconnected");
  context.clientOptions().onState(stateAt(43));
  context.clientOptions().onStatus("ready");
  context.controller.setTerminalSurface(token, { write: async () => {}, abort: () => {} });
  await flush();
  assert.equal(context.targetGates.length, 1);
  context.targetGates[0].resolve(target);
  await flush();
  assert.equal(context.latest().terminal.phase, "ready");
  assert.equal(context.calls.some((call) => call.kind === "input"), false);
});

test("lease refusal drops pending and later direct input", async () => {
  let acquisitions = 0;
  const context = terminalHarness({
    acquireImpl: async () => {
      acquisitions += 1;
      if (acquisitions === 1) throw new SessionError("stale");
      return { generation: BigInt(acquisitions) };
    },
  });
  context.controller.start();
  context.ready();
  context.controller.selectAgent(agent);
  const token = {};
  context.controller.beginTerminalSurface(token);
  context.controller.setTerminalSurface(token, { write: async () => {}, abort: () => {} });
  context.controller.sendTerminalText(token, "before refusal");
  await flush();
  context.targetGates[0].resolve(target);
  await flush();
  assert.equal(context.latest().terminal.phase, "ready");
  assert.equal(context.latest().terminal.writable, false);
  context.controller.sendTerminalText(token, "after refusal");

  context.handleOptions().onReset({ sessionId: "31".repeat(16), floor: 5n, head: 9n });
  await flush();
  await remountSurface(context);
  context.targetGates[1].resolve(target);
  await flush();
  assert.equal(context.latest().terminal.writable, true);
  assert.equal(context.calls.some((call) => call.kind === "input"), false);
});

test("stale active-task discovery retries the newer state already received", async () => {
  const context = terminalHarness();
  context.controller.start();
  context.ready();
  context.controller.selectAgent(agent);
  const token = {};
  context.controller.beginTerminalSurface(token);
  context.controller.setTerminalSurface(token, { write: async () => {}, abort: () => {} });
  await flush();
  context.clientOptions().onState(stateAt(43));
  context.targetGates[0].reject(new SessionError("stale"));
  await flush();
  assert.equal(context.latest().selectedAgent.id, agent.id);
  assert.equal(context.sessionCloses(), 0);
  await remountSurface(context);
  assert.equal(context.targetGates.length, 2);
  assert.equal(context.calls.at(-1).value.expectedHead, 43n);
});

test("a queued task keeps the configured agent idle until newer running state arrives", async () => {
  const context = terminalHarness();
  const idleTasks = new Map(fixtureState.tasks);
  const running = [...idleTasks.values()].find(
    (task) => task.assigned_agent_id === agent.id && task.status === "running",
  );
  assert.notEqual(running, undefined);
  idleTasks.set(running.id, { ...running, status: "queued" });

  context.controller.start();
  context.clientOptions().onState(stateAt(42, { tasks: idleTasks }));
  context.clientOptions().onStatus("ready");
  context.controller.selectAgent(agent);
  const token = {};
  context.controller.beginTerminalSurface(token);
  context.controller.setTerminalSurface(token, { write: async () => {}, abort: () => {} });
  await flush();

  assert.equal(context.latest().selectedAgent.id, agent.id);
  assert.equal(context.latest().terminal.phase, "idle");
  assert.equal(context.latest().terminal.queued, true);
  assert.equal(context.sessionCloses(), 0);
  assert.equal(context.targetGates.length, 0, "queued work has no terminal to discover");

  await remountSurface(context);
  assert.equal(context.targetGates.length, 0, "remounting cannot invent a terminal");
  context.clientOptions().onState(stateAt(42, { tasks: idleTasks }));
  await flush();
  assert.equal(context.latest().selectedAgent.id, agent.id);
  assert.equal(context.targetGates.length, 0, "same-head refresh must preserve queued waiting");

  const runningTasks = new Map(idleTasks);
  runningTasks.set(running.id, running);
  context.clientOptions().onState(stateAt(43, { tasks: runningTasks }));
  await flush();
  assert.equal(context.latest().terminal.queued, false);
  assert.equal(context.targetGates.length, 1);
  assert.equal(context.calls.at(-1).value.expectedHead, 43n);
});

test("same-socket state resync preserves pending terminal discovery", async () => {
  const context = terminalHarness();
  context.controller.start();
  context.ready();
  context.controller.selectAgent(agent);
  const token = {};
  context.controller.beginTerminalSurface(token);
  context.controller.setTerminalSurface(token, { write: async () => {}, abort: () => {} });
  await flush();

  context.clientOptions().onStatus("syncing");
  assert.equal(context.latest().selectedAgent.id, agent.id);
  assert.equal(context.latest().terminal.phase, "resolving");
  assert.equal(context.sessionCloses(), 0);
  context.targetGates[0].reject(new SessionError("stale"));
  await flush();
  assert.equal(context.latest().selectedAgent.id, agent.id);
  assert.equal(context.sessionCloses(), 0);

  await remountSurface(context);
  context.clientOptions().onState(stateAt(43));
  await flush();
  assert.equal(context.targetGates.length, 1, "syncing cannot start target discovery");
  context.clientOptions().onStatus("ready");
  await flush();
  assert.equal(context.targetGates.length, 2);
  assert.equal(context.calls.at(-1).value.expectedHead, 43n);
});

test("same-socket state resync preserves one live writable terminal", async () => {
  const context = terminalHarness();
  context.controller.start();
  context.ready();
  const live = await openTerminal(context);
  const resolves = context.targetGates.length;

  context.clientOptions().onStatus("syncing");
  assert.equal(context.latest().terminal.phase, "ready");
  assert.equal(context.latest().terminal.writable, true);
  assert.equal(context.sessionCloses(), 0);
  context.controller.sendTerminalText(live.token, "during resync");
  await flush();
  assert.equal(context.calls.at(-1).kind, "input");
  assert.equal(new TextDecoder().decode(context.calls.at(-1).bytes), "during resync");

  context.clientOptions().onState(stateAt(43));
  context.clientOptions().onStatus("ready");
  await flush();
  assert.equal(context.targetGates.length, resolves);
  assert.equal(context.latest().terminal.phase, "ready");
  assert.equal(context.latest().terminal.writable, true);
  context.controller.sendTerminalText(live.token, "after resync");
  await flush();
  assert.equal(new TextDecoder().decode(context.calls.at(-1).bytes), "after resync");
});

test("an agent without active work remains selected without terminal discovery", async () => {
  const context = terminalHarness();
  context.controller.start();
  context.ready();
  context.controller.selectAgent(secondAgent);
  const token = {};
  context.controller.beginTerminalSurface(token);
  context.controller.setTerminalSurface(token, { write: async () => {}, abort: () => {} });
  await flush();
  assert.equal(context.targetGates.length, 0);
  assert.equal(context.latest().selectedAgent.id, secondAgent.id);
  assert.equal(context.latest().terminal.phase, "idle");
  assert.equal(context.sessionCloses(), 0);
});

test("a blocked task presents the configured agent as idle without terminal discovery", async () => {
  const context = terminalHarness();
  const tasks = new Map(fixtureState.tasks);
  const running = [...tasks.values()].find((task) => task.assigned_agent_id === agent.id && task.status === "running");
  assert.notEqual(running, undefined);
  tasks.set(running.id, { ...running, status: "blocked" });
  context.controller.start();
  context.clientOptions().onState(stateAt(43, { tasks }));
  context.clientOptions().onStatus("ready");
  context.controller.selectAgent(agent);
  const token = {};
  context.controller.beginTerminalSurface(token);
  context.controller.setTerminalSurface(token, { write: async () => {}, abort: () => {} });
  await flush();
  assert.equal(context.targetGates.length, 0);
  assert.equal(context.latest().selectedAgent.id, agent.id);
  assert.equal(context.latest().terminal.phase, "idle");
  assert.equal(context.sessionCloses(), 0);
  context.clientOptions().onState(stateAt(44, { tasks }));
  await flush();
  assert.equal(context.targetGates.length, 0, "unrelated heads must not invent a terminal");
});

test("same-head task refresh closes discovery waiting after the run becomes terminal", async () => {
  const context = terminalHarness();
  context.controller.start();
  context.ready();
  context.controller.selectAgent(agent);
  const token = {};
  context.controller.beginTerminalSurface(token);
  context.controller.setTerminalSurface(token, { write: async () => {}, abort: () => {} });
  await flush();
  context.targetGates[0].resolve(null);
  await flush();
  assert.equal(context.latest().selectedAgent.id, agent.id);

  const tasks = new Map(fixtureState.tasks);
  const running = [...tasks.values()].find((task) => task.assigned_agent_id === agent.id && task.status === "running");
  assert.notEqual(running, undefined);
  tasks.set(running.id, { ...running, status: "blocked", revision: running.revision + 1n });
  context.clientOptions().onState(stateAt(fixtureState.head, { tasks }));
  await flush();
  assert.equal(context.latest().selectedAgent.id, agent.id);
  assert.equal(context.latest().terminal.phase, "idle");
  assert.equal(context.sessionCloses(), 0);
  assert.equal(context.targetGates.length, 1);
});

test("fatal discovery and terminal failures disarm selection before reconnect can rediscover", async () => {
  for (const failure of ["throw", "open", "attach", "input", "resize", "surface"]) {
    const context = terminalHarness({ closeStatus: true, fail: failure === "throw" ? undefined : failure });
    context.controller.start();
    context.ready();
    context.controller.selectAgent(agent);
    const token = {};
    context.controller.beginTerminalSurface(token);
    context.controller.setTerminalSurface(token, {
      write: async (bytes) => { if (failure === "surface") throw new Error("display failed"); },
      abort: () => {},
    });
    await flush();
    assert.equal(context.targetGates.length, 1, failure);
    if (failure === "throw") context.targetGates[0].reject(new SessionError("connection"));
    else context.targetGates[0].resolve(target);
    await flush();
    if (failure === "input") {
      context.controller.sendTerminalText(token, "input");
      await flush();
    } else if (failure === "resize") {
      context.controller.resizeTerminal(token, 24, 80);
      await flush();
    } else if (failure === "surface") {
      await assert.rejects(context.handleOptions().onOutput({ sequence: 0n, payload: new Uint8Array([1]) }));
      await flush();
    }
    assert.equal(context.targetGates.length, 1, failure);
    assert.equal(context.sessionCloses(), 1, failure);
    await assertNoAutomaticRestart(context);
  }
});

test("fatal lifecycle state, not an error-code exception, fences reusable failure codes", async () => {
  for (const error of [new SessionError("invalid_request"), new ProtocolError("malformed")]) {
    for (const failure of ["resolve", "open", "attach", "input", "resize", "surface"]) {
      const context = terminalHarness({ closeStatus: true, fail: failure === "resolve" ? undefined : failure, failError: error });
      context.controller.start();
      context.ready();
      context.controller.selectAgent(agent);
      const token = {};
      context.controller.beginTerminalSurface(token);
      context.controller.setTerminalSurface(token, {
        write: async () => { if (failure === "surface") throw error; },
        abort: () => {},
      });
      await flush();
      if (failure === "resolve") context.targetGates[0].reject(error);
      else context.targetGates[0].resolve(target);
      await flush();
      if (failure === "input") {
        context.controller.sendTerminalText(token, "input");
        await flush();
      } else if (failure === "resize") {
        context.controller.resizeTerminal(token, 24, 80);
        await flush();
      } else if (failure === "surface") {
        await assert.rejects(context.handleOptions().onOutput({ sequence: 0n, payload: new Uint8Array([1]) }));
        await flush();
      }
      assert.equal(context.sessionCloses(), 1, `${error.code}:${failure}`);
      await assertNoAutomaticRestart(context);
    }
  }
});

test("a fresh explicit selection can start one new terminal attempt after a fatal failure", async () => {
  const context = terminalHarness({ closeStatus: true });
  context.controller.start();
  context.ready();
  context.controller.selectAgent(agent);
  const token = {};
  context.controller.beginTerminalSurface(token);
  context.controller.setTerminalSurface(token, { write: async () => {}, abort: () => {} });
  await flush();
  context.targetGates[0].reject(new SessionError("connection"));
  await flush();
  assert.equal(context.latest().selectedAgent, undefined);
  assert.equal(context.sessionCloses(), 1);
  context.clientOptions().onStatus("connecting");
  context.clientOptions().onState(fixtureState);
  context.clientOptions().onStatus("ready");
  context.controller.selectAgent(agent);
  const freshToken = {};
  context.controller.beginTerminalSurface(freshToken);
  context.controller.setTerminalSurface(freshToken, { write: async () => {}, abort: () => {} });
  await flush();
  assert.equal(context.targetGates.length, 2);
});

test("selection detaches the old controller before presenting a paused idle agent and fences stale callbacks", async () => {
  const detachGate = deferred();
  const context = terminalHarness({ detachImpl: () => detachGate.promise });
  context.controller.start();
  context.ready();
  const first = await openTerminal(context, agent);
  const oldOptions = first.options;
  context.controller.selectAgent(secondAgent);
  context.controller.sendTerminalText(first.token, "do not cross agents");
  assert.equal(context.sessionCloses(), 0);
  assert.equal(context.latest().selectedAgent.id, agent.id, "old terminal stays mounted while detach is pending");
  assert.equal(context.latest().terminal.phase, "closing");
  assert.equal(context.targetGates.length, 1);
  const latestSecondAgent = { ...secondAgent, revision: secondAgent.revision + 1n };
  const agents = new Map(fixtureState.agents);
  agents.set(secondAgent.id, latestSecondAgent);
  context.clientOptions().onState(stateAt(43, { agents }));
  detachGate.resolve();
  await flush();
  assert.equal(context.latest().selectedAgent.id, secondAgent.id);
  assert.equal(context.latest().selectedAgent.revision, latestSecondAgent.revision);
  assert.equal(context.latest().terminal.phase, "idle");
  assert.equal(context.latest().terminal.paused, true);
  assert.equal(context.sessionCloses(), 0);
  assert.equal(context.targetGates.length, 1, "an idle paused agent has no terminal to resolve");
  oldOptions.onExit();
  assert.equal(context.latest().selectedAgent.id, secondAgent.id);
  assert.equal(context.calls.some((call) => call.kind === "input"), false);
  assert.equal(context.latest().terminal.phase, "idle");
});

test("selected-agent revision drift cannot escalate an explicit detach to session close", async () => {
  const detachGate = deferred();
  const context = terminalHarness({ detachImpl: () => detachGate.promise });
  context.controller.start();
  context.ready();
  await openTerminal(context, agent);

  context.controller.clearAgentTerminal();
  assert.equal(context.latest().terminal.phase, "closing");
  const revisedAgent = { ...agent, revision: agent.revision + 1n };
  const revisedAgents = new Map(fixtureState.agents);
  revisedAgents.set(revisedAgent.id, revisedAgent);
  context.clientOptions().onState(stateAt(43, { agents: revisedAgents }));
  assert.equal(context.sessionCloses(), 0);
  assert.equal(context.latest().selectedAgent.id, agent.id, "closing host remains mounted until detach resolves");

  detachGate.resolve();
  await flush();
  assert.equal(context.sessionCloses(), 0);
  assert.equal(context.latest().selectedAgent, undefined);
  assert.equal(context.latest().error, undefined, "the explicit close intent wins over concurrent revision drift");
});

test("a pending same-agent rebind follows the latest canonical revision", async () => {
  const detachGate = deferred();
  const context = terminalHarness({ detachImpl: () => detachGate.promise });
  context.controller.start();
  context.ready();
  await openTerminal(context, agent);

  const firstRevision = { ...agent, paused: true, revision: agent.revision + 1n };
  const firstAgents = new Map(fixtureState.agents);
  firstAgents.set(agent.id, firstRevision);
  context.clientOptions().onState(stateAt(43, { agents: firstAgents }));
  assert.equal(context.latest().terminal.phase, "closing");

  const latestRevision = { ...agent, paused: false, revision: agent.revision + 2n };
  const latestAgents = new Map(fixtureState.agents);
  latestAgents.set(agent.id, latestRevision);
  context.clientOptions().onState(stateAt(44, { agents: latestAgents }));

  detachGate.resolve();
  await flush();
  assert.equal(context.latest().selectedAgent.id, agent.id);
  assert.equal(context.latest().selectedAgent.revision, latestRevision.revision);
  assert.equal(context.latest().terminal.paused, false);
  assert.equal(context.latest().error, undefined);
  assert.equal(context.sessionCloses(), 0);
});

test("output failure during replacement closes the session and never installs the queued agent", async () => {
  const detachGate = deferred();
  const context = terminalHarness({ detachImpl: () => detachGate.promise, fail: "surface" });
  context.controller.start();
  context.ready();
  const first = await openTerminal(context, agent);

  context.controller.selectAgent(secondAgent);
  await assert.rejects(first.options.onOutput({ sequence: 0n, payload: new Uint8Array([1]) }), (error) => error.code === "closed");
  first.options.onClose(new SessionError("connection"));
  await flush();
  assert.equal(context.sessionCloses(), 1);
  assert.equal(context.latest().selectedAgent, undefined);
  assert.equal(context.targetGates.length, 1, "replacement discovery never started");

  detachGate.resolve();
  await flush();
  assert.equal(context.targetGates.length, 1, "late detach completion stays fenced");
});

test("a prior lease refusal does not turn a clean terminal switch into session failure", async () => {
  const context = terminalHarness({ fail: "acquire", failError: new SessionError("stale") });
  context.controller.start();
  context.ready();
  await openTerminal(context, agent);
  assert.equal(context.latest().terminal.writable, false);
  assert.equal(context.latest().terminal.error.code, "stale");

  context.controller.selectAgent(secondAgent);
  await flush();
  assert.equal(context.sessionCloses(), 0);
  assert.equal(context.latest().selectedAgent.id, secondAgent.id);
  assert.equal(context.latest().terminal.error, undefined);
});

test("terminal exit keeps the selected task pinned until durable finalization", async () => {
  const context = terminalHarness();
  context.controller.start();
  context.ready();
  const terminal = await openTerminal(context);
  terminal.options.onClose();
  assert.equal(context.latest().terminal.phase, "ready", "the handle closes before its typed exit callback");
  terminal.options.onExit();
  assert.equal(context.latest().selectedAgent.id, agent.id);
  assert.equal(context.latest().terminal.taskTitle, "Review the state projection");
  assert.equal(context.latest().terminal.finishing, true);
  assert.equal(context.latest().terminal.queued, false);
  assert.equal(await context.controller.enqueueAgentInstruction("must wait"), false);
  assert.equal(context.latest().error, undefined);
  assert.equal(context.sessionCloses(), 0);

  context.clientOptions().onState(stateAt(43));
  assert.equal(context.latest().terminal.finishing, true, "an unrelated public head cannot reopen a finished terminal");

  const tasks = new Map(fixtureState.tasks);
  const running = [...tasks.values()].find((task) => task.assigned_agent_id === agent.id && task.status === "running");
  assert.notEqual(running, undefined);
  tasks.set(running.id, { ...running, status: "succeeded", revision: running.revision + 1n });
  context.clientOptions().onState(stateAt(44, { tasks }));
  assert.equal(context.latest().selectedAgent.id, agent.id);
  assert.equal(context.latest().terminal.phase, "idle");
  assert.equal(context.latest().terminal.taskTitle, undefined);
  assert.equal(context.latest().terminal.finishing, false);
  assert.equal(context.latest().terminal.queued, false);
});

test("the retired terminal surface is fenced while the selected task finalizes", async () => {
  const context = terminalHarness();
  context.controller.start();
  context.ready();
  const terminal = await openTerminal(context);

  const retiredSurfaceVersion = context.latest().terminal.surfaceVersion;
  terminal.options.onExit();
  assert.equal(context.latest().selectedAgent.id, agent.id);
  assert.equal(context.latest().terminal.taskTitle, "Review the state projection");
  assert.equal(context.latest().terminal.finishing, true);

  const tasks = new Map(fixtureState.tasks);
  const running = [...tasks.values()].find((task) => task.assigned_agent_id === agent.id && task.status === "running");
  assert.notEqual(running, undefined);
  tasks.set(running.id, { ...running, status: "succeeded", revision: running.revision + 1n });
  context.clientOptions().onState(stateAt(43, { tasks }));
  assert.equal(context.latest().terminal.taskTitle, undefined);
  assert.equal(context.latest().terminal.finishing, false);

  // The old Xterm cleanup callback carries its retired surface version.
  context.controller.endTerminalSurface(terminal.token, retiredSurfaceVersion);
  assert.equal(context.latest().selectedAgent.id, agent.id);
  assert.equal(context.latest().terminal.taskTitle, undefined);
  assert.equal(context.latest().error, undefined);
  assert.equal(await context.controller.enqueueAgentInstruction("ready for the next task"), true);
});

test("clean exit after the selected task terminals clears its queued identity", async () => {
  const context = terminalHarness();
  context.controller.start();
  const initialTasks = new Map(fixtureState.tasks);
  for (const [id, task] of initialTasks) if (task.assigned_agent_id === thirdAgent.id && task.status === "queued") initialTasks.delete(id);
  context.ready(stateAt(42, { tasks: initialTasks }));
  context.controller.selectAgent(thirdAgent);
  assert.equal(await context.controller.enqueueAgentInstruction("Run once"), true);

  const runningTask = {
    id: "51".repeat(16), project_id: thirdAgent.project_id, assigned_agent_id: thirdAgent.id,
    title: "Direct instruction", status: "running", priority: 0, revision: 1n,
  };
  const runningTasks = new Map(initialTasks);
  runningTasks.set(runningTask.id, runningTask);
  context.clientOptions().onState(stateAt(43, { tasks: runningTasks }));
  const terminal = await openTerminal(context, thirdAgent);

  const terminalTasks = new Map(runningTasks);
  terminalTasks.set(runningTask.id, { ...runningTask, status: "succeeded", revision: 2n });
  context.clientOptions().onState(stateAt(44, { tasks: terminalTasks }));
  terminal.options.onClose();
  terminal.options.onExit();
  assert.equal(context.latest().selectedAgent.id, thirdAgent.id);
  assert.equal(context.latest().terminal.phase, "idle");
  assert.equal(context.latest().terminal.taskTitle, undefined);
  assert.equal(context.latest().terminal.queued, false);
});

test("clean exit immediately adopts different running work at the current head", async () => {
  const context = terminalHarness();
  context.controller.start();
  context.ready();
  const terminal = await openTerminal(context, agent);
  const tasks = new Map(fixtureState.tasks);
  const taskA = [...tasks.values()].find((task) => task.assigned_agent_id === agent.id && task.status === "running");
  assert.notEqual(taskA, undefined);
  tasks.set(taskA.id, { ...taskA, status: "succeeded", revision: taskA.revision + 1n });
  const taskB = {
    ...taskA,
    id: "63".repeat(16),
    title: "Replacement work",
    status: "running",
    revision: 1n,
  };
  tasks.set(taskB.id, taskB);
  context.clientOptions().onState(stateAt(43, { tasks }));

  terminal.options.onClose();
  terminal.options.onExit();
  assert.equal(context.latest().selectedAgent.id, agent.id);
  assert.equal(context.latest().terminal.taskTitle, "Replacement work");
  assert.equal(context.latest().terminal.phase, "idle");
  await remountSurface(context);
  assert.equal(context.targetGates.length, 2, "the current replacement is not hidden by a same-head retry fence");
  assert.deepEqual(context.calls.at(-1).value, {
    agentId: agent.id,
    expectedAgentRevision: agent.revision,
    expectedHead: 43n,
  });
});

test("HumanRequest terminal intent uses only its current public agent relationship and preserves detail draft authority", async () => {
  const context = terminalHarness();
  context.controller.start();
  context.ready();
  await context.controller.selectHumanRequest(request);
  context.controller.setHumanReply("keep this draft");
  context.controller.openTerminalForHumanRequest(request);
  assert.equal(context.latest().selectedAgent.id, agent.id);
  assert.equal(context.latest().selectedHumanRequest.reply, "keep this draft");

  const stale = { ...request, revision: request.revision + 1n };
  context.controller.clearAgentTerminal();
  context.controller.openTerminalForHumanRequest(stale);
  assert.equal(context.latest().error.code, "stale");

  const missingAgent = { ...request, agent_id: "ab".repeat(16) };
  context.clientOptions().onState(stateAt(42, { humanRequests: new Map([[missingAgent.id, missingAgent]]) }));
  context.controller.openTerminalForHumanRequest(missingAgent);
  assert.equal(context.latest().error.code, "stale");
});

test("selected terminal close detaches once, keeps the session ready, and permits reopen", async () => {
  const context = terminalHarness();
  context.controller.start();
  context.ready();
  await openTerminal(context);
  context.controller.clearAgentTerminal();
  context.controller.clearAgentTerminal();
  await flush();
  assert.deepEqual(context.calls.slice(-1).map((call) => call.kind), ["detach"]);
  assert.equal(context.sessionCloses(), 0);
  assert.equal(context.latest().error, undefined);
  assert.equal(context.latest().selectedAgent, undefined);

  context.clientOptions().onState(fixtureState);
  context.clientOptions().onStatus("ready");
  context.controller.selectAgent(agent);
  const token = {};
  context.controller.beginTerminalSurface(token);
  context.controller.setTerminalSurface(token, { write: async () => {}, abort: () => {} });
  await flush();
  context.targetGates.at(-1).resolve(target);
  await flush();
  assert.equal(context.latest().terminal.phase, "ready");
});

test("terminal failure is finite and never exposes protocol authority or output bytes in the public snapshot", async () => {
  const context = terminalHarness();
  context.controller.start();
  context.ready();
  const terminal = await openTerminal(context);
  terminal.options.onClose(new SessionError("connection"));
  const snapshotText = JSON.stringify(context.latest(), (_key, value) => typeof value === "bigint" ? value.toString() : value);
  assert.equal(snapshotText.includes("runId"), false);
  assert.equal(snapshotText.includes("sessionId"), false);
  assert.equal(snapshotText.includes("lease"), false);
  assert.equal(snapshotText.includes("same session"), false);
  assert.equal(context.latest().terminal, undefined);
});

test("post-target stale closure cannot re-enter discovery retry", async () => {
  const context = terminalHarness();
  context.controller.start();
  context.ready();
  await openTerminal(context);
  context.handleOptions().onClose(new SessionError("stale"));
  await flush();
  assert.equal(context.latest().selectedAgent, undefined);
  assert.equal(context.latest().error.code, "stale");
  assert.equal(context.sessionCloses(), 0);
  context.clientOptions().onState(stateAt(43));
  await flush();
  assert.equal(context.targetGates.length, 1);
});

async function remountSurface(context) {
  const token = {};
  const writes = [];
  context.controller.beginTerminalSurface(token);
  context.controller.setTerminalSurface(token, { write: async (bytes) => { writes.push(bytes); }, abort: () => {} });
  await flush();
  return { token, writes };
}

test("a server replay reset resumes at its head after the retained floor advances", async () => {
  const context = terminalHarness();
  context.controller.start();
  context.ready();
  await openTerminal(context);
  const resolvesBefore = context.targetGates.length;
  const versionBefore = context.latest().terminal.surfaceVersion;

  context.handleOptions().onReset({ sessionId: "31".repeat(16), floor: 5n, head: 9n });
  await flush();
  let view = context.latest();
  assert.notEqual(view.selectedAgent, undefined, "reset must keep the terminal selection");
  assert.equal(view.error, undefined, "reset is not an error");
  assert.equal(view.terminal.resets, 1);
  assert.equal(view.terminal.phase, "idle", "old controller gone, awaiting the remounted display");
  assert.equal(view.terminal.surfaceVersion, versionBefore + 1, "display remounts to clear the stale scrollback");
  assert.equal(context.sessionCloses(), 0);

  const resumed = await remountSurface(context);
  assert.equal(context.targetGates.length, resolvesBefore + 1, "a fresh controller re-resolves the target");
  context.targetGates.at(-1).resolve(target);
  await flush();
  view = context.latest();
  assert.equal(view.terminal.phase, "ready");
  assert.equal(view.terminal.resets, 1, "the banner state survives the successful re-replay");
  assert.equal(context.calls.filter((call) => call.kind === "open").at(-1).afterSequence, 9n, "the reset head stays inside the later retained range");
  assert.equal(context.calls.filter((call) => call.kind === "open").at(-1).afterSessionId, "31".repeat(16), "the retry binds the cursor to its reset session");
  await context.handleOptions().onOutput({ sequence: 9n, payload: new TextEncoder().encode("new output") });
  context.handleOptions().onOutputComplete?.();
  assert.equal(new TextDecoder().decode(resumed.writes[0]), "new output");
  context.controller.sendTerminalText(resumed.token, "next");
  await flush();
  assert.deepEqual(context.calls.filter((call) => call.kind === "input").map((call) => new TextDecoder().decode(call.bytes)), ["next"]);
});

test("a reset while holding control recovers and re-acquires through the normal path", async () => {
  const context = terminalHarness();
  context.controller.start();
  context.ready();
  await openTerminal(context);
  assert.equal(context.latest().terminal.writable, true);

  context.handleOptions().onReset({ sessionId: "31".repeat(16), floor: 5n, head: 9n });
  await flush();
  assert.equal(context.latest().terminal.writable, false, "reset revokes local control honestly");

  await remountSurface(context);
  context.targetGates.at(-1).resolve(target);
  await flush();
  const view = context.latest();
  assert.equal(view.terminal.phase, "ready");
  assert.equal(view.terminal.writable, true, "control returns only through the normal acquire path");
  assert.equal(view.terminal.resets, 1);
});

test("a reset during attachment drops buffered input before the replacement becomes writable", async () => {
  const context = terminalHarness();
  context.setAttachReset(true);
  context.controller.start();
  context.ready();
  context.controller.selectAgent(agent);
  const token = {};
  context.controller.beginTerminalSurface(token);
  context.controller.sendTerminalText(token, "stale");
  context.controller.setTerminalSurface(token, { write: async () => {}, abort: () => {} });
  await flush();
  context.targetGates.at(-1).resolve(target);
  await flush();
  assert.equal(context.latest().error, undefined);
  assert.equal(context.calls.some((call) => call.kind === "input"), false, "reset arrives before buffered input can flush");

  context.setAttachReset(false);
  const resumed = await remountSurface(context);
  context.targetGates.at(-1).resolve(target);
  await flush();
  assert.equal(context.latest().terminal.phase, "ready");
  assert.equal(context.calls.some((call) => call.kind === "input"), false, "recovery never flushes input queued before its reset");
  context.controller.sendTerminalText(resumed.token, "fresh");
  await flush();
  assert.deepEqual(context.calls.filter((call) => call.kind === "input").map((call) => new TextDecoder().decode(call.bytes)), ["fresh"]);
});

test("a reset storm is bounded: past three recoveries the stale teardown stands", async () => {
  const context = terminalHarness();
  context.setAttachReset(true);
  context.controller.start();
  context.ready();
  context.controller.selectAgent(agent);
  for (let attempt = 0; attempt < 4; attempt += 1) {
    await remountSurface(context);
    const gate = context.targetGates.at(-1);
    if (gate === undefined) break;
    gate.resolve(target);
    await flush();
  }
  const view = context.latest();
  assert.equal(view.selectedAgent, undefined, "past the bound the ordinary teardown stands");
  assert.equal(view.error?.code, "stale");
});
