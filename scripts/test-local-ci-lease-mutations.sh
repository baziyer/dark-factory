#!/bin/sh
set -eu

# This suite creates an independent throwaway repository so it can exercise
# mutated wrappers even when local-ci.sh itself owns the production repo lease.
unset DARK_FACTORY_LOCAL_CI_LEASE_HELD

repository_root=$(CDPATH= cd -- "$(dirname "$0")/.." && pwd)

# PR #210's additional summary-contract phase remains inside the one
# local-ci.sh lease entry point; the workflow's always-summary is reporting
# only and cannot acquire, split, or release this lease.
grep -F 'exec "$script_dir/with-local-ci-lease.sh" "$script_dir/local-ci.sh"' \
    "$repository_root/scripts/local-ci.sh" >/dev/null
grep -F './scripts/test-github-step-summary.sh' "$repository_root/scripts/local-ci.sh" >/dev/null

temporary=$(mktemp -d "${TMPDIR:-/tmp}/dark-factory-local-ci-lease-mutation.XXXXXX")
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
    echo "local-ci lease mutation test failed: $*" >&2
    exit 1
}

wait_for_file() {
    attempts=0
    while [ ! -f "$1" ]; do
        [ "$attempts" -lt 100 ] || fail "timed out waiting for $1"
        sleep 0.05
        attempts=$((attempts + 1))
    done
}

git init -q "$temporary/repository"
git -C "$temporary/repository" config user.email test@example.invalid
git -C "$temporary/repository" config user.name Test
printf 'mutation\n' >"$temporary/repository/README"
git -C "$temporary/repository" add README
git -C "$temporary/repository" commit -qm initial
worktree="$temporary/worktree"
git -C "$temporary/repository" worktree add -q -b test "$worktree" HEAD
mkdir -p "$worktree/scripts"
cp "$repository_root/scripts/with-local-ci-lease.sh" "$worktree/scripts/with-local-ci-lease.sh"
chmod +x "$worktree/scripts/with-local-ci-lease.sh"

holder_command="$temporary/holder.sh"
waiter_command="$temporary/waiter.sh"
printf '%s\n' '#!/bin/sh' 'set -eu' ': >"$1"' 'sleep 10' >"$holder_command"
printf '%s\n' '#!/bin/sh' 'set -eu' ': >"$1"' >"$waiter_command"
chmod +x "$holder_command"
chmod +x "$waiter_command"

# Mutation 1: remove the kernel lock from the holder. The regression must
# observe the waiter acquiring concurrently while the holder is
# still alive, proving the test would catch the exclusion disappearing.
production_lockf='exec lockf -k "$LOCAL_CI_LEASE_LOCK_FILE_NAME" sh -c'
lockf_matches=$(grep -F -c "$production_lockf" "$repository_root/scripts/local-ci-lease.sh" || true)
[ "$lockf_matches" -eq 1 ] || fail "expected exactly one production lockf wrapper, found $lockf_matches"
awk -v production_lockf="$production_lockf" '
    (match_start = index($0, production_lockf)) {
        $0 = substr($0, 1, match_start - 1) "sh -c" \
            substr($0, match_start + length(production_lockf))
    }
    { print }
' "$repository_root/scripts/local-ci-lease.sh" >"$worktree/scripts/local-ci-lease.sh"
chmod +x "$worktree/scripts/local-ci-lease.sh"
held="$temporary/held"
(
    cd "$worktree"
    ./scripts/with-local-ci-lease.sh "$holder_command" "$held"
) &
holder_pid=$!
background_pids="$background_pids $holder_pid"
wait_for_file "$held"
mutation_waiter="$temporary/mutation-waiter"
(
    cd "$worktree"
    ./scripts/with-local-ci-lease.sh "$waiter_command" "$mutation_waiter"
) &
waiter_pid=$!
background_pids="$background_pids $waiter_pid"
wait_for_file "$mutation_waiter"
wait "$waiter_pid" || fail "lock-removal mutation waiter failed"
kill -0 "$holder_pid" 2>/dev/null || fail "lock-removal mutation waiter did not overlap the holder"
kill "$holder_pid" 2>/dev/null || true
wait "$holder_pid" 2>/dev/null || true

# Mutation 2: remove the inherited owner marker. The nested contract must
# still produce its explicit refusal rather than a generic lock wait.
nested_repository="$temporary/nested-repository"
nested_worktree="$temporary/nested-worktree"
git init -q "$nested_repository"
git -C "$nested_repository" config user.email test@example.invalid
git -C "$nested_repository" config user.name Test
printf 'nested mutation\n' >"$nested_repository/README"
git -C "$nested_repository" add README
git -C "$nested_repository" commit -qm initial
git -C "$nested_repository" worktree add -q -b nested "$nested_worktree" HEAD
mkdir -p "$nested_worktree/scripts"
cp "$repository_root/scripts/with-local-ci-lease.sh" "$nested_worktree/scripts/with-local-ci-lease.sh"
cp "$repository_root/scripts/local-ci-lease.sh" "$nested_worktree/scripts/local-ci-lease.sh"
chmod +x "$nested_worktree/scripts/with-local-ci-lease.sh" "$nested_worktree/scripts/local-ci-lease.sh"
sed -i '' 's/local-ci: nested lease invocation refused/local-ci: nested lease mutation/' \
    "$nested_worktree/scripts/local-ci-lease.sh"
held="$temporary/nested-held"
nested_stderr="$temporary/nested.stderr"
(
    cd "$nested_worktree"
    ./scripts/with-local-ci-lease.sh "$holder_command" "$held"
) &
holder_pid=$!
background_pids="$background_pids $holder_pid"
wait_for_file "$held"
nested_command="$temporary/nested.sh"
printf '%s\n' '#!/bin/sh' 'if DARK_FACTORY_LOCAL_CI_WAIT=0 ./scripts/with-local-ci-lease.sh true 2>"$1"; then exit 1; fi' >"$nested_command"
chmod +x "$nested_command"
if (
    cd "$nested_worktree"
    ./scripts/with-local-ci-lease.sh "$nested_command" "$nested_stderr"
); then
    :
else
    fail "owner-marker mutation made the outer command fail"
fi
if grep -Fq 'nested lease invocation refused' "$nested_stderr"; then
    fail "owner-marker mutation was not exposed by the nested regression"
fi
kill "$holder_pid" 2>/dev/null || true
wait "$holder_pid" 2>/dev/null || true

echo "local-ci lease mutation tests passed"
