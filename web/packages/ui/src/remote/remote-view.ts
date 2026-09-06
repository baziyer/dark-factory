import { RemoteDaemonMismatchError } from "@dark-factory/client";
import type {
  HumanRequestItem,
  RemoteFactoryStatus,
  StateView,
  TaskItem,
} from "@dark-factory/client";
import { orderTasksForHome } from "../console-view.js";

/**
 * The remote console shows one factory's connection state as a glyph, in the
 * same vocabulary the floor uses: a filled mark is live, a ring is in flight,
 * and a refusal is never dressed up as a transient.
 */
export const REMOTE_STATUS_GLYPH: Record<RemoteFactoryStatus, string> = {
  offline: "·",
  connecting: "◌",
  pairing: "◌",
  authenticating: "◌",
  syncing: "◌",
  ready: "◆",
  revoked: "×",
  mismatch: "×",
  expired: "×",
  error: "!",
};

const DELIVERY_UNKNOWN = "DELIVERY UNKNOWN — CHECK THE FACTORY";
const NOT_DELIVERED = "NOT DELIVERED";
export const REQUEST_CLOSED = "THIS QUESTION IS NO LONGER OPEN";
export const REQUEST_UNAVAILABLE = "THE QUESTION COULD NOT BE OPENED";
export const FACTORY_UNREACHABLE = "FACTORY OFFLINE — NOTHING WAS SENT";

/**
 * The banner a factory's own connection state earns. A state a person can do
 * nothing about is named plainly; a state that is merely in flight is not
 * shouted at them.
 */
export function remoteFactoryBanner(status: RemoteFactoryStatus): string | undefined {
  switch (status) {
    case "revoked":
      return "ACCESS REVOKED";
    // The node still routes, but the daemon behind it is not the bound one.
    case "mismatch":
      return "FACTORY IDENTITY MISMATCH";
    // The relay would refuse this ticket, so it is never presented again.
    case "expired":
      return "INVITATION EXPIRED · PAIR AGAIN";
    // The manager only reports error for a binding no reconnection repairs.
    case "error":
      return "FACTORY REFUSED · PAIR AGAIN";
    case "offline":
    case "connecting":
      return "FACTORY OFFLINE";
    default:
      return undefined;
  }
}

/**
 * Every action control in this console is gated on one predicate: the device
 * has a network and this factory has a live authenticated session. Nothing is
 * offered that could only fail.
 */
export function remoteActionable(status: RemoteFactoryStatus | undefined, online: boolean): boolean {
  return online && status === "ready";
}

/**
 * The codes the daemon only produces by refusing before it acts — the same set
 * the desktop console names NOT SENT. A bad response frame is not among them:
 * a reply result whose revision has moved on is what a delivered answer looks
 * like, so every other failure leaves the effect genuinely unknown.
 */
const REFUSED = new Set([
  "invalid_request",
  "unauthorized",
  "stale",
  "too_large",
  "rate_limited",
  "not_found",
  "crypto_unavailable",
  "unsupported",
]);

function errorCode(error: unknown): string | undefined {
  const code = (error as { code?: unknown } | null | undefined)?.code;
  return typeof code === "string" ? code : undefined;
}

/** One-shot effects have exactly two honest outcomes when they do not succeed. */
export function remoteDeliveryNotice(error: unknown): string {
  const code = errorCode(error);
  return code !== undefined && REFUSED.has(code) ? NOT_DELIVERED : DELIVERY_UNKNOWN;
}

export const INVITATION_UNREADABLE = "THAT LINK IS NOT A FACTORY INVITATION";
export const INVITATION_SPENT = "THIS INVITATION HAS EXPIRED OR WAS ALREADY USED";

const PAIR_FAILURES = new Map<string, string>([
  ["invalid_request", INVITATION_UNREADABLE],
  ["unauthorized", "THE FACTORY REFUSED THIS INVITATION"],
  ["pairing_required", "THE FACTORY REFUSED THIS INVITATION"],
  ["pairing_uncertain", "PAIRING RESULT UNKNOWN — CHECK THE FACTORY BEFORE PAIRING AGAIN"],
  ["storage_unavailable", "THIS BROWSER CANNOT STORE A FACTORY KEY"],
  ["crypto_unavailable", "THIS BROWSER CANNOT PAIR"],
  ["connection", "THE RELAY REFUSED THE CONNECTION"],
  ["closed", "THE RELAY REFUSED THE CONNECTION"],
  ["malformed", "THE FACTORY ANSWERED WITH SOMETHING THIS DEVICE COULD NOT READ"],
]);

export function remotePairFailure(error: unknown): string {
  // An identity failure, not a refusal: the invitation was answered, but not
  // by the daemon it named.
  if (error instanceof RemoteDaemonMismatchError) return "A DIFFERENT FACTORY ANSWERED FOR THIS NODE";
  const code = errorCode(error);
  return (code === undefined ? undefined : PAIR_FAILURES.get(code)) ?? "PAIRING DID NOT COMPLETE";
}

export type RemoteProjectGroup = Readonly<{
  id: string;
  name: string;
  tasks: readonly TaskItem[];
}>;

/**
 * The factory's work, grouped by the project that owns it. Task order inside a
 * group is the console's own home order, so a phone and a desktop rank the
 * same work the same way.
 */
export function remoteProjectGroups(state: StateView): readonly RemoteProjectGroup[] {
  const groups = new Map<string, TaskItem[]>();
  for (const task of orderTasksForHome(state)) {
    const existing = groups.get(task.project_id);
    if (existing === undefined) groups.set(task.project_id, [task]);
    else existing.push(task);
  }
  // A project with nothing queued is still part of the factory's shape.
  for (const project of state.projects.values()) if (!groups.has(project.id)) groups.set(project.id, []);
  return [...groups.entries()].map(([id, tasks]) => Object.freeze({
    id,
    name: state.projects.get(id)?.name ?? `project ${id.slice(0, 8)}`,
    tasks: Object.freeze(tasks),
  }));
}

/** The factory's own open questions, ordered oldest first like the aggregate. */
export function remoteOpenRequests(state: StateView | undefined): readonly HumanRequestItem[] {
  if (state === undefined) return [];
  const open = [...state.humanRequests.values()].filter((request) => request.status === "open");
  open.sort((left, right) => left.created_at === right.created_at
    ? (left.id < right.id ? -1 : left.id > right.id ? 1 : 0)
    : left.created_at < right.created_at ? -1 : 1);
  return Object.freeze(open);
}

export function shortRemoteID(value: string): string {
  return value.slice(0, 8);
}
