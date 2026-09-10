import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import test from "node:test";
import { createElement, isValidElement, useEffect, useState } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { act, create } from "react-test-renderer";
import { MAX_TASK_PRIORITY, ProtocolError, SessionError } from "@dark-factory/client";
import { FactoryApp, FactoryConsole, floorScene } from "../dist/src/index.js";
import { TerminalPanel } from "../dist/src/factory-app.js";
import { fixtureState, fixtureTopologies, fixtureTopology } from "../../../fixtures/state.mjs";

const ids = {
  project: [...fixtureState.projects.keys()][0],
  secondProject: [...fixtureState.projects.keys()][1],
  agent: [...fixtureState.agents.keys()][0],
  orchestrator: [...fixtureState.agents.keys()][1],
  idleAgent: [...fixtureState.agents.keys()][2],
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

const runSample = (agentId, paths, taskId = ids.task, taskRevision = fixtureState.tasks.get(taskId)?.revision ?? 1n) => ({
  taskId,
  taskRevision,
  runId: "71".repeat(16),
  projectId: fixtureState.agents.get(agentId).project_id,
  paths,
});

test("error banner keeps its centered layout after the paragraph reset", () => {
  const css = readFileSync(new URL("../src/factory-console.css", import.meta.url), "utf8");
  assert.match(css, /\.dfFactoryConsole :where\(h1, h2, p, dl, ul\),[\s\S]*?\.dfConsoleSidebar :where\(h1, h2, h3, p, dl, ul\)\s*\{\s*margin: 0;\s*\}/);
  assert.match(css, /\.dfFactoryConsole__error\s*\{[\s\S]*?margin: 0 auto 1\.25rem;/);
  assert.match(css, /\.dfFactoryScene__room--empty > rect,[\s\S]*?\.dfFactoryScene__room--empty > use \{ opacity: 0\.45; \}/);
  assert.match(css, /\.dfFactoryFloor \{ overflow-x: auto; \}/);
  assert.match(css, /\.dfFactoryScene__worker \{ transition: transform 180ms ease-out; \}/);
  assert.match(css, /@media \(prefers-reduced-motion: reduce\)[\s\S]*?\.dfFactoryScene__worker \{ transition: none; \}/);
  assert.equal(css.includes("@keyframes dfFactoryScene"), false);
});

test("one screen keeps Factory and the operator panels together", () => {
  const markup = render();
  assert.match(markup, /<main class="dfFactoryConsole" aria-label="Factory operator console">/);
  for (const label of ["Factory counters", "Factory floor", "Selected detail", "NEEDS YOU", "Left view", "Right panel"]) {
    assert.match(markup, new RegExp(`aria-label="${label}"`));
  }
  // Counters read the served factory, not a second count of it.
  assert.match(markup, /<dt>ACTIVE RUNS<\/dt><dd>2 \/ 8 WORKERS \+ 1 OVERSEER<\/dd>/);
  assert.match(markup, /<dt>QUEUED<\/dt><dd>1<\/dd>/);
  assert.match(markup, /<dt>NEEDS YOU<\/dt><dd>1<\/dd>/);
  assert.match(markup, /Builder One asks/);
  assert.match(markup, /Review the state projection/);
  assert.match(markup, /North Workshop · Review the state projection/);
  assert.match(render({ detail: "queue" }), /aria-label="Queue"/);
  // No screen union survives: there is no navigation away from this screen.
  assert.equal(markup.includes("dfFactoryConsole__homeLink"), false);
  assert.equal(markup.includes("BUILDING STATE UNAVAILABLE"), false);
});

test("the roster stays visible while the optional floor opens and closes", () => {
  const floor = render();
  assert.match(floor, /aria-label="Dark Factory codebase floor"/);

  const agents = render({ view: "agents" });
  assert.match(agents, /aria-label="Agents"/);
  assert.match(agents, /aria-label="OVERSEER"/);
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
  const scene = floorScene(fixtureState, fixtureTopologies);
  // The repository root and the code it holds become rooms, largest first, and
  // every project keeps its own room before any project keeps a second. The
  // root room carries the project's name: the served one names no project.
  assert.deepEqual(scene.topology.nodes.map((node) => node.label), ["North Workshop", "South Workshop", "kernel", "web"]);
  assert.equal(scene.topology.nodes.some((node) => node.label === fixtureTopology.nodes[0].label), false);
  assert.deepEqual(scene.topology.nodes.map((node) => node.sizeBucket), ["large", undefined, "medium", "small"]);
  // Every room names its project, the heading it stands under on the floor.
  assert.deepEqual(scene.topology.nodes.map((node) => node.project.name), ["North Workshop", "South Workshop", "North Workshop", "North Workshop"]);
  assert.deepEqual(scene.topology.nodes.map((node) => node.project.id), [ids.project, ids.secondProject, ids.project, ids.project]);
  assert.equal(scene.topology.digest, fixtureTopology.digest);
  assert.deepEqual(scene, floorScene(fixtureState, new Map([[fixtureTopology.projectId, { ...fixtureTopology, nodes: [...fixtureTopology.nodes] }]])));

  // No changed-path sample puts a worker in a project room: idle and paused
  // agents rest, and a live run says that its location is not yet observed.
  const room = (path) => scene.topology.nodes.find((node) => node.path === path).id;
  const root = room(".");
  assert.deepEqual(scene.workers.map((worker) => [worker.name, worker.activity, worker.location, worker.nodeId]), [
    ["Builder One", "needs-you", "unobserved", undefined],
    ["Dispatch Lead", "idle", "resting", undefined],
    ["Builder Two", "waiting", "resting", undefined],
  ]);

  // A live run stands its worker in the room of the code it is changing: the
  // deepest displayed room that prefixes a path, and the room most paths sit in.
  const kernel = room("internal/kernel");
  const web = room("web");
  const placed = (paths) => floorScene(fixtureState, fixtureTopologies, new Map([[ids.agent, runSample(ids.agent, paths)]]))
    .workers.find((worker) => worker.id === ids.agent).nodeId;
  assert.equal(placed(["internal/kernel/store"]), kernel);
  assert.equal(placed(["web"]), web);
  // The repository root is a room like any other.
  assert.equal(placed(["."]), root);
  assert.equal(placed(["cmd/factoryd"]), root);
  // Several paths: the room holding the most of them, ties by sorted path.
  assert.equal(placed(["web", "internal/kernel", "internal/kernel/store"]), kernel);
  assert.equal(placed(["web", "internal/kernel"]), kernel);
  assert.equal(placed(["web", "web/packages/ui", "internal/kernel"]), web);
  // Empty or missing live samples never invent a root location.
  assert.equal(placed([]), undefined);
  assert.equal(floorScene(fixtureState, fixtureTopologies, new Map()).workers.find((worker) => worker.id === ids.agent).location, "unobserved");
  // A stopped agent retains a text-only last observation in the resting area.
  const observed = floorScene(fixtureState, fixtureTopologies, undefined, new Map([[ids.idleAgent, runSample(ids.idleAgent, ["web"], "72".repeat(16))]]))
    .workers.find((worker) => worker.id === ids.idleAgent);
  assert.deepEqual([observed.location, observed.nodeId, observed.locationLabel], ["last-observed", undefined, "web"]);

  // A prior run cannot place its agent after a replacement task has started.
  const replacement = { ...fixtureState.tasks.get(ids.task), revision: 2n };
  const replacementState = { ...fixtureState, tasks: new Map([[replacement.id, replacement]]) };
  const stale = floorScene(replacementState, fixtureTopologies, new Map([[ids.agent, runSample(ids.agent, ["web"])]]))
    .workers.find((worker) => worker.id === ids.agent);
  assert.deepEqual([stale.location, stale.nodeId], ["unobserved", undefined]);
  const current = floorScene(replacementState, fixtureTopologies, new Map([[ids.agent, runSample(ids.agent, ["web"], replacement.id, replacement.revision)]]))
    .workers.find((worker) => worker.id === ids.agent);
  assert.deepEqual([current.location, current.nodeId], ["working", web]);

  // Without topology the floor still has a room per project.
  const fallback = floorScene(fixtureState, undefined);
  assert.deepEqual(fallback.topology.nodes.map((node) => node.label), ["North Workshop", "South Workshop"]);
  assert.deepEqual(fallback.topology.nodes.map((node) => node.sizeBucket), [undefined, undefined]);
  assert.deepEqual(fallback.workers.map((worker) => worker.nodeId), [undefined, undefined, undefined]);
  assert.deepEqual(floorScene(undefined, undefined), { topology: { digest: "", nodes: [] }, workers: [], omittedLocations: 0 });
  // The size bucket, not a file count, is what the room subtitle carries.
  const markup = render({ topologies: fixtureTopologies });
  assert.match(markup, />kernel<\/text>/);
  assert.match(markup, />PACKAGE · MEDIUM<\/text>/);
  assert.equal(markup.includes("FILES"), false);

  // A wide root must not crowd the other projects off the floor: the cap eats
  // the root's smallest children, never a project room, so no agent loses its.
  const topRoom = fixtureTopology.nodes[0];
  const wide = floorScene(fixtureState, served({
    ...fixtureTopology,
    nodes: [topRoom, ...Array.from({ length: 30 }, (_, index) => ({
      id: `${index}`.padStart(64, "0"), parent_id: topRoom.id, kind: "directory",
      path: `dir-${index}`, label: `dir-${index}`, language: "", size_bucket: index < 4 ? "medium" : "tiny",
    }))],
  }));
  assert.equal(wide.topology.nodes.length, 24);
  assert.equal(wide.topology.nodes.filter((node) => node.path === ".").length, 1);
  assert.equal(wide.topology.nodes.some((node) => node.id === ids.secondProject), true);
  assert.deepEqual(wide.topology.nodes.filter((node) => node.sizeBucket === "medium").map((node) => node.label),
    ["dir-0", "dir-1", "dir-2", "dir-3"]);
  assert.equal(wide.workers.find((worker) => worker.id === ids.agent).location, "unobserved");
});

/**
 * A served structure for any tree at all: each entry is [kind, path, size] with
 * its parent given by index, so a repository, a module and a package can all
 * sit at "." exactly as the daemon serves them.
 */
function topologyFor(projectId, entries) {
  return {
    projectId,
    digest: `${projectId}`.slice(0, 64),
    sourceRevision: "",
    nodes: entries.map(([kind, path, sizeBucket, parent], index) => ({
      id: `${index}`.padStart(64, "0"),
      parent_id: parent === undefined ? "" : `${parent}`.padStart(64, "0"),
      kind,
      path,
      label: path === "." ? "root" : path.slice(path.lastIndexOf("/") + 1),
      language: "",
      size_bucket: sizeBucket,
    })),
  };
}

const served = (...views) => new Map(views.map((view) => [view.projectId, view]));

const PROJECT_NAME = "Any Project";

const soloState = (projectId) => ({
  ...fixtureState,
  projects: new Map([[projectId, { id: projectId, name: PROJECT_NAME, revision: 1n }]]),
  agents: new Map(),
  tasks: new Map(),
  humanRequests: new Map(),
});

test("the floor is whatever the daemon served, with no shape or name known to the code", () => {
  const state = soloState(ids.project);
  const labels = (...entries) => floorScene(state, served(topologyFor(ids.project, entries)))
    .topology.nodes.map((node) => node.label);

  // A go.mod at the root: repository "." holds a module at ".", and the rooms
  // are the module's packages, never the module standing in for the whole tree.
  assert.deepEqual(labels(
    ["repository", ".", "large"],
    ["module", ".", "large", 0],
    ["package", "gamma", "medium", 1],
    ["directory", "delta", "small", 1],
  ), [PROJECT_NAME, "gamma", "delta"]);

  // A package.json at the root: the same, one node deeper.
  assert.deepEqual(labels(
    ["repository", ".", "large"],
    ["package", ".", "large", 0],
    ["directory", "one", "medium", 1],
    ["directory", "two", "small", 1],
  ), [PROJECT_NAME, "one", "two"]);

  // A tree in no language the daemon analyses is directories and nothing else,
  // and size, not order of service, decides which rooms are shown first.
  assert.deepEqual(labels(
    ["repository", ".", "large"],
    ["directory", "docs", "small", 0],
    ["directory", "src", "medium", 0],
  ), [PROJECT_NAME, "src", "docs"]);

  // Adding, renaming and removing a directory moves the floor with it.
  const tree = (...names) => labels(["repository", ".", "large"], ...names.map((name) => ["directory", name, "medium", 0]));
  assert.deepEqual(tree("one", "two"), [PROJECT_NAME, "one", "two"]);
  assert.deepEqual(tree("one", "renamed"), [PROJECT_NAME, "one", "renamed"]);
  assert.deepEqual(tree("one"), [PROJECT_NAME, "one"]);
  assert.deepEqual(tree("added", "one"), [PROJECT_NAME, "added", "one"]);

  // Past the cap the largest rooms are the ones worth a tile.
  const many = floorScene(state, served(topologyFor(ids.project, [
    ["repository", ".", "large"],
    ...Array.from({ length: 40 }, (_, index) => ["directory", `dir-${index}`, index < 10 ? "large" : "tiny", 0]),
  ])));
  assert.equal(many.topology.nodes.length, 24);
  assert.equal(many.topology.nodes.filter((node) => node.sizeBucket === "large").length, 11);
});

test("every project is its own block of rooms and its own workers", () => {
  // Two projects holding the same path are served the same node id; two rooms
  // on one floor may not share one, and a worker may not walk into the other
  // project's code. A paused worker stays in resting rather than occupying its
  // project's latest room.
  const shape = [["repository", ".", "large"], ["directory", "src", "medium", 0]];
  const topologies = served(topologyFor(ids.project, shape), topologyFor(ids.secondProject, shape));
  const scene = floorScene(fixtureState, topologies, new Map([[ids.agent, runSample(ids.agent, ["src"])], [ids.orchestrator, runSample(ids.orchestrator, ["src"])]]));
  assert.equal(scene.topology.nodes.length, 4);
  assert.equal(new Set(scene.topology.nodes.map((node) => node.id)).size, 4);
  const nodeOf = (agentId) => scene.workers.find((worker) => worker.id === agentId).nodeId;
  assert.notEqual(nodeOf(ids.agent), undefined);
  assert.equal(nodeOf(ids.orchestrator), undefined);
  assert.equal(scene.topology.digest, [...topologies.values()].map((view) => view.digest).join(" "));
  // Two structures have arrived and neither root room reads as the served
  // label: on a floor of many projects only the project name tells them apart.
  assert.deepEqual(scene.topology.nodes.filter((node) => node.path === ".").map((node) => node.label),
    [fixtureState.projects.get(ids.project).name, fixtureState.projects.get(ids.secondProject).name]);

  // More projects than the floor can detail: every one keeps its own room
  // before any one keeps a second, and none of them is served first.
  const projects = Array.from({ length: 8 }, (_, index) => ({ id: `${index}`.padStart(32, "b"), name: `Project ${index}`, revision: 1n }));
  const crowd = floorScene(
    { ...fixtureState, projects: new Map(projects.map((project) => [project.id, project])), agents: new Map(), tasks: new Map() },
    served(...projects.map((project) => topologyFor(project.id, [
      ["repository", ".", "large"],
      ...Array.from({ length: 10 }, (_, index) => ["directory", `dir-${index}`, "medium", 0]),
    ]))),
  );
  assert.equal(crowd.topology.nodes.length, 24);
  assert.equal(crowd.topology.nodes.filter((node) => node.path === ".").length, projects.length);

  // Past one room per project the floor cannot detail them all. The projects
  // the cap reaches keep their own room; an agent whose project it never
  // reached stands off the floor rather than in another project's room.
  const overflow = Array.from({ length: 30 }, (_, index) => ({ id: `${index}`.padStart(32, "d"), name: `Project ${index}`, revision: 1n }));
  const posted = [overflow[0], overflow.at(-1)].map((project, index) => ({
    id: `${index}`.padStart(32, "e"), project_id: project.id, name: project.name,
    role: "worker", provider: "shell", paused: false, model: "", reasoning_effort: "", revision: 1n,
  }));
  const stranded = floorScene(
    {
      ...fixtureState,
      projects: new Map(overflow.map((project) => [project.id, project])),
      agents: new Map(posted.map((agent) => [agent.id, agent])),
      tasks: new Map(),
      humanRequests: new Map(),
    },
    served(...overflow.map((project) => topologyFor(project.id, [["repository", ".", "large"], ["directory", "src", "medium", 0]]))),
  );
  assert.deepEqual(stranded.topology.nodes.map((node) => node.path), Array.from({ length: 24 }, () => "."));
  assert.equal(stranded.workers[0].location, "resting");
  assert.equal(stranded.workers[0].nodeId, undefined);
  assert.equal(stranded.workers.at(-1).nodeId, undefined);
});

test("hostile names and titles are escaped as text and private detail is absent", () => {
  const hostile = "<img src=x onerror=alert(1)>";
  const hostileState = baseState({
    agents: new Map([[ids.agent, { id: ids.agent, project_id: ids.project, name: hostile, role: "worker", provider: "claude_code", paused: false, model: hostile, reasoning_effort: "", effective_model: hostile, effective_reasoning_effort: "", model_source: "agent", revision: 10n }]]),
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
  assert.match(render({ state: hostileState, selectedAgent: agentSelection(), onSaveAgentConfig: () => {} }), /<input id="df-model-[0-9a-f]+"[^>]*value="&lt;img src=x onerror=alert\(1\)&gt;"/);
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
    // SETTINGS is the only button before the factory is ready; the floor is a
    // native disclosure, not a factory action.
    const live = (markup.match(/<button(?![^>]*disabled)/g) ?? []).length;
    assert.equal(live, 1, status);
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
    // closed console is SETTINGS, which changes nothing in the factory.
    assert.equal((markup.match(/<button(?![^>]*disabled)/g) ?? []).length, 1);
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

test("Needs You and Queue keep every served item reachable", () => {
  const emptyState = baseState({ projects: new Map(), agents: new Map(), tasks: new Map(), humanRequests: new Map() });
  assert.match(render({ state: emptyState }), /all quiet — nothing needs you/);
  assert.match(render({ state: emptyState, detail: "queue" }), /THE QUEUE IS EMPTY/);
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
  assert.equal((markup.match(/<details class="dfConsoleItem"/g) ?? []).length, 9);
  const queue = render({ state: bounded, detail: "queue" });
  assert.equal((queue.match(/<li class="dfConsoleItem"/g) ?? []).length, 9);
  assert.equal((markup.match(/\+1 more/g) ?? []).length, 0);
  assert.match(markup, />9 ITEMS</);
  assert.match(queue, /Task 8/);
  assert.equal((render({ state: bounded, view: "agents" }).match(/dfAgentList__row/g) ?? []).length, 0, "no handler, no button");
  assert.equal((render({ state: bounded, view: "agents", onSelectAgent: () => {} }).match(/dfAgentList__row/g) ?? []).length, 9);
});

test("Queue keeps running tasks visible without a second queue", () => {
  const markup = render({ detail: "queue" });
  assert.match(markup, /aria-label="Running tasks"/);
  assert.match(markup, /Review the state projection/);
  assert.equal((markup.match(/aria-label="Queue"/g) ?? []).length, 1, "one queue panel");
});

test("a terminal blocked task is neither building nor current agent work", () => {
  const state = baseState({
    tasks: new Map([[ids.task, { ...fixtureState.tasks.get(ids.task), status: "blocked" }]]),
    humanRequests: new Map(),
  });
  const markup = render({ state, view: "agents" });
  assert.match(markup, /aria-label="Builder One: ready"/);
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
  assert.match(markup, /WAITING FOR SNAPSHOT/);
  assert.match(markup, /<dt>ACTIVE RUNS<\/dt><dd>—<\/dd>/);
  assert.match(markup, /<dt>QUEUED<\/dt><dd>—<\/dd>/);
  assert.equal(markup.includes("THE QUEUE IS EMPTY"), false);
  assert.equal(markup.includes("all quiet"), false);
  assert.match(render({ state: undefined, status: "syncing", view: "agents" }), /waiting for the factory/);
});

test("HumanRequest delivery states remain visibly distinct", () => {
  const request = fixtureState.humanRequests.get(ids.request);
  for (const [status, label] of [
    ["open", "OPEN"],
    ["delivering", "DELIVERING"],
    ["delivery_unknown", "DELIVERY UNKNOWN"],
  ]) {
    const markup = render({ state: baseState({ humanRequests: new Map([[request.id, { ...request, status }]]) }) });
    assert.match(markup, new RegExp(`>${label} ·`));
  }
});

test("the two-column console keeps one mounted terminal slot", () => {
  const css = readFileSync(new URL("../src/factory-console.css", import.meta.url), "utf8");
  assert.match(css, /\.dfConsoleRow__agent\s*\{[^}]*min-width: 0;[^}]*overflow-wrap: anywhere;/);
  assert.match(css, /\.dfFactoryConsole__terminalPanel :where\(p\)\s*\{\s*margin: 0;/);
  assert.match(css, /\.dfConsoleRow\s*\{[^}]*flex-wrap: wrap;/);
  assert.match(css, /\.dfConsoleLayout\s*\{[^}]*grid-template-columns: minmax\(0, 2fr\) minmax\(0, 1fr\);/);
  assert.match(css, /\.dfFactoryConsole__instructionActions\s*\{[^}]*display: flex;[^}]*flex-wrap: wrap;/);
  for (const rule of [/\.dfConsoleDialog \*/, /\.dfConsoleDialog button,/, /\.dfConsoleDialog button:disabled,[\s\S]*?\{/, /\.dfConsoleDialog\s*\{[^}]*font-family: ui-monospace/, /\.dfConsoleDialog\s*\{[^}]*color: var\(--df-console-text\)/]) {
    assert.match(css, rule);
  }
  // The panel scrolls, never the <dialog>: a scrollbar click on the dialog
  // itself has event.target === the dialog and would close SETTINGS.
  assert.match(css, /\.dfConsoleDialog \.dfConsoleSidebar__panel \{[^}]*max-height:[^}]*overflow: auto;/);
  assert.equal(/\.dfConsoleDialog\s*\{[^}]*overflow: auto/.test(css), false);
  assert.match(css, /:focus-visible\s*\{\s*outline: 2px solid var\(--df-console-accent\);/);

  const withTerminal = render({ selectedAgent: agentSelection(), terminalContent: createElement("section", { "aria-label": "Agent terminal" }) });
  assert.match(withTerminal, /<h1>DARK FACTORY<\/h1>/);
  assert.match(withTerminal, /dfConsoleLayout__left[\s\S]*?dfConsoleSidebar/);
  assert.match(withTerminal, /dfConsoleSidebar__terminalSlot" aria-label="Terminal"><section aria-label="Agent terminal"><\/section>/);
  assert.equal(withTerminal.includes("dfConsoleLayout__right"), false);
});

test("switching right panels keeps the selected terminal mounted", async () => {
  const previousAct = globalThis.IS_REACT_ACT_ENVIRONMENT;
  globalThis.IS_REACT_ACT_ENVIRONMENT = true;
  let mounted = 0;
  let unmounted = 0;
  function TerminalProbe() {
    useEffect(() => { mounted += 1; return () => { unmounted += 1; }; }, []);
    return createElement("section", { "aria-label": "Agent terminal" }, "terminal");
  }
  try {
    const props = { status: "ready", state: baseState(), selectedAgent: agentSelection(), terminalContent: createElement(TerminalProbe) };
    let renderer;
    await act(async () => { renderer = create(createElement(FactoryConsole, { ...props, detail: "agent" })); });
    await act(async () => { renderer.update(createElement(FactoryConsole, { ...props, detail: "queue" })); });
    await act(async () => { renderer.update(createElement(FactoryConsole, { ...props, detail: "needs-you" })); });
    assert.equal(mounted, 1);
    assert.equal(unmounted, 0);
    await act(async () => { renderer.unmount(); });
    assert.equal(unmounted, 1);
  } finally {
    globalThis.IS_REACT_ACT_ENVIRONMENT = previousAct;
  }
});

test("opening a selected question restores that agent's terminal panel", async () => {
  const previousAct = globalThis.IS_REACT_ACT_ENVIRONMENT;
  globalThis.IS_REACT_ACT_ENVIRONMENT = true;
  function ConsoleHarness() {
    const [detail, setDetail] = useState("agent");
    const [panel, setPanel] = useState("terminal");
    return createElement(FactoryConsole, {
      status: "ready",
      state: baseState(),
      selectedAgent: agentSelection(),
      selectedHumanRequest: selectedRequest(),
      detail,
      onDetail: setDetail,
      agentPanel: panel,
      onAgentPanel: setPanel,
      onOpenTerminalForHumanRequest: () => { setDetail("agent"); setPanel("terminal"); },
      terminalContent: createElement("p", null, "terminal"),
    });
  }
  try {
    let renderer;
    await act(async () => { renderer = create(createElement(ConsoleHarness)); });
    await act(async () => { renderer.root.findAllByType("button").find((button) => button.props.children === "CONFIG").props.onClick(); });
    await act(async () => { renderer.root.findAllByType("button").find((button) => button.props.children === "NEEDS YOU").props.onClick(); });
    await act(async () => { renderer.root.findAllByType("button").find((button) => button.props.children === "OPEN TERMINAL").props.onClick(); });
    const terminal = renderer.root.findByProps({ "aria-label": "Terminal" });
    const config = renderer.root.findByProps({ "aria-label": "Agent configuration" });
    assert.equal(terminal.props.hidden, false);
    assert.equal(config.props.hidden, true);
    await act(async () => { renderer.unmount(); });
  } finally {
    globalThis.IS_REACT_ACT_ENVIRONMENT = previousAct;
  }
});

test("selecting an agent exposes terminal and configuration controls", () => {
  const markup = render({
    view: "agents",
    selectedAgent: agentSelection(),
    onSelectAgent: () => {},
    onSaveAgentConfig: () => {},
    onEditTask: () => {},
  });
  assert.match(markup, /aria-label="Agent Builder One"/);
  assert.match(markup, /Builder One[\s\S]*?claude_code · claude-opus-5/);
  assert.match(markup, /aria-label="Agent controls"/);
  assert.match(markup, />TERMINAL</);
  assert.match(markup, />CONFIG</);
  assert.match(markup, /aria-label="Agent configuration"/);
  assert.match(markup, /value="claude-opus-5"/);
  assert.match(markup, /value="high"/);
  assert.equal(markup.includes("aria-label=\"Agent queue\""), false);
  assert.match(markup, /dfConsoleLayout__left/);
  assert.equal(markup.includes("dfConsoleLayout__right"), false);

  // Without handlers the sidebar is a readout, never a dead form.
  const readOnly = render({ selectedAgent: agentSelection() });
  assert.equal(readOnly.includes("<input"), false);
  assert.equal(readOnly.includes("OPEN TERMINAL"), false);
});

test("a paused agent with queued work says the queue is paused", () => {
  const task = [...fixtureState.tasks.values()].find((item) => item.status === "queued");
  const selected = fixtureState.agents.get(task.assigned_agent_id);
  const agent = { ...selected, paused: true };
  const agents = new Map(fixtureState.agents);
  agents.set(agent.id, agent);
  const markup = render({
    state: baseState({ agents }),
    selectedAgent: { id: agent.id, name: agent.name, revision: agent.revision },
  });
  assert.match(markup, />QUEUE PAUSED</);
  assert.equal(markup.includes("QUEUED · WAITING FOR CAPACITY"), false);
});

test("the queued task row keeps served order and changes its exact priority", async () => {
  const previousAct = globalThis.IS_REACT_ACT_ENVIRONMENT;
  globalThis.IS_REACT_ACT_ENVIRONMENT = true;
  try {
    const edits = [];
    const detailReads = [];
    const queued = { ...fixtureState.tasks.get([...fixtureState.tasks.keys()][1]), title: "Served first", priority: 2 };
    const other = { ...queued, id: "39".repeat(16), title: "Higher but served second", priority: 9, revision: 20n };
    const state = baseState({
      tasks: new Map([[queued.id, { ...queued, assigned_agent_id: ids.agent }], [other.id, { ...other, assigned_agent_id: ids.agent }]]),
    });
    const props = {
      status: "ready",
      state,
      detail: "queue",
      selectedAgent: agentSelection(),
      onSaveAgentConfig: (config) => edits.push(["config", config]),
      onEditTask: async (task, change) => { edits.push([task.id, change]); return true; },
      onLoadTaskDetail: async (task, peerOffset, expectedHead) => {
        detailReads.push(task.id);
        if (peerOffset !== undefined) throw new SessionError("stale");
        return { taskId: task.id, revision: task.revision, head: expectedHead ?? 7n, instruction: "Original brief", feedback: "Review this carefully", peerQuestions: [{ id: `${peerOffset ?? 0n}`.padStart(32, "0"), source_task_id: queued.id, target_task_id: other.id, question: "What changed?", recipient_delivery_state: "delivered", answer_delivery_state: "pending", revision: 1n, created_at_ms: 1n, updated_at_ms: 1n }], nextPeerOffset: 1n };
      },
    };
    let renderer;
    await act(async () => { renderer = create(createElement(FactoryConsole, props)); });
    const buttons = renderer.root.findAllByType("button");
    const byLabel = (label) => buttons.find((button) => button.props["aria-label"] === label);
    const queueRows = renderer.root.findByProps({ "aria-label": "Queue" }).findAllByType("details").filter((row) => row.props.className === "dfConsoleItem");
    assert.deepEqual(queueRows.map((row) => row.findByType("strong").props.children), [queued.title, other.title], "the queue keeps the server's per-agent order, rather than re-sorting priority");
    assert.ok(renderer.root.findAllByType("span").some((span) => (Array.isArray(span.props.children) ? span.props.children.join("") : String(span.props.children)).includes("QUEUED · PRIORITY 2")));
    await act(async () => { byLabel(`Increase priority for ${queued.title}`).props.onClick(); });
    assert.deepEqual(edits.at(-1), [queued.id, { priority: queued.priority + 1 }]);
    await act(async () => { byLabel(`Decrease priority for ${other.title}`).props.onClick(); });
    assert.deepEqual(edits.at(-1), [other.id, { priority: other.priority - 1 }]);

    const highest = { ...queued, title: "Highest", priority: MAX_TASK_PRIORITY };
    const lowest = { ...other, title: "Lowest", priority: -MAX_TASK_PRIORITY };
    await act(async () => { renderer.update(createElement(FactoryConsole, { ...props, state: baseState({ tasks: new Map([[highest.id, highest], [lowest.id, lowest]]) }) })); });
    const bounded = renderer.root.findAllByType("button");
    assert.equal(bounded.find((button) => button.props["aria-label"] === `Increase priority for ${highest.title}`).props.disabled, true);
    assert.equal(bounded.find((button) => button.props["aria-label"] === `Decrease priority for ${lowest.title}`).props.disabled, true);

    await act(async () => { renderer.update(createElement(FactoryConsole, props)); });

    const editBrief = () => renderer.root.findAllByType("button").find((button) => button.props.children === "EDIT BRIEF");
    const firstRow = renderer.root.findByProps({ "aria-label": "Queue" }).findAllByType("details").find((row) => row.props.className === "dfConsoleItem");
    assert.equal(firstRow.props.open, undefined, "queue rows start collapsed");
    assert.deepEqual(detailReads, [], "collapsed queued rows do not read private briefs");
    await act(async () => { firstRow.props.onToggle({ currentTarget: { open: true } }); });
    assert.deepEqual(detailReads, [queued.id]);
    const title = renderer.root.findAllByType("input").find((input) => input.props.value === queued.title);
    await act(async () => { title.props.onChange({ currentTarget: { value: "Renamed" } }); });
    const instruction = renderer.root.findAllByType("textarea").find((input) => input.props.id === `df-instruction-${queued.id}`);
    await act(async () => { instruction.props.onChange({ currentTarget: { value: "Replacement brief" } }); });
		assert.ok(renderer.root.findAllByType("pre").some((item) => item.props.children === "Review this carefully"));
    await act(async () => { await renderer.root.findAllByType("button").find((button) => button.props.children === "SAVE BRIEF").props.onClick(); });
    assert.deepEqual(edits.at(-1), [queued.id, { title: "Renamed", body: "Replacement brief" }]);
    await act(async () => { firstRow.props.onToggle({ currentTarget: { open: false } }); firstRow.props.onToggle({ currentTarget: { open: true } }); });
    assert.deepEqual(detailReads, [queued.id], "reopening a cached row does not overwrite the brief");

    const assign = renderer.root.findAllByType("select").find((select) => select.props.id === `df-assign-${queued.id}`);
    // Reassignment offers only agents in the same project.
    assert.deepEqual(assign.props.children.map((option) => option.props.children), ["Builder One", "Builder Two"]);
    await act(async () => { assign.props.onChange({ currentTarget: { value: "23".repeat(16) } }); });
    assert.deepEqual(edits.at(-1), [queued.id, { assignedAgentId: "23".repeat(16) }]);

    await act(async () => { renderer.root.findAllByType("button").find((button) => button.props.children === "CANCEL").props.onClick(); });
    assert.deepEqual(edits.at(-1), [queued.id, { cancel: true }]);

    // A refused brief edit preserves the operator's drafts at the unchanged
    // revision, rather than silently replacing the intended instruction.
    await act(async () => { await editBrief().props.onClick(); });
    const titleValue = () => renderer.root.findAllByType("input").find((input) => input.props.id === `df-title-${queued.id}`).props.value;
    const instructionValue = () => renderer.root.findAllByType("textarea").find((input) => input.props.id === `df-instruction-${queued.id}`).props.value;
    await act(async () => { renderer.root.findAllByType("input").find((input) => input.props.id === `df-title-${queued.id}`).props.onChange({ currentTarget: { value: "Keep this draft" } }); });
    await act(async () => { renderer.root.findAllByType("textarea").find((input) => input.props.id === `df-instruction-${queued.id}`).props.onChange({ currentTarget: { value: "Keep this instruction" } }); });
    assert.equal(titleValue(), "Keep this draft");
    await act(async () => { await renderer.root.findAllByType("button").find((button) => button.props.children === "OLDER CONVERSATION").props.onClick(); });
    assert.equal(titleValue(), "Keep this draft");
    assert.equal(instructionValue(), "Keep this instruction");
    assert.ok(renderer.root.findAllByProps({ role: "alert" }).some((item) => String(item.props.children).includes("SAVE OR DISCARD YOUR DRAFT")));
    const revised = baseState({ tasks: new Map([[queued.id, { ...queued, assigned_agent_id: ids.agent, revision: queued.revision + 1n }], [other.id, { ...other, assigned_agent_id: ids.agent }]]) });
    await act(async () => { renderer.update(createElement(FactoryConsole, { ...props, state: revised })); });
    assert.equal(titleValue(), "Keep this draft");
    assert.equal(instructionValue(), "Keep this instruction");
    assert.equal(renderer.root.findAllByProps({ role: "alert" }).some((item) => String(item.props.children).includes("TASK CHANGED")), true);
    assert.equal(renderer.root.findAllByType("button").find((button) => button.props.children === "SAVE BRIEF").props.disabled, true);
    await act(async () => { renderer.unmount(); });
  } finally {
    globalThis.IS_REACT_ACT_ENVIRONMENT = previousAct;
  }
});

test("a config refusal resets only its form and config saves remain partial", async () => {
  const previousAct = globalThis.IS_REACT_ACT_ENVIRONMENT;
  globalThis.IS_REACT_ACT_ENVIRONMENT = true;
  try {
    const edits = [];
    const queued = [...fixtureState.tasks.values()].find((task) => task.status === "queued");
    const agent = fixtureState.agents.get(ids.agent);
    const props = {
      status: "ready",
      state: baseState(),
      detail: "agent",
      agentPanel: "config",
      selectedAgent: agentSelection(),
      onSaveAgentConfig: (config) => edits.push(config),
    };
    let renderer;
    await act(async () => { renderer = create(createElement(FactoryConsole, props)); });
    const model = () => renderer.root.findAllByType("input").find((input) => input.props.id === `df-model-${agent.id}`);
    const config = () => renderer.root.findAllByType("form").find((form) => form.props["aria-label"] === "Agent configuration");
    const paused = () => renderer.root.findAllByType("input").find((input) => input.props.type === "checkbox");

    await act(async () => { model().props.onChange({ currentTarget: { value: "half-typed" } }); });
    await act(async () => { renderer.update(createElement(FactoryConsole, { ...props, edit: { target: queued.id, pending: false, error: new SessionError("stale") } })); });
    assert.equal(model().props.value, "half-typed", "a task refusal leaves the config draft intact");
    assert.equal(renderer.root.findAllByProps({ role: "alert" }).length, 1, "the shared alert remains visible");

    await act(async () => { renderer.update(createElement(FactoryConsole, { ...props, edit: { target: agent.id, pending: false, error: new SessionError("stale") } })); });
    assert.equal(model().props.value, agent.model, "a config refusal remounts its own form");
    assert.equal(renderer.root.findAllByProps({ role: "alert" }).length, 1, "a config refusal has one shared alert");
    await act(async () => { renderer.update(createElement(FactoryConsole, props)); });

    await act(async () => { paused().props.onChange({ currentTarget: { checked: true } }); });
    await act(async () => { config().props.onSubmit({ preventDefault() {} }); });
    assert.deepEqual(edits.at(-1), { paused: true });

    await act(async () => { model().props.onChange({ currentTarget: { value: "claude-sonnet-5" } }); });
    await act(async () => { config().props.onSubmit({ preventDefault() {} }); });
    assert.deepEqual(edits.at(-1), { model: "claude-sonnet-5", paused: true });

    await act(async () => { model().props.onChange({ currentTarget: { value: agent.model } }); });
    await act(async () => { paused().props.onChange({ currentTarget: { checked: false } }); });
    await act(async () => { config().props.onSubmit({ preventDefault() {} }); });
    assert.deepEqual(edits.at(-1), {}, "an untouched config submits no changes");
    await act(async () => { renderer.unmount(); });
  } finally {
    globalThis.IS_REACT_ACT_ENVIRONMENT = previousAct;
  }
});

test("recent work remains collapsed, bounded, and private until opened", async () => {
  const tasks = new Map(Array.from({ length: 12 }, (_, index) => {
    const task = {
      ...fixtureState.tasks.get(ids.task),
      id: String(index + 1).padStart(32, "0"),
      assigned_agent_id: ids.agent,
      title: "Direct instruction",
      status: index % 3 === 0 ? "blocked" : index % 3 === 1 ? "succeeded" : "failed",
      revision: BigInt(index + 1),
      updated_at_ms: BigInt(1_700_000_000_000 + index),
    };
    return [task.id, task];
  }));
  const detailCalls = [];
  const historyCalls = [];
  const listCalls = [];
  const props = {
    status: "ready", state: baseState({ tasks }), selectedAgent: agentSelection(), onSaveAgentConfig: () => {}, onEditTask: async () => true,
    onLoadTaskDetail: async (task, peerOffset, expectedHead) => {
      detailCalls.push([task.id, peerOffset, expectedHead]);
      return {
        taskId: task.id, revision: task.revision, head: expectedHead ?? 9n,
        instruction: `Instruction ${task.id}`,
        feedback: "https://github.com/example-owner/example-repo/pull/42 https://example.test/not-a-pr",
        outcome: `Outcome ${task.id}`,
        peerQuestions: [],
      };
    },
    onLoadTaskHistory: async (task) => {
      historyCalls.push(task.id);
      return { taskId: task.id, entries: [{ operationId: "91".repeat(16), kind: "message", actor: "operator", body: "reviewed", status: "delivered", createdAtMs: 1_700_000_000_012n }] };
    },
    onLoadTaskList: async (_agentId, cursor) => {
      listCalls.push(cursor);
      const recent = [...tasks.values()].sort((left, right) => left.updated_at_ms === right.updated_at_ms ? right.id.localeCompare(left.id) : left.updated_at_ms > right.updated_at_ms ? -1 : 1);
      const start = cursor === undefined ? 0 : recent.findIndex((task) => task.id === cursor.beforeTaskId) + 1;
      const page = recent.slice(start, start + 10);
      return { agentId: ids.agent, head: 9n, total: BigInt(recent.length), tasks: page, hasMore: start + page.length < recent.length };
    },
  };
  let renderer;
  await act(async () => { renderer = create(createElement(FactoryConsole, props)); });
  const recent = renderer.root.findByProps({ className: "dfConsoleRecentWork dfConsoleSidebar__section" });
  assert.equal(recent.props.open, undefined, "recent work starts collapsed");
  assert.deepEqual(detailCalls, [], "the collapsed list requests no private detail");
  assert.deepEqual(listCalls, [], "the collapsed list requests no private completion list");
  assert.deepEqual(historyCalls, [], "the collapsed list requests no intervention history");
  await act(async () => { recent.props.onToggle({ currentTarget: { open: true } }); });
  assert.equal(detailCalls.length, 10, "opening fetches the bounded first page only");
  assert.equal(listCalls.length, 1, "opening fetches one private completion page");
  assert.deepEqual(historyCalls, [], "row history remains lazy");
  const items = () => recent.findByProps({ className: "dfConsoleItems" }).findAllByType("li").filter((item) => item.props.className === "dfConsoleItem");
  assert.equal(items().length, 10);
  assert.ok(items()[0].findAllByType("small").some((item) => String(item.children).includes("00000000000000000000000000000012")), "newest updated task is first");
  const firstDetails = items()[0].findByType("details");
  await act(async () => { firstDetails.props.onToggle({ currentTarget: { open: true } }); });
  assert.deepEqual(historyCalls, [detailCalls[0][0]], "intervention receipts load only for the expanded row");
  assert.ok(renderer.root.findAllByType("pre").some((item) => item.props.children === `Outcome ${detailCalls[0][0]}`));
  const links = items()[0].findAllByType("a");
  assert.deepEqual(links.map((link) => link.props.href), ["https://github.com/example-owner/example-repo/pull/42"], "only an exact GitHub pull URL becomes a link");
  await act(async () => { renderer.root.findAllByType("button").find((button) => button.props.children === "SHOW MORE").props.onClick(); });
  assert.equal(detailCalls.length, 12, "show more loads exactly the next bounded page");
  assert.deepEqual(listCalls[1], { beforeUpdatedAtMs: 1_700_000_000_002n, beforeTaskId: "00000000000000000000000000000003" });
  assert.equal(items().length, 12);
  await act(async () => { renderer.unmount(); });
});

test("late recent-work detail never crosses an agent remount", async () => {
  const first = fixtureState.agents.get(ids.agent);
  const second = fixtureState.agents.get(ids.idleAgent);
  const firstTask = { ...fixtureState.tasks.get(ids.task), assigned_agent_id: first.id, status: "succeeded", revision: 21n, updated_at_ms: 21n };
  const secondTask = { ...firstTask, id: "52".repeat(16), assigned_agent_id: second.id, status: "failed", revision: 22n, updated_at_ms: 22n };
  const pending = new Map();
  const props = {
    status: "ready", state: baseState({ tasks: new Map([[firstTask.id, firstTask], [secondTask.id, secondTask]]) }), selectedAgent: agentSelection(first.id), onSaveAgentConfig: () => {},
    onLoadTaskDetail: (task) => new Promise((resolve) => pending.set(task.id, resolve)),
    onLoadTaskList: async (agentId) => ({ agentId, head: 9n, total: 1n, tasks: [agentId === first.id ? firstTask : secondTask], hasMore: false }),
  };
  let renderer;
  await act(async () => { renderer = create(createElement(FactoryConsole, props)); });
  const openRecent = async () => {
    const recent = renderer.root.findByProps({ className: "dfConsoleRecentWork dfConsoleSidebar__section" });
    await act(async () => { recent.props.onToggle({ currentTarget: { open: true } }); });
  };
  await openRecent();
  assert.equal(pending.has(firstTask.id), true);
  await act(async () => { renderer.update(createElement(FactoryConsole, { ...props, selectedAgent: agentSelection(second.id) })); });
  await openRecent();
  pending.get(secondTask.id)({ taskId: secondTask.id, revision: secondTask.revision, head: 9n, instruction: "SECOND DETAIL", feedback: "", outcome: "SECOND OUTCOME", peerQuestions: [] });
  await act(async () => {});
  pending.get(firstTask.id)({ taskId: firstTask.id, revision: firstTask.revision, head: 9n, instruction: "FIRST DETAIL", feedback: "", outcome: "FIRST OUTCOME", peerQuestions: [] });
  await act(async () => {});
  const text = renderer.toJSON();
  const markup = JSON.stringify(text);
  assert.match(markup, /SECOND DETAIL/);
  assert.equal(markup.includes("FIRST DETAIL"), false, "the late prior-agent detail has no new panel to update");
  await act(async () => { renderer.unmount(); });
});

test("a rejected edit says plainly that the durable value did not change", () => {
  const markup = render({
    selectedAgent: agentSelection(),
    onSaveAgentConfig: () => {},
    edit: { target: ids.agent, pending: false, error: new SessionError("stale") },
  });
  assert.match(markup, /SOMEONE ELSE CHANGED THIS — REOPEN IT AND TRY AGAIN/);
  assert.match(markup, /role="alert"/);
  assert.equal((markup.match(/role="alert"/g) ?? []).length, 1, "a config refusal has one shared alert");
  const unknown = render({ selectedAgent: agentSelection(), onSaveAgentConfig: () => {}, edit: { target: ids.agent, pending: false, error: { code: "internal" } } });
  assert.match(unknown, /THE EDIT DID NOT COMPLETE/);
  assert.match(render({ selectedAgent: agentSelection(), onSaveAgentConfig: () => {}, edit: { pending: true } }), />SAVING</);
});

test("a queued edit refusal remains visible after its task leaves the queue", () => {
  const rejected = { target: ids.task, pending: false, error: new SessionError("stale") };
  const task = fixtureState.tasks.get(ids.task);
  const states = [
    baseState({ tasks: new Map([[task.id, { ...task, status: "running" }]]) }),
    baseState({ tasks: new Map([[task.id, { ...task, status: "cancelled" }]]) }),
    baseState({ tasks: new Map() }),
  ];
  for (const state of states) {
    const markup = render({ state, detail: "needs-you", edit: rejected });
    assert.match(markup, /SOMEONE ELSE CHANGED THIS — REOPEN IT AND TRY AGAIN/);
    assert.equal((markup.match(/role="alert"/g) ?? []).length, 1);
  }
});

test("the settings modal carries the factory readout and a pairing mount point", () => {
  const markup = render({ settingsOpen: true, onToggleSettings: () => {} });
  assert.match(markup, /<dialog class="dfConsoleDialog" aria-label="Settings">/);
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
  // The modal is over the console, so it neither closes nor replaces a sidebar.
  const both = render({ settingsOpen: true, onToggleSettings: () => {}, selectedAgent: agentSelection() });
  assert.match(both, /aria-label="Settings"/);
  assert.match(both, /aria-label="Agent Builder One"/);
});

test("SETTINGS opens and closes as a native modal, over whatever sidebar is open", async () => {
  const previousAct = globalThis.IS_REACT_ACT_ENVIRONMENT;
  globalThis.IS_REACT_ACT_ENVIRONMENT = true;
  try {
    const calls = [];
    const node = { showModal: () => calls.push("showModal"), close: () => calls.push("close") };
    let renderer;
    await act(async () => {
      renderer = create(createElement(FactoryConsole, {
        status: "ready",
        state: baseState(),
        settingsOpen: true,
        selectedAgent: agentSelection(),
        onToggleSettings: () => calls.push("toggle"),
      }), { createNodeMock: () => node });
    });
    // The browser opens it, so ESC, the backdrop and the focus trap are its.
    assert.deepEqual(calls, ["showModal"]);
    const dialog = renderer.root.findByType("dialog");
    // The agent sidebar is still mounted underneath.
    assert.equal(renderer.root.findAll((instance) => instance.props["aria-label"] === "Agent Builder One").length, 1);

    // Every exit goes through close(), so focus always returns to SETTINGS,
    // and the close event is what tells the console the modal is gone.
    renderer.root.findAllByType("button").find((button) => button.props.children === "CLOSE").props.onClick();
    dialog.props.onClick({ target: node });
    dialog.props.onClick({ target: {} });
    assert.deepEqual(calls, ["showModal", "close", "close"]);
    await act(async () => { dialog.props.onClose(); });
    assert.deepEqual(calls.at(-1), "toggle");
    await act(async () => { renderer.unmount(); });
  } finally {
    globalThis.IS_REACT_ACT_ENVIRONMENT = previousAct;
  }
});

test("Factory and Agents are explicit left-side alternatives", () => {
  const floor = render();
  assert.match(floor, /aria-label="Left view"/);
  assert.match(floor, /aria-pressed="true" disabled="">FACTORY/);
  assert.match(render({ view: "agents" }), /aria-label="Agents"/);
  // The top bar keeps the wordmark, the counters, and SETTINGS.
  const actions = floor.split('class="dfConsoleBar__actions"')[1];
  assert.match(actions.slice(0, actions.indexOf("</div>")), />SETTINGS</);
  assert.match(floor, /<h2>FACTORY FLOOR<\/h2>/);
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
  // Selected detail expands in its original list row, with one heading.
  assert.match(markup, /<details class="dfConsoleItem" open=""><summary[^>]*><strong>Builder One asks<\/strong>/);
  assert.match(markup, /<article class="dfFactoryConsole__humanRequest"/);
  assert.match(markup, /OPEN · North Workshop · Review the state projection/);
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
  requestElements.find((element) => element.type === "summary" && element.props.className === "dfConsoleItem__summary").props.onClick({ preventDefault() {} });
  assert.equal(calls[0][0], "select");
  assert.equal(calls[0][1], request);
  const busyElements = expand(FactoryConsole({ ...baseProps, selectedHumanRequest: selectedRequest({ phase: "replying" }) }));
  const busySummary = busyElements.find((element) => element.type === "summary" && element.props.className === "dfConsoleItem__summary");
  assert.equal(busySummary.props["aria-disabled"], true);
  busySummary.props.onClick({ preventDefault() {} });
  assert.equal(calls.length, 1, "an in-flight answer cannot be collapsed or switched");

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

  const markup = render({ selectedAgent: agentSelection(), terminalContent: createElement("div", null, "<raw-output>") });
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
  assert.deepEqual(chrome.map((element) => element.props.children), ["SETTINGS", "FACTORY", "AGENTS"]);
  chrome[0].props.onClick();
  chrome.find((element) => element.props.children === "AGENTS").props.onClick();
  assert.deepEqual(calls, [["settings"], ["view", "agents"]]);
});

function expand(node, result = []) {
  if (Array.isArray(node)) {
    for (const child of node) expand(child, result);
    return result;
  }
  if (!isValidElement(node)) return result;
  if (typeof node.type === "function") {
    if (node.type.name === "QueuePanel") return result;
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
  assert.match(render({ ...settings, remoteInviteAllowed: true, selectedAgent: agentSelection() }), /PAIR A PHONE/, "the modal is over the sidebar, not behind it");
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

const shellAgent = { id: ids.agent, project_id: ids.project, name: "Shell Hand", role: "worker", provider: "shell", paused: true, model: "", reasoning_effort: "", effective_model: "", effective_reasoning_effort: "", model_source: "", revision: 10n };

const withAgent = (agent) => render({
  view: "agents",
  state: baseState({ agents: new Map([[agent.id, agent]]) }),
  selectedAgent: { id: agent.id, name: agent.name, revision: agent.revision },
  onSaveAgentConfig: () => {},
});

const inheritingAgent = () => fixtureState.agents.get("23".repeat(16));

test("the console shows the model an agent will actually run with", () => {
  // An agent that names no model still runs with one. The row and the panel
  // header say which, so a blank is never read as "no model".
  const rows = render({ view: "agents" });
  assert.match(rows, /codex · gpt-6-astra/);
  assert.match(rows, /claude_code · claude-opus-5/);
  assert.match(withAgent(inheritingAgent()), /Builder Two[\s\S]*?codex · gpt-6-astra/);
  // The provider with no model says nothing extra rather than a dangling dot.
  assert.match(withAgent(shellAgent), /Shell Hand[\s\S]*?shell/);
});

test("the config inputs stay the agent's own override and caption where it came from", () => {
  // Own: the served value is in the box and the caption names the agent.
  const own = withAgent(fixtureState.agents.get(ids.agent));
  assert.match(own, /<input id="df-model-[0-9a-f]+"[^>]*placeholder="claude-opus-5"/);
  assert.match(own, /<input id="df-model-[0-9a-f]+"[^>]*value="claude-opus-5"/);
  assert.match(own, />set on this agent</);

  // Inherited: the box is empty because the override is, and the placeholder
  // plus the caption say what the run gets and which file decided it.
  const inherited = withAgent(inheritingAgent());
  assert.match(inherited, /<input id="df-model-[0-9a-f]+"[^>]*placeholder="gpt-6-astra"/);
  assert.match(inherited, /<input id="df-model-[0-9a-f]+"[^>]*value=""/);
  assert.match(inherited, /<input id="df-effort-[0-9a-f]+"[^>]*placeholder="high"/);
  assert.match(inherited, /inherited from \/Users\/operator\/\.codex\/config\.toml/);

  // Unknown: an older daemon, or a CLI whose configuration the factory cannot
  // read. The console says so rather than implying the model is unset.
  const unknown = withAgent({ ...inheritingAgent(), effective_model: "", effective_reasoning_effort: "", model_source: "" });
  assert.match(unknown, /CLI default \(not visible to the factory\)/);
  assert.equal(unknown.includes("inherited from"), false);
});

test("the shell provider has no model inputs but keeps PAUSED", () => {
  const markup = withAgent(shellAgent);
  assert.match(markup, />shell has no model</);
  assert.equal(markup.includes("df-model-"), false);
  assert.equal(markup.includes("df-effort-"), false);
  // The daemon rejects a model for shell; pausing it is still an edit.
  assert.match(markup, /id="df-paused-[0-9a-f]+"/);
  assert.match(markup, />PAUSED</);
});

test("the agent config offers its provider's linked accounts and the provider default", async () => {
  const previousAct = globalThis.IS_REACT_ACT_ENVIRONMENT;
  globalThis.IS_REACT_ACT_ENVIRONMENT = true;
  try {
    const edits = [];
    const state = baseState();
    const linked = [...state.accounts.values()][0];
    const props = {
      status: "ready",
      state,
      selectedAgent: agentSelection(),
      onSaveAgentConfig: (config) => edits.push(config),
    };
    let renderer;
    await act(async () => { renderer = create(createElement(FactoryConsole, props)); });
    const account = renderer.root.findAllByType("select").find((select) => select.props.id === `df-account-${ids.agent}`);
    // The selected agent is claude_code, so only claude_code logins are offered.
    assert.deepEqual(account.props.children[0].props.children, "provider default");
    assert.deepEqual(account.props.children[1].map((option) => option.props.children), [linked.label]);
    assert.equal(account.props.value, linked.id);

    // Only what the operator changed is sent, and "" clears the selection.
    await act(async () => { account.props.onChange({ currentTarget: { value: "" } }); });
    const form = renderer.root.findAllByProps({ "aria-label": "Agent configuration" })[0].findByType("form");
    await act(async () => { form.props.onSubmit({ preventDefault() {} }); });
    assert.deepEqual(edits.at(-1), { accountId: "" });

    // A shell agent has no logins at all, so it has no account control.
    const shell = { ...state.agents.get(ids.agent), provider: "shell", model: "", reasoning_effort: "", account_id: "" };
    await act(async () => {
      renderer.update(createElement(FactoryConsole, { ...props, state: baseState({ agents: new Map([[shell.id, shell]]) }) }));
    });
    assert.equal(renderer.root.findAllByType("select").some((select) => select.props.id === `df-account-${ids.agent}`), false);
    await act(async () => { renderer.unmount(); });
  } finally {
    globalThis.IS_REACT_ACT_ENVIRONMENT = previousAct;
  }
});

test("settings asks the daemon for logins on open and links the one the operator names", async () => {
  const previousAct = globalThis.IS_REACT_ACT_ENVIRONMENT;
  globalThis.IS_REACT_ACT_ENVIRONMENT = true;
  try {
    const login = {
      provider: "codex",
      home: "/Users/operator/.codex-dogfood",
      label: ".codex-dogfood",
      email: "operator@example.com",
      organization: "",
      default_model: "gpt-6-astra",
      default_reasoning_effort: "high",
      linked_id: "",
    };
    const asked = [];
    const linkings = [];
    const props = {
      status: "ready",
      state: baseState(),
      settingsOpen: true,
      onToggleSettings: () => {},
      onLoadAccounts: () => asked.push("asked"),
      onLinkAccount: (candidate, label) => linkings.push([candidate.home, label]),
      accounts: [login],
    };
    let renderer;
    await act(async () => { renderer = create(createElement(FactoryConsole, props)); });
    // Discovery is an observation, so opening SETTINGS is what asks for it.
    assert.deepEqual(asked, ["asked"]);
    const section = renderer.root.findAllByProps({ "aria-label": "ACCOUNTS" })[0];
    assert.ok(section !== undefined);
    const label = renderer.root.findAllByType("input").find((input) => input.props.id === `df-account-label-${login.home}`);
    await act(async () => { label.props.onChange({ currentTarget: { value: "dogfood" } }); });
    const link = renderer.root.findAllByType("button").find((button) => button.props.children === "LINK");
    await act(async () => { link.props.onClick(); });
    assert.deepEqual(linkings, [[login.home, "dogfood"]]);

    // The daemon's refusal is shown plainly rather than retried.
    await act(async () => { renderer.update(createElement(FactoryConsole, { ...props, accountsError: "not_found" })); });
    assert.ok(renderer.root.findAllByProps({ role: "alert" }).length > 0);
    await act(async () => { renderer.unmount(); });
  } finally {
    globalThis.IS_REACT_ACT_ENVIRONMENT = previousAct;
  }
});

test("the RULES block saves an idle rule and sends only what changed", async () => {
  const edits = [];
  const props = { status: "ready", state: baseState(), selectedAgent: agentSelection(), onSaveAgentConfig: (config) => edits.push(config) };
  let renderer;
  await act(async () => { renderer = create(createElement(FactoryConsole, props)); });
  const field = (id) => renderer.root.findAll((node) => node.props.id === id)[0];
  const form = () => renderer.root.findAllByType("form")[0];
  // Waiting agents show no instruction controls; choosing the rule reveals them.
  assert.equal(field(`df-idle-instruction-${ids.agent}`), undefined);
  await act(async () => { field(`df-idle-${ids.agent}`).props.onChange({ currentTarget: { value: "standing_instruction" } }); });
  await act(async () => { field(`df-idle-after-${ids.agent}`).props.onChange({ currentTarget: { value: "2" } }); });
  await act(async () => { field(`df-idle-instruction-${ids.agent}`).props.onChange({ currentTarget: { value: "Look for follow-up work." } }); });
  await act(async () => { field(`df-idle-budget-${ids.agent}`).props.onChange({ currentTarget: { value: "3" } }); });
  await act(async () => { form().props.onSubmit({ preventDefault() {} }); });
  assert.deepEqual(edits.at(-1), { idlePolicy: "standing_instruction", idleAfterSeconds: 2, idleInstruction: "Look for follow-up work.", idleRunBudget: 3 });
  // A rule the daemon would refuse never leaves the form: no wait means no
  // save, and the form says why.
  await act(async () => { field(`df-idle-after-${ids.agent}`).props.onChange({ currentTarget: { value: "0" } }); });
  const saveButton = () => renderer.root.findAllByType("button").find((button) => button.props.type === "submit");
  assert.equal(saveButton().props.disabled, true);
  const before = edits.length;
  await act(async () => { form().props.onSubmit({ preventDefault() {} }); });
  assert.equal(edits.length, before);
  assert.ok(renderer.root.findAllByType("p").some((paragraph) => String(paragraph.props.children).includes("needs at least a second")));
  await act(async () => { field(`df-idle-after-${ids.agent}`).props.onChange({ currentTarget: { value: "1" } }); });
  assert.equal(saveButton().props.disabled, false);
  await act(async () => { form().props.onSubmit({ preventDefault() {} }); });
  assert.deepEqual(edits.at(-1), { idlePolicy: "standing_instruction", idleAfterSeconds: 1, idleInstruction: "Look for follow-up work.", idleRunBudget: 3 });
  // On a spent rule, editing the text leaves the budget out, so the count
  // stands; retyping the same budget sends it, which restarts the count.
  const spent = { ...fixtureState.agents.get(ids.agent), idle_policy: "standing_instruction", idle_after_seconds: 600, idle_instruction: "Look for follow-up work.", idle_run_budget: 3, idle_runs_used: 3 };
  const spentState = baseState({ agents: new Map([...fixtureState.agents, [spent.id, spent]]) });
  const again = [];
  let spentRenderer;
  await act(async () => { spentRenderer = create(createElement(FactoryConsole, { status: "ready", state: spentState, selectedAgent: agentSelection(), onSaveAgentConfig: (config) => again.push(config) })); });
  const spentField = (id) => spentRenderer.root.findAll((node) => node.props.id === id)[0];
  assert.match(renderToStaticMarkup(createElement(FactoryConsole, { status: "ready", state: spentState, selectedAgent: agentSelection(), onSaveAgentConfig: () => {} })), /3 of 3 idle runs used/);
  await act(async () => { spentField(`df-idle-instruction-${ids.agent}`).props.onChange({ currentTarget: { value: "Look for follow-up work, then tidy." } }); });
  await act(async () => { spentRenderer.root.findAllByType("form")[0].props.onSubmit({ preventDefault() {} }); });
  assert.deepEqual(again.at(-1), { idlePolicy: "standing_instruction", idleAfterSeconds: 600, idleInstruction: "Look for follow-up work, then tidy." });
  await act(async () => { spentField(`df-idle-budget-${ids.agent}`).props.onChange({ currentTarget: { value: "3" } }); });
  await act(async () => { spentRenderer.root.findAllByType("form")[0].props.onSubmit({ preventDefault() {} }); });
  assert.deepEqual(again.at(-1), { idlePolicy: "standing_instruction", idleAfterSeconds: 600, idleInstruction: "Look for follow-up work, then tidy.", idleRunBudget: 3 });
});

test("overseer supervision names worker events and a seconds cooldown", () => {
  const agent = { ...fixtureState.agents.get(ids.agent), role: "orchestrator", idle_policy: "standing_instruction", idle_after_seconds: 10, idle_instruction: "Inspect worker activity.", idle_run_budget: 1 };
  const agents = new Map(fixtureState.agents);
  agents.set(agent.id, agent);
  const markup = render({ state: baseState({ agents }), selectedAgent: { id: agent.id, name: agent.name, revision: agent.revision }, onSaveAgentConfig: () => {} });
  for (const text of ["SUPERVISION", "WHEN WORK CHANGES", "supervise worker activity", "COOLDOWN SECONDS", "initial inspection, then worker events"]) assert.match(markup, new RegExp(text));
  assert.match(markup, new RegExp(`id="df-idle-after-${agent.id}"[^>]*value="10"`));
});
