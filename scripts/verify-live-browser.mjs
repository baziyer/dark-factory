#!/usr/bin/env node
// Operator-owned browser profile: no user browser session or stored login is touched.
import { mkdir } from 'node:fs/promises';
import { homedir } from 'node:os';
import { join } from 'node:path';
import { createRequire } from 'node:module';

const require = createRequire(join(process.env.DARK_FACTORY_SITE || join(homedir(), 'dark-factory-site'), 'package.json'));
const { chromium, expect } = require('@playwright/test');
const profile = join(homedir(), '.dark-factory-verification-browser');
await mkdir(profile, { recursive: true, mode: 0o700 });
let context;
try {
  context = await chromium.launchPersistentContext(profile, { headless: true, viewport: { width: 1440, height: 900 } });
  await context.grantPermissions(['local-network-access'], { origin: 'https://app.darkfactory.build' });
  const page = await context.newPage();
  let pageFailed = false;
  page.on('pageerror', () => { pageFailed = true; });
  await page.goto('https://app.darkfactory.build/factory', { waitUntil: 'domcontentloaded' });
  const consoleView = page.getByRole('group', { name: 'Left view', exact: true });
  const pairLink = page.getByRole('link', { name: 'PAIR THIS BROWSER', exact: true });
  const counters = page.getByLabel('Factory counters', { exact: true });
  await expect.poll(async () => await pairLink.isVisible() || /^ACTIVE RUNS\s*\d+$/.test(await counters.innerText()), { timeout: 45000 }).toBe(true);
  if (await pairLink.isVisible()) {
    await pairLink.click();
    await page.getByRole('button', { name: 'PAIR THIS BROWSER', exact: true }).click();
  }
  await consoleView.waitFor({ state: 'visible', timeout: 45000 });
  await expect(counters).toHaveText(/^ACTIVE RUNS\s*\d+$/, { timeout: 45000 });
  await page.getByRole('button', { name: 'SETTINGS', exact: true }).click();
  await page.getByRole('dialog', { name: 'Settings', exact: true }).getByRole('button', { name: 'REFRESH ACCOUNTS', exact: true }).waitFor({ state: 'visible' });
  if (pageFailed) throw new Error('browser error');
  process.stdout.write(JSON.stringify({ healthy: true }) + '\n');
} catch {
  // Browser errors can contain pairing URLs: expose only a finite result.
  process.stderr.write('live browser verification failed\n');
  process.exitCode = 1;
} finally {
  await context?.close();
}
