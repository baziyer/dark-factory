# Agent instructions

This file is the canonical agent guidance for this repository. `CLAUDE.md`
just points here; if anything else conflicts, this file wins.

## Project overview

Dark Factory is a pure-Rust, terminal-first runtime that turns a software
backlog into continuous agent progress: a durable queue, orchestrator, and
process supervisor for Claude Code and Codex CLI sessions, watched and
directed through `factory-tui`, a detachable Ratatui board. One operator
runs many agents from one machine; `factoryd` owns every session end to
end, so closing the CLI or the TUI never stops an agent.

It is not an Electron/Tauri/browser app, not a coding model, not an agent
pretending to be an employee, and not a general agent framework. See
[README.md](README.md) for how it works and [ARCHITECTURE.md](ARCHITECTURE.md)
for the invariants that constrain every change.

## Related repos

Read-only context unless a task explicitly asks you to edit them:

- `~/dark-factory-site`: the Next.js site (Vercel), future home of the
  hosted release manifest (see `docs/development/WORKFLOW.md`).
- `~/rust-hem-runner`: an unrelated project. Its `AGENTS.md`,
  `scripts/new-worktree.sh`, and `docs/development/RELEASE_WORKFLOW.md`
  are useful *style* references for worktree/release process, nothing
  more — don't port its product specifics here.

## Critical rules

1. **Work on a branch in its own worktree, never on `main`.**
   `./scripts/new-worktree.sh <slug>` creates `.worktrees/<slug>` on a new
   branch from `main`. Push the branch and open a PR; don't commit to
   `main` directly.
2. **Every PR gets an adversarial review before merge.** A second agent (or
   person) — not the author — reviews the diff trying to break it:
   correctness bugs, missed simplification, security. Steps: (a) author
   opens the PR with what changed and why; (b) reviewer reads the diff cold
   and posts findings as PR comments, explicitly including anything they
   tried to break and couldn't; (c) author addresses each finding or
   explains why not; (d) reviewer re-checks; (e) merge only once the
   reviewer is satisfied. The author never merges their own unreviewed PR.
3. **Remove or refactor over patch.** Every change should leave the
   codebase smaller or simpler than it found it, not just working. Delete
   dead code paths instead of leaving them unreachable; collapse
   duplicated logic into one place instead of adding a third copy; no
   speculative abstractions (interfaces/traits with one implementation);
   no feature flags for behavior that should just be decided; no silent
   fallbacks that hide a real failure behind a plausible-looking success.
4. **Simplest implementation over cleverness.** Prefer the boring, obvious
   fix. Maintainability beats a clever one-liner.
5. **Shared operation first; TUI primary.** Every daemon-owned runtime action
   is a `factoryd` local-API operation. Bootstrap, service-lifecycle, and
   update actions that must work while the daemon is absent live in shared
   Rust library code. `factory-tui` is the primary operator client;
   `factoryctl` provides parity for recovery, diagnostics, scripting, and
   automation. Neither client gets a shortcut or hidden behavior unavailable
   through the shared operation.
6. **Tests around the load-bearing paths**: queue durability, event
   projection (durable state → wire events → TUI model), PTY lifecycle,
   detach/reattach. A change to any of these needs a test that would have
   caught the bug it fixes.
7. **Run `./scripts/local-ci.sh` before finishing.** It is the
   authoritative gate (fmt, clippy at `-D warnings`, the full test suite,
   `git diff --check`). CI runs the same script on every pull request as
   the `checks` status the `main` ruleset requires; a PR that isn't green
   locally won't be green there either.
8. **Small, coherent commits.** One logical change per commit; don't bundle
   unrelated cleanup into a feature commit.
9. **Docs and issue state are load-bearing.** A change that alters behavior updates
   `README.md`/`ARCHITECTURE.md`/`docs/` in the same PR, not later, and a
   fix resolves every issue it was meant to solve rather than leaving stale
   work behind. For GitHub, use `Closes #N` in the PR when possible. For an
   external tracker or backlog, update its item to the equivalent
   resolved/closed state and link the change. The source of the issue does not
   matter: after the change lands, verify every tracked item is actually closed
   at its source. A doc or issue describing work that no longer exists is worse
   than no record at all.
10. **Issue bodies are the backlog contract.** Each GitHub issue body must be
    self-contained: required scope, decisions, evidence, status, and acceptance
    criteria belong in the body. Issue comments are ignored by intake and must
    not be required to understand or execute the work. Every external body
    revision is immutable and independently identified. Editing a body creates
    a new quarantined revision; accepting or readying one revision never amends
    accepted or running work. PR review comments remain required by rule 2.
11. **Never touch the operator's live install from a task.** `~/.dark-factory`,
    the installed `launchd` job, and the running daemon behind them are the
    owner's real system. Use a temporary `$DARK_FACTORY_HOME` and `--socket`
    for every test or manual check (see `docs/development/WORKFLOW.md`).
12. **Provider runs cost the owner's subscription.** Don't send a real
    prompt to `claude`/`codex` unless the task genuinely requires
    exercising a live session; prefer the `shell` provider or existing test
    fixtures for anything that doesn't need a real model.
13. **Report exactly what passed and failed.** State which commands you
    ran and their outcome; never imply a check passed that you didn't run.

14. **Use the shared model policy.** New routine Codex workers and focused
   reviewers use the Luna default; Sol/xhigh is reserved for an explicit
   high-risk escalation with a durable reason. God remains Sol/xhigh. See
   the project guidance for the one operator-facing policy and CLI examples;
   existing profiles are not silently rewritten.

## Adding to the system

- New provider, integration, or theme: see [CONTRIBUTING.md](CONTRIBUTING.md)
  for the shortest path and [docs/providers.md](docs/providers.md) for the
  provider contract.
- Known problems and their smallest fix: [GitHub issues labelled
  `known-issue`](https://github.com/baziyer/dark-factory/issues?q=is%3Aissue+is%3Aopen+label%3Aknown-issue).
- Day-to-day workflow and the implemented release/update process:
  [docs/development/WORKFLOW.md](docs/development/WORKFLOW.md).
