#!/usr/bin/env python3
"""Send one local, content-free macOS alert for unattended factory attention."""
from __future__ import annotations

import argparse
import fcntl
import json
import os
import sqlite3
import subprocess
import sys
import tempfile
from contextlib import contextmanager
from pathlib import Path


class NotifyError(Exception):
    pass


def atomic_json(path: Path, value: dict) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    fd, temporary = tempfile.mkstemp(prefix=f".{path.name}.", dir=path.parent)
    try:
        with os.fdopen(fd, "w", encoding="utf-8") as stream:
            json.dump(value, stream, sort_keys=True)
            stream.write("\n")
            stream.flush()
            os.fsync(stream.fileno())
        os.replace(temporary, path)
        os.chmod(path, 0o600)
    except BaseException:
        try:
            os.unlink(temporary)
        except FileNotFoundError:
            pass
        raise


@contextmanager
def receipt_lock(receipt: Path):
    lock = receipt.with_name(receipt.name + ".lock")
    lock.parent.mkdir(parents=True, exist_ok=True)
    descriptor = os.open(lock, os.O_CREAT | os.O_RDWR, 0o600)
    try:
        fcntl.flock(descriptor, fcntl.LOCK_EX)
        yield
    finally:
        os.close(descriptor)


def load_receipt(path: Path) -> dict:
    if not path.exists():
        return {"version": 2, "notified_request_ids": [], "recovery_task_ids": [], "run_limit_notified": False}
    try:
        receipt = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as exc:
        raise NotifyError("notification receipt is unreadable") from exc
    if receipt.get("version") == 1 and isinstance(receipt.get("notified_request_ids"), list) and isinstance(receipt.get("run_limit_notified"), bool):
        receipt = {"version": 2, "notified_request_ids": receipt["notified_request_ids"], "recovery_task_ids": [], "run_limit_notified": receipt["run_limit_notified"], "_migrated": True}
    ids, recoveries = receipt.get("notified_request_ids"), receipt.get("recovery_task_ids")
    if receipt.get("version") != 2 or not isinstance(ids, list) or not isinstance(recoveries, list) or not all(isinstance(item, str) and len(item) == 32 for item in ids + recoveries) or len(set(ids)) != len(ids) or len(set(recoveries)) != len(recoveries) or not isinstance(receipt.get("run_limit_notified"), bool):
        raise NotifyError("notification receipt is invalid")
    return receipt


def observe(home: Path) -> tuple[tuple[str, ...], int]:
    database = home / "factory.sqlite3"
    if not database.is_file():
        raise NotifyError("factory database is missing")
    try:
        with sqlite3.connect(f"file:{database}?mode=ro", uri=True) as connection:
            requests = tuple(row[0] for row in connection.execute("SELECT lower(hex(id)) FROM human_requests WHERE status IN ('open', 'delivering', 'delivery_unknown') ORDER BY created_at_ms, id"))
            blocked = connection.execute("SELECT COUNT(*) FROM tasks AS t JOIN projects AS p ON p.id = t.project_id WHERE t.status = 'queued' AND p.run_budget_limit > 0 AND p.runs_used >= p.run_budget_limit").fetchone()
    except sqlite3.Error as exc:
        raise NotifyError("factory state could not be read") from exc
    if blocked is None or not isinstance(blocked[0], int):
        raise NotifyError("factory state was incomplete")
    return requests, blocked[0]


def recoveries(receipt_path: Path) -> tuple[str, ...]:
    journal = receipt_path.with_suffix("")
    if not journal.exists():
        return ()
    try:
        records = json.loads(journal.read_text(encoding="utf-8")).get("issues", {})
    except (OSError, json.JSONDecodeError) as exc:
        raise NotifyError("intake journal is unreadable") from exc
    if not isinstance(records, dict):
        raise NotifyError("intake journal is invalid")
    values = []
    for record in records.values():
        recovery = record.get("needs_operator_recovery") if isinstance(record, dict) else None
        task_id = recovery.get("task_id") if isinstance(recovery, dict) else None
        if not isinstance(task_id, str) or len(task_id) != 32:
            raise NotifyError("intake recovery record is invalid")
        values.append(task_id)
    return tuple(sorted(set(values)))


def notify(message: str) -> None:
    program = "on run argv\ndisplay notification (item 2 of argv) with title (item 1 of argv)\nend run"
    try:
        subprocess.run(["/usr/bin/osascript", "-e", program, "Dark Factory", message], check=True, capture_output=True, text=True, timeout=10)
    except (OSError, subprocess.SubprocessError) as exc:
        raise NotifyError("local notification failed") from exc


def run_once(home: Path, receipt_path: Path) -> dict:
    with receipt_lock(receipt_path):
        requests, blocked = observe(home)
        recovery_ids = recoveries(receipt_path)
        receipt = load_receipt(receipt_path)
        known = set(receipt["notified_request_ids"])
        new_requests = [request for request in requests if request not in known]
        new_recoveries = [task_id for task_id in recovery_ids if task_id not in set(receipt["recovery_task_ids"])]
        limit_started = blocked > 0 and not receipt["run_limit_notified"]
        messages = []
        if new_requests:
            messages.append("A decision needs your answer." if len(new_requests) == 1 else f"{len(new_requests)} decisions need your answers.")
        if new_recoveries:
            messages.append("A source task needs operator recovery." if len(new_recoveries) == 1 else f"{len(new_recoveries)} source tasks need operator recovery.")
        if limit_started:
            messages.append("A project run limit is blocking queued work." if blocked == 1 else f"Project run limits are blocking {blocked} queued tasks.")
        if messages:
            notify(" ".join(messages))
        next_receipt = {"version": 2, "notified_request_ids": list(requests), "recovery_task_ids": list(recovery_ids), "run_limit_notified": blocked > 0}
        if next_receipt != receipt:
            atomic_json(receipt_path, next_receipt)
        return {"pending_human_requests": len(requests), "queued_at_run_limit": blocked, "notified": bool(messages)}


def main(argv=None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--once", action="store_true", help="Run one observation (the only supported mode).")
    parser.add_argument("--home", type=Path, required=True)
    parser.add_argument("--receipt", type=Path, required=True)
    parser.add_argument("--status", action="store_true", help="Print state without notifying or updating the receipt.")
    args = parser.parse_args(argv)
    if not args.home.is_absolute() or not args.receipt.is_absolute():
        parser.error("--home and --receipt must be absolute paths")
    try:
        if args.status:
            requests, blocked = observe(args.home)
            receipt = load_receipt(args.receipt)
            print(json.dumps({"pending_human_requests": len(requests), "queued_at_run_limit": blocked, "operator_recoveries": len(recoveries(args.receipt)), "notified_request_ids": len(receipt["notified_request_ids"]), "run_limit_notified": receipt["run_limit_notified"]}, sort_keys=True))
        else:
            print(json.dumps(run_once(args.home, args.receipt), sort_keys=True))
        return 0
    except (OSError, NotifyError) as exc:
        print(f"factory-notify: {exc}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
