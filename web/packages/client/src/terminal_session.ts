import {
  encodeTerminalAck,
  encodeTerminalAttach,
  encodeTerminalDetach,
  encodeTerminalLeaseAcquire,
  encodeTerminalLeaseRelease,
  encodeTerminalLeaseRenew,
  encodeTerminalResize,
  type ServerControlFrame,
  type TerminalInputResultBody,
  type TerminalLeaseResultBody,
  type TerminalServerControlFrame,
} from "./control.js";
import { ProtocolError } from "./errors.js";
import { MAX_SQLITE_INTEGER, MAX_TERMINAL_COLS, MAX_TERMINAL_PAYLOAD, MAX_TERMINAL_ROWS, TERMINAL_LEASE_RENEW_INTERVAL_MS } from "./manifest.js";
import { decodeTerminalOutput, encodeTerminalInput, type TerminalFrame } from "./terminal.js";
import type { BrowserTimer } from "./session.js";

export type TerminalOutput = Readonly<{ sequence: bigint; payload: Uint8Array }>;
export type TerminalAttached = Readonly<{ sessionId: string; floor: bigint; head: bigint; acknowledgedSequence: bigint; maxUnackedBytes: bigint }>;
export type TerminalAttachReset = TerminalReset & { kind: "reset"; freshAttachRequired: true };
export type TerminalAttachOutcome = TerminalAttached | TerminalAttachReset;
export type TerminalLease = Readonly<{ generation: bigint; expiresAtMs: bigint; lastInputSequence: bigint; runRevision: bigint; sessionRevision: bigint }>;
export type TerminalLeaseResult = Readonly<TerminalLeaseResultBody>;
export type TerminalInputResult = Readonly<TerminalInputResultBody>;
export type TerminalExit = Readonly<{ sessionId: string; exitCode: number; exitSignal: number; aborted: boolean }>;
export type TerminalReset = Readonly<{ sessionId: string; floor: bigint; head: bigint }>;
export type SessionErrorLike = Error & { code?: string; retryable?: boolean };

export type TerminalOptions = Readonly<{
  afterSequence?: bigint;
  /** Optional session fence for a replay cursor carried from an earlier handle. */
  afterSessionId?: string;
  onOutput?: (output: TerminalOutput) => void | Promise<void>;
  /** Runs after the output callback, ACK, and output fence have all settled. */
  onOutputComplete?: () => void;
  onEOF?: (event: { sessionId: string }) => void | Promise<void>;
  onExit?: (event: TerminalExit) => void | Promise<void>;
  onReset?: (event: TerminalReset) => void | Promise<void>;
  onClose?: (error?: SessionErrorLike) => void;
}>;

/** Only BrowserSession may create this session-bound interface. */
export interface TerminalHandle {
  attach(): Promise<TerminalAttachOutcome>;
  acquireInput(): Promise<TerminalLease>;
  releaseInput(): Promise<TerminalLeaseResult>;
  sendInput(bytes: Uint8Array): Promise<TerminalInputResult>;
  resize(rows: number, cols: number): Promise<{ sessionId: string; generation: bigint; rows: number; cols: number }>;
  detach(): Promise<void>;
  readonly writable: boolean;
}

/** Methods reserved for the owning BrowserSession; omitted from root exports. */
export interface InternalTerminalHandle extends TerminalHandle {
  readonly closed: boolean;
  terminate(error?: SessionErrorLike): void;
  receive(frame: TerminalServerControlFrame): boolean;
  receiveError(id: string, error: SessionErrorLike): boolean;
  receiveEOF(id: string, body: { session_id: string }): boolean;
  receiveExit(id: string, body: { session_id: string; exit_code: number; exit_signal: number; aborted: boolean }): boolean;
  receiveReset(id: string, body: { session_id: string; floor: bigint; head: bigint }): boolean;
  receiveBinary(frame: TerminalFrame): Promise<boolean>;
}

type InternalTarget = Readonly<{ runId: string; sessionId: string; runRevision: bigint; sessionRevision: bigint }>;
type Send = (id: string | undefined, payload: string | Uint8Array) => void;
type NextID = (prefix: string) => string;
type Fatal = (error: SessionErrorLike) => void;
type Operation = {
  kind: "attach" | "acquire" | "renew" | "release" | "resize" | "detach" | "input";
  id: string;
  generation?: bigint;
  sequence?: bigint;
  payloadLength?: number;
  rows?: number;
  cols?: number;
  resolve: (value: unknown) => void;
  reject: (error: unknown) => void;
  detachAfter?: boolean;
};
type RetiredResponse = {
  id: string;
  type: "TERMINAL_ATTACHED" | "TERMINAL_LEASE_RESULT" | "TERMINAL_RESIZED" | "TERMINAL_DETACHED" | "TERMINAL_INPUT_RESULT" | "TERMINAL_RESET" | "TERMINAL_EXIT";
  generation?: bigint;
  sequence?: bigint;
};

const MAX_RETIRED_RESPONSES = 8;

/** Internal constructor used only by BrowserSession. */
export function createTerminalHandle(target: InternalTarget, options: TerminalOptions, send: Send, nextID: NextID, fatal: Fatal, timer: BrowserTimer, inputAllowed: boolean): InternalTerminalHandle {
  return new TerminalHandleImpl(target, options, send, nextID, fatal, timer, inputAllowed);
}

class TerminalHandleImpl implements InternalTerminalHandle {
  readonly #target: InternalTarget;
  readonly #send: Send;
  readonly #nextID: NextID;
  readonly #reportFatal: Fatal;
  readonly #timer: BrowserTimer;
  readonly #inputAllowed: boolean;
  readonly #options: TerminalOptions;
  readonly #retired: RetiredResponse[] = [];
  #operation: Operation | undefined;
  #attached = false;
  #detaching = false;
  #closed = false;
  #attachmentID: string | undefined;
  #requestedAfterSequence = 0n;
  #lease: TerminalLease | undefined;
  #nextOutputSequence = 0n;
  #acknowledgedSequence = 0n;
  #nextInputSequence = 0n;
  #inputSequenceExhausted = false;
  #generationFloor = 0n;
  #renewDue = false;
  #timerHandle: unknown;
  #timerScheduled = false;
  #timerToken = 0;
  #outputInFlight = false;
  #outputCloseResolve: (() => void) | undefined;
  #detachPromise: Promise<void> | undefined;
  #detachResolve: (() => void) | undefined;
  #detachReject: ((error: unknown) => void) | undefined;

  constructor(target: InternalTarget, options: TerminalOptions, send: Send, nextID: NextID, fatal: Fatal, timer: BrowserTimer, inputAllowed: boolean) {
    this.#target = { ...target };
    this.#options = { ...options };
    this.#send = send;
    this.#nextID = nextID;
    this.#reportFatal = fatal;
    this.#timer = timer;
    this.#inputAllowed = inputAllowed;
  }

  get closed(): boolean { return this.#closed; }

  get writable(): boolean { return !this.#closed && this.#attached && !this.#detaching && this.#lease !== undefined && !this.#inputSequenceExhausted; }

  attach(): Promise<TerminalAttachOutcome> {
    this.#ensureOpen();
    if (this.#outputInFlight) return Promise.reject(new SessionErrorLikeError("terminal output callback pending"));
    if (this.#attached || this.#operation !== undefined || this.#detaching) return Promise.reject(new SessionErrorLikeError("terminal operation pending"));
    const afterSequence = this.#options.afterSequence !== undefined &&
      (this.#options.afterSessionId === undefined || this.#options.afterSessionId === this.#target.sessionId)
      ? this.#options.afterSequence
      : this.#acknowledgedSequence;
    if (afterSequence < 0n || afterSequence > MAX_SQLITE_INTEGER) return Promise.reject(new ProtocolError("malformed"));
    this.#requestedAfterSequence = afterSequence;
    const id = this.#nextID("terminal-attach");
    this.#attachmentID = id;
    return this.#start<TerminalAttachOutcome>({ kind: "attach", id }, encodeTerminalAttach(id, {
      run_id: this.#target.runId, session_id: this.#target.sessionId,
      expected_run_revision: this.#target.runRevision, expected_session_revision: this.#target.sessionRevision,
      after_sequence: afterSequence,
    }));
  }

  acquireInput(): Promise<TerminalLease> {
    this.#ensureAttached();
    if (this.#outputInFlight) return Promise.reject(new SessionErrorLikeError("terminal output callback pending"));
    if (!this.#inputAllowed) return Promise.reject(new SessionErrorLikeError("unauthorized"));
    if (this.#lease !== undefined || this.#operation !== undefined || this.#detaching) return Promise.reject(new SessionErrorLikeError("terminal operation pending"));
    if (this.#generationFloor >= MAX_SQLITE_INTEGER) return Promise.reject(new SessionErrorLikeError("generation exhausted"));
    const id = this.#nextID("terminal-lease-acquire");
    return this.#start<TerminalLease>({ kind: "acquire", id }, encodeTerminalLeaseAcquire(id, {
      run_id: this.#target.runId, session_id: this.#target.sessionId,
      expected_run_revision: this.#target.runRevision, expected_session_revision: this.#target.sessionRevision,
    }));
  }

  releaseInput(): Promise<TerminalLeaseResult> {
    this.#ensureAttached();
    if (this.#outputInFlight) return Promise.reject(new SessionErrorLikeError("terminal output callback pending"));
    if (this.#lease === undefined) return Promise.reject(new SessionErrorLikeError("terminal lease required"));
    if (this.#operation !== undefined || this.#detaching) return Promise.reject(new SessionErrorLikeError("terminal operation pending"));
    return this.#beginRelease(false) as Promise<TerminalLeaseResult>;
  }

  sendInput(bytes: Uint8Array): Promise<TerminalInputResult> {
    this.#ensureAttached();
    if (this.#outputInFlight) return Promise.reject(new SessionErrorLikeError("terminal output callback pending"));
    if (this.#inputSequenceExhausted) return Promise.reject(new SessionErrorLikeError("input sequence exhausted"));
    const lease = this.#requireLease();
    if (!(bytes instanceof Uint8Array)) return Promise.reject(new ProtocolError("malformed"));
    if (this.#operation !== undefined || this.#detaching) return Promise.reject(new SessionErrorLikeError("terminal operation pending"));
    const payload = bytes.slice();
    const sequence = this.#nextInputSequence;
    const encoded = encodeTerminalInput(hexSessionID(this.#target.sessionId), sequence, lease.generation, payload);
    const id = this.#nextID("terminal-input");
    return this.#start<TerminalInputResult>({ kind: "input", id, generation: lease.generation, sequence, payloadLength: payload.length }, encoded);
  }

  resize(rows: number, cols: number): Promise<{ sessionId: string; generation: bigint; rows: number; cols: number }> {
    this.#ensureAttached();
    if (this.#outputInFlight) return Promise.reject(new SessionErrorLikeError("terminal output callback pending"));
    const lease = this.#requireLease();
    if (this.#operation !== undefined || this.#detaching) return Promise.reject(new SessionErrorLikeError("terminal operation pending"));
    if (!Number.isSafeInteger(rows) || rows < 1 || rows > MAX_TERMINAL_ROWS || !Number.isSafeInteger(cols) || cols < 1 || cols > MAX_TERMINAL_COLS) return Promise.reject(new ProtocolError("malformed"));
    const id = this.#nextID("terminal-resize");
    return this.#start({ kind: "resize", id, generation: lease.generation, rows, cols }, encodeTerminalResize(id, {
      run_id: this.#target.runId, session_id: this.#target.sessionId, generation: lease.generation,
      expected_run_revision: this.#target.runRevision, expected_session_revision: this.#target.sessionRevision, rows, cols,
    }));
  }

  detach(): Promise<void> {
    if (this.#detachPromise !== undefined) return this.#detachPromise;
    this.#ensureOpen();
    if (this.#outputInFlight) return Promise.reject(new SessionErrorLikeError("terminal output callback pending"));
    this.#detaching = true;
    this.#cancelTimer();
    this.#renewDue = false;
    let resolveDetach!: () => void;
    let rejectDetach!: (error: unknown) => void;
    const pending = new Promise<void>((resolve, reject) => { resolveDetach = resolve; rejectDetach = reject; });
    this.#detachPromise = pending;
    this.#detachResolve = resolveDetach;
    this.#detachReject = rejectDetach;
    this.#advanceDetach();
    return pending;
  }

  /** Called by BrowserSession when this socket generation is closing. */
  terminate(error?: SessionErrorLike): void { this.#closeLocal(error ?? new SessionErrorLikeError("closed")); }

  receive(frame: TerminalServerControlFrame): boolean {
    if (this.#closed) return this.#isRetired(frame);
    if (frame.body.session_id !== this.#target.sessionId) return false;
    if (frame.type === "TERMINAL_INPUT_RESULT") {
      const operation = this.#operation;
      if (this.#attachmentID === undefined || frame.id !== this.#attachmentID || operation?.kind !== "input" || operation.generation !== frame.body.generation || operation.sequence !== frame.body.sequence) return false;
      this.#inputResult(frame.body, operation);
      return true;
    }
    const operation = this.#operation;
    if (operation === undefined || frame.id !== operation.id) return false;
    switch (frame.type) {
      case "TERMINAL_ATTACHED":
        if (operation.kind !== "attach" || frame.body.floor > frame.body.acknowledged_sequence || frame.body.acknowledged_sequence > frame.body.head || frame.body.acknowledged_sequence !== this.#requestedAfterSequence) throw new ProtocolError("malformed");
        const attached = frozenAttached({ sessionId: frame.body.session_id, floor: frame.body.floor, head: frame.body.head, acknowledgedSequence: frame.body.acknowledged_sequence, maxUnackedBytes: frame.body.max_unacked_bytes });
        this.#complete(operation, attached, () => {
          this.#attached = true;
          this.#acknowledgedSequence = frame.body.acknowledged_sequence;
          this.#nextOutputSequence = frame.body.acknowledged_sequence;
          this.#nextInputSequence = 0n;
          this.#inputSequenceExhausted = false;
        });
        return true;
      case "TERMINAL_LEASE_RESULT":
        this.#leaseResult(frame.body, operation);
        return true;
      case "TERMINAL_RESIZED":
        if (operation.kind !== "resize" || frame.body.generation !== operation.generation || frame.body.rows !== operation.rows || frame.body.cols !== operation.cols) throw new ProtocolError("malformed");
        this.#complete(operation, { sessionId: frame.body.session_id, generation: frame.body.generation, rows: frame.body.rows, cols: frame.body.cols });
        return true;
      case "TERMINAL_DETACHED":
        if (operation.kind !== "detach") throw new ProtocolError("malformed");
        this.#complete(operation, undefined, () => {
          this.#attached = false; this.#lease = undefined; this.#cancelTimer(); this.#renewDue = false;
        }, true);
        return true;
      default:
        return false;
    }
  }

  receiveError(id: string, error: SessionErrorLike): boolean {
    if (this.#closed) return false;
    const operation = this.#operation;
    const matches = operation !== undefined && (operation.kind === "input" ? id === this.#attachmentID : id === operation.id);
    if (!matches || operation === undefined) return false;
    // A binary input ERROR is transport-ambiguous: the server cannot prove
    // whether any bytes reached the runner, so the generation must be fenced
    // rather than allowing a different payload to reuse its reservation.
    if (operation.kind === "input") { this.#fatal(error); return true; }
    if (operation.kind === "detach") { this.#fatal(error); return true; }
    this.#operation = undefined;
    if (operation.kind === "acquire") { this.#cancelTimer(); this.#renewDue = false; this.#lease = undefined; }
    // A refused release or renewal still ends this client's authority:
    // whatever the daemon's reason (the run ended, the lease lapsed or moved
    // on), it no longer honours this generation, so it is fenced here and
    // observation continues. A detach in progress carries on without it.
    if (operation.kind === "release" || operation.kind === "renew") this.#clearAuthority(operation.generation);
    if (operation.kind === "resize") this.#serviceRenewal();
    operation.reject(error);
    this.#advanceDetach();
    return true;
  }

  async receiveBinary(frame: TerminalFrame): Promise<boolean> {
    if (this.#closed) return false;
    if (!this.#attached || frame.direction !== "output" || !(frame.payload instanceof Uint8Array) || frame.payload.length === 0 || frame.payload.length > MAX_TERMINAL_PAYLOAD || !sameBytes(frame.sessionId, hexSessionID(this.#target.sessionId)) || frame.leaseGeneration !== 0n) return false;
    if (this.#outputInFlight) { this.#closeLocal(new SessionErrorLikeError("output callback reentrant")); return true; }
    if (frame.sequence !== this.#nextOutputSequence) return false;
    this.#outputInFlight = true;
    const output = Object.freeze({ sequence: frame.sequence, payload: frame.payload.slice() });
    let closeResolve: (() => void) | undefined;
    const closed = new Promise<void>((resolve) => { closeResolve = resolve; this.#outputCloseResolve = resolve; });
    try {
      let accepted: Promise<void>;
      try { accepted = Promise.resolve(this.#options.onOutput?.(output)); } catch { accepted = Promise.reject(new Error("consumer")); }
      try { await Promise.race([accepted, closed]); } catch {
        this.#closeLocal(new SessionErrorLikeError("terminal output consumer failed")); return true;
      }
      if (this.#closed || !this.#attached) return true;
      const nextSequence = this.#nextOutputSequence + BigInt(frame.payload.length);
      try { this.#send(undefined, encodeTerminalAck({ session_id: this.#target.sessionId, next_sequence: nextSequence })); } catch {
        this.#fatal(new SessionErrorLikeError("terminal ACK send failed")); return true;
      }
      if (this.#closed || !this.#attached) return true;
      this.#nextOutputSequence = nextSequence; this.#acknowledgedSequence = nextSequence;
      if (!this.#closed) this.#serviceRenewal();
      return true;
    } finally {
      if (this.#outputCloseResolve === closeResolve) this.#outputCloseResolve = undefined;
      this.#outputInFlight = false;
      try { this.#options.onOutputComplete?.(); } catch { /* output completion never owns the session */ }
      this.#advanceDetach();
    }
  }

  receiveEOF(id: string, body: { session_id: string }): boolean {
    if (this.#closed || !this.#attached || id !== this.#attachmentID || body.session_id !== this.#target.sessionId) return false;
    this.#observe(this.#options.onEOF, { sessionId: body.session_id }); return true;
  }

  receiveExit(id: string, body: { session_id: string; exit_code: number; exit_signal: number; aborted: boolean }): boolean {
    if (this.#closed) return this.#matchesRetired("TERMINAL_EXIT", id, body.session_id);
    if (!this.#attached || id !== this.#attachmentID || body.session_id !== this.#target.sessionId) return false;
    const event = { sessionId: body.session_id, exitCode: body.exit_code, exitSignal: body.exit_signal, aborted: body.aborted };
    const operation = this.#operation;
    if (operation !== undefined) { this.#operation = undefined; operation.reject(new SessionErrorLikeError("terminal exited")); }
    this.#clearAuthority();
    this.#rememberRetired({ id, type: "TERMINAL_EXIT" });
    this.#closeLocal();
    this.#observe(this.#options.onExit, event); return true;
  }

  receiveReset(id: string, body: { session_id: string; floor: bigint; head: bigint }): boolean {
    if (this.#closed) return this.#matchesRetired("TERMINAL_RESET", id, body.session_id);
    if (id !== this.#attachmentID || body.session_id !== this.#target.sessionId) return false;
    if (this.#matchesRetired("TERMINAL_RESET", id, body.session_id)) return true;
    const reset = frozenReset({ sessionId: body.session_id, floor: body.floor, head: body.head });
    const operation = this.#operation;
    if (operation?.kind === "attach") {
      this.#operation = undefined; this.#attached = false; this.#clearAuthority();
      this.#rememberRetired({ id, type: "TERMINAL_RESET" });
      operation.resolve(Object.freeze({ ...reset, kind: "reset", freshAttachRequired: true }));
    } else {
      if (operation !== undefined) { this.#operation = undefined; operation.reject(new SessionErrorLikeError("terminal reset")); }
      this.#attached = false; this.#clearAuthority(); this.#rememberRetired({ id, type: "TERMINAL_RESET" });
    }
    this.#closeLocal();
    this.#observe(this.#options.onReset, reset); return true;
  }

  #leaseResult(body: TerminalLeaseResultBody, operation: Operation): void {
    if (operation.kind !== "acquire" && operation.kind !== "renew" && operation.kind !== "release") throw new ProtocolError("malformed");
    const expectedOperation = operation.kind === "acquire" ? "acquired" : operation.kind === "renew" ? "renewed" : "released";
    if (body.run_id !== this.#target.runId || body.session_id !== this.#target.sessionId || body.run_revision !== this.#target.runRevision || body.session_revision !== this.#target.sessionRevision || body.operation !== expectedOperation) throw new ProtocolError("malformed");
    if (body.operation === "released") {
      if (operation.generation === undefined || operation.generation >= MAX_SQLITE_INTEGER || body.generation !== operation.generation + 1n || body.last_input_sequence !== 0n) throw new ProtocolError("malformed");
      const expiry = this.#lease === undefined ? undefined : this.#expiryNumber(this.#lease.expiresAtMs);
      const now = this.#now();
      if (expiry === undefined || now === undefined || now >= expiry) throw new ProtocolError("malformed");
      this.#complete(operation, frozenLeaseResult(body), () => { this.#generationFloor = body.generation > this.#generationFloor ? body.generation : this.#generationFloor; this.#clearAuthority(); });
      return;
    }
    if (body.expires_at_ms === undefined || !this.#validExpiry(body.expires_at_ms)) throw new ProtocolError("malformed");
    if (body.operation === "acquired") {
      if (operation.generation !== undefined || body.last_input_sequence !== 0n || body.generation <= this.#generationFloor) throw new ProtocolError("malformed");
      this.#complete(operation, frozenLease({ generation: body.generation, expiresAtMs: body.expires_at_ms, lastInputSequence: body.last_input_sequence, runRevision: body.run_revision, sessionRevision: body.session_revision }), () => {
        this.#generationFloor = body.generation; this.#inputSequenceExhausted = false; this.#nextInputSequence = 1n;
        this.#lease = { generation: body.generation, expiresAtMs: body.expires_at_ms!, lastInputSequence: 0n, runRevision: body.run_revision, sessionRevision: body.session_revision };
        this.#renewDue = false; this.#armRenewal();
      });
      return;
    }
    const lease = this.#lease;
    if (operation.generation === undefined || body.generation !== operation.generation || lease === undefined || lease.generation !== operation.generation || body.last_input_sequence !== lease.lastInputSequence || body.expires_at_ms < lease.expiresAtMs) throw new ProtocolError("malformed");
    const now = this.#now();
    const oldExpiry = this.#expiryNumber(lease.expiresAtMs);
    if (now === undefined || oldExpiry === undefined || now >= oldExpiry) throw new ProtocolError("malformed");
    this.#complete(operation, frozenLease({ generation: body.generation, expiresAtMs: body.expires_at_ms, lastInputSequence: body.last_input_sequence, runRevision: body.run_revision, sessionRevision: body.session_revision }), () => { this.#lease = { ...lease, expiresAtMs: body.expires_at_ms! }; this.#renewDue = false; this.#armRenewal(); });
  }

  #inputResult(body: TerminalInputResultBody, operation: Operation): void {
    if (operation.kind !== "input" || body.generation !== operation.generation || body.sequence !== operation.sequence || body.accepted_bytes > BigInt(operation.payloadLength ?? 0)) throw new ProtocolError("malformed");
    const length = BigInt(operation.payloadLength ?? 0);
    if (body.status === "accepted" && body.accepted_bytes !== length || body.status === "partial" && (body.accepted_bytes === 0n || body.accepted_bytes === length) || (body.status === "rejected" || body.status === "uncertain") && body.accepted_bytes !== 0n) throw new ProtocolError("malformed");
    this.#complete(operation, body, () => {
      if (body.status === "rejected") return;
      this.#nextInputSequence = body.sequence >= MAX_SQLITE_INTEGER ? MAX_SQLITE_INTEGER : body.sequence + 1n;
      if (body.status === "accepted") {
        const lease = this.#lease; if (lease === undefined) throw new ProtocolError("malformed");
        if (body.sequence >= MAX_SQLITE_INTEGER) { this.#inputSequenceExhausted = true; this.#clearAuthority(body.generation); } else this.#lease = { ...lease, lastInputSequence: body.sequence };
      } else this.#clearAuthority(body.generation);
    });
  }

  #beginRelease(detachAfter: boolean): Promise<unknown> {
    const lease = this.#lease;
    if (lease === undefined) return Promise.reject(new SessionErrorLikeError("terminal lease required"));
    if (lease.generation >= MAX_SQLITE_INTEGER) return Promise.reject(new SessionErrorLikeError("generation exhausted"));
    const expiry = this.#expiryNumber(lease.expiresAtMs);
    const now = this.#now();
    if (expiry === undefined || now === undefined || now >= expiry) {
      this.#fatal(new SessionErrorLikeError("terminal lease expired"));
      return Promise.reject(new SessionErrorLikeError("terminal lease expired"));
    }
    const id = this.#nextID("terminal-lease-release");
    this.#armExpiryWatchdog(expiry);
    if (this.#closed) return Promise.reject(new SessionErrorLikeError("terminal lease release failed"));
    return this.#start({ kind: "release", id, generation: lease.generation, detachAfter }, encodeTerminalLeaseRelease(id, {
      run_id: this.#target.runId, session_id: this.#target.sessionId, generation: lease.generation,
      expected_run_revision: this.#target.runRevision, expected_session_revision: this.#target.sessionRevision,
    }));
  }

  #beginDetach(): Promise<void> {
    const id = this.#nextID("terminal-detach");
    return this.#start<void>({ kind: "detach", id }, encodeTerminalDetach(id, { session_id: this.#target.sessionId }));
  }

  #beginRenew(): void {
    const lease = this.#lease;
    if (lease === undefined || this.#closed || this.#detaching || this.#operation !== undefined) return;
    const now = this.#now(); const expiry = this.#expiryNumber(lease.expiresAtMs);
    if (now === undefined || expiry === undefined || now >= expiry) { this.#fatal(new SessionErrorLikeError("terminal lease expired")); return; }
    const id = this.#nextID("terminal-lease-renew");
    const operation: Operation = { kind: "renew", id, generation: lease.generation, resolve: () => undefined, reject: () => undefined };
    const pending = this.#install<void>(operation);
    this.#rememberSent(id, operation);
    this.#armExpiryWatchdog(expiry);
    if (this.#closed || this.#operation !== operation) { void pending.catch(() => undefined); return; }
    try {
      this.#send(id, encodeTerminalLeaseRenew(id, { run_id: this.#target.runId, session_id: this.#target.sessionId, generation: lease.generation, expected_run_revision: this.#target.runRevision, expected_session_revision: this.#target.sessionRevision }));
    } catch { this.#fatal(new SessionErrorLikeError("terminal renewal send failed")); }
    void pending.catch(() => undefined);
  }

  #start<T>(operation: Omit<Operation, "resolve" | "reject">, payload: string | Uint8Array): Promise<T> {
    const result = this.#install<T>({ ...operation, resolve: () => undefined, reject: () => undefined });
    this.#rememberSent(operation.id, operation);
    try { this.#send(operation.id, payload); } catch { this.#fatal(new SessionErrorLikeError("terminal control send failed")); }
    return result;
  }

  #install<T>(operation: Operation): Promise<T> {
    const result = new Promise<T>((resolve, reject) => { operation.resolve = resolve as (value: unknown) => void; operation.reject = reject; });
    this.#operation = operation;
    return result;
  }

  #complete<T>(operation: Operation, value: T, apply?: () => void, closeAfter = false): void {
    if (this.#operation !== operation) throw new ProtocolError("malformed");
    try { apply?.(); } catch (error) {
      // Apply hooks can discover a malformed chronology. Reject the caller
      // before fencing the generation so no completed response leaves a
      // promise behind while the fatal path closes the socket.
      if (this.#operation === operation) this.#operation = undefined;
      operation.reject(error);
      if (!this.#closed) this.#fatal(error as SessionErrorLike);
      throw error;
    }
    // Timer installation or authority clearing may have failed inside the
    // apply hook and already closed this operation. Do not resurrect it.
    if (this.#operation !== operation) return;
    this.#operation = undefined;
    if (operation.detachAfter) {
      try {
        const detached = this.#beginDetach();
        void detached.then(() => operation.resolve(undefined), (error: unknown) => operation.reject(error));
      } catch (error) {
        this.#fatal(new SessionErrorLikeError("terminal detach failed"));
        operation.reject(error);
      }
      return;
    }
    if (closeAfter) { this.#closeLocal(); operation.resolve(value); return; }
    // Resolve before servicing a due renewal. A synchronous renewal send or
    // watchdog failure may close the generation, but must not strand the
    // operation whose response was already received.
    operation.resolve(value);
    if (!this.#closed) this.#serviceRenewal();
    this.#advanceDetach();
  }

  #advanceDetach(): void {
    if (!this.#detaching || this.#closed || this.#operation !== undefined || this.#outputInFlight) return;
    if (!this.#attached) { this.#closeLocal(); return; }
    try {
      // A release the daemon refused was handled where it was received and
      // left no lease behind; a rejection that leaves the lease in place is
      // one that never went out (the generation guard), and that is fatal.
      if (this.#lease !== undefined) void this.#beginRelease(true).catch((error: unknown) => { if (this.#lease !== undefined) this.#fatal(error as SessionErrorLike); });
      else void this.#beginDetach().catch((error: unknown) => this.#fatal(error as SessionErrorLike));
    } catch (error) {
      this.#fatal(error as SessionErrorLike);
    }
  }

  #serviceRenewal(): void { if (!this.#renewDue || this.#lease === undefined || this.#closed || this.#detaching) return; this.#renewDue = false; this.#beginRenew(); }
  #armRenewal(): void { this.#armTimer(TERMINAL_LEASE_RENEW_INTERVAL_MS, () => this.#timerDue()); }

  #timerDue(): void {
    if (this.#closed || this.#lease === undefined) return;
    const expiry = this.#expiryNumber(this.#lease.expiresAtMs); const now = this.#now();
    if (expiry === undefined || now === undefined || now >= expiry) { this.#fatal(new SessionErrorLikeError("terminal lease expired")); return; }
    if (this.#operation !== undefined || this.#outputInFlight) { this.#renewDue = true; this.#armExpiryWatchdog(expiry); return; }
    this.#beginRenew();
  }

  #armExpiryWatchdog(expiry: number): void {
    const now = this.#now();
    if (now === undefined || !Number.isSafeInteger(expiry) || expiry < now) { this.#fatal(new SessionErrorLikeError("invalid terminal lease expiry")); return; }
    this.#armTimer(expiry - now, () => {
      const lease = this.#lease; const currentExpiry = lease === undefined ? undefined : this.#expiryNumber(lease.expiresAtMs); const currentNow = this.#now();
      if (lease === undefined || currentExpiry === undefined || currentNow === undefined || currentNow >= currentExpiry) this.#fatal(new SessionErrorLikeError("terminal lease renewal timeout"));
      else this.#armExpiryWatchdog(currentExpiry);
    });
  }

  #armTimer(delayMs: number, callback: () => void): void {
    this.#cancelTimer();
    if (this.#closed) return;
    if (!Number.isSafeInteger(delayMs) || delayMs < 0 || delayMs > 2_147_483_647) { this.#fatal(new SessionErrorLikeError("invalid terminal timer")); return; }
    const token = ++this.#timerToken; this.#timerScheduled = true; let handle: unknown;
    try {
      handle = this.#timer.setTimeout(() => {
        if (!this.#timerScheduled || this.#timerToken !== token) return;
        this.#timerScheduled = false; this.#timerHandle = undefined;
        try { callback(); } catch { this.#fatal(new SessionErrorLikeError("terminal timer failed")); }
      }, delayMs);
    } catch { this.#timerScheduled = false; this.#timerHandle = undefined; this.#fatal(new SessionErrorLikeError("terminal timer failed")); return; }
    if (this.#timerScheduled && this.#timerToken === token) this.#timerHandle = handle;
  }

  #cancelTimer(): void {
    const scheduled = this.#timerScheduled; const handle = this.#timerHandle;
    this.#timerScheduled = false; this.#timerHandle = undefined; ++this.#timerToken;
    if (scheduled) {
      try { this.#timer.clearTimeout(handle); } catch {
        try { this.#reportFatal(new SessionErrorLikeError("terminal timer failed")); } catch { /* Session remains authoritative */ }
        this.#closeLocal(new SessionErrorLikeError("terminal timer failed"));
      }
    }
  }

  #clearAuthority(generation: bigint | undefined = this.#lease?.generation): void { this.#lease = undefined; this.#renewDue = false; this.#cancelTimer(); if (generation !== undefined) this.#fenceGeneration(generation); }

  #closeLocal(error?: SessionErrorLike): void {
    if (this.#closed) return;
    this.#closed = true; this.#cancelTimer(); this.#renewDue = false;
    const wakeOutput = this.#outputCloseResolve; this.#outputCloseResolve = undefined; wakeOutput?.();
    const operation = this.#operation; this.#operation = undefined; this.#lease = undefined; this.#attached = false;
    operation?.reject(error ?? new SessionErrorLikeError("closed"));
    const resolveDetach = this.#detachResolve;
    const rejectDetach = this.#detachReject;
    this.#detachResolve = undefined;
    this.#detachReject = undefined;
    if (error === undefined) resolveDetach?.();
    else rejectDetach?.(error);
    try { this.#options.onClose?.(error); } catch { /* callbacks never own transport */ }
  }

  #fatal(error: SessionErrorLike): void {
    if (this.#closed) return;
    this.#clearAuthority(this.#lease?.generation);
    try { this.#reportFatal(error); } catch { /* Session remains the authority */ }
    this.#closeLocal(error);
  }

  #validExpiry(value: bigint): boolean { const expiry = this.#expiryNumber(value); const now = this.#now(); return expiry !== undefined && now !== undefined && expiry > now; }
  #expiryNumber(value: bigint): number | undefined { if (value < 0n || value > BigInt(Number.MAX_SAFE_INTEGER)) return undefined; const result = Number(value); return Number.isSafeInteger(result) ? result : undefined; }
  #now(): number | undefined {
    const candidate: unknown = this.#timer.now;
    const now = typeof candidate === "function" ? (candidate as () => number)() : typeof candidate === "number" ? candidate : Date.now();
    return Number.isFinite(now) && Number.isSafeInteger(now) && now >= 0 ? now : undefined;
  }

  #rememberSent(id: string, operation: Pick<Operation, "kind" | "generation" | "sequence">): void {
    const responseID = operation.kind === "input" ? this.#attachmentID : id;
    if (responseID === undefined) return;
    const type = operation.kind === "attach" ? "TERMINAL_ATTACHED" : operation.kind === "acquire" || operation.kind === "renew" || operation.kind === "release" ? "TERMINAL_LEASE_RESULT" : operation.kind === "resize" ? "TERMINAL_RESIZED" : operation.kind === "detach" ? "TERMINAL_DETACHED" : "TERMINAL_INPUT_RESULT";
    this.#rememberRetired({ id: responseID, type, generation: operation.generation, sequence: operation.sequence });
  }
  #rememberRetired(record: RetiredResponse): void { if (this.#retired.some((candidate) => candidate.id === record.id && candidate.type === record.type && candidate.generation === record.generation && candidate.sequence === record.sequence)) return; this.#retired.push(record); if (this.#retired.length > MAX_RETIRED_RESPONSES) this.#retired.shift(); }
  #isRetired(frame: TerminalServerControlFrame): boolean {
    if (frame.type === "TERMINAL_EOF") return false;
    if (frame.type === "TERMINAL_INPUT_RESULT") return this.#matchesRetired(frame.type, frame.id, frame.body.session_id, frame.body.generation, frame.body.sequence);
    return this.#matchesRetired(frame.type, frame.id, frame.body.session_id);
  }
  #matchesRetired(type: RetiredResponse["type"], id: string, sessionId: string, generation?: bigint, sequence?: bigint): boolean { return this.#retired.some((record) => record.id === id && record.type === type && sessionId === this.#target.sessionId && (type !== "TERMINAL_INPUT_RESULT" || record.generation === generation && record.sequence === sequence)); }
  #observe<T>(callback: ((event: T) => void | Promise<void>) | undefined, event: T): void { if (callback === undefined) return; try { void Promise.resolve(callback(event)).catch(() => undefined); } catch { /* observation callbacks are isolated */ } }
  #requireLease(): TerminalLease { if (this.#lease === undefined) throw new SessionErrorLikeError("terminal lease required"); return this.#lease; }
  #ensureAttached(): void { this.#ensureOpen(); if (!this.#attached) throw new SessionErrorLikeError("terminal not attached"); }
  #ensureOpen(): void { if (this.#closed) throw new SessionErrorLikeError("closed"); }
  #fenceGeneration(generation: bigint): void { const next = generation >= MAX_SQLITE_INTEGER ? MAX_SQLITE_INTEGER : generation + 1n; if (next > this.#generationFloor) this.#generationFloor = next; }
}

/** Internal error with a finite message and no wire details. */
export class SessionErrorLikeError extends Error {
  readonly code: string;
  readonly retryable = false;
  constructor(code: string) { super(code); this.name = "SessionError"; this.code = code; }
}

export function terminalControlFrame(frame: ServerControlFrame): frame is TerminalServerControlFrame { return frame.type.startsWith("TERMINAL_") && frame.type !== "TERMINAL_TARGET"; }
export function decodeTerminalServer(data: Uint8Array): TerminalFrame { return decodeTerminalOutput(data); }
export function hexSessionID(value: string): Uint8Array {
  if (!/^[0-9a-f]{32}$/.test(value) || /^0+$/.test(value)) throw new ProtocolError("malformed");
  const result = new Uint8Array(16); for (let index = 0; index < 16; index++) result[index] = Number.parseInt(value.slice(index * 2, index * 2 + 2), 16); return result;
}
function sameBytes(a: Uint8Array, b: Uint8Array): boolean { return a.length === b.length && a.every((value, index) => value === b[index]); }
function frozenAttached(value: TerminalAttached): TerminalAttached { return Object.freeze({ ...value }); }
function frozenLease(value: TerminalLease): TerminalLease { return Object.freeze({ ...value }); }
function frozenLeaseResult(value: TerminalLeaseResultBody): TerminalLeaseResult { return Object.freeze({ ...value }); }
function frozenReset(value: TerminalReset): TerminalReset { return Object.freeze({ ...value }); }
