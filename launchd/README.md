# Local LaunchAgents

`com.dark-factory.factoryd.plist.template` keeps the daemon alive. It loads
`$DARK_FACTORY_HOME/webhooks.json` automatically if present (or an explicit
`--webhook-config PATH`) before announcing readiness. This job does not use
GitHub Actions.

Before loading it, create the state and `__LOG_DIRECTORY__` directories owned
by the current user with mode `0700`; keep the rendered webhook config,
secrets, and plist at mode `0600`.

## Render and install

launchd jobs do not inherit a login shell's `PATH` -- under `launchd` it is
just `/usr/bin:/bin:/usr/sbin:/sbin`. `factoryd` has no `--codex`/`--claude`
flag; it resolves `claude`/`codex` by bare name through the session `PATH`
it hands each spawned agent, so `__PATH__` below must already include
whatever directories `claude` and `codex` actually live in on this machine
(`~/.local/bin`, `~/.nvm/.../bin`, `/opt/homebrew/bin`, ...) or a
launchd-managed daemon will never find either provider, even though the
exact same command works fine from an
interactive shell. Find them first:

```sh
dirname "$(command -v claude)"
dirname "$(command -v codex)"
```

Render every placeholder to an absolute, canonical path, then install the
result in the user's `~/Library/LaunchAgents`:

```sh
sed \
  -e "s#__FACTORYD__#$HOME/.local/bin/factoryd#g" \
  -e "s#__DATABASE__#$HOME/.dark-factory/factory.db#g" \
  -e "s#__SOCKET__#$HOME/.dark-factory/f.sock#g" \
  -e "s#__FACTORY_RUNNER__#$HOME/.local/bin/factory-runner#g" \
  -e "s#__FACTORYCTL__#$HOME/.local/bin/factoryctl#g" \
  -e "s#__RUNTIME_ROOT__#$HOME/.dark-factory/runs#g" \
  -e "s#__WEBHOOK_CONFIG__#$HOME/.dark-factory/webhooks.json#g" \
  -e "s#__WORKING_DIRECTORY__#$HOME/.dark-factory#g" \
  -e "s#__LOG_DIRECTORY__#$HOME/.dark-factory/logs#g" \
  -e "s#__PATH__#$(dirname "$(command -v claude)"):$(dirname "$(command -v codex)"):/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin#g" \
  launchd/com.dark-factory.factoryd.plist.template \
  > ~/Library/LaunchAgents/com.dark-factory.factoryd.plist
chmod 0600 ~/Library/LaunchAgents/com.dark-factory.factoryd.plist

launchctl bootstrap gui/$(id -u) ~/Library/LaunchAgents/com.dark-factory.factoryd.plist
launchctl kickstart -k gui/$(id -u)/com.dark-factory.factoryd
```

Adjust the substituted paths to wherever `factoryd`/`factory-runner`/
`factoryctl` and `$DARK_FACTORY_HOME` actually live on this machine; the
values above are only an example. `--max-active-runs` (default 4, enforced
by the dispatcher as a hard cap on live sessions) is not templated here --
append `--max-active-runs N` to `ProgramArguments` yourself if the default
needs overriding.

Subscription headroom has no background service or log: run `factoryctl
usage` on demand in a terminal instead.

## Removing a previously installed usage-monitor job

Older checkouts installed a separate `com.dark-factory.usage-monitor` job.
It no longer exists (`factoryctl usage` replaced it): remove any surviving
install with `launchctl bootout gui/$(id -u)/com.dark-factory.usage-monitor`
and delete its plist from `~/Library/LaunchAgents`.
