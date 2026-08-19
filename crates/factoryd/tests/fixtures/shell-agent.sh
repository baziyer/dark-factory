#!/bin/sh
# Deterministic fixture agent for `providers::shell::ShellProvider`. The
# protocol implementation lives in the adjacent Python file because the
# provider boundary is bytes through a PTY, not POSIX shell lines: a prompt
# is submitted at CR and its UserPromptSubmit payload must contain the exact
# complete (possibly multiline) prompt.
set -eu
exec python3 "$(dirname "$0")/shell-agent.py"
