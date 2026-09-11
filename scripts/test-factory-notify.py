#!/usr/bin/env python3
import importlib.util
import sqlite3
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch


HERE = Path(__file__).parent
SPEC = importlib.util.spec_from_file_location("factory_notify", HERE / "factory-notify.py")
NOTIFY = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(NOTIFY)


class NotifyTest(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.home = Path(self.temp.name) / "home"
        self.home.mkdir()
        self.receipt = Path(self.temp.name) / "receipt.json"
        with sqlite3.connect(self.home / "factory.sqlite3") as connection:
            connection.executescript("""
                CREATE TABLE projects (id BLOB PRIMARY KEY, run_budget_limit INTEGER NOT NULL, runs_used INTEGER NOT NULL);
                CREATE TABLE tasks (id BLOB PRIMARY KEY, project_id BLOB NOT NULL, status TEXT NOT NULL);
                CREATE TABLE human_requests (id BLOB PRIMARY KEY, status TEXT NOT NULL, created_at_ms INTEGER NOT NULL);
            """)
            connection.execute("INSERT INTO projects VALUES (?, ?, ?)", (bytes.fromhex("11" * 16), 2, 2))

    def tearDown(self):
        self.temp.cleanup()

    def request(self, suffix):
        with sqlite3.connect(self.home / "factory.sqlite3") as connection:
            connection.execute("INSERT INTO human_requests VALUES (?, 'delivery_unknown', ?)", (bytes.fromhex(suffix * 16), int(suffix, 16)))

    def queued(self):
        with sqlite3.connect(self.home / "factory.sqlite3") as connection:
            connection.execute("INSERT INTO tasks VALUES (?, ?, 'queued')", (bytes.fromhex("aa" * 16), bytes.fromhex("11" * 16)))

    def test_new_request_notifies_once_and_receipt_is_private(self):
        calls = []
        self.request("22")
        with patch.object(NOTIFY, "notify", calls.append):
            self.assertTrue(NOTIFY.run_once(self.home, self.receipt)["notified"])
            self.assertFalse(NOTIFY.run_once(self.home, self.receipt)["notified"])
        self.assertEqual(calls, ["A decision needs your answer."])
        self.assertEqual(oct(self.receipt.stat().st_mode & 0o777), "0o600")

    def test_a_new_request_notifies_after_an_old_one(self):
        calls = []
        self.request("22")
        with patch.object(NOTIFY, "notify", calls.append):
            NOTIFY.run_once(self.home, self.receipt)
            self.request("33")
            NOTIFY.run_once(self.home, self.receipt)
        self.assertEqual(calls, ["A decision needs your answer.", "A decision needs your answer."])

    def test_run_limit_notifies_once_until_the_queue_clears(self):
        calls = []
        self.queued()
        with patch.object(NOTIFY, "notify", calls.append):
            NOTIFY.run_once(self.home, self.receipt)
            NOTIFY.run_once(self.home, self.receipt)
        self.assertEqual(calls, ["A project run limit is blocking queued work."])

    def test_notification_uses_osascript_arguments_not_source_text(self):
        with patch.object(NOTIFY.subprocess, "run") as run:
            NOTIFY.notify('$(touch /tmp/no) " quoted')
        argv = run.call_args.args[0]
        self.assertEqual(argv[:2], ["/usr/bin/osascript", "-e"])
        self.assertIn("on run argv", argv[2])
        self.assertEqual(argv[4], '$(touch /tmp/no) " quoted')
        self.assertNotIn('$(touch /tmp/no) " quoted', argv[2])


if __name__ == "__main__":
    unittest.main()
