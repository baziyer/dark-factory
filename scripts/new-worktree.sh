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
    for identity_style in -f -c; do
        identity_result=$(stat "$identity_style" '%d:%i' "$1" 2>/dev/null) || continue
        case "$identity_result" in
            '' | *[!0123456789:]* | *:*:*) continue ;;
            [0123456789]*:[0123456789]*) ;;
            *) continue ;;
        esac
        printf '%s\n' "$identity_result"
        return
    done
    return 1
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
        config_shape="${GIT_CONFIG_COUNT-}:${GIT_CONFIG_KEY_0-}:${GIT_CONFIG_VALUE_0-}:\
${GIT_CONFIG_KEY_1-}:${GIT_CONFIG_VALUE_1-}"
        [ "$config_shape" = '2:credential.helper::core.sshCommand:/usr/bin/false' ] ||
            fail "refusing ambient Git config shaping: GIT_CONFIG_COUNT"
    else
        for ambient_name in \
            GIT_CONFIG_KEY_0 GIT_CONFIG_VALUE_0 GIT_CONFIG_KEY_1 GIT_CONFIG_VALUE_1; do
            printenv "$ambient_name" >/dev/null 2>&1 &&
                fail "refusing ambient Git config shaping without GIT_CONFIG_COUNT"
        done
    fi
    for ambient_name in GIT_CONFIG_KEY_2 GIT_CONFIG_VALUE_2; do
        printenv "$ambient_name" >/dev/null 2>&1 &&
            fail "refusing extra ambient Git config shaping"
    done

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

require_branch_absent() {
    if git --git-dir="$initial_common" show-ref --verify --quiet "$branch_ref" \
        >/dev/null 2>&1; then
        fail_value "$1" "$branch"
    else
        ref_status=$?
    fi
    [ "$ref_status" -eq 1 ] || fail "$2"
}

snapshot_repository_identity() {
    initial_root=$(canonical_directory "$(dirname -- "$0")/..") ||
        fail "cannot resolve the script repository"
    [ -e "$initial_root/.git" ] && [ ! -L "$initial_root/.git" ] ||
        fail "script repository Git marker is missing or symlinked"
    initial_root_identity=$(path_identity "$initial_root") ||
        fail "cannot identify the script worktree"
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
    [ -f "$target/.git" ] && [ ! -L "$target/.git" ] || return 1

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
    created_branch=$(git -C "$target" symbolic-ref -q HEAD 2>/dev/null) || return 1
    created_commit=$(git -C "$target" rev-parse --verify HEAD 2>/dev/null) || return 1
    [ "$created_top" = "$target" ] &&
        [ "$created_common" = "$initial_common" ] &&
        [ "$created_branch" = "$branch_ref" ] &&
        [ "$created_commit" = "$base_commit" ] &&
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

snapshot_repository_identity
reject_repository_shaping_environment
git check-ref-format --branch "$slug" >/dev/null 2>&1 || fail_value "invalid slug" "$slug"

initial_top=$(canonical_directory "$(
    git -C "$initial_root" rev-parse --show-toplevel 2>/dev/null
)") ||
    fail "cannot resolve the script Git worktree"
[ "$initial_top" = "$initial_root" ] ||
    fail "script repository does not match its registered Git worktree"

initial_common_raw=$(
    git -C "$initial_root" rev-parse --path-format=absolute --git-common-dir 2>/dev/null
) || fail "cannot resolve the Git common directory"
[ -d "$initial_common_raw" ] && [ ! -L "$initial_common_raw" ] ||
    fail "Git common directory is missing or symlinked"
initial_common=$(canonical_directory "$initial_common_raw") ||
    fail "cannot resolve the Git common directory"

initial_git_dir_raw=$(
    git -C "$initial_root" rev-parse --path-format=absolute --git-dir 2>/dev/null
) || fail "cannot resolve the script worktree registration"
[ -d "$initial_git_dir_raw" ] && [ ! -L "$initial_git_dir_raw" ] ||
    fail "script worktree registration is missing or symlinked"
initial_git_dir=$(canonical_directory "$initial_git_dir_raw") ||
    fail "cannot resolve the script worktree registration"

case "$initial_common" in
    */.git) initial_anchor=$(canonical_directory "${initial_common%/.git}") ||
        fail "cannot resolve the primary worktree anchor" ;;
    *) fail "Git common directory has no trustworthy primary worktree identity" ;;
esac
[ -d "$initial_anchor/.git" ] && [ ! -L "$initial_anchor/.git" ] ||
    fail "primary worktree Git directory is missing or symlinked"
[ "$(canonical_directory "$initial_anchor/.git")" = "$initial_common" ] ||
    fail "cannot resolve the primary worktree Git directory"
anchor_top=$(canonical_directory "$(
    git -C "$initial_anchor" rev-parse --show-toplevel 2>/dev/null
)") ||
    fail "cannot validate the primary worktree anchor"
[ "$anchor_top" = "$initial_anchor" ] ||
    fail "primary worktree identity is ambiguous or mismatched"

if [ "$initial_git_dir" = "$initial_common" ]; then
    [ "$initial_root" = "$initial_anchor" ] ||
        fail "primary Git directory is attached to a different worktree"
else
    case "$initial_git_dir" in
        "$initial_common"/worktrees/*) ;;
        *) fail "linked worktree metadata is outside the Git common directory" ;;
    esac
    worktree_admin_name=${initial_git_dir#"$initial_common"/worktrees/}
    case "$worktree_admin_name" in
        '' | */*) fail "linked worktree identity is ambiguous" ;;
    esac
    [ -f "$initial_root/.git" ] && [ ! -L "$initial_root/.git" ] ||
        fail "linked worktree Git file is missing or symlinked"
    [ -f "$initial_git_dir/gitdir" ] && [ ! -L "$initial_git_dir/gitdir" ] ||
        fail "linked worktree registration is missing or symlinked"
    [ "$(sed -n '1p' "$initial_git_dir/gitdir")" = "$initial_root/.git" ] ||
        fail "linked worktree registration points to a different worktree"
fi

initial_common_identity=$(path_identity "$initial_common") ||
    fail "cannot identify the Git common directory"
initial_git_dir_identity=$(path_identity "$initial_git_dir") ||
    fail "cannot identify the script worktree registration"
initial_anchor_identity=$(path_identity "$initial_anchor") ||
    fail "cannot identify the primary worktree"
repository_identity_matches ||
    fail "repository identity changed during worktree creation"

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

require_branch_absent \
    "branch already exists" \
    "cannot determine whether the branch already exists"

if origin_url=$(git --git-dir="$initial_common" config --get remote.origin.url 2>/dev/null); then
    [ -n "$origin_url" ] || fail "origin remote has an empty URL"
    git --git-dir="$initial_common" fetch --quiet origin main >/dev/null 2>&1 ||
        fail "fetch of origin/main failed"
    base_ref=refs/remotes/origin/main
    base_commit=$(
        git --git-dir="$initial_common" rev-parse --verify 'FETCH_HEAD^{commit}' 2>/dev/null
    ) || fail "fetched origin/main does not resolve to a commit"
else
    origin_status=$?
    [ "$origin_status" -eq 1 ] || fail "cannot inspect the origin remote"
    base_ref=refs/heads/main
    base_commit=$(
        git --git-dir="$initial_common" rev-parse --verify "$base_ref^{commit}" 2>/dev/null
    ) || fail "main does not resolve to a commit"
fi

if [ "$parent_initially_present" = true ]; then
    same_identity "$worktrees_directory" "$initial_parent_identity" ||
        fail "worktree parent identity changed during validation"
else
    [ ! -e "$worktrees_directory" ] && [ ! -L "$worktrees_directory" ] ||
        fail "worktree parent appeared during validation"
fi
validate_destination_absent
require_branch_absent \
    "branch appeared during validation" \
    "cannot recheck the requested branch"
git --git-dir="$initial_common" cat-file -e "$base_commit^{commit}" 2>/dev/null ||
    fail "cannot recheck the base commit"
repository_identity_matches ||
    fail "repository identity changed during worktree creation"

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
