# Deploying the site and the live service

Two operator scripts. Both take one exact 40-hex commit and refuse a dirty or
mis-positioned worktree.

```sh
./scripts/deploy-site.sh <site-commit-sha>
./scripts/reinstall-service.sh <commit-sha>
```

`deploy-site.sh` deploys the public site to Vercel production from a detached
worktree of the site repository (`$DARK_FACTORY_SITE`, default
`$HOME/dark-factory-site`) at that commit, then prints `vercel inspect`
for `https://app.darkfactory.build`.

`reinstall-service.sh` builds `factoryctl`, `factoryd` and `factory-runner`
from a detached worktree of this repository at that commit, verifies
`go version -m` reports the same `vcs.revision` and `vcs.modified=false`, backs
up `$HOME/.dark-factory/factory.sqlite3` to
`$HOME/.dark-factory-backups/<utc-timestamp>-<sha>/`, then runs
`factoryctl service uninstall` and `service install` with the new binaries,
waits (bounded) for the daemon's socket, and prints service, web and remote
status. It refuses to run while the store holds a non-terminal run, checked
once before the build and again right before the uninstall. Binaries land in
`.worktrees/bin-<sha>`.

`scripts/local-ci.sh` runs `scripts/test-reinstall-service.sh` and
`scripts/test-deploy-site.sh`, which exercise both scripts against fakes.

## Order matters

**Pairing capability mask.** Both ends accept any subset of the five known
capability bits with `observe` set, and reject any other bit as malformed:
`knownCapabilities` in `internal/browserprotocol/wire.go` (lines 104-108) and
`capabilities()` in `web/packages/client/src/control.ts`, which bounds the
value at 31. So a change that ADDS a capability bit must deploy the site BEFORE
the daemon is reinstalled: a daemon granting the new bit to a console still
running the old client makes every deployed console reject its own
authentication result. A change that only narrows the granted mask needs no
ordering.

**Schema change.** When a change bumps `PRAGMA user_version`, keep the previous
build's `.worktrees/bin-<sha>` for rollback. The daemon migrates the store
before it listens, so the script's socket wait timing out means the migration
did not finish; confirm that from `service status` and the daemon log, then
restore the backup the script printed into `$HOME/.dark-factory/factory.sqlite3`
and reinstall from the previous binaries.

## Where the service runs from

`factoryctl service install` copies the binaries into
`$HOME/.dark-factory.service/bin/current`, and launchd runs the service from
there. The build directory is only the source of that copy.
