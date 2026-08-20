#!/bin/sh
set -eu

usage() {
    echo "usage: scripts/new-worktree.sh <slug>" >&2
    echo "  creates <primary>/.worktrees/<slug> on a new branch <slug>, based on main" >&2
}

fail() {
    echo "$*" >&2
    exit 1
}

canonical_directory() {
    CDPATH='' cd -- "$1" 2>/dev/null && pwd -P
}

resolve_repository_identity() {
    resolved_root=$(canonical_directory "$repository_root") ||
        fail "cannot resolve repository root: $repository_root"

    resolved_bare=$(git -C "$resolved_root" rev-parse --is-bare-repository 2>/dev/null) ||
        fail "not a Git worktree: $resolved_root"
    [ "$resolved_bare" = false ] || fail "bare repositories do not have a worktree anchor"

    resolved_top_raw=$(git -C "$resolved_root" rev-parse --show-toplevel 2>/dev/null) ||
        fail "cannot resolve Git worktree root: $resolved_root"
    resolved_top=$(canonical_directory "$resolved_top_raw") ||
        fail "cannot resolve Git worktree root: $resolved_top_raw"
    [ "$resolved_top" = "$resolved_root" ] ||
        fail "script repository does not match the registered Git worktree"

    resolved_common_raw=$(
        git -C "$resolved_root" rev-parse --path-format=absolute --git-common-dir 2>/dev/null
    ) || fail "cannot resolve Git common directory: $resolved_root"
    resolved_common=$(canonical_directory "$resolved_common_raw") ||
        fail "cannot resolve Git common directory: $resolved_common_raw"

    resolved_git_dir_raw=$(
        git -C "$resolved_root" rev-parse --path-format=absolute --git-dir 2>/dev/null
    ) || fail "cannot resolve Git worktree identity: $resolved_root"
    resolved_git_dir=$(canonical_directory "$resolved_git_dir_raw") ||
        fail "cannot resolve Git worktree identity: $resolved_git_dir_raw"

    case "$resolved_common" in
        */.git) resolved_anchor_raw=${resolved_common%/.git} ;;
        *) fail "Git common directory does not identify one primary worktree anchor" ;;
    esac
    resolved_anchor=$(canonical_directory "$resolved_anchor_raw") ||
        fail "cannot resolve primary worktree anchor: $resolved_anchor_raw"
    [ -d "$resolved_anchor/.git" ] && [ ! -L "$resolved_anchor/.git" ] ||
        fail "primary worktree Git directory is missing or symlinked"
    anchor_git_dir=$(canonical_directory "$resolved_anchor/.git") ||
        fail "cannot resolve primary worktree Git directory"
    [ "$anchor_git_dir" = "$resolved_common" ] ||
        fail "Git common directory does not match the primary worktree"

    anchor_bare=$(git -C "$resolved_anchor" rev-parse --is-bare-repository 2>/dev/null) ||
        fail "cannot validate primary worktree anchor"
    [ "$anchor_bare" = false ] || fail "primary worktree anchor is bare"
    anchor_top_raw=$(git -C "$resolved_anchor" rev-parse --show-toplevel 2>/dev/null) ||
        fail "cannot validate primary worktree root"
    anchor_top=$(canonical_directory "$anchor_top_raw") ||
        fail "cannot resolve primary worktree root: $anchor_top_raw"
    anchor_common_raw=$(
        git -C "$resolved_anchor" rev-parse --path-format=absolute --git-common-dir 2>/dev/null
    ) || fail "cannot validate primary worktree common directory"
    anchor_common=$(canonical_directory "$anchor_common_raw") ||
        fail "cannot resolve primary worktree common directory: $anchor_common_raw"
    anchor_git_dir_raw=$(
        git -C "$resolved_anchor" rev-parse --path-format=absolute --git-dir 2>/dev/null
    ) || fail "cannot validate primary worktree Git directory"
    anchor_git_dir=$(canonical_directory "$anchor_git_dir_raw") ||
        fail "cannot resolve primary worktree Git directory: $anchor_git_dir_raw"
    [ "$anchor_top" = "$resolved_anchor" ] &&
        [ "$anchor_common" = "$resolved_common" ] &&
        [ "$anchor_git_dir" = "$resolved_common" ] ||
        fail "primary worktree identity is ambiguous or mismatched"

    if [ "$resolved_git_dir" = "$resolved_common" ]; then
        [ "$resolved_root" = "$resolved_anchor" ] ||
            fail "primary Git directory is attached to a different worktree"
    else
        case "$resolved_git_dir" in
            "$resolved_common"/worktrees/*) ;;
            *) fail "linked worktree metadata is outside the Git common directory" ;;
        esac
        worktree_admin_name=${resolved_git_dir#"$resolved_common"/worktrees/}
        case "$worktree_admin_name" in
            '' | */*) fail "linked worktree identity is ambiguous" ;;
        esac
        [ -f "$resolved_root/.git" ] && [ ! -L "$resolved_root/.git" ] ||
            fail "linked worktree Git file is missing or symlinked"
        [ -f "$resolved_git_dir/gitdir" ] && [ ! -L "$resolved_git_dir/gitdir" ] ||
            fail "linked worktree registration is missing or symlinked"

        registered_git_file=$(sed -n '1p' "$resolved_git_dir/gitdir")
        case "$registered_git_file" in
            */.git) registered_root_raw=${registered_git_file%/.git} ;;
            *) fail "linked worktree registration is invalid" ;;
        esac
        registered_root=$(canonical_directory "$registered_root_raw") ||
            fail "linked worktree registration target is missing"
        [ "$registered_root" = "$resolved_root" ] ||
            fail "linked worktree registration points to a different repository"

        linked_git_dir=$(sed -n 's/^gitdir: //p' "$resolved_root/.git")
        [ -n "$linked_git_dir" ] || fail "linked worktree Git file is invalid"
        case "$linked_git_dir" in
            /*) ;;
            *) linked_git_dir="$resolved_root/$linked_git_dir" ;;
        esac
        linked_git_dir=$(canonical_directory "$linked_git_dir") ||
            fail "linked worktree Git directory is missing"
        [ "$linked_git_dir" = "$resolved_git_dir" ] ||
            fail "linked worktree Git file does not match its registration"
    fi
}

validate_destination() {
    if [ -L "$worktrees_directory" ]; then
        fail "worktree parent is symlinked: $worktrees_directory"
    fi
    if [ -e "$worktrees_directory" ] && [ ! -d "$worktrees_directory" ]; then
        fail "worktree parent is not a directory: $worktrees_directory"
    fi
    if [ -e "$target" ] || [ -L "$target" ]; then
        fail "worktree path already exists: $target"
    fi
}

[ "$#" -eq 1 ] || {
    usage
    exit 2
}
slug=$1

case "$slug" in
    -h | --help)
        usage
        exit 0
        ;;
    '' | */* | .*) fail "invalid slug: $slug" ;;
esac
git check-ref-format --branch "$slug" >/dev/null 2>&1 ||
    fail "invalid slug: $slug"

repository_root=$(canonical_directory "$(dirname -- "$0")/..") ||
    fail "cannot resolve the script repository"
resolve_repository_identity
initial_root=$resolved_root
initial_common=$resolved_common
initial_git_dir=$resolved_git_dir
initial_anchor=$resolved_anchor

worktrees_directory="$resolved_anchor/.worktrees"
target="$worktrees_directory/$slug"
branch=$slug
validate_destination
if git -C "$resolved_anchor" show-ref --verify --quiet "refs/heads/$branch"; then
    fail "branch already exists: $branch"
fi

track_origin=false
if git -C "$resolved_anchor" remote get-url origin >/dev/null 2>&1; then
    git -C "$resolved_anchor" fetch --quiet origin main
    base_ref=refs/remotes/origin/main
    track_origin=true
else
    base_ref=refs/heads/main
fi
base_commit=$(git -C "$resolved_anchor" rev-parse --verify "$base_ref^{commit}") ||
    fail "main does not resolve to a commit"

# Fetch and validation run external processes. Re-resolve every identity before
# the first filesystem or ref mutation so a replaced repository fails closed.
resolve_repository_identity
[ "$resolved_root" = "$initial_root" ] &&
    [ "$resolved_common" = "$initial_common" ] &&
    [ "$resolved_git_dir" = "$initial_git_dir" ] &&
    [ "$resolved_anchor" = "$initial_anchor" ] ||
    fail "repository identity changed during validation"
worktrees_directory="$resolved_anchor/.worktrees"
target="$worktrees_directory/$slug"
validate_destination
if git -C "$resolved_anchor" show-ref --verify --quiet "refs/heads/$branch"; then
    fail "branch already exists: $branch"
fi

cleanup_required=true
created_parent=false
owned_target=false
owned_branch=false

cleanup() {
    cleanup_status=$?
    trap - 0 HUP INT TERM
    if [ "$cleanup_required" = true ]; then
        if [ "$owned_target" = true ]; then
            git -C "$resolved_anchor" worktree remove --force "$target" \
                >/dev/null 2>&1 || true
            rm -rf -- "$target"
        fi
        if [ "$owned_branch" = true ]; then
            git -C "$resolved_anchor" update-ref -d "refs/heads/$branch" "$base_commit" \
                >/dev/null 2>&1 || true
            git -C "$resolved_anchor" config --remove-section "branch.$branch" \
                >/dev/null 2>&1 || true
        fi
        if [ "$created_parent" = true ]; then
            rmdir "$worktrees_directory" >/dev/null 2>&1 || true
        fi
    fi
    exit "$cleanup_status"
}

trap cleanup 0
trap 'exit 1' HUP INT TERM

if [ ! -d "$worktrees_directory" ]; then
    mkdir "$worktrees_directory"
    created_parent=true
fi
mkdir "$target"
owned_target=true
git -C "$resolved_anchor" update-ref "refs/heads/$branch" "$base_commit" ''
owned_branch=true
git -C "$resolved_anchor" worktree add "$target" "$branch"
if [ "$track_origin" = true ]; then
    git -C "$target" branch --set-upstream-to=origin/main "$branch" >/dev/null
fi
cleanup_required=false

cat <<EOF

Created $target on branch $branch (from $base_ref).

Next steps:
  cd $target
  cargo build --workspace
  ./scripts/local-ci.sh
  git push -u origin $branch   # then open a PR
EOF
