#!/bin/sh
set -eu

repository_root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
smoke="$repository_root/scripts/linux-contributor-smoke.sh"
control=$(mktemp -d /tmp/df-linux-smoke-test.XXXXXX)
rmdir "$control"
root="$control"

cleanup_test() {
    status=$?
    trap - EXIT HUP INT TERM
    if test -e "$root"; then
        echo "preserving failed smoke root for diagnosis: $root" >&2
    else
        rm -rf "$control"
    fi
    exit "$status"
}
trap cleanup_test EXIT HUP INT TERM

fail() {
    echo "Linux smoke teardown test failed: $*" >&2
    exit 1
}

if grep -Eq '(^|[[:space:];])kill[[:space:]]+(-[A-Z]+[[:space:]]+)?[^"$]+' "$smoke" \
    | grep -v '"\$daemon_pid"'; then
    fail "smoke contains a non-owned process signal"
fi
grep -Fq 'run stop' "$smoke" || fail "smoke does not stop its owned run"
grep -Fq 'wait_for_tracked_processes' "$smoke" || fail "smoke lacks bounded runner proof"
grep -Fq 'DARK_FACTORY_SMOKE_FORCE_FAILURE' "$smoke" \
    || fail "smoke lacks interrupted cleanup exercise"
grep -Fq 'project verification' "$smoke" \
    || fail "smoke does not enable daemon-owned Rust verification"
grep -Fq 'storage status --json' "$smoke" \
    || fail "smoke does not prove bounded Rust artifact reporting"
grep -Fq 'cache_count_over_limit' "$smoke" \
    || fail "smoke checks stale Rust storage fields"
grep -Fq 'verifier-launches' "$smoke" \
    || fail "smoke does not crash and resume immutable Rust verification"
grep -Fq "printf 'B\\n'" "$smoke" \
    || fail "smoke does not mutate the retained Change after source selection"

if DARK_FACTORY_SMOKE_ROOT="$root" DARK_FACTORY_SMOKE_FORCE_FAILURE=1 \
    "$smoke"; then
    fail "intentional interrupted smoke unexpectedly succeeded"
fi
test ! -e "$root" || fail "interrupted smoke left its scratch home"

echo "Linux smoke teardown tests passed"
