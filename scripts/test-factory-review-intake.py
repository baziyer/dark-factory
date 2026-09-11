#!/usr/bin/env python3
import importlib.util
import json
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch


SPEC = importlib.util.spec_from_file_location('review_intake', Path(__file__).with_name('factory-review-intake.py'))
review = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(review)
SHA = 'a' * 40


class ReviewIntakeTest(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        root = Path(self.temp.name)
        self.config = {'repository': 'o/r', 'project_id': '1' * 32, 'overseer_agent_id': '3' * 32,
                       'label': 'factory:ready', 'allowed_authors': ['maintainer'], 'factory_home': str(root),
                       'journal': str(root / 'intake.json'), 'review_mirror_root': str(root / 'mirrors')}
        review.intake.atomic_json(Path(self.config['journal']), {'version': 2, 'updated_at': 0, 'config_fingerprint': review.intake.config_fingerprint(self.config),
            'issues': {'o/r#7': {'number': 7, 'managed': True}}})
        self.operation = {'pr': 9, 'head': SHA, 'base': 'b' * 40, 'source_marker': 'FACTORY_SOURCE o/r#7',
                          'task_id': 'c' * 32, 'incarnation_id': 'd' * 32, 'priority': 0,
                          'title': 'resume', 'body': 'mirror'}

    def tearDown(self):
        self.temp.cleanup()

    def test_only_app_footer_linked_pr_is_woken_once_after_lost_response(self):
        prs = [{'number': 9, 'headRefOid': SHA, 'body': 'text\nRefs #7\n'}]
        with patch.object(review, 'mirror', return_value=Path('/mirror')), patch.object(review, 'list_prs', return_value=prs), \
             patch.object(review, 'ready', return_value=self.operation), patch.object(review.intake, 'task_state', side_effect=[None, {'status': 'queued'}]), \
             patch.object(review.intake, 'enqueue') as enqueue, patch.object(review, 'verify_existing'):
            self.assertEqual(['woke PR #9'], review.run_once(self.config))
            self.assertEqual([], review.run_once(self.config))
        enqueue.assert_called_once_with(self.config, self.operation)
        receipt = json.loads(Path(self.config['journal'] + '.reviews.json').read_text())
        self.assertEqual(self.operation, receipt['pulls']['9:' + SHA])

    def test_unlinked_pr_is_not_woken(self):
        with patch.object(review, 'mirror', return_value=Path('/mirror')), patch.object(review, 'list_prs', return_value=[{'number': 9, 'headRefOid': SHA, 'body': 'untrusted Refs #8'}]), \
             patch.object(review, 'ready') as ready:
            self.assertEqual([], review.run_once(self.config))
        ready.assert_not_called()

    def test_mirror_must_match_app_reported_head(self):
        def command(argv, **_kwargs):
            if argv[-1].startswith('refs/pull'):
                return 'b' * 40 + '\n'
            return SHA + '\n'
        with patch.object(review.intake, 'command', side_effect=command):
            with self.assertRaisesRegex(review.ReviewError, 'exact head'):
                review.ready(self.config, Path('/mirror'), {'number': 9, 'headRefOid': SHA}, 7)

    def test_existing_head_keeps_its_recorded_base_when_main_advances(self):
        receipt = {'version': 2, 'config_fingerprint': review.config_fingerprint(self.config), 'pulls': {'9:' + SHA: self.operation}}
        Path(self.config['journal'] + '.reviews.json').write_text(json.dumps(receipt))
        with patch.object(review, 'mirror', return_value=Path('/mirror')), patch.object(review, 'list_prs', return_value=[{'number': 9, 'headRefOid': SHA, 'body': 'Refs #7'}]), \
             patch.object(review, 'ready', side_effect=AssertionError('must reuse persisted base')), patch.object(review, 'verify_existing') as verify, \
             patch.object(review.intake, 'task_state', return_value={'status': 'queued'}):
            self.assertEqual([], review.run_once(self.config))
        verify.assert_called_once_with(Path('/mirror'), {'number': 9, 'headRefOid': SHA, 'body': 'Refs #7'}, self.operation)

    def test_receipt_rejects_mirror_config_change(self):
        Path(self.config['journal'] + '.reviews.json').write_text(json.dumps({'version': 2, 'config_fingerprint': review.config_fingerprint(self.config), 'pulls': {}}))
        changed = dict(self.config, review_mirror_root='/other')
        with patch.object(review, 'mirror', return_value=Path('/mirror')):
            with self.assertRaisesRegex(review.ReviewError, 'receipt is invalid'):
                review.run_once(changed)

    def test_nonbare_mirror_is_refused(self):
        path = Path(self.config['review_mirror_root']) / 'o' / 'r'
        path.mkdir(parents=True)
        (path / 'HEAD').write_text('ref: refs/heads/main\n')
        with patch.object(review.intake, 'command', return_value='false\n'):
            with self.assertRaisesRegex(review.ReviewError, 'must be bare'):
                review.mirror(self.config)


if __name__ == '__main__':
    unittest.main()
