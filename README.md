# Dark Factory

A small Rust supervisor for coding-agent processes, with a disposable native
observer. The target is closer to systemd for Claude Code and Codex than a
desktop agent simulation.

The repository is being built in working vertical slices. Today it contains the
shared domain/event protocol and the daemon's SQLite state/event store. Runner,
CLI, and observer crates will arrive only as each slice becomes executable.

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
