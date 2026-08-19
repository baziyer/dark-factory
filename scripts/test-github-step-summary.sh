#!/bin/sh
set -eu

repository_root=$(CDPATH= cd -- "$(dirname "$0")/.." && pwd)
summary_script=$repository_root/scripts/github-step-summary.sh
temporary=$(mktemp -d "${TMPDIR:-/tmp}/dark-factory-summary.XXXXXX")
trap 'rm -rf "$temporary"' EXIT HUP INT TERM

assert_contains() {
    needle=$1
    haystack=$2
    grep -F -- "$needle" "$haystack" >/dev/null
}

assert_not_contains() {
    needle=$1
    haystack=$2
    if grep -F -- "$needle" "$haystack" >/dev/null; then
        echo "unexpected output: $needle" >&2
        exit 1
    fi
}

summary=$temporary/summary
: >"$summary"
GITHUB_STEP_SUMMARY=$summary \
DF_SUMMARY_KIND=CI \
DF_SUMMARY_REF=refs/pull/208/merge \
DF_SUMMARY_SHA=0123456789abcdef0123456789abcdef01234567 \
DF_SUMMARY_RESULT=success \
DF_SUMMARY_TARGET='v0.4.0 — GitHub-connected factory' \
DF_SUMMARY_RUN_URL=https://github.com/baziyer/dark-factory/actions/runs/123 \
DF_SUMMARY_TARGET_URL=https://github.com/baziyer/dark-factory/milestone/2 \
    "$summary_script"
assert_contains '<code>CI</code>' "$summary"
assert_contains '<code>refs/pull/208/merge</code>' "$summary"
assert_contains '<code>0123456789abcdef0123456789abcdef01234567</code>' "$summary"
assert_contains 'href="https://github.com/baziyer/dark-factory/actions/runs/123"' "$summary"
assert_contains 'href="https://github.com/baziyer/dark-factory/milestone/2"' "$summary"

: >"$summary"
GITHUB_STEP_SUMMARY=$summary \
DF_SUMMARY_KIND='CI
<script>' \
DF_SUMMARY_REF='refs/pull/208/merge|[attack](javascript:alert(1))' \
DF_SUMMARY_SHA='not-a-sha
' \
DF_SUMMARY_RESULT='success
<!-- forged -->' \
DF_SUMMARY_TARGET='target & <tag>' \
DF_SUMMARY_RUN_URL='javascript:alert(1)' \
DF_SUMMARY_TARGET_URL='https://evil.example/steal' \
    "$summary_script"
assert_contains '&lt;script&gt;' "$summary"
assert_contains '<code>unknown</code>' "$summary"
assert_contains '&amp; &lt;tag&gt;' "$summary"
assert_not_contains '<script>' "$summary"
assert_not_contains '<!-- forged -->' "$summary"
assert_not_contains 'href="javascript:' "$summary"
assert_not_contains 'href="https://evil.example' "$summary"

long_value=$(printf '%02048d' 0)
: >"$summary"
GITHUB_STEP_SUMMARY=$summary \
DF_SUMMARY_KIND="$long_value" \
DF_SUMMARY_REF="$long_value" \
DF_SUMMARY_SHA=0123456789abcdef0123456789abcdef01234567 \
DF_SUMMARY_RESULT=success \
DF_SUMMARY_TARGET="$long_value" \
DF_SUMMARY_RUN_URL=https://github.com/baziyer/dark-factory/actions/runs/123 \
DF_SUMMARY_TARGET_URL=https://github.com/baziyer/dark-factory/milestone/2 \
    "$summary_script"
bytes=$(wc -c <"$summary" | tr -d ' ')
[ "$bytes" -le 4096 ] || {
    echo "summary exceeded bound: $bytes bytes" >&2
    exit 1
}

precheckout_summary=$temporary/precheckout-summary
missing_writer=$temporary/missing/scripts/github-step-summary.sh
mkdir -p "$(dirname "$missing_writer")"
: >"$precheckout_summary"
if [ -x "$missing_writer" ]; then
    "$missing_writer"
else
    {
        printf '%s\n' '### Dark Factory workflow summary'
        printf '%s\n' '- Result: <code>failure</code>'
        printf '%s\n' '- Ref: <code>unavailable before checkout</code>'
        printf '%s\n' '- SHA: <code>unavailable before checkout</code>'
        printf '%s\n' '- Release target: <code>unavailable before checkout</code>'
        printf '%s\n' '- Links: workflow run summary'
    } >>"$precheckout_summary"
fi
assert_contains 'unavailable before checkout' "$precheckout_summary"
assert_not_contains '<script>' "$precheckout_summary"
precheckout_bytes=$(wc -c <"$precheckout_summary" | tr -d ' ')
[ "$precheckout_bytes" -le 4096 ]

grep -F 'if [ -x ./scripts/github-step-summary.sh ]; then' .github/workflows/ci.yml >/dev/null
grep -F 'if [ -x ./scripts/github-step-summary.sh ]; then' .github/workflows/release.yml >/dev/null
grep -F 'unavailable before checkout' .github/workflows/ci.yml >/dev/null
grep -F 'unavailable before checkout' .github/workflows/release.yml >/dev/null
grep -F 'run: ./scripts/local-ci.sh' .github/workflows/ci.yml >/dev/null
grep -F 'cargo +1.88.0 build --locked --release --workspace --target "$ARM_TARGET"' .github/workflows/release.yml >/dev/null
grep -F 'cargo +1.88.0 build --locked --release --workspace --target "$INTEL_TARGET"' .github/workflows/release.yml >/dev/null
grep -F '"$PUBLISHER" "$TAG" "$SOURCE_SHA" "$GITHUB_REPOSITORY" dist/*' .github/workflows/release.yml >/dev/null
grep -F 'if: always()' .github/workflows/ci.yml >/dev/null
grep -F 'if: always()' .github/workflows/release.yml >/dev/null
for state in queued in-progress blocked review release-ready; do
    grep -F "state:$state|" scripts/github-repo-settings.sh >/dev/null
done

echo 'github step summary and workflow-preservation checks passed'
