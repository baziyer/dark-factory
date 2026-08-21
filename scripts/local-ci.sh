#!/bin/sh
set -eu

script_dir=$(CDPATH='' cd -- "$(dirname "$0")" && pwd)
mode=${1:-macos}
test "$#" -le 1 || {
    echo "usage: scripts/local-ci.sh [macos|--linux-source]" >&2
    exit 2
}
case "$mode" in
    macos)
        if [ "${DARK_FACTORY_LOCAL_CI_LEASE_HELD-}" != 1 ]; then
            exec "$script_dir/with-local-ci-lease.sh" "$script_dir/local-ci.sh"
        fi
        ;;
    --linux-source) ;;
    *)
        echo "unknown local-ci mode: $mode" >&2
        exit 2
        ;;
esac

# Keep lease diagnostics attributable to the invoking live task, then make
# every gate child independent of that task's runtime and identity overrides.
# shellcheck source=scripts/local-ci-environment.sh
. "$script_dir/local-ci-environment.sh"

case "$mode" in
    macos)
        ./scripts/test-local-ci-lease.sh
        ./scripts/test-local-ci-lease-mutations.sh
        ./scripts/check-toolchain-pins.sh
        # These fixtures exercise the macOS release/publisher contract. They
        # are intentionally outside the Linux source-preview gate: Linux
        # archive and installer support belongs to #142/#143.
        ./scripts/test-prepare-release-source.sh
        ./scripts/test-publish-release.sh
        ./scripts/test-package-release.sh
        ;;
    --linux-source)
        ./scripts/check-toolchain-pins.sh
        ;;
esac

# Measure after the macOS repository lease is held, so another linked worktree
# cannot begin a broad gate between this read-only preflight and our compile.
./scripts/test-local-ci-environment.sh
./scripts/test-new-worktree.sh
./scripts/check-build-headroom.sh
./scripts/test-build-headroom.sh

# The authoritative source gate is shared by macOS and Linux. Keep this
# seam explicit so a platform mode cannot silently omit a core check.
./scripts/test-github-step-summary.sh
cargo +1.88.0 fmt --all -- --check
cargo +1.88.0 clippy --locked --workspace --all-targets --all-features -- -D warnings
cargo +1.88.0 test --locked --workspace --all-targets -- --test-threads=1
git diff --check
