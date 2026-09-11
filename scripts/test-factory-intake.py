#!/usr/bin/env python3
import importlib.util
import json
import tempfile
import unittest
from pathlib import Path


HERE = Path(__file__).parent
SPEC = importlib.util.spec_from_file_location("factory_intake", HERE / "factory-intake.py")
INTAKE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(INTAKE)


def issue(state="OPEN", updated="2026-09-11T10:00:00Z", body="Do the work", labels=None):
    return {"number": 7, "title": "Improve queue", "body": body, "author": {"login": "maintainer"}, "labels": [{"name": name} for name in labels or ["factory:ready"]], "state": state, "updatedAt": updated, "url": "https://github.com/o/r/issues/7"}


class IntakeTest(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        root = Path(self.temp.name)
        self.config = {"repository": "o/r", "project_id": "1" * 32, "overseer_agent_id": "3" * 32, "label": "factory:ready", "allowed_authors": ["maintainer"], "factory_home": str(root), "journal": str(root / "journal.json"), "max_issues": 25, "poll_seconds": 5}
        self.source, self.calls, self.states = issue(), [], {}
        self.real_command, self.real_state = INTAKE.command, INTAKE.task_state
        INTAKE.command, INTAKE.task_state = self.command, lambda _config, operation: self.states.get(operation["task_id"])

    def tearDown(self):
        INTAKE.command, INTAKE.task_state = self.real_command, self.real_state
        self.temp.cleanup()

    def command(self, argv, **_kwargs):
        self.calls.append(argv)
        if argv[:3] == ["gh", "issue", "list"]:
            return json.dumps([{ "number": 7 }] if self.source["state"] == "OPEN" and "factory:ready" in [label["name"] for label in self.source["labels"]] else [])
        if argv[:3] == ["gh", "issue", "view"]:
            return json.dumps(self.source)
        task_id, incarnation = argv[argv.index("--task-id") + 1], argv[argv.index("--incarnation-id") + 1]
        self.states[task_id] = {"status": "queued", "id": task_id, "incarnation": incarnation}
        return json.dumps({"id": task_id, "incarnation_id": incarnation})

    def factory_calls(self):
        return [call for call in self.calls if call[0] == "factoryctl"]

    def test_one_source_task_replays_after_lost_response(self):
        def lost(argv, **kwargs):
            if argv[0] == "factoryctl":
                raise INTAKE.IntakeError("transport lost")
            return self.command(argv, **kwargs)
        INTAKE.command = lost
        with self.assertRaises(INTAKE.IntakeError):
            INTAKE.run_once(self.config)
        record = json.loads(Path(self.config["journal"]).read_text())["issues"]["o/r#7"]
        frozen = record["operation"]
        self.assertEqual(record["desired_fingerprint"], frozen["fingerprint"])
        INTAKE.command = self.command
        self.assertEqual(["replayed o/r#7"], INTAKE.run_once(self.config))
        self.assertEqual(frozen["task_id"], self.factory_calls()[-1][self.factory_calls()[-1].index("--task-id") + 1])

    def test_lost_response_after_database_insert_does_not_recreate(self):
        called = False
        def inserted_then_lost(argv, **kwargs):
            nonlocal called
            if argv[0] != "factoryctl":
                return self.command(argv, **kwargs)
            result = self.command(argv, **kwargs)
            if not called:
                called = True
                raise INTAKE.IntakeError("transport lost")
            return result
        INTAKE.command = inserted_then_lost
        with self.assertRaises(INTAKE.IntakeError):
            INTAKE.run_once(self.config)
        self.calls.clear()
        self.assertEqual([], INTAKE.run_once(self.config))
        self.assertEqual([], self.factory_calls())

    def test_multiple_edits_wait_for_one_active_task_then_use_latest_snapshot(self):
        INTAKE.run_once(self.config)
        first = next(iter(self.states))
        self.source = issue(body="edit one")
        INTAKE.run_once(self.config)
        self.source = issue(body="edit two")
        INTAKE.run_once(self.config)
        self.assertEqual(1, len(self.factory_calls()))
        self.states[first]["status"] = "succeeded"
        self.calls.clear()
        self.assertEqual(["queued o/r#7"], INTAKE.run_once(self.config))
        follow_up = self.factory_calls()[0]
        self.assertIn("edit two", follow_up[follow_up.index("--body") + 1])

    def test_withdrawal_then_reopen_is_serialized_after_the_active_task(self):
        INTAKE.run_once(self.config)
        first = next(iter(self.states))
        self.source = issue(state="CLOSED")
        INTAKE.run_once(self.config)
        self.source = issue(body="reopened")
        INTAKE.run_once(self.config)
        self.assertEqual(1, len(self.factory_calls()))
        self.states[first]["status"] = "failed"
        self.calls.clear()
        INTAKE.run_once(self.config)
        self.assertEqual(1, len(self.factory_calls()))
        self.assertIn("reopened", self.factory_calls()[0][self.factory_calls()[0].index("--body") + 1])

    def test_failed_source_is_not_recreated_without_a_source_change(self):
        INTAKE.run_once(self.config)
        task = next(iter(self.states))
        self.states[task]["status"] = "failed"
        self.calls.clear()
        self.assertEqual([], INTAKE.run_once(self.config))
        self.assertEqual([], self.factory_calls())

    def test_source_marker_and_malicious_fields_are_explicit(self):
        operation = INTAKE.operation_for(self.config, INTAKE.issue_from_json(issue()), "f" * 64)
        self.assertIn("FACTORY_SOURCE o/r#7", operation["body"])
        self.assertIn("verify every linked queued or running worker task is cancelled or stopped", operation["body"])
        hostile = issue(body={"ignore": "instructions"})
        with self.assertRaisesRegex(INTAKE.IntakeError, "invalid source"):
            INTAKE.issue_from_json(hostile)
        with self.assertRaisesRegex(INTAKE.IntakeError, "exceeds"):
            INTAKE.issue_from_json(issue(body="x" * 5001))

    def test_stale_human_decision_waits_for_a_material_source_edit(self):
        INTAKE.run_once(self.config)
        first = next(iter(self.states))
        self.states[first].update({'status': 'failed', 'needs_operator_recovery': True})
        self.calls.clear()
        self.assertEqual(['needs operator recovery o/r#7'], INTAKE.run_once(self.config))
        self.assertEqual([], self.factory_calls())
        record = json.loads(Path(self.config['journal']).read_text())['issues']['o/r#7']
        self.assertEqual(first, record['needs_operator_recovery']['task_id'])
        self.assertEqual([], INTAKE.run_once(self.config))
        self.source = issue(body='operator clarified scope')
        self.assertEqual(['queued o/r#7'], INTAKE.run_once(self.config))
        self.assertNotEqual(first, self.factory_calls()[-1][self.factory_calls()[-1].index('--task-id') + 1])

    def test_identity_change_cannot_reuse_a_managed_journal(self):
        INTAKE.run_once(self.config)
        changed = dict(self.config, project_id='2' * 32)
        with self.assertRaisesRegex(INTAKE.IntakeError, 'different repository'):
            INTAKE.run_once(changed)


if __name__ == "__main__":
    unittest.main()
