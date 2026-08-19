#!/bin/sh
# Post literal Markdown from stdin to one GitHub issue or PR.
#
#   scripts/github-comment.sh issue|pr NUMBER
#
# The target is deliberately narrow and the body is always read by gh from
# stdin. Never put Markdown in a shell argument or open a caller-supplied path.
set -eu

maximum_output_bytes=4096

usage() {
    echo "usage: scripts/github-comment.sh <issue|pr> <number>" >&2
    exit 2
}

[ "$#" -eq 2 ] || usage
target=$1
number=$2

case "$target" in
    issue|pr) ;;
    *) usage ;;
esac
case "$number" in
    ''|*[!0-9]*) usage ;;
esac

temporary=$(mktemp -d "${TMPDIR:-/tmp}/dark-factory-comment.XXXXXX")
trap 'rm -rf "$temporary"' EXIT HUP INT TERM
stdout_pipe=$temporary/stdout
stderr_pipe=$temporary/stderr
stdout_capture=$temporary/stdout.capture
stderr_capture=$temporary/stderr.capture
mkfifo "$stdout_pipe" "$stderr_pipe"

bounded_drain() {
    fifo=$1
    capture=$2
    {
        head -c "$maximum_output_bytes"
        cat >/dev/null
    } <"$fifo" >"$capture"
}

bounded_drain "$stdout_pipe" "$stdout_capture" &
stdout_drain=$!
bounded_drain "$stderr_pipe" "$stderr_capture" &
stderr_drain=$!

status=0
gh "$target" comment "$number" --body-file - >"$stdout_pipe" 2>"$stderr_pipe" || status=$?
wait "$stdout_drain"
wait "$stderr_drain"
cat "$stdout_capture"
cat "$stderr_capture" >&2
exit "$status"
