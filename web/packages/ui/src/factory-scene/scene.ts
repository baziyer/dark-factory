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
  nodeId?: string;
}>;

export type SceneWorkItem = Readonly<{
  id: string;
  stage: "staged" | "release-ready";
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
}>;

export type SceneWorkerPlacement = Readonly<{
  id: string;
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
export function layoutScene(topology: SceneTopology): SceneLayout {
  const nodes = [...topology.nodes].sort((left, right) =>
    compareText(left.project?.name ?? "", right.project?.name ?? "") || compareText(left.project?.id ?? "", right.project?.id ?? "")
    || compareText(left.path, right.path) || compareText(left.id, right.id));
  const columns = Math.max(1, Math.min(4, Math.ceil(Math.sqrt(nodes.length))));
  const width = PADDING * 2 + columns * ROOM_WIDTH + (columns - 1) * ROOM_GAP;
  const groups = new Map<string, SceneNode[]>();
  for (const node of nodes) groups.set(node.project?.id ?? "", [...(groups.get(node.project?.id ?? "") ?? []), node]);
  const rooms: SceneRoomLayout[] = [];
  const headings: SceneHeading[] = [];
  let top = FLOOR_TOP;
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
  return { width, height: rooms.length === 0 ? FLOOR_TOP + 30 + PADDING : top - ROOM_GAP + PADDING, rooms, headings };
}

export function placeWorkers(layout: SceneLayout, workers: readonly SceneWorker[]): readonly SceneWorkerPlacement[] {
  const rooms = new Map(layout.rooms.map((room) => [room.id, room]));
  const roomCounts = new Map<string, number>();
  const outsideColumns = Math.max(1, Math.floor((layout.width - PADDING * 2 - 16) / WORKER_GAP) + 1);
  let outside = 0;
  return [...workers]
    .sort((left, right) => compareText(left.id, right.id))
    .map((worker) => {
      const room = worker.nodeId === undefined ? undefined : rooms.get(worker.nodeId);
      const roomSlot = room === undefined ? -1 : roomCounts.get(room.id) ?? 0;
      const roomColumns = room === undefined ? 0 : Math.max(1, Math.floor((room.width - 16) / WORKER_GAP));
      const roomRows = room === undefined ? 0 : Math.max(1, Math.floor((room.height - 16) / WORKER_GAP));
      if (room === undefined || roomSlot >= roomColumns * roomRows) {
        const slot = outside;
        outside += 1;
        return {
          id: worker.id,
          ...(room === undefined ? {} : { roomId: room.id }),
          x: PADDING + 8 + (slot % outsideColumns) * WORKER_GAP,
          y: layout.height + 28 + Math.floor(slot / outsideColumns) * WORKER_GAP,
        };
      }
      roomCounts.set(room.id, roomSlot + 1);
      return {
        id: worker.id,
        roomId: room.id,
        x: room.anchor.x + centeredSlot(roomSlot % roomColumns) * WORKER_GAP,
        y: room.anchor.y + centeredSlot(Math.floor(roomSlot / roomColumns)) * WORKER_GAP,
      };
    });
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

/** busy and idle carry a second frame; every other activity holds still. */
export function alternateFrame(frame: string): string | undefined {
  const alternate = frame.replace(/\.0$/, ".1");
  return alternate in spriteAtlas.frames ? alternate : undefined;
}
