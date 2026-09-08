#!/bin/sh
set -eu

# The reinstall script replaces the live service. Everything here runs against
# a fixture repository, a temporary HOME and fake go, sqlite3 and factoryctl
# binaries, so no case builds, backs up, or touches launchd.

repository_root=$(CDPATH='' cd -- "$(dirname "$0")/.." && pwd)
# Under /private/tmp like the package smoke: the fake daemon's socket path
# must fit a Unix socket address, which a deep TMPDIR does not.
temporary=$(mktemp -d /private/tmp/dark-factory-reinstall-service-test.XXXXXX)
trap 'kill $(cat "$temporary/pids" 2>/dev/null) 2>/dev/null || true; rm -rf "$temporary"' EXIT
trap 'exit 1' HUP INT TERM

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
: >"$temporary/pids"

# go build writes a stub that records the build tree's HEAD and execs the fake
# factoryctl; go version -m prints the stub so the vcs.* checks see it. With
# DARK_FACTORY_TEST_ADMIT_DURING_BUILD set, the build also admits a run, the
# way the supervisor can while a real build takes minutes.
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
        [ -z "${DARK_FACTORY_TEST_ADMIT_DURING_BUILD-}" ] || printf '1\n' >"$DARK_FACTORY_TEST_ACTIVE_RUNS"
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
# The daemon's socket lifetime: service install returns at once, and only
# after a real /bin/sleep (the store opening and migrating) does the listener
# remove a stale socket and bind, the way launchd returns before factoryd
# does; service uninstall stops it and removes the socket. web status and remote
# status dial the socket the real client is pointed at. Two knobs select an
# unclean exit: DARK_FACTORY_TEST_UNINSTALL_LEAVES=stale keeps the socket file
# with nothing answering (a SIGKILLed daemon), =listening keeps the listener
# itself, =process replaces it with a tail -f naming the home;
# DARK_FACTORY_TEST_INSTALL_DEAD installs a daemon that never listens.
cat >"$fake_bin/factoryctl" <<'FAKE'
#!/bin/sh
set -eu
echo "$*" >>"$DARK_FACTORY_TEST_FACTORYCTL_LOG"
runtimes=$HOME/.dark-factory/runtimes
stop() {
    kill $(cat "$DARK_FACTORY_TEST_PIDS") 2>/dev/null || true
    : >"$DARK_FACTORY_TEST_PIDS"
}
case "$1 $2" in
    "service uninstall")
        case "${DARK_FACTORY_TEST_UNINSTALL_LEAVES-}" in
            stale) stop ;;
            listening) ;;
            process)
                stop
                rm -f "$runtimes/factory.sock"
                tail -f "$HOME/.dark-factory/factory.sqlite3" >/dev/null 2>&1 &
                echo $! >>"$DARK_FACTORY_TEST_PIDS"
                ;;
            *) stop; rm -f "$runtimes/factory.sock" ;;
        esac
        ;;
    "service install")
        mkdir -p "$runtimes"
        [ -n "${DARK_FACTORY_TEST_INSTALL_DEAD-}" ] || {
            (cd "$runtimes" && /bin/sleep 0.2 && rm -f factory.sock && exec perl -MSocket -MIO::Socket::UNIX -e 'my $s = IO::Socket::UNIX->new(Type => SOCK_STREAM, Local => "factory.sock", Listen => 1) or die "$!\n"; while (my $c = $s->accept) { close $c }') &
            echo $! >>"$DARK_FACTORY_TEST_PIDS"
        }
        ;;
    "web status" | "remote status")
        perl -MIO::Socket::UNIX -e 'exit !IO::Socket::UNIX->new(Peer => shift)' "$DARK_FACTORY_SOCKET"
        ;;
esac
FAKE
# The script's bounded waits poll 60 times with sleep between. A no-op sleep
# makes a timeout case take about a second (macOS stretches short real sleeps
# to well over 100 ms), and the probe each poll spawns still outlasts the fake
# listener's 200 ms of startup.
printf '#!/bin/sh\n' >"$fake_bin/sleep"
chmod 755 "$fake_bin"/*
export PATH="$fake_bin:$PATH" HOME="$fake_home"
export DARK_FACTORY_TEST_ACTIVE_RUNS="$temporary/active-runs"
export DARK_FACTORY_TEST_FACTORYCTL_LOG="$temporary/factoryctl.log"
export DARK_FACTORY_TEST_PIDS="$temporary/pids"
script=$test_repository/scripts/reinstall-service.sh

backups() {
    [ -d "$fake_home/.dark-factory-backups" ] || { echo 0; return; }
    find "$fake_home/.dark-factory-backups" -name factory.sqlite3 | wc -l | tr -d ' '
}
untouched() {
    [ "$(backups)" = 0 ] || fail "$1: backup taken"
    [ ! -e "$DARK_FACTORY_TEST_FACTORYCTL_LOG" ] || fail "$1: factoryctl invoked"
}
not_installed() {
    grep -q '^service install' "$DARK_FACTORY_TEST_FACTORYCTL_LOG" && fail "$1: service installed anyway"
    rm "$DARK_FACTORY_TEST_FACTORYCTL_LOG"
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

DARK_FACTORY_TEST_ADMIT_DURING_BUILD=1 "$script" "$sha" >/dev/null 2>"$temporary/stderr" \
    && fail "run admitted during the build accepted"
grep -q 'non-terminal run' "$temporary/stderr" || fail "run admitted during the build: wrong refusal"
[ ! -e "$DARK_FACTORY_TEST_FACTORYCTL_LOG" ] || fail "run admitted during the build: service uninstalled"
printf '0\n' >"$temporary/active-runs"
rm -rf "$fake_home/.dark-factory-backups"

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

# A socket file nothing answers on is stale, not a reason to stay uninstalled.
DARK_FACTORY_TEST_UNINSTALL_LEAVES=stale "$script" "$sha" >/dev/null 2>"$temporary/stderr" \
    || fail "stale socket after uninstall refused: $(cat "$temporary/stderr")"
cmp -s "$temporary/expected.log" "$DARK_FACTORY_TEST_FACTORYCTL_LOG" \
    || fail "stale socket: factoryctl calls: $(tr '\n' ';' <"$DARK_FACTORY_TEST_FACTORYCTL_LOG")"
rm "$DARK_FACTORY_TEST_FACTORYCTL_LOG"

# A listener still accepting is the previous daemon still owning the home.
DARK_FACTORY_TEST_UNINSTALL_LEAVES=listening "$script" "$sha" >/dev/null 2>"$temporary/stderr" \
    && fail "accepting socket after uninstall accepted"
grep -q 'has not left' "$temporary/stderr" || fail "accepting socket: wrong refusal"
grep -q 'still accepts' "$temporary/stderr" || fail "accepting socket: socket not named"
not_installed "accepting socket"

# So is any process naming the home, socket or not.
DARK_FACTORY_TEST_UNINSTALL_LEAVES=process "$script" "$sha" >/dev/null 2>"$temporary/stderr" \
    && fail "process naming the home after uninstall accepted"
grep -q 'has not left' "$temporary/stderr" || fail "surviving process: wrong refusal"
grep -q 'tail -f' "$temporary/stderr" || fail "surviving process: not listed"
not_installed "surviving process"

# A daemon that dies before listening (a failed migration) must time out with
# the restore instruction, not report success.
DARK_FACTORY_TEST_INSTALL_DEAD=1 "$script" "$sha" >/dev/null 2>"$temporary/stderr" \
    && fail "daemon that never listens accepted"
grep -q 'did not listen' "$temporary/stderr" || fail "daemon that never listens: wrong refusal"
grep -q "restore $fake_home/.dark-factory-backups/.*/factory.sqlite3 over .*factory.sqlite3-shm" "$temporary/stderr" \
    || fail "daemon that never listens: restore instruction not printed"
rm "$DARK_FACTORY_TEST_FACTORYCTL_LOG"

rm -rf "$fake_home/.dark-factory-backups"
printf 'stray\n' >"$test_repository/.worktrees/build-$sha/stray"
"$script" "$sha" >/dev/null 2>"$temporary/stderr" && fail "dirty worktree accepted"
grep -q 'worktree not clean' "$temporary/stderr" || fail "dirty worktree: wrong refusal"
untouched "dirty worktree"

echo "reinstall-service tests passed"
