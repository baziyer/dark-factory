#!/bin/sh
set -eu

repository_root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
publisher="$repository_root/scripts/publish-release.sh"
fake_gh="$repository_root/scripts/test-fixtures/fake-release-gh.sh"
temporary=$(mktemp -d "${TMPDIR:-/tmp}/dark-factory-publish-test.XXXXXX")
trap 'rm -rf "$temporary"' EXIT HUP INT TERM
mkdir -p "$temporary/bin" "$temporary/dist"
ln -s "$fake_gh" "$temporary/bin/gh"
ln -s "$fake_gh" "$temporary/bin/sleep"
for name in archive.tar.gz SHA256SUMS latest.json; do
    printf 'fixture %s\n' "$name" >"$temporary/dist/$name"
done

fail() {
    echo "publish-release test failed: $*" >&2
    exit 1
}

assert_equal() {
    expected=$1
    actual=$2
    label=$3
    [ "$actual" = "$expected" ] || fail "$label: expected $expected, got $actual"
}

count_log() {
    prefix=$1
    log=$2
    awk -v prefix="$prefix" 'index($0, prefix) == 1 { count++ } END { print count + 0 }' "$log"
}

run_publisher() {
    scenario=$1
    state=$2
    stdout=$3
    stderr=$4
    PATH="$temporary/bin:$PATH" \
        FAKE_GH_SCENARIO="$scenario" \
        FAKE_GH_STATE="$state" \
        "$publisher" v1.2.3-rc.1 example/project "$temporary"/dist/* \
        >"$stdout" 2>"$stderr"
}

# Three create-side 503s are retried. An upload and the final publication then
# each commit remotely but lose their response; the next state read observes
# success and avoids a duplicate write.
transient="$temporary/transient"
mkdir -p "$transient"
run_publisher transient "$transient" "$temporary/transient.out" "$temporary/transient.err"
assert_equal 4 "$(count_log 'release create ' "$transient/log")" "create attempts"
assert_equal 3 "$(count_log 'release upload ' "$transient/log")" "one upload per asset"
assert_equal 1 "$(count_log 'release edit ' "$transient/log")" "publish attempts"
assert_equal 3 "$(find "$transient/assets" -type f | wc -l | tr -d ' ')" "uploaded assets"
assert_equal '2 4 8' "$(tr '\n' ' ' <"$transient/sleeps" | sed 's/ $//')" "backoff delays"
grep -Fq 'GitHub release is complete: v1.2.3-rc.1' "$temporary/transient.out" \
    || fail "ambiguous publication was not reconciled"
grep -Fq 'release creation received a retryable GitHub response (attempt 3/4)' \
    "$temporary/transient.err" || fail "third 5xx did not report its final backoff"
grep -Fq -- '--prerelease' "$transient/log" || fail "prerelease flag was not preserved"

# A completed rerun is read-only: no release creation, asset overwrite, or
# second publication.
before_create=$(count_log 'release create ' "$transient/log")
before_upload=$(count_log 'release upload ' "$transient/log")
before_edit=$(count_log 'release edit ' "$transient/log")
run_publisher normal "$transient" "$temporary/rerun.out" "$temporary/rerun.err"
assert_equal "$before_create" "$(count_log 'release create ' "$transient/log")" "rerun creates"
assert_equal "$before_upload" "$(count_log 'release upload ' "$transient/log")" "rerun uploads"
assert_equal "$before_edit" "$(count_log 'release edit ' "$transient/log")" "rerun publishes"

# A release left partially populated by an earlier job gets only its missing
# assets; the existing asset is never clobbered.
partial="$temporary/partial"
mkdir -p "$partial/assets"
: >"$partial/release"
: >"$partial/published"
printf 'sha256:%s\n' "$(shasum -a 256 "$temporary/dist/archive.tar.gz" | cut -d' ' -f1)" \
    >"$partial/assets/archive.tar.gz"
run_publisher normal "$partial" "$temporary/partial.out" "$temporary/partial.err"
assert_equal 0 "$(count_log 'release create ' "$partial/log")" "partial release creates"
assert_equal 2 "$(count_log 'release upload ' "$partial/log")" "missing asset uploads"
assert_equal 0 "$(count_log 'release edit ' "$partial/log")" "published release edits"
assert_equal 3 "$(find "$partial/assets" -type f | wc -l | tr -d ' ')" "reconciled assets"

# A same-name asset from different bytes stops the run before anything else is
# uploaded. Exact-once reconciliation never clobbers or mixes build outputs.
mismatch="$temporary/mismatch"
mkdir -p "$mismatch/assets"
: >"$mismatch/release"
: >"$mismatch/published"
printf 'sha256:not-the-local-digest\n' >"$mismatch/assets/archive.tar.gz"
if run_publisher normal "$mismatch" "$temporary/mismatch.out" "$temporary/mismatch.err"; then
    fail "different existing asset digest was accepted"
fi
assert_equal 0 "$(count_log 'release upload ' "$mismatch/log")" "mismatch uploads"
grep -Fq 'already exists with a different SHA-256 digest' "$temporary/mismatch.err" \
    || fail "asset digest mismatch was not explained"

# A non-5xx authentication/permission failure is immediate and clear.
fatal="$temporary/fatal"
mkdir -p "$fatal"
if run_publisher fatal "$fatal" "$temporary/fatal.out" "$temporary/fatal.err"; then
    fail "HTTP 403 was retried or ignored"
fi
assert_equal 1 "$(count_log 'release view ' "$fatal/log")" "non-5xx lookup attempts"
grep -Fq 'release creation failed (attempt 1/4)' "$temporary/fatal.err" \
    || fail "non-5xx failure was not explained"

# Persistent GitHub 5xx responses stop at the fixed fourth attempt.
exhaust="$temporary/exhaust"
mkdir -p "$exhaust"
if run_publisher exhaust "$exhaust" "$temporary/exhaust.out" "$temporary/exhaust.err"; then
    fail "persistent HTTP 503 succeeded"
fi
assert_equal 4 "$(count_log 'release view ' "$exhaust/log")" "bounded lookup attempts"
grep -Fq 'release creation failed after 4 attempts' \
    "$temporary/exhaust.err" || fail "exhausted 5xx failure was not explained"

echo "publish-release tests passed"
