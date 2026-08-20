#!/usr/bin/env python3
"""Free Codex-shaped resident provider for the #158 E2E regression."""

import json
import os
import re
import signal
import subprocess
import sys
import time
import tty

factoryctl = os.environ.get("DARK_FACTORY_FACTORYCTL", "factoryctl")
token = os.environ["DARK_FACTORY_SESSION_TOKEN_FILE"]
resumed = "resume" in sys.argv

# A fresh Codex process creates a fresh thread; `codex resume` keeps the
# requested one. Persist the tiny counter in the per-agent CODEX_HOME so the
# fixture models both sides of that contract across resident processes.
thread_ids = [
    "9b1deb4d-3b7d-4bad-9bdd-2b0d7b3dcb6d",
    "f31a566f-544b-46f0-bd03-9c9ec3231c90",
]
thread_counter = os.path.join(os.environ["CODEX_HOME"], "fake-thread-count")
if resumed:
    thread_id = sys.argv[-1]
else:
    try:
        with open(thread_counter, encoding="utf-8") as saved:
            generation = int(saved.read())
    except (FileNotFoundError, ValueError):
        generation = 0
    thread_id = thread_ids[min(generation, len(thread_ids) - 1)]
    with open(thread_counter, "w", encoding="utf-8") as saved:
        saved.write(str(generation + 1))


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
# composer is ready. The first delivery intentionally models the observed
# no-prompt/no-run loss; the daemon must retire this poisoned resumed thread
# and let the queued task enter one fresh conversation.
ignored_resumed_deliveries = 1 if resumed else 0
buffer = bytearray()
prompt_log = os.path.join(os.path.dirname(token), "fake-codex-prompts.jsonl")
delay_hook_path = os.path.join(
    os.path.dirname(sys.argv[0]), "delay-user-prompt-submit"
)
try:
    with open(delay_hook_path, encoding="utf-8") as delay:
        delay_hook_seconds = float(delay.read())
except (FileNotFoundError, ValueError):
    delay_hook_seconds = 0
while True:
    data = os.read(sys.stdin.fileno(), 1)
    if not data:
        break
    if data == b"\r":
        text = buffer.decode("utf-8")
        if resumed and ignored_resumed_deliveries:
            ignored_resumed_deliveries -= 1
            # Real Codex can accept this CR as a recovery/repaint boundary
            # after displaying "Conversation interrupted" while leaving an
            # empty composer. The submitted task body is gone: a following
            # bare CR cannot recover it.
            buffer.clear()
            continue
        buffer.clear()
        if prompt_log:
            with open(prompt_log, "a", encoding="utf-8") as log:
                log.write(json.dumps(text) + "\n")
        if delay_hook_seconds:
            time.sleep(delay_hook_seconds)
        hook("UserPromptSubmit", {"prompt": text})
        hook("PreToolUse", {"tool_name": "Bash"})
        hook("PostToolUse", {"tool_name": "Bash"})
        match = re.search(r"task:([A-Za-z0-9_-]+)", text)
        if match:
            task_done(match.group(1), text)
        hook("Stop", {})
    else:
        buffer.extend(data)
