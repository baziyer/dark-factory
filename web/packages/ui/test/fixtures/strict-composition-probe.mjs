import assert from "node:assert/strict";
import { createElement, StrictMode } from "react";
import { act, create } from "react-test-renderer";
import { FactoryApp } from "../../dist/src/index.js";

globalThis.IS_REACT_ACT_ENVIRONMENT = true;
const listeners = new Set();
globalThis.window = {
  location: { origin: "https://strict.test", hash: "", pathname: "/", search: "" },
  history: { state: null, replaceState() {} },
  addEventListener: (_type, listener) => listeners.add(listener),
  removeEventListener: (_type, listener) => listeners.delete(listener),
  ResizeObserver: class {
    constructor(callback) { this.callback = callback; }
    observe() {}
    disconnect() {}
  },
};

async function waitFor(predicate, label) {
  for (let index = 0; index < 200; index += 1) {
    if (predicate()) return;
    await act(async () => { await new Promise((resolve) => setImmediate(resolve)); });
  }
  assert.fail(label);
}

const counters = globalThis.__darkFactoryStrictProbe;
let renderer;
try {
  await act(async () => {
    renderer = create(createElement(StrictMode, null, createElement(FactoryApp)), {
      createNodeMock: (element) => element.type === "div" ? { isConnected: true } : null,
    });
  });
  await waitFor(() => counters.states === 2, "factory state did not render");
  // Factory is the default view; the roster is its explicit alternative and
  // remains the keyboard-reachable terminal entry.
  const agentRow = () => renderer.root.findAllByType("button").find((button) => typeof button.props.className === "string" && button.props.className.includes("dfAgentList__row"));
  const open = async () => {
    const agents = renderer.root.findAllByType("button").find((button) => button.props.children === "AGENTS");
    assert.ok(agents, "public FactoryApp must expose the Agents view");
    await act(async () => { agents.props.onClick(); });
    const row = agentRow();
    assert.ok(row, "public FactoryApp must expose a selectable agent");
    await act(async () => { row.props.onClick(); });
  };
  await open();
  await waitFor(() => globalThis.__darkFactoryStrictProbe.acquires === 1 && globalThis.__darkFactoryStrictProbe.terminals - globalThis.__darkFactoryStrictProbe.disposes === 1, "first terminal did not become live");

  assert.equal(counters.clients, 2, "StrictMode must create two public app sessions");
  assert.equal(counters.resolves, 1, "selected agent must resolve one exact target");
  assert.equal(counters.opens, 1);
  assert.equal(counters.attaches, 1);
  assert.equal(counters.acquires, 1);
  assert.ok(counters.terminals >= 1, "selected public terminal must construct xterm");
  assert.equal(counters.terminals - counters.disposes, 1, "one selected terminal must remain live before unmount");

  for (const label of ["CONFIG", "TERMINAL"]) {
    const tab = renderer.root.findAllByType("button").find((button) => button.props.children === label);
    assert.ok(tab);
    await act(async () => { tab.props.onClick(); });
  }
  assert.equal(counters.opens, 1, "switching configuration preserves the terminal session");
  assert.equal(counters.acquires, 1, "switching configuration preserves terminal input ownership");
  assert.equal(counters.detaches, 0);
  assert.equal(counters.terminals - counters.disposes, 1);

  await act(async () => { renderer.unmount(); });
  assert.equal(counters.terminals, counters.disposes, "unmount must dispose the selected xterm");
  assert.equal(counters.sessionCloses, 2, "each public app session must close exactly once");
  console.log(JSON.stringify(counters));
} finally {
  if (renderer !== undefined) renderer.unmount();
}
