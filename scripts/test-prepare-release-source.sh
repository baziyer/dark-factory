#!/bin/sh
set -eu

repository_root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
resolver="$repository_root/scripts/prepare-release-source.sh"
temporary=$(mktemp -d "${TMPDIR:-/tmp}/dark-factory-release-source-test.XXXXXX")
trap 'rm -rf "$temporary"' EXIT HUP INT TERM
remote="$temporary/remote.git"
seed="$temporary/seed"

git init --bare "$remote" >/dev/null
git init -b main "$seed" >/dev/null
git -C "$seed" config user.name fixture
git -C "$seed" config user.email fixture@example.com
mkdir -p "$seed/scripts"
printf 'tagged publisher\n' >"$seed/scripts/publish-release.sh"
git -C "$seed" add scripts/publish-release.sh
git -C "$seed" commit -m tagged >/dev/null
tagged_sha=$(git -C "$seed" rev-parse HEAD)
git -C "$seed" tag v1.2.3
printf 'trusted main publisher\n' >"$seed/scripts/publish-release.sh"
git -C "$seed" commit -am main >/dev/null
main_sha=$(git -C "$seed" rev-parse HEAD)
git -C "$seed" remote add origin "$remote"
git -C "$seed" push origin main v1.2.3 >/dev/null

fail() {
    echo "prepare-release-source test failed: $*" >&2
    exit 1
}

value() {
    key=$1
    file=$2
    sed -n "s/^$key=//p" "$file"
}

fresh_workspace() {
    name=$1
    workspace="$temporary/$name"
    git clone --quiet --branch main "$remote" "$workspace"
    environment_file="$temporary/$name.env"
    runner_temp="$temporary/$name-runner"
    mkdir -p "$runner_temp"
}

run_resolver() {
    run_event=$1
    run_ref=$2
    run_sha=$3
    run_tag=${4:-}
    (
        cd "$workspace"
        GITHUB_EVENT_NAME="$run_event" \
            GITHUB_REF="$run_ref" \
            GITHUB_SHA="$run_sha" \
            RELEASE_DEFAULT_BRANCH=main \
            GITHUB_ENV="$environment_file" \
            RUNNER_TEMP="$runner_temp" \
            "$resolver" "$run_tag"
    )
}

# Recovery resolves the immutable tag but saves the publisher from the exact
# checked-out default-branch commit before switching source trees.
fresh_workspace recovery
run_resolver workflow_dispatch refs/heads/main "$main_sha" v1.2.3
[ "$(git -C "$workspace" rev-parse HEAD)" = "$tagged_sha" ] \
    || fail "recovery did not check out the tagged commit"
[ "$(value TAG "$environment_file")" = v1.2.3 ] || fail "recovery tag"
[ "$(value SOURCE_SHA "$environment_file")" = "$tagged_sha" ] \
    || fail "recovery source SHA"
publisher=$(value PUBLISHER "$environment_file")
[ -x "$publisher" ] || fail "trusted publisher is not executable"
[ "$(sed -n '1p' "$publisher")" = "trusted main publisher" ] \
    || fail "recovery used the tagged publisher"

# The ordinary tag path keeps using the publisher committed with that tag.
fresh_workspace push
git -C "$workspace" checkout --quiet --detach "$tagged_sha"
run_resolver push refs/tags/v1.2.3 "$tagged_sha"
[ "$(value PUBLISHER "$environment_file")" = ./scripts/publish-release.sh ] \
    || fail "tag push did not keep its tagged publisher"

# A dispatch from any ref except the default branch is rejected before the
# persistent runner can switch to or execute the requested tag.
fresh_workspace branch
if run_resolver workflow_dispatch refs/heads/unreviewed "$main_sha" v1.2.3; then
    fail "non-default-branch dispatch was accepted"
fi
[ "$(git -C "$workspace" rev-parse HEAD)" = "$main_sha" ] \
    || fail "rejected dispatch changed the checkout"

# The tag is one validated ref component; it cannot inject a refspec.
fresh_workspace injection
if run_resolver workflow_dispatch refs/heads/main "$main_sha" \
    'v1.2.3:refs/heads/main'; then
    fail "tag refspec injection was accepted"
fi

# Recovery cannot invent a new tag or fall back to a branch or commit.
fresh_workspace missing
if run_resolver workflow_dispatch refs/heads/main "$main_sha" v9.9.9; then
    fail "missing recovery tag was accepted"
fi
[ "$(git -C "$workspace" rev-parse HEAD)" = "$main_sha" ] \
    || fail "missing tag changed the checkout"

# A tag push must still resolve to the immutable event commit.
fresh_workspace moved
if run_resolver push refs/tags/v1.2.3 "$main_sha"; then
    fail "tag/event commit mismatch was accepted"
fi

echo "prepare-release-source tests passed"
