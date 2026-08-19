#!/bin/sh
# Apply this repository's GitHub configuration: labels, merge settings, the
# `main` branch ruleset, and security features. Idempotent — run it again
# after changing anything below. Needs `gh` authenticated as a repository
# admin. Rulesets and the security features need a public repository (or
# GitHub Pro); while the repository is private those steps report the 403
# and the script exits non-zero so the gap is visible.
set -u

repository=$(gh repo view --json nameWithOwner --jq .nameWithOwner) || exit 1
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
state:queued|D4C5F9|Accepted work has not started
state:in-progress|1D76DB|Work is actively being executed
state:blocked|B60205|Work needs a bounded resolution before it can continue
state:review|FBCA04|A change is awaiting independent review or recheck
state:release-ready|0E8A16|Exact integration or release preconditions are satisfied
size:S|C2E0C6|A focused change; hours
size:M|FBCA04|A day or two; touches more than one crate or a load-bearing path
size:L|E99695|Needs design or an upstream change first
decision|D4C5F9|Needs the maintainer to decide, not (only) code
security|D93F0B|Widens or narrows what a session, webhook caller, or PR can reach
LABELS

step "merge settings (linear history: squash or rebase only; delete merged branches)"
try gh repo edit "$repository" --enable-squash-merge --enable-rebase-merge \
    --enable-merge-commit=false --delete-branch-on-merge

# Two rulesets on main, because a bypass applies to every rule in ITS
# ruleset: "main-protect" (no bypass for anyone, not even the admin) makes
# the green `checks` run, linear history, and no force-push/deletion
# unconditional; "main-review" carries the pull-request rule with a
# Repository-admin (id 5) bypass in pull-request mode, since GitHub never
# lets an author approve their own PR and this repository has one
# maintainer. Everyone else needs the PR, the CODEOWNERS approval, resolved
# threads, and a green `checks` from GitHub Actions (integration 15368)
# against a head that is up to date with main. Nobody pushes to main.
apply_ruleset() {
    printf '%s' "$2" > "$tmp"
    existing=$(gh api "repos/$repository/rulesets" --jq ".[] | select(.name==\"$1\") | .id" 2>/dev/null | head -1)
    case "$existing" in *[!0-9]*) existing="" ;; esac   # an error body is not an id
    if [ -n "$existing" ]; then
        try gh api -X PUT "repos/$repository/rulesets/$existing" --input "$tmp" >/dev/null
    else
        try gh api -X POST "repos/$repository/rulesets" --input "$tmp" >/dev/null
    fi
}

step "ruleset: main-protect (checks + linear history + no force-push/delete; no bypass)"
apply_ruleset main-protect '{
  "name": "main-protect",
  "target": "branch",
  "enforcement": "active",
  "bypass_actors": [],
  "conditions": { "ref_name": { "include": ["~DEFAULT_BRANCH"], "exclude": [] } },
  "rules": [
    { "type": "deletion" },
    { "type": "non_fast_forward" },
    { "type": "required_linear_history" },
    { "type": "required_status_checks", "parameters": {
        "strict_required_status_checks_policy": true,
        "required_status_checks": [ { "context": "checks", "integration_id": 15368 } ] } }
  ]
}'

step "ruleset: main-review (pull request + CODEOWNERS approval; admin may bypass via a PR)"
apply_ruleset main-review '{
  "name": "main-review",
  "target": "branch",
  "enforcement": "active",
  "bypass_actors": [
    { "actor_id": 5, "actor_type": "RepositoryRole", "bypass_mode": "pull_request" }
  ],
  "conditions": { "ref_name": { "include": ["~DEFAULT_BRANCH"], "exclude": [] } },
  "rules": [
    { "type": "pull_request", "parameters": {
        "required_approving_review_count": 1,
        "dismiss_stale_reviews_on_push": true,
        "require_code_owner_review": true,
        "require_last_push_approval": false,
        "required_review_thread_resolution": true } }
  ]
}'

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
