#!/usr/bin/env python3
"""Deploy one already-merged, independently-reviewed pull request commit.

GitHub reads are performed through ``gh``.  Deployment and verification are
operator-owned fixed argv arrays; no issue or task text becomes a command.
"""
import argparse
import fcntl
import hashlib
import json
import os
import re
import signal
import subprocess
import sys
import tempfile
import time
from pathlib import Path

SHA = re.compile(r"^[0-9a-f]{40}$")
SOURCE_FOOTER = re.compile(r"(?mi)^(Refs|Closes) #([1-9][0-9]*)\s*$")
APP_MARKER_TRAILER = re.compile(r"(?mi)^<!-- dark-factory-operation:[0-9a-f]{8}(?:-[0-9a-f]{4}){3}-[0-9a-f]{12}:[0-9a-f]{64} -->\s*\Z")
MAX_RANGE_COMMITS = 100
MAX_RANGE_PULLS = 100


class ReleaseError(Exception):
    pass


def atomic_json(path, value):
    path.parent.mkdir(parents=True, exist_ok=True)
    fd, name = tempfile.mkstemp(prefix=f".{path.name}.", dir=path.parent)
    try:
        with os.fdopen(fd, "w", encoding="utf-8") as stream:
            json.dump(value, stream, indent=2, sort_keys=True)
            stream.write("\n")
            stream.flush()
            os.fsync(stream.fileno())
        os.replace(name, path)
        directory = os.open(path.parent, os.O_RDONLY)
        try:
            os.fsync(directory)
        finally:
            os.close(directory)
    except BaseException:
        try:
            os.unlink(name)
        except FileNotFoundError:
            pass
        raise


def run(argv, timeout=60, env=None):
    try:
        process = subprocess.Popen(argv, text=True, stdout=subprocess.PIPE, stderr=subprocess.PIPE, env=env, start_new_session=True)
        stdout, _ = process.communicate(timeout=timeout)
    except subprocess.TimeoutExpired as exc:
        # The fixed operator hook owns this fresh process group. Killing the
        # group prevents a timed-out wrapper from leaving its installer alive.
        try:
            os.killpg(process.pid, signal.SIGTERM)
            process.communicate(timeout=5)
        except subprocess.TimeoutExpired:
            os.killpg(process.pid, signal.SIGKILL)
            process.communicate()
        except ProcessLookupError:
            pass
        raise ReleaseError(f"command timed out: {argv[0]}") from exc
    except OSError as exc:
        # Hooks may write credentials or private task text to stderr.  Receipts
        # are durable and launcher output is public to the local operator.
        raise ReleaseError(f"command failed: {argv[0]}") from exc
    if process.returncode:
        raise ReleaseError(f"command failed: {argv[0]}")
    return stdout


def valid_argv(value, name):
    if not isinstance(value, list) or not value or any(not isinstance(item, str) or not item for item in value):
        raise ReleaseError(f"{name} must be a non-empty argv array")
    executable = Path(value[0])
    if not executable.is_absolute() or not executable.is_file() or not os.access(executable, os.X_OK):
        raise ReleaseError(f"{name} executable must be an absolute executable file")


def validate_config(config):
    if not isinstance(config, dict):
        raise ReleaseError("config must be an object")
    for key in ("repository", "base", "journal", "deploy_argv", "verify_argv", "review_verifier"):
        if key not in config:
            raise ReleaseError(f"missing config field: {key}")
    if not isinstance(config["repository"], str) or not re.fullmatch(r"[A-Za-z0-9_.-]{1,100}/[A-Za-z0-9_.-]{1,100}", config["repository"]):
        raise ReleaseError("repository must be OWNER/REPOSITORY")
    if not isinstance(config["base"], str) or not re.fullmatch(r"[A-Za-z0-9_.-]{1,240}", config["base"]):
        raise ReleaseError("base must be a branch name")
    if not os.path.isabs(config["journal"]):
        raise ReleaseError("journal must be absolute")
    valid_argv(config["deploy_argv"], "deploy_argv")
    valid_argv(config["verify_argv"], "verify_argv")
    try:
        timeout = int(config.get("command_timeout", 60))
    except (TypeError, ValueError) as exc:
        raise ReleaseError("command_timeout must be an integer") from exc
    if timeout < 5 or timeout > 1200:
        raise ReleaseError("command_timeout must be between 5 and 1200")
    valid_argv(config["review_verifier"], "review_verifier")
    if "allow_nonancestor_baseline" in config and type(config["allow_nonancestor_baseline"]) is not bool:
        raise ReleaseError("allow_nonancestor_baseline must be a boolean")


def load(path):
    if not path.exists():
        return {"version": 1, "releases": {}}
    try:
        value = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as exc:
        raise ReleaseError("release journal is unreadable") from exc
    if value.get("version") != 1 or not isinstance(value.get("releases"), dict):
        raise ReleaseError("release journal has an invalid version")
    tip = value.get("live_tip")
    if tip is not None and (not isinstance(tip, dict) or not SHA.fullmatch(str(tip.get("sha", ""))) or type(tip.get("healthy")) is not bool):
        raise ReleaseError("release journal has an invalid live tip")
    return value


def config_fingerprint(config):
    relevant = {key: config.get(key) for key in ("repository", "base", "deploy_argv", "verify_argv", "review_verifier", "command_timeout")}
    if "allow_nonancestor_baseline" in config:
        relevant["allow_nonancestor_baseline"] = config["allow_nonancestor_baseline"]
    return hashlib.sha256(json.dumps(relevant, sort_keys=True, separators=(",", ":")).encode()).hexdigest()


def review_lines(reviews):
    lines = []
    for review in reviews:
        author = review.get("user") or review.get("author") or {}
        author_id = str(author.get("id", ""))
        body = str(review.get("body", "")).replace("\t", " ").replace("\r", " ").replace("\n", " ")
        body = re.sub(r"[\x00-\x08\x0b\x0c\x0e-\x1f\x7f]", "?", body)
        lines.append("\t".join((str(review.get("commit_id", "")), str(review.get("state", "")), author_id, body)))
    return "\n".join(lines) + ("\n" if lines else "")


def review_gate(config, head, reviews):
    fd, path = tempfile.mkstemp(prefix="factory-review-")
    try:
        with os.fdopen(fd, "w", encoding="utf-8") as stream:
            stream.write(review_lines(reviews))
        env = os.environ.copy()
        env["DF_REVIEW_HEAD_SHA"] = head
        env["DF_REVIEW_REVIEWS"] = path
        run(config["review_verifier"], int(config.get("command_timeout", 60)), env)
    finally:
        try:
            os.unlink(path)
        except FileNotFoundError:
            pass


def check_runs_ok(checks):
    if not isinstance(checks, list) or not checks:
        return False
    successful = False
    for check in checks:
        if not isinstance(check, dict):
            return False
        status = str(check.get("status", "")).lower()
        conclusion = str(check.get("conclusion", "")).lower()
        if status not in {"completed", "complete"}:
            return False
        if conclusion not in {"success", "neutral", "skipped"}:
            return False
        successful = successful or conclusion == "success"
    return successful


def merge_gate(pr, default_sha, reviews, checks, config, expected):
    if not isinstance(expected, str) or not SHA.fullmatch(expected) or not isinstance(pr, dict) or str(pr.get("state", "")).upper() != "MERGED":
        raise ReleaseError("pull request is not merged")
    if pr.get("baseRefName") != config["base"] or pr.get("mergeCommitSha") != expected:
        raise ReleaseError("merged pull request does not name the exact configured base and SHA")
    if default_sha != expected:
        raise ReleaseError("configured default branch is not at the merged SHA")
    head = pr.get("headRefOid")
    if not SHA.fullmatch(str(head or "")):
        raise ReleaseError("pull request head is invalid")
    if not check_runs_ok(checks):
        raise ReleaseError("pull request checks are incomplete or failing")
    if not any(f"Dark-Factory-Review: allow {head}" in str(item.get("body", "")) and str(item.get("commit_id")) == head and str((item.get("user") or {}).get("id", "")) == "319516570" for item in reviews):
        raise ReleaseError("no exact independent Maintainer ALLOW review at the pull request head")
    return head


def verify_output(raw, expected):
    try:
        value = json.loads(raw)
    except json.JSONDecodeError as exc:
        raise ReleaseError("verification hook did not emit JSON") from exc
    if not isinstance(value, dict) or value.get("sha") != expected or not isinstance(value.get("healthy"), bool):
        raise ReleaseError("verification hook did not prove the requested SHA")
    return value


def verify(config, expected):
    value = verify_output(run(config["verify_argv"] + [expected], int(config.get("command_timeout", 60))), expected)
    if value["healthy"] is not True:
        raise ReleaseError("verification hook reports the requested SHA is not healthy")
    return value


def probe(config, expected):
    try:
        value = json.loads(run(config["verify_argv"] + [expected], int(config.get("command_timeout", 60))))
    except json.JSONDecodeError as exc:
        raise ReleaseError("live probe did not emit JSON") from exc
    if not isinstance(value, dict) or not isinstance(value.get("sha"), str) or not SHA.fullmatch(value["sha"]) or not isinstance(value.get("healthy"), bool):
        raise ReleaseError("live probe did not identify the installed revision")
    return value


def gh_snapshot(config, number):
    repo = config["repository"]
    pr = json.loads(run(["gh", "pr", "view", str(number), "--repo", repo, "--json", "state,baseRefName,mergeCommit,headRefOid" ]))
    pr["mergeCommitSha"] = (pr.get("mergeCommit") or {}).get("oid")
    default = json.loads(run(["gh", "api", f"repos/{repo}/git/ref/heads/{config['base']}"]))["object"]["sha"]
    review_pages = json.loads(run(["gh", "api", f"repos/{repo}/pulls/{number}/reviews", "--paginate", "--slurp"]))
    reviews = [item for page in review_pages for item in page] if review_pages and isinstance(review_pages[0], list) else review_pages
    check_pages = json.loads(run(["gh", "api", f"repos/{repo}/commits/{pr.get('headRefOid')}/check-runs", "--paginate", "--slurp"]))
    checks = [item for page in check_pages for item in page.get("check_runs", [])] if check_pages and isinstance(check_pages[0], dict) and "check_runs" in check_pages[0] else check_pages.get("check_runs", [])
    return pr, default, reviews, checks


def latest_pr(config):
    repo = config["repository"]
    default = json.loads(run(["gh", "api", f"repos/{repo}/git/ref/heads/{config['base']}"]))["object"]["sha"]
    pulls = json.loads(run(["gh", "api", f"repos/{repo}/commits/{default}/pulls"]))
    for pull in pulls if isinstance(pulls, list) else []:
        if pull.get("base", {}).get("ref") == config["base"] and pull.get("merge_commit_sha") == default:
            return int(pull["number"])
    return None


def range_sources(config, previous, target):
    """Return every factory PR and explicit source issue between two live tips."""
    repo = config["repository"]
    compare = json.loads(run(["gh", "api", f"repos/{repo}/compare/{previous}...{target}"]))
    if not isinstance(compare, dict) or compare.get("status") != "ahead":
        if config.get("allow_nonancestor_baseline") is True:
            return [], "nonancestor_baseline"
        raise ReleaseError("installed revision is not an ancestor of the deployment target")
    count = compare.get("total_commits")
    commits = compare.get("commits")
    if type(count) is not int or count < 1 or count > MAX_RANGE_COMMITS or not isinstance(commits, list) or len(commits) != count:
        raise ReleaseError("deployment range is incomplete or exceeds the commit bound")
    shas = []
    for commit in commits:
        sha = commit.get("sha") if isinstance(commit, dict) else None
        if not isinstance(sha, str) or not SHA.fullmatch(sha) or sha in shas:
            raise ReleaseError("deployment range contains invalid commits")
        shas.append(sha)
    by_number = {}
    for sha in shas:
        pages = json.loads(run(["gh", "api", f"repos/{repo}/commits/{sha}/pulls?per_page={MAX_RANGE_PULLS}", "--paginate", "--slurp"]))
        if not isinstance(pages, list) or len(pages) > 1 or any(not isinstance(page, list) or len(page) > MAX_RANGE_PULLS for page in pages):
            raise ReleaseError("deployment pull-request lookup is incomplete or exceeds the bound")
        for pull in (pages[0] if pages else []):
            if not isinstance(pull, dict):
                raise ReleaseError("deployment pull-request lookup is malformed")
            number = pull.get("number")
            merge = pull.get("merge_commit_sha")
            base = pull.get("base")
            if not isinstance(base, dict):
                raise ReleaseError("deployment pull-request lookup is malformed")
            if base.get("ref") != config["base"] or not pull.get("merged_at"):
                continue
            if type(number) is not int or number < 1 or not isinstance(merge, str) or merge not in shas:
                raise ReleaseError("deployment pull request is malformed")
            prior = by_number.get(number)
            if prior is not None and prior != merge:
                raise ReleaseError("deployment pull request has ambiguous merge commits")
            by_number[number] = merge
    sources = []
    for sha in shas:
        for number, merge in sorted(by_number.items()):
            if merge != sha:
                continue
            detail = json.loads(run(["gh", "pr", "view", str(number), "--repo", repo, "--json", "state,baseRefName,mergeCommit,body"]))
            actual = (detail.get("mergeCommit") or {}).get("oid") if isinstance(detail, dict) else None
            if not isinstance(detail, dict) or detail.get("state") != "MERGED" or detail.get("baseRefName") != config["base"] or actual != merge or not isinstance(detail.get("body"), str):
                raise ReleaseError("deployment pull request changed or is malformed")
            footer = SOURCE_FOOTER.findall(detail["body"])
            terminal_footer = re.search(r"(?mi)^(Refs|Closes) #([1-9][0-9]*)[ \t]*(?:\n\s*)?\Z", detail["body"])
            if terminal_footer is None:
                marker = APP_MARKER_TRAILER.search(detail["body"])
                before_marker = detail["body"][:marker.start()] if marker else ""
                terminal_footer = re.search(r"(?mi)^(Refs|Closes) #([1-9][0-9]*)[ \t]*\s*\Z", before_marker)
            if len(footer) != 1 or terminal_footer is None:
                raise ReleaseError(f"merged PR #{number} must contain exactly one Refs #N or Closes #N footer")
            kind, issue = footer[0]
            sources.append({"pr": number, "merge_sha": merge, "issue": int(issue), "reference": kind.lower()})
    if len(sources) != len(by_number):
        raise ReleaseError("deployment pull-request lookup did not cover every merged PR")
    return sources, "range"


def record_live_tip(journal, value):
    journal["live_tip"] = {"sha": value["sha"], "healthy": value["healthy"], "observed_at": int(time.time())}


def once(config, number, retry=False):
    journal_path = Path(config["journal"])
    lock_path = Path(str(journal_path) + ".lock")
    lock_path.parent.mkdir(parents=True, exist_ok=True)
    with lock_path.open("a+") as lock:
        try:
            fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError as exc:
            raise ReleaseError("another release invocation is running") from exc
        fingerprint = config_fingerprint(config)
        journal = load(journal_path)
        entry = journal["releases"].get(str(number))
        if entry and entry.get("config_fingerprint") != fingerprint:
            raise ReleaseError("journal receipt belongs to a different repository or hook configuration")
        if entry and entry.get("state") == "blocked" and not retry:
            raise ReleaseError("release is blocked; inspect the receipt and use --retry explicitly")
        if entry and entry.get("state") == "running":
            try:
                value = probe(config, entry["sha"])
                if value["healthy"] is not True or value["sha"] != entry["sha"]:
                    raise ReleaseError("deployment outcome is ambiguous; live probe reports another state")
                entry.update({"state": "verified", "verification": value, "verified_at": int(time.time())})
                record_live_tip(journal, value)
                atomic_json(journal_path, journal)
                return entry
            except ReleaseError:
                entry["state"] = "blocked"
                entry["error"] = "deployment outcome is ambiguous; inspect before retrying"
                atomic_json(journal_path, journal)
                raise ReleaseError(entry["error"])
        pr, default, reviews, checks = gh_snapshot(config, number)
        sha = pr.get("mergeCommitSha")
        merge_gate(pr, default, reviews, checks, config, sha)
        if entry and entry.get("sha") != sha:
            raise ReleaseError("journal has a different SHA for this pull request")
        review_gate(config, pr["headRefOid"], reviews)
        entry = entry or {"pr": number, "sha": sha, "state": "planned"}
        entry.update({"sha": sha, "state": "planned", "config_fingerprint": fingerprint, "updated_at": int(time.time())})
        journal["releases"][str(number)] = entry
        atomic_json(journal_path, journal)
        try:
            value = probe(config, sha)
        except ReleaseError:
            entry["state"] = "blocked"
            entry["error"] = "live probe unavailable or malformed before deployment"
            atomic_json(journal_path, journal)
            raise ReleaseError(entry["error"])
        prior_tip = journal.get("live_tip")
        if prior_tip is not None and prior_tip["sha"] != value["sha"]:
            entry["state"] = "blocked"
            entry["error"] = "live revision changed outside the release journal"
            atomic_json(journal_path, journal)
            raise ReleaseError(entry["error"])
        previous = value["sha"]
        if prior_tip is None:
            record_live_tip(journal, value)
        if previous == sha:
            sources, delivery_mode = [], "baseline_current" if prior_tip is None else "unchanged"
        else:
            try:
                sources, delivery_mode = range_sources(config, previous, sha)
            except ReleaseError as exc:
                entry["state"] = "blocked"
                entry["error"] = str(exc)
                atomic_json(journal_path, journal)
                raise
        entry.update({"delivery_from_sha": previous, "delivery_sources": sources, "delivery_mode": delivery_mode})
        # Persist the observed installed SHA and complete source mapping before
        # deployment; a crash cannot turn an unrecorded range into a delivery.
        atomic_json(journal_path, journal)
        if value["healthy"] is True and value["sha"] == sha:
            entry.update({"state": "verified", "verification": value, "verified_at": int(time.time())})
            record_live_tip(journal, value)
            atomic_json(journal_path, journal)
            return entry
        fresh_default = json.loads(run(["gh", "api", f"repos/{config['repository']}/git/ref/heads/{config['base']}"]))["object"]["sha"]
        if fresh_default != sha:
            entry["state"] = "blocked"
            entry["error"] = "configured default branch moved after planning"
            atomic_json(journal_path, journal)
            raise ReleaseError(entry["error"])
        entry["state"] = "running"
        atomic_json(journal_path, journal)
        try:
            run(config["deploy_argv"] + [sha], int(config.get("command_timeout", 60)))
            value = verify(config, sha)
        except ReleaseError as exc:
            entry["state"] = "blocked"
            entry["error"] = str(exc)
            atomic_json(journal_path, journal)
            raise
        entry.update({"state": "verified", "verification": value, "verified_at": int(time.time())})
        record_live_tip(journal, value)
        atomic_json(journal_path, journal)
        return entry


def main(argv=None):
    parser = argparse.ArgumentParser()
    parser.add_argument("config", type=Path)
    parser.add_argument("--pr", type=int)
    parser.add_argument("--latest", action="store_true")
    parser.add_argument("--once", action="store_true")
    parser.add_argument("--status", action="store_true")
    parser.add_argument("--retry", action="store_true")
    args = parser.parse_args(argv)
    try:
        config = json.loads(args.config.read_text(encoding="utf-8"))
        validate_config(config)
        path = Path(config["journal"])
        if args.status:
            print(json.dumps(load(path), indent=2, sort_keys=True))
            return 0
        if (args.pr is None) == (not args.latest):
            raise ReleaseError("choose exactly one of --pr or --latest")
        number = latest_pr(config) if args.latest else args.pr
        if number is None:
            print(json.dumps({"state": "no release available"}, sort_keys=True))
            return 0
        if number < 1:
            raise ReleaseError("pull request number is invalid")
        print(json.dumps(once(config, number, args.retry), sort_keys=True))
        return 0
    except (OSError, json.JSONDecodeError, ReleaseError) as exc:
        print(f"factory-release: {exc}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
