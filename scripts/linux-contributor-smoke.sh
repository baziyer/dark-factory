#!/bin/sh
set -eu

# A short, provider-free contributor check for Ubuntu. The workspace tests
# exercise the deeper PTY/restart/attach lifecycle; this script proves that a
# fresh source checkout can bring up the real binaries and complete one task
# through the public CLI and shell-provider boundary.

repo_root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
target_root=${CARGO_TARGET_DIR:-"$repo_root/target"}
bin_dir="$target_root/debug"
factoryd="$bin_dir/factoryd"
factoryctl="$bin_dir/factoryctl"
factory_tui="$bin_dir/factory-tui"
fixture="$repo_root/crates/factoryd/tests/fixtures/shell-agent.sh"

for binary in "$factoryd" "$factoryctl" "$factory_tui"; do
    test -x "$binary" || {
        echo "missing executable: $binary (run cargo build --workspace first)" >&2
        exit 1
    }
done
test -x "$fixture"

scratch=$(mktemp -d /tmp/dark-factory-linux-smoke.XXXXXX)
home="$scratch/home"
repo="$scratch/repo"
socket="$home/f.sock"
mkdir -p "$home" "$repo" "$scratch/user"
chmod 700 "$home" "$scratch/user"
export DARK_FACTORY_HOME="$home"
export DARK_FACTORY_SOCKET="$socket"
export HOME="$scratch/user"

daemon_pid=
cleanup() {
    if test -n "$daemon_pid"; then
        kill -TERM "$daemon_pid" 2>/dev/null || true
        wait "$daemon_pid" 2>/dev/null || true
    fi
    rm -rf "$scratch"
}
trap cleanup EXIT HUP INT TERM

git -C "$repo" init -q -b main
git -C "$repo" config user.email linux-smoke@example.invalid
git -C "$repo" config user.name "Linux source smoke"
printf '%s\n' '# Linux source smoke' >"$repo/README.md"
git -C "$repo" add README.md
git -C "$repo" commit -q -m 'initialize Linux source smoke repository'

"$factoryd" --socket "$socket" >"$scratch/factoryd.log" 2>&1 &
daemon_pid=$!

attempt=0
while ! "$factoryctl" --socket "$socket" health >/dev/null 2>&1; do
    attempt=$((attempt + 1))
    test "$attempt" -lt 300 || {
        cat "$scratch/factoryd.log" >&2
        echo "factoryd did not become healthy" >&2
        exit 1
    }
    sleep 0.1
done

socket_mode=$(stat -c '%a' "$socket")
test "$socket_mode" = 600 || {
    echo "expected private socket mode 600, got $socket_mode" >&2
    exit 1
}

# `--version` is the non-interactive source launch check for the TUI. The
# interactive PTY/attach path is covered by the deterministic sessions suite.
"$factory_tui" --version >/dev/null

"$factoryctl" project add \
    --id linux-smoke-project \
    --name 'Linux source smoke' \
    --root "$repo" >/dev/null
"$factoryctl" agent add \
    --id linux-smoke-agent \
    --project linux-smoke-project \
    --role worker \
    --provider shell \
    --model "$fixture" >/dev/null
"$factoryctl" task add \
    --id linux-smoke-task \
    --project linux-smoke-project \
    --agent linux-smoke-agent \
    --title 'Complete the Linux source smoke' \
    --body 'Use the deterministic shell provider and report success.' >/dev/null

attempt=0
while ! "$factoryctl" task get \
    --project linux-smoke-project \
    --task linux-smoke-task 2>/dev/null | grep -q '"status":"succeeded"'; do
    attempt=$((attempt + 1))
    test "$attempt" -lt 300 || {
        "$factoryctl" status >&2 || true
        cat "$scratch/factoryd.log" >&2
        echo "shell-provider task did not complete" >&2
        exit 1
    }
    sleep 0.1
done

echo "Linux source smoke passed: private socket, factoryd health, factory-tui, and shell task"
