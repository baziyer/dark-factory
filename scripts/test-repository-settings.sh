#!/bin/sh
# Static regression for the no-live-mutation repository-settings proposal.
set -eu

repository_root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
manifest="$repository_root/.github/repository-settings.yml"
workflow="$repository_root/.github/workflows/ci.yml"
publisher="$repository_root/scripts/github-repo-settings.sh"

grep -Fq 'repository: baziyer/dark-factory' "$manifest"
grep -Fq 'context: required' "$manifest"
grep -Fq 'integration_id: 15368' "$manifest"
grep -Fq '  required:' "$workflow"
grep -Fq '    if: always()' "$workflow"
grep -Fq '    needs: [checks, linux, control-plane]' "$workflow"
grep -Fq "if: needs.checks.result != 'success' || needs.linux.result != 'success' || needs.control-plane.result != 'success'" "$workflow"
grep -Fq '"context": "required"' "$publisher"
if grep -Fq '"context": "checks"' "$publisher"; then
    echo "repository settings still require the macOS-only checks context" >&2
    exit 1
fi

echo "repository settings proposal passed static checks"
