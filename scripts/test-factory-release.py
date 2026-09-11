#!/usr/bin/env python3
"""Small, offline fixtures for the exact-merge release controller."""
import importlib.util
import json
import tempfile
import unittest
from pathlib import Path
from unittest import mock


MODULE = Path(__file__).with_name("factory-release.py")
SPEC = importlib.util.spec_from_file_location("factory_release", MODULE)
release = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(release)


SHA = "a" * 40
HEAD = "b" * 40


def config(journal):
    return {
        "repository": "example/factory",
        "base": "main",
        "journal": str(journal),
        "deploy_argv": ["/bin/true"],
        "verify_argv": ["/bin/true"],
        "review_verifier": [str(MODULE.with_name("verify-adversarial-review.sh"))],
    }


def snapshot():
    return (
        {
            "state": "MERGED",
            "baseRefName": "main",
            "mergeCommitSha": SHA,
            "headRefOid": HEAD,
        },
        SHA,
        [{"commit_id": HEAD, "state": "APPROVED", "user": {"id": 319516570},
          "body": f"Dark-Factory-Review: allow {HEAD}"}],
        [{"status": "COMPLETED", "conclusion": "SUCCESS"}],
    )


class ReleaseFixtures(unittest.TestCase):
    def test_wrong_sha_verification_is_rejected(self):
        with self.assertRaises(release.ReleaseError):
            release.verify_output(json.dumps({"sha": "c" * 40, "healthy": True}), SHA)

    def test_unmerged_pr_is_rejected(self):
        pr, default, reviews, checks = snapshot()
        pr["state"] = "OPEN"
        with self.assertRaises(release.ReleaseError):
            release.merge_gate(pr, default, reviews, checks, config(Path("/tmp/release.json")), SHA)

    def test_review_without_exact_allow_is_rejected(self):
        pr, default, reviews, checks = snapshot()
        reviews[0]["body"] = "Dark-Factory-Review: deny"
        with self.assertRaises(release.ReleaseError):
            release.merge_gate(pr, default, reviews, checks, config(Path("/tmp/release.json")), SHA)

    def test_failed_check_is_rejected(self):
        pr, default, reviews, checks = snapshot()
        checks[0]["conclusion"] = "FAILURE"
        with self.assertRaises(release.ReleaseError):
            release.merge_gate(pr, default, reviews, checks, config(Path("/tmp/release.json")), SHA)

    def test_live_already_records_verified_without_deploy(self):
        with tempfile.TemporaryDirectory() as directory:
            cfg = config(Path(directory) / "release.json")
            with mock.patch.object(release, "gh_snapshot", return_value=snapshot()), \
                 mock.patch.object(release, "review_gate"), \
                 mock.patch.object(release, "probe", return_value={"sha": SHA, "healthy": True}), \
                 mock.patch.object(release, "run") as command:
                result = release.once(cfg, 633)
            self.assertEqual(result["state"], "verified")
            command.assert_not_called()

    def test_running_release_with_unknown_probe_becomes_blocked(self):
        with tempfile.TemporaryDirectory() as directory:
            journal = Path(directory) / "release.json"
            cfg = config(journal)
            release.atomic_json(journal, {"version": 1, "releases": {
                "633": {"pr": 633, "sha": SHA, "state": "running",
                         "config_fingerprint": release.config_fingerprint(cfg)}
            }})
            with mock.patch.object(release, "probe", side_effect=release.ReleaseError("probe unavailable")):
                with self.assertRaises(release.ReleaseError):
                    release.once(cfg, 633)
            receipt = release.load(journal)["releases"]["633"]
            self.assertEqual(receipt["state"], "blocked")
            self.assertIn("ambiguous", receipt["error"])

    def test_unavailable_predeploy_probe_never_deploys(self):
        with tempfile.TemporaryDirectory() as directory:
            cfg = config(Path(directory) / "release.json")
            with mock.patch.object(release, "gh_snapshot", return_value=snapshot()), \
                 mock.patch.object(release, "review_gate"), \
                 mock.patch.object(release, "probe", side_effect=release.ReleaseError("probe unavailable")), \
                 mock.patch.object(release, "run") as command:
                with self.assertRaises(release.ReleaseError):
                    release.once(cfg, 633)
            self.assertEqual(release.load(Path(cfg["journal"]))["releases"]["633"]["state"], "blocked")
            command.assert_not_called()

    def test_real_review_gate_flattens_multiline_allow(self):
        release.review_gate(config(Path("/tmp/release.json")), HEAD, [{
            "commit_id": HEAD,
            "state": "APPROVED",
            "user": {"id": 319516570},
            "body": f"findings\nDark-Factory-Review:\tallow {HEAD}\nfinal",
        }])

    def test_real_review_gate_rejects_block(self):
        with self.assertRaises(release.ReleaseError):
            release.review_gate(config(Path("/tmp/release.json")), HEAD, [{
                "commit_id": HEAD,
                "state": "CHANGES_REQUESTED",
                "user": {"id": 319516570},
                "body": f"findings\nDark-Factory-Review: block {HEAD}",
            }])


if __name__ == "__main__":
    unittest.main()
