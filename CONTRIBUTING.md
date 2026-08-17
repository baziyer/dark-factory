# Contributing

See [AGENTS.md](AGENTS.md) for the full workflow (worktree per task,
mandatory adversarial PR review, remove-or-refactor over patch) — this
file is just the shortest path to a useful first change.

## Before you send a change

```sh
./scripts/new-worktree.sh <slug>
cd .worktrees/<slug>
cargo build --workspace
./scripts/local-ci.sh
```

`local-ci.sh` is the authoritative gate (`cargo +1.85.0 fmt --check`,
`clippy --all-targets --all-features -D warnings` across the whole
workspace, every test with `--test-threads=1`, and `git diff --check`).
CI runs the same script on every pull request (the `checks` status
`main` requires), so a local pass is what makes a PR mergeable.

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
  each have a symptom, evidence, a suggested smallest fix, and a size —
  anything `size:S` is a reasonable first change.
- **A new provider**: see [docs/providers.md](docs/providers.md) — the
  whole contract is one `Provider` trait (`spawn_spec` + `capabilities`)
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
- **A rough edge in the tests**: any test in
  `crates/factoryd/tests/sessions_e2e.rs` with a comment explaining a
  workaround (e.g. the settle-window in `wait_for_stable_idle`) is a known
  rough edge with a documented root cause, not just a flaky test — a
  cleaner fix is welcome.

Every change updates docs in the same PR when it changes behavior — see
`AGENTS.md`'s "docs are load-bearing" rule.
