import type { AgentItem, StateView, TaskItem, TopologyView } from "@dark-factory/client";
import type { SceneNode, SceneTopology, SceneWorker, SceneWorkItem } from "./factory-scene/scene.js";

/** The task stages the daemon actually serves today. */
export type TaskStage = "queued" | "building" | "blocked" | "done" | "failed";

export const STAGE_SEQUENCE: readonly TaskStage[] = ["queued", "building"];

export type AgentActivity = "busy" | "waiting" | "needs-you" | "idle";

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

/** One floor holds the repository root and its direct children, and no more. */
const MAX_FLOOR_ROOMS = 24;

export type FloorScene = Readonly<{
  topology: SceneTopology;
  workers: readonly SceneWorker[];
  workItems: readonly SceneWorkItem[];
}>;

/**
 * The floor is a projection, never a second source of truth: rooms come from
 * the served topology (or one room per project until it arrives), and every
 * worker stands in its own project's room.
 */
export function floorScene(state: StateView | undefined, topology: TopologyView | undefined): FloorScene {
  const projects = state === undefined ? [] : [...state.projects.values()];
  // Topology is served one project at a time. Every other project still gets
  // its own room, so no agent is stranded in the overflow bay.
  const detailed = topology === undefined ? [] : topologyRooms(topology);
  const rooms = [...detailed, ...projectRooms(projects.filter((project) => project.id !== topology?.projectId))]
    .slice(0, MAX_FLOOR_ROOMS);
  const roomIDs = new Set(rooms.map((room) => room.id));
  const rootID = detailed[0]?.id;
  const roomOfProject = (projectID: string): string | undefined =>
    projectID === topology?.projectId ? rootID : roomIDs.has(projectID) ? projectID : undefined;
  const workers = state === undefined ? [] : [...state.agents.values()].map((agent) => {
    const nodeId = roomOfProject(agent.project_id);
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
  return { topology: { digest: topology?.digest ?? "", nodes: rooms }, workers, workItems };
}

function topologyRooms(topology: TopologyView): readonly SceneNode[] {
  const root = topology.nodes.find((node) => node.parent_id === "");
  if (root === undefined) return [];
  const children = topology.nodes.filter((node) => node.parent_id === root.id);
  return [root, ...children].slice(0, MAX_FLOOR_ROOMS).map((node) => ({
    id: node.id,
    parentId: node.parent_id,
    path: node.path,
    label: node.label,
    kind: node.kind,
    sizeBucket: node.size_bucket,
  }));
}

function projectRooms(projects: readonly { id: string; name: string }[]): readonly SceneNode[] {
  return projects.map((project) => ({
    id: project.id,
    parentId: "",
    path: project.name,
    label: project.name,
    kind: "repository" as const,
  }));
}
