#!/bin/sh
set -eu

# A short, provider-free contributor check for Ubuntu. The workspace tests
# exercise the deeper PTY/restart/attach lifecycle; this script proves that a
# fresh source checkout can bring up the real binaries and complete one task
# through the public CLI and shell-provider boundary.

repo_root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
target_root=${CARGO_TARGET_DIR:-"$repo_root/target"}
bin_dir="$target_root/debug"
factoryd="$bin_dir/factoryd"
factoryctl="$bin_dir/factoryctl"
factory_tui="$bin_dir/factory-tui"
fixture="$repo_root/crates/factoryd/tests/fixtures/shell-agent.sh"

for binary in "$factoryd" "$factoryctl" "$factory_tui"; do
    test -x "$binary" || {
        echo "missing executable: $binary (run cargo build --workspace first)" >&2
        exit 1
    }
done
test -x "$fixture"

if test -n "${DARK_FACTORY_SMOKE_ROOT:-}"; then
    scratch=$DARK_FACTORY_SMOKE_ROOT
    test ! -e "$scratch" || {
        echo "smoke root already exists: $scratch" >&2
        exit 1
    }
    mkdir -p "$scratch"
else
    scratch=$(mktemp -d /tmp/dark-factory-linux-smoke.XXXXXX)
fi
home="$scratch/home"
repo="$scratch/repo"
socket="$home/f.sock"
mkdir -p "$home" "$repo" "$scratch/user"
chmod 700 "$home" "$scratch/user"
# A contributor may invoke this from inside an agent session. Do not let that
# session's authenticated identity, outbox, or trusted helper leak into the
# throwaway smoke; the daemon will supply fresh values to its shell session.
unset DARK_FACTORY_AGENT DARK_FACTORY_PROJECT DARK_FACTORY_SESSION_TOKEN_FILE \
    DARK_FACTORY_AGENT_DIR DARK_FACTORY_FACTORYCTL DARK_FACTORY_FORCE_OUTBOX
export DARK_FACTORY_HOME="$home"
export DARK_FACTORY_SOCKET="$socket"
export HOME="$scratch/user"

daemon_pid=
session_id=
tracked_processes="$scratch/runner-processes"

session_list() {
    "$factoryctl" --socket "$socket" session list \
        --project linux-smoke-project --limit 100 >"$scratch/sessions.json"
}

discover_session() {
    session_list || return 1
    sed 's/},{/\n/g' "$scratch/sessions.json" \
        | grep -E '"state":"(starting|idle|working|waiting_for_input)"' \
        | sed -n 's/.*"id":"\([^"]*\)".*/\1/p' \
        | tail -1
}

session_is_live() {
    session_list || return 1
    sed 's/},{/\n/g' "$scratch/sessions.json" \
        | grep -F "\"id\":\"$session_id\"" \
        | grep -Eq '"state":"(starting|idle|working|waiting_for_input)"'
}

session_is_ready() {
    session_list || return 1
    sed 's/},{/\n/g' "$scratch/sessions.json" \
        | grep -F "\"id\":\"$session_id\"" \
        | grep -Eq '"state":"(idle|working|waiting_for_input)"'
}

wait_for_live_session() {
    attempt=0
    while test -z "$session_id"; do
        session_id=$(discover_session || true)
        attempt=$((attempt + 1))
        test -n "$session_id" || {
            test "$attempt" -lt 300 || {
                cat "$scratch/sessions.json" >&2 2>/dev/null || true
                echo "shell-provider session did not appear" >&2
                return 1
            }
            sleep 0.1
        }
    done
    attempt=0
    while ! session_is_live; do
        attempt=$((attempt + 1))
        test "$attempt" -lt 300 || {
            cat "$scratch/sessions.json" >&2 2>/dev/null || true
            echo "shell-provider session was not live" >&2
            return 1
        }
        sleep 0.1
    done
}

wait_for_ready_session() {
    wait_for_live_session
    attempt=0
    while ! session_is_ready; do
        attempt=$((attempt + 1))
        test "$attempt" -lt 300 || {
            cat "$scratch/sessions.json" >&2 2>/dev/null || true
            echo "shell-provider session did not become ready" >&2
            return 1
        }
        sleep 0.1
    done
}

# Capture only descendants of this exact scratch daemon. The smoke never
# signals by process name or scans the operator's process tree.
snapshot_runner_processes() {
    ps -axo pid=,ppid=,command= | awk -v root="$daemon_pid" '
        {
            pid = $1
            ppid = $2
            $1 = ""
            $2 = ""
            sub(/^[[:space:]]+/, "", $0)
            parent[pid] = ppid
            command[pid] = $0
        }
        function walk(parent_pid, child) {
            for (child in parent) {
                if (parent[child] == parent_pid) {
                    print child "\t" command[child]
                    walk(child)
                }
            }
        }
        END { walk(root) }
    ' >"$tracked_processes"
}

wait_for_tracked_processes() {
    test -f "$tracked_processes" || return 0
    attempt=0
    while :; do
        survivor=
        while IFS="$(printf '\t')" read -r pid expected; do
            test -n "$pid" || continue
            current=$(ps -p "$pid" -o command= 2>/dev/null | sed 's/^[[:space:]]*//' || true)
            if test -n "$current" && test "$current" = "$expected"; then
                survivor="$pid $expected"
                break
            fi
        done <"$tracked_processes"
        test -z "$survivor" && return 0
        attempt=$((attempt + 1))
        test "$attempt" -lt 100 || {
            echo "scratch runner descendant survived session stop: $survivor" >&2
            return 1
        }
        sleep 0.1
    done
}

wait_for_pid_exit() {
    pid=$1
    label=$2
    attempt=0
    while kill -0 "$pid" 2>/dev/null; do
        case "$(ps -p "$pid" -o stat= 2>/dev/null | sed 's/[[:space:]].*//')" in
            Z*) return 0 ;;
        esac
        attempt=$((attempt + 1))
        test "$attempt" -lt 100 || {
            echo "$label survived bounded shutdown: pid $pid" >&2
            return 1
        }
        sleep 0.1
    done
    return 0
}

stop_owned_session() {
    test -n "$session_id" || return 0
    if session_is_live; then
        "$factoryctl" --socket "$socket" session stop \
            --project linux-smoke-project --session "$session_id" --grace-ms 1000 >/dev/null
    fi
    attempt=0
    while session_is_live; do
        attempt=$((attempt + 1))
        test "$attempt" -lt 100 || {
            echo "owned smoke session did not close: $session_id" >&2
            return 1
        }
        sleep 0.1
    done
}

cleanup() {
    status=$?
    trap - EXIT HUP INT TERM
    cleanup_status=0
    if test -n "$daemon_pid" && kill -0 "$daemon_pid" 2>/dev/null; then
        if test -n "$session_id"; then
            snapshot_runner_processes || cleanup_status=1
            stop_owned_session || cleanup_status=1
            wait_for_tracked_processes || cleanup_status=1
            snapshot_runner_processes || cleanup_status=1
            test ! -s "$tracked_processes" || {
                echo "scratch daemon still owns a runner descendant after stop" >&2
                cleanup_status=1
            }
        fi
        kill -TERM "$daemon_pid" 2>/dev/null || cleanup_status=1
        wait_for_pid_exit "$daemon_pid" factoryd || cleanup_status=1
        wait "$daemon_pid" 2>/dev/null || true
    elif test -n "$session_id"; then
        echo "factoryd exited before the owned session was stopped" >&2
        cleanup_status=1
    fi
    if test -e "$socket"; then
        echo "scratch socket survived daemon shutdown: $socket" >&2
        cleanup_status=1
    fi
    if test "$cleanup_status" -eq 0; then
        rm -rf "$scratch"
        test ! -e "$scratch" || cleanup_status=1
    else
        echo "preserving scratch home for cleanup diagnosis: $scratch" >&2
    fi
    test "$cleanup_status" -eq 0 || status=1
    exit "$status"
}
trap cleanup EXIT HUP INT TERM

git -C "$repo" init -q -b main
git -C "$repo" config user.email linux-smoke@example.invalid
git -C "$repo" config user.name "Linux source smoke"
printf '%s\n' '# Linux source smoke' >"$repo/README.md"
git -C "$repo" add README.md
git -C "$repo" commit -q -m 'initialize Linux source smoke repository'

"$factoryd" --socket "$socket" >"$scratch/factoryd.log" 2>&1 &
daemon_pid=$!

attempt=0
while ! "$factoryctl" --socket "$socket" health >/dev/null 2>&1; do
    attempt=$((attempt + 1))
    test "$attempt" -lt 300 || {
        cat "$scratch/factoryd.log" >&2
        echo "factoryd did not become healthy" >&2
        exit 1
    }
    sleep 0.1
done

socket_mode=$(stat -c '%a' "$socket")
test "$socket_mode" = 600 || {
    echo "expected private socket mode 600, got $socket_mode" >&2
    exit 1
}

# `--version` is the non-interactive source launch check for the TUI. The
# interactive PTY/attach path is covered by the deterministic sessions suite.
"$factory_tui" --version >/dev/null

"$factoryctl" project add \
    --id linux-smoke-project \
    --name 'Linux source smoke' \
    --root "$repo" >/dev/null
"$factoryctl" agent add \
    --id linux-smoke-agent \
    --project linux-smoke-project \
    --role worker \
    --provider shell \
    --model "$fixture" >/dev/null
"$factoryctl" task add \
    --id linux-smoke-task \
    --project linux-smoke-project \
    --agent linux-smoke-agent \
    --title 'Complete the Linux source smoke' \
    --body 'Use the deterministic shell provider and report success.' >/dev/null

if test "${DARK_FACTORY_SMOKE_FORCE_FAILURE:-0}" = 1; then
    wait_for_ready_session
    echo "intentional Linux smoke interruption after session admission" >&2
    exit 23
fi

attempt=0
while ! "$factoryctl" task get \
    --project linux-smoke-project \
    --task linux-smoke-task 2>/dev/null | grep -q '"status":"succeeded"'; do
    attempt=$((attempt + 1))
    test "$attempt" -lt 300 || {
        "$factoryctl" status >&2 || true
        cat "$scratch/factoryd.log" >&2
        echo "shell-provider task did not complete" >&2
        exit 1
    }
    sleep 0.1
done
wait_for_live_session

echo "Linux source smoke passed: private socket, factoryd health, factory-tui, shell task, and owned session teardown"
