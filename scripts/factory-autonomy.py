#!/usr/bin/env python3
"""Run the operator-configured intake and local attention checks under launchd."""
import argparse
import fcntl
import importlib.util
import hashlib
import json
import os
from pathlib import Path
import plistlib
import subprocess
import sys
import tempfile
import time


def validate_controller_config(config):
    home, journal = config.get('factory_home'), config.get('journal')
    if not isinstance(home, str) or not isinstance(journal, str) or not Path(home).is_absolute() or not Path(journal).is_absolute():
        raise ValueError('factory_home and journal must be absolute paths')
    resolved_home, resolved_journal = Path(home).resolve(), Path(journal).resolve()
    if resolved_journal == resolved_home or resolved_home in resolved_journal.parents:
        raise ValueError('journal must be outside factory_home')
    releases = config.get('release_configs', [])
    if not isinstance(releases, list) or any(not isinstance(item, str) or not Path(item).is_absolute() for item in releases):
        raise ValueError('release_configs must be absolute config paths')
    for path in releases:
        release_journal = json.loads(Path(path).read_text()).get('journal')
        if not isinstance(release_journal, str) or not Path(release_journal).is_absolute():
            raise ValueError('release journal must be absolute')
        resolved_release = Path(release_journal).resolve()
        if resolved_release == resolved_home or resolved_home in resolved_release.parents:
            raise ValueError('release journal must be outside factory_home')


def tick(config_path, config):
    scripts = Path(__file__).resolve().parent
    # Notifications remain useful when GitHub or intake is unavailable.
    calls = [[sys.executable, str(scripts / 'factory-source-refresh.py'), str(config_path), '--once']]
    if 'review_mirror_root' in config:
        if not isinstance(config['review_mirror_root'], str) or not Path(config['review_mirror_root']).is_absolute():
            raise ValueError('review_mirror_root must be an absolute path')
        calls.append([sys.executable, str(scripts / 'factory-review-intake.py'), str(config_path), '--once'])
    calls.append([sys.executable, str(scripts / 'factory-intake.py'), str(config_path), '--once'])
    calls.append([sys.executable, str(scripts / 'factory-notify.py'), '--once', '--home', config['factory_home'], '--receipt', config['journal'] + '.notifications'])
    releases = config.get('release_configs', [])
    for release_config in releases:
        calls.append([sys.executable, str(scripts / 'factory-release.py'), release_config, '--latest', '--once'])
    results = []
    refreshed = None
    for argv in calls:
        if Path(argv[1]).name == 'factory-intake.py' and refreshed is False:
            results.append({'component': 'factory-intake', 'ok': False, 'error': 'source_refresh_failed'})
            continue
        try:
            completed = subprocess.run(argv, capture_output=True, text=True, timeout=1300)
            result = {'component': Path(argv[1]).stem, 'ok': completed.returncode == 0}
            if completed.returncode:
                result['error'] = 'exit_' + str(completed.returncode)
            results.append(result)
            if Path(argv[1]).name == 'factory-source-refresh.py':
                refreshed = result['ok']
            if completed.returncode == 0 and Path(argv[1]).name == 'factory-release.py':
                receipt = json.loads(completed.stdout)
                if receipt.get('state') == 'verified':
                    spec = importlib.util.spec_from_file_location('factory_delivery', scripts / 'factory-delivery.py')
                    delivery = importlib.util.module_from_spec(spec)
                    spec.loader.exec_module(delivery)
                    release_config = json.loads(Path(argv[2]).read_text())
                    delivery.deliver(config, release_config, receipt)

        except subprocess.TimeoutExpired:
            results.append({'component': Path(argv[1]).stem, 'ok': False, 'error': 'timeout'})
            if Path(argv[1]).name == 'factory-source-refresh.py':
                refreshed = False
        except Exception:
            results.append({'component': Path(argv[1]).stem, 'ok': False, 'error': 'exception'})
            if Path(argv[1]).name == 'factory-source-refresh.py':
                refreshed = False
    return results


def write_health(config, results):
    path = Path(config['journal'] + '.autonomy.json')
    path.parent.mkdir(parents=True, exist_ok=True)
    fd, temporary = tempfile.mkstemp(prefix='.' + path.name + '.', dir=path.parent)
    try:
        with os.fdopen(fd, 'w', encoding='utf-8') as stream:
            json.dump({'at': int(time.time()), 'components': results}, stream, sort_keys=True)
            stream.write('\n')
            stream.flush()
            os.fsync(stream.fileno())
        os.replace(temporary, path)
        os.chmod(path, 0o600)
    finally:
        try:
            os.unlink(temporary)
        except FileNotFoundError:
            pass


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('config', type=Path)
    parser.add_argument('--once', action='store_true')
    parser.add_argument('--plist', action='store_true', help='Print a launchd plist; does not install or start it.')
    args = parser.parse_args()
    config_path = args.config.resolve(strict=True)
    config = json.loads(config_path.read_text())
    validate_controller_config(config)
    interval = config.get('poll_seconds', 120)
    if type(interval) is not int or not 5 <= interval <= 86400:
        raise ValueError('poll_seconds must be 5..86400')
    # Use launchd StartInterval rather than keeping a second polling daemon.
    if args.plist:
        log = str(Path(config['journal']).with_suffix('.service.log'))
        plist = {'Label': 'build.darkfactory.autonomy.' + hashlib.sha256(str(config_path).encode()).hexdigest()[:12], 'ProgramArguments': [sys.executable, str(Path(__file__).resolve()), str(config_path), '--once'],
                 'StartInterval': interval, 'RunAtLoad': True, 'ProcessType': 'Background',
                 'StandardOutPath': log, 'StandardErrorPath': log,
                 'EnvironmentVariables': {'PATH': os.environ.get('PATH', '/usr/bin:/bin:/usr/sbin:/sbin')}}
        sys.stdout.buffer.write(plistlib.dumps(plist))
        return 0
    # ponytail: one host controller at a time; split maintenance leases only
    # when independent factories need concurrent host deployment hooks.
    descriptor = os.open(Path(str(Path(config['factory_home']).resolve()) + '.autonomy.lock'), os.O_CREAT | os.O_RDWR, 0o600)
    with os.fdopen(descriptor, 'a+') as lock:
        try:
            fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError as error:
            raise ValueError('another controller owns this factory') from error
        results = tick(config_path, config)
        write_health(config, results)
    print(json.dumps({'at': int(time.time()), 'components': results}), flush=True)
    return 0 if all(result['ok'] for result in results) else 1


if __name__ == '__main__':
    try:
        raise SystemExit(main())
    except (OSError, ValueError, KeyError) as error:
        print('factory-autonomy: ' + str(error), file=sys.stderr)
        raise SystemExit(1)
