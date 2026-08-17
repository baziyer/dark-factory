#!/bin/sh
set -eu

usage() {
    echo "usage: scripts/new-worktree.sh <slug>" >&2
    echo "  creates .worktrees/<slug> on a new branch <slug>, based on main" >&2
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

if git -C "$repository_root" remote get-url origin >/dev/null 2>&1; then
    git -C "$repository_root" fetch --quiet origin main
    base="origin/main"
else
    base="main"
fi

git -C "$repository_root" worktree add -b "$branch" "$target" "$base"

cat <<EOF

Created $target on branch $branch (from $base).

Next steps:
  cd $target
  cargo build --workspace
  ./scripts/local-ci.sh
  git push -u origin $branch   # then open a PR
EOF
