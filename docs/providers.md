# Provider contract

Dark Factory implements `shell`, `claude_code`, and `codex` through one closed
Go boundary:

```go
func Build(Request) (Launch, error)
```

`Build` returns launch facts only. The daemon and runner own the Change working
directory, task input, PTY, process group, output, wait, and cleanup. A provider
cannot select a source path or lifecycle result, and there is no registry,
plugin, fallback, or provider-owned supervision framework.

Shell and Codex are proven end to end. The Claude Code launch path is
fixture-proven in the current source; a live run against its
signed-in CLI remains required before it is included in a release.

## Create an agent

Provider choice is explicit:

```sh
factoryctl agent create --project PROJECT_ID --name worker --provider shell --tool-budget 100
factoryctl agent create --project PROJECT_ID --name worker --provider codex --model MODEL --reasoning-effort medium --tool-budget 100
```

`--model` and `--reasoning-effort` are optional for native providers and are
rejected for `shell`. Claude Code accepts `low`, `medium`, `high`, `xhigh`, or
`max`; Codex additionally accepts `ultra`.

## Shell

Shell is fixed to `/bin/sh` with argv `/bin/sh`, `/dev/fd/11`. Its bounded task
is written to that sealed descriptor after the launch gates pass. The PTY is
then available for interactive input.

## Claude Code and Codex

The managed daemon searches this fixed default tool path, never ambient
`PATH`:

```text
~/.local/bin:/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin
```

An explicit `factoryd --tool-path` replaces the default. Search is
ordered; an existing candidate that cannot be resolved and committed fails
closed rather than falling through to another executable. A symlink is resolved
once and the direct Mach-O target is committed and reverified before exec.

The native argv templates are:

```text
claude --dangerously-skip-permissions [--model MODEL] [--effort EFFORT]
codex --dangerously-bypass-approvals-and-sandbox --no-alt-screen -c check_for_update_on_startup=false -c tool_output_token_limit=32768 -c 'projects={"CHANGE-DIRECTORY"={trust_level="untrusted"}}' [--model MODEL] [-c 'model_reasoning_effort="EFFORT"'] 'FIXED BOOTSTRAP INSTRUCTION'
```

Codex receives the daemon-authorized Change directory as an invocation-only
project override with `trust_level="untrusted"`. This suppresses Codex's
interactive directory-trust screen while explicitly refusing project-local
configuration and hooks; the directory is never persisted in Codex config and
the provider cannot choose a different working directory.

Install and sign in to the chosen CLI through its normal local workflow before
dispatching work. Both providers receive a private runtime `TMPDIR`. Claude
uses the operator's normal `HOME`, which is where its CLI keeps the signed-in
account; Codex keeps a private runtime `HOME` and receives its existing account
configuration explicitly:

```text
CODEX_HOME=<account-home>/.codex
```

No provider API key is copied into the environment. The native process runs as
the operator with unrestricted interactive authority and may use that account's
normal configuration or Keychain access. Before a Claude Code launch the Change
worker records the working directory as trusted in that account's
`.claude.json`, the record the CLI's own folder-trust dialog writes; every
Change is a path the CLI has never seen, and without the record the session
would stop at that dialog with the startup task typed into it.

Native providers do not inherit the shell task descriptor. For Claude, the
runner writes one fixed instruction plus the terminal-safe JSON-quoted task to
the PTY after provider exec and before reporting the terminal ready. The
complete prepared input must fit 8 KiB; a partial or uncertain write fails the
attempt and is never replayed.

Codex starts from a fixed, non-secret positional instruction to run
`factoryctl attempt task` first. That command authenticates with the attempt's
private credential and returns the exact effective task as terminal-safe JSON;
body wins, with title used only when a native task has no body. Codex task text
is absent from argv, environment, and Change-worker configuration, and is
bounded to 8 KiB so the configured 32,768-token tool-result budget cannot
truncate it even under worst-case control-character escaping. The attempt API
serves it only while that exact run is `running`.

Subsequent browser terminal input goes directly to the same PTY. The provider
reports its durable outcome through the attempt-scoped `factoryctl` supplied by
the daemon.

Provider changes must preserve admission-time selection, daemon-owned process
lifecycle, exact task delivery, and deterministic failure when a required
launch fact is unavailable.
