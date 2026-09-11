#!/usr/bin/env python3
"""Offline tests for source-linked deployment follow-ups."""
import importlib.util
import types
import unittest
from pathlib import Path
from unittest import mock


MODULE = Path(__file__).with_name("factory-delivery.py")
SPEC = importlib.util.spec_from_file_location("factory_delivery", MODULE)
delivery = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(delivery)
SHA = "a" * 40


class DeliveryFixtures(unittest.TestCase):
    def setUp(self):
        self.enqueued = []
        self.intake = types.SimpleNamespace(
            validate_config=lambda value: value,
            sha_id=lambda *parts: ":".join(parts),
            task_state=lambda config, operation: None,
            enqueue=lambda config, operation: self.enqueued.append(operation),
        )
        self.config = {"repository": "example/factory", "project_id": "project", "priority_default": 0}
        self.receipt = {"state": "verified", "sha": SHA, "pr": 11,
                        "verification": {"sha": SHA, "healthy": True}, "delivery_mode": "range",
                        "delivery_sources": [{"pr": 10, "merge_sha": SHA, "issue": 602, "reference": "refs"},
                                             {"pr": 11, "merge_sha": SHA, "issue": 512, "reference": "closes"}]}

    def test_enqueues_one_idempotent_follow_up_per_source_mapping(self):
        with mock.patch.object(delivery, "load_intake", return_value=self.intake):
            task_ids = delivery.deliver(self.config, {"repository": "example/factory"}, self.receipt)
        self.assertEqual(len(task_ids), 2)
        self.assertEqual(len(self.enqueued), 2)
        self.assertIn("issue #602", self.enqueued[0]["body"])
        self.assertIn("issue #512", self.enqueued[1]["body"])

    def test_legacy_receipt_without_mapping_is_rejected(self):
        del self.receipt["delivery_sources"]
        with mock.patch.object(delivery, "load_intake", return_value=self.intake), self.assertRaisesRegex(ValueError, "source mapping"):
            delivery.deliver(self.config, {"repository": "example/factory"}, self.receipt)

    def test_baseline_has_no_delivery_sweep(self):
        self.receipt.update({"delivery_mode": "baseline_current", "delivery_sources": []})
        with mock.patch.object(delivery, "load_intake", return_value=self.intake):
            self.assertEqual(delivery.deliver(self.config, {"repository": "example/factory"}, self.receipt), [])
        self.assertEqual(self.enqueued, [])


if __name__ == "__main__":
    unittest.main()
