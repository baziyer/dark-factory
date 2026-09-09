import { malformed, normalizeBoundary, ProtocolError } from "./errors.js";
import {
  CAPABILITIES,
  CONTROL_TYPES,
  ERROR_CODES,
  MAX_AGENT_MODEL_BYTES,
  MAX_IDLE_AFTER_SECONDS,
  MAX_IDLE_RUN_BUDGET,
  MAX_AGENT_NAME_BYTES,
  MAX_MODEL_SOURCE_BYTES,
  MAX_ARRAY_ITEMS,
  MAX_CONTROL_BYTES,
  MAX_FACTORY_CAPACITY,
  MAX_HUMAN_QUESTION_BYTES,
  MAX_HUMAN_REPLY_BYTES,
  MAX_TASK_INSTRUCTION_BYTES,
  MAX_JSON_DEPTH,
  MAX_OBJECT_MEMBERS,
  MAX_PROJECT_NAME_BYTES,
  MAX_REMOTE_INVITE_LINK_BYTES,
  MAX_REMOTE_INVITE_SVG_BYTES,
  MAX_SNAPSHOT_BYTES,
  MAX_SNAPSHOT_ENTITIES,
  MAX_SQLITE_INTEGER,
  MAX_TERMINAL_COLS,
  MAX_TERMINAL_PAYLOAD,
  MAX_TERMINAL_ROWS,
  MAX_TERMINAL_UNACKED_BYTES,
  MAX_TASK_PRIORITY,
  MAX_TASK_TITLE_BYTES,
  type CapabilityMask,
  type ControlType,
  type ErrorCode,
} from "./manifest.js";

export type HelloBody = { daemon_id: string; boot_id: string; connection_nonce: string };
export type PairProveBody = { challenge: string; public_key_sec1: string; signature: string };
export type PairResultBody = { client_id: string; capabilities: CapabilityMask };
export type AuthProveBody = { client_id: string; signature: string };
export type AuthResultBody = { client_id: string; capabilities: CapabilityMask };
export type ErrorBody = { code: ErrorCode; retryable: boolean };

export type FactoryItem = { dispatch_enabled: boolean; capacity: number; active_runs: number; revision: bigint };
export type ProjectItem = { id: string; name: string; revision: bigint };
/**
 * `model` and `reasoning_effort` are the agent's own overrides; empty means it
 * inherits. `effective_*` is what the run will actually use, and `model_source`
 * says where the model came from: "agent" when the agent names one, the
 * provider CLI configuration path the default was read from, or "" when that
 * CLI keeps a default nobody can see. `account_id` is the linked provider
 * login it launches under; empty means that provider's default directory.
 */
export type IdlePolicy = "wait" | "standing_instruction";
export type AgentItem = { id: string; project_id: string; name: string; role: "orchestrator" | "worker"; provider: "claude_code" | "codex" | "shell"; paused: boolean; model: string; reasoning_effort: string; effective_model: string; effective_reasoning_effort: string; model_source: string; revision: bigint; account_id: string; idle_policy: IdlePolicy; idle_after_seconds: number; idle_instruction: string; idle_run_budget: number; idle_runs_used: number };
export type AccountItem = { id: string; provider: "claude_code" | "codex"; home: string; label: string; revision: bigint };
export type TaskItem = { id: string; project_id: string; assigned_agent_id: string; title: string; status: "queued" | "running" | "blocked" | "succeeded" | "failed" | "cancelled"; priority: number; revision: bigint };
export type HumanRequestItem = {
  id: string; project_id: string; agent_id: string; task_id: string;
  created_at: bigint; updated_at: bigint; revision: bigint; kind: "question";
  status: "open" | "delivering" | "delivery_unknown"; reply_max_bytes: number;
  can_reply: boolean;
};

/** STATE_GET has no cursor, continuation or selector: there is one snapshot. */
export type StateGetBody = Record<string, never>;
export type StateSnapshotBody = {
  head: bigint;
  factory: FactoryItem;
  projects: ProjectItem[];
  agents: AgentItem[];
  tasks: TaskItem[];
  human_requests: HumanRequestItem[];
  accounts: AccountItem[];
};
export type StateWatchBody = { after_head: bigint };
export type StateChangedBody = { head: bigint };
export type HumanRequestDetailGetBody = { request_id: string; expected_revision: bigint };
type HumanRequestCancelRunDetail = { expected_request_revision: bigint; expected_run_revision: bigint };
export type HumanRequestDetailBody = { request_id: string; revision: bigint; question: string; can_reply: boolean; reply_max_bytes: number; terminal_target: TerminalTargetDescriptor | null; cancel_run: HumanRequestCancelRunDetail | null };
export type HumanRequestReplyBody = { request_id: string; expected_revision: bigint; reply: string };
export type HumanRequestReplyResultBody = { request_id: string; revision: bigint; status: "resolved" | "delivery_unknown" };
export type HumanRequestCancelRunBody = { request_id: string; expected_request_revision: bigint; expected_run_revision: bigint };
export type HumanRequestCancelRunResultBody = { run_id: string; run_revision: bigint; request_id: string; request_revision: bigint };
export type TaskEnqueueBody = { task_id: string; incarnation_id: string; agent_id: string; expected_agent_revision: bigint; instruction: string; mode?: "now" | "queue" };
export type TaskEnqueueResultBody = { task_id: string; revision: bigint; agent_revision: bigint };
export type AgentControlAction = "message" | "interrupt" | "stop" | "replace";
export type AgentControlBody = { operation_id: string; task_id: string; run_id: string; expected_task_revision: bigint; expected_run_revision: bigint; action: AgentControlAction; instruction: string; successor_task_id: string; successor_incarnation_id: string };
export type AgentControlResultBody = { operation_id: string; task_id: string; run_id: string; status: "delivered" | "delivery_unknown" | "rejected" | "stopping" | "queued"; successor_task_id: string };
export type TaskHistoryGetBody = { task_id: string };
export type TaskHistoryEntry = { operation_id: string; kind: AgentControlAction; actor: string; body: string; status: "pending" | "delivered" | "unknown" | "rejected"; created_at_ms: bigint };
export type TaskHistoryBody = { task_id: string; entries: TaskHistoryEntry[] };
export type TaskDetailGetBody = { task_id: string; expected_revision: bigint; text_offset?: bigint; peer_offset?: bigint; expected_head?: bigint };
export type TaskPeerQuestion = { id: string; source_task_id: string; target_task_id: string; question: string; answer?: string; recipient_delivery_state: string; answer_delivery_state: string; revision: bigint; created_at_ms: bigint; updated_at_ms: bigint };
export type TaskDetailBody = { task_id: string; revision: bigint; head: bigint; instruction: string; feedback: string; next_text_offset?: bigint; peer_questions: TaskPeerQuestion[]; next_peer_offset?: bigint };
export type AgentUpdateBody = { agent_id: string; expected_revision: bigint; model?: string; reasoning_effort?: string; account_id?: string; paused?: boolean; idle_policy?: IdlePolicy; idle_after_seconds?: number; idle_instruction?: string; idle_run_budget?: number };
export type AgentUpdateResultBody = { agent_id: string; revision: bigint };
export type TaskUpdateBody = { task_id: string; expected_revision: bigint; title?: string; body?: string; priority?: number; assigned_agent_id?: string; status?: "cancelled" };
export type TaskUpdateResultBody = { task_id: string; revision: bigint };
export type TopologyGetBody = { project_id: string };
export type TopologyNode = { id: string; parent_id: string; kind: "repository" | "module" | "package" | "directory"; path: string; label: string; language: string; size_bucket: "empty" | "tiny" | "small" | "medium" | "large" };
export type TopologyBody = { project_id: string; digest: string; source_revision: string; nodes: TopologyNode[] };
export type RunPathsGetBody = { agent_id: string };
export type RunPathsBody = { agent_id: string; run_id: string; paths: string[] };
export type AccountsDiscoverBody = Record<string, never>;
export type DiscoveredAccount = { provider: "claude_code" | "codex"; home: string; label: string; email: string; organization: string; default_model: string; default_reasoning_effort: string; linked_id: string };
export type AccountsBody = { accounts: DiscoveredAccount[] };
export type AccountLinkBody = { provider: "claude_code" | "codex"; home: string; label: string };
export type AccountLinkResultBody = { account_id: string; revision: bigint };
export type TerminalTargetGetBody = { agent_id: string; expected_agent_revision: bigint; expected_head: bigint };
export type TerminalTargetDescriptor = { run_id: string; session_id: string; run_revision: bigint; session_revision: bigint };
export type TerminalTargetBody = { agent_id: string; agent_revision: bigint; head: bigint; target: TerminalTargetDescriptor | null };
export type TerminalAttachBody = { run_id: string; session_id: string; expected_run_revision: bigint; expected_session_revision: bigint; after_sequence: bigint };
export type TerminalAttachedBody = { session_id: string; floor: bigint; head: bigint; acknowledged_sequence: bigint; max_unacked_bytes: bigint };
export type TerminalAckBody = { session_id: string; next_sequence: bigint };
export type TerminalLeaseAcquireBody = { run_id: string; session_id: string; expected_run_revision: bigint; expected_session_revision: bigint };
export type TerminalLeaseRenewBody = { run_id: string; session_id: string; generation: bigint; expected_run_revision: bigint; expected_session_revision: bigint };
export type TerminalLeaseReleaseBody = { run_id: string; session_id: string; generation: bigint; expected_run_revision: bigint; expected_session_revision: bigint };
export type TerminalLeaseResultBody =
  | { operation: "acquired" | "renewed"; run_id: string; session_id: string; generation: bigint; expires_at_ms: bigint; last_input_sequence: bigint; run_revision: bigint; session_revision: bigint }
  | { operation: "released"; run_id: string; session_id: string; generation: bigint; last_input_sequence: bigint; run_revision: bigint; session_revision: bigint };
export type TerminalResizeBody = { run_id: string; session_id: string; generation: bigint; expected_run_revision: bigint; expected_session_revision: bigint; rows: number; cols: number };
export type TerminalResizedBody = { session_id: string; generation: bigint; rows: number; cols: number };
export type TerminalDetachBody = { session_id: string };
export type TerminalDetachedBody = { session_id: string };
export type TerminalInputResultBody = { session_id: string; generation: bigint; sequence: bigint; status: "accepted" | "rejected" | "partial" | "uncertain"; accepted_bytes: bigint };
export type TerminalEOFBody = { session_id: string };
export type TerminalExitBody = { session_id: string; exit_code: number; exit_signal: number; aborted: boolean };
export type TerminalResetBody = { session_id: string; floor: bigint; head: bigint };
export type RemoteInviteBody = Record<string, never>;
export type RemoteInviteResultBody = { link: string; expires_at_ms: bigint; svg: string };

/** The exact everything-before-the-members of a minted invitation link. */
const REMOTE_INVITE_LINK_PREFIX = "https://app.darkfactory.build/remote#df_remote&";

export type HelloFrame = { type: "HELLO"; body: HelloBody };
export type PairProveFrame = { type: "PAIR_PROVE"; id: string; body: PairProveBody };
export type PairResultFrame = { type: "PAIR_RESULT"; id: string; body: PairResultBody };
export type AuthProveFrame = { type: "AUTH_PROVE"; id: string; body: AuthProveBody };
export type AuthResultFrame = { type: "AUTH_RESULT"; id: string; body: AuthResultBody };
export type StateGetFrame = { type: "STATE_GET"; id: string; body: StateGetBody };
export type StateSnapshotFrame = { type: "STATE_SNAPSHOT"; id: string; body: StateSnapshotBody };
export type StateWatchFrame = { type: "STATE_WATCH"; id: string; body: StateWatchBody };
export type StateChangedFrame = { type: "STATE_CHANGED"; id: string; body: StateChangedBody };
export type HumanRequestDetailGetFrame = { type: "HUMAN_REQUEST_DETAIL_GET"; id: string; body: HumanRequestDetailGetBody };
export type HumanRequestDetailFrame = { type: "HUMAN_REQUEST_DETAIL"; id: string; body: HumanRequestDetailBody };
export type ErrorFrame = { type: "ERROR"; id?: string; body: ErrorBody };
export type TerminalControlFrame =
  | { type: "TERMINAL_ATTACH"; id: string; body: TerminalAttachBody }
  | { type: "TERMINAL_ACK"; body: TerminalAckBody }
  | { type: "TERMINAL_LEASE_ACQUIRE"; id: string; body: TerminalLeaseAcquireBody }
  | { type: "TERMINAL_LEASE_RENEW"; id: string; body: TerminalLeaseRenewBody }
  | { type: "TERMINAL_LEASE_RELEASE"; id: string; body: TerminalLeaseReleaseBody }
  | { type: "TERMINAL_RESIZE"; id: string; body: TerminalResizeBody }
  | { type: "TERMINAL_DETACH"; id: string; body: TerminalDetachBody };
export type TerminalServerControlFrame =
  | { type: "TERMINAL_ATTACHED"; id: string; body: TerminalAttachedBody }
  | { type: "TERMINAL_LEASE_RESULT"; id: string; body: TerminalLeaseResultBody }
  | { type: "TERMINAL_RESIZED"; id: string; body: TerminalResizedBody }
  | { type: "TERMINAL_DETACHED"; id: string; body: TerminalDetachedBody }
  | { type: "TERMINAL_INPUT_RESULT"; id: string; body: TerminalInputResultBody }
  | { type: "TERMINAL_EOF"; id: string; body: TerminalEOFBody }
  | { type: "TERMINAL_EXIT"; id: string; body: TerminalExitBody }
  | { type: "TERMINAL_RESET"; id: string; body: TerminalResetBody };

export type ServerControlFrame = HelloFrame | PairResultFrame | AuthResultFrame | StateSnapshotFrame | StateChangedFrame | HumanRequestDetailFrame
  | { type: "HUMAN_REQUEST_REPLY_RESULT"; id: string; body: HumanRequestReplyResultBody }
  | { type: "HUMAN_REQUEST_CANCEL_RUN_RESULT"; id: string; body: HumanRequestCancelRunResultBody }
  | { type: "TASK_ENQUEUE_RESULT"; id: string; body: TaskEnqueueResultBody }
  | { type: "AGENT_CONTROL_RESULT"; id: string; body: AgentControlResultBody }
  | { type: "TASK_HISTORY"; id: string; body: TaskHistoryBody }
  | { type: "TASK_DETAIL"; id: string; body: TaskDetailBody }
  | { type: "AGENT_UPDATE_RESULT"; id: string; body: AgentUpdateResultBody }
  | { type: "TASK_UPDATE_RESULT"; id: string; body: TaskUpdateResultBody }
  | { type: "TOPOLOGY"; id: string; body: TopologyBody }
  | { type: "RUN_PATHS"; id: string; body: RunPathsBody }
  | { type: "ACCOUNTS"; id: string; body: AccountsBody }
  | { type: "ACCOUNT_LINK_RESULT"; id: string; body: AccountLinkResultBody }
  | { type: "TERMINAL_TARGET"; id: string; body: TerminalTargetBody }
  | { type: "REMOTE_INVITE_RESULT"; id: string; body: RemoteInviteResultBody }
  | TerminalServerControlFrame | ErrorFrame;
export type ClientControlFrame = PairProveFrame | AuthProveFrame | StateGetFrame | StateWatchFrame | HumanRequestDetailGetFrame
  | { type: "HUMAN_REQUEST_REPLY"; id: string; body: HumanRequestReplyBody }
  | { type: "HUMAN_REQUEST_CANCEL_RUN"; id: string; body: HumanRequestCancelRunBody }
  | { type: "TASK_ENQUEUE"; id: string; body: TaskEnqueueBody }
  | { type: "AGENT_CONTROL"; id: string; body: AgentControlBody }
  | { type: "TASK_HISTORY_GET"; id: string; body: TaskHistoryGetBody }
  | { type: "TASK_DETAIL_GET"; id: string; body: TaskDetailGetBody }
  | { type: "AGENT_UPDATE"; id: string; body: AgentUpdateBody }
  | { type: "TASK_UPDATE"; id: string; body: TaskUpdateBody }
  | { type: "TOPOLOGY_GET"; id: string; body: TopologyGetBody }
  | { type: "RUN_PATHS_GET"; id: string; body: RunPathsGetBody }
  | { type: "ACCOUNTS_DISCOVER"; id: string; body: AccountsDiscoverBody }
  | { type: "ACCOUNT_LINK"; id: string; body: AccountLinkBody }
  | { type: "TERMINAL_TARGET_GET"; id: string; body: TerminalTargetGetBody }
  | { type: "REMOTE_INVITE"; id: string; body: RemoteInviteBody }
  | TerminalControlFrame | ErrorFrame;
type ControlBody = ClientControlFrame["body"] | ServerControlFrame["body"];

const HEX_BYTES = { daemon_id: 16, boot_id: 16, connection_nonce: 32, challenge: 32, client_id: 16, public_key_sec1: 65, signature: 64 } as const;
const CLIENT_TYPES: readonly ControlType[] = ["PAIR_PROVE", "AUTH_PROVE", "STATE_GET", "STATE_WATCH", "HUMAN_REQUEST_DETAIL_GET", "HUMAN_REQUEST_REPLY", "HUMAN_REQUEST_CANCEL_RUN", "TASK_ENQUEUE", "AGENT_CONTROL", "TASK_HISTORY_GET", "TASK_DETAIL_GET", "AGENT_UPDATE", "TASK_UPDATE", "TOPOLOGY_GET", "RUN_PATHS_GET", "ACCOUNTS_DISCOVER", "ACCOUNT_LINK", "TERMINAL_TARGET_GET", "TERMINAL_ATTACH", "TERMINAL_ACK", "TERMINAL_LEASE_ACQUIRE", "TERMINAL_LEASE_RENEW", "TERMINAL_LEASE_RELEASE", "TERMINAL_RESIZE", "TERMINAL_DETACH", "REMOTE_INVITE", "ERROR"];
const SERVER_TYPES: readonly ControlType[] = ["HELLO", "PAIR_RESULT", "AUTH_RESULT", "STATE_SNAPSHOT", "STATE_CHANGED", "HUMAN_REQUEST_DETAIL", "HUMAN_REQUEST_REPLY_RESULT", "HUMAN_REQUEST_CANCEL_RUN_RESULT", "TASK_ENQUEUE_RESULT", "AGENT_CONTROL_RESULT", "TASK_HISTORY", "TASK_DETAIL", "AGENT_UPDATE_RESULT", "TASK_UPDATE_RESULT", "TOPOLOGY", "RUN_PATHS", "ACCOUNTS", "ACCOUNT_LINK_RESULT", "TERMINAL_TARGET", "TERMINAL_ATTACHED", "TERMINAL_LEASE_RESULT", "TERMINAL_RESIZED", "TERMINAL_DETACHED", "TERMINAL_INPUT_RESULT", "TERMINAL_EOF", "TERMINAL_EXIT", "TERMINAL_RESET", "REMOTE_INVITE_RESULT", "ERROR"];

export function encodeClientControl(frame: ClientControlFrame): string { return normalizeBoundary(() => encode(frame, validateControl(frame, "client"))); }
export function encodePairProve(id: string, body: PairProveBody): string { return encodeClientControl({ type: "PAIR_PROVE", id, body }); }
export function encodeAuthProve(id: string, body: AuthProveBody): string { return encodeClientControl({ type: "AUTH_PROVE", id, body }); }
export function encodeStateGet(id: string, body: StateGetBody): string { return encodeClientControl({ type: "STATE_GET", id, body }); }
export function encodeStateWatch(id: string, body: StateWatchBody): string { return encodeClientControl({ type: "STATE_WATCH", id, body }); }
export function encodeHumanRequestDetailGet(id: string, body: HumanRequestDetailGetBody): string { return encodeClientControl({ type: "HUMAN_REQUEST_DETAIL_GET", id, body }); }
export function encodeHumanRequestReply(id: string, body: HumanRequestReplyBody): string { return encodeClientControl({ type: "HUMAN_REQUEST_REPLY", id, body }); }
export function encodeHumanRequestCancelRun(id: string, body: HumanRequestCancelRunBody): string { return encodeClientControl({ type: "HUMAN_REQUEST_CANCEL_RUN", id, body }); }
export function encodeTaskEnqueue(id: string, body: TaskEnqueueBody): string { return encodeClientControl({ type: "TASK_ENQUEUE", id, body }); }
export function encodeAgentControl(id: string, body: AgentControlBody): string { return encodeClientControl({ type: "AGENT_CONTROL", id, body }); }
export function encodeTaskHistoryGet(id: string, body: TaskHistoryGetBody): string { return encodeClientControl({ type: "TASK_HISTORY_GET", id, body }); }
export function encodeTaskDetailGet(id: string, body: TaskDetailGetBody): string { return encodeClientControl({ type: "TASK_DETAIL_GET", id, body }); }
export function encodeTerminalTargetGet(id: string, body: TerminalTargetGetBody): string { return encodeClientControl({ type: "TERMINAL_TARGET_GET", id, body }); }
export function encodeTerminalAttach(id: string, body: TerminalAttachBody): string { return encodeClientControl({ type: "TERMINAL_ATTACH", id, body }); }
export function encodeTerminalAck(body: TerminalAckBody): string { return encodeClientControl({ type: "TERMINAL_ACK", body }); }
export function encodeTerminalLeaseAcquire(id: string, body: TerminalLeaseAcquireBody): string { return encodeClientControl({ type: "TERMINAL_LEASE_ACQUIRE", id, body }); }
export function encodeTerminalLeaseRenew(id: string, body: TerminalLeaseRenewBody): string { return encodeClientControl({ type: "TERMINAL_LEASE_RENEW", id, body }); }
export function encodeTerminalLeaseRelease(id: string, body: TerminalLeaseReleaseBody): string { return encodeClientControl({ type: "TERMINAL_LEASE_RELEASE", id, body }); }
export function encodeTerminalResize(id: string, body: TerminalResizeBody): string { return encodeClientControl({ type: "TERMINAL_RESIZE", id, body }); }
export function encodeTerminalDetach(id: string, body: TerminalDetachBody): string { return encodeClientControl({ type: "TERMINAL_DETACH", id, body }); }
export function encodeRemoteInvite(id: string, body: RemoteInviteBody): string { return encodeClientControl({ type: "REMOTE_INVITE", id, body }); }
export function encodeClientError(body: ErrorBody, id?: string): string { return encodeClientControl({ type: "ERROR", ...(id === undefined ? {} : { id }), body }); }

export function encodeServerControl(frame: ServerControlFrame): string { return normalizeBoundary(() => encode(frame, validateControl(frame, "server"))); }
export function encodeHello(body: HelloBody): string { return encodeServerControl({ type: "HELLO", body }); }
export function encodePairResult(id: string, body: PairResultBody): string { return encodeServerControl({ type: "PAIR_RESULT", id, body }); }
export function encodeAuthResult(id: string, body: AuthResultBody): string { return encodeServerControl({ type: "AUTH_RESULT", id, body }); }
export function encodeStateSnapshot(id: string, body: StateSnapshotBody): string { return encodeServerControl({ type: "STATE_SNAPSHOT", id, body }); }
export function encodeStateChanged(id: string, body: StateChangedBody): string { return encodeServerControl({ type: "STATE_CHANGED", id, body }); }
export function encodeHumanRequestDetail(id: string, body: HumanRequestDetailBody): string { return encodeServerControl({ type: "HUMAN_REQUEST_DETAIL", id, body }); }
export function encodeHumanRequestReplyResult(id: string, body: HumanRequestReplyResultBody): string { return encodeServerControl({ type: "HUMAN_REQUEST_REPLY_RESULT", id, body }); }
export function encodeHumanRequestCancelRunResult(id: string, body: HumanRequestCancelRunResultBody): string { return encodeServerControl({ type: "HUMAN_REQUEST_CANCEL_RUN_RESULT", id, body }); }
export function encodeTaskEnqueueResult(id: string, body: TaskEnqueueResultBody): string { return encodeServerControl({ type: "TASK_ENQUEUE_RESULT", id, body }); }
export function encodeAgentControlResult(id: string, body: AgentControlResultBody): string { return encodeServerControl({ type: "AGENT_CONTROL_RESULT", id, body }); }
export function encodeTaskHistory(id: string, body: TaskHistoryBody): string { return encodeServerControl({ type: "TASK_HISTORY", id, body }); }
export function encodeTaskDetail(id: string, body: TaskDetailBody): string { return encodeServerControl({ type: "TASK_DETAIL", id, body }); }
export function encodeTerminalTarget(id: string, body: TerminalTargetBody): string { return encodeServerControl({ type: "TERMINAL_TARGET", id, body }); }
export function encodeTerminalAttached(id: string, body: TerminalAttachedBody): string { return encodeServerControl({ type: "TERMINAL_ATTACHED", id, body }); }
export function encodeTerminalLeaseResult(id: string, body: TerminalLeaseResultBody): string { return encodeServerControl({ type: "TERMINAL_LEASE_RESULT", id, body }); }
export function encodeTerminalResized(id: string, body: TerminalResizedBody): string { return encodeServerControl({ type: "TERMINAL_RESIZED", id, body }); }
export function encodeTerminalDetached(id: string, body: TerminalDetachedBody): string { return encodeServerControl({ type: "TERMINAL_DETACHED", id, body }); }
export function encodeTerminalInputResult(id: string, body: TerminalInputResultBody): string { return encodeServerControl({ type: "TERMINAL_INPUT_RESULT", id, body }); }
export function encodeTerminalEOF(id: string, body: TerminalEOFBody): string { return encodeServerControl({ type: "TERMINAL_EOF", id, body }); }
export function encodeTerminalExit(id: string, body: TerminalExitBody): string { return encodeServerControl({ type: "TERMINAL_EXIT", id, body }); }
export function encodeTerminalReset(id: string, body: TerminalResetBody): string { return encodeServerControl({ type: "TERMINAL_RESET", id, body }); }
export function encodeRemoteInviteResult(id: string, body: RemoteInviteResultBody): string { return encodeServerControl({ type: "REMOTE_INVITE_RESULT", id, body }); }
export function encodeServerError(body: ErrorBody, id?: string): string { return encodeServerControl({ type: "ERROR", ...(id === undefined ? {} : { id }), body }); }
export function decodeClientControl(data: string | Uint8Array): ClientControlFrame { return normalizeBoundary(() => decodeControl(data, "client") as ClientControlFrame); }
export function decodeServerControl(data: string | Uint8Array): ServerControlFrame { return normalizeBoundary(() => decodeControl(data, "server") as ServerControlFrame); }

function encode(frame: ClientControlFrame | ServerControlFrame, body: ControlBody): string {
  const envelope: Record<string, unknown> = { type: frame.type };
  if (Object.prototype.hasOwnProperty.call(frame, "id") && "id" in frame) envelope.id = frame.id;
  envelope.body = wireValue(body);
  const result = JSON.stringify(envelope);
  if (new TextEncoder().encode(result).length > controlLimit(frame.type)) throw new ProtocolError("oversized");
  return result;
}
function wireValue(value: unknown): unknown {
  if (typeof value === "bigint") return value.toString(10);
  if (Array.isArray(value)) return value.map(wireValue);
  if (isObject(value)) { const result: Record<string, unknown> = {}; for (const [key, item] of Object.entries(value)) result[key] = wireValue(item); return result; }
  return value;
}

/** Only the server's whole-state snapshot and project topology may exceed the 64 KiB control bound. */
function controlLimit(type: ControlType): number { return type === "STATE_SNAPSHOT" || type === "TOPOLOGY" ? MAX_SNAPSHOT_BYTES : MAX_CONTROL_BYTES; }

function decodeControl(data: string | Uint8Array, role: "client" | "server"): ClientControlFrame | ServerControlFrame {
  let text: string;
  try { text = typeof data === "string" ? data : new TextDecoder("utf-8", { fatal: true }).decode(data); } catch { malformed(); }
  // A browser only ever sends 64 KiB control frames; the larger bound and the
  // larger array bound belong to the server direction alone.
  const entryLimit = role === "server" ? MAX_SNAPSHOT_BYTES : MAX_CONTROL_BYTES;
  const arrayLimit = role === "server" ? MAX_SNAPSHOT_ENTITIES : MAX_ARRAY_ITEMS;
  const encodedLength = new TextEncoder().encode(text).length;
  if (text.length === 0 || encodedLength > entryLimit) malformed();
  rejectDuplicateKeys(text, arrayLimit);
  let value: unknown;
  try { value = JSON.parse(text) as unknown; } catch { malformed(); }
  if (!isObject(value)) malformed();
  requireKeys(value, ["type", "body"], true, ["id"]);
  if (!isControlType(value.type)) malformed();
  if (encodedLength > controlLimit(value.type)) malformed();
  const hasID = Object.prototype.hasOwnProperty.call(value, "id");
  validateControlID(value.type, hasID, value.id);
  if (role === "client" && !CLIENT_TYPES.includes(value.type)) throw new ProtocolError("wrong_direction");
  if (role === "server" && !SERVER_TYPES.includes(value.type)) throw new ProtocolError("wrong_direction");
  const body = validateBody(value.type, value.body, true);
  return { type: value.type, ...(hasID ? { id: value.id as string } : {}), body } as ClientControlFrame | ServerControlFrame;
}
function validateControl(frame: ClientControlFrame | ServerControlFrame, role: "client" | "server"): ControlBody {
  if (!isObject(frame) || !isControlType(frame.type)) malformed();
  if (role === "client" && !CLIENT_TYPES.includes(frame.type) || role === "server" && !SERVER_TYPES.includes(frame.type)) throw new ProtocolError("wrong_direction");
  const hasID = Object.prototype.hasOwnProperty.call(frame, "id");
  validateControlID(frame.type, hasID, "id" in frame ? frame.id : undefined);
  return validateBody(frame.type, frame.body, false);
}

function validateControlID(type: ControlType, hasID: boolean, id: unknown): void {
  const policy: "required" | "optional" | "forbidden" = type === "ERROR" ? "optional" : type === "HELLO" || type === "TERMINAL_ACK" ? "forbidden" : "required";
  if (hasID && (typeof id !== "string" || !validID(id)) || policy === "forbidden" && hasID || policy === "required" && !hasID) malformed();
}

function validateBody(type: ControlType, body: unknown, wire: boolean): ControlBody {
  if (!isObject(body)) malformed();
  switch (type) {
    case "HELLO": requireKeys(body, ["daemon_id", "boot_id", "connection_nonce"], wire); return { daemon_id: fixedHex(body.daemon_id, HEX_BYTES.daemon_id), boot_id: fixedHex(body.boot_id, HEX_BYTES.boot_id), connection_nonce: fixedHex(body.connection_nonce, HEX_BYTES.connection_nonce) };
    case "PAIR_PROVE": requireKeys(body, ["challenge", "public_key_sec1", "signature"], wire); return { challenge: fixedHex(body.challenge, HEX_BYTES.challenge), public_key_sec1: fixedHex(body.public_key_sec1, HEX_BYTES.public_key_sec1, true), signature: fixedHex(body.signature, HEX_BYTES.signature) };
    case "AUTH_PROVE": requireKeys(body, ["client_id", "signature"], wire); return { client_id: fixedHex(body.client_id, HEX_BYTES.client_id), signature: fixedHex(body.signature, HEX_BYTES.signature) };
    case "PAIR_RESULT": case "AUTH_RESULT": requireKeys(body, ["client_id", "capabilities"], wire); return { client_id: fixedHex(body.client_id, HEX_BYTES.client_id), capabilities: capabilities(body.capabilities) };
    case "STATE_GET": requireKeys(body, [], wire); return {};
    case "STATE_SNAPSHOT": return stateSnapshot(body, wire);
    case "STATE_WATCH": requireKeys(body, ["after_head"], wire); return { after_head: decimal(body.after_head, wire) };
    case "STATE_CHANGED": requireKeys(body, ["head"], wire); return { head: decimal(body.head, wire, true) };
    case "HUMAN_REQUEST_DETAIL_GET": requireKeys(body, ["request_id", "expected_revision"], wire); return { request_id: dynamicID(body.request_id), expected_revision: decimal(body.expected_revision, wire, true) };
    case "HUMAN_REQUEST_DETAIL": {
      requireKeys(body, ["request_id", "revision", "question", "can_reply", "reply_max_bytes", "terminal_target", "cancel_run"], wire);
      const request_id = dynamicID(body.request_id); const revision = decimal(body.revision, wire, true);
      if (typeof body.can_reply !== "boolean") malformed();
      const can_reply = body.can_reply; const reply_max_bytes = integer(body.reply_max_bytes, MAX_HUMAN_REPLY_BYTES, MAX_HUMAN_REPLY_BYTES);
      const terminal_target = body.terminal_target === null ? null : terminalTargetDescriptor(body.terminal_target, wire);
      let cancel_run: HumanRequestCancelRunDetail | null = null;
      if (body.cancel_run !== null) {
        if (!isObject(body.cancel_run)) malformed();
        requireKeys(body.cancel_run, ["expected_request_revision", "expected_run_revision"], wire);
        cancel_run = { expected_request_revision: decimal(body.cancel_run.expected_request_revision, wire, true), expected_run_revision: decimal(body.cancel_run.expected_run_revision, wire, true) };
      }
      if (cancel_run !== null && (terminal_target === null || !can_reply || cancel_run.expected_request_revision !== revision || cancel_run.expected_run_revision !== terminal_target.run_revision)) malformed();
      if (can_reply && (terminal_target === null || cancel_run === null)) malformed();
      return { request_id, revision, question: boundedText(body.question, 1, MAX_HUMAN_QUESTION_BYTES), can_reply, reply_max_bytes, terminal_target, cancel_run };
    }
    case "HUMAN_REQUEST_REPLY": requireKeys(body, ["request_id", "expected_revision", "reply"], wire); return { request_id: dynamicID(body.request_id), expected_revision: decimal(body.expected_revision, wire, true), reply: boundedText(body.reply, 1, MAX_HUMAN_REPLY_BYTES) };
    case "HUMAN_REQUEST_REPLY_RESULT": requireKeys(body, ["request_id", "revision", "status"], wire); if (body.status !== "resolved" && body.status !== "delivery_unknown") malformed(); return { request_id: dynamicID(body.request_id), revision: decimal(body.revision, wire, true), status: body.status };
    case "HUMAN_REQUEST_CANCEL_RUN": requireKeys(body, ["request_id", "expected_request_revision", "expected_run_revision"], wire); return { request_id: dynamicID(body.request_id), expected_request_revision: decimal(body.expected_request_revision, wire, true), expected_run_revision: decimal(body.expected_run_revision, wire, true) };
    case "HUMAN_REQUEST_CANCEL_RUN_RESULT": requireKeys(body, ["run_id", "run_revision", "request_id", "request_revision"], wire); return { run_id: dynamicID(body.run_id), run_revision: decimal(body.run_revision, wire, true), request_id: dynamicID(body.request_id), request_revision: decimal(body.request_revision, wire, true) };
    case "TASK_ENQUEUE": {
      requireKeys(body, ["task_id", "incarnation_id", "agent_id", "expected_agent_revision", "instruction"], wire, ["mode"]);
      const mode = present(body, "mode") ? body.mode : undefined;
      if (mode !== undefined && mode !== "now" && mode !== "queue") malformed();
      return { task_id: dynamicID(body.task_id), incarnation_id: dynamicID(body.incarnation_id), agent_id: dynamicID(body.agent_id), expected_agent_revision: decimal(body.expected_agent_revision, wire, true), instruction: boundedText(body.instruction, 1, MAX_TASK_INSTRUCTION_BYTES), ...(mode === undefined ? {} : { mode }) };
    }
    case "TASK_ENQUEUE_RESULT": requireKeys(body, ["task_id", "revision", "agent_revision"], wire); return { task_id: dynamicID(body.task_id), revision: decimal(body.revision, wire, true), agent_revision: decimal(body.agent_revision, wire, true) };
    case "AGENT_CONTROL": {
      requireKeys(body, ["operation_id", "task_id", "run_id", "expected_task_revision", "expected_run_revision", "action", "instruction", "successor_task_id", "successor_incarnation_id"], wire);
      const action = agentControlAction(body.action);
      const instruction = boundedText(body.instruction, 0, action === "replace" ? MAX_TASK_INSTRUCTION_BYTES : MAX_HUMAN_REPLY_BYTES);
      const successor_task_id = body.successor_task_id === "" ? "" : dynamicID(body.successor_task_id);
      const successor_incarnation_id = body.successor_incarnation_id === "" ? "" : dynamicID(body.successor_incarnation_id);
      if (action === "replace") {
        if (instruction === "" || successor_task_id === "" || successor_incarnation_id === "" || successor_task_id === body.task_id) malformed();
      } else if (successor_task_id !== "" || successor_incarnation_id !== "" || (action === "message" ? instruction === "" : instruction !== "")) malformed();
      return { operation_id: dynamicID(body.operation_id), task_id: dynamicID(body.task_id), run_id: dynamicID(body.run_id), expected_task_revision: decimal(body.expected_task_revision, wire, true), expected_run_revision: decimal(body.expected_run_revision, wire, true), action, instruction, successor_task_id, successor_incarnation_id };
    }
    case "AGENT_CONTROL_RESULT": {
      requireKeys(body, ["operation_id", "task_id", "run_id", "status", "successor_task_id"], wire);
      if (body.status !== "delivered" && body.status !== "delivery_unknown" && body.status !== "rejected" && body.status !== "stopping" && body.status !== "queued") malformed();
      const successor_task_id = body.successor_task_id === "" ? "" : dynamicID(body.successor_task_id);
      if (body.status === "queued" ? successor_task_id === "" : successor_task_id !== "") malformed();
      return { operation_id: dynamicID(body.operation_id), task_id: dynamicID(body.task_id), run_id: dynamicID(body.run_id), status: body.status, successor_task_id };
    }
    case "TASK_HISTORY_GET": requireKeys(body, ["task_id"], wire); return { task_id: dynamicID(body.task_id) };
    case "TASK_HISTORY": {
      requireKeys(body, ["task_id", "entries"], wire);
      if (!Array.isArray(body.entries) || body.entries.length > MAX_ARRAY_ITEMS) malformed();
      return { task_id: dynamicID(body.task_id), entries: body.entries.map((entry) => taskHistoryEntry(entry, wire)) };
    }
    case "TASK_DETAIL_GET": { requireKeys(body, ["task_id", "expected_revision"], wire, ["text_offset", "peer_offset", "expected_head"]); const result: TaskDetailGetBody = { task_id: dynamicID(body.task_id), expected_revision: decimal(body.expected_revision, wire, true) }; if (present(body, "text_offset")) result.text_offset = decimal(body.text_offset, wire); if (present(body, "peer_offset")) result.peer_offset = decimal(body.peer_offset, wire); if (present(body, "expected_head")) result.expected_head = decimal(body.expected_head, wire, true); if ((result.peer_offset ?? 0n) !== 0n && result.expected_head === undefined) malformed(); return result; }
    case "TASK_DETAIL": requireKeys(body, ["task_id", "revision", "head", "instruction", "feedback", "peer_questions"], wire, ["next_text_offset", "next_peer_offset"]); { if (!Array.isArray(body.peer_questions) || body.peer_questions.length > 1) malformed(); const result: TaskDetailBody = { task_id: dynamicID(body.task_id), revision: decimal(body.revision, wire, true), head: decimal(body.head, wire, true), instruction: boundedText(body.instruction, 0, MAX_TASK_INSTRUCTION_BYTES), feedback: boundedText(body.feedback, 0, MAX_TASK_INSTRUCTION_BYTES), peer_questions: body.peer_questions.map((item) => taskPeerQuestion(item, wire)) }; if (present(body, "next_text_offset")) result.next_text_offset = decimal(body.next_text_offset, wire, true); if (present(body, "next_peer_offset")) result.next_peer_offset = decimal(body.next_peer_offset, wire, true); return result; }
    case "AGENT_UPDATE": requireKeys(body, ["agent_id", "expected_revision"], wire, ["model", "reasoning_effort", "account_id", "paused", "idle_policy", "idle_after_seconds", "idle_instruction", "idle_run_budget"]); { const result: AgentUpdateBody = { agent_id: dynamicID(body.agent_id), expected_revision: decimal(body.expected_revision, wire, true) }; if (present(body, "model")) result.model = boundedText(body.model, 0, MAX_AGENT_MODEL_BYTES); if (present(body, "reasoning_effort")) result.reasoning_effort = boundedText(body.reasoning_effort, 0, MAX_AGENT_MODEL_BYTES); if (present(body, "account_id")) result.account_id = body.account_id === "" ? "" : dynamicID(body.account_id); if (present(body, "paused")) { if (typeof body.paused !== "boolean") malformed(); result.paused = body.paused; } if (present(body, "idle_policy")) result.idle_policy = idlePolicy(body.idle_policy); if (present(body, "idle_after_seconds")) result.idle_after_seconds = integer(body.idle_after_seconds, 0, MAX_IDLE_AFTER_SECONDS); if (present(body, "idle_instruction")) result.idle_instruction = boundedText(body.idle_instruction, 0, MAX_TASK_INSTRUCTION_BYTES); if (present(body, "idle_run_budget")) result.idle_run_budget = integer(body.idle_run_budget, 0, MAX_IDLE_RUN_BUDGET); return result; }
    case "AGENT_UPDATE_RESULT": requireKeys(body, ["agent_id", "revision"], wire); return { agent_id: dynamicID(body.agent_id), revision: decimal(body.revision, wire, true) };
    case "TASK_UPDATE": requireKeys(body, ["task_id", "expected_revision"], wire, ["title", "body", "priority", "assigned_agent_id", "status"]); { const result: TaskUpdateBody = { task_id: dynamicID(body.task_id), expected_revision: decimal(body.expected_revision, wire, true) }; if (present(body, "title")) result.title = boundedText(body.title, 1, MAX_TASK_TITLE_BYTES); if (present(body, "body")) result.body = boundedText(body.body, 0, MAX_TASK_INSTRUCTION_BYTES); if (present(body, "priority")) result.priority = integer(body.priority, -MAX_TASK_PRIORITY, MAX_TASK_PRIORITY); if (present(body, "assigned_agent_id")) result.assigned_agent_id = dynamicID(body.assigned_agent_id); if (present(body, "status")) { if (body.status !== "cancelled") malformed(); result.status = body.status; } return result; }
    case "TASK_UPDATE_RESULT": requireKeys(body, ["task_id", "revision"], wire); return { task_id: dynamicID(body.task_id), revision: decimal(body.revision, wire, true) };
    case "TOPOLOGY_GET": requireKeys(body, ["project_id"], wire); return { project_id: dynamicID(body.project_id) };
    case "TOPOLOGY": requireKeys(body, ["project_id", "digest", "source_revision", "nodes"], wire); return { project_id: dynamicID(body.project_id), digest: fixedHex(body.digest, 32), source_revision: topologySource(body.source_revision), nodes: itemArray(body.nodes, (item) => topologyNode(item, wire)) };
    case "RUN_PATHS_GET": requireKeys(body, ["agent_id"], wire); return { agent_id: dynamicID(body.agent_id) };
    // No live run means no rooms, so an empty run identity carries no paths.
    case "RUN_PATHS": requireKeys(body, ["agent_id", "run_id", "paths"], wire); { if (!Array.isArray(body.paths) || body.paths.length > MAX_ARRAY_ITEMS) malformed(); if (body.run_id === "" && body.paths.length !== 0) malformed(); return { agent_id: dynamicID(body.agent_id), run_id: body.run_id === "" ? "" : dynamicID(body.run_id), paths: body.paths.map((item) => boundedText(item, 1, MAX_TASK_TITLE_BYTES)) }; }
    case "ACCOUNTS_DISCOVER": requireKeys(body, [], wire); return {};
    case "ACCOUNTS": requireKeys(body, ["accounts"], wire); { if (!Array.isArray(body.accounts) || body.accounts.length > MAX_ARRAY_ITEMS) malformed(); return { accounts: body.accounts.map((item) => discoveredAccount(item, wire)) }; }
    case "ACCOUNT_LINK": requireKeys(body, ["provider", "home", "label"], wire); return { provider: accountProvider(body.provider), home: accountHome(body.home), label: boundedText(body.label, 1, MAX_AGENT_NAME_BYTES) };
    case "ACCOUNT_LINK_RESULT": requireKeys(body, ["account_id", "revision"], wire); return { account_id: dynamicID(body.account_id), revision: decimal(body.revision, wire, true) };
    case "TERMINAL_TARGET_GET": requireKeys(body, ["agent_id", "expected_agent_revision", "expected_head"], wire); return { agent_id: dynamicID(body.agent_id), expected_agent_revision: decimal(body.expected_agent_revision, wire, true), expected_head: decimal(body.expected_head, wire) };
    case "TERMINAL_TARGET": requireKeys(body, ["agent_id", "agent_revision", "head", "target"], wire); { const target = body.target === null ? null : terminalTargetDescriptor(body.target, wire); return { agent_id: dynamicID(body.agent_id), agent_revision: decimal(body.agent_revision, wire, true), head: decimal(body.head, wire), target }; }
    case "TERMINAL_ATTACH": requireKeys(body, ["run_id", "session_id", "expected_run_revision", "expected_session_revision", "after_sequence"], wire); return { run_id: dynamicID(body.run_id), session_id: dynamicID(body.session_id), expected_run_revision: decimal(body.expected_run_revision, wire, true), expected_session_revision: decimal(body.expected_session_revision, wire, true), after_sequence: decimal(body.after_sequence, wire) };
    case "TERMINAL_ATTACHED": requireKeys(body, ["session_id", "floor", "head", "acknowledged_sequence", "max_unacked_bytes"], wire); { const floor = decimal(body.floor, wire); const head = decimal(body.head, wire); const acknowledged_sequence = decimal(body.acknowledged_sequence, wire); const max_unacked_bytes = decimal(body.max_unacked_bytes, wire); if (floor > head || acknowledged_sequence > head || max_unacked_bytes !== BigInt(MAX_TERMINAL_UNACKED_BYTES)) malformed(); return { session_id: dynamicID(body.session_id), floor, head, acknowledged_sequence, max_unacked_bytes }; }
    case "TERMINAL_ACK": requireKeys(body, ["session_id", "next_sequence"], wire); return { session_id: dynamicID(body.session_id), next_sequence: decimal(body.next_sequence, wire, true) };
    case "TERMINAL_LEASE_ACQUIRE": requireKeys(body, ["run_id", "session_id", "expected_run_revision", "expected_session_revision"], wire); return { run_id: dynamicID(body.run_id), session_id: dynamicID(body.session_id), expected_run_revision: decimal(body.expected_run_revision, wire, true), expected_session_revision: decimal(body.expected_session_revision, wire, true) };
    case "TERMINAL_LEASE_RENEW": case "TERMINAL_LEASE_RELEASE": requireKeys(body, ["run_id", "session_id", "generation", "expected_run_revision", "expected_session_revision"], wire); return { run_id: dynamicID(body.run_id), session_id: dynamicID(body.session_id), generation: decimal(body.generation, wire, true), expected_run_revision: decimal(body.expected_run_revision, wire, true), expected_session_revision: decimal(body.expected_session_revision, wire, true) };
    case "TERMINAL_LEASE_RESULT": requireKeys(body, ["operation", "run_id", "session_id", "generation", "last_input_sequence", "run_revision", "session_revision"], wire, ["expires_at_ms"]); if (body.operation !== "acquired" && body.operation !== "renewed" && body.operation !== "released") malformed(); { const operation = body.operation; const run_id = dynamicID(body.run_id); const session_id = dynamicID(body.session_id); const generation = decimal(body.generation, wire, true); const expires_at_ms = Object.prototype.hasOwnProperty.call(body, "expires_at_ms") ? decimal(body.expires_at_ms, wire, true) : undefined; const last_input_sequence = decimal(body.last_input_sequence, wire); const run_revision = decimal(body.run_revision, wire, true); const session_revision = decimal(body.session_revision, wire, true); if (operation === "released") { if (expires_at_ms !== undefined) malformed(); return { operation, run_id, session_id, generation, last_input_sequence, run_revision, session_revision }; } if (expires_at_ms === undefined) malformed(); return { operation, run_id, session_id, generation, expires_at_ms, last_input_sequence, run_revision, session_revision }; }
    case "TERMINAL_RESIZE": requireKeys(body, ["run_id", "session_id", "generation", "expected_run_revision", "expected_session_revision", "rows", "cols"], wire); return { run_id: dynamicID(body.run_id), session_id: dynamicID(body.session_id), generation: decimal(body.generation, wire, true), expected_run_revision: decimal(body.expected_run_revision, wire, true), expected_session_revision: decimal(body.expected_session_revision, wire, true), rows: integer(body.rows, 1, MAX_TERMINAL_ROWS), cols: integer(body.cols, 1, MAX_TERMINAL_COLS) };
    case "TERMINAL_RESIZED": requireKeys(body, ["session_id", "generation", "rows", "cols"], wire); return { session_id: dynamicID(body.session_id), generation: decimal(body.generation, wire, true), rows: integer(body.rows, 1, MAX_TERMINAL_ROWS), cols: integer(body.cols, 1, MAX_TERMINAL_COLS) };
    case "TERMINAL_DETACH": requireKeys(body, ["session_id"], wire); return { session_id: dynamicID(body.session_id) };
    case "TERMINAL_DETACHED": requireKeys(body, ["session_id"], wire); return { session_id: dynamicID(body.session_id) };
    case "TERMINAL_INPUT_RESULT": requireKeys(body, ["session_id", "generation", "sequence", "status", "accepted_bytes"], wire); if (body.status !== "accepted" && body.status !== "rejected" && body.status !== "partial" && body.status !== "uncertain") malformed(); { const accepted_bytes = decimal(body.accepted_bytes, wire); if (!validTerminalInputResult(body.status, accepted_bytes)) malformed(); return { session_id: dynamicID(body.session_id), generation: decimal(body.generation, wire, true), sequence: decimal(body.sequence, wire, true), status: body.status, accepted_bytes }; }
    case "TERMINAL_EOF": requireKeys(body, ["session_id"], wire); return { session_id: dynamicID(body.session_id) };
    case "TERMINAL_EXIT": requireKeys(body, ["session_id", "exit_code", "exit_signal", "aborted"], wire); if (typeof body.aborted !== "boolean") malformed(); { const exit_code = integer(body.exit_code, 0, Number.MAX_SAFE_INTEGER); const exit_signal = integer(body.exit_signal, 0, Number.MAX_SAFE_INTEGER); if (exit_signal !== 0 && exit_code !== 0) malformed(); return { session_id: dynamicID(body.session_id), exit_code, exit_signal, aborted: body.aborted }; }
    case "TERMINAL_RESET": requireKeys(body, ["session_id", "floor", "head"], wire); { const floor = decimal(body.floor, wire); const head = decimal(body.head, wire); if (floor > head) malformed(); return { session_id: dynamicID(body.session_id), floor, head }; }
    case "REMOTE_INVITE": requireKeys(body, [], wire); return {};
    case "REMOTE_INVITE_RESULT": requireKeys(body, ["link", "expires_at_ms", "svg"], wire); { const link = boundedText(body.link, 1, MAX_REMOTE_INVITE_LINK_BYTES); const svg = boundedText(body.svg, 1, MAX_REMOTE_INVITE_SVG_BYTES); if (!link.startsWith(REMOTE_INVITE_LINK_PREFIX) || /[\u0000-\u001f\u007f]/.test(link) || !svg.startsWith("<svg")) malformed(); return { link, expires_at_ms: decimal(body.expires_at_ms, wire, true), svg }; }
    case "ERROR": requireKeys(body, ["code", "retryable"], wire); if (typeof body.code !== "string" || !(ERROR_CODES as readonly string[]).includes(body.code) || typeof body.retryable !== "boolean") malformed(); return { code: body.code as ErrorCode, retryable: body.retryable };
  }
}

function stateSnapshot(body: Record<string, unknown>, wire: boolean): StateSnapshotBody {
  // An older daemon sends no accounts at all; the console then shows none.
  requireKeys(body, ["head", "factory", "projects", "agents", "tasks", "human_requests"], wire, ["accounts"]);
  const head = decimal(body.head, wire);
  if (!isObject(body.factory)) malformed();
  const factory = factoryItem(body.factory, wire);
  const projects = itemArray(body.projects, (item) => projectItem(item, wire));
  const agents = itemArray(body.agents, (item) => agentItem(item, wire));
  const tasks = itemArray(body.tasks, (item) => taskItem(item, wire));
  const human_requests = itemArray(body.human_requests, (item) => humanRequestItem(item, wire));
  const accounts = present(body, "accounts") ? itemArray(body.accounts, (item) => accountItem(item, wire)) : [];
  // The bound is exact and fails closed. A server that cannot fit its state
  // returns a too_large error; it never sends a trimmed snapshot.
  if (1 + projects.length + agents.length + tasks.length + human_requests.length + accounts.length > MAX_SNAPSHOT_ENTITIES) malformed();
  for (const collection of [projects, agents, tasks, human_requests, accounts]) uniqueIDs(collection);
  return { head, factory, projects, agents, tasks, human_requests, accounts };
}
function itemArray<T>(value: unknown, decode: (item: unknown) => T): T[] {
  if (!Array.isArray(value) || value.length > MAX_SNAPSHOT_ENTITIES) malformed();
  return value.map(decode);
}
function uniqueIDs(items: readonly { id: string }[]): void {
  const seen = new Set<string>();
  for (const item of items) { if (seen.has(item.id)) malformed(); seen.add(item.id); }
}
function factoryItem(value: unknown, wire: boolean): FactoryItem {
  if (!isObject(value)) malformed(); requireKeys(value, ["dispatch_enabled", "capacity", "active_runs", "revision"], wire);
  if (typeof value.dispatch_enabled !== "boolean") malformed(); const capacity = integer(value.capacity, 1, MAX_FACTORY_CAPACITY); const active_runs = integer(value.active_runs, 0, MAX_FACTORY_CAPACITY + 1);
  if (active_runs > capacity + 1) malformed(); return { dispatch_enabled: value.dispatch_enabled, capacity, active_runs, revision: decimal(value.revision, wire, true) };
}
function projectItem(value: unknown, wire: boolean): ProjectItem { if (!isObject(value)) malformed(); requireKeys(value, ["id", "name", "revision"], wire); return { id: dynamicID(value.id), name: boundedText(value.name, 1, MAX_PROJECT_NAME_BYTES), revision: decimal(value.revision, wire, true) }; }
function agentItem(value: unknown, wire: boolean): AgentItem {
  // An older daemon does not send the launch controls, the resolved model or
  // the account; they read as unset, which the console shows as an unknowable
  // CLI default under that provider's own directory.
  if (!isObject(value)) malformed(); requireKeys(value, ["id", "project_id", "name", "role", "provider", "paused", "revision"], wire, ["model", "reasoning_effort", "effective_model", "effective_reasoning_effort", "model_source", "account_id", "idle_policy", "idle_after_seconds", "idle_instruction", "idle_run_budget", "idle_runs_used"]);
  // An older daemon serves no idle rule; every agent then waits.
  const idle_policy = present(value, "idle_policy") ? idlePolicy(value.idle_policy) : "wait";
  const idle_after_seconds = present(value, "idle_after_seconds") ? integer(value.idle_after_seconds, 0, MAX_IDLE_AFTER_SECONDS) : 0;
  const idle_instruction = present(value, "idle_instruction") ? boundedText(value.idle_instruction, 0, MAX_TASK_INSTRUCTION_BYTES) : "";
  const idle_run_budget = present(value, "idle_run_budget") ? integer(value.idle_run_budget, 0, MAX_IDLE_RUN_BUDGET) : 0;
  const idle_runs_used = present(value, "idle_runs_used") ? integer(value.idle_runs_used, 0, idle_run_budget) : 0;
  if (value.role !== "orchestrator" && value.role !== "worker" || typeof value.paused !== "boolean") malformed();
  if (value.provider !== "claude_code" && value.provider !== "codex" && value.provider !== "shell") malformed();
  const model = present(value, "model") ? boundedText(value.model, 0, MAX_AGENT_MODEL_BYTES) : "";
  const reasoning_effort = present(value, "reasoning_effort") ? boundedText(value.reasoning_effort, 0, MAX_AGENT_MODEL_BYTES) : "";
  const effective_model = present(value, "effective_model") ? boundedText(value.effective_model, 0, MAX_AGENT_MODEL_BYTES) : "";
  const effective_reasoning_effort = present(value, "effective_reasoning_effort") ? boundedText(value.effective_reasoning_effort, 0, MAX_AGENT_MODEL_BYTES) : "";
  const model_source = present(value, "model_source") ? boundedText(value.model_source, 0, MAX_MODEL_SOURCE_BYTES) : "";
  const account_id = present(value, "account_id") && value.account_id !== "" ? dynamicID(value.account_id) : "";
  if (account_id !== "" && value.provider === "shell") malformed();
  return { id: dynamicID(value.id), project_id: dynamicID(value.project_id), name: boundedText(value.name, 1, MAX_AGENT_NAME_BYTES), role: value.role, provider: value.provider, paused: value.paused, model, reasoning_effort, effective_model, effective_reasoning_effort, model_source, revision: decimal(value.revision, wire, true), account_id, idle_policy, idle_after_seconds, idle_instruction, idle_run_budget, idle_runs_used };
}
function accountProvider(value: unknown): "claude_code" | "codex" { if (value !== "claude_code" && value !== "codex") malformed(); return value; }
/** One absolute configuration directory, bounded exactly as the daemon does. */
function accountHome(value: unknown): string { const home = boundedText(value, 1, MAX_TASK_TITLE_BYTES); if (!home.startsWith("/") || home.includes("\u0000")) malformed(); return home; }
function accountItem(value: unknown, wire: boolean): AccountItem {
  if (!isObject(value)) malformed(); requireKeys(value, ["id", "provider", "home", "label", "revision"], wire);
  return { id: dynamicID(value.id), provider: accountProvider(value.provider), home: accountHome(value.home), label: boundedText(value.label, 1, MAX_AGENT_NAME_BYTES), revision: decimal(value.revision, wire, true) };
}
function discoveredAccount(value: unknown, wire: boolean): DiscoveredAccount {
  if (!isObject(value)) malformed(); requireKeys(value, ["provider", "home", "label", "email", "organization", "default_model", "default_reasoning_effort", "linked_id"], wire);
  return {
    provider: accountProvider(value.provider), home: accountHome(value.home), label: boundedText(value.label, 1, MAX_AGENT_NAME_BYTES),
    email: boundedText(value.email, 0, MAX_AGENT_NAME_BYTES), organization: boundedText(value.organization, 0, MAX_AGENT_NAME_BYTES),
    default_model: boundedText(value.default_model, 0, MAX_AGENT_MODEL_BYTES), default_reasoning_effort: boundedText(value.default_reasoning_effort, 0, MAX_AGENT_MODEL_BYTES),
    linked_id: value.linked_id === "" ? "" : dynamicID(value.linked_id),
  };

}
const TOPOLOGY_KINDS = ["repository", "module", "package", "directory"] as const;
const TOPOLOGY_BUCKETS = ["empty", "tiny", "small", "medium", "large"] as const;
/** Empty, or one canonical Git object name in either length Git itself uses. */
function topologySource(value: unknown): string { if (typeof value !== "string" || value !== "" && !/^([0-9a-f]{40}|[0-9a-f]{64})$/.test(value)) malformed(); return value; }
function topologyNode(value: unknown, wire: boolean): TopologyNode {
  if (!isObject(value)) malformed(); requireKeys(value, ["id", "parent_id", "kind", "path", "label", "language", "size_bucket"], wire);
  if (typeof value.kind !== "string" || !(TOPOLOGY_KINDS as readonly string[]).includes(value.kind)) malformed();
  if (typeof value.size_bucket !== "string" || !(TOPOLOGY_BUCKETS as readonly string[]).includes(value.size_bucket)) malformed();
  return { id: fixedHex(value.id, 32), parent_id: value.parent_id === "" ? "" : fixedHex(value.parent_id, 32), kind: value.kind as TopologyNode["kind"], path: boundedText(value.path, 1, MAX_TASK_TITLE_BYTES), label: boundedText(value.label, 1, MAX_AGENT_NAME_BYTES), language: boundedText(value.language, 0, MAX_AGENT_NAME_BYTES), size_bucket: value.size_bucket as TopologyNode["size_bucket"] };
}
function taskItem(value: unknown, wire: boolean): TaskItem {
  if (!isObject(value)) malformed(); requireKeys(value, ["id", "project_id", "assigned_agent_id", "title", "status", "priority", "revision"], wire);
  if (typeof value.status !== "string" || !["queued", "running", "blocked", "succeeded", "failed", "cancelled"].includes(value.status)) malformed();
  return { id: dynamicID(value.id), project_id: dynamicID(value.project_id), assigned_agent_id: dynamicID(value.assigned_agent_id), title: boundedText(value.title, 1, MAX_TASK_TITLE_BYTES), status: value.status as TaskItem["status"], priority: integer(value.priority, -MAX_TASK_PRIORITY, MAX_TASK_PRIORITY), revision: decimal(value.revision, wire, true) };
}
function agentControlAction(value: unknown): AgentControlAction {
  if (value !== "message" && value !== "interrupt" && value !== "stop" && value !== "replace") malformed();
  return value;
}
function taskHistoryEntry(value: unknown, wire: boolean): TaskHistoryEntry {
  if (!isObject(value)) malformed();
  requireKeys(value, ["operation_id", "kind", "actor", "body", "status", "created_at_ms"], wire);
  if (value.status !== "pending" && value.status !== "delivered" && value.status !== "unknown" && value.status !== "rejected") malformed();
  return { operation_id: dynamicID(value.operation_id), kind: agentControlAction(value.kind), actor: boundedText(value.actor, 1, MAX_AGENT_NAME_BYTES), body: boundedText(value.body, 0, 1024), status: value.status, created_at_ms: decimal(value.created_at_ms, wire) };
}
function taskPeerQuestion(value: unknown, wire: boolean): TaskPeerQuestion {
  if (!isObject(value)) malformed();
  requireKeys(value, ["id", "source_task_id", "target_task_id", "question", "recipient_delivery_state", "answer_delivery_state", "revision", "created_at_ms", "updated_at_ms"], wire, ["answer"]);
  const created_at_ms = decimal(value.created_at_ms, wire, true); const updated_at_ms = decimal(value.updated_at_ms, wire, true);
  if (updated_at_ms < created_at_ms) malformed();
  if (value.recipient_delivery_state !== "pending" && value.recipient_delivery_state !== "delivered" && value.recipient_delivery_state !== "unknown") malformed();
  if (value.answer_delivery_state !== "pending" && value.answer_delivery_state !== "delivered" && value.answer_delivery_state !== "unknown") malformed();
  const result: TaskPeerQuestion = { id: dynamicID(value.id), source_task_id: dynamicID(value.source_task_id), target_task_id: dynamicID(value.target_task_id), question: boundedText(value.question, 1, 2048), recipient_delivery_state: value.recipient_delivery_state, answer_delivery_state: value.answer_delivery_state, revision: decimal(value.revision, wire, true), created_at_ms, updated_at_ms };
  if (present(value, "answer")) result.answer = boundedText(value.answer, 0, 2048);
  return result;
}
function humanRequestItem(value: unknown, wire: boolean): HumanRequestItem {
  if (!isObject(value)) malformed(); requireKeys(value, ["id", "project_id", "agent_id", "task_id", "created_at", "updated_at", "revision", "kind", "status", "reply_max_bytes", "can_reply"], wire);
  const created_at = decimal(value.created_at, wire); const updated_at = decimal(value.updated_at, wire);
  if (updated_at < created_at || value.kind !== "question" || typeof value.status !== "string" || !["open", "delivering", "delivery_unknown"].includes(value.status) || typeof value.can_reply !== "boolean") malformed();
  return { id: dynamicID(value.id), project_id: dynamicID(value.project_id), agent_id: dynamicID(value.agent_id), task_id: dynamicID(value.task_id), created_at, updated_at, revision: decimal(value.revision, wire, true), kind: "question", status: value.status as HumanRequestItem["status"], reply_max_bytes: integer(value.reply_max_bytes, 1, MAX_HUMAN_REPLY_BYTES), can_reply: value.can_reply };
}
function terminalTargetDescriptor(value: unknown, wire: boolean): TerminalTargetDescriptor {
  if (!isObject(value)) malformed(); requireKeys(value, ["run_id", "session_id", "run_revision", "session_revision"], wire);
  return { run_id: dynamicID(value.run_id), session_id: dynamicID(value.session_id), run_revision: decimal(value.run_revision, wire, true), session_revision: decimal(value.session_revision, wire, true) };
}

function decimal(value: unknown, wire: boolean, positive = false): bigint {
  let result: bigint;
  if (wire) { if (typeof value !== "string" || !/^(0|[1-9][0-9]*)$/.test(value)) malformed(); try { result = BigInt(value); } catch { malformed(); } }
  else { if (typeof value !== "bigint") malformed(); result = value; }
  if (result < 0n || result > MAX_SQLITE_INTEGER || positive && result === 0n) malformed(); return result;
}
function dynamicID(value: unknown): string { if (typeof value !== "string" || !/^[0-9a-f]{32}$/.test(value) || /^0{32}$/.test(value)) malformed(); return value; }
function boundedText(value: unknown, minimum: number, maximum: number): string { if (typeof value !== "string" || hasLoneSurrogate(value)) malformed(); const length = new TextEncoder().encode(value).length; if (length < minimum || length > maximum) malformed(); return value; }
function hasLoneSurrogate(value: string): boolean {
  for (let index = 0; index < value.length; index++) {
    const code = value.charCodeAt(index);
    if (code >= 0xd800 && code <= 0xdbff) {
      if (index + 1 >= value.length) return true;
      const next = value.charCodeAt(index + 1);
      if (next < 0xdc00 || next > 0xdfff) return true;
      index++;
    } else if (code >= 0xdc00 && code <= 0xdfff) return true;
  }
  return false;
}
function integer(value: unknown, minimum: number, maximum: number): number { if (typeof value !== "number" || !Number.isSafeInteger(value) || value < minimum || value > maximum) malformed(); return value; }
function validTerminalInputResult(status: string, acceptedBytes: bigint): boolean { return status === "accepted" || status === "partial" ? acceptedBytes >= 1n && acceptedBytes <= BigInt(MAX_TERMINAL_PAYLOAD) : status === "rejected" || status === "uncertain" ? acceptedBytes === 0n : false; }
function fixedHex(value: unknown, bytes: number, requireUncompressed = false): string { if (typeof value !== "string" || value.length !== bytes * 2 || !/^[0-9a-f]+$/.test(value)) malformed(); if (requireUncompressed && !value.startsWith("04")) malformed(); return value; }
function capabilities(value: unknown): number { const result = integer(value, 0, 31); if ((result & CAPABILITIES.observe) === 0) malformed(); return result; }
function validID(value: string): boolean { return value.length > 0 && value.length <= 64 && [...value].every((character) => character.charCodeAt(0) >= 0x21 && character.charCodeAt(0) <= 0x7e); }
function isControlType(value: unknown): value is ControlType { return typeof value === "string" && (CONTROL_TYPES as readonly string[]).includes(value); }
function isObject(value: unknown): value is Record<string, unknown> { return typeof value === "object" && value !== null && !Array.isArray(value); }
function present(value: Record<string, unknown>, key: string): boolean { return Object.prototype.hasOwnProperty.call(value, key); }
/**
 * Every required member must be present and every member this build knows is
 * validated below. Decoding a wire frame ignores an ASCII member that is not a
 * known name under any case (a non-ASCII name is refused, matching Go's Unicode
 * fold), so a newer daemon may add one without a coordinated
 * release; it reaches no field of the decoded frame. Byte, depth, array and
 * object-member bounds still apply. A locally constructed frame stays exact,
 * so a caller cannot quietly hand this encoder a field it will drop.
 *
 * A member differing from a known one only in case is refused rather than
 * ignored. Go's decoder matches struct fields case-insensitively, so there it
 * would overwrite the exact member; refusing it on both sides keeps the two
 * decoders reading the same frame the same way.
 */
function requireKeys(value: Record<string, unknown>, required: string[], wire = true, optional: string[] = []): void {
  if (required.some((key) => !Object.prototype.hasOwnProperty.call(value, key))) malformed();
  const known = [...required, ...optional];
  const present = Object.keys(value).filter((key) => !known.includes(key));
  if (!wire) { if (present.length !== 0) malformed(); return; }
  const folded = new Set(known.map((key) => key.toLowerCase()));
  // Go's decoder folds Unicode, so a non-ASCII name is refused on both sides.
  if (present.some((key) => /[^\x00-\x7f]/.test(key) || folded.has(key.toLowerCase()))) malformed();
}

// JSON.parse permits duplicate names and unsafe integers. This structural scan
// applies the same duplicate/depth/member/array/number bounds as Go.
function rejectDuplicateKeys(text: string, arrayLimit: number): void {
  let index = 0;
  const stringEnd = (): number => { if (text[index] !== "\"") malformed(); const start = index++; while (index < text.length) { const character = text[index++]; if (character === "\\") index++; else if (character === "\"") { JSON.parse(text.slice(start, index)); return index; } } malformed(); };
  const whitespace = (): void => { while (/\s/.test(text[index] ?? "")) index++; };
  const value = (depth: number): void => {
    if (depth > MAX_JSON_DEPTH) malformed(); whitespace();
    if (text[index] === "{") { index++; whitespace(); const keys = new Set<string>(); if (text[index] === "}") { index++; return; } while (true) { whitespace(); const start = index; stringEnd(); const key = JSON.parse(text.slice(start, index)) as string; if (keys.has(key) || keys.size === MAX_OBJECT_MEMBERS) malformed(); keys.add(key); whitespace(); if (text[index++] !== ":") malformed(); value(depth + 1); whitespace(); if (text[index] === "}") { index++; return; } if (text[index++] !== ",") malformed(); } }
    if (text[index] === "[") { index++; whitespace(); if (text[index] === "]") { index++; return; } let count = 0; while (true) { if (count++ === arrayLimit) malformed(); value(depth + 1); whitespace(); if (text[index] === "]") { index++; return; } if (text[index++] !== ",") malformed(); } }
    if (text[index] === "\"") { stringEnd(); return; }
    const start = index; while (index < text.length && !/[\s,\]}]/.test(text[index] ?? "")) index++; if (start === index) malformed(); const token = text.slice(start, index);
    if (token[0] === "-" || token[0] !== undefined && token[0] >= "0" && token[0] <= "9") validateJSONNumber(token);
  };
  value(0); whitespace(); if (index !== text.length) malformed();
}
function validateJSONNumber(value: string): void { if (!/^-?(0|[1-9][0-9]*)$/.test(value) || value === "-0") malformed(); let parsed: bigint; try { parsed = BigInt(value); } catch { malformed(); } if (parsed < -9_007_199_254_740_991n || parsed > 9_007_199_254_740_991n) malformed(); }

function idlePolicy(value: unknown): IdlePolicy { if (value !== "wait" && value !== "standing_instruction") malformed(); return value; }
