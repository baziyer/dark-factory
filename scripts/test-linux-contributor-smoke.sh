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
for proof in stop_owned_runs wait_for_tracked_processes kill_exact_record \
    wait_for_verifier_record wait_for_verifier_descendant record_is_alive; do
    grep -Fq "$proof" "$smoke" || fail "smoke lacks $proof"
done
grep -Fq '"complete":false' "$smoke" \
    || fail "smoke does not prove live verifier cache measurement stays incomplete"
grep -Fq 'temporary_root=' "$smoke" \
    || fail "smoke does not retain verifier staging until exact group release"
grep -Fq 'exec sleep 120' "$smoke" \
    || fail "runner-kill descendant can expire inside the smoke timeout"
grep -Fq 'verifier-release' "$smoke" \
    || fail "verifier descendant has no causal release handshake"
grep -Fq ': >"$scratch/verifier-release"' "$smoke" \
    || fail "smoke never releases the verifier descendant after its assertions"
grep -Fq 'Duration::from_secs(20)' "$smoke" \
    && fail "verifier descendant still uses an arbitrary 20-second hold"
grep -Fq '; sleep 5;' "$smoke" \
    && fail "first provider uses an arbitrary five-second delay"
grep -Fxq 'sleep 0.2' "$smoke" \
    && fail "process discovery uses an arbitrary delay before bounded polling"

if DARK_FACTORY_SMOKE_ROOT="$root" DARK_FACTORY_SMOKE_FORCE_FAILURE=1 \
    "$smoke"; then
    fail "intentional interrupted smoke unexpectedly succeeded"
fi
test ! -e "$root" || fail "interrupted smoke left its scratch home"

echo "Linux smoke teardown tests passed"
