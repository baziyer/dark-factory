#!/bin/sh
set -eu

script_dir=$(CDPATH= cd -- "$(dirname "$0")" && pwd)
if [ "${DARK_FACTORY_LOCAL_CI_LEASE_HELD-}" != 1 ]; then
    exec "$script_dir/with-local-ci-lease.sh" "$script_dir/local-ci.sh"
fi

./scripts/test-local-ci-lease.sh
./scripts/test-local-ci-lease-mutations.sh
./scripts/check-toolchain-pins.sh
./scripts/test-prepare-release-source.sh
./scripts/test-publish-release.sh
./scripts/test-package-release.sh
cargo +1.88.0 fmt --all -- --check
cargo +1.88.0 clippy --locked --workspace --all-targets --all-features -- -D warnings
cargo +1.88.0 test --locked --workspace --all-targets -- --test-threads=1
git diff --check
