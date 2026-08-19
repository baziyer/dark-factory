#!/bin/sh

# The kernel lock is the authority.  The symlink and record are bounded,
# owner-aware diagnostics only; they are never used to decide that a live
# lease is stale.  lockf's descriptor is inherited by the command and its
# descendants, so killing the wrapper shell cannot release the gate while an
# owned command is still alive.

LOCAL_CI_LEASE_NAME=.dark-factory-local-ci
LOCAL_CI_LEASE_OWNER_PREFIX=.dark-factory-local-ci-owner.
LOCAL_CI_LEASE_LOCK_NAME=.dark-factory-local-ci.lock
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

local_ci_lease_setup_paths() {
    LOCAL_CI_LEASE_COMMON_DIR=$(local_ci_lease_common_dir) || return 1
    LOCAL_CI_LEASE_PATH="$LOCAL_CI_LEASE_COMMON_DIR/$LOCAL_CI_LEASE_NAME"
    LOCAL_CI_LEASE_LOCK="$LOCAL_CI_LEASE_COMMON_DIR/$LOCAL_CI_LEASE_LOCK_NAME"
}

local_ci_lease_bound_field() {
    # Drop controls before truncation.  This keeps diagnostics single-line and
    # prevents an agent/task label from forging additional owner fields.
    printf '%s' "$1" | LC_ALL=C tr -d '\000-\037\177' | cut -c "1-$LOCAL_CI_LEASE_MAX_FIELD_BYTES"
}

local_ci_lease_identifier() {
    local_ci_lease_identifier_value=$1
    case "$local_ci_lease_identifier_value" in
        '') printf '\n' ;;
        *[!A-Za-z0-9._:-]*) printf '<redacted>\n' ;;
        *) local_ci_lease_bound_field "$local_ci_lease_identifier_value" ;;
    esac
}

local_ci_lease_valid_ref() {
    case "$1" in
        "$LOCAL_CI_LEASE_OWNER_PREFIX"*)
            case "$1" in
                */*|*..*) return 1 ;;
                *) return 0 ;;
            esac
            ;;
        *) return 1 ;;
    esac
}

local_ci_lease_owner_ref() {
    local_ci_lease_ref=$(readlink "$LOCAL_CI_LEASE_PATH" 2>/dev/null) || return 1
    local_ci_lease_valid_ref "$local_ci_lease_ref" || return 1
    printf '%s\n' "$local_ci_lease_ref"
}

local_ci_lease_owner_record_path() {
    local_ci_lease_valid_ref "$1" || return 1
    printf '%s/%s\n' "$LOCAL_CI_LEASE_COMMON_DIR" "$1"
}

local_ci_lease_field() {
    local_ci_lease_record=$1
    local_ci_lease_key=$2
    head -c "$LOCAL_CI_LEASE_MAX_DIAGNOSTIC_BYTES" "$local_ci_lease_record" 2>/dev/null \
        | sed -n "s/^${local_ci_lease_key}=//p" | head -n 1
}

local_ci_lease_record_is_valid() {
    local_ci_lease_record=$1
    local_ci_lease_pid=$(local_ci_lease_field "$local_ci_lease_record" pid)
    local_ci_lease_worktree=$(local_ci_lease_field "$local_ci_lease_record" worktree)
    local_ci_lease_started_at=$(local_ci_lease_field "$local_ci_lease_record" started_at)
    local_ci_lease_head=$(local_ci_lease_field "$local_ci_lease_record" head)
    case "$local_ci_lease_pid" in
        ''|*[!0-9]*) return 1 ;;
    esac
    [ -n "$local_ci_lease_worktree" ] && [ -n "$local_ci_lease_started_at" ] \
        && [ -n "$local_ci_lease_head" ]
}

local_ci_lease_diagnostic() {
    local_ci_lease_snapshot=
    if [ -L "$LOCAL_CI_LEASE_PATH" ]; then
        local_ci_lease_ref=$(local_ci_lease_owner_ref || true)
        if [ -n "$local_ci_lease_ref" ]; then
            local_ci_lease_record=$(local_ci_lease_owner_record_path "$local_ci_lease_ref" || true)
            if [ -f "$local_ci_lease_record" ]; then
                local_ci_lease_snapshot=$(head -c "$LOCAL_CI_LEASE_MAX_DIAGNOSTIC_BYTES" "$local_ci_lease_record" 2>/dev/null || true)
            fi
        fi
    fi
    if [ -z "$local_ci_lease_snapshot" ]; then
        local_ci_lease_snapshot='owner=unavailable (kernel lease is held; metadata may be between updates)'
    fi
    printf 'local-ci: waiting for the gate; current owner:\n%s\n' "$local_ci_lease_snapshot" >&2
}

local_ci_lease_clear_metadata() {
    if [ -e "$LOCAL_CI_LEASE_PATH" ] || [ -L "$LOCAL_CI_LEASE_PATH" ]; then
        [ -L "$LOCAL_CI_LEASE_PATH" ] || {
            echo "local-ci: refusing unknown non-symlink lease path $LOCAL_CI_LEASE_PATH" >&2
            return 1
        }
        local_ci_lease_ref=$(local_ci_lease_owner_ref || true)
        [ -n "$local_ci_lease_ref" ] || {
            echo "local-ci: refusing lease with invalid owner metadata at $LOCAL_CI_LEASE_PATH" >&2
            return 1
        }
        local_ci_lease_record=$(local_ci_lease_owner_record_path "$local_ci_lease_ref") || return 1
        [ -f "$local_ci_lease_record" ] && local_ci_lease_record_is_valid "$local_ci_lease_record" || {
            echo "local-ci: refusing lease with invalid owner metadata at $LOCAL_CI_LEASE_PATH" >&2
            return 1
        }
        rm -f "$LOCAL_CI_LEASE_PATH" "$local_ci_lease_record"
    fi
}

local_ci_lease_write_owner() {
    local_ci_lease_owner_record=$1
    local_ci_lease_worktree=$(git rev-parse --show-toplevel 2>/dev/null || pwd -P)
    local_ci_lease_started_at=$(date -u '+%Y-%m-%dT%H:%M:%SZ')
    local_ci_lease_process_start=$(ps -p "$$" -o lstart= 2>/dev/null | sed 's/^ *//')
    local_ci_lease_head=$(git rev-parse HEAD 2>/dev/null || printf 'unknown')
    {
        printf 'pid=%s\n' "$$"
        printf 'process_start=%s\n' "$(local_ci_lease_bound_field "$local_ci_lease_process_start")"
        printf 'worktree=%s\n' "$(local_ci_lease_bound_field "$local_ci_lease_worktree")"
        printf 'started_at=%s\n' "$(local_ci_lease_bound_field "$local_ci_lease_started_at")"
        printf 'head=%s\n' "$(local_ci_lease_identifier "$local_ci_lease_head")"
        printf 'agent=%s\n' "$(local_ci_lease_identifier "${DARK_FACTORY_AGENT-}")"
        printf 'task=%s\n' "$(local_ci_lease_identifier "${DARK_FACTORY_TASK-}")"
    } >"$local_ci_lease_owner_record"
}

local_ci_lease_publish_owner() {
    local_ci_lease_clear_metadata || return 1
    local_ci_lease_owner_ref=$(basename "$LOCAL_CI_LEASE_OWNER_RECORD")
    ln -s "$local_ci_lease_owner_ref" "$LOCAL_CI_LEASE_PATH" 2>/dev/null || {
        echo "local-ci: cannot publish owner metadata at $LOCAL_CI_LEASE_PATH" >&2
        return 1
    }
}

local_ci_lease_release_owner() {
    local_ci_lease_current_ref=$(local_ci_lease_owner_ref || true)
    local_ci_lease_expected_ref=$(basename "${LOCAL_CI_LEASE_OWNER_RECORD-}")
    if [ -n "$local_ci_lease_expected_ref" ] && [ "$local_ci_lease_current_ref" = "$local_ci_lease_expected_ref" ]; then
        rm -f "$LOCAL_CI_LEASE_PATH" "$LOCAL_CI_LEASE_OWNER_RECORD"
    fi
}

local_ci_lease_holder() {
    LOCAL_CI_LEASE_COMMON_DIR=$1
    LOCAL_CI_LEASE_PATH="$LOCAL_CI_LEASE_COMMON_DIR/$LOCAL_CI_LEASE_NAME"
    LOCAL_CI_LEASE_LOCK="$LOCAL_CI_LEASE_COMMON_DIR/$LOCAL_CI_LEASE_LOCK_NAME"
    shift

    LOCAL_CI_LEASE_OWNER_RECORD=$(mktemp "$LOCAL_CI_LEASE_COMMON_DIR/${LOCAL_CI_LEASE_OWNER_PREFIX}XXXXXXX") || {
        echo "local-ci: cannot create owner diagnostics in $LOCAL_CI_LEASE_COMMON_DIR" >&2
        return 1
    }
    local_ci_lease_write_owner "$LOCAL_CI_LEASE_OWNER_RECORD"
    local_ci_lease_publish_owner || {
        rm -f "$LOCAL_CI_LEASE_OWNER_RECORD"
        return 1
    }

    local_ci_lease_child_pid=
    local_ci_lease_holder_cleanup() {
        local_ci_lease_status=$?
        trap - EXIT HUP INT TERM
        if [ -n "$local_ci_lease_child_pid" ]; then
            kill -TERM "$local_ci_lease_child_pid" 2>/dev/null || true
            wait "$local_ci_lease_child_pid" 2>/dev/null || true
        fi
        local_ci_lease_release_owner
        exit "$local_ci_lease_status"
    }
    local_ci_lease_holder_signal() {
        local_ci_lease_signal=$1
        trap - EXIT HUP INT TERM
        if [ -n "$local_ci_lease_child_pid" ]; then
            kill -"$local_ci_lease_signal" "$local_ci_lease_child_pid" 2>/dev/null || true
            wait "$local_ci_lease_child_pid" 2>/dev/null || true
        fi
        local_ci_lease_release_owner
        exit $((128 + local_ci_lease_signal))
    }
    trap local_ci_lease_holder_cleanup EXIT
    trap 'local_ci_lease_holder_signal 1' HUP
    trap 'local_ci_lease_holder_signal 2' INT
    trap 'local_ci_lease_holder_signal 15' TERM

    export DARK_FACTORY_LOCAL_CI_LEASE_HELD=1
    "$@" &
    local_ci_lease_child_pid=$!
    set +e
    wait "$local_ci_lease_child_pid"
    local_ci_lease_status=$?
    set -e
    local_ci_lease_child_pid=
    return "$local_ci_lease_status"
}

local_ci_lease_run() {
    [ "$#" -gt 0 ] || {
        echo "local-ci: lease wrapper requires a command" >&2
        return 64
    }
    case "${DARK_FACTORY_LOCAL_CI_LEASE_HELD-}" in
        1)
            echo "local-ci: nested lease invocation refused; use the existing owner" >&2
            return 1
            ;;
    esac

    local_ci_lease_setup_paths || return 1
    command -v lockf >/dev/null 2>&1 || {
        echo "local-ci: lockf is required for the repository lease" >&2
        return 1
    }

    local_ci_lease_wait_message=0
    if ! lockf -s -k -t 0 "$LOCAL_CI_LEASE_LOCK" true 2>/dev/null; then
        local_ci_lease_diagnostic
        local_ci_lease_wait_message=1
        if [ "${DARK_FACTORY_LOCAL_CI_WAIT-1}" = 0 ]; then
            echo "local-ci: gate is already owned; DARK_FACTORY_LOCAL_CI_WAIT=0 requested no wait" >&2
            return 1
        fi
    fi

    # The holder is a child of this wrapper.  Its lock descriptor is inherited
    # by the command, so an abnormal wrapper exit cannot release a surviving
    # command descendant's lease.
    lockf -k "$LOCAL_CI_LEASE_LOCK" sh -c '
        set -eu
        helper=$1
        common_dir=$2
        shift 2
        . "$helper"
        local_ci_lease_holder "$common_dir" "$@"
    ' local-ci-lease-holder "$LOCAL_CI_LEASE_HELPER" "$LOCAL_CI_LEASE_COMMON_DIR" "$@" &
    local_ci_lease_holder_pid=$!

    local_ci_lease_wrapper_cleanup() {
        local_ci_lease_status=$?
        trap - EXIT HUP INT TERM
        kill -TERM "$local_ci_lease_holder_pid" 2>/dev/null || true
        wait "$local_ci_lease_holder_pid" 2>/dev/null || true
        exit "$local_ci_lease_status"
    }
    local_ci_lease_wrapper_signal() {
        local_ci_lease_signal=$1
        trap - EXIT HUP INT TERM
        kill -"$local_ci_lease_signal" "$local_ci_lease_holder_pid" 2>/dev/null || true
        wait "$local_ci_lease_holder_pid" 2>/dev/null || true
        exit $((128 + local_ci_lease_signal))
    }
    trap local_ci_lease_wrapper_cleanup EXIT
    trap 'local_ci_lease_wrapper_signal 1' HUP
    trap 'local_ci_lease_wrapper_signal 2' INT
    trap 'local_ci_lease_wrapper_signal 15' TERM

    set +e
    wait "$local_ci_lease_holder_pid"
    local_ci_lease_status=$?
    set -e
    trap - EXIT HUP INT TERM
    exit "$local_ci_lease_status"
}

# The helper is sourced by the wrapper and by the lock-holder shell.
LOCAL_CI_LEASE_HELPER=${LOCAL_CI_LEASE_HELPER:-$(CDPATH= cd -- "$(dirname "$0")" && pwd)/local-ci-lease.sh}
