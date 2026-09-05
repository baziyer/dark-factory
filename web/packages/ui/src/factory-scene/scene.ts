import { spriteAtlas } from "./sprites/sprites.generated.js";

export type SceneTopology = Readonly<{
  digest: string;
  nodes: readonly SceneNode[];
}>;

export type SceneNode = Readonly<{
  id: string;
  parentId: string;
  path: string;
  label: string;
  kind: "repository" | "module" | "package" | "directory";
  /** Absent when the room stands for a project rather than a served node. */
  sizeBucket?: "empty" | "tiny" | "small" | "medium" | "large";
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

export type SceneLayout = Readonly<{
  width: number;
  height: number;
  rooms: readonly SceneRoomLayout[];
}>;

export type SceneWorkerPlacement = Readonly<{
  id: string;
  roomId?: string;
  x: number;
  y: number;
}>;

const ROOM_WIDTH = 152;
const ROOM_HEIGHT = 96;
const ROOM_GAP = 12;
const PADDING = 12;
const FLOOR_TOP = 40;
const WORKER_GAP = 18;

function compareText(left: string, right: string) {
  return left < right ? -1 : left > right ? 1 : 0;
}

function centeredSlot(index: number) {
  if (index === 0) return 0;
  const distance = Math.ceil(index / 2);
  return index % 2 === 0 ? -distance : distance;
}

export function layoutScene(topology: SceneTopology): SceneLayout {
  const nodes = [...topology.nodes].sort((left, right) =>
    compareText(left.path, right.path) || compareText(left.id, right.id));
  const columns = Math.max(1, Math.min(4, Math.ceil(Math.sqrt(nodes.length))));
  const rows = Math.ceil(nodes.length / columns);
  const width = PADDING * 2 + columns * ROOM_WIDTH + (columns - 1) * ROOM_GAP;
  const roomsHeight = rows === 0 ? 30 : rows * ROOM_HEIGHT + (rows - 1) * ROOM_GAP;
  const rooms = nodes.map((node, index) => {
    const x = PADDING + (index % columns) * (ROOM_WIDTH + ROOM_GAP);
    const y = FLOOR_TOP + Math.floor(index / columns) * (ROOM_HEIGHT + ROOM_GAP);
    return {
      id: node.id,
      x,
      y,
      width: ROOM_WIDTH,
      height: ROOM_HEIGHT,
      anchor: { x: x + ROOM_WIDTH / 2, y: y + ROOM_HEIGHT / 2 },
    };
  });
  return { width, height: FLOOR_TOP + roomsHeight + PADDING, rooms };
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
  const role = worker.role === "orchestrator" ? "overseer" : "worker";
  const provider = worker.provider === "claude_code" || worker.provider === "codex" ? worker.provider : "shell";
  const frame = `${role}.${provider}.${worker.activity}.0`;
  return frame in spriteAtlas.frames ? frame : `${role}.${provider}.idle.0`;
}

/** busy and idle carry a second frame; every other activity holds still. */
export function alternateFrame(frame: string): string | undefined {
  const alternate = frame.replace(/\.0$/, ".1");
  return alternate in spriteAtlas.frames ? alternate : undefined;
}
