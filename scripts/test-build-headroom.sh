#!/bin/sh
set -eu

repository_root=$(CDPATH= cd -- "$(dirname "$0")/.." && pwd)
preflight=$repository_root/scripts/check-build-headroom.sh
temporary=$(mktemp -d "${TMPDIR:-/tmp}/dark-factory-build-headroom.XXXXXX")
trap 'rm -rf "$temporary"' EXIT HUP INT TERM

fail() {
    echo "build headroom test failed: $*" >&2
    exit 1
}

fake_bin=$temporary/bin
target=$temporary/target
mkdir -p "$fake_bin" "$target"

cat >"$fake_bin/df" <<'EOF'
#!/bin/sh
test "$1" = -Pk
[ -z "${DF_FAKE_MEASUREMENT_MARKER-}" ] || : >"$DF_FAKE_MEASUREMENT_MARKER"
printf '%s\n' 'Filesystem 1024-blocks Used Available Capacity Mounted on'
printf 'fixture 20000000 1 %s 1%% /fixture\n' "$DF_FAKE_FREE_KIB"
EOF
cat >"$fake_bin/du" <<'EOF'
#!/bin/sh
test "$1" = -sk
[ -z "${DF_FAKE_MEASUREMENT_MARKER-}" ] || : >"$DF_FAKE_MEASUREMENT_MARKER"
printf '%s\t%s\n' "$DF_FAKE_TARGET_KIB" "$2"
EOF
chmod +x "$fake_bin/df" "$fake_bin/du"

run_preflight() {
    free_kib=$1
    target_kib=$2
    output=$3
    summary=$4
    PATH="$fake_bin:$PATH" \
    DF_FAKE_FREE_KIB=$free_kib \
    DF_FAKE_TARGET_KIB=$target_kib \
    CARGO_TARGET_DIR=$target \
    GITHUB_STEP_SUMMARY=$summary \
        "$preflight" >"$output" 2>&1
}

# The documented boundary is inclusive: exactly 12 GiB is enough, one KiB
# less fails before any Cargo process starts.
pass_output=$temporary/pass.output
pass_summary=$temporary/pass.summary
: >"$pass_summary"
run_preflight 12582912 4096 "$pass_output" "$pass_summary" \
    || fail "the exact threshold did not pass"
grep -F 'free_bytes=12884901888' "$pass_output" >/dev/null \
    || fail "pass output omitted exact free bytes"
grep -F 'target_allocated_bytes=4194304' "$pass_output" >/dev/null \
    || fail "pass output omitted exact target bytes"
grep -F 'Result: <code>success</code>' "$pass_summary" >/dev/null \
    || fail "pass summary omitted its result"

fail_output=$temporary/fail.output
fail_summary=$temporary/fail.summary
: >"$fail_summary"
if run_preflight 12582911 8192 "$fail_output" "$fail_summary"; then
    fail "one KiB below the threshold passed"
fi
grep -F 'free_bytes=12884900864' "$fail_output" >/dev/null \
    || fail "failure output omitted exact free bytes"
grep -F 'target_allocated_bytes=8388608' "$fail_output" >/dev/null \
    || fail "failure output omitted exact target bytes"
grep -F 'free at least 1024 more bytes' "$fail_output" >/dev/null \
    || fail "failure output omitted the exact actionable deficit"
grep -F 'no files were changed' "$fail_output" >/dev/null \
    || fail "failure output did not state its read-only contract"
grep -F 'Result: <code>failure</code>' "$fail_summary" >/dev/null \
    || fail "failure summary omitted its result"

# An explicitly empty target is invalid to Cargo and must not be mistaken for
# the unset/default target. Refuse before either measurement command runs.
empty_output=$temporary/empty.output
empty_measurement=$temporary/empty.measurement
if PATH="$fake_bin:$PATH" \
    DF_FAKE_MEASUREMENT_MARKER=$empty_measurement \
    CARGO_TARGET_DIR= \
    "$preflight" >"$empty_output" 2>&1; then
    fail "an explicitly empty Cargo target passed"
fi
grep -F 'CARGO_TARGET_DIR must not be empty' "$empty_output" >/dev/null \
    || fail "an empty Cargo target lacked its fail-closed reason"
[ ! -e "$empty_measurement" ] \
    || fail "an empty Cargo target invoked df or du"
if grep -F "target=$repository_root/target" "$empty_output" >/dev/null; then
    fail "an empty Cargo target measured the repository default"
fi

# Malformed platform output fails closed rather than accidentally treating an
# unavailable measurement as zero or enough space.
malformed_output=$temporary/malformed.output
if run_preflight unavailable 0 "$malformed_output" /dev/null; then
    fail "a malformed df measurement passed"
fi
grep -F 'free-space measurement was not a valid' "$malformed_output" >/dev/null \
    || fail "malformed df output lacked a durable reason"

real_target=$temporary/real-target
symlink_target=$temporary/symlink-target
mkdir "$real_target"
ln -s "$real_target" "$symlink_target"
symlink_output=$temporary/symlink.output
if PATH="$fake_bin:$PATH" CARGO_TARGET_DIR=$symlink_target \
    "$preflight" >"$symlink_output" 2>&1; then
    fail "a symbolic-link Cargo target passed"
fi
grep -F 'refusing symbolic-link Cargo target path' "$symlink_output" >/dev/null \
    || fail "symbolic-link rejection lacked a durable reason"

# A new target is valid and reports zero allocation while df probes its
# existing parent; du must not be invoked for it.
new_target=$temporary/not-created/target
mkdir "$temporary/not-created"
new_output=$temporary/new.output
PATH="$fake_bin:$PATH" \
DF_FAKE_FREE_KIB=12582912 \
DF_FAKE_TARGET_KIB=invalid \
CARGO_TARGET_DIR=$new_target \
    "$preflight" >"$new_output" 2>&1 \
    || fail "a not-yet-created target did not pass"
grep -F 'target_allocated_bytes=0' "$new_output" >/dev/null \
    || fail "a new target did not report zero allocated bytes"

line_of() {
    pattern=$1
    file=$2
    grep -n -F "$pattern" "$file" | head -1 | cut -d: -f1
}

gate=$repository_root/scripts/local-ci.sh
preflight_line=$(line_of './scripts/check-build-headroom.sh' "$gate")
clippy_line=$(line_of 'cargo +1.88.0 clippy' "$gate")
[ "$preflight_line" -lt "$clippy_line" ] \
    || fail "local-ci does not preflight before compilation"

ci=$repository_root/.github/workflows/ci.yml
linux_job=$temporary/linux-job.yml
sed -n '/^  linux:/,$p' "$ci" >"$linux_job"
linux_gate_line=$(line_of 'name: Run the Linux authoritative gate' "$linux_job")
linux_build_line=$(line_of 'name: Build the workspace binaries' "$linux_job")
[ "$linux_gate_line" -lt "$linux_build_line" ] \
    || fail "Linux CI does not run its preflighting source gate before its build"
if grep -Fq 'name: Check build headroom' "$linux_job"; then
    fail "Linux CI duplicates the source gate's build-headroom preflight"
fi

release=$repository_root/.github/workflows/release.yml
release_preflight_line=$(line_of 'name: Check build headroom' "$release")
release_build_line=$(line_of 'name: Build release binaries' "$release")
[ "$release_preflight_line" -lt "$release_build_line" ] \
    || fail "release CI does not preflight before its build"

sh -n "$preflight" "$repository_root/scripts/local-ci.sh"
echo "build headroom tests passed"
