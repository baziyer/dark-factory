#!/bin/sh
set -eu

. "$(CDPATH= cd -- "$(dirname "$0")" && pwd)/local-ci-lease.sh"

local_ci_cleanup() {
    local_ci_status=$?
    trap - EXIT HUP INT TERM
    local_ci_lease_release || true
    exit "$local_ci_status"
}

local_ci_signal() {
    local_ci_signal_status=$1
    trap - EXIT HUP INT TERM
    local_ci_lease_release || true
    exit "$local_ci_signal_status"
}

trap local_ci_cleanup EXIT
trap 'local_ci_signal 129' HUP
trap 'local_ci_signal 130' INT
trap 'local_ci_signal 143' TERM

local_ci_lease_acquire

./scripts/test-local-ci-lease.sh
./scripts/check-toolchain-pins.sh
./scripts/test-prepare-release-source.sh
./scripts/test-publish-release.sh
./scripts/test-package-release.sh
cargo +1.88.0 fmt --all -- --check
cargo +1.88.0 clippy --locked --workspace --all-targets --all-features -- -D warnings
cargo +1.88.0 test --locked --workspace --all-targets -- --test-threads=1
git diff --check
