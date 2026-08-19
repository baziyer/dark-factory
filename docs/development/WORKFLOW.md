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

### Publishing an issue change

An assigned session gets one daemon-owned issue change. From that session,
run `factoryctl change create`; the daemon derives the task branch and nested
worktree from the authenticated current task. Use the returned worktree with
`factoryctl git commit` and `factoryctl git push`, then open or update a PR
through `factoryctl pr`. The client cannot select a repository, remote,
branch, or path, and the daemon refuses dirty, detached, stale, swapped, or
unregistered worktrees. Publication uses a server-enforced exact lease and
binds PR mutations to the exact published commit. `factoryctl change abandon`
records a durable removing state before cleanup, so a daemon restart can
resume it; creation retries also recover an exact worktree left by a database
failure.

### Testing resident sessions

An E2E harness must not stop `factoryd` immediately after a `StopSession`
response. That response only confirms that `factory-runner` accepted the stop;
the runner stays alive until the daemon observes its terminal event and sends
`AcknowledgeExit`. Killing the daemon between those steps leaves the runner
waiting for an acknowledgement that can never arrive.

Tests that create resident sessions must therefore use both safeguards in
`crates/factoryd/tests/sessions_e2e.rs`: call `cleanup_session` on the normal
path, which waits until the session is non-live **and its runner process has
exited** before stopping the daemon (pausing the agent first so pending fixture
work cannot spawn a replacement), and retain `Daemon`'s `Drop` cleanup for
assertion failures. A test may stop the daemon while a session is live only when
daemon restart/recovery is the behavior under test; it must reconnect, then
perform the same cleanup handshake before returning. Never replace this with a
fixed sleep or a bare `daemon.stop()`.

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
`$DARK_FACTORY_HOME` reconnects to whatever it left running. Upgrading the
**live** daemon is `factoryctl update --install` (below), or by hand: build
the new binaries and `launchctl kickstart -k gui/$(id -u)/com.dark-factory.factoryd`
— the daemon restarts, runners survive, and `factory-tui`'s reconnect/backoff
means the board just picks the same sessions back up. **One-time caveat for
an install that predates the process-group fix** (a job loaded from the old
template, running a daemon older than 39955d2): the *loaded* job has no
`AbandonProcessGroup` and its runners share the daemon's process group, so
the first `kickstart -k`/`bootout`/`update --install` after upgrading takes
any live session with it — do that first restart while no session is live.
Every restart after it (new daemon, new job) keeps sessions.

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
the workflow file it carries, so the `runs-on` expression at `.github/workflows/ci.yml`
only governs a PR that leaves it unmodified — a PR (or a manual run) that
edits it can route itself elsewhere. Four routes, in increasing order of
trust:

- **A pull request whose `runs-on` is unmodified** — same-repository or a
  fork — runs on an ephemeral hosted macOS runner (`macos-latest`; hosted
  minutes are free for public repositories). This is the default and the
  overwhelming majority of runs.
- **A fork's workflow run additionally needs the maintainer's approval**
  before it runs at all (`scripts/github-repo-settings.sh` sets that
  policy, confirmed live: `gh api
  repos/baziyer/dark-factory/actions/permissions/fork-pr-contributor-approval`
  returns `all_external_contributors`). Approval, not the runner choice,
  is the actual gate: self-hosted runners accept any job matching their
  label regardless of which workflow or branch produced it, so an
  approved fork run that edited `runs-on` to `dark-factory-mac` *would*
  execute there. Read `.github/workflows/` in a fork's diff before
  approving it — that's still the boundary.
- **A same-repository branch gets no approval gate at all** — GitHub's
  fork-approval setting only ever covers external contributors — so it
  can route `checks` straight to `dark-factory-mac` by editing `runs-on`
  in its own diff, unreviewed, the moment the PR opens or updates; no
  repo-level setting closes this. But only the maintainer's own GitHub
  account can push a same-repository branch — every agent's `agent/<id>`
  branch is pushed under that account (see the worktree note above) — so
  this policy removes the *default and accidental* path onto the
  persistent Mac, not a *determined* one from an account that already has
  push access and can reach the daemon locally anyway. `workflow_dispatch`
  (`ci.yml:7`) is the same trust level again: any collaborator with write
  access can manually run `checks` against any ref from the Actions
  UI/API, and it always evaluates to `dark-factory-mac`
  (`event_name != 'pull_request'`). The real backstop for a
  same-repository PR is that a green `checks` run never authorizes a
  merge by itself — `main-review` still requires one CODEOWNERS approval
  and every thread resolved, per the ruleset above.
- **`push` to `main`** (already merged, already reviewed via that same
  `main-review` ruleset) and **a maintainer's tag push** in `release.yml`
  are the only routes that don't depend on any account's intent — both
  require code to already be on a protected ref. These run on the
  maintainer's persistent Mac, the self-hosted runner `dark-factory-mac`
  — warm cargo cache, real macOS, no hosted minutes.

**Follow-up, not done yet**: the remaining hardening for a determined
same-repository or write-access actor is isolating the persistent runner
itself — a separate macOS user with no access to the operator's home
directory, credentials, or the `factoryd` socket, or replacing it with an
ephemeral self-hosted runner — tracked as
[#54](https://github.com/baziyer/dark-factory/issues/54).

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
pins `cargo +1.88.0` explicitly and never changes the default toolchain,
so sharing keeps it warm without repinning anything. `CARGO_TARGET_DIR`
lives outside `_work` because the checkout step in `ci.yml`/`release.yml`
runs `git clean -ffdx` each job — see the comment on that step in either
workflow file for why.) Finally `./svc.sh install && ./svc.sh start`, and
confirm it is
`online` with `gh api repos/baziyer/dark-factory/actions/runners`. To
remove it: `./svc.sh stop && ./svc.sh uninstall && ./config.sh remove
--token "$(gh api -X POST repos/baziyer/dark-factory/actions/runners/remove-token --jq .token)"`.

## Release and update

GitHub Releases are the source of truth for binaries; nothing else builds
them.

1. **Build and publish**: pushing a semver tag (`git tag v0.2.3 && git push
   origin v0.2.3`, on a commit whose `Cargo.toml` workspace version is
   `0.2.3` — the workflow refuses a mismatch) runs
   `.github/workflows/release.yml` on the trusted self-hosted arm Mac. One
   serialized job builds `aarch64-apple-darwin` and then
   `x86_64-apple-darwin`; there is no release-writing matrix to race shared
   state. One `scripts/package-release.sh` transaction stages both flat
   four-binary archives, `SHA256SUMS`, `latest.json`, and a
   `dark-factory.rb` candidate before exposing `dist/`. It normalizes archive
   member order, mode, ownership, and timestamps so rebuilding byte-identical
   binaries produces the same resumable asset set.
   `scripts/publish-release.sh` binds the remote tag to the workflow commit,
   creates a draft, uploads only missing assets, and publishes only when the
   remote asset names and digests exactly match that build. GitHub 5xx and
   transport failures get four attempts with 2/4/8-second backoff. After any
   failed write, the publisher reads the release once and accepts an
   already-committed exact result; deterministic client errors are not
   retried. A tag with a pre-release suffix (`v0.2.3-rc.1`) is published as
   a pre-release so `releases/latest` keeps pointing at the newest full
   release. `latest.json` is `{version, tag, assets: {<target>: {url,
   sha256}}}` with both macOS targets; the newest one is always at
   `https://github.com/baziyer/dark-factory/releases/latest/download/latest.json`
   (a static URL, so no Vercel mirror is needed unless GitHub is
   unreachable from somewhere that matters).
   If a workflow defect stops publication, do not move or recreate the tag.
   After its fix reaches `main`, dispatch the Release workflow from `main`
   with the existing tag. Recovery resolves and builds that tagged commit,
   but saves the publisher from the exact reviewed `main` commit that started
   the run. The publisher revalidates the remote tag before any release write;
   dispatches from other branches are rejected before the tagged source is
   checked out.
2. **Update signal**: `factoryctl update` fetches that manifest (via
   `curl`; `DARK_FACTORY_UPDATE_URL` overrides the URL for tests/mirrors)
   and prints concise human-readable lines naming the invoking/bootstrap
   version, active `bin/current` runtime, latest release, and whether
   `update --install` has work. Pass `factoryctl update --json` for the
   machine-readable object (`current`, `active`, `latest`,
   `update_available`, and the platform `asset`). Availability compares `latest` with `active`
   when installed, not with a newer Homebrew bootstrap. The manifest result
   is cached in `$DARK_FACTORY_HOME/update-check.json`;
   `factory-tui` reads the same cache and refetches at most hourly, in a
   background thread of the running board — no background service — and
   shows `update vX available: factoryctl update --install` in its status
   line. `factoryctl health` also returns the daemon's `version`.
3. **Install**: `factoryctl update --install` first does every read-only
   check (the manifest; the launchd job, if any, and that it runs with
   *this* `$DARK_FACTORY_HOME` — a scratch home is refused rather than
   moving the operator's job), then downloads the platform asset, verifies
   its SHA-256, unpacks it into `$DARK_FACTORY_HOME/bin/<version>/` (staged,
   renamed into place only once every binary checked out; a complete
   version already on disk is reused), atomically repoints
   `$DARK_FACTORY_HOME/bin/current`, and — if
   `~/Library/LaunchAgents/com.dark-factory.factoryd.plist` exists —
   rewrites that job to run `bin/current/factoryd` (keeping its other
   arguments and environment; `PATH` gains the provider CLIs' directories
   if it lacks them), `bootout`s and `bootstrap`s it, and waits for
   `health` to answer **with the new version**. If the reload fails,
   `bin/current` is rolled back and the error names the recovery command;
   if the new daemon never answers, exit 1 says where the log is and how to
   roll back by hand. If the new version is already installed and running,
   nothing restarts. Without a launchd job it stops after activation and
   says so; restart the daemon however you run it.
4. **Migrations** run at daemon start (`crates/factoryd/migrations/`), so
   an update never needs a separate migration step.
5. **No lost work**: sessions and runners are independent process trees
   (`ARCHITECTURE.md`, invariant 4). The daemon restart is on the order of
   a second and touches no agent process; `factory-tui` reconnects on its
   own. Under launchd this holds because every runner is its own
   process-group leader *and* the job sets `AbandonProcessGroup` — without
   either, `bootout`/`kickstart -k` kills the daemon's whole group, sessions
   included (verified with a throwaway job; `sessions_e2e`'s
   `factoryd_process_group_kill_does_not_take_sessions` guards it). Two
   compatibility rules follow, in both directions: the runner control
   protocol must stay backward compatible within a major version (a runner
   spawned by daemon N is supervised by daemon N+1 after an update, and —
   in the seconds between `activate` and the new daemon answering — daemon
   N spawns runner N+1), and running sessions' hooks keep working because
   they invoke `factoryctl` through the `bin/current` symlink, which now
   resolves to the new version, against whichever daemon is up.
6. **Rollback**: `ln -sfn <previous-version> $DARK_FACTORY_HOME/bin/current`
   (or repoint it the same atomic way) and `launchctl kickstart -k
   gui/$(id -u)/com.dark-factory.factoryd`. Nothing is deleted on install,
   so a rollback never re-downloads.
7. **Homebrew bootstrap substrate**: this repository renders the exact
   custom-tap formula from the two archive checksums and the update-manifest
   checksum, then publishes it as `dark-factory.rb`. The versioned manifest
   is the formula's required top-level source; an architecture-selected
   resource supplies the binaries. The public tap is
   [`baziyer/homebrew-tap`](https://github.com/baziyer/homebrew-tap). Its
   v0.2.0 formula passed tap, install, test, binary-version, and scratch
   `init`/`doctor` checks. After each release, update the tap from the published
   formula asset:

   ```sh
   tap_dir=$(brew --repository baziyer/tap)
   gh release download "$TAG" --repo baziyer/dark-factory \
     --pattern dark-factory.rb --dir "$tap_dir/Formula" --clobber
   ruby -c "$tap_dir/Formula/dark-factory.rb"
   brew style "$tap_dir/Formula/dark-factory.rb"
   brew audit --strict --formula baziyer/tap/dark-factory
   brew install --formula baziyer/tap/dark-factory
   brew test baziyer/tap/dark-factory
   ```

   The formula checksums the manifest and selected arm or Intel archive, then
   installs all four binaries. It deliberately defines no `service` block.
   Homebrew owns only the bootstrap copy: `factoryctl init` installs the
   active versioned runtime and launchd job, and `factoryctl update --install`
   remains the sole active-runtime updater so live-session preservation,
   atomic switch, health verification, and rollback stay in one
   implementation. The formula caveats state the same split.
   Accordingly, `brew uninstall dark-factory` removes only the Homebrew
   bootstrap commands; it leaves the launchd job, active runtime, database,
   worktrees, logs, and installed versions under `~/.dark-factory`. Follow
   [the service uninstall procedure](../../launchd/README.md#uninstall) to
   stop live sessions and unload the job safely before removing anything
   else. State deletion remains a separate irreversible choice.
8. **npm remains deferred** until non-macOS demand is demonstrated. A wrapper
   would add Node as an installation dependency while reintroducing the same
   bootstrap-versus-active-runtime update split; it does not simplify the
   supported macOS product today.

Add `$DARK_FACTORY_HOME/bin/current` to your shell `PATH` to run the
installed `factoryctl`/`factory-tui`; `launchd/README.md` covers the job
itself.

### Operator install and doctor

`factoryctl init` is the guided install (README, "Install"): home
directory (a symlink is refused, as the daemon refuses it), this build's
binaries as `bin/<version>` + `current` (a different build under the same
version is refused, never overwritten), a probe of `claude`/`codex`/`git`,
the disclosure of what is written outside the home and a consent step
before launchd is touched, a refusal to race a hand-started daemon on the
same socket, then the launchd job rendered with a `PATH` that can find
those CLIs (an existing job keeps its arguments and environment and gets
its `PATH` repaired), loaded, and the daemon awaited *with this version*.
Once the daemon answers, `init` lists projects through the local API and only
suggests creating the demo project for an empty fleet; existing fleets get
`factoryctl status` and `factory-tui` instead.
`factoryctl doctor [--json]` runs the same diagnostic probes plus the daemon
(reachable? same version as the binaries?), the launchd job (installed,
loaded, `PATH` — launchd's default when the job sets none — covers the
providers?), `~/.claude.json` for worktree pre-trust, every project's root
and stale worktree directories, and the cached update check (which may fetch
and refresh `$DARK_FACTORY_HOME/update-check.json`, at most hourly). It does
not repair or reconfigure the installation; it prints one line per check and
exits 1 on any failure. `init`, `doctor`, and `update --install` share one set
of probes (`crates/factoryctl/src/probes.rs`) and one launchd path
(`launchd::apply`), so they cannot disagree about what a healthy install looks
like.

The live-session capacity is also launchd-owned durable state. New jobs receive
the finite default of 4; `factoryctl capacity set N` and the TUI's `C` setting
surface use the same shared operation, which accepts 1 through 64, requires a
loaded managed job, serializes concurrent changes, reports restart/subscription
impact, reloads only `factoryd`, waits for managed-process health, and restores
the prior plist and job on failure. `init`, `update --install`, re-init, and a
binary rollback carry the chosen `--max-active-runs` value forward. The
provider-session shell policy denies agent-originated mutation, and a manual
daemon cannot satisfy managed health.

## Task list for whoever picks this up

- [x] GitHub Actions release workflow (tag → build → attach binaries + manifest)
- [x] `factoryctl update` (check-only) and `factory-tui` status-line signal
- [x] `factoryctl update --install` (download, verify, repoint, reload, restart)
- [x] `factoryctl init` and `factoryctl doctor`
- [x] Publish and real-install-test the Homebrew tap
- [ ] Reconsider an npm wrapper after demonstrated non-macOS demand
