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
worktree="$repository_root/.worktrees/build-$sha"
bin="$repository_root/.worktrees/bin-$sha"
relay_origin=wss://relay.darkfactory.build

[ -f "$db" ] || { echo "no store at $db" >&2; exit 1; }
active=$(sqlite3 "$db" "SELECT count(*) FROM runs WHERE phase <> 'terminal'")
[ "$active" = 0 ] || { echo "refusing: $active non-terminal run(s) in $db" >&2; exit 1; }

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

"$bin/factoryctl" service uninstall --home "$home"
"$bin/factoryctl" service install --home "$home" --relay-origin "$relay_origin"
sleep 3
"$bin/factoryctl" service status --home "$home"
export DARK_FACTORY_SOCKET="$home/runtimes/factory.sock"
export DARK_FACTORY_OPERATOR_TOKEN_FILE="$home/operator.token"
"$bin/factoryctl" web status
"$bin/factoryctl" remote status
echo "user_version now: $(sqlite3 "$db" 'PRAGMA user_version')"
echo "binaries: $bin (keep the previous bin-* for rollback; after a failed migration restore $backup)"
