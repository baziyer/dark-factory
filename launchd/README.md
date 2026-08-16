# Local LaunchAgents

Render the placeholders in both templates with absolute, canonical paths.
Before loading them, create the state and `__LOG_DIRECTORY__` directories owned
by the current user with mode `0700`; keep the rendered webhook config, secrets,
and plists at mode `0600`. Install the plists in the user's
`~/Library/LaunchAgents`.

`com.dark-factory.factoryd` keeps the daemon alive and loads the generic webhook
configuration before announcing readiness. `com.dark-factory.usage-monitor`
runs once on load and then every 21,600 seconds (six hours). Neither job uses
GitHub Actions.

Only the monitor's normalized structural summary and traces may reach these
logs. Provider terminal/protocol output is bounded in memory and discarded.
