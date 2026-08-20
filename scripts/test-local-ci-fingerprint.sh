#!/bin/sh
set -eu

repository_root=$(CDPATH='' cd -- "$(dirname "$0")/.." && pwd)
actual=$("$repository_root/scripts/local-ci.sh" --fingerprint)
expected=$(printf '%s\n' \
    'lease-contract|./scripts/test-local-ci-lease.sh' \
    'lease-mutations|./scripts/test-local-ci-lease-mutations.sh' \
    'toolchain-pins|./scripts/check-toolchain-pins.sh' \
    'release-source|./scripts/test-prepare-release-source.sh' \
    'release-publish|./scripts/test-publish-release.sh' \
    'release-package|./scripts/test-package-release.sh' \
    'local-ci-environment|./scripts/test-local-ci-environment.sh' \
    'build-headroom|./scripts/check-build-headroom.sh' \
    'build-headroom-tests|./scripts/test-build-headroom.sh' \
    'workflow-summary|./scripts/test-github-step-summary.sh' \
    'phase-fingerprint|./scripts/test-local-ci-fingerprint.sh' \
    'binary-contract|./scripts/test-prepare-test-binaries.sh' \
    'fmt|cargo +1.88.0 fmt --all -- --check' \
    'clippy|cargo +1.88.0 clippy --locked --workspace --all-targets --all-features -- -D warnings' \
    'test-binaries|./scripts/prepare-test-binaries.sh' \
    'workspace-tests|cargo +1.88.0 test --locked --workspace --all-targets -- --test-threads=1' \
    'diff-check|git diff --check')

if [ "$actual" != "$expected" ]; then
    echo 'local-ci phase fingerprint changed:' >&2
    printf '%s\n' "$actual" >&2
    exit 1
fi

linux_actual=$("$repository_root/scripts/local-ci.sh" --fingerprint --linux-source)
linux_expected=$(printf '%s\n' \
    'toolchain-pins|./scripts/check-toolchain-pins.sh' \
    'local-ci-environment|./scripts/test-local-ci-environment.sh' \
    'build-headroom|./scripts/check-build-headroom.sh' \
    'build-headroom-tests|./scripts/test-build-headroom.sh' \
    'workflow-summary|./scripts/test-github-step-summary.sh' \
    'phase-fingerprint|./scripts/test-local-ci-fingerprint.sh' \
    'binary-contract|./scripts/test-prepare-test-binaries.sh' \
    'fmt|cargo +1.88.0 fmt --all -- --check' \
    'clippy|cargo +1.88.0 clippy --locked --workspace --all-targets --all-features -- -D warnings' \
    'test-binaries|./scripts/prepare-test-binaries.sh' \
    'workspace-tests|cargo +1.88.0 test --locked --workspace --all-targets -- --test-threads=1' \
    'diff-check|git diff --check')
if [ "$linux_actual" != "$linux_expected" ]; then
    echo 'Linux source phase fingerprint changed:' >&2
    printf '%s\n' "$linux_actual" >&2
    exit 1
fi

echo 'local-ci phase fingerprint passed'
