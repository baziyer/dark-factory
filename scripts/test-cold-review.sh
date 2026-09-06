#!/bin/sh
set -eu

repository_root=$(CDPATH='' cd -- "$(dirname "$0")/.." && pwd)
temporary=$(mktemp -d "${TMPDIR:-/tmp}/dark-factory-cold-review-test.XXXXXX")
trap 'rm -rf "$temporary"' EXIT HUP INT TERM

fail() {
    echo "cold-review test failed: $*" >&2
    exit 1
}

# A public repository stands in for GitHub: one base commit on main and one
# pull request ref one commit ahead of it, served over file://.
remote=$temporary/remote
source=$temporary/source
git init -q -b main "$source"
git -C "$source" config user.name fixture
git -C "$source" config user.email fixture@example.invalid
printf 'base\n' >"$source/README.md"
git -C "$source" add README.md
git -C "$source" commit -q -m base
base=$(git -C "$source" rev-parse HEAD)
printf 'changed\n' >"$source/README.md"
git -C "$source" commit -q -am change
head=$(git -C "$source" rev-parse HEAD)
git clone -q --bare "$source" "$remote/owner/repo"
git -C "$remote/owner/repo" update-ref refs/pull/7/head "$head"
git -C "$remote/owner/repo" update-ref refs/heads/main "$base"

# The session is a fake claude that records what it was allowed and answers
# with whatever verdict the test asks for; the bridge only has to exist.
tools=$temporary/tools
mkdir -p "$tools"
cat >"$tools/claude" <<'FAKE'
#!/bin/sh
printf '%s\n' "$@" >"$DARK_FACTORY_FAKE_CLAUDE_ARGS"
cat "$DARK_FACTORY_FAKE_CLAUDE_REPLY"
FAKE
printf '#!/bin/sh\nexit 0\n' >"$tools/dark-factory-maintainer-mcp-bridge"
chmod 700 "$tools/claude" "$tools/dark-factory-maintainer-mcp-bridge"
body=$temporary/body.md
printf 'body\n' >"$body"
args=$temporary/args
reply=$temporary/reply
run=$temporary/run
mkdir -p "$run"

scratch=$temporary/scratch
mkdir -p "$scratch"
review() {
    (cd "$run" && TMPDIR="$scratch" DARK_FACTORY_REVIEW_REMOTE="file://$remote" DARK_FACTORY_FAKE_CLAUDE_ARGS="$args" DARK_FACTORY_FAKE_CLAUDE_REPLY="$reply" \
        PATH="$tools:$PATH" "$repository_root/scripts/cold-review.sh" "$@" >/dev/null 2>&1)
}

printf 'Findings.\nVERDICT: ALLOW\n' >"$reply"
review owner/repo 7 "$head" "$base" "$body" "focus" || fail "ALLOW did not exit 0"
[ -f "$run/review-7-$(printf '%s' "$head" | cut -c1-8).log" ] || fail "no log for the review"
grep -q -- '--strict-mcp-config' "$args" || fail "session is not strict about MCP servers"
grep -q 'mcp__maintainer__submit_pull_request_review' "$args" || fail "verdict tool is not allowed"
if grep -E 'mcp__maintainer,|mcp__maintainer"|mcp__maintainer$' "$args" >/dev/null; then
    fail "session is allowed the whole App"
fi
grep -q "$head" "$args" || fail "prompt does not name the head"
grep -q "$base" "$args" || fail "prompt does not name the base"
grep -q 'focus' "$args" || fail "prompt does not carry the focus"

printf 'Findings.\nVERDICT: REQUEST_CHANGES\n' >"$reply"
status=0
review owner/repo 7 "$head" "$base" "$body" || status=$?
[ "$status" -eq 1 ] || fail "REQUEST_CHANGES exited $status, want 1"

printf 'The session died.\n' >"$reply"
status=0
review owner/repo 7 "$head" "$base" "$body" || status=$?
[ "$status" -eq 3 ] || fail "no verdict exited $status, want 3"

printf 'VERDICT: ALLOW\n' >"$reply"
: >"$args"
status=0
review owner/repo 7 "$base" "$base" "$body" || status=$?
[ "$status" -eq 1 ] || fail "head mismatch exited $status, want 1"
[ ! -s "$args" ] || fail "head mismatch still started a session"
status=0
review owner/repo 7 "$head" "$(printf '%s' "$base" | cut -c1-39)" "$body" || status=$?
[ "$status" -eq 2 ] || fail "short base exited $status, want 2"
status=0
review owner/repo 7x "$head" "$base" "$body" || status=$?
[ "$status" -eq 2 ] || fail "non-numeric pull request exited $status, want 2"
status=0
review owner/repo 7 "$head" "$base" "$temporary/missing.md" || status=$?
[ "$status" -eq 2 ] || fail "missing body exited $status, want 2"
[ ! -s "$args" ] || fail "an argument refusal still started a session"
[ -z "$(ls -A "$scratch")" ] || fail "scratch clones remain"

echo "cold-review tests passed"
