#!/bin/sh
set -eu

usage() {
    echo "usage: scripts/new-worktree.sh <slug>" >&2
    echo "  creates .worktrees/<slug> on a new branch <slug>, without contacting a remote" >&2
}

slug="${1:-}"
if [ "$slug" = "-h" ] || [ "$slug" = "--help" ]; then
    usage
    exit 0
fi
if [ -z "$slug" ]; then
    usage
    exit 1
fi
case "$slug" in
    */*|.*)
        echo "invalid slug: $slug (no slashes, no leading dot)" >&2
        exit 1
        ;;
esac

repository_root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
target="$repository_root/.worktrees/$slug"
branch="$slug"

if [ -e "$target" ]; then
    echo "worktree path already exists: $target" >&2
    exit 1
fi
if git -C "$repository_root" show-ref --verify --quiet "refs/heads/$branch"; then
    echo "branch already exists: $branch" >&2
    exit 1
fi

if git -C "$repository_root" show-ref --verify --quiet refs/remotes/origin/main; then
    base="origin/main"
else
    base="main"
fi

empty_hooks=$(mktemp -d "${TMPDIR:-/tmp}/dark-factory-empty-hooks.XXXXXX")
trap 'rmdir "$empty_hooks" 2>/dev/null || true' EXIT HUP INT TERM
chmod 700 "$empty_hooks"
git -C "$repository_root" -c core.hooksPath="$empty_hooks" \
    worktree add -b "$branch" "$target" "$base"
rmdir "$empty_hooks"
trap - EXIT HUP INT TERM

cat <<EOF

Created $target on branch $branch (from $base).

Next steps:
  cd $target
  cargo build --workspace
  ./scripts/local-ci.sh
  Publish $branch and open a PR through an authorized host credential broker
  or App-backed tool surface.
EOF
