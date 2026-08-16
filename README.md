# Dark Factory

A small Rust supervisor for coding-agent processes, with a disposable native
observer. The target is closer to systemd for Claude Code and Codex than a
desktop agent simulation.

The repository is being built in working vertical slices. Today it contains the
shared protocol, the daemon's SQLite state/event store, a versioned local Unix
socket, a small machine-readable CLI, and a stable provider-blind process
runner. The daemon now has a default-deny launch boundary that keeps ambient
credentials out of runners and transfers bounded task input only over stdin.
Concrete Codex 0.147 and Claude Code 2.1.233 adapters build fresh and resumed
stdin-only turns and normalise their JSONL into bounded, structurally filtered
observations. A durable execution ledger now reserves tasks atomically, binds
provider sessions, records run attempts, and supports exact runner replay and
terminal reconciliation. A bounded Codex-only execution actor now launches and
recovers stable runners without coupling their lifetime to the daemon. The
local control plane can now create Codex agents and durably reserve queued tasks
for them. Exact runner observation health is durable, so a restart cannot
mistake degraded supervision for proof that an agent stopped; native
observation remains deliberately absent until its vertical slice is executable.

## Non-goals

- embedding agent runtimes in the UI;
- simulating an office or rendering continuous animation;
- parsing terminal escape sequences when a provider exposes structured output;
- designing a general distributed workflow engine before local dogfooding earns it.

See [ARCHITECTURE.md](ARCHITECTURE.md) for the invariants that constrain each
slice.

## Development

```sh
cargo test --locked --workspace
cargo clippy --locked --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
```

## Local control plane

Start the daemon and use `factoryctl` from another shell:

```sh
cargo run -p factoryd
cargo run -p factoryctl -- health
cargo run -p factoryctl -- project add --name dark-factory --root "$PWD"
cargo run -p factoryctl -- project list
cargo run -p factoryctl -- agent add --project PROJECT_ID --role orchestrator
cargo run -p factoryctl -- task add --project PROJECT_ID --title "First task" --body "Do the work"
cargo run -p factoryctl -- task start --project PROJECT_ID --task TASK_ID --agent AGENT_ID --worktree "$PWD"
cargo run -p factoryctl -- events --follow
```

Commands emit versioned JSON frames. `task start` returns `run_accepted` once
the task reservation is durable; runner readiness and completion arrive as
events. Both programs use
`$DARK_FACTORY_SOCKET`, then `$DARK_FACTORY_HOME/f.sock`, then
`$HOME/.dark-factory/f.sock`; `--socket PATH` has highest precedence.
List commands expose `--after` and `--limit` cursors. Event subscriptions emit
their durable replay boundary and a `caught_up` frame before live events.

By default `factoryd` starts the sibling `factory-runner`, resolves `codex`
through the sanitized launch environment, stores runner state under
`$DARK_FACTORY_HOME/runs`, and allows four active runs. Use `--runner`,
`--codex`, `--runtime-root`, and `--max-active-runs` to set those explicitly.

State is private by construction. Custom database and socket paths must each
have an immediate parent directory owned by the current user with mode `0700`;
files and sockets use mode `0600`. The default state directory is created with
those permissions automatically.
