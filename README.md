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
- The browser console shows durable factory, agent, and task state. A paired
  browser can select a configured idle agent, submit a direct instruction, and
  create a normal durable task; dispatch, queue, and run state remain canonical
  in the daemon. The terminal appears only for a running task, where it
  supports observation and input, HumanRequest reply, and cancellation. Project
  and agent setup stays in `factoryctl`.
- There is no external HTTP/GitHub intake and no in-runtime updater.

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

Dark Factory is MIT licensed.
