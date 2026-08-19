#!/bin/sh
set -eu

repository_root=$(CDPATH= cd -- "$(dirname "$0")/.." && pwd)
comment_script=$repository_root/scripts/github-comment.sh
temporary=$(mktemp -d "${TMPDIR:-/tmp}/dark-factory-comment.XXXXXX")
trap 'rm -rf "$temporary"' EXIT HUP INT TERM

fake_bin=$temporary/bin
mkdir -p "$fake_bin"
cat >"$fake_bin/gh" <<'FAKE_GH'
#!/bin/sh
set -eu

printf '%s\n' "$@" >"$FAKE_GH_ARGS"
if [ "${FAKE_GH_FAIL:-0}" -eq 1 ]; then
    echo 'fake gh failed without repeating the body' >&2
    exit 17
fi

body_file=
previous=
for argument in "$@"; do
    if [ "$previous" = --body-file ]; then
        body_file=$argument
        break
    fi
    previous=$argument
done
[ -n "$body_file" ] || exit 18
if [ "$body_file" = - ]; then
    cat >"$FAKE_GH_BODY"
else
    cat -- "$body_file" >"$FAKE_GH_BODY"
fi
FAKE_GH
chmod +x "$fake_bin/gh"

assert_args() {
    expected=$1
    cmp -s "$expected" "$FAKE_GH_ARGS" || {
        echo 'fake gh received unexpected arguments' >&2
        diff -u "$expected" "$FAKE_GH_ARGS" >&2 || true
        exit 1
    }
}

body=$temporary/body.md
expected_args=$temporary/expected-args
actual_body=$temporary/actual-body
export PATH="$fake_bin:$PATH"
export FAKE_GH_ARGS=$temporary/args
export FAKE_GH_BODY=$actual_body

# Shell metacharacters and controls are data in the body, never shell syntax.
printf '%s\n' 'backtick `date`; dollar $(touch SHOULD_NOT_EXIST); "quotes"' >"$body"
printf '\tleading tab\r\033[31mred\033[0m\n' >>"$body"
printf '%s\n' issue comment 212 --body-file - >"$expected_args"
"$comment_script" issue 212 <"$body"
cmp -s "$body" "$actual_body"
assert_args "$expected_args"
[ ! -e SHOULD_NOT_EXIST ]

# An explicit file uses the same literal body contract and supports a path
# that would be an option if it were interpolated as a command fragment.
leading_option_file=$temporary/--body
printf '%s' 'single '\''quote and double "quote; `uname`; $HOME' >"$leading_option_file"
printf '%s\n' pr comment 7 --body-file "$leading_option_file" >"$expected_args"
"$comment_script" pr 7 "$leading_option_file"
cmp -s "$leading_option_file" "$actual_body"
assert_args "$expected_args"

assert_rejected() {
    rm -f "$FAKE_GH_BODY" "$FAKE_GH_ARGS"
    if "$@" >"$temporary/rejected.out" 2>"$temporary/rejected.err"; then
        echo "accepted invalid comment target: $*" >&2
        exit 1
    fi
    [ ! -e "$FAKE_GH_BODY" ]
}

assert_rejected "$comment_script" --repo 1 <"$body"
assert_rejected "$comment_script" issue --repo <"$body"
assert_rejected "$comment_script" issue 1 "$temporary/missing-body" <"$body"

# A failed publisher must return a bounded status/error without echoing the
# body. The fake records no body because the command fails before reading it.
status=0
FAKE_GH_FAIL=1 "$comment_script" issue 8 <"$body" >"$temporary/failure.out" 2>"$temporary/failure.err" || status=$?
[ "$status" -eq 17 ]
[ ! -s "$temporary/failure.out" ]
if grep -E 'SHOULD_NOT_EXIST|touch SHOULD_NOT_EXIST|backtick' "$temporary/failure.err" >/dev/null; then
    echo 'publisher error echoed body content' >&2
    exit 1
fi
[ "$(wc -c <"$temporary/failure.err" | tr -d ' ')" -le 256 ]

echo 'github comment helper checks passed'
