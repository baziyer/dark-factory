import {
  MAX_TERMINAL_COLS,
  MAX_TERMINAL_PAYLOAD,
  MAX_TERMINAL_ROWS,
  ProtocolError,
  SessionError,
  type BrowserSession,
  type TerminalHandle,
  type TerminalInputResult,
  type TerminalReset,
  type TerminalTarget,
} from "@dark-factory/client";

const MAX_PENDING_INPUT_BYTES = 64 * 1024;

export type TerminalSurface = Readonly<{
  write(payload: Uint8Array): Promise<void>;
  abort(): void;
}>;

export type TerminalPhase = "idle" | "resolving" | "attaching" | "acquiring" | "ready" | "closing" | "closed";

export type TerminalControllerSnapshot = Readonly<{
  phase: TerminalPhase;
  writable: boolean;
  error?: SessionError | ProtocolError;
  /**
   * True when this controller ended because the server reset the replay
   * (retained output no longer covers the attachment). The protocol closes
   * the handle on reset, so the controller still ends; the owner may open
   * a fresh controller against current state instead of surfacing an error.
   */
  reset?: TerminalReset;
  /** This controller ended before minting a target and may retry on new state. */
  retryDiscovery: boolean;
}>;

type TerminalControllerSession = Pick<BrowserSession, "resolveAgentTerminal" | "openTerminal" | "close">;

type TerminalControllerOptions = Readonly<{
  session: TerminalControllerSession;
  agentId: string;
  expectedAgentRevision: bigint;
  expectedHead: bigint;
  surface: TerminalSurface;
  onChange: (snapshot: TerminalControllerSnapshot) => void;
  /** Server session and cursor from a previous replay reset. */
  resume?: Pick<TerminalReset, "sessionId" | "head">;
  /** Test seam: awaited between retryable attach attempts. */
  attachRetryDelay?: (attempt: number) => Promise<void>;
}>;

/**
 * A fresh run's terminal can be observed active in durable state moments
 * before the daemon's live owner is ready to serve attachments; the server
 * types that window as retryable. Bounded, short: the window is
 * milliseconds, and a non-retryable error still fails immediately.
 */
const ATTACH_RETRY_LIMIT = 5;

function defaultAttachRetryDelay(attempt: number): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, 50 * (attempt + 1)));
}

type Input = Uint8Array;
type Resize = { rows: number; cols: number };

/** Owns one exact target, handle, display surface and input lease. */
class TerminalController {
  readonly #options: TerminalControllerOptions;
  #phase: TerminalPhase = "idle";
  #writable = false;
  #error: SessionError | ProtocolError | undefined;
  #handle: TerminalHandle | undefined;
  #handleClosed = false;
  #detachRequested = false;
  #started = false;
  #closing = false;
  #generation = 0;
  #reset: TerminalReset | undefined;
  #retryDiscovery = false;
  #surfaceAborted = false;
  #inputBuffer: Input = new Uint8Array(0);
  #inputInFlightBytes = 0;
  #pendingResize: Resize | undefined;
  #effectTask: Promise<void> | undefined;
  #outputTask: Promise<void> | undefined;
  #closePromise: Promise<void> | undefined;
  #detachPromise: Promise<void> | undefined;

  constructor(options: TerminalControllerOptions) {
    this.#options = options;
  }

  get snapshot(): TerminalControllerSnapshot { return this.#snapshot(); }

  start(): void {
    if (this.#started || this.#closing) return;
    this.#started = true;
    const generation = ++this.#generation;
    this.#phase = "resolving";
    this.#publish();
    if (!this.#current(generation)) return;
    void this.#bootstrap(generation);
  }

  sendInput(bytes: Uint8Array): boolean {
    if (!(bytes instanceof Uint8Array) || bytes.length === 0 || !this.#writable || !this.#canEffect()) return false;
    const total = this.#inputInFlightBytes + this.#inputBuffer.length;
    if (bytes.length > MAX_TERMINAL_PAYLOAD || bytes.length > MAX_PENDING_INPUT_BYTES || total + bytes.length > MAX_PENDING_INPUT_BYTES) {
      this.#fail(new SessionError("too_large"));
      return false;
    }
    const next = new Uint8Array(this.#inputBuffer.length + bytes.length);
    next.set(this.#inputBuffer);
    next.set(bytes, this.#inputBuffer.length);
    this.#inputBuffer = next;
    this.#pumpEffects();
    return true;
  }

  resize(rows: number, cols: number): boolean {
    if (!this.#writable || !this.#canEffect()) return false;
    if (!Number.isSafeInteger(rows) || rows < 1 || rows > MAX_TERMINAL_ROWS || !Number.isSafeInteger(cols) || cols < 1 || cols > MAX_TERMINAL_COLS) {
      this.#fail(new ProtocolError("malformed"));
      return false;
    }
    this.#pendingResize = { rows, cols };
    this.#pumpEffects();
    return true;
  }

  /** Detach this terminal observer while leaving the authenticated session open. */
  detach(): Promise<void> {
    if (this.#closePromise !== undefined) return this.#closePromise;
    if (this.#detachPromise !== undefined) return this.#detachPromise;
    const handle = this.#handle;
    if (handle === undefined || this.#handleClosed) {
      this.#handleEnded(new SessionError("closed"));
      return Promise.resolve();
    }
    this.#writable = false;
    this.#error = undefined;
    this.#detachRequested = true;
    this.#inputBuffer = new Uint8Array(0);
    this.#pendingResize = undefined;
    this.#phase = "closing";
    this.#publish();
    const pending = (async () => {
      const output = this.#outputTask;
      if (output !== undefined) await output.catch(() => undefined);
      if (this.#handle !== handle || this.#handleClosed) return;
      await handle.detach();
    })().then(() => {
      if (this.#handle !== handle) return;
      this.#handleClosed = true;
      this.#abortSurface();
      this.#phase = "closed";
      this.#publish();
    }, (error: unknown) => {
      if (this.#handle !== handle) return;
      this.#error = finiteError(error);
      this.#publish();
      void this.close();
      throw error;
    });
    this.#detachPromise = pending;
    return pending;
  }

  close(): Promise<void> {
    if (this.#closePromise !== undefined) return this.#closePromise;
    // Install the latch before any callback can re-enter the owner.
    this.#closePromise = Promise.resolve();
    this.#closing = true;
    ++this.#generation;
    this.#writable = false;
    this.#inputBuffer = new Uint8Array(0);
    this.#pendingResize = undefined;
    this.#phase = "closing";
    this.#abortSurface();
    try { this.#options.session.close(); } catch { if (this.#error === undefined) this.#error = new SessionError("connection"); }
    this.#publish();
    this.#phase = "closed";
    this.#publish();
    return this.#closePromise;
  }

  async #bootstrap(generation: number): Promise<void> {
    try {
      let target: TerminalTarget | null;
      try {
        target = await this.#options.session.resolveAgentTerminal({
          agentId: this.#options.agentId,
          expectedAgentRevision: this.#options.expectedAgentRevision,
          expectedHead: this.#options.expectedHead,
        });
      } catch (error) {
        if (!this.#current(generation)) return;
        const finite = finiteError(error);
        if (finite instanceof SessionError && (finite.code === "not_found" || finite.code === "stale")) {
          // Discovery can race the durable transition from an admitted run to
          // an active terminal. End only this controller so its owner can
          // retry against a newer public-state head; no terminal authority was
          // minted, so closing the authenticated browser session is needless.
          this.#handleEnded(finite, true);
          return;
        }
        throw error;
      }
      if (!this.#current(generation)) return;
      if (target === null) {
        // A public running task may precede its attachable terminal target.
        // The owner decides from public state whether to wait for the next
        // head or treat this as an ordinary agent with no live terminal.
        this.#handleEnded(new SessionError("not_found"), true);
        return;
      }
      const handle = this.#options.session.openTerminal(target as TerminalTarget, {
        ...(this.#options.resume === undefined ? {} : { afterSequence: this.#options.resume.head, afterSessionId: this.#options.resume.sessionId }),
        onOutput: (output) => {
          const task = this.#writeOutput(generation, output.payload);
          this.#outputTask = task;
          void task.then(() => this.#outputFinished(task), () => this.#outputFinished(task));
          return task;
        },
        onExit: () => this.#handleEnded(new SessionError("closed")),
        onReset: (event) => {
          if (!this.#detachRequested) this.#reset = event;
          this.#handleEnded(new SessionError(this.#detachRequested ? "closed" : "stale"));
        },
        onClose: (error) => {
          // A clean protocol exit/reset closes the handle before delivering
          // its typed callback. Let that callback own the terminal outcome;
          // an undefined close alone carries no failure evidence.
          if (error === undefined) return;
          this.#handleEnded(finiteError(error));
        },
      });
      this.#handle = handle;
      if (!this.#current(generation)) return;
      this.#phase = "attaching";
      this.#publish();
      if (!this.#current(generation)) return;
      let attached;
      for (let attempt = 0; ; attempt += 1) {
        try {
          attached = await handle.attach();
          break;
        } catch (error: unknown) {
          if (!this.#current(generation)) return;
          const finite = finiteError(error);
          const retryable = finite instanceof SessionError && finite.retryable === true;
          if (!retryable || attempt >= ATTACH_RETRY_LIMIT) throw error;
          await (this.#options.attachRetryDelay ?? defaultAttachRetryDelay)(attempt);
          if (!this.#current(generation) || this.#handleClosed) return;
        }
      }
      if (!this.#current(generation)) return;
      if ("kind" in attached) {
        this.#reset = attached;
        this.#fail(new SessionError("stale"));
        return;
      }
      this.#phase = "acquiring";
      this.#publish();
      if (!this.#current(generation)) return;
      try {
        await handle.acquireInput();
        if (!this.#current(generation)) return;
        if (!handle.writable) {
          this.#handleEnded(new SessionError("closed"));
          return;
        }
        this.#writable = true;
      } catch (error) {
        // A lease can be occupied by another observer. Keep the attached
        // display alive as a truthful read-only observer; the session/handle
        // still owns transport and terminal-end failures.
        if (this.#current(generation)) {
          this.#error = finiteError(error);
          this.#phase = "ready";
          this.#publish();
        }
        return;
      }
      if (!this.#current(generation)) return;
      this.#phase = "ready";
      this.#publish();
    } catch (error) {
      if (this.#current(generation)) this.#fail(finiteError(error));
    }
  }

  async #writeOutput(generation: number, payload: Uint8Array): Promise<void> {
    const live = this.#handle !== undefined && !this.#handleClosed && !this.#closing;
    if ((!this.#current(generation) && !this.#detachRequested) || !live) return Promise.reject(new SessionError("closed"));
    try {
      await this.#options.surface.write(payload.slice());
    } catch {
      const live = this.#current(generation) && this.#liveHandle();
      if (live) this.#fail(new SessionError("internal"));
      throw new SessionError(live ? "internal" : "closed");
    }
    const stillLive = this.#handle !== undefined && !this.#handleClosed && !this.#closing;
    if ((!this.#current(generation) && !this.#detachRequested) || !stillLive) throw new SessionError("closed");
  }

  #pumpEffects(): void {
    if (this.#effectTask !== undefined || !this.#writable || !this.#liveHandle()) return;
    const task = this.#runEffects();
    this.#effectTask = task;
    void task.then(() => this.#effectFinished(task), () => this.#effectFinished(task));
  }

  async #runEffects(): Promise<void> {
    const handle = this.#handle;
    const generation = this.#generation;
    if (handle === undefined || !this.#canEffect(handle)) return;
    let inputSinceResize = false;
    while (this.#current(generation) && this.#canEffect(handle) && this.#writable) {
      if (this.#inputBuffer.length > 0 && !inputSinceResize) {
        const payload = this.#inputBuffer.length <= MAX_TERMINAL_PAYLOAD ? this.#inputBuffer : this.#inputBuffer.slice(0, MAX_TERMINAL_PAYLOAD);
        this.#inputBuffer = this.#inputBuffer.slice(payload.length);
        this.#inputInFlightBytes = payload.length;
        let result: TerminalInputResult;
        try {
          if (!this.#canEffect(handle)) return;
          result = await handle.sendInput(payload);
        } catch (error) {
          this.#inputInFlightBytes = 0;
          if (this.#current(generation)) this.#fail(finiteError(error));
          return;
        }
        this.#inputInFlightBytes = 0;
        if (!this.#current(generation) || !this.#canEffect(handle)) return;
        if (result.status === "partial" || result.status === "uncertain") {
          this.#fail(new SessionError("connection"));
          return;
        }
        if (result.status === "rejected") {
          this.#error = new SessionError("invalid_request");
          this.#publish();
          if (!this.#current(generation) || !this.#canEffect(handle)) return;
        }
        inputSinceResize = true;
        continue;
      }
      const resize = this.#pendingResize;
      if (resize !== undefined) {
        this.#pendingResize = undefined;
        if (!this.#canEffect(handle)) return;
        try { await handle.resize(resize.rows, resize.cols); } catch (error) {
          if (this.#current(generation)) this.#fail(finiteError(error));
          return;
        }
        if (!this.#current(generation) || !this.#canEffect(handle)) return;
        inputSinceResize = false;
        continue;
      }
      inputSinceResize = false;
      if (this.#inputBuffer.length === 0) return;
    }
  }

  #effectFinished(task: Promise<void>): void {
    if (this.#effectTask === task) this.#effectTask = undefined;
  }

  #outputFinished(task: Promise<void>): void {
    if (this.#outputTask === task) this.#outputTask = undefined;
  }
  #handleEnded(error: SessionError | ProtocolError, retryDiscovery = false): void {
    if (this.#handleClosed) return;
    this.#handleClosed = true;
    this.#writable = false;
    this.#inputBuffer = new Uint8Array(0);
    this.#inputInFlightBytes = 0;
    this.#pendingResize = undefined;
    this.#retryDiscovery = retryDiscovery;
    ++this.#generation;
    if (!this.#detachRequested) this.#abortSurface();
    if (!this.#closing && this.#phase !== "closed") {
      this.#error = error;
      this.#phase = "closed";
      this.#publish();
    }
  }

  #fail(error: SessionError | ProtocolError): void {
    if (this.#closing || this.#phase === "closed") return;
    this.#error = error;
    this.#writable = false;
    this.#inputBuffer = new Uint8Array(0);
    this.#pendingResize = undefined;
    this.#phase = "closing";
    this.#publish();
    void this.close();
  }

  #liveHandle(handle = this.#handle): boolean {
    return handle !== undefined && handle === this.#handle && !this.#handleClosed && !this.#closing && this.#phase !== "closed";
  }

  #abortSurface(): void {
    if (this.#surfaceAborted) return;
    this.#surfaceAborted = true;
    try { this.#options.surface.abort(); } catch { if (this.#error === undefined) this.#error = new SessionError("internal"); }
  }

  #canEffect(handle = this.#handle): boolean {
    if (handle === undefined || !this.#liveHandle(handle)) return false;
    try {
      if (handle.writable) return true;
    } catch {
      // A broken handle is no longer an authority boundary.
    }
    this.#handleEnded(new SessionError("closed"));
    return false;
  }

  #current(generation: number): boolean {
    return generation === this.#generation && !this.#closing && this.#phase !== "closing" && this.#phase !== "closed";
  }

  #snapshot(): TerminalControllerSnapshot {
    return { phase: this.#phase, writable: this.#writable, error: this.#error, reset: this.#reset, retryDiscovery: this.#retryDiscovery };
  }

  #publish(): void {
    try { this.#options.onChange(this.#snapshot()); } catch { /* presentation callbacks never own the session */ }
  }
}

function finiteError(error: unknown): SessionError | ProtocolError {
  return error instanceof SessionError || error instanceof ProtocolError ? error : new SessionError("connection");
}

export { MAX_PENDING_INPUT_BYTES, TerminalController };
export type { TerminalControllerOptions, TerminalControllerSession };
