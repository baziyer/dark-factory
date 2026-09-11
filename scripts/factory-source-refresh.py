#!/usr/bin/env python3
"""Refresh a project root only while dispatch is durably paused and drained."""
import argparse
import importlib.util
import json
import os
from pathlib import Path
import re
import sqlite3
import subprocess
import sys
import tempfile
import time


HERE = Path(__file__).resolve().parent
SPEC = importlib.util.spec_from_file_location("factory_intake", HERE / "factory-intake.py")
intake = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(intake)


class RefreshError(Exception):
    pass


def state(home):
    with sqlite3.connect((home / "factory.sqlite3").as_uri() + "?mode=ro", uri=True) as connection:
        enabled, revision, active = connection.execute("SELECT dispatch_enabled, revision, (SELECT count(*) FROM runs WHERE phase <> 'terminal') FROM factory WHERE singleton=1").fetchone()
    return bool(enabled), revision, active


def command(argv, env, timeout=30):
    try:
        return subprocess.run(argv, check=True, capture_output=True, text=True, env=env, timeout=timeout).stdout
    except (OSError, subprocess.SubprocessError) as exc:
        raise RefreshError("command_failed:" + Path(argv[0]).name) from exc


def root(config):
    home = Path(config["factory_home"])
    try:
        with sqlite3.connect((home / "factory.sqlite3").as_uri() + "?mode=ro", uri=True) as connection:
            row = connection.execute("SELECT root FROM projects WHERE id=?", (bytes.fromhex(config["project_id"]),)).fetchone()
    except sqlite3.Error as exc:
        raise RefreshError("project_root_unavailable") from exc
    if row is None or not isinstance(row[0], str) or not os.path.isabs(row[0]):
        raise RefreshError("project_root_unavailable")
    return Path(row[0])


def validate_source(path, config, env):
    base = config.get("base", "main")
    if not isinstance(base, str) or not re.fullmatch(r"[A-Za-z0-9_.-]{1,240}", base):
        raise RefreshError("invalid_base")
    git = ["git", "-C", str(path)]
    origin = command(git + ["remote", "get-url", "origin"], env).strip()
    if origin.removesuffix(".git").lower() != "https://github.com/" + config["repository"].lower():
        raise RefreshError("origin_mismatch")
    if command(git + ["branch", "--show-current"], env).strip() != base:
        raise RefreshError("branch_mismatch")
    if command(git + ["status", "--porcelain", "--untracked-files=no"], env).strip():
        raise RefreshError("tracked_changes")
    return base


def fetch_target(path, base, env):
    with tempfile.TemporaryDirectory(prefix="factory-refresh-hooks-") as hooks:
        git = ["git", "-C", str(path), "-c", "core.hooksPath=" + hooks]
        command(git + ["fetch", "--no-recurse-submodules", "origin", "refs/heads/" + base], env, 120)
        target = command(git + ["rev-parse", "FETCH_HEAD"], env).strip()
    if not re.fullmatch(r"[0-9a-f]{40}", target):
        raise RefreshError("fetch_unproven")
    return target


def refresh(config):
    config = intake.validate_config(config)
    home, path = Path(config["factory_home"]), root(config)
    env = dict(os.environ, DARK_FACTORY_SOCKET=str(home / "runtimes" / "factory.sock"), DARK_FACTORY_OPERATOR_TOKEN_FILE=str(home / "operator.token"))
    base = validate_source(path, config, env)
    target = fetch_target(path, base, env)
    validate_source(path, config, env)
    if command(["git", "-C", str(path), "rev-parse", "HEAD"], env).strip() == target:
        return {"refreshed": False}
    enabled, original, _ = state(home)
    paused = None
    try:
        raw = command(["factoryctl", "dispatch", "off", "--revision", str(original)], env, 15)
        result = json.loads(raw)
        expected = original + int(enabled)
        if type(result.get("revision")) is not int or result["revision"] != expected:
            raise RefreshError("pause_unproven")
        paused = expected
        deadline = time.monotonic() + 300
        while True:
            current_enabled, revision, active = state(home)
            if current_enabled or revision != paused:
                raise RefreshError("operator_changed")
            if not active:
                break
            if time.monotonic() >= deadline:
                raise RefreshError("drain_timeout")
            time.sleep(1)
        validate_source(path, config, env)
        current_enabled, revision, _ = state(home)
        if current_enabled or revision != paused:
            raise RefreshError("operator_changed")
        with tempfile.TemporaryDirectory(prefix="factory-refresh-hooks-") as hooks:
            git = ["git", "-C", str(path), "-c", "core.hooksPath=" + hooks]
            command(git + ["merge-base", "--is-ancestor", "HEAD", target], env, 30)
            command(git + ["merge", "--ff-only", target], env, 120)
        if command(["git", "-C", str(path), "rev-parse", "HEAD"], env).strip() != target:
            raise RefreshError("merge_unproven")
    except (RefreshError, json.JSONDecodeError, sqlite3.Error) as exc:
        raise RefreshError(str(exc)) from exc
    finally:
        if enabled and paused is not None:
            current_enabled, revision, _ = state(home)
            if not current_enabled and revision == paused:
                command(["factoryctl", "dispatch", "on", "--revision", str(paused)], env, 15)
                restored, restored_revision, _ = state(home)
                if not restored or restored_revision != paused + 1:
                    raise RefreshError("restore_unproven")
    return {"refreshed": True}


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("config", type=Path)
    parser.add_argument("--once", action="store_true")
    args = parser.parse_args(argv)
    try:
        print(json.dumps(refresh(json.loads(args.config.read_text()))))
        return 0
    except (OSError, ValueError, json.JSONDecodeError, sqlite3.Error, intake.IntakeError, RefreshError) as exc:
        print("factory-source-refresh: " + str(exc), file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
