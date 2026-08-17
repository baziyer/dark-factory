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
as one job, `checks`, on every pull request and every push to `main`.

**Going public is one step**: flip the repository, then immediately run
`scripts/github-repo-settings.sh` — the rulesets, the security features,
and the fork-approval policy below all 403/422 on a private free-plan
repository, so until that script has run clean none of this is enforced.
It applies, idempotently:

- labels (`known-issue`, `area:*`, `size:*`, `decision`, `security`) and
  merge settings (squash or rebase only, merged branches deleted);
- ruleset `main-protect`, with no bypass for anyone: a green `checks` run
  from GitHub Actions against a head that is up to date with `main`,
  linear history, no force-push, no deletion;
- ruleset `main-review`: a pull request with one CODEOWNERS approval and
  every thread resolved. The repository admin may bypass *this* ruleset,
  and only through a pull request — GitHub never lets an author approve
  their own PR and this repository has one maintainer — so the maintainer
  can merge their own reviewed PR, but never without green `checks`, and
  nobody pushes to `main`;
- private vulnerability reporting, Dependabot alerts, secret scanning with
  push protection, and "every workflow run from an outside contributor's
  fork needs approval".

Where `checks` runs is a **policy, not a mechanism**: a pull request runs
the workflow file it carries, so the `runs-on` expression only governs an
unmodified workflow.

- **Same-repository refs** (branches only collaborators can push, and
  `main` itself) run on the maintainer's persistent Mac, the self-hosted
  runner `dark-factory-mac` — warm cargo cache, real macOS, no hosted
  minutes.
- **Pull requests from forks** run on an ephemeral hosted macOS runner —
  *if* their workflow file is unmodified. Every fork's workflow run waits
  for the maintainer's approval; a fork PR that edits `runs-on` in
  `.github/workflows/ci.yml` would run on the Mac the moment that approval
  is given. So the boundary is the maintainer reading `.github/workflows/`
  in a fork's diff before approving it — GitHub's own guidance is that
  self-hosted runners and public repositories don't mix for exactly this
  reason. If that discipline ever feels thin, switching `runs-on` to
  `macos-latest` for all pull requests is a one-line change (hosted
  minutes are free for public repositories).

Known problems are GitHub issues labelled `known-issue`, see
`CONTRIBUTING.md`.

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

## Release and update

GitHub Releases are the source of truth for binaries; nothing else builds
them.

1. **Build and publish**: pushing a semver tag (`git tag v0.1.1 && git push
   origin v0.1.1`, on a commit whose `Cargo.toml` workspace version is
   `0.1.1` — the workflow refuses a mismatch) runs
   `.github/workflows/release.yml` on the self-hosted Mac: `cargo build
   --locked --release`, then `scripts/package-release.sh` produces
   `dark-factory-<tag>-aarch64-apple-darwin.tar.gz` (the four binaries,
   flat), `SHA256SUMS`, and `latest.json`, and `gh release create` attaches
   all three. `latest.json` is `{version, tag, assets: {<target>: {url,
   sha256}}}`; the newest one is always at
   `https://github.com/baziyer/dark-factory/releases/latest/download/latest.json`
   (a static URL, so no Vercel mirror is needed unless GitHub is
   unreachable from somewhere that matters). Only macOS arm64 is built
   today; other targets are more `assets` keys when someone needs them.
2. **Update signal**: `factoryctl update` fetches that manifest (via
   `curl`; `DARK_FACTORY_UPDATE_URL` overrides the URL for tests/mirrors)
   and prints JSON: `current`, `latest`, `update_available`, the platform
   `asset`. The result is cached in `$DARK_FACTORY_HOME/update-check.json`;
   `factory-tui` reads the same cache and refetches at most hourly, in a
   background thread of the running board — no background service — and
   shows `update vX available: factoryctl update --install` in its status
   line. `factoryctl health` now also returns the daemon's `version`.
3. **Install**: `factoryctl update --install` downloads the platform
   asset, verifies its SHA-256 against the manifest, unpacks it into
   `$DARK_FACTORY_HOME/bin/<version>/` (staged and renamed into place only
   once every binary checked out), atomically repoints
   `$DARK_FACTORY_HOME/bin/current` at it, and — if
   `~/Library/LaunchAgents/com.dark-factory.factoryd.plist` exists —
   rewrites that job to run `bin/current/factoryd` (keeping its `PATH` and
   any other daemon arguments), `bootout`s and `bootstrap`s it, and waits
   for `health` from the new daemon. Without a launchd job it stops after
   activation and says so; restart the daemon however you run it.
4. **Migrations** run at daemon start (`crates/factoryd/migrations/`), so
   an update never needs a separate migration step.
5. **No lost work**: sessions and runners are independent process trees
   (`ARCHITECTURE.md`, invariant 4). The daemon restart is on the order of
   a second and touches no agent process; `factory-tui` reconnects on its
   own. Two compatibility rules follow: the runner control protocol must
   stay backward compatible within a major version (a runner spawned by
   version N is supervised by daemon N+1 after an update), and running
   sessions' hooks keep working because they invoke `factoryctl` through
   the `bin/current` symlink, which now resolves to the new version.
6. **Rollback**: `ln -sfn <previous-version> $DARK_FACTORY_HOME/bin/current`
   (or repoint it the same atomic way) and `launchctl kickstart -k
   gui/$(id -u)/com.dark-factory.factoryd`. Nothing is deleted on install,
   so a rollback never re-downloads.
7. **Later**: a Homebrew tap and an npm wrapper, both consuming the exact
   same release assets and manifest — no separate build path.

Add `$DARK_FACTORY_HOME/bin/current` to your shell `PATH` to run the
installed `factoryctl`/`factory-tui`; `launchd/README.md` covers the job
itself.

### Operator install/doctor — design

`factoryctl init` and `factoryctl doctor` (not implemented yet): `init`
creates `$DARK_FACTORY_HOME`, installs the running build's sibling
binaries as `bin/<version>` + `current`, checks `claude`/`codex`/`git`,
renders the launchd job with a `PATH` that can find them, and loads it.
`doctor` runs the same checks read-only (plus socket reachability, daemon
vs. binary version, `0700`/`0600` permissions, stale agent worktrees) and
prints one pass/fail line per check, exiting non-zero if anything fails.

## Task list for whoever picks this up

- [x] GitHub Actions release workflow (tag → build → attach binaries + manifest)
- [x] `factoryctl update` (check-only) and `factory-tui` status-line signal
- [x] `factoryctl update --install` (download, verify, repoint, reload, restart)
- [ ] `factoryctl init` and `factoryctl doctor`
- [ ] Homebrew tap, npm wrapper (after the above is proven)
