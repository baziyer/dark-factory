import type {
  AccountItem,
  AgentItem,
  FactoryItem,
  HumanRequestItem,
  ProjectItem,
  StateSnapshotBody,
  TaskItem,
} from "./control.js";

/**
 * One complete, immutable public snapshot read at `head`. There is no partial
 * or staged form: consumers either have a whole coherent snapshot or none.
 */
export type StateView = {
  head: bigint;
  factory: FactoryItem;
  projects: ReadonlyMap<string, ProjectItem>;
  agents: ReadonlyMap<string, AgentItem>;
  tasks: ReadonlyMap<string, TaskItem>;
  humanRequests: ReadonlyMap<string, HumanRequestItem>;
  /** The provider logins the operator has linked, by account identity. */
  accounts: ReadonlyMap<string, AccountItem>;
};

/**
 * Build the view from one decoded snapshot body. The codec has already proved
 * item shapes, the entity bound, and per-collection identity uniqueness, so
 * this is a pure, total conversion with no reconciliation of any kind.
 */
export function snapshotView(body: StateSnapshotBody): StateView {
  return Object.freeze({
    head: body.head,
    factory: Object.freeze({ ...body.factory }),
    projects: indexByID(body.projects),
    agents: indexByID(body.agents),
    tasks: indexByID(body.tasks),
    humanRequests: indexByID(body.human_requests),
    accounts: indexByID(body.accounts),
  });
}

function indexByID<T extends { id: string }>(items: readonly T[]): ReadonlyMap<string, T> {
  return new Map(items.map((item) => [item.id, Object.freeze({ ...item })] as const));
}
