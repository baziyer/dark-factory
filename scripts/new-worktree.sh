#!/bin/sh
set -eu

LC_ALL=C
export LC_ALL

usage() {
    echo "usage: scripts/new-worktree.sh <slug>" >&2
    echo "  creates <primary>/.worktrees/<slug> on a new branch <slug>, based on main" >&2
}

fail() {
    printf '%s\n' "$1" >&2
    exit 1
}

safe_value() {
    value=$1
    case "$value" in
    '' | *[!abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789_./:@,+\ -]*) ;;
    *)
        if [ "${#value}" -le 160 ]; then
            printf '%s' "$value"
            return
        fi
        ;;
    esac
    printf 'hex:'
    printf '%s' "$value" | od -An -v -tx1 | awk '
        {
            for (field = 1; field <= NF; field++) {
                count++
                if (count <= 80) printf "%s", $field
            }
        }
        END { if (count > 80) printf "..." }
    '
}

fail_value() {
    label=$1
    rendered=$(safe_value "$2")
    fail "$label: $rendered"
}

canonical_directory() {
    CDPATH='' cd -- "$1" 2>/dev/null && pwd -P
}

path_identity() {
    stat -f '%d:%i' "$1" 2>/dev/null || stat -c '%d:%i' "$1" 2>/dev/null
}

same_identity() {
    identity_path=$1
    expected_identity=$2
    [ ! -L "$identity_path" ] || return 1
    actual_identity=$(path_identity "$identity_path") || return 1
    [ "$actual_identity" = "$expected_identity" ]
}

reject_repository_shaping_environment() {
    for ambient_name in \
        GIT_DIR GIT_WORK_TREE GIT_COMMON_DIR GIT_NAMESPACE \
        GIT_OBJECT_DIRECTORY GIT_ALTERNATE_OBJECT_DIRECTORIES GIT_INDEX_FILE \
        GIT_CONFIG GIT_CONFIG_GLOBAL GIT_CONFIG_SYSTEM GIT_CONFIG_NOSYSTEM \
        GIT_CONFIG_PARAMETERS; do
        if printenv "$ambient_name" >/dev/null 2>&1; then
            fail "refusing ambient Git repository shaping: $ambient_name"
        fi
    done

    if [ "${GIT_CONFIG_COUNT+x}" = x ]; then
        [ "$GIT_CONFIG_COUNT" = 2 ] ||
            fail "refusing ambient Git config shaping: GIT_CONFIG_COUNT"
        [ "${GIT_CONFIG_KEY_0+x}" = x ] &&
            [ "$GIT_CONFIG_KEY_0" = credential.helper ] &&
            [ "${GIT_CONFIG_VALUE_0+x}" = x ] &&
            [ -z "$GIT_CONFIG_VALUE_0" ] ||
            fail "refusing ambient Git config shaping: GIT_CONFIG_COUNT"
        [ "${GIT_CONFIG_KEY_1+x}" = x ] &&
            [ "$GIT_CONFIG_KEY_1" = core.sshCommand ] &&
            [ "${GIT_CONFIG_VALUE_1+x}" = x ] &&
            [ "$GIT_CONFIG_VALUE_1" = /usr/bin/false ] ||
            fail "refusing ambient Git config shaping: GIT_CONFIG_COUNT"
    else
        [ "${GIT_CONFIG_KEY_0+x}" != x ] &&
            [ "${GIT_CONFIG_VALUE_0+x}" != x ] &&
            [ "${GIT_CONFIG_KEY_1+x}" != x ] &&
            [ "${GIT_CONFIG_VALUE_1+x}" != x ] ||
            fail "refusing ambient Git config shaping without GIT_CONFIG_COUNT"
    fi
    [ "${GIT_CONFIG_KEY_2+x}" != x ] &&
        [ "${GIT_CONFIG_VALUE_2+x}" != x ] ||
        fail "refusing extra ambient Git config shaping"

    GIT_TERMINAL_PROMPT=0
    GIT_ASKPASS=/usr/bin/false
    GIT_SSH_COMMAND=/usr/bin/false
    GIT_CONFIG_COUNT=2
    GIT_CONFIG_KEY_0=credential.helper
    GIT_CONFIG_VALUE_0=
    GIT_CONFIG_KEY_1=core.sshCommand
    GIT_CONFIG_VALUE_1=/usr/bin/false
    export GIT_TERMINAL_PROMPT GIT_ASKPASS GIT_SSH_COMMAND
    export GIT_CONFIG_COUNT GIT_CONFIG_KEY_0 GIT_CONFIG_VALUE_0
    export GIT_CONFIG_KEY_1 GIT_CONFIG_VALUE_1
}

show_ref_state() {
    state_common=$1
    state_ref=$2
    if git --git-dir="$state_common" show-ref --verify --quiet "$state_ref" \
        >/dev/null 2>&1; then
        return 0
    else
        state_status=$?
    fi
    [ "$state_status" -eq 1 ] && return 1
    return 2
}

resolve_repository_identity() {
    resolved_root=$(canonical_directory "$repository_root") ||
        fail "cannot resolve the script repository"
    [ -e "$resolved_root/.git" ] && [ ! -L "$resolved_root/.git" ] ||
        fail "script repository Git marker is missing or symlinked"

    resolved_bare=$(git -C "$resolved_root" rev-parse --is-bare-repository 2>/dev/null) ||
        fail "script is not inside a Git worktree"
    [ "$resolved_bare" = false ] || fail "bare repositories do not have a worktree anchor"

    resolved_top_raw=$(git -C "$resolved_root" rev-parse --show-toplevel 2>/dev/null) ||
        fail "cannot resolve the script Git worktree"
    resolved_top=$(canonical_directory "$resolved_top_raw") ||
        fail "cannot resolve the script Git worktree"
    [ "$resolved_top" = "$resolved_root" ] ||
        fail "script repository does not match its registered Git worktree"

    resolved_common_raw=$(
        git -C "$resolved_root" rev-parse --path-format=absolute --git-common-dir 2>/dev/null
    ) || fail "cannot resolve the Git common directory"
    [ -d "$resolved_common_raw" ] && [ ! -L "$resolved_common_raw" ] ||
        fail "Git common directory is missing or symlinked"
    resolved_common=$(canonical_directory "$resolved_common_raw") ||
        fail "cannot resolve the Git common directory"

    resolved_git_dir_raw=$(
        git -C "$resolved_root" rev-parse --path-format=absolute --git-dir 2>/dev/null
    ) || fail "cannot resolve the script worktree registration"
    [ -d "$resolved_git_dir_raw" ] && [ ! -L "$resolved_git_dir_raw" ] ||
        fail "script worktree registration is missing or symlinked"
    resolved_git_dir=$(canonical_directory "$resolved_git_dir_raw") ||
        fail "cannot resolve the script worktree registration"

    case "$resolved_common" in
    */.git) resolved_anchor_raw=${resolved_common%/.git} ;;
    *) fail "Git common directory has no trustworthy primary worktree identity" ;;
    esac
    resolved_anchor=$(canonical_directory "$resolved_anchor_raw") ||
        fail "cannot resolve the primary worktree anchor"
    [ -d "$resolved_anchor" ] && [ ! -L "$resolved_anchor" ] ||
        fail "primary worktree anchor is missing or symlinked"
    [ -d "$resolved_anchor/.git" ] && [ ! -L "$resolved_anchor/.git" ] ||
        fail "primary worktree Git directory is missing or symlinked"
    anchor_git_dir=$(canonical_directory "$resolved_anchor/.git") ||
        fail "cannot resolve the primary worktree Git directory"
    [ "$anchor_git_dir" = "$resolved_common" ] ||
        fail "Git common directory does not match the primary worktree"

    anchor_top_raw=$(git -C "$resolved_anchor" rev-parse --show-toplevel 2>/dev/null) ||
        fail "cannot validate the primary worktree anchor"
    anchor_top=$(canonical_directory "$anchor_top_raw") ||
        fail "cannot validate the primary worktree anchor"
    anchor_common_raw=$(
        git -C "$resolved_anchor" rev-parse --path-format=absolute --git-common-dir 2>/dev/null
    ) || fail "cannot validate the primary Git common directory"
    anchor_common=$(canonical_directory "$anchor_common_raw") ||
        fail "cannot validate the primary Git common directory"
    anchor_git_dir_raw=$(
        git -C "$resolved_anchor" rev-parse --path-format=absolute --git-dir 2>/dev/null
    ) || fail "cannot validate the primary Git directory"
    anchor_git_dir=$(canonical_directory "$anchor_git_dir_raw") ||
        fail "cannot validate the primary Git directory"
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
        [ "$registered_git_file" = "$resolved_root/.git" ] ||
            fail "linked worktree registration points to a different worktree"
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

save_repository_identity() {
    initial_root=$resolved_root
    initial_common=$resolved_common
    initial_git_dir=$resolved_git_dir
    initial_anchor=$resolved_anchor
    initial_root_identity=$(path_identity "$initial_root") ||
        fail "cannot identify the script worktree"
    initial_common_identity=$(path_identity "$initial_common") ||
        fail "cannot identify the Git common directory"
    initial_git_dir_identity=$(path_identity "$initial_git_dir") ||
        fail "cannot identify the script worktree registration"
    initial_anchor_identity=$(path_identity "$initial_anchor") ||
        fail "cannot identify the primary worktree"
    initial_root_marker_identity=$(path_identity "$initial_root/.git") ||
        fail "cannot identify the script worktree Git marker"
}

repository_identity_matches() {
    same_identity "$initial_root" "$initial_root_identity" &&
        same_identity "$initial_common" "$initial_common_identity" &&
        same_identity "$initial_git_dir" "$initial_git_dir_identity" &&
        same_identity "$initial_anchor" "$initial_anchor_identity" &&
        same_identity "$initial_root/.git" "$initial_root_marker_identity"
}

revalidate_repository_identity() {
    resolve_repository_identity
    if [ "$resolved_root" = "$initial_root" ] &&
        [ "$resolved_common" = "$initial_common" ] &&
        [ "$resolved_git_dir" = "$initial_git_dir" ] &&
        [ "$resolved_anchor" = "$initial_anchor" ] &&
        repository_identity_matches; then
        return
    fi
    fail "repository identity changed during worktree creation"
}

validate_destination_absent() {
    if [ -L "$worktrees_directory" ]; then
        fail_value "worktree parent is symlinked" "$worktrees_directory"
    fi
    if [ -e "$worktrees_directory" ] && [ ! -d "$worktrees_directory" ]; then
        fail_value "worktree parent is not a directory" "$worktrees_directory"
    fi
    if [ -e "$target" ] || [ -L "$target" ]; then
        fail_value "worktree path already exists" "$target"
    fi
}

created_worktree_matches() {
    repository_identity_matches || return 1
    [ -d "$worktrees_directory" ] && [ ! -L "$worktrees_directory" ] || return 1
    if [ "$parent_initially_present" = true ]; then
        same_identity "$worktrees_directory" "$initial_parent_identity" || return 1
    fi
    [ -d "$target" ] && [ ! -L "$target" ] || return 1
    created_target=$(canonical_directory "$target") || return 1
    [ "$created_target" = "$target" ] || return 1
    created_target_identity=$(path_identity "$target") || return 1
    [ -f "$target/.git" ] && [ ! -L "$target/.git" ] || return 1
    created_marker_identity=$(path_identity "$target/.git") || return 1

    created_top_raw=$(git -C "$target" rev-parse --show-toplevel 2>/dev/null) || return 1
    created_top=$(canonical_directory "$created_top_raw") || return 1
    created_common_raw=$(
        git -C "$target" rev-parse --path-format=absolute --git-common-dir 2>/dev/null
    ) || return 1
    created_common=$(canonical_directory "$created_common_raw") || return 1
    created_git_raw=$(
        git -C "$target" rev-parse --path-format=absolute --git-dir 2>/dev/null
    ) || return 1
    [ -d "$created_git_raw" ] && [ ! -L "$created_git_raw" ] || return 1
    created_git=$(canonical_directory "$created_git_raw") || return 1
    case "$created_git" in
    "$initial_common"/worktrees/*) ;;
    *) return 1 ;;
    esac
    created_git_dir_identity=$(path_identity "$created_git") || return 1
    created_branch=$(git -C "$target" symbolic-ref -q HEAD 2>/dev/null) || return 1
    created_commit=$(git -C "$target" rev-parse --verify HEAD 2>/dev/null) || return 1
    [ "$created_top" = "$target" ] &&
        [ "$created_common" = "$initial_common" ] &&
        [ "$created_branch" = "$branch_ref" ] &&
        [ "$created_commit" = "$base_commit" ] || return 1

    same_identity "$target" "$created_target_identity" &&
        same_identity "$target/.git" "$created_marker_identity" &&
        same_identity "$created_git" "$created_git_dir_identity" &&
        repository_identity_matches
}

report_orphan() {
    orphan_reason=$1
    rendered_target=$(safe_value "$target")
    rendered_branch=$(safe_value "$branch")
    rendered_base=$(safe_value "$base_ref")
    printf '%s; preserved orphan for daemon-owned recovery: target=%s branch=%s base=%s\n' \
        "$orphan_reason" "$rendered_target" "$rendered_branch" "$rendered_base" >&2
}

on_mutation_signal() {
    mutation_signal=$1
    trap - HUP INT TERM
    report_orphan "interrupted by $mutation_signal after native worktree creation began"
    exit 1
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
'' | */* | .*) fail_value "invalid slug" "$slug" ;;
esac

reject_repository_shaping_environment
git check-ref-format --branch "$slug" >/dev/null 2>&1 || fail_value "invalid slug" "$slug"

repository_root=$(canonical_directory "$(dirname -- "$0")/..") ||
    fail "cannot resolve the script repository"
resolve_repository_identity
save_repository_identity

worktrees_directory="$initial_anchor/.worktrees"
target="$worktrees_directory/$slug"
branch=$slug
branch_ref="refs/heads/$branch"
validate_destination_absent

parent_initially_present=false
if [ -d "$worktrees_directory" ]; then
    parent_initially_present=true
    initial_parent_identity=$(path_identity "$worktrees_directory") ||
        fail "cannot identify the worktree parent"
fi

if show_ref_state "$initial_common" "$branch_ref"; then
    fail_value "branch already exists" "$branch"
else
    ref_status=$?
    [ "$ref_status" -eq 1 ] || fail "cannot determine whether the branch already exists"
fi

if origin_url=$(git --git-dir="$initial_common" config --get remote.origin.url 2>/dev/null); then
    [ -n "$origin_url" ] || fail "origin remote has an empty URL"
    git --git-dir="$initial_common" fetch --quiet origin main >/dev/null 2>&1 ||
        fail "fetch of origin/main failed"
    base_ref=refs/remotes/origin/main
else
    origin_status=$?
    [ "$origin_status" -eq 1 ] || fail "cannot inspect the origin remote"
    base_ref=refs/heads/main
fi
base_commit=$(git --git-dir="$initial_common" rev-parse --verify "$base_ref^{commit}" 2>/dev/null) ||
    fail "main does not resolve to a commit"

revalidate_repository_identity
if [ "$parent_initially_present" = true ]; then
    same_identity "$worktrees_directory" "$initial_parent_identity" ||
        fail "worktree parent identity changed during validation"
else
    [ ! -e "$worktrees_directory" ] && [ ! -L "$worktrees_directory" ] ||
        fail "worktree parent appeared during validation"
fi
validate_destination_absent
if show_ref_state "$initial_common" "$branch_ref"; then
    fail_value "branch appeared during validation" "$branch"
else
    ref_status=$?
    [ "$ref_status" -eq 1 ] || fail "cannot recheck the requested branch"
fi
validated_base=$(
    git --git-dir="$initial_common" rev-parse --verify "$base_ref^{commit}" 2>/dev/null
) || fail "cannot recheck the base commit"
[ "$validated_base" = "$base_commit" ] || fail "base commit changed during validation"

trap 'on_mutation_signal HUP' HUP
trap 'on_mutation_signal INT' INT
trap 'on_mutation_signal TERM' TERM
if git --git-dir="$initial_common" worktree add -b "$branch" "$target" "$base_commit" \
    >/dev/null 2>&1; then
    add_status=0
else
    add_status=$?
fi
if [ "$add_status" -ne 0 ]; then
    trap - HUP INT TERM
    report_orphan "native git worktree add failed with status $add_status"
    exit 1
fi
if ! created_worktree_matches; then
    trap - HUP INT TERM
    report_orphan "native git worktree add failed its identity postcondition"
    exit 1
fi

rendered_target=$(safe_value "$target")
rendered_branch=$(safe_value "$branch")
rendered_base=$(safe_value "$base_ref")
cat <<EOF

Created $rendered_target on branch $rendered_branch (from $rendered_base).

Next steps:
  cd $rendered_target
  cargo build --workspace
  ./scripts/local-ci.sh
  git push -u origin $rendered_branch   # then open a PR
EOF
