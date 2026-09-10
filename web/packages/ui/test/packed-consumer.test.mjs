import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import { cpSync, existsSync, mkdtempSync, readFileSync, realpathSync, rmSync, writeFileSync } from "node:fs";
import { delimiter, dirname, join } from "node:path";
import { tmpdir } from "node:os";
import { fileURLToPath } from "node:url";
import test from "node:test";

test("packed UI is importable by a clean consumer with its stylesheet export", () => {
  const packageRoot = join(dirname(fileURLToPath(import.meta.url)), "..");
  const clientRoot = join(packageRoot, "..", "client");
  const webRoot = join(packageRoot, "..", "..");
  const consumer = mkdtempSync(join(tmpdir(), "dark-factory-ui-consumer-"));
  try {
    const env = {
      ...process.env,
      PATH: [dirname(process.execPath), "/usr/bin", "/bin"].join(delimiter),
      npm_config_cache: join(consumer, "npm-cache"),
      npm_config_update_notifier: "false",
    };
    const packLocal = (cwd) => {
      execFileSync("corepack", ["pnpm", "pack", "--pack-destination", consumer], { cwd, stdio: "pipe", env });
    };
    const packNpm = (cwd) => {
      const output = execFileSync("npm", ["pack", "--json", "--pack-destination", consumer], { cwd, stdio: "pipe", env, encoding: "utf8" });
      return join(consumer, JSON.parse(output)[0].filename);
    };
    packLocal(clientRoot);
    packLocal(packageRoot);
    const clientTarball = join(consumer, "dark-factory-client-0.1.0.tgz");
    const uiTarball = join(consumer, "dark-factory-ui-0.1.0.tgz");
    const reactTarball = packNpm(join(packageRoot, "node_modules", "react"));
    const xtermTarball = packNpm(join(packageRoot, "node_modules", "@xterm", "xterm"));
    const fitTarball = packNpm(join(packageRoot, "node_modules", "@xterm", "addon-fit"));
    writeFileSync(join(consumer, "package.json"), JSON.stringify({ name: "clean-ui-consumer", private: true, type: "module" }));
    execFileSync("npm", ["install", "--offline", "--ignore-scripts", "--no-package-lock", clientTarball, uiTarball, reactTarball, xtermTarball, fitTarball], { cwd: consumer, stdio: "pipe", env });

    const packageSources = new Map([
      ["react-dom", join(packageRoot, "node_modules", "react-dom")],
      ["scheduler", join(webRoot, "node_modules", ".pnpm", "node_modules", "scheduler")],
    ]);
    for (const [name, packagePath] of packageSources) {
      const source = realpathSync(packagePath);
      const target = join(consumer, "node_modules", name);
      if (!existsSync(target)) cpSync(source, target, { recursive: true });
    }
    const probe = join(consumer, "probe.mjs");
    writeFileSync(probe, "import { createElement } from 'react'; import { renderToStaticMarkup } from 'react-dom/server'; import { FactoryApp, FactoryConsole } from '@dark-factory/ui'; const css = await import.meta.resolve('@dark-factory/ui/styles.css'); const xtermCss = await import.meta.resolve('@xterm/xterm/css/xterm.css'); if (typeof FactoryApp !== 'function' || FactoryApp.length !== 0 || typeof FactoryConsole !== 'function' || !css.endsWith('/factory-console.css') || !xtermCss.endsWith('/css/xterm.css')) throw new Error('bad UI package exports'); if (typeof renderToStaticMarkup(createElement(FactoryApp)) !== 'string') throw new Error('SSR failed');");
    execFileSync(process.execPath, [probe], { cwd: consumer, stdio: "pipe" });
    const installedManifest = JSON.parse(readFileSync(join(consumer, "node_modules", "@dark-factory", "ui", "package.json"), "utf8"));
    assert.deepEqual(installedManifest.peerDependencies, { react: "19.1.0", "@xterm/addon-fit": "0.11.0", "@xterm/xterm": "6.0.0" });
    assert.equal(installedManifest.dependencies["@xterm/xterm"], undefined);
    assert.equal(installedManifest.dependencies["@xterm/addon-fit"], undefined);
    assert.equal(JSON.parse(readFileSync(join(consumer, "node_modules", "@xterm", "xterm", "package.json"), "utf8")).version, "6.0.0");
    assert.equal(JSON.parse(readFileSync(join(consumer, "node_modules", "@xterm", "addon-fit", "package.json"), "utf8")).version, "0.11.0");
    assert.equal(JSON.parse(readFileSync(join(consumer, "node_modules", "react", "package.json"), "utf8")).version, "19.1.0");
    const installedRoot = join(consumer, "node_modules", "@dark-factory", "ui", "dist", "src");
    assert.equal(existsSync(join(installedRoot, "index.d.ts")), true);
    assert.match(readFileSync(join(installedRoot, "factory-app.js"), "utf8"), /^"use client";/);
    const rootTypes = readFileSync(join(installedRoot, "index.d.ts"), "utf8");
    assert.equal(/browserPort|controllerFactory|FactoryAppLifecycle|TerminalController|TerminalSurface|XtermTerminal/.test(rootTypes), false);
    assert.match(readFileSync(join(consumer, "node_modules", "@dark-factory", "ui", "dist", "src", "factory-console.css"), "utf8"), /\.dfFactoryConsole\b/);
  } finally {
    rmSync(consumer, { recursive: true, force: true });
  }
});
