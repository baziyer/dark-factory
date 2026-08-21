# Dark Factory

Dark Factory is a local, terminal-first control plane for turning a software
backlog into bounded coding-agent runs. `factoryd` owns durable work,
capabilities, processes, and cleanup; `factoryctl` and `factory-tui` are
clients of the same private local API.

> **Development freeze:** `main` is part-way through the
> [safe-kernel refactor](docs/development/SAFE_KERNEL_REFACTOR.md). Do not
> install or start this revision, enable auto mode, submit provider work, or
> point it at `~/.dark-factory`. Stages 1 and 2 are merged. Stage 3 is being
> implemented on its isolated branch; its exact-head gates, independent review,
> merge, and the separate boot review are still incomplete.

## The smaller runtime model

There are no resident provider sessions. One admitted run owns one fresh,
non-interactive provider process:

```text
queued task -> admitted -> running -> finalizing -> terminal
                              |             |
                              |             +-- authority revoked; resources reaped
                              +-- exact attempt bearer grants bounded operations
```

- A queued task grants no process or mutation authority.
- Admission records the exact task incarnation, run, capability, and declared
  resources before a provider can execute.
- `factory-runner` prepares a blocked child. `factoryd` records its PID and
  process group, moves the run to `running`, and only then releases it to
  `exec`.
- Success, block, cancellation, spawn failure, and unexpected exit all enter
  `finalizing`. One restartable daemon finalizer releases exact registered
  resources before writing `terminal`.
- A provider request carries a private attempt bearer. The daemon derives its
  project, agent, task, run, role, and source scope; callers do not select
  those identities.
- Provider processes receive only the path to that bearer in
  `DARK_FACTORY_ATTEMPT_TOKEN_FILE`. `factoryctl` uses the ambient attempt
  credential for every provider-invoked request, so an operator-shaped command
  is checked as that attempt and refused rather than upgraded to operator
  authority.
- God/orchestrators may propose priority and assignment. They do not own
  admission, source, processes, finalization, capacity, or administration.

The safe kernel removes PTY attach, terminal input, provider resume, delivery replay,
session outboxes, message-only provider turns, and per-agent worktree
creation. Historical session events may still decode for old databases, but
the live runtime does not create sessions.

## Current stage boundary

`factoryd` now reserves one Change for an exact task incarnation and
materializes a plain writable source tree from one exact local commit before
the worker can execute. The provider view contains no `.git` locator and is
not a linked Git worktree. Provider mutations remain in that retained Change;
status, commit, push, pull-request, and publication operations are deliberately
absent.

Paths retained from the pre-kernel architecture are metadata-only
`legacy_sources`. Factoryd never stats, adopts, measures, leases, renames, or
deletes them. An operator may forget only the metadata record by its typed ID.

Orchestrator policy runs may use their bounded guidance/project policy
context, but this revision is still not a boot candidate. The Stage 3 branch
adds an operator-selected `None`/`RustWorkspaceTest` completion policy, one
shared project/toolchain cache, attempt-owned immutable test-executable
staging, and convergent cache reclamation. Providers receive no generic build
API: direct Rust toolchains are refused by their cooperative hook policy, and
`factoryctl task done` owns configured final verification after the provider
has been reaped. A separate exact-head review must accept those claims before
merge or boot review.

## Repository layout

- `factory-core`: bounded domain and wire types.
- `factory-runner`: provider-blind process host with prepare/activate gating.
- `factoryd`: durable admission, authority, resource ownership, and
  finalization.
- `factoryctl`: operator and attempt-scoped local-API client.
- `factory-tui`: operator projection over the same API.

The five crates are process and dependency boundaries, not an invitation to
add service or repository abstractions. The daemon keeps one SQLite `Store`.

## Development

Rust 1.88 or later is required.

```sh
./scripts/new-worktree.sh <slug>
cd .worktrees/<slug>
cargo build --workspace
./scripts/local-ci.sh
```

On Ubuntu x86-64, use `./scripts/local-ci.sh --linux-source`. Tests and manual
checks must use a temporary `DARK_FACTORY_HOME` and explicit temporary socket.
Never exercise a real Claude or Codex subscription when the deterministic
shell fixture proves the behavior.

Every pull request requires a cold adversarial review by someone other than
the author. The reviewer tries to break correctness, security, and the claimed
simplification before merge. See [AGENTS.md](AGENTS.md) and the
[development workflow](docs/development/WORKFLOW.md).

## Documentation

- [Architecture and invariants](ARCHITECTURE.md)
- [Safe-kernel epic and progress](docs/development/SAFE_KERNEL_REFACTOR.md)
- [Provider contract](docs/providers.md)
- [Security boundary](SECURITY.md)
- [Contributing](CONTRIBUTING.md)
- [Installation and service freeze](docs/install.md)

Dark Factory is MIT licensed. It is pre-1.0 and currently deliberately
unavailable for live operation while the safe kernel is completed.
