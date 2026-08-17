# Contributing

## Before you send a change

```sh
./scripts/local-ci.sh
```

This is the authoritative gate (`cargo +1.85.0 fmt --check`, `clippy
--all-targets --all-features -D warnings` across the whole workspace, every
test with `--test-threads=1`, and `git diff --check`). GitHub Actions is
manual-only, so this local run is what actually protects `main` — a change
that doesn't pass it locally isn't ready to send.

A few workspace-wide rules the gate enforces, worth knowing up front:

- `unsafe_code = "forbid"` and `clippy::all = "warn"` at the workspace level;
  CI runs clippy at `-D warnings`, so zero warnings, not just zero errors.
- SQLite migrations are sequential numbered files under
  `crates/factoryd/migrations/`. Never edit or delete one that has already
  shipped — add a new one instead, even for a one-line fix.
- Never touch a real `$DARK_FACTORY_HOME` (default `~/.dark-factory`) or
  launchd from a test or a manual check; use a fresh temp directory (see
  `crates/factoryd/tests/sessions_e2e.rs`'s `private_tempdir` for the
  Unix-socket-path pitfalls a naive `tempdir()` runs into on macOS).

## Add a provider

See [docs/providers.md](docs/providers.md) — the whole contract is one
`Provider` trait (`spawn_spec` + `capabilities`) in
`crates/factoryd/src/providers/mod.rs`. `shell.rs` is the minimal reference
implementation to copy from.

## Add a theme

`crates/factory-tui/src/theme.rs` is one `Theme` struct and two consts
(`FORTRESS`, `PLAIN`) — every glyph the board draws for every concept
(agent roles, queue/capacity, attention badges, workshop routes) lives
there, nowhere else. To add one: define a new `pub const` (see `PLAIN` for
the minimal ASCII-only shape), add it to `Theme::parse`'s match, and wire
its name into `factory-tui`'s `--theme` flag parsing in `main.rs`. The
`glyph_tables_are_complete` test in `theme.rs` is the regression test that
catches a theme missing a glyph the board can actually draw.

## Good first issues

- [ROADMAP.md](ROADMAP.md) lists unfinished product work; anything under
  "Next" is more concretely scoped than "Later".
- Any test in `crates/factoryd/tests/sessions_e2e.rs` marked with a comment
  explaining a workaround (e.g. the settle-window in `wait_for_stable_idle`)
  is a known rough edge with a documented root cause, not just a flaky test
  — a cleaner fix is welcome.
- `crates/factoryd/src/providers/codex.rs`: Codex's own reported thread ID
  (from its `SessionStart` hook payload) is never persisted into
  `sessions.provider_session_id`, so a Codex session never resumes across a
  restart the way a Claude one does (see `ARCHITECTURE.md`'s "Deliberately
  unresolved"). Threading the raw hook payload through
  `Store::record_hook_event` is the shape of the fix; it touches roughly a
  dozen call sites in `crates/factoryd/tests/sessions_store.rs`.
- `factory-tui`'s `Board::apply_event`'s `TaskChanged` arm
  (`crates/factory-tui/src/model/mod.rs`) deliberately preserves whatever
  `body`/`result` it already has for a task, because the wire event only
  ever carries the durable `TaskSnapshot` — never the body. That's correct
  for a task the board already knows about, but a task created *after* the
  board's initial fleet-snapshot bootstrap has no prior `body` to preserve,
  so WORKSHOP's detail pane shows "(no body)" for it permanently (confirmed
  live: assign a task while `factory-tui` is already running and watch its
  detail pane). Smallest fix: add `body`/`result` to
  `FactoryEvent::TaskChanged` in `crates/factory-core/src/lib.rs` (mirrors
  how `SessionChanged` already carries its full snapshot inline) rather than
  having the client round-trip a `GetTask` after the fact.
- FORTRESS's selection model is entirely agent-based (`Board::selected_agent`
  drives `Tab` cycling and `Enter`-to-zoom in `crates/factory-tui/src/model/
  keymap.rs`), so a project with zero agents has no station glyph to select
  and is unreachable from FORTRESS by keyboard — only `factory-tui --project
  ID` at startup can focus one. Confirmed live: `project add` a second
  project, then try to `Tab`/`Enter` into it from FORTRESS before it has any
  agent. Smallest fix: let `cycle_selected_agent`/`zoom_in`'s `Fortress` arm
  also stop on (and zoom into) a project's empty-agents placeholder station,
  not just real agent glyphs — the WORKSHOP side already renders that case
  correctly (see the empty-state hints in `ui/workshop.rs`).
