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

`.github/workflows/ci.yml` never runs pull request code — from this
repository's own branches or a fork — on the maintainer's persistent Mac,
because a pull request's `checks` run uses the workflow file *the pull
request itself carries*, so a same-repository branch could edit `runs-on`
in its own diff exactly as easily as a fork could: no PR is trusted more
than any other before it's been reviewed. Every `pull_request` event runs
on an ephemeral hosted macOS runner instead, and a fork's workflow run
additionally needs the maintainer's approval before it runs at all. The
persistent Mac only ever executes a commit that's already on protected
`main` (via `main-review`'s CODEOWNERS-approval ruleset) or a maintainer's
own tag push in `release.yml` — both refs a pull request diff cannot
reach. An agent's own generated PR code is held to that same boundary: it
runs `local-ci.sh` on the hosted runner, never on the operator's machine,
until it's merged. A change to any of this is a security change and gets
reviewed as one. AGENTS.md's adversarial review explicitly includes
"security: nothing widens what an agent session, a webhook caller, or an
untrusted PR can reach".
