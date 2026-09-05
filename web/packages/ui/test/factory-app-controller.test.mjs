import assert from "node:assert/strict";
import test from "node:test";
import { SessionError } from "@dark-factory/client";
import { FactoryAppController } from "../dist/src/factory-app-controller.js";
import { fixtureState } from "../../../fixtures/state.mjs";

const challenge = "51".repeat(32);
const request = [...fixtureState.humanRequests.values()][0];

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
    getTopology: overrides.getTopology ?? (async () => { throw new SessionError("not_found"); }),
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

  await context.controller.editTask(queued, { priority: 11 });
  assert.deepEqual(sent.at(-1), ["task", { taskId: queued.id, expectedRevision: queued.revision, priority: 11 }]);

  // Nothing is sent while the session is not ready.
  const settled = sent.length;
  context.emitStatus("syncing");
  await context.controller.editTask(queued, { cancel: true });
  assert.equal(sent.length, settled);
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

test("the floor's topology is fetched once per demand and absence is tolerated", async () => {
  const project = [...fixtureState.projects.values()][0];
  const topology = { projectId: project.id, digest: "ab".repeat(32), sourceRevision: "", nodes: [] };
  let pending = deferred();
  let requests = 0;
  const context = harness({ getTopology: (id) => { requests += 1; assert.equal(id, project.id); return pending.promise; } });
  context.controller.start();
  context.emitState(fixtureState);
  context.emitStatus("ready");

  context.controller.loadTopology();
  context.controller.loadTopology();
  assert.equal(requests, 1, "one request is in flight at a time");
  pending.resolve(topology);
  await pending.promise;
  await Promise.resolve();
  assert.deepEqual(context.latest().topology, topology);

  // A daemon that cannot serve topology leaves the last floor standing.
  pending = deferred();
  context.controller.loadTopology();
  pending.reject(new SessionError("not_found"));
  await pending.promise.catch(() => {});
  await Promise.resolve();
  assert.deepEqual(context.latest().topology, topology);
  assert.equal(requests, 2);

  // A floor belongs to its project; when that project is gone, so is it.
  context.emitState({ ...fixtureState, projects: new Map() });
  assert.equal(context.latest().topology, undefined);
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
