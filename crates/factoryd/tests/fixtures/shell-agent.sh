#!/bin/sh
# Deterministic fixture agent for `providers::shell::ShellProvider`, driven
# by `crates/factoryd/tests/sessions_e2e.rs`. Speaks the same hook/task
# protocol a real Claude Code or Codex session would, but on a fixed,
# scriptable schedule so the E2E tests never depend on a real provider CLI
# or cost tokens.
#
# Protocol, matching TRACK5C-BRIEF.md's fixture spec exactly:
#   - on start: posts SessionStart.
#   - reads lines from stdin (the daemon's PTY-typed delivery, or an
#     operator typing directly): for each line, posts UserPromptSubmit
#     {"prompt": <line>}, PreToolUse {"tool_name":"Bash"}, PostToolUse
#     {"tool_name":"Bash"}; if the line contains `task:<id>` runs
#     `factoryctl task done --task <id> --result "done: <line>"`; then posts
#     Stop and prints the reply. If the reply is
#     `{"decision":"block","reason":R}` (the Claude Stop-hook contract), R
#     becomes the next line to process, without waiting on stdin again — the
#     daemon's dispatcher uses exactly this to hand a second task/message to
#     an already-running session.
#   - a line that is exactly `exit` posts SessionEnd and exits 0.
#
# `DARK_FACTORY_FACTORYCTL` and `DARK_FACTORY_SESSION_TOKEN_FILE` are set by
# the daemon in every session's environment (see `runner_process.rs`'s
# `SESSION_ENVIRONMENT_NAMES`); this is the shell provider's only way to
# reach `factoryctl` at all, since (unlike Claude's `--settings` file or
# Codex's seeded `config.toml`) there is no generated configuration file to
# embed its trusted absolute path in.

set -eu

FACTORYCTL="${DARK_FACTORY_FACTORYCTL:-factoryctl}"
TOKEN_FILE="${DARK_FACTORY_SESSION_TOKEN_FILE:?DARK_FACTORY_SESSION_TOKEN_FILE not set}"

# post_hook EVENT PAYLOAD_JSON: forwards one hook event, printing the
# daemon's reply JSON (or `{}` on any local/daemon problem -- `factoryctl
# hook` always fails open and always exits 0).
post_hook() {
    printf '%s' "$2" | "$FACTORYCTL" hook --token-file "$TOKEN_FILE" "$1"
}

# json_prompt TEXT: {"prompt": "<escaped TEXT>"}. Escapes backslash and
# double-quote only -- sufficient for the plain-text task/message bodies
# these tests type in.
json_prompt() {
    escaped=$(printf '%s' "$1" | sed 's/\\/\\\\/g; s/"/\\"/g')
    printf '{"prompt":"%s"}' "$escaped"
}

# reason_from_reply REPLY_JSON: extracts `reason` from
# {"decision":"block","reason":"..."}, unescaping \" and \\. Anchored to the
# daemon's exact compact serialization (`decision` then `reason`, no
# whitespace) rather than a general JSON parse -- portable `sed` (this must
# run under both GNU and BSD/macOS sed, which disagree on `\|` alternation
# in basic regular expressions) has no easy general string-literal grammar.
reason_from_reply() {
    printf '%s' "$1" \
        | sed -n 's/^{"decision":"block","reason":"\(.*\)"}$/\1/p' \
        | sed 's/\\"/"/g; s/\\\\/\\/g'
}

# task_id_in LINE: the `<id>` substring of an embedded `task:<id>` marker,
# or empty if none is present.
task_id_in() {
    printf '%s' "$1" | sed -n 's/.*task:\([A-Za-z0-9_-]*\).*/\1/p'
}

# process_turn LINE: one simulated agent turn for LINE, following the
# Stop-hook block-reply chain until the daemon replies `{}`.
process_turn() {
    text="$1"
    while :; do
        post_hook UserPromptSubmit "$(json_prompt "$text")" >/dev/null
        post_hook PreToolUse '{"tool_name":"Bash"}' >/dev/null
        post_hook PostToolUse '{"tool_name":"Bash"}' >/dev/null
        task_id=$(task_id_in "$text")
        if [ -n "$task_id" ]; then
            "$FACTORYCTL" task done --task "$task_id" --result "done: $text" \
                >/dev/null 2>&1 || true
        fi
        reply=$(post_hook Stop '{}')
        printf '%s\n' "$reply"
        case "$reply" in
            *'"decision"'*)
                text=$(reason_from_reply "$reply")
                ;;
            *)
                break
                ;;
        esac
    done
}

post_hook SessionStart '{}' >/dev/null

while IFS= read -r line; do
    if [ "$line" = "exit" ]; then
        post_hook SessionEnd '{}' >/dev/null 2>&1 || true
        exit 0
    fi
    process_turn "$line"
done

post_hook SessionEnd '{}' >/dev/null 2>&1 || true
