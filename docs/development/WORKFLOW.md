# Development workflow

## Live-use boundary

Live use remains frozen until an independent exact-main boot review passes.
Do not start provider work, install or release a development revision, enable
dispatch, modify `~/.dark-factory`, load or alter the installed launchd job,
or delete retained Changes. Isolated fixtures use a temporary home, explicit
socket, exact resource identities, and an independent reaper.

## Day to day

1. Create one branch in one development worktree. For agent automation, first
   obtain a current local base through the remote-access boundary below; the
   helper itself never contacts a remote. Human contributors may refresh their
   checkout through their normal Git workflow.

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
5. Publish the branch and open a PR through the remote-access boundary below,
   describing behavior, deleted authority paths, exact base/head, focused
   proof, and unverified lanes.
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

## Review discipline

Each PR is one coherent change. Before review, record:

- the old authority paths deleted;
- production and test additions/deletions separately;
- exact causal tests and injected crash boundaries;
- unsupported operations that fail closed;
- migration preconditions and rollback requirements; and
- any compatibility code retained, with its sole caller.

Security-sensitive changes receive an independent adversarial review against
[`SAFE_KERNEL_REFACTOR.md`](SAFE_KERNEL_REFACTOR.md). Merge, boot approval,
release, installation, and live verification remain separate decisions.

## Migration rules

SQLite migrations are sequential numbered files under
`crates/factoryd/migrations/`. Never edit a shipped migration. Historical
fixtures must apply the real ordered chain to version N rather than creating a
new schema and manually deleting objects.

Kernel cutover migrations refuse databases containing live legacy authority or
other external effects whose completion cannot be proven. Preserved source
paths migrate into metadata-only `legacy_sources` quarantine, including
separate records when agents or projects shared a path. Factoryd never inspects
or owns those paths; forgetting a record does not touch the filesystem. Before
an operator database crosses an irreversible migration boundary, take an
explicit backup and rollback decision. Migration never implies boot approval.

## CI and GitHub

The pull-request workflow runs the shared source gate on hosted macOS and the
Linux source-only lane. The aggregate `required` context is the merge gate.
Review the exact `.github/workflows/` diff before approving an external run: a
PR evaluates its own workflow and can change `runs-on`. A green workflow never
replaces CODEOWNERS approval and resolved review threads.

Agent automation must use an explicitly authorized credential broker or
App-backed tool surface supplied by its host for every authenticated remote
read or write. This includes private checkout and fetch as well as publishing
refs, opening or updating pull requests, reviewing, merging, and deleting
refs. The current coordinator's connected GitHub App is one such surface;
Claude or another harness may provide an equivalent. Agents must not use
ambient `git fetch`, `git pull`, `git clone`, `git push`, `gh`, `gh auth`, or
SSH-based access: those paths can consult the operator's credential helper,
login keychain, SSH agent, or other user credential state.

Anonymous HTTPS reads of public repositories are allowed when credential
lookup and interactive prompting are disabled. A host may instead inject a
short-lived, repository-scoped read credential for checkout without exposing
it to the agent. If no authorized surface is available, authenticated access
stops without contacting the remote. There is no token fallback for agent
writes: operator approval does not authorize injecting `GH_TOKEN`,
`GITHUB_TOKEN`, a personal access token, or another write credential into an
agent process. Never use interactive authentication, write a credential into a
worktree or repository, or expose it through a prompt, command output, or log.
This is a contributor-agent workflow boundary, not the future Dark Factory
product GitHub integration. Human operators may continue to use their normal
Git and GitHub CLI configuration for separately reviewed human actions.

Public state may include a milestone, exact ref/SHA, checks, links, and next
operator action. Attempt identities, prompts, guidance, raw provider output,
credentials, messages, source, and review deliberation stay private.

## Release and install

Release and install are paused until the independent exact-main boot review.
Do not tag, publish, update the Homebrew tap, run
`factoryctl update --install`, or load a development binary into the operator
job.

After boot approval and a separate release decision, the release transaction
retains this shape:

- a semver tag matching the workspace version builds the supported archives;
- published manifests and archives carry exact SHA-256 identities;
- install stages a complete version directory, verifies every binary, then
  atomically repoints `bin/current`;
- managed daemon reload must prove the expected launchd PID and exact active
  sibling executables;
- failure restores the previous pointer/job and verifies old health; and
- migrations run at daemon start, with backup/rollback handled before an
  irreversible schema boundary.

Any updater must respect durable run resources and finalization rather than
assuming a daemon restart leaves provider work independently alive.

## Exact reporting

Report each command actually run and whether it passed, failed, or was not run.
Keep local proof, hosted CI, review approval, merge, release, install, and live
verification distinct. A source build is not provider validation; deterministic
shell proof is not Claude/Codex proof; a merged change is not a boot decision.
