#!/bin/sh
set -eu

./scripts/check-toolchain-pins.sh
./scripts/test-prepare-release-source.sh
./scripts/test-publish-release.sh
./scripts/test-package-release.sh
cargo +1.88.0 fmt --all -- --check
cargo +1.88.0 clippy --locked --workspace --all-targets --all-features -- -D warnings
cargo +1.88.0 test --locked --workspace --all-targets -- --test-threads=1
git diff --check
