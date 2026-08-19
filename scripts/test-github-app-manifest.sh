#!/bin/sh
set -eu

repository_root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)

python3 - "$repository_root" <<'PY'
import json
import hashlib
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

proposal = cases["read_only_identity_verification"]["expected"]
assert proposal["decision"] == "temporary_authority_proposal_template"
assert proposal["proposal_kind"] == "github_app_identity_verification"
assert proposal["authority_ready"] is False
assert proposal["execution_allowed"] is False
template = proposal["template"]
assert template["proposal_id"] is None
assert template["idempotency_key"] is None
assert template["target"] == {
    "repository_full_name": None,
    "repository_id": None,
    "branch_ref": None,
    "pull_request_number": None,
    "test_commit_sha": None,
}
assert template["approver_principal"] is None
assert template["expires_at"] is None
assert template["expected_revisions"] == {
    "permission_revision": None,
    "event_revision": None,
    "identity_tuple_revision": None,
}
assert template["requested_permissions"] == ["contents:write", "pull_requests:write"]
assert template["allowed_operations"] == [
    "create_one_disposable_branch",
    "create_one_test_commit",
    "create_one_test_pull_request",
    "read_bot_identity_and_logo",
    "read_commit_attribution",
    "delete_test_branch_and_pull_request",
]
assert template["forbidden_operations"] == [
    "merge_pull_request",
    "publish_or_edit_release",
    "write_any_other_branch",
    "write_selected_production_repository",
    "dispatch_or_modify_workflow",
    "grant_permissions",
    "send_credentials_to_provider",
]
assert template["teardown"] == {
    "repository_full_name": None,
    "repository_id": None,
    "branch_ref": None,
    "pull_request_number": None,
    "installation_id": None,
    "temporary_authority_id": None,
    "token_reference": None,
    "steps": [
        "delete_test_branch_and_pull_request",
        "remove_disposable_repository_from_installation",
        "revoke_temporary_authority",
        "erase_short_lived_token",
    ],
}
assert template["provider_receives_credentials"] is False
assert proposal["blockers"] == [
    "exact_proposal_id_required",
    "exact_idempotency_key_required",
    "exact_disposable_repository_id_and_name_required",
    "exact_branch_and_pull_request_targets_required",
    "operator_approver_principal_required",
    "expiry_timestamp_required",
    "permission_event_and_identity_revisions_required",
    "teardown_identities_required",
]
# A placeholder-complete object must not become executable merely by flipping a boolean.
assert any(value is None for value in (
    template["proposal_id"],
    template["idempotency_key"],
    template["target"]["repository_id"],
    template["approver_principal"],
    template["expires_at"],
    template["expected_revisions"]["permission_revision"],
    template["teardown"]["temporary_authority_id"],
))
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
assert cases["registration_gate_digest_mismatch"]["expected"] == {
    "registration_allowed": False,
    "reason": "registration_digest_mismatch",
}

def digest(value):
    canonical = json.dumps(value, sort_keys=True, separators=(",", ":")).encode()
    return "sha256:" + hashlib.sha256(canonical).hexdigest()

expected_permission_digest = digest(manifest["default_permissions"])
expected_event_digest = digest({
    "default_events": manifest["default_events"],
    "receiver_actions": actions,
})
ready_input = cases["registration_gate_exact_hypothetical"]["input"]
assert ready_input["permission_digest"] == expected_permission_digest
assert ready_input["event_digest"] == expected_event_digest
mismatch_input = cases["registration_gate_digest_mismatch"]["input"]
assert mismatch_input["permission_digest"] != expected_permission_digest
assert mismatch_input["event_digest"] == expected_event_digest

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
