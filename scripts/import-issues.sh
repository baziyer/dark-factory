#!/bin/sh
# Import a triage document into GitHub issues, one issue per `### ` heading.
#
#   scripts/import-issues.sh docs/KNOWN-ISSUES.md [--dry-run]
#
# The body of each issue is the heading's markdown verbatim (up to the next
# `###`/`##`), plus a footer naming the source file and commit. Labels:
# `known-issue`, `size:S|M|L` from a `**Size**: X` line when present
# (`decision` when that line says so instead), and an area label from the
# enclosing `## ` section (see area_label below). An
# issue whose exact title already exists (open or closed) is skipped, so
# re-running is safe.
set -eu

file="${1:-}"
dry_run="${2:-}"
if [ -z "$file" ] || [ ! -f "$file" ]; then
    echo "usage: scripts/import-issues.sh <markdown-file> [--dry-run]" >&2
    exit 1
fi

repository=$(gh repo view --json nameWithOwner --jq .nameWithOwner)
commit=$(git rev-parse --short HEAD)
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

# Split the document into one file per `### ` entry: 000.md, 001.md, ...
# Line 1 of each: the enclosing `## ` section; line 2: the title; rest: body.
awk -v dir="$work" '
    /^## /  { section = substr($0, 4); next }
    /^### / {
        if (out) close(out)
        out = sprintf("%s/%03d.md", dir, n++)
        print section > out
        print substr($0, 5) > out
        next
    }
    out { print > out }
' "$file"

area_label() {
    case "$1" in
        "Sessions and hooks"|"Startup and webhooks") echo "area:daemon" ;;
        "TUI") echo "area:tui" ;;
        "Security and state files") echo "area:providers" ;;
        "Toolchain") echo "area:ci" ;;
        "Stale documentation"*) echo "area:docs" ;;
        *) echo "" ;;
    esac
}

created=0 skipped=0
for entry in "$work"/*.md; do
    [ -f "$entry" ] || { echo "no '### ' entries found in $file" >&2; exit 1; }
    section=$(sed -n 1p "$entry")
    title=$(sed -n 2p "$entry")
    body=$(sed '1,2d' "$entry" | sed -e :a -e '/^\n*$/{$d;N;ba' -e '}')
    labels="known-issue"
    area=$(area_label "$section")
    [ -n "$area" ] && labels="$labels,$area"
    size=$(printf '%s\n' "$body" | sed -n 's/^\*\*Size\*\*: *\([SML]\).*/\1/p' | head -1)
    [ -n "$size" ] && labels="$labels,size:$size"
    if printf '%s\n' "$body" | grep -q '^\*\*Size\*\*:.*decision'; then
        labels="$labels,decision"
    fi

    if gh issue list --state all --limit 200 --search "in:title \"$title\"" \
        --json title --jq '.[].title' | grep -Fxq "$title"; then
        printf 'skip (exists): %s\n' "$title"
        skipped=$((skipped + 1))
        continue
    fi
    printf '%s\n\n---\n_Imported from `%s` at %s (`%s`) by scripts/import-issues.sh._\n' \
        "$body" "$file" "$repository" "$commit" > "$entry.body"
    if [ "$dry_run" = "--dry-run" ]; then
        printf 'would create [%s]: %s\n' "$labels" "$title"
    else
        gh issue create --title "$title" --label "$labels" --body-file "$entry.body" >/dev/null
        printf 'created [%s]: %s\n' "$labels" "$title"
    fi
    created=$((created + 1))
done
echo "done: $created created, $skipped skipped"
