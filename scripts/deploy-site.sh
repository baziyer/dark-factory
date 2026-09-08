#!/bin/sh
set -eu

usage() {
    echo "usage: scripts/deploy-site.sh <site-commit-sha>" >&2
    echo "  deploys the public site to Vercel production from a clean detached worktree of" >&2
    echo "  \$DARK_FACTORY_SITE (default \$HOME/dark-factory-site) at the commit" >&2
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

site="${DARK_FACTORY_SITE:-$HOME/dark-factory-site}"
worktree="$site/.worktrees/deploy-$sha"

[ -f "$site/.vercel/project.json" ] || { echo "no Vercel link at $site/.vercel/project.json" >&2; exit 1; }
git -C "$site" fetch -q origin
# An empty hooksPath, as new-worktree.sh: the site repository's configured
# hooks (a pnpm install sets core.hooksPath) must not run on the deploy path.
empty_hooks=$(mktemp -d "${TMPDIR:-/tmp}/dark-factory-empty-hooks.XXXXXX")
trap 'rmdir "$empty_hooks" 2>/dev/null || true' EXIT
[ -d "$worktree" ] || git -C "$site" -c core.hooksPath="$empty_hooks" \
    worktree add -q --detach "$worktree" "$sha"
[ -z "$(git -C "$worktree" status --porcelain=v1 --untracked-files=all)" ] \
    || { echo "worktree not clean: $worktree" >&2; exit 1; }
[ "$(git -C "$worktree" rev-parse HEAD)" = "$sha" ] \
    || { echo "worktree not at $sha: $worktree" >&2; exit 1; }

cd "$worktree"
mkdir -p .vercel
cp "$site/.vercel/project.json" .vercel/project.json
corepack pnpm install --frozen-lockfile --prefer-offline >/dev/null
node scripts/verify-factory-artifacts.mjs
vercel deploy --prod --yes
vercel inspect https://app.darkfactory.build
