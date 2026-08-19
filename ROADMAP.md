# Dark Factory roadmap

Status: 18 August 2026

Dark Factory's current baseline is a durable local supervisor and native
control plane: resident PTY-backed Claude Code, Codex, and shell sessions;
daemon-restart recovery; a worktree per agent; transactional task, run,
message, budget, and repository state; a bounded append-only event ledger;
provider hooks; a detachable two-screen TUI; a CLI over the same local API;
authenticated loopback webhooks; daemon-owned repository writes; and
Homebrew-backed installation and in-place updates.

This file contains product direction and ordering, not a second backlog.
The linked GitHub issues own scope and acceptance. Current defects and
decisions remain in the [`known-issue` backlog](https://github.com/baziyer/dark-factory/issues?q=is%3Aissue+is%3Aopen+label%3Aknown-issue).

## Current actions and release targets

GitHub milestones are the release plan and the linked issues are the action
list. This table intentionally contains no copied task descriptions, so the
GitHub views remain the current source of truth:

| Target | Open actions |
| --- | --- |
| [v0.2.5 — seamless dogfood update](https://github.com/baziyer/dark-factory/milestone/3) | [milestone issues](https://github.com/baziyer/dark-factory/issues?q=is%3Aissue%20is%3Aopen%20milestone%3A%22v0.2.5%20%E2%80%94%20seamless%20dogfood%20update%22) |
| [v0.3.0 — autonomous dogfood](https://github.com/baziyer/dark-factory/milestone/1) | [milestone issues](https://github.com/baziyer/dark-factory/issues?q=is%3Aissue%20is%3Aopen%20milestone%3A%22v0.3.0%20%E2%80%94%20autonomous%20dogfood%22) |
| [v0.3.1 — Linux contributor preview](https://github.com/baziyer/dark-factory/milestone/5) | [milestone issues](https://github.com/baziyer/dark-factory/issues?q=is%3Aissue%20is%3Aopen%20milestone%3A%22v0.3.1%20%E2%80%94%20Linux%20contributor%20preview%22) |
| [v0.4.0 — GitHub-connected factory](https://github.com/baziyer/dark-factory/milestone/2) | [milestone issues](https://github.com/baziyer/dark-factory/issues?q=is%3Aissue%20is%3Aopen%20milestone%3A%22v0.4.0%20%E2%80%94%20GitHub-connected%20factory%22) |
| [v1.0.0 — production-ready](https://github.com/baziyer/dark-factory/milestone/4) | [milestone issues](https://github.com/baziyer/dark-factory/issues?q=is%3Aissue%20is%3Aopen%20milestone%3A%22v1.0.0%20%E2%80%94%20production-ready%22) |

For a participating issue or pull request, the operator-maintained workflow
convention is exactly one `state:*` label: `state:queued`,
`state:in-progress`, `state:blocked`, `state:review`, or
`state:release-ready`. The setup script defines this vocabulary but does not
yet validate or reconcile conflicting labels; App reconciliation is deferred
to #188 and the contract in #208. The milestone is the release target; area,
size, security, and `known-issue` labels retain their existing meanings. The
projection is deliberately bounded: public state, target, exact ref/SHA,
checks, and links are useful to contributors; agent/session details,
prompts, guidance, transcripts, raw provider output, credentials, and private
review deliberation remain internal. See the
[GitHub projection contract](https://github.com/baziyer/dark-factory/issues/208)
for idempotency and reconciliation requirements.

## Product direction

Dark Factory should be a capability-controlled operating system for local
agent work, not a growing collection of prompts and CLI commands. Keep these
axes separate:

- **Authority**: daemon-enforced identity, capabilities, and resource scope.
- **Persona**: versioned instructions, output contracts, and provider/model
  defaults that never increase authority.
- **Input**: provenance and trust policy for work entering the factory.
- **Workflow**: durable, repeatable transitions from input to outcome.
- **Review**: explicit proposed actions whose preconditions are revalidated
  before execution.

The local API is the boundary for daemon-owned runtime actions. Bootstrap,
service-lifecycle, and update actions that must work while the daemon is
absent live in shared Rust library code. `factory-tui` is the primary operator
client; `factoryctl` provides parity for recovery, diagnostics, scripting,
and automation. Neither client owns hidden behavior.

## Gate 1: safe control plane

- Enable repository security scanning (#105).
- Authenticate session principals and enforce scoped daemon capabilities
  (#133). The current worker/orchestrator distinction shapes behavior but is
  not a same-user authority boundary.
- Add an optional hardened session runner with a separate OS/container
  boundary (#125). This is distinct from CI runner isolation (#54).
- Default new factories to reviewed operation while preserving every existing
  installation's durable setting (#139).

Capability enforcement protects against mistaken or cooperative
cross-boundary requests. Only the hardened runner can claim protection from a
hostile same-user provider process; prompts, PATH restrictions, worktrees, and
the shell tripwire are not substitutes.

## Gate 2: TUI as the product

- Complete project and agent creation in the TUI through the shared local API
  (#30).
- Add TUI-led first-run setup, project planning, doctor/update, budgets, and
  normal administration after the safe default and creation operations exist
  (#128; depends on #139 and #30).
- Keep the BUILDING/AGENT operator model focused; validate repository activity
  visualization before adding a new subsystem (#122).

Reusable setup and request construction should move into existing library
modules first. Add another crate only when dependency direction or build cost
demonstrates a real boundary.

## Gate 3: controlled inputs and decisions

- Quarantine authenticated external input as durable envelopes and work
  candidates before it can become a task or message (#126).
- Add typed, ordered source policy and keep raw content explicitly untrusted.
- Add a durable proposed-action queue with allow/review/deny policy and
  precondition revalidation (#127).
- Put approval and edit/reject operations in the TUI with CLI parity.

HMAC proves which integration sent bytes; it does not make the upstream actor
or content safe to execute. Model-based prompt-injection detection may add
evidence, but cannot be the approval boundary.

## Gate 4: reusable operating patterns

- Plan and atomically apply portable project specifications while keeping
  effective authority and secrets operator-owned (#129).
- Add versioned, data-defined personas separate from authority roles (#130).
  Start with a few curated templates; do not open an executable pack ecosystem.
- Add a small durable workflow model with bounded retries/loops, fan-out/in,
  approval steps, idempotency, and explicit termination (#131).
- Turn orchestrator-to-worker delegation into a repeatable acceptance scenario
  and bounded public proof (#38).

Dynamic delegation remains useful inside an orchestrator's capabilities.
Deterministic workflows cover repeated procedures; they do not create a
second authority system or group-chat runtime.

## Gate 5: contributor and platform expansion

- Split fast contributor feedback from process-sensitive full CI without
  weakening `./scripts/local-ci.sh` as the authoritative gate (#132), alongside
  the concrete E2E fixture cleanup in #28.
- Execute the Linux epic (#120) through its existing mergeable slices: CI and
  runtime parity (#140), paths/socket semantics (#142), systemd lifecycle
  (#141), release packaging (#143), and provider validation (#144).
- Consider pinned data-only packs after authority, project specs, and workflow
  versioning are stable. Executable third-party hooks/services wait for a
  deliberate signing and trust model.

## Preserve these boundaries

- One small provider trait.
- One transactional store/event-append boundary, even if inherent `Store`
  methods are distributed by domain.
- Bounded private guidance and public event projections.
- The TUI's pure testable board model.
- Repository target inference and daemon-owned writes.
- A small conservative command tripwire that denies unsupported syntax rather
  than growing into an incomplete shell interpreter.

Large protocol, store, execution, webhook, or CLI files should be split only
along a domain boundary exercised by the work above, without speculative
single-implementation traits or a standalone cleanup epic.

## Product boundaries

- Usage is normalized provider headroom and tool-call consumption, not
  trustworthy monetary accounting. Cost forecasts require explicit usage or
  billing observations; unknown remains unknown.
- Public events stay bounded and free of guidance content, message bodies, raw
  input/provider output, secrets, and tracing payloads.
- Desired repository configuration is never the final source of authority.
- Scheduling and executable third-party extensions remain later work.
