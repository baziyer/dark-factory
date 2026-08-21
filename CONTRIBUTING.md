# Contributing

See [AGENTS.md](AGENTS.md) for the full workflow (one development worktree per change,
mandatory adversarial PR review, remove-or-refactor over patch) — this
file is just the shortest path to a useful first change.

Live use remains frozen until an independent exact-main boot review passes.
Development fixtures use isolated temporary homes, explicit sockets, and
deterministic providers; never use the operator's installation.

## Before you send a change

```sh
./scripts/new-worktree.sh <slug>
cd .worktrees/<slug>
cargo build --workspace
```

On macOS, run the complete release-compatible gate:

```sh
./scripts/local-ci.sh
./scripts/macos-contributor-smoke.sh
```

On Ubuntu x86-64, run the source-only gate and contributor smoke instead:

```sh
./scripts/local-ci.sh --linux-source
./scripts/linux-contributor-smoke.sh
```

The source gate runs `cargo +1.88.0 fmt --check`,
`clippy --all-targets --all-features -D warnings` across the whole
workspace, every test with `--test-threads=1`, and `git diff --check`.
The macOS path additionally checks release-source, publisher, and package
fixtures. CI requires both platform jobs through the aggregate `required`
context, so contributors should run the command for their platform before
opening a PR.

A few workspace-wide rules the gate enforces, worth knowing up front:

- `unsafe_code = "forbid"` and `clippy::all = "warn"` at the workspace level;
  CI runs clippy at `-D warnings`, so zero warnings, not just zero errors.
- SQLite migrations are sequential numbered files under
  `crates/factoryd/migrations/`. Never edit or delete one that has already
  shipped — add a new one instead, even for a one-line fix.
- Never touch a real `$DARK_FACTORY_HOME` (default `~/.dark-factory`) or
  `launchd` from a test or a manual check — see
  [docs/development/WORKFLOW.md](docs/development/WORKFLOW.md) for a
  throwaway daemon on a temp directory instead.

## Where to start

- **A bug or a small gap**: [GitHub issues labelled
  `known-issue`](https://github.com/baziyer/dark-factory/issues?q=is%3Aissue+is%3Aopen+label%3Aknown-issue)
  each have a symptom, evidence (`file:line` or how it was observed), a
  suggested smallest fix, and a `size:S|M|L` label (`decision` when the
  maintainer has to choose, not code) — anything `size:S` is a reasonable
  first change. Found a new one? Open an issue with the bug template and
  label it `known-issue`; a fix closes it in the same PR (`Closes #N`).
- **A new provider**: see [docs/providers.md](docs/providers.md) — the
  whole contract is one `Provider` trait (`spawn_spec`)
  in `crates/factoryd/src/providers/mod.rs`. `shell.rs` is the minimal
  reference implementation to copy from.
- **A new theme**: `crates/factory-tui/src/theme.rs` is one `Theme` struct
  and two consts (`FORTRESS`, `PLAIN`) — every glyph the board draws for
  every concept (agent roles, queue/capacity, attention badges, workshop
  routes) lives there, nowhere else. Define a new `pub const` (see `PLAIN`
  for the minimal ASCII-only shape), add it to `Theme::parse`'s match, and
  wire its name into `factory-tui`'s `--theme` flag parsing in `main.rs`.
  The `glyph_tables_are_complete` test in `theme.rs` catches a theme
  missing a glyph the board can actually draw.
- **A kernel causal test**: use the proof matrix in
  `docs/development/SAFE_KERNEL_REFACTOR.md`. Process fixtures must register
  exact resources before use and include an independent post-test verifier;
  provider/test-process cleanup is not proof.

Every change updates docs in the same PR when it changes behavior — see
`AGENTS.md`'s "docs are load-bearing" rule.
