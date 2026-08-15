# Dark Factory

A small Rust supervisor for coding-agent processes, with a disposable native
observer. The target is closer to systemd for Claude Code and Codex than a
desktop agent simulation.

The repository is being built in working vertical slices. Today it contains the
shared protocol, the daemon's SQLite state/event store, a versioned local Unix
socket, a small machine-readable CLI, and a stable provider-blind process
runner. Provider adapters and the observer remain deliberately absent until
their vertical slices are executable.

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
cargo run -p factoryctl -- events --follow
```

Commands emit versioned JSON frames. Pass a returned project ID to
`task add --project ID --title TITLE --body BODY`. Both programs use
`$DARK_FACTORY_SOCKET`, then `$DARK_FACTORY_HOME/f.sock`, then
`$HOME/.dark-factory/f.sock`; `--socket PATH` has highest precedence.
List commands expose `--after` and `--limit` cursors. Event subscriptions emit
their durable replay boundary and a `caught_up` frame before live events.

State is private by construction. Custom database and socket paths must each
have an immediate parent directory owned by the current user with mode `0700`;
files and sockets use mode `0600`. The default state directory is created with
those permissions automatically.
