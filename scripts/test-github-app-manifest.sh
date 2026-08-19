#!/bin/sh
set -eu

repository_root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)

python3 - "$repository_root" <<'PY'
import json
import pathlib
import sys

root = pathlib.Path(sys.argv[1])
manifest = json.loads((root / "docs/github-app-manifest.json").read_text())
selection = json.loads((root / "docs/github-app-repository-selection.json").read_text())

expected_permissions = {
    "actions": "read",
    "checks": "write",
    "contents": "read",
    "issues": "write",
    "metadata": "read",
    "pull_requests": "read",
    "statuses": "read",
}
assert manifest["public"] is False
assert manifest["request_oauth_on_install"] is False
assert manifest["default_permissions"] == expected_permissions
assert set(manifest["default_events"]) == {
    "check_run",
    "check_suite",
    "installation",
    "installation_target",
    "issue_comment",
    "issues",
    "pull_request",
    "pull_request_review",
    "release",
    "repository",
    "workflow_run",
}
assert "hook_attributes" not in manifest
assert selection == {
    "repository_selection": "selected",
    "repositories": [{
        "full_name": "baziyer/dark-factory",
        "purpose": "#153 issue intake and #154 release-readiness observation",
    }],
    "not_requested": [
        "baziyer/dark-factory-site",
        "baziyer/homebrew-tap",
        "all other repositories",
    ],
    "operator_confirmation_required": True,
}

text = (root / "docs/github-app.md").read_text()
for required in (
    "factory@localhost",
    "contents:write",
    "provider worktree",
    "provider argv/env/config",
    "task bodies, logs, public events",
    "crash/retry diagnostics, or run bundles",
    "permission_revision",
    "event_revision",
    "429",
    "degraded_rate_limited",
    "degraded_unavailable",
    "installation ID",
    "Sol/xhigh",
    "dark-factory-site",
    "homebrew-tap",
):
    assert required in text, required
print("github app manifest tests passed")
PY
