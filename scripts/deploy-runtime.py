#!/usr/bin/env python3
"""Drain, reinstall an exact runtime revision, verify, and restore dispatch."""
import json
import os
from pathlib import Path
import re
import sqlite3
import subprocess
import sys
import tempfile
import time


def state(home):
    with sqlite3.connect((home / 'factory.sqlite3').as_uri() + '?mode=ro', uri=True) as connection:
        enabled, revision = connection.execute('SELECT dispatch_enabled, revision FROM factory WHERE singleton=1').fetchone()
        active = connection.execute("SELECT count(*) FROM runs WHERE phase <> 'terminal'").fetchone()[0]
    return enabled, revision, active


def failure_receipt(sha, reachable):
    directory = Path.home() / '.dark-factory-backups'
    directory.mkdir(mode=0o700, exist_ok=True)
    target = directory / ('deploy-' + sha + '.json')
    fd, temporary = tempfile.mkstemp(prefix='.deploy-', dir=directory)
    try:
        with os.fdopen(fd, 'w', encoding='utf-8') as stream:
            json.dump({'sha': sha, 'healthy': False, 'dispatch_enabled': False,
                       'service_reachable': reachable, 'error': 'deployment_failed'}, stream, sort_keys=True)
            stream.write('\n')
        os.replace(temporary, target)
        os.chmod(target, 0o600)
    finally:
        try:
            os.unlink(temporary)
        except FileNotFoundError:
            pass


def deploy(sha):
    if re.fullmatch('[0-9a-f]{40}', sha) is None:
        raise ValueError('expected a full merge SHA')
    scripts = Path(__file__).resolve().parent
    home = Path.home() / '.dark-factory'
    control = Path(str(home) + '.service') / 'bin' / 'current' / 'factoryctl'
    env = dict(os.environ, DARK_FACTORY_SOCKET=str(home / 'runtimes' / 'factory.sock'), DARK_FACTORY_OPERATOR_TOKEN_FILE=str(home / 'operator.token'))
    enabled, original_revision, _ = state(home)
    subprocess.run([str(control), 'dispatch', 'off', '--revision', str(original_revision)], env=env, check=True, timeout=15)
    _, paused_revision, _ = state(home)
    if paused_revision != original_revision + int(bool(enabled)):
        raise ValueError('operator changed dispatch during deployment pause')
    try:
        deadline = time.monotonic() + 300
        while state(home)[2]:
            if time.monotonic() >= deadline:
                raise ValueError('runs did not drain')
            time.sleep(1)
        subprocess.run(['/bin/sh', str(scripts / 'reinstall-service.sh'), sha], env=env, check=True, timeout=600,
                       capture_output=True, text=True)
        observed = json.loads(subprocess.run([sys.executable, str(scripts / 'verify-live-runtime.py'), sha], env=env, capture_output=True, text=True, check=True, timeout=60).stdout)
        if observed.get('sha') != sha or observed.get('healthy') is not True:
            raise ValueError('runtime verification failed')
    except (OSError, ValueError, json.JSONDecodeError, subprocess.SubprocessError) as exc:
        try:
            reachable = state(home)[2] >= 0
        except (OSError, sqlite3.Error):
            reachable = False
        failure_receipt(sha, reachable)
        raise ValueError('deployment failed; dispatch remains off; service_reachable=' + str(reachable).lower()) from exc
    current_enabled, revision, _ = state(home)
    # A subsequent explicit operator dispatch change wins over our restoration.
    if enabled and not current_enabled and revision == paused_revision:
        subprocess.run([str(control), 'dispatch', 'on', '--revision', str(paused_revision)], env=env, check=True, timeout=15)
    print(json.dumps({'sha': sha, 'healthy': True, 'dispatch_enabled': bool(state(home)[0])}))


if __name__ == '__main__':
    try:
        if len(sys.argv) != 2:
            raise ValueError('usage: deploy-runtime.py MERGE_SHA')
        deploy(sys.argv[1])
    except (OSError, ValueError, sqlite3.Error, subprocess.SubprocessError) as error:
        print('deploy-runtime: ' + str(error), file=sys.stderr)
        raise SystemExit(1)
