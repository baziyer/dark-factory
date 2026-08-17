# Security

## Reporting a vulnerability

Please report privately through GitHub's private vulnerability reporting:
<https://github.com/baziyer/dark-factory/security/advisories/new>. Do not
open a public issue for anything that could let a session, a local process,
or a network peer do more than this document says it can.

This is a one-maintainer project. Expect an acknowledgement within seven
days and a fix or a documented decision as soon as one is possible; the
advisory is where that conversation happens.

## Supported versions

Pre-1.0: only the current `main` (and the latest tagged release, once
releases exist) receives fixes.

## What Dark Factory promises

Dark Factory is a **local, single-operator** runtime. Its security boundary
is the operating-system user it runs as; it does not try to protect the
operator from their own agents. Concretely:

- **No public network listener.** The control plane is a private Unix
  socket (`0600`, parent directory `0700`, owned by the current user; custom
  paths are refused otherwise). The only HTTP listener is the optional
  loopback webhook (`127.0.0.1`, one configured endpoint, secret from an
  owner-only file); exposing it beyond the machine is deliberately external.
- **Provider processes run as you, with your subscriptions.** A `claude` or
  `codex` session is your CLI, authenticated the way your shell's is, in a
  git worktree the daemon created. Whatever that CLI can do on your machine,
  a session can ask it to do. Codex runs under its `workspace-write`
  sandbox; Claude Code keeps its native permission prompts, with only
  `Bash(factoryctl *)` pre-approved (see `README.md`, "Unattended
  operation"). An agent's own `permission_mode` widens or narrows that.
- **Hooks are authenticated; the rest is your user.** A provider's hook
  invocations identify their session by a per-session random token in a
  `0600` file (never on argv or in the environment). An agent's own `task
  done`/`task blocked`/`agent message` calls, and every operator command,
  are plain local-API requests: whoever can open the socket — any process
  running as you, including every session — can make them, naming any
  agent. Per-session authentication of those calls is planned (roles and
  a review queue), not present. The daemon spawns runners with a fixed
  non-secret environment allowlist, not its own ambient environment.
- **Bounded inputs everywhere.** Guidance files, hook payloads, local-API
  frames, retained terminal logs, and webhook bodies all have hard size
  caps; raw provider output never enters public events, webhook responses,
  or tracing.
- **The daemon writes only into what it owns** — `$DARK_FACTORY_HOME` —
  plus three documented, minimal writes elsewhere: a worktree pre-trust
  entry in `~/.claude.json` (only if it already exists and parses); a
  filtered copy of your `~/.codex/config.toml` into a per-agent
  `CODEX_HOME` (with `auth.json` symlinked, never copied); and, in each
  project's own git repository, `git worktree add -b agent/<id>` per agent
  (the worktree and its `.git/worktrees/<id>` metadata go when the agent
  is deleted; the `agent/<id>` branch stays in your repository). A project
  whose root is not a git repository has its sessions run directly in that
  root.

## Out of scope

- An agent doing something harmful with access it legitimately has (that is
  the provider CLI's permission model and your task design, not a Dark
  Factory boundary).
- Vulnerabilities in `claude`, `codex`, or the models behind them.
- Anything requiring the attacker to already run code as your user.

## For contributors

`.github/workflows/ci.yml` runs pull requests from this repository's own
branches on the maintainer's persistent Mac and pull requests from forks on
an ephemeral hosted runner. That split is decided by the workflow file the
pull request itself carries, so it is a policy, not a mechanism: every
workflow run from a fork needs the maintainer's approval, and the
maintainer reads `.github/workflows/` in the fork's diff before granting
it. A change to that boundary is a security change and gets reviewed as
one. AGENTS.md's adversarial review explicitly includes "security: nothing
widens what an agent session, a webhook caller, or an untrusted PR can
reach".
