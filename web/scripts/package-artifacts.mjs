import { createHash } from "node:crypto";
import { execFileSync } from "node:child_process";
import {
  copyFileSync,
  existsSync,
  lstatSync,
  mkdirSync,
  mkdtempSync,
  readFileSync,
  readdirSync,
  realpathSync,
  renameSync,
  rmSync,
  writeFileSync,
} from "node:fs";
import { basename, dirname, join, resolve, sep } from "node:path";
import { tmpdir, userInfo } from "node:os";
import { fileURLToPath } from "node:url";

const webRoot = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const repositoryRoot = resolve(webRoot, "..");
const manifestName = "dark-factory-public-artifacts.json";
const gitPath = "/usr/bin/git";
const tarPath = "/usr/bin/tar";
const toolchainIntegrityPath = join(webRoot, "toolchain-integrity.json");
const maxToolTreeFiles = 4096;
const maxToolTreeBytes = 128 * 1024 * 1024;
const maxToolTreePathBytes = 4096;
const maxArchiveBytes = 2 * 1024 * 1024;
const maxArchiveMemberBytes = 512 * 1024;
const maxArchiveTotalBytes = 2 * 1024 * 1024;
const maxArchiveMembers = 64;
const maxArchiveListingBytes = 64 * 1024;
const maxJsonBytes = 64 * 1024;
const packages = {
  client: { key: "client", name: "@dark-factory/client", root: join(webRoot, "packages", "client") },
  ui: { key: "ui", name: "@dark-factory/ui", root: join(webRoot, "packages", "ui") },
};

// This is the deliberate public artifact surface. Adding a source export or
// build output requires an inventory change, so stale and accidental files
// cannot silently enter an artifact.
const inventory = {
  client: [
    "control.d.ts", "control.js", "errors.d.ts", "errors.js", "index.d.ts", "index.js",
    "manifest.d.ts", "manifest.js", "remote/index.d.ts", "remote/index.js",
    "remote/invitation.d.ts", "remote/invitation.js", "remote/manager.d.ts", "remote/manager.js",
    "remote/relay-socket.d.ts", "remote/relay-socket.js", "remote/store.d.ts", "remote/store.js",
    "remote/tokens.d.ts", "remote/tokens.js", "session.d.ts", "session.js", "state.d.ts", "state.js",
    "terminal.d.ts", "terminal.js", "terminal_session.d.ts", "terminal_session.js",
    "transcript.d.ts", "transcript.js", "provenance.json",
  ],
  ui: [
    "console-screens.d.ts", "console-screens.js", "console-sidebar.d.ts", "console-sidebar.js",
    "console-view.d.ts", "console-view.js",
    "factory-app-controller.d.ts", "factory-app-controller.js", "factory-app.d.ts", "factory-app.js",
    "factory-console.css", "factory-console.d.ts", "factory-console.js",
    "factory-scene/factory-scene.d.ts", "factory-scene/factory-scene.js",
    "factory-scene/scene.d.ts", "factory-scene/scene.js", "index.d.ts", "index.js",
    "remote-invite.d.ts", "remote-invite.js",
    "remote/remote-app.d.ts", "remote/remote-app.js", "remote/remote-view.d.ts", "remote/remote-view.js",
    "terminal-controller.d.ts", "terminal-controller.js", "xterm-terminal.d.ts", "xterm-terminal.js",
    "provenance.json",
  ],
};

const commandEnvironment = {
  CI: "true",
  HOME: tmpdir(),
  LANG: "C",
  LC_ALL: "C",
  TMPDIR: tmpdir(),
  GIT_CONFIG_GLOBAL: "/dev/null",
  GIT_CONFIG_SYSTEM: "/dev/null",
  GIT_CONFIG_NOSYSTEM: "1",
  GIT_TERMINAL_PROMPT: "0",
  GIT_PAGER: "cat",
  npm_config_ignore_scripts: "true",
  npm_config_offline: "true",
  npm_config_registry: "http://127.0.0.1:9/",
  npm_config_update_notifier: "false",
};

function fail(message) {
  throw new Error(`package artifacts: ${message}`);
}

function readJson(path) {
  try {
    const bytes = boundedFile(path, maxJsonBytes, `JSON ${path}`);
    const text = bytes.toString("utf8");
    if (!Buffer.from(text, "utf8").equals(bytes)) fail(`JSON ${path} is not valid UTF-8`);
    return JSON.parse(text);
  } catch (error) {
    fail(`could not read JSON ${path}: ${error.message}`);
  }
}

function inspectExecutable(path, label, expected) {
  let target;
  try {
    target = realpathSync(path);
    const stat = lstatSync(target);
    const uid = typeof process.getuid === "function" ? process.getuid() : -1;
    if (!stat.isFile() || (stat.mode & 0o111) === 0 || (uid !== -1 && stat.uid !== uid && stat.uid !== 0)) {
      fail(`${label} is not a trusted executable`);
    }
    const mode = (stat.mode & 0o7777).toString(8).padStart(4, "0");
    const bytes = readFileSync(target);
    const sha512 = createHash("sha512").update(bytes).digest("hex");
    if (expected && (bytes.length !== expected.bytes || mode !== expected.mode || sha512 !== expected.sha512)) {
      fail(`${label} content differs from reviewed toolchain`);
    }
    return { path: target, bytes: bytes.length, mode, sha512 };
  } catch (error) {
    if (error.message.startsWith("package artifacts:")) throw error;
    fail(`could not resolve ${label}: ${error.message}`);
  }
}

function executable(path, label) {
  return inspectExecutable(path, label).path;
}

function git(...args) {
  return execFileSync(executable(gitPath, "git"), args, {
    cwd: repositoryRoot,
    env: { ...commandEnvironment, GIT_OPTIONAL_LOCKS: "0" },
    encoding: "utf8",
  }).trim();
}

function currentSource() {
  const commit = git("rev-parse", "--verify", "HEAD^{commit}");
  if (git("status", "--porcelain=v1", "--untracked-files=all")) {
    fail("source worktree is dirty; commit or remove all changes first");
  }
  return commit;
}

function packageJson(packageInfo) {
  return readJson(join(packageInfo.root, "package.json"));
}

function strictJsonText(text, label, allowMissingFinalNewline = false) {
  if (Buffer.byteLength(text, "utf8") > maxJsonBytes) fail(`${label} is too large`);
  let value;
  try { value = JSON.parse(text); } catch (error) { fail(`${label} is invalid JSON: ${error.message}`); }
  // Canonical re-serialization also rejects duplicate keys: JSON.parse drops
  // the duplicate, making the original bytes differ from the canonical form.
  const canonical = stableJson(value);
  if (canonical !== text && (!allowMissingFinalNewline || canonical.slice(0, -1) !== text)) fail(`${label} is not canonical JSON`);
  return value;
}

function strictJsonFile(path, label) {
  return strictJsonBuffer(boundedFile(path, maxJsonBytes, label), label);
}

function exactKeys(value, keys, label) {
  if (!value || typeof value !== "object" || Array.isArray(value)) fail(`${label} must be an object`);
  const actual = Object.keys(value).sort();
  const expected = [...keys].sort();
  if (JSON.stringify(actual) !== JSON.stringify(expected)) fail(`${label} has unknown or missing keys`);
}

function stringField(value, key, label) {
  if (typeof value[key] !== "string") fail(`${label}.${key} must be a string`);
  return value[key];
}

function integerField(value, key, label) {
  if (!Number.isSafeInteger(value[key])) fail(`${label}.${key} must be an integer`);
  return value[key];
}

function toolTreeDigest(root) {
  let rootStat;
  try { rootStat = lstatSync(root); } catch (error) { fail(`could not inspect tool tree ${root}: ${error.message}`); }
  if (rootStat.isSymbolicLink() || !rootStat.isDirectory()) fail(`tool tree ${root} must be a regular directory`);
  const records = [];
  const foldedPaths = new Set();
  let totalBytes = 0;
  const addRecord = (record) => {
    if (records.length >= maxToolTreeFiles) fail("tool tree contains too many files or directories");
    if (record.relative) {
      const foldedPath = record.relative.normalize("NFC").toLocaleLowerCase("en-US");
      if (foldedPaths.has(foldedPath)) fail(`tool tree contains case-confusable path ${record.relative}`);
      foldedPaths.add(foldedPath);
    }
    records.push(record);
  };
  const walk = (directory, prefix) => {
    let entries;
    try { entries = readdirSync(directory).sort((a, b) => Buffer.compare(Buffer.from(a), Buffer.from(b))); } catch (error) { fail(`could not read tool tree ${directory}: ${error.message}`); }
    const foldedNames = new Set();
    for (const name of entries) {
      if (name === "." || name === ".." || name.normalize("NFC") !== name) fail(`tool tree contains a noncanonical path component ${name}`);
      const foldedName = name.normalize("NFC").toLocaleLowerCase("en-US");
      if (foldedNames.has(foldedName)) fail(`tool tree contains case-confusable entries in ${directory}`);
      foldedNames.add(foldedName);
      const path = join(directory, name);
      const relative = prefix ? `${prefix}/${name}` : name;
      const relativeBytes = Buffer.from(relative);
      if (relativeBytes.length > maxToolTreePathBytes) fail("tool tree path is too long");
      let stat;
      try { stat = lstatSync(path); } catch (error) { fail(`could not inspect tool tree entry ${relative}: ${error.message}`); }
      if (stat.isSymbolicLink()) fail(`tool tree contains symlink ${relative}`);
      if (stat.isDirectory()) {
        addRecord({ relative, relativeBytes, type: "directory", mode: stat.mode & 0o7777 });
        walk(path, relative);
      } else if (stat.isFile()) {
        if (!Number.isSafeInteger(stat.size) || stat.size > maxToolTreeBytes || totalBytes > maxToolTreeBytes - stat.size) fail("tool tree is too large");
        totalBytes += stat.size;
        addRecord({ path, relative, relativeBytes, type: "file", mode: stat.mode & 0o7777, size: stat.size });
      } else {
        fail(`tool tree contains non-regular entry ${relative}`);
      }
    }
  };
  addRecord({ relative: "", relativeBytes: Buffer.alloc(0), type: "directory", mode: rootStat.mode & 0o7777 });
  walk(root, "");
  records.sort((a, b) => Buffer.compare(a.relativeBytes, b.relativeBytes) || a.type.localeCompare(b.type));
  const hash = createHash("sha512");
  const count = Buffer.alloc(4);
  count.writeUInt32BE(records.length);
  hash.update(count);
  for (const record of records) {
    const length = Buffer.alloc(4);
    length.writeUInt32BE(record.relativeBytes.length);
    hash.update(length);
    hash.update(record.relativeBytes);
    hash.update(Buffer.from(record.type === "directory" ? "d" : "f"));
    const mode = Buffer.alloc(4);
    mode.writeUInt32BE(record.mode);
    hash.update(mode);
    if (record.type === "directory") continue;
    const bytes = readFileSync(record.path);
    if (bytes.length !== record.size) fail(`tool tree file changed while hashing ${record.relative}`);
    const size = Buffer.alloc(8);
    size.writeBigUInt64BE(BigInt(record.size));
    hash.update(size);
    hash.update(bytes);
  }
  return hash.digest("hex");
}

function reviewedToolchain() {
  const value = strictJsonFile(toolchainIntegrityPath, "toolchain integrity");
  exactKeys(value, ["node", "pnpm", "typescript", "tar"], "toolchain integrity");
  exactKeys(value.node, ["version", "sha512", "bytes", "mode"], "toolchain integrity.node");
  exactKeys(value.pnpm, ["version", "treeSha512"], "toolchain integrity.pnpm");
  exactKeys(value.typescript, ["version", "treeSha512", "lockfileIntegrity"], "toolchain integrity.typescript");
  exactKeys(value.tar, ["version", "sha512", "bytes", "mode"], "toolchain integrity.tar");
  for (const key of ["version", "sha512", "mode"]) stringField(value.node, key, "toolchain integrity.node");
  integerField(value.node, "bytes", "toolchain integrity.node");
  for (const key of ["version", "treeSha512"]) stringField(value.pnpm, key, "toolchain integrity.pnpm");
  for (const key of ["version", "treeSha512", "lockfileIntegrity"]) stringField(value.typescript, key, "toolchain integrity.typescript");
  for (const key of ["version", "sha512", "mode"]) stringField(value.tar, key, "toolchain integrity.tar");
  integerField(value.tar, "bytes", "toolchain integrity.tar");
  if (!/^\d+\.\d+\.\d+$/.test(value.node.version) || !/^[0-9a-f]{128}$/.test(value.node.sha512) || value.node.bytes <= 0 || value.node.mode !== "0755") fail("toolchain integrity.node is invalid");
  if (!/^\d+\.\d+\.\d+$/.test(value.pnpm.version) || !/^[0-9a-f]{128}$/.test(value.pnpm.treeSha512)) fail("toolchain integrity.pnpm is invalid");
  if (!/^\d+\.\d+\.\d+$/.test(value.typescript.version) || !/^[0-9a-f]{128}$/.test(value.typescript.treeSha512) || !/^sha512-[A-Za-z0-9+/]{86}={0,2}$/.test(value.typescript.lockfileIntegrity)) fail("toolchain integrity.typescript is invalid");
  if (!/^[0-9a-f]{128}$/.test(value.tar.sha512) || value.tar.bytes <= 0 || value.tar.mode !== "0755") fail("toolchain integrity.tar is invalid");
  return value;
}

function lockfileTypescriptIntegrity(version) {
  const lockfile = readFileSync(join(webRoot, "pnpm-lock.yaml"), "utf8");
  const marker = `  typescript@${version}:\n`;
  const start = lockfile.indexOf(marker);
  if (start < 0) fail(`pnpm lockfile has no exact typescript@${version} package entry`);
  const remainder = lockfile.slice(start + marker.length);
  const nextPackage = remainder.search(/\n  \S/);
  const block = lockfile.slice(start, start + marker.length + (nextPackage < 0 ? remainder.length : nextPackage));
  const matches = [...block.matchAll(/^    resolution: \{integrity: (sha512-[^}]+)\}$/gm)];
  if (matches.length !== 1) fail(`pnpm lockfile has an invalid typescript@${version} package entry`);
  return matches[0][1];
}

function pnpmRoot(version) {
  let home;
  try { home = userInfo().homedir; } catch (error) { fail(`could not determine the current user's home: ${error.message}`); }
  return join(home, ".cache", "node", "corepack", "v1", "pnpm", version);
}

function trustedTools() {
  const reviewed = reviewedToolchain();
  const nodeInfo = inspectExecutable(process.execPath, "Node", reviewed.node);
  const node = nodeInfo.path;
  if (process.version !== `v${reviewed.node.version}`) fail("Node version differs from reviewed toolchain");
  const webPackage = readJson(join(webRoot, "package.json"));
  const packageManager = webPackage.packageManager?.match(/^pnpm@(\d+\.\d+\.\d+)$/);
  const typescript = webPackage.devDependencies?.typescript;
  if (!packageManager || !/^\d+\.\d+\.\d+$/.test(typescript ?? "")) fail("package/tool versions must be exact");
  if (packageManager[1] !== reviewed.pnpm.version) fail("package.json pnpm version differs from reviewed toolchain");
  if (typescript !== reviewed.typescript.version) fail("package.json TypeScript version differs from reviewed toolchain");
  const pnpmRootPath = pnpmRoot(packageManager[1]);
  const pnpmPackagePath = join(pnpmRootPath, "package.json");
  const pnpmPackage = readJson(pnpmPackagePath);
  if (pnpmPackage.name !== "pnpm" || pnpmPackage.version !== packageManager[1]) fail("cached pnpm differs from web/package.json");
  const pnpmTreeSha512 = toolTreeDigest(pnpmRootPath);
  if (pnpmTreeSha512 !== reviewed.pnpm.treeSha512) fail("cached pnpm content differs from reviewed toolchain");
  if (lockfileTypescriptIntegrity(typescript) !== reviewed.typescript.lockfileIntegrity) fail("pnpm lockfile TypeScript integrity differs from reviewed toolchain");
  const tscPackagePath = realpathSync(join(webRoot, "node_modules", "typescript", "package.json"));
  const nodeModulesRoot = realpathSync(join(webRoot, "node_modules"));
  if (!tscPackagePath.startsWith(`${nodeModulesRoot}${sep}`)) fail("TypeScript resolves outside the installed web dependency tree");
  const tscRoot = dirname(tscPackagePath);
  const tsc = executable(join(tscRoot, "bin", "tsc"), "TypeScript");
  const tscPackage = readJson(tscPackagePath);
  if (tscPackage.name !== "typescript" || tscPackage.version !== typescript) fail("installed TypeScript differs from web/package.json");
  const typescriptTreeSha512 = toolTreeDigest(tscRoot);
  if (typescriptTreeSha512 !== reviewed.typescript.treeSha512) fail("installed TypeScript content differs from reviewed toolchain");
  const safeEnv = { ...commandEnvironment, PATH: `${dirname(node)}:/usr/bin:/bin` };
  const pnpm = executable(join(pnpmRootPath, "bin", "pnpm.mjs"), "pnpm");
  const pnpmVersion = execFileSync(node, [pnpm, "--version"], { env: safeEnv, encoding: "utf8" }).trim();
  const tscVersion = execFileSync(node, [tsc, "--version"], { cwd: webRoot, env: safeEnv, encoding: "utf8" }).trim();
  const tarInfo = inspectExecutable(tarPath, "tar", reviewed.tar);
  const tarVersion = execFileSync(tarInfo.path, ["--version"], { env: safeEnv, encoding: "utf8" }).trim().split("\n")[0];
  if (tarVersion !== reviewed.tar.version) fail("tar version differs from reviewed toolchain");
  if (pnpmVersion !== packageManager[1] || tscVersion !== `Version ${typescript}`) fail("tool executable versions do not match pinned versions");
  return {
    paths: { node, pnpm, pnpmRoot: pnpmRootPath, tsc, typescriptRoot: tscRoot, tar: tarInfo.path },
    expected: { node: reviewed.node, pnpm: reviewed.pnpm, typescript: reviewed.typescript, tar: reviewed.tar },
    versions: {
      node: { version: reviewed.node.version, bytes: nodeInfo.bytes, mode: nodeInfo.mode, sha512: nodeInfo.sha512 },
      pnpm: pnpmVersion,
      pnpmTreeSha512,
      typescript,
      typescriptTreeSha512,
      typescriptIntegrity: reviewed.typescript.lockfileIntegrity,
      tar: { version: tarVersion, bytes: tarInfo.bytes, mode: tarInfo.mode, sha512: tarInfo.sha512 },
    },
  };
}

function assertToolsUnchanged(tools) {
  const node = inspectExecutable(tools.paths.node, "Node", tools.expected.node);
  const tar = inspectExecutable(tools.paths.tar, "tar", tools.expected.tar);
  if (realpathSync(process.execPath) !== tools.paths.node || node.path !== tools.paths.node || tar.path !== tools.paths.tar) fail("trusted tool path changed");
  if (toolTreeDigest(tools.paths.pnpmRoot) !== tools.expected.pnpm.treeSha512) fail("cached pnpm content changed while building");
  if (toolTreeDigest(tools.paths.typescriptRoot) !== tools.expected.typescript.treeSha512) fail("installed TypeScript content changed while building");
}

// The browser contract is unversioned, so an artifact has no protocol identity
// to bind: the exact source commit already names the contract these packages
// were built from.
function sourceIdentity(sourceCommit, tools) {
  return {
    source: { commit: sourceCommit, clean: true },
    buildTools: tools.versions,
  };
}

function stableJson(value) {
  return `${JSON.stringify(value, null, 2)}\n`;
}

function canonicalValue(value) {
  if (Array.isArray(value)) return value.map(canonicalValue);
  if (value && typeof value === "object") return Object.fromEntries(Object.keys(value).sort().map((key) => [key, canonicalValue(value[key])]));
  return value;
}

function writeStableJson(path, value) {
  writeFileSync(path, stableJson(value), { mode: 0o600 });
}

function packageFilename(packageValue) {
  return `${packageValue.name.replace(/^@/, "").replace("/", "-")}-${packageValue.version}.tgz`;
}

function boundedFile(path, limit, label) {
  let stat;
  try { stat = lstatSync(path); } catch (error) { fail(`could not inspect ${label}: ${error.message}`); }
  if (stat.isSymbolicLink() || !stat.isFile()) fail(`${label} is not a regular file`);
  if (!Number.isSafeInteger(stat.size) || stat.size > limit) fail(`${label} is too large`);
  const bytes = readFileSync(path);
  if (bytes.length !== stat.size) fail(`${label} changed while reading`);
  return bytes;
}

function digest(path) {
  const bytes = boundedFile(path, maxArchiveBytes, "archive");
  const hash = createHash("sha512").update(bytes).digest();
  return { bytes: bytes.length, sha512: hash.toString("hex"), integrity: `sha512-${hash.toString("base64")}` };
}

function provenance(packageInfo, identity, dependency) {
  const value = { ...identity, package: { name: packageInfo.name, version: packageJson(packageInfo).version } };
  if (dependency) value.dependency = dependency;
  return value;
}

function expectedPackageJson(packageInfo, dependency) {
  const value = packageJson(packageInfo);
  if (dependency) value.dependencies = { ...value.dependencies, [packages.client.name]: dependency.version };
  return value;
}

function distRoot(packageInfo) {
  return join(packageInfo.root, "dist", "src");
}

function clearBuildOutput(packageInfo) {
  const dist = join(packageInfo.root, "dist");
  if (existsSync(dist) && lstatSync(dist).isSymbolicLink()) fail(`${packageInfo.name} dist is a symlink`);
  rmSync(dist, { recursive: true, force: true });
}

function assertDirectoryInventory(root, expected, label) {
  const expectedFiles = new Set(expected);
  const seen = new Set();
  const walk = (directory, prefix) => {
    for (const entry of readdirSync(directory)) {
      const path = join(directory, entry);
      const relativePath = prefix ? `${prefix}/${entry}` : entry;
      const stat = lstatSync(path);
      if (stat.isSymbolicLink()) fail(`${label} contains symlink ${relativePath}`);
      if (stat.isDirectory()) walk(path, relativePath);
      else if (stat.isFile()) {
        if (!expectedFiles.has(relativePath) || seen.has(relativePath)) fail(`${label} contains unexpected file ${relativePath}`);
        seen.add(relativePath);
      } else fail(`${label} contains non-regular entry ${relativePath}`);
    }
  };
  if (!existsSync(root) || !lstatSync(root).isDirectory()) fail(`${label} is missing`);
  walk(root, "");
  if (seen.size !== expectedFiles.size) fail(`${label} is missing an expected file`);
}

function runTypeScript(tools, packageInfo, config) {
  assertToolsUnchanged(tools);
  execFileSync(tools.paths.node, [tools.paths.tsc, "-p", config], {
    cwd: packageInfo.root,
    env: { ...commandEnvironment, PATH: `${dirname(tools.paths.node)}:/usr/bin:/bin` },
    stdio: "inherit",
  });
  assertToolsUnchanged(tools);
}

function build(tools) {
  clearBuildOutput(packages.client);
  clearBuildOutput(packages.ui);
  runTypeScript(tools, packages.client, "../../tsconfig.build.json");
  runTypeScript(tools, packages.ui, "tsconfig.json");
  copyFileSync(join(packages.ui.root, "src", "factory-console.css"), join(distRoot(packages.ui), "factory-console.css"));
  assertDirectoryInventory(distRoot(packages.client), inventory.client.filter((name) => name !== "provenance.json"), "client build");
  assertDirectoryInventory(distRoot(packages.ui), inventory.ui.filter((name) => name !== "provenance.json"), "UI build");
}

function archiveExpected(info) {
  return new Set(["package/package.json", ...inventory[info.key].map((name) => `package/dist/src/${name}`)]);
}

function canonicalArchivePath(entry) {
  return entry && !entry.endsWith("/") && !entry.startsWith("/") && !entry.includes("\\") && entry.normalize("NFC") === entry && entry.split("/").every((part) => part && part !== "." && part !== "..");
}

function tarEntries(tarPathValue, archive, info) {
  const archiveBytes = boundedFile(archive, maxArchiveBytes, `${info.name} archive`);
  if (archiveBytes.length === 0) fail(`${info.name} archive is empty`);
  let listing;
  try {
    listing = execFileSync(tarPathValue, ["-tf", archive], { encoding: "utf8", env: commandEnvironment, maxBuffer: maxArchiveListingBytes });
  } catch (error) {
    fail(`${info.name} archive listing failed: ${error.message}`);
  }
  const entries = listing.split("\n").filter(Boolean);
  if (entries.length > maxArchiveMembers) fail(`${info.name} archive has too many members`);
  const expected = archiveExpected(info);
  const folded = new Set();
  for (const entry of entries) {
    if (!canonicalArchivePath(entry)) fail(`${info.name} archive contains unsafe path or directory`);
    const foldedPath = entry.toLocaleLowerCase("en-US");
    if (folded.has(foldedPath) || expected.has(entry) && entries.filter((candidate) => candidate === entry).length > 1) fail(`${info.name} archive contains duplicate entries`);
    folded.add(foldedPath);
  }
  if (entries.length !== expected.size || entries.some((entry) => !expected.has(entry))) fail(`${info.name} archive inventory mismatch`);
  let details;
  try {
    details = execFileSync(tarPathValue, ["-tvf", archive], { encoding: "utf8", env: commandEnvironment, maxBuffer: maxArchiveListingBytes });
  } catch (error) {
    fail(`${info.name} archive details failed: ${error.message}`);
  }
  const members = new Map();
  let totalBytes = 0;
  for (const line of details.split("\n").filter(Boolean)) {
    const fields = line.trim().split(/\s+/);
    if (fields.length < 8 || fields[0] !== "-rw-r--r--") fail(`${info.name} archive member is not mode 0644 regular file`);
    const size = Number(fields[4]);
    const entry = fields.slice(8).join(" ");
    if (!expected.has(entry) || members.has(entry) || !Number.isSafeInteger(size)) fail(`${info.name} archive details are invalid`);
    if (size < 0 || size > maxArchiveMemberBytes) fail(`${info.name} archive member is too large`);
    totalBytes += size;
    if (totalBytes > maxArchiveTotalBytes) fail(`${info.name} archive members are too large in total`);
    members.set(entry, size);
  }
  if (members.size !== expected.size) fail(`${info.name} archive details do not match inventory`);
  return members;
}

function tarMember(tarPathValue, archive, member, members) {
  const expectedBytes = members.get(member);
  if (expectedBytes === undefined || expectedBytes > maxArchiveMemberBytes) fail(`archive member ${member} is unavailable or too large`);
  let bytes;
  try {
    bytes = execFileSync(tarPathValue, ["-xOf", archive, member], { env: commandEnvironment, maxBuffer: maxArchiveMemberBytes });
  } catch (error) {
    fail(`could not read archive member ${member}: ${error.message}`);
  }
  if (bytes.length !== expectedBytes) fail(`archive member ${member} changed while reading`);
  return bytes;
}

function strictJsonBuffer(bytes, label, allowMissingFinalNewline = false) {
  const text = bytes.toString("utf8");
  if (!Buffer.from(text, "utf8").equals(bytes)) fail(`${label} is not valid UTF-8`);
  return strictJsonText(text, label, allowMissingFinalNewline);
}

function externalDependencies(packageValue, packageName) {
  for (const section of ["dependencies", "optionalDependencies", "peerDependencies", "devDependencies"]) {
    for (const [name, version] of Object.entries(packageValue[section] ?? {})) {
      if (!/^\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?$/.test(version)) fail(`${packageName} has non-exact dependency ${name}@${version}`);
    }
  }
}

function outputDestination(requested, requireAbsent) {
  const lexicalOutput = resolve(requested);
  const repositoryReal = realpathSync(repositoryRoot);
  const parent = dirname(lexicalOutput);
  let parentReal;
  try { parentReal = realpathSync(parent); } catch (error) { fail(`output parent is unavailable: ${error.message}`); }
  const output = join(parentReal, basename(lexicalOutput));
  if (output === repositoryReal || output.startsWith(`${repositoryReal}${sep}`)) fail("output must be outside the source tree");
  let existing;
  try { existing = lstatSync(output); } catch (error) { if (error.code !== "ENOENT") fail(`could not inspect output: ${error.message}`); }
  if (existing) {
    if (existing.isSymbolicLink()) fail("output must not be a symlink");
    if (requireAbsent) fail("output already exists");
  }
  return output;
}

function validateManifestShape(manifest) {
  exactKeys(manifest, ["source", "buildTools", "packages"], "manifest");
  exactKeys(manifest.source, ["commit", "clean"], "manifest.source");
  if (!/^[0-9a-f]{40}$/.test(stringField(manifest.source, "commit", "manifest.source")) || manifest.source.clean !== true) fail("manifest source is invalid");
  exactKeys(manifest.buildTools, ["node", "pnpm", "pnpmTreeSha512", "typescript", "typescriptTreeSha512", "typescriptIntegrity", "tar"], "manifest.buildTools");
  exactKeys(manifest.buildTools.node, ["version", "bytes", "mode", "sha512"], "manifest.buildTools.node");
  exactKeys(manifest.buildTools.tar, ["version", "bytes", "mode", "sha512"], "manifest.buildTools.tar");
  for (const key of ["version", "mode", "sha512"]) stringField(manifest.buildTools.node, key, "manifest.buildTools.node");
  for (const key of ["version", "mode", "sha512"]) stringField(manifest.buildTools.tar, key, "manifest.buildTools.tar");
  integerField(manifest.buildTools.node, "bytes", "manifest.buildTools.node");
  integerField(manifest.buildTools.tar, "bytes", "manifest.buildTools.tar");
  stringField(manifest.buildTools, "pnpm", "manifest.buildTools");
  stringField(manifest.buildTools, "pnpmTreeSha512", "manifest.buildTools");
  stringField(manifest.buildTools, "typescript", "manifest.buildTools");
  stringField(manifest.buildTools, "typescriptTreeSha512", "manifest.buildTools");
  stringField(manifest.buildTools, "typescriptIntegrity", "manifest.buildTools");
  if (!/^[0-9a-f]{128}$/.test(manifest.buildTools.node.sha512) || manifest.buildTools.node.bytes <= 0 || manifest.buildTools.node.mode !== "0755" || !/^[0-9a-f]{128}$/.test(manifest.buildTools.tar.sha512) || manifest.buildTools.tar.bytes <= 0 || manifest.buildTools.tar.mode !== "0755" || !/^[0-9a-f]{128}$/.test(manifest.buildTools.pnpmTreeSha512) || !/^[0-9a-f]{128}$/.test(manifest.buildTools.typescriptTreeSha512) || !/^sha512-[A-Za-z0-9+/]{86}={0,2}$/.test(manifest.buildTools.typescriptIntegrity)) fail("manifest build tool content digests are invalid");
  exactKeys(manifest.packages, [packages.client.name, packages.ui.name], "manifest.packages");
  for (const info of Object.values(packages)) {
    const entry = manifest.packages[info.name];
    exactKeys(entry, info === packages.ui ? ["source", "buildTools", "package", "dependency", "artifact"] : ["source", "buildTools", "package", "artifact"], `${info.name} provenance`);
    exactKeys(entry.package, ["name", "version"], `${info.name}.package`);
    exactKeys(entry.artifact, ["bytes", "sha512", "integrity", "filename", "version"], `${info.name}.artifact`);
    integerField(entry.artifact, "bytes", `${info.name}.artifact`);
    if (entry.artifact.bytes <= 0 || !/^[0-9a-f]{128}$/.test(entry.artifact.sha512) || !/^sha512-[A-Za-z0-9+/]{86}={0,2}$/.test(entry.artifact.integrity)) fail(`${info.name} artifact digest is invalid`);
    for (const key of ["sha512", "integrity", "filename", "version"]) stringField(entry.artifact, key, `${info.name}.artifact`);
    if (info === packages.ui) {
      exactKeys(entry.dependency, ["name", "version", "tarball", "integrity"], `${info.name}.dependency`);
      for (const key of ["name", "version", "tarball", "integrity"]) stringField(entry.dependency, key, `${info.name}.dependency`);
    }
  }
}

function outputFilenames() {
  return new Set([packageFilename(packageJson(packages.client)), packageFilename(packageJson(packages.ui)), manifestName]);
}

function assertExactOutputDirectory(output, expectedFilenames, label) {
  if (!existsSync(output) || lstatSync(output).isSymbolicLink() || !lstatSync(output).isDirectory() || realpathSync(output) !== output) fail(`${label} must be an exact non-symlink directory`);
  const names = readdirSync(output);
  if (names.length !== expectedFilenames.size || names.some((name) => !expectedFilenames.has(name))) fail(`${label} contains stale or unexpected files`);
  for (const name of names) if (!lstatSync(join(output, name)).isFile()) fail(`${label} contains a symlink or non-regular file`);
}

function compareArtifactDirectories(actual, expected, expectedFilenames) {
  assertExactOutputDirectory(actual, expectedFilenames, "artifact output");
  assertExactOutputDirectory(expected, expectedFilenames, "constructed output");
  for (const name of expectedFilenames) {
    const actualBytes = boundedFile(join(actual, name), maxArchiveBytes, `artifact ${name}`);
    const expectedBytes = boundedFile(join(expected, name), maxArchiveBytes, `constructed artifact ${name}`);
    if (!actualBytes.equals(expectedBytes)) fail(`artifact ${name} differs from clean reconstruction`);
  }
}

function validateArtifactSet(output, manifest, tools, identity) {
  validateManifestShape(manifest);
  if (JSON.stringify(manifest.source) !== JSON.stringify(identity.source) || JSON.stringify(manifest.buildTools) !== JSON.stringify(identity.buildTools)) fail("constructed manifest identity is stale");
  for (const info of Object.values(packages)) {
    const entry = manifest.packages[info.name];
    if (JSON.stringify(entry.source) !== JSON.stringify(identity.source) || JSON.stringify(entry.buildTools) !== JSON.stringify(identity.buildTools)) fail(`${info.name} constructed provenance identity is stale`);
    const sourcePackage = packageJson(info);
    if (entry.package.name !== info.name || entry.package.version !== sourcePackage.version || entry.artifact.filename !== packageFilename(sourcePackage) || entry.artifact.version !== sourcePackage.version) fail(`${info.name} constructed package identity is stale`);
    if (info === packages.ui && (entry.dependency.name !== packages.client.name || entry.dependency.version !== manifest.packages[packages.client.name].package.version || entry.dependency.tarball !== manifest.packages[packages.client.name].artifact.filename || entry.dependency.integrity !== manifest.packages[packages.client.name].artifact.integrity)) fail("constructed UI does not bind the exact client artifact and version");
    const archive = join(output, entry.artifact.filename);
    const actual = digest(archive);
    if (actual.bytes !== entry.artifact.bytes || actual.sha512 !== entry.artifact.sha512 || actual.integrity !== entry.artifact.integrity) fail(`${info.name} constructed tarball integrity failed`);
    const members = tarEntries(tools.paths.tar, archive, info);
    const packedPackage = strictJsonBuffer(tarMember(tools.paths.tar, archive, "package/package.json", members), `${info.name} packed package.json`, true);
    const expectedPackage = expectedPackageJson(info, info === packages.ui ? entry.dependency : undefined);
    if (JSON.stringify(canonicalValue(packedPackage)) !== JSON.stringify(canonicalValue(expectedPackage))) fail(`${info.name} constructed package metadata is stale`);
    externalDependencies(packedPackage, info.name);
    const embedded = strictJsonBuffer(tarMember(tools.paths.tar, archive, "package/dist/src/provenance.json", members), `${info.name} embedded provenance`);
    if (JSON.stringify(embedded) !== JSON.stringify(provenance(info, identity, info === packages.ui ? entry.dependency : undefined))) fail(`${info.name} constructed provenance failed`);
  }
}

function validateCandidateArchives(output, tools) {
  for (const info of Object.values(packages)) {
    const archive = join(output, packageFilename(packageJson(info)));
    const members = tarEntries(tools.paths.tar, archive, info);
    strictJsonBuffer(tarMember(tools.paths.tar, archive, "package/package.json", members), `${info.name} packed package.json`, true);
    strictJsonBuffer(tarMember(tools.paths.tar, archive, "package/dist/src/provenance.json", members), `${info.name} embedded provenance`);
  }
  const uiArchive = join(output, packageFilename(packageJson(packages.ui)));
  const uiMembers = tarEntries(tools.paths.tar, uiArchive, packages.ui);
  strictJsonBuffer(tarMember(tools.paths.tar, uiArchive, "package/package.json", uiMembers), "UI final packed package.json", true);
}

async function constructArtifacts(destination, tools, identity) {
  if (existsSync(destination)) fail("private artifact destination already exists");
  mkdirSync(destination);
  const stageRoot = mkdtempSync(join(dirname(destination), ".dark-factory-public-stage-"));
  try {
    build(tools);
    if (currentSource() !== identity.source.commit) fail("source changed while constructing artifacts");
    writeStableJson(join(distRoot(packages.client), "provenance.json"), provenance(packages.client, identity));
    const clientArtifact = stageAndPack(tools, packages.client, destination, stageRoot);
    const clientDependency = { name: packages.client.name, version: clientArtifact.version, tarball: clientArtifact.filename, integrity: clientArtifact.integrity };
    writeStableJson(join(distRoot(packages.ui), "provenance.json"), provenance(packages.ui, identity, clientDependency));
    const uiArtifact = stageAndPack(tools, packages.ui, destination, stageRoot, clientDependency);
    const manifest = {
      ...identity,
      packages: {
        [packages.client.name]: { ...provenance(packages.client, identity), artifact: clientArtifact },
        [packages.ui.name]: { ...provenance(packages.ui, identity, clientDependency), artifact: uiArtifact },
      },
    };
    writeStableJson(join(destination, manifestName), manifest);
    assertToolsUnchanged(tools);
    validateArtifactSet(destination, manifest, tools, identity);
    assertToolsUnchanged(tools);
    return manifest;
  } finally {
    rmSync(stageRoot, { recursive: true, force: true });
  }
}

async function verify(requested) {
  const output = outputDestination(requested, false);
  assertExactOutputDirectory(output, outputFilenames(), "artifact output");
  const supplied = strictJsonFile(join(output, manifestName), "artifact manifest");
  validateManifestShape(supplied);
  const tools = trustedTools();
  assertToolsUnchanged(tools);
  validateCandidateArchives(output, tools);
  assertToolsUnchanged(tools);
  const sourceCommit = currentSource();
  const identity = sourceIdentity(sourceCommit, tools);
  const transaction = mkdtempSync(join(dirname(output), ".dark-factory-public-verify-"));
  const expected = join(transaction, "expected");
  try {
    await constructArtifacts(expected, tools, identity);
    compareArtifactDirectories(output, expected, outputFilenames());
    return supplied;
  } finally {
    rmSync(transaction, { recursive: true, force: true });
  }
}

async function pack(requested) {
  const output = outputDestination(requested, true);
  const tools = trustedTools();
  const sourceCommit = currentSource();
  const identity = sourceIdentity(sourceCommit, tools);
  const transaction = mkdtempSync(join(dirname(output), ".dark-factory-public-artifacts-"));
  const first = join(transaction, "first");
  const second = join(transaction, "second");
  try {
    const firstManifest = await constructArtifacts(first, tools, identity);
    assertToolsUnchanged(tools);
    await constructArtifacts(second, tools, identity);
    compareArtifactDirectories(first, second, outputFilenames());
    if (existsSync(output)) fail("output appeared before atomic publish");
    renameSync(first, output);
    return firstManifest;
  } finally {
    rmSync(transaction, { recursive: true, force: true });
  }
}

function stageAndPack(tools, info, artifactDir, stageRoot, dependency) {
  const sourcePackage = expectedPackageJson(info, dependency);
  const stage = join(stageRoot, info.key);
  mkdirSync(join(stage, "dist", "src"), { recursive: true });
  assertDirectoryInventory(distRoot(info), inventory[info.key], `${info.name} source build`);
  for (const filename of inventory[info.key]) {
    const destination = join(stage, "dist", "src", filename);
    mkdirSync(dirname(destination), { recursive: true });
    copyFileSync(join(distRoot(info), filename), destination);
  }
  writeStableJson(join(stage, "package.json"), sourcePackage);
  const filename = packageFilename(sourcePackage);
  const archive = join(artifactDir, filename);
  if (existsSync(archive)) fail(`stale ${filename} already exists`);
  // npm_config_ignore_scripts is the lifecycle policy boundary for packing.
  assertToolsUnchanged(tools);
  execFileSync(tools.paths.node, [tools.paths.pnpm, "pack", "--pack-destination", artifactDir], {
    cwd: stage,
    env: { ...commandEnvironment, PATH: `${dirname(tools.paths.node)}:/usr/bin:/bin` },
    stdio: "pipe",
  });
  assertToolsUnchanged(tools);
  if (!existsSync(archive) || lstatSync(archive).isSymbolicLink()) fail(`${filename} was not produced as a regular file`);
  return { ...digest(archive), filename, version: sourcePackage.version };
}

function parseArgs(argv) {
  const [command, ...rest] = argv;
  if (rest.some((value) => value === "--source-sha" || value.startsWith("--source-sha="))) fail("source SHA is derived from clean HEAD and cannot be caller-selected");
  if (command !== "pack" && command !== "verify") fail("usage: package-artifacts.mjs <pack|verify> --output <directory>");
  if (rest.length !== 2 || rest[0] !== "--output" || !rest[1]) fail("usage: package-artifacts.mjs <pack|verify> --output <directory>");
  return { command, output: rest[1] };
}

async function main(argv) {
  const { command, output } = parseArgs(argv);
  return command === "pack" ? pack(output) : verify(output);
}

// This module is intentionally import-only. The pre-Node launcher imports
// main through a fixed eval entry after validating the Node executable.
if (process.argv[1] && !process.argv[1].startsWith("file:") && realpathSync(process.argv[1]) === realpathSync(fileURLToPath(import.meta.url))) {
  process.stderr.write("package artifacts: use the package-artifacts launcher\n");
  process.exitCode = 1;
}

// Test-only exports; pack/verify always derive and validate their production
// tool roots internally and never accept these as caller input.
export { assertDirectoryInventory, inspectExecutable, inventory, main, toolTreeDigest };
