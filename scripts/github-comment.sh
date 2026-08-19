#!/bin/sh
# Post literal Markdown from stdin or one file to one GitHub issue or PR.
#
#   scripts/github-comment.sh issue|pr NUMBER [BODY_FILE]
#
# The target is deliberately narrow and the body is always read by gh from a
# file descriptor or file. Never put Markdown in a shell argument.
set -eu

usage() {
    echo "usage: scripts/github-comment.sh <issue|pr> <number> [body-file]" >&2
    exit 2
}

[ "$#" -ge 2 ] && [ "$#" -le 3 ] || usage
target=$1
number=$2

case "$target" in
    issue|pr) ;;
    *) usage ;;
esac
case "$number" in
    ''|*[!0-9]*) usage ;;
esac

if [ "$#" -eq 3 ]; then
    body_file=$3
    [ "$body_file" != "-" ] && [ -f "$body_file" ] && [ -r "$body_file" ] || usage
else
    body_file=-
fi

exec gh "$target" comment "$number" --body-file "$body_file"
