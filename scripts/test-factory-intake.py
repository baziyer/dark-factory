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


def issue(state="OPEN", updated="2026-09-11T10:00:00Z", labels=None, body="Do the work"):
    return {"number": 7, "title": "Improve queue", "body": body,
            "author": {"login": "maintainer"}, "labels": [{"name": name} for name in labels or ["factory:ready"]],
            "state": state, "updatedAt": updated, "url": "https://github.com/o/r/issues/7",
            "isPullRequest": False}


class IntakeTest(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        root = Path(self.temp.name)
        self.config = {"repository": "o/r", "project_id": "1" * 32, "worker_agent_id": "2" * 32,
                       "overseer_agent_id": "3" * 32, "label": "factory:ready", "allowed_authors": ["maintainer"],
                       "factory_home": str(root), "journal": str(root / "journal.json"), "max_issues": 25,
                       "poll_seconds": 5}
        self.real_command = INTAKE.command
        self.real_state = INTAKE.task_state
        self.calls = []
        self.states = {}
        INTAKE.task_state = lambda config, task_id: self.states.get(task_id)

    def tearDown(self):
        INTAKE.command = self.real_command
        INTAKE.task_state = self.real_state
        self.temp.cleanup()

    def command(self, argv):
        self.calls.append(argv)
        if argv[0] == "gh" and argv[2] == "list":
            return json.dumps([issue()])
        if argv[0] == "gh":
            return json.dumps(issue())
        task_id = argv[argv.index("--task-id") + 1]
        incarnation = argv[argv.index("--incarnation-id") + 1]
        self.states[task_id] = {"id": task_id, "status": "queued", "revision": 1, "result": "", "blocked_reason": ""}
        return json.dumps({"id": task_id, "incarnation_id": incarnation})

    def test_first_enqueue_is_replayed_without_duplicate(self):
        INTAKE.command = self.command
        self.assertEqual(["queued o/r#7"], INTAKE.run_once(self.config))
        self.calls.clear()
        self.assertEqual([], INTAKE.run_once(self.config))
        self.assertEqual(2, sum(call[0] == "gh" for call in self.calls))
        self.assertEqual(0, sum(call[0] == "factoryctl" for call in self.calls))

    def test_lost_enqueue_response_leaves_planned_payload(self):
        def lost(argv):
            if argv[0] == "factoryctl":
                raise INTAKE.IntakeError("transport lost")
            return self.command(argv)
        INTAKE.command = lost
        with self.assertRaises(INTAKE.IntakeError):
            INTAKE.run_once(self.config)
        journal = json.loads(Path(self.config["journal"]).read_text())
        record = journal["issues"]["o/r#7"]
        self.assertEqual("planned", record["status"])
        self.assertEqual(record["task_id"], INTAKE.sha_id("o/r#7", record["updated_at"]))

    def test_closed_issue_creates_overseer_reconcile_task(self):
        INTAKE.command = self.command
        journal = {"version": 1, "updated_at": 0, "issues": {"o/r#7": {
            "number": 7, "task_id": INTAKE.sha_id("o/r#7", "old"), "source_state": "open",
            "updated_at": "old", "status": "running"}}}
        INTAKE.atomic_json(Path(self.config["journal"]), journal)
        old = INTAKE.command
        def closed(argv):
            if argv[0] == "gh" and argv[2] == "list":
                return json.dumps([issue(state="CLOSED")])
            if argv[0] == "gh":
                return json.dumps(issue(state="CLOSED"))
            return old(argv)
        INTAKE.command = closed
        self.assertEqual(["reconcile withdrawn o/r#7"], INTAKE.run_once(self.config))
        self.assertTrue(any(call[0] == "factoryctl" and self.config["overseer_agent_id"] in call for call in self.calls))

    def test_edit_while_worker_is_active_waits_for_overseer_reconcile(self):
        INTAKE.command = self.command
        prior = INTAKE.sha_id("o/r#7", "old")
        self.states[prior] = {"id": prior, "status": "running", "revision": 2, "result": "", "blocked_reason": ""}
        INTAKE.atomic_json(Path(self.config["journal"]), {"version": 1, "updated_at": 0, "issues": {"o/r#7": {
            "number": 7, "task_id": prior, "source_state": "open", "updated_at": "old", "status": "running"}}})
        old = INTAKE.command
        INTAKE.command = lambda argv: json.dumps([issue(updated="2026-09-11T11:00:00Z")]) if argv[0] == "gh" and argv[2] == "list" else (json.dumps(issue(updated="2026-09-11T11:00:00Z")) if argv[0] == "gh" else old(argv))
        self.assertEqual(["reconcile active o/r#7"], INTAKE.run_once(self.config))
        self.assertTrue(any(call[0] == "factoryctl" and self.config["overseer_agent_id"] in call for call in self.calls))
        self.assertFalse(any(call[0] == "factoryctl" and self.config["worker_agent_id"] in call for call in self.calls))

    def test_hostile_body_is_bounded(self):
        with self.assertRaises(INTAKE.IntakeError):
            INTAKE.prompt(self.config, INTAKE.issue_from_json(issue(body="x" * 9000), self.config), "worker")

    def test_gh_failure_is_visible(self):
        INTAKE.command = lambda argv: (_ for _ in ()).throw(INTAKE.IntakeError("rate limited"))
        with self.assertRaisesRegex(INTAKE.IntakeError, "rate limited"):
            INTAKE.run_once(self.config)
        self.assertFalse(Path(self.config["journal"]).exists())


if __name__ == "__main__":
    unittest.main()
