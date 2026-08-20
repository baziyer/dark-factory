#!/bin/sh
set -eu

repository_root=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
script_under_test="$repository_root/scripts/new-worktree.sh"
temporary=$(mktemp -d "${TMPDIR:-/tmp}/dark-factory-new-worktree.XXXXXX")
temporary=$(CDPATH='' cd -- "$temporary" && pwd -P)
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

assert_branch_present() {
    branch_repository=$1
    branch_name=$2
    git -C "$branch_repository" show-ref --verify --quiet \
        "refs/heads/$branch_name" ||
        fail "branch is missing: $branch_name"
}

assert_directory() {
    directory_path=$1
    [ -d "$directory_path" ] || fail "directory is missing: $directory_path"
}

assert_path_absent() {
    [ ! -e "$1" ] && [ ! -L "$1" ] || fail "path unexpectedly exists: $1"
}

assert_unregistered() {
    registration_repository=$1
    registration_target=$2
    if git -C "$registration_repository" worktree list --porcelain |
        grep -Fqx "worktree $registration_target"; then
        fail "worktree unexpectedly remains registered: $registration_target"
    fi
}

assert_registered() {
    registration_repository=$1
    registration_target=$2
    git -C "$registration_repository" worktree list --porcelain |
        grep -Fqx "worktree $registration_target" ||
        fail "worktree registration is missing: $registration_target"
}

assert_output_contains() {
    output_file=$1
    expected_text=$2
    grep -Fq "$expected_text" "$output_file" ||
        fail "output did not contain: $expected_text"
}

assert_bounded_output() {
    output_file=$1
    maximum_bytes=$2
    output_bytes=$(wc -c <"$output_file" | tr -d ' ')
    [ "$output_bytes" -le "$maximum_bytes" ] ||
        fail "output exceeded $maximum_bytes bytes: $output_bytes"
}

configure_checkout_failure() {
    checkout_repository=$1
    printf '%s\n' '*.blocked filter=required-failure' \
        >"$checkout_repository/.gitattributes"
    printf '%s\n' blocked >"$checkout_repository/failure.blocked"
    git -C "$checkout_repository" add .gitattributes failure.blocked
    git -C "$checkout_repository" \
        -c user.name='Dark Factory tests' \
        -c user.email='tests@dark.factory' \
        commit -q -m 'add checkout failure'
    git -C "$checkout_repository" config filter.required-failure.clean cat
    git -C "$checkout_repository" config filter.required-failure.smudge false
    git -C "$checkout_repository" config filter.required-failure.required true
}

primary="$temporary/primary repository"
resident="$temporary/resident worktree"
legacy_task="$resident/.worktrees/legacy-task"
init_repository "$primary"
git -C "$primary" worktree add -q -b resident-session "$resident" main
mkdir -p "$resident/.worktrees"
git -C "$primary" worktree add -q -b legacy-task "$legacy_task" main

run_script "$primary" from-primary >"$temporary/from-primary.out"
assert_output_contains \
    "$temporary/from-primary.out" \
    "$primary/.worktrees/from-primary"
run_script "$resident" from-resident >/dev/null
run_script "$legacy_task" from-task >/dev/null
run_script "$primary/.worktrees/from-task" from-review >/dev/null

for slug in from-primary from-resident from-task from-review; do
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
assert_path_absent "$separate_worktree/.worktrees/ambiguous"

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

copied_link="$temporary/copied linked worktree"
mkdir -p "$copied_link/scripts"
cp "$script_under_test" "$copied_link/scripts/new-worktree.sh"
chmod +x "$copied_link/scripts/new-worktree.sh"
cp "$first_link/.git" "$copied_link/.git"
expect_failure copied-pointer "$copied_link" copied-pointer
assert_branch_absent "$first_repository" copied-pointer
assert_path_absent "$first_repository/.worktrees/copied-pointer"

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
matches_fetch=false
previous=
for argument in "$@"; do
    if [ "$previous" = -C ] && [ "$argument" = "${DF_TEST_SWAP_ROOT-}" ]; then
        matches_root=true
    fi
    [ "$argument" = --git-common-dir ] && matches_common_dir=true
    [ "$previous" = worktree ] && [ "$argument" = add ] \
        && matches_worktree_add=true
    [ "$argument" = fetch ] && matches_fetch=true
    previous=$argument
done

if [ "${DF_TEST_SIGNAL-}" = before ] && [ "$matches_fetch" = true ]; then
    kill -TERM "$PPID"
    exit 143
fi

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

if [ "${DF_TEST_SIGNAL-}" = after ] && [ "$matches_worktree_add" = true ]; then
    "$DF_TEST_REAL_GIT" "$@"
    add_status=$?
    kill -TERM "$PPID"
    exit "$add_status"
fi

if [ "${DF_TEST_REPLACE_TARGET-}" = same-path ] \
    && [ "$matches_worktree_add" = true ]; then
    "$DF_TEST_REAL_GIT" "$@"
    mv "$DF_TEST_REPLACE_TARGET_PATH" "$DF_TEST_PRESERVED_TARGET_PATH"
    mkdir "$DF_TEST_REPLACE_TARGET_PATH"
    printf '%s\n' replacement \
        >"$DF_TEST_REPLACE_TARGET_PATH/replacement-survives"
    exit 0
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

hostile_repository="$temporary/hostile environment repository"
init_repository "$hostile_repository"
hostile_number=0
for hostile_assignment in \
    "GIT_DIR=$second_git_dir" \
    "GIT_WORK_TREE=$second_repository" \
    "GIT_COMMON_DIR=$second_repository/.git" \
    "GIT_INDEX_FILE=$temporary/hostile-index" \
    "GIT_CONFIG=$temporary/hostile-config"; do
    hostile_number=$((hostile_number + 1))
    hostile_slug="hostile-$hostile_number"
    if (
        cd "$hostile_repository"
        env "$hostile_assignment" ./scripts/new-worktree.sh "$hostile_slug"
    ) >"$temporary/$hostile_slug.out" 2>&1; then
        fail "$hostile_assignment unexpectedly succeeded"
    fi
    assert_branch_absent "$hostile_repository" "$hostile_slug"
    assert_path_absent "$hostile_repository/.worktrees/$hostile_slug"
done

if (
    cd "$hostile_repository"
    env \
        GIT_CONFIG_COUNT=1 \
        GIT_CONFIG_KEY_0=core.worktree \
        GIT_CONFIG_VALUE_0="$second_repository" \
        ./scripts/new-worktree.sh hostile-config-count
) >"$temporary/hostile-config-count.out" 2>&1; then
    fail "hostile Git config parameters unexpectedly succeeded"
fi
assert_branch_absent "$hostile_repository" hostile-config-count
assert_path_absent "$hostile_repository/.worktrees/hostile-config-count"

long_slug=$(awk 'BEGIN { for (i = 0; i < 1024; i++) printf "x" }')
expect_failure bounded-diagnostic "$hostile_repository" "$long_slug"
assert_bounded_output "$temporary/bounded-diagnostic.out" 512

stat_shim_directory="$temporary/stat shim"
mkdir "$stat_shim_directory"
cat >"$stat_shim_directory/stat" <<'EOF'
#!/bin/sh
set -eu

if [ "$1" = -f ]; then
    stat_count=0
    [ ! -f "$DF_TEST_STAT_STATE" ] || read -r stat_count <"$DF_TEST_STAT_STATE"
    stat_count=$((stat_count + 1))
    printf '%s\n' "$stat_count" >"$DF_TEST_STAT_STATE"
    printf 'failed-filesystem-output-%s\n' "$stat_count"
    exit 1
fi

[ "$1" = -c ] && [ "$2" = '%d:%i' ] || exit 2
exec "$DF_TEST_REAL_STAT" -f "$2" "$3"
EOF
chmod +x "$stat_shim_directory/stat"
stat_repository="$temporary/stat fallback repository"
init_repository "$stat_repository"
real_stat=$(command -v stat)
if ! (
    cd "$stat_repository"
    env \
        PATH="$stat_shim_directory:$PATH" \
        DF_TEST_REAL_STAT="$real_stat" \
        DF_TEST_STAT_STATE="$temporary/stat-state" \
        ./scripts/new-worktree.sh stat-fallback
) >"$temporary/stat-fallback.out" 2>&1; then
    fail "stat fallback output contaminated path identity"
fi
assert_directory "$stat_repository/.worktrees/stat-fallback"
assert_registered "$stat_repository" "$stat_repository/.worktrees/stat-fallback"

smudge_repository="$temporary/smudge failure repository"
smudge_target="$smudge_repository/.worktrees/smudge-failure"
init_repository "$smudge_repository"
configure_checkout_failure "$smudge_repository"
expect_failure smudge-failure "$smudge_repository" smudge-failure
assert_output_contains "$temporary/smudge-failure.out" "preserved orphan"
assert_branch_present "$smudge_repository" smudge-failure
assert_path_absent "$smudge_target"
assert_unregistered "$smudge_repository" "$smudge_target"

hook_repository="$temporary/hook failure repository"
hook_target="$hook_repository/.worktrees/hook-failure"
init_repository "$hook_repository"
cat >"$hook_repository/.git/hooks/post-checkout" <<'EOF'
#!/bin/sh
exit 75
EOF
chmod +x "$hook_repository/.git/hooks/post-checkout"
expect_failure hook-failure "$hook_repository" hook-failure
assert_output_contains "$temporary/hook-failure.out" "preserved orphan"
assert_branch_present "$hook_repository" hook-failure
assert_directory "$hook_target"
assert_registered "$hook_repository" "$hook_target"

signal_before_repository="$temporary/signal before repository"
init_repository "$signal_before_repository"
git -C "$signal_before_repository" remote add origin "$signal_before_repository"
if (
    cd "$signal_before_repository"
    env \
        PATH="$shim_directory:$PATH" \
        DF_TEST_REAL_GIT="$real_git" \
        DF_TEST_SIGNAL=before \
        ./scripts/new-worktree.sh signal-before
) >"$temporary/signal-before.out" 2>&1; then
    fail "pre-mutation signal unexpectedly succeeded"
fi
assert_branch_absent "$signal_before_repository" signal-before
assert_path_absent "$signal_before_repository/.worktrees/signal-before"
assert_unregistered \
    "$signal_before_repository" \
    "$signal_before_repository/.worktrees/signal-before"

signal_after_repository="$temporary/signal after repository"
signal_after_target="$signal_after_repository/.worktrees/signal-after"
init_repository "$signal_after_repository"
if (
    cd "$signal_after_repository"
    env \
        PATH="$shim_directory:$PATH" \
        DF_TEST_REAL_GIT="$real_git" \
        DF_TEST_SIGNAL=after \
        ./scripts/new-worktree.sh signal-after
) >"$temporary/signal-after.out" 2>&1; then
    fail "post-mutation signal unexpectedly succeeded"
fi
assert_output_contains "$temporary/signal-after.out" "preserved orphan"
assert_branch_present "$signal_after_repository" signal-after
assert_directory "$signal_after_target"
assert_registered "$signal_after_repository" "$signal_after_target"

replacement_repository="$temporary/replacement repository"
replacement_target="$replacement_repository/.worktrees/replaced"
preserved_target="$replacement_repository/.worktrees/replaced-created"
init_repository "$replacement_repository"
if (
    cd "$replacement_repository"
    env \
        PATH="$shim_directory:$PATH" \
        DF_TEST_REAL_GIT="$real_git" \
        DF_TEST_REPLACE_TARGET=same-path \
        DF_TEST_REPLACE_TARGET_PATH="$replacement_target" \
        DF_TEST_PRESERVED_TARGET_PATH="$preserved_target" \
        ./scripts/new-worktree.sh replaced
) >"$temporary/replaced.out" 2>&1; then
    fail "same-path replacement unexpectedly succeeded"
fi
assert_output_contains "$temporary/replaced.out" "preserved orphan"
[ -f "$replacement_target/replacement-survives" ] ||
    fail "same-path replacement was removed"
assert_directory "$preserved_target"
assert_branch_present "$replacement_repository" replaced
assert_registered "$replacement_repository" "$replacement_target"

echo "new-worktree tests passed"
