#!/bin/sh
set -eu

repository_root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
smoke="$repository_root/scripts/linux-contributor-smoke.sh"
root=$(mktemp -d /tmp/df-linux-smoke-test.XXXXXX)
rmdir "$root"

cleanup_test() {
    status=$?
    trap - EXIT HUP INT TERM
    test ! -e "$root" || echo "preserving failed smoke root: $root" >&2
    exit "$status"
}
trap cleanup_test EXIT HUP INT TERM

fail() {
    echo "Linux smoke teardown test failed: $*" >&2
    exit 1
}

if grep -En '(^|[[:space:];])kill[[:space:]]+-[A-Z]+' "$smoke" \
    | grep -Ev '"\$(daemon_pid|process_pid)"' >/dev/null; then
    fail "smoke contains a non-owned process signal"
fi
grep -Eq '(^|[[:space:]])(pkill|killall)([[:space:]]|$)' "$smoke" \
    && fail "smoke signals by process name"
for proof in stop_owned_runs wait_for_tracked_processes kill_exact_record; do
    grep -Fq "$proof" "$smoke" || fail "smoke lacks $proof"
done

if DARK_FACTORY_SMOKE_ROOT="$root" DARK_FACTORY_SMOKE_FORCE_FAILURE=1 \
    "$smoke"; then
    fail "intentional interrupted smoke unexpectedly succeeded"
fi
test ! -e "$root" || fail "interrupted smoke left its scratch home"

echo "Linux smoke teardown tests passed"
