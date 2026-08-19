#!/bin/sh

# The kernel lock is the authority.  The symlink and record are bounded,
# owner-aware diagnostics only; they are never used to decide that a live
# lease is stale.  lockf's descriptor is inherited by the command and its
# descendants, so killing the wrapper shell cannot release the gate while an
# owned command is still alive.

LOCAL_CI_LEASE_NAME=.dark-factory-local-ci
LOCAL_CI_LEASE_OWNER_PREFIX=.dark-factory-local-ci-owner.
LOCAL_CI_LEASE_LOCK_NAME=.dark-factory-local-ci.lock
LOCAL_CI_LEASE_LOCK_FILE_NAME=descriptor
LOCAL_CI_LEASE_STARTING_NAME=.starting
LOCAL_CI_LEASE_STARTING_OWNER_NAME=owner
LOCAL_CI_LEASE_RECOVERY_NAME=.recovery
LOCAL_CI_LEASE_MAX_DIAGNOSTIC_BYTES=2048
LOCAL_CI_LEASE_MAX_FIELD_BYTES=256
LOCAL_CI_LEASE_MAX_IDENTIFIER_BYTES=128

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
        ghp_*|github_pat_*|xox[baprs]-*|sk-*) printf '<redacted>\n' ;;
        *[!A-Za-z0-9_-]*) printf '<redacted>\n' ;;
        *)
            [ "${#local_ci_lease_identifier_value}" -le "$LOCAL_CI_LEASE_MAX_IDENTIFIER_BYTES" ] \
                && printf '%s\n' "$local_ci_lease_identifier_value" \
                || printf '<redacted>\n'
            ;;
    esac
}

local_ci_lease_valid_ref() {
    case "$1" in
        "$LOCAL_CI_LEASE_OWNER_PREFIX"*)
            local_ci_lease_ref_suffix=${1#"$LOCAL_CI_LEASE_OWNER_PREFIX"}
            [ -n "$local_ci_lease_ref_suffix" ] || return 1
            case "$local_ci_lease_ref_suffix" in
                *[!A-Za-z0-9_-]*) return 1 ;;
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

local_ci_lease_record_is_regular() {
    [ -f "$1" ] && [ ! -L "$1" ]
}

local_ci_lease_field() {
    local_ci_lease_record=$1
    local_ci_lease_key=$2
    head -c "$LOCAL_CI_LEASE_MAX_DIAGNOSTIC_BYTES" "$local_ci_lease_record" 2>/dev/null \
        | sed -n "s/^${local_ci_lease_key}=//p" | head -n 1
}

local_ci_lease_record_is_valid() {
    local_ci_lease_record=$1
    local_ci_lease_record_is_regular "$local_ci_lease_record" || return 1
    local_ci_lease_pid=$(local_ci_lease_field "$local_ci_lease_record" pid)
    local_ci_lease_worktree=$(local_ci_lease_field "$local_ci_lease_record" worktree)
    local_ci_lease_started_at=$(local_ci_lease_field "$local_ci_lease_record" started_at)
    local_ci_lease_head=$(local_ci_lease_field "$local_ci_lease_record" head)
    local_ci_lease_lock_identity=$(local_ci_lease_field "$local_ci_lease_record" lock_identity)
    case "$local_ci_lease_pid" in
        ''|*[!0-9]*) return 1 ;;
    esac
    case "$local_ci_lease_head" in
        unknown) ;;
        '') return 1 ;;
        *)
            case "$local_ci_lease_head" in
                *[!0-9a-f]*) return 1 ;;
            esac
            [ "${#local_ci_lease_head}" -eq 40 ] || return 1
            ;;
    esac
    [ -n "$local_ci_lease_worktree" ] && [ -n "$local_ci_lease_started_at" ] \
        && [ -n "$local_ci_lease_head" ] || return 1
    case "$local_ci_lease_lock_identity" in
        [0-9]*:[0-9]*) return 0 ;;
        *) return 1 ;;
    esac
}

local_ci_lease_lock_identity() {
    stat -f '%d:%i' "$LOCAL_CI_LEASE_LOCK" 2>/dev/null
}

local_ci_lease_owner_matches_lock_object() {
    [ -L "$LOCAL_CI_LEASE_PATH" ] || return 0
    local_ci_lease_ref=$(local_ci_lease_owner_ref || return 2)
    local_ci_lease_record=$(local_ci_lease_owner_record_path "$local_ci_lease_ref" || return 2)
    local_ci_lease_record_is_valid "$local_ci_lease_record" || return 2
    local_ci_lease_expected_identity=$(local_ci_lease_field "$local_ci_lease_record" lock_identity)
    local_ci_lease_current_identity=$(local_ci_lease_lock_identity || true)
    [ -n "$local_ci_lease_current_identity" ] || return 2
    [ "$local_ci_lease_expected_identity" = "$local_ci_lease_current_identity" ] || return 2
}

local_ci_lease_diagnostic() {
    local_ci_lease_snapshot=
    if [ -L "$LOCAL_CI_LEASE_PATH" ]; then
        local_ci_lease_ref=$(local_ci_lease_owner_ref || true)
        if [ -n "$local_ci_lease_ref" ]; then
            local_ci_lease_record=$(local_ci_lease_owner_record_path "$local_ci_lease_ref" || true)
            if local_ci_lease_record_is_valid "$local_ci_lease_record"; then
                local_ci_lease_pid=$(local_ci_lease_field "$local_ci_lease_record" pid)
                local_ci_lease_process_start=$(local_ci_lease_field "$local_ci_lease_record" process_start)
                local_ci_lease_worktree=$(local_ci_lease_field "$local_ci_lease_record" worktree)
                local_ci_lease_started_at=$(local_ci_lease_field "$local_ci_lease_record" started_at)
                local_ci_lease_head=$(local_ci_lease_field "$local_ci_lease_record" head)
                local_ci_lease_lock_identity=$(local_ci_lease_field "$local_ci_lease_record" lock_identity)
                local_ci_lease_agent=$(local_ci_lease_field "$local_ci_lease_record" agent)
                local_ci_lease_task=$(local_ci_lease_field "$local_ci_lease_record" task)
                local_ci_lease_snapshot=$( {
                    printf 'pid=%s\n' "$local_ci_lease_pid"
                    printf 'process_start=%s\n' "$(local_ci_lease_bound_field "$local_ci_lease_process_start")"
                    printf 'worktree=%s\n' "$(local_ci_lease_bound_field "$local_ci_lease_worktree")"
                    printf 'started_at=%s\n' "$(local_ci_lease_bound_field "$local_ci_lease_started_at")"
                    printf 'lock_identity=%s\n' "$(local_ci_lease_bound_field "$local_ci_lease_lock_identity")"
                    printf 'head=%s\n' "$(local_ci_lease_identifier "$local_ci_lease_head")"
                    printf 'agent=%s\n' "$(local_ci_lease_identifier "$local_ci_lease_agent")"
                    printf 'task=%s\n' "$(local_ci_lease_identifier "$local_ci_lease_task")"
                } | head -c "$LOCAL_CI_LEASE_MAX_DIAGNOSTIC_BYTES" )
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
        local_ci_lease_record_is_valid "$local_ci_lease_record" || {
            echo "local-ci: refusing lease with invalid owner metadata at $LOCAL_CI_LEASE_PATH" >&2
            return 1
        }
        rm -f "$LOCAL_CI_LEASE_PATH"
        rm -f "$local_ci_lease_record"
    fi
}

local_ci_lease_lock_object_is_safe() {
    [ -d "$LOCAL_CI_LEASE_LOCK" ] && [ ! -L "$LOCAL_CI_LEASE_LOCK" ] || return 1
    local_ci_lease_lock_directory=$(CDPATH= cd -P -- "$LOCAL_CI_LEASE_LOCK" 2>/dev/null && pwd -P) || return 1
    [ "$local_ci_lease_lock_directory" = "$LOCAL_CI_LEASE_LOCK" ]
}

local_ci_lease_enter_lock_object() {
    [ -d "$LOCAL_CI_LEASE_LOCK" ] && [ ! -L "$LOCAL_CI_LEASE_LOCK" ] || return 1
    CDPATH= cd -P -- "$LOCAL_CI_LEASE_LOCK" || return 1
    [ "$(pwd -P)" = "$LOCAL_CI_LEASE_LOCK" ]
}

local_ci_lease_lock_file_is_safe() {
    local_ci_lease_lock_object_is_safe || return 1
    (
        local_ci_lease_enter_lock_object &&
            { [ ! -L "$LOCAL_CI_LEASE_LOCK_FILE_NAME" ] || exit 1; } &&
            { [ ! -e "$LOCAL_CI_LEASE_LOCK_FILE_NAME" ] || [ -f "$LOCAL_CI_LEASE_LOCK_FILE_NAME" ]; }
    )
}

local_ci_lease_prepare_lock_object() {
    local_ci_lease_lock_object_is_safe || return 1
    (
        local_ci_lease_enter_lock_object &&
            [ ! -e "$LOCAL_CI_LEASE_LOCK_FILE_NAME" ] &&
            (set -C; : >"$LOCAL_CI_LEASE_LOCK_FILE_NAME")
    )
}

local_ci_lease_write_starting_marker() {
    local_ci_lease_starting_record="$LOCAL_CI_LEASE_STARTING_NAME/$LOCAL_CI_LEASE_STARTING_OWNER_NAME"
    {
        printf 'pid=%s\n' "$$"
    } >"$local_ci_lease_starting_record"
}

local_ci_lease_remove_starting() {
    local_ci_lease_expected_identity=$1
    if [ ! -e "$LOCAL_CI_LEASE_STARTING_NAME" ] && [ ! -L "$LOCAL_CI_LEASE_STARTING_NAME" ]; then
        return 0
    fi
    [ -d "$LOCAL_CI_LEASE_STARTING_NAME" ] && [ ! -L "$LOCAL_CI_LEASE_STARTING_NAME" ] || return 1
    local_ci_lease_current_identity=$(stat -f '%d:%i' "$LOCAL_CI_LEASE_STARTING_NAME" 2>/dev/null) || return 1
    [ "$local_ci_lease_current_identity" = "$local_ci_lease_expected_identity" ] || return 1
    local_ci_lease_starting_record="$LOCAL_CI_LEASE_STARTING_NAME/$LOCAL_CI_LEASE_STARTING_OWNER_NAME"
    if [ -e "$local_ci_lease_starting_record" ] || [ -L "$local_ci_lease_starting_record" ]; then
        [ -f "$local_ci_lease_starting_record" ] && [ ! -L "$local_ci_lease_starting_record" ] || return 1
        rm -f "$local_ci_lease_starting_record"
    fi
    rmdir "$LOCAL_CI_LEASE_STARTING_NAME"
}

local_ci_lease_remove_starting_stale() {
    if [ ! -e "$LOCAL_CI_LEASE_STARTING_NAME" ] && [ ! -L "$LOCAL_CI_LEASE_STARTING_NAME" ]; then
        return 0
    fi
    [ -d "$LOCAL_CI_LEASE_STARTING_NAME" ] && [ ! -L "$LOCAL_CI_LEASE_STARTING_NAME" ] || return 1
    local_ci_lease_starting_record="$LOCAL_CI_LEASE_STARTING_NAME/$LOCAL_CI_LEASE_STARTING_OWNER_NAME"
    if [ -e "$local_ci_lease_starting_record" ] || [ -L "$local_ci_lease_starting_record" ]; then
        [ -f "$local_ci_lease_starting_record" ] && [ ! -L "$local_ci_lease_starting_record" ] || return 1
        rm -f "$local_ci_lease_starting_record"
    fi
    rmdir "$LOCAL_CI_LEASE_STARTING_NAME"
}

local_ci_lease_remove_recovery_guard() {
    local_ci_lease_expected_identity=$1
    [ -n "$local_ci_lease_expected_identity" ] || return 0
    [ -d "$LOCAL_CI_LEASE_RECOVERY_NAME" ] && [ ! -L "$LOCAL_CI_LEASE_RECOVERY_NAME" ] || return 0
    [ "$(stat -f '%d:%i' "$LOCAL_CI_LEASE_RECOVERY_NAME" 2>/dev/null)" = "$local_ci_lease_expected_identity" ] || return 0
    rmdir "$LOCAL_CI_LEASE_RECOVERY_NAME" 2>/dev/null || true
}

local_ci_lease_reclaim_stale_recovery_guard() {
    [ -d "$LOCAL_CI_LEASE_RECOVERY_NAME" ] && [ ! -L "$LOCAL_CI_LEASE_RECOVERY_NAME" ] || return 1
    local_ci_lease_recovery_identity=$(stat -f '%d:%i' "$LOCAL_CI_LEASE_RECOVERY_NAME" 2>/dev/null) || return 1
    local_ci_lease_recovery_quarantine="$LOCAL_CI_LEASE_RECOVERY_NAME-reclaim-$$"
    [ ! -e "$local_ci_lease_recovery_quarantine" ] && [ ! -L "$local_ci_lease_recovery_quarantine" ] || return 1
    mv "$LOCAL_CI_LEASE_RECOVERY_NAME" "$local_ci_lease_recovery_quarantine" || return 1
    [ -d "$local_ci_lease_recovery_quarantine" ] && [ ! -L "$local_ci_lease_recovery_quarantine" ] || return 1
    [ "$(stat -f '%d:%i' "$local_ci_lease_recovery_quarantine" 2>/dev/null)" = "$local_ci_lease_recovery_identity" ] || return 1
    rmdir "$local_ci_lease_recovery_quarantine"
}

local_ci_lease_remove_lock_object() {
    local_ci_lease_lock_object_is_safe || return 0
    (
        local_ci_lease_enter_lock_object || exit 0
        local_ci_lease_object_identity=$(stat -f '%d:%i' . 2>/dev/null) || exit 0
        local_ci_lease_remove_starting "${LOCAL_CI_LEASE_STARTING_IDENTITY-}" || exit 0
        rm -f "$LOCAL_CI_LEASE_LOCK_FILE_NAME"
        # A holder never owns this guard; leave it for the contender that
        # created and identity-checked it, so recovery cannot be split.
        CDPATH= cd -- "$LOCAL_CI_LEASE_COMMON_DIR" || exit 0
        [ "$(stat -f '%d:%i' "$LOCAL_CI_LEASE_LOCK" 2>/dev/null)" = "$local_ci_lease_object_identity" ] || exit 0
        rmdir "$(basename "$LOCAL_CI_LEASE_LOCK")" 2>/dev/null || true
    )
}

local_ci_lease_lock_probe() {
    # Tri-state contract: 0 means the descriptor is currently unlocked, 1
    # means lockf reports it held, and 2 means the object cannot be validated.
    # Process enumeration is diagnostic only and never changes this result.
    local_ci_lease_lock_file_is_safe || return 2
    (
        local_ci_lease_enter_lock_object &&
            lockf -s -k -t 0 "$LOCAL_CI_LEASE_LOCK_FILE_NAME" true
    )
}

local_ci_lease_recover_lock_object() {
    local_ci_lease_lock_object_is_safe || return 2
    (
        local_ci_lease_enter_lock_object || exit 2
        local_ci_lease_object_identity=$(stat -f '%d:%i' . 2>/dev/null) || exit 2
        if lockf -k -t 0 "$LOCAL_CI_LEASE_LOCK_FILE_NAME" sh -c '
            set -eu
            helper=$1
            common_dir=$2
            object_identity=$3
            . "$helper"
            LOCAL_CI_LEASE_COMMON_DIR=$common_dir
            LOCAL_CI_LEASE_PATH="$common_dir/$LOCAL_CI_LEASE_NAME"
            LOCAL_CI_LEASE_LOCK="$common_dir/$LOCAL_CI_LEASE_LOCK_NAME"
            if [ -e "$LOCAL_CI_LEASE_RECOVERY_NAME" ] || [ -L "$LOCAL_CI_LEASE_RECOVERY_NAME" ]; then
                [ -d "$LOCAL_CI_LEASE_RECOVERY_NAME" ] && [ ! -L "$LOCAL_CI_LEASE_RECOVERY_NAME" ] || exit 2
                local_ci_lease_reclaim_stale_recovery_guard || exit 2
            fi
            mkdir "$LOCAL_CI_LEASE_RECOVERY_NAME"
            recovery_identity=$(stat -f "%d:%i" "$LOCAL_CI_LEASE_RECOVERY_NAME")
            local_ci_lease_clear_metadata || exit 2
            local_ci_lease_remove_starting_stale || exit 2
            rm -f "$LOCAL_CI_LEASE_LOCK_FILE_NAME"
            local_ci_lease_remove_recovery_guard "$recovery_identity" || exit 2
            CDPATH= cd -- "$LOCAL_CI_LEASE_COMMON_DIR"
            [ "$(stat -f "%d:%i" "$LOCAL_CI_LEASE_LOCK" 2>/dev/null)" = "$object_identity" ] || exit 2
            rmdir "$(basename "$LOCAL_CI_LEASE_LOCK")"
        ' local-ci-lease-recover "$LOCAL_CI_LEASE_HELPER" "$LOCAL_CI_LEASE_COMMON_DIR" \
            "$local_ci_lease_object_identity"; then
            exit 0
        else
            local_ci_lease_status=$?
            exit "$local_ci_lease_status"
        fi
    )
}

local_ci_lease_acquire_lock_object() {
    while :; do
        if [ ! -e "$LOCAL_CI_LEASE_LOCK" ] && [ -L "$LOCAL_CI_LEASE_PATH" ]; then
            echo "local-ci: refusing owner metadata without its lock object; invalid owner metadata or lock-object replacement requires manual inspection" >&2
            return 1
        fi
        if mkdir "$LOCAL_CI_LEASE_LOCK" 2>/dev/null; then
            local_ci_lease_prepare_lock_object || {
                rmdir "$LOCAL_CI_LEASE_LOCK" 2>/dev/null || true
                echo "local-ci: cannot create a substitution-resistant lock object at $LOCAL_CI_LEASE_LOCK" >&2
                return 1
            }
            return 0
        fi
        if ! local_ci_lease_lock_object_is_safe; then
            echo "local-ci: refusing unsafe lock object path $LOCAL_CI_LEASE_LOCK; remove the symlink or replacement manually" >&2
            return 1
        fi
        if ! local_ci_lease_owner_matches_lock_object; then
            if [ "${DARK_FACTORY_LOCAL_CI_WAIT-1}" = 0 ]; then
                echo "local-ci: refusing lock object replacement or invalid owner metadata at $LOCAL_CI_LEASE_LOCK; inspect and remove it manually" >&2
                return 1
            fi
            if local_ci_lease_lock_probe; then
                echo "local-ci: refusing lock object replacement or invalid owner metadata at $LOCAL_CI_LEASE_LOCK; inspect and remove it manually" >&2
                return 1
            fi
            local_ci_lease_probe_status=$?
            [ "$local_ci_lease_probe_status" -ne 2 ] || {
                echo "local-ci: cannot validate the lock object while owner metadata is invalid" >&2
                return 1
            }
            if [ "$local_ci_lease_reported_wait" -eq 0 ]; then
                local_ci_lease_diagnostic
                local_ci_lease_reported_wait=1
            fi
            sleep 1
            continue
        fi
        if [ "$local_ci_lease_reported_wait" -eq 0 ]; then
            local_ci_lease_diagnostic
            local_ci_lease_reported_wait=1
        fi
        if [ "${DARK_FACTORY_LOCAL_CI_WAIT-1}" = 0 ]; then
            echo "local-ci: gate is already owned; DARK_FACTORY_LOCAL_CI_WAIT=0 requested no wait" >&2
            return 1
        fi
        if local_ci_lease_recover_lock_object; then
            local_ci_lease_recovery_status=0
        else
            local_ci_lease_recovery_status=$?
        fi
        [ "$local_ci_lease_recovery_status" -eq 2 ] && return 1
        [ "$local_ci_lease_recovery_status" -eq 0 ] && continue
        sleep 1
    done
}

local_ci_lease_write_owner() {
    local_ci_lease_owner_record=$1
    local_ci_lease_worktree=$(git rev-parse --show-toplevel 2>/dev/null || pwd -P)
    local_ci_lease_started_at=$(date -u '+%Y-%m-%dT%H:%M:%SZ')
    local_ci_lease_process_start=$(ps -p "$$" -o lstart= 2>/dev/null | sed 's/^ *//')
    local_ci_lease_head=$(git rev-parse HEAD 2>/dev/null || printf 'unknown')
    local_ci_lease_lock_identity=$(local_ci_lease_lock_identity || printf 'unknown')
    {
        printf 'pid=%s\n' "$$"
        printf 'process_start=%s\n' "$(local_ci_lease_bound_field "$local_ci_lease_process_start")"
        printf 'worktree=%s\n' "$(local_ci_lease_bound_field "$local_ci_lease_worktree")"
        printf 'started_at=%s\n' "$(local_ci_lease_bound_field "$local_ci_lease_started_at")"
        printf 'lock_identity=%s\n' "$(local_ci_lease_bound_field "$local_ci_lease_lock_identity")"
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
        rm -f "$LOCAL_CI_LEASE_PATH"
    fi
}

local_ci_lease_start_child() {
    # A forked child creates its own session before publishing its PID.  This
    # avoids both a parent-side setpgid race and observing the child before the
    # process-group identity is established.
    command -v perl >/dev/null 2>&1 || {
        echo "local-ci: Perl is required to establish the command process group" >&2
        return 1
    }
    local_ci_lease_pid_file=$1
    local_ci_lease_release_fifo=$2
    shift 2
    perl -MPOSIX -e \
        'my ($pid_file, $release_fifo) = splice @ARGV, 0, 2; my $pid = fork(); die "fork: $!\n" unless defined $pid; if ($pid) { waitpid($pid, 0); my $status = $?; exit(($status & 127) ? 128 + ($status & 127) : ($status >> 8)); } POSIX::setsid() == -1 and die "setsid: $!\n"; open my $fh, ">", $pid_file or die "pid file: $!\n"; print $fh "$$\n"; close $fh; open my $release, "<", $release_fifo or die "release fifo: $!\n"; <$release>; close $release; exec @ARGV or die "exec: $!\n"' \
        -- "$local_ci_lease_pid_file" "$local_ci_lease_release_fifo" "$@"
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

local_ci_lease_lock_holder() {
    local_ci_lease_group_common_dir=$1
    local_ci_lease_group_working_directory=$2
    shift 2
    LOCAL_CI_LEASE_COMMON_DIR=$local_ci_lease_group_common_dir
    LOCAL_CI_LEASE_PATH="$LOCAL_CI_LEASE_COMMON_DIR/$LOCAL_CI_LEASE_NAME"
    LOCAL_CI_LEASE_LOCK="$LOCAL_CI_LEASE_COMMON_DIR/$LOCAL_CI_LEASE_LOCK_NAME"
    local_ci_lease_enter_lock_object || return 1
    exec lockf -k "$LOCAL_CI_LEASE_LOCK_FILE_NAME" sh -c '
        set -eu
        helper=$1
        common_dir=$2
        working_directory=$3
        shift 3
        . "$helper"
        mkdir "$LOCAL_CI_LEASE_STARTING_NAME"
        LOCAL_CI_LEASE_STARTING_IDENTITY=$(stat -f "%d:%i" "$LOCAL_CI_LEASE_STARTING_NAME")
        local_ci_lease_write_starting_marker
        # This seam pauses only after this shell owns lockf.  A killed starter
        # therefore cannot later acquire the descriptor after recovery.
        if [ -n "${DARK_FACTORY_LOCAL_CI_TEST_PAUSE_AFTER_LOCKF-}" ]; then
            read -r local_ci_lease_test_pause_token <"$DARK_FACTORY_LOCAL_CI_TEST_PAUSE_AFTER_LOCKF" || true
        fi
        local_ci_lease_remove_starting "$LOCAL_CI_LEASE_STARTING_IDENTITY" || exit 1
        CDPATH= cd -P -- "$working_directory"
        set +e
        local_ci_lease_holder "$common_dir" "$@"
        status=$?
        set -e
        local_ci_lease_release_owner
        local_ci_lease_remove_lock_object
        rm -f "$LOCAL_CI_LEASE_OWNER_RECORD"
        exit "$status"
    ' local-ci-lease-holder "$LOCAL_CI_LEASE_HELPER" "$LOCAL_CI_LEASE_COMMON_DIR" \
        "$local_ci_lease_group_working_directory" "$@"
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

    local_ci_lease_reported_wait=0
    local_ci_lease_acquire_lock_object || return 1
    local_ci_lease_working_directory=$(pwd -P)

    # The complete lock-holder tree lives in one owned process group.  The
    # descriptor still stays inherited until every descendant exits, while a
    # wrapper signal can now reach lockf, its holder shell, and the command
    # without following mutable parent-PID relationships.
    local_ci_lease_group_pid_file="$LOCAL_CI_LEASE_COMMON_DIR/.dark-factory-local-ci-group-$$"
    local_ci_lease_group_release_fifo="$LOCAL_CI_LEASE_COMMON_DIR/.dark-factory-local-ci-release-$$"
    rm -f "$local_ci_lease_group_pid_file"
    rm -f "$local_ci_lease_group_release_fifo"
    mkfifo "$local_ci_lease_group_release_fifo" || return 1
    local_ci_lease_start_child "$local_ci_lease_group_pid_file" \
        "$local_ci_lease_group_release_fifo" sh -c \
        '. "$1"; shift; local_ci_lease_lock_holder "$@"' \
        local-ci-lease-group "$LOCAL_CI_LEASE_HELPER" "$LOCAL_CI_LEASE_COMMON_DIR" \
        "$local_ci_lease_working_directory" "$@" &
    local_ci_lease_holder_wait_pid=$!
    local_ci_lease_group_attempts=0
    while [ ! -s "$local_ci_lease_group_pid_file" ]; do
        [ "$local_ci_lease_group_attempts" -lt 100 ] || {
            echo "local-ci: lock-holder process-group startup timed out" >&2
            kill "$local_ci_lease_holder_wait_pid" 2>/dev/null || true
            wait "$local_ci_lease_holder_wait_pid" 2>/dev/null || true
            rm -f "$local_ci_lease_group_pid_file" "$local_ci_lease_group_release_fifo"
            return 1
        }
        sleep 0.01
        local_ci_lease_group_attempts=$((local_ci_lease_group_attempts + 1))
    done
    local_ci_lease_holder_pid=$(cat "$local_ci_lease_group_pid_file")
    rm -f "$local_ci_lease_group_pid_file"
    local_ci_lease_identity_attempts=0
    local_ci_lease_holder_start=
    local_ci_lease_holder_pgid=
    while [ -z "$local_ci_lease_holder_start" ] || [ -z "$local_ci_lease_holder_pgid" ]; do
        [ "$local_ci_lease_identity_attempts" -lt 100 ] || break
        local_ci_lease_holder_start=$(/bin/ps -p "$local_ci_lease_holder_pid" -o lstart= 2>/dev/null | sed 's/^ *//')
        local_ci_lease_holder_pgid=$(/bin/ps -p "$local_ci_lease_holder_pid" -o pgid= 2>/dev/null | tr -d ' ')
        local_ci_lease_identity_attempts=$((local_ci_lease_identity_attempts + 1))
        [ -n "$local_ci_lease_holder_start" ] && [ -n "$local_ci_lease_holder_pgid" ] || sleep 0.01
    done

    local_ci_lease_holder_is_owned() {
        [ -n "$local_ci_lease_holder_start" ] || return 1
        local_ci_lease_current_start=$(/bin/ps -p "$local_ci_lease_holder_pid" -o lstart= 2>/dev/null | sed 's/^ *//')
        local_ci_lease_current_pgid=$(/bin/ps -p "$local_ci_lease_holder_pid" -o pgid= 2>/dev/null | tr -d ' ')
        [ "$local_ci_lease_current_start" = "$local_ci_lease_holder_start" ] \
            && [ "$local_ci_lease_current_pgid" = "$local_ci_lease_holder_pgid" ] \
            && [ "$local_ci_lease_current_pgid" = "$local_ci_lease_holder_pid" ]
    }
    local_ci_lease_signal_holder() {
        local_ci_lease_signal=$1
        if local_ci_lease_holder_is_owned; then
            /bin/kill -"$local_ci_lease_signal" -"$local_ci_lease_holder_pgid" 2>/dev/null || true
        else
            echo "local-ci: refusing to signal an unverified lock-holder process group" >&2
        fi
    }
    local_ci_lease_holder_is_owned || {
        echo "local-ci: lock holder did not enter its owned process group" >&2
        kill "$local_ci_lease_holder_wait_pid" 2>/dev/null || true
        wait "$local_ci_lease_holder_wait_pid" 2>/dev/null || true
        rm -f "$local_ci_lease_group_release_fifo"
        return 1
    }

    local_ci_lease_wrapper_cleanup() {
        local_ci_lease_status=$?
        trap - EXIT HUP INT TERM
        local_ci_lease_signal_holder 15
        wait "$local_ci_lease_holder_wait_pid" 2>/dev/null || true
        rm -f "$local_ci_lease_group_release_fifo"
        exit "$local_ci_lease_status"
    }
    local_ci_lease_wrapper_signal() {
        local_ci_lease_signal=$1
        trap - EXIT HUP INT TERM
        local_ci_lease_signal_holder "$local_ci_lease_signal"
        wait "$local_ci_lease_holder_wait_pid" 2>/dev/null || true
        rm -f "$local_ci_lease_group_release_fifo"
        exit $((128 + local_ci_lease_signal))
    }
    trap local_ci_lease_wrapper_cleanup EXIT
    trap 'local_ci_lease_wrapper_signal 1' HUP
    trap 'local_ci_lease_wrapper_signal 2' INT
    trap 'local_ci_lease_wrapper_signal 15' TERM

    printf 'go\n' >"$local_ci_lease_group_release_fifo"
    rm -f "$local_ci_lease_group_release_fifo"

    set +e
    wait "$local_ci_lease_holder_wait_pid"
    local_ci_lease_status=$?
    set -e
    trap - EXIT HUP INT TERM
    exit "$local_ci_lease_status"
}

# The helper is sourced by the wrapper and by the lock-holder shell.
LOCAL_CI_LEASE_HELPER=${LOCAL_CI_LEASE_HELPER:-$(CDPATH= cd -- "$(dirname "$0")" && pwd)/local-ci-lease.sh}
