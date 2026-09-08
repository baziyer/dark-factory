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

[ -f "$db" ] || { echo "no store at $db" >&2; exit 1; }
refuse_active_runs

git -C "$repository_root" fetch -q origin
[ -d "$worktree" ] || git -C "$repository_root" worktree add -q --detach "$worktree" "$sha"
[ -z "$(git -C "$worktree" status --porcelain=v1 --untracked-files=all)" ] \
    || { echo "worktree not clean: $worktree" >&2; exit 1; }
[ "$(git -C "$worktree" rev-parse HEAD)" = "$sha" ] \
    || { echo "worktree not at $sha: $worktree" >&2; exit 1; }

mkdir -p "$bin"
for cmd in factoryctl factoryd factory-runner; do
    (cd "$worktree" && go build -trimpath -o "$bin/$cmd" "./cmd/$cmd")
    go version -m "$bin/$cmd" | grep -q "vcs.revision=$sha" \
        || { echo "$cmd not built from $sha" >&2; exit 1; }
    go version -m "$bin/$cmd" | grep -q "vcs.modified=false" \
        || { echo "$cmd built from a modified tree" >&2; exit 1; }
done

backups="$HOME/.dark-factory-backups"
backup="$backups/$(date -u +%Y%m%dT%H%M%S)-$sha"
mkdir -p "$backup"
chmod 700 "$backups" "$backup"
sqlite3 "$db" ".backup $backup/factory.sqlite3"
chmod 600 "$backup/factory.sqlite3"
echo "backup: $backup (user_version $(sqlite3 "$backup/factory.sqlite3" 'PRAGMA user_version'))"

refuse_active_runs
"$bin/factoryctl" service uninstall --home "$home"
[ ! -e "$socket" ] || { echo "socket still present after uninstall: $socket" >&2; exit 1; }
"$bin/factoryctl" service install --home "$home" --relay-origin "$relay_origin"
# launchd returns from bootstrap before factoryd listens, and factoryd opens
# (and migrates) the store before it listens, so the socket appearing means
# the migration finished. Bounded: a daemon that dies on a failed migration
# never listens.
waited=0
until [ -S "$socket" ]; do
    [ "$waited" -lt 300 ] || {
        echo "factoryd did not listen on $socket within 60s" >&2
        echo "check 'factoryctl service status --home $home' and the daemon log before restoring $backup" >&2
        exit 1
    }
    sleep 0.2
    waited=$((waited + 1))
done
"$bin/factoryctl" service status --home "$home"
export DARK_FACTORY_SOCKET="$socket"
export DARK_FACTORY_OPERATOR_TOKEN_FILE="$home/operator.token"
"$bin/factoryctl" web status
"$bin/factoryctl" remote status
echo "user_version now: $(sqlite3 "$db" 'PRAGMA user_version')"
echo "binaries: $bin (keep the previous bin-* for rollback; after a failed migration restore $backup)"
