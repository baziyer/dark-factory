# Dark Factory

Dark Factory is a macOS-local runtime for supervised coding-agent work on
your own machine. `factoryd` owns the durable queue, attempts, provider
processes, and cleanup. `factoryctl` is the operator CLI. A hosted web console
pairs with the daemon through the daemon's own pair page at
<http://127.0.0.1:43123/pair>, which installation opens, and connects to the
paired daemon's authenticated loopback API.

The runtime is not a hosted coding service, a coding model, or a general agent
framework. It keeps work running when the CLI or browser closes, and provides
no commit, push, pull-request, or repository-publication operation.

## Current support

- macOS only.
- The `shell` and `codex` providers are proven end to end. The `claude_code`
  launch path is fixture-proven for an existing local CLI and signed-in account;
  its real-provider smoke remains outstanding. See the [provider
  contract](docs/providers.md).
- The browser opens on the overseer, with agent selection in either the factory
  floor or roster. Needs You, Queue, and the selected agent's controls share the
  right panel. Needs You and Queue expand rows in place. Send an
  instruction in a ready agent's terminal pane to create a durable
  task. Type in a live terminal to work with that session, or use Message for
  a recorded intervention. Codex Interrupt stops generation while preserving the
  task; Stop ends it; Start new preserves its history and queues a replacement.
  Completed output stays visible. Add to queue accepts later work while busy.
  Queues are grouped by agent and follow its admission order; numeric priority can be raised or
  lowered explicitly. Recent Work stays collapsed without reading private details. Opening it loads
  authorized details for the newest ten completed or blocked tasks, showing
  instruction and outcome excerpts; Show more loads the next ten. Expand a row
  for the full outcome, reported PR links, instruction, and review feedback,
  and to load its intervention history. Private prose stays out of public state.
  Project and agent setup stays in `factoryctl`.
- Agent status is Ready, Working, Needs you, or Paused. Working means a task
  is running, including startup and cleanup. Queued work alone is Ready with
  a queue/capacity hint. Pausing stops future tasks; an already-running task
  keeps Working with its queue marked paused.
- The overseer can inspect and control its project's workers through scoped
  `factoryctl overseer` commands, using its own attempt credential. Worker
  events and explicit interventions trigger bounded standing instructions,
  including events received while the overseer was busy. Factory capacity
  counts workers; one overseer can run alongside them.
- Optional [operator-owned GitHub intake and release scheduling](docs/development/UNATTENDED.md) runs on the host. There is no public HTTP intake or in-runtime updater.

Each project has agents and durable tasks. An admitted attempt gets a fresh
provider process and a daemon-owned `.git`-free Change. The browser and CLI
remain clients of the same local API; neither owns lifecycle or policy.

## Installation

The [installation guide](docs/install.md) covers the three binaries, managed
service, and paired browser console.

## Development

The [development workflow](docs/development/WORKFLOW.md) documents worktree,
temporary-home, test, and deterministic shell-provider helpers.

## Learn more

- [Installation](docs/install.md)
- [Provider contract](docs/providers.md)
- [Architecture](ARCHITECTURE.md)
- [Security](SECURITY.md)
- [Contributing](CONTRIBUTING.md)
- [Development workflow](docs/development/WORKFLOW.md)
- [Deploying the site and the live service](docs/development/DEPLOY.md)

Dark Factory is MIT licensed.
