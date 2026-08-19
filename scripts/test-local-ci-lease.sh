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

cleanup() {
    status=$?
    trap - EXIT HUP INT TERM
    for pid in $background_pids; do
        kill "$pid" 2>/dev/null || true
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

# A starter publishes an identity-bound directory marker before it can invoke
# lockf.  Kill that starter at the narrow pre-lockf seam; a waiter must not
# inherit the dead marker forever or split authority with a late starter.
starting_pause_fifo="$temporary/starting-pause"
starting_marker="$lock_path/.starting"
starting_held="$temporary/starting-held"
starting_waiter="$temporary/starting-waiter"
starting_waiter_stderr="$temporary/starting-waiter.stderr"
mkfifo "$starting_pause_fifo"
(
    cd "$first"
    DARK_FACTORY_LOCAL_CI_TEST_PAUSE_BEFORE_LOCKF="$starting_pause_fifo" \
        ./scripts/with-local-ci-lease.sh "$short_command" "$starting_held"
) &
starting_pid=$!
background_pids="$background_pids $starting_pid"
wait_for_file "$starting_marker/owner"
starting_owner_pid=$(sed -n 's/^pid=//p' "$starting_marker/owner")
case "$starting_owner_pid" in
    ''|*[!0-9]*) fail "startup marker did not contain a numeric owner PID" ;;
esac
kill -KILL "$starting_owner_pid"
wait "$starting_pid" 2>/dev/null || true
(
    cd "$second"
    ./scripts/with-local-ci-lease.sh "$short_command" "$starting_waiter"
) 2>"$starting_waiter_stderr" &
starting_waiter_pid=$!
background_pids="$background_pids $starting_waiter_pid"
wait "$starting_waiter_pid" || fail "waiter did not recover a killed pre-lockf starter"
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
wait "$waiter_pid" || fail "waiter did not continue after the holder released"
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
wait "$last_holder_pid"

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
kill -TERM "$term_pid"
wait "$term_pid" 2>/dev/null || true
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
wait "$descendant_wrapper_pid" 2>/dev/null || true
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
if ! wait "$stale_first_pid"; then
    fail "first stale-recovery contender failed"
fi
if ! wait "$stale_second_pid"; then
    fail "second stale-recovery contender failed"
fi
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
wait

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
wait
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
wait
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
