# Dark Factory

Dark Factory turns a software backlog into supervised coding-agent work on
your own machine. `factoryd` owns the durable queue and provider processes;
`factoryctl` controls it, and `factory-tui` is a detachable terminal view over
the same local API. It is not a coding model, hosted service, or general agent
framework.

## Model

- A **project** points at a local source repository and holds shared policy.
- An **agent** has a role, provider, settings, and ordered task queue.
- A **task** is durable work; assigning it makes it eligible for admission.
- An **attempt** (stored as a run) is one execution of one task.
- A **Change** is the retained, `.git`-free source tree used by worker attempts
  and retries.

Each admitted attempt gets a fresh, non-interactive provider process. Its
credential resolves only while that run is running, and the daemon derives the
caller's stored attempt identity instead of accepting a caller-selected one.
Attempt authority is revoked before cleanup and configured completion checks.
Dark Factory supplies no commit, push, or pull-request surface. Closing the CLI
or TUI does not stop active work.

See [ARCHITECTURE.md](ARCHITECTURE.md) for the detailed lifecycle and
[SECURITY.md](SECURITY.md) for threat boundaries.

## Availability

Dark Factory is pre-1.0. Current `main` is not approved for installation or
live provider work. There is no supported install command for this revision:
do not run `factoryctl init`, enable `factoryctl dispatch on`, update a live
installation, or point a source build at `~/.dark-factory`.

Supported installation steps will return only when a revision is approved for
live operation.

## Safe source preview

Rust 1.88 or later is required. This preview starts an empty daemon in a
throwaway home, checks its local API, and stops that exact daemon. It does not
submit provider work or use a Claude or Codex subscription.

```sh
cargo +1.88.0 build --locked --workspace

DF_DEV_HOME="$(mktemp -d /tmp/dark-factory.XXXXXX)"
chmod 700 "$DF_DEV_HOME"
DARK_FACTORY_HOME="$DF_DEV_HOME" \
  target/debug/factoryd --socket "$DF_DEV_HOME/factory.sock" &
DF_DAEMON_PID=$!

for _ in 1 2 3 4 5; do
  DARK_FACTORY_HOME="$DF_DEV_HOME" \
    target/debug/factoryctl --socket "$DF_DEV_HOME/factory.sock" health \
    >/dev/null 2>&1 && break
  sleep 1
done
DARK_FACTORY_HOME="$DF_DEV_HOME" \
  target/debug/factoryctl --socket "$DF_DEV_HOME/factory.sock" health
DARK_FACTORY_HOME="$DF_DEV_HOME" \
  target/debug/factoryctl --socket "$DF_DEV_HOME/factory.sock" status

kill "$DF_DAEMON_PID"
wait "$DF_DAEMON_PID"
```

For development work, use an isolated branch/worktree and the fuller
[development workflow](docs/development/WORKFLOW.md). It includes the
authoritative local gate and deterministic shell-provider fixtures.

## Operator surface

These are the main day-to-day entry points once live operation is supported:

```sh
factoryctl status
factory-tui

factoryctl run list --project PROJECT_ID
factoryctl run stop --project PROJECT_ID --run RUN_ID
```

Run `factoryctl --help` or `factoryctl <command> --help` for the exact project,
agent, and task operations. The CLI remains the canonical control path; the TUI
does not have a separate mutation API.

## Learn more

- [TUI guide](crates/factory-tui/README.md)
- [Provider contract](docs/providers.md)
- [Architecture](ARCHITECTURE.md)
- [Security](SECURITY.md)
- [Contributing](CONTRIBUTING.md)
- [Development workflow](docs/development/WORKFLOW.md)

Dark Factory is MIT licensed.
