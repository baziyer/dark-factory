#!/bin/sh
# shellcheck disable=SC2016 # grep patterns intentionally contain shell syntax
set -eu

repository_root=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
runner="$repository_root/scripts/macos-launchd-release-proof.sh"
test_source="$repository_root/crates/factoryctl/tests/launchd_release.rs"
workflow="$repository_root/.github/workflows/ci.yml"
gate="$repository_root/scripts/local-ci.sh"

fail() {
    echo "macOS launchd release proof static test failed: $*" >&2
    exit 1
}

meta_root=$(mktemp -d "${TMPDIR:-/tmp}/df-launchd-probes.XXXXXX")
trap 'rm -rf -- "$meta_root"' EXIT HUP INT TERM
fake_launchctl="$meta_root/launchctl"
printf '%s\n' \
    '#!/bin/sh' \
    'case "$1" in' \
    '  print) exit "${FAKE_PRINT_STATUS:-113}" ;;' \
    '  error) printf "%s\n" "${FAKE_ERROR_TEXT:-113: Could not find specified service}" ;;' \
    '  *) exit 64 ;;' \
    'esac' >"$fake_launchctl"
fake_pid_probe="$meta_root/pid-probe"
printf '%s\n' '#!/bin/sh' 'exit "${FAKE_PID_STATUS:-0}"' >"$fake_pid_probe"
chmod 700 "$fake_launchctl" "$fake_pid_probe"

sh -n "$runner"
grep -Fq "test \"\$classification\" = '113: Could not find specified service'" "$runner" \
    || fail 'launchctl absence is not tied to the documented classification'
grep -Fq 'exit 0 if $! == EPERM;' "$runner" \
    || fail 'EPERM is no longer classified as present'
grep -Fq 'exit 3 if $! == ESRCH;' "$runner" \
    || fail 'ESRCH is no longer the only PID absence result'
if grep -Fq 'kill -0' "$runner"; then
    fail 'shell kill -0 collapsed PID observation errors'
fi
grep -Fq 'label="com.dark-factory.fixture.$suffix"' "$runner" \
    || fail 'runner lost its randomized disposable label'
grep -Fq 'live_service="$domain/com.dark-factory.factoryd"' "$runner" \
    || fail 'external verifier no longer observes the installed label'
grep -Fq 'run_bounded "$launchctl_bin" bootout "$service"' "$runner" \
    || fail 'external verifier no longer boots out only its selected service'
grep -Fq 'cmp -s "$root/live-before.job" "$root/live-after.job"' "$runner" \
    || fail 'installed job identity is not preserved'
grep -Fq 'cmp -s "$root/live-before.plist" "$root/live-after.plist"' "$runner" \
    || fail 'installed plist identity is not preserved'
grep -Fq 'mkfifo "$root/verifier.fifo"' "$runner" \
    || fail 'verifier is not independently retained across parent exit'
grep -Fq 'wait "$verifier_pid"' "$runner" \
    || fail 'parent does not consume the exact verifier result'
grep -Fq -- '--exact --test-threads=1 9>&-' "$runner" \
    || fail 'Cargo descendants retain the verifier FIFO writer'
grep -Fq 'DARK_FACTORY_LAUNCHD_SAFE_PATH' "$runner" \
    || fail 'provider-absent PATH is not explicit'

meta_status=0
env DARK_FACTORY_LAUNCHD_PROBE_META_TEST=1 \
    DARK_FACTORY_LAUNCHD_LAUNCHCTL="$fake_launchctl" \
    "$runner" --print-state gui/0/missing "$meta_root/missing" || meta_status=$?
test "$meta_status" -eq 3 || fail 'documented launchctl not-found was not absence'

meta_status=0
env DARK_FACTORY_LAUNCHD_PROBE_META_TEST=1 \
    DARK_FACTORY_LAUNCHD_LAUNCHCTL="$fake_launchctl" \
    FAKE_ERROR_TEXT='113: injected ambiguous failure' \
    "$runner" --print-state gui/0/ambiguous "$meta_root/ambiguous" || meta_status=$?
test "$meta_status" -eq 4 || fail 'status 113 without exact classification became absence'

meta_status=0
env DARK_FACTORY_LAUNCHD_PROBE_META_TEST=1 \
    DARK_FACTORY_LAUNCHD_LAUNCHCTL="$fake_launchctl" \
    DARK_FACTORY_LAUNCHD_PROBE_ATTEMPTS=2 \
    FAKE_PRINT_STATUS=77 FAKE_ERROR_TEXT='77: Operation not permitted' \
    "$runner" --wait-service-absent gui/0/denied "$meta_root/denied" \
    || meta_status=$?
test "$meta_status" -eq 1 || fail 'operational post-bootout query became absence'
test -f "$meta_root/denied.error" \
    || fail 'operational launchctl diagnostics were discarded'

meta_status=0
env DARK_FACTORY_LAUNCHD_PROBE_META_TEST=1 \
    DARK_FACTORY_LAUNCHD_PID_PROBE="$fake_pid_probe" \
    DARK_FACTORY_LAUNCHD_PROBE_ATTEMPTS=2 FAKE_PID_STATUS=0 \
    "$runner" --wait-pid-absent 42 || meta_status=$?
test "$meta_status" -eq 1 || fail 'EPERM-equivalent presence became PID absence'

env DARK_FACTORY_LAUNCHD_PROBE_META_TEST=1 \
    DARK_FACTORY_LAUNCHD_PID_PROBE="$fake_pid_probe" \
    DARK_FACTORY_LAUNCHD_PROBE_ATTEMPTS=2 FAKE_PID_STATUS=3 \
    "$runner" --wait-pid-absent 42 \
    || fail 'ESRCH-equivalent observation was not PID absence'

meta_status=0
env DARK_FACTORY_LAUNCHD_PROBE_META_TEST=1 \
    DARK_FACTORY_LAUNCHD_PID_PROBE="$fake_pid_probe" FAKE_PID_STATUS=4 \
    "$runner" --wait-pid-absent 42 || meta_status=$?
test "$meta_status" -eq 1 || fail 'unknown PID observation became absence'

grep -Fq '#[ignore = "opt-in: loads a randomized disposable launchd job"]' "$test_source" \
    || fail 'real launchd test is not opt-in'
grep -Fq 'LaunchdTarget::new(' "$test_source" \
    || fail 'test no longer injects an explicit launchd identity'
grep -Fq 'rollback_after_health_failure_for(' "$test_source" \
    || fail 'test no longer exercises the targeted rollback seam'

grep -Fq './scripts/test-macos-launchd-release-proof.sh' "$gate" \
    || fail 'local source gate lost the launchd safety checks'
macos_job=$(sed -n '/^  checks:/,/^  required:/p' "$workflow")
printf '%s\n' "$macos_job" | grep -Fq './scripts/macos-launchd-release-proof.sh' \
    || fail 'hosted macOS no longer runs the real launchd proof'
linux_job=$(sed -n '/^  linux:/,$p' "$workflow")
if printf '%s\n' "$linux_job" | grep -Fq './scripts/macos-launchd-release-proof.sh'; then
    fail 'Linux workflow invokes real launchctl proof'
fi

echo 'macOS launchd release proof static tests passed'
