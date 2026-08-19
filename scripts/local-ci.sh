#!/bin/sh
set -eu

mode=${1:-macos}
test "$#" -le 1 || {
    echo "usage: scripts/local-ci.sh [macos|--linux-source]" >&2
    exit 2
}
case "$mode" in
    macos)
        script_dir=$(CDPATH= cd -- "$(dirname "$0")" && pwd)
        if [ "${DARK_FACTORY_LOCAL_CI_LEASE_HELD-}" != 1 ]; then
            exec "$script_dir/with-local-ci-lease.sh" "$script_dir/local-ci.sh"
        fi

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
    *)
        echo "unknown local-ci mode: $mode" >&2
        exit 2
        ;;
esac

# The authoritative source gate is shared by macOS and Linux. Keep this
# seam explicit so a platform mode cannot silently omit a core check.
./scripts/test-github-step-summary.sh
cargo +1.88.0 fmt --all -- --check
cargo +1.88.0 clippy --locked --workspace --all-targets --all-features -- -D warnings
cargo +1.88.0 test --locked --workspace --all-targets -- --test-threads=1
git diff --check
