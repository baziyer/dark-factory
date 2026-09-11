import { spriteAtlas } from "./sprites/sprites.generated.js";

export type SceneTopology = Readonly<{
  digest: string;
  nodes: readonly SceneNode[];
}>;

export type SceneNode = Readonly<{
  id: string;
  path: string;
  label: string;
  kind: "repository" | "module" | "package" | "directory";
  /** Absent when the room stands for a project rather than a served node. */
  sizeBucket?: "empty" | "tiny" | "small" | "medium" | "large";
  /** The project this room belongs to: rooms sharing an id are laid out together under its name. */
  project?: Readonly<{ id: string; name: string }>;
}>;

export type SceneWorker = Readonly<{
  id: string;
  name: string;
  role: "orchestrator" | "worker";
  provider?: "claude_code" | "codex" | "shell";
  activity: "busy" | "waiting" | "needs-you" | "idle";
  paused?: boolean;
  /** Live work is placed in a room; retained samples annotate the resting area. */
  location?: "working" | "control-room" | "last-observed" | "unobserved" | "resting";
  locationLabel?: string;
  nodeId?: string;
}>;

export type ScenePoint = Readonly<{ x: number; y: number }>;

export type SceneRoomLayout = Readonly<{
  id: string;
  x: number;
  y: number;
  width: number;
  height: number;
  anchor: ScenePoint;
}>;

export type SceneHeading = Readonly<{ label: string; x: number; y: number }>;

export type SceneLayout = Readonly<{
  width: number;
  height: number;
  rooms: readonly SceneRoomLayout[];
  headings: readonly SceneHeading[];
  /** Reserved only when resting workers need the area above the rooms. */
  restingTop?: number;
}>;

export type SceneWorkerPlacement = Readonly<{
  id: string;
  area: "room" | "resting" | "staging" | "overflow";
  roomId?: string;
  x: number;
  y: number;
}>;

// The floor and wall patterns are anchored at the SVG origin, so a room shows
// whole tiles only while its edges and both pitches stay multiples of the frame.
const ROOM_WIDTH = 160;
const ROOM_HEIGHT = 96;
const ROOM_GAP = 16;
export const PADDING = 16;
const FLOOR_TOP = 48;
const HEADING = 16;
const WORKER_GAP = 18;

/** Ordering for the floor: byte order over paths and ids, never a locale. */
export function compareText(left: string, right: string) {
  return left < right ? -1 : left > right ? 1 : 0;
}

function centeredSlot(index: number) {
  if (index === 0) return 0;
  const distance = Math.ceil(index / 2);
  return index % 2 === 0 ? -distance : distance;
}

/** Rooms sit under their project's heading, projects in name order, then id. */
export function layoutScene(topology: SceneTopology, restingCount = 0): SceneLayout {
  const nodes = [...topology.nodes].sort((left, right) =>
    compareText(left.project?.name ?? "", right.project?.name ?? "") || compareText(left.project?.id ?? "", right.project?.id ?? "")
    || compareText(left.path, right.path) || compareText(left.id, right.id));
  const columns = Math.max(1, Math.min(4, Math.ceil(Math.sqrt(nodes.length))));
  const width = PADDING * 2 + columns * ROOM_WIDTH + (columns - 1) * ROOM_GAP;
  const outsideColumns = Math.max(1, Math.floor((width - PADDING * 2 - 16) / WORKER_GAP) + 1);
  const restingTop = restingCount === 0 ? undefined : FLOOR_TOP + 28;
  const restingRows = Math.ceil(restingCount / outsideColumns);
  const restingHeight = restingTop === undefined ? 0 : Math.ceil((28 + restingRows * WORKER_GAP + PADDING) / spriteAtlas.frame) * spriteAtlas.frame;
  const groups = new Map<string, SceneNode[]>();
  for (const node of nodes) groups.set(node.project?.id ?? "", [...(groups.get(node.project?.id ?? "") ?? []), node]);
  const rooms: SceneRoomLayout[] = [];
  const headings: SceneHeading[] = [];
  let top = FLOOR_TOP + restingHeight;
  for (const members of groups.values()) {
    const project = members[0]!.project;
    if (project !== undefined) {
      headings.push({ label: project.name, x: PADDING, y: top });
      top += HEADING;
    }
    members.forEach((node, index) => {
      const x = PADDING + (index % columns) * (ROOM_WIDTH + ROOM_GAP);
      const y = top + Math.floor(index / columns) * (ROOM_HEIGHT + ROOM_GAP);
      rooms.push({ id: node.id, x, y, width: ROOM_WIDTH, height: ROOM_HEIGHT, anchor: { x: x + ROOM_WIDTH / 2, y: y + ROOM_HEIGHT / 2 } });
    });
    top += Math.ceil(members.length / columns) * (ROOM_HEIGHT + ROOM_GAP);
  }
  const height = rooms.length === 0
    ? Math.max(FLOOR_TOP + 30 + PADDING, (restingTop ?? FLOOR_TOP) + restingRows * WORKER_GAP + PADDING * 2 + 8)
    : top - ROOM_GAP + PADDING;
  return { width, height, rooms, headings, ...(restingTop === undefined ? {} : { restingTop }) };
}

export function placeWorkers(layout: SceneLayout, workers: readonly SceneWorker[]): readonly SceneWorkerPlacement[] {
  const rooms = new Map(layout.rooms.map((room) => [room.id, room]));
  const roomCounts = new Map<string, number>();
  const outsideColumns = Math.max(1, Math.floor((layout.width - PADDING * 2 - 16) / WORKER_GAP) + 1);
  const sorted = [...workers].sort((left, right) => compareText(left.id, right.id));
  const resting = sorted.filter((worker) => worker.location !== "working" && worker.location !== "control-room" && worker.location !== "unobserved");
  const staging = sorted.filter((worker) => worker.location === "unobserved");
  const restRows = Math.ceil(resting.length / outsideColumns);
  const stagingRows = Math.ceil(staging.length / outsideColumns);
  const outside = (workers: readonly SceneWorker[], area: "resting" | "staging" | "overflow", top: number) => workers.map((worker, slot) => ({
    id: worker.id,
    area,
    x: PADDING + 8 + (slot % outsideColumns) * WORKER_GAP,
    y: top + Math.floor(slot / outsideColumns) * WORKER_GAP,
  }));
  const placed: SceneWorkerPlacement[] = [];
  const overflow: SceneWorker[] = sorted.filter((worker) => (worker.location === "working" || worker.location === "control-room") && worker.nodeId === undefined);
  for (const worker of sorted) {
    if ((worker.location !== "working" && worker.location !== "control-room") || worker.nodeId === undefined) continue;
    const room = rooms.get(worker.nodeId);
    const roomSlot = room === undefined ? -1 : roomCounts.get(room.id) ?? 0;
    const roomColumns = room === undefined ? 0 : Math.max(1, Math.floor((room.width - 16) / WORKER_GAP));
    // The title and served kind occupy the first 40px of every room.
    const roomRows = room === undefined ? 0 : Math.max(1, Math.floor((room.height - 56) / WORKER_GAP));
    if (room === undefined || roomSlot >= roomColumns * roomRows) {
      overflow.push(worker);
      continue;
    }
    roomCounts.set(room.id, roomSlot + 1);
    placed.push({
      id: worker.id,
      area: "room",
      roomId: room.id,
      x: room.anchor.x + centeredSlot(roomSlot % roomColumns) * WORKER_GAP,
      y: room.y + 48 + Math.floor(roomSlot / roomColumns) * WORKER_GAP,
    });
  }
  const restingTop = layout.restingTop ?? layout.height + 28;
  const stagingTop = layout.restingTop === undefined
    ? restingTop + restRows * WORKER_GAP + (resting.length === 0 || (staging.length === 0 && overflow.length === 0) ? 0 : 32)
    : layout.height + 28;
  const overflowTop = stagingTop + stagingRows * WORKER_GAP + (staging.length === 0 || overflow.length === 0 ? 0 : 32);
  return [...placed, ...outside(resting, "resting", restingTop), ...outside(staging, "staging", stagingTop), ...outside(overflow, "overflow", overflowTop)]
    .sort((left, right) => compareText(left.id, right.id));
}

/** The sheet frame a worker stands as; an unknown provider wears shell. */
export function workerFrame(worker: SceneWorker): string {
  // FNV-1a over the id only; four appearances aid recognition, names identify.
  let hash = 2166136261;
  for (let i = 0; i < worker.id.length; i++) hash = Math.imul(hash ^ worker.id.charCodeAt(i), 16777619) >>> 0;
  const identity = hash % 4;
  const role = worker.role === "orchestrator" ? "overseer" : "worker";
  const provider = worker.provider === "claude_code" || worker.provider === "codex" ? worker.provider : "shell";
  const frame = `${role}.${provider}.${identity}.${worker.activity}.0`;
  return frame in spriteAtlas.frames ? frame : `${role}.${provider}.${identity}.idle.0`;
}
