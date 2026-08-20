#!/bin/sh
set -eu

repository_root=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
script_under_test="$repository_root/scripts/new-worktree.sh"
temporary=$(mktemp -d "${TMPDIR:-/tmp}/dark-factory-new-worktree.XXXXXX")
trap 'rm -rf "$temporary"' EXIT HUP INT TERM

fail() {
    echo "new-worktree test failed: $*" >&2
    exit 1
}

init_repository() {
    fixture_repository=$1
    mkdir -p "$fixture_repository/scripts"
    git -C "$fixture_repository" init -q -b main
    cp "$script_under_test" "$fixture_repository/scripts/new-worktree.sh"
    chmod +x "$fixture_repository/scripts/new-worktree.sh"
    printf '%s\n' fixture >"$fixture_repository/README.md"
    git -C "$fixture_repository" add README.md scripts/new-worktree.sh
    git -C "$fixture_repository" \
        -c user.name='Dark Factory tests' \
        -c user.email='tests@dark.factory' \
        commit -q -m fixture
}

run_script() {
    fixture_worktree=$1
    fixture_slug=$2
    (
        cd "$fixture_worktree"
        ./scripts/new-worktree.sh "$fixture_slug"
    )
}

expect_failure() {
    failure_name=$1
    failure_worktree=$2
    failure_slug=$3
    if run_script "$failure_worktree" "$failure_slug" \
        >"$temporary/$failure_name.out" 2>&1; then
        fail "$failure_name unexpectedly succeeded"
    fi
}

assert_branch_absent() {
    branch_repository=$1
    branch_name=$2
    if git -C "$branch_repository" show-ref --verify --quiet "refs/heads/$branch_name"; then
        fail "branch unexpectedly exists: $branch_name"
    fi
}

assert_directory() {
    directory_path=$1
    [ -d "$directory_path" ] || fail "directory is missing: $directory_path"
}

primary="$temporary/primary repository"
resident="$temporary/resident worktree"
legacy_task="$resident/.worktrees/legacy-task"
init_repository "$primary"
git -C "$primary" worktree add -q -b resident-session "$resident" main
mkdir -p "$resident/.worktrees"
git -C "$primary" worktree add -q -b legacy-task "$legacy_task" main

run_script "$primary" from-primary >/dev/null
run_script "$resident" from-resident >/dev/null
run_script "$legacy_task" from-task >/dev/null

for slug in from-primary from-resident from-task; do
    assert_directory "$primary/.worktrees/$slug"
done
[ ! -e "$resident/.worktrees/from-resident" ] ||
    fail "resident invocation created a second-level worktree"
[ ! -e "$legacy_task/.worktrees/from-task" ] ||
    fail "task invocation created a third-level worktree"
assert_directory "$legacy_task"

git -C "$primary" branch occupied-branch main
expect_failure existing-branch "$legacy_task" occupied-branch
[ ! -e "$primary/.worktrees/occupied-branch" ] ||
    fail "existing branch failure left a worktree"

mkdir "$primary/.worktrees/occupied-path"
expect_failure existing-path "$resident" occupied-path
assert_branch_absent "$primary" occupied-path

symlink_destination="$temporary/symlink destination"
mkdir "$symlink_destination"
ln -s "$symlink_destination" "$primary/.worktrees/symlink-target"
expect_failure symlink-target "$resident" symlink-target
assert_branch_absent "$primary" symlink-target
[ -L "$primary/.worktrees/symlink-target" ] ||
    fail "symlink collision was mutated"

for invalid_slug in .hidden bad/name 'bad slug'; do
    expect_failure invalid-slug "$resident" "$invalid_slug"
done

symlink_repository="$temporary/symlink parent repository"
symlink_outside="$temporary/symlink parent outside"
init_repository "$symlink_repository"
mkdir "$symlink_outside"
ln -s "$symlink_outside" "$symlink_repository/.worktrees"
expect_failure symlink-parent "$symlink_repository" through-symlink
assert_branch_absent "$symlink_repository" through-symlink
[ ! -e "$symlink_outside/through-symlink" ] ||
    fail "symlinked worktree parent was followed"

unknown_repository="$temporary/not a repository"
mkdir -p "$unknown_repository/scripts"
cp "$script_under_test" "$unknown_repository/scripts/new-worktree.sh"
chmod +x "$unknown_repository/scripts/new-worktree.sh"
expect_failure unknown-repository "$unknown_repository" unknown

bare_repository="$temporary/bare.git"
bare_caller="$temporary/bare caller"
git init -q --bare "$bare_repository"
mkdir -p "$bare_caller/scripts"
cp "$script_under_test" "$bare_caller/scripts/new-worktree.sh"
chmod +x "$bare_caller/scripts/new-worktree.sh"
printf 'gitdir: %s\n' "$bare_repository" >"$bare_caller/.git"
expect_failure bare-repository "$bare_caller" bare
assert_branch_absent "$bare_repository" bare

separate_worktree="$temporary/separate worktree"
separate_git_dir="$temporary/separate metadata"
git init -q -b main --separate-git-dir="$separate_git_dir" "$separate_worktree"
mkdir -p "$separate_worktree/scripts"
cp "$script_under_test" "$separate_worktree/scripts/new-worktree.sh"
chmod +x "$separate_worktree/scripts/new-worktree.sh"
printf '%s\n' fixture >"$separate_worktree/README.md"
git -C "$separate_worktree" add README.md scripts/new-worktree.sh
git -C "$separate_worktree" \
    -c user.name='Dark Factory tests' \
    -c user.email='tests@dark.factory' \
    commit -q -m fixture
expect_failure ambiguous-anchor "$separate_worktree" ambiguous
assert_branch_absent "$separate_worktree" ambiguous

first_repository="$temporary/first repository"
second_repository="$temporary/second repository"
first_link="$temporary/first linked worktree"
second_link="$temporary/second linked worktree"
init_repository "$first_repository"
init_repository "$second_repository"
git -C "$first_repository" worktree add -q -b first-link "$first_link" main
git -C "$second_repository" worktree add -q -b second-link "$second_link" main
first_git_file=$(sed -n '1p' "$first_link/.git")
second_git_dir=$(sed -n 's/^gitdir: //p' "$second_link/.git")
printf 'gitdir: %s\n' "$second_git_dir" >"$first_link/.git"
expect_failure mismatched-repositories "$first_link" mismatched
printf '%s\n' "$first_git_file" >"$first_link/.git"
assert_branch_absent "$first_repository" mismatched
assert_branch_absent "$second_repository" mismatched
[ ! -e "$first_link/.worktrees/mismatched" ] ||
    fail "mismatched repository created a worktree"

shim_directory="$temporary/git shim"
mkdir "$shim_directory"
cat >"$shim_directory/git" <<'EOF'
#!/bin/sh
set -eu

matches_root=false
matches_common_dir=false
matches_worktree_add=false
previous=
for argument in "$@"; do
    if [ "$previous" = -C ] && [ "$argument" = "${DF_TEST_SWAP_ROOT-}" ]; then
        matches_root=true
    fi
    [ "$argument" = --git-common-dir ] && matches_common_dir=true
    [ "$previous" = worktree ] && [ "$argument" = add ] \
        && matches_worktree_add=true
    previous=$argument
done

if [ "${DF_TEST_SWAP_REPOSITORY-}" = 1 ] \
    && [ "$matches_root" = true ] \
    && [ "$matches_common_dir" = true ]; then
    swap_count=0
    [ ! -f "$DF_TEST_SWAP_STATE" ] || read -r swap_count <"$DF_TEST_SWAP_STATE"
    swap_count=$((swap_count + 1))
    printf '%s\n' "$swap_count" >"$DF_TEST_SWAP_STATE"
    if [ "$swap_count" -eq 2 ]; then
        printf 'gitdir: %s\n' "$DF_TEST_SWAP_TO_GIT_DIR" \
            >"$DF_TEST_SWAP_ROOT/.git"
    fi
fi

if [ "${DF_TEST_FAIL_ADD-}" = 1 ] && [ "$matches_worktree_add" = true ]; then
    "$DF_TEST_REAL_GIT" -C "$DF_TEST_FAIL_REPOSITORY" update-ref \
        "refs/heads/$DF_TEST_FAIL_BRANCH" "$DF_TEST_FAIL_BASE" '' 2>/dev/null || true
    mkdir -p "$DF_TEST_FAIL_TARGET"
    printf '%s\n' partial >"$DF_TEST_FAIL_TARGET/partial"
    exit 72
fi

exec "$DF_TEST_REAL_GIT" "$@"
EOF
chmod +x "$shim_directory/git"
real_git=$(command -v git)

changing_git_file=$(sed -n '1p' "$first_link/.git")
first_link_canonical=$(CDPATH='' cd -- "$first_link" && pwd -P)
if (
    cd "$first_link"
    env \
        PATH="$shim_directory:$PATH" \
        DF_TEST_REAL_GIT="$real_git" \
        DF_TEST_SWAP_REPOSITORY=1 \
        DF_TEST_SWAP_ROOT="$first_link_canonical" \
        DF_TEST_SWAP_STATE="$temporary/swap-state" \
        DF_TEST_SWAP_TO_GIT_DIR="$second_git_dir" \
        ./scripts/new-worktree.sh identity-changed
) >"$temporary/identity-changed.out" 2>&1; then
    fail "repository identity change unexpectedly succeeded"
fi
printf '%s\n' "$changing_git_file" >"$first_link/.git"
assert_branch_absent "$first_repository" identity-changed
assert_branch_absent "$second_repository" identity-changed
[ ! -e "$first_repository/.worktrees/identity-changed" ] ||
    fail "repository identity change left a worktree"
[ ! -e "$second_repository/.worktrees/identity-changed" ] ||
    fail "repository identity change mutated the replacement repository"

failure_repository="$temporary/add failure repository"
failure_target="$failure_repository/.worktrees/add-failure"
init_repository "$failure_repository"
failure_base=$(git -C "$failure_repository" rev-parse main)
if (
    cd "$failure_repository"
    env \
        PATH="$shim_directory:$PATH" \
        DF_TEST_REAL_GIT="$real_git" \
        DF_TEST_FAIL_ADD=1 \
        DF_TEST_FAIL_REPOSITORY="$failure_repository" \
        DF_TEST_FAIL_BRANCH=add-failure \
        DF_TEST_FAIL_BASE="$failure_base" \
        DF_TEST_FAIL_TARGET="$failure_target" \
        ./scripts/new-worktree.sh add-failure
) >"$temporary/add-failure.out" 2>&1; then
    fail "partial worktree-add failure unexpectedly succeeded"
fi
assert_branch_absent "$failure_repository" add-failure
[ ! -e "$failure_target" ] || fail "failed worktree add left a path"
[ ! -e "$failure_repository/.worktrees" ] ||
    fail "failed worktree add left an empty parent"

echo "new-worktree tests passed"
