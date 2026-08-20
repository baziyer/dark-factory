#!/bin/sh
set -eu

script_dir=$(CDPATH='' cd -- "$(dirname "$0")" && pwd)

if [ "${DARK_FACTORY_LOCAL_CI_LEASE_HELD-}" != 1 ]; then
    exec "$script_dir/with-local-ci-lease.sh" "$script_dir/prepare-test-binaries.sh"
fi

# Keep the build independent of the live daemon and provider session.
# shellcheck source=scripts/local-ci-environment.sh
. "$script_dir/local-ci-environment.sh"

repository_root=$(CDPATH='' cd -- "$script_dir/.." && pwd)
target_dir=${CARGO_TARGET_DIR:-$repository_root/target}
case "$target_dir" in
    /*) ;;
    *) target_dir=$repository_root/$target_dir ;;
esac

CDPATH='' cd -- "$repository_root"
cargo +1.88.0 test --locked --workspace --all-targets --no-run

build_dir=$target_dir/debug
expected_dir=$(CDPATH='' cd -- "$build_dir" && pwd -P)
for name in factory-runner factoryctl; do
    path=$build_dir/$name
    [ -x "$path" ] || {
        echo "prepared $name binary is missing at $path" >&2
        exit 1
    }
    actual_dir=$(CDPATH='' cd -- "$(dirname "$path")" && pwd -P)
    [ "$actual_dir" = "$expected_dir" ] || {
        echo "prepared $name binary escaped the exact target directory" >&2
        exit 1
    }
done

printf 'prepared test binaries: target=%s factory-runner=%s factoryctl=%s\n' \
    "$expected_dir" "$expected_dir/factory-runner" "$expected_dir/factoryctl"
