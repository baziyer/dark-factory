#!/bin/sh
set -eu

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
    exit 1
}

wait_for_file() {
    file=$1
    attempts=0
    while [ ! -f "$file" ]; do
        [ "$attempts" -lt 50 ] || fail "timed out waiting for $file"
        sleep 0.1
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
mkdir -p "$first/scripts" "$second/scripts"
cp "$repository_root/scripts/local-ci-lease.sh" "$first/scripts/local-ci-lease.sh"
cp "$repository_root/scripts/local-ci-lease.sh" "$second/scripts/local-ci-lease.sh"

helper="$first/scripts/local-ci-lease.sh"
common_dir=$(git -C "$first" rev-parse --git-common-dir)
lease_path="$common_dir/.dark-factory-local-ci"

start_holder() {
    worktree=$1
    marker=$2
    seconds=$3
    sh -c '
        set -eu
        cd "$1"
        . "$1/scripts/local-ci-lease.sh"
        local_ci_lease_acquire
        trap 'local_ci_lease_release' EXIT
        : >"$2"
        sleep "$3"
    ' local-ci-lease-holder "$worktree" "$marker" "$seconds" &
    background_pids="$background_pids $!"
}

acquire_and_release() {
    worktree=$1
    marker=$2
    (
        cd "$worktree"
        . "$worktree/scripts/local-ci-lease.sh"
        local_ci_lease_acquire
        : >"$marker"
        local_ci_lease_release
    )
}

# Two linked worktrees share one lease. The waiter cannot acquire until the
# first holder exits, and it prints one bounded owner diagnostic.
held_marker="$temporary/held"
waiter_marker="$temporary/waiter"
waiter_stderr="$temporary/waiter.stderr"
start_holder "$first" "$held_marker" 2
wait_for_file "$held_marker"
sh -c '
    set -eu
    cd "$1"
    . "$1/scripts/local-ci-lease.sh"
    local_ci_lease_acquire
    : >"$2"
    local_ci_lease_release
' local-ci-lease-waiter "$second" "$waiter_marker" 2>"$waiter_stderr" &
waiter_pid=$!
background_pids="$background_pids $waiter_pid"
sleep 0.2
assert_absent "$waiter_marker"
wait "$waiter_pid" || fail "waiter did not continue after the holder released"
[ "$(wc -l <"$waiter_stderr" | tr -d ' ')" -ge 1 ] || fail "waiter did not report its owner"
[ "$(grep -c 'current owner:' "$waiter_stderr")" -eq 1 ] || fail "waiter printed more than one owner diagnostic"
[ "$(wc -c <"$waiter_stderr" | tr -d ' ')" -le 2300 ] || fail "owner diagnostic was not bounded"

# A failing shell releases the lease through its EXIT trap.
if (
    cd "$first"
    . "$helper"
    local_ci_lease_acquire
    trap 'local_ci_lease_release' EXIT
    exit 17
); then
    fail "failing owner unexpectedly returned success"
fi
acquire_and_release "$second" "$temporary/failure-recovered"

# TERM releases the lease before the process exits.
term_marker="$temporary/term-held"
(
    cd "$first"
    . "$helper"
    local_ci_lease_acquire
    trap 'local_ci_lease_release' EXIT
    trap 'local_ci_lease_release; exit 143' TERM INT
    : >"$term_marker"
    while :; do sleep 1; done
) &
term_pid=$!
background_pids="$background_pids $term_pid"
wait_for_file "$term_marker"
kill -TERM "$term_pid"
wait "$term_pid" 2>/dev/null || true
acquire_and_release "$second" "$temporary/term-recovered"

# A dead recorded owner is recovered, while the owner link and record are
# removed only after the moved link is verified to be the stale one.
stale_record="$common_dir/.dark-factory-local-ci-owner.stale"
stale_pid=999999
while kill -0 "$stale_pid" 2>/dev/null; do stale_pid=$((stale_pid - 1)); done
cat >"$stale_record" <<EOF
pid=$stale_pid
worktree=$first
started_at=stale
agent=test
task=lease
EOF
ln -s "$(basename "$stale_record")" "$lease_path"
acquire_and_release "$second" "$temporary/stale-recovered"
assert_absent "$stale_record"
assert_absent "$lease_path"

# A nested child refuses explicitly instead of waiting forever on its parent.
nested_stderr="$temporary/nested.stderr"
(
    cd "$first"
    . "$helper"
    local_ci_lease_acquire
    trap 'local_ci_lease_release' EXIT
    if sh -c '. "$1"; local_ci_lease_acquire' sh "$helper" 2>"$nested_stderr"; then
        exit 1
    fi
)
grep -Fq 'nested invocation would wait on its ancestor' "$nested_stderr" || fail "nested invocation did not fail explicitly"

echo "local-ci lease tests passed"
