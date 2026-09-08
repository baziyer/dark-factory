#!/bin/sh
set -eu

# The deploy script ships to public production. Everything here runs against a
# fixture site repository and fake corepack, node and vercel binaries, so no
# case installs, verifies, or reaches Vercel.

repository_root=$(CDPATH='' cd -- "$(dirname "$0")/.." && pwd)
temporary=$(mktemp -d "${TMPDIR:-/tmp}/dark-factory-deploy-site-test.XXXXXX")
# macOS sets TMPDIR with a trailing slash; the fakes record $PWD, which cd
# canonicalizes, so the expected paths must be canonical too.
temporary=$(cd "$temporary" && pwd)
trap 'rm -rf "$temporary"' EXIT
trap 'exit 1' HUP INT TERM

fail() {
    echo "deploy-site test failed: $*" >&2
    exit 1
}

site=$temporary/site
fake_bin=$temporary/fake-bin
mkdir -p "$site/scripts" "$site/.vercel" "$fake_bin"

git -C "$site" init -q -b main
git -C "$site" config user.name fixture
git -C "$site" config user.email fixture@example.invalid
printf 'fixture\n' >"$site/scripts/verify-factory-artifacts.mjs"
printf '.vercel/\n' >"$site/.gitignore"
git -C "$site" add .
git -C "$site" commit -q -m fixture
git clone -q --bare "$site" "$temporary/origin.git"
git -C "$site" remote add origin "$temporary/origin.git"
sha=$(git -C "$site" rev-parse HEAD)
printf '{"projectId":"fixture"}\n' >"$site/.vercel/project.json"
# A hook the site repository configures, as a pnpm install does: it must not
# run when the script adds its worktree.
mkdir -p "$temporary/configured-hooks"
printf '#!/bin/sh\n: >"%s"\n' "$temporary/post-checkout-ran" >"$temporary/configured-hooks/post-checkout"
chmod 700 "$temporary/configured-hooks/post-checkout"
git -C "$site" config core.hooksPath "$temporary/configured-hooks"

# Each fake records its name, arguments and working directory. node fails
# when DARK_FACTORY_TEST_VERIFY_FAILS is set, the way a broken artifact does.
for fake in corepack node vercel; do
    cat >"$fake_bin/$fake" <<'FAKE'
#!/bin/sh
echo "$(basename "$0") $* @ $PWD" >>"$DARK_FACTORY_TEST_LOG"
[ "$(basename "$0")" != node ] || [ -z "${DARK_FACTORY_TEST_VERIFY_FAILS-}" ] || exit 1
FAKE
done
chmod 755 "$fake_bin"/*
export PATH="$fake_bin:$PATH" DARK_FACTORY_SITE="$site"
export DARK_FACTORY_TEST_LOG="$temporary/calls.log"
script=$repository_root/scripts/deploy-site.sh
worktree=$site/.worktrees/deploy-$sha

untouched() {
    [ ! -e "$site/.worktrees" ] || fail "$1: worktree created"
    [ ! -e "$DARK_FACTORY_TEST_LOG" ] || fail "$1: tools invoked"
}

"$script" >/dev/null 2>"$temporary/stderr" && fail "no argument accepted"
grep -q '^usage:' "$temporary/stderr" || fail "no argument: usage not printed"
"$script" not-a-sha >/dev/null 2>"$temporary/stderr" && fail "bad argument accepted"
grep -q '^usage:' "$temporary/stderr" || fail "bad argument: usage not printed"
untouched "usage error"

mv "$site/.vercel/project.json" "$temporary/project.json"
"$script" "$sha" >/dev/null 2>"$temporary/stderr" && fail "unlinked site accepted"
grep -q 'no Vercel link' "$temporary/stderr" || fail "unlinked site: wrong refusal"
untouched "unlinked site"
mv "$temporary/project.json" "$site/.vercel/project.json"

DARK_FACTORY_TEST_VERIFY_FAILS=1 "$script" "$sha" >/dev/null 2>&1 && fail "failed verification deployed"
grep -q '^vercel' "$DARK_FACTORY_TEST_LOG" && fail "failed verification: vercel invoked"
rm "$DARK_FACTORY_TEST_LOG"

"$script" "$sha" >/dev/null || fail "clean deploy exited non-zero"
[ "$(git -C "$worktree" rev-parse HEAD)" = "$sha" ] || fail "worktree not at the commit"
[ ! -e "$temporary/post-checkout-ran" ] || fail "configured post-checkout hook executed"
cmp -s "$site/.vercel/project.json" "$worktree/.vercel/project.json" || fail "Vercel link not copied"
printf '%s\n' \
    "corepack pnpm install --frozen-lockfile --prefer-offline @ $worktree" \
    "node scripts/verify-factory-artifacts.mjs @ $worktree" \
    "vercel deploy --prod --yes @ $worktree" \
    "vercel inspect https://app.darkfactory.build @ $worktree" >"$temporary/expected.log"
cmp -s "$temporary/expected.log" "$DARK_FACTORY_TEST_LOG" \
    || fail "tool calls: $(tr '\n' ';' <"$DARK_FACTORY_TEST_LOG")"
rm "$DARK_FACTORY_TEST_LOG"

printf 'stray\n' >"$worktree/stray"
"$script" "$sha" >/dev/null 2>"$temporary/stderr" && fail "dirty worktree accepted"
grep -q 'worktree not clean' "$temporary/stderr" || fail "dirty worktree: wrong refusal"
[ ! -e "$DARK_FACTORY_TEST_LOG" ] || fail "dirty worktree: tools invoked"

echo "deploy-site tests passed"
