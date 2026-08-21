#!/bin/sh
set -eu

# Provider-free macOS proof: an external owner retains the daemon child, and
# no stored PID is used to signal a provider or runner.
test "$(uname -s)" = Darwin || {
    echo "macOS source smoke requires Darwin" >&2
    exit 2
}

repo_root=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
owner="$repo_root/scripts/macos-smoke-daemon-owner.pl"
host_rustup_home=${RUSTUP_HOME:-${HOME:-}/.rustup}
target_root=${CARGO_TARGET_DIR:-"$repo_root/target"}
factoryd="$target_root/debug/factoryd"
factoryctl="$target_root/debug/factoryctl"
cargo_bin_dir=$(dirname "$(command -v cargo)")
provider_path="$cargo_bin_dir:/usr/bin:/bin:/usr/sbin:/sbin"

for binary in "$factoryd" "$factoryctl" "$owner"; do
    test -x "$binary" || {
        echo "missing executable: $binary (run cargo build --workspace first)" >&2
        exit 1
    }
done
for provider in claude codex; do
    if PATH="$provider_path" command -v "$provider" >/dev/null 2>&1; then
        echo "$provider must be unavailable to the macOS source smoke" >&2
        exit 1
    fi
done
if test -n "${DARK_FACTORY_SMOKE_ROOT:-}"; then
    scratch=$DARK_FACTORY_SMOKE_ROOT
    test ! -e "$scratch" || {
        echo "smoke root already exists: $scratch" >&2
        exit 1
    }
    mkdir -p "$scratch"
else
    scratch=$(mktemp -d /tmp/dark-factory-macos-smoke.XXXXXX)
fi
scratch=$(CDPATH='' cd -- "$scratch" && pwd -P)
home="$scratch/home"
repo="$scratch/repo"
socket="$home/f.sock"
mkdir -p "$home" "$repo" "$scratch/user"
chmod 700 "$home" "$scratch/user"
unset DARK_FACTORY_AGENT DARK_FACTORY_PROJECT DARK_FACTORY_ATTEMPT_TOKEN_FILE \
    DARK_FACTORY_FACTORYCTL CODEX_HOME CLAUDE_CONFIG_DIR ANTHROPIC_API_KEY \
    OPENAI_API_KEY
export DARK_FACTORY_HOME="$home"
export DARK_FACTORY_SOCKET="$socket"
export HOME="$scratch/user"
export RUSTUP_HOME="$host_rustup_home"

project_id=macos-smoke-project
worker_id=macos-smoke-worker
god_id=macos-smoke-god
owner_sequence=0
owner_pid=
owner_state=
old_owner_pid=
old_owner_state=
worker_verifier_pid=
worker_verifier_base=
god_verifier_pid=
god_verifier_base=

wait_for_file() {
    path=$1
    label=$2
    limit=${3:-300}
    delay=${4:-0.1}
    attempt=0
    while ! test -e "$path"; do
        attempt=$((attempt + 1))
        test "$attempt" -lt "$limit" || {
            cat "$scratch/factoryd.log" >&2 2>/dev/null || true
            echo "$label did not appear" >&2
            return 1
        }
        sleep "$delay"
    done
}

wait_for_runner_exit() {
    runner_file=$1
    label=$2
    wait_for_file "$runner_file" "$label identity" || return 1
    runner_pid=$(cat "$runner_file")
    case "$runner_pid" in
        '' | *[!0-9]* | 0 | 1)
            echo "$label published an invalid runner PID" >&2
            return 1
            ;;
    esac
    attempt=0
    while kill -0 "$runner_pid" 2>/dev/null; do
        attempt=$((attempt + 1))
        test "$attempt" -lt 300 || {
            echo "$label $runner_pid did not disappear" >&2
            return 1
        }
        sleep 0.1
    done
}

run_list() {
    "$factoryctl" --socket "$socket" run list \
        --project "$project_id" --limit 100 >"$scratch/runs.json"
}

run_has() {
    checked_run_id=$1
    pattern=$2
    run_list || return 1
    sed 's/},{/\
/g' "$scratch/runs.json" \
        | grep -F "\"id\":\"$checked_run_id\"" \
        | grep -Eq "$pattern"
}

wait_for_task_run() {
    task_id=$1
    attempt=0
    while :; do
        run_list || true
        task_run_id=$(sed 's/},{/\
/g' "$scratch/runs.json" 2>/dev/null \
            | grep -F "\"task_id\":\"$task_id\"" \
            | grep -E '"phase":"(admitted|running|finalizing)"' \
            | sed -n 's/.*"id":"\([^"]*\)".*/\1/p' | tail -1 || true)
        test -z "$task_run_id" || {
            printf '%s\n' "$task_run_id"
            return 0
        }
        attempt=$((attempt + 1))
        test "$attempt" -lt 300 || {
            echo "task $task_id did not acquire an open run" >&2
            return 1
        }
        sleep 0.1
    done
}

wait_for_run_terminal() {
    checked_run_id=$1
    outcome=$2
    attempt=0
    while ! run_has "$checked_run_id" '"phase":"terminal"'; do
        attempt=$((attempt + 1))
        test "$attempt" -lt 300 || {
            cat "$scratch/runs.json" >&2 2>/dev/null || true
            echo "run $checked_run_id did not become terminal" >&2
            return 1
        }
        sleep 0.1
    done
    run_has "$checked_run_id" "\"outcome\":{\"type\":\"$outcome\"" || {
        cat "$scratch/runs.json" >&2
        echo "run $checked_run_id did not retain $outcome" >&2
        return 1
    }
}

start_daemon() {
    owner_sequence=$((owner_sequence + 1))
    owner_state="$scratch/daemon-owner-$owner_sequence"
    mkdir "$owner_state"
    daemon_program=$factoryd
    if test "${DARK_FACTORY_SMOKE_FAIL_RESTART:-0}" = 1 \
        && test "$owner_sequence" -eq 2; then
        daemon_program=/usr/bin/false
    fi
    PATH="$provider_path" /usr/bin/perl "$owner" "$owner_state" "$$" \
        "$HOME/reap-all" \
        "$daemon_program" --socket "$socket" >"$scratch/factoryd.log" 2>&1 &
    owner_pid=$!
    wait_for_file "$owner_state/owned-pid" 'daemon ownership record'
    if test "$daemon_program" != "$factoryd"; then
        echo 'intentional macOS smoke restart failure' >&2
        return 71
    fi
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

finish_owner() {
    finished_pid=$1
    finished_state=$2
    command=$3
    marker=$4
    test -n "$finished_pid" || return 0
    : >"$finished_state/$command"
    wait_for_file "$finished_state/$marker" 'owned daemon reap' 250 0.02 \
        || true
    if test -e "$finished_state/$marker" \
        || test -e "$finished_state/reaped"; then
        if ! test -e "$finished_state/owner-waited"; then
            if ! wait "$finished_pid"; then
                : >"$finished_state/owner-wait-failed"
            fi
            : >"$finished_state/owner-waited"
        fi
        test ! -e "$finished_state/owner-wait-failed" || return 1
        : >"$finished_state/owner-proved"
        return 0
    fi
    return 1
}

crash_daemon() {
    old_owner_pid=$owner_pid
    old_owner_state=$owner_state
    : >"$old_owner_state/crash"
    wait_for_file "$old_owner_state/crashed" 'retained daemon crash'
    attempt=0
    while "$factoryctl" --socket "$socket" health >/dev/null 2>&1; do
        attempt=$((attempt + 1))
        test "$attempt" -lt 100 || return 1
        sleep 0.1
    done
    owner_pid=
    owner_state=
}

start_verifier() {
    verifier_base="$HOME/$1-$2"
    mkfifo "$verifier_base.fifo"
    /usr/bin/perl "$owner" --observe-fifo \
        "$verifier_base.fifo" "$verifier_base.closed" 40 &
    verifier_pid=$!
}

finish_verifier() {
    verified_pid=$1
    verified_base=$2
    label=$3
    verified_done="$verified_base.closed"
    wait_for_file "$verified_done" "$label provider close proof" || true
    if ! test -e "$verified_done.waited"; then
        if ! wait "$verified_pid"; then
            : >"$verified_done.wait-failed"
        fi
        : >"$verified_done.waited"
    fi
    if ! test -e "$verified_done" \
        || test -e "$verified_done.wait-failed" \
        || ! wait_for_runner_exit "$verified_base.runner" "$label runner"; then
        return 1
    fi
    rm -f "$verified_base.fifo" "$verified_done" "$verified_done.waited" \
        "$verified_base.runner"
}

start_worker_verifier() {
    start_verifier worker "$1"
    worker_verifier_base=$verifier_base
    worker_verifier_pid=$verifier_pid
}

finish_worker_verifier() {
    test -n "$worker_verifier_pid" || return 0
    finish_verifier "$worker_verifier_pid" "$worker_verifier_base" \
        worker || return 1
    worker_verifier_pid=
    worker_verifier_base=
}

start_god_verifier() {
    start_verifier god 1
    god_verifier_base=$verifier_base
    god_verifier_pid=$verifier_pid
}

finish_god_verifier() {
    test -n "$god_verifier_pid" || return 0
    finish_verifier "$god_verifier_pid" "$god_verifier_base" \
        orchestrator || return 1
    god_verifier_pid=
    god_verifier_base=
}

cleanup() {
    status=$?
    trap - EXIT HUP INT TERM
    : >"$HOME/reap-all"
    cleanup_status=0
    test -z "$old_owner_pid" \
        || finish_owner "$old_owner_pid" "$old_owner_state" reap reaped \
        || cleanup_status=1
    test -z "$owner_pid" \
        || finish_owner "$owner_pid" "$owner_state" stop stopped \
        || cleanup_status=1
    finish_worker_verifier || cleanup_status=1
    finish_god_verifier || cleanup_status=1
    if test "$cleanup_status" -eq 0; then
        rm -rf -- "$scratch"
    else
        echo "preserving scratch home for cleanup diagnosis: $scratch" >&2
        status=1
    fi
    exit "$status"
}
trap cleanup EXIT HUP INT TERM

add_worker_task() {
    task_id=$1
    "$factoryctl" task add \
        --id "$task_id" --project "$project_id" --agent "$worker_id" \
        --title "$task_id" --body 'Exercise a macOS causal boundary.' >/dev/null
}

change_identity() {
    task_id=$1
    "$factoryctl" --socket "$socket" change list \
        --project "$project_id" >"$scratch/changes.json"
    record=$(sed 's/},{/\
/g' "$scratch/changes.json" \
        | grep -F "\"task_id\":\"$task_id\"" | tail -1)
    id=$(printf '%s\n' "$record" | sed -n 's/.*"id":"\([^"]*\)".*/\1/p')
    revision=$(printf '%s\n' "$record" \
        | sed -n 's/.*"revision":\([0-9][0-9]*\).*/\1/p')
    test -n "$id" && test -n "$revision" || return 1
    printf '%s %s\n' "$id" "$revision"
}

git -C "$repo" init -q -b main
git -C "$repo" config user.email macos-smoke@example.invalid
git -C "$repo" config user.name 'macOS source smoke'
printf '%s\n' '# macOS source smoke' >"$repo/README.md"
cat >"$repo/smoke-agent.sh" <<'EOF'
#!/bin/sh
set -eu
launch=0
test ! -f "$HOME/worker-launches" \
    || launch=$(wc -l <"$HOME/worker-launches" | tr -d '[:space:]')
launch=$((launch + 1))
echo "$launch" >>"$HOME/worker-launches"
printf '%s\n' "$PPID" >"$HOME/worker-$launch.runner"
exec 9>"$HOME/worker-$launch.fifo"
test ! -e .git
! git rev-parse --show-toplevel >/dev/null 2>&1
: >"$HOME/worker-$launch.ready"

reap_or_wait() {
    marker=$1
    while ! test -e "$marker"; do
        if test -e "$HOME/prove-survival" && test "$launch" = 1; then
            : >"$HOME/worker-survived"
        fi
        if test -e "$HOME/reap-all"; then
            kill -TERM "$PPID"
            while :; do sleep 1; done
        fi
        sleep 0.05
    done
}

case "$launch" in
    1) echo daemon-crash-mutation >>README.md; reap_or_wait "$HOME/never" ;;
    2) reap_or_wait "$HOME/provider-exit"; exit 47 ;;
    3) echo retry-mutation >>README.md; "$DARK_FACTORY_FACTORYCTL" task done --result retry-ok; exit 43 ;;
    4) reap_or_wait "$HOME/runner-exit"; : >"$HOME/runner-fault-sent"; kill -TERM "$PPID"; while :; do sleep 1; done ;;
    5) reap_or_wait "$HOME/never" ;;
    *) exit 99 ;;
esac
EOF
cat >"$repo/god-agent.sh" <<'EOF'
#!/bin/sh
set -eu
echo launch >>"$HOME/god-launches"
printf '%s\n' "$PPID" >"$HOME/god-1.runner"
exec 9>"$HOME/god-1.fifo"
: >"$HOME/god-ready"
while ! test -e "$HOME/god-exit"; do
    test ! -e "$HOME/prove-survival" || : >"$HOME/god-survived"
    if test -e "$HOME/reap-all"; then
        kill -TERM "$PPID"
        while :; do sleep 1; done
    fi
    sleep 0.05
done
exit 49
EOF
chmod 755 "$repo/smoke-agent.sh" "$repo/god-agent.sh"
git -C "$repo" add README.md smoke-agent.sh god-agent.sh
git -C "$repo" commit -q -m 'initialize macOS source smoke repository'

start_daemon
test "$(stat -f '%Lp' "$socket")" = 600 || exit 1
"$factoryctl" project add --id "$project_id" \
    --name 'macOS source smoke' --root "$repo" >/dev/null
"$factoryctl" agent add --id "$worker_id" --project "$project_id" \
    --role worker --provider shell --model 'exec ./smoke-agent.sh' >/dev/null
god_model="exec $repo/god-agent.sh"
"$factoryctl" agent add --id "$god_id" --project "$project_id" \
    --role orchestrator --provider shell --model "$god_model" >/dev/null

start_worker_verifier 1
add_worker_task macos-smoke-daemon-crash
worker_crash_run=$(wait_for_task_run macos-smoke-daemon-crash)
wait_for_file "$HOME/worker-1.ready" 'worker provider'
start_god_verifier
"$factoryctl" task add --id macos-smoke-god-task \
    --project "$project_id" --agent "$god_id" --title 'Orchestrator recovery' \
    --body 'Exercise exact orchestrator recovery.' >/dev/null
god_run=$(wait_for_task_run macos-smoke-god-task)
wait_for_file "$HOME/god-ready" 'orchestrator provider'

crash_daemon
: >"$HOME/prove-survival"
wait_for_file "$HOME/worker-survived" 'worker survival proof'
wait_for_file "$HOME/god-survived" 'orchestrator survival proof'
test "$(wc -l <"$HOME/worker-launches" | tr -d '[:space:]')" = 1 || exit 1
test "$(wc -l <"$HOME/god-launches" | tr -d '[:space:]')" = 1 || exit 1
if test "${DARK_FACTORY_SMOKE_FAIL_DAEMON_DOWN:-0}" = 1; then
    echo 'intentional macOS smoke failure while daemon is down' >&2
    exit 23
fi

start_daemon
run_has "$worker_crash_run" '"phase":"running"' || exit 1
run_has "$god_run" '"phase":"running"' || exit 1
test "$(wc -l <"$HOME/worker-launches" | tr -d '[:space:]')" = 1 || exit 1
test "$(wc -l <"$HOME/god-launches" | tr -d '[:space:]')" = 1 || exit 1
"$factoryctl" run stop --project "$project_id" \
    --run "$worker_crash_run" --grace-ms 1000 >/dev/null
wait_for_run_terminal "$worker_crash_run" cancelled
finish_worker_verifier
: >"$HOME/god-exit"
wait_for_run_terminal "$god_run" failed
run_has "$god_run" '"reason":"process"' || exit 1
finish_god_verifier
test "$(wc -l <"$HOME/god-launches" | tr -d '[:space:]')" = 1 || exit 1
finish_owner "$old_owner_pid" "$old_owner_state" reap reaped
old_owner_pid=
old_owner_state=

start_worker_verifier 2
add_worker_task macos-smoke-provider-exit
provider_run=$(wait_for_task_run macos-smoke-provider-exit)
wait_for_file "$HOME/worker-2.ready" 'provider-exit target'
: >"$HOME/provider-exit"
wait_for_run_terminal "$provider_run" failed
run_has "$provider_run" '"reason":"process"' || exit 1
finish_worker_verifier
change_record=$(change_identity macos-smoke-provider-exit)
change_id=${change_record% *}
change_revision=${change_record#* }

start_worker_verifier 3
"$factoryctl" task retry --project "$project_id" \
    --task macos-smoke-provider-exit >/dev/null
retry_run=$(wait_for_task_run macos-smoke-provider-exit)
test "$retry_run" != "$provider_run" || exit 1
wait_for_run_terminal "$retry_run" succeeded
finish_worker_verifier
new_change_record=$(change_identity macos-smoke-provider-exit)
new_change_id=${new_change_record% *}
new_change_revision=${new_change_record#* }
test "$new_change_id" = "$change_id" \
    && test "$new_change_revision" -gt "$change_revision" || exit 1

start_worker_verifier 4
add_worker_task macos-smoke-runner-exit
runner_run=$(wait_for_task_run macos-smoke-runner-exit)
wait_for_file "$HOME/worker-4.ready" 'runner-exit target'
: >"$HOME/runner-exit"
wait_for_file "$HOME/runner-fault-sent" 'runner self-termination fault'
wait_for_run_terminal "$runner_run" failed
run_has "$runner_run" '"reason":"process"' || exit 1
finish_worker_verifier

start_worker_verifier 5
add_worker_task macos-smoke-cancel
cancel_run=$(wait_for_task_run macos-smoke-cancel)
wait_for_file "$HOME/worker-5.ready" 'cancellation target'
"$factoryctl" run stop --project "$project_id" \
    --run "$cancel_run" --grace-ms 1000 >/dev/null
wait_for_run_terminal "$cancel_run" cancelled
finish_worker_verifier
test "$(wc -l <"$HOME/worker-launches" | tr -d '[:space:]')" = 5 || exit 1
test "$(wc -l <"$HOME/god-launches" | tr -d '[:space:]')" = 1 || exit 1

test ! -d "$home/runs" \
    || test -z "$(find "$home/runs" -mindepth 1 -print -quit)" || exit 1
finish_owner "$owner_pid" "$owner_state" stop stopped
owner_pid=
owner_state=
test ! -e "$socket" || exit 1

echo 'macOS source smoke passed: owned daemon crash, surviving worker/God without replay, self-exit provider/runner faults, retained-Change retry, cancellation, and exact teardown'
