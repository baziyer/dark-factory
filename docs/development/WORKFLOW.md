# Development workflow

## Day to day

1. `./scripts/new-worktree.sh <slug>` — one worktree per task, never work
   directly on `main`.
2. Build and iterate: `cargo build --workspace`.
3. Before opening a PR: `./scripts/local-ci.sh` (fmt, clippy at
   `-D warnings`, the full test suite, `git diff --check`) — this is the
   authoritative gate; GitHub Actions is manual-only.
4. Push the branch, open a PR.
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
