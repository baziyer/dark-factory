const clientModule = `
const counters = globalThis.__darkFactoryStrictProbe ??= { clients: 0, states: 0, sessionCloses: 0, resolves: 0, opens: 0, attaches: 0, acquires: 0, detaches: 0, terminals: 0, disposes: 0 };
export class SessionError extends Error { constructor(code) { super(code); this.name = "SessionError"; this.code = code; this.retryable = false; } }
export class RemoteDaemonMismatchError extends SessionError { constructor(nodeId) { super("unauthorized"); this.name = "RemoteDaemonMismatchError"; this.nodeId = nodeId; } }
export class ProtocolError extends Error { constructor(code) { super(code); this.name = "ProtocolError"; this.code = code; } }
export const MAX_TERMINAL_PAYLOAD = 4096;
export const MAX_TERMINAL_ROWS = 4096;
export const MAX_TERMINAL_COLS = 4096;
export const MAX_TASK_INSTRUCTION_BYTES = 32768;
export const CAPABILITIES = { observe: 1, private_human_request_detail: 2, human_actions: 4, terminal_input: 8 };
export function consumePairingChallenge() { return null; }
// The public barrel also carries the remote console. This probe never mounts
// it, so these exist only so the module graph resolves.
export function consumeInvitation() { return null; }
export function parseInvitation() { throw new SessionError("invalid_request"); }
export function createRemoteStore() { return { list: async () => [], get: async () => null, put: async () => {}, forgetBinding: async () => {}, forgetDevice: async () => {} }; }
export function createRemoteManager() { return { start: async () => {}, factories: () => [], selected: () => undefined, select: () => {}, client: () => undefined, needsYou: () => [], pair: async () => {}, forget: async () => {}, forgetDevice: async () => {}, close: () => {} }; }
const agent = { id: "21".repeat(16), project_id: "11".repeat(16), name: "Strict Agent", role: "worker", paused: false, revision: 1n };
const task = { id: "31".repeat(16), project_id: agent.project_id, assigned_agent_id: agent.id, title: "Strict task", status: "running", priority: 0, revision: 1n };
const state = { head: 1n, factory: { dispatch_enabled: true, capacity: 1, active_runs: 1, revision: 1n }, projects: new Map([[agent.project_id, { id: agent.project_id, name: "Strict Project", revision: 1n }]]), agents: new Map([[agent.id, agent]]), tasks: new Map([[task.id, task]]), humanRequests: new Map() };
export function createBrowserClient(options) {
  counters.clients += 1;
  let closed = false;
  const session = {
    // A daemon older than the console edit contract refuses all three; the
    // console must still mount, show its floor, and open a terminal.
    getTopology: async () => { throw new SessionError("not_found"); },
    updateAgent: async () => { throw new SessionError("not_found"); },
    updateTask: async () => { throw new SessionError("not_found"); },
    resolveAgentTerminal: async () => { counters.resolves += 1; return Object.freeze({}); },
    openTerminal: (_target, callbacks) => {
      counters.opens += 1;
      const handle = { writable: true, attach: async () => { counters.attaches += 1; return {}; }, acquireInput: async () => { counters.acquires += 1; return {}; }, detach: async () => { counters.detaches += 1; callbacks.onClose?.(); }, sendInput: async (bytes) => ({ status: "accepted", accepted_bytes: BigInt(bytes.length) }), resize: async (rows, cols) => ({ rows, cols }), close: () => callbacks.onClose?.(new SessionError("closed")) };
      return handle;
    },
    close: () => {
      if (closed) return;
      closed = true;
      counters.sessionCloses += 1;
      options.onStatus?.("closed");
    },
  };
  return {
    session,
    connect: () => { counters.states += 1; options.onState?.(state); options.onStatus?.("ready"); return Promise.resolve(); },
    close: () => session.close(),
  };
}
`;

const xtermModule = `
const counters = globalThis.__darkFactoryStrictProbe ??= { clients: 0, states: 0, sessionCloses: 0, resolves: 0, opens: 0, attaches: 0, acquires: 0, detaches: 0, terminals: 0, disposes: 0 };
export class Terminal {
  rows = 24;
  cols = 80;
  constructor() { counters.terminals += 1; }
  loadAddon(addon) { addon.activate?.(this); }
  open() {}
  onData() { return { dispose() {} }; }
  onBinary() { return { dispose() {} }; }
  onResize() { return { dispose() {} }; }
  write(_payload, done) { done?.(); }
  focus() {}
  dispose() { counters.disposes += 1; }
}
`;

const fitModule = `export class FitAddon { activate(terminal) { this.terminal = terminal; } fit() {} }`;

function dataModule(source) {
  return `data:text/javascript,${encodeURIComponent(source)}`;
}

export async function resolve(specifier, context, nextResolve) {
  if (specifier === "@dark-factory/client") return { url: dataModule(clientModule), shortCircuit: true };
  if (specifier === "@xterm/xterm") return { url: dataModule(xtermModule), shortCircuit: true };
  if (specifier === "@xterm/addon-fit") return { url: dataModule(fitModule), shortCircuit: true };
  return nextResolve(specifier, context);
}
