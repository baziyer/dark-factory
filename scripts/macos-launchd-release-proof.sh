#!/bin/sh
set -eu

launchctl_bin=${DARK_FACTORY_LAUNCHD_LAUNCHCTL:-/bin/launchctl}
pid_probe_bin=${DARK_FACTORY_LAUNCHD_PID_PROBE:-}
probe_attempts=${DARK_FACTORY_LAUNCHD_PROBE_ATTEMPTS:-200}
command_timeout=${DARK_FACTORY_LAUNCHD_COMMAND_TIMEOUT:-3}
case "$launchctl_bin" in /*) ;; *) echo 'launchctl path must be absolute' >&2; exit 2 ;; esac
test -x "$launchctl_bin" || { echo "launchctl is not executable: $launchctl_bin" >&2; exit 2; }
case "$probe_attempts:$command_timeout" in
    *[!0-9:]* | 0:* | *:0) echo 'probe limits must be positive integers' >&2; exit 2 ;;
esac
if test -n "$pid_probe_bin"; then
    case "$pid_probe_bin" in /*) ;; *) echo 'PID probe path must be absolute' >&2; exit 2 ;; esac
    test -x "$pid_probe_bin" || { echo "PID probe is not executable: $pid_probe_bin" >&2; exit 2; }
fi

# Retain and reap the exact direct command child. Status 124 is an internal
# timeout, never a launchctl absence classification.
run_bounded() {
    /usr/bin/perl -MTime::HiRes=time,sleep -MPOSIX=:sys_wait_h -e '
        my ($timeout, @command) = @ARGV;
        my $child = fork();
        defined $child or exit 125;
        if ($child == 0) {
            exec {$command[0]} @command;
            exit 127;
        }
        my $deadline = time() + $timeout;
        while (1) {
            my $waited = waitpid($child, WNOHANG);
            if ($waited == $child) {
                exit(($? & 127) ? 128 + ($? & 127) : $? >> 8);
            }
            exit 125 if $waited == -1;
            last if time() >= $deadline;
            sleep 0.02;
        }
        kill "TERM", $child;
        my $term_deadline = time() + 0.25;
        while (time() < $term_deadline) {
            exit 124 if waitpid($child, WNOHANG) == $child;
            sleep 0.02;
        }
        kill "KILL", $child;
        waitpid($child, 0);
        exit 124;
    ' "$command_timeout" "$@"
}

# Returns 0 only for a present service, 3 only for launchctl's documented
# not-found status, and 4 for every operational or classification failure.
launchctl_print_state() {
    print_service=$1
    print_output=$2
    print_error="$print_output.error"
    print_status=0
    run_bounded "$launchctl_bin" print "$print_service" \
        >"$print_output" 2>"$print_error" || print_status=$?
    if test "$print_status" -eq 0; then
        rm -f "$print_error"
        return 0
    fi
    classification_status=0
    classification=$(run_bounded "$launchctl_bin" error "$print_status" \
        2>>"$print_error") || classification_status=$?
    if test "$print_status" -eq 113 \
        && test "$classification_status" -eq 0 \
        && test "$classification" = '113: Could not find specified service'; then
        rm -f "$print_output" "$print_error"
        return 3
    fi
    echo "launchctl print failed for $print_service (status $print_status)" >&2
    test ! -s "$print_error" || sed 's/^/launchctl: /' "$print_error" >&2
    return 4
}

# Signal zero is observation-only. The helper preserves errno so only ESRCH is
# absence; EPERM is present and every other result is an operational failure.
pid_probe_state() {
    if test -n "$pid_probe_bin"; then
        "$pid_probe_bin" "$1"
        return
    fi
    /usr/bin/perl -MErrno=ESRCH,EPERM -e '
        my $pid = shift;
        exit 0 if kill 0, $pid;
        exit 0 if $! == EPERM;
        exit 3 if $! == ESRCH;
        warn "PID observation failed for $pid: $!\n";
        exit 4;
    ' "$1"
}

wait_pid_absent() {
    checked_pid=$1
    attempt=0
    pid_deadline=${cleanup_deadline_epoch:-$(($(date +%s) + 10))}
    while :; do
        pid_status=0
        pid_probe_state "$checked_pid" || pid_status=$?
        case "$pid_status" in
            0) ;;
            3) return 0 ;;
            *) return 1 ;;
        esac
        attempt=$((attempt + 1))
        test "$attempt" -lt "$probe_attempts" || return 1
        test "$(date +%s)" -lt "$pid_deadline" || return 1
        sleep 0.05
    done
}

wait_service_absent() {
    absent_service=$1
    absent_output=$2
    attempt=0
    service_deadline=${cleanup_deadline_epoch:-$(($(date +%s) + 10))}
    while :; do
        service_status=0
        launchctl_print_state "$absent_service" "$absent_output" || service_status=$?
        case "$service_status" in
            0) ;;
            3) return 0 ;;
            *) return 1 ;;
        esac
        attempt=$((attempt + 1))
        test "$attempt" -lt "$probe_attempts" || return 1
        test "$(date +%s)" -lt "$service_deadline" || return 1
        sleep 0.05
    done
}

if test "${DARK_FACTORY_LAUNCHD_PROBE_META_TEST:-0}" = 1; then
    case "${1:-}" in
        --print-state) launchctl_print_state "$2" "$3"; exit $? ;;
        --wait-service-absent) wait_service_absent "$2" "$3"; exit $? ;;
        --wait-pid-absent) wait_pid_absent "$2"; exit $? ;;
        *) echo 'unknown launchd probe meta-test' >&2; exit 2 ;;
    esac
fi

mode=${1:-run}
test "$#" -le 1 || {
    echo 'usage: scripts/macos-launchd-release-proof.sh [--fail-after-second]' >&2
    exit 2
}
case "$mode" in
    run) fail_after_second=0 ;;
    --fail-after-second) fail_after_second=1 ;;
    *) echo "unknown launchd proof mode: $mode" >&2; exit 2 ;;
esac

test "$(uname -s)" = Darwin || {
    echo 'disposable launchd release proof requires macOS' >&2
    exit 2
}

repository_root=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
target_root=${CARGO_TARGET_DIR:-"$repository_root/target"}
source_dir="$target_root/debug"
for binary in factoryd factory-runner factoryctl factory-tui; do
    test -x "$source_dir/$binary" || {
        echo "missing source-built $source_dir/$binary" >&2
        exit 1
    }
done
source_dir=$(CDPATH='' cd -- "$source_dir" && pwd -P)

operator_home=${HOME:?HOME must identify the operator before fixture isolation}
host_cargo_home=${CARGO_HOME:-"$operator_home/.cargo"}
host_rustup_home=${RUSTUP_HOME:-"$operator_home/.rustup"}
cargo=$(command -v cargo)
cargo_bin=$(dirname "$cargo")
safe_path="$cargo_bin:/usr/bin:/bin:/usr/sbin:/sbin"
for provider in claude codex; do
    if PATH="$safe_path" command -v "$provider" >/dev/null 2>&1; then
        echo "$provider must be absent from the launchd fixture PATH" >&2
        exit 1
    fi
done

root=$(mktemp -d /tmp/df-launchd-release.XXXXXX)
chmod 700 "$root"
mkdir "$root/user-home" "$root/factory-home"
chmod 700 "$root/user-home" "$root/factory-home"
suffix=$(basename "$root" | tr -cd '[:alnum:]')
uid=$(/usr/bin/id -u)
domain="gui/$uid"
label="com.dark-factory.fixture.$suffix"
service="$domain/$label"
live_service="$domain/com.dark-factory.factoryd"
live_plist="$operator_home/Library/LaunchAgents/com.dark-factory.factoryd.plist"

snapshot_live_install() {
    destination=$1
    live_status=0
    launchctl_print_state "$live_service" "$destination.launchctl" || live_status=$?
    case "$live_status" in
        0)
            {
                echo loaded
                sed -n -e '/^[[:space:]]*path = /p' \
                    -e '/^[[:space:]]*program = /p' \
                    -e '/^[[:space:]]*pid = /p' "$destination.launchctl"
            } >"$destination.job"
            ;;
        3) echo absent >"$destination.job" ;;
        *) return 1 ;;
    esac
    if test -L "$live_plist"; then
        printf 'symlink %s\n' "$(readlink "$live_plist")" >"$destination.plist"
    elif test -f "$live_plist"; then
        /usr/bin/stat -f 'file %d:%i %Sp %z' "$live_plist" >"$destination.plist"
        /usr/bin/shasum -a 256 "$live_plist" >>"$destination.plist"
    elif test -e "$live_plist"; then
        /usr/bin/stat -f 'other %d:%i %HT %Sp %z' "$live_plist" >"$destination.plist"
    else
        echo absent >"$destination.plist"
    fi
    rm -f "$destination.launchctl"
}

snapshot_live_install "$root/live-before" || {
    echo "preserving unresolved launchd fixture root: $root" >&2
    exit 1
}
collision_status=0
launchctl_print_state "$service" "$root/collision.launchctl" || collision_status=$?
case "$collision_status" in
    0)
        echo "randomized launchd label unexpectedly already exists: $label" >&2
        rm -rf -- "$root"
        exit 1
        ;;
    3) ;;
    *) echo "preserving unresolved launchd fixture root: $root" >&2; exit 1 ;;
esac

# shellcheck disable=SC2329 # invoked by the external verifier
verify_cleanup() {
    cleanup_status=0
    cleanup_deadline_epoch=$(($(date +%s) + 30))
    service_status=0
    launchctl_print_state "$service" "$root/cleanup-before.launchctl" \
        || service_status=$?
    case "$service_status" in
        0) run_bounded "$launchctl_bin" bootout "$service" \
            >"$root/bootout.out" 2>"$root/bootout.error" || cleanup_status=1 ;;
        3) ;;
        *) cleanup_status=1 ;;
    esac
    wait_service_absent "$service" "$root/cleanup-residue.launchctl" \
        || cleanup_status=1
    if test -f "$root/observed-pids"; then
        while IFS= read -r pid; do
            case "$pid" in
                '' | *[!0-9]* | 0 | 1) cleanup_status=1 ;;
                *) wait_pid_absent "$pid" || cleanup_status=1 ;;
            esac
        done <"$root/observed-pids"
    fi
    snapshot_live_install "$root/live-after" || cleanup_status=1
    cmp -s "$root/live-before.job" "$root/live-after.job" || cleanup_status=1
    cmp -s "$root/live-before.plist" "$root/live-after.plist" || cleanup_status=1
    if test "$cleanup_status" -eq 0; then
        rm -rf -- "$root"
        test ! -e "$root" || cleanup_status=1
    else
        echo "preserving unresolved launchd fixture root: $root" >&2
    fi
    test "$cleanup_status" -eq 0
}

mkfifo "$root/verifier.fifo"
(
    trap '' HUP INT TERM
    while IFS= read -r _; do :; done <"$root/verifier.fifo"
    verify_cleanup
) &
verifier_pid=$!
exec 9>"$root/verifier.fifo"

# shellcheck disable=SC2329 # invoked by trap
finish() {
    status=$?
    trap - EXIT HUP INT TERM
    exec 9>&-
    verifier_status=0
    wait "$verifier_pid" || verifier_status=$?
    if test "$verifier_status" -eq 0; then
        if test "$status" -eq 0; then
            echo 'disposable launchd release proof passed with exact external teardown'
        else
            echo "external launchd teardown passed after fixture status $status" >&2
        fi
    fi
    test "$verifier_status" -eq 0 || exit 1
    exit "$status"
}
trap finish EXIT
trap 'exit 129' HUP
trap 'exit 130' INT
trap 'exit 143' TERM

test_status=0
/usr/bin/env \
    HOME="$root/user-home" \
    DARK_FACTORY_HOME="$root/factory-home" \
    DARK_FACTORY_LAUNCHD_RELEASE_PROOF=1 \
    DARK_FACTORY_LAUNCHD_FIXTURE_ROOT="$root" \
    DARK_FACTORY_LAUNCHD_LABEL="$label" \
    DARK_FACTORY_LAUNCHD_SOURCE_DIR="$source_dir" \
    DARK_FACTORY_LAUNCHD_SAFE_PATH="$safe_path" \
    DARK_FACTORY_LAUNCHD_FAIL_AFTER_SECOND="$fail_after_second" \
    CARGO_HOME="$host_cargo_home" \
    RUSTUP_HOME="$host_rustup_home" \
    PATH="$safe_path" \
    "$cargo" +1.88.0 test --locked -p factoryctl --test launchd_release \
        disposable_launchd_release_replacement -- \
        --ignored --exact --test-threads=1 9>&- || test_status=$?

if test "$test_status" -eq 0; then
    for marker in first-live second-live crash-observed rollback-live; do
        test -f "$root/$marker" || {
            echo "launchd proof omitted $marker" >&2
            test_status=1
        }
    done
fi
exit "$test_status"
