#!/usr/bin/env python3
"""Deterministic shell-provider fixture with a byte-accurate PTY boundary."""

import json
import os
import re
import signal
import subprocess
import sys
import termios
import time
import tty

factoryctl = os.environ.get("DARK_FACTORY_FACTORYCTL", "factoryctl")
token = os.environ["DARK_FACTORY_SESSION_TOKEN_FILE"]
stop_delay = float(os.environ.get("SHELL_AGENT_STOP_DELAY", "0"))
prompt_log = os.path.join(os.path.dirname(token), "shell-agent-prompts.jsonl")


def hook(event, payload):
    completed = subprocess.run(
        [factoryctl, "hook", "--token-file", token, event],
        input=json.dumps(payload),
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.DEVNULL,
        check=False,
    )
    return completed.stdout.strip() or "{}"


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


def stop_handler(_signum, _frame):
    if stop_delay:
        time.sleep(stop_delay)
    raise SystemExit(0)


def process_turn(text):
    finish_after_stop = "finish-after-stop" in text
    while True:
        with open(prompt_log, "a", encoding="utf-8") as log:
            log.write(json.dumps(text) + "\n")
        hook("UserPromptSubmit", {"prompt": text})
        hook("PreToolUse", {"tool_name": "Bash"})
        sleep_match = re.search(r"sleep:([0-9]+)", text)
        if sleep_match:
            time.sleep(int(sleep_match.group(1)))
        hook("PostToolUse", {"tool_name": "Bash"})
        task_match = re.search(r"task:([A-Za-z0-9_-]+)", text)
        if task_match and not finish_after_stop:
            task_done(task_match.group(1), text)
        reply = hook("Stop", {})
        try:
            decision = json.loads(reply)
        except json.JSONDecodeError:
            decision = {}
        if decision.get("decision") == "block":
            text = decision.get("reason", "")
            finish_after_stop = False
            continue
        if task_match and finish_after_stop:
            task_done(task_match.group(1), text)
            finish_after_stop = False
            reply = hook("Stop", {})
            try:
                decision = json.loads(reply)
            except json.JSONDecodeError:
                decision = {}
            if decision.get("decision") == "block":
                text = decision.get("reason", "")
                continue
        return


signal.signal(signal.SIGTERM, stop_handler)
signal.signal(signal.SIGINT, stop_handler)

fd = sys.stdin.fileno()
old_terminal = termios.tcgetattr(fd)
tty.setraw(fd)
try:
    hook("SessionStart", {})
    buffer = bytearray()
    while True:
        data = os.read(fd, 1)
        if not data:
            break
        os.write(sys.stdout.fileno(), data)
        if data == b"\r":
            text = buffer.decode("utf-8", errors="replace")
            buffer.clear()
            if text == "exit":
                hook("SessionEnd", {})
                break
            process_turn(text)
        else:
            buffer.extend(data)
finally:
    termios.tcsetattr(fd, termios.TCSANOW, old_terminal)
