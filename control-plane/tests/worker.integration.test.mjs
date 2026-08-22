import assert from 'node:assert/strict';
import { createHmac } from 'node:crypto';
import { once } from 'node:events';
import { request as httpRequest } from 'node:http';
import { createServer } from 'node:net';
import { mkdtemp, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { spawn } from 'node:child_process';
import test from 'node:test';

const SECRET = '0123456789abcdef0123456789abcdef';
const REVISION = 'maintainer-v1';
const APP_ID = '5678';
const WEBHOOK_PATH = '/v1/github/maintainer/webhook';

test('the built Worker enforces the journal contract in workerd', async () => {
  const persistence = await mkdtemp(join(tmpdir(), 'df-worker-state-'));
  let worker;
  try {
    worker = await startWorker(persistence, true);
    assert.equal((await fetch(`${worker.origin}/healthz`)).status, 200);
    const readiness = await json(worker.origin, '/readyz');
    assert.equal(readiness.status, 200);
    assert.equal(readiness.body.maintainer_webhook, 'bootstrap_ping_only');

    const persistentId = '0c8a5c44-7f1f-11f0-952e-acde48001122';
    const persistentBody = '{"zen":"persist across workerd restart"}';
    assert.equal(
      (await signedRequest(worker.origin, 'ping', persistentId, persistentBody)).status,
      200,
    );
    assert.equal(
      (await signedRequest(worker.origin, 'ping', persistentId, persistentBody)).status,
      200,
    );

    const exactId = '1c8a5c44-7f1f-11f0-952e-acde48001122';
    const exact = await Promise.all(
      Array.from({ length: 8 }, () =>
        signedRequest(worker.origin, 'ping', exactId, '{"zen":"concurrent replay"}'),
      ),
    );
    assert.deepEqual(exact.map(({ status }) => status), Array(8).fill(200));

    const conflictId = '2c8a5c44-7f1f-11f0-952e-acde48001122';
    const conflicts = await Promise.all([
      signedRequest(worker.origin, 'ping', conflictId, '{"zen":"first"}'),
      signedRequest(worker.origin, 'ping', conflictId, '{"zen":"second"}'),
    ]);
    assert.deepEqual(conflicts.map(({ status }) => status).sort(), [200, 409]);

    const rejected = await signedRequest(
      worker.origin,
      'installation',
      '3c8a5c44-7f1f-11f0-952e-acde48001122',
      '{"action":"created"}',
    );
    assert.equal(rejected.status, 422);

    const oversized = 'x'.repeat(65 * 1024);
    assert.equal(
      (
        await signedRequest(
          worker.origin,
          'ping',
          '4c8a5c44-7f1f-11f0-952e-acde48001122',
          oversized,
        )
      ).status,
      413,
    );

    assert.equal(
      await duplicateSignatureStatus(
        worker.origin,
        '5c8a5c44-7f1f-11f0-952e-acde48001122',
        '{"zen":"one signature"}',
      ),
      400,
    );
    assert.equal(
      (await fetch(`${worker.origin}/v1/github/product/webhook`, { method: 'POST' })).status,
      404,
    );
    assert.equal(
      (await fetch(`${worker.origin}/v1/operator/needs-you`, { method: 'POST' })).status,
      404,
    );

    await worker.stop();
    worker = await startWorker(persistence, true);
    assert.equal(
      (
        await signedRequest(
          worker.origin,
          'ping',
          persistentId,
          '{"zen":"changed after restart"}',
        )
      ).status,
      409,
    );

    await worker.stop();
    worker = await startWorker(persistence, false);
    const inactive = await json(worker.origin, '/readyz');
    assert.equal(inactive.status, 503);
    assert.equal(inactive.body.status, 'inactive');
    assert.equal(
      (
        await signedRequest(
          worker.origin,
          'ping',
          '6c8a5c44-7f1f-11f0-952e-acde48001122',
          '{"zen":"must not route"}',
        )
      ).status,
      404,
    );
  } finally {
    await worker?.stop();
    await rm(persistence, { recursive: true, force: true });
  }
});

async function startWorker(persistence, configured) {
  const port = await freePort();
  const wrangler = join(process.cwd(), 'node_modules', '.bin', 'wrangler');
  const args = [
    'dev',
    '--local',
    '--ip',
    '127.0.0.1',
    '--port',
    String(port),
    '--persist-to',
    persistence,
    '--log-level',
    'error',
  ];
  if (configured) {
    args.push(
      '--var',
      `DARK_FACTORY_MAINTAINER_WEBHOOK_SECRET:${SECRET}`,
      '--var',
      `DARK_FACTORY_MAINTAINER_WEBHOOK_SECRET_REVISION:${REVISION}`,
      '--var',
      `DARK_FACTORY_MAINTAINER_APP_ID:${APP_ID}`,
    );
  }
  const child = spawn(wrangler, args, {
    cwd: process.cwd(),
    env: { ...process.env, NO_COLOR: '1' },
    stdio: ['ignore', 'pipe', 'pipe'],
  });
  let logs = '';
  child.stdout.on('data', (chunk) => {
    logs = `${logs}${chunk}`.slice(-16_384);
  });
  child.stderr.on('data', (chunk) => {
    logs = `${logs}${chunk}`.slice(-16_384);
  });
  const origin = `http://127.0.0.1:${port}`;
  for (let attempt = 0; attempt < 120; attempt += 1) {
    if (child.exitCode !== null) {
      throw new Error(`wrangler exited before readiness:\n${logs}`);
    }
    try {
      const response = await fetch(`${origin}/healthz`);
      if (response.status === 200) {
        return { origin, stop: () => stop(child) };
      }
    } catch {
      // The local socket is not listening yet.
    }
    await new Promise((resolve) => setTimeout(resolve, 250));
  }
  await stop(child);
  throw new Error(`wrangler did not become ready:\n${logs}`);
}

async function stop(child) {
  if (child.exitCode !== null) return;
  child.kill('SIGTERM');
  const exited = once(child, 'exit');
  const timeout = new Promise((resolve) => setTimeout(resolve, 5_000, 'timeout'));
  if ((await Promise.race([exited, timeout])) === 'timeout') {
    child.kill('SIGKILL');
    await once(child, 'exit');
  }
}

async function freePort() {
  const server = createServer();
  server.listen(0, '127.0.0.1');
  await once(server, 'listening');
  const { port } = server.address();
  server.close();
  await once(server, 'close');
  return port;
}

async function json(origin, path) {
  const response = await fetch(`${origin}${path}`);
  return { status: response.status, body: await response.json() };
}

async function signedRequest(origin, event, delivery, body) {
  return fetch(`${origin}${WEBHOOK_PATH}`, {
    method: 'POST',
    headers: signedHeaders(event, delivery, body),
    body,
  });
}

function signedHeaders(event, delivery, body) {
  return {
    'content-type': 'application/json',
    'x-github-event': event,
    'x-github-delivery': delivery,
    'x-github-hook-id': '1234',
    'x-github-hook-installation-target-id': APP_ID,
    'x-github-hook-installation-target-type': 'integration',
    'x-hub-signature-256': `sha256=${createHmac('sha256', SECRET).update(body).digest('hex')}`,
  };
}

async function duplicateSignatureStatus(origin, delivery, body) {
  const headers = signedHeaders('ping', delivery, body);
  return new Promise((resolve, reject) => {
    const request = httpRequest(
      `${origin}${WEBHOOK_PATH}`,
      {
        method: 'POST',
        headers: {
          ...headers,
          'x-hub-signature-256': [
            headers['x-hub-signature-256'],
            headers['x-hub-signature-256'],
          ],
        },
      },
      (response) => {
        response.resume();
        response.on('end', () => resolve(response.statusCode));
      },
    );
    request.on('error', reject);
    request.end(body);
  });
}
