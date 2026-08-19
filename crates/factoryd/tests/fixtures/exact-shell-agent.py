#!/usr/bin/env python3
"""Shell-shaped free provider that reports the complete submitted prompt."""

import json
import os
import re
import subprocess
import sys
import tty

factoryctl = os.environ.get("DARK_FACTORY_FACTORYCTL", "factoryctl")
token = os.environ["DARK_FACTORY_SESSION_TOKEN_FILE"]


def hook(event, payload):
    subprocess.run(
        [factoryctl, "hook", "--token-file", token, event],
        input=json.dumps(payload),
        text=True,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
        check=False,
    )


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


tty.setraw(sys.stdin.fileno())
hook("SessionStart", {})
buffer = bytearray()
while True:
    data = os.read(sys.stdin.fileno(), 1)
    if not data:
        break
    if data == b"\r":
        os.write(sys.stdout.fileno(), b"\r")
        text = buffer.decode("utf-8", errors="replace")
        buffer.clear()
        hook("UserPromptSubmit", {"prompt": text})
        hook("PreToolUse", {"tool_name": "Bash"})
        hook("PostToolUse", {"tool_name": "Bash"})
        match = re.search(r"task:([A-Za-z0-9_-]+)", text)
        if match:
            task_done(match.group(1), text)
        hook("Stop", {})
    else:
        os.write(sys.stdout.fileno(), data)
        buffer.extend(data)
