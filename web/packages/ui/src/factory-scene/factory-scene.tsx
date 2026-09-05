import {
  alternateFrame,
  layoutScene,
  placeWorkers,
  workerFrame,
  type SceneTopology,
  type SceneWorker,
  type SceneWorkItem,
} from "./scene.js";
import { spriteAtlas, spriteSheet } from "./sprites/sprites.generated.js";

export type {
  SceneLayout,
  SceneNode,
  SceneRoomLayout,
  SceneTopology,
  SceneWorker,
  SceneWorkerPlacement,
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
const SHEET_WIDTH = 128;
const SHEET_HEIGHT = 96;
const FRAME = spriteAtlas.frame;

function shortLabel(label: string) {
  const glyphs = [...label];
  return glyphs.length > 18 ? `${glyphs.slice(0, 17).join("")}…` : label;
}

/** One 16px frame of the sheet, sized and placed in scene coordinates. */
function Frame({ name, x, y, className }: { name: string; x: number; y: number; className?: string }) {
  return <use href={`#df-frame-${name}`} x={x} y={y} width={FRAME} height={FRAME} className={className} />;
}

/** A disposable SVG projection of topology and current factory state. */
export function FactoryScene({ topology, workers, workItems, onSelectWorker }: FactorySceneProps) {
  const layout = layoutScene(topology);
  const placements = placeWorkers(layout, workers);
  const nodes = new Map(topology.nodes.map((node) => [node.id, node]));
  const workerById = new Map(workers.map((worker) => [worker.id, worker]));
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
      <defs>
        {/* The sheet enters the document once; every frame is a window on it. */}
        <image id="df-sheet" href={spriteSheet} width={SHEET_WIDTH} height={SHEET_HEIGHT} style={{ imageRendering: "pixelated" }} />
        {Object.entries(spriteAtlas.frames).map(([name, cell]) => (
          <symbol key={name} id={`df-frame-${name}`} viewBox={`${cell.x} ${cell.y} ${FRAME} ${FRAME}`}>
            <use href="#df-sheet" />
          </symbol>
        ))}
        <pattern id="df-floor" patternUnits="userSpaceOnUse" width={FRAME * 2} height={FRAME * 2}>
          <Frame name="tile.floor.0" x={0} y={0} />
          <Frame name="tile.floor.1" x={FRAME} y={0} />
          <Frame name="tile.floor.1" x={0} y={FRAME} />
          <Frame name="tile.floor.0" x={FRAME} y={FRAME} />
        </pattern>
        <pattern id="df-wall" patternUnits="userSpaceOnUse" width={FRAME} height={FRAME}>
          <Frame name="tile.wall" x={0} y={0} />
        </pattern>
      </defs>
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
            <rect x={room.x} y={room.y} width={room.width} height={room.height} rx="3" fill="url(#df-floor)" stroke="#638095" />
            <rect x={room.x} y={room.y} width={room.width} height={FRAME} fill="url(#df-wall)" />
            <Frame name="tile.door" x={room.x + room.width / 2 - FRAME / 2} y={room.y + room.height - FRAME} />
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
        if (worker === undefined) return null;
        const room = placement.roomId === undefined ? undefined : nodes.get(placement.roomId);
        const location = room === undefined ? "unassigned" : room.label;
        const frame = workerFrame(worker);
        // The second frame sits on top and blinks in; without it, and under
        // reduced motion, frame zero is all that shows.
        const alternate = alternateFrame(frame);
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
            <Frame name={frame} x={-8} y={-8} />
            {alternate === undefined ? null : <Frame name={alternate} x={-8} y={-8} className="dfFactoryScene__alternate" />}
          </g>
        );
      })}

      {([[
        "FREE",
        unassigned.length,
        `${unassigned.length} unassigned workers`,
        "bay.free",
      ], [
        "STAGED",
        staged,
        `${staged} staged tasks`,
        "bay.staged",
      ], [
        "READY",
        ready,
        `${ready} release-ready tasks`,
        "bay.ready",
      ]] as const).map(([label, count, ariaLabel, bay], index) => {
        const x = PADDING + index * (bayWidth + bayGap);
        return (
          <g key={label} role="group" aria-label={ariaLabel}>
            <rect x={x} y={serviceY} width={bayWidth} height={SERVICE_HEIGHT} rx="3" fill="#101f2b" stroke="#385164" />
            <Frame name={bay} x={x + 6} y={serviceY + (SERVICE_HEIGHT - FRAME) / 2} />
            <text x={x + bayWidth / 2} y={serviceY + 17} textAnchor="middle" fill="#9db1be" fontFamily="ui-monospace, monospace" fontSize="7">{label}</text>
            <text x={x + bayWidth / 2} y={serviceY + 40} textAnchor="middle" fill="#f2f6f8" fontFamily="ui-monospace, monospace" fontSize="16" fontWeight="700">{count}</text>
          </g>
        );
      })}
    </svg>
  );
}
