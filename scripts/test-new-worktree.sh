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
    (
        cd "$1"
        ./scripts/new-worktree.sh "$2"
    )
}

expect_failure() {
    if run_script "$2" "$3" >"$temporary/$1.out" 2>&1; then
        fail "$1 unexpectedly succeeded"
    fi
}

assert_branch_absent() {
    if git -C "$1" show-ref --verify --quiet "refs/heads/$2"; then
        fail "branch unexpectedly exists: $2"
    fi
}

assert_branch_present() {
    git -C "$1" show-ref --verify --quiet "refs/heads/$2" ||
        fail "branch is missing: $2"
}

assert_directory() {
    [ -d "$1" ] || fail "directory is missing: $1"
}

assert_path_absent() {
    [ ! -e "$1" ] && [ ! -L "$1" ] || fail "path unexpectedly exists: $1"
}

assert_unregistered() {
    if git -C "$1" worktree list --porcelain | grep -Fqx "worktree $2"; then
        fail "worktree unexpectedly remains registered: $2"
    fi
}

assert_registered() {
    git -C "$1" worktree list --porcelain | grep -Fqx "worktree $2" ||
        fail "worktree registration is missing: $2"
}

assert_output_contains() {
    grep -Fq "$2" "$1" || fail "output did not contain: $2"
}

assert_bounded_output() {
    output_bytes=$(wc -c <"$1" | tr -d ' ')
    [ "$output_bytes" -le "$2" ] ||
        fail "output exceeded $2 bytes: $output_bytes"
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

if [ "${DF_TEST_REPLACE_REPOSITORY-}" = 1 ] \
    && [ "$matches_root" = true ] \
    && [ ! -e "$DF_TEST_REPLACE_STATE" ]; then
    : >"$DF_TEST_REPLACE_STATE"
    mv "$DF_TEST_SWAP_ROOT" "$DF_TEST_PRESERVED_ROOT"
    mv "$DF_TEST_REPLACEMENT_ROOT" "$DF_TEST_SWAP_ROOT"
fi

if [ "${DF_TEST_REPLACE_TARGET-}" = same-path ] \
    && [ "$matches_worktree_add" = true ]; then
    "$DF_TEST_REAL_GIT" "$@"
    mv "$DF_TEST_REPLACE_TARGET_PATH" "$DF_TEST_PRESERVED_TARGET_PATH"
    mkdir "$DF_TEST_REPLACE_TARGET_PATH"
    cp "$DF_TEST_PRESERVED_TARGET_PATH/.git" "$DF_TEST_REPLACE_TARGET_PATH/.git"
    printf '%s\n' replacement \
        >"$DF_TEST_REPLACE_TARGET_PATH/replacement-survives"
    exit 0
fi

exec "$DF_TEST_REAL_GIT" "$@"
EOF
chmod +x "$shim_directory/git"
real_git=$(command -v git)

snapshot_repository="$temporary/snapshot repository"
snapshot_replacement="$temporary/snapshot replacement"
snapshot_preserved="$temporary/snapshot preserved"
init_repository "$snapshot_repository"
init_repository "$snapshot_replacement"
snapshot_repository_canonical=$(CDPATH='' cd -- "$snapshot_repository" && pwd -P)
if (
    cd "$snapshot_repository"
    env \
        PATH="$shim_directory:$PATH" \
        DF_TEST_REAL_GIT="$real_git" \
        DF_TEST_REPLACE_REPOSITORY=1 \
        DF_TEST_SWAP_ROOT="$snapshot_repository_canonical" \
        DF_TEST_REPLACEMENT_ROOT="$snapshot_replacement" \
        DF_TEST_PRESERVED_ROOT="$snapshot_preserved" \
        DF_TEST_REPLACE_STATE="$temporary/repository-replaced" \
        ./scripts/new-worktree.sh snapshot-replaced
) >"$temporary/snapshot-replaced.out" 2>&1; then
    fail "pre-query repository replacement unexpectedly succeeded"
fi
assert_output_contains "$temporary/snapshot-replaced.out" "repository identity changed"
assert_branch_absent "$snapshot_preserved" snapshot-replaced
assert_branch_absent "$snapshot_repository" snapshot-replaced
assert_path_absent "$snapshot_preserved/.worktrees/snapshot-replaced"
assert_path_absent "$snapshot_repository/.worktrees/snapshot-replaced"

fetch_repository="$temporary/explicit fetch repository"
fetch_remote="$temporary/explicit fetch remote.git"
fetch_updater="$temporary/explicit fetch updater"
init_repository "$fetch_repository"
git init -q --bare -b main "$fetch_remote"
git -C "$fetch_repository" remote add origin "$fetch_remote"
git -C "$fetch_repository" push -q -u origin main
stale_main=$(git -C "$fetch_repository" rev-parse HEAD)
git clone -q "$fetch_remote" "$fetch_updater"
printf '%s\n' fresh >"$fetch_updater/fresh-base"
git -C "$fetch_updater" add fresh-base
git -C "$fetch_updater" \
    -c user.name='Dark Factory tests' \
    -c user.email='tests@dark.factory' \
    commit -q -m 'advance remote main'
fresh_main=$(git -C "$fetch_updater" rev-parse HEAD)
git -C "$fetch_updater" push -q origin main
git -C "$fetch_repository" config --unset-all remote.origin.fetch
for fetch_slug in missing-fetch alternate-fetch; do
    if [ "$fetch_slug" = alternate-fetch ]; then
        git -C "$fetch_repository" config --add remote.origin.fetch \
            '+refs/heads/not-main:refs/remotes/origin/not-main'
    fi
    git -C "$fetch_repository" update-ref refs/remotes/origin/main "$stale_main"
    run_script "$fetch_repository" "$fetch_slug" >/dev/null
    [ "$(git -C "$fetch_repository/.worktrees/$fetch_slug" rev-parse HEAD)" = "$fresh_main" ] ||
        fail "$fetch_slug used stale origin/main"
done

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
    printf '%s\n' 'File: "%d:%i"' 'ID: wrong-filesystem-semantics'
    exit 0
fi

[ "$1" = -c ] && [ "$2" = '%d:%i' ] || exit 2
exec "$DF_TEST_REAL_STAT" "$DF_TEST_REAL_STAT_STYLE" "$2" "$3"
EOF
chmod +x "$stat_shim_directory/stat"
stat_repository="$temporary/stat success repository"
stat_replacement="$temporary/stat success replacement"
stat_preserved="$temporary/stat success preserved"
init_repository "$stat_repository"
init_repository "$stat_replacement"
stat_repository_canonical=$(CDPATH='' cd -- "$stat_repository" && pwd -P)
real_stat=$(command -v stat)
if "$real_stat" -f '%d:%i' "$stat_repository" >/dev/null 2>&1; then
    real_stat_style=-f
else
    real_stat_style=-c
fi
if (
    cd "$stat_repository"
    env \
        PATH="$stat_shim_directory:$shim_directory:$PATH" \
        DF_TEST_REAL_GIT="$real_git" \
        DF_TEST_REPLACE_REPOSITORY=1 \
        DF_TEST_SWAP_ROOT="$stat_repository_canonical" \
        DF_TEST_REPLACEMENT_ROOT="$stat_replacement" \
        DF_TEST_PRESERVED_ROOT="$stat_preserved" \
        DF_TEST_REPLACE_STATE="$temporary/stat-repository-replaced" \
        DF_TEST_REAL_STAT="$real_stat" \
        DF_TEST_REAL_STAT_STYLE="$real_stat_style" \
        ./scripts/new-worktree.sh stat-success-replaced
) >"$temporary/stat-success-replaced.out" 2>&1; then
    fail "successful GNU stat semantics adopted a repository replacement"
fi
assert_output_contains "$temporary/stat-success-replaced.out" "repository identity changed"
assert_branch_absent "$stat_preserved" stat-success-replaced
assert_branch_absent "$stat_repository" stat-success-replaced
assert_path_absent "$stat_preserved/.worktrees/stat-success-replaced"
assert_path_absent "$stat_repository/.worktrees/stat-success-replaced"

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

signal_hook_repository="$temporary/signal hook repository"
signal_hook_target="$signal_hook_repository/.worktrees/signal-hook"
signal_hook_ready="$temporary/signal-hook-ready"
signal_hook_release="$temporary/signal-hook-release"
init_repository "$signal_hook_repository"
cat >"$signal_hook_repository/.git/hooks/post-checkout" <<'EOF'
#!/bin/sh
set -eu
: >"$DF_TEST_HOOK_READY"
while [ ! -e "$DF_TEST_HOOK_RELEASE" ]; do
    sleep 0.05
done
EOF
chmod +x "$signal_hook_repository/.git/hooks/post-checkout"
(
    cd "$signal_hook_repository"
    exec env \
        DF_TEST_HOOK_READY="$signal_hook_ready" \
        DF_TEST_HOOK_RELEASE="$signal_hook_release" \
        ./scripts/new-worktree.sh signal-hook
) >"$temporary/signal-hook.out" 2>&1 &
signal_hook_pid=$!
signal_wait=0
while [ ! -e "$signal_hook_ready" ]; do
    if ! kill -0 "$signal_hook_pid" 2>/dev/null; then
        wait "$signal_hook_pid" || true
        fail "native worktree add exited before its hook blocked"
    fi
    signal_wait=$((signal_wait + 1))
    if [ "$signal_wait" -ge 200 ]; then
        kill -TERM "$signal_hook_pid" 2>/dev/null || true
        : >"$signal_hook_release"
        wait "$signal_hook_pid" || true
        fail "native worktree hook did not block in time"
    fi
    sleep 0.05
done
kill -TERM "$signal_hook_pid"
: >"$signal_hook_release"
if wait "$signal_hook_pid"; then
    fail "signal during native worktree hook unexpectedly succeeded"
fi
assert_output_contains "$temporary/signal-hook.out" "preserved orphan"
assert_branch_present "$signal_hook_repository" signal-hook
assert_directory "$signal_hook_target"
assert_registered "$signal_hook_repository" "$signal_hook_target"

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
[ -f "$replacement_target/.git" ] || fail "same-path replacement lost its copied Git marker"
assert_directory "$preserved_target"
assert_branch_present "$replacement_repository" replaced
assert_registered "$replacement_repository" "$replacement_target"

echo "new-worktree tests passed"
