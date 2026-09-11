import assert from "node:assert/strict";
import test, { mock } from "node:test";
import { SessionError } from "@dark-factory/client";
import { FactoryAppController } from "../dist/src/factory-app-controller.js";
import { fixtureState } from "../../../fixtures/state.mjs";

const challenge = "51".repeat(32);
const request = [...fixtureState.humanRequests.values()][0];
const runningAgentID = [...fixtureState.agents.keys()][0];

/** Drain the microtasks one poll round settles through. */
const settle = async () => { for (let turn = 0; turn < 5; turn += 1) await Promise.resolve(); };

function deferred() {
  let resolve;
  let reject;
  const promise = new Promise((accept, refuse) => { resolve = accept; reject = refuse; });
  return { promise, resolve, reject };
}

function detailFor(item = request, suffix = "", replyMaxBytes = 8192) {
  return Object.freeze({
    requestId: item.id,
    revision: item.revision,
    question: `Should I continue${suffix}?`,
    canReply: true,
    replyMaxBytes,
    terminalTarget: Object.freeze({ target: suffix }),
    cancelRun: Object.freeze({ requestId: item.id, expectedRequestRevision: item.revision, expectedRunRevision: 17n }),
  });
}

const remoteInvite = Object.freeze({
  link: "https://app.darkfactory.build/remote#df_remote&node=n0&expires=1767225600",
  expiresAtMs: 1767225600000n,
  svg: "<svg viewBox=\"0 0 1 1\"/>",
});

function stateWithRequests(items) {
  return { ...fixtureState, humanRequests: new Map(items.map((item) => [item.id, item])) };
}

function harness(overrides = {}) {
  const snapshots = [];
  const statusChanges = [];
  const calls = { connect: 0, close: 0 };
  const session = {
    getHumanRequestDetail: overrides.getDetail ?? (async () => detailFor()),
    replyHumanRequest: overrides.reply ?? (async () => ({ status: "resolved" })),
    cancelHumanRequest: overrides.cancel ?? (async () => ({ request_id: request.id })),
    updateAgent: overrides.updateAgent ?? (async () => { throw new SessionError("not_found"); }),
    updateTask: overrides.updateTask ?? (async () => { throw new SessionError("not_found"); }),
    getTaskHistory: overrides.getTaskHistory ?? (async () => { throw new SessionError("not_found"); }),
    getTaskDetail: overrides.getTaskDetail ?? (async () => { throw new SessionError("not_found"); }),
    getTopology: overrides.getTopology ?? (async () => { throw new SessionError("not_found"); }),
    getRunPaths: overrides.getRunPaths ?? (async () => { throw new SessionError("not_found"); }),
    discoverAccounts: overrides.discoverAccounts ?? (async () => []),
    updateAccount: overrides.updateAccount ?? (async () => ({ accountId: "00".repeat(16), revision: 2n })),
    linkAccount: overrides.linkAccount ?? (async () => ({ accountId: "00".repeat(16), revision: 1n })),
    inviteRemote: overrides.inviteRemote ?? (async () => remoteInvite),
    capabilities: overrides.capabilities ?? 15,
  };
  const client = {
    session,
    connect: () => { calls.connect += 1; return overrides.connect?.() ?? Promise.resolve(); },
    close: () => { calls.close += 1; overrides.close?.(); },
  };
  let clientOptions;
  const historyState = { route: "factory" };
  const controller = new FactoryAppController({
    origin: overrides.origin ?? "https://app.darkfactory.build",
    location: { hash: overrides.hash ?? `#df_pair=${challenge}`, pathname: "/factory", search: "?preview=1" },
    history: {
      state: historyState,
      replaceState: (state, _title, url) => {
        overrides.order?.push("scrub");
        overrides.replaceState?.(state, url);
      },
    },
    onChange: (snapshot) => snapshots.push(snapshot),
    onStatusChange: (status) => statusChanges.push(status),
    clientFactory: (options) => {
      overrides.order?.push("create");
      clientOptions = options;
      overrides.clientFactory?.(options);
      return client;
    },
  });
  return {
    controller,
    client,
    clientOptions: () => clientOptions,
    snapshots,
    statusChanges,
    calls,
    emitStatus: (status) => clientOptions.onStatus(status),
    emitState: (state) => clientOptions.onState(state),
    emitError: (error) => clientOptions.onError(error),
    latest: () => snapshots.at(-1),
    historyState,
  };
}

test("pairing is scrubbed before exact client construction and connection", () => {
  const order = [];
  let replacement;
  const context = harness({
    order,
    replaceState: (state, url) => { replacement = { state, url }; },
    connect: () => { order.push("connect"); return Promise.resolve(); },
  });
  context.controller.start();
  assert.deepEqual(order, ["scrub", "create", "connect"]);
  assert.deepEqual(replacement, { state: context.historyState, url: "/factory?preview=1" });
  assert.equal(context.clientOptions().url, "ws://127.0.0.1:43123/browser");
  assert.equal(context.clientOptions().host, "127.0.0.1:43123");
  assert.equal(context.clientOptions().origin, "https://app.darkfactory.build");
  assert.equal(context.clientOptions().challenge, challenge);
});

test("status changes reach the host with finite closed reasons", () => {
  const context = harness();
  context.controller.start();
  context.emitStatus("ready");
  assert.deepEqual(context.statusChanges, [{ status: "ready" }]);

  context.emitError(new SessionError("pairing_required"));
  context.emitStatus("closed");
  assert.deepEqual(context.statusChanges, [{ status: "ready" }, { status: "closed", reason: "pairing_required" }]);
});

test("status changes are deduplicated without affecting snapshot updates", () => {
  const context = harness();
  context.controller.start();
  context.emitStatus("connecting");
  context.emitStatus("connecting");
  context.emitState(fixtureState);
  assert.deepEqual(context.statusChanges, [{ status: "connecting" }]);
  assert.equal(context.snapshots.length, 3);
});

test("task-detail pages cover a maximum outcome with one head and peer continuation", async () => {
  const outcome = "x".repeat(131_072);
  const task = [...fixtureState.tasks.values()][0];
  const calls = [];
  const context = harness({
    getTaskHistory: async (taskID) => ({ taskId: taskID, entries: [] }),
    getTaskDetail: async (taskID, revision, offsets) => {
      calls.push([taskID, revision, offsets]);
      if (offsets.peerOffset === 1n) {
        assert.equal(offsets.expectedHead, 9n);
        return { taskId: taskID, revision, head: 9n, instruction: "", feedback: "", peerQuestions: [], nextPeerOffset: undefined };
      }
      const offset = Number(offsets.textOffset);
      assert.equal(offsets.expectedHead, offset === 0 ? undefined : 9n);
      return {
        taskId: taskID, revision, head: 9n,
        instruction: offset === 0 ? "first" : offset === 2048 ? "second" : "",
        feedback: offset === 0 ? "review " : offset === 2048 ? "feedback" : "",
        outcome: outcome.slice(offset, offset + 2048), peerQuestions: [],
        ...(offset + 2048 < outcome.length ? { nextTextOffset: BigInt(offset + 2048) } : {}),
        ...(offset === 0 ? { nextPeerOffset: 1n } : {}),
      };
    },
  });
  context.controller.start();
  context.emitStatus("ready");

  const first = await context.controller.taskDetail(task);
  assert.equal(first.head, 9n);
  assert.equal(first.instruction, "firstsecond");
  assert.equal(first.feedback, "review feedback");
  assert.equal(first.outcome, outcome, "all 128 KiB survive the 64-page boundary");
  assert.deepEqual(calls, Array.from({ length: 64 }, (_, index) => [
    task.id, task.revision,
    { textOffset: BigInt(index * 2048), peerOffset: 0n, ...(index === 0 ? {} : { expectedHead: 9n }) },
  ]));

  await context.controller.taskDetail(task, first.nextPeerOffset, first.head);
  assert.deepEqual(calls.at(-1), [task.id, task.revision, { textOffset: 0n, peerOffset: 1n, expectedHead: 9n }]);
  const history = await context.controller.taskHistory(task);
  assert.deepEqual(history, { taskId: task.id, entries: [] });
  context.controller.close();
});

test("a failed fragment scrub creates no client or browser effect", () => {
  let clients = 0;
  const snapshots = [];
  const controller = new FactoryAppController({
    origin: "https://app.darkfactory.build",
    location: { hash: `#df_pair=${challenge}`, pathname: "/", search: "" },
    history: { state: null, replaceState: () => { throw new Error("history unavailable"); } },
    onChange: (snapshot) => snapshots.push(snapshot),
    clientFactory: () => { clients += 1; throw new Error("must not construct"); },
  });
  controller.start();
  assert.equal(clients, 0);
  const snapshot = snapshots.at(-1);
  assert.equal(snapshot.status, "closed");
  assert.equal(snapshot.error.code, "connection");
  assert.equal(snapshot.error.retryable, false);
});

test("a failed client construction is closed without a client", () => {
  const snapshots = [];
  const controller = new FactoryAppController({
    origin: "https://app.darkfactory.build",
    location: { hash: "", pathname: "/", search: "" },
    history: { state: null, replaceState: () => {} },
    onChange: (snapshot) => snapshots.push(snapshot),
    clientFactory: () => { throw new Error("construction failed"); },
  });
  controller.start();
  const snapshot = snapshots.at(-1);
  assert.equal(snapshot.status, "closed");
});

test("factory construction cannot retain a client after a reentrant close", () => {
  const snapshots = [];
  let controller;
  let callbacks;
  let connects = 0;
  let closes = 0;
  controller = new FactoryAppController({
    origin: "https://app.darkfactory.build",
    location: { hash: "", pathname: "/", search: "" },
    history: { state: null, replaceState: () => {} },
    onChange: (snapshot) => {
      snapshots.push(snapshot);
      controller.close();
    },
    clientFactory: (options) => {
      callbacks = options;
      options.onStatus("ready");
      return {
        session: {},
        connect: () => { connects += 1; return Promise.resolve(); },
        close: () => {
          closes += 1;
          options.onStatus("connecting");
          options.onState(fixtureState);
        },
      };
    },
  });

  controller.start();
  const published = snapshots.length;
  callbacks.onStatus("closed");
  callbacks.onState(fixtureState);
  assert.equal(connects, 0);
  assert.equal(closes, 1);
  assert.equal(snapshots.length, published);
});

test("a reentrant close during connect closes the exact installed client once", () => {
  const snapshots = [];
  let controller;
  let callbacks;
  let connects = 0;
  let closes = 0;
  controller = new FactoryAppController({
    origin: "https://app.darkfactory.build",
    location: { hash: "", pathname: "/", search: "" },
    history: { state: null, replaceState: () => {} },
    onChange: (snapshot) => {
      snapshots.push(snapshot);
      if (snapshot.status === "ready") controller.close();
    },
    clientFactory: (options) => {
      callbacks = options;
      return {
        session: {},
        connect: () => {
          connects += 1;
          options.onStatus("ready");
          return Promise.resolve();
        },
        close: () => { closes += 1; options.onStatus("connecting"); },
      };
    },
  });

  controller.start();
  const published = snapshots.length;
  callbacks.onStatus("closed");
  assert.equal(connects, 1);
  assert.equal(closes, 1);
  assert.equal(snapshots.length, published);
});

test("current callbacks publish while close is once-only and fences synchronous or late callbacks", () => {
  let context;
  context = harness({
    close: () => {
      context.emitStatus("connecting");
      context.emitState(fixtureState);
      context.emitError(new SessionError("connection", true));
    },
  });
  context.controller.start();
  context.emitStatus("ready");
  assert.equal(context.latest().status, "ready");
  const beforeClose = context.snapshots.length;
  context.controller.close();
  context.controller.close();
  const beforeStatusChange = context.statusChanges.length;
  context.emitStatus("closed");
  context.emitState(fixtureState);
  assert.equal(context.calls.close, 1);
  assert.equal(context.snapshots.length, beforeClose);
  assert.equal(context.statusChanges.length, beforeStatusChange);
});

test("detail selection is exact, unique, view-only, and reconnect clears private state", async () => {
  const pending = deferred();
  let detailRequest;
  let detailRequests = 0;
  let replies = 0;
  let cancels = 0;
  const context = harness({
    getDetail: (value) => { detailRequests += 1; detailRequest = value; return pending.promise; },
    reply: async () => { replies += 1; },
    cancel: async () => { cancels += 1; },
  });
  context.controller.start();
  context.emitState(fixtureState);
  context.emitStatus("ready");
  const selected = context.controller.selectHumanRequest(request);
  await context.controller.selectHumanRequest(request);
  assert.equal(detailRequests, 1);
  assert.deepEqual(detailRequest, { requestId: request.id, expectedRevision: request.revision });
  assert.equal(replies, 0);
  assert.equal(cancels, 0);
  assert.equal(context.latest().selectedHumanRequest.phase, "loading");
  const detail = detailFor();
  pending.resolve(detail);
  await selected;
  assert.equal(context.latest().selectedHumanRequest.question, detail.question);
  assert.equal(context.latest().selectedHumanRequest.canCancel, true);
  context.emitStatus("connecting");
  assert.equal(context.latest().selectedHumanRequest, undefined);
});

test("reply and cancel each consume only their exact returned authority once", async () => {
  const replyDone = deferred();
  const cancelDone = deferred();
  const replyCalls = [];
  const cancelCalls = [];
  let nextDetail = detailFor();
  const context = harness({
    getDetail: async () => nextDetail,
    reply: (detail, value) => { replyCalls.push([detail, value]); return replyDone.promise; },
    cancel: (descriptor) => { cancelCalls.push(descriptor); return cancelDone.promise; },
  });
  context.controller.start();
  context.emitState(fixtureState);
  context.emitStatus("ready");

  await context.controller.selectHumanRequest(request);
  context.controller.setHumanReply("Continue with the simpler path.");
  const firstReply = context.controller.replyHumanRequest();
  const duplicateReply = context.controller.replyHumanRequest();
  assert.deepEqual(replyCalls, [[nextDetail, "Continue with the simpler path."]]);
  replyDone.resolve({ status: "resolved" });
  await Promise.all([firstReply, duplicateReply]);
  assert.equal(context.latest().selectedHumanRequest, undefined);

  nextDetail = detailFor(request, " after review");
  await context.controller.selectHumanRequest(request);
  const firstCancel = context.controller.cancelHumanRequest();
  const duplicateCancel = context.controller.cancelHumanRequest();
  assert.deepEqual(cancelCalls, [nextDetail.cancelRun]);
  cancelDone.resolve({ request_id: request.id });
  await Promise.all([firstCancel, duplicateCancel]);
  assert.equal(context.latest().selectedHumanRequest, undefined);
});

test("reply drafts obey the exact daemon byte bound before retention or delivery", async () => {
  const replies = [];
  const boundedDetail = detailFor(request, "", 8);
  const context = harness({
    getDetail: async () => boundedDetail,
    reply: async (detail, value) => { replies.push([detail, value]); },
  });
  context.controller.start();
  context.emitState(fixtureState);
  context.emitStatus("ready");
  await context.controller.selectHumanRequest(request);

  context.controller.setHumanReply("ok");
  assert.equal(context.latest().selectedHumanRequest.reply, "ok");

  const OriginalTextEncoder = globalThis.TextEncoder;
  let encoded = 0;
  globalThis.TextEncoder = class {
    encode() { encoded += 1; throw new Error("oversized strings must not be encoded"); }
  };
  try {
    context.controller.setHumanReply("x".repeat(1_000_000));
  } finally {
    globalThis.TextEncoder = OriginalTextEncoder;
  }
  assert.equal(encoded, 0);
  assert.equal(context.latest().error.code, "too_large");
  assert.equal(context.latest().selectedHumanRequest.reply, "ok");

  context.controller.setHumanReply("😀😀");
  assert.equal(context.latest().selectedHumanRequest.reply, "😀😀");
  context.controller.setHumanReply("😀😀😀");
  assert.equal(context.latest().error.code, "too_large");
  assert.equal(context.latest().selectedHumanRequest.reply, "😀😀");

  await context.controller.replyHumanRequest();
  assert.deepEqual(replies, [[boundedDetail, "😀😀"]]);
});

test("an empty reply is refused without consuming its exact request authority", async () => {
  let replies = 0;
  const context = harness({ reply: async () => { replies += 1; } });
  context.controller.start();
  context.emitState(fixtureState);
  context.emitStatus("ready");
  await context.controller.selectHumanRequest(request);

  await context.controller.replyHumanRequest();
  assert.equal(replies, 0);
  assert.equal(context.latest().error.code, "invalid_request");
  assert.equal(context.latest().selectedHumanRequest.phase, "ready");
});

test("deletion or revision change clears detail and fences a late private response", async () => {
  const pending = deferred();
  let getDetail = () => pending.promise;
  const context = harness({ getDetail: (value) => getDetail(value) });
  context.controller.start();
  context.emitState(fixtureState);
  context.emitStatus("ready");
  const selection = context.controller.selectHumanRequest(request);
  context.emitState(stateWithRequests([]));
  assert.equal(context.latest().selectedHumanRequest, undefined);
  pending.resolve(detailFor());
  await selection;
  assert.equal(context.latest().selectedHumanRequest, undefined);

  const revised = { ...request, revision: request.revision + 1n };
  getDetail = async () => detailFor(revised);
  context.emitState(stateWithRequests([revised]));
  await context.controller.selectHumanRequest(revised);
  assert.equal(context.latest().selectedHumanRequest.question, detailFor().question);
  context.emitState(stateWithRequests([{ ...revised, revision: revised.revision + 1n }]));
  assert.equal(context.latest().selectedHumanRequest, undefined);
});

test("console edits carry the exact served revision and surface a refusal", async () => {
  const sent = [];
  const agent = fixtureState.agents.get([...fixtureState.agents.keys()][0]);
  const queued = [...fixtureState.tasks.values()].find((task) => task.status === "queued");
  let refuse = false;
  const context = harness({
    updateAgent: async (input) => { sent.push(["agent", input]); if (refuse) throw new SessionError("stale"); return { agentId: input.agentId, revision: input.expectedRevision + 1n }; },
    updateTask: async (input) => { sent.push(["task", input]); return { taskId: input.taskId, revision: input.expectedRevision + 1n }; },
  });
  context.controller.start();
  context.emitState(fixtureState);
  context.emitStatus("ready");
  context.controller.selectAgent(agent);

  // Pausing alone sends paused alone: the daemon only revalidates a launch
  // control when the patch touches it, so an agent whose stored model it no
  // longer accepts must still be pausable.
  await context.controller.updateAgentConfig({ paused: true });
  assert.deepEqual(sent.at(-1), ["agent", { agentId: agent.id, expectedRevision: agent.revision, paused: true }]);
  assert.equal(context.latest().edit, undefined, "a settled edit leaves no state behind");

  await context.controller.updateAgentConfig({ model: "claude-opus-5", reasoningEffort: "high", paused: true });
  assert.deepEqual(sent.at(-1), ["agent", { agentId: agent.id, expectedRevision: agent.revision, model: "claude-opus-5", reasoningEffort: "high", paused: true }]);

  // An empty patch is not a write, so it cannot bump a revision for nothing.
  const before = sent.length;
  await context.controller.updateAgentConfig({});
  assert.equal(sent.length, before);

  refuse = true;
  await context.controller.updateAgentConfig({ model: "gone", reasoningEffort: "", paused: false });
  assert.equal(context.latest().edit.pending, false);
  assert.equal(context.latest().edit.error.code, "stale");
  // Selecting another agent must not inherit the refusal.
  context.controller.selectAgent(fixtureState.agents.get([...fixtureState.agents.keys()][2]));
  assert.equal(context.latest().edit, undefined);

  const taskEdit = context.controller.editTask(queued, { priority: 11 });
  await settle();
  const revisedQueued = { ...queued, priority: 11, revision: queued.revision + 1n };
  context.emitState({ ...fixtureState, tasks: new Map([...fixtureState.tasks, [revisedQueued.id, revisedQueued]]) });
  await taskEdit;
  assert.deepEqual(sent.at(-1), ["task", { taskId: queued.id, expectedRevision: queued.revision, priority: 11 }]);

  // Nothing is sent while the session is not ready.
  const settled = sent.length;
  context.emitStatus("syncing");
  await context.controller.editTask(queued, { cancel: true });
  assert.equal(sent.length, settled);
});

test("a queued edit keeps queue controls disabled until its canonical revision arrives", async () => {
  const agent = fixtureState.agents.get([...fixtureState.agents.keys()][0]);
  const queued = [...fixtureState.tasks.values()].find((task) => task.status === "queued");
  const result = deferred();
  const context = harness({ updateTask: () => result.promise });
  context.controller.start();
  context.emitState(fixtureState);
  context.emitStatus("ready");
  context.controller.selectAgent(agent);

  const save = context.controller.editTask(queued, { title: "saved" });
  await settle();
  result.resolve({ taskId: queued.id, revision: queued.revision + 1n });
  await settle();
  assert.equal(await save, true, "an accepted write keeps the brief's close-on-acceptance contract");
  assert.equal(context.latest().edit.pending, true);
  assert.equal(await context.controller.editTask(queued, { priority: queued.priority + 1 }), false, "the old task cannot reopen or accept another queue action before STATE advances");

  const agentRevised = { ...agent, revision: agent.revision + 1n };
  context.emitState({ ...fixtureState, agents: new Map([...fixtureState.agents, [agentRevised.id, agentRevised]]) });
  assert.equal(context.latest().edit.pending, true, "a same-agent rebind cannot clear the task fence against an old task revision");

  const revised = { ...queued, title: "saved", revision: queued.revision + 2n };
  context.emitState({
    ...fixtureState,
    agents: new Map([...fixtureState.agents, [agentRevised.id, agentRevised]]),
    tasks: new Map([...fixtureState.tasks, [revised.id, revised]]),
  });
  assert.equal(context.latest().edit, undefined);

  const interruptedResult = deferred();
  const interrupted = harness({ updateTask: () => interruptedResult.promise });
  interrupted.controller.start();
  interrupted.emitState(fixtureState);
  interrupted.emitStatus("ready");
  const cancelled = interrupted.controller.editTask(queued, { priority: queued.priority + 1 });
  interruptedResult.resolve({ taskId: queued.id, revision: queued.revision + 1n });
  await settle();
  interrupted.emitStatus("syncing");
  assert.equal(await cancelled, true);
  assert.equal(interrupted.latest().edit, undefined, "a state restart releases the canonical-state fence");
});

test("a same-agent rebind keeps an in-flight queued edit until a later canonical state", async () => {
  const agent = fixtureState.agents.get([...fixtureState.agents.keys()][0]);
  const queued = [...fixtureState.tasks.values()].find((task) => task.status === "queued");
  const result = deferred();
  const context = harness({ updateTask: () => result.promise });
  context.controller.start();
  context.emitState(fixtureState);
  context.emitStatus("ready");
  context.controller.selectAgent(agent);

  const save = context.controller.editTask(queued, { title: "saved" });
  await settle();
  const agentRevised = { ...agent, revision: agent.revision + 1n };
  context.emitState({ ...fixtureState, agents: new Map([...fixtureState.agents, [agentRevised.id, agentRevised]]) });
  assert.equal(context.latest().edit.pending, true, "a same-agent rebind keeps the in-flight write fenced");

  const later = { ...queued, title: "saved", revision: queued.revision + 2n };
  context.emitState({
    ...fixtureState,
    agents: new Map([...fixtureState.agents, [agentRevised.id, agentRevised]]),
    tasks: new Map([...fixtureState.tasks, [later.id, later]]),
  });
  result.resolve({ taskId: queued.id, revision: queued.revision + 1n });
  assert.equal(await save, true, "a canonical revision after the accepted write settles the edit");
  assert.equal(context.latest().edit, undefined);
});

test("a queued edit refuses an acknowledgement that does not advance its revision", async () => {
  const queued = [...fixtureState.tasks.values()].find((task) => task.status === "queued");
  const context = harness({ updateTask: async () => ({ taskId: queued.id, revision: queued.revision }) });
  context.controller.start();
  context.emitState(fixtureState);
  context.emitStatus("ready");

  assert.equal(await context.controller.editTask(queued, { title: "saved" }), false);
  assert.equal(context.latest().edit.pending, false);
  assert.equal(context.latest().edit.error.code, "stale");
});

test("switching agents discards only that queued edit's canonical-state fence", async () => {
  const firstAgent = fixtureState.agents.get([...fixtureState.agents.keys()][0]);
  const secondAgent = fixtureState.agents.get([...fixtureState.agents.keys()][2]);
  const queued = [...fixtureState.tasks.values()].find((task) => task.status === "queued");
  const nextQueued = { ...queued, id: "cd".repeat(16), title: "Second queued edit" };
  const firstResult = deferred();
  const secondResult = deferred();
  const context = harness({
    updateTask: (input) => input.taskId === queued.id ? firstResult.promise : secondResult.promise,
  });
  const initial = { ...fixtureState, tasks: new Map([...fixtureState.tasks, [nextQueued.id, nextQueued]]) };
  context.controller.start();
  context.emitState(initial);
  context.emitStatus("ready");
  context.controller.selectAgent(firstAgent);

  const first = context.controller.editTask(queued, { title: "first saved" });
  await settle();
  firstResult.resolve({ taskId: queued.id, revision: queued.revision + 1n });
  assert.equal(await first, true);
  assert.equal(context.latest().edit.target, queued.id);

  context.controller.selectAgent(secondAgent);
  assert.equal(context.latest().edit, undefined, "changing agents discards the first pending fence");
  const second = context.controller.editTask(nextQueued, { title: "second saved" });
  await settle();
  secondResult.resolve({ taskId: nextQueued.id, revision: nextQueued.revision + 1n });
  assert.equal(await second, true);
  assert.equal(context.latest().edit.target, nextQueued.id);

  const firstRevised = { ...queued, title: "first saved", revision: queued.revision + 1n };
  context.emitState({ ...initial, tasks: new Map([...initial.tasks, [firstRevised.id, firstRevised]]) });
  assert.equal(context.latest().edit.target, nextQueued.id, "the first edit's delayed STATE cannot settle the second fence");
  assert.equal(context.latest().edit.pending, true);

  const secondRevised = { ...nextQueued, title: "second saved", revision: nextQueued.revision + 1n };
  context.emitState({ ...initial, tasks: new Map([...initial.tasks, [firstRevised.id, firstRevised], [secondRevised.id, secondRevised]]) });
  assert.equal(context.latest().edit, undefined);
});

test("leaving the terminal keeps the agent selected; closing the sidebar does not", () => {
  const agent = fixtureState.agents.get([...fixtureState.agents.keys()][0]);
  const context = harness();
  context.controller.start();
  context.emitState(fixtureState);
  context.emitStatus("ready");

  context.controller.selectAgent(agent);
  assert.equal(context.latest().selectedAgent.id, agent.id);
  context.controller.closeAgentTerminal();
  assert.equal(context.latest().selectedAgent.id, agent.id, "back leaves the sidebar on its agent");
  assert.equal(context.latest().error, undefined, "a deliberate teardown is not a fault");

  context.controller.clearAgentTerminal();
  assert.equal(context.latest().selectedAgent, undefined);
  // With nothing selected the two are the same finite no-op.
  context.controller.closeAgentTerminal();
  assert.equal(context.latest().selectedAgent, undefined);
});

test("every project's topology is fetched, kept by id, and refreshed while the floor is shown", async (t) => {
  mock.timers.enable({ apis: ["setInterval"] });
  t.after(() => mock.timers.reset());
  const projects = [...fixtureState.projects.keys()];
  const asked = [];
  const digests = new Map(projects.map((id, index) => [id, `a${index}`.repeat(32)]));
  let answer = async (id) => ({ projectId: id, digest: digests.get(id), sourceRevision: "", nodes: [] });
  const context = harness({ getTopology: (id) => { asked.push(id); return answer(id); } });
  context.controller.start();
  context.emitState(fixtureState);
  context.emitStatus("ready");

  // Every configured project is asked, none is preferred, and one round is in
  // flight at a time however many times the floor asks.
  context.controller.loadTopology();
  context.controller.loadTopology();
  await settle();
  assert.deepEqual(asked, projects);
  assert.deepEqual([...context.latest().topologies.keys()], projects);

  // An unchanged round is not a new snapshot; a changed digest is.
  const published = context.snapshots.length;
  context.controller.loadTopology();
  await settle();
  assert.equal(context.snapshots.length, published);
  digests.set(projects[0], "cd".repeat(32));
  context.controller.loadTopology();
  await settle();
  assert.equal(context.snapshots.length, published + 1);
  assert.equal(context.latest().topologies.get(projects[0]).digest, "cd".repeat(32));

  // A project the daemon cannot serve keeps the structure last served for it,
  // and never costs the other projects theirs.
  const ready = answer;
  answer = async (id) => { if (id === projects[1]) throw new SessionError("not_found"); return ready(id); };
  context.controller.loadTopology();
  await settle();
  assert.deepEqual([...context.latest().topologies.keys()], projects);

  // A project still answering is not asked again; every other one still is.
  const held = deferred();
  answer = async (id) => (id === projects[0] ? held.promise : ready(id));
  context.controller.loadTopology();
  await settle();
  const during = asked.length;
  context.controller.loadTopology();
  await settle();
  assert.deepEqual(asked.slice(during), [projects[1]]);
  held.resolve(await ready(projects[0]));
  await settle();
  answer = ready;

  // Code changes while the floor stays open: the run-paths timer re-reads the
  // structure every sixth tick, and only then.
  const rounds = asked.length;
  context.controller.watchRunPaths(true);
  for (let tick = 0; tick < 5; tick += 1) {
    mock.timers.tick(10_000);
    await settle();
  }
  assert.equal(asked.length, rounds);
  mock.timers.tick(10_000);
  await settle();
  assert.deepEqual(asked.slice(rounds), projects);
  context.controller.watchRunPaths(false);

  // A floor belongs to its project; when that project is gone, so is it.
  context.emitState({ ...fixtureState, projects: new Map([[projects[0], fixtureState.projects.get(projects[0])]]) });
  assert.deepEqual([...context.latest().topologies.keys()], [projects[0]]);
});

test("run paths are polled for running agents only while the floor is shown", async (t) => {
  mock.timers.enable({ apis: ["setInterval"] });
  t.after(() => mock.timers.reset());
  const asked = [];
  let answer = async (agentId) => ({ agentId, runId: "0a".repeat(16), paths: ["web/packages/ui"] });
  // Only the first project's structure is served; the second project has a
  // running agent too, and nobody asks where it is standing.
  const [servedProject, otherProject] = [...fixtureState.projects.keys()];
  const runningTask = [...fixtureState.tasks.values()].find((task) => task.assigned_agent_id === runningAgentID && task.status === "running");
  const otherAgent = [...fixtureState.agents.values()].find((agent) => agent.project_id === otherProject).id;
  const state = { ...fixtureState, tasks: new Map([...fixtureState.tasks,
    ["0b".repeat(16), { id: "0b".repeat(16), project_id: otherProject, assigned_agent_id: otherAgent, title: "Unserved", status: "running", priority: 1, revision: 1n }]]) };
  const context = harness({
    getRunPaths: (agentId) => { asked.push(agentId); return answer(agentId); },
    getTopology: async (id) => { if (id !== servedProject) throw new SessionError("not_found"); return { projectId: id, digest: "ab".repeat(32), sourceRevision: "", nodes: [] }; },
  });
  context.controller.start();
  context.emitState(state);
  context.emitStatus("ready");

  // One timer however many times the floor asks, and only the agents that are
  // on a running task in a served project are asked at all: the first round
  // has no structure yet, and the one the structure's arrival triggers does.
  context.controller.loadTopology();
  context.controller.watchRunPaths(true);
  context.controller.watchRunPaths(true);
  await settle();
  await settle();
  assert.deepEqual(asked, [runningAgentID]);
  assert.deepEqual([...context.latest().runPaths], [[runningAgentID, {
    taskId: runningTask.id,
    taskRevision: runningTask.revision,
    projectId: servedProject,
    runId: "0a".repeat(16),
    paths: ["web/packages/ui"],
  }]]);

  // Ten seconds is the cadence, and an unchanged round is not a new snapshot.
  const published = context.snapshots.length;
  mock.timers.tick(10_000);
  await settle();
  assert.equal(asked.length, 2);
  assert.equal(context.snapshots.length, published);

  // A refused answer leaves the worker where it was last seen.
  answer = async () => { throw new SessionError("not_found"); };
  mock.timers.tick(10_000);
  await settle();
  assert.equal(asked.length, 3);
  assert.deepEqual([...context.latest().runPaths], [[runningAgentID, {
    taskId: runningTask.id,
    taskRevision: runningTask.revision,
    projectId: servedProject,
    runId: "0a".repeat(16),
    paths: ["web/packages/ui"],
  }]]);

  // An agent that is no longer running loses its entry.
  context.emitState({ ...state, tasks: new Map() });
  mock.timers.tick(10_000);
  await settle();
  assert.equal(asked.length, 3);
  assert.equal(context.latest().runPaths.size, 0);

  // A hidden floor stops the timer, and so does leaving ready.
  context.controller.watchRunPaths(false);
  context.emitState(state);
  mock.timers.tick(10_000);
  await settle();
  assert.equal(asked.length, 3);

  answer = async (agentId) => ({ agentId, runId: "0a".repeat(16), paths: ["internal/kernel"] });
  context.controller.watchRunPaths(true);
  await settle();
  assert.equal(asked.length, 4);
  assert.deepEqual([...context.latest().runPaths], [[runningAgentID, {
    taskId: runningTask.id,
    taskRevision: runningTask.revision,
    projectId: servedProject,
    runId: "0a".repeat(16),
    paths: ["internal/kernel"],
  }]]);
  context.emitStatus("syncing");
  mock.timers.tick(10_000);
  await settle();
  assert.equal(asked.length, 4);
});

test("a late path answer from an earlier revision only becomes a retained observation", async () => {
  const [projectId] = [...fixtureState.projects.keys()];
  const pending = deferred();
  let asked = 0;
  const context = harness({
    getRunPaths: () => { asked += 1; return pending.promise; },
    getTopology: async (id) => ({ projectId: id, digest: "ab".repeat(32), sourceRevision: "", nodes: [] }),
  });
  const original = [...fixtureState.tasks.values()].find((task) => task.assigned_agent_id === runningAgentID && task.status === "running");
  context.controller.start();
  context.emitState(fixtureState);
  context.emitStatus("ready");
  context.controller.loadTopology();
  await settle();
  context.controller.watchRunPaths(true);
  await settle();
  assert.equal(asked, 1);
  const revised = { ...original, revision: original.revision + 1n };
  context.emitState({ ...fixtureState, tasks: new Map([[revised.id, revised]]) });
  assert.equal(context.latest().state.tasks.get(revised.id).revision, revised.revision);
  pending.resolve({ agentId: runningAgentID, runId: "0a".repeat(16), paths: ["web"] });
  await settle();
  assert.equal(context.latest().runPaths.size, 0);
  assert.deepEqual([...context.latest().lastRunPaths], [[runningAgentID, {
    taskId: original.id,
    taskRevision: original.revision,
    projectId,
    runId: "0a".repeat(16),
    paths: ["web"],
  }]]);
  context.controller.watchRunPaths(false);
});

test("account discovery links once then refreshes its provider view", async () => {
  const discovered = [];
  const linked = [];
  const updated = [];
  const account = { provider: "codex", home: "/private/account", label: "work", email: "", organization: "", default_model: "", default_reasoning_effort: "", linked_id: "" };
  const context = harness({
    discoverAccounts: async () => { discovered.push(true); return [account]; },
    updateAccount: async (request) => { updated.push(request); return { accountId: request.accountId, revision: request.expectedRevision + 1n }; },
    linkAccount: async (request) => { linked.push(request); return { accountId: "00".repeat(16), revision: 1n }; },
  });
  context.controller.start();
  context.emitStatus("ready");
  await context.controller.loadAccounts();
  assert.deepEqual(context.latest().accounts, [account]);
  await context.controller.linkAccount({ provider: "codex", home: account.home, label: account.label });
  assert.deepEqual(linked, [{ provider: "codex", home: account.home, label: account.label }]);
  assert.equal(discovered.length, 2, "link refreshes the discovered provider view");
  const request = { accountId: "00".repeat(16), expectedRevision: 1n, label: "personal" };
  await context.controller.updateAccount(request);
  assert.deepEqual(updated, [request]);
  assert.equal(discovered.length, 3, "update refreshes the discovered provider view");
  assert.equal(context.latest().accountsPending, false);
});

test("a remote invitation is offered, stored, dismissed, and its failure reported", async () => {
  const context = harness();
  context.controller.start();
  context.emitStatus("ready");
  assert.equal(context.latest().remoteInviteAllowed, true);
  await context.controller.inviteRemote();
  assert.deepEqual(context.latest().remoteInvite, { link: remoteInvite.link, svg: remoteInvite.svg, expiresAtMs: remoteInvite.expiresAtMs });
  assert.equal(context.latest().remoteInviteError, undefined);
  context.controller.dismissRemoteInvite();
  assert.equal(context.latest().remoteInvite, undefined);

  // The remote grant carries human_actions but never terminal_input: a paired
  // phone is never offered the button that would propagate its own pairing.
  for (const capabilities of [1, 7]) {
    const weaker = harness({ capabilities });
    weaker.controller.start();
    weaker.emitStatus("ready");
    assert.equal(weaker.latest().remoteInviteAllowed, false, String(capabilities));
  }

  // The mint is never retried: the finite code is what the console shows.
  const failing = harness({ inviteRemote: async () => { throw new SessionError("not_found"); } });
  failing.controller.start();
  failing.emitStatus("ready");
  await failing.controller.inviteRemote();
  assert.equal(failing.latest().remoteInvite, undefined);
  assert.equal(failing.latest().remoteInviteError, "not_found");
});

test("one invitation is minted at a time and a dropped connection leaves no stale code", async () => {
  let mints = 0;
  const release = deferred();
  const context = harness({ inviteRemote: async () => { mints += 1; await release.promise; return remoteInvite; } });
  context.controller.start();
  context.emitStatus("ready");
  const first = context.controller.inviteRemote();
  await context.controller.inviteRemote();
  assert.equal(mints, 1, "a second press while one mint is in flight is ignored");
  release.resolve();
  await first;
  assert.equal(context.latest().remoteInvite.link, remoteInvite.link);

  // The challenge belongs to the connection that minted it.
  context.emitStatus("syncing");
  assert.equal(context.latest().remoteInvite, undefined);
  assert.equal(context.latest().remoteInviteAllowed, false);
});

test("closing the controller stops the run-paths timer", async (t) => {
  mock.timers.enable({ apis: ["setInterval"] });
  t.after(() => mock.timers.reset());
  const asked = [];
  const [servedProject] = [...fixtureState.projects.keys()];
  const context = harness({
    getRunPaths: async (agentId) => { asked.push(agentId); return { agentId, runId: "0a".repeat(16), paths: [] }; },
    getTopology: async (id) => ({ projectId: id, digest: "ab".repeat(32), sourceRevision: "", nodes: [] }),
  });
  context.controller.start();
  context.emitState(fixtureState);
  context.emitStatus("ready");
  context.controller.loadTopology();
  context.controller.watchRunPaths(true);
  await settle();
  await settle();
  mock.timers.tick(10_000);
  await settle();
  const before = asked.length;
  assert.ok(before >= 1);
  context.controller.close();
  mock.timers.tick(30_000);
  await settle();
  assert.equal(asked.length, before);
});

test("a structure arriving during a run-paths round is asked as soon as the round answers, and only while shown", async (t) => {
  mock.timers.enable({ apis: ["setInterval"] });
  t.after(() => mock.timers.reset());
  const [firstProject, secondProject] = [...fixtureState.projects.keys()];
  const secondAgent = [...fixtureState.agents.values()].find((agent) => agent.project_id === secondProject).id;
  const state = { ...fixtureState, tasks: new Map([...fixtureState.tasks,
    ["0b".repeat(16), { id: "0b".repeat(16), project_id: secondProject, assigned_agent_id: secondAgent, title: "Second", status: "running", priority: 1, revision: 1n }]]) };
  const asked = [];
  const firstAnswer = deferred();
  const secondStructure = deferred();
  const context = harness({
    getRunPaths: (agentId) => { asked.push(agentId); return agentId === runningAgentID && asked.length === 1 ? firstAnswer.promise : Promise.resolve({ agentId, runId: "0a".repeat(16), paths: [] }); },
    getTopology: async (id) => {
      if (id === secondProject) await secondStructure.promise;
      return { projectId: id, digest: (id === firstProject ? "ab" : "cd").repeat(32), sourceRevision: "", nodes: [] };
    },
  });
  context.controller.start();
  context.emitState(state);
  context.emitStatus("ready");
  context.controller.loadTopology();
  context.controller.watchRunPaths(true);
  await settle();
  await settle();
  // The first structure fired a round that is still in flight.
  assert.deepEqual(asked, [runningAgentID]);
  // The second structure lands meanwhile: nothing is asked until the round
  // answers, then both served projects' agents are asked without a tick.
  secondStructure.resolve();
  await settle();
  await settle();
  assert.deepEqual(asked, [runningAgentID]);
  firstAnswer.resolve({ agentId: runningAgentID, runId: "0a".repeat(16), paths: [] });
  await settle();
  await settle();
  assert.deepEqual(asked.slice(1).sort(), [runningAgentID, secondAgent].sort());

  // The same arrival while the floor is being hidden owes nothing.
  const late = deferred();
  const hidden = [];
  const lateStructure = deferred();
  const other = harness({
    getRunPaths: (agentId) => { hidden.push(agentId); return hidden.length === 1 ? late.promise : Promise.resolve({ agentId, runId: "0a".repeat(16), paths: [] }); },
    getTopology: async (id) => {
      if (id === secondProject) await lateStructure.promise;
      return { projectId: id, digest: (id === firstProject ? "ab" : "cd").repeat(32), sourceRevision: "", nodes: [] };
    },
  });
  other.controller.start();
  other.emitState(state);
  other.emitStatus("ready");
  other.controller.loadTopology();
  other.controller.watchRunPaths(true);
  await settle();
  await settle();
  lateStructure.resolve();
  await settle();
  await settle();
  other.controller.watchRunPaths(false);
  late.resolve({ agentId: runningAgentID, runId: "0a".repeat(16), paths: [] });
  await settle();
  await settle();
  assert.deepEqual(hidden, [runningAgentID]);
});
