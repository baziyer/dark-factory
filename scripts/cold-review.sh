#!/bin/sh
# usage: scripts/cold-review.sh OWNER/REPO PR HEAD_SHA BASE_SHA BODY_FILE [focus...]
#
# One independent adversarial cold review of a pull request at one exact head,
# recorded through the Maintainer App by a fresh headless Claude session that
# has not seen the work. Needs git, claude and the App's MCP bridge on PATH and
# no GitHub credential: the head is fetched from the public repository by its
# pull request ref, the base by its commit, and the body is the file the caller
# wrote. The full transcript lands in review-PR-HEAD8.log in the current
# directory; the verdict is the last thing printed.
set -eu
if [ "$#" -lt 5 ]; then
    echo "usage: $0 OWNER/REPO PR HEAD_SHA BASE_SHA BODY_FILE [focus...]" >&2
    exit 2
fi
repository=$1 pr=$2 head=$3 base=$4 body=$5
shift 5
focus="$*"
for sha in "$head" "$base"; do
    if [ "${#sha}" -ne 40 ] || [ -n "$(printf '%s' "$sha" | tr -d '0-9a-f')" ]; then
        echo "not a full lowercase commit id: $sha" >&2
        exit 2
    fi
done
[ -f "$body" ] || { echo "no body file: $body" >&2; exit 2; }
bridge=$(command -v dark-factory-maintainer-mcp-bridge) || { echo "maintainer bridge is not on PATH" >&2; exit 2; }
work=$(mktemp -d "${TMPDIR:-/tmp}/cold-review.XXXXXX")
trap 'rm -rf "$work"' EXIT INT TERM
git clone -q --filter=blob:none --no-checkout "https://github.com/$repository" "$work/repo"
git -C "$work/repo" fetch -q origin "refs/pull/$pr/head"
if [ "$(git -C "$work/repo" rev-parse FETCH_HEAD)" != "$head" ]; then
    echo "pull request $pr is not at $head" >&2
    exit 1
fi
git -C "$work/repo" fetch -q origin "$base"
git -C "$work/repo" checkout -q "$head"
cp "$body" "$work/body.md"
out="$PWD/review-$pr-$(printf '%s' "$head" | cut -c1-8).log"
prompt="You are an independent, adversarial cold reviewer for pull request #$pr in $repository at exact head commit $head, whose base is $base. You have not seen this work before; the author is not present. Verify, do not trust: read the pull request body at $work/body.md, read the diff with 'git diff $base $head' (run git commands in $work/repo, which is checked out at that head), read the surrounding source there, and look for real defects: wrong behaviour, missing or declaration-restating tests, unhandled edge cases, races, security or trust-boundary gaps, claims in the body the diff does not support, owner identity leaks (emails, org names, /Users/<name> paths) in code, tests, fixtures, commit or pull request text, and violations of AGENTS.md (ponytail ladder: unrequested abstractions, needless code, net production delta not stated). Focus areas: ${focus:-none given}. Then record your verdict through the Maintainer App: call the maintainer MCP tool submit_pull_request_review for repository $repository, pull request $pr, head_sha $head, with a fresh operation_id from uuidgen, event ALLOW only if you found no defect that must change before merge, otherwise REQUEST_CHANGES, and a body listing every finding with file:line and why it matters. Deferred notes that need no change may accompany an ALLOW. Do not edit files. Finish by printing the verdict and findings in plain text."
cd "$work/repo"
claude -p "$prompt" --model opus \
    --strict-mcp-config --mcp-config "{\"mcpServers\":{\"maintainer\":{\"command\":\"$bridge\"}}}" \
    --allowedTools "mcp__maintainer,Bash(git diff:*),Bash(git show:*),Bash(git log:*),Bash(git ls-files:*),Bash(uuidgen),Read,Grep,Glob" \
    > "$out" 2>&1 || true
tail -60 "$out"
