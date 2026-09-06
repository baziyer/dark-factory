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
`factoryctl service uninstall` and `service install` with the new binaries and
prints service, web and remote status. It refuses to run while the store holds
a non-terminal run. Binaries land in `.worktrees/bin-<sha>`.

## Order matters

**Pairing capability mask.** The browser client accepts one exact mask
(currently 31, after administration). When a change moves that mask, deploy the
site BEFORE reinstalling the daemon. A daemon that pairs with a new mask against
a site still serving the old client makes every deployed console fail
authentication.

**Schema change.** When a change bumps `PRAGMA user_version`, keep the previous
build's `.worktrees/bin-<sha>` for rollback. If the migration fails, restore the
backup the script printed into `$HOME/.dark-factory/factory.sqlite3` and
reinstall from the previous binaries.

## Where the service runs from

`factoryctl service install` copies the binaries into
`$HOME/.dark-factory.service/bin/current`, and launchd runs the service from
there. The build directory is only the source of that copy.
