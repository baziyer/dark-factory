# Local LaunchAgents

`com.dark-factory.factoryd.plist.template` keeps the daemon alive. It loads
`$DARK_FACTORY_HOME/webhooks.json` automatically if present (or an explicit
`--webhook-config PATH`) before announcing readiness. This job does not use
GitHub Actions.

Before loading it, create `$DARK_FACTORY_HOME` and its `logs/` directory
owned by the current user with mode `0700`; keep the rendered webhook
config, secrets, and plist at mode `0600`.

## Render and install

launchd jobs do not inherit a login shell's `PATH` -- under `launchd` it is
just `/usr/bin:/bin:/usr/sbin:/sbin`. `factoryd` has no `--codex`/`--claude`
flag; it resolves `claude`/`codex` by bare name through the session `PATH`
it hands each spawned agent, so the `PATH` in `__ENVIRONMENT__` below must
already include whatever directories `claude` and `codex` actually live in on this machine
(`~/.local/bin`, `~/.nvm/.../bin`, `/opt/homebrew/bin`, ...) or a
launchd-managed daemon will never find either provider, even though the
exact same command works fine from an
interactive shell. Find them first:

```sh
dirname "$(command -v claude)"
dirname "$(command -v codex)"
```

`factoryctl init` creates this job and `factoryctl update --install`
rewrites and reloads an existing one, both from this exact template. To
create one by hand instead, render the three placeholders —
`__PROGRAM_ARGUMENTS__` (one `<string>` per argument, the first being the
absolute path to `factoryd`; the daemon finds `factory-runner`/`factoryctl`
as its own siblings and every path under `$DARK_FACTORY_HOME` by default),
`__ENVIRONMENT__` (`<key>`/`<string>` pairs; at least `PATH` and
`DARK_FACTORY_HOME`), and `__DARK_FACTORY_HOME__` — then install the result
in `~/Library/LaunchAgents`:

```sh
factoryd="$HOME/.dark-factory/bin/current/factoryd"   # or wherever it lives
path="$(dirname "$(command -v claude)"):$(dirname "$(command -v codex)"):/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin"
sed \
  -e "s#__PROGRAM_ARGUMENTS__#        <string>$factoryd</string>#g" \
  -e "s#__ENVIRONMENT__#        <key>PATH</key><string>$path</string><key>DARK_FACTORY_HOME</key><string>$HOME/.dark-factory</string>#g" \
  -e "s#__DARK_FACTORY_HOME__#$HOME/.dark-factory#g" \
  launchd/com.dark-factory.factoryd.plist.template \
  > ~/Library/LaunchAgents/com.dark-factory.factoryd.plist
chmod 0600 ~/Library/LaunchAgents/com.dark-factory.factoryd.plist
mkdir -p ~/.dark-factory/logs && chmod 0700 ~/.dark-factory ~/.dark-factory/logs

launchctl bootstrap gui/$(id -u) ~/Library/LaunchAgents/com.dark-factory.factoryd.plist
launchctl kickstart -k gui/$(id -u)/com.dark-factory.factoryd
```

`--max-active-runs` (default 4, enforced by the dispatcher as a hard cap on
live sessions) and any other `factoryd` flag go into `ProgramArguments` as
further `<string>` elements; `factoryctl update --install` carries them and
the environment over when it rewrites the job (only `--runner`/
`--factoryctl` are dropped, since they must point at the newly activated
binaries, and `PATH` gains the provider CLIs' directories if it lacks
them). A rewritten job must be `bootout`/`bootstrap`ed, not just
`kickstart`ed — launchd caches `ProgramArguments`. The template sets
`AbandonProcessGroup`: without it launchd would kill every runner — every
session — with the daemon on `bootout`/`kickstart -k`.

Subscription headroom has no background service or log: run `factoryctl
usage` on demand in a terminal instead.

## Uninstall

Commit or copy any uncommitted agent work first. List the sessions in each
project. Stop every live session before you stop the daemon:

```sh
factoryctl session list --project PROJECT_ID
factoryctl session stop --project PROJECT_ID --session SESSION_ID --grace-ms 5000
factoryctl session list --project PROJECT_ID
```

Repeat `session stop` for each entry that has no `ended_at_ms`. A stop closes
its active task run. Check the final list before you continue. Then unload the
service and remove its job file:

```sh
launchctl bootout gui/$(id -u)/com.dark-factory.factoryd
rm ~/Library/LaunchAgents/com.dark-factory.factoryd.plist
```

This keeps `~/.dark-factory`, including its database, worktrees, logs, and
installed versions. It also leaves each `agent/<id>` branch in its project
repository, Claude trust entries in `~/.claude.json`, and provider login state.
You can reinstall the service without losing this state.

Deleting `~/.dark-factory` is a separate and irreversible action. It deletes
the factory history and managed worktrees. This guide does not run that action.
Archive or inspect the directory first, and only remove it after every session
has stopped. External agent branches and Claude trust entries still remain.

## Removing a previously installed usage-monitor job

Older checkouts installed a separate `com.dark-factory.usage-monitor` job.
It no longer exists (`factoryctl usage` replaced it): remove any surviving
install with `launchctl bootout gui/$(id -u)/com.dark-factory.usage-monitor`
and delete its plist from `~/Library/LaunchAgents`.
