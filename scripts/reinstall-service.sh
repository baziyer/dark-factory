#!/bin/sh
set -eu

usage() {
    echo "usage: scripts/reinstall-service.sh <commit-sha>" >&2
    echo "  builds factoryctl, factoryd and factory-runner from a clean detached worktree at the" >&2
    echo "  commit, backs up \$HOME/.dark-factory/factory.sqlite3, reinstalls the service from the" >&2
    echo "  new build, and prints service, web and remote status plus the new PRAGMA user_version" >&2
}

sha="${1:-}"
if [ "$sha" = "-h" ] || [ "$sha" = "--help" ]; then
    usage
    exit 0
fi
case "$sha" in
    "" | *[!0-9a-f]*)
        usage
        exit 1
        ;;
esac
[ "${#sha}" -eq 40 ] || { usage; exit 1; }

repository_root=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
home="$HOME/.dark-factory"
db="$home/factory.sqlite3"
socket="$home/runtimes/factory.sock"
worktree="$repository_root/.worktrees/build-$sha"
bin="$repository_root/.worktrees/bin-$sha"
relay_origin=wss://relay.darkfactory.build

# Checked before the build and again right before the uninstall that would
# kill a run the supervisor admitted while the build was running.
refuse_active_runs() {
    active=$(sqlite3 "$db" "SELECT count(*) FROM runs WHERE phase <> 'terminal'")
    [ "$active" = 0 ] || { echo "refusing: $active non-terminal run(s) in $db" >&2; exit 1; }
}

# A connect, as the service e2e probes: nc -z cannot scan Unix sockets on macOS.
listening() {
    perl -MIO::Socket::UNIX -e 'exit !IO::Socket::UNIX->new(Peer => shift)' "$socket" 2>/dev/null
}
# The service e2e's census: launchd forgetting the job and the listener closing
# do not prove factoryd has released the home, so nothing naming the home may
# survive before a second owner starts. This script's own argv may name it.
# ponytail: pgrep -f also matches an operator's own tail -f on a home file;
# the timeout lists the survivors and a rerun recovers.
survivors() {
    pgrep -fl "$home" | grep -v "^$$ "
}
previous_left() {
    ! listening && ! survivors >/dev/null
}
# Polls the predicate for up to 60 x 1s, then hands over to the timeout handler.
await() {
    waited=0
    until "$1"; do
        [ "$waited" -lt 60 ] || "$2"
        sleep 1
        waited=$((waited + 1))
    done
}
uninstall_stalled() {
    echo "the previous factoryd has not left $home within 60s; the service is uninstalled" >&2
    survivors >&2 || echo "$socket still accepts connections" >&2
    echo "rerun once it has exited" >&2
    exit 1
}
install_stalled() {
    echo "factoryd did not listen on $socket within 60s" >&2
    echo "check 'factoryctl service status --home $home' and the daemon log; after a failed migration" >&2
    echo "restore $backup/factory.sqlite3 over $db, remove $db-wal and $db-shm, and reinstall the previous bin-*" >&2
    exit 1
}

[ -f "$db" ] || { echo "no store at $db" >&2; exit 1; }
refuse_active_runs

git -C "$repository_root" fetch -q origin
# An empty hooksPath, as new-worktree.sh: the repository's configured hooks
# must not run inside the worktree the live service is built from.
empty_hooks=$(mktemp -d "${TMPDIR:-/tmp}/dark-factory-empty-hooks.XXXXXX")
trap 'rmdir "$empty_hooks" 2>/dev/null || true' EXIT
[ -d "$worktree" ] || git -C "$repository_root" -c core.hooksPath="$empty_hooks" \
    worktree add -q --detach "$worktree" "$sha"
[ -z "$(git -C "$worktree" status --porcelain=v1 --untracked-files=all)" ] \
    || { echo "worktree not clean: $worktree" >&2; exit 1; }
[ "$(git -C "$worktree" rev-parse HEAD)" = "$sha" ] \
    || { echo "worktree not at $sha: $worktree" >&2; exit 1; }

# The compiler pinned the way the release workflow pins it, from the go.mod
# being built: the vcs.* checks below prove the source, not the toolchain.
# A missing go.mod reads as no version and takes the refusal below.
go_version=$(sed -n 's/^go \([0-9][0-9.]*\)$/\1/p' "$worktree/go.mod" 2>/dev/null || :)
case "$go_version" in
    *.*.*) ;;
    *) echo "could not read the exact Go version from $worktree/go.mod" >&2; exit 1 ;;
esac
export GOTOOLCHAIN="go$go_version" GOENV=off GOAUTH=off

mkdir -p "$bin"
for cmd in factoryctl factoryd factory-runner; do
    (cd "$worktree" && go build -trimpath -o "$bin/$cmd" "./cmd/$cmd")
    go version -m "$bin/$cmd" | grep -q "vcs.revision=$sha" \
        || { echo "$cmd not built from $sha" >&2; exit 1; }
    go version -m "$bin/$cmd" | grep -q "vcs.modified=false" \
        || { echo "$cmd built from a modified tree" >&2; exit 1; }
done

backup="$HOME/.dark-factory-backups/$(date -u +%Y%m%dT%H%M%S)-$sha"
# Owner-only from creation: a chmod after the copy leaves the store readable
# for as long as the copy takes.
(umask 077 && mkdir -p "$backup" && sqlite3 "$db" ".backup $backup/factory.sqlite3")
echo "backup: $backup (user_version $(sqlite3 "$backup/factory.sqlite3" 'PRAGMA user_version'))"

refuse_active_runs
"$bin/factoryctl" service uninstall --home "$home"
# bootout returns once launchd forgets the job; factoryd unlinks its socket
# before it closes the store and releases the home flock. A socket file that
# nothing answers on is stale, and factoryd removes it on its next start.
await previous_left uninstall_stalled
"$bin/factoryctl" service install --home "$home" --relay-origin "$relay_origin"
# launchd returns from bootstrap before factoryd listens, and factoryd opens
# (and migrates) the store before it listens, so the socket accepting means
# the migration finished. Bounded: a daemon that dies on a failed migration
# never listens.
await listening install_stalled
"$bin/factoryctl" service status --home "$home"
export DARK_FACTORY_SOCKET="$socket"
export DARK_FACTORY_OPERATOR_TOKEN_FILE="$home/operator.token"
"$bin/factoryctl" web status
"$bin/factoryctl" remote status
echo "user_version now: $(sqlite3 "$db" 'PRAGMA user_version')"
echo "binaries: $bin (keep the previous bin-* for rollback; after a failed migration restore $backup/factory.sqlite3 over $db and remove $db-wal and $db-shm)"
