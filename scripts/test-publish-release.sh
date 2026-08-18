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
expected_commit=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa
moved_commit=cccccccccccccccccccccccccccccccccccccccc

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
        FAKE_GH_EXPECTED_COMMIT="$expected_commit" \
        "$publisher" v1.2.3-rc.1 "$expected_commit" example/project "$temporary"/dist/* \
        >"$stdout" 2>"$stderr"
}

add_asset() {
    state=$1
    path=$2
    mkdir -p "$state/assets"
    printf 'sha256:%s\n' "$(shasum -a 256 "$path" | cut -d' ' -f1)" \
        >"$state/assets/$(basename "$path")"
}

add_all_assets() {
    state=$1
    for path in "$temporary"/dist/*; do add_asset "$state" "$path"; done
}

# Three create-side 503s are retried. An upload and the final publication then
# each commit remotely but lose their response; the state read observes
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

# A completed rerun is read-only, but still revalidates the immutable tag.
before_api=$(count_log 'api ' "$transient/log")
before_create=$(count_log 'release create ' "$transient/log")
before_upload=$(count_log 'release upload ' "$transient/log")
before_edit=$(count_log 'release edit ' "$transient/log")
run_publisher normal "$transient" "$temporary/rerun.out" "$temporary/rerun.err"
assert_equal "$before_create" "$(count_log 'release create ' "$transient/log")" "rerun creates"
assert_equal "$before_upload" "$(count_log 'release upload ' "$transient/log")" "rerun uploads"
assert_equal "$before_edit" "$(count_log 'release edit ' "$transient/log")" "rerun publishes"
[ "$(count_log 'api ' "$transient/log")" -gt "$before_api" ] \
    || fail "completed rerun did not verify its tag"

# An annotated tag is peeled to the expected workflow commit.
annotated="$temporary/annotated"
mkdir -p "$annotated"
: >"$annotated/tag-kind-annotated"
run_publisher normal "$annotated" "$temporary/annotated.out" "$temporary/annotated.err"
grep -Fq "api repos/example/project/git/tags/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb" \
    "$annotated/log" || fail "annotated tag was not peeled"

# Missing and moved tags fail before a release lookup or write.
for tag_case in absent moved; do
    tag_state="$temporary/tag-$tag_case"
    mkdir -p "$tag_state"
    if [ "$tag_case" = absent ]; then
        : >"$tag_state/tag-absent"
    else
        printf '%s\n' "$moved_commit" >"$tag_state/tag-sha"
    fi
    if run_publisher normal "$tag_state" "$temporary/tag-$tag_case.out" "$temporary/tag-$tag_case.err"; then
        fail "$tag_case tag was accepted"
    fi
    assert_equal 0 "$(count_log 'release view ' "$tag_state/log")" "$tag_case tag release reads"
    assert_equal 0 "$(count_log 'release create ' "$tag_state/log")" "$tag_case tag creates"
done
grep -Fq "points to $moved_commit, expected $expected_commit" "$temporary/tag-moved.err" \
    || fail "moved tag failure was not explained"

# A release left partially populated by an earlier job gets only its missing
# assets; the existing asset is never clobbered.
partial="$temporary/partial"
mkdir -p "$partial/assets"
: >"$partial/release"
: >"$partial/published"
add_asset "$partial" "$temporary/dist/archive.tar.gz"
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

# Any extra remote asset rejects both a draft and a published rerun before a
# mutation. A release is one exact build, not a superset of one.
for release_state in draft published; do
    extra="$temporary/extra-$release_state"
    mkdir -p "$extra/assets"
    : >"$extra/release"
    if [ "$release_state" = published ]; then : >"$extra/published"; fi
    add_all_assets "$extra"
    printf 'sha256:wrong-build\n' >"$extra/assets/wrong-build.tar.gz"
    if run_publisher normal "$extra" "$temporary/extra-$release_state.out" "$temporary/extra-$release_state.err"; then
        fail "unexpected asset on $release_state release was accepted"
    fi
    assert_equal 0 "$(count_log 'release upload ' "$extra/log")" "$release_state extra uploads"
    assert_equal 0 "$(count_log 'release edit ' "$extra/log")" "$release_state extra edits"
    grep -Fq 'has unexpected asset: wrong-build.tar.gz' "$temporary/extra-$release_state.err" \
        || fail "$release_state unexpected asset failure was not explained"
done

# Immutable release metadata is checked immediately after discovery, before
# an upload can mutate a release created by a different run.
wrong_state="$temporary/wrong-state"
mkdir -p "$wrong_state"
: >"$wrong_state/release"
printf '%s\n' false >"$wrong_state/prerelease"
if run_publisher normal "$wrong_state" "$temporary/wrong-state.out" "$temporary/wrong-state.err"; then
    fail "incorrect prerelease state was accepted"
fi
assert_equal 0 "$(count_log 'release upload ' "$wrong_state/log")" "wrong-state uploads"
grep -Fq 'has prerelease=false; expected true' "$temporary/wrong-state.err" \
    || fail "prerelease mismatch was not explained"

setup_failure_state() {
    operation=$1
    state=$2
    mkdir -p "$state/assets"
    case "$operation" in
        create) ;;
        upload) : >"$state/release" ;;
        edit)
            : >"$state/release"
            add_all_assets "$state"
            ;;
    esac
}

# A 422 or lost transport response is always reconciled once. A committed
# write succeeds without duplication; an uncommitted 422 fails immediately;
# an uncommitted transport loss is the only case retried.
for operation in create upload edit; do
    for response in 422 transport; do
        for result in commit no-commit; do
            matrix="$temporary/matrix-$operation-$response-$result"
            setup_failure_state "$operation" "$matrix"
            printf '%s\n' "$response-$result" >"$matrix/$operation-failure"
            expected_success=true
            expected_attempts=1
            if [ "$result" = no-commit ] && [ "$response" = 422 ]; then
                expected_success=false
            elif [ "$result" = no-commit ]; then
                expected_attempts=2
            fi
            if [ "$operation" = upload ] && [ "$expected_success" = true ]; then
                expected_attempts=$((expected_attempts + 2))
            fi
            if run_publisher normal "$matrix" "$matrix.out" "$matrix.err"; then
                actual_success=true
            else
                actual_success=false
            fi
            assert_equal "$expected_success" "$actual_success" "$operation $response $result result"
            assert_equal "$expected_attempts" \
                "$(count_log "release $operation " "$matrix/log")" \
                "$operation $response $result writes"
        done
    done
done

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
