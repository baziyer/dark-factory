import { lstat, mkdir, mkdtemp, rename } from 'node:fs/promises';
import { homedir } from 'node:os';
import { join } from 'node:path';

async function directory(path) {
  try {
    const entry = await lstat(path);
    if (!entry.isDirectory() || entry.isSymbolicLink()) throw new Error('unsafe browser verification profile path');
    return true;
  } catch (error) {
    if (error?.code === 'ENOENT') return false;
    throw error;
  }
}

async function archive(legacy, home) {
  const target = await mkdtemp(join(home, '.dark-factory-verification-browser-legacy-'));
  await rename(legacy, join(target, 'profile'));
}

export async function verificationProfile(home = homedir()) {
  const profile = join(home, '.dark-factory-verification-browser');
  const legacy = join(home, '.dark-factory', 'verification-browser');
  const [hasProfile, hasLegacy] = await Promise.all([directory(profile), directory(legacy)]);
  if (hasLegacy) {
    if (hasProfile) await archive(legacy, home);
    else await rename(legacy, profile);
  }
  await mkdir(profile, { recursive: true, mode: 0o700 });
  return profile;
}
