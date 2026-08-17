#!/bin/sh
# Apply this repository's GitHub configuration: labels, merge settings, the
# `main` branch ruleset, and security features. Idempotent — run it again
# after changing anything below. Needs `gh` authenticated as a repository
# admin. Rulesets and the security features need a public repository (or
# GitHub Pro); while the repository is private those steps report the 403
# and the script exits non-zero so the gap is visible.
set -u

repository=$(gh repo view --json nameWithOwner --jq .nameWithOwner)
failed=0
tmp=$(mktemp)
trap 'rm -f "$tmp"' EXIT
step() { printf '\n== %s\n' "$1"; }
try() { if "$@"; then :; else echo "   FAILED: $*" >&2; failed=1; fi; }

step "labels"
while IFS='|' read -r name color description; do
    try gh label create "$name" --color "$color" --description "$description" --force >/dev/null
done <<'LABELS'
known-issue|B60205|Imported from the known-issues triage; the smallest fix is in the body
area:daemon|1D76DB|factoryd: sessions, dispatch, store, hooks, webhooks
area:cli|1D76DB|factoryctl
area:tui|1D76DB|factory-tui
area:providers|1D76DB|claude/codex/shell adapters and their generated config
area:docs|0075CA|README, ARCHITECTURE, docs/
area:ci|5319E7|CI, release, toolchain, scripts
size:S|C2E0C6|A focused change; hours
size:M|FBCA04|A day or two; touches more than one crate or a load-bearing path
size:L|E99695|Needs design or an upstream change first
decision|D4C5F9|Needs the maintainer to decide, not (only) code
security|D93F0B|Widens or narrows what a session, webhook caller, or PR can reach
LABELS

step "merge settings (linear history: squash or rebase only; delete merged branches)"
try gh repo edit "$repository" --enable-squash-merge --enable-rebase-merge \
    --enable-merge-commit=false --delete-branch-on-merge

step "ruleset: main"
ruleset=$(cat <<'JSON'
{
  "name": "main",
  "target": "branch",
  "enforcement": "active",
  "bypass_actors": [
    { "actor_id": 5, "actor_type": "RepositoryRole", "bypass_mode": "pull_request" }
  ],
  "conditions": { "ref_name": { "include": ["~DEFAULT_BRANCH"], "exclude": [] } },
  "rules": [
    { "type": "deletion" },
    { "type": "non_fast_forward" },
    { "type": "required_linear_history" },
    { "type": "pull_request", "parameters": {
        "required_approving_review_count": 1,
        "dismiss_stale_reviews_on_push": true,
        "require_code_owner_review": true,
        "require_last_push_approval": false,
        "required_review_thread_resolution": true } },
    { "type": "required_status_checks", "parameters": {
        "strict_required_status_checks_policy": true,
        "required_status_checks": [ { "context": "checks" } ] } }
  ]
}
JSON
)
# The bypass actor is the Repository admin role (id 5), pull-request mode:
# the maintainer can merge a PR that lacks the (self-)approval GitHub never
# lets an author give, but still cannot push to main directly, force-push,
# or delete it. Everyone else needs the PR, the CODEOWNERS approval, and a
# green `checks` run against the current head.
printf '%s' "$ruleset" > "$tmp"
if existing=$(gh api "repos/$repository/rulesets" --jq '.[] | select(.name=="main") | .id' 2>/dev/null) \
    && [ -n "$existing" ]; then
    try gh api -X PUT "repos/$repository/rulesets/$existing" --input "$tmp" >/dev/null
else
    try gh api -X POST "repos/$repository/rulesets" --input "$tmp" >/dev/null
fi

step "security: private vulnerability reporting, dependabot alerts, secret scanning + push protection"
try gh api -X PUT "repos/$repository/private-vulnerability-reporting" >/dev/null
try gh api -X PUT "repos/$repository/vulnerability-alerts" >/dev/null
printf '%s' '{"security_and_analysis":{"secret_scanning":{"status":"enabled"},"secret_scanning_push_protection":{"status":"enabled"}}}' > "$tmp"
try gh api -X PATCH "repos/$repository" --input "$tmp" >/dev/null

step "actions: workflows from every outside contributor's fork PR need approval before they run"
printf '%s' '{"approval_policy":"all_external_contributors"}' > "$tmp"
try gh api -X PUT "repos/$repository/actions/permissions/fork-pr-contributor-approval" --input "$tmp" >/dev/null

if [ "$failed" -ne 0 ]; then
    printf '\nsome steps failed (a private repository on a free plan cannot use rulesets or the security features: flip it public and re-run)\n' >&2
    exit 1
fi
printf '\nall settings applied to %s\n' "$repository"
