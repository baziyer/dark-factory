import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import { copyFileSync, mkdtempSync, readFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { inflateSync } from "node:zlib";
import { join } from "node:path";
import test from "node:test";
import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { FactoryScene } from "../../dist/src/factory-scene/factory-scene.js";
import { PADDING, layoutScene, placeWorkers, workerFrame } from "../../dist/src/factory-scene/scene.js";
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
  { id: "worker-b", name: "Builder", role: "worker", provider: "codex", activity: "busy", location: "working", nodeId: "src" },
  { id: "worker-a", name: "Planner", role: "orchestrator", activity: "needs-you", nodeId: "missing" },
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
  return renderToStaticMarkup(createElement(FactoryScene, { topology, workers, ...props }));
}

test("the pure scene model feeds a deterministic SVG renderer", () => {
  const layout = layoutScene(topology);
  assert.deepEqual(layout, layoutScene({ ...topology, nodes: [...topology.nodes].reverse() }));
  assert.deepEqual(Object.keys(layout.rooms[0]).sort(), ["anchor", "height", "id", "width", "x", "y"]);
  assert.deepEqual(layout.rooms[0].anchor, {
    x: layout.rooms[0].x + layout.rooms[0].width / 2,
    y: layout.rooms[0].y + layout.rooms[0].height / 2,
  });

  for (const room of layout.rooms) {
    assert.equal(room.x % spriteAtlas.frame, 0, `room ${room.id} off the tile grid`);
    assert.equal(room.y % spriteAtlas.frame, 0, `room ${room.id} off the tile grid`);
  }

  const placements = placeWorkers(layout, workers);
  assert.deepEqual(placements, placeWorkers(layout, [...workers].reverse()));
  assert.equal(workerFrame(workers[0]), frameName(workers[0]));
  assert.equal(workerFrame(workers[1]), frameName(workers[1]));
  assert.equal(workerFrame({ ...workers[0], provider: "made_up" }), frameName({ ...workers[0], provider: "made_up" }));

  const first = render();
  const reordered = render({
    topology: { ...topology, nodes: [...topology.nodes].reverse() },
    workers: [...workers].reverse(),
  });
  assert.equal(first, reordered);
  assert.match(first, /data-topology-digest="fixture-1"/);
  for (const label of ["RESTING", "STAGED", "READY"]) assert.equal(first.includes(`>${label}</text>`), false, `${label} footer remains rendered`);
  assert.match(first, /data-room-id="src"/);
  // The room subtitle carries the served size bucket, and nothing when the
  // room stands for a project rather than a topology node.
  assert.match(first, />PACKAGE · MEDIUM</);
  assert.match(renderToStaticMarkup(createElement(FactoryScene, {
    topology: { digest: "d", nodes: [{ id: "p", parentId: "", path: "Project", label: "Project", kind: "repository" }] },
    workers: []
  })), />REPOSITORY<\/text>/);
  assert.match(first, /&lt;Shared &amp; Library…/);
  assert.equal(first.includes("�"), false);
  assert.equal((first.match(/data-worker-id=/g) ?? []).length, workers.length);
  assert.equal((first.match(/data-worker-location=/g) ?? []).length, workers.length);
  // The sheet is one <defs> entry, not one copy per worker, and every frame a
  // worker stands on is a real window on it.
  assert.equal(first.split(spriteSheet).length - 1, 1);
  assert.equal((first.match(/<symbol /g) ?? []).length, Object.keys(spriteAtlas.frames).length);
  for (const worker of workers) {
    const rendered = first.slice(first.indexOf(`data-worker-id="${worker.id}"`));
    const frame = rendered.slice(0, rendered.indexOf("</g>")).match(/href="#df-frame-([^"]+)"/g)
      .map((match) => match.slice('href="#df-frame-'.length, -1));
    assert.deepEqual(frame, [frameName(worker)]);
    for (const name of frame) assert.ok(name in spriteAtlas.frames, name);
  }
  assert.equal(first.includes("dfFactoryScene__alternate"), false);
  assert.equal(first.includes("<line"), false);
  assert.equal(first.includes("<animate"), false);
  const unobserved = render({ workers: [{ ...workers[0], location: "unobserved", nodeId: undefined }] });
  assert.match(unobserved, /WORKING · 1 LOCATION NOT YET OBSERVED/);
  assert.match(unobserved, /working; location not yet observed/);
  assert.match(first, /RESTING AREA · 1/);
  const restingY = Number(first.match(/data-worker-id="worker-a"[^>]*transform="translate\([^ ]+ ([0-9.]+)\)"/)[1]);
  const roomYs = [...first.matchAll(/data-room-id="[^"]+"[^>]*>[\s\S]*?<rect x="[^"]+" y="([0-9.]+)"/g)].map((match) => Number(match[1]));
  assert.ok(restingY + 24 < Math.min(...roomYs), "resting area stays above every room with clearance");
  for (const room of layoutScene(topology, 1).rooms) assert.equal(room.y % spriteAtlas.frame, 0, `resting offset moved ${room.id} off the tile grid`);
  const capped = render({ workers: [{ ...workers[0], location: "working", locationLabel: "Source", nodeId: undefined }], omittedLocations: 1 });
  assert.match(capped, /ROOM MAP AT CAPACITY · 1 LOCATIONS NOT SHOWN/);
  assert.match(capped, /working near observed changes in Source; room map at capacity/);
  const observed = render({ workers: [{ ...workers[1], location: "last-observed", locationLabel: "Source" }] });
  assert.match(observed, /last observed near changes in Source/);

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
  // Every topology tile the renderer needs stays available in the shared sheet.
  assert.deepEqual(
    Object.keys(spriteAtlas.frames).filter((name) => name.startsWith("tile.")).sort(),
    ["tile.door", "tile.floor.0", "tile.floor.1", "tile.wall"]);

  const denseWorkers = Array.from({ length: 100 }, (_, index) => ({
    id: `worker-${index}`,
    name: `Worker ${index}`,
    role: "worker",
    activity: "busy",
    location: "working",
    nodeId: "src",
  }));
  const densePlacements = placeWorkers(layout, denseWorkers);
  assert.equal(new Set(densePlacements.map(({ x, y }) => `${x},${y}`)).size, denseWorkers.length);
  const srcRoom = layout.rooms.find((room) => room.id === "src");
  assert.deepEqual(densePlacements[0], { id: "worker-0", area: "room", roomId: "src", x: srcRoom.anchor.x, y: srcRoom.y + 48 });
  for (const placement of densePlacements.filter(({ y }) => y < layout.height)) {
    assert.ok(placement.x - 8 >= srcRoom.x && placement.x + 8 <= srcRoom.x + srcRoom.width);
    assert.ok(placement.y - 8 >= srcRoom.y && placement.y + 8 <= srcRoom.y + srcRoom.height);
    assert.ok(placement.y - 8 >= srcRoom.y + 40, "room workers stay below the title and kind");
  }
  const denseSvg = render({ workers: denseWorkers });
  assert.match(denseSvg, /WORKER AREA AT CAPACITY · 84/);
  const denseHeight = Number(denseSvg.match(/viewBox="0 0 [^ ]+ ([^"]+)"/)[1]);
  assert.ok(denseHeight > Math.max(...densePlacements.map(({ y }) => y + 8)));

  const mixedPlacements = placeWorkers(layout, [
    ...denseWorkers,
    { ...workers[1], location: "resting" },
  ]);
  const restingBottom = Math.max(...mixedPlacements.filter((placement) => placement.area === "resting").map((placement) => placement.y + 8));
  const overflowTop = Math.min(...mixedPlacements.filter((placement) => placement.area === "overflow").map((placement) => placement.y));
  assert.ok(overflowTop - restingBottom >= 24, "resting and overflow areas have separate rows");
  const directOutside = placeWorkers(layout, [
    { ...workers[1], location: "resting" },
    { ...workers[0], location: "unobserved", nodeId: undefined },
  ]);
  const directRestingBottom = Math.max(...directOutside.filter((placement) => placement.area === "resting").map((placement) => placement.y + 8));
  const directStagingTop = Math.min(...directOutside.filter((placement) => placement.area === "staging").map((placement) => placement.y));
  assert.ok(directStagingTop - directRestingBottom >= 24, "direct placement keeps resting and unobserved workers apart");

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
  const emptyWorkers = denseWorkers.slice(0, 20).map(({ nodeId: _nodeId, location: _location, ...worker }) => ({ ...worker, location: "resting" }));
  const emptyPlacements = placeWorkers(emptyLayout, emptyWorkers);
  assert.equal(new Set(emptyPlacements.map(({ x, y }) => `${x},${y}`)).size, emptyWorkers.length);
  const emptySvg = render({ topology: { digest: "empty", nodes: [] }, workers: emptyWorkers });
  assert.match(emptySvg, /EMPTY FLOOR/);
  // An empty floor in a wide column stays a panel, not a poster.
  assert.match(emptySvg, new RegExp(`min-width:${Math.min(emptyLayout.width * 3, 864)}px`));
  assert.match(emptySvg, /aria-label="RESTING AREA · 20"/);
  const emptyArea = emptySvg.match(/aria-label="RESTING AREA · 20"><rect x="[^"]+" y="([0-9.]+)" width="[^"]+" height="([0-9.]+)"/);
  const emptyLabel = emptySvg.match(/<text x="[^"]+" y="([0-9.]+)"[^>]*>EMPTY FLOOR<\/text>/);
  const emptyHeight = Number(emptySvg.match(/viewBox="0 0 [^ ]+ ([0-9.]+)"/)[1]);
  assert.ok(emptyArea !== null && emptyLabel !== null);
  assert.ok(Number(emptyLabel[1]) > Number(emptyArea[1]) + Number(emptyArea[2]), "empty-floor label clears the resting area");
  assert.ok(emptyHeight > Number(emptyLabel[1]), "empty-floor label remains inside the scene");
  const emptyWithStaging = render({
    topology: { digest: "empty", nodes: [] },
    workers: [...emptyWorkers, { ...workers[0], location: "unobserved", nodeId: undefined }],
  });
  const stagingArea = emptyWithStaging.match(/aria-label="WORKING · 1 LOCATION NOT YET OBSERVED"><rect x="[^"]+" y="([0-9.]+)"/);
  const stagingLabel = emptyWithStaging.match(/<text x="[^"]+" y="([0-9.]+)"[^>]*>EMPTY FLOOR<\/text>/);
  assert.ok(stagingArea !== null && stagingLabel !== null);
  assert.ok(Number(stagingLabel[1]) + PADDING <= Number(stagingArea[1]), "empty-floor label clears the staging area");
});

test("rooms group under their project's heading and stay on the tile grid", () => {
  const grouped = {
    digest: "fixture-2",
    nodes: [
      { id: "b-root", parentId: "", path: ".", label: "Beta", kind: "repository", project: { id: "p-b", name: "Beta Works" } },
      { id: "a-web", parentId: "a-root", path: "web", label: "web", kind: "package", project: { id: "p-a", name: "Alpha Works" } },
      { id: "b-src", parentId: "b-root", path: "src", label: "src", kind: "directory", project: { id: "p-b", name: "Beta Works" } },
      { id: "a-root", parentId: "", path: ".", label: "Alpha", kind: "repository", project: { id: "p-a", name: "Alpha Works" } },
      { id: "a-cmd", parentId: "a-root", path: "cmd", label: "cmd", kind: "directory", project: { id: "p-a", name: "Alpha Works" } },
    ],
  };
  const layout = layoutScene(grouped);
  assert.deepEqual(layout, layoutScene({ ...grouped, nodes: [...grouped.nodes].reverse() }));
  assert.deepEqual(layout.rooms.map((room) => room.id), ["a-root", "a-cmd", "a-web", "b-root", "b-src"]);
  assert.deepEqual(layout.headings.map((heading) => heading.label), ["Alpha Works", "Beta Works"]);
  const [alpha, beta] = layout.headings;
  for (const room of layout.rooms.slice(0, 3)) assert.ok(room.y > alpha.y && room.y < beta.y, `${room.id} outside Alpha`);
  for (const room of layout.rooms.slice(3)) assert.ok(room.y > beta.y, `${room.id} outside Beta`);
  for (const room of layout.rooms) {
    assert.equal(room.x % spriteAtlas.frame, 0, `room ${room.id} off the tile grid`);
    assert.equal(room.y % spriteAtlas.frame, 0, `room ${room.id} off the tile grid`);
  }
  // Two projects with one name are still two blocks under two headings.
  const twins = layoutScene({ ...grouped, nodes: grouped.nodes.map((node) => ({ ...node, project: { id: node.project.id, name: "Twin" } })) });
  assert.deepEqual(twins.headings.map((heading) => heading.label), ["Twin", "Twin"]);
  assert.deepEqual(twins.rooms.map((room) => room.id), layout.rooms.map((room) => room.id));
  // The heading is its own element at the row the layout gave it, and every
  // door sits on the tile grid like the room it opens.
  const markup = renderToStaticMarkup(createElement(FactoryScene, { topology: grouped, workers: [] }));
  for (const heading of layout.headings) {
    assert.match(markup, new RegExp(`<text data-floor-heading="${heading.label}" x="${heading.x}" y="${heading.y + 11}"[^>]*>${heading.label}</text>`));
  }
  const doors = [...markup.matchAll(/href="#df-frame-tile\.door" x="([0-9.]+)"/g)].map((match) => Number(match[1]));
  assert.equal(doors.length, layout.rooms.length);
  for (const x of doors) assert.equal(x % spriteAtlas.frame, 0, `door at ${x} off the tile grid`);
  // Rooms without a project stand under no heading, as before.
  assert.deepEqual(layoutScene(topology).headings, []);
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
        }
      }
    }
  }
  const fallback = { id: idForIdentity(2), name: "Fallback", role: "worker", provider: "unknown", activity: "debugging" };
  assert.equal(workerFrame(fallback), "worker.shell.2.idle.0");
  reached.add(workerFrame(fallback));
  const personFrames = Object.keys(spriteAtlas.frames).filter((name) => /^(worker|overseer)\./.test(name) && name.endsWith(".0"));
  assert.deepEqual([...reached].sort(), personFrames.sort());
});

// Nothing else runs the generator, so the shipped module could drift from it.
// A PNG's pixels: its IHDR and its inflated scanlines. The deflate bytes
// themselves depend on the zlib a Node was built with, so two builds of the
// same image may not share a byte; they must share every pixel.
function pngPixels(bytes) {
  assert.deepEqual([...bytes.subarray(0, 8)], [137, 80, 78, 71, 13, 10, 26, 10]);
  const idats = [];
  let header;
  for (let offset = 8; offset < bytes.length;) {
    const length = bytes.readUInt32BE(offset);
    const type = bytes.toString("latin1", offset + 4, offset + 8);
    const data = bytes.subarray(offset + 8, offset + 8 + length);
    if (type === "IHDR") header = Buffer.from(data);
    if (type === "IDAT") idats.push(data);
    offset += 12 + length;
  }
  return Buffer.concat([header, inflateSync(Buffer.concat(idats))]);
}

// Text that embeds the sheet as a data URL compares by its text with every
// embedded PNG replaced, plus those PNGs' pixels in order.
function withPixels(text) {
  const pixels = [];
  const stripped = text.replaceAll(/data:image\/png;base64,([A-Za-z0-9+/=]+)/g, (_, base64) => {
    pixels.push(pngPixels(Buffer.from(base64, "base64")));
    return "data:image/png;base64,<pixels>";
  });
  return { stripped, pixels };
}

test("the committed sprite module is exactly what the generator writes", () => {
  const sprites = new URL("./sprites/", import.meta.url);
  const scratch = mkdtempSync(join(tmpdir(), "df-sprites-"));
  try {
    copyFileSync(new URL("gen-sprites.mjs", sprites), join(scratch, "gen-sprites.mjs"));
    // A failing generator reports its own assertion, not just a bad exit.
    execFileSync(process.execPath, ["gen-sprites.mjs"], { cwd: scratch, stdio: "pipe" });
    assert.deepEqual(pngPixels(readFileSync(join(scratch, "sprites.png"))), pngPixels(readFileSync(new URL("sprites.png", sprites))), "sprites.png");
    for (const name of ["sprites.generated.ts", "preview.html"]) {
      const fresh = withPixels(readFileSync(join(scratch, name), "utf8"));
      const committed = withPixels(readFileSync(new URL(name, sprites), "utf8"));
      assert.equal(fresh.stripped, committed.stripped, name);
      assert.deepEqual(fresh.pixels, committed.pixels, `${name} pixels`);
      assert.ok(fresh.pixels.length >= 1, `${name} embeds the sheet`);
    }
  } finally {
    rmSync(scratch, { recursive: true, force: true });
  }
});
