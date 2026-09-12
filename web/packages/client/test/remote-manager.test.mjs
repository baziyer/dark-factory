import assert from "node:assert/strict";
import test from "node:test";
import {
  MemoryRemoteStore,
  base64urlDecode,
  base64urlEncode,
  MAX_CONNECTED_FACTORIES,
  RemoteDaemonMismatchError,
  SessionError,
  createRemoteManager,
  parseInvitation,
  parseTicket,
} from "../dist/src/index.js";
import {
  ALL_CAPABILITIES,
  FakeFactory,
  bytes,
  FakeRelay,
  PWA_ORIGIN,
  RELAY_ORIGIN,
  VirtualTimer,
  humanRequest,
  invitationFragment,
  invitationMembers,
  nodeId,
  settle,
  snapshotBody,
} from "./remote-fake.mjs";

const NOW = 1_700_000_000;

function invitation(factory, overrides = {}) {
  return parseInvitation(invitationFragment(invitationMembers({ node: factory.node, daemon: factory.daemonId, ...overrides })), NOW);
}

function bench(options = {}) {
  const relay = new FakeRelay();
  const store = new MemoryRemoteStore();
  const timer = new VirtualTimer();
  let changes = 0;
  const manager = createRemoteManager({
    store,
    origin: PWA_ORIGIN,
    webSocket: relay.WebSocket,
    timer,
    onChange: () => { changes += 1; },
    ...options,
  });
  return { relay, store, timer, manager, changes: () => changes };
}

const nonce = (protocols) => JSON.parse(new TextDecoder().decode(base64urlDecode(protocols[2].split(".")[0]))).nonce;
const types = (socket) => socket.sent.map((wire) => JSON.parse(wire).type);
const status = (manager, node) => manager.factories().find((factory) => factory.nodeId === node)?.status;
const row = async (store, node) => (await store.list()).find((binding) => binding.nodeId === node) ?? null;
const state = (manager, node) => manager.factories().find((factory) => factory.nodeId === node)?.state;

test("pairing through the relay persists the identity and reconnects on the control ticket", async () => {
  const { relay, store, manager, changes } = bench();
  const factory = relay.add(new FakeFactory({ node: nodeId("a"), clientId: "55".repeat(16) }));
  const binding = await manager.pair(invitation(factory));
  await settle();

  assert.equal(binding.clientId, "55".repeat(16));
  assert.equal(binding.capabilities, ALL_CAPABILITIES);
  assert.equal(binding.key.extractable, false, "a device identity is never exportable");
  assert.equal(binding.label, factory.node.slice(0, 8), "a factory is named by the head of its node id");
  assert.equal(parseTicket(binding.relayTicket).purpose, "control");
  const durable = await row(store, factory.node);
  assert.equal(durable.clientId, binding.clientId);
  assert.equal(durable.capabilities, ALL_CAPABILITIES);
  assert.equal(durable.key.extractable, false);
  // Every authentication mints the next control ticket, so what is stored has
  // already rolled past the one the pairing connection returned.
  assert.equal(parseTicket(durable.relayTicket).purpose, "control");
  assert.notEqual(durable.relayTicket, binding.relayTicket);
  assert.equal(durable.daemonId, factory.daemonId);

  const sockets = relay.for(factory.node);
  assert.equal(sockets.length, 2, "one pair dial, then one control dial");
  assert.equal(sockets[0].protocols.length, 2, "a pair ticket carries no proof");
  assert.equal(types(sockets[0])[0], "PAIR_PROVE");
  assert.equal(sockets[1].protocols.length, 3);
  assert.equal(sockets[1].protocols[1], binding.relayTicket, "the control dial uses the ticket pairing minted");
  assert.deepEqual(types(sockets[1]), ["AUTH_PROVE", "STATE_GET", "STATE_WATCH"]);
  assert.equal(status(manager, factory.node), "ready");
  assert.equal(manager.selected(), factory.node);
  assert.equal(state(manager, factory.node).head, 1n);
  assert.ok(changes() > 0, "a view that never hears about a change cannot be rendered");
  manager.close();
});

test("the connection budget keeps the selected factory dialed and the rest offline", async () => {
  const { relay, manager } = bench();
  assert.equal(MAX_CONNECTED_FACTORIES, 8);
  const nodes = "abcdefghi";
  const factories = [];
  for (let index = 0; index < nodes.length; index++) {
    const factory = relay.add(new FakeFactory({ node: nodeId(nodes[index]), daemonId: "123456789abcdef"[index].repeat(32) }));
    factories.push(factory);
    await manager.pair(invitation(factory));
  }
  await settle();
  const held = (want) => manager.factories().filter((entry) => entry.status === want).map((entry) => entry.nodeId);
  const last = nodeId(nodes.at(-1));
  assert.equal(manager.selected(), last, "the factory just paired holds a slot");
  assert.equal(status(manager, last), "ready");
  assert.equal(held("ready").length, MAX_CONNECTED_FACTORIES, "one more binding than the budget is still only eight dials");
  assert.equal(held("offline").length, 1);
  const idle = held("offline")[0];
  assert.equal(manager.client(idle), undefined);

  manager.select(idle);
  await settle();
  assert.equal(status(manager, idle), "ready");
  assert.equal(held("ready").length, MAX_CONNECTED_FACTORIES, "selecting one never widens the budget");
  const spare = held("offline")[0];
  assert.equal(manager.client(spare), undefined);

  // A binding that will never dial again must not keep a slot from one that would.
  const halted = factories.find((factory) => factory.node === held("ready")[0]);
  halted.dropAll(4002, "revoked");
  await settle();
  assert.equal(status(manager, halted.node), "revoked");
  assert.equal(status(manager, spare), "ready", "the halted binding released its slot");
  manager.close();
});

test("two bound factories run side by side and select switches the exposed state", async () => {
  const { relay, manager } = bench();
  const first = relay.add(new FakeFactory({ node: nodeId("a"), daemonId: "11".repeat(16), clientId: "aa".repeat(16), snapshot: snapshotBody(4n, { human_requests: [humanRequest("a1".repeat(16))] }) }));
  const second = relay.add(new FakeFactory({ node: nodeId("b"), daemonId: "22".repeat(16), clientId: "bb".repeat(16), snapshot: snapshotBody(9n, { human_requests: [humanRequest("b1".repeat(16), { created_at: 20n, updated_at: 20n }), humanRequest("b2".repeat(16), { created_at: 30n, updated_at: 30n, status: "delivering" })] }) }));
  await manager.pair(invitation(first));
  await manager.pair(invitation(second));
  await settle();

  assert.deepEqual(manager.factories().map((entry) => [entry.label, entry.status]), [[first.node.slice(0, 8), "ready"], [second.node.slice(0, 8), "ready"]]);
  assert.equal(manager.selected(), second.node);
  assert.equal(state(manager, second.node).head, 9n);
  manager.select(first.node);
  assert.equal(state(manager, first.node).head, 4n);
  assert.equal(manager.client(first.node).status, "ready");
  assert.notEqual(manager.client(first.node), manager.client(second.node));

  // Only open requests are work waiting on a person, and each one says which
  // factory it is waiting inside.
  assert.deepEqual(manager.needsYou().map((item) => [item.nodeId, item.label, item.request.id]), [
    [first.node, first.node.slice(0, 8), "a1".repeat(16)],
    [second.node, second.node.slice(0, 8), "b1".repeat(16)],
  ]);
  manager.close();
});

test("a 4001 close takes one factory offline and leaves the other ready", async () => {
  const { relay, manager } = bench();
  const first = relay.add(new FakeFactory({ node: nodeId("a"), daemonId: "11".repeat(16) }));
  const second = relay.add(new FakeFactory({ node: nodeId("b"), daemonId: "22".repeat(16) }));
  await manager.pair(invitation(first));
  await manager.pair(invitation(second));
  await settle();

  first.dropAll(4001, "offline");
  await settle();
  assert.equal(status(manager, first.node), "offline");
  assert.equal(status(manager, second.node), "ready");
  assert.equal(manager.factories().find((entry) => entry.nodeId === first.node).state, undefined, "an offline factory shows no state");
  assert.equal(manager.needsYou().length, 0);
  manager.close();
});

test("a revoked controller stops dialing and never becomes offline again", async () => {
  const { relay, manager } = bench();
  const factory = relay.add(new FakeFactory({ node: nodeId("a") }));
  await manager.pair(invitation(factory));
  await settle();
  const before = relay.for(factory.node).length;
  factory.dropAll(4002, "revoked");
  await settle();
  assert.equal(status(manager, factory.node), "revoked");
  assert.equal(manager.client(factory.node), undefined);
  assert.equal(relay.for(factory.node).length, before, "a withdrawn credential is not re-presented");
  manager.close();
});

test("a relay that refuses the dial reads offline, not connecting, and keeps retrying", async () => {
  const { relay, timer, manager } = bench();
  const factory = relay.add(new FakeFactory({ node: nodeId("a") }));
  await manager.pair(invitation(factory));
  await settle();
  // Every relay refusal — no host, a ticket it will not take, a limit — is an
  // HTTP status before the upgrade: an error and a close with no code at all.
  factory.dropAll(4001, "gone");
  await settle();
  assert.equal(status(manager, factory.node), "offline");

  const dials = relay.for(factory.node).length;
  timer.advance(1000);
  await settle();
  const refused = relay.for(factory.node);
  assert.ok(refused.length > dials, "a factory out of reach is still retried");
  assert.equal(refused.at(-1).readyState, 3, "the refused dial never opened");
  assert.equal(status(manager, factory.node), "offline", "a dial that never opened is not a connection in progress");
  manager.close();
});

test("the status after a drop follows the dial that runs next, not the code before it", async () => {
  const { relay, timer, manager } = bench();
  const factory = relay.add(new FakeFactory({ node: nodeId("a") }));
  await manager.pair(invitation(factory));
  await settle();
  factory.dropAll(4001, "gone");
  await settle();
  assert.equal(status(manager, factory.node), "offline");

  factory.online = true;
  timer.advance(1000);
  await settle();
  assert.equal(status(manager, factory.node), "ready", "a close code never outlives the dial it ended");
  manager.close();
});

test("a binding whose control ticket has expired halts instead of dialing", async () => {
  const { relay, store, manager } = bench();
  const factory = relay.add(new FakeFactory({ node: nodeId("a") }));
  await manager.pair(invitation(factory));
  await settle();
  manager.close();
  const dials = relay.for(factory.node).length;

  // The stored ticket is good until 4_000_000_000; this device is past it.
  const later = createRemoteManager({ store, origin: PWA_ORIGIN, webSocket: relay.WebSocket, timer: new VirtualTimer(), now: () => 4_000_000_100 });
  await later.start();
  await settle();
  assert.equal(status(later, factory.node), "expired");
  assert.equal(relay.for(factory.node).length, dials, "a ticket the relay would refuse is never presented");
  assert.equal(later.client(factory.node), undefined);
  // Only a fresh invitation repairs it, and forgetting it is always allowed.
  await later.forget(factory.node);
  assert.deepEqual(later.factories(), []);
  assert.equal(await row(store, factory.node), null);
  later.close();
});

test("a pairing that never receives the relay ticket is refused, not left hanging", async () => {
  const { relay, store, manager } = bench();
  const working = relay.add(new FakeFactory({ node: nodeId("a"), daemonId: "11".repeat(16) }));
  await manager.pair(invitation(working));
  await settle();
  const before = await row(store, working.node);

  // This daemon proves the pairing and then goes quiet: without the ticket
  // that carries the next dial, the identity it minted is unusable.
  const mute = relay.add(new FakeFactory({ node: nodeId("b"), daemonId: "22".repeat(16), withTicket: false }));
  await assert.rejects(
    manager.pair(invitation(mute)),
    (error) => error instanceof SessionError && error.code === "pairing_uncertain",
  );
  await settle();
  assert.equal(await row(store, mute.node), null, "half a pairing is never written");
  assert.deepEqual(manager.factories().map((entry) => entry.nodeId), [working.node]);
  assert.deepEqual(await row(store, working.node), before, "the binding this device already had is untouched");
  manager.close();
});

test("a pairing whose socket is taken away is refused instead of waiting forever", async () => {
  const { relay, manager } = bench();
  const silent = relay.add(new FakeFactory({ node: nodeId("a") }));
  // A daemon that answers nothing leaves the pairing in flight for ever.
  silent.receive = () => {};
  const pairing = manager.pair(invitation(silent));
  const refused = assert.rejects(pairing, (error) => error instanceof SessionError && error.code === "closed");
  await settle();
  assert.equal(manager.factories().length, 1);

  await manager.forget(silent.node);
  await refused;
  assert.deepEqual(manager.factories(), []);
  manager.close();
});

test("a second pairing for the same node refuses the one still in flight", async () => {
  const { relay, manager } = bench();
  const silent = relay.add(new FakeFactory({ node: nodeId("a") }));
  silent.receive = () => {};
  const first = manager.pair(invitation(silent));
  const refused = assert.rejects(first, (error) => error instanceof SessionError && error.code === "closed");
  await settle();

  const second = manager.pair(invitation(silent));
  const abandoned = assert.rejects(second, (error) => error instanceof SessionError);
  await refused;
  manager.close();
  await abandoned;
});

test("a pairing whose ticket names another device is refused and writes nothing", async () => {
  const { relay, store, manager } = bench();
  // The ticket arrives while the session is still saving the key, so this is
  // caught at the end or not at all.
  const factory = relay.add(new FakeFactory({ node: nodeId("a"), device: base64urlEncode(bytes(65, 4)) }));
  await assert.rejects(
    manager.pair(invitation(factory)),
    (error) => error instanceof SessionError && error.code === "unauthorized",
  );
  await settle();
  assert.equal(await row(store, factory.node), null, "a binding the relay would refuse is never stored");
  assert.deepEqual(manager.factories(), []);
  manager.close();
});

test("a pairing that starts while another is being written still settles", async () => {
  const { relay, store, manager } = bench();
  const factory = relay.add(new FakeFactory({ node: nodeId("a") }));
  // Park the one write a pairing makes, so a second pair() starts while the
  // first is still inside it.
  let release;
  let held = new Promise((resolve) => { release = resolve; });
  const write = store.put.bind(store);
  store.put = async (binding) => { await held; held = Promise.resolve(); return write(binding); };

  const first = manager.pair(invitation(factory));
  await settle();
  const second = manager.pair(invitation(factory));
  release();
  assert.equal((await first).nodeId, factory.node);
  assert.equal((await second).nodeId, factory.node, "the pairing that started during the write is not left waiting");
  await settle();
  assert.equal(status(manager, factory.node), "ready");
  manager.close();
});

test("a close code the peer sends is never read as this browser's own refusal", async () => {
  const { relay, timer, manager } = bench();
  const factory = relay.add(new FakeFactory({ node: nodeId("a") }));
  await manager.pair(invitation(factory));
  await settle();

  // 4900 is what this browser closes with when a foreign daemon answers; a
  // peer sending the same code is just another drop.
  factory.dropAll(4900, "spoofed");
  factory.online = true;
  await settle();
  assert.notEqual(status(manager, factory.node), "mismatch");
  timer.advance(1000);
  await settle();
  assert.equal(status(manager, factory.node), "ready", "no code a peer can send halts a binding");
  manager.close();
});

test("a redial after the ticket's deadline never reaches the relay", async () => {
  let clock = NOW;
  const { relay, timer, manager } = bench({ now: () => clock });
  const factory = relay.add(new FakeFactory({ node: nodeId("a") }));
  await manager.pair(invitation(factory));
  await settle();
  assert.equal(status(manager, factory.node), "ready");

  // The stored ticket is good until 4_000_000_000. The clock passes it while
  // the session is live, so the reconnect BrowserClient starts on its own
  // backoff — which this manager never sees — must still refuse to dial.
  clock = 4_000_000_100;
  const dials = relay.for(factory.node).length;
  factory.dropAll(4000, "host replaced");
  await settle();
  timer.advance(1000);
  await settle();
  assert.equal(relay.for(factory.node).length, dials, "an expired ticket is never presented");
  assert.equal(status(manager, factory.node), "expired");
  assert.equal(manager.client(factory.node), undefined);
  manager.close();
});

test("a refused re-pairing leaves the identity this device already held", async () => {
  const { relay, store, manager } = bench();
  const factory = relay.add(new FakeFactory({ node: nodeId("a") }));
  await manager.pair(invitation(factory));
  await settle();
  const before = await row(store, factory.node);

  factory.helloDaemonId = "99".repeat(16);
  await assert.rejects(manager.pair(invitation(factory)), (error) => error instanceof RemoteDaemonMismatchError);
  await settle();
  const after = await row(store, factory.node);
  assert.equal(after.clientId, before.clientId);
  assert.equal(after.key, before.key, "the device key it was already holding");
  assert.equal(after.relayTicket, before.relayTicket);
  assert.deepEqual(manager.factories().map((entry) => entry.nodeId), [factory.node]);
  manager.close();
});

test("a reconnect after a drop mints a new proof and repeats the whole handshake", async () => {
  const { relay, store, timer, manager } = bench();
  const factory = relay.add(new FakeFactory({ node: nodeId("a") }));
  await manager.pair(invitation(factory));
  await settle();
  const control = relay.for(factory.node)[1];

  factory.dropAll(4000, "host replaced");
  await settle();
  timer.advance(1000);
  await settle();

  const sockets = relay.for(factory.node);
  assert.equal(sockets.length, 3);
  const next = sockets[2];
  assert.equal(next.protocols.length, 3);
  assert.notEqual(nonce(next.protocols), nonce(control.protocols), "every dial signs its own proof");
  assert.equal(parseTicket(next.protocols[1]).purpose, "control");
  assert.notEqual(next.protocols[1], control.protocols[1], "the ticket the last connection minted is the one presented");
  assert.deepEqual(types(next), ["AUTH_PROVE", "STATE_GET", "STATE_WATCH"], "a new generation replays nothing");
  assert.equal(status(manager, factory.node), "ready");

  // A ticket naming another device could never be presented from here, so the
  // one this binding already holds is the one it keeps.
  factory.device = base64urlEncode(bytes(65, 4));
  const kept = (await row(store, factory.node)).relayTicket;
  factory.dropAll(4000, "host replaced");
  await settle();
  timer.advance(1000);
  await settle();
  assert.equal(status(manager, factory.node), "ready");
  assert.equal((await row(store, factory.node)).relayTicket, kept, "a ticket for another device is dropped, not stored");
  manager.close();
});

test("forget closes the client and removes the binding; forgetDevice empties the store", async () => {
  const { relay, store, manager } = bench();
  const first = relay.add(new FakeFactory({ node: nodeId("a"), daemonId: "11".repeat(16) }));
  const second = relay.add(new FakeFactory({ node: nodeId("b"), daemonId: "22".repeat(16) }));
  await manager.pair(invitation(first));
  await manager.pair(invitation(second));
  await settle();
  const client = manager.client(first.node);
  const control = relay.for(first.node)[1];

  await manager.forget(first.node);
  await settle();
  assert.equal(client.status, "closed");
  assert.equal(control.readyState, 3);
  assert.equal(manager.client(first.node), undefined);
  assert.equal(await row(store, first.node), null);
  assert.deepEqual(manager.factories().map((entry) => entry.nodeId), [second.node]);
  assert.equal(manager.selected(), second.node);

  await manager.forgetDevice();
  await settle();
  assert.deepEqual(await store.list(), []);
  assert.deepEqual(manager.factories(), []);
  assert.equal(manager.selected(), undefined);
  assert.equal(manager.needsYou().length, 0);
  // A reload of the same installation now knows about no factory at all.
  const reloaded = createRemoteManager({ store, origin: PWA_ORIGIN, webSocket: relay.WebSocket, timer: new VirtualTimer() });
  await reloaded.start();
  assert.deepEqual(reloaded.bindings(), []);
  reloaded.close();
  manager.close();
});

test("a stored binding reconnects on start without pairing again", async () => {
  const { relay, store, manager } = bench();
  const factory = relay.add(new FakeFactory({ node: nodeId("a") }));
  await manager.pair(invitation(factory));
  await settle();
  manager.close();
  const dials = relay.for(factory.node).length;

  const reloaded = createRemoteManager({ store, origin: PWA_ORIGIN, webSocket: relay.WebSocket, timer: new VirtualTimer() });
  await reloaded.start();
  await settle();
  assert.equal(status(reloaded, factory.node), "ready");
  assert.deepEqual(types(relay.for(factory.node)[dials]), ["AUTH_PROVE", "STATE_GET", "STATE_WATCH"]);
  reloaded.close();
});

test("a bound factory that answers as another daemon halts as mismatch and stops dialing", async () => {
  const { relay, timer, manager } = bench();
  const factory = relay.add(new FakeFactory({ node: nodeId("a") }));
  await manager.pair(invitation(factory));
  await settle();
  // The node id still routes here, but the daemon behind it is not the one
  // this binding was made with.
  factory.helloDaemonId = "99".repeat(16);
  factory.dropAll(4000, "host replaced");
  await settle();
  timer.advance(1000);
  await settle();

  const dials = relay.for(factory.node).length;
  assert.equal(status(manager, factory.node), "mismatch");
  assert.equal(manager.client(factory.node), undefined);
  const view = manager.factories()[0];
  assert.equal(view.error instanceof RemoteDaemonMismatchError, true);
  assert.equal(view.state, undefined);
  timer.advance(60_000);
  await settle();
  assert.equal(relay.for(factory.node).length, dials, "a node that is not the bound daemon is never dialed again");
  manager.close();
});

test("pairing against the wrong daemon rejects distinguishably and leaves no binding behind", async () => {
  const { relay, store, manager } = bench();
  const factory = relay.add(new FakeFactory({ node: nodeId("a"), daemonId: "11".repeat(16), helloDaemonId: "99".repeat(16) }));
  await assert.rejects(manager.pair(invitation(factory)), (error) => {
    // Distinguishable from the generic connection fault a relay refusal gives.
    assert.equal(error instanceof RemoteDaemonMismatchError, true);
    assert.equal(error instanceof SessionError, true);
    assert.equal(error.code, "unauthorized");
    assert.equal(error.nodeId, factory.node);
    return true;
  });
  await settle();
  assert.equal(await row(store, factory.node), null);
  assert.deepEqual(manager.factories(), []);
  manager.close();
});

test("the manager exposes the client's own one-shot APIs and adds no retry of its own", () => {
  const { manager } = bench();
  for (const name of ["reply", "cancel", "enqueue", "retry", "resend", "send"]) {
    assert.equal(name in manager, false, `${name} would be a second effect`);
  }
  assert.deepEqual(
    Object.getOwnPropertyNames(Object.getPrototypeOf(manager)).filter((name) => name !== "constructor").sort(),
    ["bindings", "client", "close", "factories", "forget", "forgetDevice", "needsYou", "pair", "push", "select", "selected", "setPush", "start"],
  );
  manager.close();
});

const PUSH = { endpoint: "https://web.push.apple.com/QGdfl/abc", public_key: "B" + "a".repeat(86), private_key: "MIGH" };

test("one alert subscription reaches every factory: the connected ones now, later pairings as they connect", async () => {
  const { relay, store, manager } = bench();
  const north = relay.add(new FakeFactory({ node: nodeId("a"), clientId: "55".repeat(16) }));
  await manager.pair(invitation(north));
  await settle();
  assert.equal(manager.push(), undefined);
  assert.equal(types(north.sockets.at(-1)).includes("PUSH_SUBSCRIBE"), false, "nothing is sent before the device asks");

  await manager.setPush(PUSH);
  await settle();
  assert.deepEqual(manager.push(), PUSH);
  assert.deepEqual((await row(store, north.node)).push, PUSH, "the subscription is durable on the binding");
  assert.equal(types(north.sockets.at(-1)).filter((type) => type === "PUSH_SUBSCRIBE").length, 1);

  // A factory paired after the fact inherits the device's subscription and is
  // told on its first ready connection, without the device asking again.
  const south = relay.add(new FakeFactory({ node: nodeId("b"), clientId: "66".repeat(16), daemonId: "33".repeat(16) }));
  await manager.pair(invitation(south));
  await settle();
  assert.deepEqual((await row(store, south.node)).push, PUSH);
  assert.equal(types(south.sockets.at(-1)).filter((type) => type === "PUSH_SUBSCRIBE").length, 1);

  await manager.setPush(undefined);
  assert.equal(manager.push(), undefined);
  assert.equal((await row(store, north.node)).push, undefined);
  manager.close();
});

test("pairing the only factory again keeps the device's alert subscription", async () => {
  const { relay, store, manager } = bench();
  const north = relay.add(new FakeFactory({ node: nodeId("a"), clientId: "55".repeat(16) }));
  await manager.pair(invitation(north));
  await manager.setPush(PUSH);
  await settle();
  const again = relay.add(new FakeFactory({ node: nodeId("a"), clientId: "56".repeat(16) }));
  await manager.pair(invitation(again));
  await settle();
  assert.deepEqual(manager.push(), PUSH);
  assert.deepEqual((await row(store, north.node)).push, PUSH);
  assert.equal(types(again.sockets.at(-1)).filter((type) => type === "PUSH_SUBSCRIBE").length, 1, "the re-paired factory is told once it is ready");
  manager.close();
});
