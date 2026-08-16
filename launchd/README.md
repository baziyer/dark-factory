# Local LaunchAgents

Render the placeholders in the template with absolute, canonical paths.
Before loading it, create the state and `__LOG_DIRECTORY__` directories owned
by the current user with mode `0700`; keep the rendered webhook config,
secrets, and plist at mode `0600`. Install the plist in the user's
`~/Library/LaunchAgents`.

`com.dark-factory.factoryd` keeps the daemon alive. It loads
`$DARK_FACTORY_HOME/webhooks.json` automatically if present (or an explicit
`--webhook-config PATH`) before announcing readiness. This job does not use
GitHub Actions.

Subscription headroom has no background service or log: run `factoryctl usage`
on demand in a terminal instead.
