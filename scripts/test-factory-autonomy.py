#!/usr/bin/env python3
import importlib.util
import json
from pathlib import Path
import subprocess
import tempfile
import unittest
from unittest.mock import patch


def module(name):
    spec = importlib.util.spec_from_file_location(name, Path(__file__).with_name(name + '.py'))
    value = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(value)
    return value


autonomy = module('factory-autonomy')
runtime = module('verify-live-runtime')
deploy = module('deploy-runtime')


class AutonomyTest(unittest.TestCase):
    def test_notification_runs_even_if_intake_fails(self):
        config = {'factory_home': '/private/tmp/factory', 'journal': '/private/tmp/journal'}
        with patch.object(autonomy.subprocess, 'run', side_effect=[subprocess.CompletedProcess([], 0, '{}', ''), subprocess.CompletedProcess([], 1, '', 'GitHub unavailable'), subprocess.CompletedProcess([], 0, '{}', '')]) as run:
            result = autonomy.tick(Path('/private/tmp/config'), config)
        self.assertFalse(result[1]['ok'])
        self.assertTrue(result[2]['ok'])
        self.assertIn('factory-source-refresh.py', run.call_args_list[0].args[0][1])

    def test_launchd_results_do_not_retain_child_output(self):
        config = {'factory_home': '/private/tmp/factory', 'journal': '/private/tmp/journal'}
        secret = 'token=should-not-appear'
        with patch.object(autonomy.subprocess, 'run', side_effect=[subprocess.CompletedProcess([], 1, secret, secret), subprocess.CompletedProcess([], 1, secret, secret)]):
            result = autonomy.tick(Path('/private/tmp/config'), config)
        self.assertEqual([{'component': 'factory-source-refresh', 'ok': False, 'error': 'exit_1'},
                          {'component': 'factory-intake', 'ok': False, 'error': 'source_refresh_failed'},
                          {'component': 'factory-notify', 'ok': False, 'error': 'exit_1'}], result)

    def test_health_receipt_is_private_and_finite(self):
        with tempfile.TemporaryDirectory() as directory:
            config = {'journal': str(Path(directory) / 'journal.json')}
            autonomy.write_health(config, [{'component': 'factory-intake', 'ok': False, 'error': 'exit_1'}])
            receipt = Path(config['journal'] + '.autonomy.json')
            self.assertEqual(oct(receipt.stat().st_mode & 0o777), '0o600')
            self.assertEqual(json.loads(receipt.read_text())['components'][0]['error'], 'exit_1')

    def test_private_review_wakeup_is_optional(self):
        config = {'factory_home': '/private/tmp/factory', 'journal': '/private/tmp/journal', 'review_mirror_root': '/private/tmp/mirror'}
        with patch.object(autonomy.subprocess, 'run', return_value=subprocess.CompletedProcess([], 0, '{}', '')) as run:
            autonomy.tick(Path('/private/tmp/config'), config)
        self.assertTrue(any('factory-review-intake.py' in call.args[0][1] for call in run.call_args_list))

    def test_mixed_installed_binaries_cannot_prove_health(self):
        identities = ['a' * 40, 'b' * 40, 'a' * 40]
        def observe(argv, **kwargs):
            sha = identities.pop(0)
            return subprocess.CompletedProcess(argv, 0, 'vcs.revision=' + sha + '\nvcs.modified=false\n', '')
        with patch.object(runtime.shutil, 'which', return_value='/usr/local/bin/go'), patch.object(runtime.subprocess, 'run', side_effect=observe):
            with self.assertRaisesRegex(ValueError, 'different revisions'):
                runtime.observe(Path('/private/tmp/factory'))

    def test_runtime_installer_is_bounded(self):
        states = iter([(True, 4, 0), (False, 5, 0), (False, 5, 0), (False, 5, 0), (True, 6, 0)])
        def command(argv, **kwargs):
            if Path(argv[1]).name == 'verify-live-runtime.py':
                return subprocess.CompletedProcess(argv, 0, json.dumps({'sha': 'a' * 40, 'healthy': True}), '')
            return subprocess.CompletedProcess(argv, 0, '', '')
        with patch.object(deploy, 'state', side_effect=lambda _home: next(states)), patch.object(deploy.subprocess, 'run', side_effect=command) as run:
            deploy.deploy('a' * 40)
        install = next(call for call in run.call_args_list if Path(call.args[0][1]).name == 'reinstall-service.sh')
        self.assertEqual(600, install.kwargs['timeout'])
        self.assertIn([str(Path.home() / '.dark-factory.service/bin/current/factoryctl'), 'dispatch', 'off', '--revision', '4'], [call.args[0] for call in run.call_args_list])
        self.assertIn([str(Path.home() / '.dark-factory.service/bin/current/factoryctl'), 'dispatch', 'on', '--revision', '5'], [call.args[0] for call in run.call_args_list])

    def test_runtime_operator_change_after_pause_never_installs(self):
        states = iter([(True, 4, 0), (False, 6, 0)])
        with patch.object(deploy, 'state', side_effect=lambda _home: next(states)), patch.object(deploy.subprocess, 'run', return_value=subprocess.CompletedProcess([], 0, '', '')) as run:
            with self.assertRaisesRegex(ValueError, 'operator changed'):
                deploy.deploy('a' * 40)
        self.assertFalse(any(Path(call.args[0][1]).name == 'reinstall-service.sh' for call in run.call_args_list))

    def test_runtime_failure_records_a_safe_receipt(self):
        def timeout_install(argv, **_kwargs):
            if Path(argv[1]).name == 'reinstall-service.sh':
                raise subprocess.TimeoutExpired(argv, 600)
            return subprocess.CompletedProcess(argv, 0, '', '')
        with patch.object(deploy, 'state', side_effect=[(True, 4, 0), (False, 5, 0), (False, 5, 0), (False, 5, 0)]), \
             patch.object(deploy.subprocess, 'run', side_effect=timeout_install), \
             patch.object(deploy, 'failure_receipt') as receipt:
            with self.assertRaisesRegex(ValueError, 'dispatch remains off; service_reachable=true'):
                deploy.deploy('a' * 40)
        receipt.assert_called_once_with('a' * 40, True)


if __name__ == '__main__':
    unittest.main()
