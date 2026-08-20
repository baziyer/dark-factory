#!/bin/sh
set -eu

script_dir=$(CDPATH='' cd -- "$(dirname "$0")" && pwd)
fingerprint_only=0
mode=${1:-macos}
if [ "$mode" = "--fingerprint" ]; then
    fingerprint_only=1
    mode=${2:-macos}
    test "$#" -le 2 || {
        echo "usage: scripts/local-ci.sh [macos|--linux-source|--fingerprint [macos|--linux-source]]" >&2
        exit 2
    }
else
    test "$#" -le 1 || {
        echo "usage: scripts/local-ci.sh [macos|--linux-source]" >&2
        exit 2
    }
fi

local_ci_wall_clock_seconds() {
    date +%s
}

local_ci_phase() {
    phase_name=$1
    shift
    if [ "$fingerprint_only" -eq 1 ]; then
        printf '%s|%s\n' "$phase_name" "$*"
        return 0
    fi
    phase_started=$(local_ci_wall_clock_seconds)
    printf 'local-ci phase=%s start_s=%s\n' "$phase_name" "$phase_started" >&2
    if "$@"; then
        phase_status=0
    else
        phase_status=$?
    fi
    phase_finished=$(local_ci_wall_clock_seconds)
    printf 'local-ci phase=%s elapsed_s=%s status=%s\n' \
        "$phase_name" "$((phase_finished - phase_started))" "$phase_status" >&2
    return "$phase_status"
}

case "$mode" in
    macos|--linux-source) ;;
    *)
        echo "unknown local-ci mode: $mode" >&2
        exit 2
        ;;
esac

if [ "$fingerprint_only" -eq 0 ] && [ "$mode" = "macos" ] && [ "${DARK_FACTORY_LOCAL_CI_LEASE_HELD-}" != 1 ]; then
    exec "$script_dir/with-local-ci-lease.sh" "$script_dir/local-ci.sh"
fi

# Keep lease diagnostics attributable to the invoking live task, then make
# every gate child independent of that task's runtime and identity overrides.
# shellcheck source=scripts/local-ci-environment.sh
. "$script_dir/local-ci-environment.sh"

if [ "$fingerprint_only" -eq 0 ]; then
    local_ci_gate_started_s=$(local_ci_wall_clock_seconds)
    local_ci_report_total() {
        local_ci_status=$?
        trap - EXIT
        local_ci_gate_finished_s=$(local_ci_wall_clock_seconds)
        printf 'local-ci total_elapsed_s=%s status=%s\n' \
            "$((local_ci_gate_finished_s - local_ci_gate_started_s))" "$local_ci_status" >&2
        exit "$local_ci_status"
    }
    trap local_ci_report_total EXIT
fi

case "$mode" in
    macos)
        local_ci_phase lease-contract ./scripts/test-local-ci-lease.sh
        local_ci_phase lease-mutations ./scripts/test-local-ci-lease-mutations.sh
        local_ci_phase toolchain-pins ./scripts/check-toolchain-pins.sh
        # These fixtures exercise the macOS release/publisher contract. They
        # are intentionally outside the Linux source-preview gate: Linux
        # archive and installer support belongs to #142/#143.
        local_ci_phase release-source ./scripts/test-prepare-release-source.sh
        local_ci_phase release-publish ./scripts/test-publish-release.sh
        local_ci_phase release-package ./scripts/test-package-release.sh
        ;;
    --linux-source)
        local_ci_phase toolchain-pins ./scripts/check-toolchain-pins.sh
        ;;
esac

# Measure after the macOS repository lease is held, so another linked worktree
# cannot begin a broad gate between this read-only preflight and our compile.
local_ci_phase local-ci-environment ./scripts/test-local-ci-environment.sh
local_ci_phase build-headroom ./scripts/check-build-headroom.sh
local_ci_phase build-headroom-tests ./scripts/test-build-headroom.sh

# The authoritative source gate is shared by macOS and Linux. Keep this
# seam explicit so a platform mode cannot silently omit a core check.
local_ci_phase workflow-summary ./scripts/test-github-step-summary.sh
local_ci_phase phase-fingerprint ./scripts/test-local-ci-fingerprint.sh
local_ci_phase focused-contract ./scripts/test-run-focused-test.sh
if [ "$mode" = macos ]; then
    local_ci_phase focused-lease ./scripts/test-focused-binary-lease.sh
fi
local_ci_phase fmt cargo +1.88.0 fmt --all -- --check
local_ci_phase clippy cargo +1.88.0 clippy --locked --workspace --all-targets --all-features -- -D warnings
local_ci_phase focused-tests ./scripts/run-focused-test.sh "$mode" cargo +1.88.0 test --locked --workspace --all-targets -- --test-threads=1
local_ci_phase diff-check git diff --check
