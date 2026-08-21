# Development workflow

## Freeze and scope

Dark Factory is in a three-stage safe-kernel refactor. Do not start provider
work, install or release a refactor revision, enable dispatch, modify
`~/.dark-factory`, load or alter the installed launchd job, or delete preserved
worktrees. Stage-specific isolated fixtures are allowed only with a temporary
home, explicit socket, exact resource identities, and an independent reaper.

## Day to day

1. Create one branch in one development worktree:

   ```sh
   ./scripts/new-worktree.sh <slug>
   cd .worktrees/<slug>
   ```

2. Make one coherent change. Preserve unrelated dirty work and prefer deletion
   over compatibility machinery.
3. Run focused checks through the shared lease when they invoke Cargo or
   process-sensitive fixtures:

   ```sh
   ./scripts/with-local-ci-lease.sh cargo +1.88.0 test -p factoryd --lib
   ```

4. Run the authoritative gate on the exact head:

   ```sh
   ./scripts/local-ci.sh
   ```

   Ubuntu x86-64 contributors use `./scripts/local-ci.sh --linux-source`.
5. Push and open a PR describing behavior, deleted authority paths, exact
   base/head, focused proof, and unverified lanes.
6. A reviewer other than the author reads `base..head` cold, tries to break
   correctness/security/simplification, and posts findings plus what resisted
   attack. The author resolves every finding; the reviewer rechecks and gives
   an explicit ALLOW before merge.
7. Required hosted checks must pass on the exact reviewed head. Merge only
   then. Remove the development worktree after merge through the normal Git
   worktree command; never remove preserved factory Changes during this work.

### Shared local-CI lease

The macOS gate serializes compiler, release-probe, and process-sensitive work
across linked worktrees using a repository-common-directory lease. Its owner
record is diagnostic; the held kernel lock is authoritative. Do not bypass the
wrapper for a load-bearing Cargo or process fixture. Set
`DARK_FACTORY_LOCAL_CI_WAIT=0` to refuse instead of waiting.

The gate clears inherited live-factory home, socket, and attempt identity
variables. Tests set their own isolated values. The build-headroom preflight
reports and refuses low space but does not reclaim anything; inspect only
inactive regenerable Cargo targets manually. Product Rust verification uses
its own bounded daemon cache. It does not replace this daemon-independent
development lease.

## Isolated daemon checks

Use a second, throwaway home and explicit socket. Never rely on the default:

```sh
export DARK_FACTORY_HOME="$(mktemp -d /tmp/df-dev.XXXXXX)"
chmod 700 "$DARK_FACTORY_HOME"
target/debug/factoryd --socket "$DARK_FACTORY_HOME/f.sock" &
target/debug/factoryctl --socket "$DARK_FACTORY_HOME/f.sock" health
```

Worker lifecycle checks must use the deterministic shell provider and a tiny
temporary Git repository. They must prove the provider receives one
daemon-owned `.git`-free Change and that the same run and source survive the
injected boundary. Do not point any fixture at a real provider.

A lifecycle fixture must register resources before use and verify after its
test that exact descendants and disposable paths are gone. Crash/restart tests
must restart the daemon and let its durable finalizer converge. `Drop`, shell
traps, sleeps, broad process scans, and cleanup owned only by the killed fixture
are insufficient proof.

## Stage review discipline

Each safe-kernel stage is a coherent serial PR. Before implementation review,
record:

- the old authority paths deleted;
- production and test additions/deletions separately;
- exact causal tests and injected crash boundaries;
- unsupported later-stage operations that now fail closed;
- migration preconditions and rollback requirements; and
- any compatibility code retained, with its sole caller.

An independent phase review follows the stage PR review and challenges the
combined architecture against
[`SAFE_KERNEL_REFACTOR.md`](SAFE_KERNEL_REFACTOR.md). Passing one stage never
authorizes booting the factory.

## Migration rules

SQLite migrations are sequential numbered files under
`crates/factoryd/migrations/`. Never edit a shipped migration. Historical
fixtures must apply the real ordered chain to version N rather than creating a
new schema and manually deleting objects.

The Stage 1 cutover migration refuses a schema-29 database containing live
sessions, active/uncertain delivery, nonterminal runs, or other work whose
external effect cannot be proven. Stage 2 moves every preserved source path
into metadata-only `legacy_sources` quarantine, including separate records
when agents or projects shared a path. Factoryd never inspects or owns those
paths; forgetting a record does not touch the filesystem. Before any future
schema-30/31 boot, take an explicit database backup and rollback decision; the
refactor itself does not boot it.

## CI and GitHub

The pull-request workflow runs the shared source gate on hosted macOS and the
Linux source-only lane. The aggregate `required` context is the merge gate.
Review the exact `.github/workflows/` diff before approving an external run: a
PR evaluates its own workflow and can change `runs-on`. A green workflow never
replaces CODEOWNERS approval and resolved review threads.

Public state may include a milestone, exact ref/SHA, checks, links, and next
operator action. Attempt identities, prompts, guidance, raw provider output,
credentials, messages, source, and review deliberation stay private.

## Release and install

Release and install are paused until the safe-kernel boot review. Do not tag,
publish, update the Homebrew tap, run `factoryctl update --install`, or load a
refactor binary into the operator job.

After the freeze is lifted, the existing release transaction remains the
required shape:

- a semver tag matching the workspace version builds the supported archives;
- published manifests and archives carry exact SHA-256 identities;
- install stages a complete version directory, verifies every binary, then
  atomically repoints `bin/current`;
- managed daemon reload must prove the expected launchd PID and exact active
  sibling executables;
- failure restores the previous pointer/job and verifies old health; and
- migrations run at daemon start, with backup/rollback handled before an
  irreversible schema boundary.

The old zero-downtime claim based on resident provider processes no longer
applies. Any future updater must respect durable run resources and finalization
rather than assuming daemon restart leaves an independent session alive.

## Exact reporting

Report each command actually run and whether it passed, failed, or was not run.
Keep local proof, hosted CI, review approval, merge, release, install, and live
verification distinct. A source build is not provider validation; deterministic
shell proof is not Claude/Codex proof; a merged intermediate stage is not a
boot candidate.
