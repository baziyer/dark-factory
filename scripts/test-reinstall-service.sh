#!/bin/sh
set -eu

repository_root=$(CDPATH='' cd -- "$(dirname "$0")/.." && pwd)
temporary=$(mktemp -d "${TMPDIR:-/tmp}/dark-factory-reinstall-service-test.XXXXXX")
trap 'rm -rf "$temporary"' EXIT HUP INT TERM

fail() {
    echo "reinstall-service test failed: $*" >&2
    exit 1
}

test_repository=$temporary/repository
fake_bin=$temporary/fake-bin
fake_home=$temporary/home
mkdir -p "$test_repository/scripts" "$fake_bin" "$fake_home/.dark-factory"
cp "$repository_root/scripts/reinstall-service.sh" "$test_repository/scripts/reinstall-service.sh"

git -C "$test_repository" init -q -b main
git -C "$test_repository" config user.name fixture
git -C "$test_repository" config user.email fixture@example.invalid
printf 'fixture\n' >"$test_repository/README.md"
git -C "$test_repository" add README.md
git -C "$test_repository" commit -q -m fixture
git clone -q --bare "$test_repository" "$temporary/origin.git"
git -C "$test_repository" remote add origin "$temporary/origin.git"
sha=$(git -C "$test_repository" rev-parse HEAD)

printf 'live store\n' >"$fake_home/.dark-factory/factory.sqlite3"
printf '0\n' >"$temporary/active-runs"

# go build writes a stub that records the build tree's HEAD and execs the fake
# factoryctl; go version -m prints the stub so the vcs.* checks see it.
cat >"$fake_bin/go" <<'FAKE'
#!/bin/sh
set -eu
case "$1" in
    build)
        out=""
        while [ $# -gt 0 ]; do
            [ "$1" = -o ] && out=$2
            shift
        done
        printf '#!/bin/sh\n# vcs.revision=%s\n# vcs.modified=false\nexec factoryctl "$@"\n' "$(git rev-parse HEAD)" >"$out"
        chmod 755 "$out"
        ;;
    version) cat "$3" ;;
    *) exit 1 ;;
esac
FAKE
cat >"$fake_bin/sqlite3" <<'FAKE'
#!/bin/sh
set -eu
case "$2" in
    "SELECT count(*) FROM runs WHERE phase <> 'terminal'") cat "$DARK_FACTORY_TEST_ACTIVE_RUNS" ;;
    ".backup "*) cp "$1" "${2#.backup }" ;;
    "PRAGMA user_version") echo 7 ;;
    *) exit 1 ;;
esac
FAKE
cat >"$fake_bin/factoryctl" <<'FAKE'
#!/bin/sh
echo "$*" >>"$DARK_FACTORY_TEST_FACTORYCTL_LOG"
FAKE
chmod 755 "$fake_bin"/*
export PATH="$fake_bin:$PATH" HOME="$fake_home"
export DARK_FACTORY_TEST_ACTIVE_RUNS="$temporary/active-runs"
export DARK_FACTORY_TEST_FACTORYCTL_LOG="$temporary/factoryctl.log"
script=$test_repository/scripts/reinstall-service.sh

untouched() {
    [ ! -e "$fake_home/.dark-factory-backups" ] || fail "$1: backup directory created"
    [ ! -e "$DARK_FACTORY_TEST_FACTORYCTL_LOG" ] || fail "$1: factoryctl invoked"
}

"$script" >/dev/null 2>"$temporary/stderr" && fail "no argument accepted"
grep -q '^usage:' "$temporary/stderr" || fail "no argument: usage not printed"
"$script" not-a-sha >/dev/null 2>"$temporary/stderr" && fail "bad argument accepted"
grep -q '^usage:' "$temporary/stderr" || fail "bad argument: usage not printed"
[ ! -e "$test_repository/.worktrees" ] || fail "usage error created a worktree"
untouched "usage error"

printf '1\n' >"$temporary/active-runs"
"$script" "$sha" >/dev/null 2>"$temporary/stderr" && fail "active run accepted"
grep -q 'non-terminal run' "$temporary/stderr" || fail "active run: wrong refusal"
[ ! -e "$test_repository/.worktrees" ] || fail "active run created a worktree"
untouched "active run"
printf '0\n' >"$temporary/active-runs"

"$script" "$sha" >"$temporary/stdout" || fail "clean reinstall exited non-zero"
backup=$(find "$fake_home/.dark-factory-backups" -name factory.sqlite3)
case "$backup" in
    "$fake_home/.dark-factory-backups/"[0-9][0-9][0-9][0-9][0-9][0-9][0-9][0-9]T[0-9][0-9][0-9][0-9][0-9][0-9]-"$sha/factory.sqlite3") ;;
    *) fail "unexpected backup path: $backup" ;;
esac
cmp -s "$backup" "$fake_home/.dark-factory/factory.sqlite3" || fail "backup content differs"
[ "$(stat -f %Lp "$fake_home/.dark-factory-backups")" = 700 ] || fail "backups directory mode"
[ "$(stat -f %Lp "$(dirname "$backup")")" = 700 ] || fail "backup directory mode"
[ "$(stat -f %Lp "$backup")" = 600 ] || fail "backup file mode"
for cmd in factoryctl factoryd factory-runner; do
    [ -x "$test_repository/.worktrees/bin-$sha/$cmd" ] || fail "$cmd not built"
done
printf '%s\n' \
    "service uninstall --home $fake_home/.dark-factory" \
    "service install --home $fake_home/.dark-factory --relay-origin wss://relay.darkfactory.build" \
    "service status --home $fake_home/.dark-factory" \
    "web status" \
    "remote status" >"$temporary/expected.log"
cmp -s "$temporary/expected.log" "$DARK_FACTORY_TEST_FACTORYCTL_LOG" \
    || fail "factoryctl calls: $(tr '\n' ';' <"$DARK_FACTORY_TEST_FACTORYCTL_LOG")"
grep -q '^user_version now: 7$' "$temporary/stdout" || fail "user_version not printed"

rm "$DARK_FACTORY_TEST_FACTORYCTL_LOG"
printf 'stray\n' >"$test_repository/.worktrees/build-$sha/stray"
"$script" "$sha" >/dev/null 2>"$temporary/stderr" && fail "dirty worktree accepted"
grep -q 'worktree not clean' "$temporary/stderr" || fail "dirty worktree: wrong refusal"
[ ! -e "$DARK_FACTORY_TEST_FACTORYCTL_LOG" ] || fail "dirty worktree: factoryctl invoked"
[ "$(find "$fake_home/.dark-factory-backups" -name factory.sqlite3 | wc -l | tr -d ' ')" = 1 ] \
    || fail "dirty worktree: backup taken"

echo "reinstall-service tests passed"
