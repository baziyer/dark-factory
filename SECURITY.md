# Security

## Reporting a vulnerability

Use [GitHub private vulnerability
reporting](https://github.com/baziyer/dark-factory/security/advisories/new).
Do not open a public issue for a capability, credential, process-ownership, or
network-boundary failure.

Dark Factory is pre-1.0. Only current `main` and the latest release receive
security fixes.

## Current freeze

The safe-kernel refactor is not a live release. Do not install or start this
revision, enable dispatch, submit provider work, add external intake, or use
the operator's `~/.dark-factory`. Stage 3 exact-head proof, independent review,
merge, and the separate boot review remain required.

## Threat model

Dark Factory is a local, single-operator application. Provider processes run as
the operator and use the operator's Claude/Codex subscription. The current
kernel prevents confused or cooperative providers from acting outside an exact
attempt; it does **not** isolate a hostile same-user process from readable
files, credentials, other processes, or the local socket. That claim requires
a separate OS user, container, or sandbox.

The only request boundary is a private Unix socket with owner-only
directory/socket modes. There is no HTTP webhook or generic connector listener.
Exposing the local API beyond the machine is external deployment work and is
unsupported.

## Principals and capabilities

Every request carries a versioned envelope and is resolved once as one of:

- **Anonymous**: health only.
- **Operator**: authenticated by the private operator credential. Operator
  commands administer durable state but cannot impersonate an attempt for
  completion, blocking, or hooks.
- **Attempt**: authenticated by a random bearer stored in a private per-run
  file. The store derives exact project, agent, task, run, role, provider, and
  Change scope. The bearer works only while that run is `running`.

Missing credentials never imply operator access. Bearers are redacted from
debug/display output and are not accepted in argv, environment variables,
events, logs, request payloads, or caller-selected identity fields. The first
transition to `finalizing` revokes attempt mutation authority atomically. Old,
forged, cross-project, taskless, and terminal credentials fail closed.

The provider environment contains `DARK_FACTORY_ATTEMPT_TOKEN_FILE`, which is
only the path to the private bearer file, not the bearer itself. When it is
present, `factoryctl` uses that attempt credential for every local-API request.
An operator-shaped command invoked by a provider is therefore authorized as the
attempt and rejected if outside its allowlist; it never falls back to
`operator.token`.

God/orchestrator credentials grant scheduling policy only. They cannot create
source paths, launch or finalize processes, change capacity or agents, publish
repositories, or submit another run's outcome.

## Process and cleanup safety

Before provider execution, `factoryd` durably records the admitted run and its
resources. `factory-runner` prepares a child blocked before `exec`, reports its
PID and process group, and waits. Only after the daemon records those exact
identities and transitions the run to `running` may the child execute.

Success, block, failure, cancellation, and exit converge through
`finalizing`. A restartable daemon finalizer is the only writer of `terminal`.
It matches exact resource identities before signalling, deleting, or
acknowledging them. Reused PIDs, paths, runner identities, and job labels are
reported as unresolved rather than touched.

Rust `Drop`, shell traps, provider exit handlers, and test harnesses may perform
fast cleanup but are not trusted for correctness. A run remains visibly
`finalizing` while any ephemeral resource is active or unresolved.

## Provider and tool boundary

Each admitted run gets one fresh non-interactive provider process and one
startup input. There are no taskless resident processes, PTY attach/input,
delivery replay, provider resume, or session outboxes.

Provider hooks are authenticated observations and bounded requests.
`PreToolUse` applies the durable tool-call budget and a conservative command
tripwire. The tripwire denies recognized destructive or credential-sensitive
commands, every recognized `git push` publication attempt, direct `cargo`,
`rustc`, and `rustup` invocation, direct launch from a recognized mutable
Cargo `target/.../{debug,release}` path, and unsupported shell syntax. Local
source-editing commands such as `git apply`, `git mv`, and `git rm` remain
permitted. Rust-policy verification belongs to `factoryctl task done`, not a
provider build surface. This is not a sandbox: interpreters, generated
programs, MCP tools, provider defects, and direct syscalls can evade string
inspection.

Auto mode can remove a provider's own approval prompts and therefore increases
risk within the same-user boundary. It never bypasses daemon authentication,
attempt scope, run phase, or finalization rules.

Provider output remains opaque and bounded. It never becomes lifecycle
authority and does not enter public events, local-API responses, or tracing.

## Source and repository boundary

`factoryd` is the only product creator and administrator of Changes. Worker
admission reserves one daemon-derived path for one task incarnation, and a
registered wrapper materializes one exact committed tree before the provider
can execute. The leased provider view is a plain writable directory with no
Git administrative locator. Factoryd exposes no repository status, commit,
push, pull-request, or publication operation.

Pre-kernel paths are retained only as `legacy_sources` metadata. They are never
statted, adopted, measured, leased, renamed, or deleted. Forgetting a legacy
record deletes only that row. Managed Change removal requires the exact typed
ID, current revision, durable inode identity, and absence of a live lease; a
replacement or ambiguous path remains visibly pending and is never touched.

This repository scoping reduces accidental delegation but is still not OS
isolation from a hostile same-user process.

## Build and storage boundary

The complete boot contract requires a Rust-policy completion to revoke attempt
authority and reap the provider before source selection. The daemon accepts a
private source snapshot only when canonical before/copy/after manifests agree,
then builds through one project-incarnation/toolchain cache. It copies Cargo's
top-level test executables into content-addressed staging under the run's
registered temporary root, records exact identity and digest, and verifies
both before launch. The stable snapshot supplies the working tree and is
rechecked before and after every top-level test; mutation fails verification
before another test launches. Fixtures are not copied into staging and doctests
are not run. Mutable Cargo sibling
discovery and one target per checkout are not accepted top-level launch paths.
Cargo dependency resolution may use the network through the registered
verifier process. Its registry, Git, and target data live inside the bounded
project cache, but Rust verification is not a network sandbox.

Identity and digest checks prevent accidental and cooperative substitution;
they do not create isolation from a hostile same-user process racing filesystem
operations. Stronger execution isolation requires a separate user, sandbox, or
container.

Resource reclamation may remove only exact, registered, unleased regenerable
cache data; unique retained Changes are never automatic cleanup targets. A
writer makes byte status incomplete. Its exact process group is reaped before
remeasurement, after which the byte policy converges; an already measured
over-limit cache is refused for a new verification. The daemon reports total
measured bytes plus protected entry count and recoverable failure count, and
does not claim an instantaneous filesystem byte ceiling while Cargo is writing.

## Bounded inputs and durable data

Local frames, hook payloads, guidance, messages, events, logs, and generated
configuration have hard size limits. SQLite uses durable
transactions for authority. Guidance and memory files are bounded content, not
an authority ledger.

Provider credentials, repository credentials, prompts, raw output, message
bodies, and source content do not belong in public events or diagnostic
projections.

## Contributor and CI boundary

Tests use a temporary `DARK_FACTORY_HOME`, explicit private socket, disposable
resource labels, and independent cleanup verification. They never inspect or
mutate the installed job or operator home and never send paid provider prompts
unless the task explicitly requires live validation.

A pull request can modify its own workflow, including `runs-on`. Maintainers
must inspect `.github/workflows/` before approving external CI. A green check
alone never authorizes merge: protected `main` also requires independent
CODEOWNERS review and resolved threads. Persistent CI runner isolation remains
a separate hardening concern.

Every security-sensitive PR receives an adversarial review that explicitly
tries stale credentials, cross-attempt identity, crash boundaries, resource
reuse, unauthorized source/repository selection, and accidental expansion of
the same-user claim.
