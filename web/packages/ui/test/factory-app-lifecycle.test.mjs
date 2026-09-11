import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import test from "node:test";
import { createElement, StrictMode } from "react";
import { act, create } from "react-test-renderer";
import { FactoryApp } from "../dist/src/index.js";
import { browserEndpoint } from "../dist/src/factory-app-controller.js";

function runStrictCompositionProbe() {
  const loader = new URL("./fixtures/strict-composition-loader.mjs", import.meta.url);
  const probe = new URL("./fixtures/strict-composition-probe.mjs", import.meta.url);
  return new Promise((resolve, reject) => {
    const child = spawn(process.execPath, ["--experimental-loader", loader.pathname, probe.pathname], { stdio: ["ignore", "pipe", "pipe"] });
    let stdout = "";
    let stderr = "";
    child.stdout.setEncoding("utf8");
    child.stderr.setEncoding("utf8");
    child.stdout.on("data", (chunk) => { stdout += chunk; });
    child.stderr.on("data", (chunk) => { stderr += chunk; });
    child.on("error", reject);
    const timeout = setTimeout(() => child.kill("SIGKILL"), 5_000);
    child.on("close", (code, signal) => { clearTimeout(timeout); resolve({ code, signal, stdout, stderr }); });
  });
}

test("the default and isolated browser ports have exact loopback endpoints", () => {
  assert.deepEqual(browserEndpoint(), { url: "ws://127.0.0.1:43123/browser", host: "127.0.0.1:43123" });
  assert.deepEqual(browserEndpoint(45678), { url: "ws://127.0.0.1:45678/browser", host: "127.0.0.1:45678" });
  for (const port of [0, 65536, 1.5]) assert.throws(() => browserEndpoint(port));
});

test("StrictMode mounts the public FactoryApp and closes each owned BrowserClient once", async () => {
  const previousWindow = Object.getOwnPropertyDescriptor(globalThis, "window");
  const previousWebSocket = Object.getOwnPropertyDescriptor(globalThis, "WebSocket");
  const previousAct = globalThis.IS_REACT_ACT_ENVIRONMENT;
  const historyState = { route: "factory" };
  const sockets = [];
  const statuses = [];
  class FakeWebSocket {
    constructor(url) {
      this.url = url;
      this.closeCount = 0;
      sockets.push(this);
    }

    close() {
      this.closeCount += 1;
    }
  }
  Object.defineProperty(globalThis, "window", {
    configurable: true,
    value: {
      location: { origin: "https://app.darkfactory.build", hash: "", pathname: "/factory", search: "" },
      history: { state: historyState, replaceState: () => {} },
    },
  });
  Object.defineProperty(globalThis, "WebSocket", { configurable: true, value: FakeWebSocket });
  globalThis.IS_REACT_ACT_ENVIRONMENT = true;

  let renderer;
  try {
    await act(async () => {
      renderer = create(createElement(StrictMode, null, createElement(FactoryApp, { browserPort: 45678, onStatusChange: (status) => statuses.push(status) })));
    });
    assert.equal(sockets.length, 2);
    assert.deepEqual(sockets.map((socket) => ({ url: socket.url, closeCount: socket.closeCount })), [
      { url: "ws://127.0.0.1:45678/browser", closeCount: 1 },
      { url: "ws://127.0.0.1:45678/browser", closeCount: 0 },
    ]);
    assert.deepEqual(statuses, [{ status: "connecting" }, { status: "connecting" }]);

    await act(async () => { renderer.unmount(); });
    assert.deepEqual(sockets.map((socket) => socket.closeCount), [1, 1]);
  } finally {
    if (renderer !== undefined) renderer.unmount();
    if (previousWindow === undefined) delete globalThis.window;
    else Object.defineProperty(globalThis, "window", previousWindow);
    if (previousWebSocket === undefined) delete globalThis.WebSocket;
    else Object.defineProperty(globalThis, "WebSocket", previousWebSocket);
    globalThis.IS_REACT_ACT_ENVIRONMENT = previousAct;
  }
});

test("StrictMode selected-agent composition owns one real terminal surface", async () => {
  const result = await runStrictCompositionProbe();
  assert.equal(result.signal, null, result.stderr);
  assert.equal(result.code, 0, `${result.stdout}\n${result.stderr}`);
  assert.match(result.stdout, /"opens":1/);
  assert.match(result.stdout, /"attaches":1/);
  assert.match(result.stdout, /"acquires":1/);
  assert.match(result.stdout, /"detaches":0/);
});
