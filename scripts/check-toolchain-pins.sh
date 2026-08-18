#!/bin/sh
set -eu

workspace_version=$(sed -n 's/^rust-version = "\([^"]*\)"/\1/p' Cargo.toml)
case "$workspace_version" in
    *.*.*) toolchain_version=$workspace_version ;;
    *.*) toolchain_version="$workspace_version.0" ;;
    *) echo "could not read workspace rust-version from Cargo.toml" >&2; exit 1 ;;
esac

check_pin() {
    file=$1
    expected=$2
    if ! grep -Fq "$expected" "$file"; then
        echo "$file does not use workspace Rust $toolchain_version" >&2
        exit 1
    fi
}

check_pin scripts/local-ci.sh "cargo +$toolchain_version fmt"
check_pin scripts/local-ci.sh "cargo +$toolchain_version clippy"
check_pin scripts/local-ci.sh "cargo +$toolchain_version test"
check_pin scripts/launch-ui.sh "cargo +$toolchain_version build"
check_pin .github/workflows/ci.yml "rustup toolchain install $toolchain_version"
check_pin .github/workflows/release.yml "rustup toolchain install $toolchain_version"
check_pin .github/workflows/release.yml "cargo +$toolchain_version build"
