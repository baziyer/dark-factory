#!/bin/sh
set -eu

# The fixtures use throwaway repositories with independent common directories;
# they must not inherit the production gate's owner marker from local-ci.sh.
unset DARK_FACTORY_LOCAL_CI_LEASE_HELD

repository_root=$(CDPATH= cd -- "$(dirname "$0")/.." && pwd)
temporary=$(mktemp -d "${TMPDIR:-/tmp}/dark-factory-local-ci-lease-test.XXXXXX")
first="$temporary/first"
second="$temporary/second"
background_pids=
wait_timeout_seconds=5

process_parent() {
    ps -p "$1" -o ppid= 2>/dev/null | tr -d ' '
}

process_start() {
    ps -p "$1" -o lstart= 2>/dev/null | sed 's/^ *//'
}

process_command() {
    ps -p "$1" -o command= 2>/dev/null | sed 's/^ *//'
}

process_identity_matches() {
    local identity_pid=$1
    local identity_parent=$2
    local identity_start=$3
    local identity_command=$4
    [ "$(process_parent "$identity_pid")" = "$identity_parent" ] \
        && [ "$(process_start "$identity_pid")" = "$identity_start" ] \
        && [ "$(process_command "$identity_pid")" = "$identity_command" ]
}

kill_tree() {
    tree_pid=$1
    tree_parent=${2-}
    tree_start=${3-}
    tree_command=${4-}
    tree_signal=${5-KILL}
    if [ -z "$tree_start" ]; then
        tree_parent=$(process_parent "$tree_pid")
        tree_start=$(process_start "$tree_pid")
        tree_command=$(process_command "$tree_pid")
    fi
    [ -n "$tree_start" ] && [ -n "$tree_command" ] || return 1
    process_identity_matches "$tree_pid" "$tree_parent" "$tree_start" "$tree_command" || return 1

    tree_directory="$temporary/.kill-tree-$tree_pid"
    rm -rf "$tree_directory"
    mkdir "$tree_directory"
    tree_nodes="$tree_directory/nodes"
    printf '%s\n' "$tree_pid" >"$tree_nodes"
    printf '%s\n' "$tree_parent" >"$tree_directory/$tree_pid.parent"
    printf '%s\n' "$tree_start" >"$tree_directory/$tree_pid.start"
    printf '%s\n' "$tree_command" >"$tree_directory/$tree_pid.command"
    printf '0\n' >"$tree_directory/$tree_pid.depth"
    tree_queue=$tree_pid
    tree_max_depth=0
    while [ -n "$tree_queue" ]; do
        tree_next=
        for tree_current in $tree_queue; do
            tree_current_depth=$(cat "$tree_directory/$tree_current.depth")
            for child in $(pgrep -P "$tree_current" 2>/dev/null || true); do
                [ ! -e "$tree_directory/$child.start" ] || continue
                child_parent=$(process_parent "$child")
                child_start=$(process_start "$child")
                child_command=$(process_command "$child")
                [ "$child_parent" = "$tree_current" ] || {
                    rm -rf "$tree_directory"
                    return 1
                }
                printf '%s\n' "$child" >>"$tree_nodes"
                printf '%s\n' "$child_parent" >"$tree_directory/$child.parent"
                printf '%s\n' "$child_start" >"$tree_directory/$child.start"
                printf '%s\n' "$child_command" >"$tree_directory/$child.command"
                child_depth=$((tree_current_depth + 1))
                printf '%s\n' "$child_depth" >"$tree_directory/$child.depth"
                tree_next="$tree_next $child"
                [ "$child_depth" -le "$tree_max_depth" ] || tree_max_depth=$child_depth
            done
        done
        tree_queue=$tree_next
    done

    tree_depth=$tree_max_depth
    while [ "$tree_depth" -ge 0 ]; do
        while IFS= read -r tree_current; do
            [ "$(cat "$tree_directory/$tree_current.depth")" -eq "$tree_depth" ] || continue
            tree_current_parent=$(cat "$tree_directory/$tree_current.parent")
            tree_current_start=$(cat "$tree_directory/$tree_current.start")
            tree_current_command=$(cat "$tree_directory/$tree_current.command")
            [ -n "$(process_start "$tree_current")" ] || continue
            process_identity_matches "$tree_current" "$tree_current_parent" \
                "$tree_current_start" "$tree_current_command" || {
                rm -rf "$tree_directory"
                return 1
            }
            kill -"$tree_signal" "$tree_current" 2>/dev/null || true
        done <"$tree_nodes"
        tree_depth=$((tree_depth - 1))
    done
    rm -rf "$tree_directory"
}

wait_bounded() {
    wait_phase=$1
    wait_pid=$2
    wait_timed_out=0
    wait_timer_pid=
    wait_alarm() {
        wait_timed_out=1
    }
    trap wait_alarm 14
    ( sleep "$wait_timeout_seconds"; kill -14 "$$" ) &
    wait_timer_pid=$!
    if wait "$wait_pid"; then
        wait_status=0
    else
        wait_status=$?
    fi
    trap - 14
    kill "$wait_timer_pid" 2>/dev/null || true
    wait "$wait_timer_pid" 2>/dev/null || true
    if [ "$wait_timed_out" -ne 0 ]; then
        echo "local-ci lease test failed: $wait_phase: timed out after ${wait_timeout_seconds}s waiting for child pid=$wait_pid" >&2
        return 124
    fi
    return "$wait_status"
}

cleanup() {
    status=$?
    trap - EXIT HUP INT TERM
    for pid in $background_pids; do
        kill_tree "$pid" || true
        wait "$pid" 2>/dev/null || true
    done
    rm -rf "$temporary"
    exit "$status"
}
trap cleanup EXIT HUP INT TERM

fail() {
    echo "local-ci lease test failed: $*" >&2
    [ -f "${waiter_stderr-}" ] && cat "$waiter_stderr" >&2
    exit 1
}

wait_checked() {
    phase=$1
    pid=$2
    if wait_bounded "$phase" "$pid"; then
        return 0
    else
        status=$?
    fi
    fail "$phase: child pid=$pid exited status=$status"
}

wait_terminated() {
    phase=$1
    pid=$2
    if wait_bounded "$phase" "$pid"; then
        fail "$phase: killed process-tree root pid=$pid exited status=0"
    else
        status=$?
    fi
    printf 'local-ci lease test: %s child pid=%s exited status=%s after tree teardown\n' \
        "$phase" "$pid" "$status" >&2
}

wait_for_file() {
    file=$1
    attempts=0
    while [ ! -f "$file" ]; do
        [ "$attempts" -lt 100 ] || fail "timed out waiting for $file"
        sleep 0.05
        attempts=$((attempts + 1))
    done
}

assert_absent() {
    [ ! -e "$1" ] && [ ! -L "$1" ] || fail "unexpected path exists: $1"
}

# Exercise the internal deadline itself; the outer Perl alarm is only an
# independent safety bound and must not be the phase's causal timeout.
timeout_probe_stderr="$temporary/timeout-probe.stderr"
sleep 30 &
timeout_probe_pid=$!
if wait_bounded "timeout probe" "$timeout_probe_pid" 2>"$timeout_probe_stderr"; then
    fail "bounded wait timeout probe unexpectedly completed"
else
    timeout_probe_status=$?
fi
[ "$timeout_probe_status" -eq 124 ] || fail "bounded wait timeout returned status $timeout_probe_status"
kill -KILL "$timeout_probe_pid" 2>/dev/null || true
wait "$timeout_probe_pid" 2>/dev/null || true
grep -Fq 'timeout probe: timed out after' "$timeout_probe_stderr" \
    || fail "bounded wait timeout had no causal phase diagnostic"

git init -q "$temporary/repository"
git -C "$temporary/repository" config user.email test@example.invalid
git -C "$temporary/repository" config user.name Test
printf 'lease test\n' >"$temporary/repository/README"
git -C "$temporary/repository" add README
git -C "$temporary/repository" commit -qm initial
git -C "$temporary/repository" worktree add -q -b first "$first" HEAD
git -C "$temporary/repository" worktree add -q -b second "$second" HEAD
for worktree in "$first" "$second"; do
    mkdir -p "$worktree/scripts"
    cp "$repository_root/scripts/local-ci-lease.sh" "$worktree/scripts/local-ci-lease.sh"
    cp "$repository_root/scripts/with-local-ci-lease.sh" "$worktree/scripts/with-local-ci-lease.sh"
    chmod +x "$worktree/scripts/with-local-ci-lease.sh"
done

holder_command="$temporary/holder.sh"
short_command="$temporary/short.sh"
failure_command="$temporary/failure.sh"
term_command="$temporary/term.sh"
descendant_command="$temporary/descendant.sh"
descendant_done="$temporary/descendant-done"
nested_command="$temporary/nested.sh"
printf '%s\n' '#!/bin/sh' 'set -eu' ': >"$1"' 'sleep "$2"' >"$holder_command"
printf '%s\n' '#!/bin/sh' 'set -eu' ': >"$1"' >"$short_command"
printf '%s\n' '#!/bin/sh' 'exit 17' >"$failure_command"
printf '%s\n' '#!/bin/sh' 'set -eu' ': >"$1"' 'while :; do sleep 1; done' >"$term_command"
printf '%s\n' '#!/bin/sh' 'set -eu' ': >"$1"' '(while [ ! -f "$2" ]; do sleep 0.05; done) &' 'child=$!' 'printf "%s\\n" "$child" >"$3"' 'wait "$child"' ': >"$4"' >"$descendant_command"
printf '%s\n' '#!/bin/sh' 'set -eu' 'if ./scripts/with-local-ci-lease.sh true 2>"$1"; then exit 1; fi' >"$nested_command"
chmod +x "$holder_command" "$short_command" "$failure_command" "$term_command" "$descendant_command" "$nested_command"

common_dir=$(git -C "$first" rev-parse --git-common-dir)
lease_path="$common_dir/.dark-factory-local-ci"
lock_path="$common_dir/.dark-factory-local-ci.lock"

# The authority pathname is an atomic directory object, never a followable
# regular-file or symlink pathname.
outside_lock="$temporary/outside-lock"
: >"$outside_lock"
ln -s "$outside_lock" "$lock_path"
if (cd "$first" && ./scripts/with-local-ci-lease.sh true) 2>"$temporary/initial-symlink.stderr"; then
    fail "initial lock-object symlink was followed"
fi
grep -Fq 'unsafe lock object path' "$temporary/initial-symlink.stderr" || fail "initial symlink refusal was unexplained"
rm -f "$lock_path"

# A holder publishes an identity-bound directory marker only after it owns
# lockf. Kill that holder at the narrow post-lockf seam; a waiter must not
# inherit the dead marker forever or split authority with a late starter.
starting_pause_fifo="$temporary/starting-pause"
starting_marker="$lock_path/.starting"
starting_held="$temporary/starting-held"
starting_waiter="$temporary/starting-waiter"
starting_waiter_stderr="$temporary/starting-waiter.stderr"
waiter_stderr="$starting_waiter_stderr"
mkfifo "$starting_pause_fifo"
(
    cd "$first"
    DARK_FACTORY_LOCAL_CI_TEST_PAUSE_AFTER_LOCKF="$starting_pause_fifo" \
        ./scripts/with-local-ci-lease.sh "$short_command" "$starting_held"
) &
starting_pid=$!
background_pids="$background_pids $starting_pid"
wait_for_file "$starting_marker/owner"
starting_owner_pid=$(sed -n 's/^pid=//p' "$starting_marker/owner")
case "$starting_owner_pid" in
    ''|*[!0-9]*) fail "startup-kill phase: marker did not contain a numeric owner PID" ;;
esac
kill_tree "$starting_pid"
wait_terminated "startup-kill phase" "$starting_pid"
(
    cd "$second"
    ./scripts/with-local-ci-lease.sh "$short_command" "$starting_waiter"
) 2>"$starting_waiter_stderr" &
starting_waiter_pid=$!
background_pids="$background_pids $starting_waiter_pid"
wait_checked "startup-recovery waiter" "$starting_waiter_pid"
[ -f "$starting_waiter" ] || fail "recovered waiter did not run its command"
assert_absent "$starting_marker"
assert_absent "$lease_path"
assert_absent "$lock_path/.recovery"
assert_absent "$lock_path"
[ -z "$(find "$common_dir" -maxdepth 1 -type f -name '.dark-factory-local-ci-owner.*' -print)" ] \
    || fail "dead starter left owner records behind"

start_holder() {
    worktree=$1
    marker=$2
    seconds=$3
    agent=${4-}
    task=${5-}
    (
        cd "$worktree"
        DARK_FACTORY_AGENT="$agent" DARK_FACTORY_TASK="$task" \
            ./scripts/with-local-ci-lease.sh "$holder_command" "$marker" "$seconds"
    ) &
    last_holder_pid=$!
    background_pids="$background_pids $last_holder_pid"
}

start_stale_holder() {
    worktree=$1
    marker=$2
    done_marker=$3
    (
        cd "$worktree"
        ./scripts/with-local-ci-lease.sh "$holder_command" "$marker" 1
        : >"$done_marker"
    ) &
    last_holder_pid=$!
    background_pids="$background_pids $last_holder_pid"
}

acquire_and_release() {
    worktree=$1
    marker=$2
    (
        cd "$worktree"
        ./scripts/with-local-ci-lease.sh "$short_command" "$marker"
    )
}

# Interruption cleanup must kill descendants before deleting the throwaway
# tree; killing only the recorded shell can orphan a fixture child.
tree_command="$temporary/tree.sh"
tree_child_pid_file="$temporary/tree-child.pid"
printf '%s\n' '#!/bin/sh' 'set -eu' '(while :; do sleep 1; done) &' 'child=$!' 'printf "%s\\n" "$child" >"$1"' 'while :; do sleep 1; done' >"$tree_command"
chmod +x "$tree_command"
sh "$tree_command" "$tree_child_pid_file" &
tree_pid=$!
background_pids="$background_pids $tree_pid"
wait_for_file "$tree_child_pid_file"
tree_child_pid=$(cat "$tree_child_pid_file")
tree_parent=$(process_parent "$tree_pid")
tree_start=$(process_start "$tree_pid")
tree_command=$(process_command "$tree_pid")
if kill_tree "$tree_pid" "$tree_parent" "identity-mutated-start" "$tree_command"; then
    fail "identity mutation was allowed to signal a live process"
fi
if kill_tree "$tree_pid" "$tree_parent" "$tree_start" "identity-mutated-command"; then
    fail "command-path mutation was allowed to signal a live process"
fi
kill -0 "$tree_pid" 2>/dev/null || fail "identity mutation probe unexpectedly killed the process"
kill_tree "$tree_pid"
wait_terminated "process-tree cleanup phase" "$tree_pid"
kill -0 "$tree_child_pid" 2>/dev/null && fail "fixture descendant survived process-tree cleanup"

fake_ps_bin="$temporary/fake-bin"
mkdir "$fake_ps_bin"
printf '%s\n' '#!/bin/sh' 'exit 1' >"$fake_ps_bin/ps"
chmod +x "$fake_ps_bin/ps"

recover_starting_case() {
    case_name=$1
    case_marker="$temporary/starting-$case_name-recovered"
    mkdir "$lock_path"
    : >"$lock_path/descriptor"
    mkdir "$lock_path/.recovery"
    mkdir "$lock_path/.starting"
    case "$case_name" in
        missing) : ;;
        partial) printf 'pid=\n' >"$lock_path/.starting/owner" ;;
        malformed) printf 'not an owner record\n' >"$lock_path/.starting/owner" ;;
        *) fail "unknown startup fixture $case_name" ;;
    esac
    if [ "$case_name" = malformed ]; then
        (cd "$second" && PATH="$fake_ps_bin:$PATH" ./scripts/with-local-ci-lease.sh "$short_command" "$case_marker") \
            || fail "$case_name startup artifact was not recovered"
    else
        (cd "$second" && ./scripts/with-local-ci-lease.sh "$short_command" "$case_marker") \
            || fail "$case_name startup artifact was not recovered"
    fi
    [ -f "$case_marker" ] || fail "$case_name recovery command did not run"
    assert_absent "$lock_path/.starting"
    assert_absent "$lock_path/.recovery"
    assert_absent "$lock_path"
}

recover_starting_case missing
recover_starting_case partial
recover_starting_case malformed

# Linked worktrees share one kernel lease and one bounded, sanitized owner
# diagnostic that includes the exact owner head.
held_marker="$temporary/held"
waiter_marker="$temporary/waiter"
waiter_stderr="$temporary/waiter.stderr"
head=$(git -C "$first" rev-parse HEAD)
start_holder "$first" "$held_marker" 2 $'agent\nSECRET' $'task\033SECRET'
wait_for_file "$held_marker"
(
    cd "$second"
    ./scripts/with-local-ci-lease.sh "$short_command" "$waiter_marker"
) 2>"$waiter_stderr" &
waiter_pid=$!
background_pids="$background_pids $waiter_pid"
sleep 0.2
[ ! -f "$waiter_marker" ] || fail "waiter acquired before the owner released"
wait_checked "ordinary waiter" "$waiter_pid"
[ "$(grep -c 'current owner:' "$waiter_stderr")" -eq 1 ] || fail "owner diagnostic count changed"
[ "$(wc -c <"$waiter_stderr" | tr -d ' ')" -le 2300 ] || fail "owner diagnostic was not bounded"
grep -Fq "head=$head" "$waiter_stderr" || fail "owner head was not reported"
! grep -Fq SECRET "$waiter_stderr" || fail "hostile owner labels leaked"

# Identifier punctuation is not an owner-record escape hatch.
start_holder "$first" "$temporary/invalid-id-held" 2 'ghp_secret' 'agent:token'
wait_for_file "$temporary/invalid-id-held"
invalid_id_stderr="$temporary/invalid-id.stderr"
if (cd "$second" && DARK_FACTORY_LOCAL_CI_WAIT=0 ./scripts/with-local-ci-lease.sh true) \
    2>"$invalid_id_stderr"; then
    fail "invalid owner identifiers unexpectedly acquired"
fi
! grep -Fq 'ghp_secret' "$invalid_id_stderr" || fail "secret-looking agent identifier leaked"
! grep -Fq 'agent:token' "$invalid_id_stderr" || fail "invalid task identifier leaked"
wait_checked "invalid-owner diagnostic holder" "$last_holder_pid"

# A diagnostic record must be a regular file, never a symlink to arbitrary
# readable content.
forged_record="$temporary/forged-record"
forged_owner_ref=.dark-factory-local-ci-owner.forged
printf 'pid=7\nworktree=SECRET\nstarted_at=SECRET\nlock_identity=7:7\nhead=0123456789abcdef0123456789abcdef01234567\nsecret=DO_NOT_DISCLOSE\n' >"$forged_record"
mkdir "$lock_path"
: >"$lock_path/descriptor"
ln -s "$forged_record" "$common_dir/$forged_owner_ref"
ln -s "$forged_owner_ref" "$lease_path"
forged_stderr="$temporary/forged-record.stderr"
if (cd "$second" && DARK_FACTORY_LOCAL_CI_WAIT=0 ./scripts/with-local-ci-lease.sh true) 2>"$forged_stderr"; then
    fail "symlinked owner record unexpectedly acquired"
fi
! grep -Fq 'DO_NOT_DISCLOSE' "$forged_stderr" || fail "symlinked owner record leaked content"
! grep -Fq 'SECRET' "$forged_stderr" || fail "symlinked owner record leaked fields"
rm -f "$lease_path" "$common_dir/$forged_owner_ref"
rm -rf "$lock_path"

# Fail-fast remains owner-aware.
fail_fast_stderr="$temporary/fail-fast.stderr"
start_holder "$first" "$temporary/fail-fast-held" 2
wait_for_file "$temporary/fail-fast-held"
if (cd "$second" && DARK_FACTORY_LOCAL_CI_WAIT=0 ./scripts/with-local-ci-lease.sh true) 2>"$fail_fast_stderr"; then
    fail "fail-fast waiter unexpectedly succeeded"
fi
grep -Fq 'DARK_FACTORY_LOCAL_CI_WAIT=0' "$fail_fast_stderr" || fail "fail-fast reason missing"

# Failure and TERM both release the kernel lease.
if (cd "$first" && ./scripts/with-local-ci-lease.sh "$failure_command"); then
    fail "failing command unexpectedly returned success"
fi
acquire_and_release "$second" "$temporary/failure-recovered"
term_marker="$temporary/term-held"
sh -c 'cd "$1" && exec ./scripts/with-local-ci-lease.sh "$2" "$3"' \
    local-ci-term "$first" "$term_command" "$term_marker" &
term_pid=$!
background_pids="$background_pids $term_pid"
wait_for_file "$term_marker"
term_parent=$(process_parent "$term_pid")
term_start=$(process_start "$term_pid")
term_command_line=$(process_command "$term_pid")
process_identity_matches "$term_pid" "$term_parent" "$term_start" "$term_command_line" \
    || fail "TERM cleanup phase: root identity changed before signal"
kill -TERM "$term_pid"
wait_terminated "TERM cleanup phase" "$term_pid"
term_live_processes=$(ps -axo pid=,command= | awk -v root="$temporary" \
    'index($0, root) && ($0 ~ /term\.sh/ || $0 ~ /local-ci-lease\.sh/) { print }')
[ -z "$term_live_processes" ] || fail "TERM cleanup phase left owned descendants:\n$term_live_processes"
acquire_and_release "$second" "$temporary/term-recovered"

# A SIGKILLed wrapper cannot release a lock inherited by a surviving command
# descendant. The waiter proceeds only after that descendant is released.
descendant_marker="$temporary/descendant-held"
descendant_release="$temporary/descendant-release"
descendant_pid_file="$temporary/descendant.pid"
sh -c 'cd "$1" && exec ./scripts/with-local-ci-lease.sh "$2" "$3" "$4" "$5" "$6"' \
    local-ci-descendant "$first" "$descendant_command" "$descendant_marker" "$descendant_release" "$descendant_pid_file" "$descendant_done" &
descendant_wrapper_pid=$!
background_pids="$background_pids $descendant_wrapper_pid"
wait_for_file "$descendant_marker"
wait_for_file "$descendant_pid_file"
kill -KILL "$descendant_wrapper_pid"
wait_terminated "surviving-descendant phase" "$descendant_wrapper_pid"
sleep 0.3
if (cd "$second" && DARK_FACTORY_LOCAL_CI_WAIT=0 ./scripts/with-local-ci-lease.sh true) 2>"$temporary/descendant.stderr"; then
    fail "waiter acquired while a killed-owner descendant survived"
fi
: >"$descendant_release"
wait_for_file "$descendant_done"
acquire_and_release "$second" "$temporary/descendant-recovered"

# Two stale-recovery contenders cannot remove a new owner: recovery is guarded
# inside the lock object before cleaning the old diagnostic link.
stale_record="$common_dir/.dark-factory-local-ci-owner.stale"
stale_pid=999999
while kill -0 "$stale_pid" 2>/dev/null; do stale_pid=$((stale_pid - 1)); done
mkdir "$lock_path"
: >"$lock_path/descriptor"
printf 'pid=%s\nworktree=%s\nstarted_at=stale\nlock_identity=%s\nhead=%s\n' \
    "$stale_pid" "$first" "$(stat -f '%d:%i' "$lock_path")" \
    0123456789abcdef0123456789abcdef01234567 >"$stale_record"
ln -s "$(basename "$stale_record")" "$lease_path"
stale_first_done="$temporary/stale-first-done"
stale_second_done="$temporary/stale-second-done"
start_stale_holder "$first" "$temporary/stale-first" "$stale_first_done"
stale_first_pid=$last_holder_pid
start_stale_holder "$second" "$temporary/stale-second" "$stale_second_done"
stale_second_pid=$last_holder_pid
wait_for_file "$temporary/stale-first"
wait_for_file "$temporary/stale-second"
assert_absent "$stale_record"
wait_for_file "$stale_first_done"
wait_for_file "$stale_second_done"
wait_checked "first stale-recovery contender" "$stale_first_pid"
wait_checked "second stale-recovery contender" "$stale_second_pid"
assert_absent "$lock_path/.recovery"
assert_absent "$lease_path"
assert_absent "$lock_path"

# Removing or replacing a live object with a symlink or another directory must
# fail closed rather than locking a different inode.
replacement_held="$temporary/replacement-held"
start_holder "$first" "$replacement_held" 2
wait_for_file "$replacement_held"
mv "$lock_path" "$temporary/original-lock-object"
if (cd "$second" && DARK_FACTORY_LOCAL_CI_WAIT=0 ./scripts/with-local-ci-lease.sh true) \
    2>"$temporary/replacement-missing.stderr"; then
    fail "live lock-object removal allowed a replacement lease"
fi
grep -Fq 'without its lock object' "$temporary/replacement-missing.stderr" || fail "live lock-object removal refusal was unexplained"
mv "$temporary/original-lock-object" "$lock_path"
wait_checked "live lock-object removal holder" "$last_holder_pid"

replacement_held="$temporary/replacement-symlink-held"
start_holder "$first" "$replacement_held" 2
wait_for_file "$replacement_held"
mv "$lock_path" "$temporary/original-lock-object"
ln -s "$outside_lock" "$lock_path"
if (cd "$second" && DARK_FACTORY_LOCAL_CI_WAIT=0 ./scripts/with-local-ci-lease.sh true) \
    2>"$temporary/replacement-symlink.stderr"; then
    fail "live lock-object symlink replacement was followed"
fi
grep -Fq 'unsafe lock object path' "$temporary/replacement-symlink.stderr" || fail "live symlink replacement refusal was unexplained"
rm -f "$lock_path"
wait_checked "live lock-object symlink holder" "$last_holder_pid"
rm -rf "$temporary/original-lock-object"

replacement_held="$temporary/replacement-directory-held"
start_holder "$first" "$replacement_held" 2
wait_for_file "$replacement_held"
mv "$lock_path" "$temporary/original-lock-object"
mkdir "$lock_path"
: >"$lock_path/descriptor"
if (cd "$second" && DARK_FACTORY_LOCAL_CI_WAIT=0 ./scripts/with-local-ci-lease.sh true) \
    2>"$temporary/replacement-directory.stderr"; then
    fail "live lock-object directory replacement split the lease"
fi
grep -Fq 'lock object replacement' "$temporary/replacement-directory.stderr" || fail "live directory replacement refusal was unexplained"
rm -rf "$lock_path"
wait_checked "live lock-object directory holder" "$last_holder_pid"
rm -rf "$temporary/original-lock-object"

# Malformed metadata is fail-closed rather than silently treated as stale.
printf 'not an owner\n' >"$stale_record"
ln -s "$(basename "$stale_record")" "$lease_path"
if (cd "$first" && ./scripts/with-local-ci-lease.sh true) 2>"$temporary/invalid.stderr"; then
    fail "malformed metadata unexpectedly succeeded"
fi
grep -Fq 'invalid owner metadata' "$temporary/invalid.stderr" || fail "malformed metadata was not explained"
rm -f "$lease_path" "$stale_record"

# Direct load-bearing commands use the same wrapper. Nested invocation is
# refused through the inherited owner contract instead of ancestry guessing.
nested_stderr="$temporary/nested.stderr"
(cd "$first" && ./scripts/with-local-ci-lease.sh "$nested_command" "$nested_stderr") || fail "outer direct wrapper failed"
grep -Fq 'nested lease invocation refused' "$nested_stderr" || fail "nested invocation was not explicit"

echo "local-ci lease tests passed"
