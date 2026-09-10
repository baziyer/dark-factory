import {
  PADDING,
  layoutScene,
  placeWorkers,
  workerFrame,
  type SceneTopology,
  type SceneWorker,
} from "./scene.js";
import { spriteAtlas, spriteSheet, spriteSheetSize } from "./sprites/sprites.generated.js";

export type {
  SceneHeading,
  SceneLayout,
  SceneNode,
  SceneRoomLayout,
  SceneTopology,
  SceneWorker,
  SceneWorkerPlacement,
} from "./scene.js";

export type FactorySceneProps = Readonly<{
  topology: SceneTopology;
  workers: readonly SceneWorker[];
  /** Current changed locations omitted by the bounded room map. */
  omittedLocations?: number;
  /** Pointer convenience only; the AGENTS list is the keyboard path. */
  onSelectWorker?: (workerId: string) => void;
}>;

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
export function FactoryScene({ topology, workers, omittedLocations = 0, onSelectWorker }: FactorySceneProps) {
  const layout = layoutScene(topology, workers.filter((worker) => worker.location !== "working" && worker.location !== "unobserved").length);
  const placements = placeWorkers(layout, workers);
  const nodes = new Map(topology.nodes.map((node) => [node.id, node]));
  const workerById = new Map(workers.map((worker) => [worker.id, worker]));
  const resting = placements.filter((placement) => placement.area === "resting");
  const staging = placements.filter((placement) => placement.area === "staging");
  const overflow = placements.filter((placement) => placement.area === "overflow");
  const occupied = new Set(placements.filter((placement) => placement.area === "room").map((placement) => placement.roomId));
  const sceneHeight = Math.max(layout.height, ...placements.map((placement) => placement.y + 8)) + PADDING;
  // A wide column must not blow 16px frames up to poster size: the scene stops
  // at three CSS pixels per sheet pixel and centres in whatever is left.
  const maxWidth = layout.width * 3;

  return (
    <svg
      viewBox={`0 0 ${layout.width} ${sceneHeight}`}
      role="group"
      aria-label="Dark Factory codebase floor"
      data-topology-digest={topology.digest}
      style={{ display: "block", width: "100%", minWidth: Math.min(maxWidth, 864), maxWidth, height: "auto", margin: "0 auto", background: "#08131d" }}
    >
      <title>Dark Factory codebase floor</title>
      <desc>{`${layout.rooms.length} topology spaces, ${workers.length} workers${omittedLocations === 0 ? "" : `, ${omittedLocations} current locations omitted by the room cap`}`}</desc>
      <defs>
        {/* The sheet enters the document once; every frame is a window on it. */}
        <image id="df-sheet" href={spriteSheet} width={spriteSheetSize.width} height={spriteSheetSize.height} style={{ imageRendering: "pixelated" }} />
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

      {layout.headings.map((heading) => (
        <text key={heading.y} data-floor-heading={heading.label} x={heading.x} y={heading.y + 11} fill="#b9cad5" fontFamily="ui-monospace, monospace" fontSize="9" fontWeight="700">
          {shortLabel(heading.label)}
        </text>
      ))}

      {layout.rooms.map((room) => {
        const node = nodes.get(room.id);
        if (node === undefined) return null;
        return (
          <g key={room.id} data-room-id={room.id} className={occupied.has(room.id) ? undefined : "dfFactoryScene__room--empty"}>
            <title>{node.path}</title>
            <rect x={room.x} y={room.y} width={room.width} height={room.height} rx="3" fill="url(#df-floor)" stroke="#638095" />
            <rect x={room.x} y={room.y} width={room.width} height={FRAME} fill="url(#df-wall)" />
            <Frame name="tile.door" x={room.x + Math.floor(room.width / 2 / FRAME) * FRAME} y={room.y + room.height - FRAME} />
            <text x={room.x + 8} y={room.y + 18} fill="#f2f6f8" fontFamily="ui-monospace, monospace" fontSize="11" fontWeight="700">
              {shortLabel(node.label)}
            </text>
            <text x={room.x + 8} y={room.y + 34} fill="#9db1be" fontFamily="ui-monospace, monospace" fontSize="8">
              {node.kind.toUpperCase()}{node.sizeBucket === undefined ? "" : ` · ${node.sizeBucket.toUpperCase()}`}
            </text>
          </g>
        );
      })}

      {resting.length === 0 ? null : <Area label={`RESTING AREA · ${resting.length}`} width={layout.width - PADDING * 2} top={Math.min(...resting.map((placement) => placement.y)) - 28} bottom={Math.max(...resting.map((placement) => placement.y + 8)) + PADDING} />}
      {staging.length === 0 ? null : <Area label={`WORKING · ${staging.length} LOCATION${staging.length === 1 ? "" : "S"} NOT YET OBSERVED`} width={layout.width - PADDING * 2} top={Math.min(...staging.map((placement) => placement.y)) - 28} bottom={Math.max(...staging.map((placement) => placement.y + 8)) + PADDING} />}
      {overflow.length === 0 ? null : <Area label={omittedLocations === 0 ? `WORKER AREA AT CAPACITY · ${overflow.length}` : `ROOM MAP AT CAPACITY · ${omittedLocations} LOCATIONS NOT SHOWN`} width={layout.width - PADDING * 2} top={Math.min(...overflow.map((placement) => placement.y)) - 28} bottom={Math.max(...overflow.map((placement) => placement.y + 8)) + PADDING} />}

      {layout.rooms.length === 0 ? (
        <text x={layout.width / 2} y={resting.length === 0 ? 52 : Math.max(...resting.map((placement) => placement.y + 8)) + PADDING * 2} textAnchor="middle" fill="#7890a2" fontFamily="ui-monospace, monospace" fontSize="10">
          EMPTY FLOOR
        </text>
      ) : null}

      {placements.map((placement) => {
        const worker = workerById.get(placement.id);
        if (worker === undefined) return null;
        const room = placement.roomId === undefined ? undefined : nodes.get(placement.roomId);
        const location = worker.location === "working"
          ? placement.area === "room" ? `working near observed changes in ${room?.label}` : worker.nodeId === undefined ? `working near observed changes in ${worker.locationLabel}; room map at capacity` : `working near observed changes in ${worker.locationLabel}; worker area at capacity`
          : worker.location === "unobserved" ? "working; location not yet observed"
          : worker.location === "last-observed" && worker.locationLabel !== undefined ? `last observed near changes in ${worker.locationLabel}`
          : worker.paused ? "paused in resting area" : "ready in resting area";
        const frame = workerFrame(worker);
        return (
          <g
            key={worker.id}
            data-worker-id={worker.id}
            data-worker-location={worker.location ?? "resting"}
            transform={`translate(${placement.x} ${placement.y})`}
            role="img"
            aria-label={`${worker.name}, ${worker.role}, ${worker.activity}, ${location}`}
            className="dfFactoryScene__worker"
            {...(onSelectWorker === undefined ? {} : { onClick: () => onSelectWorker(worker.id), style: { cursor: "pointer" } })}
          >
            <title>{`${worker.name} · ${location}`}</title>
            <Frame name={frame} x={-8} y={-8} />
          </g>
        );
      })}

    </svg>
  );
}

function Area({ label, width, top, bottom }: { label: string; width: number; top: number; bottom: number }) {
  return (
    <g role="group" aria-label={label}>
      <rect x={PADDING} y={top} width={width} height={bottom - top} rx="3" fill="#101f2b" stroke="#385164" />
      <text x={PADDING + 6} y={top + 15} fill="#9db1be" fontFamily="ui-monospace, monospace" fontSize="8">{label}</text>
    </g>
  );
}
