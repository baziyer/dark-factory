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
emit_noise() {
    stream=$1
    printf '%s\n' "$stream"
    index=0
    while [ "$index" -lt 10000 ]; do
        printf x
        index=$((index + 1))
    done
    printf '\n'
}

if [ "${FAKE_GH_NOISY:-0}" -eq 1 ]; then
    emit_noise 'https://github.example/comment/7'
    emit_noise 'fake gh warning' >&2
fi

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

assert_rejected() {
    rm -f "$FAKE_GH_BODY" "$FAKE_GH_ARGS"
    if "$@" >"$temporary/rejected.out" 2>"$temporary/rejected.err"; then
        echo "accepted invalid comment target: $*" >&2
        exit 1
    fi
    [ ! -e "$FAKE_GH_BODY" ]
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

# Paths are outside the stdin-only contract, including symlinks and FIFOs;
# rejecting them before fake gh runs avoids pretending path checks close a
# replacement race.
body_link=$temporary/body-link
body_fifo=$temporary/body-fifo
ln -s "$body" "$body_link"
mkfifo "$body_fifo"
assert_rejected "$comment_script" pr 7 "$body_link" <"$body"
assert_rejected "$comment_script" pr 7 "$body_fifo" <"$body"

printf '%s\n' pr comment 7 --body-file - >"$expected_args"
"$comment_script" pr 7 <"$body"
cmp -s "$body" "$actual_body"
assert_args "$expected_args"

assert_rejected "$comment_script" --repo 1 <"$body"
assert_rejected "$comment_script" issue --repo <"$body"
assert_rejected "$comment_script" issue 1 "$temporary/missing-body" <"$body"

# Both publisher streams stay bounded while useful prefixes and the exit
# status survive, without echoing the body.
status=0
FAKE_GH_NOISY=1 "$comment_script" issue 8 <"$body" >"$temporary/success.out" 2>"$temporary/success.err"
[ "$(wc -c <"$temporary/success.out" | tr -d ' ')" -le 4096 ]
[ "$(wc -c <"$temporary/success.err" | tr -d ' ')" -le 4096 ]
grep -Fq 'https://github.example/comment/7' "$temporary/success.out"
grep -Fq 'fake gh warning' "$temporary/success.err"

FAKE_GH_FAIL=1 FAKE_GH_NOISY=1 "$comment_script" issue 8 <"$body" >"$temporary/failure.out" 2>"$temporary/failure.err" || status=$?
[ "$status" -eq 17 ]
[ "$(wc -c <"$temporary/failure.out" | tr -d ' ')" -le 4096 ]
[ "$(wc -c <"$temporary/failure.err" | tr -d ' ')" -le 4096 ]
grep -Fq 'https://github.example/comment/7' "$temporary/failure.out"
grep -Fq 'fake gh warning' "$temporary/failure.err"
if grep -E 'SHOULD_NOT_EXIST|touch SHOULD_NOT_EXIST|backtick' "$temporary/failure.err" >/dev/null; then
    echo 'publisher error echoed body content' >&2
    exit 1
fi
echo 'github comment helper checks passed'
