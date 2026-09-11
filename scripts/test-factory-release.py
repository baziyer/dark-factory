#!/usr/bin/env python3
"""Small, offline fixtures for the exact-merge release controller."""
import importlib.util
import json
import os
import sys
import tempfile
import time
import unittest
from pathlib import Path
from unittest import mock


MODULE = Path(__file__).with_name("factory-release.py")
SPEC = importlib.util.spec_from_file_location("factory_release", MODULE)
release = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(release)


SHA = "a" * 40
HEAD = "b" * 40
OLD = "c" * 40
SECOND = "d" * 40


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

    def test_default_config_keeps_the_existing_receipt_fingerprint(self):
        cfg = config(Path("/tmp/release.json"))
        old = {key: cfg.get(key) for key in ("repository", "base", "deploy_argv", "verify_argv", "review_verifier", "command_timeout")}
        import hashlib
        expected = hashlib.sha256(json.dumps(old, sort_keys=True, separators=(",", ":")).encode()).hexdigest()
        self.assertEqual(release.config_fingerprint(cfg), expected)

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

    def test_running_release_with_healthy_old_sha_becomes_blocked(self):
        with tempfile.TemporaryDirectory() as directory:
            journal = Path(directory) / "release.json"
            cfg = config(journal)
            release.atomic_json(journal, {"version": 1, "releases": {
                "633": {"pr": 633, "sha": SHA, "state": "running",
                         "config_fingerprint": release.config_fingerprint(cfg)}
            }})
            with mock.patch.object(release, "probe", return_value={"sha": "c" * 40, "healthy": True}):
                with self.assertRaises(release.ReleaseError):
                    release.once(cfg, 633)
            self.assertEqual(release.load(journal)["releases"]["633"]["state"], "blocked")

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

    def test_range_records_every_explicit_source_footer(self):
        cfg = config(Path("/tmp/release.json"))
        def gh(argv, *unused):
            command = " ".join(argv)
            if "/compare/" in command:
                return json.dumps({"status": "ahead", "total_commits": 2,
                                   "commits": [{"sha": SHA}, {"sha": SECOND}]})
            if SHA + "/pulls" in command:
                return json.dumps([[{"number": 10, "merge_commit_sha": SHA, "merged_at": "now", "base": {"ref": "main"}}]])
            if SECOND + "/pulls" in command:
                return json.dumps([[{"number": 11, "merge_commit_sha": SECOND, "merged_at": "now", "base": {"ref": "main"}}]])
            if "pr view 10" in command:
                return json.dumps({"state": "MERGED", "baseRefName": "main", "mergeCommit": {"oid": SHA}, "body": "Changes\n\nRefs #602\n"})
            if "pr view 11" in command:
                return json.dumps({"state": "MERGED", "baseRefName": "main", "mergeCommit": {"oid": SECOND}, "body": "Changes\n\nCloses #512\n"})
            self.fail(command)
        with mock.patch.object(release, "run", side_effect=gh):
            sources, mode = release.range_sources(cfg, OLD, SECOND)
        self.assertEqual(mode, "range")
        self.assertEqual(sources, [{"pr": 10, "merge_sha": SHA, "issue": 602, "reference": "refs"},
                                   {"pr": 11, "merge_sha": SECOND, "issue": 512, "reference": "closes"}])

    def test_range_rejects_missing_source_footer(self):
        cfg = config(Path("/tmp/release.json"))
        def gh(argv, *unused):
            command = " ".join(argv)
            if "/compare/" in command:
                return json.dumps({"status": "ahead", "total_commits": 1, "commits": [{"sha": SHA}]})
            if "/pulls" in command:
                return json.dumps([[{"number": 10, "merge_commit_sha": SHA, "merged_at": "now", "base": {"ref": "main"}}]])
            return json.dumps({"state": "MERGED", "baseRefName": "main", "mergeCommit": {"oid": SHA}, "body": "No source"})
        with mock.patch.object(release, "run", side_effect=gh), self.assertRaisesRegex(release.ReleaseError, "exactly one"):
            release.range_sources(cfg, OLD, SHA)

    def test_range_rejects_source_reference_that_is_not_a_footer(self):
        cfg = config(Path("/tmp/release.json"))
        def gh(argv, *unused):
            command = " ".join(argv)
            if "/compare/" in command:
                return json.dumps({"status": "ahead", "total_commits": 1, "commits": [{"sha": SHA}]})
            if "/pulls" in command:
                return json.dumps([[{"number": 10, "merge_commit_sha": SHA, "merged_at": "now", "base": {"ref": "main"}}]])
            return json.dumps({"state": "MERGED", "baseRefName": "main", "mergeCommit": {"oid": SHA}, "body": "Refs #602\nMore text"})
        with mock.patch.object(release, "run", side_effect=gh), self.assertRaisesRegex(release.ReleaseError, "footer"):
            release.range_sources(cfg, OLD, SHA)

    def test_range_accepts_the_app_marker_after_its_footer(self):
        cfg = config(Path("/tmp/release.json"))
        marker = "<!-- dark-factory-operation:12345678-1234-1234-1234-123456789abc:" + "e" * 64 + " -->"
        def gh(argv, *unused):
            command = " ".join(argv)
            if "/compare/" in command:
                return json.dumps({"status": "ahead", "total_commits": 1, "commits": [{"sha": SHA}]})
            if "/pulls" in command:
                return json.dumps([[{"number": 10, "merge_commit_sha": SHA, "merged_at": "now", "base": {"ref": "main"}}]])
            return json.dumps({"state": "MERGED", "baseRefName": "main", "mergeCommit": {"oid": SHA}, "body": "Work\n\nRefs #602\n\n" + marker})
        with mock.patch.object(release, "run", side_effect=gh):
            self.assertEqual(release.range_sources(cfg, OLD, SHA)[0][0]["issue"], 602)

    @unittest.skipUnless(os.name == "posix", "process groups require POSIX")
    def test_timeout_terminates_the_hook_process_group(self):
        with tempfile.TemporaryDirectory() as directory:
            child_file = Path(directory) / "child.pid"
            code = "import pathlib,subprocess,sys,time; pathlib.Path(sys.argv[1]).write_text(str(subprocess.Popen(['/bin/sh','-c','sleep 30']).pid)); time.sleep(30)"
            with self.assertRaisesRegex(release.ReleaseError, "timed out"):
                release.run([sys.executable, "-c", code, str(child_file)], timeout=1)
            child = int(child_file.read_text())
            for _ in range(20):
                try:
                    os.kill(child, 0)
                except ProcessLookupError:
                    break
                time.sleep(0.05)
            else:
                self.fail("timed-out hook left its child running")

    def test_nonancestor_range_requires_explicit_baseline_setting(self):
        cfg = config(Path("/tmp/release.json"))
        with mock.patch.object(release, "run", return_value=json.dumps({"status": "diverged"})), self.assertRaisesRegex(release.ReleaseError, "not an ancestor"):
            release.range_sources(cfg, OLD, SHA)
        cfg["allow_nonancestor_baseline"] = True
        with mock.patch.object(release, "run", return_value=json.dumps({"status": "diverged"})):
            self.assertEqual(release.range_sources(cfg, OLD, SHA), ([], "nonancestor_baseline"))

    def test_first_unhealthy_probe_persists_range_before_deploy(self):
        with tempfile.TemporaryDirectory() as directory:
            journal = Path(directory) / "release.json"
            cfg = config(journal)
            observed = {"sha": OLD, "healthy": False}
            verified = {"sha": SHA, "healthy": True}
            def command(argv, *unused):
                if argv[0] == "/bin/true":
                    receipt = release.load(journal)["releases"]["633"]
                    self.assertEqual(receipt["delivery_sources"], [{"pr": 633, "merge_sha": SHA, "issue": 602, "reference": "refs"}])
                    self.assertEqual(release.load(journal)["live_tip"]["sha"], OLD)
                    return ""
                if argv[:2] == ["gh", "api"]:
                    return json.dumps({"object": {"sha": SHA}})
                self.fail(str(argv))
            with mock.patch.object(release, "gh_snapshot", return_value=snapshot()), \
                 mock.patch.object(release, "review_gate"), \
                 mock.patch.object(release, "probe", return_value=observed), \
                 mock.patch.object(release, "range_sources", return_value=([{"pr": 633, "merge_sha": SHA, "issue": 602, "reference": "refs"}], "range")), \
                 mock.patch.object(release, "verify", return_value=verified), \
                 mock.patch.object(release, "run", side_effect=command):
                result = release.once(cfg, 633)
            self.assertEqual(result["state"], "verified")
            self.assertEqual(release.load(journal)["live_tip"]["sha"], SHA)


if __name__ == "__main__":
    unittest.main()
