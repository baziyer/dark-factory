import assert from 'node:assert/strict';
import test from 'node:test';
import { lstat, mkdir, mkdtemp, readFile, readdir, rm, symlink, writeFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { verificationProfile } from './verification-profile.mjs';

async function home(t) {
  const path = await mkdtemp(join(tmpdir(), 'dark-factory-verification-profile-'));
  t.after(() => rm(path, { recursive: true, force: true }));
  return path;
}

test('verification profile preserves fresh, legacy, and colliding profiles', async (t) => {
  const fresh = await home(t);
  const profile = await verificationProfile(fresh);
  assert.equal((await lstat(profile)).isDirectory(), true);

  const legacyOnly = await home(t);
  const legacy = join(legacyOnly, '.dark-factory', 'verification-browser');
  await mkdir(legacy, { recursive: true });
  await writeFile(join(legacy, 'session'), 'keep');
  assert.equal(await verificationProfile(legacyOnly), join(legacyOnly, '.dark-factory-verification-browser'));
  assert.equal(await readFile(join(legacyOnly, '.dark-factory-verification-browser', 'session'), 'utf8'), 'keep');
  await assert.rejects(lstat(legacy), { code: 'ENOENT' });

  const both = await home(t);
  const current = join(both, '.dark-factory-verification-browser');
  const old = join(both, '.dark-factory', 'verification-browser');
  await mkdir(current, { recursive: true });
  await mkdir(old, { recursive: true });
  await writeFile(join(current, 'current'), 'new');
  await writeFile(join(old, 'session'), 'old');
  await verificationProfile(both);
  assert.equal(await readFile(join(current, 'current'), 'utf8'), 'new');
  const archived = (await readdir(both)).find((name) => name.startsWith('.dark-factory-verification-browser-legacy-'));
  assert.ok(archived);
  assert.equal(await readFile(join(both, archived, 'profile', 'session'), 'utf8'), 'old');
  await assert.rejects(lstat(old), { code: 'ENOENT' });
  await verificationProfile(both);
  assert.equal(await readFile(join(current, 'current'), 'utf8'), 'new');
});

test('verification profile rejects a legacy symlink', async (t) => {
  const path = await home(t);
  await mkdir(join(path, '.dark-factory'), { recursive: true });
  await symlink(join(path, 'elsewhere'), join(path, '.dark-factory', 'verification-browser'));
  await assert.rejects(verificationProfile(path), /unsafe browser verification profile path/);
});
