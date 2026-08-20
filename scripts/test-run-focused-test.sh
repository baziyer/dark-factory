#!/bin/sh
set -eu

script_dir=$(CDPATH='' cd -- "$(dirname "$0")" && pwd -P)
wrapper=$script_dir/run-focused-test.sh
temporary=$(mktemp -d "${TMPDIR:-/tmp}/dark-factory-focused-contract.XXXXXX")
trap 'rm -rf "$temporary"' EXIT HUP INT TERM

fail() {
    echo "focused-test contract failed: $*" >&2
    exit 1
}

if CARGO_BUILD_TARGET=custom-target "$wrapper" --linux-source true \
    >"$temporary/target.out" 2>"$temporary/target.err"; then
    fail 'custom target configuration was accepted'
fi

mkdir "$temporary/cargo-home"
printf '%s\n' '[build]' 'target = "custom-target"' >"$temporary/cargo-home/config.toml"
if CARGO_HOME="$temporary/cargo-home" "$wrapper" --linux-source true \
    >"$temporary/config.out" 2>"$temporary/config.err"; then
    fail 'custom Cargo configuration was accepted'
fi

echo 'focused-test contract execution passed'
