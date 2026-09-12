import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import test from "node:test";
import {
  BROWSER_MANIFEST,
  ProtocolError,
  buildAuthTranscript,
  buildPairTranscript,
  decodeClientControl,
  decodeServerControl,
  decodeTerminalInput,
  decodeTerminalOutput,
  encodeClientControl,
  encodeServerControl,
  encodeServerError,
  encodeTerminalExit,
  encodeTerminalInput,
  encodeTerminalInputResult,
  encodeTerminalOutput,
  MAX_TERMINAL_COLS,
  MAX_TERMINAL_ROWS,
  MAX_TERMINAL_UNACKED_BYTES,
  hexBytes,
  MAX_TERMINAL_PAYLOAD,
  verifyP256Signature,
} from "../dist/src/index.js";

const root = join(dirname(fileURLToPath(import.meta.url)), "../../../..");
const fixture = (name) => readFileSync(join(root, "protocol/browser/fixtures", name), "utf8").trim();
const json = (name) => JSON.parse(fixture(name));
const bytes = (hex) => hexBytes(hex);
const expectMalformed = (fn) => assert.throws(fn, (e) => e instanceof ProtocolError && ["malformed", "wrong_direction"].includes(e.code));
// The contract tolerates additive change: a member this build does not know is
// ignored on the wire. The proof that it is only additive is that the mutated
// frame decodes to exactly the frame the unmutated one decodes to, so the
// added bytes reach no field and become no authority.
const expectIgnoredMember = (base, mutated, role = "client") => {
  const decode = role === "client" ? decodeClientControl : decodeServerControl;
  assert.deepEqual(decode(mutated), decode(base));
};

test("canonical control fixtures decode by sender role and re-encode exactly", () => {
  const client = ["pair_prove", "auth_prove", "remote_invite", "push_subscribe", "browser_clients_get", "browser_client_revoke", "run_paths_get", "agent_control", "task_history_get", "task_list_get"];
  const server = ["hello", "pair_result", "auth_result", "remote_invite_result", "push_subscribe_result", "browser_clients", "browser_client_revoke_result", "run_paths", "agent_control_result", "task_history", "task_list"];
  for (const name of client) assert.equal(encodeClientControl(decodeClientControl(fixture(`${name}.json`))), fixture(`${name}.json`));
  for (const name of server) assert.equal(encodeServerControl(decodeServerControl(fixture(`${name}.json`))), fixture(`${name}.json`));
  // An agent with no live run is in no room, so an empty run identity may not
  // carry paths and the array keeps the generic item bound.
  const rooms = fixture("run_paths.json");
  expectMalformed(() => decodeServerControl(rooms.replace(/"run_id":"[0-9a-f]+"/, '"run_id":""')));
  expectMalformed(() => decodeServerControl(rooms.replace('"internal/kernel"', '""')));
  const error = fixture("error.json");
  assert.equal(encodeClientControl(decodeClientControl(error)), error);
  assert.equal(encodeServerControl(decodeServerControl(error)), error);
});

test("control envelope IDs are symmetrically required, optional, or forbidden", () => {
  const withoutID = encodeServerError({ code: "not_found", retryable: false });
  const withID = encodeServerError({ code: "not_found", retryable: false }, "entity-1");
  for (const decode of [decodeClientControl, decodeServerControl]) {
    assert.equal(decode(withoutID).id, undefined);
    assert.equal(decode(withID).id, "entity-1");
    for (const id of ["", "x".repeat(65), 1, null]) {
      expectMalformed(() => decode(JSON.stringify({ type: "ERROR", id, body: { code: "not_found", retryable: false } })));
    }
  }
  for (const id of ["", "x".repeat(65), 1, null]) expectMalformed(() => encodeServerError({ code: "not_found", retryable: false }, id));

  const hello = decodeServerControl(fixture("hello.json"));
  const acknowledgement = decodeClientControl(fixture("terminal_ack.json"));
  assert.equal(encodeServerControl(hello), fixture("hello.json"));
  assert.equal(encodeClientControl(acknowledgement), fixture("terminal_ack.json"));
  expectMalformed(() => encodeServerControl({ ...hello, id: "hello" }));
  expectMalformed(() => encodeClientControl({ ...acknowledgement, id: "ack" }));
  expectMalformed(() => decodeServerControl(fixture("hello.json").replace('"body":', '"id":"hello","body":')));
  expectMalformed(() => decodeClientControl(fixture("terminal_ack.json").replace('"body":', '"id":"ack","body":')));

  const required = decodeServerControl(fixture("auth_result.json"));
  assert.equal(encodeServerControl(required), fixture("auth_result.json"));
  const { id: _id, ...missingID } = required;
  expectMalformed(() => encodeServerControl(missingID));
});

test("control role, envelope, field and capability validation is closed", () => {
  expectMalformed(() => decodeClientControl(fixture("hello.json")));
  expectMalformed(() => decodeServerControl(fixture("pair_prove.json")));
  expectIgnoredMember(fixture("auth_result.json"), fixture("auth_result.json").replace('"client_id":', '"extra":1,"client_id":'), "server");
  for (const mutation of [
    (s) => s.replace('"type":"AUTH_RESULT"', '"type":"NOPE"'),
    (s) => s.replace('"client_id":"', '"client_id":"0'),
    (s) => s.replace('"capabilities":9', '"capabilities":8'),
    (s) => s.replace('"capabilities":9', '"capabilities":32'),
    (s) => s.replace('{"type"', '{"type":"AUTH_RESULT","type"'),
    (s) => s.replace('"id":"auth-1"', '"id":"auth-1","id":"other"'),
  ]) expectMalformed(() => decodeServerControl(mutation(fixture("auth_result.json"))));
  expectMalformed(() => decodeClientControl('{"type":"ERROR","body":{"code":"secret","retryable":false}}'));
  // A daemon diagnostic added beside the finite error code reaches no field,
  // so it can never be rendered; the decoded body is code and retryable only.
  expectIgnoredMember('{"type":"ERROR","body":{"code":"internal","retryable":false}}', '{"type":"ERROR","body":{"code":"internal","retryable":false,"message":"private"}}');
  assert.deepEqual(decodeClientControl('{"type":"ERROR","body":{"code":"internal","retryable":false,"message":"private"}}').body, { code: "internal", retryable: false });
  // AUTH_PROVE has no public-key member: the daemon reads the key from its
  // durable client row, so a caller-supplied one is ignored, never adopted.
  expectIgnoredMember(fixture("auth_prove.json"), fixture("auth_prove.json").replace('"signature"', '"public_key_sec1":"046b17d1f2e12c4247f8bce6e563a440f277037d812deb33a0f4a13945d898c2964fe342e2fe1a7f9b8ee7eb4a7c0f9e162bce33576b315ececbb6406837bf51f5","signature"'));
  for (const number of ["1.0", "1e0", "01", "+1", "9007199254740992", "-9007199254740992"]) {
    expectMalformed(() => decodeServerControl(`{"type":"AUTH_RESULT","id":"auth-1","body":{"client_id":"606162636465666768696a6b6c6d6e6f","capabilities":${number}}}`));
  }
  assert.equal(decodeServerControl('{"type":"AUTH_RESULT","id":"auth-1","body":{"client_id":"606162636465666768696a6b6c6d6e6f","capabilities":1} }').body.capabilities, 1);
});

test("browser terminal and HumanRequest controls are typed, directional, and bounded", () => {
  const client = ["human_request_detail_get", "human_request_reply", "human_request_cancel_run", "terminal_target_get", "terminal_attach", "terminal_ack", "terminal_lease_acquire", "terminal_lease_renew", "terminal_lease_release", "terminal_resize", "terminal_detach"];
  const server = ["human_request_detail", "human_request_reply_result", "human_request_cancel_run_result", "terminal_target", "terminal_attached", "terminal_lease_result", "terminal_resized", "terminal_detached", "terminal_input_result", "terminal_eof", "terminal_exit", "terminal_reset"];
  for (const name of client) {
    const frame = decodeClientControl(fixture(`${name}.json`));
    assert.equal(typeof frame.body, "object");
    assert.equal(encodeClientControl(frame), fixture(`${name}.json`));
    expectMalformed(() => decodeServerControl(fixture(`${name}.json`)));
  }
  for (const name of server) {
    const frame = decodeServerControl(fixture(`${name}.json`));
    assert.equal(typeof frame.body, "object");
    assert.equal(encodeServerControl(frame), fixture(`${name}.json`));
    expectMalformed(() => decodeClientControl(fixture(`${name}.json`)));
  }
  const ack = fixture("terminal_ack.json");
  expectMalformed(() => decodeClientControl(ack.replace('"body"', '"id":"ack","body"')));
  expectMalformed(() => decodeClientControl(ack.replace('"next_sequence":"1"', '"next_sequence":"0"')));
  const attached = fixture("terminal_attached.json");
  assert.equal(decodeServerControl(attached).body.max_unacked_bytes, BigInt(MAX_TERMINAL_UNACKED_BYTES));
  expectMalformed(() => decodeServerControl(attached.replace('"max_unacked_bytes":"65536"', '"max_unacked_bytes":"65535"')));
  for (const [name, role, field, value] of [["terminal_resize", "client", "rows", MAX_TERMINAL_ROWS], ["terminal_resize", "client", "cols", MAX_TERMINAL_COLS], ["terminal_resized", "server", "rows", MAX_TERMINAL_ROWS], ["terminal_resized", "server", "cols", MAX_TERMINAL_COLS]]) {
    const raw = fixture(`${name}.json`);
    const decode = role === "client" ? decodeClientControl : decodeServerControl;
    const atMaximum = raw.replace(new RegExp(`"${field}":\\d+`), `"${field}":${value}`);
    assert.doesNotThrow(() => decode(atMaximum));
    expectMalformed(() => decode(atMaximum.replace(`"${field}":${value}`, `"${field}":${value + 1}`)));
  }
  for (const escape of ["\\ud800", "\\udc00", "\\udc00\\ud800", "\\ud800\\u0041"]) {
    expectMalformed(() => decodeClientControl(fixture("human_request_reply.json").replace('"reply":"ok"', `"reply":"${escape}"`)));
  }
  for (const literal of ["\ud800", "\udc00"]) {
    expectMalformed(() => decodeClientControl(fixture("human_request_reply.json").replace('"reply":"ok"', `"reply":"${literal}"`)));
  }
  expectMalformed(() => decodeClientControl(fixture("human_request_reply.json").replace('"reply":"ok"', `"reply":"${"x".repeat(8193)}"`)));
  const pairedReply = decodeClientControl(fixture("human_request_reply.json").replace('"reply":"ok"', '"reply":"\\ud83d\\ude00"'));
  assert.equal(pairedReply.body.reply, "😀");
  const humanReply = fixture("human_request_reply.json");
  const humanCancel = fixture("human_request_cancel_run.json");
  const humanDetail = fixture("human_request_detail.json");
  const humanCancelResult = fixture("human_request_cancel_run_result.json");
  // Reply and cancellation have no run destination member, so a caller-supplied
  // one changes nothing: the Store still derives the origin.
  expectIgnoredMember(humanReply, humanReply.replace('"request_id":', `"run_id":"${"11".repeat(16)}","request_id":`));
  expectIgnoredMember(humanCancel, humanCancel.replace('"request_id":', `"run_id":"${"11".repeat(16)}","request_id":`));
  expectMalformed(() => decodeClientControl(humanCancel.replace('"expected_run_revision"', '"Expected_Run_Revision"')));
  expectMalformed(() => decodeServerControl(humanDetail.replace('"expected_run_revision":"8"', '"expected_run_revision":"7"')));
  // Residue from the deleted generic action shape carries no meaning.
  expectIgnoredMember(humanDetail, humanDetail.replace('"cancel_run":{', '"cancel_run":{"action":"cancel_run",'), "server");
  expectIgnoredMember(humanCancelResult, humanCancelResult.replace('"run_id":', '"action":"cancel_run","run_id":'), "server");
  expectMalformed(() => decodeServerControl(humanCancelResult.replace('"type":"HUMAN_REQUEST_CANCEL_RUN_RESULT"', '"type":"HUMAN_REQUEST_ACTION_RESULT"')));
  const lease = fixture("terminal_lease_result.json");
  const target = fixture("terminal_target.json");
  const targetGet = fixture("terminal_target_get.json");
  expectMalformed(() => decodeClientControl(targetGet.replace('"expected_agent_revision":"7"', '"expected_agent_revision":"0"')));
  expectMalformed(() => decodeClientControl(targetGet.replace('"expected_head":"9007199254740993"', '"expected_head":"01"')));
  expectMalformed(() => decodeClientControl(targetGet.replace('"agent_id"', '"Agent_ID"')));
  assert.equal(decodeServerControl(target).body.target.run_id, "11".repeat(16));
  assert.equal(decodeServerControl(target).body.head, 9007199254740993n);
  // A terminal target is an observation coordinate with a closed shape: a
  // process identity added beside it can never be read.
  expectIgnoredMember(target, target.replace('"target":{', '"target":{"pid":1,'), "server");
  expectMalformed(() => decodeServerControl(target.replace(',"session_revision":"9"', '')));
  const unavailable = target.replace(/"target":\{[^}]+\}/, '"target":null');
  assert.equal(encodeServerControl(decodeServerControl(unavailable)), unavailable);
  expectIgnoredMember(lease, lease.replace('"expires_at_ms":"10000"', '"expires_at_ms":"10000","renew_after_ms":"10000"'), "server");
  expectMalformed(() => decodeServerControl(lease.replace('"expires_at_ms":"10000"', '"expires_at_ms":null')));
  expectMalformed(() => decodeServerControl(lease.replace(',"expires_at_ms":"10000"', '')));
  expectMalformed(() => decodeServerControl(lease.replace('"expires_at_ms":"10000"', '"expires_at_ms":"0"')));
  const released = lease.replace('"operation":"acquired"', '"operation":"released"').replace(',"expires_at_ms":"10000"', '');
  assert.doesNotThrow(() => decodeServerControl(released));
  expectMalformed(() => decodeServerControl(released.replace('"operation":"released"', '"operation":"released","expires_at_ms":null')));
  for (const field of ["exit_code", "exit_signal", "aborted"]) {
    const exit = fixture("terminal_exit.json").replace(new RegExp(`"${field}":(?:0|false)`), `"${field}":null`);
    expectMalformed(() => decodeServerControl(exit));
  }
});

test("terminal exit has one canonical code-or-signal status arm", () => {
  const session_id = "22".repeat(16);
  for (const [exit_code, exit_signal, aborted] of [[0, 0, false], [7, 0, true], [0, 15, false]]) {
    const wire = encodeTerminalExit("exit", { session_id, exit_code, exit_signal, aborted });
    const exit = decodeServerControl(wire).body;
    assert.equal(exit.exit_code, exit_code);
    assert.equal(exit.exit_signal, exit_signal);
    assert.equal(exit.aborted, aborted);
  }
  for (const [exit_code, exit_signal] of [
    [-1, 0], [-1, 15], [7, 9], [0, -1], [Number.MAX_SAFE_INTEGER + 1, 0], [0, Number.MAX_SAFE_INTEGER + 1], [null, 0], [0, null],
  ]) {
    expectMalformed(() => encodeTerminalExit("exit", { session_id, exit_code, exit_signal, aborted: false }));
    expectMalformed(() => decodeServerControl(JSON.stringify({ type: "TERMINAL_EXIT", id: "exit", body: { session_id, exit_code, exit_signal, aborted: false } })));
  }
});

test("terminal input result status and accepted bytes are physically consistent", () => {
  const base = { session_id: "22".repeat(16), generation: 1n, sequence: 1n };
  for (const [status, accepted_bytes] of [
    ["accepted", 1n], ["accepted", BigInt(MAX_TERMINAL_PAYLOAD)],
    ["partial", 1n], ["partial", BigInt(MAX_TERMINAL_PAYLOAD)],
    ["rejected", 0n], ["uncertain", 0n],
  ]) {
    const wire = encodeTerminalInputResult("input-result", { ...base, status, accepted_bytes });
    const decoded = decodeServerControl(wire);
    assert.equal(decoded.body.status, status);
    assert.equal(decoded.body.accepted_bytes, accepted_bytes);
  }
  const fixtureWire = fixture("terminal_input_result.json");
  for (const [status, accepted_bytes] of [
    ["accepted", 0n], ["rejected", 1n], ["partial", 0n], ["uncertain", 1n], ["accepted", 8193n],
  ]) {
    expectMalformed(() => encodeTerminalInputResult("input-result", { ...base, status, accepted_bytes }));
    const mutated = fixtureWire.replace('"status":"accepted"', `"status":"${status}"`).replace('"accepted_bytes":"2"', `"accepted_bytes":"${accepted_bytes}"`);
    expectMalformed(() => decodeServerControl(mutated));
  }
});

test("pair and auth transcripts are byte-exact and verify with WebCrypto P-1363", async () => {
  const value = json("transcript.json");
  const pair = value.pair;
  const pairTranscript = buildPairTranscript(pair);
  assert.equal(Buffer.from(pairTranscript).toString("hex"), pair.transcript);
  assert.equal(await verifyP256Signature(bytes(pair.public_key_sec1), bytes(pair.signature), pairTranscript), true);
  const auth = value.auth;
  const authTranscript = buildAuthTranscript(auth);
  assert.equal(Buffer.from(authTranscript).toString("hex"), auth.transcript);
  assert.equal(await verifyP256Signature(bytes(auth.public_key_sec1), bytes(auth.signature), authTranscript), true);
  const altered = authTranscript.slice(); altered[altered.length - 1] ^= 1;
  assert.equal(await verifyP256Signature(bytes(auth.public_key_sec1), bytes(auth.signature), altered), false);
});

test("transcript fixed fields and text are validated before signing", () => {
  const pair = json("transcript.json").pair;
  for (const field of ["daemon_id", "boot_id", "connection_nonce", "challenge", "public_key_sec1"]) {
    const altered = { ...pair, [field]: pair[field].slice(2) };
    expectMalformed(() => buildPairTranscript(altered));
  }
  expectMalformed(() => buildPairTranscript({ ...pair, host: "" }));
  expectMalformed(() => buildPairTranscript({ ...pair, origin: "https://x\u0000.example" }));
  expectMalformed(() => hexBytes("AA"));
  expectMalformed(() => buildPairTranscript(null));
  expectMalformed(() => buildPairTranscript({ ...pair, host: "https://bad\ud800" }));
  expectMalformed(() => buildPairTranscript({ ...pair, origin: "https://bad\udfff" }));
});

test("transcript preserves astral Unicode as UTF-8 and rejects only lone surrogates", () => {
  const pair = json("transcript.json").pair;
  const host = `${pair.host}🚀`;
  const origin = `${pair.origin}🌟`;
  const actual = buildPairTranscript({ ...pair, host, origin });
  const expected = [];
  const add = (value) => { const field = typeof value === "string" ? new TextEncoder().encode(value) : value; expected.push(new Uint8Array([field.length >>> 24, field.length >>> 16 & 255, field.length >>> 8 & 255, field.length & 255]), field); };
  expected.push(new TextEncoder().encode("dark-factory/browser/pair\0"));
  for (const field of [pair.daemon_id, pair.boot_id, pair.connection_nonce, pair.challenge, pair.public_key_sec1]) add(bytes(field));
  add(host); add(origin);
  const joined = new Uint8Array(expected.reduce((n, field) => n + field.length, 0)); let offset = 0;
  for (const field of expected) { joined.set(field, offset); offset += field.length; }
  assert.deepEqual(actual, joined);
});

test("terminal fixtures round-trip and preserve 64-bit values as bigint", () => {
  const session = bytes("101112131415161718191a1b1c1d1e1f");
  for (const [name, decode, encode] of [["terminal_input", decodeTerminalInput, (f) => encodeTerminalInput(f.sessionId, f.sequence, f.leaseGeneration, f.payload)], ["terminal_output", decodeTerminalOutput, (f) => encodeTerminalOutput(f.sessionId, f.sequence, f.payload)]]) {
    const raw = bytes(fixture(`${name}.hex`));
    const frame = decode(raw);
    assert.equal(Buffer.from(encode(frame)).toString("hex"), Buffer.from(raw).toString("hex"));
    assert.equal(typeof frame.sequence, "bigint");
    assert.deepEqual(frame.sessionId, session);
  }
  const high = encodeTerminalOutput(session, 0x0020_0000_0000_0001n, new Uint8Array([1]));
  assert.equal(decodeTerminalOutput(high).sequence, 0x0020_0000_0000_0001n);
});

test("terminal framing rejects wrong direction, identity, bounds and overflow", () => {
  const session = bytes("101112131415161718191a1b1c1d1e1f");
  const input = encodeTerminalInput(session, 7n, 5n, new Uint8Array([1, 2]));
  const output = encodeTerminalOutput(session, 7n, new Uint8Array([1, 2]));
  expectMalformed(() => decodeTerminalOutput(input));
  expectMalformed(() => decodeTerminalInput(output));
  for (const mutate of [
    (b) => { b[0] = 0; }, (b) => { b[2] = 2; }, (b) => { b[3] = 9; },
    (b) => { b[4] = 0; b[5] = 0; b[6] = 0; b[7] = 0; b[8] = 0; b[9] = 0; b[10] = 0; b[11] = 0; b[12] = 0; b[13] = 0; b[14] = 0; b[15] = 0; b[16] = 0; b[17] = 0; b[18] = 0; b[19] = 0; },
    (b) => { b[39] = 0; }, (b) => { b[36] = 0xff; b[37] = 0xff; b[38] = 0xff; b[39] = 0xff; },
  ]) { const copy = input.slice(); mutate(copy); expectMalformed(() => decodeTerminalInput(copy)); }
  expectMalformed(() => encodeTerminalInput(session, 0n, 1n, new Uint8Array([1])));
  expectMalformed(() => encodeTerminalInput(session, 1n, 0n, new Uint8Array([1])));
  expectMalformed(() => encodeTerminalOutput(session, 0xffff_ffff_ffff_ffffn, new Uint8Array([1, 2])));
  const maxEnd = encodeTerminalOutput(session, 0xffff_ffff_ffff_ffffn - 1n, new Uint8Array([1]));
  assert.equal(decodeTerminalOutput(maxEnd).sequence, 0xffff_ffff_ffff_ffffn - 1n);
  expectMalformed(() => encodeTerminalOutput(session, 0xffff_ffff_ffff_ffffn, new Uint8Array([1])));
  expectMalformed(() => encodeTerminalOutput(session, 1n, new Uint8Array(MAX_TERMINAL_PAYLOAD + 1)));
  expectMalformed(() => decodeTerminalInput(null));
  expectMalformed(() => encodeTerminalInput(null, 1n, 1n, new Uint8Array([1])));
  expectMalformed(() => encodeTerminalInput(session, 1, 1n, new Uint8Array([1])));
  expectMalformed(() => encodeTerminalInput(session, 1n, 1n, null));
});

test("terminal payload boundary matches the runner limit", () => {
  const session = bytes("101112131415161718191a1b1c1d1e1f");
  for (const [encode, decode] of [[encodeTerminalInput, decodeTerminalInput], [encodeTerminalOutput, decodeTerminalOutput]]) {
    const frame = encode === encodeTerminalInput
      ? encode(session, 1n, 1n, new Uint8Array(MAX_TERMINAL_PAYLOAD))
      : encode(session, 1n, new Uint8Array(MAX_TERMINAL_PAYLOAD));
    assert.equal(decode(frame).payload.length, MAX_TERMINAL_PAYLOAD);
  }
  expectMalformed(() => encodeTerminalInput(session, 1n, 1n, new Uint8Array(MAX_TERMINAL_PAYLOAD + 1)));
  expectMalformed(() => encodeTerminalOutput(session, 1n, new Uint8Array(MAX_TERMINAL_PAYLOAD + 1)));
});

test("all public malformed boundaries return the finite ProtocolError", async () => {
  await assert.rejects(() => verifyP256Signature(null, new Uint8Array(64), new Uint8Array(1)), (error) => error instanceof ProtocolError && error.code === "malformed");
  expectMalformed(() => decodeServerControl(Symbol("attacker")));
  expectMalformed(() => decodeServerControl("{"));
  expectMalformed(() => hexBytes({}));
});

// Go's decoder matches struct fields case-insensitively, so a member differing
// from a known one only in case is not unknown to it: left tolerated it would
// overwrite the exact member Go validated, while this decoder -- whose object
// keys are exact -- would keep reading the exact one. Both sides refuse it, so
// the two read the same frame the same way. A name unknown under every case is
// still ignored.
test("a case variant of a known member is refused, an unknown name is ignored", () => {
  const hello = fixture("hello.json");
  const shadow = "ff".repeat(16);
  for (const mutated of [
    hello.replace('"boot_id"', `"DAEMON_ID":"${shadow}","boot_id"`),
    hello.replace('"boot_id"', `"Daemon_Id":"${shadow}","boot_id"`),
    hello.replace('"daemon_id"', `"DAEMON_ID":"${shadow}","daemon_id"`),
    hello.replace('"daemon_id"', '"DAEMON_ID"'),
    hello.replace('{"type"', '{"Type":"NOPE","type"'),
    hello.replace('"body"', '"BODY":{},"body"'),
  ]) expectMalformed(() => decodeServerControl(mutated));
  // A long s folds onto s in Go, so `ſession_id` would reach session_id there.
  const attached = fixture("terminal_attached.json");
  expectMalformed(() => decodeServerControl(attached.replace('"floor"', `"\u017fession_id":"${"ee".repeat(16)}","floor"`)));
  expectIgnoredMember(hello, hello.replace('"boot_id"', `"FUTURE_ID":"${shadow}","boot_id"`), "server");
  // An optional envelope member is a known name too.
  const detail = fixture("human_request_detail.json");
  expectMalformed(() => decodeServerControl(detail.replace('"id"', '"ID":"x","id"')));
});

test("manifest has exactly one public mapping for every stable entry", () => {
  const source = json("../manifest.json");
  assert.deepEqual(Object.keys(BROWSER_MANIFEST.capabilities), source.capabilities.map((x) => x.name));
  assert.deepEqual(Object.values(BROWSER_MANIFEST.capabilities), source.capabilities.map((x) => x.value));
  const camel = (key) => key.split("_").map((word, index) => index === 0 ? word : ({ json: "JSON", sqlite: "SQLite" }[word] ?? `${word[0].toUpperCase()}${word.slice(1)}`)).join("");
  const bounds = Object.fromEntries(Object.entries(source.bounds).map(([key, value]) => [camel(key), key === "max_sqlite_integer" ? BigInt(value) : value]));
  assert.deepEqual(BROWSER_MANIFEST.bounds, bounds);
  for (const key of Object.keys(bounds)) {
    const mutated = { ...bounds };
    delete mutated[key];
    assert.notDeepEqual(mutated, BROWSER_MANIFEST.bounds);
    const changed = { ...bounds, [key]: typeof bounds[key] === "bigint" ? bounds[key] + 1n : bounds[key] + 1 };
    assert.notDeepEqual(changed, BROWSER_MANIFEST.bounds);
  }
  assert.deepEqual(BROWSER_MANIFEST.control, source.control);
  // The contract's stable name and the fixed binary frame version are mirrored
  // too. terminal.version silently held the wrong number while it was aliased
  // to the deleted protocol generation; nothing compared it to the source.
  assert.equal(BROWSER_MANIFEST.name, source.name);
  assert.equal(BROWSER_MANIFEST.terminal.magic, source.terminal.magic);
  assert.equal(BROWSER_MANIFEST.terminal.version, source.terminal.version);
  assert.equal(BROWSER_MANIFEST.terminal.headerBytes, source.terminal.header_bytes);
  assert.equal(BROWSER_MANIFEST.terminal.maxPayloadBytes, source.terminal.max_payload_bytes);
  assert.deepEqual(Object.keys(BROWSER_MANIFEST.terminal.opcodes), source.terminal.opcodes.map((x) => x.name));
  assert.deepEqual(Object.values(BROWSER_MANIFEST.terminal.opcodes), source.terminal.opcodes.map((x) => x.value));
});
