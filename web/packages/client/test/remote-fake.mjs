// A fake relay and a fake factoryd behind it. The real BrowserSession and
// BrowserClient are driven through the real relay socket wrapper, so every
// test here exercises the same code the PWA runs.
import {
  CAPABILITIES,
  base64urlEncode,
  decodeClientControl,
  encodeAuthResult,
  encodeHello,
  encodePairResult,
  encodeServerControl,
  encodeStateSnapshot,
} from "../dist/src/index.js";

export const ALL_CAPABILITIES = CAPABILITIES.observe | CAPABILITIES.private_human_request_detail | CAPABILITIES.human_actions | CAPABILITIES.terminal_input | CAPABILITIES.administration;
export const RELAY_ORIGIN = "wss://relay.example";
export const PWA_ORIGIN = "https://app.darkfactory.build";

export const tick = () => new Promise((resolve) => setTimeout(resolve, 0));
export async function settle(rounds = 8) { for (let index = 0; index < rounds; index++) await tick(); }

export function nodeId(character = "a") { return character.repeat(32); }

export function bytes(length, first) {
  const value = new Uint8Array(length);
  if (first !== undefined) value[0] = first;
  return value;
}

export function base64url(text) { return base64urlEncode(new TextEncoder().encode(text)); }

const hexBytes = (text) => new Uint8Array(text.match(/../g).map((pair) => parseInt(pair, 16)));

/** A ticket the relay would sign. Nothing here verifies the signature. */
export function mintTicket({ node, purpose, ticketId = base64urlEncode(bytes(16, 1)), controller = base64urlEncode(bytes(16, 2)), expires = 4_000_000_000, device, signature = base64urlEncode(bytes(64)) }) {
  const payload = { node, controller, purpose, ticket: ticketId, expires };
  if (purpose === "control") payload.device = device ?? base64urlEncode(bytes(65, 4));
  return `${base64url(JSON.stringify(payload))}.${signature}`;
}

/** The link a daemon prints: the marker, then ordinary query members. */
export function invitationFragment(members) {
  return `#df_remote&${new URLSearchParams(members)}`;
}

export function invitationMembers(overrides = {}) {
  const node = overrides.node ?? nodeId("a");
  return {
    relay: RELAY_ORIGIN,
    node,
    daemon: "22".repeat(16),
    host: "127.0.0.1:43123",
    challenge: "11".repeat(32),
    ticket: mintTicket({ node, purpose: "pair" }),
    expires: 4_000_000_000,
    ...overrides,
  };
}

export function humanRequest(id, overrides = {}) {
  return {
    id,
    project_id: "33".repeat(16),
    agent_id: "44".repeat(16),
    task_id: "55".repeat(16),
    created_at: 10n,
    updated_at: 10n,
    revision: 1n,
    kind: "question",
    status: "open",
    reply_max_bytes: 8192,
    can_reply: true,
    ...overrides,
  };
}

export function snapshotBody(head, overrides = {}) {
  return {
    head,
    factory: { dispatch_enabled: true, capacity: 8, active_runs: 0, revision: head === 0n ? 1n : head },
    projects: [],
    agents: [],
    tasks: [],
    human_requests: [],
    ...overrides,
  };
}

/** One factoryd reachable through the relay under one node id. */
export class FakeFactory {
  constructor(options = {}) {
    this.node = options.node ?? nodeId("a");
    this.daemonId = options.daemonId ?? "22".repeat(16);
    this.helloDaemonId = options.helloDaemonId ?? this.daemonId;
    this.clientId = options.clientId ?? "55".repeat(16);
    this.capabilities = options.capabilities ?? ALL_CAPABILITIES;
    this.snapshot = options.snapshot ?? snapshotBody(1n);
    this.online = options.online ?? true;
    this.sockets = [];
    this.live = new Set();
    this.frames = [];
    this.tickets = 0;
    this.pinnedDevice = options.device;
    this.withTicket = options.withTicket ?? true;
  }

  controlTicket() {
    this.tickets += 1;
    return mintTicket({ node: this.node, purpose: "control", ticketId: base64urlEncode(bytes(16, this.tickets)), device: this.device });
  }

  dial(socket) {
    this.sockets.push(socket);
    // A relay with no host for this node refuses before the upgrade, so the
    // browser sees an error and a codeless close and never an open.
    if (!this.online) { socket.refuse(); return; }
    this.live.add(socket);
    socket.open();
    socket.emit(encodeHello({ daemon_id: this.helloDaemonId, boot_id: "33".repeat(16), connection_nonce: `${this.sockets.length % 10}`.repeat(64) }));
  }

  receive(socket, wire) {
    const frame = decodeClientControl(wire);
    this.frames.push(frame);
    // The relay binds every control ticket to the device key that proved the
    // pairing, so this daemon mints them the same way.
    if (frame.type === "PAIR_PROVE") this.device = this.pinnedDevice ?? base64urlEncode(hexBytes(frame.body.public_key_sec1));
    if (frame.type === "PAIR_PROVE") this.#prove(socket, encodePairResult(frame.id, { client_id: this.clientId, capabilities: this.capabilities }));
    if (frame.type === "AUTH_PROVE") this.#prove(socket, encodeAuthResult(frame.id, { client_id: this.clientId, capabilities: this.capabilities }));
    if (frame.type === "STATE_GET") socket.emit(encodeStateSnapshot(frame.id, this.snapshot));
    if (frame.type === "PUSH_SUBSCRIBE") socket.emit(encodeServerControl({ type: "PUSH_SUBSCRIBE_RESULT", id: frame.id, body: {} }));
  }

  /** The result untouched, then the next control ticket in its own frame. */
  #prove(socket, wire) {
    socket.emit(wire);
    if (this.withTicket) socket.emit(JSON.stringify({ type: "RELAY_TICKET", ticket: this.controlTicket() }));
  }

  dropAll(code = 4001, reason = "offline") {
    this.online = code === 4000;
    for (const socket of [...this.live]) socket.drop(code, reason);
    this.live.clear();
  }
}

export class FakeRelay {
  constructor() {
    this.routes = new Map();
    this.sockets = [];
    const relay = this;
    this.WebSocket = class FakeWebSocket {
      static CONNECTING = 0;
      static OPEN = 1;
      static CLOSING = 2;
      static CLOSED = 3;
      constructor(url, protocols) {
        this.url = url;
        this.protocols = protocols;
        this.readyState = 0;
        this.sent = [];
        this.closes = [];
        this.binaryType = "blob";
        this.onopen = null;
        this.onmessage = null;
        this.onerror = null;
        this.onclose = null;
        relay.dial(this);
      }
      send(data) {
        if (this.readyState !== 1) throw new Error("socket is not open");
        this.sent.push(data);
        this.factory?.receive(this, data);
      }
      close(code = 1000, reason = "") {
        this.closes.push({ code, reason });
        if (this.readyState === 3) return;
        this.readyState = 3;
        this.factory?.live.delete(this);
      }
      open() { this.readyState = 1; queueMicrotask(() => this.onopen?.({})); }
      emit(data) { queueMicrotask(() => { if (this.readyState === 1) this.onmessage?.({ data }); }); }
      drop(code, reason) {
        if (this.readyState === 3) return;
        this.readyState = 3;
        this.onclose?.({ code, reason });
      }
      refuse() {
        if (this.readyState === 3) return;
        this.readyState = 3;
        this.onerror?.({});
        this.onclose?.({});
      }
    };
  }

  add(factory) {
    this.routes.set(`${RELAY_ORIGIN}/controller/${factory.node}`, factory);
    return factory;
  }

  dial(socket) {
    this.sockets.push(socket);
    const factory = this.routes.get(socket.url);
    socket.factory = factory;
    queueMicrotask(() => {
      if (factory === undefined) { socket.drop(4403, "no route"); return; }
      factory.dial(socket);
    });
  }

  /** Every socket dialed for one node, oldest first. */
  for(node) { return this.sockets.filter((socket) => socket.url.endsWith(`/${node}`)); }
}

export class VirtualTimer {
  now = 0;
  next = 1;
  tasks = new Map();
  delays = [];
  setTimeout(callback, delay) {
    const id = this.next++;
    this.tasks.set(id, { at: this.now + delay, callback });
    this.delays.push(delay);
    return id;
  }
  clearTimeout(id) { this.tasks.delete(id); }
  advance(milliseconds) {
    this.now += milliseconds;
    while (true) {
      const due = [...this.tasks.entries()].filter(([, task]) => task.at <= this.now).sort((a, b) => a[1].at - b[1].at)[0];
      if (due === undefined) return;
      this.tasks.delete(due[0]);
      due[1].callback();
    }
  }
}
