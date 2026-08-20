#!/bin/sh
set -eu

repository_root=$(CDPATH='' cd -- "$(dirname "$0")/.." && pwd)
helper=$repository_root/scripts/prepare-test-binaries.sh
sessions=$repository_root/crates/factoryd/tests/sessions_e2e.rs
deadline=$repository_root/crates/factoryd/tests/session_start_deadline.rs
common=$repository_root/crates/factoryd/tests/common/mod.rs

grep -F 'cargo +1.88.0 test --locked --workspace --all-targets --no-run' "$helper" >/dev/null
grep -F 'command -v lockf' "$helper" >/dev/null
grep -F 'path.parent()' "$common" >/dev/null
grep -F 'mod common;' "$sessions" >/dev/null
grep -F 'mod common;' "$deadline" >/dev/null
if grep -F 'cargo build -p factory-runner -p factoryctl' "$sessions" "$deadline" >/dev/null; then
    echo 'nested test-owned sibling builds remain' >&2
    exit 1
fi
if grep -F 'Command::new(env!("CARGO"))' "$sessions" "$deadline" >/dev/null; then
    echo 'integration tests still invoke Cargo for sibling binaries' >&2
    exit 1
fi

echo 'test binary preparation and provenance checks passed'
