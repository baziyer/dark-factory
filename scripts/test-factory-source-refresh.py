#!/usr/bin/env python3
import importlib.util
import json
import os
import subprocess
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch


SPEC = importlib.util.spec_from_file_location('source_refresh', Path(__file__).with_name('factory-source-refresh.py'))
refresh = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(refresh)


class SourceRefreshTest(unittest.TestCase):
    def setUp(self):
        self.home = tempfile.TemporaryDirectory()
        self.config = {'repository': 'o/r', 'project_id': '1' * 32, 'overseer_agent_id': '3' * 32,
                       'label': 'ready', 'allowed_authors': ['m'], 'factory_home': self.home.name,
                       'journal': '/private/tmp/journal'}
        self.fetch = patch.object(refresh, 'fetch_target', return_value='a' * 40)
        self.fetcher = self.fetch.start()

    def tearDown(self):
        self.fetch.stop()
        self.home.cleanup()

    def test_concurrent_refresh_is_refused_before_source_access(self):
        with Path(str(Path(self.home.name).resolve()) + '.source-refresh.lock').open('a+') as lock:
            refresh.fcntl.flock(lock, refresh.fcntl.LOCK_EX | refresh.fcntl.LOCK_NB)
            with patch.object(refresh, 'root') as root:
                with self.assertRaisesRegex(refresh.RefreshError, 'source_refresh_busy'):
                    refresh.refresh(dict(self.config, journal='/private/tmp/other-journal'))
                root.assert_not_called()

    def test_pause_drain_refresh_restore_uses_exact_revisions(self):
        calls = []
        merged = [False]
        def command(argv, _env, _timeout=30, **_kwargs):
            calls.append(argv)
            if argv[1:3] == ['dispatch', 'off']:
                return json.dumps({'revision': 8})
            if argv[-1] == 'HEAD':
                return ('a' * 40 if not merged[0] else 'b' * 40) + '\n'
            if 'merge' in argv:
                merged[0] = True
            return ''
        self.fetcher.return_value = 'b' * 40
        with patch.object(refresh, 'root', return_value=Path('/project')), patch.object(refresh, 'validate_source', return_value='main'), \
             patch.object(refresh, 'state', side_effect=[(True, 7, 0), (False, 8, 0), (False, 8, 0), (False, 8, 0), (True, 9, 0)]), patch.object(refresh, 'command', side_effect=command):
            self.assertEqual({'refreshed': True}, refresh.refresh(self.config))
        self.assertIn(['factoryctl', 'dispatch', 'off', '--revision', '7'], calls)
        self.assertIn(['factoryctl', 'dispatch', 'on', '--revision', '8'], calls)
        self.assertTrue(any('merge' in call for call in calls))

    def test_ambiguous_pause_never_restores(self):
        with patch.object(refresh, 'root', return_value=Path('/project')), patch.object(refresh, 'validate_source', return_value='main'), \
             patch.object(refresh, 'state', return_value=(True, 7, 0)), patch.object(refresh, 'command', side_effect=refresh.RefreshError('command_failed:factoryctl')) as command:
            with self.assertRaisesRegex(refresh.RefreshError, 'command_failed'):
                refresh.refresh(self.config)
        self.assertEqual(1, command.call_count)

    def test_initially_disabled_never_enables_dispatch(self):
        calls = []
        self.fetcher.return_value = 'b' * 40
        merged = [False]
        def command(argv, _env, _timeout=30, **_kwargs):
            calls.append(argv)
            if argv[-1] == 'HEAD':
                return ('a' * 40 if not merged[0] else 'b' * 40) + '\n'
            if 'merge' in argv:
                merged[0] = True
            return json.dumps({'revision': 7}) if argv[0] == 'factoryctl' else ''
        with patch.object(refresh, 'root', return_value=Path('/project')), patch.object(refresh, 'validate_source', return_value='main'), \
             patch.object(refresh, 'state', side_effect=[(False, 7, 0), (False, 7, 0), (False, 7, 0)]), patch.object(refresh, 'command', side_effect=command):
            refresh.refresh(self.config)
        self.assertFalse(any(call[2] == 'on' for call in calls if call[0] == 'factoryctl'))

    def test_operator_change_before_merge_aborts_without_merge(self):
        calls = []
        def command(argv, _env, _timeout=30, **_kwargs):
            calls.append(argv)
            return json.dumps({'revision': 8}) if argv[0] == 'factoryctl' else ''
        with patch.object(refresh, 'root', return_value=Path('/project')), patch.object(refresh, 'validate_source', return_value='main'), \
             patch.object(refresh, 'state', side_effect=[(True, 7, 0), (False, 8, 0), (True, 9, 0), (True, 9, 0)]), patch.object(refresh, 'command', side_effect=command):
            with self.assertRaisesRegex(refresh.RefreshError, 'operator_changed'):
                refresh.refresh(self.config)
        self.assertFalse(any('merge' in call for call in calls))

    def test_failed_prefetch_never_pauses_dispatch(self):
        calls = []
        def command(argv, _env, **_kwargs):
            calls.append(argv)
            if argv[0] == 'factoryctl':
                return json.dumps({'revision': 8 if argv[2] == 'off' else 9})
            return ''
        self.fetcher.side_effect = refresh.RefreshError('command_failed:git')
        with patch.object(refresh, 'root', return_value=Path('/project')), patch.object(refresh, 'validate_source', return_value='main'), \
             patch.object(refresh, 'state', return_value=(True, 7, 0)), patch.object(refresh, 'command', side_effect=command):
            with self.assertRaisesRegex(refresh.RefreshError, 'command_failed:git'):
                refresh.refresh(self.config)
        self.assertEqual([], calls)

    def test_real_prefetch_and_guarded_fast_forward_preserve_untracked_and_disable_hooks(self):
        self.fetch.stop()
        with tempfile.TemporaryDirectory() as directory:
            origin, writer, worker = Path(directory) / 'origin.git', Path(directory) / 'writer', Path(directory) / 'worker'
            subprocess.run(['git', 'init', '--bare', str(origin)], check=True, capture_output=True)
            subprocess.run(['git', 'clone', str(origin), str(writer)], check=True, capture_output=True)
            for argv in (['config', 'user.name', 't'], ['config', 'user.email', 't@example.invalid']):
                subprocess.run(['git', '-C', str(writer), *argv], check=True, capture_output=True)
            (writer / 'base').write_text('one')
            subprocess.run(['git', '-C', str(writer), 'add', 'base'], check=True, capture_output=True)
            subprocess.run(['git', '-C', str(writer), 'commit', '-m', 'one'], check=True, capture_output=True)
            subprocess.run(['git', '-C', str(writer), 'push', 'origin', 'HEAD:main'], check=True, capture_output=True)
            subprocess.run(['git', '-C', str(origin), 'symbolic-ref', 'HEAD', 'refs/heads/main'], check=True, capture_output=True)
            subprocess.run(['git', 'clone', str(origin), str(worker)], check=True, capture_output=True)
            before = subprocess.run(['git', '-C', str(worker), 'rev-parse', 'HEAD'], check=True, capture_output=True, text=True).stdout.strip()
            (worker / 'untracked').write_text('keep')
            (writer / 'base').write_text('two')
            subprocess.run(['git', '-C', str(writer), 'commit', '-am', 'two'], check=True, capture_output=True)
            subprocess.run(['git', '-C', str(writer), 'push', 'origin', 'HEAD:main'], check=True, capture_output=True)
            target = refresh.fetch_target(worker, 'main', os.environ.copy())
            self.assertNotEqual(before, target)
            self.assertEqual(before, subprocess.run(['git', '-C', str(worker), 'rev-parse', 'HEAD'], check=True, capture_output=True, text=True).stdout.strip())
            self.assertEqual('keep', (worker / 'untracked').read_text())
            hook = worker / '.git/hooks/post-merge'
            hook.write_text('#!/bin/sh\ntouch hook-ran\n')
            hook.chmod(0o755)
            factory = [True, 7, 0]
            actual_command = refresh.command
            def guarded_command(argv, env, timeout=30):
                if argv[0] == 'factoryctl':
                    self.assertEqual(str(factory[1]), argv[-1])
                    factory[0] = argv[2] == 'on'
                    factory[1] += 1
                    return json.dumps({'revision': factory[1]})
                if 'merge' in argv:
                    self.assertEqual([False, 8, 0], factory)
                return actual_command(argv, env, timeout)
            with patch.object(refresh, 'root', return_value=worker), patch.object(refresh, 'validate_source', return_value='main'), \
                 patch.object(refresh, 'state', side_effect=lambda home: tuple(factory)), patch.object(refresh, 'command', side_effect=guarded_command):
                self.assertEqual({'refreshed': True}, refresh.refresh(self.config))
                self.assertEqual({'refreshed': False}, refresh.refresh(self.config))
            self.assertEqual([True, 9, 0], factory)
            self.assertEqual('two', (worker / 'base').read_text())
            self.assertEqual('keep', (worker / 'untracked').read_text())
            self.assertFalse((worker / 'hook-ran').exists())



if __name__ == '__main__':
    unittest.main()
