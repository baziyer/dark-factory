#!/usr/bin/env python3
"""Wake the overseer for App-linked pull requests in a private host mirror."""
import argparse
import fcntl
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import re


HERE = Path(__file__).resolve().parent
SPEC = importlib.util.spec_from_file_location("factory_intake", HERE / "factory-intake.py")
intake = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(intake)
SHA = re.compile(r"^[0-9a-f]{40}$")
FOOTER = re.compile(r"(?im)^(?:refs|closes)\s+#([1-9][0-9]*)\s*$")


class ReviewError(Exception):
    pass


def mirror(config):
    root = config.get("review_mirror_root")
    if not isinstance(root, str) or not os.path.isabs(root):
        raise ReviewError("review_mirror_root must be an absolute path")
    owner, name = config["repository"].split("/", 1)
    path = Path(root) / owner / name
    if not path.is_dir() or not (path / "HEAD").is_file():
        raise ReviewError("review mirror is missing")
    if intake.command(["git", "-C", str(path), "rev-parse", "--is-bare-repository"]).strip() != "true":
        raise ReviewError("review mirror must be bare")
    origin = intake.command(["git", "-C", str(path), "remote", "get-url", "origin"]).strip()
    allowed = "https://github.com/" + config["repository"]
    if origin.removesuffix(".git") != allowed:
        raise ReviewError("review mirror origin must be the configured https GitHub repository")
    return path


def linked_issue(body, journal, repository):
    if not isinstance(body, str):
        raise ReviewError("pull request body is invalid")
    numbers = {int(value) for value in FOOTER.findall(body)}
    known = {record.get("number") for record in journal["issues"].values() if isinstance(record, dict) and record.get("managed")}
    matched = numbers & known
    if len(matched) > 1:
        raise ReviewError("pull request links multiple tracked source issues")
    return next(iter(matched), None)


def list_prs(config):
    limit = int(config.get("max_issues", 25))
    raw = intake.command(["gh", "pr", "list", "--repo", config["repository"], "--state", "open", "--limit", str(limit + 1), "--json", "number,headRefOid,body"], timeout=int(config.get("command_timeout", 30)))
    try:
        values = json.loads(raw)
    except json.JSONDecodeError as exc:
        raise ReviewError("gh returned invalid pull request JSON") from exc
    if not isinstance(values, list) or len(values) > limit:
        raise ReviewError("pull request review cap reached")
    for value in values:
        if not isinstance(value, dict) or not isinstance(value.get("number"), int) or value["number"] < 1 or not isinstance(value.get("body"), str) or not isinstance(value.get("headRefOid"), str) or not SHA.fullmatch(value["headRefOid"]):
            raise ReviewError("gh returned an invalid pull request")
    return values


def ready(config, path, pr, issue):
    base = config.get("base", "main")
    if not isinstance(base, str) or not re.fullmatch(r"[A-Za-z0-9_.-]{1,240}", base):
        raise ReviewError("base must be an explicit branch name")
    intake.command(["git", "-C", str(path), "fetch", "--no-tags", "origin", "+refs/heads/" + base + ":refs/remotes/origin/" + base, "+refs/pull/" + str(pr["number"]) + "/head:refs/pull/" + str(pr["number"]) + "/head"], timeout=120)
    head = intake.command(["git", "-C", str(path), "rev-parse", "refs/pull/" + str(pr["number"]) + "/head"]).strip()
    observed_base = intake.command(["git", "-C", str(path), "rev-parse", "refs/remotes/origin/" + base]).strip()
    if head != pr["headRefOid"] or not SHA.fullmatch(observed_base):
        raise ReviewError("mirror did not prove the App-reported exact head and base")
    marker = intake.source_marker(config, {"number": issue})
    task_id = intake.sha_id("review-wakeup", config["project_id"], config["repository"], str(pr["number"]), head)
    return {"pr": pr["number"], "head": head, "base": observed_base, "source_marker": marker,
            "task_id": task_id, "incarnation_id": intake.sha_id("incarnation", task_id),
            "priority": int(config.get("priority_default", 0)),
            "title": "Resume publication review for GitHub PR #" + str(pr["number"]),
            "body": "Resume the existing publication for " + marker + ". The host verified App-reported PR #" + str(pr["number"]) + " at exact head " + head + " and base " + observed_base + ". Review only from the private mirror " + str(path) + "; set DARK_FACTORY_REVIEW_REMOTE=file://" + str(path.parent.parent) + " for cold-review. Verify the exact head before any review or merge action. Do not author the independent review yourself; arrange the required independent exact-head review and resume the existing publication journal."}


def verify_existing(path, pr, operation):
    head, base = operation.get("head"), operation.get("base")
    if not isinstance(head, str) or not isinstance(base, str) or not SHA.fullmatch(head) or not SHA.fullmatch(base):
        raise ReviewError("review receipt is invalid")
    intake.command(["git", "-C", str(path), "fetch", "--no-tags", "origin", "+refs/pull/" + str(pr["number"]) + "/head:refs/pull/" + str(pr["number"]) + "/head"], timeout=120)
    observed = intake.command(["git", "-C", str(path), "rev-parse", "refs/pull/" + str(pr["number"]) + "/head"]).strip()
    if observed != pr["headRefOid"] or observed != head:
        raise ReviewError("mirror did not prove the App-reported exact head")
    intake.command(["git", "-C", str(path), "cat-file", "-e", base + "^{commit}"])


def config_fingerprint(config):
    value = {key: config.get(key) for key in ("repository", "project_id", "overseer_agent_id", "review_mirror_root")}
    return hashlib.sha256(json.dumps(value, sort_keys=True, separators=(",", ":")).encode()).hexdigest()


def run_once(config):
    config = intake.validate_config(config)
    path = mirror(config)
    journal_path = Path(config["journal"] + ".reviews.json")
    lock_path = Path(str(journal_path) + ".lock")
    journal = intake.load_journal(Path(config["journal"]))
    intake.bind_journal(config, journal)
    lock_path.parent.mkdir(parents=True, exist_ok=True)
    with lock_path.open("a+") as lock:
        try:
            fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError as exc:
            raise ReviewError("another review intake process owns the journal") from exc
        return run_locked(config, path, journal, journal_path)


def run_locked(config, path, journal, journal_path):
    if journal_path.exists():
        try:
            receipts = json.loads(journal_path.read_text())
        except (OSError, json.JSONDecodeError) as exc:
            raise ReviewError("review receipt is unreadable") from exc
        if receipts.get("version") != 2 or not isinstance(receipts.get("pulls"), dict) or receipts.get("config_fingerprint") != config_fingerprint(config):
            raise ReviewError("review receipt is invalid")
    else:
        receipts = {"version": 2, "config_fingerprint": config_fingerprint(config), "pulls": {}}
    messages = []
    for pr in list_prs(config):
        issue = linked_issue(pr["body"], journal, config["repository"])
        if issue is None:
            continue
        key = str(pr["number"]) + ":" + pr["headRefOid"]
        existing = receipts["pulls"].get(key)
        if existing is None:
            operation = ready(config, path, pr, issue)
            receipts["pulls"][key] = operation
            intake.atomic_json(journal_path, receipts)
        else:
            operation = existing
            verify_existing(path, pr, operation)
        if intake.task_state(config, operation) is None:
            intake.enqueue(config, operation)
            messages.append("woke PR #" + str(pr["number"]))
    return messages


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("config", type=Path)
    parser.add_argument("--once", action="store_true")
    args = parser.parse_args(argv)
    try:
        config = json.loads(args.config.read_text())
        print(json.dumps({"ok": True, "messages": run_once(config)}))
        return 0
    except (OSError, ValueError, json.JSONDecodeError, intake.IntakeError, ReviewError) as exc:
        print("factory-review-intake: " + str(exc), file=__import__("sys").stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
