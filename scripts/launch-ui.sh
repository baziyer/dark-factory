#!/bin/sh
set -eu

script_directory=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
repository_root=$(CDPATH= cd -- "$script_directory/.." && pwd)
factoryctl="$repository_root/target/release/factoryctl"
factory_tui="$repository_root/target/release/factory-tui"

if [ ! -x "$factoryctl" ] || [ ! -x "$factory_tui" ]; then
    echo "Dark Factory release binaries are missing." >&2
    echo "Run: cargo +1.88.0 build --locked --workspace --release" >&2
    exit 1
fi

if ! "$factoryctl" health >/dev/null 2>&1; then
    echo "factoryd is not healthy at the configured local socket." >&2
    echo "If launchd is installed, start it with:" >&2
    echo "launchctl kickstart gui/$(id -u)/com.dark-factory.factoryd" >&2
    echo "Then run this script again; keep this terminal open while using the UI." >&2
    exit 1
fi

echo "Dark Factory UI is running in the foreground. Close it with Ctrl-C; factoryd remains managed separately." >&2
exec "$factory_tui"
