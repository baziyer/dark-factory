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
  // The agent list is the keyboard-reachable way in; the sidebar it opens
  // carries the explicit action that attaches the terminal.
  const agentsView = () => renderer.root.findAllByType("button").find((button) => button.props.children === "AGENTS");
  const agentRow = () => renderer.root.findAllByType("button").find((button) => typeof button.props.className === "string" && button.props.className.includes("dfAgentList__row"));
  const openTerminal = () => renderer.root.findAllByType("button").find((button) => button.props.children === "OPEN TERMINAL");
  await act(async () => { agentsView().props.onClick(); });
  const open = async () => {
    const row = agentRow();
    assert.ok(row, "public FactoryApp must expose a selectable agent");
    await act(async () => { row.props.onClick(); });
    const terminal = openTerminal();
    assert.ok(terminal, "the selected agent's sidebar must expose the terminal action");
    await act(async () => { terminal.props.onClick(); });
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

  const close = renderer.root.findAllByType("button").find((button) => button.props.children === "CLOSE");
  assert.ok(close, "selected terminal must expose an explicit close action");
  await act(async () => {
    close.props.onClick();
  });
  await waitFor(() => counters.detaches === 1 && counters.terminals === counters.disposes, "terminal did not detach and unmount");
  // Leaving the terminal returns to the agent's own sidebar, and unmounting
  // the surface as part of that teardown is not reported as a fault.
  assert.ok(openTerminal(), "closing the terminal must return to the agent panel");
  assert.equal(renderer.root.findAll((node) => node.props?.role === "alert").length, 0, "a deliberate close raises no error");
  assert.equal(counters.detaches, 1, "close must detach exactly one terminal observer");
  assert.equal(counters.sessionCloses, 1, "terminal close must preserve the active browser session");
  assert.equal(counters.terminals, counters.disposes, "closed terminal surface must unmount");

  await open();
  await waitFor(() => counters.acquires === 2 && counters.terminals - counters.disposes === 1, "terminal did not reopen");
  assert.equal(counters.resolves, 2);
  assert.equal(counters.opens, 2);
  assert.equal(counters.attaches, 2);
  assert.equal(counters.acquires, 2);
  assert.equal(counters.terminals - counters.disposes, 1, "reopen must create one fresh terminal on the same session");

  await act(async () => { renderer.unmount(); });
  assert.equal(counters.terminals, counters.disposes, "unmount must dispose the selected xterm");
  assert.equal(counters.sessionCloses, 2, "each public app session must close exactly once");
  console.log(JSON.stringify(counters));
} finally {
  if (renderer !== undefined) renderer.unmount();
}
