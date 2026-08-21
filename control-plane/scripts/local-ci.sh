#!/bin/sh
set -eu

script_dir=$(CDPATH='' cd -- "$(dirname "$0")" && pwd)
repo_root=$(CDPATH='' cd -- "$script_dir/.." && pwd)
cd "$repo_root"

cargo +1.88.0 fmt --all -- --check
cargo +1.88.0 clippy --locked --all-targets --all-features -- -D warnings

# The default lane proves the production feature set remains fail-closed.
cargo +1.88.0 test --locked --all-targets

# SQLite exists only to make the webhook's durable replay behavior causal in
# local tests. Production adapters are still compiled by the all-features
# clippy gate above; they must not silently replace this dedicated test lane.
cargo +1.88.0 test --locked --all-targets --features development-sqlite

git diff --check -- .
