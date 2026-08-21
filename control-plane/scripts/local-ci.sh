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

for ignore_file in .gitignore .vercelignore; do
    for pattern in .env '.env.*' '!.env.example' '.vercel/'; do
        grep -Fqx -- "$pattern" "$ignore_file"
    done
done
for ignored_path in .env .env.production .vercel/project.json; do
    git check-ignore -q --no-index "$ignored_path"
done
if git check-ignore -q --no-index .env.example; then
    echo ".env.example must remain trackable" >&2
    exit 1
fi

git diff --check -- .
