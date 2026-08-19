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
actions = json.loads((root / "docs/github-app-event-actions.json").read_text())
registration = json.loads((root / "docs/github-app-registration.json").read_text())
fixtures = json.loads((root / "docs/github-app-semantic-fixtures.json").read_text())

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
assert set(actions) == set(manifest["default_events"]) | {"installation_repositories"}
assert registration["artifact_kind"] == "endpoint_bound_registration_manifest"
assert registration["registration_allowed"] is False
assert registration["endpoint"] is None
assert registration["hook_attributes"] == {"url": None, "active": False}
assert registration["hook_active"] is False
assert registration["secret_store_ref"] is None
assert registration["repository_selection"] == "selected"
assert registration["repository"] == {"full_name": "baziyer/dark-factory", "id": None}
assert registration["permission_digest"] is None
assert registration["event_digest"] is None
assert registration["operator_confirmed"] is False
assert len(registration["registration_blockers"]) == 7
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

assert fixtures["schema_version"] == 1
assert fixtures["policy"] == {
    "selected_repository": "baziyer/dark-factory",
    "contents": "read",
    "pull_requests": "read",
    "delivery_identity": [
        "delivery_guid",
        "raw_payload_digest",
        "identity_tuple_digest",
    ],
}
cases = {case["name"]: case for case in fixtures["cases"]}
assert len(cases) == len(fixtures["cases"])

for name in ("first_valid_delivery", "exact_github_redelivery", "response_loss_reconciliation"):
    expected = cases[name]["expected"]
    assert expected["durable_result"] == "candidate-created"
    assert expected["mutation_count"] == 1
assert cases["first_valid_delivery"]["expected"]["decision"] == "accept"
assert cases["exact_github_redelivery"]["expected"]["decision"] == "return-existing-result"
assert cases["response_loss_reconciliation"]["expected"]["decision"] == "reconcile-existing-result"

for name in ("conflicting_guid_digest", "conflicting_guid_identity", "missing_delivery_guid"):
    expected = cases[name]["expected"]
    assert expected["decision"] == "reject"
    assert expected["mutation_count"] == 0
assert cases["conflicting_guid_digest"]["expected"]["reason"] == "idempotency_conflict"
assert cases["conflicting_guid_identity"]["expected"]["reason"] == "idempotency_conflict"
assert cases["missing_delivery_guid"]["expected"]["reason"] == "malformed_delivery_identity"

assert cases["read_only_identity_verification"]["expected"] == {
    "decision": "temporary_authority_proposal",
    "proposal_kind": "github_app_identity_verification",
    "additional_permissions": ["contents:write", "pull_requests:write"],
    "additional_repository": "operator-selected-disposable-repository",
    "provider_receives_credentials": False,
    "expires": True,
    "teardown_required": True,
}
target = cases["target_404_after_valid_scope_check"]["expected"]
assert target["durable_state"] == "target_not_found"
assert target["installation_revoked"] is False
assert target["mutation_allowed"] is False
scope = cases["confirmed_repository_scope_revocation"]["expected"]
assert scope["durable_state"] == "scope_revoked"
assert scope["installation_revoked"] is True
install = cases["confirmed_installation_revocation"]["expected"]
assert install["durable_state"] == "installation_revoked"
assert install["installation_revoked"] is True

assert cases["registration_gate_incomplete"]["expected"] == {
    "registration_allowed": False,
    "reason": "registration_contract_incomplete",
}
assert cases["registration_gate_exact_hypothetical"]["expected"] == {
    "registration_allowed": True,
    "reason": "all_inputs_present_for_trusted_setup_validation",
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
    "endpoint-bound registration",
    "hook_attributes.active: true",
    "github-app-semantic-fixtures.json",
    "temporary-authority proposal",
    "exact redelivery",
    "target_not_found",
    "scope_revoked",
    "installation_revoked",
    "429",
    "degraded_rate_limited",
    "degraded_unavailable",
    "installation ID",
    "Sol/xhigh",
    "dark-factory-site",
    "homebrew-tap",
):
    assert required in text, required
print("github app manifest and semantic fixture tests passed")
PY
