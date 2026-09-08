import assert from "node:assert/strict";
import test from "node:test";
import { createTerminalSurface, loadXtermModules, startXtermTerminal } from "../dist/src/xterm-terminal.js";

function deferred() {
  let resolve;
  let reject;
  const promise = new Promise((accept, refuse) => { resolve = accept; reject = refuse; });
  return { promise, resolve, reject };
}

function tick() { return new Promise((resolve) => setTimeout(resolve, 0)); }

function fakeModules({ throwOnDispose = false } = {}) {
  const state = {
    terminals: 0,
    disposes: 0,
    addonDisposes: 0,
    fitCalls: 0,
    writes: [],
    resizeListeners: new Set(),
  };
  class Terminal {
    rows = 24;
    cols = 80;
    constructor() { state.terminals += 1; }
    loadAddon(addon) { this.addon = addon; addon.activate?.(this); }
    open() {}
    onData() { return { dispose() { if (throwOnDispose) throw new Error("data"); } }; }
    onBinary() { return { dispose() { if (throwOnDispose) throw new Error("binary"); } }; }
    onResize(callback) { this.resizeCallback = callback; return { dispose() { if (throwOnDispose) throw new Error("resize"); } }; }
    resize(cols, rows) { this.cols = cols; this.rows = rows; this.resizeCallback?.({ cols, rows }); }
    write(payload, callback) { state.writes.push(payload); this.writeCallback = callback; }
    focus() {}
    dispose() { state.disposes += 1; }
  }
  class FitAddon {
    fit() { state.fitCalls += 1; this.terminal.resize(100, 30); }
    dispose() { state.addonDisposes += 1; }
    activate(terminal) { this.terminal = terminal; }
  }
  return { modules: { Terminal, FitAddon }, state };
}

function fakeWindow(state) {
  return {
    addEventListener: (_type, listener) => state.resizeListeners.add(listener),
    removeEventListener: (_type, listener) => state.resizeListeners.delete(listener),
  };
}

test("surface abort is permanent and late xterm callbacks cannot settle it", async () => {
  let callback;
  const terminal = { write: (_payload, done) => { callback = done; } };
  const surface = createTerminalSurface(terminal);
  const pending = surface.write(new Uint8Array([1]));
  surface.abort();
  surface.abort();
  callback();
  await assert.rejects(pending, /disposed/);
  await assert.rejects(surface.write(new Uint8Array([2])), /disposed/);
});

test("module completion after unmount does not construct or publish a terminal", async () => {
  const modules = fakeModules();
  const loading = deferred();
  let surfaces = 0;
  const stop = startXtermTerminal(() => ({}), () => loading.promise, { onSurface: () => { surfaces += 1; }, onError: () => {} }, fakeWindow(modules.state));
  stop();
  loading.resolve(modules.modules);
  await tick();
  assert.equal(modules.state.terminals, 0);
  assert.equal(surfaces, 0);
});

test("a stale loader rejection cannot report into a StrictMode-shaped new owner", async () => {
  const oldLoading = deferred();
  const newLoading = deferred();
  const oldModules = fakeModules();
  const newModules = fakeModules();
  const failures = [];
  const stopOld = startXtermTerminal(() => ({}), () => oldLoading.promise, { onSurface: () => {}, onError: () => failures.push("old") }, fakeWindow(oldModules.state));
  stopOld();
  startXtermTerminal(() => ({}), () => newLoading.promise, { onSurface: () => {}, onError: () => failures.push("new") }, fakeWindow(newModules.state));
  oldLoading.reject(new Error("old module"));
  await tick();
  assert.deepEqual(failures, []);
  newLoading.reject(new Error("new module"));
  await tick();
  assert.deepEqual(failures, ["new"]);
});

test("the real dynamic modules expose usable constructors in Node", async () => {
  const modules = await loadXtermModules();
  assert.equal(typeof modules.Terminal, "function");
  assert.equal(typeof modules.FitAddon, "function");
});

test("module or mount failure is reported to the finite owner", async () => {
  const modules = fakeModules();
  let failures = 0;
  startXtermTerminal(() => ({}), () => Promise.reject(new Error("module")), { onSurface: () => {}, onError: () => { failures += 1; } }, fakeWindow(modules.state));
  await tick();
  assert.equal(failures, 1);

  const broken = fakeModules();
  broken.modules.Terminal.prototype.open = () => { throw new Error("mount"); };
  const surfaces = [];
  startXtermTerminal(() => ({}), async () => broken.modules, { onSurface: (surface) => surfaces.push(surface), onError: () => { failures += 1; } }, fakeWindow(broken.state));
  await tick();
  assert.equal(failures, 2);
  assert.equal(broken.state.disposes, 1);
  assert.deepEqual(surfaces, []);
});

test("initial fit reports one size and each later window fit reports one size", async () => {
  const modules = fakeModules();
  const resized = [];
  let surface;
  const windowTarget = fakeWindow(modules.state);
  const stop = startXtermTerminal(() => ({}), async () => modules.modules, {
    onSurface: (value) => { surface = value; },
    onError: () => {},
    onResize: (rows, cols) => resized.push([rows, cols]),
  }, windowTarget);
  await tick();
  assert.equal(modules.state.fitCalls, 1);
  assert.deepEqual(resized, [[30, 100]]);
  for (const listener of modules.state.resizeListeners) listener();
  assert.equal(modules.state.fitCalls, 2);
  assert.deepEqual(resized, [[30, 100], [30, 100]]);
  assert.ok(surface);
  stop();
  assert.equal(modules.state.resizeListeners.size, 0);
  assert.equal(modules.state.disposes, 1);
  assert.equal(modules.state.addonDisposes, 0);
});

test("cleanup continues when individual resources or consumer callbacks throw", async () => {
  const modules = fakeModules({ throwOnDispose: true });
  const surfaces = [];
  const stop = startXtermTerminal(() => ({}), async () => modules.modules, {
    onSurface: (surface) => {
      surfaces.push(surface);
      if (surface === undefined) throw new Error("consumer");
    },
    onError: () => {},
  }, fakeWindow(modules.state));
  await tick();
  stop();
  assert.equal(surfaces.length, 2);
  assert.equal(modules.state.disposes, 1);
});
