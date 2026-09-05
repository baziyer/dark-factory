/** The browser contract has no generation: it is unversioned and tolerant of
 * added members by owner decision on 4 September 2026. This name identifies
 * the contract; it never moves. */
export const BROWSER_PROTOCOL_NAME = "dark-factory/browser" as const;
/** The fixed third byte of every binary terminal frame, beside the "DF" magic. */
export const TERMINAL_FRAME_VERSION = 1 as const;
export const MAX_CONTROL_BYTES = 64 * 1024;
export const MAX_TERMINAL_PAYLOAD = 8 * 1024;
export const TERMINAL_HEADER_BYTES = 40;
export const MAX_JSON_DEPTH = 16;
export const MAX_ARRAY_ITEMS = 32;
export const MAX_OBJECT_MEMBERS = 32;
/** Only a server STATE_SNAPSHOT may exceed MAX_CONTROL_BYTES. */
export const MAX_SNAPSHOT_BYTES = 1024 * 1024;
export const MAX_SNAPSHOT_ENTITIES = 4096;
export const MAX_PROJECT_NAME_BYTES = 128;
export const MAX_AGENT_NAME_BYTES = 128;
export const MAX_TASK_TITLE_BYTES = 1024;
export const MAX_HUMAN_QUESTION_BYTES = 8192;
export const MAX_HUMAN_REPLY_BYTES = 8192;
export const MAX_TASK_INSTRUCTION_BYTES = 32768;
export const MAX_FACTORY_CAPACITY = 1024;
export const MAX_TASK_PRIORITY = 1_000_000;
export const MAX_SQLITE_INTEGER = 9_223_372_036_854_775_807n;
export const MAX_TERMINAL_UNACKED_BYTES = 65_536;
export const TERMINAL_ACK_TIMEOUT_MS = 10_000;
export const TERMINAL_LEASE_RENEW_INTERVAL_MS = 10_000;
export const MAX_TERMINAL_ROWS = 4096;
export const MAX_TERMINAL_COLS = 4096;
export const MAX_AGENT_MODEL_BYTES = 128;
export const MAX_MODEL_SOURCE_BYTES = 1024;
export const MAX_REMOTE_INVITE_LINK_BYTES = 8192;
export const MAX_REMOTE_INVITE_SVG_BYTES = 32768;

export const CAPABILITIES = {
  observe: 1,
  private_human_request_detail: 2,
  human_actions: 4,
  terminal_input: 8,
} as const;

export type CapabilityName = keyof typeof CAPABILITIES;
export type CapabilityMask = number;

export const CONTROL_MANIFEST = [
  { type: "HELLO", direction: "server", id: "forbidden", fixture: "hello.json" },
  { type: "PAIR_PROVE", direction: "client", id: "required", fixture: "pair_prove.json" },
  { type: "PAIR_RESULT", direction: "server", id: "required", fixture: "pair_result.json" },
  { type: "AUTH_PROVE", direction: "client", id: "required", fixture: "auth_prove.json" },
  { type: "AUTH_RESULT", direction: "server", id: "required", fixture: "auth_result.json" },
  { type: "STATE_GET", direction: "client", id: "required", fixture: "state_get.json" },
  { type: "STATE_SNAPSHOT", direction: "server", id: "required", fixture: "state_snapshot.json" },
  { type: "STATE_WATCH", direction: "client", id: "required", fixture: "state_watch.json" },
  { type: "STATE_CHANGED", direction: "server", id: "required", fixture: "state_changed.json" },
  { type: "HUMAN_REQUEST_DETAIL_GET", direction: "client", id: "required", fixture: "human_request_detail_get.json" },
  { type: "HUMAN_REQUEST_DETAIL", direction: "server", id: "required", fixture: "human_request_detail.json" },
  { type: "HUMAN_REQUEST_REPLY", direction: "client", id: "required", fixture: "human_request_reply.json" },
  { type: "HUMAN_REQUEST_REPLY_RESULT", direction: "server", id: "required", fixture: "human_request_reply_result.json" },
  { type: "HUMAN_REQUEST_CANCEL_RUN", direction: "client", id: "required", fixture: "human_request_cancel_run.json" },
  { type: "HUMAN_REQUEST_CANCEL_RUN_RESULT", direction: "server", id: "required", fixture: "human_request_cancel_run_result.json" },
  { type: "TASK_ENQUEUE", direction: "client", id: "required", fixture: "task_enqueue.json" },
  { type: "TASK_ENQUEUE_RESULT", direction: "server", id: "required", fixture: "task_enqueue_result.json" },
  { type: "TERMINAL_TARGET_GET", direction: "client", id: "required", fixture: "terminal_target_get.json" },
  { type: "TERMINAL_TARGET", direction: "server", id: "required", fixture: "terminal_target.json" },
  { type: "TERMINAL_ATTACH", direction: "client", id: "required", fixture: "terminal_attach.json" },
  { type: "TERMINAL_ATTACHED", direction: "server", id: "required", fixture: "terminal_attached.json" },
  { type: "TERMINAL_ACK", direction: "client", id: "forbidden", fixture: "terminal_ack.json" },
  { type: "TERMINAL_LEASE_ACQUIRE", direction: "client", id: "required", fixture: "terminal_lease_acquire.json" },
  { type: "TERMINAL_LEASE_RENEW", direction: "client", id: "required", fixture: "terminal_lease_renew.json" },
  { type: "TERMINAL_LEASE_RELEASE", direction: "client", id: "required", fixture: "terminal_lease_release.json" },
  { type: "TERMINAL_LEASE_RESULT", direction: "server", id: "required", fixture: "terminal_lease_result.json" },
  { type: "TERMINAL_RESIZE", direction: "client", id: "required", fixture: "terminal_resize.json" },
  { type: "TERMINAL_RESIZED", direction: "server", id: "required", fixture: "terminal_resized.json" },
  { type: "TERMINAL_DETACH", direction: "client", id: "required", fixture: "terminal_detach.json" },
  { type: "TERMINAL_DETACHED", direction: "server", id: "required", fixture: "terminal_detached.json" },
  { type: "TERMINAL_INPUT_RESULT", direction: "server", id: "required", fixture: "terminal_input_result.json" },
  { type: "TERMINAL_EOF", direction: "server", id: "required", fixture: "terminal_eof.json" },
  { type: "TERMINAL_EXIT", direction: "server", id: "required", fixture: "terminal_exit.json" },
  { type: "TERMINAL_RESET", direction: "server", id: "required", fixture: "terminal_reset.json" },
  { type: "AGENT_UPDATE", direction: "client", id: "required", fixture: "agent_update.json" },
  { type: "AGENT_UPDATE_RESULT", direction: "server", id: "required", fixture: "agent_update_result.json" },
  { type: "TASK_UPDATE", direction: "client", id: "required", fixture: "task_update.json" },
  { type: "TASK_UPDATE_RESULT", direction: "server", id: "required", fixture: "task_update_result.json" },
  { type: "TOPOLOGY_GET", direction: "client", id: "required", fixture: "topology_get.json" },
  { type: "TOPOLOGY", direction: "server", id: "required", fixture: "topology.json" },
  { type: "RUN_PATHS_GET", direction: "client", id: "required", fixture: "run_paths_get.json" },
  { type: "RUN_PATHS", direction: "server", id: "required", fixture: "run_paths.json" },
  { type: "ACCOUNTS_DISCOVER", direction: "client", id: "required", fixture: "accounts_discover.json" },
  { type: "ACCOUNTS", direction: "server", id: "required", fixture: "accounts.json" },
  { type: "ACCOUNT_LINK", direction: "client", id: "required", fixture: "account_link.json" },
  { type: "ACCOUNT_LINK_RESULT", direction: "server", id: "required", fixture: "account_link_result.json" },
  { type: "REMOTE_INVITE", direction: "client", id: "required", fixture: "remote_invite.json" },
  { type: "REMOTE_INVITE_RESULT", direction: "server", id: "required", fixture: "remote_invite_result.json" },
  { type: "ERROR", direction: "both", id: "optional", fixture: "error.json" },
] as const;
export const CONTROL_TYPES = CONTROL_MANIFEST.map((entry) => entry.type);
export type ControlType = (typeof CONTROL_TYPES)[number];

export const ERROR_CODES = [
  "unauthorized",
  "invalid_request",
  "rate_limited",
  "not_found",
  "stale",
  "too_large",
  "internal",
] as const;
export type ErrorCode = (typeof ERROR_CODES)[number];

export const TERMINAL_OPCODES = {
  TERMINAL_INPUT: 1,
  TERMINAL_OUTPUT: 2,
} as const;

export const BROWSER_MANIFEST = {
  name: BROWSER_PROTOCOL_NAME,
  capabilities: CAPABILITIES,
  bounds: {
    maxControlBytes: MAX_CONTROL_BYTES,
    maxJSONDepth: MAX_JSON_DEPTH,
    maxArrayItems: MAX_ARRAY_ITEMS,
    maxObjectMembers: MAX_OBJECT_MEMBERS,
    maxSnapshotBytes: MAX_SNAPSHOT_BYTES,
    maxSnapshotEntities: MAX_SNAPSHOT_ENTITIES,
    maxProjectNameBytes: MAX_PROJECT_NAME_BYTES,
    maxAgentNameBytes: MAX_AGENT_NAME_BYTES,
    maxTaskTitleBytes: MAX_TASK_TITLE_BYTES,
    maxHumanQuestionBytes: MAX_HUMAN_QUESTION_BYTES,
    maxHumanReplyBytes: MAX_HUMAN_REPLY_BYTES,
    maxTaskInstructionBytes: MAX_TASK_INSTRUCTION_BYTES,
    maxFactoryCapacity: MAX_FACTORY_CAPACITY,
    maxTaskPriority: MAX_TASK_PRIORITY,
    maxSQLiteInteger: MAX_SQLITE_INTEGER,
    maxTerminalUnackedBytes: MAX_TERMINAL_UNACKED_BYTES,
    terminalAckTimeoutMs: TERMINAL_ACK_TIMEOUT_MS,
    terminalLeaseRenewIntervalMs: TERMINAL_LEASE_RENEW_INTERVAL_MS,
    maxTerminalRows: MAX_TERMINAL_ROWS,
    maxTerminalCols: MAX_TERMINAL_COLS,
    maxAgentModelBytes: MAX_AGENT_MODEL_BYTES,
    maxModelSourceBytes: MAX_MODEL_SOURCE_BYTES,
    maxRemoteInviteLinkBytes: MAX_REMOTE_INVITE_LINK_BYTES,
    maxRemoteInviteSvgBytes: MAX_REMOTE_INVITE_SVG_BYTES,
  },
  control: CONTROL_MANIFEST,
  terminal: {
    magic: "DF",
    version: TERMINAL_FRAME_VERSION,
    headerBytes: TERMINAL_HEADER_BYTES,
    maxPayloadBytes: MAX_TERMINAL_PAYLOAD,
    opcodes: TERMINAL_OPCODES,
  },
} as const;
