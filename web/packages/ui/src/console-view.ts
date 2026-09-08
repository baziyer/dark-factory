import type { AgentItem, StateView, TaskItem, TopologyView } from "@dark-factory/client";
import { compareText, type SceneNode, type SceneTopology, type SceneWorker, type SceneWorkItem } from "./factory-scene/scene.js";

/** The task stages the daemon actually serves today. */
export type TaskStage = "queued" | "building" | "blocked" | "done" | "failed";

export const STAGE_SEQUENCE: readonly TaskStage[] = ["queued", "building"];

export type AgentActivity = "busy" | "waiting" | "needs-you" | "idle";
/** The operator-facing state has one name for each actionable condition. */
export type AgentStatus = "working" | "ready" | "needs-you" | "paused";

/** The durable task status projected into the console stage vocabulary. */
export function stageOfTask(task: TaskItem): TaskStage {
  switch (task.status) {
    case "queued":
      return "queued";
    case "running":
      return "building";
    case "blocked":
      return "blocked";
    case "succeeded":
      return "done";
    case "failed":
    case "cancelled":
      return "failed";
  }
}

/** Segments filled by the durable stage; done fills the complete meter. */
export function stageMeterFill(stage: TaskStage): number {
  if (stage === "done") return STAGE_SEQUENCE.length;
  if (stage === "blocked" || stage === "failed") return 0;
  return STAGE_SEQUENCE.indexOf(stage) + 1;
}

/** Tasks an agent is on right now (durable assignment, live statuses). */
export function agentCurrentTask(agent: AgentItem, state: StateView): TaskItem | undefined {
  for (const task of state.tasks.values()) {
    if (
      task.assigned_agent_id === agent.id &&
      task.status === "running"
    ) {
      return task;
    }
  }
  return undefined;
}

/** Precedence: an open question outranks activity; pause outranks waiting. */
export function agentActivity(agent: AgentItem, state: StateView): AgentActivity {
  for (const request of state.humanRequests.values()) {
    if (request.agent_id === agent.id) return "needs-you";
  }
  if (agentCurrentTask(agent, state) !== undefined) return "busy";
  return agent.paused ? "idle" : "waiting";
}

/** The console's words are smaller than the sprite vocabulary. */
export function agentStatus(agent: AgentItem, state: StateView): AgentStatus {
  for (const request of state.humanRequests.values()) {
    if (request.agent_id === agent.id) return "needs-you";
  }
  if (agentCurrentTask(agent, state) !== undefined) return "working";
  if (agent.paused) return "paused";
  return "ready";
}

/** The overseer is the console's entry point; a worker is a usable fallback. */
export function primaryAgent(state: StateView): AgentItem | undefined {
  return [...state.agents.values()]
    .sort((left, right) => (left.role === right.role ? 0 : left.role === "orchestrator" ? -1 : 1) || compareText(left.name, right.name) || compareText(left.id, right.id))[0];
}

/**
 * The glyph derives from durable facts only: the orchestrator role and the
 * served provider identity. C is Claude, X is Codex, s is the shell provider.
 */
export function agentGlyph(agent: AgentItem): string {
  if (agent.role === "orchestrator") return "◆";
  switch (agent.provider) {
    case "claude_code":
      return "C";
    case "codex":
      return "X";
    case "shell":
      return "s";
  }
}

export type FactoryCounters = Readonly<{
  queued: number | undefined;
  needsYou: number | undefined;
}>;

export function factoryCounters(state: StateView | undefined): FactoryCounters {
  if (state === undefined) return { queued: undefined, needsYou: undefined };
  let queued = 0;
  for (const task of state.tasks.values()) {
    if (stageOfTask(task) === "queued") queued += 1;
  }
  return { queued, needsYou: state.humanRequests.size };
}

/** Active work first, then queued, then finished; priority breaks ties. */
export function orderTasksForHome(state: StateView): readonly TaskItem[] {
  const rank: Record<TaskStage, number> = {
    building: 0,
    queued: 1,
    blocked: 2,
    done: 3,
    failed: 4,
  };
  return [...state.tasks.values()].sort((left, right) => {
    const byStage = rank[stageOfTask(left)] - rank[stageOfTask(right)];
    return byStage !== 0 ? byStage : right.priority - left.priority;
  });
}

/** One floor holds this many rooms, shared out across every project. */
const MAX_FLOOR_ROOMS = 24;

export type FloorScene = Readonly<{
  topology: SceneTopology;
  workers: readonly SceneWorker[];
  workItems: readonly SceneWorkItem[];
}>;

/**
 * The floor is a projection, never a second source of truth: every project is a
 * block of rooms taken from its own served topology, or the one room that
 * stands for a project whose structure the daemon has not served yet, and every
 * worker stands in the room of the code its live run is changing inside its own
 * project. The floor is a grid because the served nodes carry no edges: it
 * shows what the code is, not what depends on what.
 */
export function floorScene(
  state: StateView | undefined,
  topologies: ReadonlyMap<string, TopologyView> | undefined,
  runPaths?: ReadonlyMap<string, readonly string[]>,
): FloorScene {
  const projects = state === undefined ? [] : [...state.projects.values()];
  const blocks = projects.map((project) => projectBlock(project, topologies?.get(project.id)));
  // The cap is shared, never first come: every project keeps its own room
  // before any project keeps a second, and past that the largest rooms win.
  const rooms = blocks
    .flatMap((block, project) => block.map((room, rank) => ({ project, rank, room })))
    .sort((left, right) => left.rank - right.rank || left.project - right.project)
    .slice(0, MAX_FLOOR_ROOMS)
    .map((entry) => entry.room);
  const kept = new Set(rooms.map((room) => room.id));
  const shown = new Map(projects.map((project, index) => [project.id, blocks[index]!.filter((room) => kept.has(room.id))]));
  const workers = state === undefined ? [] : [...state.agents.values()].map((agent) => {
    // A run only ever names paths in its own project, and that project's own
    // room is the first one it keeps, so an unmapped path costs no worker its
    // room. Only an agent whose project the cap never reached has none.
    const block = shown.get(agent.project_id) ?? [];
    const nodeId = roomOfRunPaths(block, runPaths?.get(agent.id) ?? []) ?? block[0]?.id;
    return {
      id: agent.id,
      name: agent.name,
      role: agent.role,
      provider: agent.provider,
      activity: agentActivity(agent, state),
      ...(nodeId === undefined ? {} : { nodeId }),
    };
  });
  const workItems = state === undefined ? [] : [...state.tasks.values()]
    .filter((task) => task.status === "succeeded" || task.status === "running" || task.status === "blocked")
    .map((task) => ({ id: task.id, stage: task.status === "succeeded" ? "release-ready" as const : "staged" as const }));
  const digest = projects.map((project) => topologies?.get(project.id)?.digest).filter((value) => value !== undefined).join(" ");
  return { topology: { digest, nodes: rooms }, workers, workItems };
}

/**
 * The room a live run's changed paths stand a worker in: each path picks the
 * deepest displayed room whose own path prefixes it (the root's "." prefixes
 * everything), and the room holding the most paths wins, ties going to the
 * room the floor sorts first. No paths means no answer and no move.
 */
function roomOfRunPaths(rooms: readonly SceneNode[], paths: readonly string[]): string | undefined {
  const counts = new Map<SceneNode, number>();
  for (const path of paths) {
    const room = rooms
      .filter((candidate) => candidate.path === "." || path === candidate.path || path.startsWith(`${candidate.path}/`))
      .sort((left, right) => right.path.length - left.path.length)[0];
    if (room !== undefined) counts.set(room, (counts.get(room) ?? 0) + 1);
  }
  return [...counts]
    .sort(([left, leftCount], [right, rightCount]) => rightCount - leftCount || compareText(left.path, right.path))[0]?.[0].id;
}

/** Room size, largest first: past the cap the biggest rooms keep their tile. */
const SIZE_BUCKETS = ["large", "medium", "small", "tiny", "empty"];

/**
 * One project's rooms: the code its repository root holds, largest first. A Go
 * module or a JS package rooted at "." is the same place as the repository, not
 * a room of its own, so every node at "." is root and the rooms are their
 * children. The repository is always the first room, the one a worker with no
 * path of its own stands in and the one a project keeps when the cap bites; a
 * project the daemon has not served a structure for has only that room.
 */
function projectBlock(project: { id: string; name: string }, topology: TopologyView | undefined): readonly SceneNode[] {
  const root = topology?.nodes.find((node) => node.parent_id === "");
  if (topology === undefined || root === undefined) {
    return [{ id: project.id, path: project.name, label: project.name, kind: "repository", project: { id: project.id, name: project.name } }];
  }
  const roots = new Set(topology.nodes.filter((node) => node.path === ".").map((node) => node.id));
  roots.add(root.id);
  const children = topology.nodes
    .filter((node) => node.path !== "." && roots.has(node.parent_id))
    .sort((left, right) =>
      SIZE_BUCKETS.indexOf(left.size_bucket) - SIZE_BUCKETS.indexOf(right.size_bucket)
      || compareText(left.path, right.path));
  return [root, ...children].slice(0, MAX_FLOOR_ROOMS).map((node) => ({
    // The daemon salts node ids with the project; the prefix keeps two rooms
    // on one floor apart against a daemon that does not.
    id: `${project.id}:${node.id}`,
    path: node.path,
    // Every repository is served the same fixed label, so on a floor of many
    // projects only the project's own name tells its root room apart.
    label: node.path === "." ? project.name : node.label,
    kind: node.kind,
    sizeBucket: node.size_bucket,
    project: { id: project.id, name: project.name },
  }));
}
