#!/bin/sh
set -eu
repository_root=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
smoke="$repository_root/scripts/macos-contributor-smoke.sh"
owner="$repository_root/scripts/macos-smoke-daemon-owner.pl"
root=
fail() {
    echo "macOS causal teardown test failed: $*" >&2
    exit 1
}
wait_path() {
    path=$1
    label=$2
    attempt=0
    while ! test -e "$path"; do
        attempt=$((attempt + 1))
        test "$attempt" -lt 600 || {
            echo "$label did not appear" >&2
            return 1
        }
        sleep 0.02
    done
}
test "$(uname -s)" = Darwin || fail 'test requires Darwin'
# TERM is ignored, forcing the owner's bounded KILL+waitpid path.
root=$(mktemp -d /tmp/df-macos-owner-test.XXXXXX)
owner_state="$root/state"
ready="$root/child-ready"
child_fifo="$root/child.fifo"
child_closed="$root/child-closed"
mkdir "$owner_state"
mkfifo "$child_fifo"
wait_direct() {
    if test -e "$2"; then
        test ! -e "$2.failed"
        return
    fi
    wait_status=0
    wait "$1" || wait_status=$?
    : >"$2"
    test "$wait_status" -eq 0 || { : >"$2.failed"; return 1; }
}
cleanup_owner_fixture() {
    trap - EXIT HUP INT TERM
    cleanup_status=0
    if test -n "${owner_pid:-}"; then
        if ! test -e "$owner_state/stopped" \
            && ! test -e "$owner_state/reaped"; then
            : >"$owner_state/reap"
            wait_path "$owner_state/reaped" 'owner EXIT reap' \
                || cleanup_status=1
        fi
        if test -e "$owner_state/stopped" || test -e "$owner_state/reaped"; then
            wait_direct "$owner_pid" "$owner_state/owner-waited" \
                || cleanup_status=1
        else
            cleanup_status=1
        fi
    fi
    if test -n "${child_verifier_pid:-}"; then
        wait_path "$child_closed" 'bounded child self-exit' || cleanup_status=1
        wait_direct "$child_verifier_pid" "$child_closed.waited" \
            || cleanup_status=1
    fi
    if test "$cleanup_status" -eq 0; then
        rm -rf -- "$root"
    else
        echo "preserving failed owner fixture root: $root" >&2
    fi
    exit 1
}
trap cleanup_owner_fixture EXIT HUP INT TERM
/usr/bin/perl "$owner" --observe-fifo "$child_fifo" "$child_closed" 15 1 & # linger proves no post-marker signal
child_verifier_pid=$!
/usr/bin/perl "$owner" "$owner_state" "$$" "$root/reap-all" \
    /usr/bin/perl -e \
    '$SIG{TERM} = "IGNORE"; open my $f, ">", $ARGV[1] or die $!; open my $r, ">", $ARGV[0] or die $!; close $r or die $!; for (1 .. 200) { select undef, undef, undef, 0.05 }' \
    "$ready" "$child_fifo" &
owner_pid=$!
wait_path "$owner_state/owned-pid" 'owned child identity' || fail 'owner startup'
wait_path "$ready" 'TERM-ignoring child readiness' || fail 'child startup'
: >"$owner_state/stop"
wait_path "$owner_state/stopped" 'bounded owner escalation' || fail 'owner stop'
wait_direct "$owner_pid" "$owner_state/owner-waited" || fail 'owner wait'
wait_path "$child_closed" 'TERM-ignoring child close' || fail 'child close'
wait_direct "$child_verifier_pid" "$child_closed.waited" \
    || fail 'child verifier wait'
owner_pid=
child_verifier_pid=
rm -rf -- "$root"
root=
trap - EXIT HUP INT TERM
run_forced_fixture() {
    fixture=$1
    fault=$2
    expected_status=$3
    fixture_target=${4:-${CARGO_TARGET_DIR:-"$repository_root/target"}}
    root=$(mktemp -d /tmp/df-macos-smoke-test.XXXXXX)
    rmdir "$root"
    fixture_root=$root
    actual_status=0
    /usr/bin/env CARGO_TARGET_DIR="$fixture_target" \
        DARK_FACTORY_SMOKE_ROOT="$fixture_root" "$fault=1" "$smoke" \
        || actual_status=$?
    root=
    test ! -e "$fixture_root" \
        || { echo "$fixture failure left its scratch root" >&2; return 1; }
    test "$actual_status" -eq "$expected_status" \
        || { echo "$fixture exited $actual_status, expected $expected_status" >&2; return 1; }
}
missing_target=$(mktemp -d /tmp/df-macos-missing-target.XXXXXX)
rmdir "$missing_target"
if run_forced_fixture pre-scratch DARK_FACTORY_SMOKE_FAIL_DAEMON_DOWN 23 \
    "$missing_target"; then
    fail 'unrelated pre-scratch failure satisfied the fault fixture'
fi
run_forced_fixture daemon-down DARK_FACTORY_SMOKE_FAIL_DAEMON_DOWN 23 \
    || fail 'daemon-down fixture'
run_forced_fixture restart DARK_FACTORY_SMOKE_FAIL_RESTART 71 \
    || fail 'restart fixture'
echo 'macOS owner escalation and forced-failure teardown tests passed'
