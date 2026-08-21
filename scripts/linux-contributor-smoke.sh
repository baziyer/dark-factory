#!/bin/sh
set -eu

# A short, provider-free contributor check for Ubuntu. The workspace tests
# exercise the deeper attempt/resource lifecycle; this script proves that a
# fresh source checkout can bring up the real binaries, materialize one exact
# `.git`-free Change, and complete one worker task through the public CLI and
# one-shot shell provider.

repo_root=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
host_rustup_home=${RUSTUP_HOME:-${HOME:-}/.rustup}
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
export RUSTUP_HOME="$host_rustup_home"
preserve_failure=$(printenv DARK_FACTORY_SMOKE_PRESERVE_FAILURE || printf 0)

daemon_pid=
verifier_descendant=
tracked_processes="$scratch/runner-processes"
crash_processes="$scratch/crash-processes"

run_list() {
    "$factoryctl" --socket "$socket" run list \
        --project linux-smoke-project --limit 100 >"$scratch/runs.json"
}

discover_task_run() {
    task_id=$1
    run_list || return 1
    sed 's/},{/\n/g' "$scratch/runs.json" \
        | grep -F "\"task_id\":\"$task_id\"" \
        | grep -E '"phase":"(admitted|running|finalizing)"' \
        | sed -n 's/.*"id":"\([^"]*\)".*/\1/p' \
        | tail -1
}

run_has() {
    checked_run_id=$1
    pattern=$2
    run_list || return 1
    sed 's/},{/\n/g' "$scratch/runs.json" \
        | grep -F "\"id\":\"$checked_run_id\"" \
        | grep -Eq "$pattern"
}

change_record() {
    task_id=$1
    "$factoryctl" --socket "$socket" change list \
        --project linux-smoke-project >"$scratch/changes.json"
    sed 's/},{/\n/g' "$scratch/changes.json" \
        | grep -F "\"task_id\":\"$task_id\"" \
        | tail -1
}

wait_for_file() {
    path=$1
    label=$2
    attempt=0
    while ! test -e "$path"; do
        attempt=$((attempt + 1))
        test "$attempt" -lt 300 || {
            cat "$scratch/factoryd.log" >&2 2>/dev/null || true
            echo "$label did not appear" >&2
            return 1
        }
        sleep 0.1
    done
}

wait_for_task_run() {
    task_id=$1
    task_run_id=
    attempt=0
    while test -z "$task_run_id"; do
        task_run_id=$(discover_task_run "$task_id" || true)
        attempt=$((attempt + 1))
        test -n "$task_run_id" || {
            test "$attempt" -lt 300 || {
                cat "$scratch/runs.json" >&2 2>/dev/null || true
                echo "task $task_id did not acquire an open run" >&2
                return 1
            }
            sleep 0.1
        }
    done
    printf '%s\n' "$task_run_id"
}

wait_for_run_terminal() {
    checked_run_id=$1
    outcome=$2
    attempt=0
    while ! run_has "$checked_run_id" '"phase":"terminal"'; do
        attempt=$((attempt + 1))
        test "$attempt" -lt 300 || {
            cat "$scratch/runs.json" >&2 2>/dev/null || true
            cat "$scratch/factoryd.log" >&2 2>/dev/null || true
            echo "run $checked_run_id did not become terminal" >&2
            return 1
        }
        sleep 0.1
    done
    run_has "$checked_run_id" "\"outcome\":{\"type\":\"$outcome\"" || {
        cat "$scratch/runs.json" >&2
        echo "run $checked_run_id did not retain $outcome as its outcome" >&2
        return 1
    }
}

# Capture only descendants of this exact scratch daemon. The smoke never
# signals by process name or scans the operator's process tree.
snapshot_runner_processes() {
    process_file=$tracked_processes
    test "$#" -eq 0 || process_file=$1
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
    ' >"$process_file"
}

runner_record() {
    checked_run_id=$1
    snapshot_runner_processes
    grep -F -- "--run-id $checked_run_id " "$tracked_processes" \
        | grep -F 'factory-runner' \
        | head -1
}

provider_record() {
    checked_run_id=$1
    runner=$(runner_record "$checked_run_id")
    test -n "$runner" || return 1
    runner_pid=$(printf '%s\n' "$runner" | cut -f1)
    ps -axo pid=,ppid=,command= | awk -v parent="$runner_pid" '
        $2 == parent {
            pid = $1
            $1 = ""
            $2 = ""
            sub(/^[[:space:]]+/, "", $0)
            print pid "\t" $0
            exit
        }
    '
}

verifier_record() {
    snapshot_runner_processes
    grep -F -- '--rust-verify-worker ' "$tracked_processes" | head -1
}

verifier_descendant_record() {
    verifier_pid=$1
    ps -axo pid=,pgid=,command= | awk -v group="$verifier_pid" '
        $2 == group && $1 != group && index($0, "/bin/test-0000") {
            pid = $1
            $1 = ""
            $2 = ""
            sub(/^[[:space:]]+/, "", $0)
            print pid "\t" $0
            exit
        }
    '
}

wait_for_process_record() {
    kind=$1
    checked_run_id=$2
    attempt=0
    process_record=
    while test -z "$process_record"; do
        case "$kind" in
            runner) process_record=$(runner_record "$checked_run_id" || true) ;;
            provider) process_record=$(provider_record "$checked_run_id" || true) ;;
            *) echo "unknown process kind: $kind" >&2; return 1 ;;
        esac
        attempt=$((attempt + 1))
        test -n "$process_record" || {
            test "$attempt" -lt 300 || {
                cat "$tracked_processes" >&2 2>/dev/null || true
                cat "$scratch/factoryd.log" >&2 2>/dev/null || true
                echo "$kind process for run $checked_run_id did not appear" >&2
                return 1
            }
            sleep 0.1
        }
    done
    printf '%s\n' "$process_record"
}

wait_for_verifier_record() {
    attempt=0
    process_record=
    while test -z "$process_record"; do
        process_record=$(verifier_record || true)
        attempt=$((attempt + 1))
        test -n "$process_record" || {
            test "$attempt" -lt 300 || {
                cat "$tracked_processes" >&2 2>/dev/null || true
                cat "$scratch/factoryd.log" >&2 2>/dev/null || true
                echo "Rust verifier group leader did not appear" >&2
                return 1
            }
            sleep 0.1
        }
    done
    printf '%s\n' "$process_record"
}

wait_for_verifier_descendant() {
    verifier_pid=$1
    attempt=0
    process_record=
    while test -z "$process_record"; do
        process_record=$(verifier_descendant_record "$verifier_pid" || true)
        attempt=$((attempt + 1))
        test -n "$process_record" || {
            test "$attempt" -lt 300 || {
                ps -axo pid=,ppid=,pgid=,command= \
                    | awk -v group="$verifier_pid" '$3 == group' >&2
                echo "Rust verifier test descendant did not appear" >&2
                return 1
            }
            sleep 0.1
        }
    done
    printf '%s\n' "$process_record"
}

record_is_alive() {
    record=$1
    process_pid=$(printf '%s\n' "$record" | cut -f1)
    expected=$(printf '%s\n' "$record" | cut -f2-)
    current=$(ps -p "$process_pid" -o command= 2>/dev/null | sed 's/^[[:space:]]*//' || true)
    process_state=$(ps -p "$process_pid" -o stat= 2>/dev/null | sed 's/[[:space:]].*//' || true)
    test -n "$current" && test "$current" = "$expected" \
        && ! printf '%s\n' "$process_state" | grep -q '^Z'
}

wait_for_record_exit() {
    record=$1
    label=$2
    attempt=0
    while record_is_alive "$record"; do
        attempt=$((attempt + 1))
        test "$attempt" -lt 300 || {
            echo "$label survived its causal fixture bound" >&2
            return 1
        }
        sleep 0.1
    done
}

kill_exact_record() {
    record=$1
    label=$2
    process_pid=$(printf '%s\n' "$record" | cut -f1)
    expected=$(printf '%s\n' "$record" | cut -f2-)
    current=$(ps -p "$process_pid" -o command= 2>/dev/null | sed 's/^[[:space:]]*//' || true)
    test -n "$current" && test "$current" = "$expected" || {
        echo "$label identity changed before exact kill: pid $process_pid" >&2
        return 1
    }
    kill -KILL "$process_pid"
}

wait_for_tracked_processes() {
    process_file=$tracked_processes
    test "$#" -eq 0 || process_file=$1
    test -f "$process_file" || return 0
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
        done <"$process_file"
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

open_run_ids() {
    run_list || return 1
    sed 's/},{/\n/g' "$scratch/runs.json" \
        | grep -E '"phase":"(admitted|running|finalizing)"' \
        | sed -n 's/.*"id":"\([^"]*\)".*/\1/p'
}

stop_owned_runs() {
    owned_run_ids=$(open_run_ids || true)
    test -n "$owned_run_ids" || return 0
    for owned_run_id in $owned_run_ids; do
        "$factoryctl" --socket "$socket" run stop \
            --project linux-smoke-project --run "$owned_run_id" --grace-ms 1000 >/dev/null \
            || return 1
    done
    attempt=0
    while test -n "$(open_run_ids || true)"; do
        attempt=$((attempt + 1))
        test "$attempt" -lt 100 || {
            echo "owned smoke runs did not become terminal: $owned_run_ids" >&2
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
        if test -n "$(open_run_ids || true)"; then
            snapshot_runner_processes || cleanup_status=1
            stop_owned_runs || cleanup_status=1
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
    elif test -e "$home/factory.db"; then
        echo "factoryd exited before the smoke finished" >&2
        cleanup_status=1
    fi
    if test -e "$socket"; then
        echo "scratch socket survived daemon shutdown: $socket" >&2
        cleanup_status=1
    fi
    if test -n "$verifier_descendant"; then
        wait_for_record_exit "$verifier_descendant" "Rust verifier descendant" \
            || cleanup_status=1
    fi
    if test "$cleanup_status" -eq 0 \
        && { test "$status" -eq 0 \
            || test "$preserve_failure" != 1; }; then
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
mkdir -p "$repo/src"
cat >"$repo/Cargo.toml" <<'EOF'
[package]
name = "dark-factory-linux-smoke"
version = "0.0.0"
edition = "2024"
EOF
cat >"$repo/Cargo.lock" <<'EOF'
# This file is automatically @generated by Cargo.
version = 3

[[package]]
name = "dark-factory-linux-smoke"
version = "0.0.0"
EOF
cat >"$repo/revision.txt" <<'EOF'
A
EOF
cat >"$repo/src/lib.rs" <<EOF
#[cfg(test)]
mod tests {
    use std::{fs, io::Write as _, thread, time::Duration};

    #[test]
    fn immutable_prepared_binary_runs() {
        assert_eq!(fs::read_to_string("revision.txt").unwrap(), "A\n");
        writeln!(
            fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(r#"$scratch/verifier-launches"#)
                .unwrap(),
            "launch"
        )
        .unwrap();
        // This descendant must outlive factoryd's five-second effect reap
        // bound. The smoke kills only its group leader and proves no cache or
        // temporary-root cleanup races this still-running writer.
        thread::sleep(Duration::from_secs(20));
        assert_eq!(2 + 2, 4);
    }
}
EOF
cat >"$repo/smoke-agent.sh" <<'EOF'
#!/bin/sh
set -eu
launch=0
test ! -f "$HOME/launches" \
    || launch=$(wc -l <"$HOME/launches" | tr -d "[:space:]")
launch=$((launch + 1))
echo "$launch" >>"$HOME/launches"
case "$launch" in
    1) test ! -e .git; ! git rev-parse --show-toplevel >/dev/null 2>&1; ! git worktree add ../x HEAD >/dev/null 2>&1; echo retained-mutation >>README.md; sleep 5; "$DARK_FACTORY_FACTORYCTL" task done --result outcome-before-exit; exit 42 ;;
    2) : >"$HOME/provider-kill-ready"; exec sleep 30 ;;
    3) sleep 0.5; "$DARK_FACTORY_FACTORYCTL" task done --result retry; exit 43 ;;
    4) : >"$HOME/runner-kill-ready"; exec sleep 120 ;;
    5) : >"$HOME/cancel-ready"; exec sleep 30 ;;
    *) exit 99 ;;
esac
EOF
chmod 755 "$repo/smoke-agent.sh"
git -C "$repo" add README.md Cargo.toml Cargo.lock revision.txt src/lib.rs \
    smoke-agent.sh
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
"$factoryctl" project verification \
    --project linux-smoke-project \
    --mode rust-workspace-test >/dev/null
if test "${DARK_FACTORY_SMOKE_FORCE_FAILURE:-0}" = 1; then
    shell_command='sleep 30'
else
    shell_command='exec ./smoke-agent.sh'
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
if test "$shell_command" != 'sleep 30'; then
    "$factoryctl" agent add \
        --id linux-smoke-god \
        --project linux-smoke-project \
        --role orchestrator \
        --provider shell \
        --model ': >"$HOME/god-ready"; exec sleep 30' >/dev/null
fi
"$factoryctl" task add \
    --id linux-smoke-task \
    --project linux-smoke-project \
    --agent linux-smoke-agent \
    --title 'Complete the Linux source smoke' \
    --body 'Use the deterministic shell provider and report success.' >/dev/null

if test "${DARK_FACTORY_SMOKE_FORCE_FAILURE:-0}" = 1; then
    wait_for_task_run linux-smoke-task >/dev/null
    echo "intentional Linux smoke interruption after run admission" >&2
    exit 23
fi

# Success is durable before the provider's later exit 42. Kill the verifier
# group leader while its exact test descendant remains alive, then crash the
# daemon while that one effect and one God attempt are both active.
run_id=$(wait_for_task_run linux-smoke-task)
wait_for_file "$scratch/verifier-launches" "Rust verifier"
run_has "$run_id" '"phase":"finalizing".*"outcome":{"type":"succeeded"' || {
    cat "$scratch/runs.json" >&2
    echo "worker success request was not durable before provider exit" >&2
    exit 1
}
"$factoryctl" task add \
    --id linux-smoke-god \
    --project linux-smoke-project \
    --agent linux-smoke-god \
    --title 'Keep scheduling failure separate' \
    --body 'Exercise exact orchestrator cleanup.' >/dev/null
god_run=$(wait_for_task_run linux-smoke-god)
wait_for_file "$HOME/god-ready" "orchestrator provider"
sleep 0.2
god_provider=$(wait_for_process_record provider "$god_run")
verifier=$(wait_for_verifier_record)
verifier_pid=$(printf '%s\n' "$verifier" | cut -f1)
verifier_descendant=$(wait_for_verifier_descendant "$verifier_pid")
snapshot_runner_processes "$crash_processes"
change_root=$(find "$home/changes" -mindepth 1 -maxdepth 1 -type d -print -quit)
test -n "$change_root" || {
    echo "managed Change root was not found" >&2
    exit 1
}
printf 'B\n' >"$change_root/revision.txt"
kill_exact_record "$verifier" "Rust verifier group leader"
wait_for_pid_exit "$verifier_pid" "Rust verifier group leader"

# The old failure path waited five seconds, then measured the live cache and
# released the temporary root while this descendant was still writing. Hold
# the assertion beyond that bound: without the original leader fingerprint,
# the daemon must leave the completion check nonterminal and storage unknown.
temporary_root="$home/artifacts/tmp/$(printf '%s\n' "$run_id" | tr -d '-')"
attempt=0
while test "$attempt" -lt 65; do
    record_is_alive "$verifier_descendant" || {
        echo "Rust verifier descendant did not outlive the cleanup assertion" >&2
        exit 1
    }
    run_has "$run_id" '"phase":"finalizing"' || {
        cat "$scratch/runs.json" >&2
        echo "completion terminalized before its exact effect group was absent" >&2
        exit 1
    }
    "$factoryctl" storage status --json >"$scratch/storage-pending.json"
    grep -Fq '"complete":false' "$scratch/storage-pending.json" || {
        cat "$scratch/storage-pending.json" >&2
        echo "live verifier cache was measured before exact group release" >&2
        exit 1
    }
    test -d "$temporary_root" || {
        echo "verifier temporary root was released while its descendant lived" >&2
        exit 1
    }
    attempt=$((attempt + 1))
    sleep 0.1
done

kill -KILL "$daemon_pid"
wait "$daemon_pid" 2>/dev/null || true
daemon_pid=
start_daemon

record_is_alive "$verifier_descendant" || {
    echo "verifier descendant did not cross the daemon restart boundary" >&2
    exit 1
}
run_has "$run_id" '"phase":"finalizing"' || {
    cat "$scratch/runs.json" >&2
    echo "restart terminalized completion before exact group release" >&2
    exit 1
}
test -d "$temporary_root" || {
    echo "restart released verifier staging before exact group release" >&2
    exit 1
}

kill_exact_record "$god_provider" "orchestrator provider"
wait_for_run_terminal "$god_run" failed
wait_for_record_exit "$verifier_descendant" "Rust verifier descendant"
wait_for_run_terminal "$run_id" failed
run_has "$run_id" '"reason":"unverifiable"' \
    && run_has "$run_id" '"exit_code":42|"exit_signal":[1-9][0-9]*' || {
    cat "$scratch/runs.json" >&2
    echo "interrupted verifier did not fail closed after success and later provider exit" >&2
    exit 1
}
test "$(wc -l <"$scratch/verifier-launches" | tr -d '[:space:]')" = 1 || {
    cat "$scratch/verifier-launches" >&2
    echo "daemon recovery launched a second verifier against mutable source" >&2
    exit 1
}
wait_for_tracked_processes "$crash_processes"

# Only the first task is a verification subject.
"$factoryctl" project verification \
    --project linux-smoke-project --mode none >/dev/null

add_worker_task() {
    task_id=$1
    "$factoryctl" task add \
        --id "$task_id" \
        --project linux-smoke-project \
        --agent linux-smoke-agent \
        --title "$task_id" \
        --body 'Exercise the causal smoke boundary.' >/dev/null
}

change_identity() {
    record=$(change_record "$1")
    id=$(printf '%s\n' "$record" | sed -n 's/.*"id":"\([^"]*\)".*/\1/p')
    revision=$(printf '%s\n' "$record" | sed -n 's/.*"revision":\([0-9][0-9]*\).*/\1/p')
    test -n "$id" && test -n "$revision" || return 1
    printf '%s %s\n' "$id" "$revision"
}

# One exact provider kill also supplies the single failed retry case.
add_worker_task linux-smoke-provider-kill
provider_kill_run=$(wait_for_task_run linux-smoke-provider-kill)
wait_for_file "$HOME/provider-kill-ready" "provider-kill target"
provider=$(wait_for_process_record provider "$provider_kill_run")
kill_exact_record "$provider" "worker provider"
wait_for_run_terminal "$provider_kill_run" failed
run_has "$provider_kill_run" '"reason":"process"' || exit 1
wait_for_tracked_processes
set -- $(change_identity linux-smoke-provider-kill)
provider_change_id=$1
provider_change_revision=$2
"$factoryctl" task retry \
    --project linux-smoke-project --task linux-smoke-provider-kill >/dev/null
provider_retry_run=$(wait_for_task_run linux-smoke-provider-kill)
test "$provider_retry_run" != "$provider_kill_run" || exit 1
wait_for_run_terminal "$provider_retry_run" succeeded
set -- $(change_identity linux-smoke-provider-kill)
test "$1" = "$provider_change_id" && test "$2" -gt "$provider_change_revision" || {
    echo "retry did not retain and advance its exact Change" >&2
    exit 1
}

# Kill the exact runner and prove its provider group converges.
add_worker_task linux-smoke-runner-kill
runner_kill_run=$(wait_for_task_run linux-smoke-runner-kill)
wait_for_file "$HOME/runner-kill-ready" "runner-kill target"
runner=$(wait_for_process_record runner "$runner_kill_run")
kill_exact_record "$runner" "worker runner"
wait_for_run_terminal "$runner_kill_run" failed
run_has "$runner_kill_run" '"reason":"process"' || exit 1
wait_for_tracked_processes

# Cancel one active attempt through the public operator path.
add_worker_task linux-smoke-cancel
cancel_run=$(wait_for_task_run linux-smoke-cancel)
wait_for_file "$HOME/cancel-ready" "operator-cancel target"
snapshot_runner_processes
"$factoryctl" run stop \
    --project linux-smoke-project --run "$cancel_run" --grace-ms 1000 >/dev/null
wait_for_run_terminal "$cancel_run" cancelled
wait_for_tracked_processes

snapshot_runner_processes
test ! -s "$tracked_processes" || {
    cat "$tracked_processes" >&2
    echo "terminal causal smoke retained a daemon child" >&2
    exit 1
}
test ! -d "$home/runs" || test -z "$(find "$home/runs" -mindepth 1 -print -quit)" || {
    find "$home/runs" -mindepth 1 -maxdepth 2 -print >&2
    echo "terminal causal smoke retained a runtime root" >&2
    exit 1
}
test ! -d "$home/artifacts/tmp" \
    || test -z "$(find "$home/artifacts/tmp" -mindepth 1 -print -quit)" || {
    find "$home/artifacts/tmp" -mindepth 1 -maxdepth 3 -print >&2
    echo "terminal causal smoke retained verifier staging" >&2
    exit 1
}

echo "Linux source smoke passed: outcome-before-exit, fail-closed leader-loss verifier recovery, exact provider/runner/God death, retained-Change retry, active cancellation, and resource teardown"
