#!/bin/sh
set -eu

cargo +1.85.0 fmt --all -- --check
cargo +1.85.0 clippy --locked --workspace --all-targets --all-features -- -D warnings
cargo +1.85.0 test --locked --workspace --all-targets -- --test-threads=1
git diff --check
