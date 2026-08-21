#!/bin/sh
set -eu

# A short, provider-free contributor check for Ubuntu. The workspace tests
# exercise the deeper attempt/resource lifecycle; this script proves that a
# fresh source checkout can bring up the real binaries, materialize one exact
# `.git`-free Change, and complete one worker task through the public CLI and
# one-shot shell provider.

repo_root=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
target_root=${CARGO_TARGET_DIR:-"$repo_root/target"}
bin_dir="$target_root/debug"
factoryd="$bin_dir/factoryd"
factoryctl="$bin_dir/factoryctl"
factory_tui="$bin_dir/factory-tui"

for binary in "$factoryd" "$factoryctl" "$factory_tui"; do
    test -x "$binary" || {
        echo "missing executable: $binary (run cargo build --workspace first)" >&2
        exit 1
    }
done

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
scratch=$(CDPATH='' cd -- "$scratch" && pwd -P)
home="$scratch/home"
repo="$scratch/repo"
socket="$home/f.sock"
mkdir -p "$home" "$repo" "$scratch/user"
chmod 700 "$home" "$scratch/user"
# A contributor may invoke this from inside an attempt. Do not let that
# attempt identity leak into the throwaway smoke; the daemon supplies a fresh
# credential to its one-shot shell child.
unset DARK_FACTORY_AGENT DARK_FACTORY_PROJECT DARK_FACTORY_ATTEMPT_TOKEN_FILE \
    DARK_FACTORY_FACTORYCTL
export DARK_FACTORY_HOME="$home"
export DARK_FACTORY_SOCKET="$socket"
export HOME="$scratch/user"

daemon_pid=
run_id=
tracked_processes="$scratch/runner-processes"
provider_launches="$HOME/launches"

run_list() {
    "$factoryctl" --socket "$socket" run list \
        --project linux-smoke-project --limit 100 >"$scratch/runs.json"
}

discover_run() {
    run_list || return 1
    sed 's/},{/\n/g' "$scratch/runs.json" \
        | grep -E '"phase":"(admitted|running|finalizing)"' \
        | sed -n 's/.*"id":"\([^"]*\)".*/\1/p' \
        | tail -1
}

run_is_open() {
    run_list || return 1
    sed 's/},{/\n/g' "$scratch/runs.json" \
        | grep -F "\"id\":\"$run_id\"" \
        | grep -Eq '"phase":"(admitted|running|finalizing)"'
}

run_is_running() {
    run_list || return 1
    sed 's/},{/\n/g' "$scratch/runs.json" \
        | grep -F "\"id\":\"$run_id\"" \
        | grep -Eq '"phase":"running"'
}

wait_for_open_run() {
    attempt=0
    while test -z "$run_id"; do
        run_id=$(discover_run || true)
        attempt=$((attempt + 1))
        test -n "$run_id" || {
            test "$attempt" -lt 300 || {
                cat "$scratch/runs.json" >&2 2>/dev/null || true
                echo "shell-provider run did not appear" >&2
                return 1
            }
            sleep 0.1
        }
    done
}

wait_for_running_run() {
    wait_for_open_run
    attempt=0
    while ! run_is_running; do
        attempt=$((attempt + 1))
        test "$attempt" -lt 300 || {
            cat "$scratch/runs.json" >&2 2>/dev/null || true
            cat "$scratch/factoryd.log" >&2 2>/dev/null || true
            echo "shell-provider run did not become running" >&2
            return 1
        }
        sleep 0.1
    done
}

wait_for_provider_launch() {
    attempt=0
    while :; do
        launches=0
        test ! -f "$provider_launches" \
            || launches=$(wc -l <"$provider_launches" | tr -d '[:space:]')
        test "$launches" = 1 && return 0
        test "$launches" -lt 2 || {
            cat "$provider_launches" >&2
            echo "shell provider launched more than once before daemon crash" >&2
            return 1
        }
        attempt=$((attempt + 1))
        test "$attempt" -lt 300 || {
            cat "$provider_launches" >&2 2>/dev/null || true
            cat "$scratch/factoryd.log" >&2 2>/dev/null || true
            echo "shell provider did not record its exact launch" >&2
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
            echo "scratch runner descendant survived run finalization: $survivor" >&2
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

stop_owned_run() {
    test -n "$run_id" || return 0
    if run_is_open; then
        "$factoryctl" --socket "$socket" run stop \
            --project linux-smoke-project --run "$run_id" --grace-ms 1000 >/dev/null
    fi
    attempt=0
    while run_is_open; do
        attempt=$((attempt + 1))
        test "$attempt" -lt 100 || {
            echo "owned smoke run did not become terminal: $run_id" >&2
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
        if test -n "$run_id"; then
            snapshot_runner_processes || cleanup_status=1
            stop_owned_run || cleanup_status=1
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
    elif test -n "$run_id"; then
        echo "factoryd exited before the owned run was finalized" >&2
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

start_daemon() {
    "$factoryd" --socket "$socket" >"$scratch/factoryd.log" 2>&1 &
    daemon_pid=$!
    attempt=0
    while ! "$factoryctl" --socket "$socket" health >/dev/null 2>&1; do
        attempt=$((attempt + 1))
        test "$attempt" -lt 300 || {
            cat "$scratch/factoryd.log" >&2
            echo "factoryd did not become healthy" >&2
            return 1
        }
        sleep 0.1
    done
}

git -C "$repo" init -q -b main
git -C "$repo" config user.email linux-smoke@example.invalid
git -C "$repo" config user.name "Linux source smoke"
printf '%s\n' '# Linux source smoke' >"$repo/README.md"
git -C "$repo" add README.md
git -C "$repo" commit -q -m 'initialize Linux source smoke repository'

start_daemon

if socket_mode=$(stat -c '%a' "$socket" 2>/dev/null); then
    :
else
    socket_mode=$(stat -f '%Lp' "$socket")
fi
test "$socket_mode" = 600 || {
    echo "expected private socket mode 600, got $socket_mode" >&2
    exit 1
}

# `--version` is the non-interactive source launch check for the TUI.
"$factory_tui" --version >/dev/null

if ! "$factoryctl" project add \
    --id linux-smoke-project \
    --name 'Linux source smoke' \
    --root "$repo" >"$scratch/project-add.json"; then
    cat "$scratch/project-add.json" >&2
    exit 1
fi
if test "${DARK_FACTORY_SMOKE_FORCE_FAILURE:-0}" = 1; then
    shell_command='sleep 30'
else
    shell_command='test ! -e .git; ! git rev-parse --show-toplevel >/dev/null 2>&1; ! git worktree add ../x HEAD >/dev/null 2>&1; echo retained-mutation >>README.md; echo launch >>"$HOME/launches"; sleep 5; exec "$DARK_FACTORY_FACTORYCTL" task done --result done'
fi
if ! "$factoryctl" agent add \
    --id linux-smoke-agent \
    --project linux-smoke-project \
    --role worker \
    --provider shell \
    --model "$shell_command" >"$scratch/agent-add.json"; then
    cat "$scratch/agent-add.json" >&2
    exit 1
fi
"$factoryctl" task add \
    --id linux-smoke-task \
    --project linux-smoke-project \
    --agent linux-smoke-agent \
    --title 'Complete the Linux source smoke' \
    --body 'Use the deterministic shell provider and report success.' >/dev/null

if test "${DARK_FACTORY_SMOKE_FORCE_FAILURE:-0}" = 1; then
    wait_for_open_run
    echo "intentional Linux smoke interruption after run admission" >&2
    exit 23
fi

# Prove that a detached registered runner survives an abrupt daemon death and
# that the same durable attempt is recovered and finalized after restart.
wait_for_running_run
wait_for_provider_launch
snapshot_runner_processes
kill -KILL "$daemon_pid"
wait "$daemon_pid" 2>/dev/null || true
daemon_pid=
start_daemon

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
wait_for_tracked_processes
test "$(wc -l <"$provider_launches" | tr -d '[:space:]')" = 1 || {
    cat "$provider_launches" >&2 2>/dev/null || true
    echo "daemon restart replayed the provider launch" >&2
    exit 1
}
run_list
if ! test "$(grep -o '"id":"[^"]*"' "$scratch/runs.json" | wc -l | tr -d '[:space:]')" = 1 \
    || ! grep -Fq "\"id\":\"$run_id\"" "$scratch/runs.json" \
    || ! grep -Fq '"phase":"terminal"' "$scratch/runs.json" \
    || ! grep -Fq '"outcome":{"type":"succeeded"' "$scratch/runs.json"; then
    cat "$scratch/runs.json" >&2
    echo "daemon restart did not preserve one exact successful run" >&2
    exit 1
fi
snapshot_runner_processes
test ! -s "$tracked_processes" || {
    cat "$tracked_processes" >&2
    echo "terminal task retained a daemon child" >&2
    exit 1
}
test ! -d "$home/runs" || test -z "$(find "$home/runs" -mindepth 1 -print -quit)" || {
    find "$home/runs" -mindepth 1 -maxdepth 2 -print >&2
    echo "terminal task retained a runtime root" >&2
    exit 1
}

"$factoryctl" change list --project linux-smoke-project >"$scratch/changes.json"
grep -Fq '"phase":"available"' "$scratch/changes.json" || {
    cat "$scratch/changes.json" >&2
    echo "completed task did not retain one available Change" >&2
    exit 1
}
change_source=$(find "$home/changes" -mindepth 1 -maxdepth 1 -type d ! -name '.*' -print)
if ! test "$(printf '%s\n' "$change_source" | sed '/^$/d' | wc -l | tr -d '[:space:]')" = 1 \
    || test -e "$change_source/.git" \
    || ! grep -Fq 'retained-mutation' "$change_source/README.md"; then
    find "$home/changes" -mindepth 1 -maxdepth 2 -print >&2
    echo "retained Change source is missing, ambiguous, Git-linked, or lost provider edits" >&2
    exit 1
fi

change_id=$(sed -n 's/.*"changes":\[{"id":"\([^"]*\)".*/\1/p' "$scratch/changes.json")
change_revision=$(sed -n 's/.*"revision":\([0-9][0-9]*\).*/\1/p' "$scratch/changes.json")
test -n "$change_id" && test -n "$change_revision" || {
    cat "$scratch/changes.json" >&2
    echo "could not derive the typed Change identity and revision" >&2
    exit 1
}
"$factoryctl" change remove \
    --project linux-smoke-project \
    --change "$change_id" \
    --revision "$change_revision" >/dev/null
attempt=0
while ! "$factoryctl" change list --project linux-smoke-project \
    | grep -q '"phase":"removed"'; do
    attempt=$((attempt + 1))
    test "$attempt" -lt 300 || {
        "$factoryctl" change list --project linux-smoke-project >&2 || true
        cat "$scratch/factoryd.log" >&2
        echo "explicit Change removal did not converge" >&2
        exit 1
    }
    sleep 0.1
done
test ! -e "$change_source" || {
    find "$change_source" -mindepth 0 -maxdepth 2 -print >&2
    echo "removed Change retained its source directory" >&2
    exit 1
}

echo "Linux source smoke passed: exact .git-free Change, daemon crash/restart recovery, one-shot shell task, explicit removal, and resource teardown"
