import {
  CAPABILITIES,
  MAX_TERMINAL_PAYLOAD,
  MAX_TASK_INSTRUCTION_BYTES,
  ProtocolError,
  SessionError,
  consumePairingChallenge,
  createBrowserClient,
  type BrowserClient,
  type BrowserSession,
  type BrowserSessionOptions,
  type AgentItem,
  type HumanRequestDetail,
  type HumanRequestItem,
  type SessionErrorCode,
  type SessionStatus,
  type StateView,
  type TaskItem,
  type TopologyView,
} from "@dark-factory/client";
import { MAX_PENDING_INPUT_BYTES, TerminalController, type TerminalControllerSnapshot, type TerminalSurface } from "./terminal-controller.js";

const BROWSER_ENDPOINT = new URL("ws://127.0.0.1:43123/browser");
const BROWSER_URL = BROWSER_ENDPOINT.toString();
/** The one loopback address this console dials; SETTINGS shows exactly it. */
export const BROWSER_HOST = BROWSER_ENDPOINT.host;
// terminal_input is the bit a remote grant never carries: only a client paired
// on this machine's own loopback may invite a phone.
const LOOPBACK_GRANT = CAPABILITIES.human_actions | CAPABILITIES.terminal_input;

export type FactoryHumanRequestView = Readonly<{
  request: HumanRequestItem;
  phase: "loading" | "ready" | "replying" | "cancelling";
  question?: string;
  canReply: boolean;
  canCancel: boolean;
  replyMaxBytes: number;
  reply: string;
}>;

export type FactoryRemoteInvite = Readonly<{
  link: string;
  svg: string;
  expiresAtMs: bigint;
}>;

export type FactoryAgentSelection = Readonly<{
  id: string;
  name: string;
  revision: bigint;
}>;

export type FactoryTerminalView = Readonly<{
  agentId: string;
  agentName: string;
  agentRevision: bigint;
  /** Current running task title, when this selected agent has one. */
  taskTitle?: string;
  /** The terminal ended before durable task finalization was published. */
  finishing: boolean;
  phase: "idle" | "resolving" | "attaching" | "acquiring" | "ready" | "closing" | "closed";
  writable: boolean;
  error?: SessionError | ProtocolError;
  paused: boolean;
  instructionPending: boolean;
  instructionError?: SessionError | ProtocolError;
  queued: boolean;
  /** Server replay resets survived by this terminal view; > 0 shows the banner. */
  resets: number;
  surfaceVersion: number;
}>;

/** One console edit at a time: the sidebar shows exactly one form. */
export type FactoryEditView = Readonly<{
  pending: boolean;
  error?: SessionError | ProtocolError;
}>;

export type FactoryAppSnapshot = Readonly<{
  status: SessionStatus;
  state?: StateView;
  error?: SessionError | ProtocolError;
  selectedHumanRequest?: FactoryHumanRequestView;
  selectedAgent?: FactoryAgentSelection;
  terminal?: FactoryTerminalView;
  /** Regenerable project structure, absent until the daemon serves it. */
  topology?: TopologyView;
  edit?: FactoryEditView;
  /** True only while a ready session carries the full loopback grant. */
  remoteInviteAllowed?: boolean;
  remoteInvite?: FactoryRemoteInvite;
  remoteInviteError?: string;
}>;

export type FactoryAppStatus =
  | Readonly<{ status: Exclude<SessionStatus, "closed"> }>
  | Readonly<{ status: "closed"; reason: SessionErrorCode }>;

type HumanSession = Pick<BrowserSession, "getHumanRequestDetail" | "replyHumanRequest" | "cancelHumanRequest">;
type TerminalSession = Pick<BrowserSession, "resolveAgentTerminal" | "openTerminal" | "close">;
type AgentTaskSession = Pick<BrowserSession, "enqueueAgentTask">;
type ConsoleSession = Pick<BrowserSession, "updateAgent" | "updateTask" | "getTopology">;
type RemoteInviteSession = Pick<BrowserSession, "inviteRemote" | "capabilities">;
type ControlledClient = Pick<BrowserClient, "connect" | "close"> & { readonly session?: HumanSession & TerminalSession & AgentTaskSession & ConsoleSession & RemoteInviteSession };
type ClientFactory = (options: BrowserSessionOptions) => ControlledClient;

export type FactoryAppControllerOptions = {
  origin: string;
  location: Pick<Location, "hash" | "pathname" | "search">;
  history: Pick<History, "replaceState" | "state">;
  onChange: (snapshot: FactoryAppSnapshot) => void;
  onStatusChange?: (status: FactoryAppStatus) => void;
  /** Package-internal construction boundary used by DOM-free causal tests. */
  clientFactory?: ClientFactory;
};

type Selection = {
  request: HumanRequestItem;
  detail?: HumanRequestDetail;
  phase: FactoryHumanRequestView["phase"];
  reply: string;
  token: number;
};

type AgentTerminalSelection = {
  agent: AgentItem;
  head: bigint;
  /** Public task identity that terminal discovery is allowed to resolve. */
  task?: Pick<TaskItem, "id" | "revision">;
  finishing: boolean;
  /** Server replay resets survived by this terminal view (banner state). */
  resets: number;
  instructionPending: boolean;
  instructionError?: SessionError | ProtocolError;
  queuedTaskID?: string;
};

type TerminalReplacement = {
  agentId?: string;
  agentRevision?: bigint;
  error?: SessionError | ProtocolError;
};

function agentCurrentTask(agent: AgentItem, state: StateView) {
  for (const task of state.tasks.values()) {
    if (task.assigned_agent_id === agent.id && task.status === "running") return task;
  }
  return undefined;
}

function agentQueuedTask(agent: AgentItem, state: StateView) {
  for (const task of state.tasks.values()) {
    if (task.assigned_agent_id === agent.id && task.status === "queued") return task;
  }
  return undefined;
}

/** Owns one mounted FactoryApp lifecycle and its exact HumanRequest authority. */
export class FactoryAppController {
  readonly #options: FactoryAppControllerOptions;
  #client: ControlledClient | undefined;
  #status: SessionStatus = "idle";
  #statusReason: SessionErrorCode | undefined;
  #lastStatus: FactoryAppStatus | undefined;
  #state: StateView | undefined;
  #error: SessionError | ProtocolError | undefined;
  #selection: Selection | undefined;
  #selectionToken = 0;
  #detailPending = false;
  #selectedAgent: AgentTerminalSelection | undefined;
  #terminal: TerminalController | undefined;
  #terminalSurface: TerminalSurface | undefined;
  #terminalSurfaceToken: object | undefined;
  #terminalSurfaceVersion = 0;
  #terminalGeneration = 0;
  #terminalResetBurst = 0;
  #terminalRetry: { head: bigint; stale: boolean } | undefined;
  #terminalReplacement: TerminalReplacement | undefined;
  #pendingTerminalInput = new Uint8Array(0);
  #pendingTerminalResize: { rows: number; cols: number } | undefined;
  #topology: TopologyView | undefined;
  #topologyPending = false;
  #edit: FactoryEditView | undefined;
  #remoteInvite: FactoryRemoteInvite | undefined;
  #remoteInviteError: string | undefined;
  #remoteInvitePending = false;
  #generation = 0;
  #started = false;
  #closed = false;

  constructor(options: FactoryAppControllerOptions) {
    this.#options = options;
  }

  get snapshot(): FactoryAppSnapshot { return this.#snapshot(); }

  start(): void {
    if (this.#started || this.#closed) return;
    this.#started = true;
    const generation = ++this.#generation;
    let challenge: string | null;
    try {
      challenge = consumePairingChallenge(this.#options.location, this.#options.history);
    } catch {
      this.#status = "closed";
      this.#error = new SessionError("connection");
      this.#statusReason = this.#error.code;
      this.#publish();
      return;
    }

    const factory = this.#options.clientFactory ?? createBrowserClient;
    let client: ControlledClient;
    try {
      client = factory({
        url: BROWSER_URL,
        host: BROWSER_HOST,
        origin: this.#options.origin,
        challenge: challenge ?? undefined,
        onStatus: (status) => this.#receiveStatus(generation, status),
        onState: (state) => this.#receiveState(generation, state),
        onError: (error) => this.#receiveError(generation, error),
      });
    } catch {
      this.#status = "closed";
      this.#error = new SessionError("connection");
      this.#statusReason = this.#error.code;
      this.#publish();
      return;
    }
    if (!this.#current(generation)) {
      client.close();
      return;
    }
    this.#client = client;
    this.#connect(generation);
  }

  close(): void {
    if (this.#closed) return;
    this.#closed = true;
    ++this.#generation;
    this.#clearSelection();
    this.#selectedAgent = undefined;
    this.#terminalReplacement = undefined;
    this.#dropPendingTerminalInput();
    this.#closeTerminal();
    this.#client?.close();
  }

  selectAgent(agent: AgentItem): void {
    if (this.#closed || this.#status !== "ready") return;
    const current = this.#state?.agents.get(agent.id);
    if (current === undefined || current.revision !== agent.revision) {
      this.#error = new SessionError("stale");
      this.#publish();
      return;
    }
    const selected = this.#selectedAgent;
    if (selected?.agent.id === current.id && selected.agent.revision === current.revision && (selected.head === this.#state?.head || this.#terminal !== undefined)) return;
    this.#replaceTerminal({ agentId: current.id, agentRevision: current.revision });
  }

  openTerminalForHumanRequest(request: HumanRequestItem): void {
    if (this.#closed || this.#status !== "ready") return;
    const currentRequest = this.#state?.humanRequests.get(request.id);
    const agent = currentRequest === undefined || currentRequest.revision !== request.revision ? undefined : this.#state?.agents.get(currentRequest.agent_id);
    if (agent === undefined) {
      this.#error = new SessionError("stale");
      this.#publish();
      return;
    }
    this.selectAgent(agent);
  }

  /**
   * The floor's rooms. Topology is regenerable, not durable state: it is
   * fetched on demand and a daemon that cannot serve it simply leaves the
   * floor showing one room per project.
   */
  loadTopology(): void {
    const session = this.#client?.session;
    const project = this.#state === undefined ? undefined : [...this.#state.projects.values()][0];
    if (this.#closed || this.#status !== "ready" || session === undefined || project === undefined || this.#topologyPending) return;
    this.#topologyPending = true;
    const generation = this.#generation;
    void session.getTopology(project.id).then(
      (topology) => {
        this.#topologyPending = false;
        if (!this.#current(generation)) return;
        this.#topology = topology;
        this.#publish();
      },
      () => { this.#topologyPending = false; },
    );
  }

  /**
   * Save the selected agent's configuration against its exact revision. Only
   * the controls the caller changed are sent; an omitted one is left alone,
   * and an empty change is not a write at all.
   */
  async updateAgentConfig(config: { model?: string; reasoningEffort?: string; paused?: boolean }): Promise<void> {
    const selected = this.#selectedAgent;
    const session = this.#client?.session;
    if (this.#closed || this.#status !== "ready" || selected === undefined || session === undefined || this.#edit?.pending === true) return;
    if (Object.values(config).every((value) => value === undefined)) return;
    const generation = this.#generation;
    this.#edit = { pending: true };
    this.#publish();
    try {
      await session.updateAgent({ agentId: selected.agent.id, expectedRevision: selected.agent.revision, ...config });
      if (!this.#current(generation)) return;
      this.#edit = undefined;
    } catch (error) {
      if (!this.#current(generation)) return;
      this.#edit = { pending: false, error: finiteError(error) };
    }
    this.#publish();
  }

  /** Edit one queued task against its exact revision. */
  async editTask(task: Pick<TaskItem, "id" | "revision">, change: { title?: string; priority?: number; assignedAgentId?: string; cancel?: boolean }): Promise<void> {
    const session = this.#client?.session;
    if (this.#closed || this.#status !== "ready" || session === undefined || this.#edit?.pending === true) return;
    const generation = this.#generation;
    this.#edit = { pending: true };
    this.#publish();
    try {
      await session.updateTask({ taskId: task.id, expectedRevision: task.revision, ...change });
      if (!this.#current(generation)) return;
      this.#edit = undefined;
    } catch (error) {
      if (!this.#current(generation)) return;
      this.#edit = { pending: false, error: finiteError(error) };
    }
    this.#publish();
  }

  clearAgentTerminal(): void {
    if (this.#closed) return;
    this.#replaceTerminal({});
  }

  /**
   * Leave the terminal without dropping the agent the sidebar is showing.
   * Replacing the terminal with the same exact agent detaches through the one
   * teardown path, so the protocol handle is released exactly as CLOSE does.
   */
  closeAgentTerminal(): void {
    const selected = this.#selectedAgent;
    if (this.#closed) return;
    this.#replaceTerminal(selected === undefined ? {} : { agentId: selected.agent.id, agentRevision: selected.agent.revision });
  }

  async enqueueAgentInstruction(instruction: string): Promise<boolean> {
    const selected = this.#selectedAgent;
    const session = this.#client?.session;
    const body = instruction.trim();
    if (
      this.#closed ||
      this.#status !== "ready" ||
      selected === undefined ||
      selected.task !== undefined ||
      selected.queuedTaskID !== undefined ||
      selected.agent.paused ||
      selected.instructionPending ||
      session === undefined
    ) return false;
    const byteLength = new TextEncoder().encode(body).length;
    if (byteLength < 1 || byteLength > MAX_TASK_INSTRUCTION_BYTES) {
      selected.instructionError = new SessionError(byteLength > MAX_TASK_INSTRUCTION_BYTES ? "too_large" : "invalid_request");
      this.#publish();
      return false;
    }
    const generation = this.#generation;
    selected.instructionPending = true;
    selected.instructionError = undefined;
    this.#publish();
    try {
      const task = await session.enqueueAgentTask({
        agentId: selected.agent.id,
        expectedAgentRevision: selected.agent.revision,
        instruction: body,
      });
      if (!this.#current(generation) || this.#selectedAgent !== selected) return false;
      selected.instructionPending = false;
      selected.queuedTaskID = task.taskId;
      if (this.#state !== undefined) this.#refreshTerminalTask(selected, this.#state);
      this.#publish();
      this.#reconcileTerminal();
      return true;
    } catch (error) {
      if (!this.#current(generation) || this.#selectedAgent !== selected) return false;
      selected.instructionPending = false;
      selected.instructionError = finiteError(error);
      this.#publish();
      return false;
    }
  }

  /** The mint is never retried: a failure is reported and the operator asks again. */
  async inviteRemote(): Promise<void> {
    const session = this.#client?.session;
    if (this.#closed || this.#status !== "ready" || session === undefined || this.#remoteInvitePending) return;
    const generation = this.#generation;
    this.#remoteInvitePending = true;
    try {
      const invite = await session.inviteRemote();
      if (!this.#current(generation)) return;
      this.#remoteInvite = { link: invite.link, svg: invite.svg, expiresAtMs: invite.expiresAtMs };
      this.#remoteInviteError = undefined;
    } catch (error) {
      if (!this.#current(generation)) return;
      this.#remoteInvite = undefined;
      this.#remoteInviteError = finiteError(error).code;
    } finally {
      this.#remoteInvitePending = false;
    }
    this.#publish();
  }

  dismissRemoteInvite(): void {
    if (this.#closed) return;
    this.#remoteInvite = undefined;
    this.#remoteInviteError = undefined;
    this.#publish();
  }

  beginTerminalSurface(token: object, surfaceVersion = this.#terminalSurfaceVersion): void {
    if (this.#closed || this.#selectedAgent === undefined || surfaceVersion !== this.#terminalSurfaceVersion) return;
    if (this.#terminalSurfaceToken !== undefined && this.#terminalSurfaceToken !== token) return;
    this.#terminalSurfaceToken = token;
  }

  endTerminalSurface(token: object, surfaceVersion = this.#terminalSurfaceVersion): void {
    if (surfaceVersion !== this.#terminalSurfaceVersion || this.#terminalSurfaceToken !== token) return;
    this.#selectedAgent = undefined;
    this.#terminalReplacement = undefined;
    this.#dropPendingTerminalInput();
    this.#closeTerminal();
    this.#error = new SessionError("internal");
    this.#publish();
  }

  setTerminalSurface(token: object, surface: TerminalSurface | undefined, surfaceVersion = this.#terminalSurfaceVersion): void {
    if (this.#closed || this.#selectedAgent === undefined || surfaceVersion !== this.#terminalSurfaceVersion || this.#terminalSurfaceToken !== token) return;
    if (surface === undefined) {
      this.endTerminalSurface(token, surfaceVersion);
      return;
    }
    this.#terminalSurface = surface;
    this.#error = undefined;
    this.#reconcileTerminal();
    this.#publish();
  }

  terminalError(token: object, surfaceVersion = this.#terminalSurfaceVersion): void {
    if (this.#closed || this.#selectedAgent === undefined || surfaceVersion !== this.#terminalSurfaceVersion) return;
    if (this.#terminalSurfaceToken !== undefined && this.#terminalSurfaceToken !== token) return;
    this.#selectedAgent = undefined;
    this.#terminalReplacement = undefined;
    this.#dropPendingTerminalInput();
    this.#closeTerminal();
    this.#error = new SessionError("internal");
    this.#publish();
  }

  sendTerminalText(token: object, value: string, surfaceVersion = this.#terminalSurfaceVersion): void {
    if (this.#selectedAgent === undefined || this.#terminalSurfaceToken === undefined || surfaceVersion !== this.#terminalSurfaceVersion || this.#terminalSurfaceToken !== token || typeof value !== "string") return;
    if (value.length > MAX_PENDING_INPUT_BYTES) {
      this.#disarmTerminal(new SessionError("too_large"));
      return;
    }
    this.#queueTerminalInput(new TextEncoder().encode(value));
  }

  sendTerminalBinary(token: object, value: string, surfaceVersion = this.#terminalSurfaceVersion): void {
    if (this.#selectedAgent === undefined || this.#terminalSurfaceToken === undefined || surfaceVersion !== this.#terminalSurfaceVersion || this.#terminalSurfaceToken !== token || typeof value !== "string") return;
    if (value.length > MAX_PENDING_INPUT_BYTES) {
      this.#disarmTerminal(new SessionError("too_large"));
      return;
    }
    const bytes = new Uint8Array(value.length);
    for (let index = 0; index < value.length; index += 1) {
      const code = value.charCodeAt(index);
      if (code > 0xff) {
        this.#disarmTerminal(new ProtocolError("malformed"));
        return;
      }
      bytes[index] = code;
    }
    this.#queueTerminalInput(bytes);
  }

  resizeTerminal(token: object, rows: number, cols: number, surfaceVersion = this.#terminalSurfaceVersion): void {
    if (surfaceVersion !== this.#terminalSurfaceVersion || this.#terminalSurfaceToken !== token) return;
    this.#pendingTerminalResize = { rows, cols };
    this.#flushTerminalResize();
  }

  async selectHumanRequest(request: HumanRequestItem): Promise<void> {
    if (this.#closed || this.#status !== "ready" || this.#detailPending || this.#selection !== undefined) return;
    const current = this.#state?.humanRequests.get(request.id);
    const session = this.#client?.session;
    if (current === undefined || current.revision !== request.revision || session === undefined) {
      this.#error = new SessionError("stale");
      this.#publish();
      return;
    }

    const generation = this.#generation;
    const token = ++this.#selectionToken;
    this.#selection = { request: current, phase: "loading", reply: "", token };
    this.#detailPending = true;
    this.#error = undefined;
    this.#publish();
    try {
      const detail = await session.getHumanRequestDetail({ requestId: current.id, expectedRevision: current.revision });
      if (!this.#ownsSelection(generation, token)) return;
      const latest = this.#state?.humanRequests.get(current.id);
      if (latest === undefined || latest.revision !== current.revision) {
        this.#clearSelection();
      } else {
        this.#selection = { request: latest, detail, phase: "ready", reply: "", token };
      }
      this.#publish();
    } catch (error) {
      if (!this.#ownsSelection(generation, token)) return;
      this.#clearSelection();
      this.#error = finiteError(error);
      this.#publish();
    } finally {
      this.#detailPending = false;
    }
  }

  setHumanReply(reply: string): void {
    const selection = this.#selection;
    const maximum = selection?.detail?.replyMaxBytes;
    if (selection?.phase !== "ready" || maximum === undefined) return;
    if (reply.length > maximum || new TextEncoder().encode(reply).length > maximum) {
      this.#error = new SessionError("too_large");
      this.#publish();
      return;
    }
    selection.reply = reply;
    this.#error = undefined;
    this.#publish();
  }

  clearHumanRequest(): void {
    if (this.#selection?.phase === "replying" || this.#selection?.phase === "cancelling") return;
    this.#clearSelection();
    this.#publish();
  }

  async replyHumanRequest(): Promise<void> {
    const selection = this.#selection;
    const session = this.#client?.session;
    if (selection?.phase !== "ready" || selection.detail === undefined || session === undefined) return;
    const generation = this.#generation;
    const token = selection.token;
    const detail = selection.detail;
    const reply = selection.reply;
    if (reply.length === 0) {
      this.#error = new SessionError("invalid_request");
      this.#publish();
      return;
    }
    selection.phase = "replying";
    this.#error = undefined;
    this.#publish();
    try {
      await session.replyHumanRequest(detail, reply);
      if (!this.#ownsSelection(generation, token)) return;
      this.#clearSelection();
      this.#publish();
    } catch (error) {
      if (!this.#ownsSelection(generation, token)) return;
      this.#clearSelection();
      this.#error = finiteError(error);
      this.#publish();
    }
  }

  async cancelHumanRequest(): Promise<void> {
    const selection = this.#selection;
    const session = this.#client?.session;
    const cancelRun = selection?.detail?.cancelRun;
    if (selection?.phase !== "ready" || cancelRun === undefined || cancelRun === null || session === undefined) return;
    const generation = this.#generation;
    const token = selection.token;
    selection.phase = "cancelling";
    this.#error = undefined;
    this.#publish();
    try {
      await session.cancelHumanRequest(cancelRun);
      if (!this.#ownsSelection(generation, token)) return;
      this.#clearSelection();
      this.#publish();
    } catch (error) {
      if (!this.#ownsSelection(generation, token)) return;
      this.#clearSelection();
      this.#error = finiteError(error);
      this.#publish();
    }
  }

  #connect(generation: number): void {
    const client = this.#client;
    if (client === undefined || !this.#current(generation)) return;
    void client.connect().catch((error) => {
      if (!this.#current(generation) || this.#status === "closed") return;
      this.#status = "closed";
      this.#error = finiteError(error);
      this.#statusReason = this.#error.code;
      this.#clearSelection();
      this.#terminalReplacement = undefined;
      this.#dropPendingTerminalInput();
      if (this.#terminal !== undefined) this.#closeTerminal();
      this.#publish();
    });
  }

  #receiveStatus(generation: number, status: SessionStatus): void {
    if (!this.#current(generation)) return;
    this.#status = status;
    this.#statusReason = status === "closed" ? this.#error?.code ?? "closed" : undefined;
    if (status !== "ready") {
      this.#clearSelection();
      // A reconnect must not show a code minted for the connection that dropped.
      this.#remoteInvite = undefined;
      this.#remoteInviteError = undefined;
    }
    // A wire-level state restart resnapshots on the same authenticated socket;
    // exact terminal discovery and handles remain owned by that session.
    if (status !== "ready" && status !== "syncing") {
      this.#terminalReplacement = undefined;
      this.#dropPendingTerminalInput();
      if (this.#terminal !== undefined) this.#closeTerminal();
    }
    if (status !== "closed") this.#error = undefined;
    this.#publish();
    if (status === "ready") this.#reconcileTerminal();
  }

  #receiveState(generation: number, state: StateView): void {
    if (!this.#current(generation)) return;
    this.#state = state;
    // Topology belongs to one project; a project that is gone has no floor.
    if (this.#topology !== undefined && !state.projects.has(this.#topology.projectId)) this.#topology = undefined;
    const selectedAgent = this.#selectedAgent;
    const replacementAgentID = this.#terminalReplacement?.agentId;
    if (replacementAgentID !== undefined) {
      const currentAgent = state.agents.get(replacementAgentID);
      this.#terminalReplacement = currentAgent === undefined
        ? { error: new SessionError("stale") }
        : { agentId: currentAgent.id, agentRevision: currentAgent.revision };
    } else if (selectedAgent !== undefined && this.#terminalReplacement === undefined) {
      const currentAgent = state.agents.get(selectedAgent.agent.id);
      if (currentAgent === undefined) {
        this.#replaceTerminal({ error: new SessionError("stale") });
      } else if (currentAgent.revision !== selectedAgent.agent.revision) {
        this.#replaceTerminal({ agentId: currentAgent.id, agentRevision: currentAgent.revision });
      } else {
        selectedAgent.agent = { ...currentAgent };
        if (this.#terminal === undefined) {
          const headChanged = selectedAgent.head !== state.head;
          this.#refreshTerminalTask(selectedAgent, state);
          if (headChanged) {
            selectedAgent.head = state.head;
            this.#terminalRetry = undefined;
          }
        }
      }
    }
    const selected = this.#selection;
    if (selected !== undefined) {
      const current = state.humanRequests.get(selected.request.id);
      if (current === undefined || current.revision !== selected.request.revision) this.#clearSelection();
      else selected.request = current;
    }
    this.#publish();
    this.#reconcileTerminal();
  }

  #receiveError(generation: number, error: SessionError | ProtocolError): void {
    if (!this.#current(generation)) return;
    this.#error = error;
    if (this.#status === "closed") this.#statusReason = error.code;
    this.#publish();
  }

  #current(generation: number): boolean {
    return !this.#closed && generation === this.#generation;
  }

  #ownsSelection(generation: number, token: number): boolean {
    return this.#current(generation) && this.#selection?.token === token;
  }

  #clearSelection(): void {
    ++this.#selectionToken;
    this.#selection = undefined;
  }

  #reconcileTerminal(): void {
    const selected = this.#selectedAgent;
    const surface = this.#terminalSurface;
    const session = this.#client?.session;
    const stateAgent = selected === undefined ? undefined : this.#state?.agents.get(selected.agent.id);
    if (this.#closed || this.#status !== "ready" || selected === undefined || selected.task === undefined || surface === undefined || session === undefined || stateAgent === undefined || stateAgent.revision !== selected.agent.revision || this.#state?.head !== selected.head || this.#terminalRetry?.head === selected.head || this.#terminal !== undefined) return;
    const generation = ++this.#terminalGeneration;
    const controller = new TerminalController({
      session,
      agentId: selected.agent.id,
      expectedAgentRevision: selected.agent.revision,
      expectedHead: selected.head,
      surface,
      onChange: (snapshot) => this.#receiveTerminalSnapshot(generation, controller, snapshot),
    });
    this.#terminal = controller;
    controller.start();
  }

  #receiveTerminalSnapshot(generation: number, controller: TerminalController, snapshot: TerminalControllerSnapshot): void {
    if (this.#closed || generation !== this.#terminalGeneration || controller !== this.#terminal) return;
    if (snapshot.phase === "closed") {
      if (this.#terminalReplacement !== undefined && snapshot.error !== undefined && snapshot.error.code !== "closed") {
        this.#disarmTerminal(snapshot.error);
        return;
      }
      if (this.#finishTerminalReplacement()) return;
      if (snapshot.reset && this.#recoverFromTerminalReset()) return;
      const selected = this.#selectedAgent;
      const state = this.#state;
      const endedTask = selected?.task;
      const staleDiscovery = snapshot.error?.code === "stale";
      const retryDiscovery =
        snapshot.retryDiscovery &&
        selected !== undefined &&
        state !== undefined &&
        (staleDiscovery || agentCurrentTask(selected.agent, state) !== undefined);
      this.#retireTerminal();
      if (retryDiscovery && selected !== undefined && state !== undefined) {
        this.#refreshTerminalTask(selected, state);
        if (selected.head === state.head) {
          this.#terminalRetry = { head: selected.head, stale: staleDiscovery };
        }
        else selected.head = state.head;
        this.#error = undefined;
        this.#publish();
        return;
      }
      const current = selected === undefined ? undefined : state?.agents.get(selected.agent.id);
      const cleanExit = snapshot.error === undefined || snapshot.error.code === "closed";
      if (cleanExit && selected !== undefined && state !== undefined && current !== undefined) {
        selected.agent = { ...current };
        selected.head = state.head;
        if (selected.queuedTaskID === endedTask?.id) selected.queuedTaskID = undefined;
        this.#refreshQueuedTask(selected, state);
        const runningTask = agentCurrentTask(current, state);
        const running = runningTask === undefined ? undefined : { id: runningTask.id, revision: runningTask.revision };
        if (running !== undefined && !sameTaskIdentity(endedTask, running)) {
          selected.task = running;
          selected.finishing = false;
          selected.instructionError = undefined;
          this.#terminalRetry = undefined;
        } else if (running !== undefined) {
          // The provider terminal can close before durable task finalization
          // publishes its terminal state. Keep the exact task selected across
          // that gap; exposing the idle composer here would allow a second
          // instruction while the first task is still running.
          selected.task = running;
          selected.finishing = true;
          this.#terminalRetry = { head: state.head, stale: false };
        } else {
          selected.task = undefined;
          selected.finishing = false;
          this.#terminalRetry = running === undefined ? undefined : { head: state.head, stale: false };
        }
        this.#dropPendingTerminalInput();
      } else {
        this.#selectedAgent = undefined;
        this.#dropPendingTerminalInput();
      }
      this.#error = snapshot.error?.code === "stale" || snapshot.error?.code === "internal" ? snapshot.error : undefined;
      this.#publish();
      return;
    }
    if (snapshot.phase === "closing") {
      if (snapshot.error !== undefined && !snapshot.reset) this.#disarmTerminal(snapshot.error);
      return;
    }
    if (snapshot.phase === "ready") {
      if (this.#selectedAgent !== undefined) this.#selectedAgent.finishing = false;
      this.#terminalResetBurst = 0;
      if (!snapshot.writable) this.#dropPendingTerminalInput();
    }
    this.#publish();
    if (snapshot.writable) {
      this.#flushTerminalInput();
      this.#flushTerminalResize();
    }
  }

  /**
   * A server replay reset ends the protocol handle by design. Recover in
   * place: keep the terminal selection, remount the display (a fresh empty
   * surface — the retained output the old scrollback showed is gone), and
   * reconcile a new controller against current state. Bounded so a reset
   * storm cannot loop; past the bound the ordinary stale teardown stands.
   */
  #recoverFromTerminalReset(): boolean {
    const selected = this.#selectedAgent;
    if (selected === undefined || this.#status !== "ready" || this.#terminalResetBurst >= 3) return false;
    const current = this.#state?.agents.get(selected.agent.id);
    if (current === undefined) return false;
    this.#terminalResetBurst += 1;
    selected.resets += 1;
    selected.agent = { ...current };
    if (this.#state !== undefined) this.#refreshTerminalTask(selected, this.#state);
    selected.head = this.#state?.head ?? selected.head;
    this.#terminal = undefined;
    ++this.#terminalGeneration;
    this.#terminalRetry = undefined;
    this.#terminalSurface = undefined;
    this.#terminalSurfaceToken = undefined;
    this.#pendingTerminalResize = undefined;
    ++this.#terminalSurfaceVersion;
    this.#error = undefined;
    this.#publish();
    return true;
  }

  #flushTerminalResize(): void {
    const terminal = this.#terminal;
    const resize = this.#pendingTerminalResize;
    if (terminal === undefined || resize === undefined || !terminal.snapshot.writable) return;
    this.#pendingTerminalResize = undefined;
    terminal.resize(resize.rows, resize.cols);
  }

  #queueTerminalInput(bytes: Uint8Array): void {
    if (bytes.length === 0) return;
    const terminal = this.#terminal;
    const phase = terminal?.snapshot.phase;
    if (this.#selectedAgent?.task === undefined || (this.#status !== "ready" && this.#status !== "syncing") || phase === "closing" || phase === "closed" || (phase === "ready" && !terminal?.snapshot.writable)) return;
    if (bytes.length > MAX_TERMINAL_PAYLOAD || this.#pendingTerminalInput.length + bytes.length > MAX_PENDING_INPUT_BYTES) {
      this.#disarmTerminal(new SessionError("too_large"));
      return;
    }
    const next = new Uint8Array(this.#pendingTerminalInput.length + bytes.length);
    next.set(this.#pendingTerminalInput);
    next.set(bytes, this.#pendingTerminalInput.length);
    this.#pendingTerminalInput = next;
    this.#flushTerminalInput();
  }

  #flushTerminalInput(): void {
    const terminal = this.#terminal;
    if (terminal === undefined || !terminal.snapshot.writable) return;
    while (this.#pendingTerminalInput.length > 0) {
      const payload = this.#pendingTerminalInput.length <= MAX_TERMINAL_PAYLOAD
        ? this.#pendingTerminalInput
        : this.#pendingTerminalInput.slice(0, MAX_TERMINAL_PAYLOAD);
      if (!terminal.sendInput(payload)) return;
      this.#pendingTerminalInput = this.#pendingTerminalInput.slice(payload.length);
    }
  }

  #dropPendingTerminalInput(): void {
    this.#pendingTerminalInput = new Uint8Array(0);
  }

  #refreshTerminalTask(selected: AgentTerminalSelection, state: StateView): void {
    const task = agentCurrentTask(selected.agent, state);
    this.#refreshQueuedTask(selected, state);
    const current = task === undefined ? undefined : { id: task.id, revision: task.revision };
    if (sameTaskIdentity(selected.task, current)) return;
    selected.task = current;
    selected.finishing = false;
    if (current !== undefined) selected.instructionError = undefined;
    this.#dropPendingTerminalInput();
  }

  #refreshQueuedTask(selected: AgentTerminalSelection, state: StateView): void {
    if (selected.queuedTaskID !== undefined) {
      const queued = state.tasks.get(selected.queuedTaskID);
      if (
        queued !== undefined &&
        (queued.assigned_agent_id !== selected.agent.id || (queued.status !== "queued" && queued.status !== "running"))
      ) selected.queuedTaskID = undefined;
    }
    if (selected.queuedTaskID === undefined) selected.queuedTaskID = agentQueuedTask(selected.agent, state)?.id;
  }

  #replaceTerminal(replacement: TerminalReplacement): void {
    const terminal = this.#terminal;
    if (terminal === undefined) {
      this.#terminalReplacement = replacement;
      this.#finishTerminalReplacement();
      return;
    }
    this.#terminalReplacement = replacement;
    this.#error = undefined;
    // Release the display token before detaching: the view may unmount the
    // surface during teardown, and that is this teardown, not a fault.
    this.#terminalSurfaceToken = undefined;
    void terminal.detach().catch(() => undefined);
    this.#publish();
  }

  #finishTerminalReplacement(): boolean {
    const replacement = this.#terminalReplacement;
    if (replacement === undefined) return false;
    this.#terminalReplacement = undefined;
    this.#dropPendingTerminalInput();
    this.#retireTerminal();
    // A refused edit belongs to the agent it was made against; a new selection
    // must not inherit its error.
    this.#edit = undefined;
    const candidate = replacement.agentId === undefined ? undefined : this.#state?.agents.get(replacement.agentId);
    const agent = candidate?.revision === replacement.agentRevision ? candidate : undefined;
    const task = agent === undefined || this.#state === undefined ? undefined : agentCurrentTask(agent, this.#state);
    const queuedTask = agent === undefined || this.#state === undefined ? undefined : agentQueuedTask(agent, this.#state);
    this.#selectedAgent = agent === undefined ? undefined : {
      agent: { ...agent },
      head: this.#state?.head ?? 0n,
      task: task === undefined ? undefined : { id: task.id, revision: task.revision },
      finishing: false,
      resets: 0,
      instructionPending: false,
      queuedTaskID: queuedTask?.id,
    };
    this.#error = replacement.error ?? (replacement.agentId !== undefined && agent === undefined ? new SessionError("stale") : undefined);
    this.#publish();
    return true;
  }

  #retireTerminal(): TerminalController | undefined {
    const terminal = this.#terminal;
    this.#terminal = undefined;
    this.#terminalResetBurst = 0;
    this.#terminalRetry = undefined;
    ++this.#terminalGeneration;
    this.#terminalSurface = undefined;
    this.#terminalSurfaceToken = undefined;
    this.#pendingTerminalResize = undefined;
    ++this.#terminalSurfaceVersion;
    return terminal;
  }

  #closeTerminal(): void {
    const terminal = this.#retireTerminal();
    if (terminal !== undefined) void terminal.close();
  }

  #disarmTerminal(error: SessionError | ProtocolError): void {
    this.#selectedAgent = undefined;
    this.#terminalReplacement = undefined;
    this.#dropPendingTerminalInput();
    this.#closeTerminal();
    this.#error = error;
    this.#publish();
  }

  #snapshot(): FactoryAppSnapshot {
    const selection = this.#selection;
    return {
      status: this.#status,
      state: this.#state,
      error: this.#error,
      topology: this.#topology,
      edit: this.#edit,
      selectedHumanRequest: selection === undefined ? undefined : {
        request: selection.request,
        phase: selection.phase,
        question: selection.detail?.question,
        canReply: selection.detail?.canReply ?? false,
        canCancel: selection.detail?.cancelRun !== null && selection.detail?.cancelRun !== undefined,
        replyMaxBytes: selection.detail?.replyMaxBytes ?? 0,
        reply: selection.reply,
      },
      remoteInviteAllowed: this.#status === "ready" && ((this.#client?.session?.capabilities ?? 0) & LOOPBACK_GRANT) === LOOPBACK_GRANT,
      remoteInvite: this.#remoteInvite,
      remoteInviteError: this.#remoteInviteError,
      selectedAgent: this.#selectedAgent === undefined ? undefined : {
        id: this.#selectedAgent.agent.id,
        name: this.#selectedAgent.agent.name,
        revision: this.#selectedAgent.agent.revision,
      },
      terminal: this.#selectedAgent === undefined ? undefined : {
        agentId: this.#selectedAgent.agent.id,
        agentName: this.#selectedAgent.agent.name,
        agentRevision: this.#selectedAgent.agent.revision,
        taskTitle: this.#selectedAgent.task === undefined || this.#state === undefined
          ? undefined
          : this.#state.tasks.get(this.#selectedAgent.task.id)?.title,
        finishing: this.#selectedAgent.finishing,
        phase: this.#terminal?.snapshot.phase ?? "idle",
        writable: this.#terminal?.snapshot.writable ?? false,
        error: this.#terminal?.snapshot.error,
        paused: this.#selectedAgent.agent.paused,
        instructionPending: this.#selectedAgent.instructionPending,
        instructionError: this.#selectedAgent.instructionError,
        queued: this.#selectedAgent.queuedTaskID !== undefined && this.#selectedAgent.task === undefined,
        resets: this.#selectedAgent.resets,
        surfaceVersion: this.#terminalSurfaceVersion,
      },
    };
  }

  #publish(): void {
    if (!this.#closed) this.#options.onChange(this.#snapshot());
    if (this.#closed) return;
    const status: FactoryAppStatus = this.#status === "closed"
      ? { status: "closed", reason: this.#statusReason ?? "closed" }
      : { status: this.#status };
    if (sameStatus(this.#lastStatus, status)) return;
    this.#lastStatus = status;
    try { this.#options.onStatusChange?.(status); } catch { /* host callbacks cannot break controller ownership */ }
  }
}

function finiteError(error: unknown): SessionError | ProtocolError {
  return error instanceof SessionError || error instanceof ProtocolError ? error : new SessionError("connection");
}

function sameStatus(left: FactoryAppStatus | undefined, right: FactoryAppStatus): boolean {
  if (left === undefined || left.status !== right.status) return false;
  return left.status !== "closed" || (right.status === "closed" && left.reason === right.reason);
}

function sameTaskIdentity(
  left: Pick<TaskItem, "id" | "revision"> | undefined,
  right: Pick<TaskItem, "id" | "revision"> | undefined,
): boolean {
  return left === undefined ? right === undefined : right !== undefined && left.id === right.id && left.revision === right.revision;
}
