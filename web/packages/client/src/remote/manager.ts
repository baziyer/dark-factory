import type { HumanRequestItem, PushSubscribeBody } from "../control.js";
import type { ProtocolError } from "../errors.js";
import type { StateView } from "../state.js";
import {
  createBrowserClient,
  SessionError,
  type BrowserClient,
  type BrowserKeyStore,
  type BrowserTimer,
  type SessionStatus,
  type StoredClientKey,
} from "../session.js";
import type { RemoteInvitation } from "./invitation.js";
import { createRelaySocketFactory, relayControllerURL, RELAY_CLOSE, RELAY_DAEMON_MISMATCH, RELAY_TICKET_EXPIRED } from "./relay-socket.js";
import type { RemoteBinding, RemoteStore } from "./store.js";
import { base64urlEncode, parseTicket } from "./tokens.js";

/** One live browser holds a handful of factories, not a fleet. */
export const MAX_CONNECTED_FACTORIES = 8;

export type RemoteFactoryStatus =
  | "offline"
  | "connecting"
  | "pairing"
  | "authenticating"
  | "syncing"
  | "ready"
  | "revoked"
  | "mismatch"
  | "expired"
  | "error";

export type RemoteFactoryView = Readonly<{
  nodeId: string;
  label: string;
  status: RemoteFactoryStatus;
  state?: StateView;
  error?: SessionError | ProtocolError;
}>;

/** One open HumanRequest, tagged with the factory it is waiting inside. */
export type RemoteHumanRequest = Readonly<{ nodeId: string; label: string; request: HumanRequestItem }>;

/**
 * The node answered, but not as the daemon this binding names. It is an
 * identity failure like any other unauthorized, and carries its own type so a
 * caller can tell it from a credential the daemon itself refused.
 */
export class RemoteDaemonMismatchError extends SessionError {
  readonly nodeId: string;
  constructor(nodeId: string) {
    super("unauthorized");
    this.name = "RemoteDaemonMismatchError";
    this.nodeId = nodeId;
  }
}

export type RemoteManagerOptions = {
  store: RemoteStore;
  /** The PWA origin, which the daemon binds into every pair and auth transcript. */
  origin: string;
  webSocket?: typeof WebSocket;
  timer?: BrowserTimer;
  onChange?: () => void;
  /** Unix seconds; the clock a control ticket's deadline is read against. */
  now?: () => number;
};

type Entry = {
  binding: RemoteBinding;
  client?: BrowserClient;
  status: RemoteFactoryStatus;
  state?: StateView;
  error?: SessionError | ProtocolError;
  /** How the last finished dial ended; cleared at the start of the next one. */
  dial?: { opened: boolean; code: number };
  /** Set once this binding must stop dialing, and never cleared. */
  halted?: "revoked" | "mismatch" | "expired";
  generation: number;
};

type PairPending = {
  generation: number;
  persisted: boolean;
  ticket: boolean;
  resolve: () => void;
  reject: (error: unknown) => void;
};

/**
 * Owns one BrowserClient per bound factory and nothing else. Reconnection,
 * correlation and every one-shot effect stay inside the client: this manager
 * never retries a reply, a cancellation or an enqueue, because a repeat of an
 * effect is a second effect. It exposes the clients so a caller uses their own
 * one-shot APIs directly.
 */
export class RemoteManager {
  #options: RemoteManagerOptions;
  #store: RemoteStore;
  #now: () => number;
  #entries = new Map<string, Entry>();
  #pairing = new Map<string, PairPending>();
  #selected: string | undefined;
  #closed = false;

  constructor(options: RemoteManagerOptions) {
    this.#options = options;
    this.#store = options.store;
    this.#now = options.now ?? (() => Math.floor(Date.now() / 1000));
  }

  /** Loads the durable bindings and connects as many as the budget allows. */
  async start(): Promise<void> {
    if (this.#closed) throw new SessionError("closed");
    const bindings = await this.#store.list();
    for (const binding of bindings) {
      if (this.#entries.has(binding.nodeId)) continue;
      this.#entries.set(binding.nodeId, { binding, status: "offline", generation: 0 });
    }
    if (this.#selected === undefined) this.#selected = bindings[0]?.nodeId;
    this.#rebalance();
    this.#changed();
  }

  /** The alert subscription this device holds, if it has turned alerts on. */
  push(): PushSubscribeBody | undefined {
    for (const entry of this.#entries.values()) if (entry.binding.push !== undefined) return { ...entry.binding.push };
    return undefined;
  }

  /**
   * Records this device's alert subscription on every binding and hands it to
   * every factory that is connected now; the rest receive it as they connect.
   * Undefined turns alerts off on this device, though a factory that already
   * holds the subscription keeps pushing until the service refuses it.
   */
  async setPush(subscription: PushSubscribeBody | undefined): Promise<void> {
    if (this.#closed) throw new SessionError("closed");
    for (const entry of this.#entries.values()) {
      if (subscription === undefined) delete entry.binding.push; else entry.binding.push = { ...subscription };
      if (entry.binding.key !== undefined) await this.#store.put(entry.binding);
      this.#sendPush(entry);
    }
    this.#changed();
  }

  #sendPush(entry: Entry): void {
    const subscription = entry.binding.push;
    if (subscription === undefined || entry.status !== "ready") return;
    // A refusal is the factory's to make and nothing this device can repair:
    // a daemon that predates alerts says unsupported and the session lives on.
    void entry.client?.session?.subscribePush(subscription).catch(() => { /* reported by nothing; the next connection tries again */ });
  }

  bindings(): ReadonlyArray<RemoteBinding> {
    return Object.freeze([...this.#entries.values()].map((entry) => ({ ...entry.binding })));
  }

  factories(): ReadonlyArray<RemoteFactoryView> {
    return Object.freeze([...this.#entries.values()].map((entry) => Object.freeze({
      nodeId: entry.binding.nodeId,
      label: entry.binding.label,
      status: entry.status,
      state: entry.state,
      error: entry.error,
    })));
  }

  selected(): string | undefined { return this.#selected; }

  select(nodeId: string): void {
    if (!this.#entries.has(nodeId)) throw new SessionError("not_found");
    if (this.#selected === nodeId) return;
    this.#selected = nodeId;
    this.#rebalance();
    this.#changed();
  }

  /** The client itself, so detail, reply and cancel keep their one-shot fences. */
  client(nodeId: string): BrowserClient | undefined { return this.#entries.get(nodeId)?.client; }

  needsYou(): ReadonlyArray<RemoteHumanRequest> {
    const result: RemoteHumanRequest[] = [];
    for (const entry of this.#entries.values()) {
      if (entry.state === undefined) continue;
      for (const request of entry.state.humanRequests.values()) {
        if (request.status !== "open") continue;
        result.push(Object.freeze({ nodeId: entry.binding.nodeId, label: entry.binding.label, request }));
      }
    }
    result.sort((left, right) => left.request.created_at === right.request.created_at
      ? (left.request.id < right.request.id ? -1 : 1)
      : left.request.created_at < right.request.created_at ? -1 : 1);
    return Object.freeze(result);
  }

  /**
   * Runs the one pairing connection an invitation authorises, then reconnects
   * on the control ticket that pairing minted. It resolves only once the key is
   * durable and that ticket is stored: a pairing that is not both is a pairing
   * this installation could not use again.
   */
  async pair(invitation: RemoteInvitation): Promise<RemoteBinding> {
    if (this.#closed) throw new SessionError("closed");
    const nodeId = invitation.node;
    const previous = this.#entries.get(nodeId);
    if (previous !== undefined) {
      this.#disconnect(previous);
      this.#entries.delete(nodeId);
    }
    const binding: RemoteBinding = {
      // A factory is named by the head of its node id: an invitation carries no
      // label, and this console has no rename.
      nodeId,
      label: nodeId.slice(0, 8),
      relayOrigin: invitation.relay,
      host: invitation.host,
      daemonId: invitation.daemon,
    };
    // A device that already asks to be woken asks every factory it pairs with.
    const push = this.push();
    if (push !== undefined) binding.push = push;
    const entry: Entry = { binding, status: "pairing", generation: 0 };
    this.#entries.set(nodeId, entry);
    const generation = ++entry.generation;
    let pending!: PairPending;
    const settled = new Promise<void>((resolve, reject) => {
      pending = { generation, persisted: false, ticket: false, resolve, reject };
      this.#pairing.set(nodeId, pending);
    });
    entry.client = this.#build(entry, generation, () => invitation.ticket, undefined, invitation.challenge);
    this.#changed();
    void entry.client.connect().catch(() => { /* onError and onStatus are the signals */ });
    try {
      await settled;
      // Both halves are in hand only here — the ticket arrives while the
      // session is still saving the key — so this is where a ticket that names
      // some other device is caught, and it is caught before anything is
      // written: the one write a pairing makes stores a key and a ticket that
      // belong to each other, or it does not happen.
      const { relayTicket, publicKeySEC1 } = entry.binding;
      if (relayTicket === undefined || publicKeySEC1 === undefined || parseTicket(relayTicket).device !== base64urlEncode(publicKeySEC1)) throw new SessionError("unauthorized");
      await this.#store.put(entry.binding);
    } catch (error) {
      this.#disconnect(entry);
      // A later pairing for this node may already hold the slot; only the one
      // that still has it puts anything back. Nothing durable was written, so a
      // refused pairing costs this device nothing: the identity it already held
      // for this node is still the one it has.
      if (this.#entries.get(nodeId) === entry) {
        this.#entries.delete(nodeId);
        if (previous !== undefined) this.#entries.set(nodeId, { binding: previous.binding, status: "offline", generation: 0 });
        this.#rebalance();
      }
      this.#changed();
      throw error;
    }
    // A pairing that started while this one was being written owns the slot
    // now, and its record is not this one's to remove.
    if (this.#pairing.get(nodeId) === pending) this.#pairing.delete(nodeId);
    // The pair ticket is spent and the pairing socket has done its one job.
    this.#disconnect(entry);
    this.#selected = nodeId;
    this.#rebalance();
    this.#changed();
    return { ...entry.binding };
  }

  async forget(nodeId: string): Promise<void> {
    const entry = this.#entries.get(nodeId);
    if (entry !== undefined) this.#disconnect(entry);
    this.#entries.delete(nodeId);
    if (this.#selected === nodeId) this.#selected = this.#entries.keys().next().value;
    await this.#store.forgetBinding(nodeId);
    if (!this.#closed) this.#rebalance();
    this.#changed();
  }

  async forgetDevice(): Promise<void> {
    for (const entry of this.#entries.values()) this.#disconnect(entry);
    for (const pending of this.#pairing.values()) pending.reject(new SessionError("closed"));
    this.#pairing.clear();
    this.#entries.clear();
    this.#selected = undefined;
    await this.#store.forgetDevice();
    this.#changed();
  }

  close(): void {
    if (this.#closed) return;
    this.#closed = true;
    for (const entry of this.#entries.values()) this.#disconnect(entry);
    for (const pending of this.#pairing.values()) pending.reject(new SessionError("closed"));
    this.#pairing.clear();
    this.#changed();
  }

  #build(entry: Entry, generation: number, ticket: () => string, key: CryptoKey | undefined, challenge?: string): BrowserClient {
    const nodeId = entry.binding.nodeId;
    // The binding this manager holds is the whole identity, so the session
    // reads it from memory and hands its half back the same way. Nothing is
    // written here: pairing writes once, at the end, or not at all.
    const keyStore: BrowserKeyStore = {
      load: async () => {
        const { clientId, publicKeySEC1, key: identity, capabilities } = entry.binding;
        if (clientId === undefined || publicKeySEC1 === undefined || identity === undefined || capabilities === undefined) return null;
        return { clientId, publicKeySEC1: publicKeySEC1.slice(), key: identity, capabilities };
      },
      save: async (value: StoredClientKey) => {
        // The same refusal IndexedDBKeyStore makes: an exportable key is not an
        // identity this installation is willing to hold.
        if (value.key.extractable) throw new SessionError("storage_unavailable");
        this.#paired(nodeId, generation, value);
      },
    };
    const relaySocket = createRelaySocketFactory({
      relayOrigin: entry.binding.relayOrigin,
      nodeId,
      ticket,
      key,
      expectedDaemonId: entry.binding.daemonId,
      now: this.#now,
      webSocket: this.#options.webSocket,
      onTicket: (value: string) => { void this.#relayTicket(nodeId, generation, value); },
      onRelayClose: (code: number, opened: boolean, local: boolean) => this.#relayClosed(nodeId, generation, code, opened, local),
    });
    // BrowserClient redials by itself, without asking this manager, so each
    // dial clears the record: one generation's outcome never decides another's.
    const socketFactory = (url: string) => {
      const live = this.#live(nodeId, generation);
      if (live !== undefined) live.dial = undefined;
      return relaySocket(url);
    };
    return createBrowserClient({
      url: relayControllerURL(entry.binding.relayOrigin, nodeId),
      host: entry.binding.host,
      origin: this.#options.origin,
      challenge,
      keyStore,
      socketFactory,
      timer: this.#options.timer,
      onStatus: (status: SessionStatus) => this.#status(nodeId, generation, status),
      onState: (state: StateView) => this.#state(nodeId, generation, state),
      onError: (error: SessionError | ProtocolError) => this.#error(nodeId, generation, error),
    });
  }

  #connect(entry: Entry): void {
    const ticket = entry.binding.relayTicket;
    const key = entry.binding.key;
    if (ticket === undefined || key === undefined) {
      // A binding with no control ticket cannot dial; only a fresh invitation
      // can repair it, so it is reported rather than retried.
      entry.status = "error";
      entry.error = new SessionError("unauthorized");
      return;
    }
    const generation = ++entry.generation;
    const nodeId = entry.binding.nodeId;
    entry.status = "connecting";
    entry.error = undefined;
    entry.dial = undefined;
    entry.client = this.#build(entry, generation, () => this.#currentTicket(nodeId), key);
    void entry.client.connect().catch(() => { /* onError and onStatus are the signals */ });
  }

  #disconnect(entry: Entry): void {
    // A pairing whose socket is taken away can never settle: eviction, forget,
    // and a second pair() for the same node all arrive through this one door.
    // Only the entry that still holds the slot owns the pairing waiting on it.
    const nodeId = entry.binding.nodeId;
    if (this.#entries.get(nodeId) === entry) {
      const pending = this.#pairing.get(nodeId);
      this.#pairing.delete(nodeId);
      pending?.reject(new SessionError("closed"));
    }
    entry.generation += 1;
    const client = entry.client;
    entry.client = undefined;
    entry.state = undefined;
    if (entry.halted === undefined) entry.status = "offline";
    try { client?.close(); } catch { /* closing is finite */ }
  }

  /** Selected first, then binding order, up to the concurrent-connection budget. */
  #rebalance(): void {
    if (this.#closed) return;
    const selected = this.#selected === undefined ? undefined : this.#entries.get(this.#selected);
    const order: Entry[] = [];
    if (selected !== undefined) order.push(selected);
    for (const entry of this.#entries.values()) if (entry !== selected) order.push(entry);
    const wanted = new Set<Entry>();
    for (const entry of order) {
      if (entry.halted !== undefined) continue;
      if (wanted.size >= MAX_CONNECTED_FACTORIES) break;
      wanted.add(entry);
    }
    for (const entry of this.#entries.values()) {
      if (wanted.has(entry)) { if (entry.client === undefined) this.#connect(entry); }
      else if (entry.client !== undefined) this.#disconnect(entry);
    }
  }

  #currentTicket(nodeId: string): string {
    const ticket = this.#entries.get(nodeId)?.binding.relayTicket;
    if (ticket === undefined) throw new SessionError("unauthorized");
    return ticket;
  }

  #status(nodeId: string, generation: number, status: SessionStatus): void {
    const entry = this.#live(nodeId, generation);
    if (entry === undefined) return;
    switch (status) {
      case "idle":
      case "connecting":
        entry.status = "connecting";
        break;
      case "authenticating":
        entry.status = entry.binding.clientId === undefined ? "pairing" : "authenticating";
        break;
      case "syncing":
        entry.status = "syncing";
        break;
      case "ready":
        entry.status = "ready";
        entry.error = undefined;
        entry.dial = undefined;
        this.#sendPush(entry);
        // A pairing that reached the factory but never received the relay's
        // next ticket could not be used again, so it is refused here rather
        // than left waiting on a frame that is not coming.
        this.#rejectPairing(nodeId, generation, new SessionError("pairing_uncertain"));
        break;
      case "closed":
        // The session discards its snapshot on close and so does this manager:
        // there is no state without a live session to keep it true.
        entry.state = undefined;
        entry.status = this.#closedStatus(entry);
        this.#rejectPairing(nodeId, generation, entry.error ?? new SessionError("connection"));
        break;
    }
    this.#changed();
  }

  // A halt bumps the generation, so a halted entry never reaches here.
  #closedStatus(entry: Entry): RemoteFactoryStatus {
    const offline = dialStatus(entry);
    if (offline !== undefined) return offline;
    const error = entry.error;
    // BrowserClient retries a connection fault by itself; anything it will not
    // retry is a state a person has to see.
    if (error !== undefined && !(error instanceof SessionError && (error.code === "connection" || error.retryable))) return "error";
    return "connecting";
  }

  #state(nodeId: string, generation: number, state: StateView): void {
    const entry = this.#live(nodeId, generation);
    if (entry === undefined) return;
    entry.state = state;
    this.#changed();
  }

  #error(nodeId: string, generation: number, error: SessionError | ProtocolError): void {
    const entry = this.#live(nodeId, generation);
    if (entry === undefined) return;
    entry.error = error;
    this.#rejectPairing(nodeId, generation, error);
    this.#changed();
  }

  #relayClosed(nodeId: string, generation: number, code: number, opened: boolean, local: boolean): void {
    const entry = this.#live(nodeId, generation);
    if (entry === undefined) return;
    entry.dial = { opened, code };
    // Revocation is the daemon's decision and a mismatch is a broken binding.
    // Neither is retried: another dial would only present a credential its
    // owner withdrew, or reach a daemon this binding was never made with.
    if (code === RELAY_CLOSE.controllerRevoked) this.#halt(entry, "revoked", generation, new SessionError("unauthorized"));
    // The two refusals this browser makes itself: a peer sending the same code
    // is just another close, and never halts a binding.
    else if (local && code === RELAY_DAEMON_MISMATCH) this.#halt(entry, "mismatch", generation, new RemoteDaemonMismatchError(nodeId));
    else if (local && code === RELAY_TICKET_EXPIRED) this.#halt(entry, "expired", generation, new SessionError("unauthorized"));
    // The close arrives before or after the session reports one; both read the
    // same record, so the order they land in cannot change the answer.
    else entry.status = dialStatus(entry) ?? entry.status;
    this.#changed();
  }

  #halt(entry: Entry, reason: "revoked" | "mismatch" | "expired", generation: number, error: SessionError): void {
    entry.halted = reason;
    entry.status = reason;
    entry.state = undefined;
    entry.error = error;
    const client = entry.client;
    entry.client = undefined;
    // The bump fences every callback of the session being closed here, so the
    // halted status is the last word rather than a passing one.
    entry.generation += 1;
    try { client?.close(); } catch { /* closing is finite */ }
    this.#rejectPairing(entry.binding.nodeId, generation, error);
    // A binding that will never dial again must not hold a connection slot.
    this.#rebalance();
  }

  async #relayTicket(nodeId: string, generation: number, ticket: string): Promise<void> {
    const entry = this.#live(nodeId, generation);
    if (entry === undefined) return;
    let parsed;
    try { parsed = parseTicket(ticket); } catch { return; }
    if (parsed.purpose !== "control" || parsed.node !== nodeId) return;
    // A ticket names the one device key whose proof it accepts. One naming
    // another device could never be presented from here, so it is dropped and
    // the ticket this binding already holds is kept.
    const publicKey = entry.binding.publicKeySEC1;
    if (publicKey !== undefined && parsed.device !== base64urlEncode(publicKey)) return;
    entry.binding = { ...entry.binding, relayTicket: ticket };
    const pending = this.#pairing.get(nodeId);
    // A pairing's ticket is written with the key it belongs to, once, by pair().
    if (pending !== undefined && pending.generation === generation) {
      pending.ticket = true;
      this.#settle(nodeId);
      return;
    }
    // An established binding must survive a reload holding the newest ticket.
    try { await this.#store.put(entry.binding); } catch { /* the next authentication mints another */ }
  }

  #paired(nodeId: string, generation: number, value: StoredClientKey): void {
    const entry = this.#live(nodeId, generation);
    if (entry === undefined) return;
    entry.binding = {
      ...entry.binding,
      clientId: value.clientId,
      publicKeySEC1: value.publicKeySEC1.slice(),
      key: value.key,
      capabilities: value.capabilities,
    };
    const pending = this.#pairing.get(nodeId);
    if (pending !== undefined && pending.generation === generation) {
      pending.persisted = true;
      this.#settle(nodeId);
    }
    this.#changed();
  }

  #settle(nodeId: string): void {
    const pending = this.#pairing.get(nodeId);
    if (pending !== undefined && pending.persisted && pending.ticket) pending.resolve();
  }

  #rejectPairing(nodeId: string, generation: number, error: unknown): void {
    const pending = this.#pairing.get(nodeId);
    if (pending === undefined || pending.generation !== generation) return;
    if (pending.persisted && pending.ticket) return;
    pending.reject(error);
  }

  #live(nodeId: string, generation: number): Entry | undefined {
    const entry = this.#entries.get(nodeId);
    return entry === undefined || entry.generation !== generation ? undefined : entry;
  }

  #changed(): void {
    try { this.#options.onChange?.(); } catch { /* consumer callbacks cannot break ownership */ }
  }
}

export function createRemoteManager(options: RemoteManagerOptions): RemoteManager { return new RemoteManager(options); }

/**
 * What one finished dial says on its own. Every relay refusal — no host, a
 * ticket it will not take, a limit — is an HTTP status before the upgrade, so
 * the browser reports only an error and a codeless close: a dial that never
 * opened is this factory being out of reach, not a connection in progress.
 */
function dialStatus(entry: Entry): "offline" | undefined {
  const dial = entry.dial;
  if (dial === undefined) return undefined;
  return dial.opened && dial.code !== RELAY_CLOSE.factoryOffline ? undefined : "offline";
}
