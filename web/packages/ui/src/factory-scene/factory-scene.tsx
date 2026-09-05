import {
  layoutScene,
  placeWorkers,
  workerSprite,
  type SceneNode,
  type SceneTopology,
  type SceneWorker,
  type SceneWorkerSprite,
  type SceneWorkItem,
} from "./scene.js";

export type {
  SceneLayout,
  SceneNode,
  SceneRoomLayout,
  SceneTopology,
  SceneWorker,
  SceneWorkerPlacement,
  SceneWorkerSprite,
  SceneWorkItem,
} from "./scene.js";

export type FactorySceneProps = Readonly<{
  topology: SceneTopology;
  workers: readonly SceneWorker[];
  workItems: readonly SceneWorkItem[];
  /** Pointer convenience only; the AGENTS list is the keyboard path. */
  onSelectWorker?: (workerId: string) => void;
}>;

const PADDING = 12;
const SERVICE_HEIGHT = 52;

const roomFill: Record<SceneNode["kind"], string> = {
  repository: "#16364a",
  module: "#1b3a4a",
  package: "#193d38",
  directory: "#282f42",
};

function shortLabel(label: string) {
  const glyphs = [...label];
  return glyphs.length > 18 ? `${glyphs.slice(0, 17).join("")}…` : label;
}

function SpriteSymbol({ sprite }: { sprite: SceneWorkerSprite }) {
  return (
    <symbol id={`df-worker-${sprite.key}`} viewBox="0 0 16 16">
      {sprite.rects.map((rect, index) => (
        <rect
          key={index}
          x={rect.x}
          y={rect.y}
          width={rect.width}
          height={rect.height}
          fill={rect.color}
          opacity={rect.opacity}
        />
      ))}
    </symbol>
  );
}

/** A disposable SVG projection of topology and current factory state. */
export function FactoryScene({ topology, workers, workItems, onSelectWorker }: FactorySceneProps) {
  const layout = layoutScene(topology);
  const placements = placeWorkers(layout, workers);
  const nodes = new Map(topology.nodes.map((node) => [node.id, node]));
  const workerById = new Map(workers.map((worker) => [worker.id, worker]));
  const spriteByWorker = new Map(workers.map((worker) => [worker.id, workerSprite(worker)]));
  const sprites = new Map<string, SceneWorkerSprite>();
  for (const sprite of spriteByWorker.values()) sprites.set(sprite.key, sprite);
  const orderedSprites = [...sprites.values()].sort((left, right) => left.key < right.key ? -1 : left.key > right.key ? 1 : 0);
  const unassigned = placements.filter((placement) => placement.roomId === undefined);
  const outside = placements.filter((placement) => placement.y > layout.height);
  const workerBottom = Math.max(layout.height, ...placements.map((placement) => placement.y + 8));
  const serviceY = workerBottom + PADDING;
  const sceneHeight = serviceY + SERVICE_HEIGHT + PADDING;
  const bayGap = 6;
  const bayWidth = (layout.width - PADDING * 2 - bayGap * 2) / 3;
  const staged = workItems.filter((item) => item.stage === "staged").length;
  const ready = workItems.length - staged;

  return (
    <svg
      viewBox={`0 0 ${layout.width} ${sceneHeight}`}
      role="group"
      aria-label="Dark Factory codebase floor"
      data-topology-digest={topology.digest}
      style={{ display: "block", width: "100%", height: "auto", background: "#08131d" }}
    >
      <title>Dark Factory codebase floor</title>
      <desc>{`${layout.rooms.length} topology spaces, ${workers.length} workers, ${workItems.length} tasks`}</desc>
      <defs>{orderedSprites.map((sprite) => <SpriteSymbol key={sprite.key} sprite={sprite} />)}</defs>
      <rect width={layout.width} height={sceneHeight} fill="#08131d" />
      <text x={PADDING} y="20" fill="#b9cad5" fontFamily="ui-monospace, monospace" fontSize="10" fontWeight="700">
        FACTORY FLOOR · {layout.rooms.length} SPACES
      </text>

      {layout.rooms.map((room) => {
        const node = nodes.get(room.id);
        if (node === undefined) return null;
        return (
          <g key={room.id} data-room-id={room.id}>
            <title>{node.path}</title>
            <rect x={room.x} y={room.y} width={room.width} height={room.height} rx="3" fill={roomFill[node.kind]} stroke="#638095" />
            <text x={room.x + 8} y={room.y + 18} fill="#f2f6f8" fontFamily="ui-monospace, monospace" fontSize="11" fontWeight="700">
              {shortLabel(node.label)}
            </text>
            <text x={room.x + 8} y={room.y + 34} fill="#9db1be" fontFamily="ui-monospace, monospace" fontSize="8">
              {node.kind.toUpperCase()}{node.sizeBucket === undefined ? "" : ` · ${node.sizeBucket.toUpperCase()}`}
            </text>
          </g>
        );
      })}

      {layout.rooms.length === 0 ? (
        <text x={layout.width / 2} y="52" textAnchor="middle" fill="#7890a2" fontFamily="ui-monospace, monospace" fontSize="10">
          EMPTY FLOOR
        </text>
      ) : null}

      {outside.length === 0 ? null : (
        <g role="group" aria-label={`${outside.length} workers outside rooms`}>
          <rect x={PADDING} y={layout.height} width={layout.width - PADDING * 2} height={workerBottom - layout.height + PADDING} rx="3" fill="#101f2b" stroke="#385164" />
          <text x={PADDING + 6} y={layout.height + 15} fill="#9db1be" fontFamily="ui-monospace, monospace" fontSize="8">
            WORKER OVERFLOW · {outside.length}
          </text>
        </g>
      )}

      {placements.map((placement) => {
        const worker = workerById.get(placement.id);
        const sprite = spriteByWorker.get(placement.id);
        if (worker === undefined || sprite === undefined) return null;
        const room = placement.roomId === undefined ? undefined : nodes.get(placement.roomId);
        const location = room === undefined ? "unassigned" : room.label;
        return (
          <g
            key={worker.id}
            data-worker-id={worker.id}
            transform={`translate(${placement.x} ${placement.y})`}
            role="img"
            aria-label={`${worker.name}, ${worker.role}, ${worker.activity}, ${location}`}
            {...(onSelectWorker === undefined ? {} : { onClick: () => onSelectWorker(worker.id), style: { cursor: "pointer" } })}
          >
            <title>{worker.name}</title>
            <use href={`#df-worker-${sprite.key}`} x="-8" y="-8" width="16" height="16" />
          </g>
        );
      })}

      {([[
        "FREE",
        unassigned.length,
        `${unassigned.length} unassigned workers`,
      ], [
        "STAGED",
        staged,
        `${staged} staged tasks`,
      ], [
        "READY",
        ready,
        `${ready} release-ready tasks`,
      ]] as const).map(([label, count, ariaLabel], index) => {
        const x = PADDING + index * (bayWidth + bayGap);
        return (
          <g key={label} role="group" aria-label={ariaLabel}>
            <rect x={x} y={serviceY} width={bayWidth} height={SERVICE_HEIGHT} rx="3" fill="#101f2b" stroke="#385164" />
            <text x={x + bayWidth / 2} y={serviceY + 17} textAnchor="middle" fill="#9db1be" fontFamily="ui-monospace, monospace" fontSize="7">{label}</text>
            <text x={x + bayWidth / 2} y={serviceY + 40} textAnchor="middle" fill="#f2f6f8" fontFamily="ui-monospace, monospace" fontSize="16" fontWeight="700">{count}</text>
          </g>
        );
      })}
    </svg>
  );
}
