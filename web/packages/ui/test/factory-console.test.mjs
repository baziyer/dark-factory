import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import test from "node:test";
import { createElement, isValidElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { act, create } from "react-test-renderer";
import { ProtocolError, SessionError } from "@dark-factory/client";
import { FactoryApp, FactoryConsole, floorScene } from "../dist/src/index.js";
import { TerminalPanel } from "../dist/src/factory-app.js";
import { fixtureState, fixtureTopology } from "../../../fixtures/state.mjs";

const ids = {
  project: [...fixtureState.projects.keys()][0],
  secondProject: [...fixtureState.projects.keys()][1],
  agent: [...fixtureState.agents.keys()][0],
  orchestrator: [...fixtureState.agents.keys()][1],
  task: [...fixtureState.tasks.keys()][0],
  request: [...fixtureState.humanRequests.keys()][0],
};

const baseState = (overrides = {}) => ({ ...fixtureState, ...overrides });

const render = (props = {}) => renderToStaticMarkup(createElement(FactoryConsole, {
  status: "ready",
  state: baseState(),
  ...props,
}));

const VIEWS = ["floor", "agents"];

const agentSelection = (id = ids.agent) => {
  const agent = fixtureState.agents.get(id);
  return { id: agent.id, name: agent.name, revision: agent.revision };
};

const selectedRequest = (overrides = {}) => ({
  request: fixtureState.humanRequests.get(ids.request),
  phase: "ready",
  question: "Proceed with the migration?",
  canReply: true,
  canCancel: true,
  replyMaxBytes: 8192,
  reply: "",
  ...overrides,
});

test("error banner keeps its centered layout after the paragraph reset", () => {
  const css = readFileSync(new URL("../src/factory-console.css", import.meta.url), "utf8");
  // The sidebar is a sibling of the console, so it needs the same reset.
  assert.match(css, /\.dfFactoryConsole :where\(h1, h2, p, dl, ul\),\s*\.dfConsoleSidebar :where\(h1, h2, h3, p, dl, ul\)\s*\{\s*margin: 0;\s*\}/);
  assert.match(css, /\.dfFactoryConsole__error\s*\{[\s\S]*?margin: 0 auto 1\.25rem;/);
});

test("one screen shows the floor, the counters, and what needs you at once", () => {
  const markup = render();
  assert.match(markup, /<main class="dfFactoryConsole" aria-label="Factory operator console">/);
  for (const label of ["Factory counters", "Left view", "Factory floor", "NEEDS YOU", "Queue"]) {
    assert.match(markup, new RegExp(`aria-label="${label}"`));
  }
  // Counters read the served factory, not a second count of it.
  assert.match(markup, /<dt>ACTIVE RUNS<\/dt><dd>2\/8<\/dd>/);
  assert.match(markup, /<dt>QUEUED<\/dt><dd>1<\/dd>/);
  assert.match(markup, /<dt>NEEDS YOU<\/dt><dd>1<\/dd>/);
  assert.match(markup, /Builder One asks/);
  assert.match(markup, /Review the state projection/);
  assert.match(markup, /North Workshop · Review the state projection/);
  // No screen union survives: there is no navigation away from this screen.
  assert.equal(markup.includes("dfFactoryConsole__homeLink"), false);
  assert.equal(markup.includes("BUILDING STATE UNAVAILABLE"), false);
});

test("the left view toggles between the floor and the ranked agent list", () => {
  const floor = render();
  assert.match(floor, /aria-label="Dark Factory codebase floor"/);
  assert.equal(floor.includes('aria-label="OVERSEER"'), false);

  const agents = render({ view: "agents" });
  assert.match(agents, /aria-label="Agents"/);
  // Rank is the served role, oversight first, and nothing invents a new field.
  const overseer = agents.indexOf('aria-label="OVERSEER"');
  const worker = agents.indexOf('aria-label="WORKER"');
  assert.ok(overseer > -1 && worker > overseer);
  assert.ok(agents.indexOf("Dispatch Lead") < agents.indexOf("Builder One"));
  assert.match(agents, /Builder One[\s\S]*?claude_code[\s\S]*?needs you/);
  assert.match(agents, /Builder Two[\s\S]*?1 queued/);
  assert.equal(agents.includes("rank"), false);
});

test("the floor maps topology to rooms and agents to workers deterministically", () => {
  const scene = floorScene(fixtureState, fixtureTopology);
  // Only the repository root and its direct children become rooms.
  assert.deepEqual(scene.topology.nodes.map((node) => node.label), ["north-workshop", "kernel", "web", "South Workshop"]);
  assert.deepEqual(scene.topology.nodes.map((node) => node.sizeBucket), ["large", "medium", "small", undefined]);
  assert.equal(scene.topology.digest, fixtureTopology.digest);
  assert.deepEqual(scene, floorScene(fixtureState, { ...fixtureTopology, nodes: [...fixtureTopology.nodes] }));

  // The detailed project's agents stand in its repository root; a project the
  // topology does not cover still gets a room, so nobody is stranded.
  const root = scene.topology.nodes[0].id;
  assert.deepEqual(scene.topology.nodes.at(-1).label, "South Workshop");
  assert.deepEqual(scene.workers.map((worker) => [worker.name, worker.activity, worker.nodeId]), [
    ["Builder One", "needs-you", root],
    ["Dispatch Lead", "idle", ids.secondProject],
    ["Builder Two", "waiting", root],
  ]);
  assert.deepEqual(scene.workItems.map((item) => item.stage), ["staged", "release-ready"]);

  // Without topology the floor still has a room per project.
  const fallback = floorScene(fixtureState, undefined);
  assert.deepEqual(fallback.topology.nodes.map((node) => node.label), ["North Workshop", "South Workshop"]);
  assert.deepEqual(fallback.topology.nodes.map((node) => node.sizeBucket), [undefined, undefined]);
  assert.deepEqual(fallback.workers.map((worker) => worker.nodeId), [ids.project, ids.secondProject, ids.project]);
  assert.deepEqual(floorScene(undefined, undefined), { topology: { digest: "", nodes: [] }, workers: [], workItems: [] });
  // The size bucket, not a file count, is what the room subtitle carries.
  const markup = render({ topology: fixtureTopology });
  assert.match(markup, />kernel<\/text>/);
  assert.match(markup, />PACKAGE · MEDIUM<\/text>/);
  assert.equal(markup.includes("FILES"), false);
});

test("hostile names and titles are escaped as text and private detail is absent", () => {
  const hostile = "<img src=x onerror=alert(1)>";
  const hostileState = baseState({
    agents: new Map([[ids.agent, { id: ids.agent, project_id: ids.project, name: hostile, role: "worker", provider: "claude_code", paused: false, model: hostile, reasoning_effort: "", revision: 10n }]]),
    tasks: new Map([[ids.task, { id: ids.task, project_id: ids.project, assigned_agent_id: ids.agent, title: hostile, status: "running", priority: 10, revision: 12n }]]),
  });
  for (const view of VIEWS) {
    // The second pass renders the config form, so the hostile model reaches
    // an input value rather than being dropped with the whole section.
    for (const props of [{ view }, { view, selectedAgent: agentSelection(), onSaveAgentConfig: () => {} }]) {
      const markup = render({ state: hostileState, ...props });
      assert.equal(markup.includes("<img"), false, view);
      assert.equal(markup.includes("question"), false, view);
    }
  }
  assert.match(render({ state: hostileState }), /&lt;img src=x onerror=alert\(1\)&gt;/);
  assert.match(render({ state: hostileState, selectedAgent: agentSelection(), onSaveAgentConfig: () => {} }), /<input id="df-model-[0-9a-f]+" value="&lt;img src=x onerror=alert\(1\)&gt;"/);
});

test("the console never shows a kernel-grammar or retired vocabulary word", () => {
  const withDetail = {
    selectedHumanRequest: selectedRequest(),
    onHumanReplyChange: () => {},
    onReplyHumanRequest: () => {},
    onCancelHumanRequest: () => {},
    onCloseHumanRequest: () => {},
    onSelectAgent: () => {},
  };
  const terminalView = (overrides = {}) => createElement(TerminalPanel, {
    terminal: { agentId: "21".repeat(16), agentName: "Builder One", agentRevision: 10n, phase: "ready", writable: true, resets: 0, surfaceVersion: 0, ...overrides.terminal },
    onClose: () => {},
  }, createElement("div"));
  const surfaces = [
    ...VIEWS.map((view) => [view, render({ ...withDetail, view })]),
    ["agent", render({ view: "agents", selectedAgent: agentSelection(), onSaveAgentConfig: () => {} })],
    ["settings", render({ settingsOpen: true, onToggleSettings: () => {} })],
    ["terminal", renderToStaticMarkup(terminalView())],
    ["terminal-reset", renderToStaticMarkup(terminalView({ terminal: { resets: 1 } }))],
  ];
  // "overseer" left this list by owner decision: it is the console's word for
  // the orchestrator rank. Everything else is still kernel grammar. The lease
  // ban is anchored at a word start so it still catches lease/leased/leases
  // without catching the floor's "release-ready".
  for (const [name, markup] of surfaces) {
    for (const forbidden of [/attempt/i, /converge/i, /admission/i, /finalize/i, /unresolved/i, /proposal/i, /verdict/i, /\bALLOW\b/, /\bBLOCK\b/, /\blease/i, /intake/i, /quarantine/i, /work item/i, /cancel run/i]) {
      assert.equal(forbidden.test(markup), false, `${name}: ${forbidden}`);
    }
  }
  assert.match(render({ view: "agents" }), />OVERSEER</);
});

test("transitional session statuses have stable live labels and offer no factory action", () => {
  for (const status of ["idle", "connecting", "authenticating", "syncing", "closed"]) {
    const markup = render({ status, onSelectAgent: () => {}, onSelectHumanRequest: () => {}, onView: () => {}, onToggleSettings: () => {} });
    assert.match(markup, new RegExp(`>${status.toUpperCase()}<`));
    // Only the three local chrome controls are live before the factory is.
    const live = (markup.match(/<button(?![^>]*disabled)/g) ?? []).length;
    assert.equal(live, 3, status);
    assert.match(markup, /<button type="button" aria-pressed="false" disabled=""/);
  }
  const ready = render({ status: "ready" });
  assert.match(ready, /class="dfFactoryConsole__connection dfFactoryConsole__visuallyHidden"/);
  assert.match(ready, /role="status" aria-live="polite" aria-atomic="true"/);
});

test("closed and pairing-uncertain errors have no ineffective action", () => {
  for (const error of [new SessionError("connection", true), new SessionError("pairing_uncertain"), new ProtocolError("malformed")]) {
    const markup = render({ status: "closed", error, onSelectAgent: () => {}, onSelectHumanRequest: () => {}, onView: () => {}, onToggleSettings: () => {} });
    assert.match(markup, /role="alert"/);
    // The banner offers nothing to press, and the only live buttons on a
    // closed console are the three that change nothing in the factory.
    assert.equal((markup.match(/<button(?![^>]*disabled)/g) ?? []).length, 3);
    assert.doesNotMatch(markup, /role="alert"[^>]*>[^<]*<button/);
    assert.equal(markup.includes("RETRY CONNECTION"), false);
    assert.equal(markup.includes("Error:"), false);
    assert.equal(markup.includes("secret"), false);
  }
});

test("protocol errors remain finite and never expose their message", () => {
  const markup = render({ status: "closed", error: new ProtocolError("malformed") });
  assert.match(markup, /The server sent an invalid frame\./);
  assert.equal(markup.includes("malformed"), false);
});

test("unknown and inherited error codes use a finite fallback", () => {
  for (const code of ["__proto__", "constructor", "toString", "hasOwnProperty", ""]) {
    const markup = render({ status: "closed", error: { code } });
    assert.match(markup, /The connection could not continue\./);
    if (code !== "") assert.equal(markup.includes(code), false);
  }
});

test("empty and bounded right-column collections are explicit and capped", () => {
  const emptyState = baseState({ projects: new Map(), agents: new Map(), tasks: new Map(), humanRequests: new Map() });
  assert.match(render({ state: emptyState }), /all quiet — nothing needs you/);
  assert.match(render({ state: emptyState }), /the queue is empty/);
  assert.match(render({ state: emptyState, view: "agents" }), /no agents/);

  const agents = new Map();
  const tasks = new Map();
  const requests = new Map();
  for (let index = 0; index < 9; index += 1) {
    const suffix = String(index).padStart(2, "0");
    const agentID = `${suffix}${"21".repeat(15)}`;
    const taskID = `${suffix}${"31".repeat(15)}`;
    const requestID = `${suffix}${"41".repeat(15)}`;
    agents.set(agentID, { id: agentID, project_id: ids.project, name: `Agent ${index}`, role: "worker", provider: "codex", paused: false, model: "", reasoning_effort: "", revision: BigInt(index + 1) });
    tasks.set(taskID, { id: taskID, project_id: ids.project, assigned_agent_id: agentID, title: `Task ${index}`, status: "queued", priority: index, revision: BigInt(index + 1) });
    requests.set(requestID, { id: requestID, project_id: ids.project, agent_id: agentID, task_id: taskID, created_at: 1n, updated_at: 1n, revision: BigInt(index + 1), kind: "question", status: "open", reply_max_bytes: 8192, can_reply: true });
  }
  const bounded = baseState({ agents, tasks, humanRequests: requests });
  const markup = render({ state: bounded });
  assert.equal((markup.match(/class="dfFactoryConsole__card"/g) ?? []).length, 8);
  assert.equal((markup.match(/class="dfConsoleRow"/g) ?? []).length, 8);
  assert.equal((markup.match(/\+1 more/g) ?? []).length, 2, "both columns own their overflow");
  assert.match(markup, />9 ITEMS</);
  assert.match(markup, />9 open</);
  assert.equal((render({ state: bounded, view: "agents" }).match(/dfAgentList__row/g) ?? []).length, 0, "no handler, no button");
  assert.equal((render({ state: bounded, view: "agents", onSelectAgent: () => {} }).match(/dfAgentList__row/g) ?? []).length, 9);
});

test("stage meters fill only on store-backed stage", () => {
  const markup = render();
  assert.match(markup, /aria-label="stage: building"/);
  assert.match(markup, /aria-label="stage: queued"/);
  const buildingMeter = markup.split('aria-label="stage: building"')[1].split("</span></span>")[0];
  assert.equal((buildingMeter.match(/dfStageMeter__segment--filled/g) ?? []).length, 2);
  // Finished work has left the queue column entirely.
  assert.equal(markup.includes('aria-label="stage: done"'), false);
  assert.equal(markup.includes('aria-label="stage: failed"'), false);
});

test("a terminal blocked task is neither building nor current agent work", () => {
  const state = baseState({
    tasks: new Map([[ids.task, { ...fixtureState.tasks.get(ids.task), status: "blocked" }]]),
    humanRequests: new Map(),
  });
  const markup = render({ state, view: "agents" });
  assert.match(markup, /aria-label="Builder One: waiting"/);
  assert.equal(markup.includes('aria-label="Builder One: busy"'), false);
});

test("the production console exposes no speculative or unsupported surface", () => {
  for (const view of VIEWS) {
    const markup = render({ view }).toLowerCase();
    for (const text of ["not yet served", "awaiting deploy", "suggestions", "add work", "accept", "dismiss", "task record"]) {
      assert.equal(markup.includes(text), false, `${view}: ${text}`);
    }
  }
});

test("an unavailable snapshot is explicit and does not invent runtime state", () => {
  const markup = render({ state: undefined, status: "syncing" });
  assert.match(markup, /WAITING FOR SNAPSHOT/);
  assert.match(markup, /waiting for snapshot/);
  assert.match(markup, /<dt>ACTIVE RUNS<\/dt><dd>—<\/dd>/);
  assert.match(markup, /<dt>QUEUED<\/dt><dd>—<\/dd>/);
  assert.equal(markup.includes("the queue is empty"), false);
  assert.equal(markup.includes("all quiet"), false);
  assert.match(render({ state: undefined, status: "syncing", view: "agents" }), /waiting for the factory/);
});

test("HumanRequest delivery states remain visibly distinct", () => {
  const request = fixtureState.humanRequests.get(ids.request);
  for (const [status, label, description] of [
    ["open", "OPEN", "Awaiting your answer"],
    ["delivering", "DELIVERING", "Answer delivery in progress"],
    ["delivery_unknown", "DELIVERY UNKNOWN", "Answer delivery could not be confirmed"],
  ]) {
    const markup = render({ state: baseState({ humanRequests: new Map([[request.id, { ...request, status }]]) }) });
    assert.match(markup, new RegExp(`>${label}<`));
    assert.match(markup, new RegExp(`>${description}<`));
  }
});

test("the sidebar replaces the right column and the terminal owns it outright", () => {
  const css = readFileSync(new URL("../src/factory-console.css", import.meta.url), "utf8");
  assert.match(css, /\.dfConsoleRow__agent\s*\{[^}]*min-width: 0;[^}]*overflow-wrap: anywhere;/);
  assert.match(css, /\.dfConsoleShell\s*\{[^}]*display: flex;[^}]*align-items: flex-start;/);
  assert.match(css, /\.dfFactoryConsole__terminalPanel :where\(p\)\s*\{\s*margin: 0;/);
  assert.match(css, /\.dfConsoleLayout\s*\{[^}]*grid-template-columns: minmax\(0, 2fr\) minmax\(0, 1fr\);/);
  assert.match(css, /\.dfConsoleLayout--narrow \{ grid-template-columns: minmax\(0, 1fr\); \}/);
  assert.match(css, /\.dfConsoleSidebar\s*\{[^}]*flex: 0 0 clamp\(22rem, 40vw, 44rem\);[^}]*min-width: 0;/);
  // Every rule the console scopes to its own subtree names the sidebar too,
  // or the sidebar renders in the browser's default serif on the page ground.
  for (const rule of [/\.dfConsoleSidebar \*,/, /\.dfConsoleSidebar button \{/, /\.dfConsoleSidebar button:disabled/, /\.dfConsoleSidebar\s*\{[^}]*font-family: ui-monospace/, /\.dfConsoleSidebar\s*\{[^}]*color: var\(--df-console-text\)/]) {
    assert.match(css, rule);
  }
  assert.match(css, /@media \(max-width: 1024px\)[\s\S]*?\.dfConsoleShell \{ display: block; \}/);
  assert.match(css, /:focus-visible\s*\{\s*outline: 2px solid var\(--df-console-accent\);/);

  const quiet = render();
  assert.match(quiet, /dfConsoleLayout__right/);
  assert.equal(quiet.includes("dfConsoleSidebar"), false);

  const withTerminal = render({ terminalContent: createElement("section", { "aria-label": "Terminal sidebar" }) });
  assert.match(withTerminal, /<\/main><aside class="dfConsoleSidebar" aria-label="Selected detail"><section aria-label="Terminal sidebar"><\/section><\/aside><\/div>$/);
  assert.match(withTerminal, /dfConsoleLayout dfConsoleLayout--narrow/);
  assert.equal(withTerminal.includes("dfConsoleLayout__right"), false);
});

test("selecting an agent opens the agent sidebar with its config and queue", () => {
  const markup = render({
    view: "agents",
    selectedAgent: agentSelection(),
    onSelectAgent: () => {},
    onSaveAgentConfig: () => {},
    onEditTask: () => {},
    onOpenAgentTerminal: () => {},
    onCloseAgent: () => {},
  });
  assert.match(markup, /aria-label="Agent Builder One"/);
  assert.match(markup, />WORKER · claude_code</);
  assert.match(markup, /aria-label="NOW"[\s\S]*?Review the state projection/);
  assert.match(markup, /aria-label="Agent configuration"/);
  assert.match(markup, /value="claude-opus-5"/);
  assert.match(markup, /value="high"/);
  assert.match(markup, /aria-label="Agent queue"/);
  assert.match(markup, />OPEN TERMINAL</);
  // The whole right column is gone while the sidebar is open.
  assert.equal(markup.includes("dfConsoleLayout__right"), false);

  // Without handlers the sidebar is a readout, never a dead form.
  const readOnly = render({ selectedAgent: agentSelection() });
  assert.equal(readOnly.includes("<input"), false);
  assert.equal(readOnly.includes("OPEN TERMINAL"), false);
});

test("the queued task row edits title, order, assignment, and cancellation", async () => {
  const previousAct = globalThis.IS_REACT_ACT_ENVIRONMENT;
  globalThis.IS_REACT_ACT_ENVIRONMENT = true;
  try {
    const edits = [];
    const queued = fixtureState.tasks.get([...fixtureState.tasks.keys()][1]);
    const other = { ...queued, id: "39".repeat(16), title: "Second in line", priority: 3, revision: 20n };
    const state = baseState({
      tasks: new Map([[queued.id, { ...queued, assigned_agent_id: ids.agent }], [other.id, { ...other, assigned_agent_id: ids.agent }]]),
    });
    const props = {
      status: "ready",
      state,
      selectedAgent: agentSelection(),
      onSaveAgentConfig: (config) => edits.push(["config", config]),
      onEditTask: (task, change) => edits.push([task.id, change]),
    };
    let renderer;
    await act(async () => { renderer = create(createElement(FactoryConsole, props)); });
    const buttons = renderer.root.findAllByType("button");
    const byLabel = (label) => buttons.find((button) => button.props["aria-label"] === label);
    // Moving down takes the neighbour below's priority minus one.
    await act(async () => { byLabel(`Move ${queued.title} down`).props.onClick(); });
    assert.deepEqual(edits.at(-1), [queued.id, { priority: other.priority - 1 }]);
    // Moving up takes the neighbour above's priority plus one.
    await act(async () => { byLabel(`Move ${other.title} up`).props.onClick(); });
    assert.deepEqual(edits.at(-1), [other.id, { priority: queued.priority + 1 }]);
    assert.equal(byLabel(`Move ${queued.title} up`).props.disabled, true, "the first task cannot rise");
    assert.equal(byLabel(`Move ${other.title} down`).props.disabled, true, "the last task cannot fall");

    const title = renderer.root.findAllByType("input").find((input) => input.props.value === queued.title);
    await act(async () => { title.props.onChange({ currentTarget: { value: "Renamed" } }); });
    await act(async () => { title.props.onBlur(); });
    assert.deepEqual(edits.at(-1), [queued.id, { title: "Renamed" }]);

    const assign = renderer.root.findAllByType("select")[0];
    // Reassignment offers only agents in the same project.
    assert.deepEqual(assign.props.children.map((option) => option.props.children), ["Builder One", "Builder Two"]);
    await act(async () => { assign.props.onChange({ currentTarget: { value: "23".repeat(16) } }); });
    assert.deepEqual(edits.at(-1), [queued.id, { assignedAgentId: "23".repeat(16) }]);

    await act(async () => { buttons.filter((button) => button.props.children === "CANCEL")[0].props.onClick(); });
    assert.deepEqual(edits.at(-1), [queued.id, { cancel: true }]);

    // A refused rename reverts to the served title. A refusal never moves the
    // revision, so without this the input would resend it on the next blur.
    const titleValue = () => renderer.root.findAllByType("input").find((input) => input.props.id === `df-title-${queued.id}`).props.value;
    assert.equal(titleValue(), "Renamed");
    await act(async () => { renderer.update(createElement(FactoryConsole, { ...props, edit: { pending: false, error: new SessionError("stale") } })); });
    assert.equal(titleValue(), queued.title);
    await act(async () => { renderer.update(createElement(FactoryConsole, props)); });

    // A save carries only what changed. Sending an untouched model would make
    // the daemon revalidate it, so an agent whose stored pair it no longer
    // accepts could never be paused.
    const config = () => renderer.root.findAllByType("form")[0];
    const paused = renderer.root.findAllByType("input").find((input) => input.props.type === "checkbox");
    await act(async () => { paused.props.onChange({ currentTarget: { checked: true } }); });
    await act(async () => { config().props.onSubmit({ preventDefault() {} }); });
    assert.deepEqual(edits.at(-1), ["config", { paused: true }]);

    const model = renderer.root.findAllByType("input").find((input) => input.props.value === "claude-opus-5");
    await act(async () => { model.props.onChange({ currentTarget: { value: "claude-sonnet-5" } }); });
    await act(async () => { config().props.onSubmit({ preventDefault() {} }); });
    assert.deepEqual(edits.at(-1), ["config", { model: "claude-sonnet-5", paused: true }]);

    // Saving an untouched form is not a write at all.
    await act(async () => { model.props.onChange({ currentTarget: { value: "claude-opus-5" } }); });
    await act(async () => { paused.props.onChange({ currentTarget: { checked: false } }); });
    await act(async () => { config().props.onSubmit({ preventDefault() {} }); });
    assert.deepEqual(edits.at(-1), ["config", {}]);
    await act(async () => { renderer.unmount(); });
  } finally {
    globalThis.IS_REACT_ACT_ENVIRONMENT = previousAct;
  }
});

test("a rejected edit says plainly that the durable value did not change", () => {
  const markup = render({
    selectedAgent: agentSelection(),
    onSaveAgentConfig: () => {},
    edit: { pending: false, error: new SessionError("stale") },
  });
  assert.match(markup, /SOMEONE ELSE CHANGED THIS — REOPEN IT AND TRY AGAIN/);
  assert.match(markup, /role="alert"/);
  const unknown = render({ selectedAgent: agentSelection(), onSaveAgentConfig: () => {}, edit: { pending: false, error: { code: "internal" } } });
  assert.match(unknown, /THE EDIT DID NOT COMPLETE/);
  assert.match(render({ selectedAgent: agentSelection(), onSaveAgentConfig: () => {}, edit: { pending: true } }), />SAVING</);
});

test("the settings sidebar carries the factory readout and a pairing mount point", () => {
  const markup = render({ settingsOpen: true, onToggleSettings: () => {} });
  assert.match(markup, /aria-label="Settings"/);
  assert.match(markup, /aria-label="BUILDING"/);
  assert.match(markup, /<dt>DISPATCH<\/dt><dd>ENABLED<\/dd>/);
  assert.match(markup, /<dt>REVISION<\/dt><dd>42<\/dd>/);
  assert.match(markup, /127\.0\.0\.1:43123/);
  assert.match(markup, /aria-label="PAIRING"/);
  assert.match(markup, /phone pairing arrives here/);
  // The peer PR drops its own component into the same slot.
  const paired = render({ settingsOpen: true, onToggleSettings: () => {}, pairing: createElement("p", null, "PAIR A PHONE") });
  assert.match(paired, /PAIR A PHONE/);
  assert.equal(paired.includes("phone pairing arrives here"), false);
  // A selected agent outranks settings: only one sidebar is ever open.
  const both = render({ settingsOpen: true, onToggleSettings: () => {}, selectedAgent: agentSelection() });
  assert.equal(both.includes('aria-label="Settings"'), false);
  assert.match(both, /aria-label="Agent Builder One"/);
});

test("FactoryApp server-renders without reading browser globals", () => {
  const markup = renderToStaticMarkup(createElement(FactoryApp));
  assert.match(markup, /Factory operator console/);
  assert.match(markup, />IDLE</);
  assert.match(markup, /WAITING FOR SNAPSHOT/);
});

test("selected hostile private detail is escaped and actions remain semantic", () => {
  const hostile = "<script>steal(authority)</script>";
  const markup = render({
    selectedHumanRequest: selectedRequest({ question: hostile, reply: "<reply>" }),
    onHumanReplyChange: () => {},
    onReplyHumanRequest: () => {},
    onCancelHumanRequest: () => {},
    onCloseHumanRequest: () => {},
  });
  assert.match(markup, /aria-label="Selected question"/);
  assert.match(markup, /aria-label="Answer this question"/);
  // The decision card is a sidebar panel, headed like the agent panel.
  assert.match(markup, /<article class="dfConsoleSidebar__panel dfFactoryConsole__humanRequest"/);
  assert.match(markup, /<div class="dfConsoleSidebar__heading"><div><p class="dfFactoryConsole__eyebrow">North Workshop · Review the state projection<\/p><h2>Builder One needs you<\/h2><\/div><button type="button">CLOSE<\/button>/);
  assert.match(markup, /&lt;script&gt;steal\(authority\)&lt;\/script&gt;/);
  assert.equal(markup.includes("<script>"), false);
  assert.match(markup, /<textarea[^>]*>&lt;reply&gt;<\/textarea>/);
  assert.match(markup, />ANSWER</);
  assert.match(markup, />Stop</);
  assert.equal(markup.includes("expectedRunRevision"), false);
});

test("request, reply, cancel, and close controls forward only presentation intent", () => {
  const request = fixtureState.humanRequests.get(ids.request);
  const calls = [];
  const baseProps = {
    status: "ready",
    state: baseState(),
    onSelectHumanRequest: (value) => calls.push(["select", value]),
    onHumanReplyChange: (value) => calls.push(["change", value]),
    onReplyHumanRequest: () => calls.push(["reply"]),
    onCancelHumanRequest: () => calls.push(["cancel"]),
    onCloseHumanRequest: () => calls.push(["close"]),
  };

  const requestElements = expand(FactoryConsole(baseProps));
  requestElements.find((element) => element.type === "button" && element.props.children === "VIEW").props.onClick();
  assert.equal(calls[0][0], "select");
  assert.equal(calls[0][1], request);

  const selectedElements = expand(FactoryConsole({ ...baseProps, selectedHumanRequest: selectedRequest() }));
  selectedElements.find((element) => element.type === "textarea").props.onChange({ currentTarget: { value: "Proceed." } });
  let prevented = false;
  selectedElements.find((element) => element.type === "form").props.onSubmit({ preventDefault: () => { prevented = true; } });
  selectedElements.find((element) => element.type === "button" && element.props.children === "Stop").props.onClick();
  selectedElements.find((element) => element.type === "button" && element.props.children === "CLOSE").props.onClick();
  assert.equal(prevented, true);
  assert.deepEqual(calls.slice(1), [["change", "Proceed."], ["reply"], ["cancel"], ["close"]]);
});

test("agent and question terminal actions expose only current public intent", () => {
  const request = fixtureState.humanRequests.get(ids.request);
  // Oversight is listed first, so the first row is the orchestrator's.
  const agent = fixtureState.agents.get(ids.orchestrator);
  const calls = [];
  const elements = expand(FactoryConsole({
    status: "ready",
    state: baseState(),
    view: "agents",
    selectedHumanRequest: selectedRequest(),
    onSelectAgent: (value) => calls.push(["agent", value]),
    onOpenTerminalForHumanRequest: (value) => calls.push(["request", value]),
  }));
  const row = elements.find((element) => element.type === "button" && typeof element.props.className === "string" && element.props.className.includes("dfAgentList__row"));
  row.props.onClick();
  elements.filter((element) => element.type === "button" && element.props.children === "OPEN TERMINAL").at(-1).props.onClick();
  assert.equal(calls[0][0], "agent");
  assert.equal(calls[0][1].id, agent.id);
  assert.equal(calls[0][1].revision, agent.revision);
  assert.deepEqual(calls[1], ["request", request]);

  const markup = render({ terminalContent: createElement("div", null, "<raw-output>") });
  assert.match(markup, /&lt;raw-output&gt;/);
  assert.equal(markup.includes("runId"), false);
  assert.equal(markup.includes("sessionId"), false);
});

test("the view toggle and settings forward exactly one intent each", () => {
  const calls = [];
  const elements = expand(FactoryConsole({
    status: "ready",
    state: baseState(),
    onView: (value) => calls.push(["view", value]),
    onToggleSettings: () => calls.push(["settings"]),
  }));
  const chrome = elements.filter((element) => element.type === "button" && element.props.disabled !== true);
  assert.deepEqual(chrome.map((element) => element.props.children), ["FLOOR", "AGENTS", "SETTINGS"]);
  for (const button of chrome) button.props.onClick();
  assert.deepEqual(calls, [["view", "floor"], ["view", "agents"], ["settings"]]);
});

function expand(node, result = []) {
  if (Array.isArray(node)) {
    for (const child of node) expand(child, result);
    return result;
  }
  if (!isValidElement(node)) return result;
  if (typeof node.type === "function") {
    expand(node.type(node.props), result);
    return result;
  }
  result.push(node);
  expand(node.props.children, result);
  return result;
}

test("PAIR A PHONE appears in settings only with authority, and shows the minted code", () => {
  const svg = '<svg viewBox="0 0 1 1"/>';
  const link = "https://app.darkfactory.build/remote#df_remote&node=n0&expires=1767225600";
  const settings = { settingsOpen: true, onToggleSettings: () => {} };
  // Pairing lives in the settings sidebar, which is the only place it shows.
  assert.equal(render({ ...settings }).includes("PAIR A PHONE"), false);
  assert.match(render({ ...settings, remoteInviteAllowed: true }), /PAIR A PHONE/);
  assert.equal(render({ remoteInviteAllowed: true }).includes("PAIR A PHONE"), false, "not without settings");
  assert.equal(render({ ...settings, remoteInviteAllowed: true, selectedAgent: agentSelection() }).includes("PAIR A PHONE"), false, "not behind another sidebar");
  // Its slot still takes an explicit override.
  assert.match(render({ ...settings, remoteInviteAllowed: true, pairing: createElement("p", null, "OTHER") }), /OTHER/);

  const shown = render({ ...settings, remoteInviteAllowed: true, remoteInvite: { link, svg, expiresAtMs: 1767225600000n } });
  assert.ok(shown.includes(`src="data:image/svg+xml;utf8,${encodeURIComponent(svg)}"`), shown);
  assert.ok(shown.includes(link.replaceAll("&", "&amp;")));
  assert.match(shown, /DISMISS/);
  // The minted code is never handed to the browser as markup. The floor draws
  // its own SVG, so the check names the invite's exact bytes.
  assert.equal(shown.includes(svg), false);

  const failed = render({ ...settings, remoteInviteAllowed: true, remoteInviteError: "not_found" });
  assert.match(failed, /NO PAIRING CODE — NOT FOUND/);
  assert.match(failed, /DISMISS/);
  assert.equal(render({ ...settings, remoteInviteAllowed: true }).includes("DISMISS"), false);
});
