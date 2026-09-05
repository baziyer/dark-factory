import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import test from "node:test";
import {
  BROWSER_MANIFEST,
  MAX_AGENT_MODEL_BYTES,
  MAX_CONTROL_BYTES,
  MAX_HUMAN_QUESTION_BYTES,
  MAX_SNAPSHOT_BYTES,
  MAX_SNAPSHOT_ENTITIES,
  MAX_SQLITE_INTEGER,
  ProtocolError,
  decodeClientControl,
  decodeServerControl,
  encodeClientControl,
  encodeServerControl,
  encodeStateChanged,
  encodeStateGet,
  encodeStateSnapshot,
  encodeStateWatch,
  snapshotView,
} from "../dist/src/index.js";

const root = join(dirname(fileURLToPath(import.meta.url)), "../../../..");
const fixture = (name) => readFileSync(join(root, "protocol/browser/fixtures", name), "utf8").trim();
const manifest = JSON.parse(fixture("../manifest.json"));
const expectMalformed = (operation) => assert.throws(operation, (error) => error instanceof ProtocolError && ["malformed", "wrong_direction", "oversized"].includes(error.code));
// The contract tolerates additive change: a member this build does not know is
// ignored on the wire. Proving it is only additive means showing the mutated
// frame decodes to exactly the frame the unmutated one decodes to.
const expectIgnoredMember = (base, mutated, role = "client") => {
  const decode = role === "client" ? decodeClientControl : decodeServerControl;
  assert.deepEqual(decode(mutated), decode(base));
};
const ids = {
  project: "01010101010101010101010101010101",
  agent: "02020202020202020202020202020202",
  task: "03030303030303030303030303030303",
  request: "04040404040404040404040404040404",
};
const factoryItem = (revision = 1n) => ({ dispatch_enabled: true, capacity: 8, active_runs: 2, revision });
const projectItem = (revision = 1n) => ({ id: ids.project, name: "Factory", revision });
const agentItem = (revision = 1n) => ({ id: ids.agent, project_id: ids.project, name: "Worker", role: "worker", provider: "claude_code", paused: false, model: "claude-opus-5", reasoning_effort: "high", revision });
const taskItem = (revision = 1n, title = "Ship") => ({ id: ids.task, project_id: ids.project, assigned_agent_id: ids.agent, title, status: "queued", priority: 1, revision });
const requestItem = (revision = 1n) => ({ id: ids.request, project_id: ids.project, agent_id: ids.agent, task_id: ids.task, created_at: 10n, updated_at: 11n, revision, kind: "question", status: "open", reply_max_bytes: 8192, can_reply: true });
const snapshotBody = (overrides = {}) => ({
  head: 1n, factory: factoryItem(),
  projects: [projectItem()], agents: [agentItem()], tasks: [taskItem()], human_requests: [requestItem()],
  ...overrides,
});
const rawID = (value) => BigInt(value).toString(16).padStart(32, "0");

test("every Go control fixture is consumed by the matching closed TypeScript direction", () => {
  for (const entry of manifest.control) {
    const wire = fixture(entry.fixture);
    if (entry.direction === "client") assert.equal(encodeClientControl(decodeClientControl(wire)), wire, entry.type);
    else if (entry.direction === "server") assert.equal(encodeServerControl(decodeServerControl(wire)), wire, entry.type);
    else {
      assert.equal(encodeClientControl(decodeClientControl(wire)), wire, `${entry.type}/client`);
      assert.equal(encodeServerControl(decodeServerControl(wire)), wire, `${entry.type}/server`);
    }
  }
  assert.equal(typeof decodeServerControl(fixture("state_snapshot.json")).body.head, "bigint");
  assert.equal(decodeServerControl(fixture("state_snapshot.json")).body.head, 9_007_199_254_740_993n);
});

test("the Go-produced snapshot fixture becomes one complete TypeScript state view", () => {
  const body = decodeServerControl(fixture("state_snapshot.json")).body;
  const view = snapshotView(body);
  assert.equal(view.head, 9_007_199_254_740_993n);
  assert.equal(view.factory.dispatch_enabled, true);
  assert.equal(view.factory.capacity, 8);
  // The fixture is the cross-language contract: it carries one of every kind.
  assert.equal(view.projects.size, 1);
  assert.equal(view.agents.size, 1);
  assert.equal(view.tasks.size, 1);
  assert.equal(view.humanRequests.size, 1);
  const [project] = view.projects.values();
  const [agent] = view.agents.values();
  const [task] = view.tasks.values();
  const [request] = view.humanRequests.values();
  assert.equal(view.projects.get(project.id), project);
  assert.equal(agent.project_id, project.id);
  assert.equal(task.assigned_agent_id, agent.id);
  assert.equal(request.task_id, task.id);
  assert.equal(typeof task.revision, "bigint");
  // The view is immutable: readers cannot mutate one another's snapshot.
  assert.throws(() => { task.title = "mutated"; }, TypeError);
  assert.throws(() => { view.head = 0n; }, TypeError);
});

test("STATE_GET carries no selector and STATE_CHANGED carries only a head", () => {
  const request = encodeStateGet("state", {});
  assert.equal(request, `{"type":"STATE_GET","id":"state","body":{}}`);
  assert.deepEqual(decodeClientControl(request).body, {});
  // STATE_GET has no selector member, so none of these reaches a field.
  for (const body of ['{"cursor":null}', '{"kind":"task"}', '{"after":"1"}']) {
    expectIgnoredMember(request, `{"type":"STATE_GET","id":"state","body":${body}}`);
  }
  const changed = encodeStateChanged("watch", { head: 9n });
  assert.equal(changed, `{"type":"STATE_CHANGED","id":"watch","body":{"head":"9"}}`);
  assert.equal(decodeServerControl(changed).body.head, 9n);
  expectMalformed(() => encodeStateChanged("watch", { head: 0n }));
  // Residue from the deleted per-entity taxonomy reaches no field.
  for (const body of ['{"head":"9","entity_kind":"task"}', '{"head":"9","revision":"2"}', '{"head":"9","deleted":false}']) {
    expectIgnoredMember(changed, `{"type":"STATE_CHANGED","id":"watch","body":${body}}`, "server");
  }
  // The head itself stays required.
  expectMalformed(() => decodeServerControl(`{"type":"STATE_CHANGED","id":"watch","body":{}}`));
});

test("retired state frames are refused and the deleted envelope generation is ignored", () => {
  // The envelope has no generation to refuse: an older console's `v` member is
  // an unknown member like any other, so it is ignored.
  assert.deepEqual(
    decodeClientControl('{"v":1,"type":"STATE_GET","id":"state","body":{"cursor":null}}'),
    decodeClientControl(encodeStateGet("state", {})),
  );
  for (const type of ["STATE_SUBSCRIBE", "STATE_ENTITY_GET"]) {
    expectMalformed(() => decodeClientControl(`{"type":"${type}","id":"x","body":{}}`));
  }
  for (const type of ["STATE_RESTART", "STATE_EVENT", "STATE_ENTITY"]) {
    expectMalformed(() => decodeServerControl(`{"type":"${type}","id":"x","body":{}}`));
  }
});

test("decimal chronology uses canonical strings and bigint across unsafe boundaries", () => {
  for (const value of [0n, 1n, 9_007_199_254_740_991n, 9_007_199_254_740_992n, 9_007_199_254_740_993n, MAX_SQLITE_INTEGER]) {
    const wire = encodeStateWatch("watch", { after_head: value });
    assert.match(wire, new RegExp(`"after_head":"${value}"`));
    assert.equal(decodeClientControl(wire).body.after_head, value);
  }
  for (const value of ["", "+1", "-1", "-0", "00", "01", " 1", "1 ", "9223372036854775808", "1.0", "1e0"]) {
    expectMalformed(() => decodeClientControl(`{"type":"STATE_WATCH","id":"watch","body":{"after_head":${JSON.stringify(value)}}}`));
  }
  for (const value of ["0", "1", "9007199254740991", "9007199254740992", "9223372036854775807", "-1"]) {
    expectMalformed(() => decodeClientControl(`{"type":"STATE_WATCH","id":"watch","body":{"after_head":${value}}}`));
  }
  expectMalformed(() => encodeStateWatch("watch", { after_head: MAX_SQLITE_INTEGER + 1n }));
  expectMalformed(() => encodeStateWatch("watch", { after_head: 1 }));
});

test("the snapshot entity bound is exact, fails closed, and never truncates", () => {
  const fill = (total) => snapshotBody({ projects: Array.from({ length: total }, (_, index) => ({ ...projectItem(), id: rawID(index + 1) })), agents: [], tasks: [], human_requests: [] });
  const maximum = encodeStateSnapshot("state", fill(MAX_SNAPSHOT_ENTITIES - 1));
  assert.equal(decodeServerControl(maximum).body.projects.length, MAX_SNAPSHOT_ENTITIES - 1);
  expectMalformed(() => encodeStateSnapshot("state", fill(MAX_SNAPSHOT_ENTITIES)));
  // The bound counts every collection plus the factory together.
  const half = MAX_SNAPSHOT_ENTITIES / 2;
  expectMalformed(() => encodeStateSnapshot("state", snapshotBody({
    projects: Array.from({ length: half }, (_, index) => ({ ...projectItem(), id: rawID(index + 1) })),
    agents: Array.from({ length: half }, (_, index) => ({ ...agentItem(), id: rawID(index + 1 + half) })),
    tasks: [], human_requests: [],
  })));
  // A duplicate identity would let a server hide one entity behind another.
  expectMalformed(() => encodeStateSnapshot("state", snapshotBody({ projects: [projectItem(), projectItem()] })));
  expectMalformed(() => encodeStateSnapshot("state", snapshotBody({ tasks: [taskItem(), taskItem()] })));
});

test("only a server snapshot may exceed the control bound, and never the snapshot bound", () => {
  assert.equal(MAX_CONTROL_BYTES, 64 * 1024);
  assert.equal(MAX_SNAPSHOT_BYTES, 1024 * 1024);
  const big = snapshotBody({
    tasks: Array.from({ length: 2048 }, (_, index) => ({ ...taskItem(), id: rawID(index + 1), title: "t".repeat(1024) })),
    projects: [], agents: [], human_requests: [],
  });
  assert.throws(() => encodeStateSnapshot("state", big), (error) => error instanceof ProtocolError && error.code === "oversized");
  const ordinary = encodeStateSnapshot("state", snapshotBody());
  assert.ok(Buffer.byteLength(ordinary) < MAX_CONTROL_BYTES);
  expectMalformed(() => decodeServerControl(new Uint8Array(MAX_SNAPSHOT_BYTES + 1).fill(0x20)));
  expectMalformed(() => decodeClientControl(new Uint8Array(MAX_CONTROL_BYTES + 1).fill(0x20)));
});

test("every JSON boolean rejects null exactly", () => {
  const wire = encodeStateSnapshot("boolean", snapshotBody());
  const samples = [
    [wire, '"dispatch_enabled":true'],
    [wire, '"paused":false'],
    [wire, '"can_reply":true'],
    [encodeServerControl({ type: "ERROR", id: "boolean", body: { code: "invalid_request", retryable: false } }), '"retryable":false'],
  ];
  for (const [frame, field] of samples) expectMalformed(() => decodeServerControl(frame.replace(field, field.replace(/(true|false)$/, "null"))));
});

test("the agent provider is exact on the wire in both roles", () => {
  // Go pins this with a field census and an enum loop; without the same
  // negatives here a one-line loosening of agentItem would ship green.
  const wire = encodeStateSnapshot("provider", snapshotBody());
  assert.equal(wire.includes('"provider":"claude_code"'), true);
  for (const provider of ["claude_code", "codex", "shell"]) {
    const encoded = encodeStateSnapshot("provider", snapshotBody({ agents: [{ ...agentItem(), provider }] }));
    assert.equal(decodeServerControl(encoded).body.agents[0].provider, provider);
    expectMalformed(() => decodeClientControl(encoded));
  }
  for (const bad of ["gemini", "Claude_Code", "CODEX", "", "claude code"]) {
    expectMalformed(() => encodeStateSnapshot("provider", snapshotBody({ agents: [{ ...agentItem(), provider: bad }] })));
    expectMalformed(() => decodeServerControl(wire.replace('"provider":"claude_code"', `"provider":${JSON.stringify(bad)}`)));
  }
  for (const bad of [null, undefined, 7, true, ["shell"], { provider: "shell" }]) {
    expectMalformed(() => encodeStateSnapshot("provider", snapshotBody({ agents: [{ ...agentItem(), provider: bad }] })));
  }
  for (const raw of ["null", "7", "true", '["shell"]']) {
    expectMalformed(() => decodeServerControl(wire.replace('"provider":"claude_code"', `"provider":${raw}`)));
  }
  const { provider: _dropped, ...without } = agentItem();
  expectMalformed(() => encodeStateSnapshot("provider", snapshotBody({ agents: [without] })));
  expectMalformed(() => decodeServerControl(wire.replace('"provider":"claude_code",', "")));
  // The launch controls beside the provider are served facts, but still bounded.
  assert.equal(decodeServerControl(encodeStateSnapshot("provider", snapshotBody({ agents: [{ ...agentItem(), model: "sonnet" }] }))).body.agents[0].model, "sonnet");
  expectMalformed(() => encodeStateSnapshot("provider", snapshotBody({ agents: [{ ...agentItem(), model: "m".repeat(MAX_AGENT_MODEL_BYTES + 1) }] })));
  // A private agent field added beside the provider reaches no field of the
  // decoded item, so the UI can never read it.
  expectIgnoredMember(wire, wire.replace('"provider":"claude_code"', '"provider":"claude_code","tool_budget_limit":9'), "server");
});

test("an older daemon's agent reads back as unset launch controls, never as absent", () => {
  // The console renders these two directly, so the view must never hand it
  // undefined: a daemon that predates the contract simply has them unset.
  const wire = encodeStateSnapshot("older", snapshotBody())
    .replace('"model":"claude-opus-5","reasoning_effort":"high",', "");
  assert.equal(wire.includes("model"), false);
  const agent = snapshotView(decodeServerControl(wire).body).agents.get(ids.agent);
  assert.equal(agent.model, "");
  assert.equal(agent.reasoning_effort, "");
  assert.equal(snapshotView(decodeServerControl(encodeStateSnapshot("newer", snapshotBody())).body).agents.get(ids.agent).reasoning_effort, "high");
});

test("public state cannot carry private fields and detail is separately bounded", () => {
  const wire = encodeStateSnapshot("state", snapshotBody());
  for (const field of ["run_id", "question", "reply", "terminal_target", "cancel_run", "action", "project_name", "agent_name", "task_title", "summary", "why_human_needed", "root", "instruction"]) {
    assert.equal(wire.includes(`"${field}":`), false, field);
  }
  // The agent's launch controls are served, not private.
  for (const field of ["model", "reasoning_effort"]) assert.equal(wire.includes(`"${field}":`), true, field);
  expectMalformed(() => encodeStateSnapshot("state", snapshotBody({ human_requests: [{ ...requestItem(), question: "private" }] })));
  expectMalformed(() => encodeStateSnapshot("state", snapshotBody({ projects: [{ ...projectItem(), root: "/private" }] })));
  expectMalformed(() => encodeStateSnapshot("state", snapshotBody({ tasks: [{ ...taskItem(), body: "private" }] })));
  const detail = { type: "HUMAN_REQUEST_DETAIL", id: "detail", body: { request_id: ids.request, revision: 1n, question: "\0".repeat(MAX_HUMAN_QUESTION_BYTES), can_reply: false, reply_max_bytes: 8192, terminal_target: null, cancel_run: null } };
  const detailWire = encodeServerControl(detail);
  assert.ok(Buffer.byteLength(detailWire) < MAX_CONTROL_BYTES);
  assert.ok(Buffer.byteLength(detailWire) > 49_000);
  expectMalformed(() => encodeServerControl({ ...detail, body: { ...detail.body, question: "" } }));
  expectMalformed(() => encodeServerControl({ ...detail, body: { ...detail.body, question: "x".repeat(MAX_HUMAN_QUESTION_BYTES + 1) } }));
});

test("state parsing rejects case-folded/duplicate/unknown/trailing/depth/member/array/UTF-8 violations", () => {
  const valid = fixture("state_snapshot.json");
  for (const wire of [
    valid.replace('"head"', '"Head"'),
    valid.replace('"factory"', '"Factory"'),
    valid.replace('"title"', '"TITLE"'),
    valid.replace('"head":"9007199254740993"', '"head":"9007199254740993","head":"2"'),
    `${valid}{}`,
  ]) expectMalformed(() => decodeServerControl(wire));
  expectIgnoredMember(valid, valid.replace('"title"', '"extra":false,"title"'), "server");
  const members = Array.from({ length: 33 }, (_, index) => `"x${index}":0`).join(",");
  expectMalformed(() => decodeClientControl(`{"type":"STATE_WATCH","id":"watch","body":{${members}}}`));
  // A client frame keeps the small array bound; only the server snapshot may
  // carry a large array, and even that stops at the entity bound.
  const clientArray = Array.from({ length: 33 }, () => "0").join(",");
  expectMalformed(() => decodeClientControl(`{"type":"STATE_WATCH","id":"watch","body":{"after_head":[${clientArray}]}}`));
  const serverArray = Array.from({ length: MAX_SNAPSHOT_ENTITIES + 1 }, () => "{}").join(",");
  expectMalformed(() => decodeServerControl(`{"type":"STATE_SNAPSHOT","id":"state","body":{"head":"1","factory":{},"projects":[${serverArray}],"agents":[],"tasks":[],"human_requests":[]}}`));
  const nested = "[".repeat(18) + "0" + "]".repeat(18);
  expectMalformed(() => decodeClientControl(`{"type":"STATE_WATCH","id":"watch","body":{"after_head":"1","x":${nested}}}`));
  expectMalformed(() => decodeServerControl(new Uint8Array([0xff])));
});

test("manifest bounds and registry are an exact readable mirror", () => {
  assert.equal(BROWSER_MANIFEST.name, manifest.name);
  assert.deepEqual(BROWSER_MANIFEST.control, manifest.control);
  assert.deepEqual(BROWSER_MANIFEST.bounds, {
    maxControlBytes: manifest.bounds.max_control_bytes,
    maxJSONDepth: manifest.bounds.max_json_depth,
    maxArrayItems: manifest.bounds.max_array_items,
    maxObjectMembers: manifest.bounds.max_object_members,
    maxSnapshotBytes: manifest.bounds.max_snapshot_bytes,
    maxSnapshotEntities: manifest.bounds.max_snapshot_entities,
    maxProjectNameBytes: manifest.bounds.max_project_name_bytes,
    maxAgentNameBytes: manifest.bounds.max_agent_name_bytes,
    maxTaskTitleBytes: manifest.bounds.max_task_title_bytes,
    maxHumanQuestionBytes: manifest.bounds.max_human_question_bytes,
    maxHumanReplyBytes: manifest.bounds.max_human_reply_bytes,
    maxTaskInstructionBytes: manifest.bounds.max_task_instruction_bytes,
    maxFactoryCapacity: manifest.bounds.max_factory_capacity,
    maxTaskPriority: manifest.bounds.max_task_priority,
    maxSQLiteInteger: BigInt(manifest.bounds.max_sqlite_integer),
    maxTerminalUnackedBytes: manifest.bounds.max_terminal_unacked_bytes,
    terminalAckTimeoutMs: manifest.bounds.terminal_ack_timeout_ms,
    terminalLeaseRenewIntervalMs: manifest.bounds.terminal_lease_renew_interval_ms,
    maxTerminalRows: manifest.bounds.max_terminal_rows,
    maxTerminalCols: manifest.bounds.max_terminal_cols,
    maxAgentModelBytes: manifest.bounds.max_agent_model_bytes,
    maxRemoteInviteLinkBytes: manifest.bounds.max_remote_invite_link_bytes,
    maxRemoteInviteSvgBytes: manifest.bounds.max_remote_invite_svg_bytes,
  });
});

test("the client tolerates added members but nothing else", () => {
  // A newer daemon may add a member to any frame without a coordinated
  // release. STATE_SNAPSHOT and HELLO cover both the largest server frame and
  // the first frame of a session.
  const snapshot = encodeStateSnapshot("state", snapshotBody());
  expectIgnoredMember(snapshot, snapshot.replace('"head":"1"', '"head":"1","future_head":"2"'), "server");
  expectIgnoredMember(snapshot, snapshot.replace('"body":{', '"future":{"nested":[1,2,3]},"body":{'), "server");
  expectIgnoredMember(snapshot, snapshot.replace('"capacity":8', '"capacity":8,"future_capacity":9'), "server");
  const hello = fixture("hello.json");
  expectIgnoredMember(hello, hello.replace('"body":{', '"future":1,"body":{'), "server");
  expectIgnoredMember(hello, hello.replace('"daemon_id":', '"future_id":"00","daemon_id":'), "server");

  // Tolerance is additive only. The same frame is still refused when it
  // exceeds its bound, and a missing required member is still malformed.
  const oversized = snapshot.replace('"body":{', `"future":"${"x".repeat(MAX_SNAPSHOT_BYTES)}","body":{`);
  expectMalformed(() => decodeServerControl(oversized));
  const watch = encodeStateWatch("watch", { after_head: 1n });
  const padded = watch.replace('"body":{', `"future":"${"x".repeat(MAX_CONTROL_BYTES)}","body":{`);
  expectMalformed(() => decodeClientControl(padded));
  expectMalformed(() => decodeServerControl(snapshot.replace('"head":"1"', '"future_head":"1"')));
  expectMalformed(() => decodeServerControl(snapshot.replace('"capacity":8,', "")));
  expectMalformed(() => decodeServerControl(hello.replace('"boot_id"', '"future_id"')));
  // An unknown member is still a member: the object bound is unchanged.
  const crowded = Array.from({ length: 32 }, (_, index) => `"future${index}":0`).join(",");
  expectMalformed(() => decodeClientControl(watch.replace('"after_head":"1"', `"after_head":"1",${crowded}`)));
  // A tolerated member is not an unmeasured hole: the array and depth bounds
  // apply inside it exactly as they do to a known member.
  const wide = Array.from({ length: 33 }, () => "0").join(",");
  expectMalformed(() => decodeClientControl(watch.replace('"after_head":"1"', `"after_head":"1","future":[${wide}]`)));
  const deep = "[".repeat(18) + "0" + "]".repeat(18);
  expectMalformed(() => decodeClientControl(watch.replace('"after_head":"1"', `"after_head":"1","future":${deep}`)));
  // Only members are tolerated: an unknown type and a wrong direction remain
  // finite refusals.
  expectMalformed(() => decodeServerControl(snapshot.replace('"STATE_SNAPSHOT"', '"STATE_FUTURE"')));
  expectMalformed(() => decodeClientControl(snapshot.replace('"body":{', '"future":1,"body":{')));
});
