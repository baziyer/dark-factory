#!/bin/sh
set -eu

repository_root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
gate="$repository_root/scripts/local-ci.sh"

fail() {
    echo "local-ci mode test failed: $*" >&2
    exit 1
}

grep -Fq 'cargo +1.88.0 fmt --all -- --check' "$gate" \
    || fail "Linux source mode lost rustfmt"
grep -Fq 'cargo +1.88.0 clippy --locked --workspace --all-targets --all-features -- -D warnings' "$gate" \
    || fail "Linux source mode lost clippy"
grep -Fq 'cargo +1.88.0 test --locked --workspace --all-targets -- --test-threads=1' "$gate" \
    || fail "Linux source mode lost workspace tests"
grep -Fq 'git diff --check' "$gate" || fail "Linux source mode lost diff check"

linux_mode=$(sed -n '/^[[:space:]]*--linux-source)/,/^[[:space:]]*;;/p' "$gate")
printf '%s\n' "$linux_mode" | grep -Fq './scripts/check-toolchain-pins.sh' \
    || fail "Linux source mode lost toolchain pin validation"
for mac_fixture in test-prepare-release-source.sh test-publish-release.sh test-package-release.sh; do
    if printf '%s\n' "$linux_mode" | grep -Fq "$mac_fixture"; then
        fail "Linux source mode invokes macOS fixture $mac_fixture"
    fi
done

macos_mode=$(sed -n '/^[[:space:]]*macos)/,/^[[:space:]]*;;/p' "$gate")
for mac_fixture in test-prepare-release-source.sh test-publish-release.sh test-package-release.sh; do
    printf '%s\n' "$macos_mode" | grep -Fq "$mac_fixture" \
        || fail "macOS mode lost fixture $mac_fixture"
done

sh -n "$gate"
echo "local-ci mode tests passed"
