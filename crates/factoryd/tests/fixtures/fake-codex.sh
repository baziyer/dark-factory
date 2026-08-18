#!/usr/bin/env python3
"""Free Codex-shaped resident provider for the #158 E2E regression."""

import json
import os
import re
import signal
import subprocess
import sys
import tty

factoryctl = os.environ.get("DARK_FACTORY_FACTORYCTL", "factoryctl")
token = os.environ["DARK_FACTORY_SESSION_TOKEN_FILE"]
thread_id = "9b1deb4d-3b7d-4bad-9bdd-2b0d7b3dcb6d"
resumed = "resume" in sys.argv


def hook(event, payload):
    completed = subprocess.run(
        [factoryctl, "hook", "--token-file", token, event],
        input=json.dumps(payload),
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.DEVNULL,
        check=False,
    )
    try:
        return json.loads(completed.stdout or "{}")
    except json.JSONDecodeError:
        return {}


def task_done(task_id, text):
    subprocess.run(
        [
            factoryctl,
            "task",
            "done",
            "--task",
            task_id,
            "--result",
            "done: " + text,
        ],
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
        check=False,
    )


def terminate(_signum, _frame):
    raise SystemExit(0)


signal.signal(signal.SIGTERM, terminate)
signal.signal(signal.SIGINT, terminate)
tty.setraw(sys.stdin.fileno())
hook("SessionStart", {"session_id": thread_id})

# A resumed provider can expose a raw terminal before its prior thread's
# composer is ready. The first two bounded deliveries intentionally model the
# observed no-prompt/no-run loss; the daemon's one outer retry must recover.
ignored_resumed_deliveries = 2 if resumed else 0
buffer = ""
while True:
    data = os.read(sys.stdin.fileno(), 1)
    if not data:
        break
    character = data.decode("utf-8", errors="replace")
    if character == "\r":
        text = buffer
        buffer = ""
        if resumed and ignored_resumed_deliveries:
            ignored_resumed_deliveries -= 1
            continue
        hook("UserPromptSubmit", {"prompt": text})
        hook("PreToolUse", {"tool_name": "Bash"})
        hook("PostToolUse", {"tool_name": "Bash"})
        match = re.search(r"task:([A-Za-z0-9_-]+)", text)
        if match:
            task_done(match.group(1), text)
        hook("Stop", {})
    else:
        buffer += character
