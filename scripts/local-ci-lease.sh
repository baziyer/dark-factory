#!/bin/sh

# The local CI lease is a repository-common-directory primitive: every linked
# worktree resolves to the same git common directory, while unrelated clones
# get independent leases.

LOCAL_CI_LEASE_NAME=.dark-factory-local-ci
LOCAL_CI_LEASE_OWNER_PREFIX=.dark-factory-local-ci-owner.
LOCAL_CI_LEASE_MAX_DIAGNOSTIC_BYTES=2048
LOCAL_CI_LEASE_MAX_FIELD_BYTES=256

local_ci_lease_common_dir() {
    local_ci_lease_dir=$(git rev-parse --git-common-dir 2>/dev/null) || {
        echo "local-ci: cannot resolve the git common directory" >&2
        return 1
    }
    case "$local_ci_lease_dir" in
        /*) ;;
        *) local_ci_lease_dir=$(CDPATH= cd -- "$local_ci_lease_dir" && pwd -P) || return 1 ;;
    esac
    printf '%s\n' "$local_ci_lease_dir"
}

local_ci_lease_path() {
    local_ci_lease_common_dir=$(local_ci_lease_common_dir) || return 1
    printf '%s/%s\n' "$local_ci_lease_common_dir" "$LOCAL_CI_LEASE_NAME"
}

local_ci_lease_trim_field() {
    printf '%s' "$1" | cut -c "1-$LOCAL_CI_LEASE_MAX_FIELD_BYTES"
}

local_ci_lease_write_owner() {
    local_ci_lease_owner_record=$1
    local_ci_lease_worktree=$(git rev-parse --show-toplevel 2>/dev/null || pwd -P)
    local_ci_lease_started_at=$(date -u '+%Y-%m-%dT%H:%M:%SZ')
    local_ci_lease_process_start=$(ps -p "$$" -o lstart= 2>/dev/null | sed 's/^ *//')
    local_ci_lease_agent=${DARK_FACTORY_AGENT-}
    local_ci_lease_task=${DARK_FACTORY_TASK-}
    {
        printf 'pid=%s\n' "$$"
        printf 'process_start=%s\n' "$(local_ci_lease_trim_field "$local_ci_lease_process_start")"
        printf 'worktree=%s\n' "$(local_ci_lease_trim_field "$local_ci_lease_worktree")"
        printf 'started_at=%s\n' "$(local_ci_lease_trim_field "$local_ci_lease_started_at")"
        printf 'agent=%s\n' "$(local_ci_lease_trim_field "$local_ci_lease_agent")"
        printf 'task=%s\n' "$(local_ci_lease_trim_field "$local_ci_lease_task")"
    } >"$local_ci_lease_owner_record"
}

local_ci_lease_exists() {
    [ -e "$LOCAL_CI_LEASE_PATH" ] || [ -L "$LOCAL_CI_LEASE_PATH" ]
}

local_ci_lease_owner_ref() {
    local_ci_lease_ref=$(readlink "$LOCAL_CI_LEASE_PATH" 2>/dev/null) || return 1
    case "$local_ci_lease_ref" in
        "$LOCAL_CI_LEASE_OWNER_PREFIX"*) printf '%s\n' "$local_ci_lease_ref" ;;
        *) return 1 ;;
    esac
}

local_ci_lease_owner_record_path() {
    local_ci_lease_ref=$1
    case "$local_ci_lease_ref" in
        "$LOCAL_CI_LEASE_OWNER_PREFIX"*)
            case "$local_ci_lease_ref" in
                */*|*..*) return 1 ;;
            esac
            ;;
        *) return 1 ;;
    esac
    printf '%s/%s\n' "$LOCAL_CI_LEASE_COMMON_DIR" "$local_ci_lease_ref"
}

local_ci_lease_field() {
    local_ci_lease_record=$1
    local_ci_lease_key=$2
    head -c "$LOCAL_CI_LEASE_MAX_DIAGNOSTIC_BYTES" "$local_ci_lease_record" 2>/dev/null \
        | sed -n "s/^${local_ci_lease_key}=//p" | head -n 1
}

local_ci_lease_owner_pid() {
    local_ci_lease_record=$1
    local_ci_lease_field "$local_ci_lease_record" pid
}

local_ci_lease_owner_is_live() {
    local_ci_lease_record=$1
    local_ci_lease_pid=$(local_ci_lease_owner_pid "$local_ci_lease_record")
    case "$local_ci_lease_pid" in
        ''|*[!0-9]*) return 1 ;;
    esac
    kill -0 "$local_ci_lease_pid" 2>/dev/null || return 1

    local_ci_lease_expected_start=$(local_ci_lease_field "$local_ci_lease_record" process_start)
    if [ -n "$local_ci_lease_expected_start" ]; then
        local_ci_lease_actual_start=$(ps -p "$local_ci_lease_pid" -o lstart= 2>/dev/null | sed 's/^ *//')
        [ -z "$local_ci_lease_actual_start" ] || [ "$local_ci_lease_actual_start" = "$local_ci_lease_expected_start" ] || return 1
    fi
    return 0
}

local_ci_lease_owner_is_ancestor() {
    local_ci_lease_owner_pid=$1
    local_ci_lease_cursor=$$
    local_ci_lease_depth=0
    while [ "$local_ci_lease_cursor" -gt 1 ] && [ "$local_ci_lease_depth" -lt 32 ]; do
        [ "$local_ci_lease_cursor" = "$local_ci_lease_owner_pid" ] && return 0
        local_ci_lease_parent=$(ps -p "$local_ci_lease_cursor" -o ppid= 2>/dev/null | tr -d ' ')
        case "$local_ci_lease_parent" in
            ''|*[!0-9]*) return 1 ;;
        esac
        local_ci_lease_cursor=$local_ci_lease_parent
        local_ci_lease_depth=$((local_ci_lease_depth + 1))
    done
    return 1
}

local_ci_lease_diagnostic() {
    local_ci_lease_record=$1
    local_ci_lease_snapshot=$(head -c "$LOCAL_CI_LEASE_MAX_DIAGNOSTIC_BYTES" "$local_ci_lease_record" 2>/dev/null || true)
    printf 'local-ci: waiting for the gate; current owner:\n%s\n' "$local_ci_lease_snapshot" >&2
}

local_ci_lease_recover_stale() {
    local_ci_lease_ref=$1
    local_ci_lease_record=$(local_ci_lease_owner_record_path "$local_ci_lease_ref") || return 1
    local_ci_lease_recovery="$LOCAL_CI_LEASE_PATH.stale.$$"
    local_ci_lease_suffix=0
    while [ -e "$local_ci_lease_recovery" ] || [ -L "$local_ci_lease_recovery" ]; do
        local_ci_lease_suffix=$((local_ci_lease_suffix + 1))
        local_ci_lease_recovery="$LOCAL_CI_LEASE_PATH.stale.$$.$local_ci_lease_suffix"
    done

    # Rename the exact link we inspected. If another waiter replaced it in
    # the meantime, compare the moved link and never remove the new owner.
    mv "$LOCAL_CI_LEASE_PATH" "$local_ci_lease_recovery" 2>/dev/null || return 1
    local_ci_lease_moved_ref=$(readlink "$local_ci_lease_recovery" 2>/dev/null || true)
    if [ "$local_ci_lease_moved_ref" = "$local_ci_lease_ref" ]; then
        rm -f "$local_ci_lease_recovery" "$local_ci_lease_record"
    else
        rm -f "$local_ci_lease_recovery"
    fi
}

local_ci_lease_acquire() {
    LOCAL_CI_LEASE_PATH=$(local_ci_lease_path) || return 1
    LOCAL_CI_LEASE_COMMON_DIR=${LOCAL_CI_LEASE_PATH%/*}
    LOCAL_CI_LEASE_OWNER_RECORD=
    LOCAL_CI_LEASE_ACQUIRED=0
    local_ci_lease_wait_message=0

    while :; do
        local_ci_lease_owner_record=$(mktemp "$LOCAL_CI_LEASE_COMMON_DIR/${LOCAL_CI_LEASE_OWNER_PREFIX}XXXXXXX") || {
            echo "local-ci: cannot create owner diagnostics in $LOCAL_CI_LEASE_COMMON_DIR" >&2
            return 1
        }
        local_ci_lease_write_owner "$local_ci_lease_owner_record"
        local_ci_lease_ref=$(basename "$local_ci_lease_owner_record")
        if ln -s "$local_ci_lease_ref" "$LOCAL_CI_LEASE_PATH" 2>/dev/null; then
            LOCAL_CI_LEASE_OWNER_RECORD="$local_ci_lease_owner_record"
            LOCAL_CI_LEASE_ACQUIRED=1
            return 0
        fi
        rm -f "$local_ci_lease_owner_record"

        if ! local_ci_lease_exists; then
            echo "local-ci: cannot create lease at $LOCAL_CI_LEASE_PATH" >&2
            return 1
        fi
        if [ ! -L "$LOCAL_CI_LEASE_PATH" ]; then
            echo "local-ci: refusing unknown non-symlink lease path $LOCAL_CI_LEASE_PATH" >&2
            return 1
        fi
        local_ci_lease_ref=$(local_ci_lease_owner_ref || true)
        if [ -z "$local_ci_lease_ref" ]; then
            echo "local-ci: refusing lease with invalid owner metadata at $LOCAL_CI_LEASE_PATH" >&2
            return 1
        fi
        local_ci_lease_record=$(local_ci_lease_owner_record_path "$local_ci_lease_ref") || return 1
        if [ ! -f "$local_ci_lease_record" ]; then
            echo "local-ci: refusing lease with missing owner metadata ($local_ci_lease_ref)" >&2
            return 1
        fi
        local_ci_lease_pid=$(local_ci_lease_owner_pid "$local_ci_lease_record")
        if local_ci_lease_owner_is_live "$local_ci_lease_record"; then
            if local_ci_lease_owner_is_ancestor "$local_ci_lease_pid"; then
                echo "local-ci: nested invocation would wait on its ancestor (pid $local_ci_lease_pid); refusing" >&2
                return 1
            fi
            if [ "${DARK_FACTORY_LOCAL_CI_WAIT-1}" = 0 ]; then
                local_ci_lease_diagnostic "$local_ci_lease_record"
                echo "local-ci: gate is already owned; DARK_FACTORY_LOCAL_CI_WAIT=0 requested no wait" >&2
                return 1
            fi
            if [ "$local_ci_lease_wait_message" -eq 0 ]; then
                local_ci_lease_diagnostic "$local_ci_lease_record"
                local_ci_lease_wait_message=1
            fi
            sleep 1
            continue
        fi
        local_ci_lease_recover_stale "$local_ci_lease_ref" || sleep 1
    done
}

local_ci_lease_release() {
    [ "${LOCAL_CI_LEASE_ACQUIRED-0}" -eq 1 ] || return 0
    local_ci_lease_current_ref=$(readlink "$LOCAL_CI_LEASE_PATH" 2>/dev/null || true)
    local_ci_lease_expected_ref=$(basename "$LOCAL_CI_LEASE_OWNER_RECORD")
    if [ "$local_ci_lease_current_ref" = "$local_ci_lease_expected_ref" ]; then
        rm -f "$LOCAL_CI_LEASE_PATH" "$LOCAL_CI_LEASE_OWNER_RECORD"
    fi
    LOCAL_CI_LEASE_ACQUIRED=0
}
