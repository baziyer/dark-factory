#!/bin/sh
set -eu

repository_root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
gate="$repository_root/scripts/local-ci.sh"
contributing="$repository_root/CONTRIBUTING.md"
ci="$repository_root/.github/workflows/ci.yml"
runner_manifest="$repository_root/crates/factory-runner/Cargo.toml"
runner_library="$repository_root/crates/factory-runner/src/lib.rs"

fail() {
    echo "local-ci mode test failed: $*" >&2
    exit 1
}

grep -Fq 'cargo +1.88.0 fmt --all -- --check' "$gate" \
    || fail "Linux source mode lost rustfmt"
grep -Fq 'cargo +1.88.0 clippy --locked --workspace --all-targets --all-features -- -D warnings' "$gate" \
    || fail "Linux source mode lost clippy"
grep -Fq 'cargo +1.88.0 test --locked --workspace -- --test-threads=1' "$gate" \
    || fail "Linux source mode lost workspace tests"
grep -Fq 'git diff --check' "$gate" || fail "Linux source mode lost diff check"

runner_lib_section=$(awk '
    $0 == "[lib]" { in_lib = 1; next }
    /^\[/ { if (in_lib) exit }
    in_lib { print }
' "$runner_manifest")
if printf '%s\n' "$runner_lib_section" \
    | grep -Eq '^[[:space:]]*test[[:space:]]*=[[:space:]]*false'; then
    fail "factory-runner library unit tests are disabled"
fi
grep -Fq '#[cfg(test)]' "$runner_library" \
    || fail "factory-runner lost its substantive library unit tests"

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

linux_job=$(sed -n '/^  linux:/,$p' "$ci")
line_of() {
    printf '%s\n' "$linux_job" | grep -n -F "$1" | head -1 | cut -d: -f1
}
linux_gate_line=$(line_of 'name: Run the Linux authoritative gate')
linux_build_line=$(line_of 'name: Build the workspace binaries')
linux_smoke_line=$(line_of 'name: Run the source contributor smoke')
[ -n "$linux_gate_line" ] && [ -n "$linux_build_line" ] && [ -n "$linux_smoke_line" ] \
    || fail "Linux CI lost its gate, binary build, or smoke step"
[ "$linux_gate_line" -lt "$linux_build_line" ] \
    || fail "Linux CI rebuilds source-gate inputs after building smoke binaries"
[ "$linux_build_line" -lt "$linux_smoke_line" ] \
    || fail "Linux CI does not build its smoke binaries before using them"
printf '%s\n' "$linux_job" \
    | grep -Fq 'cargo +1.88.0 build --locked --workspace --bins' \
    || fail "Linux CI does not limit its post-gate build to smoke binaries"
if printf '%s\n' "$linux_job" | grep -Fq 'name: Check build headroom'; then
    fail "Linux CI duplicates the authoritative gate's build-headroom check"
fi

grep -Fxq './scripts/local-ci.sh' "$contributing" \
    || fail "CONTRIBUTING lost the macOS gate command"
grep -Fxq './scripts/local-ci.sh --linux-source' "$contributing" \
    || fail "CONTRIBUTING does not use the Linux source gate"
grep -Fxq './scripts/linux-contributor-smoke.sh' "$contributing" \
    || fail "CONTRIBUTING lost the Linux source smoke"

sh -n "$gate"
echo "local-ci mode tests passed"
