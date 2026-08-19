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
printf '%s\n' '#!/bin/sh' 'set -eu' ': >"$1"' 'sleep 1' >"$holder_command"
chmod +x "$holder_command"

# Mutation 1: remove the kernel lock from the holder. The regression must
# observe the fail-fast waiter acquiring concurrently, proving the test would
# catch the exclusion disappearing.
sed 's/        lockf -k "$LOCAL_CI_LEASE_LOCK_FILE_NAME" sh -c/        sh -c/' \
    "$repository_root/scripts/local-ci-lease.sh" >"$worktree/scripts/local-ci-lease.sh"
chmod +x "$worktree/scripts/local-ci-lease.sh"
held="$temporary/held"
(
    cd "$worktree"
    ./scripts/with-local-ci-lease.sh "$holder_command" "$held"
) &
holder_pid=$!
background_pids="$background_pids $holder_pid"
wait_for_file "$held"
if (
    cd "$worktree"
    DARK_FACTORY_LOCAL_CI_WAIT=0 ./scripts/with-local-ci-lease.sh true
); then
    :
else
    fail "lock-removal mutation was not exposed by the exclusion regression"
fi
kill "$holder_pid" 2>/dev/null || true
wait "$holder_pid" 2>/dev/null || true

# Mutation 2: remove the inherited owner marker. The nested contract must
# still produce its explicit refusal rather than a generic lock wait.
cp "$repository_root/scripts/local-ci-lease.sh" "$worktree/scripts/local-ci-lease.sh"
sed -i '' 's/local-ci: nested lease invocation refused/local-ci: nested lease mutation/' \
    "$worktree/scripts/local-ci-lease.sh"
held="$temporary/nested-held"
nested_stderr="$temporary/nested.stderr"
(
    cd "$worktree"
    ./scripts/with-local-ci-lease.sh "$holder_command" "$held"
) &
holder_pid=$!
background_pids="$background_pids $holder_pid"
wait_for_file "$held"
nested_command="$temporary/nested.sh"
printf '%s\n' '#!/bin/sh' 'if DARK_FACTORY_LOCAL_CI_WAIT=0 ./scripts/with-local-ci-lease.sh true 2>"$1"; then exit 1; fi' >"$nested_command"
chmod +x "$nested_command"
if (
    cd "$worktree"
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
