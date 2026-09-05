import assert from "node:assert/strict";
import test from "node:test";
import {
  BrowserSession,
  BrowserClient,
  CAPABILITIES,
  MAX_TASK_INSTRUCTION_BYTES,
  buildAuthTranscript,
  consumePairingChallenge,
  hexBytes,
  verifyP256Signature,
  ProtocolError,
  SessionError,
  decodeClientControl,
  encodeHello,
  encodePairResult,
  encodeServerError,
  encodeAuthResult,
  encodeHumanRequestCancelRunResult,
  encodeHumanRequestDetail,
  encodeHumanRequestReplyResult,
  encodeServerControl,
  encodeRemoteInviteResult,
  encodeTaskEnqueueResult,
  encodeStateChanged,
  encodeStateSnapshot,
  encodeTerminalAttached,
  encodeTerminalExit,
  encodeTerminalReset,
  encodeTerminalTarget,
} from "../dist/src/index.js";

const challenge = "11".repeat(32);
const daemonID = "22".repeat(16);
const bootID = "33".repeat(16);
const nonce = "44".repeat(32);
const clientID = "55".repeat(16);
const runID = "66".repeat(16);
const factory = (revision = 1n) => ({ dispatch_enabled: true, capacity: 8, active_runs: 0, revision });

class MemoryKeys {
  value = null;
  async load() { return this.value; }
  async save(value) { this.value = value; }
}

class Socket {
  readyState = 1;
  onopen = null;
  onmessage = null;
  onerror = null;
  onclose = null;
  sent = [];
  constructor(server) {
    this.server = server;
    queueMicrotask(() => this.onmessage?.({ data: encodeHello({ daemon_id: daemonID, boot_id: bootID, connection_nonce: nonce }) }));
  }
  send(data) { this.sent.push(data); this.server(this, decodeClientControl(data)); }
  reply(data) { queueMicrotask(() => this.onmessage?.({ data })); }
  close() { this.readyState = 3; this.onclose?.({ code: 1000 }); }
}

function serverFor(socket) {
  const frame = decodeClientControl(socket.sent.at(-1));
  const capabilities = CAPABILITIES.observe | CAPABILITIES.private_human_request_detail | CAPABILITIES.human_actions | CAPABILITIES.terminal_input;
  if (frame.type === "PAIR_PROVE") socket.reply(encodePairResult(frame.id, { client_id: clientID, capabilities }));
  if (frame.type === "AUTH_PROVE") socket.reply(encodeAuthResult(frame.id, { client_id: clientID, capabilities }));
  if (frame.type === "STATE_GET") replySnapshot(socket, frame, 1n);
}

function snapshotBody(head, overrides = {}) {
  return { head, factory: factory(head === 0n ? 1n : head), projects: [], agents: [], tasks: [], human_requests: [], ...overrides };
}

function replySnapshot(socket, frame, head, overrides = {}) {
  socket.reply(encodeStateSnapshot(frame.id, snapshotBody(head, overrides)));
}

function tick() { return new Promise((resolve) => setTimeout(resolve, 0)); }

function lastFrame(socket, type) {
  return decodeClientControl(socket.sent.findLast((wire) => decodeClientControl(wire).type === type));
}

async function openControlledStateSession(options = {}) {
  let socket;
  let automatic = true;
  const capabilities = CAPABILITIES.observe | CAPABILITIES.private_human_request_detail | CAPABILITIES.human_actions | CAPABILITIES.terminal_input;
  const server = (current, frame) => {
    if (frame.type === "PAIR_PROVE") current.reply(encodePairResult(frame.id, { client_id: clientID, capabilities }));
    if (frame.type === "AUTH_PROVE") current.reply(encodeAuthResult(frame.id, { client_id: clientID, capabilities }));
    if (frame.type === "STATE_GET" && automatic) replySnapshot(current, frame, 1n);
  };
  const session = new BrowserSession({
    url: "ws://127.0.0.1/browser", host: "127.0.0.1", origin: "https://preview.example",
    challenge, keyStore: new MemoryKeys(), socketFactory: () => { socket = new Socket(server); return socket; }, ...options,
  });
  await session.connect();
  await new Promise((resolve) => setTimeout(resolve, 10));
  assert.equal(session.status, "ready");
  automatic = false;
  return { session, socket };
}

async function completePendingSnapshot(socket, head, overrides = {}) {
  const frame = decodeClientControl(socket.sent.at(-1));
  assert.equal(frame.type, "STATE_GET");
  replySnapshot(socket, frame, head, overrides);
  await tick();
}

function stateRequests(socket) {
  return socket.sent.map((wire) => decodeClientControl(wire)).filter((frame) => frame.type === "STATE_GET");
}

async function openHumanSession(onError) {
  let socket;
  const session = new BrowserSession({
    url: "ws://127.0.0.1:43123/browser",
    host: "127.0.0.1:43123",
    origin: "https://preview.example",
    challenge,
    keyStore: new MemoryKeys(),
    socketFactory: () => { socket = new Socket(serverFor); return socket; },
    onError,
  });
  await session.connect();
  await new Promise((resolve) => setTimeout(resolve, 10));
  return { session, socket };
}

function humanDetail(requestId, overrides = {}) {
  return {
    request_id: requestId,
    revision: 1n,
    question: "Choose",
    can_reply: true,
    reply_max_bytes: 8192,
    terminal_target: { run_id: runID, session_id: "99".repeat(16), run_revision: 1n, session_revision: 1n },
    cancel_run: { expected_request_revision: 1n, expected_run_revision: 1n },
    ...overrides,
  };
}

test("pairing signs through WebCrypto, persists the key, reads one snapshot, and watches", async () => {
  const store = new MemoryKeys();
  const sockets = [];
  const states = [];
  const session = new BrowserSession({
    url: "ws://127.0.0.1:43123/browser",
    host: "127.0.0.1:43123",
    origin: "https://preview.example",
    challenge,
    keyStore: store,
    socketFactory: () => { const socket = new Socket(serverFor); sockets.push(socket); return socket; },
    onState: (state) => states.push(state),
  });
  await session.connect();
  await new Promise((resolve) => setTimeout(resolve, 10));
  assert.equal(session.status, "ready");
  assert.equal(session.clientId, clientID);
  assert.equal(store.value.key.extractable, false);
  assert.equal(states.at(-1).head, 1n);
  assert.equal(decodeClientControl(sockets[0].sent.at(-1)).type, "STATE_WATCH");
  assert.equal(stateRequests(sockets[0]).length, 1, "one coherent snapshot needs one request");
  assert.equal(sockets[0].sent.some((wire) => wire.includes(challenge)), true, "challenge is used only in proof, never URL");
  session.close();
});

test("HumanRequest methods are fenced before HELLO and after close, without a raw getter", async () => {
  let socket;
  const session = new BrowserSession({
    url: "ws://127.0.0.1:43123/browser",
    host: "127.0.0.1:43123",
    origin: "https://preview.example",
    keyStore: new MemoryKeys(),
    socketFactory: () => { socket = new Socket(() => {}); return socket; },
  });
  const connecting = session.connect();
  const detail = session.getHumanRequestDetail({ requestId: "77".repeat(16), expectedRevision: 1n });
  assert.equal("humanRequests" in session, false);
  assert.equal(socket.sent.length, 0);
  session.close();
  await assert.rejects(detail, (error) => error instanceof SessionError && error.code === "unauthorized");
  await assert.rejects(connecting, (error) => error instanceof SessionError && error.code === "closed");
  const closedDetail = session.getHumanRequestDetail({ requestId: "77".repeat(16), expectedRevision: 1n });
  assert.equal(socket.sent.length, 0);
  await assert.rejects(closedDetail, (error) => error instanceof SessionError && error.code === "closed");
});

test("authenticated HumanRequest methods emit exact frames and correlate results", async () => {
  const store = new MemoryKeys();
  let socket;
  const session = new BrowserSession({
    url: "ws://127.0.0.1:43123/browser",
    host: "127.0.0.1:43123",
    origin: "https://preview.example",
    challenge,
    keyStore: store,
    socketFactory: () => { socket = new Socket(serverFor); return socket; },
  });
  await session.connect();
  await new Promise((resolve) => setTimeout(resolve, 10));
  assert.equal("humanRequests" in session, false);

  const requestId = "77".repeat(16);
  const detailPending = session.getHumanRequestDetail({ requestId, expectedRevision: 1n });
  const detailFrame = decodeClientControl(socket.sent.at(-1));
  assert.equal(detailFrame.type, "HUMAN_REQUEST_DETAIL_GET");
  assert.deepEqual(detailFrame.body, { request_id: requestId, expected_revision: 1n });
  socket.reply(encodeHumanRequestDetail(detailFrame.id, {
    request_id: requestId, revision: 1n, question: "Choose", can_reply: true, reply_max_bytes: 8192,
    terminal_target: { run_id: runID, session_id: "99".repeat(16), run_revision: 1n, session_revision: 1n },
    cancel_run: { expected_request_revision: 1n, expected_run_revision: 1n },
  }));
  const detail = await detailPending;
  assert.equal(Object.isFrozen(detail), true);
  assert.equal(Object.isFrozen(detail.terminalTarget), true);
  assert.equal(Object.isFrozen(detail.cancelRun), true);
  assert.equal(typeof session.openTerminal(detail.terminalTarget).attach, "function");

  const reply = session.replyHumanRequest(detail, "ok");
  const replyFrame = decodeClientControl(socket.sent.at(-1));
  assert.equal(replyFrame.type, "HUMAN_REQUEST_REPLY");
  assert.deepEqual(replyFrame.body, { request_id: requestId, expected_revision: 1n, reply: "ok" });
  socket.reply(encodeHumanRequestReplyResult(replyFrame.id, { request_id: requestId, revision: 3n, status: "resolved" }));
  assert.equal((await reply).status, "resolved");
  await assert.rejects(session.replyHumanRequest(detail, "again"), (error) => error instanceof SessionError && error.code === "stale");
  await assert.rejects(session.cancelHumanRequest(detail.cancelRun), (error) => error instanceof SessionError && error.code === "stale");

  const cancelRequestId = "88".repeat(16);
  const cancelDetailPending = session.getHumanRequestDetail({ requestId: cancelRequestId, expectedRevision: 1n });
  const cancelDetailFrame = decodeClientControl(socket.sent.at(-1));
  socket.reply(encodeHumanRequestDetail(cancelDetailFrame.id, {
    request_id: cancelRequestId, revision: 1n, question: "Cancel?", can_reply: true, reply_max_bytes: 8192,
    terminal_target: { run_id: runID, session_id: "99".repeat(16), run_revision: 1n, session_revision: 1n },
    cancel_run: { expected_request_revision: 1n, expected_run_revision: 1n },
  }));
  const cancelDetail = await cancelDetailPending;
  const cancel = session.cancelHumanRequest(cancelDetail.cancelRun);
  const cancelFrame = decodeClientControl(socket.sent.at(-1));
  assert.equal(cancelFrame.type, "HUMAN_REQUEST_CANCEL_RUN");
  assert.deepEqual(cancelFrame.body, { request_id: cancelRequestId, expected_request_revision: 1n, expected_run_revision: 1n });
  socket.reply(encodeHumanRequestCancelRunResult(cancelFrame.id, { run_id: runID, run_revision: 2n, request_id: cancelRequestId, request_revision: 2n }));
  assert.equal((await cancel).run_id, runID);
  await assert.rejects(session.replyHumanRequest(cancelDetail, "too late"), (error) => error instanceof SessionError && error.code === "stale");
  session.close();
});

test("authenticated task enqueue mints exact IDs and correlates the durable result", async () => {
  const { session, socket } = await openHumanSession();
  const agentId = "77".repeat(16);
  const pending = session.enqueueAgentTask({ agentId, expectedAgentRevision: 7n, instruction: "Repair the queue" });
  const frame = decodeClientControl(socket.sent.at(-1));
  assert.equal(frame.type, "TASK_ENQUEUE");
  assert.deepEqual({
    agent_id: frame.body.agent_id,
    expected_agent_revision: frame.body.expected_agent_revision,
    instruction: frame.body.instruction,
  }, {
    agent_id: agentId,
    expected_agent_revision: 7n,
    instruction: "Repair the queue",
  });
  assert.match(frame.body.task_id, /^[0-9a-f]{32}$/);
  assert.match(frame.body.incarnation_id, /^[0-9a-f]{32}$/);
  assert.notEqual(frame.body.task_id, "00".repeat(16));
  assert.notEqual(frame.body.incarnation_id, "00".repeat(16));
  assert.notEqual(frame.body.task_id, frame.body.incarnation_id);

  socket.reply(encodeTaskEnqueueResult(frame.id, {
    task_id: frame.body.task_id,
    revision: 1n,
    agent_revision: 7n,
  }));
  const result = await pending;
  assert.deepEqual(result, { taskId: frame.body.task_id, revision: 1n });
  assert.equal(Object.isFrozen(result), true);

  const unicodeWhitespace = session.enqueueAgentTask({ agentId, expectedAgentRevision: 7n, instruction: "\u0085" });
  const unicodeFrame = decodeClientControl(socket.sent.at(-1));
  assert.equal(unicodeFrame.body.instruction, "\u0085", "wire validation uses the same explicit ASCII blank set as Go");
  socket.reply(encodeTaskEnqueueResult(unicodeFrame.id, {
    task_id: unicodeFrame.body.task_id,
    revision: 1n,
    agent_revision: 7n,
  }));
  await unicodeWhitespace;
  session.close();
});

test("console edits and topology carry exact bodies and correlate their results", async () => {
  const { session, socket } = await openHumanSession();
  const agentId = "78".repeat(16);
  const taskId = "79".repeat(16);
  const projectId = "7d".repeat(16);

  const agentPending = session.updateAgent({ agentId, expectedRevision: 7n, model: "claude-opus-5", paused: true });
  const agentFrame = decodeClientControl(socket.sent.at(-1));
  assert.equal(agentFrame.type, "AGENT_UPDATE");
  // An omitted member never reaches the wire, so it cannot overwrite a value.
  assert.deepEqual(agentFrame.body, { agent_id: agentId, expected_revision: 7n, model: "claude-opus-5", paused: true });
  socket.reply(encodeServerControl({ type: "AGENT_UPDATE_RESULT", id: agentFrame.id, body: { agent_id: agentId, revision: 8n } }));
  assert.deepEqual(await agentPending, { agentId, revision: 8n });

  const taskPending = session.updateTask({ taskId, expectedRevision: 3n, priority: 5, cancel: true });
  const taskFrame = decodeClientControl(socket.sent.at(-1));
  assert.equal(taskFrame.type, "TASK_UPDATE");
  assert.deepEqual(taskFrame.body, { task_id: taskId, expected_revision: 3n, priority: 5, status: "cancelled" });
  socket.reply(encodeServerControl({ type: "TASK_UPDATE_RESULT", id: taskFrame.id, body: { task_id: taskId, revision: 4n } }));
  assert.deepEqual(await taskPending, { taskId, revision: 4n });

  const node = { id: "a1".repeat(32), parent_id: "", kind: "repository", path: ".", label: "repo", language: "", size_bucket: "medium" };
  const topologyPending = session.getTopology(projectId);
  const topologyFrame = decodeClientControl(socket.sent.at(-1));
  assert.equal(topologyFrame.type, "TOPOLOGY_GET");
  socket.reply(encodeServerControl({ type: "TOPOLOGY", id: topologyFrame.id, body: { project_id: projectId, digest: "ab".repeat(32), source_revision: "", nodes: [node] } }));
  const topology = await topologyPending;
  assert.equal(topology.digest, "ab".repeat(32));
  assert.deepEqual(topology.nodes, [node]);

  // A result for another entity is a protocol fault, not a resolution.
  const mismatched = session.updateAgent({ agentId, expectedRevision: 9n, paused: false });
  const mismatchedFrame = decodeClientControl(socket.sent.at(-1));
  await assert.rejects(Promise.all([
    mismatched,
    socket.reply(encodeServerControl({ type: "AGENT_UPDATE_RESULT", id: mismatchedFrame.id, body: { agent_id: "7e".repeat(16), revision: 10n } })),
  ]), (error) => error instanceof ProtocolError && error.code === "malformed");

  // Bounds are refused before anything reaches the socket.
  const closed = new BrowserSession({ url: "ws://127.0.0.1:1/browser", host: "127.0.0.1:1", origin: "http://127.0.0.1:1" });
  for (const request of [
    () => closed.updateAgent({ agentId, expectedRevision: 1n, model: "m".repeat(129) }),
    () => closed.updateTask({ taskId, expectedRevision: 1n, title: "" }),
  ]) await assert.rejects(request(), (error) => error instanceof SessionError);
  closed.close();
  session.close();
});

test("remote invitation correlates its own result and needs bounded human-actions authority", async () => {
  const invitation = {
    link: "https://app.darkfactory.build/remote#df_remote&node=n0&expires=1767225600",
    expires_at_ms: 1767225600000n,
    svg: "<svg viewBox=\"0 0 1 1\"/>",
  };
  const { session, socket } = await openHumanSession();
  const pending = session.inviteRemote();
  const frame = decodeClientControl(socket.sent.at(-1));
  assert.equal(frame.type, "REMOTE_INVITE");
  assert.deepEqual(frame.body, {});
  socket.reply(encodeRemoteInviteResult(frame.id, invitation));
  const result = await pending;
  assert.deepEqual(result, { link: invitation.link, expiresAtMs: invitation.expires_at_ms, svg: invitation.svg });
  assert.equal(Object.isFrozen(result), true);

  // A result nobody asked for is a protocol fault, not a second invitation.
  const errors = [];
  const forged = await openHumanSession((error) => errors.push(error));
  forged.socket.reply(encodeRemoteInviteResult("forged-invite", invitation));
  await tick();
  assert.equal(forged.session.status, "closed");
  assert.equal(errors.at(-1) instanceof ProtocolError, true);
  session.close();

  // The remote grant carries human_actions but never terminal_input, so a
  // paired phone cannot propagate its own pairing to another phone.
  const remoteGrant = CAPABILITIES.observe | CAPABILITIES.private_human_request_detail | CAPABILITIES.human_actions;
  let remoteSocket;
  const remote = new BrowserSession({
    url: "ws://127.0.0.1/browser",
    host: "127.0.0.1",
    origin: "https://preview.example",
    challenge,
    keyStore: new MemoryKeys(),
    socketFactory: () => {
      remoteSocket = new Socket((current, request) => {
        if (request.type === "PAIR_PROVE") current.reply(encodePairResult(request.id, { client_id: clientID, capabilities: remoteGrant }));
        if (request.type === "STATE_GET") replySnapshot(current, request, 1n);
      });
      return remoteSocket;
    },
  });
  await remote.connect();
  await new Promise((resolve) => setTimeout(resolve, 10));
  const sent = remoteSocket.sent.length;
  await assert.rejects(remote.inviteRemote(), (error) => error instanceof SessionError && error.code === "unauthorized");
  assert.equal(remoteSocket.sent.length, sent);
  remote.close();
});

test("closing rejects an authenticated pending task enqueue and fences its late result", async () => {
  const { session, socket } = await openHumanSession();
  const pending = session.enqueueAgentTask({
    agentId: "7c".repeat(16),
    expectedAgentRevision: 12n,
    instruction: "Wait for the durable result",
  });
  const frame = decodeClientControl(socket.sent.at(-1));
  assert.equal(frame.type, "TASK_ENQUEUE");
  const sent = socket.sent.length;

  session.close();
  await assert.rejects(pending, (error) => error instanceof SessionError && error.code === "closed");
  assert.equal(session.status, "closed");
  assert.equal(socket.sent.length, sent);

  socket.reply(encodeTaskEnqueueResult(frame.id, {
    task_id: frame.body.task_id,
    revision: 1n,
    agent_revision: 12n,
  }));
  await tick();
  assert.equal(session.status, "closed");
  assert.equal(socket.sent.length, sent, "a late result cannot restart or replay the operation");
  await assert.rejects(
    session.enqueueAgentTask({ agentId: "7c".repeat(16), expectedAgentRevision: 12n, instruction: "Do not revive" }),
    (error) => error instanceof SessionError && error.code === "closed",
  );
});

test("task enqueue result mismatches close the generation", async (t) => {
  for (const mismatch of ["envelope", "task", "agent-revision"]) {
    await t.test(mismatch, async () => {
      const errors = [];
      const { session, socket } = await openHumanSession((error) => errors.push(error));
      const pending = session.enqueueAgentTask({
        agentId: "78".repeat(16),
        expectedAgentRevision: 8n,
        instruction: "Inspect this",
      });
      const frame = decodeClientControl(socket.sent.at(-1));
      socket.reply(encodeTaskEnqueueResult(
        mismatch === "envelope" ? "forged-task" : frame.id,
        {
          task_id: mismatch === "task" ? "79".repeat(16) : frame.body.task_id,
          revision: 1n,
          agent_revision: mismatch === "agent-revision" ? 9n : 8n,
        },
      ));
      await assert.rejects(pending, (error) => error instanceof ProtocolError && error.code === "malformed");
      assert.equal(session.status, "closed");
      assert.equal(errors.at(-1) instanceof ProtocolError, true);
    });
  }
});

test("task enqueue requires bounded human-actions authority before sending", async () => {
  let socket;
  const session = new BrowserSession({
    url: "ws://127.0.0.1/browser",
    host: "127.0.0.1",
    origin: "https://preview.example",
    challenge,
    keyStore: new MemoryKeys(),
    socketFactory: () => {
      socket = new Socket((current, frame) => {
        if (frame.type === "PAIR_PROVE") current.reply(encodePairResult(frame.id, { client_id: clientID, capabilities: CAPABILITIES.observe }));
        if (frame.type === "STATE_GET") replySnapshot(current, frame, 1n);
      });
      return socket;
    },
  });
  await session.connect();
  await new Promise((resolve) => setTimeout(resolve, 10));
  const sent = socket.sent.length;
  await assert.rejects(
    session.enqueueAgentTask({ agentId: "7a".repeat(16), expectedAgentRevision: 1n, instruction: "No authority" }),
    (error) => error instanceof SessionError && error.code === "unauthorized",
  );
  assert.equal(socket.sent.length, sent);
  session.close();

  const authorized = await openHumanSession();
  const beforeInvalid = authorized.socket.sent.length;
  for (const instruction of ["   ", "x".repeat(MAX_TASK_INSTRUCTION_BYTES + 1)]) {
    await assert.rejects(
      authorized.session.enqueueAgentTask({ agentId: "7b".repeat(16), expectedAgentRevision: 1n, instruction }),
      (error) => error instanceof SessionError && error.code === "invalid_request",
    );
  }
  assert.equal(authorized.socket.sent.length, beforeInvalid);
  authorized.session.close();
});

test("HumanRequest detail operations are bounded and exact-envelope correlated", async (t) => {
  await t.test("envelope, subject, and revision mismatches close the generation", async () => {
    for (const mismatch of ["envelope", "subject", "revision"]) {
      const errors = [];
      const { session, socket } = await openHumanSession((error) => errors.push(error));
      const requestId = "71".repeat(16);
      const pending = session.getHumanRequestDetail({ requestId, expectedRevision: 1n });
      const frame = decodeClientControl(socket.sent.at(-1));
      socket.reply(encodeHumanRequestDetail(mismatch === "envelope" ? "forged-detail" : frame.id, humanDetail(mismatch === "subject" ? "72".repeat(16) : requestId, { revision: mismatch === "revision" ? 2n : 1n, can_reply: false, terminal_target: null, cancel_run: null })));
      await assert.rejects(pending, (error) => error instanceof ProtocolError && error.code === "malformed");
      assert.equal(session.status, "closed", mismatch);
      assert.equal(errors.at(-1) instanceof ProtocolError, true, mismatch);
    }
  });

  await t.test("close settles pending detail without replay", async () => {
    const { session, socket } = await openHumanSession();
    const pending = session.getHumanRequestDetail({ requestId: "73".repeat(16), expectedRevision: 1n });
    const sent = socket.sent.length;
    session.close();
    await assert.rejects(pending, (error) => error instanceof SessionError && error.code === "closed");
    assert.equal(socket.sent.length, sent);
  });

  await t.test("capacity and same-subject gates are finite", async () => {
    const { session } = await openHumanSession();
    const pending = [];
    for (let index = 1; index <= 32; index += 1) {
      const requestId = index.toString(16).padStart(32, "0");
      pending.push(session.getHumanRequestDetail({ requestId, expectedRevision: 1n }).then(() => "resolved", (error) => error));
      if (index === 1) await assert.rejects(session.getHumanRequestDetail({ requestId, expectedRevision: 1n }), (error) => error instanceof SessionError && error.code === "rate_limited");
    }
    await assert.rejects(session.getHumanRequestDetail({ requestId: "ff".repeat(16), expectedRevision: 1n }), (error) => error instanceof SessionError && error.code === "rate_limited");
    session.close();
    const settled = await Promise.all(pending);
    assert.equal(settled.every((error) => error instanceof SessionError && error.code === "closed"), true);
  });
});

test("HumanRequest detail-bound reply and cancellation authority is one-shot", async () => {
  const first = await openHumanSession();
  const unavailableId = "74".repeat(16);
  const unavailablePending = first.session.getHumanRequestDetail({ requestId: unavailableId, expectedRevision: 1n });
  let frame = decodeClientControl(first.socket.sent.at(-1));
  first.socket.reply(encodeHumanRequestDetail(frame.id, humanDetail(unavailableId, { can_reply: false, terminal_target: null, cancel_run: null })));
  const unavailable = await unavailablePending;
  const unavailableSent = first.socket.sent.length;
  await assert.rejects(first.session.replyHumanRequest(unavailable, "no"), (error) => error instanceof SessionError && error.code === "stale");
  assert.equal(first.socket.sent.length, unavailableSent);

  const availableId = "75".repeat(16);
  const availablePending = first.session.getHumanRequestDetail({ requestId: availableId, expectedRevision: 1n });
  frame = decodeClientControl(first.socket.sent.at(-1));
  first.socket.reply(encodeHumanRequestDetail(frame.id, humanDetail(availableId)));
  const available = await availablePending;
  const beforeInvalid = first.socket.sent.length;
  await assert.rejects(first.session.replyHumanRequest(available, "x".repeat(8193)), (error) => error instanceof SessionError && error.code === "invalid_request");
  await assert.rejects(first.session.cancelHumanRequest(Object.freeze({ ...available.cancelRun })), (error) => error instanceof SessionError && error.code === "stale");
  assert.equal(first.socket.sent.length, beforeInvalid);

  first.session.close();
  const second = await openHumanSession();
  await assert.rejects(second.session.replyHumanRequest(available, "old"), (error) => error instanceof SessionError && error.code === "stale");
  await assert.rejects(second.session.cancelHumanRequest(available.cancelRun), (error) => error instanceof SessionError && error.code === "stale");

  const forgedId = "76".repeat(16);
  const forgedDetailPending = second.session.getHumanRequestDetail({ requestId: forgedId, expectedRevision: 1n });
  frame = decodeClientControl(second.socket.sent.at(-1));
  second.socket.reply(encodeHumanRequestDetail(frame.id, humanDetail(forgedId)));
  const forgedDetail = await forgedDetailPending;
  const cancel = second.session.cancelHumanRequest(forgedDetail.cancelRun);
  const cancelFrame = decodeClientControl(second.socket.sent.at(-1));
  second.socket.reply(encodeHumanRequestCancelRunResult(cancelFrame.id, { run_id: "aa".repeat(16), run_revision: 2n, request_id: forgedId, request_revision: 2n }));
  await assert.rejects(cancel, (error) => error instanceof ProtocolError && error.code === "malformed");
  assert.equal(second.session.status, "closed");
});

test("malformed and binary frames fail with finite errors and never leak frame data", async () => {
  const errors = [];
  let socket;
  const session = new BrowserSession({
    url: "ws://127.0.0.1/browser",
    host: "127.0.0.1",
    origin: "https://preview.example",
    keyStore: new MemoryKeys(),
    socketFactory: () => { socket = new Socket(() => {}); return socket; },
    onError: (error) => errors.push(error),
  });
  const pending = session.connect();
  socket.onmessage({ data: new Uint8Array([1, 2, 3]) });
  await assert.rejects(pending, (error) => error instanceof ProtocolError && error.code === "malformed");
  assert.equal(errors.length, 1);
  assert.equal(errors[0].message, "malformed");
  assert.equal(String(errors[0]).includes("1,2,3"), false);
});

test("a closed pairing generation rejects as uncertain and never saves a permanent key", async () => {
  const store = new MemoryKeys();
  let socket;
  const session = new BrowserSession({ url: "ws://127.0.0.1/browser", host: "127.0.0.1", origin: "https://preview.example", challenge, keyStore: store, socketFactory: () => { socket = new Socket(() => {}); return socket; } });
  const pending = session.connect();
  await new Promise((resolve) => setTimeout(resolve, 0));
  socket.close();
  await assert.rejects(pending, (error) => error instanceof SessionError && error.code === "pairing_uncertain");
  assert.equal(store.value, null);
});

// One refresh is in flight at a time, a burst collapses into at most one
// trailing refresh, and the session ends at the greatest notified head.
test("a change burst during one refresh produces one in-flight and one trailing refresh", async () => {
  const states = [];
  const { session, socket } = await openControlledStateSession({ onState: (state) => states.push(state) });
  const watch = lastFrame(socket, "STATE_WATCH");
  assert.equal(watch.body.after_head, 1n);
  const initial = stateRequests(socket).length;

  socket.reply(encodeStateChanged(watch.id, { head: 2n }));
  await tick();
  const inflight = decodeClientControl(socket.sent.at(-1));
  assert.equal(inflight.type, "STATE_GET");
  assert.equal(stateRequests(socket).length, initial + 1);

  for (const head of [3n, 4n, 5n]) {
    socket.reply(encodeStateChanged(watch.id, { head }));
    await tick();
  }
  assert.equal(stateRequests(socket).length, initial + 1, "a burst opened more than one in-flight refresh");
  assert.equal(session.state.head, 1n, "an unfinished refresh replaced the published snapshot");

  replySnapshot(socket, inflight, 2n);
  await tick();
  assert.equal(states.at(-1).head, 2n);
  const trailing = decodeClientControl(socket.sent.at(-1));
  assert.equal(trailing.type, "STATE_GET");
  assert.equal(stateRequests(socket).length, initial + 2, "the burst produced more than one trailing refresh");

  replySnapshot(socket, trailing, 5n);
  await tick();
  assert.equal(states.at(-1).head, 5n);
  assert.equal(stateRequests(socket).length, initial + 2, "a satisfied notification kept refreshing");
  session.close();
});

// A snapshot older than a head the session was already told about is not
// published; the session refetches until it catches up.
test("a snapshot older than the greatest notified head is refetched, never published", async () => {
  const states = [];
  const { session, socket } = await openControlledStateSession({ onState: (state) => states.push(state) });
  const watch = lastFrame(socket, "STATE_WATCH");
  const published = states.length;
  socket.reply(encodeStateChanged(watch.id, { head: 4n }));
  await tick();
  const first = decodeClientControl(socket.sent.at(-1));
  replySnapshot(socket, first, 1n);
  await tick();
  assert.equal(states.length, published, "an overtaken snapshot was published");
  assert.equal(session.state.head, 1n);
  const retry = decodeClientControl(socket.sent.at(-1));
  assert.equal(retry.type, "STATE_GET");
  assert.notEqual(retry.id, first.id);
  replySnapshot(socket, retry, 4n);
  await tick();
  assert.equal(states.at(-1).head, 4n);
  session.close();
});

// Publication is monotonic for the life of one session, and a change head that
// repeats or regresses is a protocol fault rather than something to reconcile.
test("published snapshots and notified heads are monotonic", async (t) => {
  await t.test("a regressed snapshot closes the session", async () => {
    const errors = [];
    const { session, socket } = await openControlledStateSession({ onError: (error) => errors.push(error) });
    const watch = lastFrame(socket, "STATE_WATCH");
    socket.reply(encodeStateChanged(watch.id, { head: 2n }));
    await tick();
    const refresh = decodeClientControl(socket.sent.at(-1));
    socket.reply(encodeStateSnapshot(refresh.id, snapshotBody(0n)));
    await tick();
    assert.equal(session.status, "closed");
    assert.equal(errors.at(-1) instanceof ProtocolError, true);
  });
  await t.test("a repeated or regressed change head closes the session", async () => {
    for (const second of [2n, 1n]) {
      const errors = [];
      const { session, socket } = await openControlledStateSession({ onError: (error) => errors.push(error) });
      const watch = lastFrame(socket, "STATE_WATCH");
      socket.reply(encodeStateChanged(watch.id, { head: 2n }));
      await tick();
      socket.reply(encodeStateChanged(watch.id, { head: second }));
      await tick();
      assert.equal(session.status, "closed");
      assert.equal(errors.at(-1) instanceof ProtocolError, true);
    }
  });
});

// A forged correlation cannot inject state: only the exact outstanding request
// id and the exact installed watch id are accepted.
test("forged snapshot and change correlations fail closed", async (t) => {
  await t.test("unknown snapshot id", async () => {
    const errors = [];
    const { session, socket } = await openControlledStateSession({ onError: (error) => errors.push(error) });
    socket.reply(encodeStateSnapshot("forged-state", snapshotBody(9n)));
    await tick();
    assert.equal(session.status, "closed");
    assert.equal(errors.at(-1) instanceof ProtocolError, true);
  });
  await t.test("unknown watch id", async () => {
    const errors = [];
    const { session, socket } = await openControlledStateSession({ onError: (error) => errors.push(error) });
    socket.reply(encodeStateChanged("forged-watch", { head: 9n }));
    await tick();
    assert.equal(session.status, "closed");
    assert.equal(errors.at(-1) instanceof ProtocolError, true);
  });
});

test("reconnect creates a fresh generation and stale socket frames are fenced", async () => {
  const store = new MemoryKeys();
  const sockets = [];
  const pairing = new BrowserSession({ url: "ws://127.0.0.1/browser", host: "127.0.0.1", origin: "https://preview.example", challenge, keyStore: store, socketFactory: () => { const socket = new Socket(serverFor); sockets.push(socket); return socket; } });
  await pairing.connect();
  await new Promise((resolve) => setTimeout(resolve, 10));
  pairing.close();
  const errors = [];
  const timer = new VirtualTimer();
  const client = new BrowserClient({ url: "ws://127.0.0.1/browser", host: "127.0.0.1", origin: "https://preview.example", keyStore: store, timer, reconnectInitialDelayMs: 10, socketFactory: () => { const socket = new Socket(serverFor); sockets.push(socket); return socket; }, onError: (error) => errors.push(error) });
  await client.connect();
  await new Promise((resolve) => setTimeout(resolve, 10));
  sockets[1].close();
  timer.advance(10);
  await new Promise((resolve) => setTimeout(resolve, 10));
  assert.equal(sockets.length, 3, errors.map((error) => error.code).join(","));
  sockets[1].reply(encodeHello({ daemon_id: "aa".repeat(16), boot_id: "bb".repeat(16), connection_nonce: "cc".repeat(32) }));
  await new Promise((resolve) => setTimeout(resolve, 0));
  assert.notEqual(client.session, undefined);
  assert.equal(client.session.status, "ready", errors.map((error) => error.code).join(","));
  client.close();
});

// The old socket keeps its correlation ids. Neither a newer-looking snapshot
// nor a newer change on it may overwrite the reconnected generation.
test("old-socket snapshot and change frames cannot overwrite a reconnected session", async () => {
  const store = new MemoryKeys();
  const sockets = [];
  const states = [];
  const seed = new BrowserSession({ url: "ws://127.0.0.1/browser", host: "127.0.0.1", origin: "https://preview.example", challenge, keyStore: store, socketFactory: () => new Socket(serverFor) });
  await seed.connect();
  await new Promise((resolve) => setTimeout(resolve, 10));
  seed.close();

  const timer = new VirtualTimer();
  const client = new BrowserClient({
    url: "ws://127.0.0.1/browser", host: "127.0.0.1", origin: "https://preview.example", keyStore: store, timer, reconnectInitialDelayMs: 10,
    socketFactory: () => { const socket = new Socket(serverFor); sockets.push(socket); return socket; },
    onState: (state) => states.push(state),
  });
  await client.connect();
  await new Promise((resolve) => setTimeout(resolve, 10));
  const staleSession = client.session;
  const staleSocket = sockets[0];
  const staleWatch = lastFrame(staleSocket, "STATE_WATCH");
  const staleSnapshot = stateRequests(staleSocket).at(-1);

  staleSocket.close();
  timer.advance(10);
  await new Promise((resolve) => setTimeout(resolve, 10));
  assert.equal(sockets.length, 2);
  assert.notEqual(client.session, staleSession);
  assert.equal(client.session.status, "ready");
  assert.equal(client.state.head, 1n);
  const published = states.length;

  staleSocket.readyState = 1;
  staleSocket.onmessage?.({ data: encodeStateChanged(staleWatch.id, { head: 99n }) });
  staleSocket.onmessage?.({ data: encodeStateSnapshot(staleSnapshot.id, snapshotBody(99n)) });
  await new Promise((resolve) => setTimeout(resolve, 10));

  assert.equal(states.length, published, "an old socket published into the reconnected session");
  assert.equal(client.state.head, 1n);
  assert.equal(client.session.status, "ready");
  assert.equal(staleSession.status, "closed");
  client.close();
});

class VirtualTimer {
  now = 0;
  next = 1;
  tasks = new Map();
  delays = [];
  setTimeout(callback, delay) {
    const id = this.next++;
    this.tasks.set(id, { at: this.now + delay, callback });
    this.delays.push(delay);
    return id;
  }
  clearTimeout(id) { this.tasks.delete(id); }
  advance(milliseconds) {
    this.now += milliseconds;
    while (true) {
      const due = [...this.tasks.entries()].filter(([, task]) => task.at <= this.now).sort((a, b) => a[1].at - b[1].at)[0];
      if (due === undefined) return;
      this.tasks.delete(due[0]);
      due[1].callback();
    }
  }
}

test("successful pairing consumes challenge state and reconnects through AUTH after close", async () => {
  const store = new MemoryKeys();
  const timer = new VirtualTimer();
  const sockets = [];
  const client = new BrowserClient({ url: "ws://127.0.0.1/browser", host: "127.0.0.1", origin: "https://preview.example", challenge, keyStore: store, timer, reconnectInitialDelayMs: 10, reconnectMaxDelayMs: 20, socketFactory: () => { const socket = new Socket(serverFor); sockets.push(socket); return socket; } });
  await client.connect();
  await new Promise((resolve) => setTimeout(resolve, 10));
  assert.equal(decodeClientControl(sockets[0].sent.find((wire) => decodeClientControl(wire).type === "PAIR_PROVE")).type, "PAIR_PROVE");
  sockets[0].close();
  assert.deepEqual(timer.delays, [10]);
  timer.advance(10);
  await new Promise((resolve) => setTimeout(resolve, 10));
  assert.equal(sockets.length, 2);
  assert.equal(decodeClientControl(sockets[1].sent[0]).type, "AUTH_PROVE");
  client.close();
});

test("stale saved authorization repairs once through the still-held pairing challenge", async () => {
  const store = new MemoryKeys();
  const seed = new BrowserSession({ url: "ws://127.0.0.1/browser", host: "127.0.0.1", origin: "https://preview.example", challenge, keyStore: store, socketFactory: () => new Socket(serverFor) });
  await seed.connect();
  await new Promise((resolve) => setTimeout(resolve, 10));
  seed.close();
  const sockets = [];
  const client = new BrowserClient({
    url: "ws://127.0.0.1/browser", host: "127.0.0.1", origin: "https://preview.example", challenge, keyStore: store,
    socketFactory: () => { const socket = new Socket((current, frame) => {
      if (frame.type === "AUTH_PROVE") current.reply(encodeServerError({ code: "unauthorized", retryable: false }));
      if (frame.type === "PAIR_PROVE") current.reply(encodePairResult(frame.id, { client_id: "77".repeat(16), capabilities: CAPABILITIES.observe }));
    }); sockets.push(socket); return socket; },
  });
  await client.connect();
  await new Promise((resolve) => setTimeout(resolve, 10));
  assert.equal(sockets.length, 2);
  assert.equal(decodeClientControl(sockets[0].sent[0]).type, "AUTH_PROVE");
  assert.equal(decodeClientControl(sockets[1].sent[0]).type, "PAIR_PROVE");
  assert.equal(store.value.clientId, "77".repeat(16));
  client.close();
});

test("a persisted repair that closes before ready reconnects through AUTH", async () => {
  const store = new MemoryKeys();
  const seed = new BrowserSession({ url: "ws://127.0.0.1/browser", host: "127.0.0.1", origin: "https://preview.example", challenge, keyStore: store, socketFactory: () => new Socket(serverFor) });
  await seed.connect();
  await new Promise((resolve) => setTimeout(resolve, 10));
  seed.close();
  const timer = new VirtualTimer();
  const sockets = [];
  let client;
  client = new BrowserClient({
    url: "ws://127.0.0.1/browser", host: "127.0.0.1", origin: "https://preview.example", challenge, keyStore: store, timer, reconnectInitialDelayMs: 10,
    onStatus: (status) => { if (status === "syncing" && sockets.length === 2) client.session?.close(); },
    socketFactory: () => { const server = sockets.length === 0 ? ((_current, frame) => { if (frame.type === "AUTH_PROVE") _current.reply(encodeServerError({ code: "unauthorized", retryable: false })); if (frame.type === "PAIR_PROVE") _current.reply(encodePairResult(frame.id, { client_id: "88".repeat(16), capabilities: CAPABILITIES.observe })); }) : serverFor; const socket = new Socket(server); sockets.push(socket); return socket; },
  });
  await assert.rejects(client.connect(), (error) => error instanceof SessionError && error.code === "closed");
  assert.equal(sockets[1].sent.filter((wire) => decodeClientControl(wire).type === "PAIR_PROVE").length, 1);
  assert.deepEqual(timer.delays, [10]);
  timer.advance(10);
  await new Promise((resolve) => setTimeout(resolve, 10));
  assert.equal(sockets.length, 3);
  assert.equal(decodeClientControl(sockets[2].sent[0]).type, "AUTH_PROVE");
  assert.equal(sockets[2].sent.filter((wire) => decodeClientControl(wire).type === "PAIR_PROVE").length, 0);
  client.close();
});

test("closing while repair persistence is pending does not replay pairing", async () => {
  const seedStore = new MemoryKeys();
  const seed = new BrowserSession({ url: "ws://127.0.0.1/browser", host: "127.0.0.1", origin: "https://preview.example", challenge, keyStore: seedStore, socketFactory: () => new Socket(serverFor) });
  await seed.connect();
  await new Promise((resolve) => setTimeout(resolve, 10));
  seed.close();
  let releaseSave;
  let saveStarted;
  let persisted = seedStore.value;
  const store = { async load() { return persisted; }, async save(value) { saveStarted?.(); await new Promise((resolve) => { releaseSave = resolve; }); persisted = value; } };
  const sockets = [];
  let client;
  client = new BrowserClient({
    url: "ws://127.0.0.1/browser", host: "127.0.0.1", origin: "https://preview.example", challenge, keyStore: store,
    socketFactory: () => { const index = sockets.length; const server = index === 0 ? ((_current, frame) => { if (frame.type === "AUTH_PROVE") _current.reply(encodeServerError({ code: "unauthorized", retryable: false })); }) : index === 1 ? ((_current, frame) => { if (frame.type === "PAIR_PROVE") _current.reply(encodePairResult(frame.id, { client_id: "99".repeat(16), capabilities: CAPABILITIES.observe })); }) : ((_current, frame) => { if (frame.type === "AUTH_PROVE") _current.reply(encodeAuthResult(frame.id, { client_id: "99".repeat(16), capabilities: CAPABILITIES.observe })); if (frame.type === "STATE_GET") replySnapshot(_current, frame, 1n); }); const socket = new Socket(server); sockets.push(socket); return socket; },
  });
  saveStarted = () => client.close();
  const connecting = client.connect();
  await assert.rejects(connecting, (error) => error instanceof SessionError && error.code === "closed");
  assert.equal(sockets.length, 2);
  assert.equal(sockets[1].sent.filter((wire) => decodeClientControl(wire).type === "PAIR_PROVE").length, 1);
  releaseSave();
  await new Promise((resolve) => setTimeout(resolve, 10));
  await client.connect();
  await new Promise((resolve) => setTimeout(resolve, 10));
  assert.equal(sockets.length, 3);
  assert.equal(decodeClientControl(sockets[2].sent[0]).type, "AUTH_PROVE");
  client.close();
});

test("a persisted repair clears the challenge before a rejected manual reconnect", async () => {
  const seedStore = new MemoryKeys();
  const seed = new BrowserSession({ url: "ws://127.0.0.1/browser", host: "127.0.0.1", origin: "https://preview.example", challenge, keyStore: seedStore, socketFactory: () => new Socket(serverFor) });
  await seed.connect();
  await new Promise((resolve) => setTimeout(resolve, 10));
  seed.close();
  let client;
  let persisted;
  const store = {
    async load() { return persisted ?? seedStore.value; },
    async save(value) { persisted = value; client.close(); },
  };
  const sockets = [];
  client = new BrowserClient({
    url: "ws://127.0.0.1/browser", host: "127.0.0.1", origin: "https://preview.example", challenge, keyStore: store,
    socketFactory: () => {
      const index = sockets.length;
      const server = index === 0
        ? ((_current, frame) => { if (frame.type === "AUTH_PROVE") _current.reply(encodeServerError({ code: "unauthorized", retryable: false })); })
        : index === 1
          ? ((_current, frame) => { if (frame.type === "PAIR_PROVE") _current.reply(encodePairResult(frame.id, { client_id: "99".repeat(16), capabilities: CAPABILITIES.observe })); })
          : ((_current, frame) => {
            if (frame.type === "AUTH_PROVE") _current.reply(encodeServerError({ code: "unauthorized", retryable: false }));
            if (frame.type === "PAIR_PROVE") _current.reply(encodeServerError({ code: "unauthorized", retryable: false }));
          });
      const socket = new Socket(server);
      sockets.push(socket);
      return socket;
    },
  });
  await assert.rejects(client.connect(), (error) => error instanceof SessionError && error.code === "closed");
  await assert.rejects(client.connect(), (error) => error instanceof SessionError && error.code === "unauthorized");
  await new Promise((resolve) => setTimeout(resolve, 10));
  assert.equal(sockets.length, 3);
  assert.equal(decodeClientControl(sockets[2].sent[0]).type, "AUTH_PROVE");
  assert.equal(sockets[2].sent.filter((wire) => decodeClientControl(wire).type === "PAIR_PROVE").length, 0);
  client.close();
});

test("a pairing authorization error never replays the one-shot challenge", async () => {
  const sockets = [];
  const client = new BrowserClient({
    url: "ws://127.0.0.1/browser", host: "127.0.0.1", origin: "https://preview.example", challenge, keyStore: new MemoryKeys(),
    socketFactory: () => { const socket = new Socket((_current, frame) => { if (frame.type === "PAIR_PROVE") socket.reply(encodeServerError({ code: "unauthorized", retryable: false })); }); sockets.push(socket); return socket; },
  });
  await assert.rejects(client.connect(), (error) => error instanceof SessionError && error.code === "unauthorized");
  await new Promise((resolve) => setTimeout(resolve, 10));
  assert.equal(sockets.length, 1);
  assert.equal(decodeClientControl(sockets[0].sent[0]).type, "PAIR_PROVE");
  client.close();
});

test("closing from stale authorization error fences the pending repair", async () => {
  const store = new MemoryKeys();
  const seed = new BrowserSession({ url: "ws://127.0.0.1/browser", host: "127.0.0.1", origin: "https://preview.example", challenge, keyStore: store, socketFactory: () => new Socket(serverFor) });
  await seed.connect();
  await new Promise((resolve) => setTimeout(resolve, 10));
  seed.close();
  const sockets = [];
  let client;
  client = new BrowserClient({
    url: "ws://127.0.0.1/browser", host: "127.0.0.1", origin: "https://preview.example", challenge, keyStore: store,
    onError: () => client.close(),
    socketFactory: () => { const server = sockets.length === 0 ? ((_current, frame) => { if (frame.type === "AUTH_PROVE") _current.reply(encodeServerError({ code: "unauthorized", retryable: false })); }) : serverFor; const socket = new Socket(server); sockets.push(socket); return socket; },
  });
  await assert.rejects(client.connect(), (error) => error instanceof SessionError && error.code === "unauthorized");
  await new Promise((resolve) => setTimeout(resolve, 10));
  assert.equal(sockets.length, 1);
  assert.equal(sockets[0].sent.filter((wire) => ["PAIR_PROVE", "STATE_GET"].includes(decodeClientControl(wire).type)).length, 0);
});

test("a newer generation started by the auth error fences the old repair", async () => {
  const store = new MemoryKeys();
  const seed = new BrowserSession({ url: "ws://127.0.0.1/browser", host: "127.0.0.1", origin: "https://preview.example", challenge, keyStore: store, socketFactory: () => new Socket(serverFor) });
  await seed.connect();
  await new Promise((resolve) => setTimeout(resolve, 10));
  seed.close();
  const sockets = [];
  let client;
  let replaced = false;
  client = new BrowserClient({
    url: "ws://127.0.0.1/browser", host: "127.0.0.1", origin: "https://preview.example", challenge, keyStore: store,
    onError: () => {
      if (!replaced) { replaced = true; void client.connect().catch(() => {}); }
    },
    socketFactory: () => { const server = sockets.length === 0 ? ((_current, frame) => { if (frame.type === "AUTH_PROVE") _current.reply(encodeServerError({ code: "unauthorized", retryable: false })); }) : serverFor; const socket = new Socket(server); sockets.push(socket); return socket; },
  });
  await assert.rejects(client.connect(), (error) => error instanceof SessionError && error.code === "unauthorized");
  await new Promise((resolve) => setTimeout(resolve, 10));
  assert.equal(sockets.length, 2);
  assert.equal(sockets.flatMap((socket) => socket.sent).filter((wire) => decodeClientControl(wire).type === "PAIR_PROVE").length, 0);
  client.close();
});

test("authorization repair requires a fresh challenge and only exact nonretryable unauthorized", async () => {
  for (const error of [
    { code: "unauthorized", retryable: true },
    { code: "unauthorized", retryable: false },
  ]) {
    const store = new MemoryKeys();
    const seed = new BrowserSession({ url: "ws://127.0.0.1/browser", host: "127.0.0.1", origin: "https://preview.example", challenge, keyStore: store, socketFactory: () => new Socket(serverFor) });
    await seed.connect();
    await new Promise((resolve) => setTimeout(resolve, 10));
    seed.close();
    let sockets = 0;
    const client = new BrowserClient({ url: "ws://127.0.0.1/browser", host: "127.0.0.1", origin: "https://preview.example", keyStore: store, challenge: error.retryable ? challenge : undefined, socketFactory: () => { sockets += 1; return new Socket((_current, frame) => { if (frame.type === "AUTH_PROVE") _current.reply(encodeServerError(error)); }); } });
    await assert.rejects(client.connect(), (actual) => actual instanceof SessionError && actual.code === "unauthorized");
    await new Promise((resolve) => setTimeout(resolve, 10));
    assert.equal(sockets, 1);
    client.close();
  }
});

test("valid saved authorization with a fresh challenge does not duplicate the client", async () => {
  const store = new MemoryKeys();
  const seed = new BrowserSession({ url: "ws://127.0.0.1/browser", host: "127.0.0.1", origin: "https://preview.example", challenge, keyStore: store, socketFactory: () => new Socket(serverFor) });
  await seed.connect();
  await new Promise((resolve) => setTimeout(resolve, 10));
  seed.close();
  let sockets = 0;
  const client = new BrowserClient({ url: "ws://127.0.0.1/browser", host: "127.0.0.1", origin: "https://preview.example", challenge, keyStore: store, socketFactory: () => { sockets += 1; return new Socket(serverFor); } });
  await client.connect();
  await new Promise((resolve) => setTimeout(resolve, 10));
  assert.equal(sockets, 1);
  client.close();
});

test("pairing challenge helper consumes the exact factoryctl fragment and clears it first", () => {
  const location = { hash: `#df_pair=${challenge}`, pathname: "/", search: "?preview=1" };
  const state = { route: "factory" };
  let replacedState;
  let replacedURL;
  const value = consumePairingChallenge(location, {
    state,
    replaceState: (nextState, _title, url) => { replacedState = nextState; replacedURL = url; },
  });
  assert.equal(value, challenge);
  assert.equal(replacedState, state);
  assert.equal(replacedURL, "/?preview=1");
});

test("pairing challenge helper scrubs and refuses every noncanonical pairing fragment", () => {
  const rejected = [
    "#df_pair=",
    `#df_pair=${"11".repeat(31)}`,
    `#df_pair=${challenge}11`,
    `#df_pair=${"gg".repeat(32)}`,
    `#df_pair=${"AA".repeat(32)}`,
    `#df_pair=${"00".repeat(32)}`,
    `#df_pair=${challenge}&df_pair=${challenge}`,
    `#df_pair=${challenge}&mode=pair`,
    `#mode=pair&df_pair=${challenge}`,
    `#df_pair=${challenge.slice(0, 62)}%31%31`,
    `#DF_PAIR=${challenge}`,
    `#%64f_pair=${challenge}`,
    `#df_pair%3D${challenge}`,
    `##df_pair=${challenge}`,
    `df_pair=${challenge}`,
    `#?df_pair=${challenge}`,
    `#building?df_pair=${challenge}`,
    `#building%3Fdf_pair%3D${challenge}`,
    `#challenge=${challenge}`,
  ];
  for (const hash of rejected) {
    let replaced;
    const value = consumePairingChallenge(
      { hash, pathname: "/factory", search: "?preview=1" },
      { state: null, replaceState: (_state, _title, url) => { replaced = url; } },
    );
    assert.equal(value, null, hash);
    assert.equal(replaced, "/factory?preview=1", hash);
  }
});

test("pairing challenge helper never returns authority if fragment scrubbing fails", () => {
  const scrubFailure = new Error("replaceState failed");
  assert.throws(
    () => consumePairingChallenge(
      { hash: `#df_pair=${challenge}`, pathname: "/", search: "" },
      { state: null, replaceState: () => { throw scrubFailure; } },
    ),
    scrubFailure,
  );
});

test("ordinary anchors are neither pairing authority nor scrubbed", () => {
  for (const hash of ["", "#building", "#request-123", "#mode=observe"]) {
    let replaced = false;
    const value = consumePairingChallenge(
      { hash, pathname: "/", search: "" },
      { state: null, replaceState: () => { replaced = true; } },
    );
    assert.equal(value, null, hash);
    assert.equal(replaced, false, hash);
  }
});

test("consumer callback exceptions cannot escape cleanup or stop a later state lifecycle", async () => {
  let malformedSocket;
  const malformed = new BrowserSession({ url: "ws://127.0.0.1/browser", host: "127.0.0.1", origin: "https://preview.example", keyStore: new MemoryKeys(), socketFactory: () => { malformedSocket = new Socket(() => {}); return malformedSocket; }, onError: () => { throw new Error("onError"); }, onStatus: () => { throw new Error("onStatus"); } });
  const rejected = malformed.connect();
  malformedSocket.onmessage({ data: new Uint8Array([0xff]) });
  await assert.rejects(rejected, (error) => error instanceof ProtocolError && error.code === "malformed");
  assert.equal(malformed.status, "closed");

  const states = [];
  const session = new BrowserSession({ url: "ws://127.0.0.1/browser", host: "127.0.0.1", origin: "https://preview.example", challenge, keyStore: new MemoryKeys(), socketFactory: () => new Socket(serverFor), onState: (state) => { states.push(state); throw new Error("onState"); }, onStatus: () => { throw new Error("onStatus"); } });
  await session.connect();
  await new Promise((resolve) => setTimeout(resolve, 10));
  assert.equal(session.status, "ready");
  assert.ok(states.length > 0);
  session.close();
});

test("unavailable daemon reconnects use bounded exponential virtual time", async () => {
  const pairingStore = new MemoryKeys();
  const pairing = new BrowserSession({ url: "ws://127.0.0.1/browser", host: "127.0.0.1", origin: "https://preview.example", challenge, keyStore: pairingStore, socketFactory: () => new Socket(serverFor) });
  await pairing.connect();
  await new Promise((resolve) => setTimeout(resolve, 10));
  pairing.close();
  const timer = new VirtualTimer();
  let attempts = 0;
  const unavailable = new BrowserClient({ url: "ws://127.0.0.1/browser", host: "127.0.0.1", origin: "https://preview.example", keyStore: pairingStore, timer, reconnectInitialDelayMs: 10, reconnectMaxDelayMs: 20, socketFactory: () => { attempts += 1; throw new Error("no daemon"); } });
  await assert.rejects(unavailable.connect(), (error) => error instanceof SessionError && error.code === "connection");
  assert.equal(attempts, 1);
  timer.advance(9);
  assert.equal(attempts, 1);
  timer.advance(1);
  assert.equal(attempts, 2);
  timer.advance(20);
  assert.equal(attempts, 3);
  timer.advance(20);
  assert.equal(attempts, 4);
  unavailable.close();
  assert.equal(timer.tasks.size, 0);
});

test("close clears a pending reconnect timer and manual connect can schedule a new lifecycle", async () => {
  const pairingStore = new MemoryKeys();
  const pairing = new BrowserSession({ url: "ws://127.0.0.1/browser", host: "127.0.0.1", origin: "https://preview.example", challenge, keyStore: pairingStore, socketFactory: () => new Socket(serverFor) });
  await pairing.connect();
  await new Promise((resolve) => setTimeout(resolve, 10));
  pairing.close();
  const timer = new VirtualTimer();
  let available = false;
  let attempts = 0;
  const client = new BrowserClient({ url: "ws://127.0.0.1/browser", host: "127.0.0.1", origin: "https://preview.example", keyStore: pairingStore, timer, reconnectInitialDelayMs: 10, socketFactory: () => { attempts += 1; if (!available) throw new Error("no daemon"); return new Socket(serverFor); } });
  await assert.rejects(client.connect());
  assert.equal(timer.tasks.size, 1);
  client.close();
  timer.advance(100);
  assert.equal(attempts, 1);
  available = true;
  await client.connect();
  await new Promise((resolve) => setTimeout(resolve, 10));
  assert.equal(client.status, "ready");
  assert.equal(attempts, 2);
  client.close();
});

test("close fences deferred load, sign, and pairing persistence completions", async () => {
  let releaseLoad;
  let loadStarted;
  const loadStore = { async load() { loadStarted?.(); await new Promise((resolve) => { releaseLoad = resolve; }); return null; }, async save() {} };
  let loadSocket;
  const loading = new BrowserSession({ url: "ws://127.0.0.1/browser", host: "127.0.0.1", origin: "https://preview.example", challenge, keyStore: loadStore, socketFactory: () => { loadSocket = new Socket(() => {}); return loadSocket; } });
  const loadReady = new Promise((resolve) => { loadStarted = resolve; });
  const loadingConnect = loading.connect();
  await loadReady;
  loading.close();
  releaseLoad();
  await assert.rejects(loadingConnect, (error) => error instanceof SessionError && error.code === "closed");
  assert.equal(loadSocket.sent.length, 0);

  const sourceStore = new MemoryKeys();
  const source = new BrowserSession({ url: "ws://127.0.0.1/browser", host: "127.0.0.1", origin: "https://preview.example", challenge, keyStore: sourceStore, socketFactory: () => new Socket(serverFor) });
  await source.connect();
  await new Promise((resolve) => setTimeout(resolve, 10));
  source.close();
  let releaseSign;
  let signStarted;
  const realCrypto = globalThis.crypto;
  const signingCrypto = { ...realCrypto, subtle: { ...realCrypto.subtle, sign: async (...args) => { signStarted?.(); await new Promise((resolve) => { releaseSign = resolve; }); return realCrypto.subtle.sign(...args); } } };
  let signSocket;
  const signing = new BrowserSession({ url: "ws://127.0.0.1/browser", host: "127.0.0.1", origin: "https://preview.example", keyStore: sourceStore, crypto: signingCrypto, socketFactory: () => { signSocket = new Socket(() => {}); return signSocket; } });
  const signReady = new Promise((resolve) => { signStarted = resolve; });
  const signingConnect = signing.connect();
  await signReady;
  signing.close();
  releaseSign();
  await assert.rejects(signingConnect, (error) => error instanceof SessionError && error.code === "closed");
  assert.equal(signSocket.sent.length, 0);

  let releaseSave;
  let saveStarted;
  const saveStore = { async load() { return null; }, async save() { saveStarted?.(); await new Promise((resolve) => { releaseSave = resolve; }); } };
  let saveSocket;
  const saving = new BrowserSession({ url: "ws://127.0.0.1/browser", host: "127.0.0.1", origin: "https://preview.example", challenge, keyStore: saveStore, socketFactory: () => { saveSocket = new Socket(serverFor); return saveSocket; } });
  const saveReady = new Promise((resolve) => { saveStarted = resolve; });
  const savingConnect = saving.connect();
  await saveReady;
  saving.close();
  releaseSave();
  await assert.rejects(savingConnect, (error) => error instanceof SessionError && error.code === "closed");
  assert.equal(saveSocket.sent.filter((wire) => decodeClientControl(wire).type === "STATE_GET").length, 0);
});

test("reentrant connecting status close rejects before creating a socket", async () => {
  let session;
  let sessionSockets = 0;
  session = new BrowserSession({
    url: "ws://127.0.0.1/browser",
    host: "127.0.0.1",
    origin: "https://preview.example",
    keyStore: new MemoryKeys(),
    onStatus: (status) => { if (status === "connecting") session.close(); },
    socketFactory: () => { sessionSockets += 1; return new Socket(() => {}); },
  });
  await assert.rejects(session.connect(), (error) => error instanceof SessionError && error.code === "closed");
  assert.equal(sessionSockets, 0);
  assert.equal(session.status, "closed");

  let factorySession;
  let factorySocket;
  factorySession = new BrowserSession({
    url: "ws://127.0.0.1/browser",
    host: "127.0.0.1",
    origin: "https://preview.example",
    keyStore: new MemoryKeys(),
    socketFactory: () => {
      factorySocket = new Socket(() => {});
      factorySession.close();
      return factorySocket;
    },
  });
  await assert.rejects(factorySession.connect(), (error) => error instanceof SessionError && error.code === "closed");
  assert.equal(factorySocket.readyState, 3);

  let client;
  let clientSockets = 0;
  client = new BrowserClient({
    url: "ws://127.0.0.1/browser",
    host: "127.0.0.1",
    origin: "https://preview.example",
    keyStore: new MemoryKeys(),
    onStatus: (status) => { if (status === "connecting") client.close(); },
    socketFactory: () => { clientSockets += 1; return new Socket(() => {}); },
  });
  await assert.rejects(client.connect(), (error) => error instanceof SessionError && error.code === "closed");
  assert.equal(clientSockets, 0);
  assert.equal(client.status, "closed");
});

test("BrowserClient does not replay a one-shot proof while PairResult save is deferred", async () => {
  let releaseSave;
  let saveStarted;
  let saved;
  let persisted = null;
  const store = {
    async load() { return persisted; },
    async save(value) {
      saved = value;
      saveStarted?.();
      await new Promise((resolve) => { releaseSave = resolve; });
      persisted = value;
    },
  };
  const timer = new VirtualTimer();
  const sockets = [];
  const client = new BrowserClient({
    url: "ws://127.0.0.1/browser",
    host: "127.0.0.1",
    origin: "https://preview.example",
    challenge,
    keyStore: store,
    timer,
    socketFactory: () => { const socket = new Socket(serverFor); sockets.push(socket); return socket; },
  });
  const saveReady = new Promise((resolve) => { saveStarted = resolve; });
  const first = client.connect();
  await saveReady;
  assert.equal(sockets[0].sent.filter((wire) => decodeClientControl(wire).type === "PAIR_PROVE").length, 1);
  const second = client.connect();
  assert.strictEqual(second, first);
  client.close();
  const duringSave = client.connect();
  assert.strictEqual(duringSave, first);
  assert.equal(sockets.length, 1);
  assert.equal(sockets[0].sent.filter((wire) => decodeClientControl(wire).type === "PAIR_PROVE").length, 1);
  releaseSave();
  await assert.rejects(first, (error) => error instanceof SessionError && error.code === "closed");
  await new Promise((resolve) => setTimeout(resolve, 0));
  assert.ok(saved);
  assert.ok(persisted);
  assert.equal(client.status, "closed");
  assert.equal(sockets[0].sent.filter((wire) => decodeClientControl(wire).type === "STATE_GET").length, 0);
  await client.connect();
  await new Promise((resolve) => setTimeout(resolve, 10));
  assert.equal(sockets.length, 2);
  assert.equal(decodeClientControl(sockets[1].sent[0]).type, "AUTH_PROVE");
  assert.equal(sockets[1].sent.filter((wire) => decodeClientControl(wire).type === "PAIR_PROVE").length, 0);
  client.close();
});

class HostileTimer {
  callbacks = [];
  clearCalls = [];
  setTimeout(callback, delay) {
    this.callbacks.push({ callback, delay });
    return undefined;
  }
  clearTimeout(handle) {
    this.clearCalls.push(handle);
    throw new Error("timer cleanup failed");
  }
}

test("reconnect timer ownership fences stale callbacks with undefined handles", async () => {
  const store = new MemoryKeys();
  const timer = new HostileTimer();
  let attempts = 0;
  const client = new BrowserClient({
    url: "ws://127.0.0.1/browser",
    host: "127.0.0.1",
    origin: "https://preview.example",
    keyStore: store,
    timer,
    reconnectInitialDelayMs: 10,
    reconnectMaxDelayMs: 20,
    socketFactory: () => { attempts += 1; throw new Error("no daemon"); },
  });
  await assert.rejects(client.connect(), (error) => error instanceof SessionError && error.code === "connection");
  assert.equal(attempts, 1);
  assert.equal(timer.callbacks.length, 1);

  await assert.rejects(client.connect(), (error) => error instanceof SessionError && error.code === "connection");
  assert.equal(attempts, 2);
  assert.equal(timer.callbacks.length, 2);
  timer.callbacks[0].callback();
  assert.equal(attempts, 2, "stale callback must not consume the current schedule");
  timer.callbacks[1].callback();
  assert.equal(attempts, 3);
  assert.equal(timer.callbacks.length, 3);

  client.close();
  assert.ok(timer.clearCalls.length >= 2);
  assert.ok(timer.clearCalls.every((handle) => handle === undefined));
  timer.callbacks[2].callback();
  assert.equal(attempts, 3, "callback after close must remain fenced");
});

test("agent terminal discovery mints an opaque generation-bound target for openTerminal", async () => {
  const store = new MemoryKeys();
  const agentId = "77".repeat(16);
  const terminalSessionId = "88".repeat(16);
  let socket;
  const targetServer = (current) => {
    serverFor(current);
    const frame = decodeClientControl(current.sent.at(-1));
    if (frame.type === "TERMINAL_TARGET_GET") current.reply(encodeTerminalTarget(frame.id, {
      agent_id: frame.body.agent_id,
      agent_revision: frame.body.expected_agent_revision,
      head: frame.body.expected_head,
      target: { run_id: runID, session_id: terminalSessionId, run_revision: 3n, session_revision: 4n },
    }));
  };
  const session = new BrowserSession({
    url: "ws://127.0.0.1:43123/browser", host: "127.0.0.1:43123", origin: "https://preview.example",
    challenge, keyStore: store, socketFactory: () => { socket = new Socket(targetServer); return socket; },
  });
  await session.connect();
  await new Promise((resolve) => setTimeout(resolve, 10));
  const pending = session.resolveAgentTerminal({ agentId, expectedAgentRevision: 9n, expectedHead: 12n });
  const targetGet = decodeClientControl(socket.sent.at(-1));
  assert.equal(targetGet.type, "TERMINAL_TARGET_GET");
  const target = await pending;
  assert.equal(Object.isFrozen(target), true);
  assert.equal("runId" in target, false);
  assert.throws(() => session.openTerminal({ ...target }), (error) => error instanceof SessionError && error.code === "stale");
  const terminal = session.openTerminal(target);
  const other = await openHumanSession();
  assert.throws(() => other.session.openTerminal(target), (error) => error instanceof SessionError && error.code === "stale");
  other.session.close();
  const attach = terminal.attach();
  const attachFrame = decodeClientControl(socket.sent.at(-1));
  assert.deepEqual(attachFrame.body, { run_id: runID, session_id: terminalSessionId, expected_run_revision: 3n, expected_session_revision: 4n, after_sequence: 0n });
  socket.reply(encodeTerminalAttached(attachFrame.id, { session_id: terminalSessionId, floor: 0n, head: 0n, acknowledged_sequence: 0n, max_unacked_bytes: 65536n }));
  await attach;
  const pendingDetach = terminal.detach();
  socket.close();
  await assert.rejects(pendingDetach, (error) => error instanceof SessionError && error.code === "connection");
  assert.equal(session.status, "closed");
  session.close();
  assert.throws(() => session.openTerminal(target), (error) => error instanceof SessionError && error.code === "closed");
});

test("a newer public head makes an overtaken terminal target response stale", async () => {
  const { session, socket } = await openControlledStateSession();
  const agentId = "77".repeat(16);
  const terminalSessionId = "88".repeat(16);
  const pending = session.resolveAgentTerminal({
    agentId,
    expectedAgentRevision: 1n,
    expectedHead: 1n,
  });
  const targetGet = lastFrame(socket, "TERMINAL_TARGET_GET");
  const watch = lastFrame(socket, "STATE_WATCH");

  socket.reply(encodeStateChanged(watch.id, { head: 2n }));
  await tick();
  await completePendingSnapshot(socket, 2n);
  assert.equal(session.state.head, 2n);

  const rejected = assert.rejects(
    pending,
    (error) => error instanceof SessionError && error.code === "stale",
  );
  socket.reply(
    encodeTerminalTarget(targetGet.id, {
      agent_id: agentId,
      agent_revision: 1n,
      head: 1n,
      target: {
        run_id: runID,
        session_id: terminalSessionId,
        run_revision: 3n,
        session_revision: 4n,
      },
    }),
  );
  await rejected;
  assert.equal(session.status, "ready");
  session.close();
});

test("a refresh preserves pending discovery but rejects its old-head target", async () => {
  const { session, socket } = await openControlledStateSession();
  const agentId = "78".repeat(16);
  const pending = session.resolveAgentTerminal({
    agentId,
    expectedAgentRevision: 1n,
    expectedHead: 1n,
  });
  const targetGet = lastFrame(socket, "TERMINAL_TARGET_GET");
  const watch = lastFrame(socket, "STATE_WATCH");

  socket.reply(encodeStateChanged(watch.id, { head: 2n }));
  await tick();
  await completePendingSnapshot(socket, 2n);
  assert.equal(session.state.head, 2n);

  const rejected = assert.rejects(
    pending,
    (error) => error instanceof SessionError && error.code === "stale",
  );
  socket.reply(
    encodeTerminalTarget(targetGet.id, {
      agent_id: agentId,
      agent_revision: 1n,
      head: 1n,
      target: null,
    }),
  );
  await rejected;
  assert.equal(session.status, "ready");
  session.close();
});

test("terminal target discovery is bounded, exact-correlated, and null is explicit", async () => {
  const store = new MemoryKeys();
  const agentId = "79".repeat(16);
  let socket;
  const session = new BrowserSession({
    url: "ws://127.0.0.1:43123/browser", host: "127.0.0.1:43123", origin: "https://preview.example",
    challenge, keyStore: store, socketFactory: () => { socket = new Socket(serverFor); return socket; },
  });
  await session.connect();
  await new Promise((resolve) => setTimeout(resolve, 10));
  const pending = Array.from({ length: 32 }, () => session.resolveAgentTerminal({ agentId, expectedAgentRevision: 1n, expectedHead: 1n }));
  await assert.rejects(session.resolveAgentTerminal({ agentId, expectedAgentRevision: 1n, expectedHead: 1n }), (error) => error instanceof SessionError && error.code === "rate_limited");
  const first = decodeClientControl(socket.sent.find((wire) => decodeClientControl(wire).type === "TERMINAL_TARGET_GET"));
  socket.reply(encodeTerminalTarget(first.id, { agent_id: agentId, agent_revision: 1n, head: 1n, target: null }));
  assert.equal(await pending[0], null);
  const next = session.resolveAgentTerminal({ agentId, expectedAgentRevision: 1n, expectedHead: 1n });
  session.close();
  await assert.rejects(next, (error) => error instanceof SessionError && error.code === "closed");
  await Promise.allSettled(pending.slice(1));

  let malformedSocket;
  const malformed = new BrowserSession({
    url: "ws://127.0.0.1:43123/browser", host: "127.0.0.1:43123", origin: "https://preview.example",
    challenge, keyStore: new MemoryKeys(), socketFactory: () => { malformedSocket = new Socket(serverFor); return malformedSocket; },
  });
  await malformed.connect();
  await new Promise((resolve) => setTimeout(resolve, 10));
  const bad = malformed.resolveAgentTerminal({ agentId, expectedAgentRevision: 2n, expectedHead: 4n });
  const badFrame = decodeClientControl(malformedSocket.sent.at(-1));
  malformedSocket.reply(encodeTerminalTarget(badFrame.id, { agent_id: agentId, agent_revision: 3n, head: 4n, target: null }));
  await assert.rejects(bad, (error) => error instanceof ProtocolError && error.code === "malformed");
  assert.equal(malformed.status, "closed");
});

test("EXIT closes the old handle and routes its duplicate away from a replacement", async () => {
  const agentId = "81".repeat(16);
  const terminalSessionId = "82".repeat(16);
  let socket;
  const targetServer = (current) => {
    serverFor(current);
    const frame = decodeClientControl(current.sent.at(-1));
    if (frame.type === "TERMINAL_TARGET_GET") current.reply(encodeTerminalTarget(frame.id, {
      agent_id: frame.body.agent_id,
      agent_revision: frame.body.expected_agent_revision,
      head: frame.body.expected_head,
      target: { run_id: runID, session_id: terminalSessionId, run_revision: 3n, session_revision: 4n },
    }));
  };
  const session = new BrowserSession({
    url: "ws://127.0.0.1:43123/browser", host: "127.0.0.1:43123", origin: "https://preview.example",
    challenge, keyStore: new MemoryKeys(), socketFactory: () => { socket = new Socket(targetServer); return socket; },
  });
  await session.connect();
  await new Promise((resolve) => setTimeout(resolve, 10));
  const targetPending = session.resolveAgentTerminal({ agentId, expectedAgentRevision: 1n, expectedHead: 1n });
  const target = await targetPending;
  const terminal = session.openTerminal(target);
  const attach = terminal.attach();
  const attachFrame = decodeClientControl(socket.sent.at(-1));
  socket.reply(encodeTerminalAttached(attachFrame.id, { session_id: terminalSessionId, floor: 0n, head: 0n, acknowledged_sequence: 0n, max_unacked_bytes: 65536n }));
  await attach;
  const exit = { session_id: terminalSessionId, exit_code: 0, exit_signal: 0, aborted: false };
  socket.reply(encodeTerminalExit(attachFrame.id, exit));
  await new Promise((resolve) => setTimeout(resolve, 10));
  assert.equal(terminal.closed, true);
  const before = socket.sent.length;
  for (const effect of [
    () => terminal.attach(),
    () => terminal.acquireInput(),
    () => terminal.releaseInput(),
    () => terminal.sendInput(new Uint8Array([1])),
    () => terminal.resize(24, 80),
    () => terminal.detach(),
  ]) assert.throws(effect, /closed/);
  assert.equal(socket.sent.length, before);

  const replacement = session.openTerminal(target);
  const replacementAttach = replacement.attach();
  const replacementFrame = decodeClientControl(socket.sent.at(-1));
  assert.equal(replacementFrame.body.after_sequence, 0n);
  socket.reply(encodeTerminalAttached(replacementFrame.id, { session_id: terminalSessionId, floor: 0n, head: 0n, acknowledged_sequence: 0n, max_unacked_bytes: 65536n }));
  await replacementAttach;
  socket.reply(encodeTerminalExit(attachFrame.id, exit));
  await new Promise((resolve) => setTimeout(resolve, 10));
  assert.equal(replacement.closed, false);
  session.close();
});

test("RESET closes the old handle and a replacement may select a fresh after-sequence", async () => {
  const agentId = "83".repeat(16);
  const terminalSessionId = "84".repeat(16);
  let socket;
  const targetServer = (current) => {
    serverFor(current);
    const frame = decodeClientControl(current.sent.at(-1));
    if (frame.type === "TERMINAL_TARGET_GET") current.reply(encodeTerminalTarget(frame.id, {
      agent_id: frame.body.agent_id,
      agent_revision: frame.body.expected_agent_revision,
      head: frame.body.expected_head,
      target: { run_id: runID, session_id: terminalSessionId, run_revision: 3n, session_revision: 4n },
    }));
  };
  const session = new BrowserSession({
    url: "ws://127.0.0.1:43123/browser", host: "127.0.0.1:43123", origin: "https://preview.example",
    challenge, keyStore: new MemoryKeys(), socketFactory: () => { socket = new Socket(targetServer); return socket; },
  });
  await session.connect();
  await new Promise((resolve) => setTimeout(resolve, 10));
  const targetPending = session.resolveAgentTerminal({ agentId, expectedAgentRevision: 1n, expectedHead: 1n });
  const target = await targetPending;
  const terminal = session.openTerminal(target);
  const attach = terminal.attach();
  const attachFrame = decodeClientControl(socket.sent.at(-1));
  socket.reply(encodeTerminalAttached(attachFrame.id, { session_id: terminalSessionId, floor: 0n, head: 8n, acknowledged_sequence: 0n, max_unacked_bytes: 65536n }));
  await attach;
  const reset = { session_id: terminalSessionId, floor: 4n, head: 8n };
  socket.reply(encodeTerminalReset(attachFrame.id, reset));
  await new Promise((resolve) => setTimeout(resolve, 10));
  assert.equal(terminal.closed, true);
  const before = socket.sent.length;
  for (const effect of [
    () => terminal.attach(),
    () => terminal.acquireInput(),
    () => terminal.releaseInput(),
    () => terminal.sendInput(new Uint8Array([1])),
    () => terminal.resize(24, 80),
    () => terminal.detach(),
  ]) assert.throws(effect, /closed/);
  assert.equal(socket.sent.length, before);

  const replacement = session.openTerminal(target, { afterSequence: 6n });
  const replacementAttach = replacement.attach();
  const replacementFrame = decodeClientControl(socket.sent.at(-1));
  assert.equal(replacementFrame.body.after_sequence, 6n);
  socket.reply(encodeTerminalAttached(replacementFrame.id, { session_id: terminalSessionId, floor: 4n, head: 8n, acknowledged_sequence: 6n, max_unacked_bytes: 65536n }));
  await replacementAttach;
  socket.reply(encodeTerminalReset(attachFrame.id, reset));
  await new Promise((resolve) => setTimeout(resolve, 10));
  assert.equal(replacement.closed, false);
  session.close();
});

test("terminal replacement retains only the previous closed handle and fences evicted evidence", async () => {
  const agentId = "85".repeat(16);
  const terminalSessionId = "86".repeat(16);
  const errors = [];
  let socket;
  const targetServer = (current) => {
    serverFor(current);
    const frame = decodeClientControl(current.sent.at(-1));
    if (frame.type === "TERMINAL_TARGET_GET") current.reply(encodeTerminalTarget(frame.id, {
      agent_id: frame.body.agent_id,
      agent_revision: frame.body.expected_agent_revision,
      head: frame.body.expected_head,
      target: { run_id: runID, session_id: terminalSessionId, run_revision: 3n, session_revision: 4n },
    }));
  };
  const session = new BrowserSession({
    url: "ws://127.0.0.1:43123/browser", host: "127.0.0.1:43123", origin: "https://preview.example",
    challenge, keyStore: new MemoryKeys(), socketFactory: () => { socket = new Socket(targetServer); return socket; },
    onError: (error) => errors.push(error),
  });
  await session.connect();
  await new Promise((resolve) => setTimeout(resolve, 10));
  const target = await session.resolveAgentTerminal({ agentId, expectedAgentRevision: 1n, expectedHead: 1n });
  const attachFrames = [];
  let live;
  for (let cycle = 0; cycle < 64; cycle += 1) {
    live = session.openTerminal(target, { afterSequence: BigInt(cycle) });
    const attach = live.attach();
    const attachFrame = decodeClientControl(socket.sent.at(-1));
    attachFrames.push(attachFrame);
    socket.reply(encodeTerminalAttached(attachFrame.id, {
      session_id: terminalSessionId,
      floor: 0n,
      head: BigInt(cycle),
      acknowledged_sequence: BigInt(cycle),
      max_unacked_bytes: 65536n,
    }));
    await attach;
    socket.reply(encodeTerminalExit(attachFrame.id, { session_id: terminalSessionId, exit_code: 0, exit_signal: 0, aborted: false }));
    await new Promise((resolve) => setTimeout(resolve, 0));
    assert.equal(live.closed, true);
    if (cycle > 0) {
      socket.reply(encodeTerminalExit(attachFrames[cycle - 1].id, { session_id: terminalSessionId, exit_code: 0, exit_signal: 0, aborted: false }));
      await new Promise((resolve) => setTimeout(resolve, 0));
      assert.equal(live.closed, true, "immediately previous retired evidence cannot affect the current closed handle");
    }
  }

  const replacement = session.openTerminal(target);
  const replacementAttach = replacement.attach();
  const replacementFrame = decodeClientControl(socket.sent.at(-1));
  socket.reply(encodeTerminalAttached(replacementFrame.id, {
    session_id: terminalSessionId, floor: 0n, head: 0n, acknowledged_sequence: 0n, max_unacked_bytes: 65536n,
  }));
  await replacementAttach;
  socket.reply(encodeTerminalExit(attachFrames[0].id, { session_id: terminalSessionId, exit_code: 0, exit_signal: 0, aborted: false }));
  await new Promise((resolve) => setTimeout(resolve, 0));
  assert.equal(session.status, "closed", "evidence older than the immediately previous handle is a protocol violation");
  assert.equal(replacement.closed, true);
  assert.equal(errors.filter((error) => error instanceof ProtocolError && error.code === "malformed").length, 1);
  session.close();
});

// The browser contract lost its generation, and with it the version bytes in
// the pairing and auth transcript domains. A durable pairing must survive that
// unchanged: the key, not the transcript, is what persists. This pairs, then
// reconnects with only the stored record and proves the daemon can verify the
// new AUTH_PROVE signature against the same stored public key.
test("a pairing survives the versionless transcript domain", async () => {
  const store = new MemoryKeys();
  const pairing = new BrowserSession({ url: "ws://127.0.0.1/browser", host: "127.0.0.1", origin: "https://preview.example", challenge, keyStore: store, socketFactory: () => new Socket(serverFor) });
  await pairing.connect();
  await new Promise((resolve) => setTimeout(resolve, 10));
  pairing.close();
  const paired = store.value;
  assert.equal(paired.clientId, clientID);
  assert.equal(paired.key.extractable, false);

  let socket;
  const client = new BrowserClient({
    url: "ws://127.0.0.1/browser", host: "127.0.0.1", origin: "https://preview.example",
    keyStore: store, socketFactory: () => { socket = new Socket(serverFor); return socket; },
  });
  await client.connect();
  await new Promise((resolve) => setTimeout(resolve, 10));
  assert.equal(client.session.status, "ready");
  // The stored record is untouched by reconnecting: the same client id and the
  // same non-exportable key object.
  assert.equal(store.value.clientId, paired.clientId);
  assert.equal(store.value.key, paired.key);

  const prove = lastFrame(socket, "AUTH_PROVE");
  assert.equal(prove.body.client_id, clientID);
  const transcript = buildAuthTranscript({
    daemon_id: daemonID, boot_id: bootID, connection_nonce: nonce, client_id: clientID,
    host: "127.0.0.1", origin: "https://preview.example",
  });
  assert.equal(new TextDecoder().decode(transcript.slice(0, 25)), "dark-factory/browser/auth");
  assert.equal(await verifyP256Signature(paired.publicKeySEC1, hexBytes(prove.body.signature), transcript), true);
  client.close();
});
