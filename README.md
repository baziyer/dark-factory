# Dark Factory

Dark Factory exists to make long-running, autonomous software factories practical: persistent teams of coding agents that can plan, delegate, execute, review, and keep making progress with minimal supervision.

It is provider-agnostic by design. Claude Code, Codex, and future agent runtimes are interchangeable workers behind a durable local control plane. The goal is simple: turn a software backlog into continuous, observable progress.

Dark Factory runs coding agents in separate Git worktrees. A local daemon owns
their sessions, tasks, messages, and history. Sessions continue when you close
the terminal UI. Use `factoryctl` to control the factory. Use `factory-tui` to
watch it and respond when an agent needs you.

Dark Factory currently supports macOS on Apple silicon. It supports Claude
Code, Codex CLI, and a shell provider for tests.

## Install

Install Git and at least one provider. Run the matching login and check before
you run `init`:

```sh
claude auth login && claude auth status  # for Claude Code
codex login && codex login status        # for Codex CLI
```

To keep factory Codex sessions on a separate account, set up a dedicated home
first. Keep `CODEX_HOME` set when you later run `factoryctl init`:

```sh
export CODEX_HOME="$HOME/.codex-dark-factory"
codex login && codex login status
```

See the [provider guide](docs/providers.md) for other account choices. Then
download and verify the latest Dark Factory release:

```sh
work_dir=$(mktemp -d /tmp/dark-factory-install.XXXXXX)
curl -fsSL https://github.com/baziyer/dark-factory/releases/latest/download/latest.json \
  -o "$work_dir/latest.json"
asset_url=$(plutil -extract assets.aarch64-apple-darwin.url raw -o - "$work_dir/latest.json")
asset_sha=$(plutil -extract assets.aarch64-apple-darwin.sha256 raw -o - "$work_dir/latest.json")
curl -fL "$asset_url" -o "$work_dir/dark-factory.tar.gz"
printf '%s  %s\n' "$asset_sha" "$work_dir/dark-factory.tar.gz" | shasum -a 256 -c -
tar -xzf "$work_dir/dark-factory.tar.gz" -C "$work_dir"
"$work_dir/factoryctl" init
```

The commands use `curl` to avoid macOS browser quarantine. If you use a browser,
clear quarantine from the extracted directory before you run `init`:

```sh
xattr -dr com.apple.quarantine /path/to/extracted/dark-factory
```

`init` installs the binaries in `~/.dark-factory/bin/current`. It asks before
it installs the background service. Add the installed commands to your shell:

```sh
echo 'export PATH="$HOME/.dark-factory/bin/current:$PATH"' >> ~/.zprofile
export PATH="$HOME/.dark-factory/bin/current:$PATH"
factoryctl doctor
```

`doctor` reports each failed check and does not change your system.

## Start a factory

Auto mode is on by default. It bypasses Claude permissions or Codex approvals
and sandboxing. Dark Factory still applies its tool deny policy. Run
`factoryctl auto off` before assignment to use provider-native approval for
future sessions. Read the [security policy](SECURITY.md) before you assign real
work.

Run these commands in a Git repository:

```sh
factoryctl project add --id my-project --name "My project" --root "$PWD"

factoryctl agent add --id lead --project my-project \
  --role orchestrator --provider codex
factoryctl agent add --id worker-1 --project my-project \
  --role worker --provider codex --parent lead

factoryctl task add --id first-task --project my-project \
  --title "Make the first change" --body "Describe the result and its limits."
factoryctl task assign --project my-project --task first-task --agent worker-1

factory-tui --project my-project
```

Assignment starts delivery. You do not need a separate start command. Each
agent has a persistent session and its own worktree.

Each agent starts with a 1,000 tool-call budget. An exhausted budget stops new
delivery and needs operator action. Use `factoryctl agent budget status` to
inspect it and `factoryctl agent budget reset` to reopen it. Run either command
with `--help` for the required project and agent arguments.

The TUI has two screens:

- **BUILDING** shows all projects, agents, work, and items that need you.
- **AGENT** shows one agent's terminal, queue, inbox, and settings.

Select an agent and press `Enter` to open AGENT. Press `i` or `Enter` to type
in its terminal. Press `Ctrl-]` to return control to the board. Press `g` to
jump to the next item in NEEDS YOU. Press `?` for all keys. Press `q` to
detach. Detaching does not stop any agent.

For a quick check outside the TUI, use:

```sh
factoryctl status
factoryctl agent status --project my-project --agent worker-1
```

You can answer an agent in its terminal or send a durable message:

```sh
factoryctl agent message --project my-project --to worker-1 \
  --body "Continue with the smaller fix."
```

Pause an agent before recovery work. Use `task retry` for failed or cancelled
tasks. Use each command's `--help` option for its exact arguments.

Before starting any agent session, pin the project's repository authority
once; later configuration and retarget attempts are rejected:

```sh
factoryctl project repository set --project my-project \
  --remote https://github.com/OWNER/REPOSITORY.git --base main
```

Inside a session, `factoryctl git status|diff|commit|push` and `factoryctl pr
open|update` authenticate that exact session. They accept no caller-selected
worktree, branch, remote, force, delete, merge, or release target. Review and
merge remain independent operator actions.

## Update

```sh
factoryctl update
factoryctl update --install
factoryctl doctor
```

An update restarts the daemon. Running agent sessions continue. See the
[release and update guide](docs/development/WORKFLOW.md#release-and-update)
for rollback details. See the
[service guide](launchd/README.md#uninstall) to uninstall Dark Factory.

## Learn more

- [TUI guide](crates/factory-tui/README.md)
- [Provider setup and behavior](docs/providers.md)
- [Architecture and safety rules](ARCHITECTURE.md)
- [External integrations](https://github.com/baziyer/dark-factory/issues/100)
- [Contributing](CONTRIBUTING.md)
- [Security policy](SECURITY.md)

## Build from source

Rust 1.86 or later is required.

```sh
cargo build --release --workspace
target/release/factoryctl init
```

Contributors must follow [AGENTS.md](AGENTS.md) and run
`./scripts/local-ci.sh` before they open a pull request.
