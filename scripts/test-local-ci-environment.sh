#!/bin/sh
set -eu

repository_root=$(CDPATH='' cd -- "$(dirname "$0")/.." && pwd)
boundary=$repository_root/scripts/local-ci-environment.sh
temporary=$(mktemp -d "${TMPDIR:-/tmp}/dark-factory-local-ci-environment.XXXXXX")
trap 'rm -rf "$temporary"' EXIT HUP INT TERM

fail() {
    echo "local-ci environment test failed: $*" >&2
    exit 1
}

live_names='DARK_FACTORY_HOME
DARK_FACTORY_SOCKET
DARK_FACTORY_PROJECT
DARK_FACTORY_AGENT
DARK_FACTORY_SESSION
DARK_FACTORY_SESSION_TOKEN_FILE
DARK_FACTORY_AGENT_DIR
DARK_FACTORY_TASK
DARK_FACTORY_RUN'

child_environment=$temporary/child.env
(
    for name in $live_names; do
        export "$name=hostile live value; \$(must remain data)"
    done
    export DARK_FACTORY_LOCAL_CI_TEST_SENTINEL=preserved-local-ci-test-seam
    export DARK_FACTORY_LOCAL_CI_LEASE_HELD=1
    export CARGO_TARGET_DIR=/intentional/build-target
    export RUSTUP_TOOLCHAIN=1.88.0
    export CODEX_HOME=/intentional/provider-home
    export OPENAI_API_KEY=preserved-fake-provider-credential
    export DARK_FACTORY_UPDATE_URL=https://fixture.invalid/manifest.json
    # shellcheck source=scripts/local-ci-environment.sh
    . "$boundary"
    exec sh -c 'env'
) >"$child_environment"

for name in $live_names; do
    if grep -q "^$name=" "$child_environment"; then
        fail "$name reached a child gate command"
    fi
done

for expected in \
    'DARK_FACTORY_LOCAL_CI_TEST_SENTINEL=preserved-local-ci-test-seam' \
    'DARK_FACTORY_LOCAL_CI_LEASE_HELD=1' \
    'CARGO_TARGET_DIR=/intentional/build-target' \
    'RUSTUP_TOOLCHAIN=1.88.0' \
    'CODEX_HOME=/intentional/provider-home' \
    'OPENAI_API_KEY=preserved-fake-provider-credential' \
    'DARK_FACTORY_UPDATE_URL=https://fixture.invalid/manifest.json'
do
    grep -F -x "$expected" "$child_environment" >/dev/null \
        || fail "intentional input was removed: ${expected%%=*}"
done

sh -n "$boundary" "$repository_root/scripts/local-ci.sh"
echo "local-ci environment tests passed"
