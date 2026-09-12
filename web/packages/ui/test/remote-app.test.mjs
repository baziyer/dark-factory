import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import test from "node:test";
import { StrictMode, createElement } from "react";
import { act, create } from "react-test-renderer";
import { MemoryRemoteStore, ProtocolError, RemoteDaemonMismatchError, SessionError } from "@dark-factory/client";
import { RemoteApp } from "../dist/src/index.js";
import { fixtureState } from "../../../fixtures/state.mjs";
import { invitationFragment, invitationMembers, nodeId } from "../../client/test/remote-fake.mjs";

const NORTH = nodeId("a");
const SOUTH = nodeId("b");

const northRequest = [...fixtureState.humanRequests.values()][0];

const southProject = "51".repeat(16);
const southAgent = "52".repeat(16);
const southTask = "53".repeat(16);
const southRequestID = "54".repeat(16);
const southRequest = {
  id: southRequestID,
  project_id: southProject,
  agent_id: southAgent,
  task_id: southTask,
  created_at: 50n,
  updated_at: 51n,
  revision: 7n,
  kind: "question",
  status: "open",
  reply_max_bytes: 8192,
  can_reply: true,
};
const southState = {
  head: 9n,
  factory: { dispatch_enabled: true, capacity: 4, active_runs: 1, revision: 9n },
  projects: new Map([[southProject, { id: southProject, name: "Harbour Line", revision: 2n }]]),
  agents: new Map([[southAgent, { id: southAgent, project_id: southProject, name: "Harbour One", role: "worker", provider: "codex", paused: false, revision: 3n }]]),
  tasks: new Map([[southTask, { id: southTask, project_id: southProject, assigned_agent_id: southAgent, title: "Re-tile the harbour", status: "running", priority: 5, revision: 4n }]]),
  humanRequests: new Map([[southRequestID, southRequest]]),
  accounts: new Map(),
};

const northFactory = (overrides = {}) => ({ nodeId: NORTH, label: "North Shop", status: "ready", state: fixtureState, ...overrides });
const southFactory = (overrides = {}) => ({ nodeId: SOUTH, label: "South Shop", status: "ready", state: southState, ...overrides });

function detailFor(request, overrides = {}) {
  return {
    requestId: request.id,
    revision: request.revision,
    question: "Proceed with the migration?",
    options: [],
    canReply: true,
    replyMaxBytes: 8192,
    terminalTarget: {},
    cancelRun: { requestId: request.id, expectedRequestRevision: request.revision, expectedRunRevision: 4n },
    ...overrides,
  };
}

/** A session that records every one-shot call and answers from a script. */
function fakeSession(script = {}) {
  const calls = { detail: [], reply: [], cancel: [] };
  return {
    calls,
    async getHumanRequestDetail(request) {
      calls.detail.push({ ...request });
      if (script.detail === undefined) throw new SessionError("not_found");
      return script.detail(calls.detail.length);
    },
    async replyHumanRequest(detail, reply) {
      calls.reply.push({ detail, reply });
      return script.reply === undefined ? { request_id: detail.requestId } : script.reply();
    },
    async cancelHumanRequest(cancelRun) {
      calls.cancel.push(cancelRun);
      return script.cancel === undefined ? { request_id: cancelRun.requestId } : script.cancel();
    },
  };
}

/**
 * The RemoteManager surface the console actually consumes. needsYou is derived
 * exactly as the real manager derives it, so the aggregate and the per-factory
 * lists cannot silently disagree.
 */
function fakeManager(factories = [], sessions = new Map()) {
  const calls = { start: 0, select: [], pair: [], forget: [], forgetDevice: 0, close: 0, options: [], setPush: [] };
  const manager = {
    calls,
    sessions,
    pushRecord: undefined,
    push() { return manager.pushRecord; },
    async setPush(subscription) { calls.setPush.push(subscription); manager.pushRecord = subscription; manager.onChange(); },
    factories: () => manager.list,
    list: factories,
    selectedId: factories[0]?.nodeId,
    onChange: () => {},
    async start() { calls.start += 1; },
    selected() { return manager.selectedId; },
    select(node) {
      if (!manager.list.some((factory) => factory.nodeId === node)) throw new SessionError("not_found");
      calls.select.push(node);
      manager.selectedId = node;
      manager.onChange();
    },
    client(node) {
      const session = sessions.get(node);
      return session === undefined ? undefined : { session };
    },
    needsYou() {
      const items = [];
      for (const factory of manager.list) {
        for (const request of factory.state?.humanRequests.values() ?? []) {
          if (request.status === "open") items.push({ nodeId: factory.nodeId, label: factory.label, request });
        }
      }
      return items;
    },
    async pair(invitation) { calls.pair.push(invitation); },
    async forget(node) {
      calls.forget.push(node);
      manager.list = manager.list.filter((factory) => factory.nodeId !== node);
      manager.selectedId = manager.list[0]?.nodeId;
      manager.onChange();
    },
    async forgetDevice() {
      calls.forgetDevice += 1;
      manager.list = [];
      manager.selectedId = undefined;
      manager.onChange();
    },
    close() { calls.close += 1; },
  };
  return manager;
}

function props(manager, overrides = {}) {
  const { location: where, ...rest } = overrides;
  const location = { hash: "", pathname: "/remote", search: "", origin: "https://app.darkfactory.build", ...where };
  return {
    store: new MemoryRemoteStore(),
    navigator: { onLine: true },
    ...rest,
    location,
    // The browser clears the fragment when history rewrites the URL; so does this.
    history: { state: null, replaceState() { location.hash = ""; } },
    managerFactory: (options) => { manager.calls.options.push(options); manager.onChange = options.onChange; return manager; },
  };
}

async function settle() {
  await act(async () => { await Promise.resolve(); await Promise.resolve(); await Promise.resolve(); });
}

async function withApp(appProps, body) {
  const previous = globalThis.IS_REACT_ACT_ENVIRONMENT;
  globalThis.IS_REACT_ACT_ENVIRONMENT = true;
  let renderer;
  try {
    await act(async () => { renderer = create(createElement(RemoteApp, appProps)); });
    await settle();
    await body(renderer);
  } finally {
    if (renderer !== undefined) await act(async () => { renderer.unmount(); });
    globalThis.IS_REACT_ACT_ENVIRONMENT = previous;
  }
}

const classNames = (value) => (typeof value === "string" ? value.split(" ") : []);
const buttons = (renderer, className) => renderer.root.findAll(
  (node) => node.type === "button" && classNames(node.props.className).includes(className),
  { deep: true },
);
function button(renderer, className) {
  const found = buttons(renderer, className);
  assert.equal(found.length, 1, `${className}: expected exactly one control`);
  return found[0];
}
const allButtons = (renderer) => renderer.root.findAll((node) => node.type === "button", { deep: true });

function findNode(node, className) {
  if (node === null || node === undefined || typeof node !== "object") return undefined;
  if (Array.isArray(node)) {
    for (const child of node) {
      const found = findNode(child, className);
      if (found !== undefined) return found;
    }
    return undefined;
  }
  if (classNames(node.props?.className).includes(className)) return node;
  return findNode(node.children, className);
}

function strings(node, out = []) {
  if (node === null || node === undefined) return out;
  if (typeof node === "string") { out.push(node); return out; }
  if (Array.isArray(node)) { for (const child of node) strings(child, out); return out; }
  return strings(node.children, out);
}

/** Adjacent text nodes read as one line, exactly as a person sees them. */
const flat = (node) => strings(node).join(" ").replace(/\s+/g, " ").trim();
const textOf = (renderer) => flat(renderer.toJSON());
function sectionText(renderer, className) {
  const node = findNode(renderer.toJSON(), className);
  assert.ok(node !== undefined, `${className}: section is absent`);
  return flat(node);
}

test("a device with no factory explains how to pair one", async () => {
  const manager = fakeManager([]);
  await withApp(props(manager), (renderer) => {
    assert.equal(manager.calls.start, 1);
    assert.equal(manager.calls.options.length, 1);
    assert.ok(manager.calls.options[0].store instanceof MemoryRemoteStore);
    assert.equal(manager.calls.options[0].origin, "https://app.darkfactory.build");
    const copy = sectionText(renderer, "dfRemote__none");
    assert.match(copy, /PAIR A PHONE/);
    assert.match(copy, /on this phone/);
    assert.equal(buttons(renderer, "dfRemote__factory").length, 0);
    assert.equal(buttons(renderer, "dfRemote__pasteOpen").length, 1);
  });
});

test("two factories switch which projects the console shows", async () => {
  const manager = fakeManager([northFactory(), southFactory()]);
  await withApp(props(manager), async (renderer) => {
    const switcher = buttons(renderer, "dfRemote__factory");
    assert.equal(switcher.length, 2);
    assert.deepEqual(switcher.map((control) => control.props["aria-label"]), [
      "North Shop: ready, 1 needs you",
      "South Shop: ready, 1 needs you",
    ]);
    assert.equal(switcher[0].props["aria-pressed"], true);

    const north = sectionText(renderer, "dfRemote__projects");
    assert.match(north, /North Workshop/);
    assert.match(north, /Review the state projection/);
    assert.equal(north.includes("Harbour Line"), false);

    await act(async () => { switcher[1].props.onClick(); });
    assert.deepEqual(manager.calls.select, [SOUTH]);
    const south = sectionText(renderer, "dfRemote__projects");
    assert.match(south, /Harbour Line/);
    assert.match(south, /Re-tile the harbour/);
    assert.equal(south.includes("North Workshop"), false);
    assert.equal(buttons(renderer, "dfRemote__factory")[1].props["aria-pressed"], true);
  });
});

test("NEEDS YOU aggregates both factories and tags each item with its label", async () => {
  const manager = fakeManager([northFactory(), southFactory()]);
  await withApp(props(manager), (renderer) => {
    const aggregate = sectionText(renderer, "dfRemote__needsYou");
    assert.match(aggregate, /2 QUESTIONS/);
    assert.match(aggregate, /Builder One asks/);
    assert.match(aggregate, /North Shop/);
    assert.match(aggregate, /Harbour One asks/);
    assert.match(aggregate, /South Shop/);
    assert.equal(buttons(renderer, "dfRemote__answer").length, 3, "two aggregate rows plus the selected factory's own row");
  });
});

test("opening a question fetches it at its exact revision and renders it as text", async () => {
  const hostile = '<img src=x onerror="alert(1)">';
  const session = fakeSession({ detail: () => detailFor(northRequest, { question: hostile }) });
  const manager = fakeManager([northFactory()], new Map([[NORTH, session]]));
  await withApp(props(manager), async (renderer) => {
    await act(async () => { buttons(renderer, "dfRemote__answer")[0].props.onClick(); });
    assert.deepEqual(session.calls.detail, [{ requestId: northRequest.id, expectedRevision: northRequest.revision }]);

    const question = findNode(renderer.toJSON(), "dfRemote__questionText");
    assert.ok(question !== undefined);
    assert.deepEqual(question.children, [hostile], "the question is one text node and nothing else");
    assert.equal(renderer.root.findAllByType("img").length, 0);
    assert.match(textOf(renderer), /<img src=x onerror="alert\(1\)">/);
  });
});

test("REPLY sends the bounded answer once and a second press during it does nothing", async () => {
  let release;
  const pending = new Promise((resolve) => { release = resolve; });
  const detail = detailFor(northRequest, { replyMaxBytes: 16 });
  const session = fakeSession({ detail: () => detail, reply: () => pending });
  const manager = fakeManager([northFactory()], new Map([[NORTH, session]]));
  await withApp(props(manager), async (renderer) => {
    await act(async () => { buttons(renderer, "dfRemote__answer")[0].props.onClick(); });
    const field = renderer.root.findByProps({ className: "dfRemote__replyText" });
    assert.equal(field.props.maxLength, 16);

    await act(async () => { field.props.onChange({ currentTarget: { value: "x".repeat(17) } }); });
    assert.equal(renderer.root.findByProps({ className: "dfRemote__replyText" }).props.value, "", "the detail's bound is enforced");
    await act(async () => { field.props.onChange({ currentTarget: { value: "ship it" } }); });

    const send = button(renderer, "dfRemote__replyAction");
    await act(async () => { send.props.onClick(); });
    assert.equal(session.calls.reply.length, 1);
    assert.equal(session.calls.reply[0].reply, "ship it");
    assert.equal(session.calls.reply[0].detail, detail, "the reply carries the exact detail authority");

    const pendingSend = button(renderer, "dfRemote__replyAction");
    assert.equal(pendingSend.props.disabled, true);
    await act(async () => { pendingSend.props.onClick(); });
    assert.equal(session.calls.reply.length, 1);

    await act(async () => { release({ request_id: northRequest.id }); await pending; });
    await settle();
    assert.equal(session.calls.detail.length, 2, "the detail is read again, and only once");
  });
});

test("a suggested answer fills the draft and waits for explicit reply", async () => {
  const detail = detailFor(northRequest, { options: ["Continue", "Stop"] });
  const session = fakeSession({ detail: () => detail });
  const manager = fakeManager([northFactory()], new Map([[NORTH, session]]));
  await withApp(props(manager), async (renderer) => {
    await act(async () => { buttons(renderer, "dfRemote__answer")[0].props.onClick(); });
    const suggested = allButtons(renderer).find((control) => flat(control.props.children) === "Continue · RECOMMENDED");
    assert.ok(suggested !== undefined);
    await act(async () => { suggested.props.onClick(); });
    assert.equal(renderer.root.findByProps({ className: "dfRemote__replyText" }).props.value, "Continue");
    assert.equal(session.calls.reply.length, 0);
    await act(async () => { button(renderer, "dfRemote__replyAction").props.onClick(); });
    assert.deepEqual(session.calls.reply.map((call) => call.reply), ["Continue"]);
  });
});

test("read-only questions keep their suggested answers disabled", async () => {
  const detail = detailFor(northRequest, { canReply: false, options: ["Continue", "Stop"] });
  const session = fakeSession({ detail: () => detail });
  const manager = fakeManager([northFactory()], new Map([[NORTH, session]]));
  await withApp(props(manager), async (renderer) => {
    await act(async () => { buttons(renderer, "dfRemote__answer")[0].props.onClick(); });
    const suggested = allButtons(renderer).find((control) => flat(control.props.children) === "Continue · RECOMMENDED");
    assert.ok(suggested !== undefined);
    assert.equal(suggested.props.disabled, true);
    assert.equal(renderer.root.findAllByProps({ className: "dfRemote__replyText" }).length, 0);
    assert.match(sectionText(renderer, "dfRemote__detail"), /THIS OPEN DECISION IS READ-ONLY IN THIS VIEW\./);
  });
});

test("an uncertain reply is named as unknown and offers no retry", async () => {
  const session = fakeSession({
    detail: () => detailFor(northRequest),
    reply: () => Promise.reject(new SessionError("connection", true)),
  });
  const manager = fakeManager([northFactory()], new Map([[NORTH, session]]));
  await withApp(props(manager), async (renderer) => {
    await act(async () => { buttons(renderer, "dfRemote__answer")[0].props.onClick(); });
    const field = renderer.root.findByProps({ className: "dfRemote__replyText" });
    await act(async () => { field.props.onChange({ currentTarget: { value: "go ahead" } }); });
    await act(async () => { button(renderer, "dfRemote__replyAction").props.onClick(); });
    await settle();

    assert.equal(session.calls.reply.length, 1, "an uncertain effect is never repeated");
    const detail = sectionText(renderer, "dfRemote__detail");
    assert.match(detail, /DELIVERY UNKNOWN — CHECK THE FACTORY/);
    assert.equal(detail.includes("NOT DELIVERED"), false);
    for (const control of allButtons(renderer)) {
      assert.equal(/RETRY|TRY AGAIN|SEND AGAIN/i.test(flat(control.props.children)), false);
    }
  });
});

test("CANCEL RUN appears only when the detail advertises it and fires once", async () => {
  const withoutCancel = fakeSession({ detail: () => detailFor(southRequest, { cancelRun: null }) });
  const quiet = fakeManager([southFactory()], new Map([[SOUTH, withoutCancel]]));
  await withApp(props(quiet), async (renderer) => {
    await act(async () => { buttons(renderer, "dfRemote__answer")[0].props.onClick(); });
    assert.equal(buttons(renderer, "dfRemote__cancelOpen").length, 0);
  });

  const detail = detailFor(northRequest);
  const session = fakeSession({ detail: () => detail });
  const manager = fakeManager([northFactory()], new Map([[NORTH, session]]));
  await withApp(props(manager), async (renderer) => {
    await act(async () => { buttons(renderer, "dfRemote__answer")[0].props.onClick(); });
    await act(async () => { button(renderer, "dfRemote__cancelOpen").props.onClick(); });

    const confirm = button(renderer, "dfRemote__cancelAction");
    assert.equal(confirm.props.disabled, true, "the phrase must be typed first");
    const typed = renderer.root.findByProps({ className: "dfRemote__cancelText" });
    await act(async () => { typed.props.onChange({ currentTarget: { value: "cancel run" } }); });
    // Two presses of the one control, before anything can re-render between them.
    await act(async () => {
      const confirmed = button(renderer, "dfRemote__cancelAction");
      confirmed.props.onClick();
      confirmed.props.onClick();
    });
    await settle();

    assert.deepEqual(session.calls.cancel, [detail.cancelRun], "a second press is not a second cancellation");
    assert.equal(session.calls.detail.length, 2, "the detail is read again after the effect");
  });
});

test("only a refusal reads as NOT DELIVERED; every other failure stays unknown", async () => {
  for (const [failure, copy, other] of [
    [new SessionError("too_large"), "NOT DELIVERED", "DELIVERY UNKNOWN"],
    // session.ts throws malformed on a reply result whose revision has moved
    // on — which is exactly what a delivered answer looks like.
    [new ProtocolError("malformed"), "DELIVERY UNKNOWN — CHECK THE FACTORY", "NOT DELIVERED"],
  ]) {
    const session = fakeSession({ detail: () => detailFor(northRequest), reply: () => Promise.reject(failure) });
    const manager = fakeManager([northFactory()], new Map([[NORTH, session]]));
    await withApp(props(manager), async (renderer) => {
      await act(async () => { buttons(renderer, "dfRemote__answer")[0].props.onClick(); });
      const field = renderer.root.findByProps({ className: "dfRemote__replyText" });
      await act(async () => { field.props.onChange({ currentTarget: { value: "go ahead" } }); });
      await act(async () => { button(renderer, "dfRemote__replyAction").props.onClick(); });
      await settle();

      assert.equal(session.calls.reply.length, 1, "an effect is never repeated, whatever it answered");
      const detail = sectionText(renderer, "dfRemote__detail");
      assert.ok(detail.includes(copy), `${failure.code}: ${detail}`);
      assert.equal(detail.includes(other), false, `${failure.code} must not read as ${other}`);
      assert.equal(session.calls.detail.length, 2, "the answer is whatever the factory now says");
    });
  }
});

test("StrictMode spends the invitation once and pairs with the manager it keeps", async () => {
  const managers = [];
  const location = {
    hash: invitationFragment(invitationMembers({ node: NORTH })),
    pathname: "/remote",
    search: "",
    origin: "https://app.darkfactory.build",
  };
  const appProps = {
    store: new MemoryRemoteStore(),
    navigator: { onLine: true },
    location,
    history: { state: null, replaceState() { location.hash = ""; } },
    managerFactory: (options) => {
      const made = fakeManager([]);
      made.onChange = options.onChange;
      managers.push(made);
      return made;
    },
  };
  const previous = globalThis.IS_REACT_ACT_ENVIRONMENT;
  globalThis.IS_REACT_ACT_ENVIRONMENT = true;
  let renderer;
  try {
    await act(async () => { renderer = create(createElement(StrictMode, null, createElement(RemoteApp, appProps))); });
    await settle();
    assert.equal(managers.length, 2, "StrictMode runs the effect twice");
    assert.equal(managers[0].calls.close, 1, "the run that is thrown away closes what it built");
    assert.equal(managers[1].calls.close, 0);
    // The fragment is spent by the mount, and paired by the run that survives.
    assert.deepEqual(managers[0].calls.pair, []);
    assert.equal(managers[1].calls.pair.length, 1);
    assert.equal(managers[1].calls.pair[0].node, NORTH);
    assert.equal(location.hash, "");
  } finally {
    if (renderer !== undefined) await act(async () => { renderer.unmount(); });
    globalThis.IS_REACT_ACT_ENVIRONMENT = previous;
  }
  assert.equal(managers[1].calls.close, 1, "unmounting closes the one it kept");
});

test("ANSWER cannot start a read while a one-shot effect is in flight", async () => {
  let release;
  const landed = new Promise((resolve) => { release = resolve; });
  const session = fakeSession({
    detail: () => detailFor(northRequest),
    reply: () => landed.then(() => Promise.reject(new SessionError("connection", true))),
  });
  const manager = fakeManager([northFactory()], new Map([[NORTH, session]]));
  await withApp(props(manager), async (renderer) => {
    await act(async () => { buttons(renderer, "dfRemote__answer")[0].props.onClick(); });
    const field = renderer.root.findByProps({ className: "dfRemote__replyText" });
    await act(async () => { field.props.onChange({ currentTarget: { value: "go ahead" } }); });
    await act(async () => { button(renderer, "dfRemote__replyAction").props.onClick(); });

    // Nothing may move the detail out from under an answer that is coming back.
    for (const control of buttons(renderer, "dfRemote__answer")) assert.equal(control.props.disabled, true);
    assert.equal(button(renderer, "dfRemote__close").props.disabled, true);
    await act(async () => { buttons(renderer, "dfRemote__answer")[0].props.onClick(); });
    assert.equal(session.calls.detail.length, 1, "a tap during an effect starts no read");

    await act(async () => { release(); await landed; });
    await settle();
    assert.equal(session.calls.reply.length, 1);
    const detail = sectionText(renderer, "dfRemote__detail");
    assert.ok(detail.includes("DELIVERY UNKNOWN — CHECK THE FACTORY"), detail);
    assert.equal(session.calls.detail.length, 2, "one re-read, once the effect answered");
  });
});

test("a drop while a reply is in flight keeps the unknown notice", async () => {
  let dropped = () => {};
  const session = fakeSession({
    detail: () => detailFor(northRequest),
    reply: () => { dropped(); return Promise.reject(new SessionError("connection", true)); },
  });
  const manager = fakeManager([northFactory()], new Map([[NORTH, session]]));
  // The manager discards its snapshot the moment the socket goes, so the
  // console observes nothing at all about the question.
  dropped = () => { manager.list = [northFactory({ state: undefined })]; };
  await withApp(props(manager), async (renderer) => {
    await act(async () => { buttons(renderer, "dfRemote__answer")[0].props.onClick(); });
    const field = renderer.root.findByProps({ className: "dfRemote__replyText" });
    await act(async () => { field.props.onChange({ currentTarget: { value: "go ahead" } }); });
    await act(async () => { button(renderer, "dfRemote__replyAction").props.onClick(); });
    await settle();

    const detail = sectionText(renderer, "dfRemote__detail");
    assert.ok(detail.includes("DELIVERY UNKNOWN — CHECK THE FACTORY"), detail);
    assert.equal(detail.includes("THIS QUESTION IS NO LONGER OPEN"), false, "no snapshot observed it closing");
  });
});

test("ANSWER opens one question at a time", async () => {
  let release;
  const held = new Promise((resolve) => { release = resolve; });
  const session = fakeSession({ detail: () => held.then(() => detailFor(northRequest)) });
  const manager = fakeManager([northFactory()], new Map([[NORTH, session]]));
  await withApp(props(manager), async (renderer) => {
    assert.equal(buttons(renderer, "dfRemote__answer").length, 2, "the aggregate row and the factory's own row");
    await act(async () => { buttons(renderer, "dfRemote__answer")[0].props.onClick(); });

    // The daemon allows one human operation per request at a time, so both
    // controls are inert while the first read is in flight.
    for (const control of buttons(renderer, "dfRemote__answer")) assert.equal(control.props.disabled, true);
    await act(async () => { buttons(renderer, "dfRemote__answer")[1].props.onClick(); });
    assert.equal(session.calls.detail.length, 1, "a second tap never races the first read");

    await act(async () => { release(); await held; });
    await settle();
    assert.equal(session.calls.detail.length, 1);
    assert.match(sectionText(renderer, "dfRemote__detail"), /Proceed with the migration\?/);
  });
});

test("an offline factory says so and offers nothing that could only fail", async () => {
  const session = fakeSession({ detail: () => detailFor(northRequest) });
  // Status governs, never the presence of a snapshot: a stale snapshot must
  // not resurrect controls the connection can no longer carry.
  const manager = fakeManager([northFactory({ status: "offline" })], new Map([[NORTH, session]]));
  await withApp(props(manager), async (renderer) => {
    const banner = findNode(renderer.toJSON(), "dfRemote__banner--offline");
    assert.ok(banner !== undefined);
    assert.equal(banner.props.role, "status");
    assert.deepEqual(banner.children, ["FACTORY OFFLINE"]);

    for (const control of buttons(renderer, "dfRemote__answer")) assert.equal(control.props.disabled, true);
    await act(async () => { buttons(renderer, "dfRemote__answer")[0].props.onClick(); });
    assert.equal(session.calls.detail.length, 0, "a disabled control still refuses the effect");
    assert.equal(findNode(renderer.toJSON(), "dfRemote__detail"), undefined);
  });

  const revoked = fakeManager([northFactory({ status: "revoked", state: undefined })]);
  await withApp(props(revoked), (renderer) => {
    const banner = findNode(renderer.toJSON(), "dfRemote__banner--revoked");
    assert.ok(banner !== undefined);
    assert.deepEqual(banner.children, ["ACCESS REVOKED"]);
    assert.equal(button(renderer, "dfRemote__forgetFactory").props.disabled, false, "a revoked factory can always be forgotten");
  });

  // The node still routes, but the daemon behind it is not the bound one: that
  // is an identity failure, and it is never dressed up as a transient.
  const mismatched = fakeManager([northFactory({ status: "mismatch" })], new Map([[NORTH, session]]));
  await withApp(props(mismatched), async (renderer) => {
    const banner = findNode(renderer.toJSON(), "dfRemote__banner--mismatch");
    assert.ok(banner !== undefined);
    assert.equal(banner.props.role, "status");
    assert.deepEqual(banner.children, ["FACTORY IDENTITY MISMATCH"]);
    for (const control of buttons(renderer, "dfRemote__answer")) assert.equal(control.props.disabled, true);
    await act(async () => { buttons(renderer, "dfRemote__answer")[0].props.onClick(); });
    assert.equal(session.calls.detail.length, 0, "nothing is sent to a factory this device cannot identify");
    assert.equal(button(renderer, "dfRemote__forgetFactory").props.disabled, false, "a mismatched factory can always be forgotten");
  });

  // A binding the manager reports as error is one no reconnection repairs.
  const refused = fakeManager([northFactory({ status: "error" })], new Map([[NORTH, session]]));
  await withApp(props(refused), async (renderer) => {
    const banner = findNode(renderer.toJSON(), "dfRemote__banner--error");
    assert.ok(banner !== undefined);
    assert.equal(banner.props.role, "status");
    assert.deepEqual(banner.children, ["FACTORY REFUSED · PAIR AGAIN"]);
    for (const control of buttons(renderer, "dfRemote__answer")) assert.equal(control.props.disabled, true);
    await act(async () => { buttons(renderer, "dfRemote__answer")[0].props.onClick(); });
    assert.equal(session.calls.detail.length, 0);
    assert.equal(button(renderer, "dfRemote__forgetFactory").props.disabled, false, "a refused binding can always be forgotten");
  });

  // A control ticket the relay would refuse is never presented again, so the
  // console asks for the one thing that repairs it.
  const expired = fakeManager([northFactory({ status: "expired" })]);
  await withApp(props(expired), (renderer) => {
    const banner = findNode(renderer.toJSON(), "dfRemote__banner--expired");
    assert.ok(banner !== undefined);
    assert.equal(banner.props.role, "status");
    assert.deepEqual(banner.children, ["INVITATION EXPIRED · PAIR AGAIN"]);
    const answers = buttons(renderer, "dfRemote__answer");
    assert.ok(answers.length > 0);
    for (const control of answers) assert.equal(control.props.disabled, true);
    assert.equal(button(renderer, "dfRemote__forgetFactory").props.disabled, false, "an expired binding can always be forgotten");
  });
});

test("a device with no network disables every control that needs one", async () => {
  const manager = fakeManager([northFactory(), southFactory()]);
  await withApp(props(manager, { navigator: { onLine: false } }), (renderer) => {
    const banner = findNode(renderer.toJSON(), "dfRemote__banner--device");
    assert.ok(banner !== undefined);
    assert.equal(banner.props.role, "status");
    assert.deepEqual(banner.children, ["DEVICE OFFLINE"]);

    const local = ["dfRemote__forgetDevice", "dfRemote__forgetFactory"];
    for (const control of allButtons(renderer)) {
      const names = classNames(control.props.className);
      const isLocal = local.some((name) => names.includes(name));
      assert.equal(control.props.disabled, !isLocal, names.join(" "));
    }
  });
});

test("an invitation in the address bar pairs once and never survives the read", async () => {
  const manager = fakeManager([]);
  let released;
  const paired = new Promise((resolve) => { released = resolve; });
  manager.pair = async (invitation) => { manager.calls.pair.push(invitation); await paired; };
  const members = invitationMembers({ node: NORTH });
  const appProps = props(manager, { location: { hash: invitationFragment(members) } });
  await withApp(appProps, async (renderer) => {
    assert.match(sectionText(renderer, "dfRemote__pair"), /PAIRING FACTORY…/);
    await act(async () => { released(); await paired; });
    await settle();
    assert.equal(manager.calls.pair.length, 1);
    assert.equal(manager.calls.pair[0].node, NORTH);
    assert.equal(appProps.location.hash, "", "the one-shot fragment is cleared before it is used");
    assert.equal(textOf(renderer).includes("PAIRING FACTORY…"), false, "pairing has finished");
  });

  const spent = fakeManager([]);
  const spentProps = props(spent, { location: { hash: "#df_remote&node=not-an-invitation" } });
  await withApp(spentProps, (renderer) => {
    assert.equal(spent.calls.pair.length, 0);
    assert.match(sectionText(renderer, "dfRemote__pair"), /THIS INVITATION HAS EXPIRED OR WAS ALREADY USED/);
  });

  const wrong = fakeManager([]);
  wrong.pair = async () => { throw new RemoteDaemonMismatchError(NORTH); };
  const wrongProps = props(wrong, { location: { hash: invitationFragment(invitationMembers({ node: NORTH })) } });
  await withApp(wrongProps, (renderer) => {
    // A daemon that is not the invited one is named, not called a refusal.
    assert.match(sectionText(renderer, "dfRemote__pair"), /A DIFFERENT FACTORY ANSWERED FOR THIS NODE/);
  });
});

test("forgetting a factory or the device takes a second, inline confirmation", async () => {
  const manager = fakeManager([northFactory(), southFactory()]);
  await withApp(props(manager), async (renderer) => {
    await act(async () => { button(renderer, "dfRemote__forgetFactory").props.onClick(); });
    assert.deepEqual(manager.calls.forget, []);
    const confirm = button(renderer, "dfRemote__forgetFactory--confirm");
    assert.match(flat(confirm.props.children), /North Shop/);
    await act(async () => { confirm.props.onClick(); });
    await settle();
    assert.deepEqual(manager.calls.forget, [NORTH]);
    assert.equal(buttons(renderer, "dfRemote__factory").length, 1);

    await act(async () => { button(renderer, "dfRemote__forgetDevice").props.onClick(); });
    assert.equal(manager.calls.forgetDevice, 0);
    await act(async () => { button(renderer, "dfRemote__forgetDevice--confirm").props.onClick(); });
    await settle();
    assert.equal(manager.calls.forgetDevice, 1);
    assert.equal(buttons(renderer, "dfRemote__factory").length, 0);
    assert.match(sectionText(renderer, "dfRemote__none"), /PAIR A PHONE/);
  });
});

test("the remote stylesheet stays legible and thumb-sized on one phone column", () => {
  const css = readFileSync(new URL("../src/factory-console.css", import.meta.url), "utf8");
  const marker = css.indexOf("/* remote");
  assert.ok(marker > 0, "the remote section is delimited");
  const remote = css.slice(marker);
  assert.equal(css.slice(0, marker).includes("dfRemote"), false, "remote rules stay inside their section");
  for (const [, size] of remote.matchAll(/font-size:\s*([0-9.]+)rem/g)) {
    assert.ok(Number(size) >= 0.85, `${size}rem is below the legibility floor`);
  }
  assert.match(remote, /min-height:\s*44px/);
  assert.match(remote, /@media \(min-width: 721px\)/);
  // One phone column comes from the console's own grid, restated nowhere.
  assert.match(css, /@media \(max-width: 720px\) \{[\s\S]*?\.dfFactoryConsole__columns \{ grid-template-columns: 1fr; gap: 0; \}/);
  assert.equal(remote.includes("__columns"), false, "the remote section restates no column grid");
  assert.equal(/xterm|terminal/i.test(remote), false, "there is no terminal on a phone");
});

test("the remote console imports no terminal machinery at all", () => {
  const source = readFileSync(new URL("../src/remote/remote-app.tsx", import.meta.url), "utf8");
  assert.equal(/xterm|TerminalController|terminal-controller|openTerminal|dangerouslySetInnerHTML/i.test(source), false);
});

test("alerts turn on through the host's subscription and off with the device", async () => {
  const manager = fakeManager([northFactory()]);
  const subscription = { endpoint: "https://web.push.apple.com/QGdfl/abc", public_key: "B" + "a".repeat(86), private_key: "MIGH" };
  let attempts = 0;
  const subscribePush = async () => { attempts += 1; if (attempts === 1) throw new Error("ALERTS WERE REFUSED"); return subscription; };
  await withApp(props(manager, { subscribePush }), async (renderer) => {
    assert.match(sectionText(renderer, "dfRemote__alerts"), /ALERTS.*OFF/s);
    await act(async () => { button(renderer, "dfRemote__alertsOn").props.onClick(); });
    await settle();
    assert.match(sectionText(renderer, "dfRemote__alerts"), /ALERTS WERE REFUSED/);
    assert.deepEqual(manager.calls.setPush, []);

    await act(async () => { button(renderer, "dfRemote__alertsOn").props.onClick(); });
    await settle();
    assert.deepEqual(manager.calls.setPush, [subscription]);
    assert.match(sectionText(renderer, "dfRemote__alerts"), /ALERTS.*ON/s);
    assert.equal(buttons(renderer, "dfRemote__alertsOn").length, 0, "one subscription per device");

    // The destructive control sits last on the page, not first.
    const all = allButtons(renderer);
    assert.ok(classNames(all.at(-1).props.className).includes("dfRemote__forgetDevice"));
  });

  await withApp(props(fakeManager([])), (renderer) => {
    assert.equal(buttons(renderer, "dfRemote__forgetDevice").length, 0, "nothing to forget on an empty device");
    assert.match(textOf(renderer), /Add to Home Screen/);
  });
});

test("a subscription the browser no longer lets this site use reads as OFF", async () => {
  const manager = fakeManager([northFactory()]);
  manager.pushRecord = { endpoint: "https://web.push.apple.com/QGdfl/abc", public_key: "B" + "a".repeat(86), private_key: "MIGH" };
  const previous = globalThis.Notification;
  globalThis.Notification = { permission: "denied" };
  try {
    await withApp(props(manager), (renderer) => {
      assert.match(sectionText(renderer, "dfRemote__alerts"), /ALERTS.*OFF/s);
      assert.equal(buttons(renderer, "dfRemote__alertsOn").length, 1, "the button comes back so alerts can be turned on again");
    });
    globalThis.Notification = { permission: "granted" };
    await withApp(props(manager), (renderer) => {
      assert.match(sectionText(renderer, "dfRemote__alerts"), /ALERTS.*ON/s);
    });
  } finally {
    if (previous === undefined) delete globalThis.Notification; else globalThis.Notification = previous;
  }
});
