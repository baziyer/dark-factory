# Development workflow

## Day to day

1. `./scripts/new-worktree.sh <slug>` — one worktree per task, never work
   directly on `main`.
2. Build and iterate: `cargo build --workspace`.
3. Before opening a PR: `./scripts/local-ci.sh` (fmt, clippy at
   `-D warnings`, the full test suite, `git diff --check`) — this is the
   authoritative gate; CI runs the exact same script (see "CI and GitHub"
   below).
4. Push the branch, open a PR (the template carries the review checklist).
5. **Adversarial review before merge**: a second agent or person reads the
   diff cold and tries to break it — correctness, missed simplification,
   security — and posts findings on the PR. The author addresses each one
   or explains why not. The reviewer re-checks. Only then merge. See
   `AGENTS.md`'s "Critical rules" for the exact steps.
6. Remove the worktree once merged (`git worktree remove .worktrees/<slug>`).

### Developing the daemon without disrupting a running factory

Never point a development build at `~/.dark-factory` or the installed
`launchd` job — that is the operator's live system, with real agent
sessions in it. Run a second, throwaway daemon instead:

```sh
export DARK_FACTORY_HOME=$(mktemp -d /tmp/df-dev.XXXXXX)
chmod 700 "$DARK_FACTORY_HOME"
target/debug/factoryd --socket "$DARK_FACTORY_HOME/f.sock" &
target/debug/factoryctl --socket "$DARK_FACTORY_HOME/f.sock" health
```

A resident session's provider process is a detached process tree,
independent of `factoryd` (`ARCHITECTURE.md`'s invariant 4): killing and
restarting *this* development daemon on the *same* temp
`$DARK_FACTORY_HOME` reconnects to whatever it left running. This is also
how upgrading the **live** daemon works today, manually: build the new
binaries, then `launchctl kickstart -k gui/$(id -u)/com.dark-factory.factoryd`
— the daemon restarts, runners survive, and `factory-tui`'s reconnect/backoff
means the board just picks the same sessions back up.

## CI and GitHub

`.github/workflows/ci.yml` runs `./scripts/local-ci.sh` — nothing else —
as one job, `checks`, on every pull request and every push to `main`. The
`main` ruleset requires that check green against the current head, a
pull request with one CODEOWNERS approval, linear history, and forbids
force-pushes and deletion; the repository admin can bypass the review
requirement (GitHub never lets an author approve their own PR) but only
through a pull request, never by pushing to `main`. Merge methods are
squash or rebase; merged branches are deleted.

Where the job runs is a security boundary, not a cost setting:

- **Same-repository refs** (branches only collaborators can push, and
  `main` itself) run on the maintainer's persistent Mac, the self-hosted
  runner `dark-factory-mac` — warm cargo cache, real macOS, no hosted
  minutes.
- **Pull requests from forks** execute untrusted code, so they run on an
  ephemeral hosted macOS runner and never reach that machine. Workflows
  from an outside contributor's fork additionally need maintainer approval
  before they run at all.

`scripts/github-repo-settings.sh` applies all of the above (labels, merge
settings, the ruleset, private vulnerability reporting, secret scanning
and push protection, the fork-approval policy) and is idempotent — re-run
it after changing anything there. Known problems are GitHub issues
labelled `known-issue` (`docs/KNOWN-ISSUES.md` is only a pointer);
`scripts/import-issues.sh` turns a `###`-sectioned triage document into
labelled issues if a batch ever needs importing again.

### The self-hosted runner

`~/actions-runner-dark-factory-repo` on the maintainer's Mac, registered
to this repository as `dark-factory-mac` (label `dark-factory-mac`),
installed as the launchd service
`actions.runner.baziyer-dark-factory.dark-factory-mac`. It is not in
version control; to rebuild it on a new machine:

```sh
V=$(gh api repos/actions/runner/releases/latest --jq .tag_name | sed 's/^v//')
mkdir -p ~/actions-runner-dark-factory-repo && cd ~/actions-runner-dark-factory-repo
curl -fsSL -o runner.tar.gz "https://github.com/actions/runner/releases/download/v${V}/actions-runner-osx-arm64-${V}.tar.gz"
tar xzf runner.tar.gz && rm runner.tar.gz
./config.sh --unattended --url https://github.com/baziyer/dark-factory \
  --token "$(gh api -X POST repos/baziyer/dark-factory/actions/runners/registration-token --jq .token)" \
  --name dark-factory-mac --labels dark-factory-mac --work _work
```

Then write `.env` next to it — a launchd service gets no shell profile, so
every line is load-bearing (absolute paths; the runner does not expand
`$HOME`):

```
PATH=/Users/<you>/.cargo/bin:/opt/homebrew/bin:/usr/bin:/bin:/usr/sbin:/sbin
LANG=en_GB.UTF-8
CARGO_TARGET_DIR=/Users/<you>/actions-runner-dark-factory-repo/_cargo-target
```

(`RUSTUP_HOME` is deliberately shared with the login user: `local-ci.sh`
pins `cargo +1.85.0` explicitly and never changes the default toolchain,
so sharing keeps it warm without repinning anything. `CARGO_TARGET_DIR`
lives outside `_work` because `actions/checkout` runs `git clean -ffdx`
each job.) Finally `./svc.sh install && ./svc.sh start`, and confirm it is
`online` with `gh api repos/baziyer/dark-factory/actions/runners`. To
remove it: `./svc.sh stop && ./svc.sh uninstall && ./config.sh remove
--token "$(gh api -X POST repos/baziyer/dark-factory/actions/runners/remove-token --jq .token)"`.

## Release and update design — not implemented

Everything below is a design, not shipped code. No `factoryctl update`,
manifest, or Homebrew/npm packaging exists yet. Recorded here so the shape
is agreed before anyone builds it.

**Distribution, in order**: a hosted manifest first (this repo's GitHub
Releases, mirrored by `~/dark-factory-site`'s Vercel deployment for a
stable static URL), then npm, then Homebrew — never block a standalone
Rust binary on the packaging story.

1. **Build**: a semver tag triggers GitHub Actions to build release
   binaries for macOS arm64 (x86_64 and Linux later) and attach them, plus
   a SHA256 manifest, to a GitHub Release.
2. **Manifest**: a small JSON "latest" manifest (version, per-platform
   download URL, SHA256) is mirrored by `dark-factory-site` (Vercel) so a
   stable static URL exists independent of GitHub's own API.
3. **Update signal**: `factoryctl update` checks the manifest against the
   running version. `factory-tui` shows "update vX available" in its
   status line, checked at most hourly, in-process — no background
   service, no polling daemon.
4. **Install**: `factoryctl update --install` downloads the new release to
   `$DARK_FACTORY_HOME/bin/<version>/`, verifies its SHA256 against the
   manifest, atomically repoints a `current` symlink at it, rewrites and
   reloads the `launchd` job (see `launchd/README.md`), and restarts the
   daemon.
5. **Migrations** run automatically at daemon start (already how SQLite
   migrations work today — see `crates/factoryd/migrations/`), so an
   update never needs a separate migration step.
6. **Zero lost work**: sessions and runners are independent process trees
   (invariant 4) — the daemon restart itself is on the order of a second,
   and no agent process is touched. `factory-tui`'s existing
   reconnect/backoff means the board reattaches on its own.
7. **Rollback**: repoint `current` back to the previous version's
   directory and restart; nothing is deleted on install, so a rollback
   never needs a re-download.
8. **Later**: a Homebrew tap and an npm wrapper, both consuming the exact
   same release assets and manifest — no separate build path.

### Operator install/doctor — design

`factoryctl doctor` (not implemented): checks `claude`/`codex` are on
`PATH` and reports their versions, whether the `launchd` job is loaded and
healthy, whether the socket is reachable, that `$DARK_FACTORY_HOME`
permissions match the `0700`/`0600` requirements in `ARCHITECTURE.md`, and
that the current directory's git installation supports worktrees. Prints
one pass/fail line per check; exits non-zero if anything fails.

## Task list for whoever picks this up

- [ ] GitHub Actions release workflow (tag → build → attach binaries + manifest)
- [ ] `dark-factory-site` route mirroring the manifest JSON
- [ ] `factoryctl update` (check-only) and `factory-tui` status-line signal
- [ ] `factoryctl update --install` (download, verify, repoint, reload, restart)
- [ ] `factoryctl doctor`
- [ ] Homebrew tap, npm wrapper (after the above is proven)
