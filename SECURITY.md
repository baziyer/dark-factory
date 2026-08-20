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
  a session can ask it to do. Auto mode is on by default: Claude bypasses
  permissions and Codex bypasses approvals and its sandbox. These processes
  can read, modify, execute, delete, and transmit anything accessible to
  the operator's OS user, including credentials. `factoryctl auto off`
  changes the default for future sessions; an explicit agent profile
  permission mode wins for that agent.
- **Remote repository writes use a narrower daemon capability.** Sessions do
  not inherit token variables or `SSH_AUTH_SOCK`; their environment also
  resets Git credential helpers, disables Git SSH and prompts, and hides the
  operator's `gh` configuration. `factoryctl git status|diff|commit|push` and
  `factoryctl pr open|update` authenticate the live session token and let the
  daemon operate only on that session's exact worktree and `agent/<id>` ref.
  Callers cannot provide a path, remote, refspec, force/delete flag, or PR
  head. Request/result events omit diffs, commit/PR prose, and credentials;
  merge and release remain external-reviewer/operator actions. This reduces
  accidentally delegated authority, but is not OS isolation: a process
  already running as the operator can still search filesystem-readable
  credentials or evade environment policy. The optional hardened agent
  runner in #125 is the planned outer security boundary.
  The operator first pins each project's canonical remote and PR base in the
  durable store, before any agent session exists; later first-writer or
  retarget attempts are rejected. Privileged Git pins the linked-worktree
  gitdir and common-dir path and inode, and runs with empty global/system config and a
  daemon-created temporary gitdir/index; repository hooks, clean filters,
  fsmonitor, credential/SSH helpers, external diff drivers, URL rewrites, and
  non-allowlisted protocols are inert. GitHub credentials travel only through
  the canonical, non-writable `gh` credential helper's pipe, never a child
  environment or process argument. Commit publication is a compare-and-
  swap against the validated HEAD. Output is streamed into a hard bound; a
  timeout, overflow, or retained descendant pipe kills and reaps the entire
  command process group.
- **The hook policy is a tripwire, not a sandbox.** Every ordinary provider
  tool call reaches an authenticated `PreToolUse` hook. The daemon denies
  recognizable `factoryctl capacity set`, force-push, pushed-ref deletion,
  branch-delete, and reset-hard commands. It permits
  `rm -rf` only when every literal normalized target is inside the agent
  worktree; changed cwd and compound destructive deletion are denied. It
  also denies direct structured file-tool access, recognized file commands,
  and redirections involving
  named secret paths (`.ssh`, `.aws`, `.gnupg`, `.env`, Codex `auth.json`,
  credentials, and gcloud config). Bash commands use an intentionally small
  accepted grammar: simple commands, assignments, `env`/`command`/`builtin`/
  `exec` and plain `sudo` wrappers, quotes and backslash escapes, separators,
  pipelines, syntactic redirections, and heredocs. Quoted metacharacters are
  ordinary arguments, not shell operators. Quoted heredoc bodies are literal;
  unquoted bodies are accepted only without expansion characters. Variable
  and command substitution, backticks, process substitution, globs,
  malformed quoting/redirection, and wrapper options whose operands are
  ambiguous deny the whole tool call as `unsupported_shell_syntax`. Auto
  mode remains on by default because ordinary GitHub review/comment commands
  fit this grammar (including quoted bodies and quoted heredocs); a denied
  command must be rewritten into the accepted form or run with auto mode off.
  The hook fails closed if the daemon cannot answer and records each decision
  as an append-only event. This narrow parser is still not a sandbox:
  interpreters, generated scripts, MCP tools, provider bugs, or direct
  syscalls can evade a string-level hook policy; it does not protect the
  operator from a malicious agent. Worktrees provide collision isolation,
  not filesystem or credential isolation. A separate OS user remains the
  planned outer boundary (#125). CI runner isolation is a separate boundary
  tracked in #54.
- **Budgets are a tool-call circuit breaker, not monetary accounting.** Each
  agent defaults to 1,000 authenticated `PreToolUse` calls per reset. The
  daemon durably counts observations, pauses delivery and denies subsequent
  calls at exhaustion, and requires an explicit reset (changing the limit
  alone never reopens an exhausted circuit). The
  shipped provider hook protocols do not report trustworthy per-agent token,
  subscription, or currency spend, so those values are unavailable rather
  than estimated. Calls that bypass hooks also bypass this limit; provider
  billing controls remain the actual monetary boundary.
  Ordinary pause and budget exhaustion are separate durable holds, and both
  spawn and delivery consult exhaustion directly rather than trusting a
  shared or cached pause projection.
- **Agent API calls have one authenticated principal.** The documented
  operator socket remains credentialless and owner-only. A new provider
  session instead receives the path of a separate private session socket and
  a fresh 32-byte bearer in an atomic `0600` file under its `0700` runtime
  directory; the bearer itself is never placed on argv or directly in an
  environment value. The session endpoint rejects missing, invalid, ended, or
  legacy credentials. The daemon derives project, agent, session, message
  sender, permitted message-recipient hierarchy, and exact current task/run
  ownership; request bodies cannot select those identities. This first tranche
  permits provider hooks, repository operations, self/parent/approved-
  descendant messaging, and exact current-session completion/blocking. Every
  other local request, including observation and subscriptions, remains
  operator-only. Completion and blocking bind the credential's exact session
  to the canonical transactional `session_work` transition.

  This boundary protects against cooperative or buggy agents accidentally
  using another agent's or the operator's authority. It is **not hostile
  same-OS-user process isolation**: a malicious process already running as the
  operator may inspect another process, discover filesystem paths, or open the
  credentialless operator socket. Issue #125 remains the planned hardened outer
  execution boundary. The daemon still spawns runners with a fixed non-secret
  environment allowlist, not its own ambient environment.
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

- An agent evading the hook tripwire or doing something harmful with the
  operator-level access auto mode intentionally grants it.
- Vulnerabilities in `claude`, `codex`, or the models behind them.
- Anything requiring the attacker to already run code as your user.

## For contributors

By default, `.github/workflows/ci.yml`'s `checks` job runs pull request
code on an ephemeral, GitHub-hosted macOS runner, not the maintainer's
persistent Mac — but only for a PR that leaves `runs-on` unmodified,
because a PR's `checks` run uses the workflow file *the PR itself
carries*. A fork's workflow run additionally needs the maintainer's
approval before it runs at all, and approval — not the runner choice — is
the real gate: the maintainer reads `.github/workflows/` in the fork's
diff before approving it, because an approved fork run that edited
`runs-on` does execute on the persistent Mac (self-hosted runners accept
any job matching their label, from any workflow or branch). A
same-repository branch — including an agent's own generated PR, before
any review — gets no approval gate at all and can route itself to the
persistent Mac the same way; only the maintainer's own GitHub account can
push one, and `workflow_dispatch` is the same trust level again (any
write-access collaborator can already run `checks` against any ref
manually). So this boundary keeps the *default and accidental* path onto
the persistent Mac closed, not a *determined* one from an account that
already has push and daemon access there — the actual backstop for a
same-repository PR is that a green `checks` run never authorizes a merge
by itself (`docs/development/WORKFLOW.md`'s `main-review` ruleset still
requires CODEOWNERS approval). Isolating the persistent runner itself
from the operator's own account is the remaining hardening and is not
done yet: [issue #54](https://github.com/baziyer/dark-factory/issues/54).
Full enumeration of every route: `docs/development/WORKFLOW.md`, "CI and
GitHub". A change to any of this is a security change and gets reviewed
as one. AGENTS.md's adversarial review explicitly includes "security:
nothing widens what an agent session, a webhook caller, or an untrusted
PR can reach".
