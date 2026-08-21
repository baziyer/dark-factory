#!/bin/sh
set -eu

repository_root=$(CDPATH='' cd -- "$(dirname "$0")/.." && pwd)
temporary=$(mktemp -d "${TMPDIR:-/tmp}/dark-factory-new-worktree-test.XXXXXX")
trap 'rm -rf "$temporary"' EXIT HUP INT TERM

fail() {
    echo "new-worktree test failed: $*" >&2
    exit 1
}

test_repository=$temporary/repository
configured_hooks=$temporary/configured-hooks
sentinel=$temporary/post-checkout-ran
mkdir -p "$test_repository/scripts" "$configured_hooks"
cp "$repository_root/scripts/new-worktree.sh" "$test_repository/scripts/new-worktree.sh"

git -C "$test_repository" init -q -b main
git -C "$test_repository" config user.name fixture
git -C "$test_repository" config user.email fixture@example.invalid
printf 'fixture\n' >"$test_repository/README.md"
git -C "$test_repository" add README.md
git -C "$test_repository" commit -q -m fixture

cat >"$configured_hooks/post-checkout" <<'EOF'
#!/bin/sh
set -eu
: >"$DARK_FACTORY_POST_CHECKOUT_SENTINEL"
EOF
chmod 700 "$configured_hooks/post-checkout"
git -C "$test_repository" config core.hooksPath "$configured_hooks"

DARK_FACTORY_POST_CHECKOUT_SENTINEL=$sentinel \
    "$test_repository/scripts/new-worktree.sh" fixture-worktree >/dev/null

[ ! -e "$sentinel" ] || fail "configured post-checkout hook executed"
git -C "$test_repository/.worktrees/fixture-worktree" diff --quiet \
    || fail "created worktree is dirty"
test "$(git -C "$test_repository/.worktrees/fixture-worktree" branch --show-current)" = fixture-worktree \
    || fail "created worktree has the wrong branch"

echo "new-worktree tests passed"
