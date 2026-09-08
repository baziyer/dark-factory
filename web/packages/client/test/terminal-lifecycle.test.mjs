import assert from "node:assert/strict";
import test from "node:test";
import { ProtocolError } from "../dist/src/errors.js";
import { SessionErrorLikeError, createTerminalHandle } from "../dist/src/terminal_session.js";
import {
  decodeClientControl,
  decodeServerControl,
  encodeTerminalAttached,
  encodeTerminalDetached,
  encodeTerminalInputResult,
  encodeTerminalLeaseResult,
  encodeTerminalResized,
} from "../dist/src/control.js";
import { decodeTerminalInput } from "../dist/src/terminal.js";

const runId = "11".repeat(16);
const sessionId = "22".repeat(16);
const responseIds = ["01", "02", "03", "04", "05", "06", "07", "08"].map((value) => value.repeat(16));

class VirtualTimer {
  now = 100_000;
  next = 0;
  tasks = new Map();
  delays = [];

  setTimeout(callback, delay) {
    const id = ++this.next;
    this.tasks.set(id, { at: this.now + delay, callback });
    this.delays.push(delay);
    return id;
  }

  clearTimeout(id) { this.tasks.delete(id); }

  advance(milliseconds) {
    this.now += milliseconds;
    while (true) {
      const due = [...this.tasks.entries()]
        .filter(([, task]) => task.at <= this.now)
        .sort((a, b) => a[1].at - b[1].at)[0];
      if (due === undefined) return;
      this.tasks.delete(due[0]);
      due[1].callback();
    }
  }
}

function makeHandle(overrides = {}) {
  const sent = [];
  const timer = overrides.timer ?? new VirtualTimer();
  let id = 0;
  const fatals = [];
  const handle = createTerminalHandle(
    { runId, sessionId, runRevision: 1n, sessionRevision: 1n },
    overrides.options ?? {},
    overrides.send ?? ((requestId, payload) => sent.push({ requestId, payload })),
    () => responseIds[id++] ?? "09".repeat(16),
    (error) => fatals.push(error),
    timer,
    overrides.inputAllowed ?? true,
  );
  return { handle, sent, timer, fatals };
}

function serverFrame(wire) { return decodeServerControl(wire); }
function lastControl(sent) { return decodeClientControl(sent.at(-1).payload); }
function replyAttached(value, frame) {
  return encodeTerminalAttached(frame.id, {
    session_id: sessionId,
    floor: 0n,
    head: 0n,
    acknowledged_sequence: 0n,
    max_unacked_bytes: 65536n,
    ...value,
  });
}

test("a replay cursor from session A does not cross to session B", async () => {
  const context = makeHandle({ options: { afterSequence: 9n, afterSessionId: "33".repeat(16) } });
  const attach = context.handle.attach();
  const frame = lastControl(context.sent);
  assert.equal(frame.body.after_sequence, 0n);
  context.handle.receive(serverFrame(replyAttached({}, frame)));
  await attach;

  const sameSession = makeHandle({ options: { afterSequence: 9n, afterSessionId: sessionId } });
  const resumed = sameSession.handle.attach();
  const resumedFrame = lastControl(sameSession.sent);
  assert.equal(resumedFrame.body.after_sequence, 9n);
  sameSession.handle.receive(serverFrame(replyAttached({ floor: 8n, head: 12n, acknowledged_sequence: 9n }, resumedFrame)));
  await resumed;
});

async function attachedWithLease(context, expiresAtMs = BigInt(context.timer.now + 30_000)) {
  const { handle, sent } = context;
  const attach = handle.attach();
  const attachFrame = lastControl(sent);
  handle.receive(serverFrame(replyAttached({}, attachFrame)));
  await attach;
  const acquire = handle.acquireInput();
  const acquireFrame = lastControl(sent);
  handle.receive(serverFrame(encodeTerminalLeaseResult(acquireFrame.id, {
    operation: "acquired",
    run_id: runId,
    session_id: sessionId,
    generation: 1n,
    expires_at_ms: expiresAtMs,
    last_input_sequence: 0n,
    run_revision: 1n,
    session_revision: 1n,
  })));
  await acquire;
  return { attachFrame, acquireFrame };
}

async function attachedOnly(context) {
  const attach = context.handle.attach();
  const attachFrame = lastControl(context.sent);
  context.handle.receive(serverFrame(replyAttached({}, attachFrame)));
  await attach;
  return { attachFrame };
}

function outputFrame(sequence, payload = [1]) {
  return { direction: "output", sessionId: new Uint8Array(16).fill(0x22), sequence, leaseGeneration: 0n, payload: new Uint8Array(payload) };
}

function inputResult(id, body) {
  return { v: 1, type: "TERMINAL_INPUT_RESULT", id, body };
}

function leaseResult(id, body) {
  return { v: 1, type: "TERMINAL_LEASE_RESULT", id, body };
}

test("acquire arms one ten-second timer and renewal replaces it with one timer", async () => {
  const context = makeHandle();
  await attachedWithLease(context);
  assert.equal(context.handle.writable, true);
  assert.equal(context.timer.tasks.size, 1);
  assert.deepEqual(context.timer.delays, [10_000]);
  context.timer.advance(10_000);
  const renewal = lastControl(context.sent);
  assert.equal(renewal.type, "TERMINAL_LEASE_RENEW");
  assert.equal(context.timer.tasks.size, 1);
  context.handle.receive(serverFrame(encodeTerminalLeaseResult(renewal.id, {
    operation: "renewed", run_id: runId, session_id: sessionId, generation: 1n,
    expires_at_ms: BigInt(context.timer.now + 30_000), last_input_sequence: 0n,
    run_revision: 1n, session_revision: 1n,
  })));
  assert.equal(context.timer.tasks.size, 1);
  assert.deepEqual(context.timer.delays, [10_000, 20_000, 10_000]);
});

test("a due timer waits for input, then renews once before another effect", async () => {
  const context = makeHandle();
  const { attachFrame } = await attachedWithLease(context);
  const input = context.handle.sendInput(new Uint8Array([1]));
  context.timer.advance(10_000);
  assert.equal(context.sent.filter(({ payload }) => typeof payload === "string" && decodeClientControl(payload).type === "TERMINAL_LEASE_RENEW").length, 0);
  context.handle.receive(serverFrame(encodeTerminalInputResult(attachFrame.id, {
    session_id: sessionId, generation: 1n, sequence: 1n, status: "accepted", accepted_bytes: 1n,
  })));
  await input;
  assert.equal(context.sent.filter(({ payload }) => typeof payload === "string" && decodeClientControl(payload).type === "TERMINAL_LEASE_RENEW").length, 1);
  const renewal = lastControl(context.sent);
  context.handle.receive(serverFrame(encodeTerminalLeaseResult(renewal.id, {
    operation: "renewed", run_id: runId, session_id: sessionId, generation: 1n,
    expires_at_ms: BigInt(context.timer.now + 30_000), last_input_sequence: 1n,
    run_revision: 1n, session_revision: 1n,
  })));
  const nextInput = context.handle.sendInput(new Uint8Array([2]));
  const nextInputWire = context.sent.filter(({ payload }) => payload instanceof Uint8Array).at(-1).payload;
  assert.equal(decodeTerminalInput(nextInputWire).sequence, 2n);
  context.handle.receive(serverFrame(encodeTerminalInputResult(attachFrame.id, {
    session_id: sessionId, generation: 1n, sequence: 2n, status: "accepted", accepted_bytes: 1n,
  })));
  await nextInput;
});

test("a due timer waits for resize, then renews before accepting another effect", async () => {
  const context = makeHandle();
  await attachedWithLease(context);
  const resize = context.handle.resize(24, 80);
  const resizeFrame = lastControl(context.sent);
  context.timer.advance(10_000);
  assert.equal(context.sent.filter(({ payload }) => typeof payload === "string" && decodeClientControl(payload).type === "TERMINAL_LEASE_RENEW").length, 0);
  context.handle.receive(serverFrame(encodeTerminalResized(resizeFrame.id, { session_id: sessionId, generation: 1n, rows: 24, cols: 80 })));
  await resize;
  const renewal = lastControl(context.sent);
  assert.equal(renewal.type, "TERMINAL_LEASE_RENEW");
  await assert.rejects(context.handle.resize(30, 100), /terminal operation pending/);
  context.handle.receive(serverFrame(encodeTerminalLeaseResult(renewal.id, {
    operation: "renewed", run_id: runId, session_id: sessionId, generation: 1n,
    expires_at_ms: BigInt(context.timer.now + 30_000), last_input_sequence: 0n,
    run_revision: 1n, session_revision: 1n,
  })));
  const nextResize = context.handle.resize(30, 100);
  const nextFrame = lastControl(context.sent);
  context.handle.receive(serverFrame(encodeTerminalResized(nextFrame.id, { session_id: sessionId, generation: 1n, rows: 30, cols: 100 })));
  await nextResize;
});

test("completed input settles before a due renewal send failure closes the generation", async () => {
  let context;
  const send = (requestId, payload) => {
    context.sent.push({ requestId, payload });
    if (typeof payload === "string" && decodeClientControl(payload).type === "TERMINAL_LEASE_RENEW") throw new Error("renewal transport failed");
  };
  context = makeHandle({ send });
  const { attachFrame } = await attachedWithLease(context);
  const input = context.handle.sendInput(new Uint8Array([7]));
  context.timer.advance(10_000);
  context.handle.receive(serverFrame(encodeTerminalInputResult(attachFrame.id, {
    session_id: sessionId, generation: 1n, sequence: 1n, status: "accepted", accepted_bytes: 1n,
  })));
  let deadline;
  try {
    const timeout = new Promise((_, reject) => { deadline = setTimeout(() => reject(new Error("completed input hung")), 50); });
    const result = await Promise.race([
      input,
      timeout,
    ]);
    assert.equal(result.status, "accepted");
  } finally {
    clearTimeout(deadline);
  }
  assert.equal(context.handle.closed, true);
  assert.equal(context.fatals.length, 1);
  assert.equal(context.sent.filter(({ payload }) => typeof payload === "string" && decodeClientControl(payload).type === "TERMINAL_LEASE_RENEW").length, 1);
});

test("rejected input preserves lease and sequence for deliberate retry", async () => {
  const context = makeHandle();
  const { attachFrame } = await attachedWithLease(context);
  const input = context.handle.sendInput(new Uint8Array([1]));
  context.handle.receive(serverFrame(encodeTerminalInputResult(attachFrame.id, {
    session_id: sessionId, generation: 1n, sequence: 1n, status: "rejected", accepted_bytes: 0n,
  })));
  await input;
  assert.equal(context.handle.writable, true);
  const retry = context.handle.sendInput(new Uint8Array([2]));
  assert.equal(context.sent.at(-1).payload instanceof Uint8Array, true);
  context.handle.receive(serverFrame(encodeTerminalInputResult(attachFrame.id, {
    session_id: sessionId, generation: 1n, sequence: 1n, status: "accepted", accepted_bytes: 1n,
  })));
  await retry;
});

test("TERMINAL_ATTACHED resolves the exact frozen snapshot", async () => {
  const context = makeHandle();
  const attaching = context.handle.attach();
  const attachFrame = lastControl(context.sent);
  context.handle.receive(serverFrame(replyAttached({ head: 4n }, attachFrame)));
  const attached = await attaching;
  assert.equal(Object.isFrozen(attached), true);
  assert.throws(() => { attached.head = 99n; }, TypeError);
  assert.equal(attached.head, 4n);
});

test("an input ERROR is ambiguous, fences the socket, and forbids replay", async () => {
  const context = makeHandle();
  const { attachFrame } = await attachedWithLease(context);
  const input = context.handle.sendInput(new Uint8Array([8, 9]));
  assert.equal(context.handle.receiveError(attachFrame.id, new Error("input delivery unknown")), true);
  await assert.rejects(input, /input delivery unknown/);
  assert.equal(context.handle.closed, true);
  assert.equal(context.handle.writable, false);
  assert.equal(context.sent.filter(({ payload }) => payload instanceof Uint8Array).length, 1);
  assert.throws(() => context.handle.sendInput(new Uint8Array([10])), /closed/);
});

test("exit rejects a pending input before clearing writable authority", async () => {
  let writableAtCallback;
  let terminal;
  const context = makeHandle({ options: { onExit: () => { writableAtCallback = terminal.writable; } } });
  terminal = context.handle;
  const { attachFrame } = await attachedWithLease(context);
  const input = terminal.sendInput(new Uint8Array([1]));
  assert.equal(terminal.receiveExit(attachFrame.id, { session_id: sessionId, exit_code: 0, exit_signal: 15, aborted: true }), true);
  await assert.rejects(input, /terminal exited/);
  assert.equal(writableAtCallback, false);
  assert.equal(terminal.writable, false);
});

test("exit rejects every pending control operation without sending another effect", async () => {
  for (const kind of ["acquire", "resize", "release", "detach"]) {
    const context = makeHandle();
    const { attachFrame } = kind === "acquire" ? await attachedOnly(context) : await attachedWithLease(context);
    const pending = kind === "acquire" ? context.handle.acquireInput() : kind === "resize" ? context.handle.resize(24, 80) : kind === "release" ? context.handle.releaseInput() : context.handle.detach();
    const before = context.sent.length;
    assert.equal(context.handle.receiveExit(attachFrame.id, { session_id: sessionId, exit_code: 1, exit_signal: 0, aborted: false }), true, kind);
    if (kind === "detach") await pending;
    else await assert.rejects(pending, /terminal exited/, kind);
    assert.equal(context.sent.length, before, `${kind} must not continue after exit`);
    assert.equal(context.handle.writable, false, kind);
  }
});

test("reset rejects a pending input, clears authority, and ignores its duplicate evidence", async () => {
  let resets = 0;
  const context = makeHandle({ options: { onReset: () => { resets += 1; assert.equal(context.handle.writable, false); } } });
  const { attachFrame } = await attachedWithLease(context);
  const input = context.handle.sendInput(new Uint8Array([1]));
  assert.equal(context.handle.receiveReset(attachFrame.id, { session_id: sessionId, floor: 4n, head: 5n }), true);
  await assert.rejects(input, /terminal reset/);
  assert.equal(resets, 1);
  assert.equal(context.handle.receiveReset(attachFrame.id, { session_id: sessionId, floor: 4n, head: 5n }), true);
  assert.equal(resets, 1);
  assert.equal(context.handle.writable, false);
});

test("pending attach reset resolves explicitly before closing the old handle", async () => {
  let callbackClosed;
  const callbacks = [];
  let resetResult;
  const context = makeHandle({ options: { onClose: () => { callbacks.push("close"); }, onReset: () => {
    callbacks.push("reset");
    callbackClosed = context.handle.closed;
    assert.equal(context.handle.writable, false);
    assert.throws(() => context.handle.attach(), /closed/);
  } } });
  const pending = context.handle.attach();
  const attachFrame = lastControl(context.sent);
  assert.equal(context.handle.receiveReset(attachFrame.id, { session_id: sessionId, floor: 7n, head: 8n }), true);
  resetResult = await pending;
  assert.deepEqual(resetResult, { sessionId, floor: 7n, head: 8n, kind: "reset", freshAttachRequired: true });
  assert.equal(callbackClosed, true);
  assert.equal(context.handle.closed, true);
  assert.deepEqual(callbacks, ["close", "reset"]);
  assert.equal(context.sent.length, 1);
  assert.equal(context.handle.receiveReset(attachFrame.id, { session_id: sessionId, floor: 7n, head: 8n }), true);
});

test("acquire timer installation failure rejects the completed operation", async () => {
  const timer = {
    now: () => 100_000,
    setTimeout: () => { throw new Error("timer install failed"); },
    clearTimeout: () => {},
  };
  const context = makeHandle({ timer });
  await attachedOnly(context);
  const acquire = context.handle.acquireInput();
  const acquireFrame = lastControl(context.sent);
  context.handle.receive(serverFrame(encodeTerminalLeaseResult(acquireFrame.id, {
    operation: "acquired", run_id: runId, session_id: sessionId, generation: 1n,
    expires_at_ms: 130_000n, last_input_sequence: 0n, run_revision: 1n, session_revision: 1n,
  })));
  await assert.rejects(acquire, /terminal timer failed/);
  assert.equal(context.handle.closed, true);
  assert.equal(context.fatals.length, 1);
});

test("input chronology rejects impossible accepted, partial, rejected, and uncertain byte counts", async () => {
  const cases = [
    ["accepted", 1n],
    ["partial", 2n],
    ["rejected", 1n],
    ["uncertain", 1n],
  ];
  for (const [status, acceptedBytes] of cases) {
    const context = makeHandle();
    const { attachFrame } = await attachedWithLease(context);
    const pending = context.handle.sendInput(new Uint8Array([1, 2]));
    assert.throws(() => context.handle.receive(inputResult(attachFrame.id, {
      session_id: sessionId, generation: 1n, sequence: 1n, status, accepted_bytes: acceptedBytes,
    })), /malformed/, status);
    context.handle.terminate(new Error("malformed result"));
    await assert.rejects(pending, /closed|malformed result/, status);
  }
});

test("forged or stale input results cannot consume a reservation", async () => {
  const context = makeHandle();
  const { attachFrame } = await attachedWithLease(context);
  const pending = context.handle.sendInput(new Uint8Array([1]));
  assert.equal(context.handle.receive(inputResult("ff".repeat(16), {
    session_id: sessionId, generation: 1n, sequence: 1n, status: "accepted", accepted_bytes: 1n,
  })), false);
  assert.equal(context.handle.receive(inputResult(attachFrame.id, {
    session_id: sessionId, generation: 2n, sequence: 1n, status: "accepted", accepted_bytes: 1n,
  })), false);
  assert.equal(context.handle.receive(inputResult(attachFrame.id, {
    session_id: sessionId, generation: 1n, sequence: 2n, status: "accepted", accepted_bytes: 1n,
  })), false);
  context.handle.receive(inputResult(attachFrame.id, {
    session_id: sessionId, generation: 1n, sequence: 1n, status: "accepted", accepted_bytes: 1n,
  }));
  assert.equal((await pending).status, "accepted");
});

test("attach, lease, input, and resize effects are single-flight", async () => {
  const context = makeHandle();
  const attach = context.handle.attach();
  await assert.rejects(context.handle.attach(), /terminal operation pending/);
  const attachFrame = lastControl(context.sent);
  assert.equal(context.handle.receive(inputResult(attachFrame.id, {
    session_id: sessionId, generation: 1n, sequence: 1n, status: "accepted", accepted_bytes: 0n,
  })), false);
  context.handle.receive(serverFrame(replyAttached({}, attachFrame)));
  await attach;

  const acquire = context.handle.acquireInput();
  await assert.rejects(context.handle.acquireInput(), /terminal operation pending/);
  const acquireFrame = lastControl(context.sent);
  context.handle.receive(serverFrame(encodeTerminalLeaseResult(acquireFrame.id, {
    operation: "acquired", run_id: runId, session_id: sessionId, generation: 1n,
    expires_at_ms: 130_000n, last_input_sequence: 0n, run_revision: 1n, session_revision: 1n,
  })));
  await acquire;
  const input = context.handle.sendInput(new Uint8Array([1]));
  await assert.rejects(context.handle.sendInput(new Uint8Array([2])), /terminal operation pending/);
  const resize = context.handle.resize(24, 80);
  await assert.rejects(resize, /terminal operation pending/);
  context.handle.receive(inputResult(attachFrame.id, {
    session_id: sessionId, generation: 1n, sequence: 1n, status: "accepted", accepted_bytes: 1n,
  }));
  await input;
});

test("renewal, release, and resize results preserve exact chronology and correlation", async () => {
  const context = makeHandle();
  const { attachFrame } = await attachedWithLease(context);
  const input = context.handle.sendInput(new Uint8Array([1, 2]));
  context.handle.receive(inputResult(attachFrame.id, {
    session_id: sessionId, generation: 1n, sequence: 1n, status: "accepted", accepted_bytes: 2n,
  }));
  await input;

  context.timer.advance(10_000);
  const renewFrame = lastControl(context.sent);
  assert.equal(renewFrame.type, "TERMINAL_LEASE_RENEW");
  assert.throws(() => context.handle.receive(leaseResult(renewFrame.id, {
    operation: "renewed", run_id: runId, session_id: sessionId, generation: 1n,
    expires_at_ms: 140_000n, last_input_sequence: 0n, run_revision: 1n, session_revision: 1n,
  })), /malformed/);
  context.handle.terminate(new Error("bad renewal"));

  const second = makeHandle();
  await attachedWithLease(second);
  const resize = second.handle.resize(24, 80);
  const resizeFrame = lastControl(second.sent);
  assert.throws(() => second.handle.receive(serverFrame(encodeTerminalResized(resizeFrame.id, {
    session_id: sessionId, generation: 1n, rows: 25, cols: 80,
  }))), /malformed/);
  second.handle.terminate(new Error("bad resize"));
  await assert.rejects(resize, /bad resize|closed/);

  const third = makeHandle();
  await attachedWithLease(third);
  const release = third.handle.releaseInput();
  const releaseFrame = lastControl(third.sent);
  assert.throws(() => third.handle.receive(leaseResult(releaseFrame.id, {
    operation: "released", run_id: runId, session_id: sessionId, generation: 2n,
    last_input_sequence: 1n, run_revision: 1n, session_revision: 1n,
  })), /malformed/);
  third.handle.terminate(new Error("bad release"));
  await assert.rejects(release, /bad release|closed/);
});

test("valid renewal preserves input chronology and release returns the exact next generation", async () => {
  const context = makeHandle();
  const { attachFrame } = await attachedWithLease(context);
  const input = context.handle.sendInput(new Uint8Array([1, 2]));
  context.handle.receive(inputResult(attachFrame.id, {
    session_id: sessionId, generation: 1n, sequence: 1n, status: "accepted", accepted_bytes: 2n,
  }));
  await input;
  context.timer.advance(10_000);
  const renewal = lastControl(context.sent);
  context.handle.receive(serverFrame(encodeTerminalLeaseResult(renewal.id, {
    operation: "renewed", run_id: runId, session_id: sessionId, generation: 1n,
    expires_at_ms: BigInt(context.timer.now + 30_000), last_input_sequence: 1n,
    run_revision: 1n, session_revision: 1n,
  })));
  const release = context.handle.releaseInput();
  const releaseFrame = lastControl(context.sent);
  context.handle.receive(serverFrame(encodeTerminalLeaseResult(releaseFrame.id, {
    operation: "released", run_id: runId, session_id: sessionId, generation: 2n,
    last_input_sequence: 0n, run_revision: 1n, session_revision: 1n,
  })));
  assert.equal((await release).generation, 2n);
  assert.equal(context.handle.writable, false);
});

test("acquire rejects a nonzero last-input chronology and never grants authority", async () => {
  const context = makeHandle();
  await attachedOnly(context);
  const acquire = context.handle.acquireInput();
  const acquireFrame = lastControl(context.sent);
  assert.throws(() => context.handle.receive(leaseResult(acquireFrame.id, {
    operation: "acquired", run_id: runId, session_id: sessionId, generation: 1n,
    expires_at_ms: 130_000n, last_input_sequence: 1n, run_revision: 1n, session_revision: 1n,
  })), /malformed/);
  context.handle.terminate(new Error("noncanonical acquire"));
  await assert.rejects(acquire, /noncanonical acquire|closed/);
  assert.equal(context.handle.writable, false);
});

test("release fences the generation floor and rejects a rewind before accepting a fresh generation", async () => {
  const context = makeHandle();
  await attachedWithLease(context);
  const release = context.handle.releaseInput();
  const releaseFrame = lastControl(context.sent);
  context.handle.receive(serverFrame(encodeTerminalLeaseResult(releaseFrame.id, {
    operation: "released", run_id: runId, session_id: sessionId, generation: 2n,
    last_input_sequence: 0n, run_revision: 1n, session_revision: 1n,
  })));
  await release;
  const rewind = context.handle.acquireInput();
  const rewindFrame = lastControl(context.sent);
  assert.throws(() => context.handle.receive(leaseResult(rewindFrame.id, {
    operation: "acquired", run_id: runId, session_id: sessionId, generation: 2n,
    expires_at_ms: 130_000n, last_input_sequence: 0n, run_revision: 1n, session_revision: 1n,
  })), /malformed/);
  context.handle.terminate(new Error("generation rewind"));
  await assert.rejects(rewind, /generation rewind|closed/);

});

test("partial and uncertain input consume the reservation and recover only with a fresh generation", async () => {
  for (const status of ["partial", "uncertain"]) {
    const context = makeHandle();
    const { attachFrame } = await attachedWithLease(context);
    const pending = context.handle.sendInput(new Uint8Array([1, 2, 3]));
    context.handle.receive(serverFrame(encodeTerminalInputResult(attachFrame.id, {
      session_id: sessionId, generation: 1n, sequence: 1n, status, accepted_bytes: status === "partial" ? 1n : 0n,
    })));
    await pending;
    assert.equal(context.handle.writable, false, status);
    assert.equal(context.sent.filter(({ payload }) => payload instanceof Uint8Array).length, 1, status);

    const sameGeneration = context.handle.acquireInput();
    const sameGenerationFrame = lastControl(context.sent);
    assert.throws(() => context.handle.receive(serverFrame(encodeTerminalLeaseResult(sameGenerationFrame.id, {
      operation: "acquired", run_id: runId, session_id: sessionId, generation: 1n,
      expires_at_ms: BigInt(context.timer.now + 30_000), last_input_sequence: 0n,
      run_revision: 1n, session_revision: 1n,
    }))), /malformed/, status);
    context.handle.terminate(new Error("same generation rejected"));
    await assert.rejects(sameGeneration, /same generation rejected|closed/, status);
    assert.equal(context.handle.writable, false, status);
    assert.equal(context.sent.filter(({ payload }) => payload instanceof Uint8Array).length, 1, status);

    const fresh = makeHandle();
    const freshAttach = await attachedOnly(fresh);
    const freshLease = fresh.handle.acquireInput();
    const freshFrame = lastControl(fresh.sent);
    fresh.handle.receive(serverFrame(encodeTerminalLeaseResult(freshFrame.id, {
      operation: "acquired", run_id: runId, session_id: sessionId, generation: 3n,
      expires_at_ms: BigInt(fresh.timer.now + 30_000), last_input_sequence: 0n,
      run_revision: 1n, session_revision: 1n,
    })));
    await freshLease;
    assert.equal(fresh.handle.writable, true, status);
    const retry = fresh.handle.sendInput(new Uint8Array([9]));
    const inputFrames = fresh.sent.filter(({ payload }) => payload instanceof Uint8Array);
    assert.deepEqual([...inputFrames.at(-1).payload.slice(40)], [9], status);
    fresh.handle.receive(serverFrame(encodeTerminalInputResult(freshAttach.attachFrame.id, {
      session_id: sessionId, generation: 3n, sequence: 1n, status: "accepted", accepted_bytes: 1n,
    })));
    await retry;
    fresh.handle.terminate();
  }
});

test("all terminal control and input send failures are fatal and never retry", async () => {
  const cases = [
    ["TERMINAL_ATTACH", async (context) => context.handle.attach()],
    ["TERMINAL_LEASE_ACQUIRE", async (context) => { await attachedOnly(context); return context.handle.acquireInput(); }],
    ["TERMINAL_RESIZE", async (context) => { await attachedWithLease(context); return context.handle.resize(24, 80); }],
    ["TERMINAL_LEASE_RELEASE", async (context) => { await attachedWithLease(context); return context.handle.releaseInput(); }],
    ["TERMINAL_DETACH", async (context) => { await attachedOnly(context); return context.handle.detach(); }],
    ["TERMINAL_INPUT", async (context) => { await attachedWithLease(context); return context.handle.sendInput(new Uint8Array([1])); }],
  ];
  for (const [type, start] of cases) {
    let context;
    const send = (requestId, payload) => {
      context.sent.push({ requestId, payload });
      if ((payload instanceof Uint8Array && type === "TERMINAL_INPUT") || (typeof payload === "string" && decodeClientControl(payload).type === type)) throw new Error(`${type} send failed`);
    };
    context = makeHandle({ send });
    const pending = start(context);
    await assert.rejects(pending, /failed|closed/, type);
    assert.equal(context.handle.closed, true, type);
    assert.equal(context.fatals.length, 1, type);
    const attempts = context.sent.length;
    context.handle.terminate(new Error("late"));
    assert.equal(context.sent.length, attempts, `${type} must not retry`);
  }
});

test("EOF is observation-only and callback rejection cannot revoke input authority", async () => {
  let eofCalls = 0;
  const context = makeHandle({ options: { onEOF: async () => { eofCalls += 1; throw new Error("observer failed"); } } });
  const { attachFrame } = await attachedWithLease(context);
  assert.equal(context.handle.receiveEOF(attachFrame.id, { session_id: sessionId }), true);
  await Promise.resolve();
  assert.equal(eofCalls, 1);
  assert.equal(context.handle.writable, true);
});

test("exit and reset observation callback rejections stay outside transport ownership", async () => {
  const exitContext = makeHandle({ options: { onExit: async () => { throw new Error("exit observer failed"); } } });
  const { attachFrame } = await attachedOnly(exitContext);
  assert.equal(exitContext.handle.receiveExit(attachFrame.id, { session_id: sessionId, exit_code: 0, exit_signal: 0, aborted: false }), true);
  await Promise.resolve();
  assert.equal(exitContext.handle.closed, true);

  const resetContext = makeHandle({ options: { onReset: async () => { throw new Error("reset observer failed"); } } });
  const attach = resetContext.handle.attach();
  const resetFrame = lastControl(resetContext.sent);
  resetContext.handle.receiveReset(resetFrame.id, { session_id: sessionId, floor: 1n, head: 1n });
  await attach;
  await Promise.resolve();
  assert.equal(resetContext.handle.closed, true);
});

test("generation exhaustion refuses release without emitting an overflow operation", async () => {
  const context = makeHandle();
  await attachedOnly(context);
  const acquire = context.handle.acquireInput();
  const acquireFrame = lastControl(context.sent);
  context.handle.receive(serverFrame(encodeTerminalLeaseResult(acquireFrame.id, {
    operation: "acquired", run_id: runId, session_id: sessionId, generation: 9_223_372_036_854_775_807n,
    expires_at_ms: 130_000n, last_input_sequence: 0n, run_revision: 1n, session_revision: 1n,
  })));
  await acquire;
  const before = context.sent.length;
  await assert.rejects(context.handle.releaseInput(), /generation exhausted/);
  assert.equal(context.sent.length, before);
  context.handle.terminate();
});

test("stale renewal timer callbacks cannot renew a new timer or generation", async () => {
  const context = makeHandle();
  await attachedWithLease(context);
  const stale = [...context.timer.tasks.values()][0].callback;
  context.timer.advance(10_000);
  const renewal = lastControl(context.sent);
  context.handle.receive(serverFrame(encodeTerminalLeaseResult(renewal.id, {
    operation: "renewed", run_id: runId, session_id: sessionId, generation: 1n,
    expires_at_ms: BigInt(context.timer.now + 30_000), last_input_sequence: 0n,
    run_revision: 1n, session_revision: 1n,
  })));
  const renewals = context.sent.filter(({ payload }) => typeof payload === "string" && decodeClientControl(payload).type === "TERMINAL_LEASE_RENEW").length;
  stale();
  assert.equal(context.sent.filter(({ payload }) => typeof payload === "string" && decodeClientControl(payload).type === "TERMINAL_LEASE_RENEW").length, renewals);
  context.handle.terminate();
  stale();
  assert.equal(context.sent.filter(({ payload }) => typeof payload === "string" && decodeClientControl(payload).type === "TERMINAL_LEASE_RENEW").length, renewals);
});

test("late duplicate responses consume only exact retired operations", async () => {
  const context = makeHandle();
  const { attachFrame } = await attachedOnly(context);
  const attached = { v: 1, type: "TERMINAL_ATTACHED", id: attachFrame.id, body: { session_id: sessionId, floor: 0n, head: 0n, acknowledged_sequence: 0n, max_unacked_bytes: 65_536n } };
  assert.equal(context.handle.receive(attached), false);
  context.handle.terminate();
  assert.equal(context.handle.receive(attached), true);
  assert.equal(context.handle.receive({ ...attached, id: "ff".repeat(16) }), false);

  const leaseContext = makeHandle();
  const { attachFrame: leaseAttachFrame } = await attachedOnly(leaseContext);
  const acquire = leaseContext.handle.acquireInput();
  const acquireFrame = lastControl(leaseContext.sent);
  leaseContext.handle.terminate();
  await assert.rejects(acquire, /closed/);
  assert.equal(leaseContext.handle.receive(leaseResult(acquireFrame.id, {
    operation: "acquired", run_id: runId, session_id: sessionId, generation: 1n,
    expires_at_ms: 130_000n, last_input_sequence: 0n, run_revision: 1n, session_revision: 1n,
  })), true);
  assert.equal(leaseContext.handle.receive(leaseResult("ff".repeat(16), {
    operation: "acquired", run_id: runId, session_id: sessionId, generation: 1n,
    expires_at_ms: 130_000n, last_input_sequence: 0n, run_revision: 1n, session_revision: 1n,
  })), false);

  const errorContext = makeHandle();
  await attachedOnly(errorContext);
  const pendingAcquire = errorContext.handle.acquireInput();
  const pendingAcquireFrame = lastControl(errorContext.sent);
  assert.equal(errorContext.handle.receiveError("ff".repeat(16), new Error("unrelated")), false);
  assert.equal(errorContext.handle.receiveError(pendingAcquireFrame.id, new Error("correlated")), true);
  await assert.rejects(pendingAcquire, /correlated/);
  assert.equal(errorContext.handle.closed, false);
  errorContext.handle.terminate();
  assert.equal(errorContext.handle.receiveError(pendingAcquireFrame.id, new Error("late")), false);

  assert.equal(await leaseContext.handle.receiveBinary({ ...outputFrame(0n), sessionId: new Uint8Array(16).fill(0x33) }), false);
  assert.equal(await leaseContext.handle.receiveBinary(outputFrame(0n)), false);
  assert.equal(leaseContext.handle.receiveEOF("ff".repeat(16), { session_id: sessionId }), false);
  assert.equal(leaseContext.handle.receiveEOF(leaseAttachFrame.id, { session_id: sessionId }), false);
  assert.equal(leaseContext.handle.receiveReset("ff".repeat(16), { session_id: sessionId, floor: 0n, head: 0n }), false);
  assert.equal(leaseContext.handle.receiveReset(leaseAttachFrame.id, { session_id: sessionId, floor: 0n, head: 0n }), false);
});

test("partial input fences authority and never replays a suffix", async () => {
  const context = makeHandle();
  const { attachFrame } = await attachedWithLease(context);
  const input = context.handle.sendInput(new Uint8Array([1, 2, 3]));
  context.handle.receive(serverFrame(encodeTerminalInputResult(attachFrame.id, {
    session_id: sessionId, generation: 1n, sequence: 1n, status: "partial", accepted_bytes: 1n,
  })));
  await input;
  assert.equal(context.handle.writable, false);
  assert.throws(() => context.handle.sendInput(new Uint8Array([9])), /terminal lease required/);
  assert.equal(context.sent.filter(({ payload }) => payload instanceof Uint8Array).length, 1);
});

test("release precedes detach and exact release ambiguity never sends detach", async () => {
  const context = makeHandle();
  await attachedWithLease(context);
  const detached = context.handle.detach();
  const release = lastControl(context.sent);
  assert.equal(release.type, "TERMINAL_LEASE_RELEASE");
  context.handle.receive(serverFrame(encodeTerminalLeaseResult(release.id, {
    operation: "released", run_id: runId, session_id: sessionId, generation: 2n,
    last_input_sequence: 0n, run_revision: 1n, session_revision: 1n,
  })));
  const detach = lastControl(context.sent);
  assert.equal(detach.type, "TERMINAL_DETACH");
  context.handle.receive(serverFrame(encodeTerminalDetached(detach.id, { session_id: sessionId })));
  await detached;

  // A release the daemon refuses (the run was cancelled first, the lease
  // moved on) is not fatal: authority is dropped and fenced, the detach goes
  // on, and the session keeps observing (#542).
  const refused = makeHandle();
  await attachedWithLease(refused);
  const pending = refused.handle.detach();
  const releaseFrame = lastControl(refused.sent);
  assert.equal(refused.handle.receiveError(releaseFrame.id, new SessionErrorLikeError("stale")), true);
  assert.equal(refused.fatals.length, 0);
  assert.equal(refused.handle.writable, false);
  // The authority is gone, not merely parked behind the detach.
  assert.throws(() => refused.handle.sendInput(new Uint8Array([1])), /terminal lease required/);
  const detachFrame = lastControl(refused.sent);
  assert.equal(detachFrame.type, "TERMINAL_DETACH");
  refused.handle.receive(serverFrame(encodeTerminalDetached(detachFrame.id, { session_id: sessionId })));
  await pending;
  assert.equal(refused.fatals.length, 0);
});

test("detach waits for a pending attach before sending detach", async () => {
  const context = makeHandle();
  const attach = context.handle.attach();
  const attachFrame = lastControl(context.sent);
  const detached = context.handle.detach();
  assert.equal(context.sent.length, 1);
  context.handle.receive(serverFrame(replyAttached({}, attachFrame)));
  await attach;
  assert.equal(lastControl(context.sent).type, "TERMINAL_DETACH");
  const detachFrame = lastControl(context.sent);
  context.handle.receive(serverFrame(encodeTerminalDetached(detachFrame.id, { session_id: sessionId })));
  await detached;
  assert.equal(context.handle.closed, true);
});

test("detach queues behind acquire and manual release transitions", async (t) => {
  await t.test("acquire", async () => {
    const context = makeHandle();
    await attachedOnly(context);
    const acquire = context.handle.acquireInput();
    const acquireFrame = lastControl(context.sent);
    const detached = context.handle.detach();
    context.handle.receive(serverFrame(encodeTerminalLeaseResult(acquireFrame.id, {
      operation: "acquired", run_id: runId, session_id: sessionId, generation: 1n,
      expires_at_ms: 200_000n, last_input_sequence: 0n, run_revision: 1n, session_revision: 1n,
    })));
    await acquire;
    const releaseFrame = lastControl(context.sent);
    assert.equal(releaseFrame.type, "TERMINAL_LEASE_RELEASE");
    context.handle.receive(serverFrame(encodeTerminalLeaseResult(releaseFrame.id, {
      operation: "released", run_id: runId, session_id: sessionId, generation: 2n,
      last_input_sequence: 0n, run_revision: 1n, session_revision: 1n,
    })));
    const detachFrame = lastControl(context.sent);
    context.handle.receive(serverFrame(encodeTerminalDetached(detachFrame.id, { session_id: sessionId })));
    await detached;
  });

  await t.test("manual release", async () => {
    const context = makeHandle();
    await attachedWithLease(context);
    const released = context.handle.releaseInput();
    const releaseFrame = lastControl(context.sent);
    const detached = context.handle.detach();
    context.handle.receive(serverFrame(encodeTerminalLeaseResult(releaseFrame.id, {
      operation: "released", run_id: runId, session_id: sessionId, generation: 2n,
      last_input_sequence: 0n, run_revision: 1n, session_revision: 1n,
    })));
    await released;
    const detachFrame = lastControl(context.sent);
    assert.equal(detachFrame.type, "TERMINAL_DETACH");
    context.handle.receive(serverFrame(encodeTerminalDetached(detachFrame.id, { session_id: sessionId })));
    await detached;
  });
});

test("detach is single-flight, waits for release, and fences late duplicate results", async () => {
  const context = makeHandle();
  await attachedWithLease(context);
  const detached = context.handle.detach();
  const releaseFrame = lastControl(context.sent);
  assert.equal(context.handle.detach(), detached);
  context.handle.receive(serverFrame(encodeTerminalLeaseResult(releaseFrame.id, {
    operation: "released", run_id: runId, session_id: sessionId, generation: 2n,
    last_input_sequence: 0n, run_revision: 1n, session_revision: 1n,
  })));
  const detachFrame = lastControl(context.sent);
  context.handle.receive(serverFrame(encodeTerminalDetached(detachFrame.id, { session_id: sessionId })));
  await detached;
  assert.equal(context.handle.closed, true);
  assert.equal(context.handle.receive(serverFrame(encodeTerminalDetached(detachFrame.id, { session_id: sessionId }))), true);
});

test("detach cannot overtake a pending input effect", async () => {
  const context = makeHandle();
  const { attachFrame } = await attachedWithLease(context);
  const input = context.handle.sendInput(new Uint8Array([1]));
  const detached = context.handle.detach();
  assert.equal(context.sent.filter(({ payload }) => typeof payload === "string" && decodeClientControl(payload).type === "TERMINAL_DETACH").length, 0);
  context.handle.receive(inputResult(attachFrame.id, {
    session_id: sessionId, generation: 1n, sequence: 1n, status: "accepted", accepted_bytes: 1n,
  }));
  await input;
  const releaseFrame = lastControl(context.sent);
  context.handle.receive(serverFrame(encodeTerminalLeaseResult(releaseFrame.id, {
    operation: "released", run_id: runId, session_id: sessionId, generation: 2n,
    last_input_sequence: 0n, run_revision: 1n, session_revision: 1n,
  })));
  const detachFrame = lastControl(context.sent);
  context.handle.receive(serverFrame(encodeTerminalDetached(detachFrame.id, { session_id: sessionId })));
  await detached;
});

test("detach cannot overtake a pending resize effect", async () => {
  const context = makeHandle();
  await attachedWithLease(context);
  const resize = context.handle.resize(24, 80);
  const detached = context.handle.detach();
  assert.equal(context.sent.filter(({ payload }) => typeof payload === "string" && decodeClientControl(payload).type === "TERMINAL_DETACH").length, 0);
  const resizeFrame = lastControl(context.sent);
  context.handle.receive(serverFrame(encodeTerminalResized(resizeFrame.id, { session_id: sessionId, generation: 1n, rows: 24, cols: 80 })));
  await resize;
  const releaseFrame = lastControl(context.sent);
  context.handle.receive(serverFrame(encodeTerminalLeaseResult(releaseFrame.id, {
    operation: "released", run_id: runId, session_id: sessionId, generation: 2n,
    last_input_sequence: 0n, run_revision: 1n, session_revision: 1n,
  })));
  const detachFrame = lastControl(context.sent);
  context.handle.receive(serverFrame(encodeTerminalDetached(detachFrame.id, { session_id: sessionId })));
  await detached;
});

test("detach queues behind an autonomous lease renewal", async () => {
  const context = makeHandle();
  await attachedWithLease(context);
  context.timer.advance(10_000);
  const renewal = lastControl(context.sent);
  assert.equal(renewal.type, "TERMINAL_LEASE_RENEW");
  const detached = context.handle.detach();
  assert.equal(context.sent.filter(({ payload }) => typeof payload === "string" && decodeClientControl(payload).type === "TERMINAL_LEASE_RELEASE").length, 0);
  context.handle.receive(serverFrame(encodeTerminalLeaseResult(renewal.id, {
    operation: "renewed", run_id: runId, session_id: sessionId, generation: 1n,
    expires_at_ms: 200_000n, last_input_sequence: 0n, run_revision: 1n, session_revision: 1n,
  })));
  const release = lastControl(context.sent);
  assert.equal(release.type, "TERMINAL_LEASE_RELEASE");
  context.handle.receive(serverFrame(encodeTerminalLeaseResult(release.id, {
    operation: "released", run_id: runId, session_id: sessionId, generation: 2n,
    last_input_sequence: 0n, run_revision: 1n, session_revision: 1n,
  })));
  const detach = lastControl(context.sent);
  context.handle.receive(serverFrame(encodeTerminalDetached(detach.id, { session_id: sessionId })));
  await detached;
});

test("exit and reset clear writable state before callbacks", async () => {
  let terminal;
  let exitWritable;
  const context = makeHandle({ options: { onExit: () => { exitWritable = terminal.writable; } } });
  terminal = context.handle;
  const { attachFrame } = await attachedWithLease(context);
  assert.equal(terminal.receiveExit(attachFrame.id, { session_id: sessionId, exit_code: 0, exit_signal: 0, aborted: false }), true);
  assert.equal(exitWritable, false);
  const resetContext = makeHandle({ options: { onReset: () => { assert.equal(resetContext.handle.writable, false); } } });
  const resetAttach = resetContext.handle.attach();
  const resetAttachFrame = lastControl(resetContext.sent);
  assert.equal(resetContext.handle.receiveReset(resetAttachFrame.id, { session_id: sessionId, floor: 1n, head: 1n }), true);
  await resetAttach;
});

test("expiry watchdog fails once without renewal retry", async () => {
  const context = makeHandle();
  await attachedWithLease(context, BigInt(context.timer.now + 30_000));
  context.timer.advance(10_000);
  const renewal = lastControl(context.sent);
  assert.equal(renewal.type, "TERMINAL_LEASE_RENEW");
  context.timer.advance(20_000);
  assert.equal(context.fatals.length, 1);
  assert.equal(context.handle.writable, false);
  assert.equal(context.sent.filter(({ payload }) => typeof payload === "string" && decodeClientControl(payload).type === "TERMINAL_LEASE_RENEW").length, 1);
});

test("receiveBinary delivers output before ACK and advances the exact byte cursor", async () => {
  let outputs = 0;
  const context = makeHandle({ options: { onOutput: (output) => {
    if (outputs === 0) {
      assert.equal(context.sent.filter(({ payload }) => typeof payload === "string" && decodeClientControl(payload).type === "TERMINAL_ACK").length, 0);
      assert.deepEqual([...output.payload], [4, 5]);
    }
    outputs += 1;
  } } });
  await attachedOnly(context);
  assert.equal(await context.handle.receiveBinary(outputFrame(0n, [4, 5])), true);
  const ack = lastControl(context.sent);
  assert.equal(ack.type, "TERMINAL_ACK");
  assert.deepEqual(ack.body, { session_id: sessionId, next_sequence: 2n });
  assert.equal(await context.handle.receiveBinary(outputFrame(2n, [6])), true);
  assert.equal(lastControl(context.sent).body.next_sequence, 3n);
  assert.equal(outputs, 2);
});

test("synchronous ACK failure closes exactly once without advancing the output cursor", async () => {
  let outputs = 0;
  let context;
  context = makeHandle({
    options: { onOutput: () => { outputs += 1; } },
    send: (requestId, payload) => {
      context.sent.push({ requestId, payload });
      if (typeof payload === "string" && decodeClientControl(payload).type === "TERMINAL_ACK") throw new Error("ACK transport failed");
    },
  });
  await attachedOnly(context);
  assert.equal(await context.handle.receiveBinary(outputFrame(0n, [4])), true);
  assert.equal(context.handle.closed, true);
  assert.equal(context.fatals.length, 1);
  assert.equal(outputs, 1);
  assert.equal(context.sent.filter(({ payload }) => typeof payload === "string" && decodeClientControl(payload).type === "TERMINAL_ACK").length, 1);
  assert.equal(await context.handle.receiveBinary(outputFrame(0n, [4])), false);
  assert.equal(outputs, 1);
  assert.equal(context.sent.filter(({ payload }) => typeof payload === "string" && decodeClientControl(payload).type === "TERMINAL_ACK").length, 1);
});

test("output callback rejection closes without a false ACK", async () => {
  const context = makeHandle({ options: { onOutput: () => { throw new Error("consumer rejected"); } } });
  await attachedOnly(context);
  assert.equal(await context.handle.receiveBinary(outputFrame(0n)), true);
  assert.equal(context.handle.closed, true);
  assert.equal(context.sent.filter(({ payload }) => typeof payload === "string" && decodeClientControl(payload).type === "TERMINAL_ACK").length, 0);
});

test("closing a blocked output callback cancels its ACK and late completion", async () => {
  let finish;
  const blocked = new Promise((resolve) => { finish = resolve; });
  const context = makeHandle({ options: { onOutput: () => blocked } });
  await attachedOnly(context);
  const receiving = context.handle.receiveBinary(outputFrame(0n));
  await Promise.resolve();
  context.handle.terminate(new Error("socket closed"));
  assert.equal(await receiving, true);
  finish();
  await Promise.resolve();
  assert.equal(context.sent.filter(({ payload }) => typeof payload === "string" && decodeClientControl(payload).type === "TERMINAL_ACK").length, 0);
});

test("recursive output callback is fenced before a duplicate delivery or ACK", async () => {
  let terminal;
  let nested;
  let calls = 0;
  const context = makeHandle({ options: { onOutput: async () => {
    calls += 1;
    if (calls === 1) nested = terminal.receiveBinary(outputFrame(0n, [2]));
  } } });
  terminal = context.handle;
  await attachedOnly(context);
  await terminal.receiveBinary(outputFrame(0n));
  assert.equal(await nested, true);
  assert.equal(calls, 1);
  assert.equal(terminal.closed, true);
  assert.equal(context.sent.filter(({ payload }) => typeof payload === "string" && decodeClientControl(payload).type === "TERMINAL_ACK").length, 0);
});

test("output callback cannot await an authoritative effect or deadlock the serialized reader", async () => {
  let callbackErrors;
  const context = makeHandle({ options: { onOutput: async () => {
    const effects = [
      () => context.handle.attach(),
      () => context.handle.acquireInput(),
      () => context.handle.releaseInput(),
      () => context.handle.sendInput(new Uint8Array([3])),
      () => context.handle.resize(24, 80),
      () => context.handle.detach(),
    ];
    callbackErrors = await Promise.all(effects.map((effect) => effect().then(() => undefined, (error) => error)));
    assert.equal(context.sent.filter(({ payload }) => typeof payload === "string" && decodeClientControl(payload).type === "TERMINAL_ACK").length, 0);
  } } });
  await attachedWithLease(context);
  const before = context.sent.length;
  assert.equal(await context.handle.receiveBinary(outputFrame(0n)), true);
  assert.equal(callbackErrors.length, 6);
  assert.ok(callbackErrors.every((error) => error?.message === "terminal output callback pending"));
  const newControls = context.sent.slice(before).filter(({ payload }) => typeof payload === "string").map(({ payload }) => decodeClientControl(payload).type);
  assert.deepEqual(newControls, ["TERMINAL_ACK"]);
  assert.equal(context.handle.writable, true);
});

test("ACK send reentrancy cannot acknowledge output after the generation is fenced", async () => {
  let terminal;
  let nested;
  const sent = [];
  const context = makeHandle({ options: { onOutput: () => {} } });
  // Replace the test transport with one that re-enters exactly at ACK send.
  terminal = createTerminalHandle(
    { runId, sessionId, runRevision: 1n, sessionRevision: 1n },
    { onOutput: () => {} },
    (requestId, payload) => {
      sent.push({ requestId, payload });
      if (typeof payload === "string" && decodeClientControl(payload).type === "TERMINAL_ACK") nested = terminal.receiveBinary(outputFrame(0n, [2]));
    },
    (() => { let id = 0; return () => responseIds[id++] ?? "09".repeat(16); })(),
    () => {},
    context.timer,
    true,
  );
  const attach = terminal.attach();
  const attachFrame = decodeClientControl(sent.at(-1).payload);
  terminal.receive(serverFrame(replyAttached({}, attachFrame)));
  await attach;
  assert.equal(await terminal.receiveBinary(outputFrame(0n)), true);
  assert.equal(await nested, true);
  assert.equal(terminal.closed, true);
  assert.equal(sent.filter(({ payload }) => typeof payload === "string" && decodeClientControl(payload).type === "TERMINAL_ACK").length, 1);
});

test("invalid output observations are not delivered or acknowledged", async () => {
  let outputs = 0;
  const context = makeHandle({ options: { onOutput: () => { outputs += 1; } } });
  await attachedOnly(context);
  assert.equal(await context.handle.receiveBinary({ ...outputFrame(0n), sessionId: new Uint8Array(16).fill(0x33) }), false);
  assert.equal(await context.handle.receiveBinary({ ...outputFrame(1n) }), false);
  assert.equal(await context.handle.receiveBinary({ ...outputFrame(0n), leaseGeneration: 1n }), false);
  assert.equal(await context.handle.receiveBinary({ ...outputFrame(0n), payload: new Uint8Array() }), false);
  assert.equal(outputs, 0);
  assert.equal(context.sent.filter(({ payload }) => typeof payload === "string" && decodeClientControl(payload).type === "TERMINAL_ACK").length, 0);
});

test("manual lease renewal and raw identities are absent from the public package", async () => {
  const publicApi = await import("../dist/src/index.js");
  assert.equal("TerminalHandle" in publicApi, false);
  assert.equal("createTerminalHandle" in publicApi, false);
  assert.equal("TerminalHandleOptions" in publicApi, false);
});

test("a refused release outside a detach fences its generation and keeps observing", async () => {
  const context = makeHandle();
  await attachedWithLease(context);
  const pending = context.handle.releaseInput();
  const releaseFrame = lastControl(context.sent);
  assert.equal(context.handle.receiveError(releaseFrame.id, new SessionErrorLikeError("stale")), true);
  await assert.rejects(pending, /stale/);
  assert.equal(context.fatals.length, 0);
  assert.equal(context.handle.writable, false);
  assert.throws(() => context.handle.sendInput(new Uint8Array([1])), /terminal lease required/);
  // The fence: the daemon moves to the next generation when it clears a
  // lease it refused to release, and this client must never type under that
  // one either, so a later acquire answered with it is a protocol fault. The
  // refused generation itself was already floored when it was acquired, so
  // only the next one tells the fence apart from that floor.
  void context.handle.acquireInput().catch(() => undefined);
  const acquireFrame = lastControl(context.sent);
  assert.equal(acquireFrame.type, "TERMINAL_LEASE_ACQUIRE");
  assert.throws(() => context.handle.receive(serverFrame(encodeTerminalLeaseResult(acquireFrame.id, {
    operation: "acquired", run_id: runId, session_id: sessionId, generation: 2n,
    expires_at_ms: BigInt(context.timer.now + 30_000), last_input_sequence: 0n, run_revision: 1n, session_revision: 1n,
  }))), ProtocolError);
});

test("a refused renewal drops authority without ending the session", async () => {
  const context = makeHandle();
  await attachedWithLease(context);
  context.timer.advance(10_000);
  const renewal = lastControl(context.sent);
  assert.equal(renewal.type, "TERMINAL_LEASE_RENEW");
  assert.equal(context.handle.receiveError(renewal.id, new SessionErrorLikeError("stale")), true);
  assert.equal(context.fatals.length, 0);
  assert.equal(context.handle.writable, false);
  assert.throws(() => context.handle.sendInput(new Uint8Array([1])), /terminal lease required/);
  // Still attached: a fresh lease past the fence (the refused generation and
  // the one the daemon moves to when it clears it) is accepted.
  const acquire = context.handle.acquireInput();
  const acquireFrame = lastControl(context.sent);
  context.handle.receive(serverFrame(encodeTerminalLeaseResult(acquireFrame.id, {
    operation: "acquired", run_id: runId, session_id: sessionId, generation: 3n,
    expires_at_ms: BigInt(context.timer.now + 30_000), last_input_sequence: 0n, run_revision: 1n, session_revision: 1n,
  })));
  await acquire;
  assert.equal(context.handle.writable, true);
  assert.equal(context.fatals.length, 0);
});
