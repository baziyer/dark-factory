#!/usr/bin/env python3
"""Observe installed runtime identity and API health for a release receipt."""
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys


def observe(home):
    binary_root = Path(str(home) + '.service') / 'bin' / 'current'
    go = shutil.which('go')
    if go is None:
        raise ValueError('go tool is unavailable in PATH')
    revisions = set()
    for name in ('factoryctl', 'factoryd', 'factory-runner'):
        metadata = subprocess.run([go, 'version', '-m', str(binary_root / name)], check=True, capture_output=True, text=True, timeout=15).stdout
        revision = re.search(r'vcs\.revision=([0-9a-f]{40})', metadata)
        if revision is None or 'vcs.modified=false' not in metadata:
            raise ValueError('installed binary identity is missing or modified')
        revisions.add(revision.group(1))
    if len(revisions) != 1:
        raise ValueError('installed binaries have different revisions')
    env = dict(os.environ, DARK_FACTORY_SOCKET=str(home / 'runtimes' / 'factory.sock'), DARK_FACTORY_OPERATOR_TOKEN_FILE=str(home / 'operator.token'))
    status = json.loads(subprocess.run([str(binary_root / 'factoryctl'), 'web', 'status'], env=env, check=True, capture_output=True, text=True, timeout=15).stdout)
    return {'sha': revisions.pop(), 'healthy': status.get('ready') is True}


if __name__ == '__main__':
    try:
        if len(sys.argv) != 2 or re.fullmatch('[0-9a-f]{40}', sys.argv[1]) is None:
            raise ValueError('usage: verify-live-runtime.py EXPECTED_SHA')
        print(json.dumps(observe(Path.home() / '.dark-factory')))
    except (ValueError, OSError, subprocess.SubprocessError) as error:
        print('verify-live-runtime: ' + str(error), file=sys.stderr)
        raise SystemExit(1)
