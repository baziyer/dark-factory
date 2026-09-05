import assert from "node:assert/strict";
import test from "node:test";
import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { FactoryScene } from "../../dist/src/factory-scene/factory-scene.js";
import { alternateFrame, layoutScene, placeWorkers, workerFrame } from "../../dist/src/factory-scene/scene.js";
import { spriteAtlas, spriteSheet } from "../../dist/src/factory-scene/sprites/sprites.generated.js";

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

  const placements = placeWorkers(layout, workers);
  assert.deepEqual(placements, placeWorkers(layout, [...workers].reverse()));
  assert.equal(workerFrame(workers[0]), "worker.codex.busy.0");
  assert.equal(workerFrame(workers[1]), "overseer.shell.needs-you.0");
  assert.equal(workerFrame({ ...workers[0], provider: "made_up" }), "worker.shell.busy.0");
  assert.equal(alternateFrame("worker.codex.busy.0"), "worker.codex.busy.1");
  assert.equal(alternateFrame("overseer.shell.needs-you.0"), undefined);

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
    const role = worker.role === "orchestrator" ? "overseer" : "worker";
    assert.deepEqual(frame, [`${role}.${worker.provider ?? "shell"}.${worker.activity}.0`]
      .concat(worker.activity === "busy" || worker.activity === "idle" ? [`${role}.${worker.provider ?? "shell"}.${worker.activity}.1`] : []));
    for (const name of frame) assert.ok(name in spriteAtlas.frames, name);
  }
  // Only a two-frame activity animates, and it does so in CSS so that the
  // reduced-motion rule can stop it.
  assert.equal((first.match(/dfFactoryScene__alternate/g) ?? []).length,
    workers.filter((worker) => worker.activity === "busy" || worker.activity === "idle").length);
  assert.equal(first.includes("<line"), false);
  assert.equal(first.includes("<animate"), false);

  // The sheet the renderer reads is 47 frames on a 16px grid inside 128 × 96.
  assert.equal(Object.keys(spriteAtlas.frames).length, 47);
  assert.equal(spriteAtlas.frame, 16);
  for (const [name, cell] of Object.entries(spriteAtlas.frames)) {
    assert.ok(cell.x % 16 === 0 && cell.y % 16 === 0, name);
    assert.ok(cell.x >= 0 && cell.y >= 0 && cell.x + 16 <= 128 && cell.y + 16 <= 96, name);
  }

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
  assert.match(emptySvg, /aria-label="20 unassigned workers"/);
});
