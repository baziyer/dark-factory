import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import test from "node:test";
import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { act, create } from "react-test-renderer";
import { AgentInstruction, TerminalContent, TerminalPanel } from "../dist/src/factory-app.js";

function terminalView(overrides = {}) {
  return {
    agentId: "21".repeat(16),
    agentName: "Builder One",
    agentRevision: 10n,
    phase: "ready",
    writable: true,
    paused: false,
    instructionPending: false,
    queued: false,
    hasOutputSurface: false,
    resets: 0,
    finishing: false,
    surfaceVersion: 0,
    ...overrides,
  };
}

function panel(terminal = terminalView(), onClose = () => {}) {
  return createElement(
    TerminalPanel,
    { terminal, onClose },
    createElement("div", { className: "terminal-surface" }, "live terminal surface"),
  );
}

test("the terminal is a quiet sidebar", () => {
  const markup = renderToStaticMarkup(panel(terminalView({ taskTitle: "Repair finalization" })));
  assert.match(markup, /dfFactoryConsole__terminalPanel/);
  assert.match(markup, /Builder One · Repair finalization/);
  assert.match(markup, /live terminal surface/);
  assert.match(markup, />CLOSE<\/button>/);
  for (const noise of ["CURRENT RUN TERMINAL", "READY", "you have control", "watching", "take control", "hand back", "Steer"]) {
    assert.equal(markup.includes(noise), false, noise);
  }
  assert.equal((markup.match(/<button/g) ?? []).length, 1, "close is the only terminal control");
});

test("finalizing work shows no blank terminal or idle input", () => {
  const markup = renderToStaticMarkup(createElement(TerminalContent, {
    terminal: terminalView({ taskTitle: "Repair finalization", finishing: true }),
    controller: {},
  }));
  assert.match(markup, />FINISHING<\/p>/);
  assert.equal(markup.includes("textarea"), false);
  assert.equal(markup.includes('role="application"'), false);
});

test("an idle configured agent accepts one compact instruction", async () => {
  const previousAct = globalThis.IS_REACT_ACT_ENVIRONMENT;
  globalThis.IS_REACT_ACT_ENVIRONMENT = true;
  try {
    const submitted = [];
    let renderer;
    await act(async () => {
      renderer = create(createElement(AgentInstruction, {
        terminal: terminalView({ phase: "idle", writable: false }),
        onSubmit: async (instruction) => { submitted.push(instruction); return true; },
      }));
    });
    const textarea = renderer.root.findByType("textarea");
    await act(async () => { textarea.props.onChange({ target: { value: "Repair the queue" } }); });
    const form = renderer.root.findByType("form");
    await act(async () => { form.props.onSubmit({ preventDefault() {} }); });
    assert.deepEqual(submitted, ["Repair the queue"]);
    assert.equal(renderer.root.findByType("textarea").props.value, "");
    await act(async () => { renderer.unmount(); });
  } finally {
    globalThis.IS_REACT_ACT_ENVIRONMENT = previousAct;
  }
});

test("the composer is a modest box wherever it flows after other content", () => {
  const markup = renderToStaticMarkup(createElement(AgentInstruction, {
    terminal: terminalView({ phase: "idle", writable: false }),
    onSubmit: async () => true,
  }));
  assert.match(markup, /<textarea[^>]* rows="3"/);
  const css = readFileSync(new URL("../src/factory-console.css", import.meta.url), "utf8");
  assert.match(css, /\.dfFactoryConsole__instruction textarea \{[^}]*resize: vertical;/);
  // Only the terminal panel, whose whole body the composer is, lets it grow:
  // under the sidebar queue it would otherwise swallow the section it sits in.
  assert.match(css, /\.dfFactoryConsole__terminalPanel \.dfFactoryConsole__instruction textarea \{[^}]*flex: 1 1 auto;/);
  assert.doesNotMatch(css, /\n\.dfFactoryConsole__instruction \{[^}]*flex: 1 1 auto;/);
  assert.doesNotMatch(css, /\n\.dfFactoryConsole__instruction textarea \{[^}]*flex: 1 1 auto;/);
});

test("paused agents remain identifiable without a false input", () => {
  const markup = renderToStaticMarkup(createElement(AgentInstruction, {
    terminal: terminalView({ phase: "idle", writable: false, paused: true }),
    onSubmit: async () => true,
  }));
  assert.match(markup, />PAUSED<\/p>/);
  assert.equal(markup.includes("textarea"), false);
});

test("paused or capacity-queued idle agents can add follow-up work", () => {
  for (const overrides of [{ paused: true }, { queued: true }]) {
    const markup = renderToStaticMarkup(createElement(TerminalContent, {
      terminal: terminalView({ phase: "idle", writable: false, ...overrides }),
      controller: {},
    }));
    const id = `df-instruction-${"21".repeat(16)}-queue`;
    assert.match(markup, new RegExp(`for="${id}"`));
    assert.match(markup, new RegExp(`id="${id}"`));
    assert.match(markup, />ADD TO QUEUE</);
  }
});

test("queued instructions state their capacity wait", () => {
  const markup = renderToStaticMarkup(createElement(AgentInstruction, {
    terminal: terminalView({ phase: "idle", writable: false, queued: true }),
    onSubmit: async () => true,
  }));
  assert.match(markup, /QUEUED · WAITING FOR CAPACITY/);
  assert.equal(markup.includes("textarea"), false);
});

test("an uncertain instruction send never claims the task was absent", () => {
  const markup = renderToStaticMarkup(createElement(AgentInstruction, {
    terminal: terminalView({ phase: "idle", writable: false, instructionError: { code: "connection" } }),
    onSubmit: async () => false,
  }));
  assert.match(markup, /SEND NOT CONFIRMED — CHECK TASKS BEFORE RETRYING/);
  assert.equal(markup.includes(">NOT SENT<"), false);
});

test("crypto-unavailable preflight is definitively not sent", () => {
  const markup = renderToStaticMarkup(createElement(AgentInstruction, {
    terminal: terminalView({ phase: "idle", writable: false, instructionError: { code: "crypto_unavailable" } }),
    onSubmit: async () => false,
  }));
  assert.match(markup, />NOT SENT</);
  assert.equal(markup.includes("SEND NOT CONFIRMED"), false);
});

test("close remains available through setup and invokes the owner once", async () => {
  const previousAct = globalThis.IS_REACT_ACT_ENVIRONMENT;
  globalThis.IS_REACT_ACT_ENVIRONMENT = true;
  try {
    for (const phase of ["idle", "resolving", "attaching", "acquiring", "ready", "closing", "closed"]) {
      let closes = 0;
      let renderer;
      await act(async () => {
        renderer = create(panel(terminalView({ phase }), () => { closes += 1; }));
      });
      const button = renderer.root.findByType("button");
      assert.equal(button.props.disabled, phase === "closing" || phase === "closed", phase);
      if (!button.props.disabled) {
        await act(async () => { button.props.onClick(); });
        assert.equal(closes, 1, phase);
      }
      await act(async () => { renderer.unmount(); });
    }
  } finally {
    globalThis.IS_REACT_ACT_ENVIRONMENT = previousAct;
  }
});

test("exceptional input ownership and replay loss are concise", () => {
  const occupied = renderToStaticMarkup(panel(terminalView({ writable: false, error: { code: "stale" } })));
  assert.match(occupied, /TERMINAL OPEN ELSEWHERE/);
  const unavailable = renderToStaticMarkup(panel(terminalView({ writable: false, error: { code: "connection" } })));
  assert.match(unavailable, /TERMINAL UNAVAILABLE/);
  assert.equal(unavailable.includes("TERMINAL OPEN ELSEWHERE"), false);
  const reset = renderToStaticMarkup(panel(terminalView({ resets: 1 })));
  assert.match(reset, /Earlier output is no longer retained/);
  const quiet = renderToStaticMarkup(panel());
  assert.equal(quiet.includes("TERMINAL OPEN ELSEWHERE"), false);
  assert.equal(quiet.includes("Earlier output"), false);
});
