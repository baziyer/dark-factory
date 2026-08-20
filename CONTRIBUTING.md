# Contributing

See [AGENTS.md](AGENTS.md) for the full workflow (worktree per task,
mandatory adversarial PR review, remove-or-refactor over patch) — this
file is just the shortest path to a useful first change.

## Before you send a change

```sh
./scripts/new-worktree.sh <slug>
# cd to the absolute path printed by the script
cargo build --workspace
```

The script resolves the repository's primary checkout and always creates new
linked worktrees under that checkout's single `.worktrees/` directory. Running
it from a resident or task-linked worktree therefore does not add another
nested `.worktrees/` level; existing nested worktrees are left in place. It
proves the destination absent, reserves its exact directory identity, and then
uses one native Git add. After reservation, any failure, signal, or identity
replacement is preserved and reported as an orphan; the script does not roll
back paths, registrations, or refs.

On macOS, run the complete release-compatible gate:

```sh
./scripts/local-ci.sh
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
  Batches found during a dogfood run are triaged first in a local,
  gitignored note under `docs/internal/` and turned into issues with
  `scripts/import-issues.sh <note.md>` (one issue per `###` section; #24–#40
  and #59–#76 came in that way).
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
