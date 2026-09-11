import type { AgentItem, StateView, TaskItem, TopologyView } from "@dark-factory/client";
import { compareText, type SceneNode, type SceneTopology, type SceneWorker } from "./factory-scene/scene.js";

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

/** The operator-facing state has one name for each actionable condition. */
export function agentStatus(agent: AgentItem, state: StateView): AgentStatus {
  for (const request of state.humanRequests.values()) {
    if (request.agent_id === agent.id) return "needs-you";
  }
  if (agentCurrentTask(agent, state) !== undefined) return "working";
  return agent.paused ? "paused" : "ready";
}

/** The sprite vocabulary derives from the operator-facing status once. */
export function agentActivity(agent: AgentItem, state: StateView): AgentActivity {
  switch (agentStatus(agent, state)) {
    case "needs-you": return "needs-you";
    case "working": return "busy";
    case "paused": return "idle";
    case "ready": return "waiting";
  }
}

/** The overseer is the console's entry point; a worker is a usable fallback. */
export function primaryAgent(state: StateView): AgentItem | undefined {
  return [...state.agents.values()]
    .sort((left, right) =>
      (left.role === right.role ? 0 : left.role === "orchestrator" ? -1 : 1)
      || (left.role === "orchestrator" && left.paused !== right.paused ? left.paused ? 1 : -1 : 0)
      || compareText(left.name, right.name)
      || compareText(left.id, right.id))[0];
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
  omittedLocations: number;
}>;

/** One changed-path sample is tied to the task and provider run that produced it. */
export type RunPathSample = Readonly<{
  taskId: string;
  taskRevision: bigint;
  runId: string;
  projectId: string;
  paths: readonly string[];
}>;

/**
 * The floor is a projection, never a second source of truth: every project is a
 * block of rooms taken from its own served topology, or the one room that
 * stands for a project whose structure the daemon has not served yet, and every
 * worker stands in the room of code its matching live run is changing. A
 * retained sample only annotates a resting worker; an active overseer without
 * an observed path uses its assigned project control room. The floor is a grid because served nodes carry no edges: it
 * shows what the code is, not what depends on what.
 */
export function floorScene(
  state: StateView | undefined,
  topologies: ReadonlyMap<string, TopologyView> | undefined,
  runPaths?: ReadonlyMap<string, RunPathSample>,
  lastRunPaths?: ReadonlyMap<string, RunPathSample>,
): FloorScene {
  const projects = state === undefined ? [] : [...state.projects.values()].sort((left, right) => compareText(left.name, right.name) || compareText(left.id, right.id));
  const blocks = projects.map((project) => projectBlock(project, topologies?.get(project.id)));
  const blocksByProject = new Map(projects.map((project, index) => [project.id, blocks[index]]));
  const allRooms = new Map(blocks.flat().map((room) => [room.id, room]));
  const liveRooms = new Set<string>();
  if (state !== undefined) for (const agent of state.agents.values()) {
    const task = agentCurrentTask(agent, state);
    const sample = runPaths?.get(agent.id);
    if (task === undefined || sample?.taskId !== task.id || sample.taskRevision !== task.revision || sample.projectId !== agent.project_id || sample.runId === "") continue;
    const nodeId = roomOfRunPaths(blocksByProject.get(agent.project_id) ?? [], sample.paths);
    if (nodeId !== undefined) liveRooms.add(nodeId);
  }
  // A current observed location wins the shared cap. Project roots make the
  // rest of the map stable; remaining rooms then fill by the served size rank.
  const roots = blocks.map((block) => block[0]).filter((room): room is SceneNode => room !== undefined);
  const order = (rooms: readonly SceneNode[]) => [...rooms].sort((left, right) =>
    compareText(left.project?.name ?? "", right.project?.name ?? "") || compareText(left.project?.id ?? "", right.project?.id ?? "") || compareText(left.path, right.path) || compareText(left.id, right.id));
  const occupied = order([...liveRooms].map((id) => allRooms.get(id)).filter((room): room is SceneNode => room !== undefined));
  const remaining = blocks.flat().filter((room) => !liveRooms.has(room.id) && !roots.some((root) => root.id === room.id)).sort((left, right) =>
    SIZE_BUCKETS.indexOf(left.sizeBucket ?? "empty") - SIZE_BUCKETS.indexOf(right.sizeBucket ?? "empty")
    || compareText(left.project?.name ?? "", right.project?.name ?? "") || compareText(left.path, right.path) || compareText(left.id, right.id));
  const roomIDs = new Set<string>();
  const rooms = [...occupied, ...order(roots), ...remaining].filter((room) => {
    if (roomIDs.has(room.id)) return false;
    roomIDs.add(room.id);
    return true;
  }).slice(0, MAX_FLOOR_ROOMS);
  const kept = new Set(rooms.map((room) => room.id));
  const workers = state === undefined ? [] : [...state.agents.values()].map((agent) => {
    const task = agentCurrentTask(agent, state);
    const block = blocksByProject.get(agent.project_id) ?? [];
    const sample = runPaths?.get(agent.id);
    const live = task === undefined || sample?.taskId !== task.id || sample.taskRevision !== task.revision || sample.projectId !== agent.project_id || sample.runId === "" ? undefined : roomOfRunPaths(block, sample.paths);
    const previous = lastRunPaths?.get(agent.id);
    const last = previous?.projectId === agent.project_id && previous.paths.length > 0 ? roomOfRunPaths(block, previous.paths) : undefined;
    const controlRoom = agent.role === "orchestrator" && task !== undefined && live === undefined ? block[0] : undefined;
    const location: "working" | "control-room" | "last-observed" | "unobserved" | "resting" = task === undefined ? last === undefined ? "resting" : "last-observed" : live !== undefined ? "working" : controlRoom === undefined ? "unobserved" : "control-room";
    const room = location === "working" ? allRooms.get(live!) : location === "control-room" ? controlRoom : location === "last-observed" ? allRooms.get(last!) : undefined;
    return {
      id: agent.id,
      name: agent.name,
      role: agent.role,
      provider: agent.provider,
      activity: agentActivity(agent, state),
      paused: agent.paused,
      location,
      ...(room === undefined ? {} : { locationLabel: room.label }),
      ...((location === "working" && live !== undefined && kept.has(live)) || (location === "control-room" && room !== undefined && kept.has(room.id)) ? { nodeId: location === "working" ? live! : room!.id } : {}),
    };
  });
  const digest = projects.map((project) => topologies?.get(project.id)?.digest).filter((value) => value !== undefined).join(" ");
  return { topology: { digest, nodes: rooms }, workers, omittedLocations: [...liveRooms].filter((id) => !kept.has(id)).length };
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
