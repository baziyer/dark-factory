import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import { copyFileSync, mkdtempSync, readFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import test from "node:test";
import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { FactoryScene } from "../../dist/src/factory-scene/factory-scene.js";
import { alternateFrame, layoutScene, placeWorkers, workerFrame } from "../../dist/src/factory-scene/scene.js";
import { spriteAtlas, spriteSheet, spriteSheetSize } from "../../dist/src/factory-scene/sprites/sprites.generated.js";

const topology = {
  digest: "fixture-1",
  nodes: [
    { id: "repo", parentId: "", path: ".", label: "Repository", kind: "repository", sizeBucket: "large" },
    { id: "lib", parentId: "repo", path: "packages/lib", label: "<Shared & Library> 📦📦", kind: "package", sizeBucket: "medium" },
    { id: "src", parentId: "lib", path: "packages/lib/src", label: "Source", kind: "directory", sizeBucket: "small" },
  ],
};

const workers = [
  { id: "worker-b", name: "Builder", role: "worker", provider: "codex", activity: "busy", nodeId: "src" },
  { id: "worker-a", name: "Planner", role: "orchestrator", activity: "needs-you", nodeId: "missing" },
];

const workItems = [
  { id: "release", stage: "release-ready" },
  { id: "staged", stage: "staged" },
];

// Pinned outputs keep these tests independent of the hash implementation.
const pinnedIdentities = Object.freeze({
  "identity-2": 0,
  "identity-1": 1,
  "identity-0": 2,
  "identity-3": 3,
  "worker-a": 1,
  "worker-b": 0,
  "stable-worker": 3,
  "worker-😀": 3,
});

function identityFor(id) {
  const identity = pinnedIdentities[id];
  assert.notEqual(identity, undefined, `missing pinned identity for ${id}`);
  return identity;
}

function idForIdentity(identity) {
  const id = ["identity-2", "identity-1", "identity-0", "identity-3"][identity];
  assert.ok(id, `missing pinned id for identity ${identity}`);
  return id;
}

function frameIdentity(frame) {
  return Number(frame.split(".")[2]);
}

function frameName(worker) {
  const role = worker.role === "orchestrator" ? "overseer" : "worker";
  const provider = worker.provider === "claude_code" || worker.provider === "codex" ? worker.provider : "shell";
  const activity = ["busy", "waiting", "needs-you", "idle"].includes(worker.activity) ? worker.activity : "idle";
  return `${role}.${provider}.${identityFor(worker.id)}.${activity}.0`;
}

function render(props = {}) {
  return renderToStaticMarkup(createElement(FactoryScene, { topology, workers, workItems, ...props }));
}

test("the pure scene model feeds a deterministic SVG renderer", () => {
  const layout = layoutScene(topology);
  assert.deepEqual(layout, layoutScene({ ...topology, nodes: [...topology.nodes].reverse() }));
  assert.deepEqual(Object.keys(layout.rooms[0]).sort(), ["anchor", "height", "id", "width", "x", "y"]);
  assert.deepEqual(layout.rooms[0].anchor, {
    x: layout.rooms[0].x + layout.rooms[0].width / 2,
    y: layout.rooms[0].y + layout.rooms[0].height / 2,
  });

  for (const room of layout.rooms) assert.equal(room.y % spriteAtlas.frame, 0, `room ${room.id} off the tile grid`);

  const placements = placeWorkers(layout, workers);
  assert.deepEqual(placements, placeWorkers(layout, [...workers].reverse()));
  assert.equal(workerFrame(workers[0]), frameName(workers[0]));
  assert.equal(workerFrame(workers[1]), frameName(workers[1]));
  assert.equal(workerFrame({ ...workers[0], provider: "made_up" }), frameName({ ...workers[0], provider: "made_up" }));
  assert.equal(alternateFrame(frameName({ ...workers[0], activity: "busy" })), `${frameName(workers[0]).replace("busy.0", "busy.1")}`);
  assert.equal(alternateFrame(frameName({ ...workers[1], activity: "needs-you" })), undefined);

  const first = render();
  const reordered = render({
    topology: { ...topology, nodes: [...topology.nodes].reverse() },
    workers: [...workers].reverse(),
    workItems: [...workItems].reverse(),
  });
  assert.equal(first, reordered);
  assert.match(first, /data-topology-digest="fixture-1"/);
  assert.match(first, /data-room-id="src"/);
  // The room subtitle carries the served size bucket, and nothing when the
  // room stands for a project rather than a topology node.
  assert.match(first, />PACKAGE · MEDIUM</);
  assert.match(renderToStaticMarkup(createElement(FactoryScene, {
    topology: { digest: "d", nodes: [{ id: "p", parentId: "", path: "Project", label: "Project", kind: "repository" }] },
    workers: [],
    workItems: [],
  })), />REPOSITORY<\/text>/);
  assert.match(first, /&lt;Shared &amp; Library…/);
  assert.equal(first.includes("�"), false);
  assert.equal((first.match(/data-worker-id=/g) ?? []).length, workers.length);
  assert.equal((first.match(/data-worker-id="[^"]+" transform="translate\([^)]+\)"/g) ?? []).length, workers.length);
  // The sheet is one <defs> entry, not one copy per worker, and every frame a
  // worker stands on is a real window on it.
  assert.equal(first.split(spriteSheet).length - 1, 1);
  assert.equal((first.match(/<symbol /g) ?? []).length, Object.keys(spriteAtlas.frames).length);
  for (const worker of workers) {
    const rendered = first.slice(first.indexOf(`data-worker-id="${worker.id}"`));
    const frame = rendered.slice(0, rendered.indexOf("</g>")).match(/href="#df-frame-([^"]+)"/g)
      .map((match) => match.slice('href="#df-frame-'.length, -1));
    assert.deepEqual(frame, [frameName(worker)]
      .concat(worker.activity === "busy" || worker.activity === "idle" ? [`${frameName(worker).replace(`${worker.activity}.0`, `${worker.activity}.1`)}`] : []));
    for (const name of frame) assert.ok(name in spriteAtlas.frames, name);
    if (frame.length === 2) {
      assert.match(rendered, /<use[^>]+class="dfFactoryScene__primary"/);
      assert.match(rendered, /<use[^>]+class="dfFactoryScene__alternate"/);
    }
  }
  // Only a two-frame activity animates, and it does so in CSS so that the
  // reduced-motion rule can stop it.
  assert.equal((first.match(/dfFactoryScene__alternate/g) ?? []).length,
    workers.filter((worker) => worker.activity === "busy" || worker.activity === "idle").length);
  assert.equal(first.includes("<line"), false);
  assert.equal(first.includes("<animate"), false);

  // The sheet the renderer reads is every frame it can draw, on a 16px grid,
  // inside the size the generator wrote next to it.
  assert.equal(Object.keys(spriteAtlas.frames).length, 151);
  assert.equal(spriteAtlas.frame, 16);
  assert.deepEqual(spriteSheetSize, { width: 128, height: 304 });
  assert.match(first, new RegExp(`width="${spriteSheetSize.width}" height="${spriteSheetSize.height}"`));
  for (const [name, cell] of Object.entries(spriteAtlas.frames)) {
    assert.ok(cell.x % 16 === 0 && cell.y % 16 === 0, name);
    assert.ok(cell.x >= 0 && cell.y >= 0 && cell.x + 16 <= spriteSheetSize.width && cell.y + 16 <= spriteSheetSize.height, name);
  }
  // Every packed frame is one a render can reach, so no symbol is dead weight.
  assert.deepEqual(
    Object.keys(spriteAtlas.frames).filter((name) => name.startsWith("tile.") || name.startsWith("bay.")).sort(),
    ["bay.free", "bay.ready", "bay.staged", "tile.door", "tile.floor.0", "tile.floor.1", "tile.wall"]);

  const denseWorkers = Array.from({ length: 100 }, (_, index) => ({
    id: `worker-${index}`,
    name: `Worker ${index}`,
    role: "worker",
    activity: "busy",
    nodeId: "src",
  }));
  const densePlacements = placeWorkers(layout, denseWorkers);
  assert.equal(new Set(densePlacements.map(({ x, y }) => `${x},${y}`)).size, denseWorkers.length);
  const srcRoom = layout.rooms.find((room) => room.id === "src");
  assert.deepEqual(densePlacements[0], { id: "worker-0", roomId: "src", x: srcRoom.anchor.x, y: srcRoom.anchor.y });
  for (const placement of densePlacements.filter(({ y }) => y < layout.height)) {
    assert.ok(placement.x - 8 >= srcRoom.x && placement.x + 8 <= srcRoom.x + srcRoom.width);
    assert.ok(placement.y - 8 >= srcRoom.y && placement.y + 8 <= srcRoom.y + srcRoom.height);
  }
  const denseSvg = render({ workers: denseWorkers });
  assert.match(denseSvg, /WORKER OVERFLOW · 72/);
  const denseHeight = Number(denseSvg.match(/viewBox="0 0 [^ ]+ ([^"]+)"/)[1]);
  assert.ok(denseHeight > Math.max(...densePlacements.map(({ y }) => y + 8)));

  const stackedLayout = {
    width: 176,
    height: 256,
    rooms: [
      { id: "top", x: 12, y: 40, width: 152, height: 96, anchor: { x: 88, y: 88 } },
      { id: "bottom", x: 12, y: 148, width: 152, height: 96, anchor: { x: 88, y: 196 } },
    ],
  };
  const stackedWorkers = Array.from({ length: 100 }, (_, index) => ({
    id: `stacked-${index}`,
    name: `Stacked ${index}`,
    role: "worker",
    activity: "idle",
    nodeId: index < 50 ? "top" : "bottom",
  }));
  const stackedPlacements = placeWorkers(stackedLayout, stackedWorkers);
  assert.equal(new Set(stackedPlacements.map(({ x, y }) => `${x},${y}`)).size, stackedWorkers.length);

  const changed = layoutScene({
    digest: "fixture-2",
    nodes: [...topology.nodes, { id: "docs", parentId: "repo", path: "docs", label: "Docs", kind: "directory", sizeBucket: "tiny" }],
  });
  assert.equal(changed.rooms.some((room) => room.id === "docs"), true);
  assert.notDeepEqual(changed, layout);

  const emptyLayout = layoutScene({ digest: "empty", nodes: [] });
  const emptyWorkers = denseWorkers.slice(0, 20).map(({ nodeId: _nodeId, ...worker }) => worker);
  const emptyPlacements = placeWorkers(emptyLayout, emptyWorkers);
  assert.equal(new Set(emptyPlacements.map(({ x, y }) => `${x},${y}`)).size, emptyWorkers.length);
  const emptySvg = render({ topology: { digest: "empty", nodes: [] }, workers: emptyWorkers, workItems: [] });
  assert.match(emptySvg, /EMPTY FLOOR/);
  // An empty floor in a wide column stays a panel, not a poster.
  assert.match(emptySvg, new RegExp(`max-width:${emptyLayout.width * 3}px`));
  assert.match(emptySvg, /aria-label="20 unassigned workers"/);
});

test("worker identity is stable while operational state changes", () => {
  const base = { id: "stable-worker", name: "Builder", role: "worker", provider: "codex", activity: "busy", nodeId: "src" };
  const stableIdentity = 3;
  const variants = [
    { ...base, name: "Renamed", nodeId: "repo" },
    { ...base, activity: "waiting" },
    { ...base, provider: "claude_code" },
    { ...base, role: "orchestrator" },
    { ...base, provider: "made_up", activity: "unknown" },
  ];
  for (const worker of variants) assert.equal(frameIdentity(workerFrame(worker)), stableIdentity);
  assert.equal(workerFrame({ ...base, id: "worker-😀" }), "worker.codex.3.busy.0");
});

test("every identity and operational frame is reachable, including fallbacks", () => {
  const reached = new Set();
  for (const role of ["worker", "orchestrator"]) {
    for (const provider of ["claude_code", "codex", "shell"]) {
      for (const activity of ["busy", "waiting", "needs-you", "idle"]) {
        for (let identity = 0; identity < 4; identity++) {
          const worker = { id: idForIdentity(identity), name: "Agent", role, provider, activity };
          const frame = workerFrame(worker);
          reached.add(frame);
          const alternate = alternateFrame(frame);
          if (alternate !== undefined) reached.add(alternate);
        }
      }
    }
  }
  const fallback = { id: idForIdentity(2), name: "Fallback", role: "worker", provider: "unknown", activity: "debugging" };
  assert.equal(workerFrame(fallback), "worker.shell.2.idle.0");
  reached.add(workerFrame(fallback));
  reached.add(alternateFrame(workerFrame({ ...fallback, activity: "busy" })));
  const personFrames = Object.keys(spriteAtlas.frames).filter((name) => /^(worker|overseer)\./.test(name));
  assert.deepEqual([...reached].sort(), personFrames.sort());
});

// Nothing else runs the generator, so the shipped module could drift from it.
test("the committed sprite module is exactly what the generator writes", () => {
  const sprites = new URL("./sprites/", import.meta.url);
  const scratch = mkdtempSync(join(tmpdir(), "df-sprites-"));
  try {
    copyFileSync(new URL("gen-sprites.mjs", sprites), join(scratch, "gen-sprites.mjs"));
    execFileSync(process.execPath, ["gen-sprites.mjs"], { cwd: scratch, stdio: "ignore" });
    for (const name of ["sprites.png", "sprites.generated.ts", "preview.html"]) {
      assert.deepEqual(readFileSync(join(scratch, name)), readFileSync(new URL(name, sprites)), name);
    }
  } finally {
    rmSync(scratch, { recursive: true, force: true });
  }
});
