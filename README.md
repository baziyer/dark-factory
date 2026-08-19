# Dark Factory

Dark Factory exists to make long-running, autonomous software factories practical: persistent teams of coding agents that can plan, delegate, execute, review, and keep making progress with minimal supervision.

It is provider-agnostic by design. Claude Code, Codex, and future agent runtimes are interchangeable workers behind a durable local control plane. The goal is simple: turn a software backlog into continuous, observable progress.

Dark Factory runs coding agents in separate Git worktrees. A local daemon owns
their sessions, tasks, messages, and history. Sessions continue when you close
the terminal UI. Use `factoryctl` to control the factory. Use `factory-tui` to
watch it and respond when an agent needs you.

Dark Factory currently supports macOS on Apple silicon and Intel. The Linux
contributor preview targets Ubuntu x86-64 source builds and the deterministic
shell provider only. Claude Code and Codex CLI are explicitly unverified on
Linux; this preview makes no Linux archive, systemd, installation, or paid
provider claim.

## Install

After installing Git and at least one provider, install the Dark Factory
bootstrap:

```sh
brew install baziyer/tap/dark-factory
```

Sign in to the provider you will use:

```sh
claude auth login && claude auth status  # Claude Code
codex login && codex login status        # Codex CLI
```

To use a separate Codex account for factory sessions, set `CODEX_HOME` before
you sign in and keep it set for `init`:

```sh
export CODEX_HOME="$HOME/.codex-dark-factory"
codex login && codex login status
```

Initialize Dark Factory, add its active commands to your path, and check the
installation:

```sh
factoryctl init
echo 'export PATH="$HOME/.dark-factory/bin/current:$PATH"' >> ~/.zprofile
source ~/.zprofile
factoryctl doctor
```

`init` installs the active runtime in `~/.dark-factory/bin/current` and asks
before installing the background service. Its final next steps offer a first
project for an empty fleet, or `factoryctl status` and `factory-tui` when a
project already exists. `doctor` does not repair or
reconfigure the installation; its release check may refresh
`~/.dark-factory/update-check.json`. See [provider setup](docs/providers.md)
for other account choices. The [manual release
install](docs/install.md#manual-release-archive) covers archive selection,
checksum verification, and Gatekeeper recovery.

## Start a factory

Auto mode is on by default. It bypasses Claude permissions or Codex approvals
and sandboxing. Dark Factory still applies its tool deny policy. Run
`factoryctl auto off` before assignment to use provider-native approval for
future sessions. Read the [security policy](SECURITY.md) before you assign real
work.

`factoryctl agent status` reports each session's configured override separately
from the exact runtime values it could establish; unavailable provider metadata
is shown as `unreported`.

Run these commands in a Git repository:

```sh
factoryctl project add --id my-project --name "My project" --root "$PWD"
factoryctl project repository set --project my-project \
  --remote https://github.com/OWNER/REPOSITORY.git --base main

factoryctl agent add --id lead --project my-project \
  --role orchestrator --provider codex
factoryctl agent add --id worker-1 --project my-project \
  --role worker --provider codex --parent lead

factoryctl task add --id first-task --project my-project --agent worker-1 \
  --title "Make the first change" --body "Describe the result and its limits."

factory-tui --project my-project
```

Assignment starts delivery. You do not need a separate start command. Each
agent has a persistent session and its own worktree.

The project backlog is unassigned queued work. A worker queue is the ordered
work assigned to that worker; `factoryctl task add --agent ID` creates directly
in it atomically, `factoryctl task list --agent ID` shows that worker's work in
execution order, and `factoryctl task assign --project ID --task ID` moves queued work
between workers or back to the backlog. The inbox is for messages, and review
items remain separate from both.

Each agent starts with a 1,000 tool-call budget. An exhausted budget stops new
delivery and needs operator action. Use `factoryctl agent budget status` to
inspect it and `factoryctl agent budget reset` to reopen it. Run either command
with `--help` for the required project and agent arguments.

The daemon keeps a finite factory-wide live-session bound. New installs use a
conservative capacity of 4. Operators can inspect or change the managed
launchd setting with `factoryctl capacity status` and `factoryctl capacity set
8`; valid values are 1 through 64. Only `factoryd` restarts, while runner
processes, live session IDs, queued work, and durable state are preserved. A
higher value can increase concurrent provider/subscription use; a lower value
leaves saturated work queued. A failed reload or health check restores the
prior setting; the provider-session shell policy denies capacity mutation.

The TUI has two screens:

- **BUILDING** shows all projects, agents, work, and items that need you. Agent
  rows separate durable lifecycle from named current activity and age, show
  assigned work as `queue N`, and explicitly label missing or stale activity.
  A client/daemon version mismatch is called out with detach-and-relaunch
  guidance; relaunching the TUI never restarts the daemon or agents.
- **AGENT** shows one agent's terminal, queue, inbox, and settings.

Select an agent and press `Enter` to open AGENT. Press `i` or `Enter` to type
in its terminal. Press `Ctrl-]` to return control to the board. Press `g` to
jump to the next item in NEEDS YOU and open its explicit action card before
typing. The card distinguishes provider questions and permissions from worker
blocks, delivery/recovery failures, observer problems, exhausted budgets, and
inferred lifecycle state. Press `?` for all keys. Press `q` to detach.
Detaching does not stop any agent.

Mouse navigation uses the same selections as the keyboard: click the footer's
screen tabs, help/detach controls, visible agent/task/NEEDS YOU rows, or the
terminal pane. A pane click only focuses it; `Enter` or `i` starts typing.
Terminal mouse events are kept separate and reach the child only when its own
output has enabled an xterm mouse protocol; otherwise the wheel scrolls local
terminal history. If history is visible, the first child-bound coordinate event
returns to the live screen and is consumed; only a later event against the
redrawn live frame is forwarded.

For a quick check outside the TUI, use:

```sh
factoryctl status
factoryctl agent status --project my-project --agent worker-1
```

`factoryctl status` is a concise fleet summary for people, including each
bounded attention reason, its task/session/age, and a safe next action.
`factoryctl agent status` exposes the same structured attention projection for
one agent. Terminal attach failures are durable observer problems in that same
projection and disappear after a successful reattach without erasing an
independent provider question, permission, or delivery wait. Use `--json` when a
script needs the complete protocol frame. If a runner ends, is replaced, or
refuses the attach during the selection race, AGENT keeps the board visible,
reports the session/runner state in one concise message, and refreshes durable
fleet state; it never presents an empty exited pane.

You can answer an agent in its terminal or send a durable message:

```sh
factoryctl agent message --project my-project --to worker-1 \
  --body "Continue with the smaller fix."
```

Pause an agent before recovery work. After resolving a reported block, use
`task retry` to requeue a blocked task; it also requeues failed or cancelled
tasks. Use each command's `--help` option for its exact arguments.

Repository authority is write-once and must be set while the factory has no
live sessions; later configuration and retarget attempts are rejected. Inside
a session, `factoryctl git status|diff|commit|push` and `factoryctl pr
open|update` authenticate that exact session. They accept no caller-selected
worktree, branch, remote, force, delete, merge, or release target. Review and
merge remain independent operator actions.

## Update

```sh
brew upgrade baziyer/tap/dark-factory
factoryctl update --install
factoryctl doctor
```

Homebrew updates the bootstrap. `factoryctl update --install` updates the active
runtime and restarts the daemon; running agent sessions continue. Do not use
`brew services` for Dark Factory.

When the board shows `[u update vX.Y.Z]`, press `u` or click that exact action
to run the same verified install transaction without leaving the TUI. The
board reports download, verification, unpack, activation, reload, and health
progress, then replaces only its own process image with the active
release's digest-verified version-directory `factory-tui` (never a later
`bin/current` lookup). Its PID and current project/screen selection are
retained; daemon-supervised runners and provider sessions are not restarted.
This is a manual action: Dark Factory does not auto-update. If relaunch
preparation or exec fails, the active runtime and managed launchd job are
rolled back and the old TUI remains available with the error. An older viewer
still offers the action when the matching runtime was already installed from
`factoryctl`.
Detach is delayed while this transaction owns the runtime. A private durable
phase record lets the next viewer roll back a crash-interrupted mutation, or
commit a handoff only when the exact new managed daemon is already healthy.
That record is bound to the canonical factory home, socket, launchd plist,
operator UID, and job label; a viewer aimed at another daemon cannot consume
or act on it. Managed health requires the socket responder to be launchd's
exact job PID using the active runtime's sibling executables, not merely a
daemon that reports the expected version.

`factoryctl update` reports the invoking bootstrap, active runtime, latest
release, and install availability in human-readable lines. Use
`factoryctl update --json` for the machine-readable report.
Fetched and cached manifests accept only canonical stable `MAJOR.MINOR.PATCH`
versions, bounded safe asset fields, and exact lowercase SHA-256 digests.
Reusing an installed release requires both its archive identity and every
binary digest to match; executable-looking modified files are refused.

`brew uninstall dark-factory` removes only the bootstrap. The active runtime,
launchd job, and state remain. See the [service and uninstall
guide](launchd/README.md#uninstall) for safe removal and the [release and update
guide](docs/development/WORKFLOW.md#release-and-update) for rollback details.

## Learn more

- [TUI guide](crates/factory-tui/README.md)
- [Provider setup and behavior](docs/providers.md)
- [Architecture and safety rules](ARCHITECTURE.md)
- [External integrations](docs/webhooks.md)
- [Contributing](CONTRIBUTING.md)
- [Security policy](SECURITY.md)

## Build from source

Rust 1.88 or later is required.

```sh
cargo build --release --workspace
target/release/factoryctl init
```

Contributors must follow [AGENTS.md](AGENTS.md) and run
`./scripts/local-ci.sh` on macOS, or `./scripts/local-ci.sh --linux-source`
on Ubuntu, before they open a pull request.

### Linux contributor preview

On Ubuntu x86-64, install Rust 1.88 or later, then run the source smoke from a
clean checkout:

```sh
cargo +1.88.0 build --workspace
./scripts/linux-contributor-smoke.sh
```

The smoke uses a throwaway `DARK_FACTORY_HOME`, starts the source-built
`factoryd`, checks the private local socket, invokes `factoryctl` and
`factory-tui --version`, and completes one deterministic shell-provider task.
It then stops the exact resident session through `factoryctl`, proves its
runner descendants exit within a finite bound, shuts down the scratch daemon,
and removes the home only after the private socket is gone. An interrupted
cleanup regression exercises the same path and proves the scratch home is
removed without scanning or signaling unrelated processes.
The workspace tests additionally cover PTY attach/detach, process-group
cleanup, and daemon-restart recovery. Claude Code and Codex are not validated
by this preview and must not be treated as supported Linux providers yet.
