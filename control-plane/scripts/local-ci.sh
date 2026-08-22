#!/bin/sh
set -eu

script_dir=$(CDPATH='' cd -- "$(dirname "$0")" && pwd)
repo_root=$(CDPATH='' cd -- "$script_dir/.." && pwd)
cd "$repo_root"

if find . -maxdepth 1 -type f \( -name '.env*' -o -name '.dev.vars*' \) | grep -q .; then
    echo "local environment files are forbidden in the control-plane tree" >&2
    exit 1
fi

cargo +1.88.0 fmt --all -- --check
cargo +1.88.0 clippy --locked --all-targets --all-features -- -D warnings

# The default lane proves an unconfigured native build has no webhook route.
cargo +1.88.0 test --locked --all-targets

# Native SQLite is a fast causal model of the same exact replay contract. It is
# never a production adapter and deliberately cannot make readiness succeed.
cargo +1.88.0 test --locked --all-targets --features development-sqlite

rustup target add wasm32-unknown-unknown --toolchain 1.88.0
cargo +1.88.0 clippy --locked --lib --target wasm32-unknown-unknown -- -D warnings

worker_build="$repo_root/.tools/bin/worker-build"
if ! "$worker_build" --version 2>/dev/null | grep -Fqx '0.8.5'; then
    cargo +1.88.0 install worker-build --version 0.8.5 --locked --root "$repo_root/.tools"
fi
PATH="$repo_root/.tools/bin:$PATH"
export PATH

node -e "if (Number(process.versions.node.split('.')[0]) < 22) process.exit(1)"
npm ci --ignore-scripts
worker-build --release

dry_run_dir=$(mktemp -d "${TMPDIR:-/tmp}/df-wrangler-dry-run.XXXXXX")
cleanup() {
    rm -rf -- "$dry_run_dir"
}
trap cleanup EXIT HUP INT TERM
npx wrangler deploy --dry-run --outdir "$dry_run_dir"
npm run test:worker

for ignore_file in ../.gitignore .gitignore; do
    for pattern in '.env' '.env.*' '.dev.vars' '.dev.vars.*' '.wrangler/' 'node_modules/' '.tools/'; do
        grep -Fqx -- "$pattern" "$ignore_file"
    done
done
for ignored_path in .env .env.production .dev.vars .dev.vars.production .wrangler/state node_modules/wrangler; do
    git check-ignore -q --no-index "$ignored_path"
done

git diff --check -- .
