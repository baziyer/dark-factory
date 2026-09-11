import {
  decodeServerControl,
  encodeAuthProve,
  encodeClientControl,
  encodeHumanRequestCancelRun,
  encodeHumanRequestDetailGet,
  encodeHumanRequestReply,
  encodeAgentControl,
  encodePairProve,
  encodeRemoteInvite,
  encodeStateGet,
  encodeStateWatch,
  encodeTaskEnqueue,
  encodeTaskDetailGet,
  encodeTaskHistoryGet,
  encodeTerminalTargetGet,
  type AccountLinkResultBody,
  type AccountUpdateResultBody,
  type AccountsBody,
  type AgentUpdateBody,
  type ProjectLimitsBody,
  type AgentControlAction,
  type AgentControlResultBody,
  type IdlePolicy,
  type SpriteAppearance,
  type AuthResultFrame,
  type ErrorFrame,
  type HelloBody,
  type HumanRequestCancelRunResultBody,
  type HumanRequestReplyResultBody,
  type PairResultFrame,
  type RemoteInviteResultBody,
  type ServerControlFrame,
  type StateChangedFrame,
  type StateSnapshotFrame,
  type TaskEnqueueResultBody,
  type TaskItem,
  type TaskHistoryBody,
  type TaskDetailBody,
  type TaskPeerQuestion,
  type TaskUpdateBody,
  type TerminalTargetDescriptor,
  type TopologyBody,
} from "./control.js";
import { ProtocolError, type ProtocolErrorCode } from "./errors.js";
import { CAPABILITIES, MAX_AGENT_MODEL_BYTES, MAX_AGENT_NAME_BYTES,
  MAX_IDLE_AFTER_SECONDS,
  MAX_IDLE_RUN_BUDGET, MAX_ARRAY_ITEMS, MAX_HUMAN_REPLY_BYTES, MAX_SQLITE_INTEGER, MAX_TASK_INSTRUCTION_BYTES, MAX_TASK_PRIORITY, MAX_TASK_TITLE_BYTES, type CapabilityMask, type ErrorCode } from "./manifest.js";
import { snapshotView, type StateView } from "./state.js";
import { createTerminalHandle, terminalControlFrame, type InternalTerminalHandle, type TerminalHandle, type TerminalOptions } from "./terminal_session.js";
import { decodeTerminalServer } from "./terminal_session.js";
import { buildAuthTranscript, buildPairTranscript, hexBytes } from "./transcript.js";

const CLOSED = 3;
const PUBLIC_KEY_BYTES = 65;
const SIGNATURE_BYTES = 64;

export interface BrowserTimer {
  setTimeout(callback: () => void, delayMs: number): unknown;
  clearTimeout(handle: unknown): void;
  now?: () => number;
}

const REAL_TIMER: BrowserTimer = {
  setTimeout: (callback, delayMs) => globalThis.setTimeout(callback, delayMs),
  clearTimeout: (handle) => globalThis.clearTimeout(handle as ReturnType<typeof setTimeout>),
};

/** The intentionally tiny socket surface makes the client usable without a DOM test framework. */
export interface BrowserSocket {
  readonly readyState: number;
  binaryType?: BinaryType;
  onopen: ((event: unknown) => void) | null;
  onmessage: ((event: { data: unknown }) => void) | null;
  onerror: ((event: unknown) => void) | null;
  onclose: ((event: { code?: number; reason?: string }) => void) | null;
  send(data: string | Uint8Array): void;
  close(code?: number, reason?: string): void;
}

export type BrowserSocketFactory = (url: string) => BrowserSocket;

export type StoredClientKey = {
  clientId: string;
  publicKeySEC1: Uint8Array;
  key: CryptoKey;
  capabilities: CapabilityMask;
};

/** Storage is deliberately narrower than IndexedDB: tests can provide an in-memory store. */
export interface BrowserKeyStore {
  load(): Promise<StoredClientKey | null>;
  save(value: StoredClientKey): Promise<void>;
}

export type SessionErrorCode =
  | "connection"
  | "closed"
  | "pairing_required"
  | "pairing_uncertain"
  | "storage_unavailable"
  | "crypto_unavailable"
  | ProtocolErrorCode
  | ErrorCode;

export class SessionError extends Error {
  readonly code: SessionErrorCode;
  readonly retryable: boolean;

  constructor(code: SessionErrorCode, retryable = false) {
    super(code);
    this.name = "SessionError";
    this.code = code;
    this.retryable = retryable;
  }
}

export type SessionStatus = "idle" | "connecting" | "authenticating" | "syncing" | "ready" | "closed";

export type BrowserSessionOptions = {
  url: string;
  /** Exact values supplied by the already validated HTTP request. */
  host: string;
  origin: string;
  challenge?: string;
  /** Re-pair with the supplied one-shot challenge instead of using a saved key. */
  forcePair?: boolean;
  keyStore?: BrowserKeyStore;
  socketFactory?: BrowserSocketFactory;
  crypto?: Crypto;
  timer?: BrowserTimer;
  reconnectInitialDelayMs?: number;
  reconnectMaxDelayMs?: number;
  /** Called once a PAIR_RESULT has been durably saved, even if the session closed during the save. */
  onPairingPersisted?: () => void;
  onStatus?: (status: SessionStatus) => void;
  onState?: (state: StateView) => void;
  onError?: (error: SessionError | ProtocolError) => void;
};

type Pending = "pair" | "auth" | "snapshot";
type TargetPending = {
  agentId: string;
  expectedAgentRevision: bigint;
  expectedHead: bigint;
  resolve: (value: TerminalTarget | null) => void;
  reject: (error: unknown) => void;
};
type HumanPending = {
  kind: "detail" | "reply" | "cancel";
  requestId: string;
  expectedRevision: bigint;
  expectedRunRevision: bigint;
  runId?: string;
  resolve: (value: unknown) => void;
  reject: (error: unknown) => void;
};
type TaskPending = { taskId: string; expectedAgentRevision: bigint; resolve: (value: { taskId: string; revision: bigint }) => void; reject: (error: unknown) => void };
type AgentControlPending = { operationId: string; taskId: string; runId: string; resolve: (value: AgentControlResult) => void; reject: (error: unknown) => void };
type TaskHistoryPending = { taskId: string; resolve: (value: TaskHistoryView) => void; reject: (error: unknown) => void };
type TaskDetailPending = { taskId: string; expectedRevision: bigint; resolve: (value: TaskDetailView) => void; reject: (error: unknown) => void };
type ConsolePending = { kind: "AGENT_UPDATE_RESULT" | "PROJECT_LIMITS_RESULT" | "TASK_UPDATE_RESULT" | "TOPOLOGY" | "RUN_PATHS" | "TASK_LIST"; entityId: string; resolve: (value: never) => void; reject: (error: unknown) => void };

export type AgentUpdateResult = Readonly<{ agentId: string; revision: bigint }>;
export type ProjectLimitsResult = Readonly<{ projectId: string; revision: bigint }>;
export type TaskUpdateResult = Readonly<{ taskId: string; revision: bigint }>;
export type AgentControlRequest = Readonly<{
  operationId: string;
  taskId: string;
  expectedTaskRevision: bigint;
  target: TerminalTarget;
  action: AgentControlAction;
  instruction?: string;
  successorTaskId?: string;
  successorIncarnationId?: string;
}>;
export type AgentControlResult = Readonly<{ operationId: string; taskId: string; runId: string; status: AgentControlResultBody["status"]; successorTaskId: string }>;
export type TaskHistoryView = Readonly<{ taskId: string; entries: readonly Readonly<{ operationId: string; kind: AgentControlAction; actor: string; body: string; status: "pending" | "delivered" | "unknown" | "rejected"; createdAtMs: bigint }>[] }>;
export type TaskDetailView = Readonly<{ taskId: string; revision: bigint; head: bigint; instruction: string; feedback: string; outcome?: string; nextTextOffset?: bigint; peerQuestions: readonly TaskPeerQuestion[]; nextPeerOffset?: bigint }>;
export type TopologyView = Readonly<{ projectId: string; digest: string; sourceRevision: string; nodes: readonly TopologyBody["nodes"][number][] }>;
/** One agent's live run and the repository directories it has changed. */
export type RunPathsView = Readonly<{ agentId: string; runId: string; paths: readonly string[] }>;
export type TaskListView = Readonly<{ agentId: string; head: bigint; total: bigint; tasks: readonly TaskItem[]; hasMore: boolean }>;
type InvitePending = { resolve: (value: RemoteInvite) => void; reject: (error: unknown) => void };
type AccountPending = { kind: "ACCOUNTS" | "ACCOUNT_LINK_RESULT" | "ACCOUNT_UPDATE_RESULT"; accountId?: string; expectedRevision?: bigint; resolve: (value: never) => void; reject: (error: unknown) => void };

/** One provider login found on the daemon's machine, linked or not. */
export type DiscoveredAccountView = AccountsBody["accounts"][number];
export type AccountLinkResult = Readonly<{ accountId: string; revision: bigint }>;
export type AccountUpdateResult = Readonly<{ accountId: string; revision: bigint }>;

/** One minted remote pairing invitation and the code that carries it. */
export type RemoteInvite = Readonly<{ link: string; expiresAtMs: bigint; svg: string }>;

export type HumanRequestCancelRunDescriptor = Readonly<{
  requestId: string;
  expectedRequestRevision: bigint;
  expectedRunRevision: bigint;
}>;

export type HumanRequestDetail = Readonly<{
  requestId: string;
  revision: bigint;
  question: string;
  options: readonly string[];
  canReply: boolean;
  replyMaxBytes: number;
  terminalTarget: TerminalTarget | null;
  cancelRun: HumanRequestCancelRunDescriptor | null;
}>;

export type HumanReplyResult = Readonly<HumanRequestReplyResultBody>;
export type HumanCancelRunResult = Readonly<HumanRequestCancelRunResultBody>;

declare const TERMINAL_TARGET_BRAND: unique symbol;
/** A target is only constructible by a live BrowserSession. */
export type TerminalTarget = Readonly<{ readonly [TERMINAL_TARGET_BRAND]: true }>;

type TargetAuthority = {
  owner: BrowserSession;
  generation: object;
  descriptor: Readonly<TerminalTargetDescriptor>;
};
const TARGET_AUTHORITIES = new WeakMap<object, TargetAuthority>();

/**
 * One connection generation. It owns one socket and one accumulator and is
 * never reused after close. BrowserClient is the optional reconnect owner.
 */
export class BrowserSession {
  #options: BrowserSessionOptions;
  #socket: BrowserSocket | undefined;
  #status: SessionStatus = "idle";
  #hello: HelloBody | undefined;
  #clientId: string | undefined;
  #authenticated = false;
  #authAttempted = false;
  #capabilities = 0;
  #key: CryptoKey | undefined;
  #publicKey: Uint8Array | undefined;
  #pending = new Map<string, Pending>();
  #targetPending = new Map<string, TargetPending>();
  #subscriptionID: string | undefined;
  #stateHeadFloor = 0n;
  // The published snapshot survives a refresh: readers keep coherent state
  // while a newer one is in flight.
  #state: StateView | undefined;
  // The greatest head any STATE_CHANGED announced, which may be ahead of the
  // published snapshot while a refresh is in flight.
  #notifiedHead = 0n;
  // At most one STATE_GET is outstanding, and a burst of notifications during
  // it collapses into at most one trailing refresh.
  #refreshID: string | undefined;
  #requestNumber = 1;
  #closed = false;
  #pairing = false;
  #pairingBlocked = false;
  #authPromise: Promise<void> | undefined;
  #incoming = Promise.resolve();
  #connectPromise: Promise<void> | undefined;
  #resolveConnect: (() => void) | undefined;
  #rejectConnect: ((error: unknown) => void) | undefined;
  #terminalHandles = new Set<InternalTerminalHandle>();
  #humanPending = new Map<string, HumanPending>();
  #taskPending = new Map<string, TaskPending>();
  #agentControlPending = new Map<string, AgentControlPending>();
  #taskHistoryPending = new Map<string, TaskHistoryPending>();
  #taskDetailPending = new Map<string, TaskDetailPending>();
  #consolePending = new Map<string, ConsolePending>();
  #invitePending = new Map<string, InvitePending>();
  #accountPending = new Map<string, AccountPending>();
  #humanDetails = new WeakSet<HumanRequestDetail>();
  #humanCancelRuns = new WeakMap<HumanRequestCancelRunDescriptor, { detail: HumanRequestDetail; runId: string }>();
  #generationToken: object = {};

  constructor(options: BrowserSessionOptions) {
    this.#options = { ...options, keyStore: options.keyStore ?? createIndexedDBKeyStore() };
  }

  get status(): SessionStatus { return this.#status; }
  get state(): StateView | undefined { return this.#state; }
  get clientId(): string | undefined { return this.#clientId; }
  get capabilities(): CapabilityMask { return this.#capabilities; }
  get pairingBlocked(): boolean { return this.#pairingBlocked; }
  get authAttempted(): boolean { return this.#authAttempted; }

  enqueueAgentTask(request: { agentId: string; expectedAgentRevision: bigint; instruction: string; mode?: "now" | "queue" }): Promise<{ taskId: string; revision: bigint }> {
    try { this.#ensureLive(); } catch (error) { return Promise.reject(error); }
    if (!this.#authenticated) return Promise.reject(new SessionError("unauthorized"));
    if ((this.#capabilities & CAPABILITIES.human_actions) === 0) return Promise.reject(new SessionError("unauthorized"));
    if (!validDynamicID(request.agentId) || request.expectedAgentRevision < 1n || request.expectedAgentRevision > MAX_SQLITE_INTEGER || request.mode !== undefined && request.mode !== "now" && request.mode !== "queue") return Promise.reject(new SessionError("invalid_request"));
    const bytes = new TextEncoder().encode(request.instruction).length;
    if (bytes < 1 || bytes > MAX_TASK_INSTRUCTION_BYTES || /^[ \t\r\n]*$/.test(request.instruction)) return Promise.reject(new SessionError("invalid_request"));
    if (this.#taskPending.size >= MAX_ARRAY_ITEMS) return Promise.reject(new SessionError("rate_limited"));
    let taskId: string, incarnationId: string;
    try { taskId = this.#randomID(); incarnationId = this.#randomID(); } catch (error) { return Promise.reject(error); }
    const id = this.#nextID("task-enqueue");
    let payload: string;
    try { payload = encodeTaskEnqueue(id, { task_id: taskId, incarnation_id: incarnationId, agent_id: request.agentId, expected_agent_revision: request.expectedAgentRevision, instruction: request.instruction, ...(request.mode === "queue" ? { mode: "queue" } : {}) }); } catch (error) { return Promise.reject(error); }
    const result = new Promise<{ taskId: string; revision: bigint }>((resolve, reject) => this.#taskPending.set(id, { taskId, expectedAgentRevision: request.expectedAgentRevision, resolve, reject }));
    try { this.#send(payload); } catch { this.#fail(new SessionError("connection")); }
    return result;
  }

  /** One explicit control against the exact task/run the UI just resolved. */
  controlAgent(request: AgentControlRequest): Promise<AgentControlResult> {
    try { this.#ensureLive(); } catch (error) { return Promise.reject(error); }
    if (!this.#authenticated || (this.#capabilities & CAPABILITIES.human_actions) === 0) return Promise.reject(new SessionError("unauthorized"));
    const needsTerminalInput = request.action === "message" || request.action === "interrupt";
    if (needsTerminalInput && (this.#capabilities & CAPABILITIES.terminal_input) === 0) return Promise.reject(new SessionError("unauthorized"));
    const instruction = request.instruction ?? "";
    const successorTaskId = request.successorTaskId ?? "";
    const successorIncarnationId = request.successorIncarnationId ?? "";
    if (!validDynamicID(request.operationId) || !validDynamicID(request.taskId) || request.expectedTaskRevision < 1n || request.expectedTaskRevision > MAX_SQLITE_INTEGER || !validAgentControlRequest(request.action, instruction, successorTaskId, successorIncarnationId)) return Promise.reject(new SessionError("invalid_request"));
    let target: TargetAuthority;
    try { target = this.#targetAuthority(request.target); } catch (error) { return Promise.reject(error); }
    if (this.#agentControlPending.size >= MAX_ARRAY_ITEMS) return Promise.reject(new SessionError("rate_limited"));
    const id = this.#nextID("agent-control");
    let payload: string;
    try {
      payload = encodeAgentControl(id, {
        operation_id: request.operationId,
        task_id: request.taskId,
        run_id: target.descriptor.run_id,
        expected_task_revision: request.expectedTaskRevision,
        expected_run_revision: target.descriptor.run_revision,
        action: request.action,
        instruction,
        successor_task_id: successorTaskId,
        successor_incarnation_id: successorIncarnationId,
      });
    } catch (error) { return Promise.reject(error); }
    const result = new Promise<AgentControlResult>((resolve, reject) => this.#agentControlPending.set(id, { operationId: request.operationId, taskId: request.taskId, runId: target.descriptor.run_id, resolve, reject }));
    try { this.#send(payload); } catch { this.#fail(new SessionError("connection")); }
    return result;
  }

  /** Durable operator receipts are private to a task, never terminal bytes. */
  getTaskHistory(taskId: string): Promise<TaskHistoryView> {
    try { this.#ensureLive(); } catch (error) { return Promise.reject(error); }
    if (!this.#authenticated || (this.#capabilities & CAPABILITIES.private_human_request_detail) === 0) return Promise.reject(new SessionError("unauthorized"));
    if (!validDynamicID(taskId)) return Promise.reject(new SessionError("invalid_request"));
    if (this.#taskHistoryPending.size >= MAX_ARRAY_ITEMS) return Promise.reject(new SessionError("rate_limited"));
    const id = this.#nextID("task-history");
    let payload: string;
    try { payload = encodeTaskHistoryGet(id, { task_id: taskId }); } catch (error) { return Promise.reject(error); }
    const result = new Promise<TaskHistoryView>((resolve, reject) => this.#taskHistoryPending.set(id, { taskId, resolve, reject }));
    try { this.#send(payload); } catch { this.#fail(new SessionError("connection")); }
    return result;
  }

  /** Private editable base instruction and retained review feedback. */
  getTaskDetail(taskId: string, expectedRevision: bigint, offsets: { textOffset?: bigint; peerOffset?: bigint; expectedHead?: bigint } = {}): Promise<TaskDetailView> {
    try { this.#ensureLive(); } catch (error) { return Promise.reject(error); }
    if (!this.#authenticated || (this.#capabilities & CAPABILITIES.private_human_request_detail) === 0) return Promise.reject(new SessionError("unauthorized"));
    if (!validDynamicID(taskId) || expectedRevision < 1n || expectedRevision > MAX_SQLITE_INTEGER) return Promise.reject(new SessionError("invalid_request"));
    if (this.#taskDetailPending.size >= MAX_ARRAY_ITEMS) return Promise.reject(new SessionError("rate_limited"));
    const id = this.#nextID("task-detail");
    let payload: string;
    try { payload = encodeTaskDetailGet(id, { task_id: taskId, expected_revision: expectedRevision, ...(offsets.textOffset === undefined ? {} : { text_offset: offsets.textOffset }), ...(offsets.peerOffset === undefined ? {} : { peer_offset: offsets.peerOffset }), ...(offsets.expectedHead === undefined ? {} : { expected_head: offsets.expectedHead }) }); } catch (error) { return Promise.reject(error); }
    const result = new Promise<TaskDetailView>((resolve, reject) => this.#taskDetailPending.set(id, { taskId, expectedRevision, resolve, reject }));
    try { this.#send(payload); } catch { this.#fail(new SessionError("connection")); }
    return result;
  }

  /** Edit one agent's configuration. An omitted member is left alone. */
  updateAgent(request: { agentId: string; expectedRevision: bigint; appearance?: SpriteAppearance; model?: string; reasoningEffort?: string; accountId?: string; paused?: boolean; idlePolicy?: IdlePolicy; idleAfterSeconds?: number; idleInstruction?: string; idleRunBudget?: number }): Promise<AgentUpdateResult> {
    const body: AgentUpdateBody = { agent_id: request.agentId, expected_revision: request.expectedRevision };
    if (request.appearance !== undefined) body.appearance = request.appearance;
    if (request.model !== undefined) body.model = request.model;
    if (request.reasoningEffort !== undefined) body.reasoning_effort = request.reasoningEffort;
    if (request.accountId !== undefined) body.account_id = request.accountId;
    if (request.paused !== undefined) body.paused = request.paused;
    if (request.idlePolicy !== undefined) body.idle_policy = request.idlePolicy;
    if (request.idleAfterSeconds !== undefined) body.idle_after_seconds = request.idleAfterSeconds;
    if (request.idleInstruction !== undefined) body.idle_instruction = request.idleInstruction;
    if (request.idleRunBudget !== undefined) body.idle_run_budget = request.idleRunBudget;
    if (bounded(request.model, MAX_AGENT_MODEL_BYTES) || bounded(request.reasoningEffort, MAX_AGENT_MODEL_BYTES) || bounded(request.idleInstruction, MAX_TASK_INSTRUCTION_BYTES)) return Promise.reject(new SessionError("invalid_request"));
    if (request.idleAfterSeconds !== undefined && !(Number.isSafeInteger(request.idleAfterSeconds) && request.idleAfterSeconds >= 0 && request.idleAfterSeconds <= MAX_IDLE_AFTER_SECONDS)) return Promise.reject(new SessionError("invalid_request"));
    if (request.idleRunBudget !== undefined && !(Number.isSafeInteger(request.idleRunBudget) && request.idleRunBudget >= 0 && request.idleRunBudget <= MAX_IDLE_RUN_BUDGET)) return Promise.reject(new SessionError("invalid_request"));
    // Which login an agent runs as is administration, like the logins themselves.
    if (request.accountId !== undefined && (this.#capabilities & CAPABILITIES.administration) === 0) return Promise.reject(new SessionError("unauthorized"));
    return this.#consoleRequest("AGENT_UPDATE_RESULT", request.agentId, request.expectedRevision, "agent-update", (id) => encodeClientControl({ type: "AGENT_UPDATE", id, body }));
  }

  /** Edit one project's future allowance and per-run duration. */
  setProjectLimits(request: { projectId: string; expectedRevision: bigint; runBudget: bigint; maxRunSeconds: number }): Promise<ProjectLimitsResult> {
    const body: ProjectLimitsBody = { project_id: request.projectId, expected_revision: request.expectedRevision, run_budget: request.runBudget, max_run_seconds: request.maxRunSeconds };
    if (request.runBudget < 0n || request.runBudget > MAX_SQLITE_INTEGER || !Number.isSafeInteger(request.maxRunSeconds) || request.maxRunSeconds < 0 || request.maxRunSeconds > 86400) return Promise.reject(new SessionError("invalid_request"));
    if ((this.#capabilities & CAPABILITIES.administration) === 0) return Promise.reject(new SessionError("unauthorized"));
    return this.#consoleRequest("PROJECT_LIMITS_RESULT", request.projectId, request.expectedRevision, "project-limits", (id) => encodeClientControl({ type: "PROJECT_LIMITS", id, body }));
  }

  /** Edit one still-queued task: its brief, priority, assignment, or cancel it. */
  updateTask(request: { taskId: string; expectedRevision: bigint; title?: string; body?: string; priority?: number; assignedAgentId?: string; cancel?: boolean }): Promise<TaskUpdateResult> {
    const body: TaskUpdateBody = { task_id: request.taskId, expected_revision: request.expectedRevision };
    if (request.title !== undefined) body.title = request.title;
    if (request.body !== undefined) body.body = request.body;
    if (request.priority !== undefined) body.priority = request.priority;
    if (request.assignedAgentId !== undefined) body.assigned_agent_id = request.assignedAgentId;
    if (request.cancel === true) body.status = "cancelled";
    if (
      (request.title !== undefined && (request.title.length === 0 || bounded(request.title, MAX_TASK_TITLE_BYTES))) ||
      bounded(request.body, MAX_TASK_INSTRUCTION_BYTES) ||
      (request.priority !== undefined && (!Number.isSafeInteger(request.priority) || Math.abs(request.priority) > MAX_TASK_PRIORITY)) ||
      (request.assignedAgentId !== undefined && !validDynamicID(request.assignedAgentId))
    ) return Promise.reject(new SessionError("invalid_request"));
    return this.#consoleRequest("TASK_UPDATE_RESULT", request.taskId, request.expectedRevision, "task-update", (id) => encodeClientControl({ type: "TASK_UPDATE", id, body }));
  }

  /** The project's regenerable structure, computed on demand by the daemon. */
  getTopology(projectId: string): Promise<TopologyView> {
    return this.#consoleRequest("TOPOLOGY", projectId, 1n, "topology", (id) => encodeClientControl({ type: "TOPOLOGY_GET", id, body: { project_id: projectId } }));
  }

  /** The directories one agent's live run has changed; no run, no paths. */
  getRunPaths(agentId: string): Promise<RunPathsView> {
    return this.#consoleRequest("RUN_PATHS", agentId, 1n, "run-paths", (id) => encodeClientControl({ type: "RUN_PATHS_GET", id, body: { agent_id: agentId } }));
  }

  /** Private, cursor-paged completed work for one agent. */
  getTaskList(agentId: string, cursor: { beforeUpdatedAtMs?: bigint; beforeTaskId?: string } = {}): Promise<TaskListView> {
    try { this.#ensureLive(); } catch (error) { return Promise.reject(error); }
    if (!this.#authenticated || (this.#capabilities & CAPABILITIES.private_human_request_detail) === 0) return Promise.reject(new SessionError("unauthorized"));
    if (!validDynamicID(agentId) || (cursor.beforeUpdatedAtMs === undefined) !== (cursor.beforeTaskId === undefined) || (cursor.beforeUpdatedAtMs !== undefined && (cursor.beforeUpdatedAtMs < 1n || cursor.beforeUpdatedAtMs > MAX_SQLITE_INTEGER)) || (cursor.beforeTaskId !== undefined && !validDynamicID(cursor.beforeTaskId))) return Promise.reject(new SessionError("invalid_request"));
    if (this.#consolePending.size >= MAX_ARRAY_ITEMS) return Promise.reject(new SessionError("rate_limited"));
    const id = this.#nextID("task-list");
    const body = { agent_id: agentId, ...(cursor.beforeUpdatedAtMs === undefined ? {} : { before_updated_at_ms: cursor.beforeUpdatedAtMs, before_task_id: cursor.beforeTaskId! }) };
    let payload: string;
    try { payload = encodeClientControl({ type: "TASK_LIST_GET", id, body }); } catch (error) { return Promise.reject(error); }
    const result = new Promise<TaskListView>((resolve, reject) => this.#consolePending.set(id, { kind: "TASK_LIST", entityId: agentId, resolve: resolve as (value: never) => void, reject }));
    try { this.#send(payload); } catch { this.#fail(new SessionError("connection")); }
    return result;
  }

  /** The provider logins present on the daemon's machine. A linked one
   * carries the account identity an agent selects; nothing secret is served. */
  discoverAccounts(): Promise<readonly DiscoveredAccountView[]> {
    return this.#accountRequest("ACCOUNTS", CAPABILITIES.administration, "accounts", (id) => encodeClientControl({ type: "ACCOUNTS_DISCOVER", id, body: {} }));
  }

  /** Registers one already-existing login so agents can be pointed at it. */
  linkAccount(request: { provider: "claude_code" | "codex"; home: string; label: string }): Promise<AccountLinkResult> {
    return this.#accountRequest("ACCOUNT_LINK_RESULT", CAPABILITIES.administration, "account-link", (id) => encodeClientControl({ type: "ACCOUNT_LINK", id, body: { provider: request.provider, home: request.home, label: request.label } }));
  }

  /** Renames or removes a linked login; removal is refused while referenced. */
  updateAccount(request: { accountId: string; expectedRevision: bigint; label?: string; remove?: boolean }): Promise<AccountUpdateResult> {
    const hasLabel = request.label !== undefined;
    const hasRemove = request.remove === true;
    if (hasLabel === hasRemove || request.label !== undefined && bounded(request.label, MAX_AGENT_NAME_BYTES)) return Promise.reject(new SessionError("invalid_request"));
    if (request.accountId === "" || !validDynamicID(request.accountId) || request.expectedRevision < 1n || request.expectedRevision > MAX_SQLITE_INTEGER) return Promise.reject(new SessionError("invalid_request"));
    if ((this.#capabilities & CAPABILITIES.administration) === 0) return Promise.reject(new SessionError("unauthorized"));
    return this.#accountRequest("ACCOUNT_UPDATE_RESULT", CAPABILITIES.administration, "account-update", (id) => encodeClientControl({ type: "ACCOUNT_UPDATE", id, body: { account_id: request.accountId, expected_revision: request.expectedRevision, ...(request.label === undefined ? { remove: true } : { label: request.label }) } }), { accountId: request.accountId, expectedRevision: request.expectedRevision });
  }

  /** Mints one remote pairing invitation. The mint is never retried: a failed
   * request is reported and the operator asks for another. It needs the full
   * loopback grant, whose terminal_input bit a remote grant never carries, so
   * a remote controller cannot propagate its own pairing. */
  inviteRemote(): Promise<RemoteInvite> {
    try { this.#ensureLive(); } catch (error) { return Promise.reject(error); }
    if (!this.#authenticated) return Promise.reject(new SessionError("unauthorized"));
    const loopbackGrant = CAPABILITIES.human_actions | CAPABILITIES.terminal_input;
    if ((this.#capabilities & loopbackGrant) !== loopbackGrant) return Promise.reject(new SessionError("unauthorized"));
    if (this.#invitePending.size >= MAX_ARRAY_ITEMS) return Promise.reject(new SessionError("rate_limited"));
    const id = this.#nextID("remote-invite");
    let payload: string;
    try { payload = encodeRemoteInvite(id, {}); } catch (error) { return Promise.reject(error); }
    const result = new Promise<RemoteInvite>((resolve, reject) => this.#invitePending.set(id, { resolve, reject }));
    try { this.#send(payload); } catch { this.#fail(new SessionError("connection")); }
    return result;
  }

  resolveAgentTerminal(request: { agentId: string; expectedAgentRevision: bigint; expectedHead: bigint }): Promise<TerminalTarget | null> {
    try { this.#ensureLive(); } catch (error) { return Promise.reject(error); }
    if (!this.#authenticated) return Promise.reject(new SessionError("unauthorized"));
    if ((this.#capabilities & CAPABILITIES.observe) === 0) return Promise.reject(new SessionError("unauthorized"));
    if (!validDynamicID(request.agentId) || request.expectedAgentRevision < 1n || request.expectedAgentRevision > MAX_SQLITE_INTEGER || request.expectedHead < 0n || request.expectedHead > MAX_SQLITE_INTEGER) return Promise.reject(new SessionError("invalid_request"));
    if (this.#targetPending.size >= MAX_ARRAY_ITEMS) return Promise.reject(new SessionError("rate_limited"));
    const id = this.#nextID("terminal-target");
    let payload: string;
    try { payload = encodeTerminalTargetGet(id, { agent_id: request.agentId, expected_agent_revision: request.expectedAgentRevision, expected_head: request.expectedHead }); } catch (error) { return Promise.reject(error); }
    const result = new Promise<TerminalTarget | null>((resolve, reject) => this.#targetPending.set(id, { ...request, resolve, reject }));
    try { this.#send(payload); } catch { this.#fail(new SessionError("connection")); }
    return result;
  }

  openTerminal(target: TerminalTarget, options: TerminalOptions = {}): TerminalHandle {
    this.#ensureLive();
    if (!this.#authenticated) throw new SessionError("unauthorized");
    if (typeof target !== "object" || target === null) throw new SessionError("stale");
    const authority = TARGET_AUTHORITIES.get(target as object);
    if (authority === undefined || authority.owner !== this || authority.generation !== this.#generationToken) throw new SessionError("stale");
    let previous: InternalTerminalHandle | undefined;
    for (const existing of this.#terminalHandles) {
      if (!existing.closed) throw new SessionError("invalid_request");
      previous = existing;
    }
    this.#terminalHandles.clear();
    if (previous !== undefined) this.#terminalHandles.add(previous);
    const handle = createTerminalHandle({ runId: authority.descriptor.run_id, sessionId: authority.descriptor.session_id, runRevision: authority.descriptor.run_revision, sessionRevision: authority.descriptor.session_revision }, options, (id, payload) => {
      this.#ensureLive();
      if (!this.#authenticated) throw new SessionError("unauthorized");
      this.#send(payload);
    }, (prefix) => this.#nextID(prefix), (error) => this.#fail(error instanceof SessionError ? error : new SessionError("connection")), this.#options.timer ?? REAL_TIMER, (this.#capabilities & CAPABILITIES.terminal_input) !== 0);
    this.#terminalHandles.add(handle);
    return handle;
  }

  getHumanRequestDetail(request: { requestId: string; expectedRevision: bigint }): Promise<HumanRequestDetail> {
    try { this.#ensureHumanOperation(request.requestId); } catch (error) { return Promise.reject(error); }
    if ((this.#capabilities & CAPABILITIES.private_human_request_detail) === 0) return Promise.reject(new SessionError("unauthorized"));
    if (request.expectedRevision < 1n || request.expectedRevision > MAX_SQLITE_INTEGER) return Promise.reject(new SessionError("invalid_request"));
    const id = this.#nextID("human-detail");
    let payload: string;
    try { payload = encodeHumanRequestDetailGet(id, { request_id: request.requestId, expected_revision: request.expectedRevision }); } catch (error) { return Promise.reject(error); }
    return this.#humanRequest<HumanRequestDetail>(id, { kind: "detail", requestId: request.requestId, expectedRevision: request.expectedRevision, expectedRunRevision: 0n }, payload);
  }

  replyHumanRequest(detail: HumanRequestDetail, reply: string): Promise<HumanReplyResult> {
    try { this.#ensureHumanOperation(detail.requestId); } catch (error) { return Promise.reject(error); }
    if ((this.#capabilities & CAPABILITIES.human_actions) === 0) return Promise.reject(new SessionError("unauthorized"));
    if (!this.#humanDetails.has(detail) || !detail.canReply || detail.terminalTarget === null || detail.cancelRun === null) return Promise.reject(new SessionError("stale"));
    const replyBytes = new TextEncoder().encode(reply).length;
    if (replyBytes < 1 || replyBytes > detail.replyMaxBytes || detail.replyMaxBytes !== MAX_HUMAN_REPLY_BYTES) return Promise.reject(new SessionError("invalid_request"));
    if (detail.revision > MAX_SQLITE_INTEGER - 2n) return Promise.reject(new SessionError("stale"));
    const id = this.#nextID("human-reply");
    let payload: string;
    try { payload = encodeHumanRequestReply(id, { request_id: detail.requestId, expected_revision: detail.revision, reply }); } catch (error) { return Promise.reject(error); }
    this.#humanDetails.delete(detail);
    this.#humanCancelRuns.delete(detail.cancelRun);
    return this.#humanRequest<HumanReplyResult>(id, { kind: "reply", requestId: detail.requestId, expectedRevision: detail.revision, expectedRunRevision: 0n }, payload);
  }

  cancelHumanRequest(cancelRun: HumanRequestCancelRunDescriptor): Promise<HumanCancelRunResult> {
    const authority = this.#humanCancelRuns.get(cancelRun);
    if (authority === undefined) return Promise.reject(new SessionError("stale"));
    try { this.#ensureHumanOperation(cancelRun.requestId); } catch (error) { return Promise.reject(error); }
    if ((this.#capabilities & CAPABILITIES.human_actions) === 0) return Promise.reject(new SessionError("unauthorized"));
    if (cancelRun.expectedRequestRevision > MAX_SQLITE_INTEGER - 1n || cancelRun.expectedRunRevision > MAX_SQLITE_INTEGER - 1n) return Promise.reject(new SessionError("stale"));
    const id = this.#nextID("human-cancel");
    let payload: string;
    try { payload = encodeHumanRequestCancelRun(id, { request_id: cancelRun.requestId, expected_request_revision: cancelRun.expectedRequestRevision, expected_run_revision: cancelRun.expectedRunRevision }); } catch (error) { return Promise.reject(error); }
    this.#humanCancelRuns.delete(cancelRun);
    this.#humanDetails.delete(authority.detail);
    return this.#humanRequest<HumanCancelRunResult>(id, { kind: "cancel", requestId: cancelRun.requestId, expectedRevision: cancelRun.expectedRequestRevision, expectedRunRevision: cancelRun.expectedRunRevision, runId: authority.runId }, payload);
  }

  connect(): Promise<void> {
    if (this.#connectPromise !== undefined) return this.#connectPromise;
    if (this.#closed) return Promise.reject(new SessionError("closed"));
    this.#connectPromise = new Promise<void>((resolve, reject) => {
      this.#resolveConnect = resolve;
      this.#rejectConnect = reject;
    });
    this.#setStatus("connecting");
    if (this.#closed) return this.#connectPromise;
    try {
      const factory = this.#options.socketFactory ?? ((url: string) => new WebSocket(url) as unknown as BrowserSocket);
      const socket = factory(this.#options.url);
      if (this.#closed) {
        try { socket.close(1000, "closed"); } catch { /* the session is already closed */ }
        return this.#connectPromise;
      }
      this.#socket = socket;
      socket.binaryType = "arraybuffer";
      socket.onopen = () => { /* HELLO is the first meaningful frame. */ };
      // Keep one reader owner: WebSocket callbacks may arrive while an
      // earlier handler is awaiting WebCrypto or IndexedDB.
      socket.onmessage = (event) => {
        this.#incoming = this.#incoming.then(() => this.#receive(event.data));
      };
      socket.onerror = () => this.#fail(new SessionError("connection"));
      socket.onclose = () => this.#closedByPeer();
    } catch {
      this.#fail(new SessionError("connection"));
    }
    return this.#connectPromise;
  }

  close(): void {
    if (this.#closed) return;
    this.#closed = true;
    this.#pending.clear();
    this.#closeTargetPending(new SessionError("closed"));
    this.#closeTaskPending(new SessionError("closed"));
    this.#closeAgentControlPending(new SessionError("closed"));
    this.#closeTaskHistoryPending(new SessionError("closed"));
    this.#closeTaskDetailPending(new SessionError("closed"));
    this.#closeConsolePending(new SessionError("closed"));
    this.#closeInvitePending(new SessionError("closed"));
    this.#closeAccountPending(new SessionError("closed"));
    this.#closeHumanPending(new SessionError("closed"));
    for (const handle of this.#terminalHandles) handle.terminate(new SessionError("closed"));
    this.#terminalHandles.clear();
    this.#discardState();
    const socket = this.#socket;
    this.#socket = undefined;
    try { socket?.close(1000, "closed"); } catch { /* finite close semantics */ }
    this.#rejectConnect?.(new SessionError("closed"));
    this.#rejectConnect = undefined;
    this.#resolveConnect = undefined;
    this.#setStatus("closed");
  }

  #setStatus(status: SessionStatus): void {
    this.#status = status;
    notify(this.#options.onStatus, status);
  }

  async #receive(data: unknown): Promise<void> {
    if (this.#closed) return;
    if (typeof Blob !== "undefined" && data instanceof Blob) { try { data = new Uint8Array(await data.arrayBuffer()); } catch { this.#fail(new ProtocolError("malformed")); return; } }
    else if (data instanceof ArrayBuffer) data = new Uint8Array(data);
    else if (ArrayBuffer.isView(data)) data = new Uint8Array(data.buffer, data.byteOffset, data.byteLength);
    if (typeof data !== "string") {
      if (!this.#authenticated || !(data instanceof Uint8Array)) { this.#fail(new ProtocolError("malformed")); return; }
      try {
        const terminal = decodeTerminalServer(data);
        const matched = await this.#routeBinary(terminal);
        if (!matched) this.#fail(new ProtocolError("malformed"));
      } catch (error) { this.#fail(error instanceof ProtocolError ? error : new ProtocolError("malformed")); }
      return;
    }
    let frame: ServerControlFrame;
    try { frame = decodeServerControl(data); } catch (error) {
      this.#fail(error instanceof ProtocolError ? error : new ProtocolError("malformed"));
      return;
    }
    try {
      if (this.#hello === undefined) {
        if (frame.type !== "HELLO") throw new ProtocolError("wrong_direction");
        this.#hello = frame.body;
        await this.#authenticate();
        return;
      }
      if (!this.#authenticated) {
        await this.#authenticationFrame(frame);
        return;
      }
      this.#applicationFrame(frame);
    } catch (error) {
      this.#fail(error instanceof ProtocolError || error instanceof SessionError ? error : new SessionError("connection"));
    }
  }

  async #authenticate(): Promise<void> {
    if (this.#authPromise !== undefined) return this.#authPromise;
    this.#setStatus("authenticating");
    this.#authPromise = (async () => {
      let stored: StoredClientKey | null;
      try { stored = await this.#options.keyStore!.load(); } catch { throw new SessionError("storage_unavailable"); }
      this.#ensureLive();
      if (stored === null || this.#options.forcePair === true) {
        if (this.#options.challenge === undefined) throw new SessionError("pairing_required");
        await this.#pair();
      } else {
        this.#validateStored(stored);
        this.#key = stored.key;
        this.#publicKey = stored.publicKeySEC1.slice();
        this.#clientId = stored.clientId;
        this.#capabilities = stored.capabilities;
        const signature = await this.#sign(buildAuthTranscript({ ...this.#transcriptBase(), client_id: stored.clientId }));
        this.#ensureLive();
        const id = this.#nextID("auth");
        this.#authAttempted = true;
        this.#sendAuth("auth", id, encodeAuthProve(id, { client_id: stored.clientId, signature }));
      }
    })();
    return this.#authPromise;
  }

  async #pair(): Promise<void> {
    this.#pairing = true;
    this.#pairingBlocked = true;
    const crypto = this.#crypto();
    let generated: CryptoKeyPair;
    try {
      generated = await crypto.subtle.generateKey({ name: "ECDSA", namedCurve: "P-256" }, false, ["sign", "verify"]) as CryptoKeyPair;
      const publicKey = await crypto.subtle.exportKey("raw", generated.publicKey);
      if (!(publicKey instanceof ArrayBuffer) || publicKey.byteLength !== PUBLIC_KEY_BYTES) throw new SessionError("crypto_unavailable");
      this.#key = generated.privateKey;
      this.#publicKey = new Uint8Array(publicKey);
      this.#ensureLive();
    } catch (error) {
      if (error instanceof SessionError) throw error;
      throw new SessionError("crypto_unavailable");
    }
    const challenge = this.#options.challenge;
    if (challenge === undefined) throw new SessionError("pairing_required");
    const signature = await this.#sign(buildPairTranscript({ ...this.#transcriptBase(), challenge, public_key_sec1: toHex(this.#publicKey) }));
    this.#ensureLive();
    const id = this.#nextID("pair");
    this.#sendAuth("pair", id, encodePairProve(id, { challenge, public_key_sec1: toHex(this.#publicKey), signature }));
  }

  async #authenticationFrame(frame: ServerControlFrame): Promise<void> {
    if (frame.type === "ERROR") throw new SessionError(frame.body.code, frame.body.retryable);
    if (frame.type === "PAIR_RESULT") {
      if (!this.#pairing || !this.#pending.has(frame.id) || this.#pending.get(frame.id) !== "pair") throw new ProtocolError("malformed");
      this.#pending.delete(frame.id);
      await this.#finishPair(frame);
    } else if (frame.type === "AUTH_RESULT") {
      if (this.#pairing || !this.#pending.has(frame.id) || this.#pending.get(frame.id) !== "auth") throw new ProtocolError("malformed");
      this.#pending.delete(frame.id);
      this.#finishAuth(frame);
    } else {
      throw new ProtocolError("wrong_direction");
    }
  }

  async #finishPair(frame: PairResultFrame): Promise<void> {
    const key = this.#key;
    const publicKey = this.#publicKey;
    if (key === undefined || publicKey === undefined) throw new SessionError("crypto_unavailable");
    this.#ensureLive();
    try {
      await this.#options.keyStore!.save({ clientId: frame.body.client_id, publicKeySEC1: publicKey.slice(), key, capabilities: frame.body.capabilities });
    } catch { throw new SessionError("storage_unavailable"); }
    notify(this.#options.onPairingPersisted);
    // Persistence resolves the one-shot pairing outcome even if close raced
    // the await. The live-session fence below still prevents any old socket
    // or closed lifecycle from being revived.
    this.#pairingBlocked = false;
    this.#ensureLive();
    this.#pairing = false;
    this.#options = { ...this.#options, challenge: undefined };
    this.#clientId = frame.body.client_id;
    this.#capabilities = frame.body.capabilities;
    this.#authenticated = true;
    this.#ready();
  }

  #finishAuth(frame: AuthResultFrame): void {
    if (frame.body.client_id !== this.#clientId) throw new ProtocolError("unauthorized");
    this.#capabilities = frame.body.capabilities;
    this.#authenticated = true;
    this.#ready();
  }

  #ready(): void {
    this.#setStatus("syncing");
    this.#ensureLive();
    this.#resolveConnect?.();
    this.#resolveConnect = undefined;
    this.#rejectConnect = undefined;
    this.#requestSnapshot();
  }

  #applicationFrame(frame: ServerControlFrame): void {
    if (frame.type === "TERMINAL_TARGET") {
      this.#terminalTarget(frame);
      return;
    }
    if (frame.type === "TASK_ENQUEUE_RESULT") {
      this.#taskResult(frame.body, frame.id);
      return;
    }
    if (frame.type === "AGENT_CONTROL_RESULT") {
      this.#agentControlResult(frame.body, frame.id);
      return;
    }
    if (frame.type === "TASK_HISTORY") {
      this.#taskHistoryResult(frame.body, frame.id);
      return;
    }
    if (frame.type === "TASK_DETAIL") {
      this.#taskDetailResult(frame.body, frame.id);
      return;
    }
    if (frame.type === "AGENT_UPDATE_RESULT" || frame.type === "PROJECT_LIMITS_RESULT" || frame.type === "TASK_UPDATE_RESULT" || frame.type === "TOPOLOGY" || frame.type === "RUN_PATHS" || frame.type === "TASK_LIST") {
      this.#consoleResult(frame);
      return;
    }
    if (frame.type === "REMOTE_INVITE_RESULT") {
      this.#inviteResult(frame.body, frame.id);
      return;
    }
    if (frame.type === "ACCOUNTS" || frame.type === "ACCOUNT_LINK_RESULT" || frame.type === "ACCOUNT_UPDATE_RESULT") {
      this.#accountResult(frame);
      return;
    }
    if (terminalControlFrame(frame)) {
      if (frame.type === "TERMINAL_EOF") { if (!this.#anyTerminal((handle) => handle.receiveEOF(frame.id, frame.body))) throw new ProtocolError("malformed"); return; }
      if (frame.type === "TERMINAL_EXIT") { if (!this.#anyTerminal((handle) => handle.receiveExit(frame.id, frame.body))) throw new ProtocolError("malformed"); return; }
      if (frame.type === "TERMINAL_RESET") { if (!this.#anyTerminal((handle) => handle.receiveReset(frame.id, frame.body))) throw new ProtocolError("malformed"); return; }
      if (!this.#anyTerminal((handle) => handle.receive(frame))) throw new ProtocolError("malformed");
      return;
    }
    if (frame.type === "HUMAN_REQUEST_DETAIL" || frame.type === "HUMAN_REQUEST_REPLY_RESULT" || frame.type === "HUMAN_REQUEST_CANCEL_RUN_RESULT") {
      this.#humanResult(frame);
      return;
    }
    switch (frame.type) {
      case "STATE_SNAPSHOT": this.#snapshot(frame); return;
      case "STATE_CHANGED": this.#changed(frame); return;
      case "ERROR": this.#errorFrame(frame); return;
      default: throw new ProtocolError("wrong_direction");
    }
  }

  #snapshot(frame: StateSnapshotFrame): void {
    if (this.#refreshID !== frame.id || this.#pending.get(frame.id) !== "snapshot") throw new ProtocolError("malformed");
    this.#refreshID = undefined;
    this.#pending.delete(frame.id);
    // Publication is monotonic. A snapshot older than one already published,
    // or older than a head this session already observed, is never shown.
    if (frame.body.head < this.#stateHeadFloor) throw new ProtocolError("malformed");
    const first = this.#subscriptionID === undefined;
    if (!first && this.#state !== undefined && frame.body.head <= this.#state.head) {
      this.#maybeRefresh();
      return;
    }
    this.#advanceStateHead(frame.body.head);
    this.#state = snapshotView(frame.body);
    notify(this.#options.onState, this.#state);
    this.#ensureLive();
    if (first) {
      this.#setStatus("ready");
      const id = this.#nextID("watch");
      this.#subscriptionID = id;
      this.#send(encodeStateWatch(id, { after_head: frame.body.head }));
    }
    this.#maybeRefresh();
  }

  #changed(frame: StateChangedFrame): void {
    if (frame.id !== this.#subscriptionID) throw new ProtocolError("malformed");
    // Heads are strictly increasing for one watch. A repeat or a regression is
    // a protocol fault, not something to reconcile.
    if (frame.body.head <= this.#notifiedHead) throw new ProtocolError("malformed");
    this.#notifiedHead = frame.body.head;
    this.#maybeRefresh();
  }

  /** One refresh in flight; a burst becomes at most one trailing refresh. */
  #maybeRefresh(): void {
    if (this.#state !== undefined && this.#notifiedHead <= this.#state.head) return;
    if (this.#refreshID !== undefined) return;
    this.#requestSnapshot();
  }

  #requestSnapshot(): void {
    const id = this.#nextID("state");
    this.#refreshID = id;
    this.#pending.set(id, "snapshot");
    this.#send(encodeStateGet(id, {}));
  }

  #discardState(): void {
    this.#state = undefined;
    this.#refreshID = undefined;
    this.#subscriptionID = undefined;
  }

  #errorFrame(frame: ErrorFrame): void {
    const id = frame.id;
    if (id !== undefined) {
      const target = this.#targetPending.get(id);
      if (target !== undefined) {
        this.#targetPending.delete(id);
        target.reject(new SessionError(frame.body.code, frame.body.retryable));
        return;
      }
      const task = this.#taskPending.get(id);
      if (task !== undefined) {
        this.#taskPending.delete(id);
        task.reject(new SessionError(frame.body.code, frame.body.retryable));
        return;
      }
      const control = this.#agentControlPending.get(id);
      if (control !== undefined) {
        this.#agentControlPending.delete(id);
        control.reject(new SessionError(frame.body.code, frame.body.retryable));
        return;
      }
      const history = this.#taskHistoryPending.get(id);
      if (history !== undefined) {
        this.#taskHistoryPending.delete(id);
        history.reject(new SessionError(frame.body.code, frame.body.retryable));
        return;
      }
      const detail = this.#taskDetailPending.get(id);
      if (detail !== undefined) {
        this.#taskDetailPending.delete(id);
        detail.reject(new SessionError(frame.body.code, frame.body.retryable));
        return;
      }
      const console = this.#consolePending.get(id);
      if (console !== undefined) {
        this.#consolePending.delete(id);
        console.reject(new SessionError(frame.body.code, frame.body.retryable));
        return;
      }
      const invite = this.#invitePending.get(id);
      if (invite !== undefined) {
        this.#invitePending.delete(id);
        invite.reject(new SessionError(frame.body.code, frame.body.retryable));
        return;
      }
      const account = this.#accountPending.get(id);
      if (account !== undefined) {
        this.#accountPending.delete(id);
        account.reject(new SessionError(frame.body.code, frame.body.retryable));
        return;
      }
    }
    if (id !== undefined && this.#anyTerminal((handle) => handle.receiveError(id, new SessionError(frame.body.code, frame.body.retryable)))) return;
    if (id !== undefined) {
      const pending = this.#humanPending.get(id);
      if (pending !== undefined) {
        this.#humanPending.delete(id);
        pending.reject(new SessionError(frame.body.code, frame.body.retryable));
        return;
      }
    }
    if (id !== undefined && this.#pending.has(id)) {
      this.#pending.delete(id);
    }
    throw new SessionError(frame.body.code, frame.body.retryable);
  }

  #advanceStateHead(head: bigint): void {
    if (head < this.#stateHeadFloor) throw new ProtocolError("malformed");
    this.#stateHeadFloor = head;
  }

  #terminalTarget(frame: Extract<ServerControlFrame, { type: "TERMINAL_TARGET" }>): void {
    const pending = this.#targetPending.get(frame.id);
    if (pending === undefined || frame.body.agent_id !== pending.agentId || frame.body.agent_revision !== pending.expectedAgentRevision || frame.body.head !== pending.expectedHead) throw new ProtocolError("malformed");
    this.#targetPending.delete(frame.id);
    // State events and restart snapshots share this socket with private target
    // replies. A reply computed at N may arrive after canonical state has
    // already advanced to N+1; never mint or accept absence from that
    // overtaken observation.
    if (frame.body.head < this.#stateHeadFloor) {
      pending.reject(new SessionError("stale"));
      return;
    }
    if (frame.body.target === null) {
      pending.resolve(null);
      return;
    }
    this.#ensureLive();
    pending.resolve(this.#mintTarget(frame.body.target));
  }

  #sendAuth(kind: "pair" | "auth", id: string, payload: string): void {
    this.#pending.set(id, kind);
    this.#send(payload);
  }

  #send(payload: string | Uint8Array): void {
    if (this.#closed || this.#socket === undefined || this.#socket.readyState === CLOSED) throw new SessionError("closed");
    try { this.#socket.send(payload); } catch { throw new SessionError("connection"); }
  }

  async #routeBinary(frame: import("./terminal.js").TerminalFrame): Promise<boolean> {
    let matched = false;
    for (const handle of this.#terminalHandles) if (await handle.receiveBinary(frame)) matched = true;
    return matched;
  }

  #anyTerminal(test: (handle: InternalTerminalHandle) => boolean): boolean {
    for (const handle of this.#terminalHandles) if (test(handle)) return true;
    return false;
  }

  #nextID(prefix: string): string { return `${prefix}-${this.#requestNumber++}`; }

  #randomID(): string {
    return randomOperationID(this.#crypto());
  }

  #transcriptBase(): { daemon_id: string; boot_id: string; connection_nonce: string; host: string; origin: string } {
    const hello = this.#hello;
    if (hello === undefined) throw new ProtocolError("unauthorized");
    return { daemon_id: hello.daemon_id, boot_id: hello.boot_id, connection_nonce: hello.connection_nonce, host: this.#options.host, origin: this.#options.origin };
  }

  #crypto(): Crypto {
    const value = this.#options.crypto ?? globalThis.crypto;
    if (value === undefined) throw new SessionError("crypto_unavailable");
    return value;
  }

  async #sign(message: Uint8Array): Promise<string> {
    const key = this.#key;
    if (key === undefined || key.extractable || key.algorithm.name !== "ECDSA") throw new SessionError("crypto_unavailable");
    try {
      const signed = await this.#crypto().subtle.sign({ name: "ECDSA", hash: "SHA-256" }, key, message as unknown as BufferSource);
      this.#ensureLive();
      if (!(signed instanceof ArrayBuffer) || signed.byteLength !== SIGNATURE_BYTES) throw new SessionError("crypto_unavailable");
      return toHex(new Uint8Array(signed));
    } catch (error) {
      if (error instanceof SessionError) throw error;
      throw new SessionError("crypto_unavailable");
    }
  }

  #validateStored(value: StoredClientKey): void {
    const knownCapabilities = CAPABILITIES.observe | CAPABILITIES.private_human_request_detail | CAPABILITIES.human_actions | CAPABILITIES.terminal_input | CAPABILITIES.administration;
    if (value === null || typeof value !== "object") throw new SessionError("storage_unavailable");
    const algorithm = value.key?.algorithm as EcKeyAlgorithm | undefined;
    if (typeof value.clientId !== "string" || !(value.publicKeySEC1 instanceof Uint8Array) || value.key === undefined || value.key === null || value.publicKeySEC1.length !== PUBLIC_KEY_BYTES || value.publicKeySEC1[0] !== 4 || value.key.extractable || algorithm?.name !== "ECDSA" || algorithm.namedCurve !== "P-256" || !Number.isSafeInteger(value.capabilities) || value.capabilities < CAPABILITIES.observe || (value.capabilities & ~knownCapabilities) !== 0) throw new SessionError("storage_unavailable");
    // Exercise the same strict fixed-width boundary used by the wire codec.
    try { hexBytes(value.clientId, 16); } catch { throw new SessionError("storage_unavailable"); }
  }

  #fail(error: SessionError | ProtocolError | import("./terminal_session.js").SessionErrorLike): void {
    if (this.#closed) return;
    let normalized: SessionError | ProtocolError = error instanceof SessionError || error instanceof ProtocolError ? error : new SessionError("connection");
    if (this.#pairing && !(normalized instanceof ProtocolError) && (normalized.code === "connection" || normalized.code === "closed")) normalized = new SessionError("pairing_uncertain");
    this.#closed = true;
    this.#pending.clear();
    this.#closeTargetPending(normalized);
    this.#closeTaskPending(normalized);
    this.#closeAgentControlPending(normalized);
    this.#closeTaskHistoryPending(normalized);
    this.#closeTaskDetailPending(normalized);
    this.#closeConsolePending(normalized);
    this.#closeInvitePending(normalized);
    this.#closeAccountPending(normalized);
    this.#closeHumanPending(normalized);
    for (const handle of this.#terminalHandles) handle.terminate(normalized);
    this.#terminalHandles.clear();
    this.#discardState();
    const socket = this.#socket;
    this.#socket = undefined;
    try { socket?.close(1000, "protocol"); } catch { /* ignore close failures */ }
    this.#rejectConnect?.(normalized);
    this.#rejectConnect = undefined;
    this.#resolveConnect = undefined;
    notify(this.#options.onError, normalized);
    this.#setStatus("closed");
  }

  #humanResult(frame: Extract<ServerControlFrame, { type: "HUMAN_REQUEST_DETAIL" | "HUMAN_REQUEST_REPLY_RESULT" | "HUMAN_REQUEST_CANCEL_RUN_RESULT" }>): void {
    const pending = this.#humanPending.get(frame.id);
    const kind = frame.type === "HUMAN_REQUEST_DETAIL" ? "detail" : frame.type === "HUMAN_REQUEST_REPLY_RESULT" ? "reply" : "cancel";
    if (pending === undefined || pending.kind !== kind || frame.body.request_id !== pending.requestId) throw new ProtocolError("malformed");
    if (frame.type === "HUMAN_REQUEST_DETAIL") {
      if (frame.body.revision !== pending.expectedRevision) throw new ProtocolError("malformed");
      const terminalTarget = frame.body.terminal_target === null ? null : this.#mintTarget(frame.body.terminal_target);
      let cancelRun: HumanRequestCancelRunDescriptor | null = null;
      if (frame.body.cancel_run !== null) {
        if (terminalTarget === null) throw new ProtocolError("malformed");
        cancelRun = Object.freeze({
          requestId: frame.body.request_id,
          expectedRequestRevision: frame.body.cancel_run.expected_request_revision,
          expectedRunRevision: frame.body.cancel_run.expected_run_revision,
        });
      }
      const detail: HumanRequestDetail = Object.freeze({
        requestId: frame.body.request_id,
        revision: frame.body.revision,
        question: frame.body.question,
        options: Object.freeze([...(frame.body.options ?? [])]),
        canReply: frame.body.can_reply,
        replyMaxBytes: frame.body.reply_max_bytes,
        terminalTarget,
        cancelRun,
      });
      this.#humanDetails.add(detail);
      if (cancelRun !== null && terminalTarget !== null) this.#humanCancelRuns.set(cancelRun, { detail, runId: this.#targetAuthority(terminalTarget).descriptor.run_id });
      this.#humanPending.delete(frame.id);
      pending.resolve(detail);
      return;
    }
    if (frame.type === "HUMAN_REQUEST_REPLY_RESULT") {
      if (frame.body.revision !== pending.expectedRevision + 2n) throw new ProtocolError("malformed");
      this.#humanPending.delete(frame.id);
      pending.resolve(Object.freeze({ ...frame.body }));
      return;
    }
    if (pending.runId === undefined || frame.body.run_id !== pending.runId || frame.body.run_revision !== pending.expectedRunRevision + 1n || frame.body.request_revision !== pending.expectedRevision + 1n) throw new ProtocolError("malformed");
    this.#humanPending.delete(frame.id);
    pending.resolve(Object.freeze({ ...frame.body }));
  }

  #ensureHumanOperation(requestId: string): void {
    this.#ensureLive();
    if (!this.#authenticated) throw new SessionError("unauthorized");
    if (this.#humanPending.size >= MAX_ARRAY_ITEMS) throw new SessionError("rate_limited");
    for (const pending of this.#humanPending.values()) if (pending.requestId === requestId) throw new SessionError("rate_limited");
  }

  #humanRequest<T>(id: string, pending: Omit<HumanPending, "resolve" | "reject">, payload: string): Promise<T> {
    const result = new Promise<T>((resolve, reject) => this.#humanPending.set(id, { ...pending, resolve: resolve as (value: unknown) => void, reject }));
    try { this.#send(payload); } catch { this.#fail(new SessionError("connection")); }
    return result;
  }

  #closeHumanPending(error: SessionError | ProtocolError): void {
    for (const pending of this.#humanPending.values()) pending.reject(error);
    this.#humanPending.clear();
  }

  #closeTargetPending(error: SessionError | ProtocolError): void {
    for (const pending of this.#targetPending.values()) pending.reject(error);
    this.#targetPending.clear();
  }

  #closeTaskPending(error: SessionError | ProtocolError): void {
    for (const pending of this.#taskPending.values()) pending.reject(error);
    this.#taskPending.clear();
  }

  #closeAgentControlPending(error: SessionError | ProtocolError): void {
    for (const pending of this.#agentControlPending.values()) pending.reject(error);
    this.#agentControlPending.clear();
  }

  #closeTaskHistoryPending(error: SessionError | ProtocolError): void {
    for (const pending of this.#taskHistoryPending.values()) pending.reject(error);
    this.#taskHistoryPending.clear();
  }

  #closeTaskDetailPending(error: SessionError | ProtocolError): void {
    for (const pending of this.#taskDetailPending.values()) pending.reject(error);
    this.#taskDetailPending.clear();
  }

  /** One shape for the console request/result pairs. */
  #consoleRequest<T>(kind: ConsolePending["kind"], entityId: string, expectedRevision: bigint, prefix: string, encode: (id: string) => string): Promise<T> {
    try { this.#ensureLive(); } catch (error) { return Promise.reject(error); }
    if (!this.#authenticated) return Promise.reject(new SessionError("unauthorized"));
    const capability = kind === "TOPOLOGY" || kind === "RUN_PATHS" ? CAPABILITIES.observe : kind === "PROJECT_LIMITS_RESULT" ? CAPABILITIES.administration : CAPABILITIES.human_actions;
    if ((this.#capabilities & capability) === 0) return Promise.reject(new SessionError("unauthorized"));
    if (!validDynamicID(entityId) || expectedRevision < 1n || expectedRevision > MAX_SQLITE_INTEGER) return Promise.reject(new SessionError("invalid_request"));
    if (this.#consolePending.size >= MAX_ARRAY_ITEMS) return Promise.reject(new SessionError("rate_limited"));
    const id = this.#nextID(prefix);
    let payload: string;
    try { payload = encode(id); } catch (error) { return Promise.reject(error); }
    const result = new Promise<T>((resolve, reject) => this.#consolePending.set(id, { kind, entityId, resolve: resolve as (value: never) => void, reject }));
    try { this.#send(payload); } catch { this.#fail(new SessionError("connection")); }
    return result;
  }

  #consoleResult(frame: Extract<ServerControlFrame, { type: "AGENT_UPDATE_RESULT" | "PROJECT_LIMITS_RESULT" | "TASK_UPDATE_RESULT" | "TOPOLOGY" | "RUN_PATHS" | "TASK_LIST" }>): void {
    const pending = this.#consolePending.get(frame.id);
    if (pending === undefined || pending.kind !== frame.type) throw new ProtocolError("malformed");
    const identity = frame.type === "AGENT_UPDATE_RESULT" || frame.type === "RUN_PATHS" || frame.type === "TASK_LIST" ? frame.body.agent_id : frame.type === "TASK_UPDATE_RESULT" ? frame.body.task_id : frame.body.project_id;
    if (identity !== pending.entityId) throw new ProtocolError("malformed");
    this.#consolePending.delete(frame.id);
    if (frame.type === "AGENT_UPDATE_RESULT") { pending.resolve(Object.freeze({ agentId: frame.body.agent_id, revision: frame.body.revision }) as never); return; }
    if (frame.type === "PROJECT_LIMITS_RESULT") { pending.resolve(Object.freeze({ projectId: frame.body.project_id, revision: frame.body.revision }) as never); return; }
    if (frame.type === "TASK_UPDATE_RESULT") { pending.resolve(Object.freeze({ taskId: frame.body.task_id, revision: frame.body.revision }) as never); return; }
    if (frame.type === "RUN_PATHS") { pending.resolve(Object.freeze({ agentId: frame.body.agent_id, runId: frame.body.run_id, paths: Object.freeze([...frame.body.paths]) }) as never); return; }
    if (frame.type === "TASK_LIST") { pending.resolve(Object.freeze({ agentId: frame.body.agent_id, head: frame.body.head, total: frame.body.total, tasks: Object.freeze(frame.body.tasks.map((task) => Object.freeze({ ...task }))), hasMore: frame.body.has_more }) as never); return; }
    pending.resolve(Object.freeze({ projectId: frame.body.project_id, digest: frame.body.digest, sourceRevision: frame.body.source_revision, nodes: Object.freeze(frame.body.nodes.map((node) => Object.freeze({ ...node }))) }) as never);
  }

  #closeConsolePending(error: SessionError | ProtocolError): void {
    for (const pending of this.#consolePending.values()) pending.reject(error);
    this.#consolePending.clear();
  }

  #closeInvitePending(error: SessionError | ProtocolError): void {
    for (const pending of this.#invitePending.values()) pending.reject(error);
    this.#invitePending.clear();
  }

  #closeAccountPending(error: SessionError | ProtocolError): void {
    for (const pending of this.#accountPending.values()) pending.reject(error);
    this.#accountPending.clear();
  }

  /** One shape for account requests; updates also correlate the returned revision. */
  #accountRequest<T>(kind: AccountPending["kind"], capability: number, prefix: string, encode: (id: string) => string, correlation?: Pick<AccountPending, "accountId" | "expectedRevision">): Promise<T> {
    try { this.#ensureLive(); } catch (error) { return Promise.reject(error); }
    if (!this.#authenticated) return Promise.reject(new SessionError("unauthorized"));
    if ((this.#capabilities & capability) === 0) return Promise.reject(new SessionError("unauthorized"));
    if (this.#accountPending.size >= MAX_ARRAY_ITEMS) return Promise.reject(new SessionError("rate_limited"));
    const id = this.#nextID(prefix);
    let payload: string;
    try { payload = encode(id); } catch (error) { return Promise.reject(error); }
    const result = new Promise<T>((resolve, reject) => this.#accountPending.set(id, { kind, ...correlation, resolve: resolve as (value: never) => void, reject }));
    try { this.#send(payload); } catch { this.#fail(new SessionError("connection")); }
    return result;
  }

  #accountResult(frame: Extract<ServerControlFrame, { type: "ACCOUNTS" | "ACCOUNT_LINK_RESULT" | "ACCOUNT_UPDATE_RESULT" }>): void {
    const pending = this.#accountPending.get(frame.id);
    if (pending === undefined || pending.kind !== frame.type) throw new ProtocolError("malformed");
    if (frame.type === "ACCOUNTS") { this.#accountPending.delete(frame.id); pending.resolve(Object.freeze(frame.body.accounts.map((account) => Object.freeze({ ...account }))) as never); return; }
    if (frame.type === "ACCOUNT_UPDATE_RESULT" && (pending.accountId !== frame.body.account_id || pending.expectedRevision === undefined || frame.body.revision !== pending.expectedRevision + 1n)) throw new ProtocolError("malformed");
    this.#accountPending.delete(frame.id);
    pending.resolve(Object.freeze({ accountId: frame.body.account_id, revision: frame.body.revision }) as never);
  }

  #inviteResult(body: RemoteInviteResultBody, id: string): void {
    const pending = this.#invitePending.get(id);
    if (pending === undefined) throw new ProtocolError("malformed");
    this.#invitePending.delete(id);
    pending.resolve(Object.freeze({ link: body.link, expiresAtMs: body.expires_at_ms, svg: body.svg }));
  }

  #taskResult(body: TaskEnqueueResultBody, id: string): void {
    const pending = this.#taskPending.get(id);
    if (pending === undefined || body.task_id !== pending.taskId || body.agent_revision !== pending.expectedAgentRevision || body.revision < 1n) throw new ProtocolError("malformed");
    this.#taskPending.delete(id);
    pending.resolve(Object.freeze({ taskId: body.task_id, revision: body.revision }));
  }

  #agentControlResult(body: AgentControlResultBody, id: string): void {
    const pending = this.#agentControlPending.get(id);
    if (pending === undefined || body.operation_id !== pending.operationId || body.task_id !== pending.taskId || body.run_id !== pending.runId) throw new ProtocolError("malformed");
    this.#agentControlPending.delete(id);
    pending.resolve(Object.freeze({ operationId: body.operation_id, taskId: body.task_id, runId: body.run_id, status: body.status, successorTaskId: body.successor_task_id }));
  }

  #taskHistoryResult(body: TaskHistoryBody, id: string): void {
    const pending = this.#taskHistoryPending.get(id);
    if (pending === undefined || body.task_id !== pending.taskId) throw new ProtocolError("malformed");
    this.#taskHistoryPending.delete(id);
    pending.resolve(Object.freeze({
      taskId: body.task_id,
      entries: Object.freeze(body.entries.map((entry) => Object.freeze({ operationId: entry.operation_id, kind: entry.kind, actor: entry.actor, body: entry.body, status: entry.status, createdAtMs: entry.created_at_ms }))),
    }));
  }

  #taskDetailResult(body: TaskDetailBody, id: string): void {
    const pending = this.#taskDetailPending.get(id);
    if (pending === undefined || body.task_id !== pending.taskId || body.revision !== pending.expectedRevision) throw new ProtocolError("malformed");
    this.#taskDetailPending.delete(id);
    pending.resolve(Object.freeze({ taskId: body.task_id, revision: body.revision, head: body.head, instruction: body.instruction, feedback: body.feedback, ...(body.outcome === undefined ? {} : { outcome: body.outcome }), ...(body.next_text_offset === undefined ? {} : { nextTextOffset: body.next_text_offset }), peerQuestions: Object.freeze(body.peer_questions.map((question) => Object.freeze({ ...question }))), ...(body.next_peer_offset === undefined ? {} : { nextPeerOffset: body.next_peer_offset }) }));
  }

  #mintTarget(descriptor: TerminalTargetDescriptor): TerminalTarget {
    const target = Object.freeze(Object.create(null)) as TerminalTarget;
    TARGET_AUTHORITIES.set(target as object, { owner: this, generation: this.#generationToken, descriptor: Object.freeze({ ...descriptor }) });
    return target;
  }

  #targetAuthority(target: TerminalTarget): TargetAuthority {
    const authority = TARGET_AUTHORITIES.get(target as object);
    if (authority === undefined || authority.owner !== this || authority.generation !== this.#generationToken) throw new ProtocolError("malformed");
    return authority;
  }

  #ensureLive(): void {
    if (this.#closed) throw new SessionError("closed");
  }

  #closedByPeer(): void {
    if (this.#closed) return;
    this.#fail(this.#pairing ? new SessionError("pairing_uncertain") : new SessionError("connection"));
  }
}

/** Reconnect owner. Each connect creates a new BrowserSession and accumulator. */
export class BrowserClient {
  #options: BrowserSessionOptions;
  #session: BrowserSession | undefined;
  #generation = 0;
  #closed = false;
  #reconnect = true;
  #timer: BrowserTimer;
  #timerHandle: unknown;
  #timerScheduled = false;
  #timerToken = 0;
  #reconnectAttempt = 0;
  #initialDelay: number;
  #maxDelay: number;
  #pairRepairUsed = false;
  #connectPromise: Promise<void> | undefined;

  constructor(options: BrowserSessionOptions) {
    this.#options = options;
    this.#timer = options.timer ?? REAL_TIMER;
    this.#initialDelay = reconnectDelay(options.reconnectInitialDelayMs, 100);
    this.#maxDelay = Math.max(this.#initialDelay, reconnectDelay(options.reconnectMaxDelayMs, 5_000));
  }
  get session(): BrowserSession | undefined { return this.#session; }
  get state(): StateView | undefined { return this.#session?.state; }
  get status(): SessionStatus { return this.#session?.status ?? "idle"; }

  connect(): Promise<void> {
    // A one-shot PAIR_PROVE may have an uncertain result while its durable
    // PairResult is being saved. Do not create another generation that would
    // replay that proof for the same challenge.
    if (this.#session?.pairingBlocked) return this.#connectPromise ?? this.#session.connect();
    this.#cancelTimer();
    this.#reconnectAttempt = 0;
    this.#closed = false;
    this.#pairRepairUsed = false;
    this.#reconnect = true;
    const session = this.#newSession();
    this.#connectPromise = session.connect().catch((error) => {
      if (this.#canRepairPairing(error, session)) return this.#repairPairing();
      throw error;
    });
    return this.#connectPromise;
  }

  close(): void {
    this.#closed = true;
    this.#reconnect = false;
    this.#cancelTimer();
    this.#session?.close();
  }

  #newSession(forcePair = false): BrowserSession {
    const generation = ++this.#generation;
    this.#session?.close();
    let reconnectable = true;
    const session = new BrowserSession({
      ...this.#options,
      forcePair,
      onPairingPersisted: () => { this.#options = { ...this.#options, challenge: undefined }; },
      onStatus: (status) => {
        if (generation !== this.#generation) return;
        if ((status === "syncing" || status === "ready") && session.clientId !== undefined && this.#options.challenge !== undefined) this.#options = { ...this.#options, challenge: undefined };
        if (status === "ready") this.#reconnectAttempt = 0;
        if (status === "closed" && reconnectable && this.#reconnect && !this.#closed && (this.#options.challenge === undefined || session.clientId !== undefined)) this.#schedule(generation);
        notify(this.#options.onStatus, status);
      },
      onState: (state) => { if (generation === this.#generation) notify(this.#options.onState, state); },
      onError: (error) => {
        if (generation !== this.#generation) return;
        reconnectable = error instanceof SessionError && (error.code === "connection" || error.retryable);
        notify(this.#options.onError, error);
      },
    });
    this.#session = session;
    return session;
  }

  #schedule(generation: number): void {
    if (this.#timerScheduled || generation !== this.#generation) return;
    const exponent = Math.min(this.#reconnectAttempt, 6);
    const delay = Math.min(this.#maxDelay, this.#initialDelay * 2 ** exponent);
    this.#reconnectAttempt += 1;
    const token = ++this.#timerToken;
    this.#timerScheduled = true;
    try {
      const handle = this.#timer.setTimeout(() => {
        // A cleared timer can still invoke its callback in a host or test
        // implementation. Only its exact schedule may release ownership.
        if (!this.#timerScheduled || this.#timerToken !== token || generation !== this.#generation) return;
        this.#timerScheduled = false;
        this.#timerHandle = undefined;
        if (this.#closed || !this.#reconnect || generation !== this.#generation) return;
        const next = this.#newSession();
        void next.connect().catch(() => { /* status/error callback is the public signal */ });
      }, delay);
      // A synchronous timer callback may already have scheduled a successor.
      if (this.#timerScheduled && this.#timerToken === token) this.#timerHandle = handle;
    } catch {
      if (this.#timerToken === token) {
        this.#timerScheduled = false;
        this.#timerHandle = undefined;
      }
    }
  }

  #cancelTimer(): void {
    const scheduled = this.#timerScheduled;
    const handle = this.#timerHandle;
    this.#timerScheduled = false;
    this.#timerHandle = undefined;
    ++this.#timerToken;
    if (!scheduled) return;
    try { this.#timer.clearTimeout(handle); } catch { /* timer cleanup is best-effort */ }
  }

  #canRepairPairing(error: unknown, session: BrowserSession): error is SessionError {
    return !this.#closed && this.#session === session && !this.#pairRepairUsed && error instanceof SessionError && error.code === "unauthorized" && !error.retryable && session.authAttempted && this.#options.challenge !== undefined;
  }

  #repairPairing(): Promise<void> {
    if (this.#closed) return Promise.reject(new SessionError("closed"));
    this.#pairRepairUsed = true;
    this.#session?.close();
    return this.#newSession(true).connect();
  }
}

function reconnectDelay(value: number | undefined, fallback: number): number {
  return value !== undefined && Number.isFinite(value) && value >= 1 ? Math.min(Math.floor(value), 60_000) : fallback;
}

function bounded(value: string | undefined, maximum: number): boolean { return value !== undefined && new TextEncoder().encode(value).length > maximum; }
function validAgentControlRequest(action: unknown, instruction: unknown, successorTaskId: unknown, successorIncarnationId: unknown): action is AgentControlAction {
  if (typeof instruction !== "string" || typeof successorTaskId !== "string" || typeof successorIncarnationId !== "string") return false;
  if (action === "message") return new TextEncoder().encode(instruction).length >= 1 && new TextEncoder().encode(instruction).length <= MAX_HUMAN_REPLY_BYTES && successorTaskId === "" && successorIncarnationId === "";
  if (action === "interrupt" || action === "stop") return instruction === "" && successorTaskId === "" && successorIncarnationId === "";
  return action === "replace" && new TextEncoder().encode(instruction).length >= 1 && new TextEncoder().encode(instruction).length <= MAX_TASK_INSTRUCTION_BYTES && validDynamicID(successorTaskId) && validDynamicID(successorIncarnationId);
}
function validDynamicID(value: unknown): value is string { return typeof value === "string" && /^[0-9a-f]{32}$/.test(value) && !/^0+$/.test(value); }

/** Mint one canonical nonzero operation identity for an explicit user action. */
export function randomOperationID(source: Crypto | undefined = globalThis.crypto): string {
  if (source === undefined) throw new SessionError("crypto_unavailable");
  const bytes = new Uint8Array(16);
  for (let attempt = 0; attempt < 8; attempt += 1) {
    try { source.getRandomValues(bytes); } catch { throw new SessionError("crypto_unavailable"); }
    if (bytes.some((value) => value !== 0)) return toHex(bytes);
  }
  throw new SessionError("crypto_unavailable");
}

function notify<T extends readonly unknown[]>(callback: ((...args: T) => void) | undefined, ...args: T): void {
  try { callback?.(...args); } catch { /* consumer callbacks cannot break protocol ownership */ }
}

export function createBrowserClient(options: BrowserSessionOptions): BrowserClient { return new BrowserClient(options); }

/** Read-and-clear a one-shot challenge fragment before constructing a session. */
export function consumePairingChallenge(location: Pick<Location, "hash" | "pathname" | "search">, history?: Pick<History, "replaceState" | "state">): string | null {
  const hash = location.hash;
  const exact = /^#df_pair=([0-9a-f]{64})$/.exec(hash);
  let decoded = hash;
  try { decoded = decodeURIComponent(hash); } catch { /* malformed pairing attempts still match their raw key */ }
  const pairingKey = /(?:^|[?#&])(?:df_pair|challenge)=/i;
  if (!pairingKey.test(hash) && !pairingKey.test(decoded)) return null;
  const browserHistory = history ?? globalThis.history;
  const target = `${location.pathname}${location.search}`;
  browserHistory.replaceState(browserHistory.state, "", target);
  const value = exact?.[1];
  return value === undefined || /^0+$/.test(value) ? null : value;
}

/** Native IndexedDB store for the non-exportable profile key. No localStorage fallback exists. */
export class IndexedDBKeyStore implements BrowserKeyStore {
  #database: string;
  #object = "client";
  constructor(database = "dark-factory-browser-v1") { this.#database = database; }

  async load(): Promise<StoredClientKey | null> {
    const record = await this.#request("readonly", (store) => store.get("profile"));
    return record === undefined ? null : record as StoredClientKey;
  }

  async save(value: StoredClientKey): Promise<void> {
    if (value.key.extractable) throw new SessionError("storage_unavailable");
    await this.#request("readwrite", (store) => store.put(value, "profile"));
  }

  #request(mode: IDBTransactionMode, operation: (store: IDBObjectStore) => IDBRequest): Promise<unknown> {
    const indexed = globalThis.indexedDB;
    if (indexed === undefined) return Promise.reject(new SessionError("storage_unavailable"));
    return new Promise((resolve, reject) => {
      const open = indexed.open(this.#database, 1);
      open.onerror = () => reject(new SessionError("storage_unavailable"));
      open.onupgradeneeded = () => { if (!open.result.objectStoreNames.contains(this.#object)) open.result.createObjectStore(this.#object); };
      open.onsuccess = () => {
        const database = open.result;
        try {
          const request = operation(database.transaction(this.#object, mode).objectStore(this.#object));
          request.onerror = () => reject(new SessionError("storage_unavailable"));
          request.onsuccess = () => resolve(request.result);
        } catch { reject(new SessionError("storage_unavailable")); }
        database.close();
      };
    });
  }
}

export function createIndexedDBKeyStore(database?: string): BrowserKeyStore { return new IndexedDBKeyStore(database); }

function toHex(value: Uint8Array | undefined): string {
  if (value === undefined) throw new SessionError("crypto_unavailable");
  return [...value].map((byte) => byte.toString(16).padStart(2, "0")).join("");
}
